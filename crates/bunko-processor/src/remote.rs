//! `processor serve`: log in, register, hold the socket, run what the library sends,
//! and register again whenever the connection ends (spec remote-processors §8.1, §9.1).
//!
//! A refused login is final (the caller exits 1). A socket answered 409 (or a
//! registration the library no longer knows) re-registers after a 1 s floor; every
//! other ending backs off 5 s, doubling to 300 s, reset by a successful registration.
//! Whatever ends a connection, everything in flight is shut down silently: the
//! library sees the socket close without the sessions' `exit`s, which is a processor
//! going away — every claim goes back unrecorded and nothing is blamed.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bunko_proto::{Event, HEARTBEAT_SECONDS, Op, PROTOCOL_VERSION, RegisterReply, RegisterRequest};
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

/// Run until `shutdown` (Ok) or a refused login (Err).
pub async fn serve(options: ServeOptions) -> Result<(), ServeError> {
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
    if result.is_ok() {
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

async fn serve_loop(options: &ServeOptions, spool: Arc<ArchiveSpool>) -> Result<(), ServeError> {
    let config = &options.config;
    let storage = &config.processor.storage;
    let url = &config.library.url;
    let start = options.backoff_start;
    let mut backoff = start;
    loop {
        if options.shutdown.is_cancelled() {
            return Ok(());
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
                let ended = run_connection(options, &client, &reply, ws, spool.clone()).await;
                write_status(storage, State::Disconnected, url, None, None);
                let why = match &ended {
                    Ended::Shutdown => None,
                    Ended::Lost(r) | Ended::Closed(r) => Some(r.as_str()),
                };
                link_state(options, bunko_control::LinkPhase::Disconnected, why);
                match ended {
                    Ended::Shutdown => return Ok(()),
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
            return Ok(());
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
}

async fn run_connection(
    options: &ServeOptions,
    client: &LibraryClient,
    reply: &RegisterReply,
    mut ws: WebSocket,
    spool: Arc<ArchiveSpool>,
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
    let ended = loop {
        tokio::select! {
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
