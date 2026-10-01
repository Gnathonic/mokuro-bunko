//! Processor protocol v3, library side (docs/rust-port/PROTOCOL.md; spec
//! remote-processors §2–§5): `POST /_processor/register`, the WebSocket
//! `GET /_processor/{pid}/socket`, result uploads
//! `PUT /_processor/{pid}/results/{sid}/{claim}` and the benchmark sample
//! `GET|HEAD /_processor/{pid}/bench/{bid}/sample`.
//!
//! The socket task owns liveness: a WebSocket ping every 15 s, "gone" after 30 s with no
//! frame at all, and the account re-check every 15 s. Any of those, or the socket
//! closing, is `registry.drop` (claims go back unrecorded).

use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRef, Path, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use bunko_proto::{ACCOUNT_RECHECK_SECONDS, Event, HEADER_RESULT_NAME, HEADER_RESULT_SHA256, HEARTBEAT_SECONDS, MAX_REGISTER_BODY_BYTES, MAX_RESULT_BYTES, Op, SILENCE_SECONDS, valid_id};
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use serde_json::{Value, json};
use sha2::Digest;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

use super::OcrControl;
use super::sched::{Msg, RegisterInput, RegisterOutcome};
use crate::core::{Core, RequestCtx};

impl FromRef<OcrControl> for Core {
    fn from_ref(o: &OcrControl) -> Core {
        o.core().clone()
    }
}

pub fn json_response(status: u16, body: &Value) -> Response {
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

fn error(status: u16, message: impl Into<String>) -> Response {
    json_response(status, &json!({"error": message.into()}))
}

fn refused(status: u16, message: impl Into<String>, code: &str) -> Response {
    json_response(status, &json!({"error": message.into(), "code": code}))
}

pub fn router(ocr: OcrControl) -> Router {
    Router::new()
        .route("/_processor/register", post(register))
        .route("/_processor/{pid}/socket", get(socket))
        .route("/_processor/{pid}/results/{sid}/{claim}", put(result_upload))
        .route("/_processor/{pid}/bench/{bid}/sample", get(bench_sample).head(bench_sample))
        .route("/_processor", any(unknown))
        .route("/_processor/", any(unknown))
        .route("/_processor/{pid}", any(unknown))
        .with_state(ocr)
}

async fn unknown() -> Response {
    error(404, "No such processor endpoint")
}

/// The second lock (after the auth gate): a processor account, by name.
#[allow(clippy::result_large_err)]
fn processor_of(ocr: &OcrControl, ctx: &RequestCtx) -> Result<String, Response> {
    let id = &ctx.identity;
    if let Some(err) = &id.error
        && !id.authenticated()
    {
        if let Some(u) = &id.attempted_username {
            ocr.record_failed_login(u, &format!("invalid credentials from {}", ctx.client_ip));
        }
        let status = if err.contains("Too many failed attempts") { 429 } else { 401 };
        return Err(error(status, err.clone()));
    }
    match &id.user {
        None => Err(error(401, "Authentication required")),
        Some(u) if u.role == bunko_core::Role::Processor && !u.username.is_empty() => Ok(u.username.clone()),
        Some(_) => Err(error(403, "Processor access required")),
    }
}

async fn stamp_of_account(ocr: &OcrControl, username: &str) -> Option<String> {
    let db = ocr.db()?.clone();
    let u = username.to_string();
    tokio::task::spawn_blocking(move || db.processor_account_stamp(&u).ok().flatten()).await.ok().flatten()
}

// --- register -------------------------------------------------------------------------------

async fn register(State(ocr): State<OcrControl>, ctx: RequestCtx, req: Request) -> Response {
    let username = match processor_of(&ocr, &ctx) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if let Some(len) = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()) {
        match len.trim().parse::<usize>() {
            Ok(n) if n > MAX_REGISTER_BODY_BYTES => return error(400, "registration body too large"),
            Ok(_) => {}
            Err(_) => return error(400, "invalid Content-Length"),
        }
    }
    let body = match axum::body::to_bytes(req.into_body(), MAX_REGISTER_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return error(400, "registration body too large"),
    };
    if body.is_empty() {
        return error(400, "a registration needs a body");
    }
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return error(400, "registration body is not readable JSON"),
    };
    if !value.is_object() {
        return error(400, "registration body is not an object");
    }
    let account_stamp = stamp_of_account(&ocr, &username).await;
    let (reply, rx) = oneshot::channel();
    if !ocr.send(Msg::Register { input: RegisterInput { username, body: value, account_stamp }, reply }) {
        return error(503, "OCR is not running");
    }
    match rx.await {
        Ok(RegisterOutcome::Ok(reply)) => json_response(200, &serde_json::to_value(reply).unwrap_or_default()),
        Ok(RegisterOutcome::Refused { status, body }) => json_response(status, &body),
        Err(_) => error(503, "OCR is not running"),
    }
}

// --- the socket ---------------------------------------------------------------------------

/// What the socket needs to know before upgrading.
enum Precheck {
    Ok(Option<String>),
    Refused(u16, &'static str),
    Ghost,
}

async fn socket(State(ocr): State<OcrControl>, ctx: RequestCtx, Path(pid): Path<String>, ws: WebSocketUpgrade) -> Response {
    let username = match processor_of(&ocr, &ctx) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let user2 = username.clone();
    let pid2 = pid.clone();
    let check = ocr
        .ask(move |s| match s.machines.get(&pid2) {
            None => Precheck::Refused(404, "No such processor"),
            Some(m) if m.local || m.username.as_deref() != Some(user2.as_str()) => Precheck::Refused(403, "Not your processor"),
            Some(m) if m.connected() => {
                s.drop_processor(&pid2, "a second socket was opened");
                Precheck::Ghost
            }
            Some(m) => Precheck::Ok(m.account_stamp.clone()),
        })
        .await;
    let stamp = match check {
        None => return error(503, "OCR is not running"),
        Some(Precheck::Refused(status, msg)) => return error(status, msg),
        Some(Precheck::Ghost) => return error(409, "Socket already open; register again"),
        Some(Precheck::Ok(stamp)) => stamp,
    };
    ws.on_upgrade(move |socket| run_socket(ocr, pid, username, stamp, socket))
}

async fn run_socket(ocr: OcrControl, pid: String, username: String, stamp: Option<String>, socket: WebSocket) {
    let (ops_tx, mut ops_rx) = mpsc::unbounded_channel::<Op>();
    let (reply, rx) = oneshot::channel();
    if !ocr.send(Msg::SocketOpen { pid: pid.clone(), username: username.clone(), ops: ops_tx, reply }) {
        return;
    }
    if !matches!(rx.await, Ok(Ok(()))) {
        return;
    }
    let (mut tx, mut rx) = socket.split();
    let mut last_frame = Instant::now();
    let mut last_check = Instant::now();
    let mut ping = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECONDS));
    ping.tick().await;
    let mut watch = tokio::time::interval(Duration::from_secs(1));
    let reason: Option<String> = loop {
        tokio::select! {
            op = ops_rx.recv() => match op {
                Some(op) => {
                    let text = match serde_json::to_string(&op) {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    if tx.send(Message::Text(text.into())).await.is_err() {
                        break Some("the socket closed".into());
                    }
                }
                // The scheduler dropped this registration: its reason stands.
                None => break None,
            },
            frame = rx.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    last_frame = Instant::now();
                    match serde_json::from_str::<Event>(text.as_str()) {
                        Ok(event) => {
                            ocr.send(Msg::Event { pid: pid.clone(), event });
                        }
                        Err(_) => {
                            tracing::warn!("unreadable event from processor {pid}: {:.80}", text.as_str());
                            ocr.send(Msg::Seen { pid: pid.clone() });
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break Some("the socket closed".into()),
                Some(Ok(_)) => {
                    last_frame = Instant::now();
                    ocr.send(Msg::Seen { pid: pid.clone() });
                }
                Some(Err(e)) => break Some(format!("the socket failed: {e}")),
            },
            _ = ping.tick() => {
                if tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break Some("the socket closed".into());
                }
            }
            _ = watch.tick() => {
                let silent = last_frame.elapsed().as_secs();
                if silent >= SILENCE_SECONDS {
                    break Some(format!("it has sent nothing for {silent}s"));
                }
                if last_check.elapsed().as_secs() >= ACCOUNT_RECHECK_SECONDS && ocr.db().is_some() {
                    last_check = Instant::now();
                    let now = stamp_of_account(&ocr, &username).await;
                    match (&now, &stamp) {
                        (None, _) => break Some(format!("the account {username} is no longer an active processor")),
                        (Some(a), Some(b)) if a != b => break Some(format!("the account {username} changed since it registered")),
                        _ => {}
                    }
                }
            }
        }
    };
    if let Some(reason) = reason {
        ocr.send(Msg::Drop { pid: pid.clone(), reason });
    }
    let _ = tx.send(Message::Close(None)).await;
}

// --- result uploads ---------------------------------------------------------------------

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty())
}

/// What an upload is for, checked before a byte is read.
enum UploadCheck {
    Ok(String),
    Refused(u16, &'static str, &'static str),
}

async fn result_upload(State(ocr): State<OcrControl>, ctx: RequestCtx, Path((pid, sid, claim)): Path<(String, String, String)>, req: Request) -> Response {
    let username = match processor_of(&ocr, &ctx) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if !valid_id(&pid) || !valid_id(&sid) || !valid_id(&claim) {
        return refused(404, "No such claim", "unknown");
    }
    let headers = req.headers().clone();
    // Percent-encoded by the processor (a header is ASCII; names often are not).
    let Some(name) = header_str(&headers, HEADER_RESULT_NAME).map(|v| percent_encoding::percent_decode_str(v).decode_utf8_lossy().into_owned()) else {
        return error(400, format!("{HEADER_RESULT_NAME} is required"));
    };
    if name.contains('/') || name.contains('\\') || name.starts_with('.') || !name.ends_with(".mokuro") {
        return error(400, format!("{HEADER_RESULT_NAME} must be a sidecar file name"));
    }
    let Some(sha) = header_str(&headers, HEADER_RESULT_SHA256).map(str::to_ascii_lowercase) else {
        return error(400, format!("{HEADER_RESULT_SHA256} is required"));
    };
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return error(400, format!("{HEADER_RESULT_SHA256} must be a hex sha256"));
    }
    if let Some(len) = header_str(&headers, "content-length").and_then(|v| v.parse::<u64>().ok())
        && len > MAX_RESULT_BYTES
    {
        return error(413, "the result is larger than the library accepts");
    }
    let (p, s, c, u) = (pid.clone(), sid.clone(), claim.clone(), username.clone());
    let check = ocr
        .ask(move |sch| {
            let Some(m) = sch.machines.get(&p) else { return UploadCheck::Refused(404, "No such processor", "unknown") };
            if m.local || m.username.as_deref() != Some(u.as_str()) {
                return UploadCheck::Refused(403, "Not your processor", "not_owner");
            }
            match sch.sessions.get(&s).filter(|x| x.pid == p) {
                Some(session) => match session.jobs.get(&c) {
                    Some(j) => UploadCheck::Ok(j.sidecar_name.clone()),
                    None => UploadCheck::Refused(409, "That claim is not outstanding", "session_ended"),
                },
                None if sch.ended_sessions.contains_key(&s) => UploadCheck::Refused(409, "That session has ended", "session_ended"),
                None => UploadCheck::Refused(404, "No such session", "unknown"),
            }
        })
        .await;
    let expected = match check {
        None => return error(503, "OCR is not running"),
        Some(UploadCheck::Refused(status, msg, code)) => return refused(status, msg, code),
        Some(UploadCheck::Ok(expected)) => expected,
    };
    if expected != name {
        ocr.send(Msg::ResultStored { pid, sid, claim, name, sha256: sha });
        return refused(409, "the sidecar name is not the one the library chose", "rejected");
    }
    let dir = ocr.storage().join(".processing").join(&sid).join(&claim);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return error(507, format!("could not store the result: {e}"));
    }
    let part = dir.join(format!("{name}.part"));
    let final_path = dir.join(&name);
    let mut file = match tokio::fs::File::create(&part).await {
        Ok(f) => f,
        Err(e) => return error(507, format!("could not store the result: {e}")),
    };
    let mut hasher = sha2::Sha256::new();
    let mut received: u64 = 0;
    let mut stream = req.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = tokio::fs::remove_file(&part).await;
                return error(400, format!("the upload was cut short: {e}"));
            }
        };
        received += chunk.len() as u64;
        if received > MAX_RESULT_BYTES {
            let _ = tokio::fs::remove_file(&part).await;
            return error(413, "the result is larger than the library accepts");
        }
        hasher.update(&chunk);
        if let Err(e) = file.write_all(&chunk).await {
            let _ = tokio::fs::remove_file(&part).await;
            return error(507, format!("could not store the result: {e}"));
        }
    }
    if let Err(e) = file.flush().await {
        let _ = tokio::fs::remove_file(&part).await;
        return error(507, format!("could not store the result: {e}"));
    }
    drop(file);
    let actual = hex::encode(hasher.finalize());
    if actual != sha {
        let _ = tokio::fs::remove_file(&part).await;
        return error(400, format!("the body's sha256 is {actual}, not the {HEADER_RESULT_SHA256} sent"));
    }
    if let Err(e) = tokio::fs::rename(&part, &final_path).await {
        return error(507, format!("could not store the result: {e}"));
    }
    ocr.send(Msg::ResultStored { pid, sid, claim, name, sha256: actual.clone() });
    json_response(200, &json!({"received": received, "sha256": actual}))
}

// --- the benchmark sample -----------------------------------------------------------------

/// `Range: bytes=a-b | a- | -n` → `(start, end_inclusive)`. Err(400) unreadable,
/// Err(416) unsatisfiable.
pub fn parse_range(value: &str, size: u64) -> Result<(u64, u64), u16> {
    let spec = value.trim().strip_prefix("bytes=").ok_or(400u16)?;
    if spec.contains(',') {
        return Err(400);
    }
    let (a, b) = spec.split_once('-').ok_or(400u16)?;
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        let n: u64 = b.parse().map_err(|_| 400u16)?;
        if n == 0 || size == 0 {
            return Err(416);
        }
        return Ok((size.saturating_sub(n), size - 1));
    }
    let start: u64 = a.parse().map_err(|_| 400u16)?;
    let end: Option<u64> = if b.is_empty() { None } else { Some(b.parse().map_err(|_| 400u16)?) };
    if end.is_some_and(|e| e < start) {
        return Err(400);
    }
    if start >= size {
        return Err(416);
    }
    let end = end.unwrap_or(size - 1);
    Ok((start, end.min(size - 1)))
}

async fn bench_sample(State(ocr): State<OcrControl>, ctx: RequestCtx, Path((pid, bid)): Path<(String, String)>, method: Method, headers: HeaderMap) -> Response {
    let username = match processor_of(&ocr, &ctx) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let p = pid.clone();
    let found = ocr
        .ask(move |s| {
            let owned = s.machines.get(&p).is_some_and(|m| m.username.as_deref() == Some(username.as_str()));
            if !owned {
                return Err((404, "No such processor"));
            }
            s.bench_sample(&p, &bid)
        })
        .await;
    let path = match found {
        None => return error(503, "OCR is not running"),
        Some(Err((status, msg))) => return error(status, msg),
        Some(Ok(path)) => path,
    };
    let Ok(meta) = tokio::fs::metadata(&path).await else { return error(404, "No such sample") };
    let size = meta.len();
    let (status, start, end) = match headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        None => (StatusCode::OK, 0, size.saturating_sub(1)),
        Some(r) => match parse_range(r, size) {
            Ok((a, b)) => (StatusCode::PARTIAL_CONTENT, a, b),
            Err(400) => return error(400, "bad Range header"),
            Err(_) => {
                let mut resp = error(416, "range not satisfiable");
                resp.headers_mut().insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes */{size}")).unwrap_or(HeaderValue::from_static("bytes */0")));
                return resp;
            }
        },
    };
    let length = if size == 0 { 0 } else { end - start + 1 };
    let body = if method == Method::HEAD || length == 0 {
        Body::empty()
    } else {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let Ok(mut f) = tokio::fs::File::open(&path).await else { return error(404, "No such sample") };
        if f.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return error(500, "could not read the sample");
        }
        Body::from_stream(tokio_util::io::ReaderStream::with_capacity(f.take(length), 256 * 1024))
    };
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/vnd.comicbook+zip"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if status == StatusCode::PARTIAL_CONTENT
        && let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{size}"))
    {
        h.insert(header::CONTENT_RANGE, v);
    }
    resp.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-9", 100), Ok((0, 9)));
        assert_eq!(parse_range("bytes=90-", 100), Ok((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Ok((90, 99)));
        assert_eq!(parse_range("bytes=100-", 100), Err(416));
        assert_eq!(parse_range("items=0-1", 100), Err(400));
        assert_eq!(parse_range("bytes=5-1", 100), Err(400));
    }
}
