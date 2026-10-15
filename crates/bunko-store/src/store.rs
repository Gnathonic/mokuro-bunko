//! The [`LibraryStore`] trait and the values it trades in (spec §3.3).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

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
    /// A case merge (spec §5.9) is open on `p` or a folder above it: the OCR scheduler
    /// claims nothing there and the upgrade census skips it until the merge ends.
    fn merge_busy(&self, _p: &Path) -> bool {
        false
    }
    /// Can a Sync write at `p` (a bridge PUT, MOVE, DELETE) reach the store now? `Err`
    /// is the error its publish would fail with (a remote store without the lease,
    /// §9.2: 503), so a client hears it before it sends a body. Default: yes.
    fn writable(&self, _p: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// A store-level failure that has no `io::ErrorKind` of its own, carried inside an
/// `io::Error` of kind `Other` (spec §3.5). [`classify`]-style helpers recognise it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// A precondition failed: the destination changed since its base version, a
    /// create found the name taken, or the parent is gone (bridge: 412).
    Conflict(String),
    /// The store cannot do this now (an outage, another Bunko holds the lease, a
    /// barrier timed out); try again later (bridge: 503).
    Unavailable(String),
    /// [`StoreError::Unavailable`] with the provider's delay: throttling, a transfer
    /// quota, or an interactive call during a longer pause (spec §3.5
    /// `Unavailable{retry_after}`; bridge: 503 with `Retry-After`).
    UnavailableFor { msg: String, retry_after: Duration },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Conflict(m) => write!(f, "conflict: {m}"),
            StoreError::Unavailable(m) => write!(f, "unavailable: {m}"),
            StoreError::UnavailableFor { msg, retry_after } => {
                write!(
                    f,
                    "unavailable for {} s: {msg}",
                    retry_after.as_secs().max(1)
                )
            }
        }
    }
}

impl std::error::Error for StoreError {}

/// An `io::Error` carrying [`StoreError::Conflict`].
pub fn conflict(msg: impl Into<String>) -> io::Error {
    io::Error::other(StoreError::Conflict(msg.into()))
}

/// An `io::Error` carrying [`StoreError::Unavailable`].
pub fn unavailable(msg: impl Into<String>) -> io::Error {
    io::Error::other(StoreError::Unavailable(msg.into()))
}

/// An `io::Error` carrying [`StoreError::UnavailableFor`].
pub fn unavailable_for(msg: impl Into<String>, retry_after: Duration) -> io::Error {
    io::Error::other(StoreError::UnavailableFor {
        msg: msg.into(),
        retry_after,
    })
}

/// How long `e` says to wait before trying again, when it says.
pub fn retry_after(e: &io::Error) -> Option<Duration> {
    match store_error(e) {
        Some(StoreError::UnavailableFor { retry_after, .. }) => Some(*retry_after),
        _ => None,
    }
}

/// The [`StoreError`] inside `e`, if any.
pub fn store_error(e: &io::Error) -> Option<&StoreError> {
    e.get_ref().and_then(|x| x.downcast_ref::<StoreError>())
}

/// Whether `e` is a write conflict (the DAV layer answers 412).
pub fn is_conflict(e: &io::Error) -> bool {
    matches!(store_error(e), Some(StoreError::Conflict(_)))
}

/// Whether `e` means "not now" (the DAV layer answers 503).
pub fn is_unavailable(e: &io::Error) -> bool {
    matches!(
        store_error(e),
        Some(StoreError::Unavailable(_) | StoreError::UnavailableFor { .. })
    )
}

/// The tag of a "this Bunko is still claiming the library lease" refusal (its own
/// settle wait, §9.2), as opposed to `storage-lease` (another Bunko holds it).
pub const LEASE_CLAIMING: &str = "storage-lease-claiming";

/// The message of a store error without its class prefix (`unavailable: `), for a
/// caller that shows it to a client; any other error as it displays.
pub fn error_message(e: &io::Error) -> String {
    match store_error(e) {
        Some(StoreError::Conflict(m) | StoreError::Unavailable(m)) => m.clone(),
        Some(StoreError::UnavailableFor { msg, .. }) => msg.clone(),
        None => e.to_string(),
    }
}

/// What an error means for a caller that answers clients (spec §3.5 table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    NotFound,
    AlreadyExists,
    /// A precondition failed (bridge: 412).
    Conflict,
    /// An outage, throttling or a pause (bridge: 503 + Retry-After).
    Unavailable {
        retry_after: Option<std::time::Duration>,
    },
    /// Expired or revoked credentials (bridge: 503, never 401: the reader would blame
    /// its own login).
    NeedsAuth,
    /// Out of space (bridge: 507).
    StorageFull,
    /// A download quota (bridge: 503 + Retry-After).
    TransferQuota {
        retry_after: Option<std::time::Duration>,
    },
    /// Another Bunko holds the lease (bridge: 503, `storage-lease`).
    LeaseHeld,
    /// This Bunko is still claiming (or confirming) its lease: its own first-claim
    /// settle wait, or the first sync before it (bridge: 503 with `Retry-After` set to
    /// what is left, `storage-lease-claiming`).
    LeaseClaiming {
        retry_after: Option<std::time::Duration>,
    },
    /// A name the provider refuses (bridge: 400).
    InvalidName,
    /// Anything else (a local I/O error).
    Other,
}

/// Recognises a store's own error types inside an `io::Error` (the provider error of
/// a remote store); registered by whoever links the store (the server).
pub type Classifier = fn(&(dyn std::error::Error + Send + Sync + 'static)) -> Option<ErrorClass>;

static CLASSIFIERS: parking_lot::RwLock<Vec<Classifier>> = parking_lot::RwLock::new(Vec::new());

/// Add a [`Classifier`] that [`classify`] consults (idempotent per function).
pub fn register_classifier(f: Classifier) {
    let mut list = CLASSIFIERS.write();
    if !list.iter().any(|g| std::ptr::fn_addr_eq(*g, f)) {
        list.push(f);
    }
}

/// The [`ErrorClass`] of `e` (spec §3.5): a [`StoreError`], a registered store error
/// type, else the `io::ErrorKind`.
pub fn classify(e: &io::Error) -> ErrorClass {
    match store_error(e) {
        Some(StoreError::Conflict(_)) => return ErrorClass::Conflict,
        Some(StoreError::Unavailable(m)) => {
            return if m.contains(LEASE_CLAIMING) {
                ErrorClass::LeaseClaiming { retry_after: None }
            } else if m.contains("storage-lease") {
                ErrorClass::LeaseHeld
            } else {
                ErrorClass::Unavailable { retry_after: None }
            };
        }
        Some(StoreError::UnavailableFor { msg, retry_after }) => {
            return if msg.contains(LEASE_CLAIMING) {
                ErrorClass::LeaseClaiming {
                    retry_after: Some(*retry_after),
                }
            } else if msg.contains("storage-lease") {
                ErrorClass::LeaseHeld
            } else {
                ErrorClass::Unavailable {
                    retry_after: Some(*retry_after),
                }
            };
        }
        None => {}
    }
    if let Some(inner) = e.get_ref() {
        for f in CLASSIFIERS.read().iter() {
            if let Some(c) = f(inner) {
                return c;
            }
        }
    }
    match e.kind() {
        io::ErrorKind::NotFound => ErrorClass::NotFound,
        io::ErrorKind::AlreadyExists => ErrorClass::AlreadyExists,
        io::ErrorKind::StorageFull => ErrorClass::StorageFull,
        io::ErrorKind::NotConnected => ErrorClass::NeedsAuth,
        _ => ErrorClass::Other,
    }
}

/// The error for an operation that spans two stores.
pub fn crosses_devices() -> io::Error {
    io::Error::new(
        io::ErrorKind::CrossesDevices,
        "the paths are in different stores",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct ProviderErr(bool);

    impl std::fmt::Display for ProviderErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("provider")
        }
    }

    impl std::error::Error for ProviderErr {}

    fn provider(e: &(dyn std::error::Error + Send + Sync + 'static)) -> Option<ErrorClass> {
        e.downcast_ref::<ProviderErr>().map(|p| {
            if p.0 {
                ErrorClass::NeedsAuth
            } else {
                ErrorClass::Unavailable {
                    retry_after: Some(std::time::Duration::from_secs(7)),
                }
            }
        })
    }

    #[test]
    fn errors_classify_by_store_error_registered_type_then_kind() {
        assert_eq!(classify(&conflict("x")), ErrorClass::Conflict);
        assert_eq!(
            classify(&unavailable("y")),
            ErrorClass::Unavailable { retry_after: None }
        );
        assert_eq!(
            classify(&unavailable("storage-lease: held by nas")),
            ErrorClass::LeaseHeld
        );
        assert_eq!(
            classify(&unavailable_for(
                "storage-lease-claiming: still claiming",
                Duration::from_secs(90)
            )),
            ErrorClass::LeaseClaiming {
                retry_after: Some(Duration::from_secs(90))
            }
        );
        assert_eq!(
            classify(&unavailable("storage-lease-claiming: starting")),
            ErrorClass::LeaseClaiming { retry_after: None }
        );
        assert_eq!(
            error_message(&unavailable_for(
                "storage-lease-claiming: x",
                Duration::from_secs(9)
            )),
            "storage-lease-claiming: x"
        );
        assert_eq!(
            classify(&io::Error::other(ProviderErr(true))),
            ErrorClass::Other
        );
        register_classifier(provider);
        register_classifier(provider);
        assert_eq!(CLASSIFIERS.read().len(), 1);
        assert_eq!(
            classify(&io::Error::other(ProviderErr(true))),
            ErrorClass::NeedsAuth
        );
        assert_eq!(
            classify(&io::Error::other(ProviderErr(false))),
            ErrorClass::Unavailable {
                retry_after: Some(std::time::Duration::from_secs(7))
            }
        );
        assert_eq!(
            classify(&io::Error::from(io::ErrorKind::StorageFull)),
            ErrorClass::StorageFull
        );
        assert_eq!(
            classify(&io::Error::from(io::ErrorKind::PermissionDenied)),
            ErrorClass::Other
        );
    }
}
