//! The library's metadata runtime and the public catalog (0.5.2 `metadata/service.py`
//! timers, `metadata/middleware.py`, `library_index.py`, `middleware/fs_watcher.py`,
//! `catalog/api.py`, `catalog/community.py`; spec metadata-catalog.md).
//!
//! * [`DbMetadataStore`]: `bunko_library`'s [`MetadataStore`](bunko_library::MetadataStore)
//!   over `bunko_db::Database`.
//! * [`LibraryRuntime`]: owns the [`MetadataService`](bunko_library::MetadataService), the
//!   shared [`LibraryIndexCache`](bunko_library::LibraryIndexCache), the debounce timers
//!   (20 s startup pass, 6 h rescan, 10 s debounce capped at 60 s, 5 s retries), the
//!   filesystem watcher and the community fetcher; `stop()` keeps 0.5.2's shutdown order.
//! * [`series_put`]: the `PUT /mokuro-reader/<Series>/series.json` handler the WebDAV
//!   fallback calls after authorisation.
//! * [`router`] (`GET /catalog/api/manifest`, always on) and [`catalog_middleware`]
//!   (every other `/catalog*` path, only while `catalog.enabled`; otherwise the request
//!   falls through exactly as in 0.5.2).
//!
//! Wiring (orchestrator):
//! ```ignore
//! let mut rdeps = RuntimeDeps::new(core.clone(), db.clone());
//! rdeps.locks = Arc::new(ocr::types::DavLocks(dav_write_locks.clone())); // bunko-dav's table
//! rdeps.hooks = LibraryHooks { propfind_refresh, on_published, archive_events };
//! let runtime = LibraryRuntime::new(rdeps);           // inside the tokio runtime
//! runtime.start();
//! server_dav_hooks.add_listener(runtime.clone());     // DavHooks::library_changed
//! let deps = LibraryDeps::new(core, db, runtime.clone()); // + ocr_status/outlook/layer_order
//! modules.push(library::router(deps.clone()));
//! app = app.layer(axum::middleware::from_fn_with_state(deps.clone(), library::catalog_middleware));
//! // in the DAV fallback, after authorize():
//! if library::is_series_put(req.method(), &decoded_path) { return library::series_put(&deps, req, ctx).await; }
//! // accounts: AccountsDeps.library = Some(runtime.counts());
//! // shutdown: runtime.stop().await, before the PROPFIND cache stops
//! ```

mod catalog;
mod community;
mod runtime;
mod series_put;
mod store;
mod util;
mod watcher;

pub use catalog::{catalog_middleware, router};
pub use community::{CommunityFetcher, CommunitySettings, normalize_anilist, normalize_jikan};
pub use runtime::{LibraryHooks, LibraryRuntime, RuntimeDeps};
pub use series_put::{is_series_put, series_put};
pub use store::DbMetadataStore;
pub use watcher::{LibraryWatcher, WatchEvent};

use crate::core::Core;
use bunko_db::Database;
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::Arc;

/// Archive arrivals/removals the filesystem WATCHER saw (out-of-band changes: a copy
/// into the library folder, a sync tool), for the OCR queue. Called on the watcher
/// thread: keep it cheap and non-blocking. A DAV write is seen here too, after the DAV
/// layer reported it through its own hooks; implementations must be idempotent.
pub trait ArchiveEvents: Send + Sync {
    /// A `.cbz` appeared (created, or moved/renamed into place).
    fn archive_added(&self, cbz: &Path);
    /// A `.cbz` disappeared (deleted, or moved/renamed away).
    fn archive_removed(&self, cbz: &Path);
    /// A directory changed or the watcher lost events: re-check the whole library.
    fn rescan(&self) {}
}

/// The live OCR progress document of 0.5.2's `<storage>/.ocr-progress.json`
/// (`{"active": true, "series", "volume", "relative_cbz", "percent", "eta_seconds",
/// "status", "processed_pages", "total_pages", "updated_at", "jobs": [...]}`), which
/// the OCR module implements. Called from blocking context.
pub trait OcrStatusSource: Send + Sync {
    /// The progress object, or `None` when nothing runs. `/catalog/api/ocr-status`
    /// returns it verbatim when its `active` is truthy, else `{"active": false}`.
    fn progress(&self) -> Option<Map<String, Value>>;
}

/// A volume's OCR outlook for its manifest (0.5.2 `control.volume_pending` +
/// `volume_outlook.recheck_after`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VolumeOutlook {
    /// Jobs still to run for this volume (dicts with ISO `eta` strings).
    pub pending: Vec<Value>,
    /// Seconds until the reader should re-fetch the manifest; `None` = `null`.
    pub recheck_after: Option<Value>,
}

/// What the OCR module tells the manifest about a volume. Called from blocking
/// context; may wait briefly (0.5.2 waited up to 1 s for a priced queue).
pub trait OutlookSource: Send + Sync {
    fn volume_outlook(&self, cbz: &Path, series: &str, volume: &str) -> VolumeOutlook;
}

/// The configured non-primary generation names, in order (manifest layer order).
pub type LayerOrder = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// What the catalog routes and the `series.json` PUT handler need.
#[derive(Clone)]
pub struct LibraryDeps {
    pub core: Core,
    pub db: Arc<Database>,
    pub runtime: Arc<LibraryRuntime>,
    /// `/catalog/api/ocr-status` and the series page's OCR badges; `None` = no OCR.
    pub ocr_status: Option<Arc<dyn OcrStatusSource>>,
    /// Manifest `pending` / `recheck_after`; `None` = `[]` / `null`.
    pub outlook: Option<Arc<dyn OutlookSource>>,
    /// Manifest layer order; `None` = alphabetical.
    pub layer_order: Option<LayerOrder>,
}

impl LibraryDeps {
    pub fn new(core: Core, db: Arc<Database>, runtime: Arc<LibraryRuntime>) -> Self {
        Self {
            core,
            db,
            runtime,
            ocr_status: None,
            outlook: None,
            layer_order: None,
        }
    }
}

impl axum::extract::FromRef<LibraryDeps> for Core {
    fn from_ref(d: &LibraryDeps) -> Core {
        d.core.clone()
    }
}
