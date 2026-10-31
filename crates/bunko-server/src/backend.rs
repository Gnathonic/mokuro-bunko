//! `AuthBackend` on top of `bunko_db::Database` and the storage layout.

use crate::auth::{AuthBackend, AuthUser, paths};
use bunko_core::StorageLayout;
use bunko_core::reader_tree::{self, Caller};
use bunko_core::sidecar_names;
use bunko_db::Database;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::warn;

pub struct DbAuthBackend {
    pub db: Arc<Database>,
    pub layout: StorageLayout,
    logins: LoginCache,
    /// Series folder adoption was seen done (it never goes back to pending).
    adopted: std::sync::atomic::AtomicBool,
}

/// Verified Basic credentials, keyed by a salted SHA-256 of `username \0 password` (the
/// password itself is never stored), valid for [`LOGIN_TTL`] and only while
/// `users_version` is unchanged (any role/status/password change empties it).
struct LoginCache {
    salt: [u8; 16],
    entries: parking_lot::Mutex<
        std::collections::HashMap<[u8; 32], (AuthUser, u64, std::time::Instant)>,
    >,
}

const LOGIN_TTL: std::time::Duration = std::time::Duration::from_secs(300);
const LOGIN_CACHE_MAX: usize = 1024;

impl LoginCache {
    fn new() -> Self {
        let mut salt = [0u8; 16];
        rand::fill(&mut salt);
        Self {
            salt,
            entries: Default::default(),
        }
    }

    fn key(&self, username: &str, password: &str) -> [u8; 32] {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(self.salt);
        h.update(username.as_bytes());
        h.update([0u8]);
        h.update(password.as_bytes());
        h.finalize().into()
    }
}

impl DbAuthBackend {
    pub fn new(db: Arc<Database>, layout: StorageLayout) -> Self {
        Self {
            db,
            layout,
            logins: LoginCache::new(),
            adopted: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

fn to_auth(u: bunko_db::User) -> AuthUser {
    AuthUser {
        id: u.id,
        username: u.username,
        role: u.role,
    }
}

impl DbAuthBackend {
    /// Physical path of a virtual DAV path (0.5.2 `PathMapper.virtual_to_physical`), without
    /// resolving symlinks.
    pub fn virtual_to_physical(
        &self,
        virtual_path: &str,
        caller: Option<Caller<'_>>,
    ) -> Option<PathBuf> {
        virtual_to_physical(&self.layout, virtual_path, caller)
    }
}

/// See [`DbAuthBackend::virtual_to_physical`]. A per-user path maps into `caller`'s own
/// tree ([`reader_tree::user_base`]); anonymous has none.
fn virtual_to_physical(
    layout: &StorageLayout,
    virtual_path: &str,
    caller: Option<Caller<'_>>,
) -> Option<PathBuf> {
    let rel = virtual_path
        .trim_start_matches('/')
        .strip_prefix(paths::READER_ROOT)?;
    if !(rel.is_empty() || rel.starts_with('/')) {
        return None;
    }
    let rel = rel.trim_matches('/');
    if rel.split('/').any(|s| s == "..") {
        return None;
    }
    if reader_tree::is_per_user_rel(rel) {
        let base = reader_tree::user_base(&layout.library(), &layout.users(), caller?)?;
        return Some(base.join(rel));
    }
    Some(layout.library().join(rel))
}

impl AuthBackend for DbAuthBackend {
    fn resolve_token(&self, token: &str) -> Option<AuthUser> {
        match self.db.resolve_auth_token(token) {
            Ok(u) => u.map(to_auth),
            Err(e) => {
                warn!("token lookup failed: {e}");
                None
            }
        }
    }

    fn cached_login(&self, username: &str, password: &str) -> Option<AuthUser> {
        let key = self.logins.key(username, password);
        let version = self.db.users_version();
        let mut entries = self.logins.entries.lock();
        match entries.get(&key) {
            Some((user, v, at)) if *v == version && at.elapsed() < LOGIN_TTL => Some(user.clone()),
            Some(_) => {
                entries.remove(&key);
                None
            }
            None => None,
        }
    }

    fn check_password(&self, username: &str, password: &str) -> Option<AuthUser> {
        let version = self.db.users_version();
        match self.db.authenticate_user(username, password) {
            Ok(Some(u)) => {
                let user = to_auth(u);
                let mut entries = self.logins.entries.lock();
                if entries.len() >= LOGIN_CACHE_MAX {
                    entries.retain(|_, (_, v, at)| *v == version && at.elapsed() < LOGIN_TTL);
                    if entries.len() >= LOGIN_CACHE_MAX {
                        entries.clear();
                    }
                }
                entries.insert(
                    self.logins.key(username, password),
                    (user.clone(), version, std::time::Instant::now()),
                );
                Some(user)
            }
            Ok(None) => None,
            Err(e) => {
                warn!("password check failed: {e}");
                None
            }
        }
    }

    fn physical_exists(&self, virtual_path: &str, caller: Option<Caller<'_>>) -> bool {
        self.virtual_to_physical(virtual_path, caller)
            .is_some_and(bunko_store::fs::exists)
    }

    fn is_volume_primary(&self, virtual_path: &str) -> bool {
        is_volume_primary(&self.layout, virtual_path)
    }

    fn series_owner(&self, series: &str) -> Option<String> {
        let owner = match self.db.series_folder_owner(series) {
            Ok(owner) => owner?,
            Err(e) => {
                warn!("series owner lookup failed: {e}");
                return None;
            }
        };
        crate::library::owners::series_folder_is_live(&self.layout.library(), series)
            .then_some(owner)
    }

    fn governing_archives(&self, virtual_path: &str) -> Vec<String> {
        governing_archives(&self.layout, virtual_path)
    }

    fn volume_uploader(&self, rel: &str) -> Option<String> {
        self.db.get_volume_owner(rel).unwrap_or_else(|e| {
            warn!("volume owner lookup failed: {e}");
            None
        })
    }

    fn is_library_folder(&self, virtual_path: &str) -> bool {
        paths::library_rel(virtual_path)
            .is_some_and(|rel| bunko_store::fs::is_dir(self.layout.library().join(rel)))
    }

    fn ownership_pending(&self) -> bool {
        use std::sync::atomic::Ordering;
        if self.adopted.load(Ordering::Relaxed) {
            return false;
        }
        match self.db.series_folder_adoption_pending() {
            Ok(true) => true,
            Ok(false) => {
                self.adopted.store(true, Ordering::Relaxed);
                false
            }
            Err(e) => {
                warn!("series folder adoption state: {e}");
                false
            }
        }
    }
}

/// See [`AuthBackend::governing_archives`]: rule v1's listing of the file's library
/// folder (files only), the archives as library-relative keys.
fn governing_archives(layout: &StorageLayout, virtual_path: &str) -> Vec<String> {
    let Some(rel) = paths::library_rel(virtual_path) else {
        return Vec::new();
    };
    let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel.as_str()));
    let Some(names) = bunko_library::sidecar::list_names(&layout.library().join(dir)) else {
        return Vec::new();
    };
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    sidecar_names::governing_archives(name, &refs)
        .into_iter()
        .map(|archive| {
            if dir.is_empty() {
                archive.to_string()
            } else {
                format!("{dir}/{archive}")
            }
        })
        .collect()
}

/// See [`AuthBackend::is_volume_primary`]: rule v1 against the library folder's listing.
fn is_volume_primary(layout: &StorageLayout, virtual_path: &str) -> bool {
    let Some(file) = virtual_to_physical(layout, virtual_path, None) else {
        return false;
    };
    let (Some(dir), Some(name)) = (file.parent(), file.file_name().and_then(|n| n.to_str())) else {
        return false;
    };
    let Some(names) = bunko_library::sidecar::list_names(dir) else {
        return false;
    };
    let stems = sidecar_names::volume_stems(names.iter().map(String::as_str));
    matches!(
        sidecar_names::classify(name, &stems),
        sidecar_names::SidecarClass::Primary { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_user_paths_map_into_the_callers_tree() {
        let layout = StorageLayout::new("/s");
        let alice = Caller {
            username: "alice",
            owns_cloud_root: false,
        };
        let owner = Caller {
            owns_cloud_root: true,
            ..alice
        };
        let map = |p: &str, c| virtual_to_physical(&layout, p, c);
        assert_eq!(
            map("/mokuro-reader/S/v.cbz", None),
            Some(layout.library().join("S/v.cbz"))
        );
        assert_eq!(
            map("/mokuro-reader/goals.json", Some(alice)),
            Some(layout.users().join("alice/goals.json"))
        );
        assert_eq!(
            map("/mokuro-reader/history/d/2026-10.events", Some(alice)),
            Some(layout.users().join("alice/history/d/2026-10.events"))
        );
        assert_eq!(
            map("/mokuro-reader/history/d/2026-10.events", Some(owner)),
            Some(layout.library().join("history/d/2026-10.events"))
        );
        assert_eq!(map("/mokuro-reader/goals.json", None), None);
        assert_eq!(map("/mokuro-reader/history", None), None);
        // Case-sensitive: a series that only folds to a per-user name is the library's.
        assert_eq!(
            map("/mokuro-reader/History/v.cbz", Some(alice)),
            Some(layout.library().join("History/v.cbz"))
        );
        assert_eq!(map("/mokuro-reader/../x", Some(alice)), None);
        assert_eq!(map("/mokuro-readerx/y", Some(alice)), None);
    }

    #[test]
    fn the_listing_names_a_files_governing_archives() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(dir.path());
        let series = layout.library().join("S");
        bunko_store::fs::create_dir_all(series.join("V.edit.cbz")).unwrap();
        for name in ["V.cbz", "V.curated.cbz", "W.cbz"] {
            bunko_store::fs::write(series.join(name), b"x").unwrap();
        }
        bunko_store::fs::write(layout.library().join("loose.cbz"), b"x").unwrap();
        let gov = |p: &str| governing_archives(&layout, p);
        assert_eq!(gov("/mokuro-reader/S/V.curated.mokuro"), ["S/V.cbz"]);
        assert_eq!(gov("/mokuro-reader//S/./V.curated.cbz"), ["S/V.cbz"]);
        assert_eq!(gov("/mokuro-reader/S/W.webp"), ["S/W.cbz"]);
        assert_eq!(gov("/mokuro-reader/loose.mokuro"), ["loose.cbz"]);
        // A folder named like an archive governs nothing (rule 2).
        assert_eq!(
            gov("/mokuro-reader/S/V.edit.cbz/x.cbz"),
            Vec::<String>::new()
        );
        assert!(gov("/mokuro-reader/S/X.cbz").is_empty());
        assert!(gov("/mokuro-reader/T/V.mokuro").is_empty());
        assert!(gov("/mokuro-reader/goals.json").is_empty());
    }

    #[test]
    fn a_record_whose_folder_is_gone_owns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(dir.path());
        bunko_store::fs::create_dir_all(layout.library().join("Dr Stone")).unwrap();
        let db = Arc::new(Database::open(dir.path().join("mokuro.db")).unwrap());
        let backend = DbAuthBackend::new(db.clone(), layout);
        db.record_series_folder_owner("Dr Stone", "alice").unwrap();
        db.record_series_folder_owner("Gone", "alice").unwrap();
        assert_eq!(backend.series_owner("Dr Stone").as_deref(), Some("alice"));
        assert_eq!(backend.series_owner("dr  stone").as_deref(), Some("alice"));
        assert_eq!(backend.series_owner("Gone"), None);
        assert_eq!(backend.series_owner("Nothing"), None);
        assert!(!backend.ownership_pending());
    }

    #[test]
    fn a_dotted_volumes_primary_is_no_layer() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(dir.path());
        let series = layout.library().join("S");
        bunko_store::fs::create_dir_all(&series).unwrap();
        for name in ["V.edit.CBZ", "W.cbz"] {
            bunko_store::fs::write(series.join(name), b"x").unwrap();
        }
        let primary = |p: &str| is_volume_primary(&layout, p);
        assert!(primary("/mokuro-reader/S/V.edit.mokuro"));
        assert!(primary("/mokuro-reader/S/v.EDIT.mokuro.gz"));
        assert!(!primary("/mokuro-reader/S/W.edit.mokuro"));
        assert!(!primary("/mokuro-reader/T/V.edit.mokuro"));
        assert!(!primary("/mokuro-reader/goals.json"));
    }
}
