//! The OCR module over HTTP: the queue page status (levels, ETag/304, viewers), the
//! reader's queue file, and a whole remote round trip over a real WebSocket.

mod ocr_common;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use bunko_core::{Config, Role};
use bunko_db::{Database, DbOptions, UserStatus};
use bunko_proto::{Event, Op};
use bunko_server::backend::DbAuthBackend;
use bunko_server::core::Core;
use bunko_server::ocr::queue_file::{self, QueueFileState};
use bunko_server::ocr::sched::{Msg, RegisterInput, RegisterOutcome};
use bunko_server::ocr::{DavLocks, FileFacts, OcrControl, OcrDeps};
use futures_util::{SinkExt, StreamExt};
use http::{Request, StatusCode, header};
use parking_lot::RwLock;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

struct Env {
    _dir: tempfile::TempDir,
    db: Arc<Database>,
    core: Core,
    ocr: OcrControl,
    stop: CancellationToken,
}

fn env(configure: impl FnOnce(&mut Config)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.storage.base_path = dir.path().join("storage");
    config.ocr.autobench = false;
    configure(&mut config);
    let layout = config.storage.layout();
    layout.ensure_directories().unwrap();
    let db = Arc::new(
        Database::open_with(
            layout.database(),
            &DbOptions {
                bcrypt_cost: 4,
                ..DbOptions::default()
            },
        )
        .unwrap(),
    );
    let backend = Arc::new(DbAuthBackend::new(db.clone(), layout.clone()));
    let core = Core::new(Arc::new(RwLock::new(config)), None, backend);
    let ocr = OcrControl::new(OcrDeps {
        core: core.clone(),
        db: Some(db.clone()),
        facts: Arc::new(FileFacts),
        locks: Arc::new(DavLocks(bunko_dav::PathWriteLocks::new())),
        local: None,
        clock: None,
    });
    let stop = CancellationToken::new();
    ocr.start(stop.clone());
    Env {
        _dir: dir,
        db,
        core,
        ocr,
        stop,
    }
}

fn basic(user: &str, pass: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
    )
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, http::HeaderMap, Vec<u8>) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 24)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

/// A fake processor registered straight with the scheduler.
async fn fake_processor(ocr: &OcrControl, name: &str) -> tokio::sync::mpsc::UnboundedReceiver<Op> {
    let (reply, rx) = tokio::sync::oneshot::channel();
    let body = json!({"protocol": 3, "name": name, "catalog": {"engines": ["hayai-nova"], "detectors": ["ppocr-manga"], "devices": []}, "max_sessions": 1});
    ocr.send(Msg::Register {
        input: RegisterInput {
            username: format!("acct-{name}"),
            body,
            account_stamp: None,
        },
        reply,
    });
    let RegisterOutcome::Ok(r) = rx.await.unwrap() else {
        panic!("refused")
    };
    let (tx, ops) = tokio::sync::mpsc::unbounded_channel();
    let (reply, rx) = tokio::sync::oneshot::channel();
    ocr.send(Msg::SocketOpen {
        pid: r.processor_id,
        username: format!("acct-{name}"),
        ops: tx,
        reply,
    });
    rx.await.unwrap().unwrap();
    ops
}

#[tokio::test]
async fn queue_status_levels_etag_and_viewers() {
    let e = env(|_| {});
    e.db.create_user(
        "boss",
        "boss-password-1",
        Role::Admin,
        UserStatus::Active,
        "",
    )
    .unwrap();
    let lib = e.core.layout.library();
    ocr_common::write_cbz(&lib.join("Series/V1.cbz"), 3);
    ocr_common::write_cbz(&lib.join("Series/V2.cbz"), 3);
    e.ocr.archive_arrived(&lib.join("Series/V1.cbz"));
    e.ocr.archive_arrived(&lib.join("Series/V2.cbz"));
    let app = bunko_server::ocr::queue_router(e.ocr.clone());

    // Nothing connected: held for want of a processor.
    let (status, headers, body) = send(
        &app,
        Request::get("/queue/api/status")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["level"], "normal");
    assert_eq!(v["processing_hold"]["reason"], "no-processor");
    assert_eq!(v["pending_count"], 2);
    for key in [
        "machines",
        "pending",
        "failed",
        "failed_count",
        "skipped_missing_pages",
        "generations",
        "pending_thumbnails",
        "queue_done_at",
        "paused_for_benchmark",
    ] {
        assert!(v.get(key).is_some(), "missing {key}: {v}");
    }
    assert!(
        v.get("backend").is_none(),
        "a visitor never sees the backend"
    );
    assert!(v.get("held_rows").is_none());
    assert_eq!(v["pending"][0]["series"], "Series");
    assert_eq!(v["pending"][0]["volume"], "V1");
    assert_eq!(headers["cache-control"], "private, no-cache");
    assert_eq!(headers["vary"], "Authorization");
    let etag = headers["etag"].to_str().unwrap().to_string();
    assert!(
        etag.starts_with("\"normal-v-") && etag.len() == "\"normal-v-\"".len() + 20,
        "{etag}"
    );
    // The body is Python's `json.dumps(sort_keys=True)`.
    let text = String::from_utf8(body.clone()).unwrap();
    assert!(text.starts_with("{\"failed\": []"), "{text}");

    // Same version: 304, no body.
    let req = Request::get("/queue/api/status")
        .header(header::IF_NONE_MATCH, etag.clone())
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = send(&app, req).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
    assert_eq!(headers["etag"].to_str().unwrap(), etag);

    // A processor connects: a machine card, and the hold lifts.
    let mut ops = fake_processor(&e.ocr, "tower").await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (_, _, body) = send(
        &app,
        Request::get("/queue/api/status")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(v["processing_hold"].is_null(), "{v}");
    assert_eq!(
        v["machines"][0]["name"], "machine 1",
        "a visitor sees an alias: {v}"
    );
    assert_eq!(v["machines"][0]["jobs"].as_array().unwrap().len(), 1, "{v}");
    assert!(matches!(ops.recv().await, Some(Op::OpenSession { .. })));

    // An admin sees real names and the admin-only fields.
    let req = Request::get("/queue/api/status")
        .header(header::AUTHORIZATION, basic("boss", "boss-password-1"))
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = send(&app, req).await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["machines"][0]["name"], "tower");
    assert!(v.get("backend").is_some() && v.get("held_rows").is_some());
    assert!(headers["etag"].to_str().unwrap().starts_with("\"normal-a-"));

    // A bad password: the visitor body, flagged.
    let req = Request::get("/queue/api/status")
        .header(header::AUTHORIZATION, basic("boss", "wrong-password"))
        .body(Body::empty())
        .unwrap();
    let (status, headers, _) = send(&app, req).await;
    assert_eq!(status, 200);
    assert_eq!(headers["x-queue-auth"], "failed");

    // Minimal level: compact machines, the first pending items only.
    e.core.config.write().queue.display = "minimal".into();
    let (_, _, body) = send(
        &app,
        Request::get("/queue/api/status")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["level"], "minimal");
    assert!(v.get("failed").is_none());
    let job = &v["machines"][0]["jobs"][0];
    assert_eq!(
        job.as_object().unwrap().keys().cloned().collect::<Vec<_>>(),
        vec![
            "eta_at",
            "generation",
            "percent",
            "series",
            "state",
            "volume"
        ]
    );

    // Private queue: a visitor is refused.
    e.core.config.write().queue.public_access = false;
    let (status, _, body) = send(
        &app,
        Request::get("/queue/api/status")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, 401);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"error": "Authentication required"})
    );
    let (status, _, body) = send(
        &app,
        Request::get("/queue/api/config")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"show_in_nav": false, "public_access": false, "display": "minimal"})
    );
    e.stop.cancel();
    e.ocr.stop().await;
}

#[tokio::test]
async fn queue_file_document_etag_gzip_and_refusals() {
    let e = env(|_| {});
    let lib = e.core.layout.library();
    ocr_common::write_cbz(&lib.join("Series A/V1.cbz"), 3);
    e.ocr.archive_arrived(&lib.join("Series A/V1.cbz"));
    let app = Router::new()
        .fallback(|| async { (StatusCode::IM_A_TEAPOT, "dav") })
        .layer(axum::middleware::from_fn_with_state(
            QueueFileState {
                core: e.core.clone(),
                ocr: Some(e.ocr.clone()),
            },
            queue_file::middleware,
        ));
    let get = || {
        Request::get(queue_file::QUEUE_FILE_PATH)
            .body(Body::empty())
            .unwrap()
    };
    let (status, headers, body) = send(&app, get()).await;
    assert_eq!(status, 200);
    let doc: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["held"], json!({"reason": "no-processor"}));
    assert_eq!(doc["pending_volumes"], 1);
    let vol = &doc["volumes"][0];
    assert_eq!(vol["path"], "/mokuro-reader/Series%20A/V1.cbz");
    assert_eq!(
        vol["manifest"],
        "/catalog/api/manifest?series=Series%20A&volume=V1"
    );
    assert_eq!(
        vol["jobs"][0],
        json!({"kind": "ocr", "id": "hayai-nova", "state": "held", "eta": null, "progress": null})
    );
    let etag = headers["etag"].to_str().unwrap().to_string();
    assert_eq!(etag.len(), 34, "{etag}");
    assert_eq!(headers["cache-control"], "no-cache");
    assert_eq!(headers["vary"], "Accept-Encoding");
    // A rebuild of the same queue keeps the ETag (generated_at is outside it).
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (_, headers2, _) = send(&app, get()).await;
    assert_eq!(headers2["etag"].to_str().unwrap(), etag);
    let req = Request::get(queue_file::QUEUE_FILE_PATH)
        .header(header::IF_NONE_MATCH, etag.clone())
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = send(&app, req).await;
    assert_eq!(status, 304);
    assert!(body.is_empty());
    // The gzip representation has its own tag.
    let req = Request::get(queue_file::QUEUE_FILE_PATH)
        .header(header::ACCEPT_ENCODING, "gzip, br")
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = send(&app, req).await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-encoding"], "gzip");
    assert_eq!(
        headers["etag"].to_str().unwrap(),
        format!("{}-gz\"", &etag[..etag.len() - 1])
    );
    let mut text = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&body[..]), &mut text).unwrap();
    assert!(text.starts_with("{\"version\":1,"));
    // Writes are refused; OPTIONS and other paths pass through.
    let (status, headers, body) = send(
        &app,
        Request::put(queue_file::QUEUE_FILE_PATH)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, 405);
    assert_eq!(headers["allow"], "GET, HEAD, OPTIONS");
    assert_eq!(
        body,
        b"The OCR queue file is generated by the server and cannot be written."
    );
    let mv = Request::builder()
        .method("MOVE")
        .uri("/mokuro-reader/x.json")
        .header("destination", "http://h/mokuro-reader/.mokuro-queue.json")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, mv).await.0, 405);
    let opt = Request::builder()
        .method("OPTIONS")
        .uri(queue_file::QUEUE_FILE_PATH)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, opt).await.0, StatusCode::IM_A_TEAPOT);
    e.stop.cancel();
    e.ocr.stop().await;
}

#[test]
fn queue_file_eta_hysteresis() {
    let doc = |eta: &str| {
        queue_file::build_document(
            None,
            vec![
                json!({"series": "S", "volume": "V", "jobs": [{"kind": "ocr", "id": "main", "state": "queued", "eta": eta, "progress": null}]}),
            ],
            1,
            1_000_000.0,
        )
    };
    let published = doc("2026-10-01T12:00:00Z");
    let mut close = doc("2026-10-01T12:00:45Z");
    queue_file::keep_close_etas(&mut close, &published);
    assert_eq!(
        close["volumes"][0]["jobs"][0]["eta"], "2026-10-01T12:00:00Z",
        "within 60 s: the published ETA stays"
    );
    assert_eq!(queue_file::etag(&close), queue_file::etag(&published));
    let mut far = doc("2026-10-01T12:01:30Z");
    queue_file::keep_close_etas(&mut far, &published);
    assert_eq!(far["volumes"][0]["jobs"][0]["eta"], "2026-10-01T12:01:30Z");
    assert_ne!(queue_file::etag(&far), queue_file::etag(&published));
    // generated_at is never part of the tag.
    let mut later = published.clone();
    later.insert("generated_at".into(), json!("2030-01-01T00:00:00Z"));
    assert_eq!(queue_file::etag(&later), queue_file::etag(&published));
}

// --- a whole remote round trip over a real WebSocket ----------------------------------------

async fn ws_send(
    ws: &mut (
             impl SinkExt<
        tokio_tungstenite::tungstenite::Message,
        Error = tokio_tungstenite::tungstenite::Error,
    > + Unpin
         ),
    e: &Event,
) {
    let text = serde_json::to_string(e).unwrap();
    ws.send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
        .await
        .unwrap();
}

async fn next_op(
    ws: &mut (
             impl StreamExt<
        Item = Result<
            tokio_tungstenite::tungstenite::Message,
            tokio_tungstenite::tungstenite::Error,
        >,
    > + Unpin
         ),
) -> Op {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("an op in time")
            .expect("socket open")
            .unwrap();
        if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
            return serde_json::from_str(t.as_str()).unwrap();
        }
    }
}

#[tokio::test]
async fn remote_round_trip_over_a_websocket() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let e = env(|_| {});
    e.db.create_user(
        "tower-acct",
        "processor-pass-1",
        Role::Processor,
        UserStatus::Active,
        "",
    )
    .unwrap();
    e.db.create_user(
        "reader",
        "reader-pass-12",
        Role::Registered,
        UserStatus::Active,
        "",
    )
    .unwrap();
    let lib = e.core.layout.library();
    let cbz = lib.join("Series/Vol 1.cbz");
    ocr_common::write_cbz(&cbz, 3);
    e.ocr.archive_arrived(&cbz);

    let app = bunko_server::ocr::processor_router(e.ocr.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{addr}");
    let auth = basic("tower-acct", "processor-pass-1");
    let http = reqwest::Client::new();

    // The setup wizard's probe: protocol 0 is answered with the protocols spoken.
    let probe = http
        .post(format!("{base}/_processor/register"))
        .header("authorization", &auth)
        .json(&json!({"protocol": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(probe.status(), 400);
    let body: Value = probe.json().await.unwrap();
    assert_eq!(body["protocols"], json!([3]));
    assert_eq!(body["error"], "this server speaks protocol 3, not 0");
    // Not a processor account.
    let other = http
        .post(format!("{base}/_processor/register"))
        .header("authorization", basic("reader", "reader-pass-12"))
        .json(&json!({"protocol": 3}))
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 403);

    let reg = http
        .post(format!("{base}/_processor/register"))
        .header("authorization", &auth)
        .json(&json!({"protocol": 3, "name": "tower", "host": {"gpu": "RTX 4090", "version": "0.7.0"}, "catalog": {"engines": ["hayai-nova"], "detectors": ["ppocr-manga"], "devices": [{"id": "gpu:0", "label": "GPU 0"}]}, "max_sessions": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(reg.status(), 200);
    let reg: Value = reg.json().await.unwrap();
    let pid = reg["processor_id"].as_str().unwrap().to_string();
    assert_eq!(reg["socket"], format!("/_processor/{pid}/socket"));
    assert_eq!(
        reg["results"],
        format!("/_processor/{pid}/results/{{sid}}/{{claim}}")
    );
    assert_eq!(reg["archives"], "/mokuro-reader/");

    let mut req = format!("ws://{addr}/_processor/{pid}/socket")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", auth.parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let (mut tx, mut rx) = ws.split();

    let Op::OpenSession { sid, generation } = next_op(&mut rx).await else {
        panic!("expected open_session")
    };
    assert_eq!(generation.engine, "hayai-nova");
    let Op::Volume(vol) = next_op(&mut rx).await else {
        panic!("expected a volume")
    };
    assert_eq!(vol.archive, "/mokuro-reader/Series/Vol 1.cbz");
    assert_eq!(vol.sidecar_name, "Vol 1.mokuro");
    assert_eq!(vol.title, "Series");
    assert_eq!(vol.size, Some(std::fs::metadata(&cbz).unwrap().len()));
    ws_send(
        &mut tx,
        &Event::Ready {
            sid: sid.clone(),
            startup_seconds: 2.5,
            weights: Default::default(),
            stage_workers: Default::default(),
            queue_capacity: Default::default(),
            stage_device: Default::default(),
            pipeline: "detect -> engine".into(),
            precision: None,
        },
    )
    .await;
    let mut detail = std::collections::BTreeMap::new();
    detail.insert("bytes".to_string(), json!(100));
    detail.insert("requests".to_string(), json!(1));
    ws_send(
        &mut tx,
        &Event::Fetch {
            sid: sid.clone(),
            id: vol.claim.clone(),
            state: "ready".into(),
            detail,
        },
    )
    .await;
    ws_send(
        &mut tx,
        &Event::VolumeStarted {
            sid: sid.clone(),
            id: vol.claim.clone(),
            pages: 3,
        },
    )
    .await;
    ws_send(
        &mut tx,
        &Event::Page {
            sid: sid.clone(),
            id: vol.claim.clone(),
            done: 3,
            total: 3,
        },
    )
    .await;

    // The result goes up as a PUT, before its volume_done.
    let sidecar = br#"{"version":"0.2.5","title":"whatever","volume":"whatever","ocr_engine":{"id":"hayai-nova","detector":"ppocr-manga"},"pages":[{"img_path":"001.jpg","blocks":[]},{"img_path":"002.jpg","blocks":[]},{"img_path":"003.jpg","blocks":[]}]}"#;
    use sha2::Digest;
    let sha = hex::encode(sha2::Sha256::digest(sidecar));
    let put_url = format!("{base}/_processor/{pid}/results/{sid}/{}", vol.claim);
    // A wrong hash is refused and nothing is stored.
    let bad = http
        .put(&put_url)
        .header("authorization", &auth)
        .header("x-mokuro-sidecar-name", "Vol 1.mokuro")
        .header("x-mokuro-sha256", "0".repeat(64))
        .body(sidecar.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let put = http
        .put(&put_url)
        .header("authorization", &auth)
        .header("x-mokuro-sidecar-name", "Vol%201.mokuro")
        .header("x-mokuro-sha256", &sha)
        .body(sidecar.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200, "{}", put.text().await.unwrap());
    ws_send(
        &mut tx,
        &Event::VolumeDone {
            sid: sid.clone(),
            id: vol.claim.clone(),
            pages: 3,
            failed_pages: 0,
            seconds: 1.5,
            stats: Value::Null,
            cpu_pressure: None,
            other_cpu: None,
            sidecar_sha256: Some(sha.clone()),
        },
    )
    .await;

    // The session has nothing more: it is closed, and the processor says exit.
    let Op::CloseSession { sid: closed } = next_op(&mut rx).await else {
        panic!("expected close_session")
    };
    assert_eq!(closed, sid);
    ws_send(
        &mut tx,
        &Event::Exit {
            sid: sid.clone(),
            returncode: Some(0),
        },
    )
    .await;

    // Installed beside the archive, normalised, with provenance.
    let installed = lib.join("Series/Vol 1.mokuro");
    for _ in 0..50 {
        if installed.is_file() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let text = std::fs::read_to_string(&installed).expect("sidecar installed beside the archive");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["title"], "Series");
    assert_eq!(v["volume"], "Vol 1");
    assert!(v["volume_uuid"].is_string());
    tokio::time::sleep(Duration::from_millis(200)).await;
    let row =
        e.db.get_ocr_sidecar("Series/Vol 1.mokuro")
            .unwrap()
            .expect("provenance row");
    assert_eq!(row.machine, "tower");
    assert_eq!(row.account.as_deref(), Some("tower-acct"));
    assert_eq!(row.pages, Some(3));
    assert_eq!(row.volume_key, "Series/Vol 1.cbz");
    // The machine's speed was learned under its own key, the run counted in its profile.
    let prof = bunko_server::ocr::profiles::Profiles::new(&e.core.layout.base);
    assert_eq!(prof.row("tower", "g-1", None).unwrap().runs["volumes"], 1);
    let processors = e.ocr.ask(|s| s.processors()).await.unwrap();
    assert_eq!(processors[0]["label"], "tower (RTX 4090)");
    assert_eq!(processors[0]["transfer"]["volumes"], 1);

    // The account is disabled: its processor is cut off, the socket closes.
    e.ocr.drop_account("tower-acct", "the account was disabled");
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.next().await {
                None
                | Some(Err(_))
                | Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => break,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the socket of a dropped processor closes");
    // Registered again: a second socket for the same registration is a ghost (409).
    let reg: Value = http
        .post(format!("{base}/_processor/register"))
        .header("authorization", &auth)
        .json(&json!({"protocol": 3, "name": "tower", "catalog": {"engines": ["hayai-nova"], "detectors": ["ppocr-manga"]}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pid = reg["processor_id"].as_str().unwrap().to_string();
    let mut req1 = format!("ws://{addr}/_processor/{pid}/socket")
        .into_client_request()
        .unwrap();
    req1.headers_mut()
        .insert("authorization", auth.parse().unwrap());
    let (_ws1, _) = tokio_tungstenite::connect_async(req1).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut req2 = format!("ws://{addr}/_processor/{pid}/socket")
        .into_client_request()
        .unwrap();
    req2.headers_mut()
        .insert("authorization", auth.parse().unwrap());
    let err = tokio_tungstenite::connect_async(req2).await.unwrap_err();
    assert!(err.to_string().contains("409"), "{err}");
    e.stop.cancel();
    e.ocr.stop().await;
}

#[tokio::test]
async fn a_closed_socket_returns_the_claims() {
    let e = env(|_| {});
    let lib = e.core.layout.library();
    ocr_common::write_cbz(&lib.join("S/V.cbz"), 2);
    e.ocr.archive_arrived(&lib.join("S/V.cbz"));
    let ops = fake_processor(&e.ocr, "tower").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let claims = e.ocr.ask(|s| s.claims.len()).await.unwrap();
    assert_eq!(claims, 1);
    drop(ops);
    let pid = e
        .ocr
        .ask(|s| s.machines.keys().next().cloned())
        .await
        .unwrap()
        .unwrap();
    e.ocr.send(Msg::Drop {
        pid,
        reason: "the socket closed".into(),
    });
    let (claims, failures, hold) = e
        .ocr
        .ask(|s| (s.claims.len(), s.failures.len(), s.processing_hold()))
        .await
        .unwrap();
    assert_eq!((claims, failures), (0, 0));
    assert_eq!(hold.unwrap()["last"]["name"], "tower");
    e.stop.cancel();
    e.ocr.stop().await;
}

// --- the full build: this server's own in-process processor ------------------------------

struct FakeLocal(bunko_processor::FakePipeline);

impl bunko_server::ocr::LocalProcessorFactory for FakeLocal {
    fn start(
        &self,
        results_dir: &std::path::Path,
    ) -> Result<bunko_server::ocr::LocalChannels, String> {
        use bunko_processor::PagePipeline;
        let info = self.0.describe();
        let link = bunko_processor::LocalProcessor::spawn(
            Arc::new(self.0.clone()),
            bunko_processor::LocalConfig {
                results_dir: results_dir.to_path_buf(),
            },
        );
        let ops = link.ops;
        let events = link.events;
        Ok(bunko_server::ocr::LocalChannels {
            ops,
            events,
            catalog: info.catalog,
            host: info.host,
        })
    }
}

#[tokio::test]
async fn local_lanes_run_the_in_process_processor() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.storage.base_path = dir.path().join("storage");
    config.ocr.autobench = false;
    config.ocr.local_processing = true;
    config.ocr.concurrency = 2;
    let layout = config.storage.layout();
    layout.ensure_directories().unwrap();
    let db = Arc::new(
        Database::open_with(
            layout.database(),
            &DbOptions {
                bcrypt_cost: 4,
                ..DbOptions::default()
            },
        )
        .unwrap(),
    );
    let backend = Arc::new(DbAuthBackend::new(db.clone(), layout.clone()));
    let core = Core::new(Arc::new(RwLock::new(config)), None, backend);
    let fake = bunko_processor::FakePipeline::new(bunko_processor::FakeConfig {
        engines: vec!["hayai-nova".into()],
        page_delay: Duration::from_millis(5),
        overlap: 1,
        ..Default::default()
    });
    let lib = layout.library();
    for v in ["A/V1.cbz", "A/V2.cbz", "B/V1.cbz"] {
        ocr_common::write_cbz(&lib.join(v), 3);
    }
    let ocr = OcrControl::new(OcrDeps {
        core,
        db: Some(db.clone()),
        facts: Arc::new(FileFacts),
        locks: Arc::new(DavLocks(bunko_dav::PathWriteLocks::new())),
        local: Some(Arc::new(FakeLocal(fake.clone()))),
        clock: None,
    });
    let stop = CancellationToken::new();
    ocr.start(stop.clone());
    let want = [
        lib.join("A/V1.mokuro"),
        lib.join("A/V2.mokuro"),
        lib.join("B/V1.mokuro"),
    ];
    for _ in 0..100 {
        if want.iter().all(|p| p.is_file()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for p in &want {
        assert!(p.is_file(), "{} was not installed", p.display());
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let row = db
        .get_ocr_sidecar("A/V1.mokuro")
        .unwrap()
        .expect("provenance");
    assert_eq!(row.machine, "local");
    assert!(row.account.is_none());
    let (lanes, failures, hold) = ocr
        .ask(|s| (s.lanes.len(), s.failures.len(), s.processing_hold()))
        .await
        .unwrap();
    assert_eq!(lanes, 2, "ocr.concurrency local lanes");
    assert_eq!(failures, 0);
    assert!(hold.is_none());
    // Nothing is left behind in the workspace.
    let leftovers: Vec<_> = walk(&dir.path().join("storage/.processing"));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    stop.cancel();
    ocr.stop().await;
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[tokio::test]
async fn put_follow_up_names_the_manifest_and_when_to_look_again() {
    use bunko_dav::DavHooks;
    let e = env(|_| {});
    let lib = e.core.layout.library();
    let cbz = lib.join("My Series/Vol 2.cbz");
    ocr_common::write_cbz(&cbz, 3);
    let _ops = fake_processor(&e.ocr, "tower").await;
    let ocr = e.ocr.clone();
    let c2 = cbz.clone();
    let follow = tokio::task::spawn_blocking(move || {
        DavHooks::archive_arrived(&ocr, &c2);
        DavHooks::put_follow_up(&ocr, &c2, "My Series", "Vol 2")
    })
    .await
    .unwrap()
    .expect("OCR is owed");
    assert_eq!(
        follow.manifest,
        "/catalog/api/manifest?series=My%20Series&volume=Vol%202"
    );
    assert!(
        (30..=3600).contains(&follow.recheck_after),
        "{}",
        follow.recheck_after
    );
    let (pending, _) = e.ocr.volume_outlook(&cbz).await.unwrap();
    assert_eq!(pending[0]["kind"], "ocr");
    assert_eq!(pending[0]["id"], "hayai-nova");
    // A volume with every sidecar owes nothing: no headers.
    std::fs::write(lib.join("My Series/Vol 3.mokuro"), "{}").unwrap();
    ocr_common::write_cbz(&lib.join("My Series/Vol 3.cbz"), 3);
    let ocr = e.ocr.clone();
    let c3 = lib.join("My Series/Vol 3.cbz");
    let none = tokio::task::spawn_blocking(move || {
        DavHooks::archive_arrived(&ocr, &c3);
        DavHooks::put_follow_up(&ocr, &c3, "My Series", "Vol 3")
    })
    .await
    .unwrap();
    assert!(none.is_none());
    let health = tokio::task::spawn_blocking({
        let o = e.ocr.clone();
        move || o.health()
    })
    .await
    .unwrap();
    assert_eq!(health["worker_alive"], true);
    assert_eq!(health["failed"], 0);
    e.stop.cancel();
    e.ocr.stop().await;
}

#[tokio::test]
async fn bench_sample_is_served_with_ranges_to_its_processor_only() {
    let e = env(|_| {});
    e.db.create_user(
        "acct-bench",
        "processor-pass-1",
        Role::Processor,
        UserStatus::Active,
        "",
    )
    .unwrap();
    e.db.create_user(
        "acct-other",
        "processor-pass-2",
        Role::Processor,
        UserStatus::Active,
        "",
    )
    .unwrap();
    let lib = e.core.layout.library();
    ocr_common::write_cbz(&lib.join("S/V1.cbz"), 12);
    let mut ops = fake_processor(&e.ocr, "bench").await;
    let req = bunko_server::ocr::BenchRequest {
        key: "g-1".into(),
        processor: "bench".into(),
        pages: Some(json!(8)),
        ..Default::default()
    };
    let queued = e
        .ocr
        .ask(move |s| s.bench_enqueue(req))
        .await
        .unwrap()
        .expect("queued");
    assert_eq!(queued["sample"]["pages"], 8);
    let bench = loop {
        match tokio::time::timeout(Duration::from_secs(5), ops.recv())
            .await
            .unwrap()
            .unwrap()
        {
            Op::Bench(b) => break b,
            _ => continue,
        }
    };
    assert_eq!(bench.pages, 8);
    let app = bunko_server::ocr::processor_router(e.ocr.clone());
    let req = |auth: &str, range: Option<&str>, method: &str| {
        let mut r = Request::builder()
            .method(method)
            .uri(bench.sample.clone())
            .header(header::AUTHORIZATION, auth);
        if let Some(range) = range {
            r = r.header(header::RANGE, range);
        }
        r.body(Body::empty()).unwrap()
    };
    let own = basic("acct-bench", "processor-pass-1");
    let (status, headers, body) = send(&app, req(&own, None, "GET")).await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "application/vnd.comicbook+zip");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(&body[..2], b"PK");
    let size = body.len();
    let (status, headers, part) = send(&app, req(&own, Some("bytes=0-9"), "GET")).await;
    assert_eq!(status, 206);
    assert_eq!(part.len(), 10);
    assert_eq!(
        headers["content-range"].to_str().unwrap(),
        format!("bytes 0-9/{size}")
    );
    assert_eq!(send(&app, req(&own, Some("bytes=a-b"), "GET")).await.0, 400);
    assert_eq!(
        send(&app, req(&own, Some(&format!("bytes={size}-")), "GET"))
            .await
            .0,
        416
    );
    let (status, headers, body) = send(&app, req(&own, None, "HEAD")).await;
    assert_eq!(status, 200);
    assert!(body.is_empty());
    assert_eq!(
        headers["content-length"].to_str().unwrap(),
        size.to_string()
    );
    // Another processor account gets nothing.
    let other = basic("acct-other", "processor-pass-2");
    assert_eq!(send(&app, req(&other, None, "GET")).await.0, 404);
    e.stop.cancel();
    e.ocr.stop().await;
}

#[tokio::test]
async fn the_admin_panel_reads_the_scheduler() {
    use bunko_server::admin::OcrAdmin;
    let e = env(|_| {});
    let lib = e.core.layout.library();
    ocr_common::write_cbz(&lib.join("S/V1.cbz"), 3);
    e.ocr.archive_arrived(&lib.join("S/V1.cbz"));
    let _ops = fake_processor(&e.ocr, "tower").await;
    let ocr = e.ocr.clone();
    let config = e.core.config.read().clone();
    let (procs, payload, stats, census, outcome) = tokio::task::spawn_blocking(move || {
        let procs = ocr.processors(&config);
        let stats = ocr.generation_stats(&config);
        let payload = ocr.generations_payload(&config);
        let census = ocr.other(
            &config,
            &http::Method::GET,
            &["ocr", "upgrade"],
            "",
            &json!({}),
        );
        let outcome = OcrAdmin::apply(&ocr, &config);
        (procs, payload, stats, census, outcome)
    })
    .await
    .unwrap();
    assert_eq!(procs["processors"][0]["name"], "tower");
    assert_eq!(procs["processors"][0]["local"], false);
    assert!(procs["processing_hold"].is_null());
    assert_eq!(stats["generations"]["g-1"]["volumes_total"], 1);
    assert_eq!(stats["generations"]["g-1"]["volumes_done"], 0);
    assert_eq!(payload["generations"][0]["id"], "g-1");
    assert_eq!(payload["generations"][0]["volumes_total"], 1);
    assert_eq!(payload["processors"][0]["name"], "tower");
    assert!(payload["catalog"]["engines"].is_array());
    let census = census.unwrap().unwrap();
    assert_eq!(census.0, 200);
    assert_eq!(census.1["enabled"], false);
    let outcome = outcome.unwrap();
    assert_eq!(outcome["applied"], true);
    e.stop.cancel();
    e.ocr.stop().await;
}

/// Regression (review finding): a processor socket accepted 64 MiB messages (a 60 MB
/// fragmented message was buffered whole) and binary frames. Messages and frames are now
/// capped at 1 MiB and a binary message closes the socket with 1008 (policy).
#[tokio::test]
async fn oversized_and_binary_messages_close_the_socket() {
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let e = env(|_| {});
    e.db.create_user(
        "tower-acct",
        "processor-pass-1",
        Role::Processor,
        UserStatus::Active,
        "",
    )
    .unwrap();
    let app = bunko_server::ocr::processor_router(e.ocr.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let auth = basic("tower-acct", "processor-pass-1");
    let http = reqwest::Client::new();
    let connect = || async {
        let reg: Value = http
            .post(format!("http://{addr}/_processor/register"))
            .header("authorization", &auth)
            .json(&json!({"protocol": 3, "name": "tower", "catalog": {"engines": ["hayai-nova"], "detectors": ["ppocr-manga"]}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let pid = reg["processor_id"].as_str().unwrap().to_string();
        let mut req = format!("ws://{addr}/_processor/{pid}/socket")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", auth.parse().unwrap());
        tokio_tungstenite::connect_async(req).await.unwrap().0
    };
    // Waits for the server to end the socket; the close frame it sent, if any.
    async fn closed_by_server(
        rx: &mut (
                 impl futures_util::Stream<
            Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>,
        > + Unpin
             ),
    ) -> Option<u16> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match rx.next().await {
                    Some(Ok(WsMessage::Close(frame))) => return frame.map(|f| u16::from(f.code)),
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => return None,
                }
            }
        })
        .await
        .expect("the server closes the socket")
    }

    // A legitimate event well under the cap is read.
    let ws = connect().await;
    let (mut tx, mut rx) = ws.split();
    let stats = json!({"event": "stats", "sid": "nope", "pipeline": {"pad": "x".repeat(200_000)}});
    tx.send(WsMessage::Text(stats.to_string().into()))
        .await
        .unwrap();
    // Over 1 MiB: refused while it arrives, the socket ends.
    let big = format!(
        r#"{{"event":"stats","sid":"x","pipeline":"{}"}}"#,
        "0".repeat(2 << 20)
    );
    let _ = tx.send(WsMessage::Text(big.into())).await;
    let _ = closed_by_server(&mut rx).await;

    // A binary message: closed with 1008.
    let ws = connect().await;
    let (mut tx, mut rx) = ws.split();
    tx.send(WsMessage::Binary(vec![0u8; 16].into()))
        .await
        .unwrap();
    assert_eq!(closed_by_server(&mut rx).await, Some(1008));
    e.stop.cancel();
    e.ocr.stop().await;
}
