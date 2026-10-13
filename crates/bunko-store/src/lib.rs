//! The library storage seam (0.8 design §3).
//!
//! Every library filesystem access in bunko-library, bunko-dav and bunko-server goes
//! through [`fs`] (sync) or `afs` (tokio, feature `tokio`). They route a path under a
//! mounted [`LibraryStore`] to that store, and anything else straight to `std::fs`.
//! [`LocalStore`] is today's on-disk library; remote stores implement the same trait.

#[cfg(feature = "tokio")]
pub mod afs;
pub mod file;
pub mod fs;
pub mod latency;
pub mod local;
pub mod meta;
pub mod mount;
pub mod store;
pub mod watch;

pub use file::{DirEntry, File, OpenOptions, ReadDir, RemoteEntry, RemoteFile};
pub use latency::{interactive, is_interactive};
pub use local::LocalStore;
pub use meta::{FileType, Meta, RemoteMeta, Version};
pub use mount::{MountGuard, mount, same_store, store_for};
pub use store::{
    Blocked, Capacity, Caps, Durability, Health, LibraryStore, LocalPath, Mode, OpId, Origin,
    Published, SpoolPin, StoreError, WatchEvent, WatchGuard, WatchSink, WriteIntent, conflict,
    crosses_devices, is_conflict, is_unavailable, retry_after, store_error, unavailable,
    unavailable_for,
};
pub use watch::Translate;
