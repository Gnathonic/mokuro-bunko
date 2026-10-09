//! `processor.auto_update` against a fake library that reports a version mismatch in
//! its registration reply (PROTOCOL.md "Version mismatch"): newer → drain, install,
//! `ServeExit::Updated`; older → never a downgrade; auto off → report only; a failed
//! install → keep working, retry later; a running volume finishes before the install.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use bunko_processor::config::{LibrarySettings, ProcessorSettings, TlsVerify};
use bunko_processor::{FakeConfig, FakePipeline, ProcessorConfig, ServeExit, ServeOptions, serve};
use bunko_proto::{Event, Op, RegisterRequest, VersionMismatch, VolumeOp};
use bunko_update::auto::{InstallFailure, ReleaseInstaller};
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::Notify;

const OURS: &str = env!("CARGO_PKG_VERSION");

struct Lib {
    version: String,
    archive: Vec<u8>,
    /// In order: `register`, `socket`, `<event>[:<claim>]`, `install:<n>`.
    log: Vec<String>,
    mismatches: Vec<Option<VersionMismatch>>,
    /// Why each `volume_failed` failed (shown when a wait times out).
    errors: Vec<String>,
    /// Open the session only after the first install attempt failed (so the retry
    /// finds a volume running).
    open_after_failure: bool,
    opened: bool,
}

type Shared = Arc<(Mutex<Lib>, Notify)>;

fn push(lib: &Shared, entry: String) {
    lib.0.lock().log.push(entry);
    lib.1.notify_waiters();
}

async fn token() -> Response {
    axum::Json(json!({"token": "t", "token_type": "Bearer"})).into_response()
}

async fn register(State(lib): State<Shared>, body: Bytes) -> Response {
    let request: RegisterRequest = serde_json::from_slice(&body).unwrap();
    let version = lib.0.lock().version.clone();
    let mismatch = VersionMismatch::between(&version, &request.host.version);
    {
        let mut l = lib.0.lock();
        l.mismatches.push(mismatch.clone());
    }
    push(&lib, "register".into());
    let mut reply = json!({
        "protocol": 3, "processor_id": "p1", "socket": "/_processor/p1/socket",
        "results": "/_processor/p1/results/{sid}/{claim}", "archives": "/mokuro-reader/",
        "version": version,
    });
    if let Some(m) = mismatch {
        reply["version_mismatch"] = serde_json::to_value(m).unwrap();
    }
    axum::Json(reply).into_response()
}

async fn archive(State(lib): State<Shared>) -> Response {
    let body = lib.0.lock().archive.clone();
    (
        [(header::ETAG, "\"a1\""), (header::ACCEPT_RANGES, "bytes")],
        body,
    )
        .into_response()
}

/// Reads the whole upload before answering, as the library does: answering first closes
/// the connection under a body still being sent, which Windows reports to the client as
/// "connection aborted" (os error 10053) and the processor counts as a failed upload.
async fn result(_body: Bytes) -> Response {
    StatusCode::OK.into_response()
}

async fn socket(State(lib): State<Shared>, upgrade: WebSocketUpgrade) -> Response {
    push(&lib, "socket".into());
    upgrade.on_upgrade(move |ws| drive(ws, lib))
}

fn op(op: &Op) -> Message {
    Message::Text(serde_json::to_string(op).unwrap().into())
}

async fn open(ws: &mut WebSocket) {
    ws.send(op(&Op::OpenSession {
        sid: "s1".into(),
        generation: common::row("fake"),
    }))
    .await
    .unwrap();
}

async fn drive(mut ws: WebSocket, lib: Shared) {
    if !lib.0.lock().open_after_failure {
        lib.0.lock().opened = true;
        open(&mut ws).await;
    }
    while let Some(Ok(message)) = ws.recv().await {
        let Message::Text(text) = message else {
            continue;
        };
        let event: Event = serde_json::from_str(text.as_str()).unwrap();
        let mut name = serde_json::to_value(&event).unwrap()["event"]
            .as_str()
            .unwrap()
            .to_string();
        if let Event::UpdateStatus(r) = &event {
            name = format!("update_status:{}", r.state);
        }
        if let Event::Availability(a) = &event {
            name = format!("availability:{}", a.paused);
        }
        if let Event::VolumeFailed { error, .. } = &event {
            lib.0.lock().errors.push(error.clone());
        }
        if matches!(
            event,
            Event::Page { .. } | Event::Stats { .. } | Event::Ping
        ) {
            continue;
        }
        push(
            &lib,
            match event.claim() {
                Some(c) => format!("{name}:{c}"),
                None => name.clone(),
            },
        );
        match &event {
            Event::UpdateStatus(r) if r.state == "failed" => {
                let first = {
                    let mut l = lib.0.lock();
                    let first = !l.opened;
                    l.opened = true;
                    first
                };
                if first {
                    open(&mut ws).await;
                }
            }
            Event::Ready { .. } => ws
                .send(op(&Op::Volume(VolumeOp {
                    sid: "s1".into(),
                    claim: "v1".into(),
                    archive: "/mokuro-reader/A/Vol 1.cbz".into(),
                    sidecar_name: "Vol 1.mokuro".into(),
                    title: "A".into(),
                    volume_title: "Vol 1".into(),
                    title_uuid: None,
                    volume_uuid: None,
                    size: None,
                    etag: Some("\"a1\"".into()),
                })))
                .await
                .unwrap(),
            _ => {}
        }
    }
}

async fn library(version: &str, open_after_failure: bool) -> (String, Shared) {
    // The processor's own log lines, shown with a failing test's output.
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .try_init();
    let lib: Shared = Arc::new((
        Mutex::new(Lib {
            version: version.into(),
            archive: common::volume(4, 1000),
            log: Vec::new(),
            mismatches: Vec::new(),
            errors: Vec::new(),
            open_after_failure,
            opened: false,
        }),
        Notify::new(),
    ));
    let app = Router::new()
        .route("/login/api/token", post(token))
        .route("/_processor/register", post(register))
        .route("/_processor/{pid}/socket", get(socket))
        .route("/_processor/{pid}/results/{sid}/{claim}", put(result))
        .route("/mokuro-reader/{*path}", get(archive))
        .with_state(lib.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), lib)
}

/// Fails the first `fail_first` attempts, then succeeds; logs each call.
struct FakeInstaller {
    lib: Shared,
    calls: Mutex<u32>,
    fail_first: u32,
}

impl ReleaseInstaller for FakeInstaller {
    fn install(&self, version: String) -> BoxFuture<'static, Result<String, InstallFailure>> {
        let n = {
            let mut c = self.calls.lock();
            *c += 1;
            *c
        };
        push(&self.lib, format!("install:{n}:{version}"));
        let fail = n <= self.fail_first;
        Box::pin(async move {
            if fail {
                Err(InstallFailure::retry("the download failed"))
            } else {
                Ok(version)
            }
        })
    }
}

fn options(url: &str, storage: &std::path::Path, auto: bool, page_ms: u64) -> ServeOptions {
    let config = ProcessorConfig {
        library: LibrarySettings {
            url: url.into(),
            username: "gpu".into(),
            password: "pw".into(),
            tls_verify: TlsVerify::Yes,
        },
        processor: ProcessorSettings {
            name: "tower".into(),
            public_name: None,
            max_sessions: 1,
            storage: storage.to_path_buf(),
            archive_memory_mb: 0,
            auto_update: auto,
        },
        update: Default::default(),
    };
    let fake = FakePipeline::new(FakeConfig {
        load_delay: Duration::from_millis(20),
        page_delay: Duration::from_millis(page_ms),
        ..Default::default()
    });
    let mut o = ServeOptions::new(config, Arc::new(fake));
    o.backoff_start = Duration::from_millis(100);
    o.retry_first = Duration::from_millis(300);
    o
}

async fn wait_for(lib: &Shared, secs: u64, done: impl Fn(&Lib) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let notified = lib.1.notified();
        if done(&lib.0.lock()) {
            return;
        }
        if tokio::time::timeout_at(deadline, async {
            tokio::select! {
                _ = notified => {},
                _ = tokio::time::sleep(Duration::from_millis(50)) => {},
            }
        })
        .await
        .is_err()
        {
            let l = lib.0.lock();
            panic!("timed out; log so far: {:?}; errors: {:?}", l.log, l.errors);
        }
    }
}

fn pos(log: &[String], entry: &str) -> usize {
    log.iter()
        .position(|e| e == entry)
        .unwrap_or_else(|| panic!("{entry} not in {log:?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn newer_library_drains_then_updates_after_a_failed_try() {
    let dir = tempfile::tempdir().unwrap();
    let (url, lib) = library("99.0.0", true).await;
    let mut o = options(&url, dir.path(), true, 400);
    o.installer = Some(Arc::new(FakeInstaller {
        lib: lib.clone(),
        calls: Mutex::new(0),
        fail_first: 1,
    }));
    let task = tokio::spawn(serve(o));
    let ended = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("serve ended")
        .unwrap();
    assert_eq!(ended, Ok(ServeExit::Updated("99.0.0".into())));
    let log = lib.0.lock().log.clone();
    let mismatch = lib.0.lock().mismatches[0].clone().unwrap();
    // The mismatch round trip: the library compared the registered version.
    assert_eq!(mismatch.processor_version, OURS);
    assert!(mismatch.library_newer());
    // First try at once (nothing running), it fails: the processor keeps working.
    let first = pos(&log, "install:1:99.0.0");
    let failed = pos(&log, "update_status:failed");
    let started = pos(&log, "volume_started:v1");
    assert!(first < failed && failed < started, "{log:?}");
    // The retry drains: the running volume finishes and is uploaded BEFORE the install.
    let waiting = log
        .iter()
        .rposition(|e| e == "update_status:waiting")
        .unwrap();
    let done = pos(&log, "volume_done:v1");
    let second = pos(&log, "install:2:99.0.0");
    assert!(
        started < waiting,
        "drain begins while the volume runs: {log:?}"
    );
    assert!(
        done < second,
        "the volume finishes before the install: {log:?}"
    );
    assert!(
        log[started..second]
            .iter()
            .any(|e| e == "availability:true"),
        "the drain is announced as a pause: {log:?}"
    );
    assert_eq!(
        log.last().map(String::as_str),
        Some("update_status:restarting")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn older_library_is_never_followed_and_auto_off_only_reports() {
    for (library_version, auto) in [("0.0.1", true), ("99.0.0", false)] {
        let dir = tempfile::tempdir().unwrap();
        let (url, lib) = library(library_version, false).await;
        let mut o = options(&url, dir.path(), auto, 5);
        let calls = Arc::new(FakeInstaller {
            lib: lib.clone(),
            calls: Mutex::new(0),
            fail_first: 0,
        });
        o.installer = Some(calls.clone());
        let shutdown = o.shutdown.clone();
        let task = tokio::spawn(serve(o));
        // It works as usual: the volume runs to the end.
        wait_for(&lib, 15, |l| l.log.iter().any(|e| e == "volume_done:v1")).await;
        shutdown.cancel();
        let ended = task.await.unwrap();
        assert_eq!(ended, Ok(ServeExit::Stopped));
        let log = lib.0.lock().log.clone();
        assert_eq!(*calls.calls.lock(), 0, "nothing installed: {log:?}");
        let expected = if auto {
            "update_status:blocked" // the library is older: reported, never a downgrade
        } else {
            "update_status:off" // auto_update off: reported only
        };
        assert!(
            log.iter().any(|e| e == expected),
            "{library_version}: {log:?}"
        );
        assert!(!log.iter().any(|e| e.starts_with("install")));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_version_says_nothing_about_updates() {
    let dir = tempfile::tempdir().unwrap();
    let (url, lib) = library(OURS, false).await;
    let mut o = options(&url, dir.path(), true, 5);
    o.installer = Some(Arc::new(FakeInstaller {
        lib: lib.clone(),
        calls: Mutex::new(0),
        fail_first: 0,
    }));
    let shutdown = o.shutdown.clone();
    let task = tokio::spawn(serve(o));
    wait_for(&lib, 15, |l| l.log.iter().any(|e| e == "volume_done:v1")).await;
    shutdown.cancel();
    assert_eq!(task.await.unwrap(), Ok(ServeExit::Stopped));
    let l = lib.0.lock();
    assert_eq!(l.mismatches, vec![None]);
    assert!(
        !l.log
            .iter()
            .any(|e| e.starts_with("update_status") || e.starts_with("install"))
    );
}
