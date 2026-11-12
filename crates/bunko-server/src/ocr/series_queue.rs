//! The queue file v2 over Bunko's HTTP (`docs/design/queue-file-v2.md` §7):
//! `GET /mokuro-reader/<series>/.mokuro-series-queue.json`, rendered from memory by
//! the HTTP sink and never stored or listed.
//!
//! **The HTTP sink** ([`SeriesQueueHttp`]) keeps what it last served of every series
//! ([`bunko_sched::series_queue::Sink`]): a series' body changes only when the dirty
//! test says so, and its ETag with it. The plan is priced lazily: only when a GET finds
//! the last snapshot older than [`RENDER_SECONDS`], never in a background loop. The
//! work clock is refitted at most every [`bunko_sched::series_queue::REFIT_SECONDS`],
//! as part of such a refresh. The body adds `live` (hold, held rows, clock): a local
//! library has no lease marker to carry them.
//!
//! **Settle rule** (§5.4): a job that is no longer owed stays in the file, shown
//! running, while its series' metadata has a recompile pending (the hook
//! [`SeriesQueueHttp::set_settled`] installs; settled by default).
//!
//! **The store's copy** (§6) is written by the connected store's sink
//! (`bunko_remote::series_queue`); [`StoreQueueSource`] is where it reads the queue: the
//! same snapshot as this sink (the plan priced at most once per [`RENDER_SECONDS`] for
//! both), only once the scheduler's first walk of the library is over (a partial owed
//! board would read as every other job finished).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use bunko_sched::series_queue::{Published, Sink, Snapshot, live_block};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use sha2::Digest;

use super::OcrControl;

/// The file's name in each series folder.
pub use bunko_core::names::SERIES_QUEUE_FILE;

/// A series' body is rebuilt at most this often (and the plan priced for it).
pub const RENDER_SECONDS: f64 = 5.0;
/// The `batch_s` the HTTP copy announces: how often a reader may ask again.
pub const BATCH_SECONDS: u64 = 5;
/// Bodies at least this long are offered gzipped.
pub const GZIP_MIN_BYTES: usize = 1024;

/// `settled(folder)`: the series has no write of its own left to land (§5.3).
pub type SettledHook = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// One series' representation: the JSON body, its strong ETag, and its gzip form with
/// its own tag (bodies under [`GZIP_MIN_BYTES`] have none).
#[derive(Clone, Debug)]
pub struct Rendered {
    pub body: bytes::Bytes,
    pub etag: String,
    pub packed: Option<(bytes::Bytes, String)>,
}

struct State {
    sink: Sink,
    /// The snapshot the sink was last brought up to, and when.
    snap: Option<(Instant, Arc<Snapshot>)>,
    /// Bodies rendered since that snapshot, by series fold key.
    bodies: HashMap<String, Rendered>,
}

/// The HTTP sink. One per OCR control.
pub struct SeriesQueueHttp {
    state: Mutex<State>,
    building: tokio::sync::Mutex<()>,
    settled: RwLock<Option<SettledHook>>,
    instance_id: RwLock<String>,
}

impl Default for SeriesQueueHttp {
    fn default() -> Self {
        Self::new()
    }
}

/// `"` + the first 32 hex digits of sha256 over the compact body without `at` + `"`.
pub fn etag(body: &serde_json::Map<String, Value>) -> String {
    let mut stable = body.clone();
    stable.remove("at");
    let text = serde_json::to_vec(&Value::Object(stable)).unwrap_or_default();
    let digest = hex::encode(sha2::Sha256::digest(&text));
    format!("\"{}\"", &digest[..32])
}

fn gzip(body: &[u8]) -> bytes::Bytes {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
    let _ = enc.write_all(body);
    bytes::Bytes::from(enc.finish().unwrap_or_default())
}

fn wall_clock() -> f64 {
    bunko_sched::rate::Clock::time(&bunko_sched::rate::SystemClock::default())
}

impl SeriesQueueHttp {
    pub fn new() -> Self {
        // Nothing is kept across restarts: revisions start from the time in ms, so a
        // reader holding an earlier revision of this instance never takes a new file
        // for a stale copy, and the id is new for each run.
        let rev_base = (wall_clock() * 1000.0) as u64;
        let id: [u8; 8] = rand::random();
        SeriesQueueHttp {
            state: Mutex::new(State {
                sink: Sink::new(rev_base, Some(bunko_sched::series_queue::REFIT_SECONDS)),
                snap: None,
                bodies: HashMap::new(),
            }),
            building: tokio::sync::Mutex::new(()),
            settled: RwLock::new(None),
            instance_id: RwLock::new(hex::encode(id)),
        }
    }

    /// The id files carry: a connected store's `instance_id` (the marker's).
    pub fn set_instance_id(&self, id: &str) {
        *self.instance_id.write() = id.to_string();
        self.state.lock().bodies.clear();
    }

    pub fn instance_id(&self) -> String {
        self.instance_id.read().clone()
    }

    /// Install the settle check (§5.3/§5.4).
    pub fn set_settled(&self, hook: SettledHook) {
        *self.settled.write() = Some(hook);
    }

    /// Bring the sink up to `snap` (a fresh snapshot of the scheduler).
    pub fn refresh(&self, snap: Snapshot) {
        let hook = self.settled.read().clone();
        let settled = move |folder: &str| hook.as_ref().is_none_or(|h| h(folder));
        let mut st = self.state.lock();
        st.sink.refresh(&snap, &settled);
        st.snap = Some((Instant::now(), Arc::new(snap)));
        st.bodies.clear();
    }

    /// The representation of series `fold` (spelled `folder` in the library), from the
    /// last snapshot (an empty, unheld queue before the first).
    pub fn render(&self, fold: &str, folder: &str) -> Rendered {
        let mut st = self.state.lock();
        if let Some(hit) = st.bodies.get(fold) {
            return hit.clone();
        }
        let snap = st.snap.as_ref().map(|(_, s)| s.clone()).unwrap_or_else(|| {
            Arc::new(Snapshot {
                now: wall_clock(),
                ..Default::default()
            })
        });
        let clock = st.sink.clock_at(snap.now).clone();
        let empty;
        let published = match st.sink.series.get(fold) {
            Some(p) => p,
            None => {
                empty = Published::empty(folder, &clock, snap.now);
                &empty
            }
        };
        let mut body = published.body(&self.instance_id.read());
        body.insert("live".into(), live_block(&snap, &clock, BATCH_SECONDS));
        let tag = etag(&body);
        let bytes =
            bytes::Bytes::from(serde_json::to_vec(&Value::Object(body)).unwrap_or_default());
        let packed = (bytes.len() >= GZIP_MIN_BYTES)
            .then(|| (gzip(&bytes), format!("{}-gz\"", &tag[..tag.len() - 1])));
        let out = Rendered {
            body: bytes,
            etag: tag,
            packed,
        };
        st.bodies.insert(fold.to_string(), out.clone());
        out
    }

    fn fresh(&self) -> bool {
        self.state
            .lock()
            .snap
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed().as_secs_f64() < RENDER_SECONDS)
    }

    /// Bring the sink up to the scheduler when its last snapshot is
    /// [`RENDER_SECONDS`] old (one question to the scheduler, which prices the plan).
    async fn ensure_fresh(&self, ocr: Option<&OcrControl>) {
        if self.fresh() {
            return;
        }
        let _guard = self.building.lock().await;
        if self.fresh() {
            return;
        }
        let snap = match ocr {
            Some(o) => o.ask(|s| s.series_queue_snapshot()).await,
            None => Some(Snapshot {
                now: wall_clock(),
                ..Default::default()
            }),
        };
        match snap {
            Some(snap) => self.refresh(snap),
            // The scheduler did not answer: serve what it last said.
            None => {
                if let Some((at, _)) = self.state.lock().snap.as_mut() {
                    *at = Instant::now();
                }
            }
        }
    }

    /// The current representation of series `fold`: the plan is priced (one question
    /// to the scheduler) only when the last snapshot is [`RENDER_SECONDS`] old.
    pub async fn current(&self, ocr: Option<&OcrControl>, fold: &str, folder: &str) -> Rendered {
        self.ensure_fresh(ocr).await;
        self.render(fold, folder)
    }

    /// The scheduler's snapshot, at most [`RENDER_SECONDS`] old (shared with the
    /// store's sink, [`StoreQueueSource`]). None before the first.
    pub async fn snapshot(&self, ocr: &OcrControl) -> Option<Arc<Snapshot>> {
        self.ensure_fresh(Some(ocr)).await;
        self.state.lock().snap.as_ref().map(|(_, s)| s.clone())
    }

    /// What the sink last published of series `fold`. For tests and the admin.
    pub fn published(&self, fold: &str) -> Option<Published> {
        self.state.lock().sink.series.get(fold).cloned()
    }

    /// Forget the last snapshot, so the next GET asks the scheduler again. For tests.
    pub fn expire(&self) {
        let mut st = self.state.lock();
        st.snap = None;
        st.bodies.clear();
    }
}

/// Where a connected store's series-file sink reads the queue (§6): the HTTP sink's
/// snapshot (shared, so the plan is priced once for both), once the scheduler has
/// walked the whole library; and the library runtime's recompile check (§5.3).
pub struct StoreQueueSource {
    ocr: OcrControl,
    library: std::sync::Weak<crate::library::LibraryRuntime>,
    /// The scheduler finished its first walk (it stays so).
    walked: std::sync::atomic::AtomicBool,
}

impl StoreQueueSource {
    pub fn new(
        ocr: OcrControl,
        library: std::sync::Weak<crate::library::LibraryRuntime>,
    ) -> StoreQueueSource {
        StoreQueueSource {
            ocr,
            library,
            walked: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl bunko_remote::SeriesQueueSource for StoreQueueSource {
    fn snapshot(&self) -> bunko_remote::SnapshotFuture<'_> {
        Box::pin(async move {
            use std::sync::atomic::Ordering;
            if !self.walked.load(Ordering::Acquire) {
                // Until the first walk ends the owed board is partial: a series missing
                // from it is not a series that finished.
                let walked = self.ocr.ask(|s| s.walks > 0).await?;
                if !walked {
                    return None;
                }
                self.walked.store(true, Ordering::Release);
            }
            let snap = self.ocr.series_queue_state().snapshot(&self.ocr).await?;
            Some((*snap).clone())
        })
    }

    fn regeneration_pending(&self, folder: &str) -> bool {
        self.library
            .upgrade()
            .is_some_and(|l| l.series_regeneration_pending(folder))
    }
}
