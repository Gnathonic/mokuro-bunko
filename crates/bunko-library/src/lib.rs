//! `bunko-library`: everything mokuro-bunko knows about the library tree on
//! disk, ported byte-for-byte from 0.5.2.
//!
//! * [`archive`] — volume archives (`.cbz`/`.zip`, image directories): page
//!   lists in 0.5.2's order and streamed page reads.
//! * [`sidecar`] — `.mokuro` / layer / cover file naming and sidecar loading.
//! * [`compiler`], [`schema`], [`update`], [`service`] — the metadata compiler
//!   (`<Series>/series.json`, `catalog.json`), client fact updates and the
//!   `MetadataService` the server drives; database access through the
//!   [`store::MetadataStore`] trait.
//! * [`index`] — the cached library index (series, volumes, pending OCR).
//! * [`manifest`] — the per-volume reader manifest document.
//! * [`paths`] — which virtual paths are compiled metadata; watcher routing.
//! * [`compat`], [`pyjson`], [`pyunicode`], [`isodate`] — the Python/reader
//!   semantics everything above depends on (natural sort, folding keys,
//!   placeholder uuids, `json` reading/writing, float `repr`, ISO stamps).
//!
//! Synchronous by design: async callers use `spawn_blocking`.

pub mod archive;
pub mod compat;
pub mod compiler;
pub mod fsutil;
pub mod index;
pub mod isodate;
pub mod manifest;
pub mod paths;
pub mod pyjson;
pub mod pyunicode;
pub mod schema;
pub mod service;
pub mod sidecar;
pub mod store;
pub mod update;

pub use archive::{ArchiveError, Page, Volume};
pub use compiler::SeriesFolder;
pub use index::{LibraryIndexCache, LibrarySnapshot, SeriesSnapshot, VolumeSnapshot};
pub use schema::{SeriesFacts, SeriesIndexData, VolumeEntry};
pub use service::{
    DebouncePolicy, Debouncer, MetadataHooks, MetadataService, PathWriteLocks, UpdateError,
};
pub use store::{
    CachedEntryRow, CachedEntryWrite, CatalogSeriesRow, MetadataStore, SeriesFactsRow, StoreError,
};
