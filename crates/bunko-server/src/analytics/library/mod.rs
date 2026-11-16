//! The library's shape (design §7): file types, page formats inside archives, growth,
//! uploads per uploader and dormant series, from a background scan.
//!
//! A pass has two phases, both off the async workers and both bounded in memory:
//! - **files**: a stat-only walk of the library through `bunko_store::fs` (so a cloud
//!   library is listed from its store's index and nothing is downloaded), a few folders
//!   per `spawn_blocking` hop, folded into fixed-size totals plus one small entry per
//!   series ([`walk`]). It writes `library_scan`, the day's `library_daily` row and
//!   `library_series` in one transaction.
//! - **pages** (local libraries with `library_scan.page_formats` on): each archive that
//!   has no `archive_pages` row for its current size and mtime has its zip central
//!   directory read ([`pages`]), one archive per `spawn_blocking` call and at most
//!   [`PAGES_PER_SECOND`] a second; rows land in small batches, so a restart resumes
//!   where the last pass stopped instead of reading everything again. Rows of archives
//!   that are gone are dropped first.
//!
//! Passes run 2 minutes after start, then every `library_scan.interval_hours`, and at
//! once on [`Scanner::rescan`]. A cloud library whose store is not mounted yet (the
//! store's first sync can take minutes) is retried every minute. With analytics off
//! nothing is written.
//!
//! WebDAV parity: the walk sees the files, not the requests, so an archive, sidecar or
//! cover is counted however it arrived (a WebDAV PUT or MOVE from the reader or any
//! generic client, the web UI, a direct copy into the folder, a cloud-side change).

pub mod api;
pub mod pages;
pub mod walk;

use crate::analytics::{Analytics, now_s};
use bunko_db::analytics::AnalyticsDb;
use bunko_db::analytics::library::{
    ArchivePages, LibraryDay, LibrarySeries, PAGES_PENDING_KEY, UNREADABLE,
};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use walk::{Queued, Walk, key_hash};

/// The first pass waits this long after start (the server has other start-up work).
pub const STARTUP_DELAY: Duration = Duration::from_secs(120);
/// A cloud library whose store is not mounted yet, or analytics that are off, are
/// looked at again this often.
pub const RETRY: Duration = Duration::from_secs(60);
/// Archive directories read a second at most.
pub const PAGES_PER_SECOND: u64 = 20;
/// Folders listed per `spawn_blocking` hop.
const FOLDERS_PER_HOP: usize = 32;
/// Page-format rows written per transaction (or every [`BATCH_EVERY`]).
const BATCH: usize = 25;
const BATCH_EVERY: Duration = Duration::from_secs(2);
/// `archive_pages` keys looked at per read when dropping vanished archives.
const PRUNE_CHUNK: usize = 1000;

/// What the scanner is doing (the API's `scanning` and `scan`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStatus {
    /// A pass is running.
    pub running: bool,
    /// A rescan was asked for and has not started yet.
    pub queued: bool,
    /// `files` or `pages` while running.
    pub phase: Option<&'static str>,
    /// Epoch seconds the running (or last) pass started.
    pub started_at: Option<i64>,
    /// Archives still to read for page formats, as of the last pass of this process.
    pub pages_pending: Option<u64>,
    /// Why no pass can run now (a cloud store not mounted yet, analytics off).
    pub waiting: Option<String>,
    /// The last pass's error, if it failed.
    pub error: Option<String>,
}

/// How a pass ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// The cloud store is not mounted yet.
    NotReady,
    /// Analytics are off: nothing was written.
    Disabled,
    Cancelled,
}

/// What one pass found (tests, logs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassStats {
    pub series: u64,
    pub volumes: u64,
    pub files: u64,
    pub archives_read: u64,
    pub archives_dropped: u64,
    pub pages_pending: u64,
}

#[derive(Clone)]
struct Shared {
    wake: Arc<Notify>,
    /// The settings changed: the wait until the next pass is worked out again.
    settings: Arc<Notify>,
    status: Arc<Mutex<ScanStatus>>,
    /// One pass at a time (the periodic task and a test's `scan_now`).
    pass: Arc<tokio::sync::Mutex<()>>,
    gap: Duration,
}

/// The library scanner (see the module docs). Lives in [`Analytics`].
pub struct Scanner(Shared);

impl Scanner {
    pub fn new() -> Scanner {
        Scanner(Shared {
            wake: Arc::new(Notify::new()),
            settings: Arc::new(Notify::new()),
            status: Arc::new(Mutex::new(ScanStatus::default())),
            pass: Arc::new(tokio::sync::Mutex::new(())),
            gap: Duration::from_millis(1000 / PAGES_PER_SECOND),
        })
    }

    /// Start the periodic scan (2 min after start, then every
    /// `analytics.library_scan.interval_hours`, and at once on [`Scanner::rescan`]).
    pub fn start(&self, a: &Analytics, stop: CancellationToken) {
        let me = self.0.clone();
        let a = a.clone();
        tokio::spawn(async move {
            let interval = |a: &Analytics| {
                Duration::from_secs(u64::from(a.config().library_scan.interval_hours.max(1)) * 3600)
            };
            let mut next = tokio::time::Instant::now() + STARTUP_DELAY;
            // When the last pass ended, while the next one waits out the interval (a
            // settings change works the wait out again from it).
            let mut ended: Option<tokio::time::Instant> = None;
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tokio::time::sleep_until(next) => {}
                    _ = me.wake.notified() => {}
                    _ = me.settings.notified() => {
                        if let Some(end) = ended {
                            next = end + interval(&a);
                        }
                        continue;
                    }
                }
                let outcome = me.pass(&a, &stop).await;
                let now = tokio::time::Instant::now();
                (next, ended) = match outcome {
                    Ok((Outcome::Cancelled, _)) => return,
                    Ok((Outcome::NotReady | Outcome::Disabled, _)) => (now + RETRY, None),
                    Ok((Outcome::Done, _)) => (now + interval(&a), Some(now)),
                    Err(e) => {
                        tracing::warn!("[ANALYTICS] library scan failed: {e}");
                        (now + interval(&a), Some(now))
                    }
                };
            }
        });
    }

    /// The `analytics:` settings changed: a new interval applies to the wait already
    /// running (a running pass sees `enabled` going off at its next archive).
    pub fn settings_changed(&self) {
        self.0.settings.notify_one();
    }

    /// Scan again now (`POST .../library/rescan`). A pass already running finishes
    /// first; the next one starts right after it.
    pub fn rescan(&self) {
        self.0.status.lock().queued = true;
        self.0.wake.notify_one();
    }

    pub fn status(&self) -> ScanStatus {
        self.0.status.lock().clone()
    }

    /// Run one whole pass now and wait for it (tests; the periodic task runs the same).
    pub async fn scan_now(&self, a: &Analytics) -> anyhow::Result<(Outcome, PassStats)> {
        self.0.pass(a, &CancellationToken::new()).await
    }
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

/// The library root a pass walks.
fn library_root(a: &Analytics) -> PathBuf {
    match a.library_runtime() {
        Some(l) => l.library_path().to_path_buf(),
        None => a.core().layout.library(),
    }
}

/// Is the library in a cloud folder (from the config, which is what the server opened)?
pub fn cloud_library(a: &Analytics) -> bool {
    a.core().config.read().storage.library.backend.is_cloud()
}

/// Read one archive's directory (blocking). `None`: it is gone or cannot be opened (it
/// is tried again next pass).
fn read_archive(q: &Queued, now: i64) -> Option<ArchivePages> {
    let mut f = bunko_store::fs::File::open(&q.path).ok()?;
    let meta = f.metadata().ok()?;
    // A panic on a hostile archive marks that archive unreadable instead of stopping
    // the page phase at it on every pass.
    let counted =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pages::count_pages(&mut f)))
            .unwrap_or_else(|_| Err(std::io::Error::other("the zip reader panicked")));
    let (pages, formats) = match counted {
        Ok(c) => (i64::try_from(c.pages).unwrap_or(i64::MAX), c.formats_json()),
        Err(e) => {
            tracing::debug!(
                "[ANALYTICS] library scan: {} has no readable zip directory: {e}",
                q.volume_key
            );
            (UNREADABLE, "{}".to_string())
        }
    };
    Some(ArchivePages {
        volume_key: q.volume_key.clone(),
        size: meta.len(),
        mtime: walk::mtime_ns(&meta),
        pages,
        formats,
        scanned_at: now,
    })
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow::anyhow!("library scan worker: {e}"))
}

impl Shared {
    async fn pass(
        &self,
        a: &Analytics,
        stop: &CancellationToken,
    ) -> anyhow::Result<(Outcome, PassStats)> {
        let _one = self.pass.lock().await;
        if !a.enabled() {
            let mut st = self.status.lock();
            st.queued = false;
            st.waiting = Some("analytics are off".into());
            return Ok((Outcome::Disabled, PassStats::default()));
        }
        let root = library_root(a);
        let cloud = cloud_library(a);
        if cloud && !bunko_store::store_for(&root).is_some_and(|s| s.caps().remote) {
            let mut st = self.status.lock();
            st.queued = false;
            st.waiting = Some("the cloud library is not connected yet".into());
            return Ok((Outcome::NotReady, PassStats::default()));
        }
        let pages_on = !cloud && a.config().library_scan.page_formats;
        let started_at = now_s();
        {
            let mut st = self.status.lock();
            *st = ScanStatus {
                running: true,
                phase: Some("files"),
                started_at: Some(started_at),
                pages_pending: st.pages_pending,
                ..ScanStatus::default()
            };
        }
        let out = self.run(a, stop, root, cloud, pages_on, started_at).await;
        let mut st = self.status.lock();
        st.running = false;
        st.phase = None;
        if let Err(e) = &out {
            st.error = Some(e.to_string());
        }
        out
    }

    async fn run(
        &self,
        a: &Analytics,
        stop: &CancellationToken,
        root: PathBuf,
        cloud: bool,
        pages_on: bool,
        started_at: i64,
    ) -> anyhow::Result<(Outcome, PassStats)> {
        let clock = Instant::now();
        let db = a.db().clone();
        // Files: a few folders per hop, the stack and the totals moved in and out.
        let mut w = Walk::new(pages_on);
        let mut stack: Vec<(PathBuf, String)> = vec![(root, String::new())];
        while !stack.is_empty() {
            if stop.is_cancelled() {
                return Ok((Outcome::Cancelled, PassStats::default()));
            }
            if !a.enabled() {
                // Turned off mid-walk: nothing of this pass is written.
                return Ok((Outcome::Disabled, PassStats::default()));
            }
            let db = db.clone();
            (w, stack) = blocking(move || {
                for _ in 0..FOLDERS_PER_HOP {
                    let Some((dir, rel)) = stack.pop() else { break };
                    let subs = w.folder(&dir, &rel, pages_on.then_some(db.as_ref()));
                    stack.extend(subs.into_iter().rev());
                }
                (w, stack)
            })
            .await?;
        }
        let files_ms = clock.elapsed().as_millis() as u64;
        let total = w.total();
        let mut stats = PassStats {
            series: w.series.len() as u64,
            volumes: w.volumes(),
            files: total.files,
            pages_pending: w.pages_pending,
            ..PassStats::default()
        };
        let mut summary = w.summary();
        if let Some(m) = summary.as_object_mut() {
            m.insert("started_at".into(), started_at.into());
            m.insert("duration_ms".into(), files_ms.into());
            m.insert("cloud".into(), cloud.into());
            m.insert("page_formats".into(), pages_on.into());
        }
        let day = LibraryDay {
            day: started_at.div_euclid(86400),
            series: stats.series,
            volumes: stats.volumes,
            bytes: total.bytes,
            json: Some(w.kinds_json().to_string()),
        };
        let series: Vec<LibrarySeries> = std::mem::take(&mut w.series)
            .into_iter()
            .map(|(key, s)| LibrarySeries {
                series_key: key,
                title: s.title,
                volumes: s.volumes,
                bytes: s.bytes,
            })
            .collect();
        let scanned_at = now_s();
        let (pending, json) = (w.pages_pending, summary.to_string());
        let db2 = db.clone();
        blocking(move || {
            db2.write(|tx| {
                tx.set_library_scan(scanned_at, &json)?;
                tx.set_library_day(&day)?;
                tx.replace_library_series(&series)?;
                if pages_on {
                    tx.meta_set(PAGES_PENDING_KEY, &pending.to_string())?;
                }
                Ok(())
            })
        })
        .await??;
        tracing::info!(
            series = stats.series,
            volumes = stats.volumes,
            files = stats.files,
            ms = files_ms,
            "[ANALYTICS] library scan: file types done"
        );
        if !pages_on {
            return Ok((Outcome::Done, stats));
        }
        {
            let mut st = self.status.lock();
            st.phase = Some("pages");
            st.pages_pending = Some(w.pages_pending);
        }
        let seen = std::mem::take(&mut w.seen);
        stats.archives_dropped = prune_vanished(&db, seen).await?;
        let queue = std::mem::take(&mut w.queue);
        let capped = w.pages_pending > queue.len() as u64;
        let mut left = w.pages_pending;
        let mut batch: Vec<ArchivePages> = Vec::with_capacity(BATCH);
        let mut flushed = Instant::now();
        for q in queue {
            if stop.is_cancelled() {
                write_batch(&db, &mut batch, left).await?;
                return Ok((Outcome::Cancelled, stats));
            }
            if !a.enabled() {
                // Turned off: what was read is kept, nothing more is opened.
                write_batch(&db, &mut batch, left).await?;
                self.status.lock().pages_pending = Some(left);
                return Ok((Outcome::Disabled, stats));
            }
            let t0 = Instant::now();
            let row = blocking(move || read_archive(&q, now_s())).await?;
            left = left.saturating_sub(1);
            if let Some(row) = row {
                stats.archives_read += 1;
                batch.push(row);
            }
            if batch.len() >= BATCH || flushed.elapsed() >= BATCH_EVERY {
                write_batch(&db, &mut batch, left).await?;
                flushed = Instant::now();
                self.status.lock().pages_pending = Some(left);
            }
            if let Some(rest) = self.gap.checked_sub(t0.elapsed()) {
                tokio::select! {
                    _ = stop.cancelled() => {}
                    _ = tokio::time::sleep(rest) => {}
                }
            }
        }
        write_batch(&db, &mut batch, left).await?;
        stats.pages_pending = left;
        self.status.lock().pages_pending = Some(left);
        if let Some(m) = summary.as_object_mut() {
            m.insert(
                "pages_duration_ms".into(),
                (clock.elapsed().as_millis() as u64)
                    .saturating_sub(files_ms)
                    .into(),
            );
            m.insert("archives_read".into(), stats.archives_read.into());
        }
        let json = summary.to_string();
        let db2 = db.clone();
        blocking(move || db2.write(|tx| tx.set_library_scan(scanned_at, &json))).await??;
        tracing::info!(
            read = stats.archives_read,
            dropped = stats.archives_dropped,
            left,
            "[ANALYTICS] library scan: page formats done"
        );
        if capped {
            // More archives were stale than one pass queues: go on with the rest.
            self.wake.notify_one();
        }
        Ok((Outcome::Done, stats))
    }
}

/// Write `batch` (and how many archives are still to read) in one transaction.
async fn write_batch(
    db: &Arc<AnalyticsDb>,
    batch: &mut Vec<ArchivePages>,
    left: u64,
) -> anyhow::Result<()> {
    let rows = std::mem::take(batch);
    let db = db.clone();
    blocking(move || {
        db.write(|tx| {
            for r in &rows {
                tx.put_archive_pages(r)?;
            }
            tx.meta_set(PAGES_PENDING_KEY, &left.to_string())
        })
    })
    .await??;
    Ok(())
}

/// Drop the `archive_pages` rows of archives the walk did not see, a chunk of keys
/// at a time.
async fn prune_vanished(db: &Arc<AnalyticsDb>, seen: HashSet<u64>) -> anyhow::Result<u64> {
    let db = db.clone();
    let dropped = blocking(move || -> bunko_db::Result<u64> {
        let mut after = String::new();
        let mut dropped = 0u64;
        loop {
            let keys = db.archive_page_keys_after(&after, PRUNE_CHUNK)?;
            let Some(last) = keys.last().cloned() else {
                break;
            };
            let gone: Vec<String> = keys
                .into_iter()
                .filter(|k| !seen.contains(&key_hash(k)))
                .collect();
            if !gone.is_empty() {
                dropped += gone.len() as u64;
                db.write(|tx| tx.delete_archive_pages(&gone))?;
            }
            after = last;
        }
        Ok(dropped)
    })
    .await??;
    Ok(dropped)
}
