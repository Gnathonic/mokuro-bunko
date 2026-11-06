//! `AuthBackend` on top of `bunko_db::Database` and the storage layout.

use crate::auth::{AuthBackend, AuthUser, OwnerUnknown, paths};
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
    /// Additions are not held for adoption in this process ([`Self::adoption_waiver`]).
    adoption_waived: Arc<std::sync::atomic::AtomicBool>,
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
            adoption_waived: Default::default(),
        }
    }

    /// Set, it stops holding uploaders' additions while series folder adoption is still
    /// pending, which stays pending: for a cloud library whose store does not run in
    /// this process (F4), so has no listing to adopt from, and whose folders no
    /// addition can reach until a later start adopts them.
    pub fn adoption_waiver(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.adoption_waived.clone()
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

    fn series_owner(&self, series: &str) -> Result<Option<String>, OwnerUnknown> {
        let owner = match self.db.series_folder_owner(series) {
            Ok(Some(owner)) => owner,
            Ok(None) => return Ok(None),
            Err(e) => {
                warn!("series owner lookup failed: {e}");
                return Err(OwnerUnknown);
            }
        };
        match crate::library::owners::series_folder_is_live(&self.layout.library(), series) {
            Ok(live) => Ok(live.then_some(owner)),
            Err(e) => {
                warn!("series owner lookup: the library root cannot be listed: {e}");
                Err(OwnerUnknown)
            }
        }
    }

    fn library_listing(&self, virtual_path: &str) -> Option<Vec<String>> {
        library_listing(&self.layout, virtual_path)
    }

    fn volume_uploader(&self, rel: &str) -> Result<Option<String>, OwnerUnknown> {
        self.db.get_volume_owner(rel).map_err(|e| {
            warn!("volume owner lookup failed: {e}");
            OwnerUnknown
        })
    }

    fn is_library_folder(&self, virtual_path: &str) -> bool {
        paths::library_rel(virtual_path)
            .is_some_and(|rel| bunko_store::fs::is_dir(self.layout.library().join(rel)))
    }

    fn ownership_pending(&self) -> Result<bool, OwnerUnknown> {
        use std::sync::atomic::Ordering;
        if self.adopted.load(Ordering::Relaxed) || self.adoption_waived.load(Ordering::Relaxed) {
            return Ok(false);
        }
        match self.db.series_folder_adoption_pending() {
            Ok(true) => Ok(true),
            Ok(false) => {
                self.adopted.store(true, Ordering::Relaxed);
                Ok(false)
            }
            Err(e) => {
                warn!("series folder adoption state: {e}");
                Err(OwnerUnknown)
            }
        }
    }
}

/// See [`AuthBackend::library_listing`]: the files of the library folder that holds
/// the path, as rule v1 lists them (no folders); empty when that folder is not there
/// (a PUT into it answers 409 later), `None` when it cannot be read.
fn library_listing(layout: &StorageLayout, virtual_path: &str) -> Option<Vec<String>> {
    let Some(rel) = paths::library_rel(virtual_path) else {
        return Some(Vec::new());
    };
    let dir = rel.rsplit_once('/').map_or("", |(dir, _)| dir);
    let entries = match bunko_store::fs::read_dir(layout.library().join(dir)) {
        Ok(entries) => entries,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Some(Vec::new());
        }
        Err(e) => {
            warn!("library listing of {dir:?} failed: {e}");
            return None;
        }
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.ok()?;
        if entry.file_type().ok()?.is_dir() {
            continue;
        }
        if let Ok(name) = entry.file_name().into_string() {
            names.push(name);
        }
    }
    names.sort();
    Some(names)
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
    fn the_listing_is_the_files_of_the_paths_folder() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(dir.path());
        let series = layout.library().join("S");
        bunko_store::fs::create_dir_all(series.join("V.edit.cbz")).unwrap();
        for name in ["V.cbz", "V.curated.cbz", "W.cbz"] {
            bunko_store::fs::write(series.join(name), b"x").unwrap();
        }
        bunko_store::fs::write(layout.library().join("loose.cbz"), b"x").unwrap();
        let list = |p: &str| library_listing(&layout, p);
        let s = Some(vec![
            "V.cbz".to_string(),
            "V.curated.cbz".into(),
            "W.cbz".into(),
        ]);
        assert_eq!(list("/mokuro-reader/S/V.curated.mokuro"), s);
        assert_eq!(list("/mokuro-reader//S/./X.cbz"), s);
        assert!(
            list("/mokuro-reader/loose.mokuro")
                .unwrap()
                .contains(&"loose.cbz".to_string())
        );
        // A folder named like an archive is no file (rule 2), and lists nothing.
        assert_eq!(list("/mokuro-reader/S/V.edit.cbz/x.cbz"), Some(vec![]));
        // A folder that is not there, or is a file, lists nothing.
        assert_eq!(list("/mokuro-reader/T/V.mokuro"), Some(vec![]));
        assert_eq!(list("/mokuro-reader/S/V.cbz/x.mokuro"), Some(vec![]));
        assert_eq!(list("/mokuro-reader/goals.json.x/y"), Some(vec![]));
    }

    /// A folder that cannot be read is no folder that governs nothing (OWN-8).
    #[cfg(unix)]
    #[test]
    fn an_unreadable_folder_has_no_listing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(dir.path());
        let series = layout.library().join("S");
        bunko_store::fs::create_dir_all(&series).unwrap();
        bunko_store::fs::write(series.join("V.cbz"), b"x").unwrap();
        std::fs::set_permissions(&series, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = bunko_store::fs::read_dir(&series).is_ok(); // root reads anything
        let listed = library_listing(&layout, "/mokuro-reader/S/V.curated.cbz");
        std::fs::set_permissions(&series, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !readable {
            assert_eq!(listed, None);
        }
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
        assert_eq!(backend.series_owner("Dr Stone"), Ok(Some("alice".into())));
        assert_eq!(backend.series_owner("dr  stone"), Ok(Some("alice".into())));
        assert_eq!(backend.series_owner("Gone"), Ok(None));
        assert_eq!(backend.series_owner("Nothing"), Ok(None));
        assert_eq!(backend.ownership_pending(), Ok(false));
    }

    fn uploader(name: &str) -> crate::auth::Identity {
        crate::auth::Identity {
            user: Some(AuthUser {
                id: 7,
                username: name.into(),
                role: bunko_core::Role::Uploader,
            }),
            ..Default::default()
        }
    }

    /// `(status, Retry-After)` of an uploader's request, or `None` when it is allowed.
    fn decide(
        backend: &DbAuthBackend,
        method: &str,
        path: &str,
        user: &str,
    ) -> Option<(u16, Option<u64>)> {
        let anon = crate::auth::AnonymousAccess {
            browse: true,
            download: true,
        };
        let m = http::Method::from_bytes(method.as_bytes()).unwrap();
        crate::auth::authorize(&m, path, None, &uploader(user), anon, backend)
            .err()
            .map(|d| (d.status.as_u16(), d.retry_after))
    }

    /// A library with `S/` owned by alice, holding her `S/V.cbz`.
    fn owned_library() -> (tempfile::TempDir, Arc<Database>, DbAuthBackend) {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(dir.path());
        bunko_store::fs::create_dir_all(layout.library().join("S")).unwrap();
        bunko_store::fs::write(layout.library().join("S/V.cbz"), b"x").unwrap();
        let db = Arc::new(Database::open(dir.path().join("mokuro.db")).unwrap());
        db.record_series_folder_owner("S", "alice").unwrap();
        db.record_volume_upload("S/V.cbz", "alice").unwrap();
        let backend = DbAuthBackend::new(db.clone(), layout);
        (dir, db, backend)
    }

    fn drop_table(db: &Database, table: &str) {
        db.with_writer_connection(|c| c.execute(&format!("DROP TABLE {table}"), []))
            .unwrap();
    }

    /// An owner lookup that fails is no "unowned": the addition waits (503), it is never
    /// let into the folder.
    #[test]
    fn a_failed_owner_lookup_holds_an_addition() {
        let (_dir, db, backend) = owned_library();
        assert_eq!(
            decide(&backend, "PUT", "/mokuro-reader/S/W.cbz", "bob"),
            Some((403, None))
        );
        drop_table(&db, "series_folder_owners");
        for (method, path) in [
            ("PUT", "/mokuro-reader/S/W.cbz"),
            ("MKCOL", "/mokuro-reader/S/Sub"),
            ("PUT", "/mokuro-reader/S/series.json"),
        ] {
            assert_eq!(
                decide(&backend, method, path, "bob"),
                Some((503, Some(10))),
                "{method} {path}"
            );
        }
    }

    /// Adoption state that cannot be read is no "adoption done".
    #[test]
    fn a_failed_adoption_lookup_holds_an_addition() {
        let (_dir, db, backend) = owned_library();
        drop_table(&db, "series_folder_adoption");
        assert_eq!(
            decide(&backend, "PUT", "/mokuro-reader/S/W.cbz", "alice"),
            Some((503, Some(10)))
        );
    }

    /// An uploader lookup that fails says "not now", not "not yours".
    #[test]
    fn a_failed_uploader_lookup_is_no_refusal() {
        let (_dir, db, backend) = owned_library();
        drop_table(&db, "series_folder_owners");
        drop_table(&db, "volume_uploads");
        assert_eq!(
            decide(&backend, "DELETE", "/mokuro-reader/S/V.cbz", "alice"),
            Some((503, Some(10)))
        );
    }

    /// A library root that cannot be listed cannot tell whether another spelling of an
    /// owned series is live: its owner is unknown, never "none" (an addition through a
    /// spelling the store folds onto `S/` would otherwise pass).
    #[cfg(unix)]
    #[test]
    fn an_unlistable_root_leaves_the_owner_unknown() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, db, backend) = owned_library();
        let root = backend.layout.library();
        bunko_store::fs::create_dir_all(root.join("Dr Stone")).unwrap();
        db.record_series_folder_owner("Dr Stone", "alice").unwrap();
        // Searchable (`S/` is still reached by name) but not listable.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o311)).unwrap();
        let listable = bunko_store::fs::read_dir(&root).is_ok(); // root reads anything
        // A case-insensitive filesystem (APFS, NTFS) reaches `S/` by the name `s`.
        let folds_case = bunko_store::fs::is_dir(root.join("s"));
        // No filesystem folds a run of spaces: only the root listing would find this.
        let variant = decide(&backend, "PUT", "/mokuro-reader/Dr  Stone/W.cbz", "bob");
        let case_only = decide(&backend, "PUT", "/mokuro-reader/s/W.cbz", "bob");
        let named = decide(&backend, "PUT", "/mokuro-reader/S/W.cbz", "bob");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !listable {
            assert_eq!(variant, Some((503, Some(10))));
            // Reached by name it is `S/` itself, alice's; otherwise only listing tells.
            let case_only_expected = if folds_case {
                (403, None)
            } else {
                (503, Some(10))
            };
            assert_eq!(case_only, Some(case_only_expected));
            assert_eq!(named, Some((403, None)));
        }
    }

    /// F4: a waived adoption holds nothing back, and stays pending for a later start.
    #[test]
    fn a_waived_adoption_holds_no_addition() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(dir.path());
        let db = Arc::new(Database::open(dir.path().join("mokuro.db")).unwrap());
        db.with_writer_connection(|c| c.execute("DELETE FROM series_folder_adoption", []))
            .unwrap();
        let backend = DbAuthBackend::new(db.clone(), layout);
        assert_eq!(backend.ownership_pending(), Ok(true));
        backend
            .adoption_waiver()
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(backend.ownership_pending(), Ok(false));
        assert!(db.series_folder_adoption_pending().unwrap());
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
