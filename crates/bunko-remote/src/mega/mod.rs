//! The MEGA driver (spec §5.3, §5.6, §5.7, §5.11, §6, §7) on `bunko-mega`.
//!
//! - **Ids and versions.** pid = node handle. A new content version is a new node
//!   (a new handle), so `cver` = `pin` = the handle and reads are pinned inherently;
//!   `version` = `handle:ts`; there is no content hash.
//! - **Tree.** MEGA has no per-folder listing: the whole tree comes in one `f` answer.
//!   The driver spools it to a temporary file in `state_dir` and scans it twice: the
//!   first pass keeps only the structure (handle, parent, kind) of every node and
//!   decrypts folder names to find the roots; the second decrypts the nodes inside the
//!   library subtree. That *mirror* answers `stat`, `list_dir` and `find_named` after a
//!   drain of the `sc` feed, and one fetch serves a whole FULL_SYNC
//!   (`locate_roots` → `start_cursor` → `list_all`) while it is younger than
//!   [`MegaConfig::snapshot_ttl`].
//! - **Feed.** cursor = `sn`. Moves fold (`d` + `t` of one handle); a node leaving the
//!   subtree is `Removed` with its decrypted descendants, children first. An
//!   unmodelled packet re-lists the subtree (a fresh fetch diffed against the
//!   mirror) when the store's cursor is the mirror's, and is `CursorLost` otherwise;
//!   a stale `sn` and any other `sc` error are `CursorLost` (spec §5.3).
//! - **Writes.** Replace uploads a new node under the final name and returns it; the
//!   store removes the old handle (SUPERSEDE_OLD, §5.7). Uploads are chunked and
//!   resume from [`OpCtx::resume`] (URL, key, acknowledged offset, chunk MACs); an
//!   uncertain completion is resolved by matching name, size and fingerprint in the
//!   parent. `trash` moves to the rubbish bin; `copy` is unsupported.
//! - **Auth.** [`MegaSession::login`] once (email, password, optional 2FA) gives a
//!   [`SessionBlob`]; the password is never stored. `-15` (ESID) is `AuthRevoked`
//!   (NeedsReauth); the driver never logs in again by itself.
//! - **Limits** (spec §5.11): one `cs` batch per second and two transfers, in the
//!   client. `-17` on a download is `TransferQuota(30 min)`; on an upload it is
//!   `StorageFull`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bunko_mega::{
    Client, ClientConfig, Error as MErr, ErrorCode, Event, FileNode, Node, NodeAttributes,
    NodeKind, Poll, Session, Snapshot, UploadState, file_fingerprint, idempotence_id,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::backend::{
    AccountInfo, BErr, Backend, BackendCaps, ChangeItem, ChangePage, Cursor, OpCtx, Pid, RNode,
    RemoteBody, ServerTime, UploadSource, UploadTarget,
};

/// How long one fetched tree serves `locate_roots`/`start_cursor`/`list_all`.
pub const DEFAULT_SNAPSHOT_TTL: Duration = Duration::from_secs(120);
/// The pause the governor gets for `-17` / HTTP 509 when MEGA does not say (§5.11).
pub const TRANSFER_QUOTA_PAUSE: Duration = Duration::from_secs(30 * 60);
/// MEGA's MAC chunk: uploads go up in whole multiples of it.
pub const CHUNK: u64 = 128 << 10;
const MAX_DEPTH: usize = 4096;
const GRAVEYARD_MAX: usize = 10_000;

/// Driver settings.
#[derive(Debug, Clone)]
pub struct MegaConfig {
    pub client: ClientConfig,
    /// The store's state folder; tree fetches are spooled here.
    pub state_dir: PathBuf,
    pub snapshot_ttl: Duration,
    /// Largest upload POST (whole MAC chunks).
    pub upload_post_max: u64,
    /// Failed chunk sends one `upload` call retries before giving the error to the
    /// outbox (which resumes from `OpCtx::resume`).
    pub upload_retries: u32,
}

impl MegaConfig {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        MegaConfig {
            client: ClientConfig::default(),
            state_dir: state_dir.into(),
            snapshot_ttl: DEFAULT_SNAPSHOT_TTL,
            upload_post_max: bunko_mega::DEFAULT_POST_MAX,
            upload_retries: 3,
        }
    }
}

/// A serialised MEGA session: password-equivalent together with MEGA's servers.
/// Keep it in the credential store (`StoredCredential::mega_session`), never in
/// config or logs; `Debug` prints nothing of it.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionBlob(String);

impl SessionBlob {
    pub fn new(blob: impl Into<String>) -> Self {
        SessionBlob(blob.into())
    }

    /// The blob, to store.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for SessionBlob {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

impl std::fmt::Debug for SessionBlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionBlob(<redacted>)")
    }
}

/// Why a login gave no session.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoginError {
    #[error("wrong email or password")]
    BadCredentials,
    #[error("this account needs its two-factor code")]
    MfaRequired,
    #[error("MEGA has blocked this account")]
    Blocked,
    #[error("MEGA is limiting logins; wait before trying again")]
    Throttled,
    #[error("could not reach MEGA: {0}")]
    Network(String),
    #[error("login failed: {0}")]
    Other(String),
}

/// The one-time sign-in (spec §7).
pub struct MegaSession;

impl MegaSession {
    /// Log in to MEGA and serialise the session. The caller stores the blob; the
    /// password is not kept anywhere.
    pub async fn login(
        email: &str,
        password: &str,
        totp: Option<String>,
    ) -> Result<SessionBlob, LoginError> {
        Self::login_with(ClientConfig::default(), email, password, totp.as_deref()).await
    }

    pub async fn login_with(
        cfg: ClientConfig,
        email: &str,
        password: &str,
        totp: Option<&str>,
    ) -> Result<SessionBlob, LoginError> {
        let client = Client::login(cfg, email, password, totp)
            .await
            .map_err(|e| match e {
                MErr::Api(ErrorCode::ENOENT | ErrorCode::EARGS) => LoginError::BadCredentials,
                MErr::Crypto(_) => LoginError::BadCredentials,
                MErr::Api(ErrorCode::EMFAREQUIRED) => LoginError::MfaRequired,
                MErr::Api(ErrorCode::EBLOCKED) => LoginError::Blocked,
                MErr::Api(
                    ErrorCode::ERATELIMIT | ErrorCode::ETOOMANY | ErrorCode::ETOOMANYCONNECTIONS,
                )
                | MErr::Hashcash(_) => LoginError::Throttled,
                MErr::Network(s) => LoginError::Network(s),
                e => LoginError::Other(e.to_string()),
            })?;
        let session = client
            .session()
            .ok_or_else(|| LoginError::Other("no session".into()))?;
        session
            .serialize()
            .map(SessionBlob)
            .map_err(|e| LoginError::Other(e.to_string()))
    }
}

/// MEGA's error codes in the store's terms (the table in the module docs).
fn code_err(c: ErrorCode) -> BErr {
    use ErrorCode::*;
    match c {
        ENOENT => BErr::NotFound,
        ESID | EBLOCKED | EACCESS | EMFAREQUIRED => BErr::AuthRevoked,
        ERATELIMIT => BErr::Throttled(Some(Duration::from_secs(5))),
        ETOOMANY | ETOOMANYCONNECTIONS => BErr::Throttled(Some(Duration::from_secs(10))),
        EOVERQUOTA => BErr::TransferQuota(Some(TRANSFER_QUOTA_PAUSE)),
        EGOINGOVERQUOTA | EPAYWALL => BErr::StorageFull,
        EEXIST => BErr::NameConflict,
        EAGAIN | EEXPIRED | EINTERNAL | ETEMPUNAVAIL | EFAILED | ERANGE | EINCOMPLETE
        | EROLLEDBACK | EREAD | EWRITE => BErr::Transient(format!("mega: {c}")),
        c => BErr::Fatal(format!("mega: {c} ({})", c.as_i64())),
    }
}

fn berr(e: MErr) -> BErr {
    match e {
        MErr::Api(c) => code_err(c),
        MErr::OverQuota { retry_after } => {
            BErr::TransferQuota(Some(retry_after.unwrap_or(TRANSFER_QUOTA_PAUSE)))
        }
        MErr::Http {
            status,
            retry_after,
        } => match status {
            429 => BErr::Throttled(retry_after),
            509 => BErr::TransferQuota(Some(retry_after.unwrap_or(TRANSFER_QUOTA_PAUSE))),
            // An expired transfer URL, or a server hiccup: a fresh URL next time.
            403 | 404 | 410 | 500..=599 => BErr::Transient(format!("mega: HTTP {status}")),
            s => BErr::Fatal(format!("mega: HTTP {s}")),
        },
        MErr::Network(s) => BErr::Transient(format!("mega: {s}")),
        MErr::Protocol(s) => BErr::Transient(format!("mega: {s}")),
        MErr::MacMismatch => BErr::Transient("mega: condensed MAC mismatch".into()),
        e @ MErr::ShortRead { .. } => BErr::Transient(format!("mega: {e}")),
        MErr::InvalidSession(_) => BErr::AuthRevoked,
        MErr::Hashcash(s) => {
            tracing::warn!(reason = %s, "mega: hashcash not answered");
            BErr::Throttled(Some(Duration::from_secs(60)))
        }
        MErr::Crypto(s) => BErr::Fatal(format!("mega: {s}")),
        MErr::Io(e) => BErr::Fatal(format!("mega: local file: {e}")),
    }
}

/// Upload commands: `-17` means the account is full, not the transfer quota.
fn upload_err(e: MErr) -> BErr {
    match e {
        MErr::Api(ErrorCode::EOVERQUOTA) => BErr::StorageFull,
        e => berr(e),
    }
}

/// `sc` errors: `Ok(())` means the cursor is lost (a stale `sn` answers `-6`, as the
/// MEGA SDK treats it; any other code too, spec §5.3).
fn sc_err(e: MErr) -> Result<(), BErr> {
    match e {
        MErr::Api(ErrorCode::ESID | ErrorCode::EBLOCKED | ErrorCode::EACCESS) => {
            Err(BErr::AuthRevoked)
        }
        MErr::Api(ErrorCode::ERATELIMIT | ErrorCode::ETOOMANYCONNECTIONS) => {
            Err(BErr::Throttled(Some(Duration::from_secs(10))))
        }
        MErr::Api(c) => {
            tracing::info!(code = c.as_i64(), "mega: sc refused the cursor");
            Ok(())
        }
        e => Err(berr(e)),
    }
}

// ---------------------------------------------------------------- the mirror

/// A node handle (MEGA's 8 base64 characters), stored inline.
type H = [u8; 8];

fn hk(s: &str) -> Option<H> {
    s.as_bytes().try_into().ok()
}

fn hstr(h: &H) -> String {
    String::from_utf8_lossy(h).into_owned()
}

fn parse_pid(pid: &str) -> Result<H, BErr> {
    hk(pid).ok_or(BErr::NotFound)
}

#[derive(Debug, Clone, Copy)]
struct Skel {
    parent: Option<H>,
    kind: NodeKind,
}

#[derive(Default)]
struct Mirror {
    /// The feed position the mirror reflects (plus Bunko's own writes, applied at once).
    sn: Option<String>,
    built: Option<Instant>,
    root: Option<H>,
    rubbish: Option<H>,
    /// Every node: structure only.
    skel: HashMap<H, Skel>,
    kids: HashMap<H, Vec<H>>,
    /// Decrypted nodes: those inside the subtree, and those that have been.
    full: HashMap<H, Node>,
    /// Subtree roots: the located roots and any folder listed directly.
    tracked: HashSet<H>,
    /// Folders named `root_name` at the last fetch, oldest first.
    candidates: Vec<H>,
    root_name: Option<String>,
    /// Decrypted subtrees (children first) of nodes removed since the fetch.
    graveyard: HashMap<H, Vec<H>>,
    /// A folder entered the subtree with descendants not yet decrypted.
    stale: bool,
}

type Overlay = HashMap<H, Option<H>>;

fn to_rnode(n: &Node, parent: Option<H>, trashed: bool) -> RNode {
    let file = n.kind.is_file();
    let ms = n.ts.saturating_mul(1000);
    RNode {
        pid: n.handle.clone(),
        parent: parent.map(|p| hstr(&p)),
        name_raw: n.name.as_bytes().to_vec(),
        is_dir: !file,
        size: if file { n.size } else { 0 },
        base_mtime_ms: ms,
        created_ms: ms,
        version: format!("{}:{}", n.handle, n.ts),
        cver: n.handle.clone(),
        pin: file.then(|| n.handle.clone()),
        hash: None,
        trashed,
    }
}

impl Mirror {
    fn fresh(&self, ttl: Duration) -> bool {
        self.sn.is_some() && !self.stale && self.built.is_some_and(|t| t.elapsed() < ttl)
    }

    fn parent_in(&self, h: &H, overlay: &Overlay) -> Option<H> {
        match overlay.get(h) {
            Some(p) => *p,
            None => self.skel.get(h).and_then(|s| s.parent),
        }
    }

    fn parent(&self, h: &H) -> Option<H> {
        self.skel.get(h).and_then(|s| s.parent)
    }

    /// Set `h`'s parent and kind, keeping the children lists in step.
    fn link(&mut self, h: H, parent: Option<H>, kind: NodeKind) {
        let old = self.skel.insert(h, Skel { parent, kind });
        let old_parent = old.and_then(|s| s.parent);
        if old.is_some() && old_parent == parent {
            return;
        }
        if let Some(op) = old_parent
            && let Some(v) = self.kids.get_mut(&op)
        {
            v.retain(|x| *x != h);
        }
        if let Some(p) = parent {
            self.kids.entry(p).or_default().push(h);
        }
    }

    fn put_full(&mut self, n: Node) -> Option<H> {
        let h = hk(&n.handle)?;
        let p = n.parent.as_deref().and_then(hk);
        self.link(h, p, n.kind);
        self.full.insert(h, n);
        Some(h)
    }

    /// Inside a tracked root (the root itself included).
    fn in_scope(&self, h: H, overlay: &Overlay) -> bool {
        let mut cur = h;
        for _ in 0..MAX_DEPTH {
            if self.tracked.contains(&cur) {
                return true;
            }
            match self.parent_in(&cur, overlay) {
                Some(p) => cur = p,
                None => return false,
            }
        }
        false
    }

    /// Under the rubbish bin (`from` is the first node to test).
    fn trashed_from(&self, from: Option<H>, overlay: &Overlay) -> bool {
        let Some(rubbish) = self.rubbish else {
            return false;
        };
        let mut cur = from;
        for _ in 0..MAX_DEPTH {
            match cur {
                Some(c) if c == rubbish => return true,
                Some(c) => cur = self.parent_in(&c, overlay),
                None => return false,
            }
        }
        false
    }

    fn trashed(&self, h: H) -> bool {
        self.trashed_from(Some(h), &Overlay::new())
    }

    /// Reaches the cloud root, inbox or rubbish bin (a node half-way through a move
    /// across `sc` batches does not).
    fn attached(&self, h: H) -> bool {
        let mut cur = h;
        for _ in 0..MAX_DEPTH {
            let Some(s) = self.skel.get(&cur) else {
                return false;
            };
            match s.parent {
                Some(p) => cur = p,
                None => {
                    return matches!(s.kind, NodeKind::Root | NodeKind::Inbox | NodeKind::Trash);
                }
            }
        }
        false
    }

    fn live(&self, h: H) -> Option<&Node> {
        let n = self.full.get(&h)?;
        (self.attached(h) && !self.trashed(h)).then_some(n)
    }

    fn live_container(&self, h: H) -> bool {
        self.root == Some(h) || self.live(h).is_some_and(|n| n.kind.is_container())
    }

    fn is_ancestor_or_self(&self, anc: H, mut h: H) -> bool {
        for _ in 0..MAX_DEPTH {
            if h == anc {
                return true;
            }
            match self.parent(&h) {
                Some(p) => h = p,
                None => return false,
            }
        }
        false
    }

    /// `h` and its descendants, children first (all, or only the decrypted ones).
    fn subtree(&self, h: H, decrypted_only: bool) -> Vec<H> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![(h, false)];
        while let Some((x, done)) = stack.pop() {
            if done {
                if !decrypted_only || self.full.contains_key(&x) {
                    out.push(x);
                }
                continue;
            }
            if !seen.insert(x) {
                continue;
            }
            stack.push((x, true));
            for k in self.kids.get(&x).into_iter().flatten() {
                stack.push((*k, false));
            }
        }
        out
    }

    fn has_undecrypted_below(&self, h: H) -> bool {
        self.subtree(h, false)
            .iter()
            .any(|x| *x != h && !self.full.contains_key(x))
    }

    fn remove_subtree(&mut self, h: H) {
        let all = self.subtree(h, false);
        if let Some(p) = self.parent(&h)
            && let Some(v) = self.kids.get_mut(&p)
        {
            v.retain(|x| *x != h);
        }
        for x in all {
            self.skel.remove(&x);
            self.kids.remove(&x);
            self.full.remove(&x);
        }
    }

    fn bury(&mut self, h: H, gone: Vec<H>) {
        if self.graveyard.len() >= GRAVEYARD_MAX {
            self.graveyard.clear();
        }
        self.graveyard.insert(h, gone);
    }

    fn rnode(&self, h: H) -> Option<RNode> {
        let n = self.full.get(&h)?;
        Some(to_rnode(n, self.parent(&h), self.trashed(h)))
    }

    /// The decrypted live nodes under `h` (not `h`), parents before children.
    fn descendants_preorder(&self, h: H, out: &mut Vec<H>) {
        let mut stack: Vec<H> = self.sorted_kids(h).into_iter().rev().collect();
        let mut seen = HashSet::new();
        while let Some(x) = stack.pop() {
            if !seen.insert(x) || !self.full.contains_key(&x) {
                continue;
            }
            out.push(x);
            stack.extend(self.sorted_kids(x).into_iter().rev());
        }
    }

    fn sorted_kids(&self, h: H) -> Vec<H> {
        let mut v: Vec<H> = self
            .kids
            .get(&h)
            .into_iter()
            .flatten()
            .filter(|k| self.full.contains_key(*k))
            .copied()
            .collect();
        v.sort_by_key(|k| (self.full[k].ts, *k));
        v
    }

    /// Every decrypted node inside the subtree, children first.
    fn scope_children_first(&self) -> Vec<H> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for r in &self.tracked {
            for x in self.subtree(*r, true) {
                if seen.insert(x) {
                    out.push(x);
                }
            }
        }
        out
    }
}

/// Build a mirror from a fetched tree (blocking: two scans of the spooled answer).
fn build(
    snap: &Snapshot,
    session: &Session,
    root_name: Option<&str>,
    keep: &HashSet<H>,
) -> Result<Mirror, MErr> {
    let mut m = Mirror {
        root_name: root_name.map(str::to_string),
        ..Mirror::default()
    };
    let mut candidates: Vec<(i64, H)> = Vec::new();
    // Pass 1: structure of everything; folder names only, to find the roots.
    let sn = snap.scan(&mut |f: FileNode| {
        let Some(h) = hk(&f.handle) else {
            return;
        };
        m.link(h, hk(&f.parent), f.kind);
        match f.kind {
            NodeKind::Root => m.root = Some(h),
            NodeKind::Trash => m.rubbish = Some(h),
            NodeKind::Folder if root_name.is_some() => {
                if let Ok(Some(n)) = session.decrypt_node(&f)
                    && Some(n.name.as_str()) == root_name
                {
                    candidates.push((n.ts, h));
                }
            }
            _ => {}
        }
    })?;
    candidates.retain(|(_, h)| !m.trashed(*h));
    candidates.sort();
    m.candidates = candidates.into_iter().map(|(_, h)| h).collect();
    m.tracked = keep
        .iter()
        .filter(|h| m.skel.contains_key(*h))
        .copied()
        .chain(m.candidates.iter().copied())
        .collect();
    // Pass 2: decrypt the subtree only.
    let none = Overlay::new();
    let mut undecryptable = 0usize;
    let mut decrypted: Vec<Node> = Vec::new();
    snap.scan(&mut |f: FileNode| {
        let Some(h) = hk(&f.handle) else {
            return;
        };
        if !m.in_scope(h, &none) {
            return;
        }
        match session.decrypt_node(&f) {
            Ok(Some(n)) => decrypted.push(n),
            _ => undecryptable += 1,
        }
    })?;
    for n in decrypted {
        if let Some(h) = hk(&n.handle) {
            m.full.insert(h, n);
        }
    }
    if undecryptable > 0 {
        tracing::warn!(
            undecryptable,
            "mega: nodes in the library this account cannot decrypt were skipped"
        );
    }
    tracing::debug!(
        nodes = m.skel.len(),
        decrypted = m.full.len(),
        bytes = snap.len(),
        "mega: tree fetched"
    );
    m.sn = Some(sn);
    m.built = Some(Instant::now());
    Ok(m)
}

// ---------------------------------------------------------------- the backend

/// What `OpCtx::resume` holds for an upload. It contains the file key (inside the
/// upload state): the outbox keeps it in `state.sqlite` with the staged bytes.
#[derive(Default, Serialize, Deserialize)]
struct Resume {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<UploadState>,
    sha256: String,
    parent: String,
    name: String,
    idem: String,
    mtime: i64,
    /// `p` was sent at least once: look for the node before sending it again.
    #[serde(default)]
    completing: bool,
}

impl Resume {
    fn load(op: &OpCtx) -> Option<Resume> {
        op.resume
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
    }
    fn save(&self, op: &mut OpCtx) {
        op.resume = serde_json::to_string(self).ok();
    }
}

fn file_mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or_else(|_| SystemTime::now())
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The MEGA backend. Cheap to clone.
#[derive(Clone)]
pub struct MegaBackend(Arc<Inner>);

struct Inner {
    client: Client,
    cfg: MegaConfig,
    mirror: Mutex<Mirror>,
}

impl MegaBackend {
    /// Resume a stored session (one `us` call; `AuthRevoked` when MEGA killed it).
    pub async fn resume(blob: &SessionBlob, cfg: MegaConfig) -> Result<MegaBackend, BErr> {
        let client = Client::resume(cfg.client.clone(), blob.expose())
            .await
            .map_err(berr)?;
        Ok(MegaBackend(Arc::new(Inner {
            client,
            cfg,
            mirror: Mutex::new(Mirror::default()),
        })))
    }

    /// End Bunko's session at MEGA (Disconnect; the reader's sessions stay).
    pub async fn logout(&self) -> Result<(), BErr> {
        self.0.client.logout().await.map_err(berr)
    }
}

impl Inner {
    fn session(&self) -> Result<&Session, BErr> {
        self.client.session().ok_or(BErr::AuthRevoked)
    }

    async fn rebuild(&self, m: &mut Mirror, extra: &[H]) -> Result<(), BErr> {
        let snap = self
            .client
            .fetch_nodes(&self.cfg.state_dir)
            .await
            .map_err(berr)?;
        let session = self.session()?.clone();
        let name = m.root_name.clone();
        let mut keep = m.tracked.clone();
        keep.extend(extra.iter().copied());
        let new =
            tokio::task::spawn_blocking(move || build(&snap, &session, name.as_deref(), &keep))
                .await
                .map_err(|e| BErr::Fatal(format!("mega: tree build: {e}")))?
                .map_err(|e| match e {
                    MErr::Api(c) => code_err(c),
                    e => berr(e),
                })?;
        *m = new;
        Ok(())
    }

    async fn ensure_built(&self, m: &mut Mirror) -> Result<(), BErr> {
        if m.sn.is_none() {
            self.rebuild(m, &[]).await?;
        }
        Ok(())
    }

    /// A fresh mirror covering `roots`.
    async fn ensure(&self, m: &mut Mirror, roots: &[H]) -> Result<(), BErr> {
        if !m.fresh(self.cfg.snapshot_ttl) || roots.iter().any(|r| !m.tracked.contains(r)) {
            self.rebuild(m, roots).await?;
        }
        Ok(())
    }

    /// Bring the mirror to the head of the feed ("drain pending sc", §5.7).
    async fn drain(&self, m: &mut Mirror) -> Result<(), BErr> {
        self.ensure_built(m).await?;
        for _ in 0..64 {
            let sn = m.sn.clone().unwrap_or_default();
            match self.client.poll_events(&sn).await {
                Ok(Poll::Idle { .. }) => return Ok(()),
                Ok(Poll::Batch(b)) => {
                    let mut discard = Vec::new();
                    if self.translate(m, &b.events, true, &mut discard)? {
                        return self.rebuild(m, &[]).await;
                    }
                    m.sn = Some(b.sn);
                    if b.events.is_empty() {
                        return Ok(());
                    }
                }
                Err(e) => {
                    sc_err(e)?;
                    return self.rebuild(m, &[]).await;
                }
            }
        }
        Ok(())
    }

    /// Turn folded events into change items. With `apply`, the mirror follows them
    /// (it is at the batch's starting `sn`); without, it is ahead and only consulted,
    /// with nodes the page moves tracked in an overlay. Returns true when the batch
    /// holds something unmodelled.
    fn translate(
        &self,
        m: &mut Mirror,
        events: &[Event],
        apply: bool,
        items: &mut Vec<ChangeItem>,
    ) -> Result<bool, BErr> {
        let session = self.session()?;
        let mut overlay = Overlay::new();
        for ev in events {
            match ev {
                Event::Resync { packet } => {
                    tracing::info!(packet = %packet, "mega: unmodelled sc packet; re-listing");
                    return Ok(true);
                }
                Event::Added(f) | Event::Moved(f) => {
                    let Some(h) = hk(&f.handle) else {
                        continue;
                    };
                    let parent = hk(&f.parent);
                    let now_in =
                        m.tracked.contains(&h) || parent.is_some_and(|p| m.in_scope(p, &overlay));
                    let entering = now_in && !m.in_scope(h, &overlay);
                    if !now_in {
                        if m.full.contains_key(&h) {
                            items.extend(
                                m.subtree(h, true)
                                    .iter()
                                    .map(|x| ChangeItem::Removed { pid: hstr(x) }),
                            );
                        }
                    } else {
                        match session.decrypt_node(f) {
                            Ok(Some(n)) => {
                                let trashed = m.trashed_from(parent, &overlay);
                                items.push(ChangeItem::Upsert(to_rnode(&n, parent, trashed)));
                                if apply {
                                    if entering
                                        && n.kind.is_container()
                                        && m.has_undecrypted_below(h)
                                    {
                                        m.stale = true;
                                    }
                                    m.put_full(n);
                                    continue;
                                }
                            }
                            _ => tracing::warn!(
                                handle = %f.handle,
                                "mega: a node in the library cannot be decrypted; skipped"
                            ),
                        }
                    }
                    if apply {
                        m.link(h, parent, f.kind);
                        if let Some(n) = m.full.get_mut(&h) {
                            n.parent = parent.map(|p| hstr(&p));
                        }
                    } else {
                        overlay.insert(h, parent);
                    }
                }
                Event::Updated { handle, attr, ts } => {
                    let Some(h) = hk(handle) else {
                        continue;
                    };
                    let Some(node) = m.full.get(&h) else {
                        continue;
                    };
                    let attrs = match NodeAttributes::decrypt_b64(node.key.aes(), attr) {
                        Ok(a) => a,
                        Err(e) => {
                            tracing::warn!(handle = %handle, error = %e, "mega: attribute update does not decrypt");
                            continue;
                        }
                    };
                    let mut n = node.clone();
                    n.set_attributes(attrs);
                    if let Some(ts) = ts {
                        n.ts = *ts;
                    }
                    if m.in_scope(h, &overlay) {
                        let parent = m.parent_in(&h, &overlay);
                        let trashed = m.trashed_from(Some(h), &overlay);
                        items.push(ChangeItem::Upsert(to_rnode(&n, parent, trashed)));
                    }
                    if apply {
                        m.full.insert(h, n);
                    }
                }
                Event::Removed { handle, moved } => {
                    let Some(h) = hk(handle) else {
                        continue;
                    };
                    let gone = if m.skel.contains_key(&h) {
                        m.subtree(h, true)
                    } else {
                        m.graveyard.get(&h).cloned().unwrap_or_default()
                    };
                    items.extend(gone.iter().map(|x| ChangeItem::Removed { pid: hstr(x) }));
                    if apply {
                        if *moved {
                            // Half of a move whose `t` is in a later batch: detach and
                            // keep the data for when it reappears.
                            let kind = m.skel.get(&h).map_or(NodeKind::Folder, |s| s.kind);
                            m.link(h, None, kind);
                        } else {
                            m.remove_subtree(h);
                            m.bury(h, gone);
                        }
                    } else {
                        overlay.insert(h, None);
                    }
                }
            }
        }
        Ok(false)
    }

    /// Re-list the subtree (an unmodelled packet at the mirror's cursor): a fresh tree,
    /// every node in it as an upsert, every node gone from it as a removal.
    async fn relist(&self, m: &mut Mirror) -> Result<Vec<ChangeItem>, BErr> {
        let old = m.scope_children_first();
        self.rebuild(m, &[]).await?;
        let mut now = Vec::new();
        let mut roots: Vec<H> = m.tracked.iter().copied().collect();
        roots.sort();
        for r in roots {
            if m.full.contains_key(&r) {
                now.push(r);
            }
            m.descendants_preorder(r, &mut now);
        }
        let keep: HashSet<H> = now.iter().copied().collect();
        let mut items: Vec<ChangeItem> = old
            .iter()
            .filter(|h| !keep.contains(*h))
            .map(|h| ChangeItem::Removed { pid: hstr(h) })
            .collect();
        items.extend(
            now.iter()
                .filter_map(|h| m.rnode(*h))
                .map(ChangeItem::Upsert),
        );
        Ok(items)
    }

    /// The live folder `dir`, decrypted with its children (a folder outside the
    /// subtree is added to it).
    async fn open_dir(&self, m: &mut Mirror, dir: H) -> Result<(), BErr> {
        self.ensure_built(m).await?;
        if !m.in_scope(dir, &Overlay::new()) || m.stale || m.has_undecrypted_below(dir) {
            m.tracked.insert(dir);
            self.rebuild(m, &[dir]).await?;
        }
        self.drain(m).await?;
        if !m.live_container(dir) {
            return Err(BErr::NotFound);
        }
        Ok(())
    }

    fn children(&self, m: &Mirror, dir: H) -> Vec<RNode> {
        m.sorted_kids(dir)
            .into_iter()
            .filter(|k| m.attached(*k))
            .filter_map(|k| m.rnode(k))
            .collect()
    }

    /// Where an upload goes: `(parent, name)`.
    async fn target(&self, to: UploadTarget<'_>) -> Result<(String, String), BErr> {
        let mut m = self.mirror.lock().await;
        match to {
            UploadTarget::New { parent, name } => {
                let p = parse_pid(parent)?;
                self.ensure_built(&mut m).await?;
                if !m.live_container(p) {
                    self.drain(&mut m).await?;
                }
                if !m.live_container(p) {
                    return Err(BErr::NotFound);
                }
                Ok((parent.to_string(), name.to_string()))
            }
            UploadTarget::Replace { pid, if_cver } => {
                if if_cver.is_some_and(|c| c != pid) {
                    return Err(BErr::PreconditionFailed);
                }
                let h = parse_pid(pid)?;
                // MEGA has no conditional write: drain, then re-check the handle.
                self.drain(&mut m).await?;
                let n = m.live(h).ok_or(BErr::NotFound)?;
                if !n.kind.is_file() {
                    return Err(BErr::NotFound);
                }
                let parent = m.parent(&h).ok_or(BErr::NotFound)?;
                Ok((hstr(&parent), n.name.clone()))
            }
        }
    }

    /// A node a previous, uncertain `p` created: same name, size and fingerprint.
    async fn find_completed(
        &self,
        r: &Resume,
        size: u64,
        path: &Path,
    ) -> Result<Option<RNode>, BErr> {
        let fp = file_fingerprint(path, r.mtime).map_err(berr)?;
        let parent = parse_pid(&r.parent)?;
        let mut m = self.mirror.lock().await;
        self.drain(&mut m).await?;
        Ok(m.sorted_kids(parent)
            .into_iter()
            .filter_map(|k| m.full.get(&k).map(|n| (k, n)))
            .find(|(_, n)| {
                n.kind.is_file() && n.name == r.name && n.size == size && n.fingerprint == Some(fp)
            })
            .and_then(|(k, _)| m.rnode(k)))
    }

    async fn upload(
        &self,
        to: UploadTarget<'_>,
        src: UploadSource<'_>,
        op: &mut OpCtx,
    ) -> Result<RNode, BErr> {
        let UploadSource::File { path, size, sha256 } = src;
        let sha = hex::encode(sha256);
        let (parent, name) = self.target(to).await?;
        let mut r = Resume::load(op)
            .filter(|r| {
                r.sha256 == sha
                    && r.parent == parent
                    && r.name == name
                    && r.state.as_ref().is_none_or(|s| s.size == size)
            })
            .unwrap_or_else(|| Resume {
                sha256: sha,
                parent,
                name,
                idem: idempotence_id(),
                mtime: file_mtime(path),
                ..Resume::default()
            });
        if r.completing
            && let Some(done) = self.find_completed(&r, size, path).await?
        {
            op.resume = None;
            return Ok(done);
        }
        let mut failures = 0u32;
        let state = loop {
            let mut st = match r.state.take() {
                Some(s) if s.is_consistent() => s,
                _ => self.client.upload_begin(size).await.map_err(upload_err)?,
            };
            r.state = Some(st.clone());
            r.save(op);
            let sent = {
                let (rr, oo) = (&mut r, &mut *op);
                self.client
                    .upload_send(&mut st, path, self.cfg.upload_post_max, &mut |s| {
                        rr.state = Some(s.clone());
                        rr.save(oo);
                    })
                    .await
            };
            match sent {
                Ok(_) => break st,
                Err(e) => {
                    let restart = matches!(
                        e.code(),
                        Some(ErrorCode::EEXPIRED | ErrorCode::EFAILED | ErrorCode::ERANGE)
                    ) || matches!(
                        e,
                        MErr::Http {
                            status: 403 | 404 | 410,
                            ..
                        }
                    ) || matches!(e, MErr::Protocol(_));
                    r.state = if restart { None } else { Some(st) };
                    r.save(op);
                    let err = upload_err(e);
                    failures += 1;
                    let retry =
                        matches!(err, BErr::Transient(_)) && failures <= self.cfg.upload_retries;
                    if !retry {
                        return Err(err);
                    }
                    tracing::debug!(error = %err, failures, "mega: upload chunk failed, resuming");
                    tokio::time::sleep(self.client.config().retry_base * failures).await;
                }
            }
        };
        r.completing = true;
        r.state = Some(state.clone());
        r.save(op);
        let fp = file_fingerprint(path, r.mtime).map_err(berr)?;
        let f = self
            .client
            .upload_complete(&r.parent, &r.name, &state, fp, &r.idem)
            .await
            .map_err(upload_err)?;
        let node = self
            .session()?
            .decrypt_node(&f)
            .map_err(berr)?
            .ok_or_else(|| BErr::Fatal("mega: uploaded node does not decrypt".into()))?;
        op.resume = None;
        let mut m = self.mirror.lock().await;
        let h = m
            .put_full(node)
            .ok_or_else(|| BErr::Fatal("mega: bad handle".into()))?;
        m.rnode(h)
            .ok_or_else(|| BErr::Fatal("mega: uploaded node missing".into()))
    }
}

impl Backend for MegaBackend {
    fn caps(&self) -> BackendCaps {
        BackendCaps {
            cas_create: false,
            // `if_cver` is a drained re-check of the handle, not atomic (§5.7).
            cas_overwrite: false,
            atomic_replace: false,
            server_copy: false,
            range: true,
            pinned_read: true,
            push: true,
            hash_kind: None,
            mtime_granularity_ns: 1_000_000_000,
            simple_upload_max: self.0.cfg.upload_post_max,
            chunk_multiple: CHUNK,
            path_ids: false,
        }
    }

    async fn account(&self) -> Result<AccountInfo, BErr> {
        let u = self.0.client.user_info().await.map_err(berr)?;
        Ok(AccountInfo {
            id: u.handle,
            label: u.email,
        })
    }

    /// Every folder named exactly `name` outside the rubbish bin (the reader's rule,
    /// not case-folded), oldest first. Always a fresh tree.
    async fn locate_roots(&self, name: &str) -> Result<Vec<RNode>, BErr> {
        let mut m = self.0.mirror.lock().await;
        m.root_name = Some(name.to_string());
        m.tracked.clear();
        self.0.rebuild(&mut m, &[]).await?;
        Ok(m.candidates
            .iter()
            .filter(|h| m.attached(**h))
            .filter_map(|h| m.rnode(*h))
            .collect())
    }

    async fn create_root(&self, name: &str) -> Result<RNode, BErr> {
        let mut m = self.0.mirror.lock().await;
        self.0.ensure_built(&mut m).await?;
        let root = m
            .root
            .ok_or_else(|| BErr::Fatal("mega: no cloud drive root".into()))?;
        let f = self
            .0
            .client
            .create_folder(&hstr(&root), name, &idempotence_id())
            .await
            .map_err(upload_err)?;
        let n = self
            .0
            .session()?
            .decrypt_node(&f)
            .map_err(berr)?
            .ok_or_else(|| BErr::Fatal("mega: new folder does not decrypt".into()))?;
        let h = m
            .put_full(n)
            .ok_or_else(|| BErr::Fatal("mega: bad handle".into()))?;
        m.tracked.insert(h);
        m.candidates.push(h);
        m.rnode(h)
            .ok_or_else(|| BErr::Fatal("mega: new folder missing".into()))
    }

    async fn start_cursor(&self) -> Result<Cursor, BErr> {
        let mut m = self.0.mirror.lock().await;
        self.0.ensure(&mut m, &[]).await?;
        m.sn.clone()
            .map(Cursor)
            .ok_or_else(|| BErr::Fatal("mega: no sn".into()))
    }

    async fn list_all(
        &self,
        roots: &[Pid],
        sink: &mut (dyn FnMut(RNode) + Send),
    ) -> Result<(), BErr> {
        let hs: Vec<H> = roots
            .iter()
            .map(|r| parse_pid(r))
            .collect::<Result<_, _>>()?;
        let mut m = self.0.mirror.lock().await;
        self.0.ensure(&mut m, &hs).await?;
        let mut order = Vec::new();
        for r in &hs {
            if !m.live_container(*r) {
                return Err(BErr::NotFound);
            }
            m.descendants_preorder(*r, &mut order);
        }
        for h in order {
            if let Some(n) = m.rnode(h) {
                sink(n);
            }
        }
        Ok(())
    }

    async fn list_dir(&self, dir: &str, sink: &mut (dyn FnMut(RNode) + Send)) -> Result<(), BErr> {
        let h = parse_pid(dir)?;
        let mut m = self.0.mirror.lock().await;
        self.0.open_dir(&mut m, h).await?;
        self.0.children(&m, h).into_iter().for_each(sink);
        Ok(())
    }

    async fn find_named(&self, parent: &str, name: &str) -> Result<(Vec<RNode>, ServerTime), BErr> {
        let h = parse_pid(parent)?;
        let mut m = self.0.mirror.lock().await;
        self.0.open_dir(&mut m, h).await?;
        let found = self
            .0
            .children(&m, h)
            .into_iter()
            .filter(|n| n.name_raw == name.as_bytes())
            .collect();
        Ok((found, ServerTime(self.0.client.server_now_ms())))
    }

    async fn changes(&self, c: &Cursor) -> Result<ChangePage, BErr> {
        let mut m = self.0.mirror.lock().await;
        self.0.ensure_built(&mut m).await?;
        if m.sn.as_deref() != Some(c.0.as_str()) {
            self.0.drain(&mut m).await?;
        }
        let lockstep = m.sn.as_deref() == Some(c.0.as_str());
        let batch = match self.0.client.poll_events(&c.0).await {
            Ok(Poll::Idle { .. }) => {
                return Ok(ChangePage::Page {
                    items: Vec::new(),
                    next: c.clone(),
                    more: false,
                });
            }
            Ok(Poll::Batch(b)) => b,
            Err(e) => {
                sc_err(e)?;
                return Ok(ChangePage::CursorLost);
            }
        };
        let mut items = Vec::new();
        if self
            .0
            .translate(&mut m, &batch.events, lockstep, &mut items)?
        {
            if !lockstep {
                return Ok(ChangePage::CursorLost);
            }
            let items = self.0.relist(&mut m).await?;
            let next = m.sn.clone().map(Cursor).unwrap_or_else(|| c.clone());
            return Ok(ChangePage::Page {
                items,
                next,
                more: false,
            });
        }
        if lockstep {
            m.sn = Some(batch.sn.clone());
        }
        Ok(ChangePage::Page {
            items,
            next: Cursor(batch.sn),
            more: !batch.events.is_empty(),
        })
    }

    async fn wait_change(&self, c: &Cursor, max: Duration) -> Result<(), BErr> {
        match self.0.client.poll_events(&c.0).await {
            Ok(Poll::Batch(_)) => Ok(()),
            Ok(Poll::Idle { wait_url }) => {
                self.0.client.sc_wait(&wait_url, max).await.map_err(berr)
            }
            // A lost cursor is for `changes` to report.
            Err(e) => sc_err(e),
        }
    }

    async fn stat(&self, pid: &str) -> Result<Option<RNode>, BErr> {
        let Some(h) = hk(pid) else {
            return Ok(None);
        };
        let mut m = self.0.mirror.lock().await;
        self.0.drain(&mut m).await?;
        Ok(if m.attached(h) { m.rnode(h) } else { None })
    }

    async fn read(
        &self,
        pid: &str,
        pin: Option<&str>,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<RemoteBody, BErr> {
        // A new version is a new handle: a pin other than the handle is another version.
        if pin.is_some_and(|p| p != pid) {
            return Err(BErr::PreconditionFailed);
        }
        let h = parse_pid(pid)?;
        let key = {
            let mut m = self.0.mirror.lock().await;
            self.0.ensure_built(&mut m).await?;
            if m.live(h).is_none() {
                self.0.drain(&mut m).await?;
            }
            match m.live(h) {
                Some(n) if n.kind.is_file() => n.key.clone(),
                _ => return Err(BErr::NotFound),
            }
        };
        let target = self.0.client.download_target(pid).await.map_err(berr)?;
        let (first, last) = range.unwrap_or((0, None));
        let body = self
            .0
            .client
            .read_range(&key, &target, first, last)
            .await
            .map_err(berr)?;
        Ok(body)
    }

    async fn upload(
        &self,
        to: UploadTarget<'_>,
        src: UploadSource<'_>,
        op: &mut OpCtx,
    ) -> Result<RNode, BErr> {
        self.0.upload(to, src, op).await
    }

    async fn mkdir(&self, parent: &str, name: &str, _op: &OpCtx) -> Result<RNode, BErr> {
        let p = parse_pid(parent)?;
        let mut m = self.0.mirror.lock().await;
        self.0.ensure_built(&mut m).await?;
        if !m.live_container(p) {
            self.0.drain(&mut m).await?;
            if !m.live_container(p) {
                return Err(BErr::NotFound);
            }
        }
        let f = self
            .0
            .client
            .create_folder(parent, name, &idempotence_id())
            .await
            .map_err(upload_err)?;
        let n = self
            .0
            .session()?
            .decrypt_node(&f)
            .map_err(berr)?
            .ok_or_else(|| BErr::Fatal("mega: new folder does not decrypt".into()))?;
        let h = m
            .put_full(n)
            .ok_or_else(|| BErr::Fatal("mega: bad handle".into()))?;
        m.rnode(h)
            .ok_or_else(|| BErr::Fatal("mega: new folder missing".into()))
    }

    async fn update(
        &self,
        pid: &str,
        parent: Option<&str>,
        name: Option<&str>,
        if_cver: Option<&str>,
    ) -> Result<RNode, BErr> {
        if if_cver.is_some_and(|c| c != pid) {
            return Err(BErr::PreconditionFailed);
        }
        let h = parse_pid(pid)?;
        let mut m = self.0.mirror.lock().await;
        self.0.drain(&mut m).await?;
        let node = m.live(h).ok_or(BErr::NotFound)?.clone();
        let new_parent = match parent {
            Some(p) => {
                let hp = parse_pid(p)?;
                if !m.live_container(hp) {
                    return Err(BErr::NotFound);
                }
                if m.is_ancestor_or_self(h, hp) {
                    return Err(BErr::Fatal("move into itself".into()));
                }
                (m.parent(&h) != Some(hp)).then_some(hp)
            }
            None => None,
        };
        if let Some(name) = name.filter(|n| *n != node.name) {
            let mut attrs = node.attributes();
            attrs.name = name.to_string();
            self.0
                .client
                .set_attributes(pid, &node.key, &attrs, &idempotence_id())
                .await
                .map_err(berr)?;
            if let Some(n) = m.full.get_mut(&h) {
                n.set_attributes(attrs);
            }
        }
        if let Some(hp) = new_parent {
            self.0
                .client
                .move_node(pid, &hstr(&hp), &idempotence_id())
                .await
                .map_err(berr)?;
            m.link(h, Some(hp), node.kind);
            if let Some(n) = m.full.get_mut(&h) {
                n.parent = Some(hstr(&hp));
            }
        }
        m.rnode(h).ok_or(BErr::NotFound)
    }

    async fn copy(&self, _pid: &str, _parent: &str, _name: &str) -> Result<RNode, BErr> {
        Err(BErr::Unsupported)
    }

    async fn trash(&self, pid: &str, if_cver: Option<&str>) -> Result<(), BErr> {
        if if_cver.is_some_and(|c| c != pid) {
            return Err(BErr::PreconditionFailed);
        }
        let h = parse_pid(pid)?;
        let mut m = self.0.mirror.lock().await;
        self.0.ensure_built(&mut m).await?;
        if !m.skel.contains_key(&h) {
            self.0.drain(&mut m).await?;
        }
        let kind = m.skel.get(&h).ok_or(BErr::NotFound)?.kind;
        let rubbish = m
            .rubbish
            .ok_or_else(|| BErr::Fatal("mega: no rubbish bin".into()))?;
        self.0
            .client
            .move_node(pid, &hstr(&rubbish), &idempotence_id())
            .await
            .map_err(berr)?;
        m.link(h, Some(rubbish), kind);
        if let Some(n) = m.full.get_mut(&h) {
            n.parent = Some(hstr(&rubbish));
        }
        Ok(())
    }

    async fn delete_permanent(&self, pid: &str) -> Result<(), BErr> {
        let h = parse_pid(pid)?;
        let mut m = self.0.mirror.lock().await;
        self.0
            .client
            .delete_node(pid, &idempotence_id())
            .await
            .map_err(berr)?;
        let gone = m.subtree(h, true);
        m.remove_subtree(h);
        m.bury(h, gone);
        Ok(())
    }

    async fn quota(&self) -> Result<Option<bunko_store::Capacity>, BErr> {
        let q = self.0.client.storage_quota().await.map_err(berr)?;
        Ok((q.total > 0).then(|| bunko_store::Capacity {
            total: q.total,
            available: q.total.saturating_sub(q.used),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_table() {
        use ErrorCode::*;
        assert_eq!(
            code_err(EAGAIN),
            BErr::Transient("mega: request failed, retrying".into())
        );
        for c in [ERATELIMIT, ETOOMANY, ETOOMANYCONNECTIONS] {
            assert!(matches!(code_err(c), BErr::Throttled(Some(_))), "{c:?}");
        }
        assert_eq!(code_err(ENOENT), BErr::NotFound);
        for c in [ESID, EBLOCKED, EACCESS] {
            assert_eq!(code_err(c), BErr::AuthRevoked, "{c:?}");
        }
        assert_eq!(
            code_err(EOVERQUOTA),
            BErr::TransferQuota(Some(Duration::from_secs(1800)))
        );
        assert!(matches!(code_err(EEXPIRED), BErr::Transient(_)));
        assert_eq!(upload_err(MErr::Api(EOVERQUOTA)), BErr::StorageFull);
        assert_eq!(
            berr(MErr::OverQuota { retry_after: None }),
            BErr::TransferQuota(Some(TRANSFER_QUOTA_PAUSE))
        );
        assert!(matches!(sc_err(MErr::Api(ETOOMANY)), Ok(())));
        assert_eq!(sc_err(MErr::Api(ESID)), Err(BErr::AuthRevoked));
    }

    #[test]
    fn session_blob_debug_is_redacted() {
        let b = SessionBlob::new("c2VjcmV0c2Vzc2lvbmJsb2I");
        assert_eq!(format!("{b:?}"), "SessionBlob(<redacted>)");
        assert_eq!(b.expose(), "c2VjcmV0c2Vzc2lvbmJsb2I");
    }
}
