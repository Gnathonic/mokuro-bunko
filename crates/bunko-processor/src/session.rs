//! The session runtime: ops in, events out, for every session of one processor
//! connection (remote) or of the in-process local processor.
//!
//! Per `open_session` there are three actors:
//!
//! * a **feeder** (tokio task) takes the session's `volume` ops strictly in arrival
//!   order, makes each archive available (fetches it, or finds it on disk), and hands
//!   it to the runner — so the claims the pipeline receives are always a prefix of
//!   the library's order (what makes "only a delivered claim can be blamed" sound);
//! * a **runner** (dedicated OS threads, never tokio workers) loads the models through
//!   [`PagePipeline::open`], then runs volumes through [`VolumeRunner::run_volume`];
//! * a **reporter** (tokio task) turns the runner's messages into protocol events in
//!   order, uploads (or hands over) each finished sidecar BEFORE its `volume_done`,
//!   and sends the session's one and only `exit`.
//!
//! Invariants kept (spec ocr-recognizers §1, ocr-scheduling §17, remote-processors §8):
//! every volume handed to the runner gets exactly one `volume_started` and exactly one
//! of `volume_done`/`volume_failed`, terminal events in arrival order; `fetch ready`
//! precedes the claim's `volume_started`; a claim that never reached the runner is
//! either returned (`volume_returned{class}`) or, when the session is stopping,
//! abandoned without a word; `cancel` abandons the whole session and says nothing but
//! its `exit`; `close_session` finishes the accepted volumes then exits; a processor
//! that is leaving says nothing at all (the library reads a socket that closes without
//! `exit` as the processor going away, which blames nobody); every session ends with
//! exactly one `exit`. Volume `seconds` partition the session's time: a volume's
//! seconds run from `max(fed, previous volume's end)` to its end.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bunko_proto::{
    BenchOp, Event, MAX_OUTSTANDING_VOLUMES, Op, RowSpec, VolumeOp, return_class, valid_id,
};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bench::{BenchAbort, BenchConfig, BenchRun};
use crate::client::{LibraryClient, sha256_file};
use crate::fetch::{ArchiveFetcher, FetchError, FetchedArchive, TransferFault};
use crate::hostload::{HostMeter, HostSample, cpu_pressure, other_cpu_share};
use crate::pipeline::{
    CancelToken, PagePipeline, PageProgress, PipelineReport, ReadyInfo, RunError, VolumeMeta,
    VolumeOutcome, VolumeRunner,
};

/// How often `stats` may go out while pages flow.
const STATS_INTERVAL: Duration = Duration::from_secs(2);
/// How long a benchmark waits for this machine's cancelled sessions to wind down.
const QUIET_WAIT: Duration = Duration::from_secs(60);
/// A volume_done whose sidecar did not reach the library becomes this failure.
pub const SIDECAR_NOT_SENT: &str = "the finished sidecar could not be sent from the processor";

// --- where archives come from and sidecars go -----------------------------------------------

/// The archive a volume runs on, held until its terminal event.
pub(crate) enum ArchiveHandle {
    Fetched(FetchedArchive),
    Local(PathBuf),
}

impl ArchiveHandle {
    fn path(&self) -> &Path {
        match self {
            ArchiveHandle::Fetched(f) => f.path(),
            ArchiveHandle::Local(p) => p,
        }
    }
}

pub(crate) struct Delivered {
    archive: ArchiveHandle,
    /// The `fetch ready` detail (remote only).
    ready: Option<BTreeMap<String, Value>>,
    damaged_note: Option<String>,
}

/// A remote connection's way of getting archives and returning sidecars.
pub(crate) struct RemoteLink {
    pub fetcher: ArchiveFetcher,
    pub client: LibraryClient,
    /// Upload path template with `{sid}` and `{claim}`.
    pub results: String,
    /// `<storage>/.processing/work`.
    pub work: PathBuf,
    /// Set when the library refused the account mid-download: the connection ends.
    pub lost: CancellationToken,
}

/// The in-process processor's way: archives are local paths, sidecars are written
/// where the server collects them.
pub(crate) struct LocalLink {
    pub results_dir: PathBuf,
}

pub(crate) enum Link {
    Remote(Box<RemoteLink>),
    Local(LocalLink),
}

/// Why a benchmark's sample is not there.
enum SampleError {
    Abort(BenchAbort),
    /// The library refused the account: the connection ends, nothing is said.
    Lost(String),
}

/// A running benchmark: cancel it, and hear when it is over.
struct BenchHandle {
    cancel: CancellationToken,
    finished: CancellationToken,
}

impl Link {
    /// The directory a claim's sidecar is written into.
    fn claim_dir(&self, sid: &str, claim: &str) -> PathBuf {
        match self {
            Link::Remote(r) => r.work.join(sid).join(claim),
            Link::Local(l) => l.results_dir.join(sid).join(claim),
        }
    }

    async fn fetch(&self, op: &VolumeOp, session: &Arc<Session>) -> Result<Delivered, FetchError> {
        match self {
            Link::Remote(remote) => {
                let claim = op.claim.clone();
                let who = session.clone();
                let progress = move |mut detail: BTreeMap<String, Value>| {
                    let state = detail
                        .remove("state")
                        .and_then(|s| s.as_str().map(str::to_string))
                        .unwrap_or_else(|| "downloading".to_string());
                    who.say(Event::Fetch {
                        sid: who.sid.clone(),
                        id: claim.clone(),
                        state,
                        detail,
                    });
                };
                let where_ = op
                    .archive
                    .rsplit('/')
                    .take(2)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("/");
                let label = format!("{} {}", op.claim, where_);
                let fetched = remote
                    .fetcher
                    .fetch(
                        &op.archive,
                        op.size,
                        &session.stopping,
                        Some(&progress),
                        &label,
                    )
                    .await?;
                let note = fetched.damaged_note();
                Ok(Delivered {
                    ready: Some(fetched.summary()),
                    damaged_note: note,
                    archive: ArchiveHandle::Fetched(fetched),
                })
            }
            Link::Local(_) => {
                let path = PathBuf::from(&op.archive);
                match tokio::fs::metadata(&path).await {
                    Ok(m) if m.is_file() => Ok(Delivered {
                        archive: ArchiveHandle::Local(path),
                        ready: None,
                        damaged_note: None,
                    }),
                    _ => Err(FetchError::Fault(TransferFault::new(
                        return_class::MISSING,
                        format!("the library has no {}", op.archive),
                    ))),
                }
            }
        }
    }

    /// Hand the finished sidecar over; its sha256.
    async fn deliver(
        &self,
        sid: &str,
        claim: &str,
        path: &Path,
        name: &str,
    ) -> Result<String, String> {
        match self {
            Link::Remote(r) => {
                r.client
                    .upload_result(&r.results, sid, claim, path, name)
                    .await
            }
            Link::Local(_) => sha256_file(path)
                .await
                .map_err(|e| format!("could not read {}: {e}", path.display())),
        }
    }

    /// A claim is over: its workspace goes (a local claim's finished sidecar stays for
    /// the server to collect).
    async fn discard(&self, sid: &str, claim: &str, delivered: bool) {
        if matches!(self, Link::Local(_)) && delivered {
            return;
        }
        let _ = tokio::fs::remove_dir_all(self.claim_dir(sid, claim)).await;
    }

    async fn finish_session(&self, sid: &str) {
        match self {
            Link::Remote(r) => {
                let _ = tokio::fs::remove_dir_all(r.work.join(sid)).await;
            }
            // Only if empty: finished sidecars wait there for the server.
            Link::Local(l) => {
                let _ = tokio::fs::remove_dir(l.results_dir.join(sid)).await;
            }
        }
    }

    /// A benchmark's scratch directory.
    fn bench_dir(&self, bid: &str) -> PathBuf {
        match self {
            Link::Remote(r) => r.work.join(bid),
            Link::Local(l) => l.results_dir.join(bid),
        }
    }

    /// The benchmark's sample archive: fetched from the library like a volume (held
    /// until the benchmark ends), or read in place by the local processor.
    async fn bench_sample(
        &self,
        op: &BenchOp,
        cancel: &CancellationToken,
    ) -> Result<(PathBuf, Option<FetchedArchive>), SampleError> {
        match self {
            Link::Remote(remote) => {
                let label = format!("the benchmark sample {}", op.bid);
                match remote
                    .fetcher
                    .fetch(&op.sample, None, cancel, None, &label)
                    .await
                {
                    Ok(fetched) => Ok((fetched.path().to_path_buf(), Some(fetched))),
                    Err(FetchError::Cancelled) => Err(SampleError::Abort(BenchAbort::Cancelled)),
                    Err(FetchError::LostLibrary(reason)) => Err(SampleError::Lost(reason)),
                    Err(FetchError::Fault(fault)) => Err(SampleError::Abort(BenchAbort::Fatal(
                        format!("could not fetch the benchmark sample: {fault}")
                            .chars()
                            .take(300)
                            .collect(),
                    ))),
                }
            }
            Link::Local(_) => {
                let path = PathBuf::from(&op.sample);
                match tokio::fs::metadata(&path).await {
                    Ok(m) if m.is_file() => Ok((path, None)),
                    _ => Err(SampleError::Abort(BenchAbort::Fatal(format!(
                        "could not fetch the benchmark sample: there is no {}",
                        op.sample
                    )))),
                }
            }
        }
    }

    fn lost_library(&self, reason: &str, leaving: &AtomicBool) {
        if let Link::Remote(r) = self {
            tracing::error!("the library could not be read from ({reason}); registering again");
            // Silence first: not one more event may reach the library.
            leaving.store(true, Ordering::SeqCst);
            r.lost.cancel();
        }
    }
}

// --- the hub ------------------------------------------------------------------------------

type Sessions = Arc<Mutex<HashMap<String, Arc<Session>>>>;

/// Every session of one connection (or of the local processor).
pub(crate) struct Hub {
    pipeline: Arc<dyn PagePipeline>,
    link: Arc<Link>,
    tx: mpsc::UnboundedSender<Event>,
    leaving: Arc<AtomicBool>,
    sessions: Sessions,
    benches: Mutex<HashMap<String, BenchHandle>>,
    bench_config: BenchConfig,
}

impl Hub {
    pub(crate) fn new(
        pipeline: Arc<dyn PagePipeline>,
        link: Link,
        tx: mpsc::UnboundedSender<Event>,
        leaving: Arc<AtomicBool>,
        bench_config: BenchConfig,
    ) -> Arc<Hub> {
        Arc::new(Hub {
            pipeline,
            link: Arc::new(link),
            tx,
            leaving,
            sessions: Arc::default(),
            benches: Mutex::new(HashMap::new()),
            bench_config,
        })
    }

    pub(crate) fn session_count(&self) -> usize {
        self.sessions.lock().len()
    }

    fn say(&self, event: Event) {
        if !self.leaving.load(Ordering::SeqCst) {
            let _ = self.tx.send(event);
        }
    }

    fn session(&self, sid: &str) -> Option<Arc<Session>> {
        self.sessions.lock().get(sid).cloned()
    }

    /// One op. Whatever goes wrong handling it costs that op only.
    pub(crate) fn handle(self: &Arc<Self>, op: Op) {
        let ids: Vec<(&str, Option<&str>)> = match &op {
            Op::OpenSession { sid, .. } | Op::CloseSession { sid } => vec![("sid", Some(sid))],
            Op::Volume(v) => vec![("sid", Some(&v.sid)), ("claim", Some(&v.claim))],
            Op::Cancel { sid, claim, bid } => {
                vec![
                    ("sid", sid.as_deref()),
                    ("claim", claim.as_deref()),
                    ("bid", bid.as_deref()),
                ]
            }
            Op::Bench(b) => vec![("bid", Some(&b.bid))],
            Op::Heartbeat => Vec::new(),
        };
        for (key, value) in ids {
            if let Some(value) = value
                && !valid_id(value)
            {
                tracing::error!("dropping an op: its {key} {value:?} is not an id");
                return;
            }
        }
        match op {
            Op::Heartbeat => {}
            Op::OpenSession { sid, generation } => self.open_session(sid, generation),
            Op::Volume(volume) => match self.session(&volume.sid) {
                Some(session) => session.offer(volume),
                None => tracing::warn!(
                    "volume {:?} for an unknown session {:?}",
                    volume.claim,
                    volume.sid
                ),
            },
            Op::Cancel { bid: Some(bid), .. } => match self.benches.lock().get(&bid) {
                Some(handle) => {
                    tracing::info!("benchmark {bid}: cancelled");
                    handle.cancel.cancel();
                }
                None => tracing::info!("cancel for benchmark {bid}, which is not running here"),
            },
            Op::Cancel {
                sid: Some(sid),
                claim,
                ..
            } => {
                if let Some(session) = self.session(&sid) {
                    tracing::info!(
                        "session {sid}: cancelled ({})",
                        claim.as_deref().unwrap_or("the whole session")
                    );
                    session.abandon();
                }
            }
            Op::Cancel { .. } => {
                tracing::warn!("a cancel op naming neither a session nor a benchmark")
            }
            Op::CloseSession { sid } => {
                if let Some(session) = self.session(&sid) {
                    session.close();
                }
            }
            Op::Bench(bench) => self.bench(bench),
        }
    }

    /// A `bench` op: measure the row on this machine (see [`crate::bench`]). Runs on a
    /// dedicated OS thread; every benchmark ends with `exit` (after `bench_done`, or
    /// after `fatal`), except when this processor is leaving.
    fn bench(self: &Arc<Self>, op: BenchOp) {
        if self.leaving.load(Ordering::SeqCst) {
            return;
        }
        let cancel = CancellationToken::new();
        let finished = CancellationToken::new();
        {
            let mut benches = self.benches.lock();
            if benches.contains_key(&op.bid) {
                tracing::warn!("benchmark {} is already running here", op.bid);
                return;
            }
            benches.insert(
                op.bid.clone(),
                BenchHandle {
                    cancel: cancel.clone(),
                    finished: finished.clone(),
                },
            );
        }
        tracing::info!(
            "benchmark {}: {} ({}) over a sample of {} pages{}",
            op.bid,
            op.spec.name,
            op.spec.engine,
            op.pages,
            if op.precision_only {
                ", precision only"
            } else {
                ""
            }
        );
        let hub = self.clone();
        tokio::spawn(async move {
            let local = matches!(&*hub.link, Link::Local(_));
            let workspace = hub.link.bench_dir(&op.bid);
            // The library cancelled this machine's sessions for the benchmark: measure
            // only once they are gone (0.5.2 waited for the machine to go quiet), so no
            // winding-down pipeline shares the device with the warm-up.
            hub.quiet(QUIET_WAIT, &op.bid).await;
            let result = match hub.link.bench_sample(&op, &cancel).await {
                Err(SampleError::Lost(reason)) => {
                    hub.link
                        .lost_library(&format!("benchmark {}: {reason}", op.bid), &hub.leaving);
                    hub.benches.lock().remove(&op.bid);
                    finished.cancel();
                    return;
                }
                Err(SampleError::Abort(abort)) => Err(abort),
                Ok((sample, held)) => match tokio::fs::create_dir_all(&workspace).await {
                    Err(e) => Err(BenchAbort::Fatal(format!(
                        "could not make the benchmark workspace {}: {e}",
                        workspace.display()
                    ))),
                    Ok(()) => {
                        hub.run_bench(op.clone(), sample, held, workspace.clone(), cancel)
                            .await
                    }
                },
            };
            let _ = tokio::fs::remove_dir_all(&workspace).await;
            match &result {
                Ok(()) => tracing::info!("benchmark {}: done", op.bid),
                Err(BenchAbort::Cancelled) => tracing::info!("benchmark {}: cancelled", op.bid),
                Err(BenchAbort::Fatal(e)) => tracing::error!("benchmark {} failed: {e}", op.bid),
            }
            let error = match result {
                Ok(()) => None,
                Err(BenchAbort::Cancelled) => Some("cancelled".to_string()),
                Err(BenchAbort::Fatal(e)) => Some(e),
            };
            // 0.5.2: the local runner exited 0 or 1; the remote bridge said no status.
            let returncode = local.then_some(i32::from(error.is_some()));
            if let Some(error) = error {
                hub.say(Event::Fatal {
                    sid: op.bid.clone(),
                    error,
                });
            }
            hub.say(Event::Exit {
                sid: op.bid.clone(),
                returncode,
            });
            hub.benches.lock().remove(&op.bid);
            finished.cancel();
        });
    }

    /// Wait (up to `wait`) for every open session to end.
    async fn quiet(&self, wait: Duration, bid: &str) {
        let sessions: Vec<CancellationToken> = self
            .sessions
            .lock()
            .values()
            .map(|s| s.finished.clone())
            .collect();
        if sessions.is_empty() {
            return;
        }
        let all = futures_util::future::join_all(
            sessions
                .into_iter()
                .map(|f| async move { f.cancelled().await }),
        );
        if tokio::time::timeout(wait, all).await.is_err() {
            tracing::warn!(
                "benchmark {bid}: sessions were still open after {}s; measuring anyway",
                wait.as_secs()
            );
        }
    }

    /// The measurement itself, on its own OS thread (model loads and page feeds block).
    async fn run_bench(
        self: &Arc<Self>,
        op: BenchOp,
        sample: PathBuf,
        held: Option<FetchedArchive>,
        workspace: PathBuf,
        cancel: CancellationToken,
    ) -> Result<(), BenchAbort> {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let pipeline = self.pipeline.clone();
        let config = self.bench_config.clone();
        let tx = self.tx.clone();
        let leaving = self.leaving.clone();
        let bid = op.bid.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("ocr-{bid}"))
            .spawn(move || {
                let say = move |event: Event| {
                    if !leaving.load(Ordering::SeqCst) {
                        let _ = tx.send(event);
                    }
                };
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    BenchRun::new(
                        pipeline.as_ref(),
                        &op,
                        sample,
                        workspace,
                        &config,
                        &cancel,
                        &say,
                    )
                    .run()
                }))
                .unwrap_or_else(|p| {
                    Err(BenchAbort::Fatal(format!(
                        "the benchmark panicked: {}",
                        panic_text(&*p)
                    )))
                });
                drop(held);
                let _ = done_tx.send(result);
            });
        if let Err(e) = spawned {
            return Err(BenchAbort::Fatal(format!(
                "could not start the benchmark thread: {e}"
            )));
        }
        done_rx.await.unwrap_or_else(|_| {
            Err(BenchAbort::Fatal(format!(
                "the benchmark {bid} ended without a word"
            )))
        })
    }

    fn open_session(self: &Arc<Self>, sid: String, generation: RowSpec) {
        if self.leaving.load(Ordering::SeqCst) {
            return;
        }
        if self.sessions.lock().contains_key(&sid) {
            tracing::warn!("open_session for {sid}, which is already open; ignored");
            return;
        }
        let opened_at = Instant::now();
        let (volumes_tx, volumes_rx) = mpsc::unbounded_channel();
        let (jobs_tx, jobs_rx) = std::sync::mpsc::channel::<Job>();
        let (reports_tx, reports_rx) = mpsc::unbounded_channel::<Msg>();
        let session = Arc::new(Session {
            sid: sid.clone(),
            tx: self.tx.clone(),
            leaving: self.leaving.clone(),
            quiet: AtomicBool::new(false),
            stopping: CancellationToken::new(),
            abandon: CancellationToken::new(),
            intake: Mutex::new(Intake {
                runner: Some((jobs_tx, reports_tx.clone())),
                volumes: Some(volumes_tx),
                next_seq: 0,
            }),
            book: Mutex::new(Book::default()),
            finished: CancellationToken::new(),
        });
        self.sessions.lock().insert(sid.clone(), session.clone());
        tracing::info!(
            "session {sid}: opening {} ({})",
            generation.name,
            generation.engine
        );

        let runner = RunnerCtx {
            session: session.clone(),
            pipeline: self.pipeline.clone(),
            spec: generation,
            jobs: jobs_rx,
            reports: reports_tx.clone(),
            opened_at,
        };
        let spawned = std::thread::Builder::new()
            .name(format!("ocr-session-{sid}"))
            .spawn(move || runner.run());
        if let Err(e) = spawned {
            tracing::error!("session {sid}: could not start its runner thread: {e}");
            session.close_intake();
            let _ = reports_tx.send(Msg::SpawnFailed(format!("could not start the runner: {e}")));
            let _ = reports_tx.send(Msg::Exit(None));
        }
        drop(reports_tx);
        tokio::spawn(feed(session.clone(), volumes_rx, self.link.clone()));
        tokio::spawn(report(
            session,
            reports_rx,
            self.link.clone(),
            self.sessions.clone(),
        ));
    }

    /// The processor is leaving: every session goes quiet BEFORE anything is stopped,
    /// so no event of the teardown reaches the library; then everything stops. Waits
    /// up to `wait` for the sessions to wind down.
    pub(crate) async fn leave(&self, wait: Duration) {
        self.leaving.store(true, Ordering::SeqCst);
        let sessions: Vec<Arc<Session>> = self.sessions.lock().values().cloned().collect();
        for session in &sessions {
            session.abandon();
        }
        let benches: Vec<CancellationToken> = self
            .benches
            .lock()
            .values()
            .map(|b| {
                b.cancel.cancel();
                b.finished.clone()
            })
            .collect();
        let all = futures_util::future::join_all(
            sessions
                .iter()
                .map(|s| s.finished.clone())
                .chain(benches)
                .map(|f| async move { f.cancelled().await }),
        );
        if tokio::time::timeout(wait, all).await.is_err() {
            tracing::warn!(
                "some sessions were still winding down after {}s; leaving them",
                wait.as_secs()
            );
        }
    }
}

// --- one session -------------------------------------------------------------------------

#[derive(Default)]
struct Book {
    /// Claims the library counts as outstanding (no terminal event yet).
    outstanding: HashSet<String>,
    /// Every claim this session was ever sent (duplicates are refused).
    seen: HashSet<String>,
    /// Claims whose archive was proven damaged at the library, and the note.
    damaged: HashMap<String, String>,
}

struct Intake {
    /// The runner's job queue and the reporter, while the session takes volumes.
    runner: Option<(std::sync::mpsc::Sender<Job>, mpsc::UnboundedSender<Msg>)>,
    /// The feeder's queue, while the session takes volume ops.
    volumes: Option<mpsc::UnboundedSender<VolumeOp>>,
    next_seq: u64,
}

pub(crate) struct Session {
    sid: String,
    tx: mpsc::UnboundedSender<Event>,
    /// The processor is leaving: nothing more is said.
    leaving: Arc<AtomicBool>,
    /// Cancelled: nothing more is said but `exit`.
    quiet: AtomicBool,
    /// No more volumes: the feeder drops its queue and downloads are cut.
    stopping: CancellationToken,
    /// The pipeline drops what it is doing.
    abandon: CancellationToken,
    intake: Mutex<Intake>,
    book: Mutex<Book>,
    finished: CancellationToken,
}

impl Session {
    fn silenced(&self, event: &Event) -> bool {
        self.leaving.load(Ordering::SeqCst)
            || (self.quiet.load(Ordering::SeqCst) && !matches!(event, Event::Exit { .. }))
    }

    fn say(&self, event: Event) {
        if !self.silenced(&event) {
            let _ = self.tx.send(event);
        }
    }

    fn talking(&self) -> bool {
        !self.leaving.load(Ordering::SeqCst) && !self.quiet.load(Ordering::SeqCst)
    }

    /// A `volume` op for this session.
    fn offer(&self, op: VolumeOp) {
        let claim = op.claim.clone();
        let intake = self.intake.lock();
        let Some(volumes) = intake
            .volumes
            .as_ref()
            .filter(|_| !self.stopping.is_cancelled())
        else {
            tracing::warn!(
                "volume {claim:?} arrived after session {} closed; ignored",
                self.sid
            );
            return;
        };
        let mut book = self.book.lock();
        if !book.seen.insert(claim.clone()) {
            tracing::error!(
                "volume {claim:?} is already in session {}; ignored",
                self.sid
            );
            return;
        }
        if book.outstanding.len() >= MAX_OUTSTANDING_VOLUMES {
            drop(book);
            drop(intake);
            let error = format!(
                "session {} already holds {MAX_OUTSTANDING_VOLUMES} volumes",
                self.sid
            );
            tracing::error!("volume {claim}: giving it back: {error}");
            self.say(Event::VolumeReturned {
                sid: self.sid.clone(),
                id: claim,
                class: return_class::LOCAL.to_string(),
                error,
                detail: BTreeMap::new(),
            });
            return;
        }
        book.outstanding.insert(claim);
        let _ = volumes.send(op);
    }

    /// No more volumes in; the runner's queue ends after what it holds.
    fn close_intake(&self) {
        let mut intake = self.intake.lock();
        intake.runner = None;
        intake.volumes = None;
    }

    /// `close_session`: finish the accepted volumes, abandon the downloads, exit.
    fn close(&self) {
        tracing::info!("session {}: closing", self.sid);
        self.stopping.cancel();
        self.close_intake();
    }

    /// `cancel` (or the processor leaving): stop everything, say nothing but `exit`.
    fn abandon(&self) {
        self.quiet.store(true, Ordering::SeqCst);
        self.stopping.cancel();
        self.abandon.cancel();
        self.close_intake();
    }

    fn settle(&self, claim: &str) {
        let mut book = self.book.lock();
        book.outstanding.remove(claim);
        book.damaged.remove(claim);
    }

    /// Tell the library this claim never reached the pipeline, and why — unless the
    /// session is stopping (the library settles its own claims then).
    fn give_back(&self, claim: &str, fault: &TransferFault) {
        self.settle(claim);
        if self.stopping.is_cancelled() || !self.talking() {
            tracing::info!(
                "volume {claim}: not returned ({}): its session is ending",
                fault.kind
            );
            return;
        }
        tracing::error!(
            "volume {claim}: giving it back ({}): {}",
            fault.kind,
            fault.message
        );
        self.say(Event::VolumeReturned {
            sid: self.sid.clone(),
            id: claim.to_string(),
            class: fault.kind.to_string(),
            error: fault.message.chars().take(300).collect(),
            detail: fault.detail(),
        });
    }

    /// Hand a verified archive to the runner (and say `fetch ready` ahead of it).
    fn hand_over(&self, op: &VolumeOp, delivered: Delivered, claim_dir: PathBuf) {
        let claim = op.claim.clone();
        let sidecar_name = sidecar_name(op);
        let meta = VolumeMeta {
            claim: claim.clone(),
            title: op.title.clone(),
            volume: op.volume_title.clone(),
            title_uuid: op.title_uuid.clone(),
            volume_uuid: op.volume_uuid.clone(),
            stem: archive_stem(&op.archive, &op.volume_title),
            sidecar_name: sidecar_name.clone(),
        };
        let mut intake = self.intake.lock();
        let accepting = !self.stopping.is_cancelled();
        let Some((jobs, reports)) = intake.runner.as_ref().filter(|_| accepting) else {
            drop(intake);
            tracing::info!(
                "volume {claim}: never fed: session {} had already ended",
                self.sid
            );
            self.settle(&claim);
            return;
        };
        let seq = intake.next_seq;
        if let Some(note) = delivered.damaged_note {
            self.book.lock().damaged.insert(claim.clone(), note);
        }
        if let Some(detail) = delivered.ready {
            let _ = reports.send(Msg::FetchReady {
                claim: claim.clone(),
                detail,
            });
        }
        let job = Job {
            seq,
            archive: delivered.archive,
            meta,
            out: claim_dir.join(&sidecar_name),
        };
        if jobs.send(job).is_ok() {
            intake.next_seq += 1;
        } else {
            drop(intake);
            tracing::warn!(
                "the runner of session {} would not take volume {claim}",
                self.sid
            );
            self.settle(&claim);
        }
    }
}

/// The sidecar's file name: `op.sidecar_name` (or `<volume_title>.mokuro`), reduced to
/// a basename so nothing off the wire can point outside the claim's directory.
fn sidecar_name(op: &VolumeOp) -> String {
    let raw = if op.sidecar_name.is_empty() {
        format!("{}.mokuro", op.volume_title)
    } else {
        op.sidecar_name.clone()
    };
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("").to_string();
    if base.is_empty() || base == "." || base == ".." {
        "volume.mokuro".to_string()
    } else {
        base
    }
}

/// The archive's own stem (the thumbnail rule and messages are keyed on it).
fn archive_stem(archive: &str, fallback: &str) -> String {
    let base = archive.rsplit(['/', '\\']).next().unwrap_or("");
    let stem = Path::new(base)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if stem.is_empty() {
        let f = fallback.rsplit(['/', '\\']).next().unwrap_or("");
        if f.is_empty() {
            "volume".to_string()
        } else {
            f.to_string()
        }
    } else {
        stem.to_string()
    }
}

// --- the feeder ---------------------------------------------------------------------------

async fn feed(
    session: Arc<Session>,
    mut volumes: mpsc::UnboundedReceiver<VolumeOp>,
    link: Arc<Link>,
) {
    while let Some(op) = volumes.recv().await {
        let claim = op.claim.clone();
        if session.stopping.is_cancelled() {
            tracing::info!(
                "volume {claim:?} was never fed: session {} had already ended",
                session.sid
            );
            session.settle(&claim);
            continue;
        }
        // Each fetch on its own task: a panic in it is a `local` return, not a dead
        // feeder (a dead feeder would leave every later claim to the wedge timer).
        let task = {
            let (session, link, op) = (session.clone(), link.clone(), op.clone());
            tokio::spawn(async move {
                let dir = link.claim_dir(&session.sid, &op.claim);
                let fetched = link.fetch(&op, &session).await;
                (fetched, dir)
            })
        };
        let (fetched, dir) = match task.await {
            Ok(done) => done,
            Err(e) => {
                session.give_back(
                    &claim,
                    &TransferFault::new(
                        return_class::LOCAL,
                        format!("the download task failed: {e}"),
                    ),
                );
                continue;
            }
        };
        match fetched {
            Ok(delivered) => {
                if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                    let fault = TransferFault::new(
                        return_class::LOCAL,
                        format!("could not make the workspace {}: {e}", dir.display()),
                    );
                    drop(delivered);
                    session.give_back(&claim, &fault);
                    continue;
                }
                session.hand_over(&op, delivered, dir);
            }
            Err(FetchError::Cancelled) => {
                tracing::info!("volume {claim}: its download ended with its session");
                session.settle(&claim);
            }
            Err(FetchError::LostLibrary(reason)) => {
                session.settle(&claim);
                link.lost_library(&format!("volume {claim}: {reason}"), &session.leaving);
            }
            Err(FetchError::Fault(fault)) => session.give_back(&claim, &fault),
        }
    }
}

// --- the runner ---------------------------------------------------------------------------

struct Job {
    seq: u64,
    archive: ArchiveHandle,
    meta: VolumeMeta,
    out: PathBuf,
}

struct Terminal {
    seq: u64,
    claim: String,
    sidecar_name: String,
    out: PathBuf,
    result: Result<VolumeOutcome, RunError>,
    started: bool,
    fed_at: Instant,
    finished_at: Instant,
    cpu_pressure: Option<f64>,
    other_cpu: Option<f64>,
}

enum Msg {
    Ready {
        info: ReadyInfo,
        startup_seconds: f64,
    },
    FetchReady {
        claim: String,
        detail: BTreeMap<String, Value>,
    },
    Started {
        claim: String,
        pages: u32,
    },
    Page {
        claim: String,
        done: u32,
        total: u32,
    },
    Stats {
        report: Option<PipelineReport>,
        cpu_pressure: Option<f64>,
        other_cpu: Option<f64>,
    },
    Terminal(Box<Terminal>),
    Fatal(String),
    SpawnFailed(String),
    Exit(Option<i32>),
}

struct RunnerCtx {
    session: Arc<Session>,
    pipeline: Arc<dyn PagePipeline>,
    spec: RowSpec,
    jobs: std::sync::mpsc::Receiver<Job>,
    reports: mpsc::UnboundedSender<Msg>,
    opened_at: Instant,
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic".to_string())
}

/// What the workers of one session share.
struct Shared {
    session: Arc<Session>,
    runner: Arc<dyn VolumeRunner>,
    jobs: Mutex<std::sync::mpsc::Receiver<Job>>,
    reports: mpsc::UnboundedSender<Msg>,
    /// Cancelled on abandon or on a fatal error: in-flight volumes stop.
    run_cancel: CancelToken,
    fatal: Mutex<Option<String>>,
    pages_flowed: AtomicBool,
}

impl RunnerCtx {
    fn run(self) {
        let RunnerCtx {
            session,
            pipeline,
            spec,
            jobs,
            reports,
            opened_at,
        } = self;
        let opened = std::panic::catch_unwind(AssertUnwindSafe(|| pipeline.open(&spec)))
            .unwrap_or_else(|p| Err(format!("loading the models panicked: {}", panic_text(&*p))));
        let runner: Arc<dyn VolumeRunner> = match opened {
            Ok(r) => Arc::from(r),
            Err(error) => {
                tracing::error!(
                    "session {}: the models would not load: {error}",
                    session.sid
                );
                // Nobody is blamed for a session that never became ready; the archives
                // already handed over are dropped without a word.
                session.stopping.cancel();
                session.close_intake();
                while jobs.try_recv().is_ok() {}
                let _ = reports.send(Msg::Fatal(error));
                let _ = reports.send(Msg::Exit(Some(1)));
                return;
            }
        };
        if session.abandon.is_cancelled() {
            session.close_intake();
            while jobs.try_recv().is_ok() {}
            let _ = reports.send(Msg::Exit(None));
            return;
        }
        let startup_seconds = (opened_at.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
        let _ = reports.send(Msg::Ready {
            info: runner.ready(),
            startup_seconds,
        });
        tracing::info!("session {}: ready in {startup_seconds:.1}s", session.sid);

        let shared = Arc::new(Shared {
            session: session.clone(),
            runner: runner.clone(),
            jobs: Mutex::new(jobs),
            reports: reports.clone(),
            run_cancel: session.abandon.child_token(),
            fatal: Mutex::new(None),
            pages_flowed: AtomicBool::new(false),
        });
        let (stop_stats, stats_stopped) = std::sync::mpsc::channel::<()>();
        let stats = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name(format!("ocr-stats-{}", session.sid))
                .spawn(move || stats_loop(&shared, &stats_stopped))
                .ok()
        };
        let width = runner.overlap().clamp(1, MAX_OUTSTANDING_VOLUMES);
        let extra: Vec<_> = (1..width)
            .filter_map(|i| {
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name(format!("ocr-volume-{}-{i}", session.sid))
                    .spawn(move || work(&shared))
                    .ok()
            })
            .collect();
        work(&shared);
        for handle in extra {
            let _ = handle.join();
        }
        drop(stop_stats);
        if let Some(handle) = stats {
            let _ = handle.join();
        }
        let fatal = shared.fatal.lock().take();
        // The session's models (and its stage threads) go before it says it ended:
        // whoever waits for the end -- the processor leaving, the server stopping before
        // the process exits -- finds them freed, not freed under the exit.
        drop(shared);
        drop(runner);
        if let Some(error) = fatal {
            let _ = reports.send(Msg::Fatal(error));
            let _ = reports.send(Msg::Exit(Some(1)));
        } else if session.abandon.is_cancelled() {
            let _ = reports.send(Msg::Exit(None));
        } else {
            tracing::info!("session {}: closed", session.sid);
            let _ = reports.send(Msg::Exit(Some(0)));
        }
    }
}

fn stats_loop(shared: &Shared, stop: &std::sync::mpsc::Receiver<()>) {
    let mut meter = HostMeter::default();
    let _ = meter.other_cpu();
    loop {
        match stop.recv_timeout(STATS_INTERVAL) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            _ => return,
        }
        if shared.pages_flowed.swap(false, Ordering::SeqCst) {
            let report = std::panic::catch_unwind(AssertUnwindSafe(|| shared.runner.stats()))
                .unwrap_or(None);
            let _ = shared.reports.send(Msg::Stats {
                report,
                cpu_pressure: cpu_pressure(),
                other_cpu: meter.other_cpu(),
            });
        }
    }
}

/// One worker: volumes off the session's queue until it ends.
fn work(shared: &Shared) {
    loop {
        let next = shared.jobs.lock().recv();
        let Ok(job) = next else { return };
        let claim = job.meta.claim.clone();
        let fed_at = Instant::now();
        let fatal = shared.fatal.lock().clone();
        let mut terminal = Terminal {
            seq: job.seq,
            claim: claim.clone(),
            sidecar_name: job.meta.sidecar_name.clone(),
            out: job.out.clone(),
            result: Err(RunError::Cancelled),
            started: false,
            fed_at,
            finished_at: fed_at,
            cpu_pressure: None,
            other_cpu: None,
        };
        if let Some(error) = fatal {
            // Accepted but never run: failed with the session's error, once.
            terminal.result = Err(RunError::Volume(error));
            let _ = shared.reports.send(Msg::Terminal(Box::new(terminal)));
            continue;
        }
        if shared.run_cancel.is_cancelled() {
            let _ = shared.reports.send(Msg::Terminal(Box::new(terminal)));
            continue;
        }
        let before = HostSample::now();
        let started = AtomicBool::new(false);
        let progress = |p: PageProgress| match p {
            PageProgress::Started { pages } => {
                if !started.swap(true, Ordering::SeqCst) {
                    let _ = shared.reports.send(Msg::Started {
                        claim: claim.clone(),
                        pages,
                    });
                }
            }
            PageProgress::Page { done, total } => {
                if !started.swap(true, Ordering::SeqCst) {
                    let _ = shared.reports.send(Msg::Started {
                        claim: claim.clone(),
                        pages: total,
                    });
                }
                shared.pages_flowed.store(true, Ordering::SeqCst);
                let _ = shared.reports.send(Msg::Page {
                    claim: claim.clone(),
                    done,
                    total,
                });
            }
        };
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            shared.runner.run_volume(
                job.archive.path(),
                &job.meta,
                &job.out,
                &progress,
                &shared.run_cancel,
            )
        }))
        .unwrap_or_else(|p| {
            Err(RunError::Fatal(format!(
                "the pipeline panicked: {}",
                panic_text(&*p)
            )))
        });
        // The archive goes back BEFORE the terminal event: the library's top-up that
        // event triggers must never find three archives held here.
        drop(job.archive);
        let result = match result {
            Err(RunError::Fatal(error)) => {
                tracing::error!(
                    "session {}: the pipeline failed: {error}",
                    shared.session.sid
                );
                let mut fatal = shared.fatal.lock();
                if fatal.is_none() {
                    *fatal = Some(error.clone());
                }
                drop(fatal);
                shared.session.stopping.cancel();
                shared.session.close_intake();
                shared.run_cancel.cancel();
                Err(RunError::Volume(error))
            }
            Err(RunError::Cancelled) if !shared.session.abandon.is_cancelled() => {
                match shared.fatal.lock().clone() {
                    Some(error) => Err(RunError::Volume(error)),
                    None => Err(RunError::Volume(
                        "the session ended before this volume did".to_string(),
                    )),
                }
            }
            other => other,
        };
        terminal.result = result;
        terminal.started = started.load(Ordering::SeqCst);
        terminal.finished_at = Instant::now();
        terminal.cpu_pressure = cpu_pressure();
        terminal.other_cpu = other_cpu_share(before, HostSample::now());
        let _ = shared.reports.send(Msg::Terminal(Box::new(terminal)));
    }
}

// --- the reporter -------------------------------------------------------------------------

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

async fn report(
    session: Arc<Session>,
    mut reports: mpsc::UnboundedReceiver<Msg>,
    link: Arc<Link>,
    sessions: Sessions,
) {
    let sid = session.sid.clone();
    let mut pending: BTreeMap<u64, Box<Terminal>> = BTreeMap::new();
    let mut next_seq = 0u64;
    let mut previous_end: Option<Instant> = None;
    let mut exited = false;
    while let Some(msg) = reports.recv().await {
        match msg {
            Msg::Ready {
                info,
                startup_seconds,
            } => session.say(Event::Ready {
                sid: sid.clone(),
                startup_seconds,
                weights: info.weights,
                stage_workers: info.stage_workers,
                queue_capacity: info.queue_capacity,
                stage_device: info.stage_device,
                pipeline: info.pipeline,
                precision: info.precision,
            }),
            Msg::FetchReady { claim, detail } => session.say(Event::Fetch {
                sid: sid.clone(),
                id: claim,
                state: "ready".to_string(),
                detail,
            }),
            Msg::Started { claim, pages } => session.say(Event::VolumeStarted {
                sid: sid.clone(),
                id: claim,
                pages,
            }),
            Msg::Page { claim, done, total } => session.say(Event::Page {
                sid: sid.clone(),
                id: claim,
                done,
                total,
            }),
            Msg::Stats {
                report,
                cpu_pressure,
                other_cpu,
            } => session.say(Event::Stats {
                sid: sid.clone(),
                pipeline: report.map(|r| r.to_value()).unwrap_or(Value::Null),
                cpu_pressure,
                other_cpu,
            }),
            Msg::Terminal(terminal) => {
                pending.insert(terminal.seq, terminal);
                while let Some(terminal) = pending.remove(&next_seq) {
                    next_seq += 1;
                    let end =
                        previous_end.map_or(terminal.finished_at, |p| p.max(terminal.finished_at));
                    let start = previous_end.map_or(terminal.fed_at, |p| p.max(terminal.fed_at));
                    previous_end = Some(end);
                    let seconds = round3(end.saturating_duration_since(start).as_secs_f64());
                    finish_volume(&session, &link, *terminal, seconds).await;
                }
            }
            Msg::Fatal(error) => session.say(Event::Fatal {
                sid: sid.clone(),
                error,
            }),
            Msg::SpawnFailed(error) => session.say(Event::SpawnFailed {
                sid: sid.clone(),
                error,
            }),
            Msg::Exit(returncode) => {
                exited = true;
                end_session(&session, &link, &sessions, returncode).await;
                break;
            }
        }
    }
    if !exited {
        // The runner thread died without its exit; the session still ends with one.
        end_session(&session, &link, &sessions, Some(1)).await;
    }
    session.finished.cancel();
}

/// Tidy up, then say the session's one `exit` (last, so whoever hears it finds the
/// session gone and its workspace removed).
async fn end_session(session: &Session, link: &Link, sessions: &Sessions, returncode: Option<i32>) {
    session.close_intake();
    session.stopping.cancel();
    sessions.lock().remove(&session.sid);
    link.finish_session(&session.sid).await;
    session.say(Event::Exit {
        sid: session.sid.clone(),
        returncode,
    });
}

async fn finish_volume(session: &Session, link: &Link, terminal: Terminal, seconds: f64) {
    let sid = session.sid.clone();
    let claim = terminal.claim.clone();
    let note = session.book.lock().damaged.get(&claim).cloned();
    // The slot is free before the terminal event goes: the library may answer it
    // with the next volume at once.
    session.settle(&claim);
    if !session.talking() {
        link.discard(&sid, &claim, false).await;
        return;
    }
    let pages_started = match &terminal.result {
        Ok(outcome) => outcome.pages,
        Err(_) => 0,
    };
    if !terminal.started {
        session.say(Event::VolumeStarted {
            sid: sid.clone(),
            id: claim.clone(),
            pages: pages_started,
        });
    }
    let mut delivered = false;
    let event = match terminal.result {
        Ok(outcome) => match tokio::select! {
            delivered = link.deliver(&sid, &claim, &terminal.out, &terminal.sidecar_name) => delivered,
            _ = session.abandon.cancelled() => Err("the session was abandoned".to_string()),
        } {
            Ok(sha) => {
                delivered = true;
                Event::VolumeDone {
                    sid: sid.clone(),
                    id: claim.clone(),
                    pages: outcome.pages,
                    failed_pages: outcome.failed_pages,
                    seconds,
                    stats: outcome.stats.map(|s| s.to_value()).unwrap_or(Value::Null),
                    cpu_pressure: terminal.cpu_pressure,
                    other_cpu: terminal.other_cpu,
                    sidecar_sha256: Some(sha),
                }
            }
            Err(error) => {
                tracing::error!("volume {claim}: the sidecar could not be sent: {error}");
                Event::VolumeFailed {
                    sid: sid.clone(),
                    id: claim.clone(),
                    error: SIDECAR_NOT_SENT.to_string(),
                }
            }
        },
        Err(RunError::Volume(error)) | Err(RunError::Fatal(error)) => Event::VolumeFailed {
            sid: sid.clone(),
            id: claim.clone(),
            error: format!("{error}{}", note.unwrap_or_default()),
        },
        Err(RunError::Cancelled) => Event::VolumeFailed {
            sid: sid.clone(),
            id: claim.clone(),
            error: "the session ended before this volume did".to_string(),
        },
    };
    session.say(event);
    link.discard(&sid, &claim, delivered).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(archive: &str, sidecar: &str, title: &str) -> VolumeOp {
        VolumeOp {
            sid: "s".into(),
            claim: "v1".into(),
            archive: archive.into(),
            sidecar_name: sidecar.into(),
            title: "T".into(),
            volume_title: title.into(),
            title_uuid: None,
            volume_uuid: None,
            size: None,
            etag: None,
        }
    }

    #[test]
    fn wire_strings_become_basenames() {
        assert_eq!(
            sidecar_name(&op("/a/b.cbz", "../../evil.mokuro", "B")),
            "evil.mokuro"
        );
        assert_eq!(sidecar_name(&op("/a/b.cbz", "", "Vol 1")), "Vol 1.mokuro");
        assert_eq!(sidecar_name(&op("/a/b.cbz", "..", "B")), "volume.mokuro");
        assert_eq!(archive_stem("/mokuro-reader/S/Vol 01.cbz", "x"), "Vol 01");
        assert_eq!(archive_stem("", "dir/Vol"), "Vol");
    }
}
