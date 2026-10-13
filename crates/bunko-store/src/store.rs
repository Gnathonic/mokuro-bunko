//! The [`LibraryStore`] trait and the values it trades in (spec §3.3).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::file::{File, OpenOptions, ReadDir};
use crate::meta::{Meta, Version};

/// What a store can do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    pub remote: bool,
    /// Lookup is byte-exact (PathCase folds at the edge and skips its probe).
    pub case_sensitive: bool,
    pub symlinks: bool,
    /// Files have real local paths (X-Accel, mmap, `local_path` in place).
    pub local_paths: bool,
    pub atomic_replace: bool,
    pub server_copy: bool,
    pub trash_on_delete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    StorageFull,
    TransferQuota,
    DailyLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Online,
    Syncing { done: u64, total: u64 },
    Degraded { retry_at: SystemTime },
    Offline,
    NeedsReauth,
    Blocked(Blocked),
    Suspect,
    LeaseHeld { by: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    pub total: u64,
    pub available: u64,
}

/// How a published file relates to what is at its destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Replace the destination; when `base` is set, only if it is still that version.
    Replace { base: Option<Version> },
    /// The destination must not exist.
    CreateNew,
}

/// Who writes, which decides the conflict handler (spec §5.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Compiler,
    Ocr { claim: String },
    Upgrade,
    Thumbs,
    Status,
    Dav { user: Option<String> },
    Admin,
}

/// Sync: the caller waits until the provider has the bytes and other readers see the
/// old version until then. Deferred: queued durably and visible at once (spec D6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    Sync,
    Deferred,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteIntent {
    pub mode: Mode,
    pub origin: Origin,
    pub durability: Durability,
}

impl WriteIntent {
    pub fn new(mode: Mode, origin: Origin, durability: Durability) -> Self {
        WriteIntent {
            mode,
            origin,
            durability,
        }
    }

    /// What a plain `rename` into a store means (spec §3.4).
    pub fn default_publish() -> Self {
        WriteIntent::new(
            Mode::Replace { base: None },
            Origin::Admin,
            Durability::Sync,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpId(pub u64);

/// The result of a publish: the new version when the store knows it at once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Published {
    pub version: Option<Version>,
    pub sha256: Option<[u8; 32]>,
    pub op: Option<OpId>,
}

/// Keeps a spooled copy of a remote file alive while held.
pub trait SpoolPin: Send + Sync {
    fn path(&self) -> &Path;
}

/// A path a whole-file consumer (mmap, the OCR engines) can open.
pub enum LocalPath {
    InPlace(PathBuf),
    Spooled(Arc<dyn SpoolPin>),
}

impl LocalPath {
    pub fn path(&self) -> &Path {
        match self {
            LocalPath::InPlace(p) => p,
            LocalPath::Spooled(pin) => pin.path(),
        }
    }
}

/// One relevant change in the library tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// `path` appeared (created, or the destination of a move).
    Added { path: PathBuf, is_dir: bool },
    /// `path` vanished (deleted, or the source of a move).
    Removed { path: PathBuf, is_dir: bool },
    /// Events were lost (a kernel queue overflow, a feed resync): rescan.
    Rescan,
}

impl WatchEvent {
    pub fn path(&self) -> Option<&Path> {
        match self {
            WatchEvent::Added { path, .. } | WatchEvent::Removed { path, .. } => Some(path),
            WatchEvent::Rescan => None,
        }
    }
}

/// Watching runs while this value lives.
pub struct WatchGuard(pub Box<dyn Send>);

pub type WatchSink = Arc<dyn Fn(WatchEvent) + Send + Sync>;

/// The storage seam. Synchronous: bunko-library is synchronous by design, and async
/// callers already run filesystem work on the blocking pool.
pub trait LibraryStore: Send + Sync + 'static {
    fn root(&self) -> &Path;
    fn caps(&self) -> Caps;
    fn health(&self) -> Health {
        Health::Online
    }

    // metadata
    fn metadata(&self, p: &Path) -> io::Result<Meta>;
    fn symlink_metadata(&self, p: &Path) -> io::Result<Meta>;
    fn read_dir(&self, p: &Path) -> io::Result<ReadDir>;
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf>;
    fn read_link(&self, p: &Path) -> io::Result<PathBuf>;

    // content
    fn open(&self, p: &Path) -> io::Result<File>;
    fn local_path(&self, p: &Path, expect: Option<&Version>) -> io::Result<LocalPath>;
    fn content_sha256(&self, _p: &Path) -> io::Result<Option<[u8; 32]>> {
        Ok(None)
    }

    // mutation
    fn open_with(&self, p: &Path, o: &OpenOptions) -> io::Result<File>;
    fn publish(&self, staged: &Path, dest: &Path, w: &WriteIntent) -> io::Result<Published>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64>;
    fn remove_file(&self, p: &Path) -> io::Result<()>;
    fn remove_dir(&self, p: &Path) -> io::Result<()>;
    fn remove_dir_all(&self, p: &Path) -> io::Result<()>;
    fn create_dir(&self, p: &Path) -> io::Result<()>;
    fn create_dir_all(&self, p: &Path) -> io::Result<()>;

    // environment
    fn watch(&self, sink: WatchSink) -> io::Result<WatchGuard>;
    fn capacity(&self) -> Option<Capacity> {
        None
    }
    /// A remote path removed within the quiet period (spec §5.4).
    fn recently_removed(&self, _p: &Path) -> Option<SystemTime> {
        None
    }
    /// The spelling PathCase folds a request to when no exact spelling exists
    /// (spec §5.9). `None`: PathCase keeps its own choice.
    fn case_winner(&self, _parent: &Path, _fold: &str) -> Option<String> {
        None
    }
}

/// The error for an operation that spans two stores.
pub fn crosses_devices() -> io::Error {
    io::Error::new(
        io::ErrorKind::CrossesDevices,
        "the paths are in different stores",
    )
}
