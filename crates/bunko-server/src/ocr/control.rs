//! [`OcrControl`]: the public handle of the OCR orchestrator, its dependencies and the
//! in-process processor hook.
//!
//! # Wiring (for the orchestrator)
//!
//! ```text
//! let ocr = OcrControl::new(OcrDeps { core, db, facts, locks, local, clock: None });
//! ocr.start(stop.child_token());                       // the scheduler thread (+ local)
//! app modules:  ocr::processor_router(ocr.clone())     // /_processor/*
//!               ocr::queue_router(ocr.clone())         // /queue, /queue/api/*
//! layer:        from_fn_with_state(Some(ocr), ocr::queue_file::middleware)
//!               (before the DAV fallback: /mokuro-reader/.mokuro-queue.json)
//! DavHooks:     server_dav_hooks.add_listener(Arc::new(ocr.clone()))   (arrivals,
//!               removals, the PUT follow-up headers)
//! admin:        AdminDeps.ocr = Arc::new(ocr.clone()); drop_processors → drop_account
//! accounts:     health = Some(Arc::new(ocr.clone()))
//! shutdown:     ocr.stop().await   (after the HTTP server stops)
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bunko_core::Config;
use bunko_db::Database;
use bunko_proto::{Catalog, Event, HostInfo, Op};
use bunko_sched::rate::{Clock, SystemClock};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::sched::{self, Exec, Msg, SchedDeps, Scheduler, Settings};
use super::types::{self, LibraryFacts, PathLocks};
use super::{actor, api, queue_api, queue_file, upgrade};
use crate::core::Core;

pub use super::sched::BenchRequest;
pub use super::types::{DavLocks, FileFacts, Job};

/// What the in-process processor hands back when started (full build).
pub struct LocalChannels {
    /// Ops in (bunko-processor's `LocalLink.ops`).
    pub ops: mpsc::Sender<Op>,
    /// Events out (bunko-processor's `LocalLink.events`).
    pub events: mpsc::UnboundedReceiver<Event>,
    pub catalog: Catalog,
    pub host: HostInfo,
    /// Ends once the processor has left (its op sender dropped) and its sessions wound
    /// down, their models freed (bunko-processor's `LocalLink::take_finished`). The
    /// server's stop awaits it, so no OCR thread is still running, and no model still
    /// held, when the process exits.
    pub finished: Option<tokio::task::JoinHandle<()>>,
}

/// How long the server's stop waits for its local processor to wind down (the
/// processor itself gives its sessions 10 s; a stage finishing its page can add to it).
const LOCAL_STOP_WAIT: Duration = Duration::from_secs(15);

/// The binary's hook that starts `bunko_processor::LocalProcessor` over the real engines.
/// The lite build passes none: `ocr.local_processing` is then off.
pub trait LocalProcessorFactory: Send + Sync {
    /// Start it; sidecars land in `<results_dir>/<sid>/<claim>/<name>`.
    fn start(&self, results_dir: &Path) -> Result<LocalChannels, String>;
}

/// What the OCR module is handed.
pub struct OcrDeps {
    pub core: Core,
    pub db: Option<Arc<Database>>,
    /// Page counts / missing pages / recompile hook (bunko-library over the database).
    pub facts: Arc<dyn LibraryFacts>,
    /// The DAV write locks (`Arc::new(DavLocks(locks))`).
    pub locks: PathLocks,
    pub local: Option<Arc<dyn LocalProcessorFactory>>,
    /// None: the system clock.
    pub clock: Option<Arc<dyn Clock>>,
}

struct Inner {
    core: Core,
    db: Option<Arc<Database>>,
    tx: std::sync::mpsc::Sender<Msg>,
    pending: Mutex<Option<(Scheduler, std::sync::mpsc::Receiver<Msg>)>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    local: Option<Arc<dyn LocalProcessorFactory>>,
    /// The running local processor's end ([`LocalChannels::finished`]).
    local_finished: Mutex<Option<tokio::task::JoinHandle<()>>>,
    upgrade: Arc<upgrade::Upgrade>,
    queue_file: queue_file::QueueFile,
    status: queue_api::StatusCache,
    storage: PathBuf,
}

/// The public handle of the OCR orchestrator (cheap to clone).
#[derive(Clone)]
pub struct OcrControl(Arc<Inner>);

/// `generator`: `mokuro-bunko <version>`.
pub fn generator() -> String {
    format!("mokuro-bunko {}", bunko_core::VERSION)
}

/// The live settings out of the config.
pub fn settings_from_config(config: &Config, local_available: bool) -> Settings {
    Settings {
        rows: config.ocr.generations.clone(),
        poll_interval: f64::from(config.ocr.poll_interval.max(1)),
        local_processing: config.processes_locally(local_available),
        concurrency: config.ocr.concurrency.max(1),
        autobench: config.ocr.autobench,
        upgrade: config.ocr.upgrade.clone(),
    }
}

/// `urllib.parse.quote(value, safe="!*'()")` (UTF-8).
pub fn quote_component(value: &str) -> String {
    let mut out = String::new();
    for b in value.as_bytes() {
        let c = *b as char;
        if b.is_ascii_alphanumeric() || "_.-~!*'()".contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `/catalog/api/manifest?series=<s>&volume=<v>`.
pub fn manifest_url(series: &str, volume: &str) -> String {
    format!(
        "/catalog/api/manifest?series={}&volume={}",
        quote_component(series),
        quote_component(volume)
    )
}

/// `/mokuro-reader/<series>/<file>`.
pub fn reader_file_url(series: &str, file: &str) -> String {
    format!(
        "/mokuro-reader/{}/{}",
        quote_component(series),
        quote_component(file)
    )
}

impl OcrControl {
    pub fn new(deps: OcrDeps) -> OcrControl {
        let (tx, rx) = actor::channel();
        let config = deps.core.config.read().clone();
        let settings = settings_from_config(&config, deps.local.is_some());
        let clock: Arc<dyn Clock> = deps
            .clock
            .clone()
            .unwrap_or_else(|| Arc::new(SystemClock::default()));
        let layout = deps.core.layout.clone();
        let up = Arc::new(upgrade::Upgrade::new(
            layout.library(),
            deps.db.clone(),
            deps.facts.clone(),
            deps.locks.clone(),
            generator(),
        ));
        up.configure(&settings.upgrade, &settings.rows);
        let sched = Scheduler::new(
            SchedDeps {
                layout: layout.clone(),
                db: deps.db.clone(),
                facts: deps.facts.clone(),
                locks: deps.locks.clone(),
                clock,
                exec: Exec::Threads(tx.clone()),
                upgrade: Some(up.clone()),
                generator: generator(),
                version: bunko_core::VERSION.to_string(),
            },
            settings,
        );
        OcrControl(Arc::new(Inner {
            core: deps.core,
            db: deps.db,
            tx,
            pending: Mutex::new(Some((sched, rx))),
            thread: Mutex::new(None),
            local: deps.local,
            local_finished: Mutex::new(None),
            upgrade: up,
            queue_file: queue_file::QueueFile::default(),
            status: queue_api::StatusCache::default(),
            storage: layout.base.clone(),
        }))
    }

    pub fn core(&self) -> &Core {
        &self.0.core
    }

    pub fn db(&self) -> Option<&Arc<Database>> {
        self.0.db.as_ref()
    }

    pub fn storage(&self) -> &Path {
        &self.0.storage
    }

    /// The full build: this process may run OCR itself.
    pub fn has_local(&self) -> bool {
        self.0.local.is_some()
    }

    pub(crate) fn queue_file_state(&self) -> &queue_file::QueueFile {
        &self.0.queue_file
    }

    pub(crate) fn status_cache(&self) -> &queue_api::StatusCache {
        &self.0.status
    }

    pub fn upgrade(&self) -> &Arc<upgrade::Upgrade> {
        &self.0.upgrade
    }

    /// Start the scheduler thread, the in-process processor (full build, local
    /// processing on) and the stop watcher. Must be called inside a tokio runtime.
    pub fn start(&self, stop: CancellationToken) {
        let Some((sched, rx)) = self.0.pending.lock().take() else {
            return;
        };
        let local_processing = sched.settings().local_processing;
        match actor::spawn(sched, rx) {
            Ok(handle) => *self.0.thread.lock() = Some(handle),
            Err(e) => {
                tracing::error!("could not start the OCR scheduler: {e}");
                return;
            }
        }
        if local_processing && let Some(factory) = self.0.local.clone() {
            self.start_local(factory.as_ref());
        }
        let me = self.clone();
        tokio::spawn(async move {
            stop.cancelled().await;
            me.send(Msg::Stop);
        });
    }

    fn start_local(&self, factory: &dyn LocalProcessorFactory) {
        let results = self.0.storage.join(".processing");
        let _ = std::fs::create_dir_all(&results);
        let channels = match factory.start(&results) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("could not start this server's OCR: {e}");
                return;
            }
        };
        if let Some(f) = channels.finished {
            *self.0.local_finished.lock() = Some(f);
        }
        let (ops_tx, mut ops_rx) = mpsc::unbounded_channel::<Op>();
        let bounded = channels.ops;
        tokio::spawn(async move {
            while let Some(op) = ops_rx.recv().await {
                if bounded.send(op).await.is_err() {
                    break;
                }
            }
        });
        // `LocalUp` goes first: the processor's first events (a pause in force says
        // `availability` at once) must find the `local` machine registered.
        self.send(Msg::LocalUp {
            ops: ops_tx,
            catalog: channels.catalog,
            host: channels.host,
        });
        let tx = self.0.tx.clone();
        let mut events = channels.events;
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if tx
                    .send(Msg::Event {
                        pid: types::LOCAL.into(),
                        event,
                    })
                    .is_err()
                {
                    return;
                }
            }
            let _ = tx.send(Msg::Drop {
                pid: types::LOCAL.into(),
                reason: "the local processor stopped".into(),
            });
        });
    }

    /// Stop the scheduler (in-flight volumes go back unrecorded) and wait for it, then
    /// for the local processor it let go of: its sessions wind down and free their
    /// models before this returns (bounded), not under the process exit.
    pub async fn stop(&self) {
        self.send(Msg::Stop);
        let handle = self.0.thread.lock().take();
        if let Some(h) = handle {
            let _ = tokio::task::spawn_blocking(move || h.join()).await;
        }
        let local = self.0.local_finished.lock().take();
        if let Some(f) = local
            && tokio::time::timeout(LOCAL_STOP_WAIT, f).await.is_err()
        {
            tracing::warn!(
                "this server's OCR was still stopping after {}s; exiting anyway",
                LOCAL_STOP_WAIT.as_secs()
            );
        }
    }

    pub fn send(&self, msg: Msg) -> bool {
        self.0.tx.send(msg).is_ok()
    }

    /// Run `f` on the scheduler and await its answer (None: stopped or busy > 30 s).
    pub async fn ask<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Scheduler) -> T + Send + 'static,
    ) -> Option<T> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let q: sched::Query = Box::new(move |s: &mut Scheduler| {
            let _ = reply.send(f(s));
        });
        if !self.send(Msg::Query(q)) {
            return None;
        }
        tokio::time::timeout(Duration::from_secs(30), rx)
            .await
            .ok()?
            .ok()
    }

    /// [`Self::ask`] from synchronous code (a blocking thread), with a deadline.
    pub fn ask_blocking<T: Send + 'static>(
        &self,
        wait: Duration,
        f: impl FnOnce(&mut Scheduler) -> T + Send + 'static,
    ) -> Option<T> {
        let (reply, rx) = std::sync::mpsc::channel();
        let q: sched::Query = Box::new(move |s: &mut Scheduler| {
            let _ = reply.send(f(s));
        });
        if !self.send(Msg::Query(q)) {
            return None;
        }
        rx.recv_timeout(wait).ok()
    }

    // --- hooks the DAV layer calls (from blocking threads) ------------------------------------

    /// A library `.cbz` is in place (PUT, MOVE/COPY into place): queue its OCR now.
    pub fn archive_arrived(&self, cbz: &Path) {
        self.send(Msg::ArchiveArrived(cbz.to_path_buf()));
    }

    /// Archives or folders a DELETE / MOVE took away: cancel their OCR.
    pub fn archives_removed(&self, paths: &[PathBuf]) {
        for p in paths {
            self.send(Msg::ArchiveRemoved(p.clone()));
        }
    }

    /// The PUT follow-up headers: `(X-Mokuro-Manifest, X-Mokuro-Recheck-After)` when
    /// OCR is owed. Call after `archive_arrived` (the scheduler handles messages in
    /// order, so it has seen the arrival).
    pub fn put_follow_up(&self, cbz: &Path, series: &str, volume: &str) -> Option<(String, i64)> {
        let library = self.0.core.layout.library();
        let rel = types::rel_of(&library, cbz)?;
        let (pending, now) = self.ask_blocking(Duration::from_secs(1), move |s| {
            (s.volume_pending(&rel, Some(300)), s.now())
        })?;
        if pending.is_empty() {
            return None;
        }
        let recheck = bunko_sched::outlook::recheck_after(&pending, now)?;
        Some((manifest_url(series, volume), recheck))
    }

    /// A catalog manifest's `pending` and `recheck_after` for one volume (None: the
    /// scheduler is not running; `([], None)` when nothing is owed).
    pub async fn volume_outlook(&self, cbz: &Path) -> Option<(Vec<Value>, Option<i64>)> {
        let library = self.0.core.layout.library();
        let rel = types::rel_of(&library, cbz)?;
        let (pending, now) = self
            .ask(move |s| (s.volume_pending(&rel, None), s.now()))
            .await?;
        let recheck = bunko_sched::outlook::recheck_after(&pending, now);
        Some((pending, recheck))
    }

    // --- settings and accounts -------------------------------------------------------------

    /// `OcrControl.apply`: push saved OCR settings into the running scheduler.
    /// `{applied, installing, restart_required, reason}`.
    pub fn apply_config(&self, config: &Config) -> Value {
        let settings = settings_from_config(config, self.has_local());
        self.0.upgrade.configure(&settings.upgrade, &settings.rows);
        let wanted_local = settings.local_processing;
        let local_running = if self.0.pending.lock().is_some() {
            // Not started yet: the new settings simply become the starting ones.
            if let Some((s, _)) = self.0.pending.lock().as_mut() {
                s.apply_settings(settings);
            }
            wanted_local
        } else {
            self.ask_blocking(Duration::from_secs(10), move |s| {
                s.apply_settings(settings);
                s.machines.contains_key(types::LOCAL)
            })
            .unwrap_or(false)
        };
        let mut restart_required = false;
        let mut reason = String::new();
        if config.ocr.local_processing && !self.has_local() && config.ocr.backend != "skip" {
            reason = "this build runs no OCR of its own; only a connected processor reads volumes"
                .into();
        } else if wanted_local && !local_running {
            restart_required = true;
            reason = "local processing starts with the next server start".into();
        }
        json!({"applied": true, "installing": false, "restart_required": restart_required, "reason": reason})
    }

    /// Cut off every processor of an account now (disabled, deleted, re-roled).
    pub fn drop_account(&self, username: &str, reason: &str) {
        self.send(Msg::DropAccount {
            username: username.to_string(),
            reason: reason.to_string(),
        });
    }

    /// A login on a processor path was refused (the admin's Processors card lists them).
    pub fn record_failed_login(&self, username: &str, reason: &str) {
        self.send(Msg::FailedLogin {
            username: username.to_string(),
            reason: reason.to_string(),
        });
    }

    // --- holds -------------------------------------------------------------------------------

    /// An automatic update waits: no machine gets new claims (`false`: they do again).
    pub async fn set_update_drain(&self, on: bool) {
        let _ = self.ask(move |s| s.set_update_drain(on)).await;
    }

    /// What OCR has in flight that a restart would break (None: nothing).
    pub async fn in_flight(&self) -> Option<String> {
        self.ask(|s| s.in_flight()).await.flatten()
    }

    pub async fn hold(&self, machine: &str) {
        let m = machine.to_string();
        let _ = self.ask(move |s| s.hold_queue(&m)).await;
    }

    pub async fn release(&self, machine: &str) {
        let m = machine.to_string();
        let _ = self.ask(move |s| s.release_queue(&m)).await;
    }

    /// `processing_hold()`: `{reason: "no-processor", since, last?}` or None.
    pub async fn processing_hold(&self) -> Option<Value> {
        self.ask(|s| s.processing_hold()).await.flatten()
    }

    /// `queue_hold()`: `no-processor` | `benchmarking` | `paused` | None.
    pub async fn queue_hold(&self) -> Option<&'static str> {
        self.ask(|s| s.queue_hold()).await.flatten()
    }

    // --- health ------------------------------------------------------------------------------

    /// The `ocr` block of `/api/health`: `{backend, worker_alive, pending, failed}`.
    pub fn health(&self) -> Value {
        let alive = self
            .0
            .thread
            .lock()
            .as_ref()
            .is_some_and(|h| !h.is_finished());
        let counts = self.ask_blocking(Duration::from_secs(2), |s| {
            (s.pending_jobs().len(), s.failures.len())
        });
        let backend = self.0.core.config.read().ocr.backend.clone();
        match counts {
            Some((pending, failed)) => {
                json!({"backend": backend, "worker_alive": alive, "pending": pending, "failed": failed})
            }
            None => {
                json!({"backend": backend, "worker_alive": false, "pending": null, "failed": 0})
            }
        }
    }
}

/// The OCR side of the WebDAV hooks: add it with `ServerDavHooks::add_listener`.
impl bunko_dav::DavHooks for OcrControl {
    fn archive_arrived(&self, cbz: &Path) {
        OcrControl::archive_arrived(self, cbz);
    }

    fn archives_removed(&self, paths: &[PathBuf]) {
        OcrControl::archives_removed(self, paths);
    }

    fn put_follow_up(
        &self,
        cbz: &Path,
        series: &str,
        volume: &str,
    ) -> Option<bunko_dav::PutFollowUp> {
        let (manifest, recheck) = OcrControl::put_follow_up(self, cbz, series, volume)?;
        Some(bunko_dav::PutFollowUp {
            manifest,
            recheck_after: recheck.max(0) as u64,
        })
    }
}

impl crate::accounts::HealthSource for OcrControl {
    fn ocr_health(&self) -> Value {
        self.health()
    }
}

/// `/_processor/*`: registration, the socket, result uploads, bench samples.
pub fn processor_router(ocr: OcrControl) -> axum::Router {
    api::router(ocr)
}

/// `/queue`, `/queue/`, `/queue/api/config`, `/queue/api/status`, `/queue/<file>`.
pub fn queue_router(ocr: OcrControl) -> axum::Router {
    queue_api::router(ocr)
}
