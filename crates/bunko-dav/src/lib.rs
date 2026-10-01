//! The mokuro-bunko WebDAV layer (0.5.2 `webdav/`, `middleware/upload.py`,
//! `middleware/propfind_cache.py` and the WsgiDAV 4.3.5 behaviour they inherited; spec
//! `docs/rust-port/spec/http-webdav.md` §8–§10).
//!
//! The server authenticates and authorises a request (spec §4), then hands it to
//! [`Dav::handle`] with a [`DavContext`] naming who is asking. Everything below that is
//! here: the virtual tree (shared library + per-user progress files), every DAV method,
//! conditional requests and ranges, staged/verified/atomic uploads with their JSON
//! verdicts, per-path write locks, the in-memory DAV lock table, and the `Depth:
//! infinity` PROPFIND cache. Side effects the rest of the server cares about (ownership,
//! audit, OCR queue) go out through [`DavHooks`], after each write commits.
//!
//! Unlike the other library crates this one is async (tokio): it streams request and
//! response bodies. Filesystem work runs in `spawn_blocking`.

mod cache;
mod conditional;
mod hooks;
mod locks;
mod methods;
mod paths;
mod propfind;
mod resource;
mod response;
mod sidecars;
mod upload;
mod write_locks;
mod xml;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use bunko_core::{Role, StorageLayout};
use http::{HeaderMap, Request, Response, StatusCode};

pub use cache::{CacheConfig, PropfindCache};
pub use hooks::{AuditEvent, DavHooks, NoHooks, PutFollowUp};
pub use upload::{DamageMemory, DigestAlgo, parse_content_digest};
pub use write_locks::{PathWriteLocks, WriteLockGuard};

use crate::locks::LockManager;
use crate::propfind::DeadProps;
use crate::resource::Roots;

/// Tunables of the DAV layer.
#[derive(Debug, Clone, Default)]
pub struct DavConfig {
    pub propfind_cache: CacheConfig,
}

/// Who is asking, as the server's auth layer decided (spec §2.3).
#[derive(Clone)]
pub struct DavContext {
    /// The authenticated username (`None` = anonymous). Per-user files map to
    /// `users/<username>/`.
    pub username: Option<String>,
    pub role: Role,
    /// `MOKURO_NGINX_ACCEL=1`: answer library GET/HEAD with `X-Accel-Redirect` (§8.10).
    pub nginx_accel: bool,
    /// The resolved client IP (logging only).
    pub client_ip: Option<String>,
    pub hooks: Arc<dyn DavHooks>,
}

impl DavContext {
    pub fn anonymous(hooks: Arc<dyn DavHooks>) -> Self {
        Self {
            username: None,
            role: Role::Anonymous,
            nginx_accel: false,
            client_ip: None,
            hooks,
        }
    }

    pub fn user(username: impl Into<String>, role: Role, hooks: Arc<dyn DavHooks>) -> Self {
        Self {
            username: Some(username.into()),
            role,
            nginx_accel: false,
            client_ip: None,
            hooks,
        }
    }

    pub(crate) fn principal(&self) -> &str {
        self.username.as_deref().unwrap_or("")
    }
}

pub(crate) struct Inner {
    pub roots: Roots,
    pub write_locks: PathWriteLocks,
    pub locks: LockManager,
    pub dead: DeadProps,
    pub damage: DamageMemory,
    pub cache: Arc<PropfindCache>,
}

/// The WebDAV handler. Cheap to clone; build one per server.
#[derive(Clone)]
pub struct Dav {
    inner: Arc<Inner>,
}

#[derive(Debug, thiserror::Error)]
pub enum DavInitError {
    #[error("cannot prepare the storage directories: {0}")]
    Storage(#[from] std::io::Error),
}

impl Dav {
    /// Build the handler over `layout` (creates `library/`, `inbox/`, `users/` and
    /// `library/thumbnails/` like 0.5.2). Call inside a tokio runtime so the PROPFIND cache
    /// can schedule background refreshes.
    pub fn new(layout: &StorageLayout, config: DavConfig) -> Result<Dav, DavInitError> {
        layout.ensure_directories()?;
        let roots = Roots {
            library: std::fs::canonicalize(layout.library())?,
            users: std::fs::canonicalize(layout.users())?,
        };
        let cache = PropfindCache::new(config.propfind_cache, roots.clone());
        Ok(Dav {
            inner: Arc::new(Inner {
                roots,
                write_locks: PathWriteLocks::new(),
                locks: LockManager::new(),
                dead: DeadProps::default(),
                damage: DamageMemory::default(),
                cache,
            }),
        })
    }

    /// Answer one DAV request. The caller has already authenticated and authorised it.
    pub async fn handle(&self, req: Request<Body>, ctx: DavContext) -> Response<Body> {
        methods::dispatch(self.inner.clone(), req, ctx).await
    }

    /// The PROPFIND cache (`schedule_refresh` for the filesystem watcher and the metadata
    /// publisher, `warm` at start-up, `stop` at shutdown).
    pub fn propfind_cache(&self) -> &Arc<PropfindCache> {
        &self.inner.cache
    }

    /// The per-path write-lock registry shared with non-DAV writers (the metadata compiler
    /// takes it around `series.json`/`catalog.json` writes, spec §8.7).
    pub fn write_locks(&self) -> &PathWriteLocks {
        &self.inner.write_locks
    }

    /// The physical file a virtual path names for `username` (existing or not), or
    /// `None` for virtual/unmapped paths and traversal. For the auth layer's "does this PUT
    /// replace an existing file" check (spec §4.4). Blocking (resolves symlinks).
    pub fn physical_path(&self, virtual_path: &str, username: Option<&str>) -> Option<PathBuf> {
        match paths::classify(virtual_path) {
            paths::Target::Progress(name) => self.inner.roots.user_file(username?, name),
            paths::Target::Library(rel) => paths::resolve_under(&self.inner.roots.library, &rel),
            _ => None,
        }
    }

    /// Stop background work (shutdown).
    pub fn shutdown(&self) {
        self.inner.cache.stop();
    }

    /// Drop cached listings touching these virtual paths (for writers outside DAV).
    pub fn invalidate_listing(&self, virtual_paths: &[String]) {
        self.inner.cache.invalidate_paths(virtual_paths);
    }

    /// Schedule a debounced refresh of every cached listing.
    pub fn schedule_listing_refresh(&self, delay: Duration) {
        self.inner.cache.schedule_refresh(delay);
    }
}

/// Is `path` (raw request path) one of the WebDAV paths (0.5.2 `cors.is_dav_path`)?
pub fn is_dav_path(path: &str) -> bool {
    matches!(path, "" | "/" | "/mokuro-reader" | "/inbox")
        || path.starts_with("/mokuro-reader/")
        || path.starts_with("/inbox/")
}

/// 0.5.2's compiled-metadata predicate (`catalog.json` at the library root,
/// `<Series>/series.json`), for the auth gate and the verdict rule.
pub fn is_compiled_metadata_path(path: &str) -> bool {
    paths::is_compiled_metadata_path(path)
}

/// Does a PUT to this (decoded) path end in an upload verdict (spec §9)?
pub fn put_gives_verdict(path: &str) -> bool {
    (path.starts_with("/mokuro-reader/") || path.starts_with("/inbox/"))
        && !paths::is_compiled_metadata_path(path)
}

/// Turn a refusal the server produced itself for a PUT (401/403/429 from auth, 413 from
/// a body limit, ...) into the JSON verdict a reader expects (spec §9; in 0.5.2 the upload
/// middleware wrapped the auth layer). Other headers (`WWW-Authenticate`) are kept. A
/// response for a path without a verdict, or a success, is returned unchanged; a `.cbz`
/// PUT always gets `X-Mokuro-Put: verified`.
pub fn put_refusal_verdict(path: &str, resp: Response<Body>) -> Response<Body> {
    methods::put::refusal_verdict(path, resp)
}

/// The kind of library change a filesystem event is (0.5.2 `fs_watcher.classify_change`,
/// spec §12.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryChange {
    /// A top-level entry, the root itself, or a path outside the library.
    Library,
    /// Inside the series folder of this name.
    Series(String),
    /// Under `thumbnails/`.
    Ignore,
}

pub fn classify_change(library_root: &Path, path: &Path) -> LibraryChange {
    let Ok(rel) = path.strip_prefix(library_root) else {
        return LibraryChange::Library;
    };
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.len() < 2 {
        return LibraryChange::Library;
    }
    if parts[0] == "thumbnails" {
        return LibraryChange::Ignore;
    }
    LibraryChange::Series(parts[0].clone())
}

/// Whether a watcher event on `path` matters (0.5.2 `_is_relevant`): any directory, or a
/// file with suffix `.cbz`, `.mokuro`, `.gz`, `.webp` (incl. `.mokuro.gz`).
pub fn is_relevant_change(path: &Path, is_dir: bool) -> bool {
    if is_dir {
        return true;
    }
    let name = paths::file_name(path);
    matches!(
        paths::py_suffix(&name),
        ".cbz" | ".mokuro" | ".gz" | ".webp"
    ) || name.ends_with(".mokuro.gz")
}

pub(crate) fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

pub(crate) const SERVER_ERROR: StatusCode = StatusCode::INTERNAL_SERVER_ERROR;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_classification() {
        let root = Path::new("/lib");
        assert_eq!(
            classify_change(root, Path::new("/lib")),
            LibraryChange::Library
        );
        assert_eq!(
            classify_change(root, Path::new("/lib/x.cbz")),
            LibraryChange::Library
        );
        assert_eq!(
            classify_change(root, Path::new("/lib/thumbnails/a.webp")),
            LibraryChange::Ignore
        );
        assert_eq!(
            classify_change(root, Path::new("/lib/S/v.cbz")),
            LibraryChange::Series("S".into())
        );
        assert_eq!(
            classify_change(root, Path::new("/elsewhere/S/v.cbz")),
            LibraryChange::Library
        );
        assert!(is_relevant_change(Path::new("/lib/S/v.mokuro.gz"), false));
        assert!(is_relevant_change(Path::new("/lib/S/v.webp"), false));
        assert!(!is_relevant_change(Path::new("/lib/S/series.json"), false));
        assert!(!is_relevant_change(
            Path::new("/lib/S/.v.cbz.upload-1.tmp"),
            false
        ));
        assert!(is_relevant_change(Path::new("/lib/S"), true));
    }

    #[test]
    fn dav_paths() {
        for p in [
            "",
            "/",
            "/mokuro-reader",
            "/mokuro-reader/",
            "/inbox",
            "/inbox/x",
        ] {
            assert!(is_dav_path(p), "{p}");
        }
        assert!(!is_dav_path("/catalog/api/manifest"));
        assert!(put_gives_verdict("/mokuro-reader/S/v.cbz"));
        assert!(!put_gives_verdict("/mokuro-reader/S/series.json"));
    }
}
