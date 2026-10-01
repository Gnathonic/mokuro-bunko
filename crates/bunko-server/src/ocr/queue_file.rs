//! The reader's queue file `GET /mokuro-reader/.mokuro-queue.json` (0.5.2
//! `middleware/queue_file.py`, spec http-webdav §11): rebuilt at most once a second,
//! ETAs kept within 60 s of what was published, a strong ETag over the document without
//! `generated_at`, a gzip representation with its own tag, every write refused with 405.

use std::time::Instant;

use axum::body::Body;
use axum::extract::{FromRef, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use sha2::Digest;

use super::OcrControl;
use crate::core::{Core, RequestCtx};

pub const QUEUE_FILE_PATH: &str = "/mokuro-reader/.mokuro-queue.json";
pub const QUEUE_FILE_VERSION: u64 = 1;
pub const REBUILD_SECONDS: f64 = 1.0;
pub const ETA_HYSTERESIS_SECONDS: f64 = 60.0;
const ALLOW: &str = "GET, HEAD, OPTIONS";

struct Built {
    at: Instant,
    body: bytes::Bytes,
    etag: String,
    packed: bytes::Bytes,
    document: Map<String, Value>,
}

/// The cached representation.
#[derive(Default)]
pub struct QueueFile {
    built: Mutex<Option<Built>>,
    building: tokio::sync::Mutex<()>,
}

/// `build_document`: the file's content (`generated_at` included).
pub fn build_document(
    held: Option<&str>,
    volumes: Vec<Value>,
    pending_volumes: usize,
    now: f64,
) -> Map<String, Value> {
    let all_jobs: Vec<Value> = volumes
        .iter()
        .flat_map(|v| v["jobs"].as_array().cloned().unwrap_or_default())
        .collect();
    let volumes: Vec<Value> = volumes
        .into_iter()
        .map(|v| {
            let series = v["series"].as_str().unwrap_or("").to_string();
            let volume = v["volume"].as_str().unwrap_or("").to_string();
            json!({
                "series": series,
                "volume": volume,
                "path": super::reader_file_url(&series, &format!("{volume}.cbz")),
                "manifest": super::manifest_url(&series, &volume),
                "jobs": v["jobs"],
            })
        })
        .collect();
    let mut doc = Map::new();
    doc.insert("version".into(), json!(QUEUE_FILE_VERSION));
    doc.insert("generated_at".into(), json!(bunko_sched::py::iso_utc(now)));
    doc.insert(
        "held".into(),
        held.map_or(Value::Null, |r| json!({"reason": r})),
    );
    doc.insert(
        "next_check_after".into(),
        json!(bunko_sched::outlook::recheck_after(&all_jobs, now)),
    );
    doc.insert("pending_volumes".into(), json!(pending_volumes));
    doc.insert("volumes".into(), Value::Array(volumes));
    doc
}

fn parse_eta(v: &Value) -> Option<f64> {
    v.as_str().and_then(bunko_sched::py::parse_iso_timestamp)
}

/// `_keep_close_etas`: a job keeps its published ETA while the new one is within 60 s.
pub fn keep_close_etas(document: &mut Map<String, Value>, previous: &Map<String, Value>) {
    let mut published: std::collections::HashMap<(String, String, String), Value> =
        Default::default();
    for v in previous
        .get("volumes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for j in v["jobs"].as_array().into_iter().flatten() {
            if j["eta"].is_string() {
                let key = (
                    v["series"].as_str().unwrap_or("").into(),
                    v["volume"].as_str().unwrap_or("").into(),
                    j["id"].as_str().unwrap_or("").into(),
                );
                published.insert(key, j["eta"].clone());
            }
        }
    }
    let Some(Value::Array(volumes)) = document.get_mut("volumes") else {
        return;
    };
    for v in volumes {
        let (s, vol) = (
            v["series"].as_str().unwrap_or("").to_string(),
            v["volume"].as_str().unwrap_or("").to_string(),
        );
        let Some(Value::Array(jobs)) = v.get_mut("jobs") else {
            continue;
        };
        for j in jobs {
            let key = (
                s.clone(),
                vol.clone(),
                j["id"].as_str().unwrap_or("").to_string(),
            );
            let Some(old) = published.get(&key) else {
                continue;
            };
            if let (Some(new), Some(was)) = (parse_eta(&j["eta"]), parse_eta(old))
                && (new - was).abs() < ETA_HYSTERESIS_SECONDS
            {
                j["eta"] = old.clone();
            }
        }
    }
}

fn same_queue(a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    ["version", "held", "volumes"]
        .iter()
        .all(|k| a.get(*k) == b.get(*k))
}

/// `_etag`: sha256 of the document without `generated_at` (sorted keys, compact).
pub fn etag(document: &Map<String, Value>) -> String {
    let stable: Map<String, Value> = document
        .iter()
        .filter(|(k, _)| *k != "generated_at")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let text = super::pyjson::dumps(&Value::Object(stable), super::pyjson::SORTED_COMPACT);
    let digest = hex::encode(sha2::Sha256::digest(text.as_bytes()));
    format!("\"{}\"", &digest[..32])
}

fn gzip(body: &[u8]) -> bytes::Bytes {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
    let _ = enc.write_all(body);
    bytes::Bytes::from(enc.finish().unwrap_or_default())
}

impl QueueFile {
    /// `_current`: `(body, etag, gzip body)`.
    pub async fn current(&self, ocr: Option<&OcrControl>) -> (bytes::Bytes, String, bytes::Bytes) {
        let fresh = |b: &Option<Built>| {
            b.as_ref()
                .filter(|b| b.at.elapsed().as_secs_f64() < REBUILD_SECONDS)
                .map(|b| (b.body.clone(), b.etag.clone(), b.packed.clone()))
        };
        if let Some(hit) = fresh(&self.built.lock()) {
            return hit;
        }
        let _guard = self.building.lock().await;
        if let Some(hit) = fresh(&self.built.lock()) {
            return hit;
        }
        let answer = match ocr {
            Some(o) => o.ask(|s| (s.queue_document(), s.now())).await,
            None => None,
        };
        let now = answer.as_ref().map_or_else(
            || bunko_sched::rate::Clock::time(&bunko_sched::rate::SystemClock::default()),
            |a| a.1,
        );
        let (held, volumes, pending) = answer.map(|a| a.0).unwrap_or((None, Vec::new(), 0));
        let mut document = build_document(held, volumes, pending, now);
        let mut built = self.built.lock();
        if let Some(prev) = built.as_mut() {
            keep_close_etas(&mut document, &prev.document);
            if same_queue(&document, &prev.document) {
                prev.at = Instant::now();
                return (prev.body.clone(), prev.etag.clone(), prev.packed.clone());
            }
        }
        let tag = etag(&document);
        let body = bytes::Bytes::from(super::pyjson::dumps(
            &Value::Object(document.clone()),
            super::pyjson::COMPACT,
        ));
        let packed = gzip(&body);
        *built = Some(Built {
            at: Instant::now(),
            body: body.clone(),
            etag: tag.clone(),
            packed: packed.clone(),
            document,
        });
        (body, tag, packed)
    }
}

fn not_allowed() -> Response {
    let body = "The OCR queue file is generated by the server and cannot be written.";
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
    let h = resp.headers_mut();
    h.insert(header::ALLOW, HeaderValue::from_static(ALLOW));
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    resp
}

/// The 405 cases, decided before authentication: any write to the file, and a
/// MOVE/COPY whose Destination is the file. None: not this module's request.
pub fn refusal(method: &Method, path: &str, destination: Option<&str>) -> Option<Response> {
    if (method.as_str() == "MOVE" || method.as_str() == "COPY")
        && destination
            .and_then(crate::auth::paths::destination_path)
            .as_deref()
            == Some(QUEUE_FILE_PATH)
    {
        return Some(not_allowed());
    }
    if path != QUEUE_FILE_PATH
        || *method == Method::OPTIONS
        || *method == Method::GET
        || *method == Method::HEAD
    {
        return None;
    }
    Some(not_allowed())
}

/// Serve the file for an already authorised GET/HEAD.
pub async fn serve(
    ocr: Option<&OcrControl>,
    state: &QueueFile,
    method: &Method,
    headers: &HeaderMap,
) -> Response {
    let (body, tag, packed) = state.current(ocr).await;
    let gz = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_lowercase().contains("gzip"));
    let (body, tag) = if gz {
        (packed, format!("{}-gz\"", &tag[..tag.len() - 1]))
    } else {
        (body, tag)
    };
    let inm = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let not_modified = inm.split(',').any(|t| t.trim() == tag);
    let len = body.len();
    let mut resp = if not_modified || *method == Method::HEAD {
        Response::new(Body::empty())
    } else {
        Response::new(Body::from(body))
    };
    *resp.status_mut() = if not_modified {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::OK
    };
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    if gz {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    if let Ok(v) = HeaderValue::from_str(&tag) {
        h.insert(header::ETAG, v);
    }
    if !not_modified {
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    }
    resp
}

static NO_OCR_FILE: std::sync::LazyLock<QueueFile> = std::sync::LazyLock::new(QueueFile::default);

/// The middleware's state: the core (for the read gate) and the OCR handle, if any.
#[derive(Clone)]
pub struct QueueFileState {
    pub core: Core,
    pub ocr: Option<OcrControl>,
}

impl FromRef<QueueFileState> for Core {
    fn from_ref(s: &QueueFileState) -> Core {
        s.core.clone()
    }
}

/// The middleware the orchestrator layers before the DAV fallback:
/// `from_fn_with_state(QueueFileState { core, ocr }, queue_file::middleware)`.
/// Without OCR it still answers (an empty, unheld queue), as 0.5.2 did.
pub async fn middleware(
    State(st): State<QueueFileState>,
    ctx: RequestCtx,
    req: Request,
    next: Next,
) -> Response {
    let ocr = st.ocr;
    let path = percent_encoding::percent_decode_str(req.uri().path())
        .decode_utf8_lossy()
        .into_owned();
    let destination = req
        .headers()
        .get("destination")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(refused) = refusal(req.method(), &path, destination.as_deref()) {
        return refused;
    }
    if path != QUEUE_FILE_PATH || *req.method() == Method::OPTIONS {
        return next.run(req).await;
    }
    // The same gate as a GET of a library file.
    let core = &st.core;
    if let Err(denied) = crate::auth::authorize(
        &Method::GET,
        QUEUE_FILE_PATH,
        None,
        &ctx.identity,
        core.anonymous_access(),
        core.backend.as_ref(),
    ) {
        return denied.into_response();
    }
    let state = match &ocr {
        Some(o) => o.queue_file_state(),
        None => &NO_OCR_FILE,
    };
    serve(ocr.as_ref(), state, req.method(), req.headers()).await
}
