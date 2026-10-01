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
}

fn to_auth(u: bunko_db::User) -> AuthUser {
    AuthUser { id: u.id, username: u.username, role: u.role }
}

impl DbAuthBackend {
    /// Physical path of a virtual DAV path (0.5.2 `PathMapper.virtual_to_physical`), without
    /// resolving symlinks.
    pub fn virtual_to_physical(&self, virtual_path: &str, username: Option<&str>) -> Option<PathBuf> {
        let rel = virtual_path.trim_start_matches('/').strip_prefix(paths::READER_ROOT)?;
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

    fn check_password(&self, username: &str, password: &str) -> Option<AuthUser> {
        match self.db.authenticate_user(username, password) {
            Ok(u) => u.map(to_auth),
            Err(e) => {
                warn!("password check failed: {e}");
                None
            }
        }
    }

    fn can_user_delete_library_path(&self, username: &str, virtual_path: &str) -> bool {
        self.db.can_user_delete_library_path(username, virtual_path).unwrap_or(false)
    }

    fn can_user_edit_series(&self, username: &str, series_title: &str) -> bool {
        self.db.can_user_edit_series(username, series_title).unwrap_or(false)
    }

    fn physical_exists(&self, virtual_path: &str, username: Option<&str>) -> bool {
        self.virtual_to_physical(virtual_path, username).is_some_and(|p| p.exists())
    }
}
