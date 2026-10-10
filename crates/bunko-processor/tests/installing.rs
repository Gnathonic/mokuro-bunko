//! A processor whose OCR backend installs in the background (`ServeOptions::install`):
//! it registers at once as not available (reason `installing`, with the progress), gives
//! back any work offered meanwhile, tells the library how far the install is, and when
//! the install is done registers again and takes work. A failed install makes it
//! available again with what it has.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bunko_processor::config::{LibrarySettings, ProcessorSettings, TlsVerify};
use bunko_processor::{FakeConfig, FakePipeline, ProcessorConfig, ServeOptions, serve};
use bunko_proto::{Availability, Event, OcrInstall, Op, RegisterRequest};
use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::{Notify, watch};

#[derive(Default)]
struct Lib {
    /// `register:<reason or ->`, `socket`, `<event>`, `availability:<paused>:<reason>:<stage>:<percent>`.
    log: Vec<String>,
    registrations: Vec<Option<Availability>>,
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
    let reason = request
        .availability
        .as_ref()
        .and_then(|a| a.reason.clone())
        .unwrap_or_else(|| "-".into());
    lib.0
        .lock()
        .registrations
        .push(request.availability.clone());
    push(&lib, format!("register:{reason}"));
    axum::Json(json!({
        "protocol": 3, "processor_id": "p1", "socket": "/_processor/p1/socket",
        "results": "/_processor/p1/results/{sid}/{claim}", "archives": "/mokuro-reader/",
        "version": env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

async fn socket(State(lib): State<Shared>, upgrade: WebSocketUpgrade) -> Response {
    push(&lib, "socket".into());
    upgrade.on_upgrade(move |ws| drive(ws, lib))
}

/// Offer a session at once (as a library that missed the `availability` would).
async fn drive(mut ws: WebSocket, lib: Shared) {
    let open = Op::OpenSession {
        sid: format!("s{}", lib.0.lock().registrations.len()),
        generation: common::row("fake"),
    };
    ws.send(Message::Text(serde_json::to_string(&open).unwrap().into()))
        .await
        .unwrap();
    while let Some(Ok(message)) = ws.recv().await {
        let Message::Text(text) = message else {
            continue;
        };
        let event: Event = serde_json::from_str(text.as_str()).unwrap();
        let entry = match &event {
            Event::Ping | Event::Stats { .. } => continue,
            Event::Availability(a) => format!(
                "availability:{}:{}:{}:{}",
                a.paused,
                a.reason.as_deref().unwrap_or("-"),
                a.install.as_ref().map(|i| i.stage.as_str()).unwrap_or("-"),
                a.install
                    .as_ref()
                    .and_then(|i| i.percent)
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".into())
            ),
            other => {
                let name = serde_json::to_value(other).unwrap()["event"]
                    .as_str()
                    .unwrap()
                    .to_string();
                match other.sid() {
                    Some(sid) => format!("{name}:{sid}"),
                    None => name,
                }
            }
        };
        push(&lib, entry);
    }
}

async fn library() -> (String, Shared) {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .try_init();
    let lib: Shared = Arc::new((Mutex::new(Lib::default()), Notify::new()));
    let app = Router::new()
        .route("/login/api/token", post(token))
        .route("/_processor/register", post(register))
        .route("/_processor/{pid}/socket", get(socket))
        .with_state(lib.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), lib)
}

fn options(url: &str, storage: &std::path::Path) -> ServeOptions {
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
            auto_update: false,
        },
        update: Default::default(),
    };
    let fake = FakePipeline::new(FakeConfig {
        load_delay: Duration::from_millis(20),
        ..Default::default()
    });
    let mut o = ServeOptions::new(config, Arc::new(fake));
    o.backoff_start = Duration::from_millis(100);
    o
}

async fn wait_for(lib: &Shared, secs: u64, done: impl Fn(&[String]) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let notified = lib.1.notified();
        if done(&lib.0.lock().log) {
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
            panic!("timed out; log so far: {:?}", lib.0.lock().log);
        }
    }
}

fn running(stage: &str, percent: Option<u32>) -> Option<OcrInstall> {
    Some(OcrInstall {
        state: OcrInstall::RUNNING.into(),
        stage: stage.into(),
        percent,
        variant: Some("cpu".into()),
        ..OcrInstall::default()
    })
}

fn has(log: &[String], entry: &str) -> bool {
    log.iter().any(|e| e == entry)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registers_at_once_takes_no_work_while_installing_then_registers_again() {
    let dir = tempfile::tempdir().unwrap();
    let (url, lib) = library().await;
    let (tx, rx) = watch::channel(running("downloading", Some(3)));
    let mut o = options(&url, dir.path());
    o.install = Some(rx);
    let stop = o.shutdown.clone();
    let task = tokio::spawn(serve(o));

    // Registered at once, as not available, with the install's progress.
    wait_for(&lib, 10, |l| has(l, "register:installing")).await;
    let first = lib.0.lock().registrations[0].clone().unwrap();
    assert!(first.paused && first.is_installing());
    assert_eq!(first.install.as_ref().unwrap().percent, Some(3));
    // The session offered meanwhile is not opened (it exits without `ready`).
    wait_for(&lib, 10, |l| has(l, "exit:s1")).await;
    assert!(!lib.0.lock().log.iter().any(|e| e.starts_with("ready")));

    // Progress reaches the library (a new stage at once).
    tx.send_replace(running("models", Some(50)));
    wait_for(&lib, 10, |l| {
        has(l, "availability:true:installing:models:50")
    })
    .await;

    // Done: it registers again, available, and takes the session it is offered.
    tx.send_replace(Some(OcrInstall {
        state: OcrInstall::DONE.into(),
        stage: "done".into(),
        ..OcrInstall::default()
    }));
    wait_for(&lib, 10, |l| has(l, "register:-") && has(l, "ready:s2")).await;
    let log = lib.0.lock().log.clone();
    let again = log.iter().position(|e| e == "register:-").unwrap();
    let exit = log.iter().position(|e| e == "exit:s1").unwrap();
    assert!(exit < again, "{log:?}");
    assert_eq!(lib.0.lock().registrations[1], None);
    stop.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(20), task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_install_makes_it_available_with_what_it_has() {
    let dir = tempfile::tempdir().unwrap();
    let (url, lib) = library().await;
    let (tx, rx) = watch::channel(running("downloading", None));
    let mut o = options(&url, dir.path());
    o.install = Some(rx);
    let stop = o.shutdown.clone();
    let task = tokio::spawn(serve(o));
    wait_for(&lib, 10, |l| {
        has(l, "register:installing") && has(l, "exit:s1")
    })
    .await;
    tx.send_replace(Some(OcrInstall {
        state: OcrInstall::FAILED.into(),
        stage: "failed".into(),
        message: Some("the download failed".into()),
        ..OcrInstall::default()
    }));
    // Available again on the same connection (no new registration).
    wait_for(&lib, 10, |l| has(l, "availability:false:-:-:-")).await;
    assert_eq!(
        lib.0
            .lock()
            .log
            .iter()
            .filter(|e| e.starts_with("register"))
            .count(),
        1
    );
    stop.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(20), task).await;
}

/// No install: nothing changes (no availability at registration).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_an_install_it_registers_available() {
    let dir = tempfile::tempdir().unwrap();
    let (url, lib) = library().await;
    let (_tx, rx) = watch::channel(None);
    let mut o = options(&url, dir.path());
    o.install = Some(rx);
    let stop = o.shutdown.clone();
    let task = tokio::spawn(serve(o));
    wait_for(&lib, 10, |l| has(l, "register:-") && has(l, "ready:s1")).await;
    stop.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(20), task).await;
}
