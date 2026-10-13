//! Remote library storage (0.8 design §5, §6).
//!
//! [`backend::Backend`] is the provider seam (WebDAV, Google Drive, OneDrive, MEGA).
//! The `testing` feature adds `fake::FakeBackend`, an in-memory provider with fault
//! injection that the store's tests run on. [`index::Index`] is the disposable
//! `index.sqlite` that maps provider nodes to library paths; [`names`] holds the NFC,
//! fold and effective-mtime rules. Default features add the providers' drivers
//! (`webdav::WebDavBackend`, `gdrive::GDriveBackend`, `onedrive::OneDriveBackend`)
//! and [`auth`], which signs in to the OAuth providers and keeps their credentials.

#[cfg(feature = "oauth")]
pub mod auth;
pub mod backend;
#[cfg(feature = "testing")]
pub mod fake;
#[cfg(feature = "gdrive")]
pub mod gdrive;
pub mod index;
pub mod names;
#[cfg(feature = "onedrive")]
pub mod onedrive;
#[cfg(any(feature = "gdrive", feature = "onedrive"))]
pub mod token_dyn;
#[cfg(feature = "webdav")]
pub mod webdav;

pub use backend::{
    AccountInfo, BErr, Backend, BackendCaps, ChangeItem, ChangePage, Cursor, HashKind, OpCtx, Pid,
    RNode, RemoteBody, ServerTime, UploadSource, UploadTarget,
};
