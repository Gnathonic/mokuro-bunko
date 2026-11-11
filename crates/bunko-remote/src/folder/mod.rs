//! The local folder driver: a library in a folder on this computer (a local disk, a
//! mounted network share, a Docker bind mount), treated exactly like a cloud provider.
//! The folder chosen plays the role the `mokuro-reader` folder plays on Google Drive:
//! it is the library root, and Mokuro Reader's Local Folder reads the same folder.
//!
//! **Ids.** A pid is the path relative to the folder, `/`-separated, in its on-disk
//! spelling (names that are not UTF-8 are skipped); `""` is the folder itself, reported
//! under the configured root name (`storage.library.folder`) whatever the directory is
//! called. A move changes the pid (`caps.path_ids`), as on WebDAV. On a filesystem that
//! folds case or Unicode, only the on-disk spelling of every segment is a pid: `stat`
//! of another spelling answers `None`.
//!
//! **Versions.** Files: `version` = `cver` = `<mtime_ns>-<size>`; folders: `"dir"`.
//!
//! **Change feed.** A persisted snapshot diff (`<state_dir>/folder-snapshot.json`), as
//! WebDAV's; a filesystem watcher only wakes the feed early ([`watch`]).
//!
//! **Hidden.** Bunko's internal names (`names::is_reserved_name`: staged uploads,
//! probe and case-rename leftovers, the trash), Chromium's `*.crswap` swap files, a
//! reader write in progress (a 0-byte file while its swap file exists, or written less
//! than [`EMPTY_SETTLE`] ago, unless Bunko wrote it), symbolic links and junctions,
//! names that are not UTF-8, and at the top `$RECYCLE.BIN`, `System Volume Information`
//! and `lost+found`.
//!
//! **Writes.** Uploads go to `.bunko-<token>.tmp` beside the target, are checked
//! against the staged sha256, fsynced, then renamed into place (a create never
//! replaces). Removals go to the in-folder trash ([`trash`]). A case-only rename is a
//! journaled two-step ([`journal`]). Every filesystem call runs on the blocking pool
//! under a timeout, so a hung network mount cannot stall the store.
//!
//! **Known limit.** A per-directory casefold (`chattr +F`) inside a case-sensitive
//! folder is not detected: the probe looks at the folder itself.

mod fsops;
mod journal;
mod memo;
mod probe;
mod snap;
mod trash;
mod watch;

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::Metadata;
use std::io::{Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use parking_lot::{Mutex, RwLock};
use sha2::{Digest, Sha256};

use crate::backend::{
    AccountInfo, BErr, Backend, BackendCaps, ChangeItem, ChangePage, Cursor, HashKind, OpCtx, Pid,
    RNode, RemoteBody, ServerTime, UploadSource, UploadTarget,
};
use crate::governor::Limits;
use crate::names::{fold, fold_path_pid, nfc};

use fsops::{Act, io_err};
use snap::{Entries, Generation, SnapFile};

/// A folder's version (its mtime moves on every child change; the index bumps folder
/// mtimes itself).
const DIR_VERSION: &str = "dir";
/// One single-node call.
const OP_LIMIT: Duration = Duration::from_secs(60);
/// One walk of the whole folder.
const WALK_LIMIT: Duration = Duration::from_secs(600);
/// One read chunk.
const CHUNK_LIMIT: Duration = Duration::from_secs(60);
const READ_CHUNK: usize = 1 << 20;
/// How often `changes` lists again before giving up on a listing that keeps leaving
/// out something that is still there (a rename away and back during the walk).
const LIST_ATTEMPTS: u32 = 3;
const LIST_RETRY_PAUSE: Duration = Duration::from_millis(250);
const CUT_SHORT: &str = "the folder changed during the listing";
/// How long a folder listing serves the exact-spelling check.
const DIR_CACHE_TTL: Duration = Duration::from_secs(1);
/// Staged uploads and probe folders older than this are leftovers of a crash.
const LEFTOVER_AGE: Duration = Duration::from_secs(86_400);
/// The trash is purged this often.
const PURGE_EVERY: Duration = Duration::from_secs(86_400);
/// An empty file written less than this long ago is a write in progress, swap file or
/// not. Chromium makes the empty target at `getFileHandle(create)` and the swap file
/// at `createWritable()`: Mokuro Reader calls the two back to back, so the gap is
/// milliseconds, a few seconds on a slow share. The margin is for the rest: a
/// filesystem keeping times to 2 s (FAT), and a share whose clock runs behind this
/// computer's (its stamp looks older than it is). Holding an empty file back costs
/// next to nothing: an empty archive or sidecar is no volume and no OCR.
pub const EMPTY_SETTLE: Duration = Duration::from_secs(60);
/// A modification time further ahead of this computer's clock than this is a wrong
/// clock, not a write just made (5 minutes: the clock skew Kerberos, and so a domain
/// share, tolerates). Without the bound, a far-future stamp would hide a file forever.
const CLOCK_AHEAD: Duration = Duration::from_secs(300);
/// How long after an empty file settles the feed looks again (stamps of 2 s and the
/// gap between the two clocks it is measured by).
const SETTLE_SLACK: Duration = Duration::from_millis(500);
/// Top-level names a filesystem or OS keeps for itself.
const ROOT_SYSTEM: [&str; 3] = ["$RECYCLE.BIN", "System Volume Information", "lost+found"];

/// How to open a folder library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderConfig {
    /// The library folder: absolute; the root Mokuro Reader's Local Folder uses.
    pub path: PathBuf,
    /// The name the root is reported by: `storage.library.folder` (`mokuro-reader`).
    pub root_name: String,
    /// Snapshot, memo, journal: `<base>/cloud/<store-id>/` (or the connect scratch dir).
    pub state_dir: PathBuf,
    /// Start the watcher (false for a connect Check).
    pub watch: bool,
}

/// What the connect probe found out about the folder's filesystem.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct FolderProbe {
    /// The folder exists and is a directory.
    pub reachable: bool,
    /// Bunko can create files in it (the write tests ran).
    pub writable: bool,
    /// `Aa` and `aA` name one entry (NTFS, APFS, vfat, CIFS, casefold).
    pub folds_case: bool,
    /// The filesystem stores names in another form than it was given (HFS+: NFD).
    pub rewrites_names: bool,
    /// A rename that refuses to replace works (`renameat2`, `renamex_np`, `MoveFileExW`).
    pub no_replace_rename: bool,
    /// `link()` works (the fallback for a create that must not replace).
    pub hard_links: bool,
    /// The modification-time resolution, in nanoseconds (0 when unknown).
    pub mtime_granularity_ns: i64,
    /// The folder is a mount point (its device differs from its parent's).
    pub mount_point: bool,
    /// The filesystem watcher started.
    pub watcher: bool,
}

/// A library in a local folder.
pub struct FolderBackend {
    inner: Arc<Inner>,
    snap: tokio::sync::Mutex<Option<SnapFile>>,
    /// cursor → the watcher's event count when its walk started.
    marks: Mutex<VecDeque<(String, u64)>>,
}

impl std::fmt::Debug for FolderBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderBackend")
            .field("path", &self.inner.root)
            .field("root_name", &self.inner.root_name)
            .field("probe", &self.inner.probe())
            .finish_non_exhaustive()
    }
}

/// Test knobs (feature `testing`).
#[cfg(feature = "testing")]
#[derive(Default)]
struct Knobs {
    granularity_ns: std::sync::atomic::AtomicI64,
    stop_case_rename: std::sync::atomic::AtomicBool,
    /// Runs once in the next replace, after its copy and before its rename.
    before_replace: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

struct Inner {
    root: PathBuf,
    root_name: String,
    state_dir: PathBuf,
    watch_on: bool,
    probe: RwLock<(FolderProbe, probe::Hidden)>,
    probe_lock: Mutex<()>,
    memo: memo::Memo,
    /// Folder listings for the exact-spelling check, with the generation they were
    /// read under (a mutation bumps it, so a listing read before it is never cached).
    dirs: Mutex<HashMap<PathBuf, DirNames>>,
    dirs_gen: AtomicU64,
    case_lock: Mutex<()>,
    /// The case-rename journal was replayed (on the first sight of the folder).
    replayed: std::sync::atomic::AtomicBool,
    /// Folders whose names that are not UTF-8 were logged.
    warned: Mutex<HashSet<PathBuf>>,
    watch: watch::Watch,
    last_purge: Mutex<Option<Instant>>,
    /// When the youngest empty file held back as a write in progress settles: no event
    /// marks that moment, so `wait_change` returns by then.
    settles: Mutex<Option<Instant>>,
    #[cfg(feature = "testing")]
    knobs: Knobs,
}

/// A folder's names and when they were read.
type DirNames = (Instant, Arc<HashSet<OsString>>);

/// The way from the library folder down to a pid's parent ([`Inner::way_to`]).
enum Way {
    /// Real folders all the way.
    Clear,
    /// A folder on the way is missing.
    Missing,
    /// A folder on the way is a link, not a folder, or out of Bunko's reach.
    Blocked,
}

/// Where the library folder stands.
enum RootState {
    Dir(Metadata),
    /// Gone, but its parent is there (deleted or renamed): removals are real.
    Missing,
    /// Neither it nor its parent is reachable (a drive or share away), or it is an
    /// empty mount point: nothing about its contents can be said.
    Unreachable(String),
}

static UNIQUE: AtomicU64 = AtomicU64::new(0);

/// `n` hex characters, unique to this call.
fn token(n: usize) -> String {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut h = Sha256::new();
    h.update(t.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(UNIQUE.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    let mut s = hex::encode(h.finalize());
    s.truncate(n);
    s
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn ns_of(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

fn mtime_ns(m: &Metadata) -> i64 {
    m.modified().map(ns_of).unwrap_or(0)
}

fn birth_ms(m: &Metadata) -> i64 {
    m.created()
        .map(|t| ns_of(t).div_euclid(1_000_000))
        .unwrap_or(0)
}

/// A file's version: `<mtime_ns>-<size>`.
fn version_of(m: &Metadata) -> String {
    if m.is_dir() {
        DIR_VERSION.to_string()
    } else {
        format!("{}-{}", mtime_ns(m), m.len())
    }
}

/// `a/b/c` -> (`a/b`, `c`); `c` -> (``, `c`).
fn split_pid(pid: &str) -> (&str, &str) {
    match pid.rsplit_once('/') {
        Some((p, n)) => (p, n),
        None => ("", pid),
    }
}

fn join_pid(parent: &str, name: &str) -> Pid {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// Is `pid` `root` or below it?
fn under(pid: &str, root: &str) -> bool {
    root.is_empty()
        || pid == root
        || (pid.len() > root.len() && pid.starts_with(root) && pid.as_bytes()[root.len()] == b'/')
}

fn cut_short(why: impl std::fmt::Display) -> BErr {
    BErr::Transient(format!("{CUT_SHORT}: {why}"))
}

fn is_cut_short(e: &BErr) -> bool {
    matches!(e, BErr::Transient(m) if m.starts_with(CUT_SHORT))
}

fn unreachable_err(why: &str) -> BErr {
    BErr::Transient(format!("the library folder is not reachable: {why}"))
}

/// A name the folder driver never lists (and never creates): Bunko's internal names,
/// Chromium's swap files, and at the top the names a filesystem keeps for itself.
pub(crate) fn hidden_name(name: &str, at_root: bool) -> bool {
    crate::names::is_reserved_name(name)
        || name.to_ascii_lowercase().ends_with(".crswap")
        || (at_root && ROOT_SYSTEM.iter().any(|s| s.eq_ignore_ascii_case(name)))
}

/// `candidate` is a Chromium swap file of `name`: `<name>.crswap`, or
/// `<name>.<n>.crswap` when that one was taken (a write left one behind).
fn is_swap_of(candidate: &str, name: &str) -> bool {
    let Some(rest) = candidate
        .strip_prefix(name)
        .and_then(|r| r.strip_prefix('.'))
    else {
        return false;
    };
    let lower = rest.to_ascii_lowercase();
    let Some(n) = lower.strip_suffix("crswap") else {
        return false;
    };
    n.is_empty()
        || n.strip_suffix('.')
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// How much longer an empty file last written at `mtime` is a write in progress, as of
/// `now`: `None` once it is not ([`EMPTY_SETTLE`] after its time; at once when its time
/// is more than [`CLOCK_AHEAD`] ahead of `now`).
fn unsettled_for(mtime: SystemTime, now: SystemTime) -> Option<Duration> {
    match now.duration_since(mtime) {
        Ok(age) => EMPTY_SETTLE.checked_sub(age).filter(|d| !d.is_zero()),
        Err(ahead) => {
            let ahead = ahead.duration();
            (ahead <= CLOCK_AHEAD).then(|| ahead + EMPTY_SETTLE)
        }
    }
}

/// One path segment the driver can address: a single normal component.
fn seg_ok(seg: &str) -> bool {
    if seg.is_empty() || seg == "." || seg == ".." || seg.contains(['/', '\0']) {
        return false;
    }
    if cfg!(windows) && seg.contains(['\\', ':']) {
        return false;
    }
    let mut c = Path::new(seg).components();
    matches!(
        (c.next(), c.next()),
        (Some(Component::Normal(part)), None) if part == OsStr::new(seg)
    )
}

/// Names Windows cannot hold: `<>:"|?*\`, control characters, a trailing dot or
/// space, device names.
fn windows_name_ok(name: &str) -> bool {
    !name
        .chars()
        .any(|c| "<>:\"|?*\\".contains(c) || (c as u32) < 0x20)
        && !name.ends_with(['.', ' '])
        && !crate::names::is_windows_device_name(name)
}

/// A name the driver may create (`at_root`: directly in the library folder).
fn check_name(name: &str, at_root: bool) -> Result<(), BErr> {
    if !seg_ok(name) || hidden_name(name, at_root) || (cfg!(windows) && !windows_name_ok(name)) {
        return Err(BErr::InvalidName);
    }
    Ok(())
}

fn node_of(pid: Pid, parent: Option<Pid>, name: &str, m: &Metadata) -> RNode {
    let is_dir = m.is_dir();
    let version = version_of(m);
    RNode {
        pid,
        parent,
        name_raw: name.as_bytes().to_vec(),
        is_dir,
        size: if is_dir { 0 } else { m.len() },
        base_mtime_ms: mtime_ns(m).div_euclid(1_000_000),
        created_ms: birth_ms(m),
        cver: version.clone(),
        version,
        pin: None,
        hash: None,
        trashed: false,
    }
}

/// `t` rounded up to a multiple of `g` ns.
fn ceil_to(t: SystemTime, g: i64) -> SystemTime {
    let g = g.max(1) as u128;
    let ns = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let up = ns.div_ceil(g) * g;
    UNIX_EPOCH + Duration::from_nanos(u64::try_from(up).unwrap_or(u64::MAX))
}

/// Rename `from` to `to` unless `to` exists (in any spelling the filesystem folds to
/// it): `AlreadyExists` then. Without a no-replace rename: a file is hard-linked then
/// unlinked; a folder (or a filesystem without links) is checked, then renamed.
pub(crate) fn rename_new(from: &Path, to: &Path, is_dir: bool) -> std::io::Result<()> {
    if fsops::rename_noreplace(from, to)? {
        return Ok(());
    }
    rename_new_fallback(from, to, is_dir, true)
}

/// [`rename_new`] where the filesystem has no rename that refuses to replace
/// (`links`: try a hard link first, for a file).
fn rename_new_fallback(from: &Path, to: &Path, is_dir: bool, links: bool) -> std::io::Result<()> {
    if links && !is_dir {
        match std::fs::hard_link(from, to) {
            Ok(()) => {
                return std::fs::remove_file(from).inspect_err(|_| {
                    // All or nothing: never two names.
                    let _ = std::fs::remove_file(to);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(e),
            Err(_) => {}
        }
    }
    match std::fs::symlink_metadata(to) {
        Ok(_) => Err(std::io::ErrorKind::AlreadyExists.into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::rename(from, to),
        Err(e) => Err(e),
    }
}

/// `path` with `~` expanded and `.`, `..` and a trailing separator removed, never
/// touching the disk. `..` never climbs above the root.
fn lexical(path: &Path) -> PathBuf {
    let expanded = bunko_core::storage::expand_user(path);
    let mut out = PathBuf::new();
    // Normal components pushed so far (a `..` pops only those, never the root).
    let mut depth = 0usize;
    for c in expanded.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if depth > 0 {
                    out.pop();
                    depth -= 1;
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            Component::Normal(s) => {
                out.push(s);
                depth += 1;
            }
        }
    }
    out
}

/// `p` with its longest existing ancestor resolved (symbolic links, on-disk case),
/// the rest appended as given.
fn resolve_existing(p: &Path) -> PathBuf {
    let p = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut rest = Vec::new();
    let mut cur = p.as_path();
    loop {
        if let Ok(c) = std::fs::canonicalize(cur) {
            let mut out = c;
            for r in rest.iter().rev() {
                out.push(r);
            }
            return out;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return p.clone(),
        }
    }
}

/// Run `f` on the blocking pool, giving up after `limit` (the thread is left to
/// finish on its own).
async fn blocking<T: Send + 'static>(
    limit: Duration,
    f: impl FnOnce() -> Result<T, BErr> + Send + 'static,
) -> Result<T, BErr> {
    match tokio::time::timeout(limit, tokio::task::spawn_blocking(f)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => Err(BErr::Transient(format!(
            "a library folder task failed: {e}"
        ))),
        Err(_) => Err(BErr::Transient("the library folder did not answer".into())),
    }
}

impl Inner {
    fn probe(&self) -> FolderProbe {
        self.probe.read().0.clone()
    }

    fn hidden(&self) -> probe::Hidden {
        self.probe.read().1
    }

    /// Whether a lookup can succeed in another spelling than the one on disk (or it
    /// is not known yet): `stat` then checks every segment against its folder.
    fn needs_exact(&self) -> bool {
        let g = self.probe.read();
        !g.1.ran || g.0.folds_case || g.0.rewrites_names || g.1.folds_unicode
    }

    fn granularity(&self) -> i64 {
        #[cfg(feature = "testing")]
        {
            let forced = self.knobs.granularity_ns.load(Ordering::Relaxed);
            if forced > 0 {
                return forced;
            }
        }
        self.probe.read().0.mtime_granularity_ns.max(1)
    }

    /// Run the probe (the folder is a directory); `again`: even when it ran before.
    fn run_probe(&self, again: bool) {
        let _g = self.probe_lock.lock();
        if !again && self.hidden().ran {
            return;
        }
        let (mut p, h) = probe::run(&self.root, &token(8));
        p.watcher = self.watch.running();
        tracing::info!(
            "folder: {} probed: {}",
            self.root.display(),
            serde_json::to_string(&p).unwrap_or_default()
        );
        *self.probe.write() = (p, h);
    }

    fn set_watcher(&self, on: bool) {
        self.probe.write().0.watcher = on;
    }

    fn root_state(&self) -> RootState {
        match std::fs::metadata(&self.root) {
            Ok(m) if m.is_dir() => {
                if !self.hidden().ran {
                    self.run_probe(false);
                }
                if !self.replayed.load(Ordering::Acquire) {
                    self.replay_once();
                }
                let (p, h) = (self.probe(), self.hidden());
                if p.mount_point
                    && let (Some(dev), Some(parent)) = (fsops::device_of(&m), self.root.parent())
                    && let Ok(pm) = std::fs::metadata(parent)
                    && fsops::device_of(&pm) == Some(dev)
                    && h.device != Some(dev)
                {
                    return RootState::Unreachable(format!(
                        "{} is not mounted",
                        self.root.display()
                    ));
                }
                RootState::Dir(m)
            }
            Ok(_) => self.missing_or_unreachable(None),
            Err(e) => self.missing_or_unreachable(Some(e)),
        }
    }

    fn missing_or_unreachable(&self, e: Option<std::io::Error>) -> RootState {
        if let Some(e) = &e
            && !matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            )
        {
            return RootState::Unreachable(format!("{}: {e}", self.root.display()));
        }
        match self.root.parent().map(std::fs::metadata) {
            Some(Ok(pm)) if pm.is_dir() => RootState::Missing,
            _ => RootState::Unreachable(self.root.display().to_string()),
        }
    }

    fn root_node(&self, m: &Metadata) -> RNode {
        RNode {
            pid: String::new(),
            parent: None,
            name_raw: self.root_name.as_bytes().to_vec(),
            is_dir: true,
            size: 0,
            base_mtime_ms: mtime_ns(m).div_euclid(1_000_000),
            created_ms: birth_ms(m),
            version: DIR_VERSION.into(),
            cver: DIR_VERSION.into(),
            pin: None,
            hash: None,
            trashed: false,
        }
    }

    /// The absolute path of `pid`; `None` for a pid no listing could have produced.
    fn abs(&self, pid: &str) -> Option<PathBuf> {
        let mut p = self.root.clone();
        if pid.is_empty() {
            return Some(p);
        }
        for seg in pid.split('/') {
            if !seg_ok(seg) {
                return None;
            }
            p.push(seg);
        }
        Some(p)
    }

    fn abs_or_missing(&self, pid: &str) -> Result<PathBuf, BErr> {
        self.abs(pid).ok_or(BErr::NotFound)
    }

    /// Our own change: folder listings read before it are stale.
    fn forget_dirs(&self) {
        self.dirs_gen.fetch_add(1, Ordering::AcqRel);
        self.dirs.lock().clear();
    }

    /// The names in `dir` (cached for [`DIR_CACHE_TTL`]).
    fn dir_names(&self, dir: &Path) -> std::io::Result<Arc<HashSet<OsString>>> {
        if let Some((at, names)) = self.dirs.lock().get(dir)
            && at.elapsed() < DIR_CACHE_TTL
        {
            return Ok(names.clone());
        }
        let generation = self.dirs_gen.load(Ordering::Acquire);
        let mut names = HashSet::new();
        for e in std::fs::read_dir(dir)? {
            names.insert(e?.file_name());
        }
        let names = Arc::new(names);
        let mut g = self.dirs.lock();
        if self.dirs_gen.load(Ordering::Acquire) == generation {
            if g.len() > 4096 {
                g.clear();
            }
            g.insert(dir.to_path_buf(), (Instant::now(), names.clone()));
        }
        Ok(names)
    }

    /// Whether the way from the root down to `pid`'s parent holds only real folders.
    ///
    /// `O_NOFOLLOW` and `lstat` look at a path's last segment only: through a folder
    /// link (a symbolic link, or a junction or another name-surrogate reparse point on
    /// Windows, which `FileType::is_symlink` covers) further up, a read, a write, a
    /// rename, a trash or a delete would reach a folder outside the library. Every
    /// operation reaches its paths through [`Inner::exact`], which asks this first; the
    /// walk never descends into such a link either (it lists links as nothing).
    ///
    /// A check by `lstat` per segment, not a chain of `openat(O_NOFOLLOW)`: it works
    /// the same on Windows, and the operations stay on the path-based calls the rest
    /// of the driver uses. What it leaves is the moment between the check and the
    /// call: a folder swapped for a link in that window by someone who can already
    /// write in the library folder.
    fn way_to(&self, pid: &str) -> std::io::Result<Way> {
        let mut p = self.root.clone();
        let segs: Vec<&str> = pid.split('/').collect();
        for seg in &segs[..segs.len().saturating_sub(1)] {
            if !seg_ok(seg) {
                return Ok(Way::Blocked);
            }
            p.push(seg);
            match std::fs::symlink_metadata(&p) {
                Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
                Ok(_) => return Ok(Way::Blocked),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    return Ok(Way::Missing);
                }
                // A folder Bunko may not enter: what is below it is out of its reach.
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    return Ok(Way::Blocked);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(Way::Clear)
    }

    /// Nothing is at a pid: `None`, unless the library folder itself is away.
    fn absent(&self) -> Result<Option<Metadata>, BErr> {
        match self.root_state() {
            RootState::Unreachable(why) => Err(unreachable_err(&why)),
            _ => Ok(None),
        }
    }

    /// The node at `pid` in exactly this spelling, as listings show it: `None` when it
    /// is absent, hidden, a symbolic link or below one ([`Inner::way_to`]), neither file
    /// nor folder, or spelled otherwise on disk.
    fn exact(&self, pid: &str) -> Result<Option<Metadata>, BErr> {
        if pid.is_empty() {
            return match self.root_state() {
                RootState::Dir(m) => Ok(Some(m)),
                RootState::Missing => Ok(None),
                RootState::Unreachable(why) => Err(unreachable_err(&why)),
            };
        }
        let Some(abs) = self.abs(pid) else {
            return Ok(None);
        };
        let segs: Vec<&str> = pid.split('/').collect();
        if segs.iter().enumerate().any(|(i, s)| hidden_name(s, i == 0)) {
            return Ok(None);
        }
        match self.way_to(pid) {
            Ok(Way::Clear) => {}
            Ok(Way::Missing) => return self.absent(),
            Ok(Way::Blocked) => return Ok(None),
            Err(e) => return Err(io_err(&e, Act::Read, pid)),
        }
        let m = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return self.absent();
            }
            // A folder Bunko may not enter: what is below it is out of its reach.
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(None),
            Err(e) => return Err(io_err(&e, Act::Read, pid)),
        };
        if m.file_type().is_symlink() || !(m.is_file() || m.is_dir()) {
            return Ok(None);
        }
        if self.needs_exact() {
            let mut dir = self.root.clone();
            for seg in &segs {
                match self.dir_names(&dir) {
                    Ok(names) if names.contains(OsStr::new(seg)) => {}
                    Ok(_) => return Ok(None),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(io_err(&e, Act::Read, pid)),
                }
                dir.push(seg);
            }
        }
        let (dir, name) = (abs.parent().unwrap_or(&self.root), split_pid(pid).1);
        let writing = self.writing(pid, &m, || {
            std::fs::read_dir(dir).is_ok_and(|rd| {
                rd.flatten()
                    .any(|e| e.file_name().to_str().is_some_and(|n| is_swap_of(n, name)))
            })
        });
        if writing {
            return Ok(None);
        }
        Ok(Some(m))
    }

    /// The file at `pid` (metadata `m`) is a reader's write in progress, listed by
    /// nothing: an empty file while a swap file beside it exists (`swapping` asks;
    /// a slow write keeps it however long it takes), or one written less than
    /// [`EMPTY_SETTLE`] ago that Bunko did not write itself (Chromium makes the empty
    /// target before the swap file). An older empty file is a file.
    fn writing(&self, pid: &str, m: &Metadata, swapping: impl FnOnce() -> bool) -> bool {
        if !(m.is_file() && m.len() == 0) {
            return false;
        }
        if let Ok(t) = m.modified()
            && let Some(left) = unsettled_for(t, SystemTime::now())
            // Bunko's own empty files are in the memo (an upload's or copy's version).
            && self.memo.get(pid, &version_of(m)).is_none()
        {
            self.settles_in(left);
            return true;
        }
        swapping()
    }

    /// An empty file held back settles in `left`: the feed looks again by then.
    /// The earliest such time is kept, even once past: a file that settled before the
    /// feed got to its wait is due a look at once ([`Inner::snapshot`] starts afresh).
    fn settles_in(&self, left: Duration) {
        let at = Instant::now() + left + SETTLE_SLACK;
        let mut g = self.settles.lock();
        if g.is_none_or(|t| at < t) {
            *g = Some(at);
        }
    }

    /// `max`, or less when an empty file held back settles sooner: nothing when one
    /// has settled since the last walk began.
    fn until_settled(&self, max: Duration) -> Duration {
        let mut g = self.settles.lock();
        match *g {
            None => max,
            Some(t) => {
                let left = t.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    *g = None;
                }
                max.min(left)
            }
        }
    }

    /// A folder (`dir`, at `dir_abs`) Bunko must be able to write into.
    fn exact_dir(&self, dir: &str) -> Result<PathBuf, BErr> {
        match self.exact(dir)? {
            Some(m) if m.is_dir() => self.abs_or_missing(dir),
            _ => Err(BErr::NotFound),
        }
    }

    fn with_hash(&self, mut n: RNode) -> RNode {
        if !n.is_dir
            && let Some(h) = self.memo.get(&n.pid, &n.cver)
        {
            n.hash = Some((HashKind::Sha256, h));
        }
        n
    }

    /// The node at an exact `pid` with metadata `m`.
    fn node_at(&self, pid: &str, m: &Metadata) -> RNode {
        if pid.is_empty() {
            return self.root_node(m);
        }
        let (parent, name) = split_pid(pid);
        node_of(pid.to_string(), Some(parent.to_string()), name, m)
    }

    /// The listed children of the folder `dir_pid` (at `dir_abs`), sorted by name.
    fn list_children(&self, dir_abs: &Path, dir_pid: &str) -> std::io::Result<Vec<RNode>> {
        let at_root = dir_pid.is_empty();
        let mut all: HashSet<String> = HashSet::new();
        let mut kept: Vec<(String, std::fs::DirEntry)> = Vec::new();
        let mut not_utf8 = false;
        for e in std::fs::read_dir(dir_abs)? {
            let e = e?;
            let Ok(name) = e.file_name().into_string() else {
                not_utf8 = true;
                continue;
            };
            all.insert(name.clone());
            let Ok(t) = e.file_type() else { continue };
            if t.is_symlink() || !(t.is_file() || t.is_dir()) || hidden_name(&name, at_root) {
                continue;
            }
            kept.push((name, e));
        }
        if not_utf8 && self.warned.lock().insert(dir_abs.to_path_buf()) {
            tracing::warn!(
                "folder: {} holds names that are not UTF-8; they are skipped",
                dir_abs.display()
            );
        }
        let mut out = Vec::with_capacity(kept.len());
        for (name, e) in kept {
            // Without following links (lstat on Unix, the directory data on Windows).
            let Ok(m) = e.metadata() else { continue };
            if m.file_type().is_symlink() {
                continue;
            }
            let pid = join_pid(dir_pid, &name);
            if self.writing(&pid, &m, || all.iter().any(|n| is_swap_of(n, &name))) {
                continue;
            }
            out.push(node_of(pid, Some(dir_pid.to_string()), &name, &m));
        }
        out.sort_by(|a, b| a.name_raw.cmp(&b.name_raw));
        Ok(out)
    }

    /// Everything below `top` (the folder `top_pid`), parents before children. A folder
    /// that vanishes mid-walk is skipped; one that cannot be read keeps what `carry`
    /// held below it (`carried`), or is skipped with a warning without one.
    fn walk(
        &self,
        top_pid: &str,
        top_abs: PathBuf,
        carry: Option<&Generation>,
    ) -> Result<(Vec<RNode>, Entries), BErr> {
        let mut nodes = Vec::new();
        let mut carried = Entries::new();
        let mut stack = vec![(top_pid.to_string(), top_abs)];
        let mut first = true;
        while let Some((pid, abs)) = stack.pop() {
            let kids = match self.list_children(&abs, &pid) {
                Ok(k) => k,
                Err(e) if first => {
                    return Err(match e.kind() {
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                            BErr::NotFound
                        }
                        _ => BErr::Transient(format!(
                            "cannot list {}: {e}",
                            if pid.is_empty() {
                                "the library folder"
                            } else {
                                &pid
                            }
                        )),
                    });
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue;
                }
                Err(e) => {
                    tracing::warn!("folder: cannot list {pid}: {e}");
                    if let Some(g) = carry {
                        snap::copy_subtree(g, &pid, &mut carried);
                    }
                    continue;
                }
            };
            first = false;
            let mut subdirs = Vec::new();
            for n in kids {
                if n.is_dir {
                    let mut p = abs.clone();
                    p.push(String::from_utf8_lossy(&n.name_raw).as_ref());
                    subdirs.push((n.pid.clone(), p));
                }
                nodes.push(n);
            }
            subdirs.reverse();
            stack.extend(subdirs);
        }
        Ok((nodes, carried))
    }

    /// The whole library (the root included) as a generation's entries, and its nodes.
    /// A missing folder is an empty one; one that is away fails.
    fn snapshot(&self, carry: Option<&Generation>) -> Result<(Vec<RNode>, Entries), BErr> {
        // This walk sees every file held back: it notes again those still unsettled.
        *self.settles.lock() = None;
        let m = match self.root_state() {
            RootState::Dir(m) => m,
            RootState::Missing => return Ok((Vec::new(), Entries::new())),
            RootState::Unreachable(why) => {
                self.watch.lost();
                return Err(unreachable_err(&why));
            }
        };
        let (mut nodes, carried) = match self.walk("", self.root.clone(), carry) {
            Ok(r) => r,
            Err(BErr::NotFound) => {
                return match self.root_state() {
                    RootState::Missing => Ok((Vec::new(), Entries::new())),
                    RootState::Unreachable(why) => Err(unreachable_err(&why)),
                    RootState::Dir(_) => Err(cut_short("the library folder vanished")),
                };
            }
            Err(e) => return Err(e),
        };
        nodes.insert(0, self.root_node(&m));
        let mut entries = carried;
        for n in &nodes {
            entries.insert(n.pid.clone(), (n.version.clone(), n.is_dir));
        }
        Ok((nodes, entries))
    }

    /// `removed` is what `base` held and the walk (`listed`) no longer shows: the top
    /// of each missing subtree must really be gone. Not asked about: what went with
    /// its folder, and a name the walk shows in another spelling.
    fn confirm_removed(&self, listed: &Entries, removed: &[Pid]) -> Result<(), BErr> {
        if removed.is_empty() {
            return Ok(());
        }
        let shown: HashSet<String> = listed.keys().map(|p| fold_path_pid(p)).collect();
        let unexplained: BTreeSet<&str> = removed
            .iter()
            .map(String::as_str)
            .filter(|p| !shown.contains(&fold_path_pid(p)))
            .collect();
        for pid in &unexplained {
            if !pid.is_empty() && unexplained.contains(split_pid(pid).0) {
                continue;
            }
            if self.exact(pid)?.is_some() {
                return Err(cut_short(format!(
                    "it left out {pid:?}, which is still there"
                )));
            }
        }
        Ok(())
    }

    /// The node just created as `name` in `parent` (at `parent_abs`), in its on-disk
    /// spelling (read back from the folder where the filesystem rewrites names).
    fn read_back(&self, parent: &str, parent_abs: &Path, name: &str) -> Result<RNode, BErr> {
        let mut on_disk = name.to_string();
        if self.probe().rewrites_names {
            let want = nfc(name.as_bytes());
            let made = parent_abs.join(name);
            if let Ok(rd) = std::fs::read_dir(parent_abs) {
                for e in rd.flatten() {
                    if let Ok(n) = e.file_name().into_string()
                        && nfc(n.as_bytes()) == want
                        && fsops::same_file(&e.path(), &made).unwrap_or(false)
                    {
                        on_disk = n;
                        break;
                    }
                }
            }
        }
        let pid = join_pid(parent, &on_disk);
        let m = std::fs::symlink_metadata(parent_abs.join(&on_disk))
            .map_err(|e| BErr::Transient(format!("{pid} is not visible after the write: {e}")))?;
        Ok(node_of(pid, Some(parent.to_string()), &on_disk, &m))
    }

    /// Copy the staged file to the new `tmp` while hashing it, then give it its time
    /// and make it durable. The bytes must be the staged ones (size and sha256).
    fn write_temp(
        &self,
        tmp: &Path,
        src: &Path,
        size: u64,
        sha256: [u8; 32],
        bump_from: Option<SystemTime>,
        rel: &str,
    ) -> Result<(), BErr> {
        let mut input = std::fs::File::open(src).map_err(|e| {
            BErr::Fatal(format!(
                "cannot read the staged file {}: {e}",
                src.display()
            ))
        })?;
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(tmp)
            .map_err(|e| io_err(&e, Act::Create, rel))?;
        let mut h = Sha256::new();
        let mut n = 0u64;
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            let k = input.read(&mut buf).map_err(|e| {
                BErr::Transient(format!("reading the staged file {}: {e}", src.display()))
            })?;
            if k == 0 {
                break;
            }
            h.update(&buf[..k]);
            out.write_all(&buf[..k])
                .map_err(|e| io_err(&e, Act::Create, rel))?;
            n += k as u64;
        }
        if n != size || <[u8; 32]>::from(h.finalize()) != sha256 {
            return Err(BErr::Transient(format!(
                "the staged file for {rel} changed while it was copied"
            )));
        }
        // An explicit, precise time: the kernel's own write stamps can be a tick
        // coarse, and a filesystem keeping whole seconds (or two) would give a
        // same-size replace inside one tick the old version.
        let g = self.granularity();
        let mut t = ceil_to(SystemTime::now(), g);
        if let Some(old) = bump_from {
            t = t.max(old + Duration::from_nanos(g as u64));
        }
        if let Err(e) = out.set_modified(t) {
            tracing::debug!("folder: cannot set the time of {rel}: {e}");
        }
        out.sync_all().map_err(|e| io_err(&e, Act::Create, rel))?;
        Ok(())
    }

    fn temp_in(dir: &Path) -> PathBuf {
        dir.join(format!(".bunko-{}.tmp", token(16)))
    }

    /// Why a create of `name` in `parent_abs` found the name taken.
    fn taken(&self, parent_abs: &Path, name: &str) -> BErr {
        let want = fold(&nfc(name.as_bytes()));
        let same = nfc(name.as_bytes());
        let mut file_same_spelling = false;
        if let Ok(rd) = std::fs::read_dir(parent_abs) {
            for e in rd.flatten() {
                let Ok(n) = e.file_name().into_string() else {
                    continue;
                };
                if fold(&nfc(n.as_bytes())) != want {
                    continue;
                }
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    return BErr::NameConflict;
                }
                if nfc(n.as_bytes()) == same {
                    file_same_spelling = true;
                }
            }
        }
        if file_same_spelling {
            BErr::PreconditionFailed
        } else {
            BErr::NameConflict
        }
    }

    fn upload_new(
        &self,
        parent: &str,
        name: &str,
        src: &Path,
        size: u64,
        sha256: [u8; 32],
    ) -> Result<RNode, BErr> {
        check_name(name, parent.is_empty())?;
        let parent_abs = self.exact_dir(parent)?;
        let pid = join_pid(parent, name);
        let dest = parent_abs.join(name);
        let tmp = Self::temp_in(&parent_abs);
        let r = (|| {
            self.write_temp(&tmp, src, size, sha256, None, &pid)?;
            let cver = std::fs::symlink_metadata(&tmp)
                .map(|m| version_of(&m))
                .map_err(|e| io_err(&e, Act::Create, &pid))?;
            self.memo.put(&pid, &cver, &hex::encode(sha256));
            match rename_new(&tmp, &dest, false) {
                Ok(()) => Ok(cver),
                Err(e) => {
                    self.memo.forget(&pid, &cver);
                    Err(if e.kind() == std::io::ErrorKind::AlreadyExists {
                        self.taken(&parent_abs, name)
                    } else {
                        io_err(&e, Act::Rename, &pid)
                    })
                }
            }
        })();
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        let cver = r?;
        self.forget_dirs();
        fsops::fsync_dir(&parent_abs);
        self.memo.keep_only(&pid, &cver);
        let node = self.read_back(parent, &parent_abs, name)?;
        if node.pid != pid {
            self.memo.rebase(&pid, &node.pid);
        }
        Ok(self.with_hash(node))
    }

    fn upload_replace(
        &self,
        pid: &str,
        if_cver: Option<&str>,
        src: &Path,
        size: u64,
        sha256: [u8; 32],
    ) -> Result<RNode, BErr> {
        let old = match self.exact(pid)? {
            Some(m) if m.is_file() => m,
            _ => return Err(BErr::NotFound),
        };
        let old_version = version_of(&old);
        if if_cver.is_some_and(|want| want != old_version) {
            return Err(BErr::PreconditionFailed);
        }
        let abs = self.abs_or_missing(pid)?;
        let parent_abs = abs.parent().map(Path::to_path_buf).ok_or(BErr::NotFound)?;
        let tmp = Self::temp_in(&parent_abs);
        let r = (|| {
            self.write_temp(&tmp, src, size, sha256, old.modified().ok(), pid)?;
            let cver = std::fs::symlink_metadata(&tmp)
                .map(|m| version_of(&m))
                .map_err(|e| io_err(&e, Act::Create, pid))?;
            #[cfg(feature = "testing")]
            if let Some(f) = self.knobs.before_replace.lock().take() {
                f();
            }
            // Nothing is compare-and-swap here, but the copy took a while: a reader
            // that wrote the file meanwhile keeps its bytes (the outbox sees the
            // conflict), and the window left is this stat to the rename.
            match std::fs::symlink_metadata(&abs) {
                Ok(now) if now.is_file() && version_of(&now) == old_version => {}
                Ok(_) => return Err(BErr::PreconditionFailed),
                Err(e) => return Err(io_err(&e, Act::Read, pid)),
            }
            self.memo.put(pid, &cver, &hex::encode(sha256));
            if let Err(e) = std::fs::rename(&tmp, &abs) {
                self.memo.forget(pid, &cver);
                return Err(io_err(&e, Act::Rename, pid));
            }
            Ok(cver)
        })();
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        let cver = r?;
        self.forget_dirs();
        fsops::fsync_dir(&parent_abs);
        self.memo.keep_only(pid, &cver);
        let m = std::fs::symlink_metadata(&abs)
            .map_err(|e| BErr::Transient(format!("{pid} is not visible after the write: {e}")))?;
        Ok(self.with_hash(self.node_at(pid, &m)))
    }

    fn mkdir(&self, parent: &str, name: &str) -> Result<RNode, BErr> {
        check_name(name, parent.is_empty())?;
        let parent_abs = self.exact_dir(parent)?;
        let pid = join_pid(parent, name);
        std::fs::create_dir(parent_abs.join(name)).map_err(|e| io_err(&e, Act::Create, &pid))?;
        self.forget_dirs();
        fsops::fsync_dir(&parent_abs);
        self.read_back(parent, &parent_abs, name)
    }

    /// The path `pid` resolves to segment by segment under the filesystem's folding
    /// (NFC + case), whatever its spelling: `None` when nothing matches.
    fn resolve_folded(&self, pid: &str) -> Option<PathBuf> {
        let mut cur = self.root.clone();
        for seg in pid.split('/') {
            let want = fold(&nfc(seg.as_bytes()));
            let hit = std::fs::read_dir(&cur).ok()?.flatten().find_map(|e| {
                // Never through a link (one could lead out of the library).
                if !e.file_type().is_ok_and(|t| !t.is_symlink()) {
                    return None;
                }
                let n = e.file_name().into_string().ok()?;
                (fold(&nfc(n.as_bytes())) == want).then_some(n)
            })?;
            cur.push(hit);
        }
        Some(cur)
    }

    /// The source of a move is gone: when it went to `dest` (a case-only rename that
    /// finished before a crash), the move is done.
    fn already_moved(&self, pid: &str, dest: &str) -> Result<RNode, BErr> {
        let Some(dm) = self.exact(dest)? else {
            return Err(BErr::NotFound);
        };
        let dest_abs = self.abs_or_missing(dest)?;
        match self.resolve_folded(pid) {
            Some(p) if fsops::same_file(&p, &dest_abs).unwrap_or(false) => {
                Ok(self.with_hash(self.node_at(dest, &dm)))
            }
            _ => Err(BErr::NotFound),
        }
    }

    /// `from` → `to` in one folder where the two names differ in case or Unicode form
    /// only: through `.bunko-case-<token>.tmp`, written to the journal first.
    fn case_rename(
        &self,
        parent: &str,
        parent_abs: &Path,
        from: &str,
        to: &str,
        is_dir: bool,
    ) -> Result<(), BErr> {
        let _g = self.case_lock.lock();
        let (src, dest) = (parent_abs.join(from), parent_abs.join(to));
        match std::fs::symlink_metadata(&dest) {
            // Both spellings exist (a case-sensitive host): two names.
            Ok(_) if !fsops::same_file(&src, &dest).unwrap_or(false) => {
                return Err(BErr::NameConflict);
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&e, Act::Rename, &join_pid(parent, to))),
        }
        let temp_name = format!(".bunko-case-{}.tmp", token(16));
        let temp = parent_abs.join(&temp_name);
        let entry = journal::Entry {
            temp: join_pid(parent, &temp_name),
            from: join_pid(parent, from),
            to: join_pid(parent, to),
        };
        journal::write(&self.state_dir, std::slice::from_ref(&entry))
            .map_err(|e| BErr::Transient(format!("cannot write the rename journal: {e}")))?;
        if let Err(e) = std::fs::rename(&src, &temp) {
            let _ = journal::write(&self.state_dir, &[]);
            return Err(io_err(&e, Act::Rename, &entry.from));
        }
        #[cfg(feature = "testing")]
        if self.knobs.stop_case_rename.swap(false, Ordering::AcqRel) {
            return Err(BErr::Transient(
                "test: stopped inside a case-only rename".into(),
            ));
        }
        if let Err(e) = rename_new(&temp, &dest, is_dir) {
            // Put it back where it was; the journal stays if even that fails.
            if std::fs::rename(&temp, &src).is_ok() {
                let _ = journal::write(&self.state_dir, &[]);
            }
            return Err(if e.kind() == std::io::ErrorKind::AlreadyExists {
                BErr::NameConflict
            } else {
                io_err(&e, Act::Rename, &entry.to)
            });
        }
        fsops::fsync_dir(parent_abs);
        if let Err(e) = journal::write(&self.state_dir, &[]) {
            tracing::warn!("folder: cannot clear the rename journal: {e}");
        }
        Ok(())
    }

    /// [`Inner::replay_journal`], once, before any case-only rename can start.
    fn replay_once(&self) {
        let _g = self.case_lock.lock();
        if !self.replayed.swap(true, Ordering::AcqRel) {
            self.replay_journal();
        }
    }

    /// Finish the case-only renames a crash interrupted.
    fn replay_journal(&self) {
        let entries = journal::read(&self.state_dir);
        if entries.is_empty() {
            return;
        }
        for e in &entries {
            let (Some(temp), Some(from), Some(to)) =
                (self.abs(&e.temp), self.abs(&e.from), self.abs(&e.to))
            else {
                continue;
            };
            // Through real folders only, as every other operation.
            if !matches!(self.way_to(&e.temp), Ok(Way::Clear))
                || std::fs::symlink_metadata(&temp).is_err()
            {
                continue;
            }
            let is_dir = std::fs::symlink_metadata(&temp).is_ok_and(|m| m.is_dir());
            match rename_new(&temp, &to, is_dir) {
                Ok(()) => tracing::info!("folder: finished the rename of {} to {}", e.from, e.to),
                Err(_) => match rename_new(&temp, &from, is_dir) {
                    Ok(()) => {
                        tracing::warn!("folder: {} was taken; {} keeps its old name", e.to, e.from)
                    }
                    Err(err) => tracing::warn!(
                        "folder: cannot finish the rename of {} ({}): {err}",
                        e.from,
                        e.temp
                    ),
                },
            }
        }
        self.forget_dirs();
        if let Err(err) = journal::write(&self.state_dir, &[]) {
            tracing::warn!("folder: cannot clear the rename journal: {err}");
        }
    }

    fn update(
        &self,
        pid: &str,
        new_parent: Option<&str>,
        new_name: Option<&str>,
        if_cver: Option<&str>,
    ) -> Result<RNode, BErr> {
        if pid.is_empty() {
            return Err(BErr::Unsupported);
        }
        let (cur_parent, cur_name) = split_pid(pid);
        let to_parent = new_parent.unwrap_or(cur_parent);
        let to_name = new_name.unwrap_or(cur_name);
        check_name(to_name, to_parent.is_empty())?;
        let dest = join_pid(to_parent, to_name);
        let Some(m) = self.exact(pid)? else {
            return self.already_moved(pid, &dest);
        };
        let to_parent_abs = self.exact_dir(to_parent)?;
        if under(to_parent, pid) {
            return Err(BErr::Fatal(format!("cannot move {pid} into itself")));
        }
        if let Some(want) = if_cver
            && m.is_file()
            && version_of(&m) != want
        {
            return Err(BErr::PreconditionFailed);
        }
        if dest == pid {
            return Ok(self.with_hash(self.node_at(pid, &m)));
        }
        let src_abs = self.abs_or_missing(pid)?;
        let case_only = to_parent == cur_parent
            && fold(&nfc(to_name.as_bytes())) == fold(&nfc(cur_name.as_bytes()));
        if case_only {
            self.case_rename(cur_parent, &to_parent_abs, cur_name, to_name, m.is_dir())?;
        } else {
            rename_new(&src_abs, &to_parent_abs.join(to_name), m.is_dir()).map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    BErr::NameConflict
                } else {
                    io_err(&e, Act::Rename, pid)
                }
            })?;
            fsops::fsync_dir(&to_parent_abs);
            if let Some(p) = src_abs.parent() {
                fsops::fsync_dir(p);
            }
        }
        self.forget_dirs();
        let node = self.read_back(to_parent, &to_parent_abs, to_name)?;
        self.memo.rebase(pid, &node.pid);
        Ok(self.with_hash(node))
    }

    fn copy(&self, pid: &str, parent: &str, name: &str) -> Result<RNode, BErr> {
        check_name(name, parent.is_empty())?;
        let sm = self.exact(pid)?.ok_or(BErr::NotFound)?;
        if sm.is_dir() {
            return Err(BErr::Unsupported);
        }
        let src_abs = self.abs_or_missing(pid)?;
        let parent_abs = self.exact_dir(parent)?;
        let dest = join_pid(parent, name);
        let tmp = Self::temp_in(&parent_abs);
        let r = (|| {
            std::fs::copy(&src_abs, &tmp).map_err(|e| io_err(&e, Act::Create, &dest))?;
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&tmp) {
                let _ = f.sync_all();
            }
            let made =
                std::fs::symlink_metadata(&tmp).map_err(|e| io_err(&e, Act::Create, &dest))?;
            let cver = version_of(&made);
            // An empty copy's hash is known; the memo then also marks it Bunko's own
            // (`Inner::writing`), its time being new.
            let hash = if made.len() == 0 {
                Some(hex::encode(Sha256::digest(b"")))
            } else {
                self.memo.get(pid, &version_of(&sm))
            };
            if let Some(h) = &hash {
                self.memo.put(&dest, &cver, h);
            }
            match rename_new(&tmp, &parent_abs.join(name), false) {
                Ok(()) => Ok(cver),
                Err(e) => {
                    self.memo.forget(&dest, &cver);
                    Err(if e.kind() == std::io::ErrorKind::AlreadyExists {
                        BErr::NameConflict
                    } else {
                        io_err(&e, Act::Rename, &dest)
                    })
                }
            }
        })();
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        let cver = r?;
        self.forget_dirs();
        fsops::fsync_dir(&parent_abs);
        self.memo.keep_only(&dest, &cver);
        let node = self.read_back(parent, &parent_abs, name)?;
        if node.pid != dest {
            // A filesystem that rewrote the name: the memo follows its spelling.
            self.memo.rebase(&dest, &node.pid);
        }
        Ok(self.with_hash(node))
    }

    fn trash(&self, pid: &str, if_cver: Option<&str>) -> Result<(), BErr> {
        if pid.is_empty() {
            return Err(BErr::Unsupported);
        }
        let m = self.exact(pid)?.ok_or(BErr::NotFound)?;
        if let Some(want) = if_cver
            && m.is_file()
            && version_of(&m) != want
        {
            return Err(BErr::PreconditionFailed);
        }
        let abs = self.abs_or_missing(pid)?;
        let r = trash::trash(
            &self.root,
            pid,
            &abs,
            m.is_dir(),
            SystemTime::now(),
            &token(8),
        );
        self.forget_dirs();
        r?;
        self.memo.drop_tree(pid);
        Ok(())
    }

    fn delete_permanent(&self, pid: &str) -> Result<(), BErr> {
        if pid.is_empty() {
            return Err(BErr::Unsupported);
        }
        let m = self.exact(pid)?.ok_or(BErr::NotFound)?;
        if m.is_dir() {
            return Err(BErr::Unsupported);
        }
        // The engine judged the version it saw Bunko's own; a reader may have put its
        // own bytes there since. Bunko wrote this pid before but not this version: it
        // is someone else's, and goes to the trash instead (D16).
        if self.memo.wrote_any(pid) == Some(true) && self.memo.get(pid, &version_of(&m)).is_none() {
            tracing::info!(
                "folder: {pid} changed since it was judged Bunko's: trashed, not deleted"
            );
            return self.trash(pid, None);
        }
        let abs = self.abs_or_missing(pid)?;
        std::fs::remove_file(&abs).map_err(|e| io_err(&e, Act::Delete, pid))?;
        self.forget_dirs();
        if let Some(p) = abs.parent() {
            fsops::fsync_dir(p);
        }
        self.memo.drop_tree(pid);
        Ok(())
    }

    fn purge_if_due(&self) {
        {
            let mut last = self.last_purge.lock();
            if last.is_some_and(|t| t.elapsed() < PURGE_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        let n = trash::purge(&self.root, SystemTime::now());
        if n > 0 {
            tracing::info!(
                "folder: purged {n} trash batches older than {} days",
                trash::KEEP_DAYS
            );
        }
    }
}

/// Remove the staged uploads (`.bunko-*.tmp`) and probe folders a crash left, older
/// than [`LEFTOVER_AGE`]. A case-rename stop (`.bunko-case-*`) is never removed: it
/// may be the only name a file has.
fn sweep_leftovers(root: &Path) {
    let old = |m: &Metadata| {
        m.modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > LEFTOVER_AGE)
    };
    let mut stack = vec![(root.to_path_buf(), true)];
    let mut gone = 0usize;
    while let Some((dir, at_root)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(name) = e.file_name().into_string() else {
                continue;
            };
            let Ok(t) = e.file_type() else { continue };
            let lower = name.to_ascii_lowercase();
            if t.is_dir() {
                if at_root && lower.starts_with(".bunko-probe-") {
                    if e.metadata().is_ok_and(|m| old(&m))
                        && std::fs::remove_dir_all(e.path()).is_ok()
                    {
                        gone += 1;
                    }
                } else if !t.is_symlink() && lower != crate::names::FOLDER_TRASH_DIR {
                    stack.push((e.path(), false));
                }
                continue;
            }
            if lower.starts_with(".bunko-")
                && !lower.starts_with(".bunko-case-")
                && lower.ends_with(".tmp")
                && e.metadata().is_ok_and(|m| old(&m))
                && std::fs::remove_file(e.path()).is_ok()
            {
                gone += 1;
            }
        }
    }
    if gone > 0 {
        tracing::info!("folder: removed {gone} leftovers of interrupted writes");
    }
}

impl FolderBackend {
    /// Opens the folder library. Never fails because the folder or its drive is
    /// missing: the store opens and reports offline until it is back. Fails
    /// ([`BErr::Fatal`]) on a path that is not absolute, or a state directory and
    /// library folder that contain one another.
    pub async fn connect(cfg: FolderConfig) -> Result<FolderBackend, BErr> {
        if !cfg.path.is_absolute() {
            return Err(BErr::Fatal(format!(
                "the library folder must be a full path: {}",
                cfg.path.display()
            )));
        }
        let root = lexical(&cfg.path);
        let state_dir = std::path::absolute(&cfg.state_dir).unwrap_or(cfg.state_dir.clone());
        {
            let (root, state_dir) = (root.clone(), state_dir.clone());
            let r = blocking(OP_LIMIT, move || {
                let (r, s) = (resolve_existing(&root), resolve_existing(&state_dir));
                if s.starts_with(&r) || r.starts_with(&s) {
                    return Err(BErr::Fatal(format!(
                        "Bunko's state folder ({}) and the library folder ({}) must not contain one another",
                        state_dir.display(),
                        root.display()
                    )));
                }
                Ok(())
            })
            .await;
            match r {
                Err(BErr::Fatal(m)) => return Err(BErr::Fatal(m)),
                Err(e) => tracing::warn!("folder: cannot check where the library folder is: {e}"),
                Ok(()) => {}
            }
        }
        let memo_dir = state_dir.clone();
        let memo = tokio::task::spawn_blocking(move || memo::Memo::open(&memo_dir))
            .await
            .map_err(|e| BErr::Fatal(e.to_string()))?;
        let inner = Arc::new(Inner {
            watch: watch::Watch::new(root.clone()),
            root,
            root_name: cfg.root_name,
            state_dir,
            watch_on: cfg.watch,
            probe: RwLock::new((FolderProbe::default(), probe::Hidden::default())),
            probe_lock: Mutex::new(()),
            memo,
            dirs: Mutex::new(HashMap::new()),
            dirs_gen: AtomicU64::new(0),
            case_lock: Mutex::new(()),
            replayed: std::sync::atomic::AtomicBool::new(false),
            warned: Mutex::new(HashSet::new()),
            last_purge: Mutex::new(None),
            settles: Mutex::new(None),
            #[cfg(feature = "testing")]
            knobs: Knobs::default(),
        });
        let b = FolderBackend {
            inner,
            snap: tokio::sync::Mutex::new(None),
            marks: Mutex::new(VecDeque::new()),
        };
        let r = b
            .run(OP_LIMIT, |i| {
                // The first sight of the folder probes it and finishes any
                // interrupted case-only rename.
                if let RootState::Dir(_) = i.root_state()
                    && i.watch_on
                {
                    i.purge_if_due();
                    let root = i.root.clone();
                    let _ = std::thread::Builder::new()
                        .name("bunko-folder-sweep".into())
                        .spawn(move || sweep_leftovers(&root));
                }
                Ok(())
            })
            .await;
        if let Err(e) = r {
            tracing::warn!("folder: {} at connect: {e}", b.inner.root.display());
        }
        if b.inner.watch_on {
            b.start_watch().await;
        }
        Ok(b)
    }

    /// What the connect probe found (all false when the folder was not reachable).
    pub fn probe(&self) -> FolderProbe {
        self.inner.probe()
    }

    async fn run<T: Send + 'static>(
        &self,
        limit: Duration,
        f: impl FnOnce(&Inner) -> Result<T, BErr> + Send + 'static,
    ) -> Result<T, BErr> {
        let inner = self.inner.clone();
        blocking(limit, move || f(&inner)).await
    }

    /// Start the watcher when it is not running and the folder is there.
    async fn start_watch(&self) {
        let r = self
            .run(OP_LIMIT, |i| {
                let on = matches!(i.root_state(), RootState::Dir(_)) && i.watch.ensure();
                i.set_watcher(on);
                Ok(on)
            })
            .await;
        if let Err(e) = r {
            tracing::warn!("folder: cannot start the watcher: {e}");
        }
    }

    fn mark(&self, c: &Cursor, events: u64) {
        let mut m = self.marks.lock();
        m.retain(|(k, _)| *k != c.0);
        m.push_back((c.0.clone(), events));
        while m.len() > 16 {
            m.pop_front();
        }
    }

    fn mark_of(&self, c: &Cursor) -> Option<u64> {
        self.marks
            .lock()
            .iter()
            .find(|(k, _)| *k == c.0)
            .map(|(_, v)| *v)
    }

    /// A fresh walk diffed against `base`: upserts in walk order (parents first),
    /// then removals children first. Also the watcher's count at the walk's start.
    async fn diff(&self, base: Arc<Generation>) -> Result<(Vec<ChangeItem>, Entries, u64), BErr> {
        self.run(WALK_LIMIT, move |i| {
            let events = i.watch.events();
            let (nodes, entries) = i.snapshot(Some(&base))?;
            let mut items: Vec<ChangeItem> = nodes
                .into_iter()
                .filter(|n| base.entries.get(&n.pid) != Some(&(n.version.clone(), n.is_dir)))
                .map(ChangeItem::Upsert)
                .collect();
            let removed: Vec<Pid> = base
                .entries
                .keys()
                .filter(|p| !entries.contains_key(*p))
                .cloned()
                .collect();
            i.confirm_removed(&entries, &removed)?;
            let mut order: Vec<&Pid> = removed.iter().collect();
            snap::children_first(&mut order);
            items.extend(
                order
                    .into_iter()
                    .map(|p| ChangeItem::Removed { pid: p.clone() }),
            );
            Ok((items, entries, events))
        })
        .await
    }

    async fn save_snap(&self, snap: SnapFile) -> Result<(), BErr> {
        let dir = self.inner.state_dir.clone();
        blocking(OP_LIMIT, move || {
            snap::save(&dir, &snap)
                .map_err(|e| BErr::Fatal(format!("cannot save the folder snapshot: {e}")))
        })
        .await
    }
}

#[cfg(feature = "testing")]
impl FolderBackend {
    /// Tests: treat the filesystem as keeping modification times to `ns` (0: what the
    /// probe found).
    pub fn force_mtime_granularity(&self, ns: i64) {
        self.inner.knobs.granularity_ns.store(ns, Ordering::Relaxed);
    }

    /// Tests: the next case-only rename stops after its first rename, as a crash
    /// would, leaving the journal behind.
    pub fn stop_next_case_rename(&self) {
        self.inner
            .knobs
            .stop_case_rename
            .store(true, Ordering::Release);
    }

    /// Tests: `f` runs inside the next replace, after the bytes are copied and before
    /// they are renamed into place (where a reader's write would land).
    pub fn before_next_replace_rename(&self, f: impl FnOnce() + Send + 'static) {
        *self.inner.knobs.before_replace.lock() = Some(Box::new(f));
    }
}

impl Backend for FolderBackend {
    fn caps(&self) -> BackendCaps {
        let p = self.inner.probe();
        BackendCaps {
            cas_create: p.no_replace_rename || p.hard_links,
            cas_overwrite: false,
            atomic_replace: true,
            server_copy: true,
            range: true,
            pinned_read: false,
            push: true,
            hash_kind: None,
            mtime_granularity_ns: 1_000_000,
            simple_upload_max: u64::MAX,
            chunk_multiple: 1,
            path_ids: true,
            duplicate_names: false,
        }
    }

    fn limits(&self) -> Limits {
        Limits::folder()
    }

    async fn account(&self) -> Result<AccountInfo, BErr> {
        let id = account_id(&self.inner.root);
        self.run(OP_LIMIT, |i| {
            let root_there = std::fs::symlink_metadata(&i.root).is_ok();
            let parent_there = i.root.parent().is_some_and(|p| p.is_dir());
            if root_there || parent_there {
                Ok(())
            } else {
                Err(unreachable_err(&i.root.display().to_string()))
            }
        })
        .await?;
        Ok(AccountInfo {
            label: id.clone(),
            id,
        })
    }

    async fn locate_roots(&self, name: &str) -> Result<Vec<RNode>, BErr> {
        if name != self.inner.root_name {
            return Ok(Vec::new());
        }
        let found = self
            .run(OP_LIMIT, |i| match i.root_state() {
                RootState::Dir(m) => Ok(vec![i.root_node(&m)]),
                RootState::Missing => Ok(Vec::new()),
                RootState::Unreachable(why) => Err(unreachable_err(&why)),
            })
            .await?;
        if !found.is_empty() && self.inner.watch_on && !self.inner.watch.running() {
            self.start_watch().await;
        }
        Ok(found)
    }

    async fn create_root(&self, name: &str) -> Result<RNode, BErr> {
        if name != self.inner.root_name {
            return Err(BErr::Fatal(format!(
                "this library folder is reported as {:?}, not {name:?}",
                self.inner.root_name
            )));
        }
        let node = self
            .run(OP_LIMIT, |i| {
                std::fs::create_dir(&i.root).map_err(|e| io_err(&e, Act::Create, ""))?;
                i.forget_dirs();
                i.run_probe(true);
                match i.root_state() {
                    RootState::Dir(m) => Ok(i.root_node(&m)),
                    _ => Err(BErr::Transient(
                        "the new library folder is not visible".into(),
                    )),
                }
            })
            .await?;
        if self.inner.watch_on {
            self.start_watch().await;
        }
        Ok(node)
    }

    async fn start_cursor(&self) -> Result<Cursor, BErr> {
        let mut slot = self.snap.lock().await;
        let (entries, events) = self
            .run(WALK_LIMIT, |i| {
                let events = i.watch.events();
                Ok((i.snapshot(None)?.1, events))
            })
            .await?;
        let snap = SnapFile {
            epoch: token(16),
            base: Generation { gen_no: 1, entries },
            next: None,
        };
        self.save_snap(snap.clone()).await?;
        let c = snap::cursor_of(&snap.epoch, snap.base.gen_no);
        *slot = Some(snap);
        self.mark(&c, events);
        Ok(c)
    }

    async fn list_all(
        &self,
        roots: &[Pid],
        sink: &mut (dyn FnMut(RNode) + Send),
    ) -> Result<(), BErr> {
        for root in roots {
            let root = root.clone();
            let nodes = self
                .run(WALK_LIMIT, move |i| {
                    let abs = if root.is_empty() {
                        match i.root_state() {
                            RootState::Dir(_) => i.root.clone(),
                            RootState::Missing => return Err(BErr::NotFound),
                            RootState::Unreachable(why) => return Err(unreachable_err(&why)),
                        }
                    } else {
                        i.exact_dir(&root)?
                    };
                    match i.walk(&root, abs, None) {
                        Err(BErr::NotFound) if root.is_empty() => match i.root_state() {
                            RootState::Unreachable(why) => Err(unreachable_err(&why)),
                            _ => Err(BErr::NotFound),
                        },
                        r => r.map(|(nodes, _)| nodes),
                    }
                })
                .await?;
            nodes.into_iter().for_each(&mut *sink);
        }
        Ok(())
    }

    async fn list_dir(&self, dir: &str, sink: &mut (dyn FnMut(RNode) + Send)) -> Result<(), BErr> {
        let dir = dir.to_string();
        let nodes = self
            .run(OP_LIMIT, move |i| {
                let abs = i.exact_dir(&dir)?;
                i.list_children(&abs, &dir).map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                        BErr::NotFound
                    }
                    _ => BErr::Transient(format!("cannot list {dir}: {e}")),
                })
            })
            .await?;
        nodes.into_iter().for_each(sink);
        Ok(())
    }

    async fn find_named(&self, parent: &str, name: &str) -> Result<(Vec<RNode>, ServerTime), BErr> {
        let (parent, name) = (parent.to_string(), name.to_string());
        let nodes = self
            .run(OP_LIMIT, move |i| {
                let abs = i.exact_dir(&parent)?;
                let want = nfc(name.as_bytes());
                let kids = i.list_children(&abs, &parent).map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                        BErr::NotFound
                    }
                    _ => BErr::Transient(format!("cannot list {parent}: {e}")),
                })?;
                Ok(kids
                    .into_iter()
                    .filter(|n| nfc(&n.name_raw) == want)
                    .map(|n| i.with_hash(n))
                    .collect::<Vec<_>>())
            })
            .await?;
        Ok((nodes, ServerTime(now_ms())))
    }

    async fn changes(&self, c: &Cursor) -> Result<ChangePage, BErr> {
        let mut slot = self.snap.lock().await;
        if slot.is_none() {
            let dir = self.inner.state_dir.clone();
            *slot = blocking(OP_LIMIT, move || Ok(snap::load(&dir))).await?;
        }
        let Some(snap) = slot.as_mut() else {
            return Ok(ChangePage::CursorLost);
        };
        let Some((epoch, gen_no)) = snap::parse_cursor(c) else {
            return Ok(ChangePage::CursorLost);
        };
        if epoch != snap.epoch {
            return Ok(ChangePage::CursorLost);
        }
        if snap.next.as_ref().is_some_and(|n| n.gen_no == gen_no) {
            snap.base = snap.next.take().expect("checked");
        } else if snap.base.gen_no != gen_no {
            return Ok(ChangePage::CursorLost);
        }
        let base = Arc::new(snap.base.clone());
        let mut attempt = 1;
        let (items, entries, events) = loop {
            match self.diff(base.clone()).await {
                Err(e) if is_cut_short(&e) && attempt < LIST_ATTEMPTS => {
                    tracing::debug!(attempt, "folder: {e}; listing again");
                    tokio::time::sleep(LIST_RETRY_PAUSE * attempt).await;
                    attempt += 1;
                }
                r => break r?,
            }
        };
        let _ = self
            .run(OP_LIMIT, |i| {
                i.purge_if_due();
                Ok(())
            })
            .await;
        if items.is_empty() {
            self.mark(c, events);
            return Ok(ChangePage::Page {
                items,
                next: c.clone(),
                more: false,
            });
        }
        let next = Generation {
            gen_no: base.gen_no + 1,
            entries,
        };
        let cursor = snap::cursor_of(&snap.epoch, next.gen_no);
        snap.next = Some(next);
        let to_save = snap.clone();
        self.save_snap(to_save).await?;
        self.mark(&cursor, events);
        Ok(ChangePage::Page {
            items,
            next: cursor,
            more: false,
        })
    }

    /// Returns once the watcher saw a change after the walk behind `c` started and the
    /// folder has been quiet a moment, when an empty file held back as a write in
    /// progress settles, or at `max`. Never fails.
    async fn wait_change(&self, c: &Cursor, max: Duration) -> Result<(), BErr> {
        // An empty file held back as a write in progress settles without an event.
        let max = self.inner.until_settled(max);
        if !self.inner.watch_on {
            tokio::time::sleep(max).await;
            return Ok(());
        }
        if !self.inner.watch.running() {
            self.start_watch().await;
        }
        if !self.inner.watch.running() {
            tokio::time::sleep(max).await;
            return Ok(());
        }
        let since = self.mark_of(c).unwrap_or(0);
        self.inner.watch.wait(since, max).await;
        Ok(())
    }

    async fn stat(&self, pid: &str) -> Result<Option<RNode>, BErr> {
        let pid = pid.to_string();
        self.run(OP_LIMIT, move |i| {
            Ok(i.exact(&pid)?.map(|m| i.with_hash(i.node_at(&pid, &m))))
        })
        .await
    }

    async fn read(
        &self,
        pid: &str,
        _pin: Option<&str>,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<RemoteBody, BErr> {
        let pid = pid.to_string();
        let (file, left) = self
            .run(OP_LIMIT, move |i| {
                match i.exact(&pid)? {
                    Some(m) if m.is_file() => {}
                    _ => return Err(BErr::NotFound),
                }
                let abs = i.abs_or_missing(&pid)?;
                let mut f = fsops::open_nofollow(&abs).map_err(|e| io_err(&e, Act::Read, &pid))?;
                let len = f.metadata().map_err(|e| io_err(&e, Act::Read, &pid))?.len();
                let (first, left) = match range {
                    None => (0, len),
                    Some((first, last)) => {
                        if first >= len {
                            return Err(BErr::Fatal(format!(
                                "range {first}- not satisfiable for {pid} (size {len})"
                            )));
                        }
                        let end = last.map_or(len, |l| l.saturating_add(1).min(len));
                        (first, end.saturating_sub(first))
                    }
                };
                if first > 0 {
                    f.seek(std::io::SeekFrom::Start(first))
                        .map_err(|e| io_err(&e, Act::Read, &pid))?;
                }
                Ok((f, left))
            })
            .await?;
        let stream = futures_util::stream::unfold((Some(file), left), |(file, left)| async move {
            let mut f = file?;
            if left == 0 {
                return None;
            }
            let want = left.min(READ_CHUNK as u64) as usize;
            let r = tokio::time::timeout(
                CHUNK_LIMIT,
                tokio::task::spawn_blocking(move || {
                    let mut buf = vec![0u8; want];
                    let mut n = 0;
                    while n < want {
                        match f.read(&mut buf[n..]) {
                            Ok(0) => break,
                            Ok(k) => n += k,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(e) => return Err(e),
                        }
                    }
                    buf.truncate(n);
                    Ok((f, buf))
                }),
            )
            .await;
            match r {
                Ok(Ok(Ok((f, buf)))) => {
                    if buf.is_empty() {
                        // Shorter than it was: the store's length check catches it.
                        return None;
                    }
                    let rest = left - buf.len() as u64;
                    Some((Ok(Bytes::from(buf)), (Some(f), rest)))
                }
                Ok(Ok(Err(e))) => Some((Err(e), (None, 0))),
                Ok(Err(e)) => Some((Err(std::io::Error::other(e)), (None, 0))),
                Err(_) => Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "the library folder did not answer",
                    )),
                    (None, 0),
                )),
            }
        });
        Ok(stream.boxed())
    }

    async fn upload(
        &self,
        to: UploadTarget<'_>,
        src: UploadSource<'_>,
        _op: &mut OpCtx,
    ) -> Result<RNode, BErr> {
        let UploadSource::File { path, size, sha256 } = src;
        let path = path.to_path_buf();
        // A transfer: a minute plus a second per MiB.
        let limit = OP_LIMIT + Duration::from_secs(size >> 20);
        match to {
            UploadTarget::New { parent, name } => {
                let (parent, name) = (parent.to_string(), name.to_string());
                self.run(limit, move |i| {
                    i.upload_new(&parent, &name, &path, size, sha256)
                })
                .await
            }
            UploadTarget::Replace { pid, if_cver } => {
                let pid = pid.to_string();
                let if_cver = if_cver.map(str::to_string);
                self.run(limit, move |i| {
                    i.upload_replace(&pid, if_cver.as_deref(), &path, size, sha256)
                })
                .await
            }
        }
    }

    async fn mkdir(&self, parent: &str, name: &str, _op: &OpCtx) -> Result<RNode, BErr> {
        let (parent, name) = (parent.to_string(), name.to_string());
        self.run(OP_LIMIT, move |i| i.mkdir(&parent, &name)).await
    }

    async fn update(
        &self,
        pid: &str,
        parent: Option<&str>,
        name: Option<&str>,
        if_cver: Option<&str>,
    ) -> Result<RNode, BErr> {
        let pid = pid.to_string();
        let parent = parent.map(str::to_string);
        let name = name.map(str::to_string);
        let if_cver = if_cver.map(str::to_string);
        self.run(OP_LIMIT, move |i| {
            i.update(&pid, parent.as_deref(), name.as_deref(), if_cver.as_deref())
        })
        .await
    }

    async fn copy(&self, pid: &str, parent: &str, name: &str) -> Result<RNode, BErr> {
        let (pid, parent, name) = (pid.to_string(), parent.to_string(), name.to_string());
        // A transfer, like an upload: the size is not known yet, so a generous bound.
        self.run(WALK_LIMIT, move |i| i.copy(&pid, &parent, &name))
            .await
    }

    async fn trash(&self, pid: &str, if_cver: Option<&str>) -> Result<(), BErr> {
        let pid = pid.to_string();
        let if_cver = if_cver.map(str::to_string);
        self.run(WALK_LIMIT, move |i| i.trash(&pid, if_cver.as_deref()))
            .await
    }

    async fn delete_permanent(&self, pid: &str) -> Result<(), BErr> {
        let pid = pid.to_string();
        self.run(OP_LIMIT, move |i| i.delete_permanent(&pid)).await
    }

    async fn quota(&self) -> Result<Option<bunko_store::Capacity>, BErr> {
        self.run(OP_LIMIT, |i| {
            Ok(match i.root_state() {
                RootState::Dir(_) => fs4::statvfs(&i.root).ok().map(|s| bunko_store::Capacity {
                    total: s.total_space(),
                    available: s.available_space(),
                }),
                _ => None,
            })
        })
        .await
    }
}

/// The store id's and the credentials' account for a folder library: the absolute
/// path after `~` expansion, lexically normalised (no `.` or `..`, no trailing
/// separator), never resolved, so it does not change while the folder is away.
pub fn account_id(path: &Path) -> String {
    lexical(path).to_string_lossy().into_owned()
}

/// For tests: the path the change-feed snapshot is kept at.
pub fn snapshot_path(state_dir: &Path) -> PathBuf {
    state_dir.join(snap::SNAPSHOT_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn account_id_is_lexical() {
        assert_eq!(account_id(Path::new("/srv/manga")), "/srv/manga");
        assert_eq!(account_id(Path::new("/srv/manga/")), "/srv/manga");
        assert_eq!(account_id(Path::new("/srv/./x/../manga//")), "/srv/manga");
        assert_eq!(account_id(Path::new("/../srv")), "/srv");
        assert_eq!(account_id(Path::new("/")), "/");
        // Never resolved: a folder that does not exist keeps its id.
        assert_eq!(
            account_id(Path::new("/nonexistent.test/Manga")),
            "/nonexistent.test/Manga"
        );
    }

    #[cfg(windows)]
    #[test]
    fn account_id_is_lexical() {
        assert_eq!(account_id(Path::new(r"D:\Manga\")), r"D:\Manga");
        assert_eq!(account_id(Path::new(r"D:\x\..\Manga")), r"D:\Manga");
        assert_eq!(account_id(Path::new(r"D:\")), r"D:\");
        assert_eq!(
            account_id(Path::new(r"\\files.example.com\manga\")),
            r"\\files.example.com\manga\"
        );
    }

    #[test]
    fn snapshot_lives_in_the_state_dir() {
        assert_eq!(
            snapshot_path(Path::new("state")),
            Path::new("state").join("folder-snapshot.json")
        );
    }

    #[tokio::test]
    async fn connect_wants_a_full_path() {
        let cfg = FolderConfig {
            path: PathBuf::from("relative/manga"),
            root_name: "mokuro-reader".into(),
            state_dir: PathBuf::from("state"),
            watch: false,
        };
        assert!(matches!(
            FolderBackend::connect(cfg).await,
            Err(BErr::Fatal(_))
        ));
    }

    #[test]
    fn names_the_driver_hides_and_refuses() {
        for n in [
            ".bunko-0123.tmp",
            ".bunko-case-1.tmp",
            ".mokuro-bunko-trash",
            "a.cbz.bunko-trashed",
            "Vol 1.cbz.crswap",
            "x.CRSWAP",
        ] {
            assert!(hidden_name(n, false), "{n}");
            assert_eq!(check_name(n, false), Err(BErr::InvalidName), "{n}");
        }
        for n in ["$RECYCLE.BIN", "System Volume Information", "lost+found"] {
            assert!(hidden_name(n, true), "{n}");
            assert!(!hidden_name(n, false), "{n}");
        }
        for n in [".mokuro-bunko.json", ".mokuro-queue.json", "Vol 1.cbz"] {
            assert!(!hidden_name(n, true), "{n}");
        }
        for n in ["", ".", "..", "a/b", "x\0y"] {
            assert_eq!(check_name(n, false), Err(BErr::InvalidName), "{n:?}");
        }
        for n in [
            "a<b", "a:b", "q?", "dot.", "space ", "CON", "nul.txt", "tab\t",
        ] {
            assert!(!windows_name_ok(n), "{n:?}");
        }
        for n in ["Vol 1.cbz", "ガ", "...a", "Why!"] {
            assert!(windows_name_ok(n), "{n:?}");
        }
    }

    #[test]
    fn swap_files_of_a_name() {
        for swap in ["a.cbz.crswap", "a.cbz.1.crswap", "a.cbz.12.CRSWAP"] {
            assert!(is_swap_of(swap, "a.cbz"), "{swap}");
        }
        for other in [
            "a.cbz",
            "a.cbz.x.crswap",
            "a.cbz..crswap",
            "a.cbzz.crswap",
            "b.cbz.crswap",
            "a.cbz.crswap.tmp",
        ] {
            assert!(!is_swap_of(other, "a.cbz"), "{other}");
        }
    }

    /// The paths a filesystem without `RENAME_NOREPLACE` takes (NFS, CIFS, FUSE,
    /// vfat), which the hosts tests run on never take by themselves.
    #[test]
    fn the_fallback_renames_never_replace() {
        let d = tempfile::tempdir().unwrap();
        let p = |n: &str| d.path().join(n);
        for links in [true, false] {
            std::fs::write(p("a"), b"a").unwrap();
            std::fs::write(p("b"), b"b").unwrap();
            let e = rename_new_fallback(&p("a"), &p("b"), false, links).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists, "links {links}");
            assert_eq!(std::fs::read(p("b")).unwrap(), b"b");
            rename_new_fallback(&p("a"), &p("c"), false, links).unwrap();
            assert_eq!(std::fs::read(p("c")).unwrap(), b"a");
            assert!(!p("a").exists());
            for f in ["b", "c"] {
                std::fs::remove_file(p(f)).unwrap();
            }
        }
        std::fs::create_dir(p("D")).unwrap();
        std::fs::write(p("D/x"), b"x").unwrap();
        std::fs::create_dir(p("E")).unwrap();
        let e = rename_new_fallback(&p("D"), &p("E"), true, true).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        rename_new_fallback(&p("D"), &p("F"), true, true).unwrap();
        assert_eq!(std::fs::read(p("F/x")).unwrap(), b"x");
    }

    #[test]
    fn an_empty_file_settles_after_its_time_and_a_wrong_clock_holds_nothing() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let s = Duration::from_secs;
        assert_eq!(unsettled_for(now, now), Some(EMPTY_SETTLE));
        assert_eq!(unsettled_for(now - s(20), now), Some(EMPTY_SETTLE - s(20)));
        // A FAT stamp, rounded down to an even second, is a little older.
        assert_eq!(unsettled_for(now - s(1), now), Some(EMPTY_SETTLE - s(1)));
        assert_eq!(unsettled_for(now - EMPTY_SETTLE, now), None);
        assert_eq!(unsettled_for(now - EMPTY_SETTLE - s(1), now), None);
        assert_eq!(unsettled_for(UNIX_EPOCH, now), None);
        // A share's clock ahead of ours: the stamp counts from when it is reached.
        assert_eq!(unsettled_for(now + s(10), now), Some(EMPTY_SETTLE + s(10)));
        assert_eq!(
            unsettled_for(now + CLOCK_AHEAD, now),
            Some(EMPTY_SETTLE + CLOCK_AHEAD)
        );
        // Further ahead is a wrong clock: nothing is held back for it.
        assert_eq!(unsettled_for(now + CLOCK_AHEAD + s(1), now), None);
        assert_eq!(unsettled_for(now + s(365 * 86_400), now), None);
    }

    #[test]
    fn times_round_up_to_the_granularity() {
        let t = UNIX_EPOCH + Duration::from_nanos(1_500_000_001);
        assert_eq!(ceil_to(t, 1), t);
        assert_eq!(
            ceil_to(t, 1_000_000_000),
            UNIX_EPOCH + Duration::from_secs(2)
        );
        assert_eq!(
            ceil_to(UNIX_EPOCH + Duration::from_secs(4), 2_000_000_000),
            UNIX_EPOCH + Duration::from_secs(4)
        );
    }

    #[tokio::test]
    async fn a_state_dir_inside_the_library_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let lib = d.path().join("Manga");
        std::fs::create_dir(&lib).unwrap();
        for (path, state) in [
            (lib.clone(), lib.join("state")),
            (lib.join("inner"), lib.clone()),
            (lib.clone(), lib.clone()),
        ] {
            let cfg = FolderConfig {
                path,
                root_name: "mokuro-reader".into(),
                state_dir: state,
                watch: false,
            };
            assert!(
                matches!(FolderBackend::connect(cfg).await, Err(BErr::Fatal(_))),
                "nested"
            );
        }
        let ok = FolderConfig {
            path: lib.clone(),
            root_name: "mokuro-reader".into(),
            state_dir: d.path().join("state"),
            watch: false,
        };
        let b = FolderBackend::connect(ok).await.unwrap();
        assert!(b.probe().reachable);
        assert!(b.probe().writable);
    }
}
