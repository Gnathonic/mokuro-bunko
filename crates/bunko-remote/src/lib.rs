//! Remote library storage (0.8 design §5, §6).
//!
//! [`backend::Backend`] is the provider seam (WebDAV, Google Drive, OneDrive, MEGA).
//! The `testing` feature adds `fake::FakeBackend`, an in-memory provider with fault
//! injection that the store's tests run on. [`index::Index`] is the disposable
//! `index.sqlite` that maps provider nodes to library paths; [`names`] holds the NFC,
//! fold and effective-mtime rules.

pub mod backend;
#[cfg(feature = "testing")]
pub mod fake;
pub mod index;
pub mod names;

pub use backend::{
    AccountInfo, BErr, Backend, BackendCaps, ChangeItem, ChangePage, Cursor, HashKind, OpCtx, Pid,
    RNode, RemoteBody, ServerTime, UploadSource, UploadTarget,
};
