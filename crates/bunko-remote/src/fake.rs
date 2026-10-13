//! An in-memory [`Backend`] for tests (feature `testing`).
//!
//! One tree behind a mutex, a change log whose cursor is the log index, and a server
//! clock on `tokio::time` (create the fake inside the runtime when the test pauses
//! time). A profile picks the provider behaviour that matters to the store: id scheme,
//! name uniqueness, CAS, pinned reads, how a replace and a trash look in the feed.
//!
//! Content hashes: every [`HashKind`] is stood in for by the sha256 hex of the bytes,
//! so equal bytes compare equal under any kind (only `Sha256` is the real function).
//! The `reader_*` helpers act as Mokuro Reader would on that provider; they bypass call
//! counting and fault injection.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::backend::{
    AccountInfo, BErr, Backend, BackendCaps, ChangeItem, ChangePage, Cursor, HashKind, OpCtx, Pid,
    RNode, RemoteBody, ServerTime, UploadSource, UploadTarget,
};

/// The fake server clock's value at construction (unix ms).
pub const EPOCH_MS: i64 = 1_760_000_000_000;
const READ_CHUNK: usize = 64 * 1024;

/// Called by `list_all` after each node it hands to the sink, with the count so far.
pub type ListHook = std::sync::Arc<dyn Fn(usize) + Send + Sync>;

/// A backend call, for [`FakeBackend::fail_next`] and [`FakeBackend::calls`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FakeOp {
    Account,
    LocateRoots,
    CreateRoot,
    StartCursor,
    ListAll,
    ListDir,
    FindNamed,
    Changes,
    WaitChange,
    Stat,
    Read,
    Upload,
    Mkdir,
    Update,
    Copy,
    Trash,
    DeletePermanent,
    Quota,
}

/// One [`Backend::read`] call, for [`FakeBackend::read_log`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeRead {
    pub pid: Pid,
    pub pin: Option<String>,
    pub range: Option<(u64, Option<u64>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ids {
    /// pid = path from the account root; a move changes it.
    Path,
    Opaque,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Names {
    Duplicates,
    Exact,
    CaseInsensitive,
}

#[derive(Debug, Clone)]
struct Profile {
    name: &'static str,
    caps: BackendCaps,
    ids: Ids,
    names: Names,
    /// A replace makes a new node and removes the old one (MEGA).
    replace_is_new_node: bool,
    /// Old content versions stay readable by their pin (Drive revisions).
    keeps_revisions: bool,
    /// `locate_roots` finds the name anywhere, not only at the top level.
    roots_anywhere: bool,
    /// A trash is an upsert with `trashed` (Drive), not a removal.
    trash_flags: bool,
}

#[derive(Debug, Clone)]
struct FNode {
    pid: Pid,
    parent: Option<Pid>,
    name: Vec<u8>,
    is_dir: bool,
    data: Bytes,
    created_ms: i64,
    mtime_ms: i64,
    version: u64,
    cver: u64,
    trashed: bool,
}

#[derive(Default)]
struct State {
    nodes: HashMap<Pid, FNode>,
    children: HashMap<Pid, BTreeSet<Pid>>,
    revisions: HashMap<(Pid, u64), Bytes>,
    log: Vec<ChangeItem>,
    log_floor: usize,
    /// A cursor equal to `log_floor` was issued after the loss (and so is valid).
    floor_reissued: bool,
    next_id: u64,
    next_ver: u64,
    last_ms: i64,
    calls: HashMap<FakeOp, usize>,
    fail: HashMap<FakeOp, VecDeque<BErr>>,
    latency: Duration,
    throttle_until: Option<Instant>,
    reorder: bool,
    duplicate: bool,
    page_size: usize,
    quota: Option<u64>,
    trashed_log: Vec<Pid>,
    deleted_log: Vec<Pid>,
    read_log: Vec<FakeRead>,
    /// A misbehaving provider: these bytes answer every read of the pid.
    serve_instead: HashMap<Pid, Bytes>,
    upload_log: Vec<String>,
    /// A misbehaving provider: this many next uploads store their bytes with the
    /// last one dropped.
    mangle_uploads: usize,
}

/// The in-memory provider.
pub struct FakeBackend {
    profile: Profile,
    state: Mutex<State>,
    notify: Notify,
    list_hook: Mutex<Option<ListHook>>,
    clock0: Instant,
}

fn base_caps() -> BackendCaps {
    BackendCaps {
        cas_create: false,
        cas_overwrite: false,
        atomic_replace: true,
        server_copy: true,
        range: true,
        pinned_read: true,
        push: false,
        hash_kind: None,
        mtime_granularity_ns: 1_000_000_000,
        simple_upload_max: u64::MAX,
        chunk_multiple: 1,
        path_ids: false,
    }
}

impl FakeBackend {
    /// Google Drive: opaque ids, duplicate names, no CAS, reads pinned by revision
    /// (old revisions stay readable), a trash is an upsert with `trashed`, 1 ms mtimes.
    pub fn drive_like() -> Self {
        Self::new(Profile {
            name: "drive_like",
            caps: BackendCaps {
                hash_kind: Some(HashKind::Sha256),
                mtime_granularity_ns: 1_000_000,
                simple_upload_max: 5 << 20,
                chunk_multiple: 256 << 10,
                ..base_caps()
            },
            ids: Ids::Opaque,
            names: Names::Duplicates,
            replace_is_new_node: false,
            keeps_revisions: true,
            roots_anywhere: true,
            trash_flags: true,
        })
    }

    /// WebDAV whose host honours `If-Match` / `If-None-Match: *`: path ids, unique
    /// case-sensitive names, strong ETags as pins.
    pub fn webdav_cas() -> Self {
        Self::webdav("webdav_cas", true)
    }

    /// WebDAV without conditional requests: a PUT to an existing name overwrites it,
    /// reads cannot be pinned.
    pub fn webdav_plain() -> Self {
        Self::webdav("webdav_plain", false)
    }

    fn webdav(name: &'static str, cas: bool) -> Self {
        Self::new(Profile {
            name,
            caps: BackendCaps {
                cas_create: cas,
                cas_overwrite: cas,
                pinned_read: cas,
                path_ids: true,
                ..base_caps()
            },
            ids: Ids::Path,
            names: Names::Exact,
            replace_is_new_node: false,
            keeps_revisions: false,
            roots_anywhere: false,
            trash_flags: false,
        })
    }

    /// OneDrive: opaque ids, case-insensitive unique names, CAS, cTag pins.
    pub fn onedrive_like() -> Self {
        Self::new(Profile {
            name: "onedrive_like",
            caps: BackendCaps {
                cas_create: true,
                cas_overwrite: true,
                hash_kind: Some(HashKind::QuickXor),
                simple_upload_max: 4 << 20,
                chunk_multiple: 320 << 10,
                ..base_caps()
            },
            ids: Ids::Opaque,
            names: Names::CaseInsensitive,
            replace_is_new_node: false,
            keeps_revisions: false,
            roots_anywhere: false,
            trash_flags: false,
        })
    }

    /// MEGA: opaque ids, duplicate names, a replace is a new node, push feed, no copy.
    pub fn mega_like() -> Self {
        Self::new(Profile {
            name: "mega_like",
            caps: BackendCaps {
                atomic_replace: false,
                server_copy: false,
                push: true,
                ..base_caps()
            },
            ids: Ids::Opaque,
            names: Names::Duplicates,
            replace_is_new_node: true,
            keeps_revisions: false,
            roots_anywhere: true,
            trash_flags: false,
        })
    }

    fn new(profile: Profile) -> Self {
        let root = account_root(profile.ids);
        let mut st = State {
            page_size: 500,
            last_ms: EPOCH_MS,
            floor_reissued: true,
            ..State::default()
        };
        st.nodes.insert(
            root.clone(),
            FNode {
                pid: root,
                parent: None,
                name: Vec::new(),
                is_dir: true,
                data: Bytes::new(),
                created_ms: EPOCH_MS,
                mtime_ms: EPOCH_MS,
                version: 0,
                cver: 0,
                trashed: false,
            },
        );
        FakeBackend {
            profile,
            state: Mutex::new(st),
            notify: Notify::new(),
            list_hook: Mutex::new(None),
            clock0: Instant::now(),
        }
    }

    // ---- knobs ------------------------------------------------------------------

    /// The next `n` uploads store their bytes minus the last one (and answer with
    /// that node), as a provider that corrupts a transfer would.
    pub fn mangle_uploads(&self, n: usize) {
        self.state.lock().mangle_uploads = n;
    }

    /// Every backend call waits this long first.
    pub fn set_latency(&self, d: Duration) {
        self.state.lock().latency = d;
    }

    /// The next call of `op` fails with `e` (queued: several calls fail in order).
    pub fn fail_next(&self, op: FakeOp, e: BErr) {
        self.state.lock().fail.entry(op).or_default().push_back(e);
    }

    /// Every call fails with `Throttled(remaining)` for `d`.
    pub fn throttle_for(&self, d: Duration) {
        self.state.lock().throttle_until = Some(Instant::now() + d);
    }

    /// Every cursor issued so far becomes `CursorLost`.
    pub fn lose_cursor(&self) {
        let mut st = self.state.lock();
        st.log_floor = st.log.len();
        st.floor_reissued = false;
    }

    /// Deliver each page's items in reverse order.
    pub fn reorder_changes(&self, on: bool) {
        self.state.lock().reorder = on;
    }

    /// Deliver each change item twice.
    pub fn duplicate_deliveries(&self, on: bool) {
        self.state.lock().duplicate = on;
    }

    /// Items per change page (default 500).
    pub fn set_page_size(&self, n: usize) {
        self.state.lock().page_size = n.max(1);
    }

    /// `list_all` calls `hook` after every node it streams (to observe the consumer
    /// mid-listing); `None` removes it.
    pub fn set_list_hook(&self, hook: Option<ListHook>) {
        *self.list_hook.lock() = hook;
    }

    /// The names of successful uploads (new files and new versions), in order.
    pub fn upload_log(&self) -> Vec<String> {
        self.state.lock().upload_log.clone()
    }

    /// Make every read of `pid` return `data` whatever its range or pin (a provider
    /// that lies about sizes); `None` restores honest reads.
    pub fn serve_instead(&self, pid: &str, data: Option<Vec<u8>>) {
        let mut st = self.state.lock();
        match data {
            Some(d) => st.serve_instead.insert(pid.to_string(), Bytes::from(d)),
            None => st.serve_instead.remove(pid),
        };
    }

    /// Account storage limit in bytes; `None` (the default) reports no quota.
    pub fn set_quota(&self, total: Option<u64>) {
        self.state.lock().quota = total;
    }

    // ---- inspection -------------------------------------------------------------

    /// How many times `op` was called through the [`Backend`] trait.
    pub fn calls(&self, op: FakeOp) -> usize {
        self.state.lock().calls.get(&op).copied().unwrap_or(0)
    }

    /// Reset every call counter and the read log.
    pub fn reset_calls(&self) {
        let mut st = self.state.lock();
        st.calls.clear();
        st.read_log.clear();
    }

    /// Pids passed to [`Backend::trash`], in order.
    pub fn trashed_log(&self) -> Vec<Pid> {
        self.state.lock().trashed_log.clone()
    }

    /// Pids passed to [`Backend::delete_permanent`], in order.
    pub fn deleted_log(&self) -> Vec<Pid> {
        self.state.lock().deleted_log.clone()
    }

    /// Every [`Backend::read`] call since the last [`FakeBackend::reset_calls`], in
    /// order (failed ones included).
    pub fn read_log(&self) -> Vec<FakeRead> {
        self.state.lock().read_log.clone()
    }

    /// The server clock (unix ms).
    pub fn now_ms(&self) -> i64 {
        EPOCH_MS + self.clock0.elapsed().as_millis() as i64
    }

    /// The pid of the account's top folder (the parent of top-level roots).
    pub fn account_root(&self) -> Pid {
        account_root(self.profile.ids)
    }

    /// The live node at `path` (`/`-separated from the account root; the oldest of
    /// duplicate names).
    pub fn node_at(&self, path: &str) -> Option<RNode> {
        let st = self.state.lock();
        let pid = st.resolve(&self.profile, path)?;
        Some(st.rnode(&self.profile, &pid))
    }

    /// The current bytes of a file.
    pub fn content(&self, pid: &str) -> Option<Vec<u8>> {
        let st = self.state.lock();
        st.nodes.get(pid).map(|n| n.data.to_vec())
    }

    // ---- the reader -------------------------------------------------------------

    /// The reader creates `path` (and missing parents) or writes a new version of it.
    pub fn reader_put(&self, path: &str, bytes: &[u8]) -> RNode {
        let (parent, name) = split(path);
        let now = self.now_ms();
        let out = {
            let mut st = self.state.lock();
            let dir = st.mkdir_p(&self.profile, parent, now);
            let data = Bytes::copy_from_slice(bytes);
            let pid = match st.child_named(&dir, name.as_bytes()) {
                Some(pid) => {
                    assert!(!st.nodes[&pid].is_dir, "reader_put over a folder: {path}");
                    st.replace(&self.profile, &pid, data, now)
                }
                None => {
                    if let Some(p) = st.name_taken(&self.profile, &dir, name, None) {
                        panic!("reader_put: {path} collides with {p}");
                    }
                    let pid = st.insert(&self.profile, &dir, name, false, data, now);
                    st.log_upsert(&self.profile, &pid);
                    pid
                }
            };
            st.rnode(&self.profile, &pid)
        };
        self.notify.notify_waiters();
        out
    }

    /// The reader deletes `path` the way it does on this provider (trash, or a
    /// permanent delete on MEGA).
    pub fn reader_delete(&self, path: &str) {
        {
            let mut st = self.state.lock();
            let pid = st
                .resolve(&self.profile, path)
                .unwrap_or_else(|| panic!("reader_delete: no {path}"));
            st.trash(&self.profile, &pid);
        }
        self.notify.notify_waiters();
    }

    /// The reader moves or renames `from` to `to` (missing parents are created).
    pub fn reader_rename(&self, from: &str, to: &str) -> RNode {
        let now = self.now_ms();
        let out = {
            let mut st = self.state.lock();
            let pid = st
                .resolve(&self.profile, from)
                .unwrap_or_else(|| panic!("reader_rename: no {from}"));
            let (parent, name) = split(to);
            let dir = st.mkdir_p(&self.profile, parent, now);
            if let Some(p) = st.name_taken(&self.profile, &dir, name, Some(&pid)) {
                panic!("reader_rename: {to} collides with {p}");
            }
            let pid = st.move_node(&self.profile, &pid, &dir, name);
            st.rnode(&self.profile, &pid)
        };
        self.notify.notify_waiters();
        out
    }

    /// The first half of a move that the feed reports in two steps (a MEGA delete and
    /// create of the same handle spanning two batches; opaque ids only): `path` leaves
    /// the tree and the feed logs `Removed` for it. [`FakeBackend::reader_attach`]
    /// finishes the move.
    pub fn reader_detach(&self, path: &str) -> Pid {
        assert_eq!(
            self.profile.ids,
            Ids::Opaque,
            "reader_detach needs opaque ids"
        );
        let pid = {
            let mut st = self.state.lock();
            let pid = st
                .resolve(&self.profile, path)
                .unwrap_or_else(|| panic!("reader_detach: no {path}"));
            let parent = st.nodes[&pid].parent.clone().unwrap_or_default();
            if let Some(set) = st.children.get_mut(&parent) {
                set.remove(&pid);
            }
            let n = st.nodes.get_mut(&pid).expect("resolved node");
            n.trashed = true;
            st.log.push(ChangeItem::Removed { pid: pid.clone() });
            pid
        };
        self.notify.notify_waiters();
        pid
    }

    /// The second half of [`FakeBackend::reader_detach`]: `pid` reappears at `to`
    /// (missing parents are created) and the feed logs one `Upsert` for it.
    pub fn reader_attach(&self, pid: &str, to: &str) -> RNode {
        let now = self.now_ms();
        let out = {
            let mut st = self.state.lock();
            let (parent, name) = split(to);
            let dir = st.mkdir_p(&self.profile, parent, now);
            let v = st.next_ver();
            let n = st.nodes.get_mut(pid).expect("reader_attach: unknown pid");
            n.trashed = false;
            n.parent = Some(dir.clone());
            n.name = name.as_bytes().to_vec();
            n.version = v;
            st.children.entry(dir).or_default().insert(pid.to_string());
            st.log_upsert(&self.profile, pid);
            st.rnode(&self.profile, pid)
        };
        self.notify.notify_waiters();
        out
    }

    /// The reader creates the folder `path` (and missing parents).
    pub fn reader_mkdir(&self, path: &str) -> RNode {
        let now = self.now_ms();
        let out = {
            let mut st = self.state.lock();
            let pid = st.mkdir_p(&self.profile, path, now);
            st.rnode(&self.profile, &pid)
        };
        self.notify.notify_waiters();
        out
    }

    // ---- plumbing ---------------------------------------------------------------

    /// Count the call, apply latency, throttling and injected failures.
    async fn enter(&self, op: FakeOp) -> Result<(), BErr> {
        let latency = {
            let mut st = self.state.lock();
            *st.calls.entry(op).or_default() += 1;
            st.latency
        };
        if !latency.is_zero() {
            tokio::time::sleep(latency).await;
        }
        let mut st = self.state.lock();
        if let Some(e) = st.fail.get_mut(&op).and_then(VecDeque::pop_front) {
            return Err(e);
        }
        if let Some(until) = st.throttle_until {
            let now = Instant::now();
            if now < until {
                return Err(BErr::Throttled(Some(until - now)));
            }
            st.throttle_until = None;
        }
        Ok(())
    }

    /// Run a mutation under the lock, then wake push waiters.
    fn mutate<T>(
        &self,
        f: impl FnOnce(&mut State, &Profile, i64) -> Result<T, BErr>,
    ) -> Result<T, BErr> {
        let now = self.now_ms();
        let out = f(&mut self.state.lock(), &self.profile, now);
        self.notify.notify_waiters();
        out
    }
}

fn account_root(ids: Ids) -> Pid {
    match ids {
        Ids::Path => String::new(),
        Ids::Opaque => "root".into(),
    }
}

fn split(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((p, n)) => (p, n),
        None => ("", path),
    }
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// The case-insensitive name rule.
fn fold(name: &[u8]) -> String {
    crate::names::fold(&crate::names::nfc(name))
}

impl State {
    /// Cursors issued before the last `lose_cursor` are lost (positions are reused,
    /// so one equal to the floor counts only once it is reissued).
    fn valid_cursor(&self, at: usize) -> bool {
        at <= self.log.len()
            && (at > self.log_floor || (at == self.log_floor && self.floor_reissued))
    }

    fn issued(&mut self, at: usize) {
        if at == self.log_floor {
            self.floor_reissued = true;
        }
    }

    fn tick(&mut self, now: i64) -> i64 {
        self.last_ms = now.max(self.last_ms + 1);
        self.last_ms
    }

    fn next_ver(&mut self) -> u64 {
        self.next_ver += 1;
        self.next_ver
    }

    fn rnode(&self, prof: &Profile, pid: &str) -> RNode {
        let n = &self.nodes[pid];
        let file = !n.is_dir;
        RNode {
            pid: n.pid.clone(),
            parent: n.parent.clone(),
            name_raw: n.name.clone(),
            is_dir: n.is_dir,
            size: n.data.len() as u64,
            base_mtime_ms: n.mtime_ms,
            created_ms: n.created_ms,
            version: format!("v{}", n.version),
            cver: format!("c{}", n.cver),
            pin: (file && prof.caps.pinned_read).then(|| format!("c{}", n.cver)),
            hash: prof
                .caps
                .hash_kind
                .filter(|_| file)
                .map(|k| (k, hex::encode(Sha256::digest(&n.data)))),
            trashed: n.trashed,
        }
    }

    fn log_upsert(&mut self, prof: &Profile, pid: &str) {
        let n = self.rnode(prof, pid);
        self.log.push(ChangeItem::Upsert(n));
    }

    fn live(&self, pid: &str) -> Option<&FNode> {
        self.nodes.get(pid).filter(|n| !n.trashed)
    }

    fn live_dir(&self, pid: &str) -> Result<(), BErr> {
        match self.live(pid) {
            Some(n) if n.is_dir => Ok(()),
            _ => Err(BErr::NotFound),
        }
    }

    fn live_children(&self, pid: &str) -> Vec<Pid> {
        let mut out: Vec<Pid> = self
            .children
            .get(pid)
            .into_iter()
            .flatten()
            .filter(|c| self.live(c).is_some())
            .cloned()
            .collect();
        out.sort_by_key(|c| (self.nodes[c].created_ms, c.clone()));
        out
    }

    fn child_named(&self, parent: &str, name: &[u8]) -> Option<Pid> {
        self.live_children(parent)
            .into_iter()
            .find(|c| self.nodes[c].name == name)
    }

    /// The existing live child a new `name` would collide with under this profile.
    fn name_taken(
        &self,
        prof: &Profile,
        parent: &str,
        name: &str,
        except: Option<&str>,
    ) -> Option<Pid> {
        let want = fold(name.as_bytes());
        self.live_children(parent).into_iter().find(|c| {
            Some(c.as_str()) != except
                && match prof.names {
                    Names::Duplicates => false,
                    Names::Exact => self.nodes[c].name == name.as_bytes(),
                    Names::CaseInsensitive => fold(&self.nodes[c].name) == want,
                }
        })
    }

    fn resolve(&self, prof: &Profile, path: &str) -> Option<Pid> {
        let mut cur = account_root(prof.ids);
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            cur = self.child_named(&cur, seg.as_bytes())?;
        }
        Some(cur)
    }

    fn mkdir_p(&mut self, prof: &Profile, path: &str, now: i64) -> Pid {
        let mut cur = account_root(prof.ids);
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            cur = match self.child_named(&cur, seg.as_bytes()) {
                Some(c) => c,
                None => {
                    let pid = self.insert(prof, &cur, seg, true, Bytes::new(), now);
                    self.log_upsert(prof, &pid);
                    pid
                }
            };
        }
        cur
    }

    fn insert(
        &mut self,
        prof: &Profile,
        parent: &str,
        name: &str,
        is_dir: bool,
        data: Bytes,
        now: i64,
    ) -> Pid {
        let pid = match prof.ids {
            Ids::Path => join(parent, name),
            Ids::Opaque => {
                self.next_id += 1;
                format!("id-{}", self.next_id)
            }
        };
        let t = self.tick(now);
        let v = self.next_ver();
        self.nodes.insert(
            pid.clone(),
            FNode {
                pid: pid.clone(),
                parent: Some(parent.to_string()),
                name: name.as_bytes().to_vec(),
                is_dir,
                data,
                created_ms: t,
                mtime_ms: t,
                version: v,
                cver: v,
                trashed: false,
            },
        );
        self.children
            .entry(parent.to_string())
            .or_default()
            .insert(pid.clone());
        pid
    }

    /// A new content version of `pid`; returns the pid that now holds it.
    fn replace(&mut self, prof: &Profile, pid: &str, data: Bytes, now: i64) -> Pid {
        if prof.replace_is_new_node {
            let n = self.nodes[pid].clone();
            self.remove_subtree(pid);
            let parent = n.parent.unwrap_or_default();
            let new = self.insert(
                prof,
                &parent,
                &String::from_utf8_lossy(&n.name),
                false,
                data,
                now,
            );
            self.log_upsert(prof, &new);
            return new;
        }
        let t = self.tick(now);
        let v = self.next_ver();
        let keep = prof.keeps_revisions;
        let n = self.nodes.get_mut(pid).expect("replace of a missing node");
        let old = std::mem::replace(&mut n.data, data);
        let old_cver = n.cver;
        n.cver = v;
        n.version = v;
        n.mtime_ms = t;
        if keep {
            self.revisions.insert((pid.to_string(), old_cver), old);
        }
        self.log_upsert(prof, pid);
        pid.to_string()
    }

    /// Remove `pid` and everything under it; logs `Removed`, children first.
    fn remove_subtree(&mut self, pid: &str) {
        let kids: Vec<Pid> = self.children.remove(pid).into_iter().flatten().collect();
        for k in kids {
            self.remove_subtree(&k);
        }
        if let Some(n) = self.nodes.remove(pid) {
            if let Some(p) = &n.parent
                && let Some(set) = self.children.get_mut(p)
            {
                set.remove(pid);
            }
            self.revisions.retain(|(p, _), _| p != pid);
            self.log.push(ChangeItem::Removed {
                pid: pid.to_string(),
            });
        }
    }

    fn trash(&mut self, prof: &Profile, pid: &str) {
        if prof.trash_flags {
            let v = self.next_ver();
            let n = self.nodes.get_mut(pid).expect("trash of a missing node");
            n.trashed = true;
            n.version = v;
            self.log_upsert(prof, pid);
        } else {
            self.remove_subtree(pid);
        }
    }

    /// Move `pid` under `parent` as `name`; returns its (possibly new) pid.
    fn move_node(&mut self, prof: &Profile, pid: &str, parent: &str, name: &str) -> Pid {
        let old_parent = self.nodes[pid].parent.clone().unwrap_or_default();
        if prof.ids == Ids::Opaque {
            let v = self.next_ver();
            let n = self.nodes.get_mut(pid).expect("move of a missing node");
            n.parent = Some(parent.to_string());
            n.name = name.as_bytes().to_vec();
            n.version = v;
            if let Some(set) = self.children.get_mut(&old_parent) {
                set.remove(pid);
            }
            self.children
                .entry(parent.to_string())
                .or_default()
                .insert(pid.to_string());
            self.log_upsert(prof, pid);
            return pid.to_string();
        }
        // Path ids: every pid in the subtree changes.
        let new_top = join(parent, name);
        if new_top == pid {
            return new_top;
        }
        let mut order = Vec::new();
        self.preorder(pid, &mut order);
        let mut map: HashMap<Pid, Pid> = HashMap::new();
        for p in &order {
            let new = if p == pid {
                new_top.clone()
            } else {
                let n = &self.nodes[p];
                let np = &map[n.parent.as_ref().expect("child has a parent")];
                join(np, &String::from_utf8_lossy(&n.name))
            };
            map.insert(p.clone(), new);
        }
        if let Some(set) = self.children.get_mut(&old_parent) {
            set.remove(pid);
        }
        let v = self.next_ver();
        let mut moved = Vec::new();
        for p in &order {
            let mut n = self.nodes.remove(p).expect("subtree node");
            let kids = self.children.remove(p);
            n.pid = map[p].clone();
            if p == pid {
                n.parent = Some(parent.to_string());
                n.name = name.as_bytes().to_vec();
                n.version = v;
            } else {
                n.parent = n.parent.as_ref().map(|q| map[q].clone());
            }
            if let Some(kids) = kids {
                self.children.insert(
                    n.pid.clone(),
                    kids.into_iter().map(|k| map[&k].clone()).collect(),
                );
            }
            moved.push(n);
        }
        for n in moved {
            self.nodes.insert(n.pid.clone(), n);
        }
        self.children
            .entry(parent.to_string())
            .or_default()
            .insert(new_top.clone());
        for p in order.iter().rev() {
            self.log.push(ChangeItem::Removed { pid: p.clone() });
        }
        for p in &order {
            self.log_upsert(prof, &map[p]);
        }
        new_top
    }

    fn preorder(&self, pid: &str, out: &mut Vec<Pid>) {
        out.push(pid.to_string());
        if let Some(kids) = self.children.get(pid) {
            for k in kids {
                self.preorder(k, out);
            }
        }
    }

    fn is_ancestor_or_self<'a>(&'a self, anc: &str, mut pid: &'a str) -> bool {
        loop {
            if pid == anc {
                return true;
            }
            match self.nodes.get(pid).and_then(|n| n.parent.as_deref()) {
                Some(p) => pid = p,
                None => return false,
            }
        }
    }

    fn used(&self) -> u64 {
        self.nodes
            .values()
            .filter(|n| !n.is_dir)
            .map(|n| n.data.len() as u64)
            .sum()
    }

    fn check_quota(&self, adding: u64, freeing: u64) -> Result<(), BErr> {
        match self.quota {
            Some(total) if self.used().saturating_sub(freeing) + adding > total => {
                Err(BErr::StorageFull)
            }
            _ => Ok(()),
        }
    }

    fn check_cas(&self, prof: &Profile, pid: &str, if_cver: Option<&str>) -> Result<(), BErr> {
        if let (true, Some(want)) = (prof.caps.cas_overwrite, if_cver)
            && format!("c{}", self.nodes[pid].cver) != want
        {
            return Err(BErr::PreconditionFailed);
        }
        Ok(())
    }
}

fn load_source(src: UploadSource<'_>) -> Result<Bytes, BErr> {
    let UploadSource::File { path, size, sha256 } = src;
    let data = std::fs::read(path)
        .map_err(|e| BErr::Fatal(format!("upload source {}: {e}", path.display())))?;
    if data.len() as u64 != size || <[u8; 32]>::from(Sha256::digest(&data)) != sha256 {
        return Err(BErr::Fatal(format!(
            "upload source {} changed",
            path.display()
        )));
    }
    Ok(Bytes::from(data))
}

fn body(data: Bytes) -> RemoteBody {
    let chunks: Vec<std::io::Result<Bytes>> = (0..data.len())
        .step_by(READ_CHUNK)
        .map(|i| Ok(data.slice(i..(i + READ_CHUNK).min(data.len()))))
        .collect();
    futures_util::stream::iter(chunks).boxed()
}

impl Backend for FakeBackend {
    fn caps(&self) -> BackendCaps {
        self.profile.caps.clone()
    }

    async fn account(&self) -> Result<AccountInfo, BErr> {
        self.enter(FakeOp::Account).await?;
        Ok(AccountInfo {
            id: "fake-account".into(),
            label: self.profile.name.into(),
        })
    }

    async fn locate_roots(&self, name: &str) -> Result<Vec<RNode>, BErr> {
        self.enter(FakeOp::LocateRoots).await?;
        let st = self.state.lock();
        let top = account_root(self.profile.ids);
        let mut found: Vec<&FNode> = st
            .nodes
            .values()
            .filter(|n| {
                n.is_dir
                    && !n.trashed
                    && n.name == name.as_bytes()
                    && (self.profile.roots_anywhere || n.parent.as_deref() == Some(top.as_str()))
            })
            .collect();
        found.sort_by_key(|n| (n.created_ms, n.pid.clone()));
        Ok(found
            .iter()
            .map(|n| st.rnode(&self.profile, &n.pid))
            .collect())
    }

    async fn create_root(&self, name: &str) -> Result<RNode, BErr> {
        self.enter(FakeOp::CreateRoot).await?;
        self.mutate(|st, prof, now| {
            let top = account_root(prof.ids);
            if st.name_taken(prof, &top, name, None).is_some() {
                return Err(BErr::NameConflict);
            }
            let pid = st.insert(prof, &top, name, true, Bytes::new(), now);
            st.log_upsert(prof, &pid);
            Ok(st.rnode(prof, &pid))
        })
    }

    async fn start_cursor(&self) -> Result<Cursor, BErr> {
        self.enter(FakeOp::StartCursor).await?;
        let mut st = self.state.lock();
        let at = st.log.len();
        st.issued(at);
        Ok(Cursor(at.to_string()))
    }

    async fn list_all(
        &self,
        roots: &[Pid],
        sink: &mut (dyn FnMut(RNode) + Send),
    ) -> Result<(), BErr> {
        self.enter(FakeOp::ListAll).await?;
        let nodes = {
            let st = self.state.lock();
            let mut out = Vec::new();
            let mut stack: Vec<Pid> = Vec::new();
            for r in roots {
                st.live_dir(r)?;
                stack.extend(st.live_children(r).into_iter().rev());
            }
            while let Some(p) = stack.pop() {
                stack.extend(st.live_children(&p).into_iter().rev());
                out.push(st.rnode(&self.profile, &p));
            }
            out
        };
        let hook = self.list_hook.lock().clone();
        for (i, n) in nodes.into_iter().enumerate() {
            sink(n);
            if let Some(h) = &hook {
                h(i + 1);
            }
        }
        Ok(())
    }

    async fn list_dir(&self, dir: &str, sink: &mut (dyn FnMut(RNode) + Send)) -> Result<(), BErr> {
        self.enter(FakeOp::ListDir).await?;
        let nodes: Vec<RNode> = {
            let st = self.state.lock();
            st.live_dir(dir)?;
            st.live_children(dir)
                .iter()
                .map(|c| st.rnode(&self.profile, c))
                .collect()
        };
        nodes.into_iter().for_each(sink);
        Ok(())
    }

    async fn find_named(&self, parent: &str, name: &str) -> Result<(Vec<RNode>, ServerTime), BErr> {
        self.enter(FakeOp::FindNamed).await?;
        let st = self.state.lock();
        st.live_dir(parent)?;
        let found = st
            .live_children(parent)
            .iter()
            .filter(|c| st.nodes[*c].name == name.as_bytes())
            .map(|c| st.rnode(&self.profile, c))
            .collect();
        Ok((found, ServerTime(self.now_ms())))
    }

    async fn changes(&self, c: &Cursor) -> Result<ChangePage, BErr> {
        self.enter(FakeOp::Changes).await?;
        let mut st = self.state.lock();
        let at: usize = match c.0.parse() {
            Ok(at) if st.valid_cursor(at) => at,
            _ => return Ok(ChangePage::CursorLost),
        };
        let end = (at + st.page_size).min(st.log.len());
        let mut items: Vec<ChangeItem> = st.log[at..end].to_vec();
        if st.reorder {
            items.reverse();
        }
        if st.duplicate {
            items = items.into_iter().flat_map(|i| [i.clone(), i]).collect();
        }
        st.issued(end);
        Ok(ChangePage::Page {
            items,
            next: Cursor(end.to_string()),
            more: end < st.log.len(),
        })
    }

    async fn wait_change(&self, c: &Cursor, max: Duration) -> Result<(), BErr> {
        self.enter(FakeOp::WaitChange).await?;
        if !self.profile.caps.push {
            return Err(BErr::Unsupported);
        }
        let at: usize = c.0.parse().map_err(|_| BErr::CursorLost)?;
        let deadline = Instant::now() + max;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state.lock().log.len() > at {
                return Ok(());
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Ok(());
            }
        }
    }

    async fn stat(&self, pid: &str) -> Result<Option<RNode>, BErr> {
        self.enter(FakeOp::Stat).await?;
        let st = self.state.lock();
        Ok(st
            .nodes
            .contains_key(pid)
            .then(|| st.rnode(&self.profile, pid)))
    }

    async fn read(
        &self,
        pid: &str,
        pin: Option<&str>,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<RemoteBody, BErr> {
        self.state.lock().read_log.push(FakeRead {
            pid: pid.to_string(),
            pin: pin.map(str::to_string),
            range,
        });
        self.enter(FakeOp::Read).await?;
        let lie = self.state.lock().serve_instead.get(pid).cloned();
        if let Some(d) = lie {
            return Ok(body(d));
        }
        let data = {
            let st = self.state.lock();
            let n = st.live(pid).filter(|n| !n.is_dir).ok_or(BErr::NotFound)?;
            match pin {
                Some(p) if self.profile.caps.pinned_read && p != format!("c{}", n.cver) => {
                    let rev = p.strip_prefix('c').and_then(|v| v.parse::<u64>().ok());
                    rev.and_then(|v| st.revisions.get(&(pid.to_string(), v)))
                        .cloned()
                        .ok_or(BErr::PreconditionFailed)?
                }
                _ => n.data.clone(),
            }
        };
        let data = match range {
            None => data,
            Some((first, last)) => {
                let len = data.len() as u64;
                if first >= len {
                    return Err(BErr::Fatal(format!(
                        "range {first}- not satisfiable (size {len})"
                    )));
                }
                let end = last.map_or(len, |l| (l + 1).min(len));
                data.slice(first as usize..end as usize)
            }
        };
        Ok(body(data))
    }

    async fn upload(
        &self,
        to: UploadTarget<'_>,
        src: UploadSource<'_>,
        _op: &mut OpCtx,
    ) -> Result<RNode, BErr> {
        self.enter(FakeOp::Upload).await?;
        let mut data = load_source(src)?;
        self.mutate(|st, prof, now| {
            if st.mangle_uploads > 0 && !data.is_empty() {
                st.mangle_uploads -= 1;
                data = data.slice(..data.len() - 1);
            }
            let pid = match to {
                UploadTarget::New { parent, name } => {
                    st.live_dir(parent)?;
                    match st.name_taken(prof, parent, name, None) {
                        Some(existing) if st.nodes[&existing].is_dir => {
                            return Err(BErr::NameConflict);
                        }
                        Some(_) if prof.ids == Ids::Opaque => return Err(BErr::NameConflict),
                        Some(_) if prof.caps.cas_create => return Err(BErr::PreconditionFailed),
                        Some(existing) => {
                            let freeing = st.nodes[&existing].data.len() as u64;
                            st.check_quota(data.len() as u64, freeing)?;
                            st.replace(prof, &existing, data, now)
                        }
                        None => {
                            st.check_quota(data.len() as u64, 0)?;
                            let pid = st.insert(prof, parent, name, false, data, now);
                            st.log_upsert(prof, &pid);
                            pid
                        }
                    }
                }
                UploadTarget::Replace { pid, if_cver } => {
                    if st.live(pid).is_none_or(|n| n.is_dir) {
                        return Err(BErr::NotFound);
                    }
                    st.check_cas(prof, pid, if_cver)?;
                    st.check_quota(data.len() as u64, st.nodes[pid].data.len() as u64)?;
                    st.replace(prof, pid, data, now)
                }
            };
            let name = String::from_utf8_lossy(&st.nodes[&pid].name).into_owned();
            st.upload_log.push(name);
            Ok(st.rnode(prof, &pid))
        })
    }

    async fn mkdir(&self, parent: &str, name: &str, _op: &OpCtx) -> Result<RNode, BErr> {
        self.enter(FakeOp::Mkdir).await?;
        self.mutate(|st, prof, now| {
            st.live_dir(parent)?;
            if st.name_taken(prof, parent, name, None).is_some() {
                return Err(BErr::NameConflict);
            }
            let pid = st.insert(prof, parent, name, true, Bytes::new(), now);
            st.log_upsert(prof, &pid);
            Ok(st.rnode(prof, &pid))
        })
    }

    async fn update(
        &self,
        pid: &str,
        parent: Option<&str>,
        name: Option<&str>,
        if_cver: Option<&str>,
    ) -> Result<RNode, BErr> {
        self.enter(FakeOp::Update).await?;
        self.mutate(|st, prof, _now| {
            let n = st.live(pid).ok_or(BErr::NotFound)?;
            let new_parent = parent
                .map(str::to_string)
                .or_else(|| n.parent.clone())
                .ok_or(BErr::Unsupported)?;
            let new_name = name
                .map(str::to_string)
                .unwrap_or_else(|| String::from_utf8_lossy(&n.name).into_owned());
            st.live_dir(&new_parent)?;
            st.check_cas(prof, pid, if_cver)?;
            if st.is_ancestor_or_self(pid, &new_parent) {
                return Err(BErr::Fatal("move into itself".into()));
            }
            if st
                .name_taken(prof, &new_parent, &new_name, Some(pid))
                .is_some()
            {
                return Err(BErr::NameConflict);
            }
            let pid = st.move_node(prof, pid, &new_parent, &new_name);
            Ok(st.rnode(prof, &pid))
        })
    }

    async fn copy(&self, pid: &str, parent: &str, name: &str) -> Result<RNode, BErr> {
        self.enter(FakeOp::Copy).await?;
        if !self.profile.caps.server_copy {
            return Err(BErr::Unsupported);
        }
        self.mutate(|st, prof, now| {
            let data = match st.live(pid) {
                Some(n) if n.is_dir => return Err(BErr::Unsupported),
                Some(n) => n.data.clone(),
                None => return Err(BErr::NotFound),
            };
            st.live_dir(parent)?;
            if st.name_taken(prof, parent, name, None).is_some() {
                return Err(BErr::NameConflict);
            }
            st.check_quota(data.len() as u64, 0)?;
            let new = st.insert(prof, parent, name, false, data, now);
            st.log_upsert(prof, &new);
            Ok(st.rnode(prof, &new))
        })
    }

    async fn trash(&self, pid: &str, if_cver: Option<&str>) -> Result<(), BErr> {
        self.enter(FakeOp::Trash).await?;
        self.mutate(|st, prof, _now| {
            st.live(pid).ok_or(BErr::NotFound)?;
            st.check_cas(prof, pid, if_cver)?;
            st.trash(prof, pid);
            st.trashed_log.push(pid.to_string());
            Ok(())
        })
    }

    async fn delete_permanent(&self, pid: &str) -> Result<(), BErr> {
        self.enter(FakeOp::DeletePermanent).await?;
        self.mutate(|st, _prof, _now| {
            if !st.nodes.contains_key(pid) {
                return Err(BErr::NotFound);
            }
            st.remove_subtree(pid);
            st.deleted_log.push(pid.to_string());
            Ok(())
        })
    }

    async fn quota(&self) -> Result<Option<bunko_store::Capacity>, BErr> {
        self.enter(FakeOp::Quota).await?;
        let st = self.state.lock();
        Ok(st.quota.map(|total| bunko_store::Capacity {
            total,
            available: total.saturating_sub(st.used()),
        }))
    }
}
