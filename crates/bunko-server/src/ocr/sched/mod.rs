//! The scheduler: the single owner of every piece of OCR scheduling state (0.5.2
//! `OCRWorker`'s ~40 lock-guarded fields), driven one message at a time.
//!
//! [`Scheduler::handle`] is synchronous and deterministic given its clock; the actor
//! (`actor.rs`) runs it on a dedicated OS thread and feeds it messages and a 1 s tick.
//! Blocking side work that would stall it (the library walk, installing a sidecar,
//! reading the library's own copy of an archive) runs on helper threads and comes back
//! as a message; tests run that work inline ([`Exec::Inline`]) so a whole round trip is
//! reproducible under a [`bunko_sched::rate::ManualClock`].
//!
//! Lanes hold no state of their own beyond "which session is open on me": every claim,
//! drain, strike, backoff, collection and cancellation is decided here.

mod bench;
mod claim;
mod registry;
mod session;
mod settle;
mod status;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use bunko_core::StorageLayout;
use bunko_core::config::UpgradeConfig;
use bunko_core::generations::Generation;
use bunko_db::Database;
use bunko_proto::{Catalog, Event, HostInfo, Op};
use bunko_sched::breaker::{DownloadBreaker, JobReturns, StartBackoff};
use bunko_sched::congestion::CongestionHistory;
use bunko_sched::eft::EftDeadlines;
use bunko_sched::failures::FailureStore;
use bunko_sched::rate::{Clock, FilePriors, RateModel};
use indexmap::IndexMap;
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot};
use tracing::info;

use super::owed::OwedIndex;
use super::profiles::Profiles;
use super::types::{Job, LibraryFacts, PathLocks};

pub use bench::{BenchRequest, BenchState, cpu_label, physical_cores};
pub use claim::{catalog_can_run, supported_for};
pub use registry::{
    FailedLogin, Machine, PublicNames, RegisterInput, RegisterOutcome, SocketRefusal, TransferStats,
};
pub use status::RawStatus;

/// Claims a session holds at once (`SESSION_LOOKAHEAD`).
pub const SESSION_LOOKAHEAD: usize = bunko_proto::MAX_OUTSTANDING_VOLUMES;
/// No event at all from a session for this long: wedged.
pub const SESSION_WEDGE_SECONDS: f64 = bunko_proto::SESSION_WEDGE_SECONDS as f64;
/// Consecutive sessions dying with nothing finished: the row stops on that machine.
pub const SESSION_CRASH_LIMIT: u32 = 2;
/// A processor silent this long is gone (wedge judgement).
pub const SILENCE_SECONDS: f64 = bunko_proto::SILENCE_SECONDS as f64;
/// A closing session that has not said `exit` after this long is killed.
pub const CLOSE_GRACE_SECONDS: f64 = 30.0;
/// `PRECISION_REFUSAL`: a runner that refused the row's forced precision.
pub const PRECISION_REFUSAL: &str = "precision not available here";
/// The release reason of a result whose archive changed under it.
pub const DISCARDED: &str =
    "the archive was replaced or deleted while it was being read, so the result was discarded";

/// Live settings (`apply_settings`).
#[derive(Clone, Debug)]
pub struct Settings {
    pub rows: Vec<Generation>,
    pub poll_interval: f64,
    /// This process runs OCR of its own (full build, `ocr.local_processing`).
    pub local_processing: bool,
    pub concurrency: u32,
    pub autobench: bool,
    pub upgrade: UpgradeConfig,
}

/// Where helper-thread work runs.
#[derive(Clone)]
pub enum Exec {
    /// Run it now and queue its answer (tests).
    Inline,
    /// A helper thread per job; the answer goes back on the actor's channel.
    Threads(std::sync::mpsc::Sender<Msg>),
}

/// Everything the scheduler is handed at construction.
#[derive(Clone)]
pub struct SchedDeps {
    pub layout: StorageLayout,
    pub db: Option<Arc<Database>>,
    pub facts: Arc<dyn LibraryFacts>,
    pub locks: PathLocks,
    pub clock: Arc<dyn Clock>,
    pub exec: Exec,
    /// Generation upgrades (census, swap), when configured.
    pub upgrade: Option<Arc<super::upgrade::Upgrade>>,
    /// `mokuro-bunko <version>`.
    pub generator: String,
    pub version: String,
}

/// A reply a query sends back.
pub type Reply<T> = oneshot::Sender<T>;

/// A closure run on the scheduler with full access (status reads, admin calls).
pub type Query = Box<dyn FnOnce(&mut Scheduler) + Send>;

/// The scheduler's inbox.
pub enum Msg {
    /// The 1 s timer.
    Tick,
    /// A full library walk finished (`epoch` guards against a settings change during it).
    Walked {
        epoch: u64,
        index: OwedIndex,
    },
    /// A frame from a processor (remote socket or local channel).
    Event {
        pid: String,
        event: Event,
    },
    /// Any frame at all from a remote processor (liveness).
    Seen {
        pid: String,
    },
    /// A remote processor's registration.
    Register {
        input: RegisterInput,
        reply: Reply<RegisterOutcome>,
    },
    /// A remote processor opened its socket.
    SocketOpen {
        pid: String,
        username: String,
        ops: mpsc::UnboundedSender<Op>,
        reply: Reply<Result<(), SocketRefusal>>,
    },
    /// The in-process processor is up.
    LocalUp {
        ops: mpsc::UnboundedSender<Op>,
        catalog: Catalog,
        host: HostInfo,
    },
    /// A processor is gone (socket closed, silent, account revoked, ...): `registry.drop`.
    Drop {
        pid: String,
        reason: String,
    },
    /// Drop every processor of an account (disabled, deleted, re-roled).
    DropAccount {
        username: String,
        reason: String,
    },
    /// A login on a processor path was refused.
    FailedLogin {
        username: String,
        reason: String,
    },
    /// A result upload landed: `<storage>/.processing/<sid>/<claim>/<name>`.
    ResultStored {
        pid: String,
        sid: String,
        claim: String,
        name: String,
        sha256: String,
    },
    /// A sidecar installation finished on a helper thread.
    Collected(Box<super::collect::CollectDone>),
    /// `read_own_copy` finished for a returned claim.
    OwnCopyRead {
        job: Job,
        error: Option<String>,
        then: Box<settle::ReturnContext>,
    },
    ArchiveArrived(PathBuf),
    ArchiveRemoved(PathBuf),
    /// A benchmark's sample was packed on a helper thread: `(pages, volumes)`.
    BenchSample {
        machine: String,
        bid: String,
        result: Result<(i64, i64), String>,
    },
    Apply {
        settings: Box<Settings>,
        reply: Option<Reply<()>>,
    },
    Query(Query),
    Stop,
}

impl std::fmt::Debug for Msg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Msg::Tick => "Tick",
            Msg::Walked { .. } => "Walked",
            Msg::Event { .. } => "Event",
            Msg::Seen { .. } => "Seen",
            Msg::Register { .. } => "Register",
            Msg::SocketOpen { .. } => "SocketOpen",
            Msg::LocalUp { .. } => "LocalUp",
            Msg::Drop { .. } => "Drop",
            Msg::DropAccount { .. } => "DropAccount",
            Msg::FailedLogin { .. } => "FailedLogin",
            Msg::ResultStored { .. } => "ResultStored",
            Msg::Collected(_) => "Collected",
            Msg::OwnCopyRead { .. } => "OwnCopyRead",
            Msg::ArchiveArrived(_) => "ArchiveArrived",
            Msg::ArchiveRemoved(_) => "ArchiveRemoved",
            Msg::BenchSample { .. } => "BenchSample",
            Msg::Apply { .. } => "Apply",
            Msg::Query(_) => "Query",
            Msg::Stop => "Stop",
        };
        f.write_str(name)
    }
}

/// One slot that may run a session.
#[derive(Clone, Debug)]
pub struct Lane {
    /// Stable for the life of the process; the cards' `slot`.
    pub id: u64,
    pub pid: String,
    pub session: Option<String>,
    /// The EFT walk left volumes to faster lanes: stay in the scan, retry soon.
    pub waiting_for_faster: bool,
    /// The queue generation at which this lane last found nothing to claim.
    pub idle_at: Option<u64>,
}

/// A job some lane holds.
#[derive(Clone, Debug)]
pub struct Claimed {
    pub lane: u64,
    pub pid: String,
    pub machine: String,
    pub sid: Option<String>,
    pub claim: Option<String>,
    /// `(size, mtime_ns)` at claim / submit: the file the result must match.
    pub stamp: Option<super::types::Stamp>,
    /// Its result is being installed: a disconnect must not take it back.
    pub settling: bool,
    pub row: Generation,
}

/// Autobench bookkeeping (§14.2).
#[derive(Default, Debug)]
pub struct AutobenchState {
    pub asked: HashSet<(String, String)>,
    pub failed: HashSet<(String, String)>,
    pub inflight: HashSet<(String, String)>,
    pub wanted: Vec<(String, String)>,
}

/// The cached pending list and the queue generation it was computed at.
pub type PendingCache = Option<(u64, Arc<Vec<Map<String, Value>>>)>;

pub struct Scheduler {
    pub deps: SchedDeps,
    pub settings: Settings,
    pub rates: Arc<RateModel>,
    pub profiles: Profiles,
    pub congestion: CongestionHistory,
    pub failure_store: FailureStore,
    pub failures: Map<String, Value>,
    pub owed: OwedIndex,
    pub walk_epoch: u64,
    pub walking: bool,
    pub last_walk_at: Option<f64>,
    pub machines: IndexMap<String, Machine>,
    pub lanes: Vec<Lane>,
    pub next_lane_id: u64,
    pub sessions: HashMap<String, session::Session>,
    pub claims: HashMap<Job, Claimed>,
    pub attempted: HashSet<Job>,
    pub cancelled: HashSet<Job>,
    pub last_served: HashMap<String, String>,
    pub holds: HashMap<String, u32>,
    pub stopped: HashSet<(String, String)>,
    pub strikes: HashMap<(String, String), u32>,
    pub start_backoff: HashMap<(String, String), StartBackoff>,
    pub breakers: HashMap<String, DownloadBreaker>,
    pub download_returns: HashMap<Job, JobReturns>,
    pub returned_by: HashMap<Job, HashSet<String>>,
    pub eft_deadlines: EftDeadlines<Job>,
    pub left_logged: HashSet<(Job, String)>,
    pub cards: IndexMap<Job, Map<String, Value>>,
    /// Invalidates the cached pending list (`_queue_generation`).
    pub queue_generation: u64,
    /// The queue page's state version (`QueueStateVersion`).
    pub page_version: u64,
    pub scan_active: bool,
    pub started_at: f64,
    pub last_disconnect: Option<(String, f64)>,
    pub failed_logins: VecDeque<FailedLogin>,
    pub public_names: PublicNames,
    pub claim_seq: u64,
    pub autobench: AutobenchState,
    pub bench: BenchState,
    pub internal: VecDeque<Msg>,
    pub stopping: bool,
    pub pending_cache: PendingCache,
    /// Uploaded results waiting for their `volume_done`: `(sid, claim)` → (path, sha256).
    pub results: HashMap<(String, String), (PathBuf, String)>,
    /// Ended sessions per processor, for late events: sid → when.
    pub ended_sessions: HashMap<String, f64>,
    pub hold_logged: bool,
    pub last_poll_tick: f64,
    /// When the per-scan state (attempted, strikes, stopped rows...) was last reset.
    pub scan_reset_at: f64,
}

impl Scheduler {
    pub fn new(deps: SchedDeps, settings: Settings) -> Scheduler {
        let storage = deps.layout.base.clone();
        let priors = Arc::new(FilePriors::new(
            Some(&storage),
            bunko_sched::rate::CACHE_TTL_SECONDS,
            deps.clock.clone(),
        ));
        let rates = Arc::new(RateModel::new(
            deps.clock.clone(),
            priors,
            bunko_sched::rate::SESSION_ALPHA,
        ));
        let failure_store = FailureStore::new(&storage);
        let failures = failure_store.load();
        let started_at = deps.clock.time();
        Scheduler {
            profiles: Profiles::new(&storage),
            congestion: CongestionHistory::new(&storage),
            failure_store,
            failures,
            rates,
            settings,
            owed: OwedIndex::default(),
            walk_epoch: 0,
            walking: false,
            last_walk_at: None,
            machines: IndexMap::new(),
            lanes: Vec::new(),
            next_lane_id: 0,
            sessions: HashMap::new(),
            claims: HashMap::new(),
            attempted: HashSet::new(),
            cancelled: HashSet::new(),
            last_served: HashMap::new(),
            holds: HashMap::new(),
            stopped: HashSet::new(),
            strikes: HashMap::new(),
            start_backoff: HashMap::new(),
            breakers: HashMap::new(),
            download_returns: HashMap::new(),
            returned_by: HashMap::new(),
            eft_deadlines: EftDeadlines::new(),
            left_logged: HashSet::new(),
            cards: IndexMap::new(),
            queue_generation: 0,
            page_version: 1,
            scan_active: false,
            started_at,
            last_disconnect: None,
            failed_logins: VecDeque::new(),
            public_names: PublicNames::default(),
            claim_seq: 0,
            autobench: AutobenchState::default(),
            bench: BenchState::default(),
            internal: VecDeque::new(),
            stopping: false,
            pending_cache: None,
            results: HashMap::new(),
            ended_sessions: HashMap::new(),
            hold_logged: false,
            last_poll_tick: f64::NEG_INFINITY,
            scan_reset_at: f64::NEG_INFINITY,
            deps,
        }
    }

    // --- clocks and small helpers ----------------------------------------------------------

    pub fn now(&self) -> f64 {
        self.deps.clock.time()
    }

    pub fn mono(&self) -> f64 {
        self.deps.clock.monotonic()
    }

    pub fn library(&self) -> PathBuf {
        self.deps.layout.library()
    }

    pub fn storage(&self) -> PathBuf {
        self.deps.layout.base.clone()
    }

    pub fn rates(&self) -> &Arc<RateModel> {
        &self.rates
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The live, enabled row with this id.
    pub fn row(&self, id: &str) -> Option<&Generation> {
        bunko_core::generations::enabled_generations(&self.settings.rows).find(|r| r.id == id)
    }

    pub fn enabled_rows(&self) -> Vec<Generation> {
        bunko_core::generations::enabled_generations(&self.settings.rows)
            .cloned()
            .collect()
    }

    pub fn primary(&self) -> Option<&Generation> {
        bunko_core::generations::primary_generation(&self.settings.rows)
    }

    /// `_generation_rank`: row id → position among the enabled rows.
    pub fn rank(&self) -> HashMap<String, usize> {
        self.enabled_rows()
            .iter()
            .enumerate()
            .map(|(i, r)| (r.id.clone(), i))
            .collect()
    }

    pub fn bump(&mut self) {
        self.queue_generation += 1;
        self.pending_cache = None;
    }

    pub fn bump_page(&mut self) {
        self.page_version += 1;
    }

    pub fn page_version(&self) -> u64 {
        self.page_version
    }

    pub fn log(&self, line: impl AsRef<str>) {
        info!("{}", line.as_ref());
    }

    pub fn run_background(&mut self, job: Box<dyn FnOnce() -> Msg + Send>) {
        match &self.deps.exec {
            Exec::Inline => {
                let msg = job();
                self.internal.push_back(msg);
            }
            Exec::Threads(tx) => {
                let tx = tx.clone();
                let spawned =
                    std::thread::Builder::new()
                        .name("ocr-helper".into())
                        .spawn(move || {
                            let msg = job();
                            let _ = tx.send(msg);
                        });
                if let Err(e) = spawned {
                    tracing::error!("could not start an OCR helper thread: {e}");
                }
            }
        }
    }

    // --- the message loop ----------------------------------------------------------------

    /// Process one message, everything it caused, then fill whatever lanes can work.
    pub fn handle(&mut self, msg: Msg) {
        self.dispatch(msg);
        loop {
            while let Some(next) = self.internal.pop_front() {
                self.dispatch(next);
            }
            self.pump();
            if self.internal.is_empty() {
                break;
            }
        }
    }

    fn dispatch(&mut self, msg: Msg) {
        match msg {
            Msg::Tick => self.tick(),
            Msg::Walked { epoch, index } => self.walked(epoch, index),
            Msg::Event { pid, event } => self.on_event(&pid, event),
            Msg::Seen { pid } => self.seen(&pid),
            Msg::Register { input, reply } => {
                let outcome = self.register(input);
                let _ = reply.send(outcome);
            }
            Msg::SocketOpen {
                pid,
                username,
                ops,
                reply,
            } => {
                let outcome = self.socket_open(&pid, &username, ops);
                let _ = reply.send(outcome);
            }
            Msg::LocalUp { ops, catalog, host } => self.local_up(ops, catalog, host),
            Msg::Drop { pid, reason } => self.drop_processor(&pid, &reason),
            Msg::DropAccount { username, reason } => self.drop_account(&username, &reason),
            Msg::FailedLogin { username, reason } => self.record_failed_login(&username, &reason),
            Msg::ResultStored {
                pid,
                sid,
                claim,
                name,
                sha256,
            } => self.result_stored(&pid, &sid, &claim, &name, sha256),
            Msg::Collected(done) => self.collected(*done),
            Msg::OwnCopyRead { job, error, then } => self.own_copy_read(job, error, *then),
            Msg::ArchiveArrived(path) => self.archive_arrived(&path),
            Msg::ArchiveRemoved(path) => self.archive_removed(&path),
            Msg::BenchSample {
                machine,
                bid,
                result,
            } => self.bench_sample_built(&machine, &bid, result),
            Msg::Apply { settings, reply } => {
                self.apply_settings(*settings);
                if let Some(r) = reply {
                    let _ = r.send(());
                }
            }
            Msg::Query(f) => f(self),
            Msg::Stop => self.stop(),
        }
    }

    // --- scans --------------------------------------------------------------------------

    /// The 1 s timer: wedges, closing sessions, throttled top-ups, EFT waits, the poll.
    pub fn tick(&mut self) {
        if self.stopping {
            return;
        }
        self.check_sessions();
        let now = self.mono();
        if now - self.last_poll_tick >= self.settings.poll_interval {
            self.last_poll_tick = now;
            self.start_walk();
            self.bench_tick();
        }
        // A waiting lane retries every tick: its left volume's deadline may have passed.
        for lane in &mut self.lanes {
            if lane.waiting_for_faster {
                lane.idle_at = None;
            }
        }
    }

    /// Start a full walk on a helper thread (single-flight).
    pub fn start_walk(&mut self) {
        if self.walking {
            return;
        }
        self.walking = true;
        let epoch = self.walk_epoch;
        let library = self.library();
        let rows = self.settings.rows.clone();
        let facts = self.deps.facts.clone();
        let probe = self.upgrade_probe();
        let known: super::owed::KnownPages = self
            .owed
            .volumes
            .iter()
            .map(|(rel, v)| (rel.clone(), (v.size, v.mtime_ns, v.pages)))
            .collect();
        self.run_background(Box::new(move || {
            let index = super::owed::walk(
                &library,
                &rows,
                facts.as_ref(),
                probe
                    .as_deref()
                    .map(|p| p as &dyn super::owed::UpgradeProbe),
                &known,
            );
            Msg::Walked { epoch, index }
        }));
    }

    fn walked(&mut self, epoch: u64, index: OwedIndex) {
        self.walking = false;
        if epoch != self.walk_epoch {
            // Settings moved during the walk: walk again with the new rows.
            self.start_walk();
            return;
        }
        self.owed = index;
        self.last_walk_at = Some(self.mono());
        self.bump();
        self.bump_page();
        self.maybe_start_scan();
    }

    /// The OCR loop's "scan": unless every machine is held or nothing can run.
    pub fn maybe_start_scan(&mut self) {
        if self.scan_active || self.stopping {
            return;
        }
        if self.processing_hold().is_some() {
            if !self.hold_logged {
                self.hold_logged = true;
                self.log("OCR is waiting for hardware: local processing is off and no processor is connected");
            }
            return;
        }
        self.hold_logged = false;
        if self.every_machine_held() {
            return;
        }
        self.start_scan();
    }

    /// A new scan. Its per-scan state (rows stopped on a machine, strikes, autobench
    /// asks) is reset once per poll interval: a processor connecting or an upload
    /// arriving between polls starts work at once without forgiving a row that just
    /// struck out twice (0.5.2 reset it only on its poll-paced scans).
    fn start_scan(&mut self) {
        self.prune_failures();
        self.attempted.clear();
        self.returned_by.clear();
        let now = self.mono();
        if now - self.scan_reset_at >= self.settings.poll_interval {
            self.scan_reset_at = now;
            self.left_logged.clear();
            self.stopped.clear();
            self.strikes.clear();
            self.autobench.asked.clear();
        }
        self.bump();
        let jobs = self.candidates(&HashSet::new(), true);
        if !jobs.is_empty() {
            let volumes: HashSet<&str> = jobs.iter().map(|j| &*j.rel).collect();
            let order: Vec<String> = self.enabled_rows().iter().map(|r| r.name.clone()).collect();
            self.log(format!(
                "Found {} missing OCR sidecar(s) across {} CBZ file(s) (generations, in run order: {}; within a generation series take turns)",
                jobs.len(),
                volumes.len(),
                order.join(", ")
            ));
        }
        self.scan_active = true;
        for lane in &mut self.lanes {
            lane.idle_at = None;
        }
    }

    /// The scan is over when nothing is in flight, no lane waits for a faster one, no
    /// autobench is pending, no machine is held, and every lane found nothing.
    fn maybe_end_scan(&mut self) {
        if !self.scan_active {
            return;
        }
        if !self.claims.is_empty() || !self.autobench.inflight.is_empty() {
            return;
        }
        if self.holds.values().any(|n| *n > 0) {
            return;
        }
        let g = self.queue_generation;
        if self
            .lanes
            .iter()
            .any(|l| l.waiting_for_faster || l.session.is_some() || l.idle_at != Some(g))
        {
            return;
        }
        self.scan_active = false;
        self.attempted.clear();
        self.returned_by.clear();
        self.bump();
    }

    /// Fill every lane that can work, top up open sessions.
    pub fn pump(&mut self) {
        if self.stopping {
            return;
        }
        self.drain_autobench_requests();
        let lane_ids: Vec<u64> = self.lanes.iter().map(|l| l.id).collect();
        for id in lane_ids {
            let Some(lane) = self.lanes.iter().find(|l| l.id == id).cloned() else {
                continue;
            };
            if let Some(sid) = &lane.session {
                self.top_up(sid.clone());
                continue;
            }
            if !self.scan_active || lane.idle_at == Some(self.queue_generation) {
                continue;
            }
            let (job, _) = self.claim(id, None);
            match job {
                Some(job) => {
                    self.open_session(id, job);
                    // Fill the lookahead at once, as the session loop does.
                    if let Some(sid) = self
                        .lanes
                        .iter()
                        .find(|l| l.id == id)
                        .and_then(|l| l.session.clone())
                    {
                        self.top_up(sid);
                    }
                }
                None => {
                    let g = self.queue_generation;
                    if let Some(l) = self.lanes.iter_mut().find(|l| l.id == id) {
                        l.idle_at = Some(g);
                    }
                }
            }
        }
        self.drain_autobench_requests();
        self.maybe_end_scan();
    }

    fn stop(&mut self) {
        self.stopping = true;
        // Everything in flight goes back untouched: a shutdown is nobody's failure.
        let sids: Vec<String> = self.sessions.keys().cloned().collect();
        for sid in sids {
            self.kill_session(&sid, None);
        }
        let pids: Vec<String> = self.machines.keys().cloned().collect();
        for pid in pids {
            self.drop_processor(&pid, "the server is stopping");
        }
    }

    /// Restart-free reconfiguration (`OCRWorker.apply_settings`).
    pub fn apply_settings(&mut self, settings: Settings) {
        let local_changed = self.settings.local_processing != settings.local_processing;
        self.settings = settings;
        self.autobench.asked.clear();
        self.autobench.failed.clear();
        self.bump();
        self.bump_page();
        // Cancel what a removed / disabled / recipe-changed row is running.
        let mut to_kill: Vec<(String, String)> = Vec::new();
        for (sid, s) in &self.sessions {
            if let Some(reason) = cancel_reason(&s.row, &self.settings.rows) {
                to_kill.push((sid.clone(), reason));
            }
        }
        for (sid, reason) in to_kill {
            let jobs: Vec<Job> = self
                .sessions
                .get(&sid)
                .map(|s| s.jobs.values().map(|j| j.job.clone()).collect())
                .unwrap_or_default();
            for j in jobs {
                self.cancelled.insert(j);
            }
            if let Some(s) = self.sessions.get(&sid) {
                self.log(format!("Stopping the {} session: {reason}", s.row.name));
            }
            self.kill_session(&sid, None);
        }
        let ids: Vec<String> = self.settings.rows.iter().map(|r| r.id.clone()).collect();
        if let Err(e) = self.congestion.prune(&ids) {
            tracing::warn!("could not prune the congestion history: {e}");
        }
        self.walk_epoch += 1;
        self.prune_failures();
        self.start_walk();
        let order: Vec<String> = self.enabled_rows().iter().map(|r| r.name.clone()).collect();
        self.log(format!(
            "OCR settings applied (generations, in run order: {})",
            order.join(", ")
        ));
        if local_changed {
            self.rebuild_lanes();
        }
    }

    /// `_prune_failure_records`.
    pub fn prune_failures(&mut self) {
        let names: Vec<String> = self.settings.rows.iter().map(|r| r.name.clone()).collect();
        let library = self.library();
        let before = self.failures.len();
        let kept: Map<String, Value> = std::mem::take(&mut self.failures)
            .into_iter()
            .filter(|(_, v)| {
                v.as_object().is_some_and(|r| {
                    bunko_sched::failures::keep_record(r, &names, |s, v| {
                        bunko_sched::failures::archive_exists_in(&library, s, v)
                    })
                })
            })
            .collect();
        self.failures = kept;
        if self.failures.len() != before {
            self.save_failures();
        }
    }

    pub fn save_failures(&mut self) {
        if let Err(e) = self.failure_store.save(&self.failures) {
            tracing::error!("could not save the OCR failure records: {e}");
        }
        self.bump_page();
    }
}

/// `_cancel_reason`: why a running row must stop after a settings change.
pub fn cancel_reason(running: &Generation, rows: &[Generation]) -> Option<String> {
    match rows.iter().find(|r| r.id == running.id) {
        None => Some("the generation was removed from settings".into()),
        Some(r) if !r.runnable() => Some("the generation was disabled".into()),
        Some(r) if r.output_affecting() != running.output_affecting() => {
            Some("its engine, detector or patch budget changed".into())
        }
        _ => None,
    }
}
