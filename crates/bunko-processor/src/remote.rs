//! `processor serve`: log in, register, hold the socket, run what the library sends,
//! and register again whenever the connection ends (spec remote-processors §8.1, §9.1).
//!
//! A refused login is final (the caller exits 1). A socket answered 409 (or a
//! registration the library no longer knows) re-registers after a 1 s floor; every
//! other ending backs off 5 s, doubling to 300 s, reset by a successful registration.
//! Whatever ends a connection, everything in flight is shut down silently: the
//! library sees the socket close without the sessions' `exit`s, which is a processor
//! going away — every claim goes back unrecorded and nothing is blamed.
//!
//! Automatic update (`processor.auto_update`, opt-in; PROTOCOL.md "Version mismatch"):
//! when the registration reply's `version_mismatch` says the library is newer, the hub
//! drains (after-volume: the running volumes finish and upload, the rest go back), the
//! [`ServeOptions::installer`] installs exactly the library's release, and `serve`
//! returns [`ServeExit::Updated`] so the binary restarts into it. A library that is
//! older is only reported (never a downgrade); a failed install keeps this version
//! running and is retried with backoff (`bunko_update::auto::Retry`). Progress goes to
//! the library as `update_status` events and to the control API's `status.update`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bunko_proto::{
    Event, HEARTBEAT_SECONDS, Op, PROTOCOL_VERSION, RegisterReply, RegisterRequest, UpdateReport,
    VersionMismatch,
};
use bunko_update::auto::{InstallFailure, ProcessorAction, ReleaseInstaller, Retry};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::bench::BenchConfig;
use crate::client::{ClientError, LibraryClient, WebSocket, http_client};
use crate::config::ProcessorConfig;
use crate::fetch::{ArchiveFetcher, FetchTiming};
use crate::lock::lock_storage;
use crate::pipeline::PagePipeline;
use crate::session::{Hub, Link, RemoteLink};
use crate::spool::ArchiveSpool;
use crate::status::{State, write_status};

pub const BACKOFF_START: Duration = Duration::from_secs(5);
pub const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// A 409 says "register again now" — but not faster than this.
pub const REREGISTER_FLOOR: Duration = Duration::from_secs(1);
/// No frame at all from the library for this long: it is gone (2 heartbeats + 5 s).
pub const SOCKET_SILENCE: Duration = Duration::from_secs(2 * HEARTBEAT_SECONDS + 5);
/// The processor says `ping` when it has said nothing for this long.
pub const KEEPALIVE: Duration = Duration::from_secs(10);
/// How long a leaving processor waits for its sessions to wind down.
const LEAVE_WAIT: Duration = Duration::from_secs(10);

/// Everything `serve` needs.
pub struct ServeOptions {
    pub config: ProcessorConfig,
    pub pipeline: Arc<dyn PagePipeline>,
    pub timing: FetchTiming,
    /// Cancel to stop (SIGTERM / Ctrl+C): sessions are abandoned silently.
    pub shutdown: CancellationToken,
    /// Log every op.
    pub verbose: bool,
    /// The first reconnect wait (doubles up to [`BACKOFF_MAX`]); tests shorten it.
    pub backoff_start: Duration,
    /// The numbers benchmarks run by (`jobs` = this processor's sessions).
    pub bench: BenchConfig,
    /// The local control API (GUI.md §2-3): its pause is obeyed, its activity and link
    /// state kept current. None: no control API (tests, `MOKURO_CONTROL=off`).
    pub control: Option<bunko_control::Control>,
    /// Installs a release for `processor.auto_update` (the binary supplies it). None:
    /// a version mismatch is only reported.
    pub installer: Option<Arc<dyn ReleaseInstaller>>,
    /// The first retry wait after a failed automatic update (tests shorten it).
    pub retry_first: Duration,
}

/// How `serve` ended without an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeExit {
    /// Shutdown (SIGTERM / Ctrl+C / the control API's stop).
    Stopped,
    /// An automatic update installed this release: restart into it.
    Updated(String),
}

impl ServeOptions {
    pub fn new(config: ProcessorConfig, pipeline: Arc<dyn PagePipeline>) -> ServeOptions {
        let bench = BenchConfig {
            jobs: config.processor.max_sessions.max(1) as usize,
            ..BenchConfig::default()
        };
        ServeOptions {
            bench,
            config,
            pipeline,
            timing: FetchTiming::default(),
            shutdown: CancellationToken::new(),
            verbose: false,
            backoff_start: BACKOFF_START,
            control: None,
            installer: None,
            retry_first: bunko_update::auto::retry_first(),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ServeError {
    #[error("another processor is running on {}; give each processor its own `storage`", .0.display())]
    StorageLocked(PathBuf),
    /// Exit 1.
    #[error("Login refused: {0}")]
    LoginRefused(String),
    #[error("{0}")]
    Storage(String),
}

/// `<storage>/.processing/work`: per-claim workspaces (sidecars before upload).
pub fn work_dir(config: &ProcessorConfig) -> PathBuf {
    config.processor.storage.join(".processing").join("work")
}

/// Run until `shutdown` or an installed automatic update (Ok) or a refused login (Err).
pub async fn serve(options: ServeOptions) -> Result<ServeExit, ServeError> {
    let storage = options.config.processor.storage.clone();
    std::fs::create_dir_all(&storage)
        .map_err(|e| ServeError::Storage(format!("could not create {}: {e}", storage.display())))?;
    // FIRST, before anything touches the storage.
    let _lock = lock_storage(&storage)
        .map_err(|e| ServeError::Storage(format!("could not lock {}: {e}", storage.display())))?
        .ok_or_else(|| ServeError::StorageLocked(storage.clone()))?;
    let spool = Arc::new(ArchiveSpool::new(
        &storage,
        options.config.processor.archive_memory_mb,
    ));
    spool.sweep();
    let _ = std::fs::remove_dir_all(work_dir(&options.config));
    let result = serve_loop(&options, spool).await;
    if result == Ok(ServeExit::Stopped) {
        write_status(
            &storage,
            State::Stopped,
            &options.config.library.url,
            None,
            None,
        );
        tracing::info!("Processor stopped");
    }
    result
}

/// Keep the control API's view of the link current.
fn link_state(options: &ServeOptions, phase: bunko_control::LinkPhase, error: Option<&str>) {
    if let Some(c) = &options.control {
        c.set_link(phase, Some(&options.config.library.url), error);
    }
}

async fn pause(shutdown: &CancellationToken, wait: Duration) -> bool {
    tokio::select! {
        _ = shutdown.cancelled() => false,
        _ = tokio::time::sleep(wait) => true,
    }
}

enum Next {
    Backoff,
    AtOnce,
}

/// The automatic update across connections: its retry clock and what it last said.
struct UpdateTrack {
    retry: Retry,
    report: Option<UpdateReport>,
}

impl UpdateTrack {
    /// Remember and publish (control API) a new state; the caller sends it to the
    /// library too when connected.
    fn set(
        &mut self,
        options: &ServeOptions,
        report: UpdateReport,
        problems: Vec<bunko_control::Problem>,
    ) {
        if let Some(c) = &options.control {
            let since = match &self.report {
                Some(r) if r.state == report.state => c.update_view().and_then(|v| v.since),
                _ => Some(bunko_update::auto::now_rfc3339()),
            };
            c.set_update(Some(bunko_control::UpdateView {
                state: report.state.clone(),
                version: report.version.clone(),
                from: None,
                message: report.message.clone(),
                auto: options.config.processor.auto_update,
                since,
            }));
            c.set_update_problems(problems);
        }
        self.report = Some(report);
    }
}

/// What the registration reply means for the automatic update, and the report to make.
fn mismatch_of(reply: &RegisterReply) -> (Option<VersionMismatch>, bool) {
    match &reply.version_mismatch {
        Some(m) => (Some(m.clone()), true),
        // A library that predates the field: compare here, but only to report.
        None => (
            VersionMismatch::between(&reply.version, env!("CARGO_PKG_VERSION")),
            false,
        ),
    }
}

fn report(
    state: &str,
    version: Option<&str>,
    message: Option<String>,
    action: Option<String>,
) -> UpdateReport {
    UpdateReport {
        state: state.into(),
        version: version.map(str::to_string),
        message,
        action,
    }
}

/// Decide at registration: `Some(version)` to update now; otherwise report.
fn on_registered(
    options: &ServeOptions,
    track: &mut UpdateTrack,
    reply: &RegisterReply,
) -> Option<String> {
    let (mismatch, from_library) = mismatch_of(reply);
    let auto = options.config.processor.auto_update && from_library && options.installer.is_some();
    let ours = env!("CARGO_PKG_VERSION");
    match bunko_update::auto::processor_action(mismatch.as_ref(), auto) {
        ProcessorAction::Nothing => {
            if track.report.as_ref().is_some_and(|r| r.state != "idle") {
                track.set(options, report("idle", None, None, None), Vec::new());
            }
            None
        }
        ProcessorAction::Update(v) => {
            if let Some(b) = bunko_update::auto::Blocked::read(&options.config.processor.storage)
                .filter(|b| b.blocks(&v))
            {
                tracing::warn!(
                    "The library runs {v}; the update to it was rolled back here ({}): not trying again",
                    b.reason
                );
                let action = format!(
                    "Fix the cause, then install {v} by hand ('mokuro-bunko update apply'); this processor stays on {ours}."
                );
                track.set(
                    options,
                    report(
                        "blocked",
                        Some(&v),
                        Some(format!("rolled back: {}", b.reason)),
                        Some(action.clone()),
                    ),
                    vec![bunko_control::Problem::update_needs_you(
                        format!("The update to {v} was rolled back: {}", b.reason),
                        action,
                    )],
                );
                return None;
            }
            if track.retry.may_try(&v, Instant::now()) {
                Some(v)
            } else {
                tracing::info!(
                    "The library runs {v}; the last automatic update to it failed, trying again later"
                );
                None
            }
        }
        ProcessorAction::ReportNewer(v) => {
            tracing::warn!(
                "The library runs mokuro-bunko {v}, this processor {ours}: update this machine (or set processor.auto_update: true in processor.yaml)"
            );
            track.set(
                options,
                report("off", Some(&v), Some(format!("the library runs {v}; this processor runs {ours}")), None),
                vec![bunko_control::Problem::update_warning(
                    format!("The library runs mokuro-bunko {v}; this processor runs {ours}"),
                    Some("Update this machine, or turn on automatic updates (processor.auto_update: true) so it follows the library.".into()),
                )],
            );
            None
        }
        ProcessorAction::ReportOlder(v) => {
            tracing::warn!(
                "The library runs mokuro-bunko {v}, older than this processor ({ours}): a processor never downgrades itself; update the library"
            );
            let action = format!(
                "Update the library to {ours} or newer (its admin panel → Updates); a processor never downgrades itself."
            );
            track.set(
                options,
                report(
                    "blocked",
                    Some(&v),
                    Some(format!("the library runs {v}, older than this processor ({ours})")),
                    Some(action.clone()),
                ),
                vec![bunko_control::Problem::update_needs_you(
                    format!("This processor ({ours}) cannot follow its library: the library runs the older {v}"),
                    action,
                )],
            );
            None
        }
        ProcessorAction::ReportUnknown(v) => {
            tracing::warn!(
                "The library runs mokuro-bunko {v:?}, this processor {ours}: versions do not compare; not updating"
            );
            None
        }
    }
}

async fn serve_loop(
    options: &ServeOptions,
    spool: Arc<ArchiveSpool>,
) -> Result<ServeExit, ServeError> {
    let config = &options.config;
    let storage = &config.processor.storage;
    let url = &config.library.url;
    let start = options.backoff_start;
    let mut backoff = start;
    let mut track = UpdateTrack {
        retry: Retry::with_first(options.retry_first),
        report: None,
    };
    loop {
        if options.shutdown.is_cancelled() {
            return Ok(ServeExit::Stopped);
        }
        let next = match connect(options).await {
            Err(ClientError::LoginRefused(e)) => {
                write_status(storage, State::Refused, url, None, Some(&e));
                link_state(
                    options,
                    bunko_control::LinkPhase::Refused,
                    Some(&format!("login refused: {e}")),
                );
                tracing::error!("Login refused: {e}");
                return Err(ServeError::LoginRefused(e));
            }
            Err(ClientError::Reregister(e)) => {
                tracing::warn!("{e}");
                Next::AtOnce
            }
            Err(ClientError::Library(e)) => {
                write_status(storage, State::Unreachable, url, None, Some(&e));
                link_state(options, bunko_control::LinkPhase::Disconnected, Some(&e));
                tracing::warn!("{e}; retrying in {}s", backoff.as_secs());
                Next::Backoff
            }
            Ok((client, reply, ws, engines)) => {
                backoff = start;
                write_status(
                    storage,
                    State::Connected,
                    url,
                    Some(&config.processor.name),
                    None,
                );
                link_state(options, bunko_control::LinkPhase::Connected, None);
                tracing::info!(
                    "Connected to {url} as {} ({engines} engine(s), {} session slot(s)); library {}",
                    config.processor.name,
                    config.processor.max_sessions,
                    reply.version
                );
                let update = on_registered(options, &mut track, &reply);
                let ended = run_connection(
                    options,
                    &client,
                    &reply,
                    ws,
                    spool.clone(),
                    &mut track,
                    update,
                )
                .await;
                write_status(storage, State::Disconnected, url, None, None);
                let why = match &ended {
                    Ended::Shutdown | Ended::Updated(_) => None,
                    Ended::Lost(r) | Ended::Closed(r) => Some(r.as_str()),
                };
                link_state(options, bunko_control::LinkPhase::Disconnected, why);
                match ended {
                    Ended::Shutdown => return Ok(ServeExit::Stopped),
                    Ended::Updated(v) => {
                        tracing::info!("Automatic update: installed mokuro-bunko {v}; restarting");
                        return Ok(ServeExit::Updated(v));
                    }
                    Ended::Lost(reason) | Ended::Closed(reason) => {
                        tracing::warn!(
                            "Disconnected ({reason}); reconnecting in {}s",
                            backoff.as_secs()
                        );
                        Next::Backoff
                    }
                }
            }
        };
        let wait = match next {
            Next::AtOnce => REREGISTER_FLOOR.min(start),
            Next::Backoff => {
                let wait = backoff;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                wait
            }
        };
        if !pause(&options.shutdown, wait).await {
            return Ok(ServeExit::Stopped);
        }
    }
}

/// Probe, register, open the socket.
async fn connect(
    options: &ServeOptions,
) -> Result<(LibraryClient, RegisterReply, WebSocket, usize), ClientError> {
    let config = &options.config;
    let client = LibraryClient::new(&config.library, &config.processor.name)?;
    // Re-probed at every registration: models that finished downloading show up.
    let pipeline = options.pipeline.clone();
    let mut info = tokio::task::spawn_blocking(move || pipeline.describe())
        .await
        .map_err(|e| ClientError::Library(format!("describing this machine failed: {e}")))?;
    if info.host.version.is_empty() {
        info.host.version = env!("CARGO_PKG_VERSION").to_string();
    }
    let engines = info.catalog.engines.len();
    if let Some(c) = &options.control {
        c.set_devices(&info.catalog.devices);
    }
    // A paused processor says so in its registration: it is offered nothing at all.
    let availability = options
        .control
        .as_ref()
        .and_then(|c| c.pause_ctl())
        .and_then(|p| p.current())
        .map(|s| s.availability());
    let request = RegisterRequest {
        protocol: PROTOCOL_VERSION,
        name: Some(config.processor.name.clone()),
        public_name: config.processor.public_name.clone(),
        host: info.host,
        catalog: info.catalog,
        max_sessions: config.processor.max_sessions,
        availability,
    };
    let reply = tokio::select! {
        _ = options.shutdown.cancelled() => return Err(ClientError::Library("stopping".into())),
        r = client.register(&request) => r?,
    };
    let ws = client.connect_socket(&reply.socket).await?;
    Ok((client, reply, ws, engines))
}

enum Ended {
    Shutdown,
    Lost(String),
    Closed(String),
    /// The automatic update installed this version.
    Updated(String),
}

/// The automatic update within one connection.
enum Phase {
    Idle,
    /// Drained; waiting for the running volumes to finish.
    Draining(String),
    Installing(
        String,
        tokio::task::JoinHandle<Result<String, InstallFailure>>,
    ),
}

#[allow(clippy::too_many_arguments)]
async fn run_connection(
    options: &ServeOptions,
    client: &LibraryClient,
    reply: &RegisterReply,
    mut ws: WebSocket,
    spool: Arc<ArchiveSpool>,
    track: &mut UpdateTrack,
    update: Option<String>,
) -> Ended {
    let leaving = Arc::new(AtomicBool::new(false));
    let lost = CancellationToken::new();
    let (tx, mut events) = mpsc::unbounded_channel::<Event>();
    let fetcher = match http_client(&client.tls(), options.timing.connect_timeout, false) {
        Ok(http) => ArchiveFetcher::new(
            http,
            client.url(),
            client.credentials(),
            spool,
            options.timing.clone(),
        ),
        Err(e) => return Ended::Closed(e.to_string()),
    };
    let link = Link::Remote(Box::new(RemoteLink {
        fetcher,
        client: client.clone(),
        results: reply.results.clone(),
        work: work_dir(&options.config),
        lost: lost.clone(),
    }));
    let hub = Hub::new(
        options.pipeline.clone(),
        link,
        tx,
        leaving.clone(),
        options.bench.clone(),
        options.control.clone(),
    );
    let mut last_heard = Instant::now();
    let mut last_sent = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    // The library hears where the update stands (and an older processor's state).
    // Only a library that reported the mismatch hears about the update (an older one
    // would log every report as an unknown event).
    let tells = reply.version_mismatch.is_some();
    let say = |ev: UpdateReport| {
        if tells {
            hub.tell(Event::UpdateStatus(ev));
        }
    };
    if let Some(r) = &track.report {
        say(r.clone());
    }
    let mut phase = Phase::Idle;
    // The version to follow and when to try (now, or after a failure's wait).
    let target = update.or_else(|| {
        // A failed earlier attempt at the library's version: try again when due.
        let (m, from_library) = mismatch_of(reply);
        let auto =
            options.config.processor.auto_update && from_library && options.installer.is_some();
        match bunko_update::auto::processor_action(m.as_ref(), auto) {
            ProcessorAction::Update(v)
                if !bunko_update::auto::Blocked::read(&options.config.processor.storage)
                    .is_some_and(|b| b.blocks(&v)) =>
            {
                Some(v)
            }
            _ => None,
        }
    });
    let ended = loop {
        // Start draining when an update is due.
        if let (Phase::Idle, Some(v)) = (&phase, &target)
            && track.retry.may_try(v, Instant::now())
        {
            tracing::info!(
                "Automatic update: the library runs mokuro-bunko {v}; finishing the running volume(s), then updating"
            );
            hub.drain_for_update();
            let r = report(
                "waiting",
                Some(v),
                Some("finishing the running volume(s) first".into()),
                None,
            );
            track.set(options, r.clone(), Vec::new());
            say(r);
            phase = Phase::Draining(v.clone());
        }
        // Quiet: nothing running, and every event of the finished volumes is out.
        if let Phase::Draining(v) = &phase
            && hub.in_flight() == 0
            && events.is_empty()
        {
            let v = v.clone();
            tracing::info!("Automatic update: nothing running; installing mokuro-bunko {v}");
            let r = report(
                "installing",
                Some(&v),
                Some("downloading and checking the release".into()),
                None,
            );
            track.set(options, r.clone(), Vec::new());
            say(r);
            let installer = options.installer.clone();
            let version = v.clone();
            let task = tokio::spawn(async move {
                match installer {
                    Some(i) => i.install(version).await,
                    None => Err(InstallFailure::retry("no installer")),
                }
            });
            phase = Phase::Installing(v, task);
        }
        let installing = matches!(phase, Phase::Installing(..));
        tokio::select! {
            done = async {
                match &mut phase {
                    Phase::Installing(_, task) => task.await,
                    _ => std::future::pending().await,
                }
            }, if installing => {
                let Phase::Installing(v, _) = std::mem::replace(&mut phase, Phase::Idle) else { unreachable!() };
                let result = done.unwrap_or_else(|e| Err(InstallFailure::retry(format!("the installer stopped: {e}"))));
                match result {
                    Ok(installed) => {
                        track.retry.succeeded();
                        let r = report("restarting", Some(&installed), Some(format!("restarting into {installed}")), None);
                        track.set(options, r.clone(), Vec::new());
                        say(r);
                        // Let the report reach the library before the socket closes.
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        while let Ok(event) = events.try_recv() {
                            if let Ok(text) = serde_json::to_string(&event) {
                                let _ = ws.send(Message::Text(text.into())).await;
                            }
                        }
                        break Ended::Updated(installed);
                    }
                    Err(f) => {
                        let wait = track.retry.failed(&v, &f.message, Instant::now());
                        tracing::error!(
                            "Automatic update to {v} failed: {}; still running {}, taking work again; next try in {} min",
                            f.message,
                            env!("CARGO_PKG_VERSION"),
                            wait.as_secs().div_ceil(60)
                        );
                        hub.end_drain();
                        let (state, problems) = if f.needs_owner {
                            ("blocked", vec![bunko_control::Problem::update_needs_you(
                                format!("The automatic update to {v} needs you: {}", f.message),
                                f.action.clone().unwrap_or_else(|| "See the processor log.".into()),
                            )])
                        } else if track.retry.keeps_failing() {
                            ("failed", vec![bunko_control::Problem::update_needs_you(
                                format!("The automatic update to {v} keeps failing: {}", f.message),
                                "See the processor log.".to_string(),
                            )])
                        } else {
                            ("failed", vec![bunko_control::Problem::update_warning(
                                format!("The automatic update to {v} failed (trying again later): {}", f.message),
                                None,
                            )])
                        };
                        let r = report(state, Some(&v), Some(f.message.clone()), f.action.clone());
                        track.set(options, r.clone(), problems);
                        say(r);
                    }
                }
            }
            _ = options.shutdown.cancelled() => break Ended::Shutdown,
            _ = lost.cancelled() => break Ended::Lost("the library refused this account for a download".into()),
            event = events.recv() => {
                let Some(event) = event else { continue };
                let text = match serde_json::to_string(&event) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!("could not encode an event: {e}");
                        continue;
                    }
                };
                if let Err(e) = ws.send(Message::Text(text.into())).await {
                    break Ended::Closed(format!("the socket failed: {e}"));
                }
                last_sent = Instant::now();
            }
            message = ws.next() => match message {
                None => break Ended::Closed("the library closed the socket".into()),
                Some(Err(e)) => break Ended::Closed(format!("the socket failed: {e}")),
                Some(Ok(Message::Close(_))) => break Ended::Closed("the library closed the socket".into()),
                Some(Ok(Message::Text(text))) => {
                    last_heard = Instant::now();
                    match serde_json::from_str::<Op>(text.as_str()) {
                        Ok(op) => {
                            if options.verbose {
                                tracing::debug!("op {}", text.as_str().chars().take(200).collect::<String>());
                            }
                            hub.handle(op);
                        }
                        Err(e) => tracing::warn!("unreadable op from the library ({e}): {}", text.as_str().chars().take(200).collect::<String>()),
                    }
                }
                Some(Ok(Message::Ping(_))) => {
                    last_heard = Instant::now();
                    // The pong is queued by the read; send it now.
                    if let Err(e) = ws.flush().await {
                        break Ended::Closed(format!("the socket failed: {e}"));
                    }
                }
                Some(Ok(_)) => last_heard = Instant::now(),
            },
            _ = tick.tick() => {
                if last_heard.elapsed() > SOCKET_SILENCE {
                    break Ended::Closed(format!("the library sent nothing for {}s", SOCKET_SILENCE.as_secs()));
                }
                if last_sent.elapsed() >= KEEPALIVE {
                    if let Ok(text) = serde_json::to_string(&Event::Ping)
                        && let Err(e) = ws.send(Message::Text(text.into())).await
                    {
                        break Ended::Closed(format!("the socket failed: {e}"));
                    }
                    last_sent = Instant::now();
                }
            }
        }
    };
    if let Phase::Installing(_, task) = phase {
        task.abort();
    }
    // Silence BEFORE anything is stopped: no event of the teardown may reach the
    // library, or a processor switched off would read as a runner that crashed.
    leaving.store(true, Ordering::SeqCst);
    let sessions = hub.session_count();
    hub.leave(LEAVE_WAIT).await;
    if sessions > 0 {
        tracing::info!(
            "{sessions} session(s) ended with the connection; the library returns their volumes"
        );
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), ws.close(None)).await;
    ended
}
