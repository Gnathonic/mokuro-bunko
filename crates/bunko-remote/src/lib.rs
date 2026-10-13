//! Remote library storage (0.8 design §5, §6).
//!
//! [`backend::Backend`] is the provider seam (WebDAV, Google Drive, OneDrive, MEGA).
//! The `testing` feature adds `fake::FakeBackend`, an in-memory provider with fault
//! injection that the store's tests run on. [`index::Index`] is the disposable
//! `index.sqlite` that maps provider nodes to library paths; [`names`] holds the NFC,
//! fold and effective-mtime rules; [`feed::Feed`] keeps the index current from the
//! provider's change feed and replays cloud-side changes to the server.
//! [`store::RemoteStore`] puts it all behind `bunko_store::LibraryStore`, with the
//! read caches of [`cache`] and the open files of [`file`](mod@file). Default
//! features add the providers' drivers (`webdav::WebDavBackend`,
//! `gdrive::GDriveBackend`, `onedrive::OneDriveBackend`) and [`auth`], which signs in
//! to the OAuth providers and keeps their credentials.

#[cfg(feature = "oauth")]
pub mod auth;
pub mod backend;
pub mod cache;
#[cfg(feature = "testing")]
pub mod fake;
pub mod feed;
pub mod file;
#[cfg(feature = "gdrive")]
pub mod gdrive;
pub mod index;
pub mod names;
#[cfg(feature = "onedrive")]
pub mod onedrive;
#[cfg(any(feature = "gdrive", feature = "onedrive"))]
pub mod token_dyn;
pub mod store;
#[cfg(feature = "webdav")]
pub mod webdav;

pub use backend::{
    AccountInfo, BErr, Backend, BackendCaps, ChangeItem, ChangePage, Cursor, HashKind, OpCtx, Pid,
    RNode, RemoteBody, ServerTime, UploadSource, UploadTarget,
};
pub use cache::CacheStats;
pub use feed::{
    CloudHooks, EngineState, Feed, FeedConfig, FeedHandle, FeedState, NoFeedState, SubmissionResult,
};
pub use store::{RemoteStore, RemoteStoreConfig, berr_to_io, on_store_runtime};
