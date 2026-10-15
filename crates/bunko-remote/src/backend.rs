//! The provider seam (spec §6): one trait every cloud backend implements, and the
//! values it trades in.
//!
//! The trait uses `impl Future + Send` returns (RPITIT) rather than `async fn` so that a
//! `RemoteStore<B>` can `tokio::spawn` the futures; it is generic, never `dyn`.

use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

/// A provider id. WebDAV: the path relative to the mount URL (a move changes it).
pub type Pid = String;

/// One provider object (file or folder) as the provider lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RNode {
    pub pid: Pid,
    /// `None` only when a root candidate's own parent is unknown.
    pub parent: Option<Pid>,
    /// The provider's name bytes (may be NFD); the index stores the NFC form.
    pub name_raw: Vec<u8>,
    pub is_dir: bool,
    pub size: u64,
    /// The timestamp the reader lists (spec §5.2 table), unix ms.
    pub base_mtime_ms: i64,
    /// Server-assigned creation time, unix ms; 0 when unknown.
    pub created_ms: i64,
    /// Changes on any metadata or content change.
    pub version: String,
    /// Changes on content change only.
    pub cver: String,
    /// A token that pins a read to this version (Drive `headRevisionId`, a strong ETag).
    pub pin: Option<String>,
    pub hash: Option<(HashKind, String)>,
    pub trashed: bool,
}

/// The content hash a provider reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HashKind {
    Sha256,
    Md5,
    QuickXor,
    Sha1,
    NextcloudSha1,
}

impl HashKind {
    /// The stable name stored in the index (`node.hash_kind`).
    pub fn as_str(self) -> &'static str {
        match self {
            HashKind::Sha256 => "sha256",
            HashKind::Md5 => "md5",
            HashKind::QuickXor => "quickxor",
            HashKind::Sha1 => "sha1",
            HashKind::NextcloudSha1 => "nextcloud-sha1",
        }
    }

    /// The inverse of [`HashKind::as_str`].
    pub fn parse(s: &str) -> Option<HashKind> {
        Some(match s {
            "sha256" => HashKind::Sha256,
            "md5" => HashKind::Md5,
            "quickxor" => HashKind::QuickXor,
            "sha1" => HashKind::Sha1,
            "nextcloud-sha1" => HashKind::NextcloudSha1,
            _ => return None,
        })
    }
}

/// A change-feed position, opaque to everything but the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor(pub String);

/// One change-feed item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeItem {
    /// A node was created or changed (a trash is an upsert with `trashed` on providers
    /// that report it that way).
    Upsert(RNode),
    /// A node is gone.
    Removed { pid: Pid },
}

/// One page of the change feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangePage {
    /// `next` is where the following call starts; `more` asks for it now.
    Page {
        items: Vec<ChangeItem>,
        next: Cursor,
        more: bool,
    },
    /// The provider no longer knows the cursor: a full resync is due.
    CursorLost,
}

/// The provider's clock (unix ms from the `Date` header). Lease freshness compares
/// only these (spec §9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ServerTime(pub i64);

/// The signed-in account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountInfo {
    pub id: String,
    pub label: String,
}

/// What a provider can do (spec §6 table, filled by the driver or its probe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendCaps {
    /// Creating a new name fails when it exists (`If-None-Match: *`, conflictBehavior=fail).
    pub cas_create: bool,
    /// A replace honours `if_cver`.
    pub cas_overwrite: bool,
    /// A replace is seen whole or not at all.
    pub atomic_replace: bool,
    pub server_copy: bool,
    pub range: bool,
    /// Reads can be pinned to a version (`RNode::pin`).
    pub pinned_read: bool,
    /// `wait_change` long-polls; otherwise the feed polls.
    pub push: bool,
    pub hash_kind: Option<HashKind>,
    /// G of the effective mtime (D3).
    pub mtime_granularity_ns: i64,
    pub simple_upload_max: u64,
    pub chunk_multiple: u64,
    /// WebDAV: pid == path, so a move changes the pid.
    pub path_ids: bool,
}

/// Where an upload goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadTarget<'a> {
    New {
        parent: &'a str,
        name: &'a str,
    },
    /// A new content version of `pid`; with `if_cver`, only if it is still that version
    /// (where `cas_overwrite`).
    Replace {
        pid: &'a str,
        if_cver: Option<&'a str>,
    },
}

/// The bytes to upload: a local file whose size and sha256 are already known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadSource<'a> {
    File {
        path: &'a Path,
        size: u64,
        sha256: [u8; 32],
    },
}

/// Per-operation context. A resumable upload stores its session in `resume` so the
/// outbox can persist it and continue after a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpCtx {
    pub op_id: String,
    pub resume: Option<String>,
}

/// A read's body.
pub type RemoteBody = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

/// Collect a [`RemoteBody`] into memory.
pub async fn read_to_vec(mut body: RemoteBody) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}

/// Collect a [`RemoteBody`] of at most `max` bytes: a longer body fails as soon as
/// it passes `max` (InvalidData), so a provider that streams more than it listed
/// cannot make Bunko buffer it.
pub async fn read_to_vec_max(mut body: RemoteBody, max: u64) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(max.min(1 << 20) as usize);
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        if out.len() as u64 + chunk.len() as u64 > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("the body is longer than the expected {max} bytes"),
            ));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// A backend failure, classified for the governor and the outbox.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BErr {
    #[error("throttled (retry after {0:?})")]
    Throttled(Option<Duration>),
    #[error("transient error: {0}")]
    Transient(String),
    #[error("authorization expired")]
    AuthExpired,
    #[error("authorization revoked")]
    AuthRevoked,
    #[error("not found")]
    NotFound,
    #[error("precondition failed")]
    PreconditionFailed,
    #[error("name already exists")]
    NameConflict,
    #[error("storage full")]
    StorageFull,
    #[error("transfer quota exceeded (retry after {0:?})")]
    TransferQuota(Option<Duration>),
    /// The provider's daily request limit (Drive `dailyLimitExceeded`, project-wide):
    /// every call pauses (`Health::Blocked(DailyLimit)`).
    #[error("the provider's daily limit is reached (retry after {0:?})")]
    DailyLimit(Option<Duration>),
    #[error("change cursor lost")]
    CursorLost,
    #[error("too large")]
    TooLarge,
    #[error("invalid name")]
    InvalidName,
    #[error("unsupported by this provider")]
    Unsupported,
    #[error("{0}")]
    Fatal(String),
}

/// A cloud provider. Ranges are HTTP-style: `(first, Some(last))` with `last`
/// inclusive, or `(first, None)` to the end.
pub trait Backend: Send + Sync + 'static {
    fn caps(&self) -> BackendCaps;
    /// The governor's limits for this provider (§5.11 Defaults).
    fn limits(&self) -> crate::governor::Limits {
        crate::governor::Limits::unlimited()
    }
    fn account(&self) -> impl Future<Output = Result<AccountInfo, BErr>> + Send;
    /// Every folder named exactly `name` that the reader would use as its root.
    fn locate_roots(&self, name: &str) -> impl Future<Output = Result<Vec<RNode>, BErr>> + Send;
    fn create_root(&self, name: &str) -> impl Future<Output = Result<RNode, BErr>> + Send;
    /// A cursor at the current head of the change feed.
    fn start_cursor(&self) -> impl Future<Output = Result<Cursor, BErr>> + Send;
    /// Every live descendant of `roots` (not the roots themselves), streamed to `sink`.
    fn list_all(
        &self,
        roots: &[Pid],
        sink: &mut (dyn FnMut(RNode) + Send),
    ) -> impl Future<Output = Result<(), BErr>> + Send;
    /// The live children of `dir`.
    fn list_dir(
        &self,
        dir: &str,
        sink: &mut (dyn FnMut(RNode) + Send),
    ) -> impl Future<Output = Result<(), BErr>> + Send;
    /// The live children of `parent` named exactly `name`, and the provider's clock.
    fn find_named(
        &self,
        parent: &str,
        name: &str,
    ) -> impl Future<Output = Result<(Vec<RNode>, ServerTime), BErr>> + Send;
    fn changes(&self, c: &Cursor) -> impl Future<Output = Result<ChangePage, BErr>> + Send;
    /// Push backends: return when the feed has moved past `c`, or after `max`.
    fn wait_change(
        &self,
        c: &Cursor,
        max: Duration,
    ) -> impl Future<Output = Result<(), BErr>> + Send;
    fn stat(&self, pid: &str) -> impl Future<Output = Result<Option<RNode>, BErr>> + Send;
    fn read(
        &self,
        pid: &str,
        pin: Option<&str>,
        range: Option<(u64, Option<u64>)>,
    ) -> impl Future<Output = Result<RemoteBody, BErr>> + Send;
    fn upload(
        &self,
        to: UploadTarget<'_>,
        src: UploadSource<'_>,
        op: &mut OpCtx,
    ) -> impl Future<Output = Result<RNode, BErr>> + Send;
    fn mkdir(
        &self,
        parent: &str,
        name: &str,
        op: &OpCtx,
    ) -> impl Future<Output = Result<RNode, BErr>> + Send;
    /// Move and/or rename.
    fn update(
        &self,
        pid: &str,
        parent: Option<&str>,
        name: Option<&str>,
        if_cver: Option<&str>,
    ) -> impl Future<Output = Result<RNode, BErr>> + Send;
    /// Server-side copy; `Unsupported` where the provider has none.
    fn copy(
        &self,
        pid: &str,
        parent: &str,
        name: &str,
    ) -> impl Future<Output = Result<RNode, BErr>> + Send;
    /// Move to the provider's trash (D16).
    fn trash(
        &self,
        pid: &str,
        if_cver: Option<&str>,
    ) -> impl Future<Output = Result<(), BErr>> + Send;
    /// Permanent delete: Bunko's own ledger versions only (D16).
    fn delete_permanent(&self, pid: &str) -> impl Future<Output = Result<(), BErr>> + Send;
    fn quota(&self) -> impl Future<Output = Result<Option<bunko_store::Capacity>, BErr>> + Send;
}
