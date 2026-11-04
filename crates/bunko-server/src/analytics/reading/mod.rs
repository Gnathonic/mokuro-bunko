//! Reading (design §6): what people read, from the per-user files the reader uploads
//! (`positions.json`, `history/**`, `volume-data.json`), read back from disk after they
//! are written, off the request path.
//!
//! The collector calls [`Reading::on_file_written`] for every successful (2xx) PUT,
//! DELETE or MOVE of a per-user file, whatever client made it (Mokuro Reader's WebDAV
//! sync or any WebDAV client writing the same paths); it queues the event on a bounded
//! channel (drops counted), never blocking the request. The worker
//! ([`Reading::start`]):
//!
//! - debounces per (user, file): a file is read [`DEBOUNCE`] after its last write, or
//!   [`MAX_WAIT`] after its first, so a burst (the reader's DELETE-then-PUT, a sync of
//!   every file, positions every few seconds) is read once, as it ended;
//! - reads the file from the user's own tree (`users/<username>/...`, or the cloud root
//!   for its progress user) through `bunko_store::fs` on a blocking thread, bounded in
//!   size per kind;
//! - folds it into `analytics.sqlite` in one transaction ([`bunko_db::analytics::reading`]).
//!
//! Every file says its whole state (the positions of 30 days, a month of history, all
//! progress), and every write is a diff against what was stored (positions by `t`,
//! history by `seq` past a cursor, progress replaced), so reading a file twice, late,
//! or after its DELETE and PUT again never counts anything twice, and a dropped event
//! is made up by the next write of that file.

pub mod api;
pub mod codec;
pub mod files;
pub mod resolve;

use crate::analytics::Analytics;
use bunko_core::reader_tree::{self, Caller};
use bunko_dav::DavTarget;
use bunko_db::analytics::reading::{
    Attribution, DeviceSeen, HistoryEvent, HistoryKind, ProgressRecord, is_skip,
};
use files::FileKind;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How many events wait for the worker before new ones are dropped.
pub const QUEUE: usize = 1024;
/// A file is read this long after its last write...
pub const DEBOUNCE: Duration = Duration::from_secs(10);
/// ...or this long after its first, if writes keep coming.
pub const MAX_WAIT: Duration = Duration::from_secs(60);
/// Files waiting for their debounce; a new one past this is dropped (counted; the next
/// write of it makes up for it).
pub const PENDING_MAX: usize = 4096;
/// How long a shutdown waits for the files still waiting their debounce to be read.
pub const STOP_BUDGET: Duration = Duration::from_secs(10);

/// Largest file read back, per kind (bigger ones are skipped and counted).
pub const MAX_POSITIONS_BYTES: u64 = 4 << 20;
pub const MAX_VOLUME_DATA_BYTES: u64 = 32 << 20;
pub const MAX_SEGMENT_BYTES: u64 = reader_tree::HISTORY_MAX_FILE_BYTES;
pub const MAX_DEVICE_FILE_BYTES: u64 = 64 << 10;

/// History events dated before this (2015-01-01) or more than a day ahead of now are
/// not counted (a device with a broken clock).
const MIN_EVENT_MS: i64 = 1_420_070_400_000;
const FUTURE_EVENT_MS: i64 = 86_400_000;

/// A per-user file that was just written (or removed, or moved into place).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileWritten {
    pub username: String,
    /// [`DavTarget::Progress`] (the root files and `history/**`).
    pub target: DavTarget,
    /// The per-user-relative path: `positions.json`, `history/<device>/<file>`, ...
    pub rel: String,
    /// `PUT`, `DELETE` or `MOVE` (a MOVE reports its destination).
    pub method: &'static str,
    /// Epoch milliseconds the request started.
    pub at_ms: i64,
}

struct Pending {
    first: Instant,
    /// Write order: files due together are read in the order they were first written
    /// (a page is credited to the first view that arrives with it).
    order: u64,
    due: Instant,
    at_ms: i64,
}

/// What the worker has done since start (the reading view shows it).
#[derive(Default)]
struct Counters {
    files: AtomicU64,
    positions: AtomicU64,
    segments: AtomicU64,
    volume_data: AtomicU64,
    device_files: AtomicU64,
    missing: AtomicU64,
    too_large: AtomicU64,
    unknown_format: AtomicU64,
    malformed: AtomicU64,
    failed: AtomicU64,
    coalesced: AtomicU64,
    pending_dropped: AtomicU64,
    events_out_of_range: AtomicU64,
}

/// [`Reading::stats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct ReadingStats {
    pub queued: u64,
    pub dropped: u64,
    pub pending: u64,
    pub files: u64,
    pub positions: u64,
    pub segments: u64,
    pub volume_data: u64,
    pub device_files: u64,
    pub missing: u64,
    pub too_large: u64,
    pub unknown_format: u64,
    pub malformed: u64,
    pub failed: u64,
    pub coalesced: u64,
    pub pending_dropped: u64,
    pub events_out_of_range: u64,
}

pub struct Reading {
    tx: mpsc::Sender<FileWritten>,
    /// Until the worker takes it.
    rx: Mutex<Option<mpsc::Receiver<FileWritten>>>,
    queued: AtomicU64,
    dropped: AtomicU64,
    pending: Mutex<HashMap<(String, String), Pending>>,
    /// The next [`Pending::order`].
    next_order: AtomicU64,
    /// One file is read at a time (the worker and [`Reading::settle`]).
    work: tokio::sync::Mutex<()>,
    resolver: resolve::Resolver,
    counters: Counters,
    /// [`DEBOUNCE`] in milliseconds (tests shorten it).
    debounce_ms: AtomicU64,
    /// The worker, for [`Reading::stopped`].
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// What reading one file did.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Positions,
    Segment,
    VolumeData,
    DeviceFile,
    /// Gone (a DELETE, or a MOVE's source).
    Missing,
    TooLarge,
    UnknownFormat,
    Malformed(String),
    /// Not a file the reading area reads (profiles, goals, anything else).
    Ignored,
    Failed(String),
}

impl Reading {
    pub fn new() -> Reading {
        let (tx, rx) = mpsc::channel(QUEUE);
        Reading {
            tx,
            rx: Mutex::new(Some(rx)),
            queued: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            next_order: AtomicU64::new(0),
            work: tokio::sync::Mutex::new(()),
            resolver: resolve::Resolver::new(),
            counters: Counters::default(),
            debounce_ms: AtomicU64::new(DEBOUNCE.as_millis() as u64),
            worker: Mutex::new(None),
        }
    }

    /// Shorten the debounce (tests).
    #[doc(hidden)]
    pub fn set_debounce(&self, d: Duration) {
        self.debounce_ms
            .store(d.as_millis() as u64, Ordering::Relaxed);
    }

    /// Hold the worker off reading files while the guard lives (tests: a slow read).
    #[doc(hidden)]
    pub async fn hold_work(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.work.lock().await
    }

    fn debounce(&self) -> Duration {
        Duration::from_millis(self.debounce_ms.load(Ordering::Relaxed))
    }

    /// Queue a written per-user file for the worker (never blocks; a full queue drops
    /// the event and counts it). Files the reading area never reads (profiles, goals)
    /// take no slot.
    pub fn on_file_written(&self, ev: FileWritten) {
        if !reads(&ev) {
            return;
        }
        match self.tx.try_send(ev) {
            Ok(()) => {
                self.queued.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Events queued since start.
    pub fn queued(&self) -> u64 {
        self.queued.load(Ordering::Relaxed)
    }

    /// Events dropped (queue full) since start.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Take the queue's receiving end (the worker, once).
    pub fn take_receiver(&self) -> Option<mpsc::Receiver<FileWritten>> {
        self.rx.lock().take()
    }

    pub fn resolver(&self) -> &resolve::Resolver {
        &self.resolver
    }

    pub fn stats(&self) -> ReadingStats {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        ReadingStats {
            queued: self.queued(),
            dropped: self.dropped(),
            pending: self.pending.lock().len() as u64,
            files: get(&c.files),
            positions: get(&c.positions),
            segments: get(&c.segments),
            volume_data: get(&c.volume_data),
            device_files: get(&c.device_files),
            missing: get(&c.missing),
            too_large: get(&c.too_large),
            unknown_format: get(&c.unknown_format),
            malformed: get(&c.malformed),
            failed: get(&c.failed),
            coalesced: get(&c.coalesced),
            pending_dropped: get(&c.pending_dropped),
            events_out_of_range: get(&c.events_out_of_range),
        }
    }

    /// Hold `ev` for its debounce. Files the reading area does not read (profiles,
    /// goals: their bytes are counted by the collector) are let go here.
    fn enqueue(&self, ev: FileWritten, now: Instant) {
        if !reads(&ev) {
            return;
        }
        let mut pending = self.pending.lock();
        let len = pending.len();
        match pending.entry((ev.username, ev.rel)) {
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let p = o.get_mut();
                p.due = (now + self.debounce()).min(p.first + MAX_WAIT);
                p.at_ms = p.at_ms.max(ev.at_ms);
                self.counters.coalesced.fetch_add(1, Ordering::Relaxed);
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                if len >= PENDING_MAX {
                    self.counters
                        .pending_dropped
                        .fetch_add(1, Ordering::Relaxed);
                    return;
                }
                v.insert(Pending {
                    first: now,
                    order: self.next_order.fetch_add(1, Ordering::Relaxed),
                    due: now + self.debounce(),
                    at_ms: ev.at_ms,
                });
            }
        }
    }

    fn next_due(&self) -> Option<Instant> {
        self.pending.lock().values().map(|p| p.due).min()
    }

    /// The files due by `now` (every one with `all`), first written first.
    fn take_due(&self, now: Instant, all: bool) -> Vec<(String, String, i64)> {
        let mut pending = self.pending.lock();
        let keys: Vec<(String, String)> = pending
            .iter()
            .filter(|(_, p)| all || p.due <= now)
            .map(|(k, _)| k.clone())
            .collect();
        // `order` follows `first` (both are set on a file's first write).
        let mut out: Vec<(u64, (String, String, i64))> = keys
            .into_iter()
            .filter_map(|k| pending.remove(&k).map(|p| (p.order, (k.0, k.1, p.at_ms))))
            .collect();
        out.sort_by_key(|(order, _)| *order);
        out.into_iter().map(|(_, f)| f).collect()
    }

    /// Start the worker: debounce per (user, file), read each file back, update the
    /// reading tables. A batch of due files is read in a task of its own, so the queue
    /// keeps being taken meanwhile (into the bounded, coalescing pending map); on stop
    /// the worker reads what is still waiting (at most [`STOP_BUDGET`]).
    pub fn start(&self, a: &Analytics, stop: CancellationToken) {
        let Some(mut rx) = self.take_receiver() else {
            return;
        };
        let a = a.clone();
        let handle = tokio::spawn(async move {
            let mut batch: Option<tokio::task::JoinHandle<()>> = None;
            loop {
                let next = match batch {
                    Some(_) => None,
                    None => a.reading().next_due(),
                };
                let wake = async move {
                    match next {
                        Some(at) => tokio::time::sleep_until(at.into()).await,
                        None => std::future::pending::<()>().await,
                    }
                };
                let reading = async {
                    match batch.as_mut() {
                        Some(h) => {
                            let _ = h.await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    _ = stop.cancelled() => break,
                    ev = rx.recv() => match ev {
                        Some(ev) => a.reading().enqueue(ev, Instant::now()),
                        None => break,
                    },
                    _ = reading => batch = None,
                    _ = wake => {
                        let due = a.reading().take_due(Instant::now(), false);
                        let a2 = a.clone();
                        batch = Some(tokio::spawn(async move {
                            a2.reading().process(&a2, due).await;
                        }));
                    }
                }
            }
            // Shutting down: what was written last (a restart right after a sync) is
            // read now, not lost with the queue.
            while let Ok(ev) = rx.try_recv() {
                a.reading().enqueue(ev, Instant::now());
            }
            let rest = async {
                if let Some(h) = batch {
                    let _ = h.await;
                }
                let due = a.reading().take_due(Instant::now(), true);
                a.reading().process(&a, due).await;
            };
            if tokio::time::timeout(STOP_BUDGET, rest).await.is_err() {
                tracing::warn!(
                    "[ANALYTICS] shutting down before every written reading file was read; the next write of each makes up for it"
                );
            }
        });
        if let Some(old) = self.worker.lock().replace(handle) {
            old.abort();
        }
    }

    /// Wait for the worker to end (after its stop token): [`super::Analytics::stop`].
    pub async fn stopped(&self) {
        let handle = self.worker.lock().take();
        if let Some(h) = handle {
            let _ = h.await;
        }
    }

    /// Read every waiting file now, without its debounce (tests; the worker's own
    /// queue when it was never started). Returns how many files were read.
    pub async fn settle(&self, a: &Analytics) -> usize {
        let drained: Vec<FileWritten> = {
            let mut rx = self.rx.lock();
            let mut out = Vec::new();
            if let Some(rx) = rx.as_mut() {
                while let Ok(ev) = rx.try_recv() {
                    out.push(ev);
                }
            }
            out
        };
        let now = Instant::now();
        for ev in drained {
            self.enqueue(ev, now);
        }
        let due = self.take_due(now, true);
        let n = due.len();
        self.process(a, due).await;
        n
    }

    async fn process(&self, a: &Analytics, files: Vec<(String, String, i64)>) {
        if files.is_empty() {
            return;
        }
        let _one_at_a_time = self.work.lock().await;
        for (username, rel, at_ms) in files {
            let a2 = a.clone();
            let (u, r) = (username.clone(), rel.clone());
            let started = Instant::now();
            let outcome = tokio::task::spawn_blocking(move || {
                read_file(&a2, &u, &r, at_ms, crate::analytics::now_ms())
            })
            .await
            .unwrap_or_else(|e| Outcome::Failed(format!("worker: {e}")));
            self.count(&username, &rel, &outcome, started.elapsed());
        }
    }

    fn count(&self, username: &str, rel: &str, outcome: &Outcome, took: Duration) {
        let c = &self.counters;
        let counter = match outcome {
            Outcome::Positions => &c.positions,
            Outcome::Segment => &c.segments,
            Outcome::VolumeData => &c.volume_data,
            Outcome::DeviceFile => &c.device_files,
            Outcome::Missing => &c.missing,
            Outcome::TooLarge => &c.too_large,
            Outcome::UnknownFormat => &c.unknown_format,
            Outcome::Malformed(_) => &c.malformed,
            Outcome::Ignored => return,
            Outcome::Failed(_) => &c.failed,
        };
        c.files.fetch_add(1, Ordering::Relaxed);
        counter.fetch_add(1, Ordering::Relaxed);
        match outcome {
            Outcome::Failed(e) => {
                tracing::warn!("[ANALYTICS] reading {rel} of {username} failed: {e}");
            }
            Outcome::Malformed(e) => {
                tracing::debug!("[ANALYTICS] {rel} of {username} is not readable: {e}");
            }
            other => tracing::debug!(
                "[ANALYTICS] {rel} of {username}: {other:?} in {:.1} ms",
                took.as_secs_f64() * 1000.0
            ),
        }
    }
}

/// Is `ev` a file the reading area reads?
fn reads(ev: &FileWritten) -> bool {
    ev.target == DavTarget::Progress && files::kind_of(&ev.rel) != FileKind::Other
}

impl Default for Reading {
    fn default() -> Self {
        Self::new()
    }
}

/// The user's per-user file at `rel` (their tree: `users/<username>`, or the cloud
/// root for its progress user, as the DAV layer maps it).
fn user_file(a: &Analytics, username: &str, rel: &str) -> Option<PathBuf> {
    let core = a.core();
    let caller = Caller {
        username,
        owns_cloud_root: core.owns_cloud_root(Some(username)),
    };
    let base = reader_tree::user_base(&core.layout.library(), &core.layout.users(), caller)?;
    let rel = rel.trim_matches('/');
    if rel
        .split('/')
        .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return None;
    }
    Some(base.join(rel))
}

enum Bytes {
    Read(Vec<u8>),
    Missing,
    TooLarge,
}

/// At most `max` bytes of `path` (one more tells it is bigger).
fn read_bounded(path: &Path, max: u64) -> std::io::Result<Bytes> {
    let file = match bunko_store::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Bytes::Missing),
        Err(e) => return Err(e),
    };
    let mut buf = Vec::new();
    file.take(max + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        return Ok(Bytes::TooLarge);
    }
    Ok(Bytes::Read(buf))
}

/// Read one file back and fold it in (blocking).
fn read_file(a: &Analytics, username: &str, rel: &str, at_ms: i64, now_ms: i64) -> Outcome {
    let kind = files::kind_of(rel);
    let max = match kind {
        FileKind::Positions => MAX_POSITIONS_BYTES,
        FileKind::VolumeData => MAX_VOLUME_DATA_BYTES,
        FileKind::Month { .. } | FileKind::Legacy { .. } => MAX_SEGMENT_BYTES,
        FileKind::DeviceFacts { .. } => MAX_DEVICE_FILE_BYTES,
        FileKind::Other => return Outcome::Ignored,
    };
    let Some(path) = user_file(a, username, rel) else {
        return Outcome::Ignored;
    };
    let bytes = match read_bounded(&path, max) {
        Ok(Bytes::Read(b)) => b,
        Ok(Bytes::TooLarge) => return Outcome::TooLarge,
        Ok(Bytes::Missing) => {
            if kind == FileKind::VolumeData {
                // The reader's progress is gone with its file (a DELETE that no PUT
                // followed within the debounce).
                if let Err(e) = a.db().write(|tx| tx.clear_volume_progress(username)) {
                    return Outcome::Failed(e.to_string());
                }
            }
            return Outcome::Missing;
        }
        Err(e) => return Outcome::Failed(format!("{}: {e}", path.display())),
    };
    let result = match kind {
        FileKind::Positions => positions(a, username, &bytes, now_ms),
        FileKind::VolumeData => volume_data(a, username, &bytes),
        FileKind::Month { device, month } => {
            segment(a, username, device, Some(month), &bytes, (at_ms, now_ms))
        }
        FileKind::Legacy { device } => segment(a, username, device, None, &bytes, (at_ms, now_ms)),
        FileKind::DeviceFacts { device } => device_file(a, username, device, &bytes, at_ms),
        FileKind::Other => Ok(Outcome::Ignored),
    };
    result.unwrap_or_else(|e| Outcome::Failed(e.to_string()))
}

fn file_error(e: files::FileError) -> Outcome {
    match e {
        files::FileError::UnknownFormat => Outcome::UnknownFormat,
        files::FileError::Malformed(m) => Outcome::Malformed(m),
    }
}

/// `positions.json`: diffed against what was stored (`ingest_positions`); the devices
/// its entries name are seen at their `t`.
fn positions(
    a: &Analytics,
    username: &str,
    bytes: &[u8],
    now_ms: i64,
) -> bunko_db::Result<Outcome> {
    let entries = match files::parse_positions(bytes, now_ms) {
        Ok(e) => e,
        Err(e) => return Ok(file_error(e)),
    };
    let known = a
        .reading()
        .resolver
        .attributions(a.main_db(), entries.iter().map(|e| e.volume_uuid.as_str()));
    let mut devices: HashMap<&str, (i64, i64)> = HashMap::new();
    for e in &entries {
        if files::is_device_id(&e.device) {
            let s = e.t_ms.div_euclid(1000);
            let d = devices.entry(e.device.as_str()).or_insert((s, s));
            d.0 = d.0.min(s);
            d.1 = d.1.max(s);
        }
    }
    a.db().write(|tx| {
        tx.ingest_positions(username, &entries, &known, now_ms)?;
        for (id, (first, last)) in &devices {
            tx.see_device(
                username,
                &DeviceSeen {
                    device_id: id,
                    label: None,
                    first_seen: *first,
                    last_seen: *last,
                    source: "positions",
                },
            )?;
        }
        Ok(())
    })?;
    Ok(Outcome::Positions)
}

/// The UTC `YYYY-MM` of an epoch-ms time (`historyMonthOf`).
fn month_of(t_ms: i64) -> Option<String> {
    let d = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(t_ms) * 1_000_000).ok()?;
    Some(format!("{:04}-{:02}", d.year(), u8::from(d.month())))
}

/// A history segment: decoded (the reader's codec), checked against its path as the
/// reader's importer checks it, and its events past the cursor added. `at`: when it
/// was written and now, epoch ms.
fn segment(
    a: &Analytics,
    username: &str,
    device: &str,
    month: Option<&str>,
    bytes: &[u8],
    (at_ms, now_ms): (i64, i64),
) -> bunko_db::Result<Outcome> {
    let opts = codec::Options {
        keep_page_chars: true,
        ..codec::Options::default()
    };
    let seg = match codec::decode_with(bytes, opts) {
        Ok(s) => s,
        Err(codec::SegmentError::UnsupportedFormat) => return Ok(Outcome::UnknownFormat),
        Err(codec::SegmentError::TooLarge) => return Ok(Outcome::TooLarge),
        Err(e) => return Ok(Outcome::Malformed(e.to_string())),
    };
    let matches_path = seg.header.device == device
        && match month {
            None => seg.header.legacy,
            Some(m) => {
                !seg.header.legacy
                    && seg.header.month == m
                    && seg
                        .events
                        .iter()
                        .all(|e| month_of(e.t).as_deref() == Some(m))
            }
        };
    if !matches_path {
        return Ok(Outcome::Malformed("segment does not match its path".into()));
    }
    // A legacy segment's events belong to `legacy:<volume>` (one cursor each).
    let legacy_devices: Vec<String> = if seg.header.legacy {
        seg.volumes
            .iter()
            .map(|v| format!("legacy:{}", v.as_deref().unwrap_or_default()))
            .collect()
    } else {
        Vec::new()
    };
    let mut out_of_range = 0u64;
    let events: Vec<HistoryEvent<'_>> = seg
        .events
        .iter()
        .filter(|e| {
            let ok = e.t >= MIN_EVENT_MS && e.t <= now_ms.saturating_add(FUTURE_EVENT_MS);
            out_of_range += u64::from(!ok);
            ok
        })
        .map(|e| HistoryEvent {
            cursor_device: if seg.header.legacy {
                legacy_devices.get(e.volume).map_or("", String::as_str)
            } else {
                device
            },
            seq: e.seq,
            t_ms: e.t,
            volume_uuid: seg.volume(e),
            kind: match e.payload {
                codec::Payload::Page {
                    first_page,
                    chars_sum,
                    dwell_ms,
                    ..
                } => {
                    let page_chars = seg.page_chars(e);
                    // The reader's `prepareVolume`: a native view shows a page per
                    // `page_chars` value; a converted (legacy) turn is its first page
                    // only, and never a skip.
                    let page_chars = if seg.header.legacy {
                        page_chars.get(..1).unwrap_or(&[0.0])
                    } else {
                        page_chars
                    };
                    HistoryKind::Page {
                        first_page,
                        page_chars,
                        dwell_ms: dwell_ms.map(|d| d.round() as i64),
                        skip: !seg.header.legacy && is_skip(chars_sum, dwell_ms),
                    }
                }
                codec::Payload::Adjust {
                    time_delta_ms,
                    chars_delta,
                } => HistoryKind::Adjust {
                    time_delta_ms: time_delta_ms.round() as i64,
                    chars_delta: chars_delta.round() as i64,
                },
                codec::Payload::Restart => HistoryKind::Restart,
                _ => HistoryKind::Other,
            },
        })
        .collect();
    if out_of_range > 0 {
        a.reading()
            .counters
            .events_out_of_range
            .fetch_add(out_of_range, Ordering::Relaxed);
    }
    let uuids: HashSet<&str> = events.iter().map(|e| e.volume_uuid).collect();
    let known = a.reading().resolver.attributions(a.main_db(), uuids);
    let segment_key = month.unwrap_or("legacy");
    let seen = at_ms.div_euclid(1000);
    a.db().write(|tx| {
        tx.ingest_history(username, segment_key, &events, &known)?;
        tx.see_device(
            username,
            &DeviceSeen {
                device_id: device,
                label: None,
                first_seen: seen,
                last_seen: seen,
                source: "history",
            },
        )?;
        Ok(())
    })?;
    Ok(Outcome::Segment)
}

/// `volume-data.json`: the user's progress rows replaced (tombstoned volumes left
/// out), and the user's own device names kept.
fn volume_data(a: &Analytics, username: &str, bytes: &[u8]) -> bunko_db::Result<Outcome> {
    let data = match files::parse_volume_data(bytes) {
        Ok(d) => d,
        Err(e) => return Ok(file_error(e)),
    };
    let live: Vec<&files::VolumeRecord> = data.records.iter().filter(|r| !r.deleted).collect();
    let known = a
        .reading()
        .resolver
        .attributions(a.main_db(), live.iter().map(|r| r.volume_uuid.as_str()));
    let records: Vec<ProgressRecord> = live
        .iter()
        .map(|r| {
            let attribution = match known.get(&r.volume_uuid) {
                Some(lib) => Attribution {
                    volume_title: r.volume_title.clone().or_else(|| lib.volume_title.clone()),
                    ..lib.clone()
                },
                None => {
                    let title = r
                        .series_title
                        .as_deref()
                        .map(str::trim)
                        .filter(|t| !t.is_empty());
                    Attribution {
                        volume_key: None,
                        series_key: title
                            .map(bunko_db::fold_series_title_key)
                            .unwrap_or_default(),
                        series_title: title.map(str::to_string),
                        volume_title: r.volume_title.clone(),
                    }
                }
            };
            ProgressRecord {
                volume_uuid: r.volume_uuid.clone(),
                series_uuid: r.series_uuid.clone(),
                attribution,
                progress: clamp_count(r.progress),
                chars: clamp_count(r.chars),
                completed: r.completed,
                completed_at: r.completed_at.as_deref().and_then(files::iso_seconds),
                time_read_min: if r.time_read_min.is_finite() {
                    r.time_read_min.max(0.0)
                } else {
                    0.0
                },
                last_progress_update: r
                    .last_progress_update
                    .as_deref()
                    .and_then(files::iso_seconds),
            }
        })
        .collect();
    let now_s = crate::analytics::now_s();
    a.db().write(|tx| {
        tx.replace_volume_progress(username, &records, now_s)?;
        for (id, name) in &data.device_names {
            tx.name_device(username, id, name)?;
        }
        Ok(())
    })?;
    Ok(Outcome::VolumeData)
}

fn clamp_count(v: f64) -> i64 {
    if v.is_finite() {
        v.clamp(0.0, i64::MAX as f64) as i64
    } else {
        0
    }
}

/// `history/<device>/device.json`: the device's generated label and the span its facts
/// say it was seen in (never past the upload).
fn device_file(
    a: &Analytics,
    username: &str,
    device: &str,
    bytes: &[u8],
    at_ms: i64,
) -> bunko_db::Result<Outcome> {
    let facts = match files::parse_device_facts(bytes, device) {
        Ok(f) => f,
        Err(e) => return Ok(file_error(e)),
    };
    let at = at_ms.div_euclid(1000);
    let first = files::iso_seconds(&facts.first_seen).unwrap_or(at).min(at);
    let last = files::iso_seconds(&facts.last_seen).unwrap_or(at).min(at);
    let label = facts.label();
    a.db().write(|tx| {
        tx.see_device(
            username,
            &DeviceSeen {
                device_id: device,
                label: Some(&label),
                first_seen: first,
                last_seen: last.max(first),
                source: "history",
            },
        )
    })?;
    Ok(Outcome::DeviceFile)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(user: &str, rel: &str) -> FileWritten {
        FileWritten {
            username: user.into(),
            target: DavTarget::Progress,
            rel: rel.into(),
            method: "PUT",
            at_ms: 1,
        }
    }

    #[test]
    fn debounce_coalesces_per_user_and_file_and_caps_the_wait() {
        let r = Reading::new();
        let t0 = Instant::now();
        r.enqueue(ev("alice", "positions.json"), t0);
        r.enqueue(ev("alice", "positions.json"), t0 + Duration::from_secs(5));
        r.enqueue(ev("bob", "positions.json"), t0);
        // Profiles and goals are bytes only.
        r.enqueue(ev("alice", "profiles.json"), t0);
        r.enqueue(ev("alice", "history/dev-a/2026-10.events"), t0);
        assert_eq!(r.stats().pending, 3);
        assert_eq!(r.stats().coalesced, 1);
        assert!(r.take_due(t0 + Duration::from_secs(9), false).is_empty());
        let due = r.take_due(t0 + Duration::from_secs(11), false);
        assert_eq!(
            due.len(),
            2,
            "bob and the segment; alice's write moved hers on"
        );
        assert_eq!(r.take_due(t0 + Duration::from_secs(15), false).len(), 1);
        // A file written every few seconds is still read within MAX_WAIT.
        let t1 = t0 + Duration::from_secs(100);
        for i in 0..30 {
            r.enqueue(
                ev("carol", "positions.json"),
                t1 + Duration::from_secs(3 * i),
            );
        }
        let due_at = r.next_due().unwrap();
        assert!(due_at <= t1 + MAX_WAIT, "{:?}", due_at - t1);
    }

    #[test]
    fn pending_is_bounded() {
        let r = Reading::new();
        let t0 = Instant::now();
        for i in 0..PENDING_MAX + 10 {
            r.enqueue(ev(&format!("u{i}"), "positions.json"), t0);
        }
        let s = r.stats();
        assert_eq!((s.pending, s.pending_dropped), (PENDING_MAX as u64, 10));
    }

    #[test]
    fn months_are_utc() {
        assert_eq!(month_of(1_791_028_800_000).as_deref(), Some("2026-10"));
        assert_eq!(month_of(1_793_491_199_999).as_deref(), Some("2026-10"));
        assert_eq!(month_of(1_793_491_200_000).as_deref(), Some("2026-11"));
    }
}
