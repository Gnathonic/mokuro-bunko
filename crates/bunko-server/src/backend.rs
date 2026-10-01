//! `AuthBackend` on top of `bunko_db::Database` and the storage layout.

use crate::auth::{AuthBackend, AuthUser, paths};
use bunko_core::StorageLayout;
use bunko_db::Database;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::warn;

pub struct DbAuthBackend {
    pub db: Arc<Database>,
    pub layout: StorageLayout,
    logins: LoginCache,
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
        username: Option<&str>,
    ) -> Option<PathBuf> {
        let rel = virtual_path
            .trim_start_matches('/')
            .strip_prefix(paths::READER_ROOT)?;
        let rel = rel.trim_start_matches('/');
        if paths::PER_USER_FILES.contains(&rel) {
            return Some(self.layout.users().join(username?).join(rel));
        }
        if rel.split('/').any(|s| s == "..") {
            return None;
        }
        Some(self.layout.library().join(rel))
    }
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

    fn can_user_delete_library_path(&self, username: &str, virtual_path: &str) -> bool {
        self.db
            .can_user_delete_library_path(username, virtual_path)
            .unwrap_or(false)
    }

    fn can_user_edit_series(&self, username: &str, series_title: &str) -> bool {
        self.db
            .can_user_edit_series(username, series_title)
            .unwrap_or(false)
    }

    fn physical_exists(&self, virtual_path: &str, username: Option<&str>) -> bool {
        self.virtual_to_physical(virtual_path, username)
            .is_some_and(|p| p.exists())
    }
}
