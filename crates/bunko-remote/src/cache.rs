//! The read-side caches of a RemoteStore (spec §5.5, §5.6, D24).
//!
//! - [`BlobCache`]: files of at most [`SMALL_MAX`] bytes as content-addressed blobs
//!   under `<dir>/blobs`, indexed by the `blob` and `cached` tables of
//!   `index.sqlite`. A blob is fresh for a node iff a `cached(pid, cver)` row names it,
//!   so the feed (which changes `cver`) is the invalidator. LRU by `last_access`
//!   within the byte budget.
//! - [`Spool`]: whole archives in `<dir>/spool/`, filled progressively by one shared
//!   download per `(pid, cver)`; readers wait for their bytes; size and provider hash
//!   are verified on completion; pins keep a file alive; admission against a byte
//!   budget (§5.6 "Budget").
//! - [`ZipDirCache`]: the tail of an archive (central directory, zip64 records, end
//!   record, comment) per `(pid, cver)` in the `zipdir` table, so listing an archive
//!   again costs no network.
//! - [`BlockCache`]: 256 KiB blocks of large files keyed `(pid, cver, block)`, in
//!   memory, LRU within a byte budget.
//!
//! Every cache is filled only from version-pinned reads (D24); the callers in
//! `store.rs` and `file.rs` check the pin before anything is inserted here.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, watch};

use crate::backend::{BErr, HashKind};

/// Files up to this size are served from the blob cache (§5.5).
pub const SMALL_MAX: u64 = 4 << 20;
/// The block-cache granularity (§3.7).
pub const BLOCK: u64 = 256 << 10;
/// How much of an archive's end the first tail read fetches (§5.6 step 1).
pub const TAIL: u64 = 256 << 10;
/// A reader this far ahead of a progressive spool's fill uses ranged reads instead
/// of waiting (§5.6).
pub const SPOOL_AHEAD: u64 = 4 << 20;
/// A central directory larger than this is not cached beyond the tail read.
pub const MAX_CD: u64 = 16 << 20;

/// `(pid, cver)`.
pub(crate) type Key = (String, String);

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn db_io(e: rusqlite::Error) -> io::Error {
    io::Error::other(e)
}

/// Write `data` to `path` through a temporary sibling and a rename.
fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// A second connection to `index.sqlite` for the cache tables, so cache traffic does
/// not wait on the feed's lock of the [`crate::index::Index`].
pub(crate) struct CacheDb(Mutex<Connection>);

impl CacheDb {
    /// Open `path` (already created by `Index::open`, which owns the schema).
    pub(crate) fn open(path: &Path) -> io::Result<CacheDb> {
        let conn = Connection::open(path).map_err(db_io)?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(db_io)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(db_io)?;
        Ok(CacheDb(Mutex::new(conn)))
    }

    fn with<T>(&self, what: &str, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Option<T> {
        f(&self.0.lock())
            .map_err(|e| tracing::warn!("cache db {what}: {e}"))
            .ok()
    }
}

// ---- hashing ----------------------------------------------------------------------

/// sha256 of the bytes, plus the provider's own hash where Bunko can compute it.
pub(crate) struct Hasher {
    sha: Sha256,
    md5: Option<md5::Md5>,
}

impl Hasher {
    pub(crate) fn new(provider: Option<HashKind>) -> Hasher {
        Hasher {
            sha: Sha256::new(),
            md5: (provider == Some(HashKind::Md5)).then(md5::Md5::new),
        }
    }

    pub(crate) fn update(&mut self, b: &[u8]) {
        self.sha.update(b);
        if let Some(m) = &mut self.md5 {
            md5::Digest::update(m, b);
        }
    }

    /// The sha256, after checking the provider hash `expect` (when Bunko can compute
    /// that kind: sha256 and md5; the others are not verified locally yet).
    pub(crate) fn finish(self, expect: Option<&(HashKind, String)>) -> Result<[u8; 32], BErr> {
        let sha: [u8; 32] = self.sha.finalize().into();
        let ok = match expect {
            None => true,
            Some((HashKind::Sha256, want)) => hex::encode(sha).eq_ignore_ascii_case(want),
            Some((HashKind::Md5, want)) => self
                .md5
                .map(|m| hex::encode(md5::Digest::finalize(m)).eq_ignore_ascii_case(want))
                .unwrap_or(true),
            Some((kind, _)) => {
                tracing::debug!("provider hash {} not verified locally", kind.as_str());
                true
            }
        };
        if ok {
            Ok(sha)
        } else {
            Err(BErr::Transient(
                "content does not match the provider's hash".into(),
            ))
        }
    }
}

// ---- small files ------------------------------------------------------------------

/// Content-addressed small-file blobs (§5.5).
pub struct BlobCache {
    dir: PathBuf,
    db: Arc<CacheDb>,
    budget: u64,
}

impl BlobCache {
    pub(crate) fn open(dir: PathBuf, db: Arc<CacheDb>, budget: u64) -> io::Result<BlobCache> {
        std::fs::create_dir_all(&dir)?;
        Ok(BlobCache { dir, db, budget })
    }

    fn path(&self, sha: &[u8; 32]) -> PathBuf {
        let h = hex::encode(sha);
        self.dir.join(&h[..2]).join(h)
    }

    /// The blob that holds `(pid, cver)`, if cached; refreshes its LRU stamp.
    pub(crate) fn lookup(&self, pid: &str, cver: &str) -> Option<[u8; 32]> {
        let sha: String = self
            .db
            .with("lookup", |c| {
                c.query_row(
                    "SELECT sha256 FROM cached WHERE pid = ?1 AND cver = ?2",
                    params![pid, cver],
                    |r| r.get(0),
                )
                .optional()
            })
            .flatten()?;
        let sha: [u8; 32] = hex::decode(&sha).ok()?.try_into().ok()?;
        self.db.with("touch", |c| {
            c.execute(
                "UPDATE blob SET last_access = ?1 WHERE sha256 = ?2",
                params![now_ms(), hex::encode(sha)],
            )
        });
        Some(sha)
    }

    /// The bytes of a blob, re-hashed (a damaged blob is dropped: `None`).
    pub(crate) fn read(&self, sha: &[u8; 32]) -> Option<Vec<u8>> {
        let data = std::fs::read(self.path(sha)).ok();
        match data {
            Some(d) if <[u8; 32]>::from(Sha256::digest(&d)) == *sha => Some(d),
            _ => {
                self.forget(sha);
                None
            }
        }
    }

    fn forget(&self, sha: &[u8; 32]) {
        let h = hex::encode(sha);
        let _ = std::fs::remove_file(self.path(sha));
        self.db.with("forget", |c| {
            c.execute("DELETE FROM cached WHERE sha256 = ?1", [&h])?;
            c.execute("DELETE FROM blob WHERE sha256 = ?1", [&h])
        });
    }

    /// Store `data` (whose sha256 is `sha`) as the bytes of `(pid, cver)`; older
    /// versions of `pid` lose their `cached` row. Evicts down to the budget.
    pub(crate) fn insert(
        &self,
        pid: &str,
        cver: &str,
        data: &[u8],
        sha: [u8; 32],
    ) -> io::Result<()> {
        let path = self.path(&sha);
        if std::fs::metadata(&path).ok().map(|m| m.len()) != Some(data.len() as u64) {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            write_atomic(&path, data)?;
        }
        let h = hex::encode(sha);
        self.db
            .with("insert", |c| {
                c.execute(
                    "INSERT INTO blob(sha256, size, last_access) VALUES(?1, ?2, ?3) \
                     ON CONFLICT(sha256) DO UPDATE SET last_access = excluded.last_access",
                    params![h, data.len() as i64, now_ms()],
                )?;
                c.execute(
                    "DELETE FROM cached WHERE pid = ?1 AND cver != ?2",
                    params![pid, cver],
                )?;
                c.execute(
                    "INSERT OR REPLACE INTO cached(pid, cver, sha256) VALUES(?1, ?2, ?3)",
                    params![pid, cver, h],
                )
            })
            .ok_or_else(|| io::Error::other("blob cache: database write failed"))?;
        self.evict(&sha);
        Ok(())
    }

    /// Drop least-recently used blobs (never `keep`) until the total fits.
    fn evict(&self, keep: &[u8; 32]) {
        let keep = hex::encode(keep);
        let victims: Vec<String> = self
            .db
            .with("evict", |c| {
                let total: i64 =
                    c.query_row("SELECT COALESCE(SUM(size), 0) FROM blob", [], |r| r.get(0))?;
                let mut over = total.saturating_sub(i64::try_from(self.budget).unwrap_or(i64::MAX));
                let mut out = Vec::new();
                if over <= 0 {
                    return Ok(out);
                }
                let mut s = c.prepare(
                    "SELECT sha256, size FROM blob WHERE sha256 != ?1 ORDER BY last_access, sha256",
                )?;
                let rows = s.query_map([&keep], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?;
                for row in rows {
                    let (sha, size) = row?;
                    out.push(sha);
                    over -= size;
                    if over <= 0 {
                        break;
                    }
                }
                Ok(out)
            })
            .unwrap_or_default();
        for v in victims {
            if let Some(sha) = hex::decode(&v)
                .ok()
                .and_then(|b| <[u8; 32]>::try_from(b).ok())
            {
                self.forget(&sha);
            }
        }
    }

    /// `(blobs, bytes)`.
    pub(crate) fn stats(&self) -> (u64, u64) {
        self.db
            .with("stats", |c| {
                c.query_row(
                    "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM blob",
                    [],
                    |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
                )
            })
            .unwrap_or((0, 0))
    }
}

// ---- archive tails ----------------------------------------------------------------

/// The cached end of one archive version: `bytes` are the file's bytes from `start`
/// to its end.
#[derive(Debug)]
pub struct ZipTail {
    pub start: u64,
    /// Where the end-of-central-directory record starts; `None`: not a zip.
    pub eocd_off: Option<u64>,
    pub bytes: Vec<u8>,
}

fn u16_at(b: &[u8], i: usize) -> u64 {
    u64::from(u16::from_le_bytes([b[i], b[i + 1]]))
}

fn u32_at(b: &[u8], i: usize) -> u64 {
    u64::from(u32::from_le_bytes(b[i..i + 4].try_into().expect("4 bytes")))
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().expect("8 bytes"))
}

/// Find the end record in `tail` (the file's bytes from `tail_start`): its offset
/// and where the central directory starts (zip64 aware, located from the end record
/// so prepended data does not matter). `(None, None)` when `tail` holds no end record.
pub fn locate_central_directory(tail: &[u8], tail_start: u64) -> (Option<u64>, Option<u64>) {
    const EOCD: &[u8; 4] = b"PK\x05\x06";
    const LOC64: &[u8; 4] = b"PK\x06\x07";
    const EOCD64: &[u8; 4] = b"PK\x06\x06";
    let Some(at) = tail
        .windows(4)
        .enumerate()
        .rev()
        .find(|(i, w)| *w == EOCD && tail.len() - i >= 22)
        .map(|(i, _)| i)
    else {
        return (None, None);
    };
    let eocd_off = tail_start + at as u64;
    let rec = &tail[at..];
    let mut size_cd = u32_at(rec, 12);
    let mut cd_start = eocd_off.checked_sub(size_cd);
    let zip64 =
        size_cd == 0xFFFF_FFFF || u32_at(rec, 16) == 0xFFFF_FFFF || u16_at(rec, 10) == 0xFFFF;
    if at >= 20 && &tail[at - 20..at - 16] == LOC64 {
        // The zip64 end record sits right before its locator.
        let rec64_at = (at - 20).checked_sub(56);
        match rec64_at {
            Some(r) if &tail[r..r + 4] == EOCD64 => {
                size_cd = u64_at(tail, r + 40);
                cd_start = (tail_start + r as u64).checked_sub(size_cd);
            }
            _ if zip64 => cd_start = None,
            _ => {}
        }
    } else if zip64 {
        cd_start = None;
    }
    (Some(eocd_off), cd_start)
}

/// Archive tails per `(pid, cver)` in the `zipdir` table, with a small in-memory
/// front.
pub struct ZipDirCache {
    db: Arc<CacheDb>,
    mem: Mutex<HashMap<Key, Arc<ZipTail>>>,
}

const ZIPDIR_MEM: usize = 256;

impl ZipDirCache {
    pub(crate) fn new(db: Arc<CacheDb>) -> ZipDirCache {
        ZipDirCache {
            db,
            mem: Mutex::new(HashMap::new()),
        }
    }

    /// The tail of `(pid, cver)`, whose file is `size` bytes long.
    pub(crate) fn get(&self, pid: &str, cver: &str, size: u64) -> Option<Arc<ZipTail>> {
        let key = (pid.to_string(), cver.to_string());
        if let Some(t) = self.mem.lock().get(&key) {
            return Some(t.clone());
        }
        let (eocd, cd): (i64, Vec<u8>) = self
            .db
            .with("zipdir get", |c| {
                c.query_row(
                    "SELECT eocd_off, cd FROM zipdir WHERE pid = ?1 AND cver = ?2",
                    params![pid, cver],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
            })
            .flatten()?;
        let start = size.checked_sub(cd.len() as u64)?;
        let t = Arc::new(ZipTail {
            start,
            eocd_off: u64::try_from(eocd).ok(),
            bytes: cd,
        });
        self.remember(key, t.clone());
        Some(t)
    }

    fn remember(&self, key: Key, t: Arc<ZipTail>) {
        let mut m = self.mem.lock();
        if m.len() >= ZIPDIR_MEM {
            m.clear();
        }
        m.insert(key, t);
    }

    /// Store a verified tail; older versions of `pid` are dropped.
    pub(crate) fn put(&self, pid: &str, cver: &str, t: Arc<ZipTail>) {
        let eocd = t.eocd_off.map_or(-1, |o| o as i64);
        self.db.with("zipdir put", |c| {
            c.execute(
                "DELETE FROM zipdir WHERE pid = ?1 AND cver != ?2",
                params![pid, cver],
            )?;
            c.execute(
                "INSERT OR REPLACE INTO zipdir(pid, cver, eocd_off, cd) VALUES(?1, ?2, ?3, ?4)",
                params![pid, cver, eocd, t.bytes],
            )
        });
        self.mem.lock().retain(|k, _| k.0 != pid);
        self.remember((pid.to_string(), cver.to_string()), t);
    }

    pub(crate) fn count(&self) -> u64 {
        self.db
            .with("zipdir count", |c| {
                c.query_row("SELECT COUNT(*) FROM zipdir", [], |r| r.get::<_, i64>(0))
            })
            .unwrap_or(0) as u64
    }
}

// ---- blocks -----------------------------------------------------------------------

type BlockKey = (String, String, u64);

#[derive(Default)]
struct BlockLru {
    map: HashMap<BlockKey, (Bytes, u64)>,
    order: BTreeMap<u64, BlockKey>,
    tick: u64,
    bytes: u64,
}

/// 256 KiB blocks of large files, in memory (§5.6).
pub struct BlockCache {
    cap: u64,
    lru: Mutex<BlockLru>,
}

impl BlockCache {
    pub(crate) fn new(cap: u64) -> BlockCache {
        BlockCache {
            cap,
            lru: Mutex::new(BlockLru::default()),
        }
    }

    pub(crate) fn get(&self, pid: &str, cver: &str, block: u64) -> Option<Bytes> {
        let mut l = self.lru.lock();
        let key = (pid.to_string(), cver.to_string(), block);
        l.tick += 1;
        let tick = l.tick;
        let (data, old) = {
            let e = l.map.get_mut(&key)?;
            let old = std::mem::replace(&mut e.1, tick);
            (e.0.clone(), old)
        };
        l.order.remove(&old);
        l.order.insert(tick, key);
        Some(data)
    }

    pub(crate) fn contains(&self, pid: &str, cver: &str, block: u64) -> bool {
        self.lru
            .lock()
            .map
            .contains_key(&(pid.to_string(), cver.to_string(), block))
    }

    /// Insert a block read under a verified pin.
    pub(crate) fn put(&self, pid: &str, cver: &str, block: u64, data: Bytes) {
        if data.len() as u64 > self.cap {
            return;
        }
        let mut l = self.lru.lock();
        let key = (pid.to_string(), cver.to_string(), block);
        l.tick += 1;
        let tick = l.tick;
        l.bytes += data.len() as u64;
        if let Some((old, t)) = l.map.insert(key.clone(), (data, tick)) {
            l.bytes -= old.len() as u64;
            l.order.remove(&t);
        }
        l.order.insert(tick, key);
        while l.bytes > self.cap {
            let Some((_, k)) = l.order.pop_first() else {
                break;
            };
            if let Some((d, _)) = l.map.remove(&k) {
                l.bytes -= d.len() as u64;
            }
        }
    }

    /// `(blocks, bytes)`.
    pub(crate) fn stats(&self) -> (u64, u64) {
        let l = self.lru.lock();
        (l.map.len() as u64, l.bytes)
    }
}

// ---- the spool --------------------------------------------------------------------

/// A spool file's fill state.
#[derive(Debug, Clone, Default)]
pub(crate) struct Fill {
    /// Bytes readers may use (published only under a pinned read; see
    /// `RemoteStore`'s download task).
    pub filled: u64,
    /// `Some(Ok)`: complete and verified.
    pub done: Option<Result<(), BErr>>,
}

/// One spooled archive version.
pub(crate) struct SpoolEntry {
    pub key: Key,
    pub path: PathBuf,
    pub size: u64,
    fill: watch::Sender<Fill>,
    pins: AtomicUsize,
    last_access: AtomicI64,
    pub sha256: OnceLock<[u8; 32]>,
}

impl SpoolEntry {
    pub(crate) fn fill(&self) -> Fill {
        self.fill.borrow().clone()
    }

    pub(crate) fn set_filled(&self, n: u64) {
        self.fill.send_modify(|f| f.filled = n);
    }

    /// Wait until `upto` bytes are filled or the download ended.
    pub(crate) async fn wait(&self, upto: u64) -> Fill {
        let mut rx = self.fill.subscribe();
        let r = rx
            .wait_for(|f| f.filled >= upto || f.done.is_some())
            .await
            .map(|f| f.clone());
        r.unwrap_or_else(|_| self.fill())
    }

    fn done_ok(&self) -> bool {
        matches!(self.fill.borrow().done, Some(Ok(())))
    }
}

impl std::fmt::Debug for SpoolEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpoolEntry")
            .field("key", &self.key)
            .field("path", &self.path)
            .field("size", &self.size)
            .field("fill", &self.fill())
            .finish()
    }
}

/// Keeps a spool file from eviction while held (`LocalPath::Spooled`, an open
/// archive reader).
pub struct SpoolPinGuard {
    pub(crate) entry: Arc<SpoolEntry>,
    spool: Arc<Spool>,
}

impl SpoolPinGuard {
    fn new(entry: Arc<SpoolEntry>, spool: Arc<Spool>) -> SpoolPinGuard {
        entry.pins.fetch_add(1, Ordering::AcqRel);
        SpoolPinGuard { entry, spool }
    }

    /// Another pin on the same file.
    pub(crate) fn clone_pin(&self) -> SpoolPinGuard {
        SpoolPinGuard::new(self.entry.clone(), self.spool.clone())
    }
}

impl std::fmt::Debug for SpoolPinGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SpoolPinGuard")
            .field(&self.entry.path)
            .finish()
    }
}

impl bunko_store::SpoolPin for SpoolPinGuard {
    fn path(&self) -> &Path {
        &self.entry.path
    }
}

impl Drop for SpoolPinGuard {
    fn drop(&mut self) {
        self.entry.last_access.store(now_ms(), Ordering::Relaxed);
        self.entry.pins.fetch_sub(1, Ordering::AcqRel);
        self.spool.freed.notify_waiters();
    }
}

/// Archive spool files under a byte budget (§5.6).
pub struct Spool {
    dir: PathBuf,
    db: Arc<CacheDb>,
    budget: u64,
    wait: Duration,
    entries: Mutex<HashMap<Key, Arc<SpoolEntry>>>,
    freed: Notify,
}

fn spool_name(key: &Key) -> String {
    let mut h = Sha256::new();
    h.update(key.0.as_bytes());
    h.update([0u8]);
    h.update(key.1.as_bytes());
    format!("{}.part", hex::encode(&h.finalize()[..16]))
}

impl Spool {
    /// Open the spool at `dir`: verified files from an earlier run are kept, anything
    /// else there is deleted.
    pub(crate) fn open(
        dir: PathBuf,
        db: Arc<CacheDb>,
        budget: u64,
        wait: Duration,
    ) -> io::Result<Spool> {
        std::fs::create_dir_all(&dir)?;
        let rows: Vec<(String, String, String, i64)> = db
            .with("spool load", |c| {
                c.prepare("SELECT pid, cver, file, size FROM spool WHERE verified = 1")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                    .collect()
            })
            .unwrap_or_default();
        let mut entries = HashMap::new();
        for (pid, cver, file, size) in rows {
            let key = (pid, cver);
            let path = dir.join(&file);
            if file != spool_name(&key)
                || std::fs::metadata(&path).ok().map(|m| m.len()) != Some(size as u64)
            {
                continue;
            }
            let (fill, _) = watch::channel(Fill {
                filled: size as u64,
                done: Some(Ok(())),
            });
            entries.insert(
                key.clone(),
                Arc::new(SpoolEntry {
                    key,
                    path,
                    size: size as u64,
                    fill,
                    pins: AtomicUsize::new(0),
                    last_access: AtomicI64::new(now_ms()),
                    sha256: OnceLock::new(),
                }),
            );
        }
        db.with("spool prune", |c| {
            c.execute("DELETE FROM spool WHERE verified != 1", [])
        });
        for f in std::fs::read_dir(&dir)?.flatten() {
            let keep = entries.values().any(|e| e.path == f.path());
            if !keep {
                let _ = std::fs::remove_file(f.path());
            }
        }
        let spool = Spool {
            dir,
            db,
            budget,
            wait,
            entries: Mutex::new(entries),
            freed: Notify::new(),
        };
        // Rows for files that were not kept.
        let kept: Vec<Key> = spool.entries.lock().keys().cloned().collect();
        spool.db.with("spool prune rows", |c| {
            let all: Vec<Key> = c
                .prepare("SELECT pid, cver FROM spool")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for k in all.iter().filter(|k| !kept.contains(k)) {
                c.execute(
                    "DELETE FROM spool WHERE pid = ?1 AND cver = ?2",
                    params![k.0, k.1],
                )?;
            }
            Ok(())
        });
        Ok(spool)
    }

    /// The entry for `key` (filling or complete), pinned.
    pub(crate) fn attach(self: &Arc<Self>, key: &Key) -> Option<SpoolPinGuard> {
        let m = self.entries.lock();
        m.get(key)
            .map(|e| SpoolPinGuard::new(e.clone(), self.clone()))
    }

    /// The entry for `key`, pinned, creating it (and reserving `size` bytes of the
    /// budget) when there is none; the flag says it is new and its download must be
    /// started. Without room: waits up to the configured admission wait when `wait`,
    /// else `None`. A size above the whole budget never fits.
    pub(crate) async fn admit(
        self: &Arc<Self>,
        key: &Key,
        size: u64,
        wait: bool,
    ) -> io::Result<Option<(SpoolPinGuard, bool)>> {
        let deadline = tokio::time::Instant::now() + self.wait;
        loop {
            let notified = self.freed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut m = self.entries.lock();
                if let Some(e) = m.get(key) {
                    return Ok(Some((SpoolPinGuard::new(e.clone(), self.clone()), false)));
                }
                if size > self.budget {
                    return if wait {
                        Err(io::Error::new(
                            io::ErrorKind::StorageFull,
                            format!(
                                "the archive ({size} bytes) is larger than the whole spool budget ({} bytes)",
                                self.budget
                            ),
                        ))
                    } else {
                        Ok(None)
                    };
                }
                if self.make_room(&mut m, size) {
                    let name = spool_name(key);
                    let (fill, _) = watch::channel(Fill::default());
                    let e = Arc::new(SpoolEntry {
                        key: key.clone(),
                        path: self.dir.join(&name),
                        size,
                        fill,
                        pins: AtomicUsize::new(0),
                        last_access: AtomicI64::new(now_ms()),
                        sha256: OnceLock::new(),
                    });
                    m.insert(key.clone(), e.clone());
                    self.db.with("spool insert", |c| {
                        c.execute(
                            "INSERT OR REPLACE INTO spool(pid, cver, file, size, filled, verified, pins, last_access) \
                             VALUES(?1, ?2, ?3, ?4, 0, 0, 0, ?5)",
                            params![key.0, key.1, name, size as i64, now_ms()],
                        )
                    });
                    return Ok(Some((SpoolPinGuard::new(e, self.clone()), true)));
                }
            }
            if !wait {
                return Ok(None);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    format!(
                        "the spool budget ({} bytes) is full of pinned or filling archives",
                        self.budget
                    ),
                ));
            }
        }
    }

    /// Evict unpinned complete entries, least recently used first, until `size` more
    /// bytes fit. `false` when they cannot.
    fn make_room(&self, m: &mut HashMap<Key, Arc<SpoolEntry>>, size: u64) -> bool {
        loop {
            let reserved = m.values().fold(0u64, |a, e| a.saturating_add(e.size));
            if reserved.saturating_add(size) <= self.budget {
                return true;
            }
            let victim = m
                .values()
                .filter(|e| e.pins.load(Ordering::Acquire) == 0 && e.done_ok())
                .min_by_key(|e| e.last_access.load(Ordering::Relaxed))
                .map(|e| e.key.clone());
            let Some(k) = victim else {
                return false;
            };
            if let Some(e) = m.remove(&k) {
                let _ = std::fs::remove_file(&e.path);
                self.db.with("spool evict", |c| {
                    c.execute(
                        "DELETE FROM spool WHERE pid = ?1 AND cver = ?2",
                        params![k.0, k.1],
                    )
                });
            }
        }
    }

    /// The download of `e` finished and verified.
    pub(crate) fn complete(&self, e: &SpoolEntry) {
        self.db.with("spool complete", |c| {
            c.execute(
                "UPDATE spool SET filled = ?3, verified = 1, last_access = ?4 WHERE pid = ?1 AND cver = ?2",
                params![e.key.0, e.key.1, e.size as i64, now_ms()],
            )
        });
        e.fill.send_modify(|f| {
            f.filled = e.size;
            f.done = Some(Ok(()));
        });
    }

    /// The download of `e` failed: forget it (a later reader starts over).
    pub(crate) fn fail(&self, e: &SpoolEntry, err: BErr) {
        {
            let mut m = self.entries.lock();
            if m.get(&e.key).is_some_and(|x| std::ptr::eq(x.as_ref(), e)) {
                m.remove(&e.key);
            }
        }
        let _ = std::fs::remove_file(&e.path);
        self.db.with("spool fail", |c| {
            c.execute(
                "DELETE FROM spool WHERE pid = ?1 AND cver = ?2",
                params![e.key.0, e.key.1],
            )
        });
        e.fill.send_modify(|f| f.done = Some(Err(err)));
        self.freed.notify_waiters();
    }

    /// `(entries, reserved bytes)`.
    pub(crate) fn stats(&self) -> (u64, u64) {
        let m = self.entries.lock();
        (
            m.len() as u64,
            m.values().fold(0u64, |a, e| a.saturating_add(e.size)),
        )
    }
}

/// What the caches hold (doctor, admin, tests).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub blobs: u64,
    pub blob_bytes: u64,
    pub zipdirs: u64,
    pub blocks: u64,
    pub block_bytes: u64,
    pub spools: u64,
    pub spool_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zip_of(members: &[(&str, &[u8])]) -> Vec<u8> {
        // A minimal stored zip: local headers, central directory, end record.
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for (name, data) in members {
            let off = out.len() as u32;
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
            cd.extend_from_slice(b"PK\x01\x02");
            cd.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            cd.extend_from_slice(&[0; 4]);
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
            cd.extend_from_slice(&[0; 12]);
            cd.extend_from_slice(&off.to_le_bytes());
            cd.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        out.extend_from_slice(&cd);
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    #[test]
    fn central_directory_is_located_from_the_end_record() {
        let z = zip_of(&[("a.jpg", b"aaaa"), ("b.jpg", b"bbbbbb")]);
        let cd_off = u32_at(&z, z.len() - 6);
        // the whole file as the tail
        assert_eq!(
            locate_central_directory(&z, 0),
            (Some(z.len() as u64 - 22), Some(cd_off))
        );
        // a tail that starts inside the central directory still finds its start
        let cut = (cd_off + 10) as usize;
        assert_eq!(
            locate_central_directory(&z[cut..], cut as u64),
            (Some(z.len() as u64 - 22), Some(cd_off))
        );
        assert_eq!(
            locate_central_directory(b"not a zip at all", 0),
            (None, None)
        );
    }

    #[test]
    fn block_cache_evicts_least_recently_used() {
        let c = BlockCache::new(3 * 4);
        for b in 0..3 {
            c.put("p", "c", b, Bytes::from_static(b"abcd"));
        }
        assert!(c.get("p", "c", 0).is_some()); // 0 is now the newest
        c.put("p", "c", 3, Bytes::from_static(b"abcd"));
        assert!(c.contains("p", "c", 0));
        assert!(!c.contains("p", "c", 1));
        assert_eq!(c.stats(), (3, 12));
    }

    #[test]
    fn hasher_checks_sha256_and_md5() {
        let mut h = Hasher::new(Some(HashKind::Sha256));
        h.update(b"abc");
        let want = hex::encode(Sha256::digest(b"abc"));
        assert!(h.finish(Some(&(HashKind::Sha256, want))).is_ok());
        let mut h = Hasher::new(Some(HashKind::Md5));
        h.update(b"abc");
        assert!(
            h.finish(Some(&(
                HashKind::Md5,
                "900150983cd24fb0d6963f7d28e17f72".into()
            )))
            .is_ok()
        );
        let mut h = Hasher::new(Some(HashKind::Md5));
        h.update(b"abd");
        assert!(
            h.finish(Some(&(
                HashKind::Md5,
                "900150983cd24fb0d6963f7d28e17f72".into()
            )))
            .is_err()
        );
    }
}
