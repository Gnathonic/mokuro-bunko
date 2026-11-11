//! `serve` end to end against a minimal fake library: token → register → socket →
//! open_session → volume (archive GET) → result PUT → volume_done → close → exit.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use bunko_processor::config::{LibrarySettings, ProcessorSettings, TlsVerify};
use bunko_processor::{FakeConfig, FakePipeline, ProcessorConfig, ServeError, ServeOptions, serve};
use bunko_proto::{Event, Op, RegisterRequest, VolumeOp};
use parking_lot::Mutex;
use serde_json::json;
use sha2::Digest;
use tokio::sync::Notify;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    RoundTrip,
    /// Close the first socket at the first page of the first volume.
    DropOnFirstPage,
    /// Two sessions one after the other on the same archive (a library taking work by
    /// volume): the first claim says `keep_archive: keep`, the second names the ETag
    /// `second_etag` and, unless `let_go`, the archive among those the processor still
    /// runs (`kept_archives`).
    TwoClaims {
        keep: bool,
        second_etag: &'static str,
        let_go: bool,
    },
}

struct Lib {
    mode: Mode,
    archive: Vec<u8>,
    token_ok: bool,
    upload_status: u16,
    /// Everything that happened, in order: `token`, `register`, `socket`,
    /// `upload:<claim>`, `<socket n>:<event>[:<claim>]`.
    log: Vec<String>,
    events: Vec<(u32, Event)>,
    registers: Vec<RegisterRequest>,
    uploads: HashMap<String, (String, String, Vec<u8>)>,
    sockets: u32,
    /// Archive GETs answered.
    gets: u32,
}

type Shared = Arc<(Mutex<Lib>, Notify)>;

const TOKEN: &str = "tok-1";

fn bearer_ok(headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {TOKEN}"))
}

async fn token(State(lib): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let mut l = lib.0.lock();
    l.log.push("token".into());
    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let request: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    if !l.token_ok
        || basic != "Basic Z3B1OnB3"
        || request["kind"] != "processor"
        || request["label"] != "tower"
    {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": "Invalid credentials"})),
        )
            .into_response();
    }
    axum::Json(json!({"token": TOKEN, "token_type": "Bearer", "kind": "processor", "user": {"username": "gpu", "role": "processor"}}))
        .into_response()
}

async fn register(State(lib): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    if !bearer_ok(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": "Authentication required"})),
        )
            .into_response();
    }
    let request: RegisterRequest = serde_json::from_slice(&body).unwrap();
    if request.protocol != 3 {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": format!("this server speaks protocol 3, not {}", request.protocol), "protocols": [3], "version": "0.7.0-test"})),
        )
            .into_response();
    }
    let mut l = lib.0.lock();
    l.log.push("register".into());
    l.registers.push(request);
    let pid = format!("p{}", l.registers.len());
    axum::Json(json!({
        "protocol": 3,
        "processor_id": pid,
        "socket": format!("/_processor/{pid}/socket"),
        "results": format!("/_processor/{pid}/results/{{sid}}/{{claim}}"),
        "archives": "/mokuro-reader/",
        "version": "0.7.0-test",
    }))
    .into_response()
}

async fn archive(
    State(lib): State<Shared>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !bearer_ok(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if path != "Series A/Vol 1.cbz" {
        return StatusCode::NOT_FOUND.into_response();
    }
    let body = {
        let mut l = lib.0.lock();
        l.gets += 1;
        l.archive.clone()
    };
    (
        [(header::ETAG, "\"a1\""), (header::ACCEPT_RANGES, "bytes")],
        body,
    )
        .into_response()
}

async fn result(
    State(lib): State<Shared>,
    Path((_pid, _sid, claim)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !bearer_ok(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let name = headers
        .get("x-mokuro-sidecar-name")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let sha = headers
        .get("x-mokuro-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mut l = lib.0.lock();
    l.log.push(format!("upload:{claim}"));
    l.uploads.insert(claim, (name, sha, body.to_vec()));
    StatusCode::from_u16(l.upload_status)
        .unwrap()
        .into_response()
}

async fn socket(
    State(lib): State<Shared>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !bearer_ok(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let n = {
        let mut l = lib.0.lock();
        l.sockets += 1;
        l.log.push("socket".into());
        l.sockets
    };
    upgrade.on_upgrade(move |ws| drive(ws, lib, n))
}

fn op(op: &Op) -> Message {
    Message::Text(serde_json::to_string(op).unwrap().into())
}

const ARCHIVE: &str = "/mokuro-reader/Series A/Vol 1.cbz";

fn volume() -> Op {
    volume_of("s1", "v1", "\"a1\"", false, None)
}

fn volume_of(
    sid: &str,
    claim: &str,
    etag: &str,
    keep_archive: bool,
    kept_archives: Option<Vec<String>>,
) -> Op {
    Op::Volume(VolumeOp {
        sid: sid.into(),
        claim: claim.into(),
        archive: ARCHIVE.into(),
        sidecar_name: "Vol 1.mokuro".into(),
        title: "Series A".into(),
        volume_title: "Vol 1".into(),
        title_uuid: Some("tu".into()),
        volume_uuid: Some("vu".into()),
        size: None,
        etag: Some(etag.into()),
        keep_archive,
        kept_archives,
    })
}

/// The library's side of one socket.
async fn drive(mut ws: WebSocket, lib: Shared, n: u32) {
    let mode = lib.0.lock().mode;
    if n == 1 {
        ws.send(op(&Op::OpenSession {
            sid: "s1".into(),
            generation: common::row("fake"),
        }))
        .await
        .unwrap();
    }
    while let Some(Ok(message)) = ws.recv().await {
        let Message::Text(text) = message else {
            continue;
        };
        let event: Event = serde_json::from_str(text.as_str()).unwrap();
        let name = serde_json::to_value(&event).unwrap()["event"]
            .as_str()
            .unwrap()
            .to_string();
        {
            let mut l = lib.0.lock();
            l.log.push(match event.claim() {
                Some(c) => format!("{n}:{name}:{c}"),
                None => format!("{n}:{name}"),
            });
            l.events.push((n, event.clone()));
        }
        lib.1.notify_waiters();
        if n != 1 {
            continue;
        }
        if let Mode::TwoClaims {
            keep,
            second_etag,
            let_go,
        } = mode
        {
            let held = || Some(vec![ARCHIVE.to_string()]);
            match &event {
                Event::Ready { sid, .. } if sid == "s1" => {
                    ws.send(op(&volume_of("s1", "v1", "\"a1\"", keep, held())))
                        .await
                        .unwrap();
                }
                Event::Ready { sid, .. } => {
                    let named = if let_go { Some(Vec::new()) } else { held() };
                    ws.send(op(&volume_of(sid, "v2", second_etag, false, named)))
                        .await
                        .unwrap();
                }
                Event::VolumeDone { sid, .. } | Event::VolumeFailed { sid, .. } => {
                    ws.send(op(&Op::CloseSession { sid: sid.clone() }))
                        .await
                        .unwrap();
                }
                Event::Exit { sid, .. } if sid == "s1" => {
                    ws.send(op(&Op::OpenSession {
                        sid: "s2".into(),
                        generation: common::row("fake"),
                    }))
                    .await
                    .unwrap();
                }
                _ => {}
            }
            continue;
        }
        match (&event, mode) {
            (Event::Ready { .. }, _) => ws.send(op(&volume())).await.unwrap(),
            (Event::Page { .. }, Mode::DropOnFirstPage) => return,
            (Event::VolumeDone { .. } | Event::VolumeFailed { .. }, Mode::RoundTrip) => {
                ws.send(op(&Op::CloseSession { sid: "s1".into() }))
                    .await
                    .unwrap();
            }
            _ => {}
        }
    }
}

async fn library(mode: Mode, token_ok: bool, upload_status: u16) -> (String, Shared) {
    let lib: Shared = Arc::new((
        Mutex::new(Lib {
            mode,
            archive: common::volume(3, 10_000),
            token_ok,
            upload_status,
            log: Vec::new(),
            events: Vec::new(),
            registers: Vec::new(),
            uploads: HashMap::new(),
            sockets: 0,
            gets: 0,
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
    (format!("http://{addr}/"), lib)
}

fn config(url: &str, storage: &std::path::Path) -> ProcessorConfig {
    ProcessorConfig {
        library: LibrarySettings {
            url: url.trim_end_matches('/').into(),
            username: "gpu".into(),
            password: "pw".into(),
            tls_verify: TlsVerify::Yes,
        },
        processor: ProcessorSettings {
            name: "tower".into(),
            public_name: Some("the big box".into()),
            max_sessions: 2,
            storage: storage.to_path_buf(),
            archive_memory_mb: 0,
            auto_update: false,
        },
        update: Default::default(),
    }
}

/// Wait until `done(lib)` holds.
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
            panic!("timed out; log so far: {:?}", lib.0.lock().log);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn register_socket_volume_upload_done_close_exit() {
    let dir = tempfile::tempdir().unwrap();
    let (url, lib) = library(Mode::RoundTrip, true, 200).await;
    let fake = FakePipeline::new(FakeConfig {
        load_delay: Duration::from_millis(50),
        ..Default::default()
    });
    let mut options = ServeOptions::new(config(&url, dir.path()), Arc::new(fake.clone()));
    options.backoff_start = Duration::from_millis(100);
    let shutdown = options.shutdown.clone();
    let task = tokio::spawn(serve(options));

    wait_for(&lib, 15, |l| l.log.iter().any(|e| e == "1:exit")).await;
    let (log, events, registers, uploads) = {
        let l = lib.0.lock();
        (
            l.log.clone(),
            l.events.clone(),
            l.registers.clone(),
            l.uploads.clone(),
        )
    };
    let flow: Vec<&str> = log
        .iter()
        .filter(|e| {
            !e.starts_with("1:page") && !e.starts_with("1:stats") && !e.starts_with("upload")
        })
        .map(String::as_str)
        .collect();
    assert_eq!(
        flow,
        [
            "token",
            "register",
            "socket",
            "1:ready",
            "1:fetch:v1",
            "1:volume_started:v1",
            "1:volume_done:v1",
            "1:exit"
        ]
    );
    let at = |name: &str| log.iter().position(|e| e == name).unwrap();
    assert!(
        at("upload:v1") < at("1:volume_done:v1"),
        "the sidecar is uploaded BEFORE volume_done: {log:?}"
    );

    let request = &registers[0];
    assert_eq!(request.protocol, 3);
    assert_eq!(request.name.as_deref(), Some("tower"));
    assert_eq!(request.public_name.as_deref(), Some("the big box"));
    assert_eq!(request.max_sessions, 2);
    assert_eq!(request.catalog.engines, ["fake"]);
    assert_eq!(
        request.catalog.engine_revisions,
        [("fake".to_string(), 1)].into(),
        "each listed engine's revision is announced"
    );
    assert_eq!(request.host.version, env!("CARGO_PKG_VERSION"));

    let archive = lib.0.lock().archive.clone();
    let (name, sha, body) = &uploads["v1"];
    assert_eq!(
        name, "Vol%201.mokuro",
        "the header carries the percent-encoded name"
    );
    assert_eq!(*sha, hex::encode(sha2::Sha256::digest(body)));
    let sidecar: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert_eq!(sidecar["pages"].as_array().unwrap().len(), 3);
    assert_eq!(sidecar["title"], "Series A");
    assert_eq!(sidecar["ocr_engine"]["revision"], 1);
    for (_, event) in &events {
        match event {
            Event::Fetch { state, detail, .. } => {
                assert_eq!(state, "ready");
                assert_eq!(
                    detail["crc32"],
                    format!("{:08x}", crc32fast::hash(&archive))
                );
                assert_eq!(detail["placement"], "disk");
                assert_eq!(detail["members"], 3);
            }
            Event::VolumeDone {
                sidecar_sha256,
                pages,
                ..
            } => {
                assert_eq!(sidecar_sha256.as_deref(), Some(sha.as_str()));
                assert_eq!(*pages, 3);
            }
            Event::Exit { returncode, .. } => assert_eq!(*returncode, Some(0)),
            _ => {}
        }
    }
    // Workspaces and spooled archives are gone once the claim is over.
    let work = dir.path().join(".processing/work");
    assert!(!work.join("s1").exists());
    assert_eq!(
        std::fs::read_dir(dir.path().join(".processing/archives"))
            .unwrap()
            .count(),
        0
    );
    let status = bunko_processor::status::read_status(dir.path());
    assert_eq!(status["state"], "connected");

    shutdown.cancel();
    let ended = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ended, Ok(bunko_processor::ServeExit::Stopped));
    assert_eq!(
        bunko_processor::status::read_status(dir.path())["state"],
        "stopped"
    );
    assert_eq!(fake.opened(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_upload_turns_volume_done_into_volume_failed() {
    let dir = tempfile::tempdir().unwrap();
    let (url, lib) = library(Mode::RoundTrip, true, 400).await;
    let mut options = ServeOptions::new(
        config(&url, dir.path()),
        Arc::new(FakePipeline::new(FakeConfig::default())),
    );
    options.backoff_start = Duration::from_millis(100);
    let shutdown = options.shutdown.clone();
    let task = tokio::spawn(serve(options));
    wait_for(&lib, 15, |l| l.log.iter().any(|e| e == "1:exit")).await;
    let events = lib.0.lock().events.clone();
    let failed = events.iter().find_map(|(_, e)| match e {
        Event::VolumeFailed { error, .. } => Some(error.clone()),
        _ => None,
    });
    assert_eq!(
        failed.as_deref(),
        Some("the finished sidecar could not be sent from the processor")
    );
    assert!(
        !events
            .iter()
            .any(|(_, e)| matches!(e, Event::VolumeDone { .. }))
    );
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_login_is_final() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _lib) = library(Mode::RoundTrip, false, 200).await;
    let options = ServeOptions::new(
        config(&url, dir.path()),
        Arc::new(FakePipeline::new(FakeConfig::default())),
    );
    let ended = tokio::time::timeout(Duration::from_secs(5), serve(options))
        .await
        .unwrap();
    match ended {
        Err(ServeError::LoginRefused(m)) => assert!(m.contains("401"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        bunko_processor::status::read_status(dir.path())["state"],
        "refused"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_socket_ends_everything_silently_and_registers_again() {
    let dir = tempfile::tempdir().unwrap();
    let (url, lib) = library(Mode::DropOnFirstPage, true, 200).await;
    let fake = FakePipeline::new(FakeConfig {
        page_delay: Duration::from_millis(50),
        ..Default::default()
    });
    let mut options = ServeOptions::new(config(&url, dir.path()), Arc::new(fake));
    options.backoff_start = Duration::from_millis(100);
    let shutdown = options.shutdown.clone();
    let task = tokio::spawn(serve(options));
    wait_for(&lib, 15, |l| l.sockets >= 2).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    {
        let l = lib.0.lock();
        assert_eq!(l.registers.len(), 2, "{:?}", l.log);
        let later: Vec<_> = l.events.iter().filter(|(n, _)| *n == 2).collect();
        assert!(
            later.iter().all(|(_, e)| matches!(e, Event::Ping)),
            "nothing of the old session may reach the library: {later:?}"
        );
        assert!(
            !l.events
                .iter()
                .any(|(_, e)| matches!(e, Event::Exit { .. })),
            "no exit: the processor left"
        );
    }
    // A second processor on the same storage is refused.
    let other = ServeOptions::new(
        config(&url, dir.path()),
        Arc::new(FakePipeline::new(FakeConfig::default())),
    );
    assert!(matches!(
        serve(other).await,
        Err(ServeError::StorageLocked(_))
    ));
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

/// The library answers a registration from its OCR scheduler, which works one message at
/// a time; with a large queue it once took longer than the 30 s the processor waited.
/// The processor then gave up, registered again and so queued yet another registration
/// behind the first. A reply that comes after a minute still registers it.
#[tokio::test(start_paused = true)]
async fn a_registration_the_library_answers_after_a_minute_still_registers() {
    async fn slow_register() -> Response {
        tokio::time::sleep(Duration::from_secs(60)).await;
        axum::Json(json!({
            "protocol": 3, "processor_id": "p1", "socket": "/_processor/p1/socket",
            "results": "/_processor/p1/results/{sid}/{claim}", "archives": "/mokuro-reader/",
            "version": env!("CARGO_PKG_VERSION"),
        }))
        .into_response()
    }
    let app = Router::new()
        .route(
            "/login/api/token",
            post(|| async { axum::Json(json!({"token": "t", "token_type": "Bearer"})) }),
        )
        .route("/_processor/register", post(slow_register));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = bunko_processor::client::LibraryClient::new(
        &LibrarySettings {
            url,
            username: "gpu".into(),
            password: "pw".into(),
            tls_verify: TlsVerify::Yes,
        },
        "tower",
    )
    .unwrap();
    let request = RegisterRequest {
        protocol: 3,
        name: Some("tower".into()),
        public_name: None,
        host: Default::default(),
        catalog: Default::default(),
        max_sessions: 1,
        availability: None,
    };
    let started = tokio::time::Instant::now();
    let reply = client.register(&request).await.expect("registered");
    assert_eq!(reply.processor_id, "p1");
    assert!(started.elapsed() >= Duration::from_secs(60));
}

/// Two claims of one archive, the first asking to keep it (by volume): `(GETs, the
/// second claim's fetch-ready detail)`.
async fn two_claims(
    keep: bool,
    second_etag: &'static str,
    let_go: bool,
) -> (u32, serde_json::Value) {
    two_claims_with(keep, second_etag, let_go, FakeConfig::default()).await
}

/// [`two_claims`] over a fake pipeline configured as `fake`.
async fn two_claims_with(
    keep: bool,
    second_etag: &'static str,
    let_go: bool,
    fake: FakeConfig,
) -> (u32, serde_json::Value) {
    let dir = tempfile::tempdir().unwrap();
    let mode = Mode::TwoClaims {
        keep,
        second_etag,
        let_go,
    };
    let (url, lib) = library(mode, true, 200).await;
    let mut options =
        ServeOptions::new(config(&url, dir.path()), Arc::new(FakePipeline::new(fake)));
    options.backoff_start = Duration::from_millis(100);
    let shutdown = options.shutdown.clone();
    let task = tokio::spawn(serve(options));
    wait_for(&lib, 15, |l| {
        l.events
            .iter()
            .any(|(_, e)| matches!(e, Event::Exit { sid, .. } if sid == "s2"))
    })
    .await;
    let (gets, ready, log) = {
        let l = lib.0.lock();
        let ready = l.events.iter().find_map(|(_, e)| match e {
            Event::Fetch {
                id, state, detail, ..
            } if id == "v2" && state == "ready" => Some(serde_json::to_value(detail).unwrap()),
            _ => None,
        });
        (l.gets, ready, l.log.clone())
    };
    assert!(
        log.iter().any(|e| e == "1:volume_done:v2"),
        "the second claim ran: {log:?}"
    );
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    // Nothing kept outlives the connection.
    assert_eq!(
        std::fs::read_dir(dir.path().join(".processing/archives"))
            .unwrap()
            .count(),
        0
    );
    (gets, ready.expect("the second claim's fetch ready"))
}

/// By volume the library asks to keep the archive after a claim: the volume's next
/// claim (another session, another row) runs on the kept copy, downloaded once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kept_archive_serves_the_next_claim_of_its_volume() {
    let (gets, ready) = two_claims(true, "\"a1\"", false).await;
    assert_eq!(gets, 1, "one download for both claims");
    assert_eq!(ready["reused"], true);
    assert_eq!(ready["requests"], 0);
    assert_eq!(ready["members"], 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_keep_archive_each_claim_downloads() {
    let (gets, ready) = two_claims(false, "\"a1\"", false).await;
    assert_eq!(gets, 2);
    assert!(ready.get("reused").is_none(), "{ready}");
}

/// The library names another ETag for the second claim (the archive was replaced):
/// the kept copy is not used.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kept_archive_of_another_version_is_downloaded_again() {
    let (gets, ready) = two_claims(true, "\"a2\"", false).await;
    assert_eq!(gets, 2);
    assert!(ready.get("reused").is_none(), "{ready}");
}

/// The library stopped naming the archive among those it runs on this processor (it
/// let the volume go): the kept copy is dropped, and a later claim downloads again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kept_archive_the_library_lets_go_is_dropped() {
    let (gets, ready) = two_claims(true, "\"a1\"", true).await;
    assert_eq!(gets, 2);
    assert!(ready.get("reused").is_none(), "{ready}");
}

/// Finding: a claim's copy was kept whenever the library asked, even when its run
/// failed or was cancelled (a late worker could then add a copy after the connection
/// cleared its copies). Only a run that completed keeps its copy: after a failed one
/// the next claim downloads the archive again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_run_keeps_no_copy() {
    let fake = FakeConfig {
        fail_volumes: ["v1".to_string()].into_iter().collect(),
        ..FakeConfig::default()
    };
    let (gets, ready) = two_claims_with(true, "\"a1\"", false, fake).await;
    assert_eq!(gets, 2, "the failed claim's copy was not kept");
    assert!(ready.get("reused").is_none(), "{ready}");
}
