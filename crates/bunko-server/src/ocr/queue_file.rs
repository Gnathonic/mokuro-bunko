//! The reader's queue file `GET /mokuro-reader/.mokuro-queue.json` (0.5.2
//! `middleware/queue_file.py`, spec http-webdav §11): rebuilt at most once a second,
//! ETAs kept within 60 s of what was published, a strong ETag over the document without
//! `generated_at`, a gzip representation with its own tag, every write refused with 405.
//!
//! The file is recognised in every spelling the filename fold joins
//! ([`bunko_core::names::is_fixed_name`]: `.MOKURO-QUEUE.JSON`, `.mokuro-queue.jſon`,
//! one with a zero width space in it), as every host makes them one name: a GET of any
//! serves the generated document, and a write of any answers 405, on a local library as
//! on a cloud one (where the store lists the file). Bunko's other control file at the
//! library root, the cloud lease marker `.mokuro-bunko.json`, is written by the lease
//! only: every write of it, in any spelling, answers 403 for every account, as a cloud
//! store refuses it, so a local library holds none a client made either.
//!
//! The same middleware serves the queue file v2 of each series,
//! `/mokuro-reader/<series>/.mokuro-series-queue.json` ([`super::series_queue`]), in
//! every spelling of the name: a GET or HEAD with the read gate of a file in that
//! folder, 404 for a folder the library does not show, 405 for every write (a MOVE or
//! COPY onto it included), 404 for a PROPFIND (no listing shows it either).

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
/// The queue file's name at the library root.
pub const QUEUE_FILE_NAME: &str = ".mokuro-queue.json";
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

/// The whole document but `generated_at`: a rebuild that changed only the time keeps
/// the published copy, any other change (`next_check_after`, `pending_volumes`
/// included) replaces it.
fn same_queue(a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    let rest = |m: &Map<String, Value>| {
        m.iter()
            .filter(|(k, _)| *k != "generated_at")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<std::collections::BTreeMap<String, Value>>()
    };
    rest(a) == rest(b)
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

fn marker_refused() -> Response {
    let body = "The storage lease marker is written by Bunko only.";
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = StatusCode::FORBIDDEN;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    resp
}

/// The segments of a decoded request path (or Destination path), `.` and `..`
/// segments and empty ones resolved.
fn segments(path: &str) -> Vec<&str> {
    let mut segs: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    segs
}

/// The library-root name a decoded path names: `Some("x")` for `/mokuro-reader/x`,
/// `/mokuro-reader/./x/`, `None` for anything else.
fn root_name(path: &str) -> Option<String> {
    match segments(path)[..] {
        ["mokuro-reader", name] => Some(name.to_string()),
        _ => None,
    }
}

/// The series folder a decoded path names the queue file v2 of, in any spelling the
/// filename fold joins with `.mokuro-series-queue.json`: `Some("S")` for
/// `/mokuro-reader/S/.mokuro-series-queue.json`.
pub fn series_queue_folder(path: &str) -> Option<String> {
    match segments(path)[..] {
        ["mokuro-reader", series, name]
            if bunko_core::names::is_fixed_name(name, super::series_queue::SERIES_QUEUE_FILE) =>
        {
            Some(series.to_string())
        }
        _ => None,
    }
}

/// Whether `path` (decoded) is the queue file, in any spelling the filename fold joins
/// with `.mokuro-queue.json`.
pub fn is_queue_file(path: &str) -> bool {
    root_name(path).is_some_and(|n| bunko_core::names::is_fixed_name(&n, QUEUE_FILE_NAME))
}

/// Whether `path` (decoded) is the cloud lease marker, in any spelling the filename
/// fold joins with `.mokuro-bunko.json`.
pub fn is_marker_file(path: &str) -> bool {
    root_name(path)
        .is_some_and(|n| bunko_core::names::is_fixed_name(&n, bunko_remote::status::MARKER_NAME))
}

/// The refusals, decided before authentication: any write to the queue file in any
/// spelling, and a MOVE/COPY whose Destination is it (405); any write to the lease
/// marker in any spelling, a MOVE of it, and a MOVE/COPY onto it (403). None: not this
/// module's request.
pub fn refusal(method: &Method, path: &str, destination: Option<&str>) -> Option<Response> {
    let m = method.as_str();
    if m == "MOVE" || m == "COPY" {
        let to = destination.and_then(crate::auth::paths::destination_path);
        if to
            .as_deref()
            .is_some_and(|t| is_queue_file(t) || series_queue_folder(t).is_some())
        {
            return Some(not_allowed());
        }
        if to.as_deref().is_some_and(is_marker_file) {
            return Some(marker_refused());
        }
    }
    let read = *method == Method::OPTIONS || *method == Method::GET || *method == Method::HEAD;
    if is_queue_file(path) && !read {
        return Some(not_allowed());
    }
    if series_queue_folder(path).is_some() && !read && m != "PROPFIND" {
        return Some(not_allowed());
    }
    if is_marker_file(path) && !read && m != "PROPFIND" && m != "COPY" {
        return Some(marker_refused());
    }
    None
}

/// Serve the file for an already authorised GET/HEAD.
pub async fn serve(
    ocr: Option<&OcrControl>,
    state: &QueueFile,
    method: &Method,
    headers: &HeaderMap,
) -> Response {
    let (body, tag, packed) = state.current(ocr).await;
    let gz_tag = format!("{}-gz\"", &tag[..tag.len() - 1]);
    respond(method, headers, (body, tag), Some((packed, gz_tag)), false)
}

/// The answer to a GET/HEAD of a generated JSON file: `plain` (body, tag), or its gzip
/// form when one is offered and the client takes gzip; 304 when `If-None-Match` names
/// the tag served (`either_tag`: either representation's tag).
fn respond(
    method: &Method,
    headers: &HeaderMap,
    plain: (bytes::Bytes, String),
    packed: Option<(bytes::Bytes, String)>,
    either_tag: bool,
) -> Response {
    let gz = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_lowercase().contains("gzip"));
    let other = packed.as_ref().map(|p| p.1.clone());
    let plain_tag = plain.1.clone();
    let (gz, (body, tag)) = match packed {
        Some(p) if gz => (true, p),
        _ => (false, plain),
    };
    let inm = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let not_modified = inm
        .split(',')
        .map(str::trim)
        .any(|t| t == tag || (either_tag && (t == plain_tag || other.as_deref() == Some(t))));
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

/// The series folder `requested` names, as the library spells it, or None when the
/// library shows no such folder (a per-user name, a nested path, a file). Blocking.
fn series_folder(core: &Core, requested: &str) -> Option<String> {
    use bunko_core::reader_tree::is_per_user_root_name;
    if requested.contains(['/', '\\']) || is_per_user_root_name(requested) {
        return None;
    }
    let library = core.layout.library();
    let canon = bunko_dav::LibraryPathCanonicalizer::new(library.clone());
    let spelled = canon.canonicalize(requested, None);
    if spelled.contains(['/', '\\']) || is_per_user_root_name(&spelled) || !canon.reaches(&spelled)
    {
        return None;
    }
    bunko_store::fs::is_dir(library.join(&spelled)).then_some(spelled)
}

fn not_found() -> Response {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::NOT_FOUND;
    resp.headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(0));
    resp
}

static NO_OCR_SERIES: std::sync::LazyLock<super::series_queue::SeriesQueueHttp> =
    std::sync::LazyLock::new(super::series_queue::SeriesQueueHttp::new);

/// A GET/HEAD/PROPFIND of `/mokuro-reader/<requested>/.mokuro-series-queue.json`.
async fn serve_series(
    st: &QueueFileState,
    ctx: &RequestCtx,
    requested: &str,
    method: &Method,
    headers: &HeaderMap,
) -> Response {
    // Neither the generated file nor a store's copy is a member of any listing.
    if method.as_str() == "PROPFIND" {
        return not_found();
    }
    let core = &st.core;
    let gate = format!(
        "/mokuro-reader/{requested}/{}",
        super::series_queue::SERIES_QUEUE_FILE
    );
    if let Err(denied) = crate::auth::authorize(
        &Method::GET,
        &gate,
        None,
        &ctx.identity,
        core.anonymous_access(),
        core.backend.as_ref(),
    ) {
        return denied.into_response();
    }
    let lookup = {
        let (core, requested) = (core.clone(), requested.to_string());
        tokio::task::spawn_blocking(move || series_folder(&core, &requested)).await
    };
    let Ok(Some(folder)) = lookup else {
        return not_found();
    };
    let fold = bunko_core::names::fold_name(&folder);
    let sink = match &st.ocr {
        Some(o) => o.series_queue_state(),
        None => &NO_OCR_SERIES,
    };
    let r = sink.current(st.ocr.as_ref(), &fold, &folder).await;
    respond(method, headers, (r.body, r.etag), r.packed, true)
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
    let ocr = st.ocr.clone();
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
    if let Some(series) = series_queue_folder(&path)
        && *req.method() != Method::OPTIONS
    {
        return serve_series(&st, &ctx, &series, req.method(), req.headers()).await;
    }
    // Any spelling (PathCase may have given the path a listed one): the generated
    // document, never a file a client made.
    if !is_queue_file(&path) || *req.method() == Method::OPTIONS {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Finding 7: a rebuild that changed only `next_check_after` or `pending_volumes`
    /// kept serving the old body (and so a stale `next_check_after`).
    #[test]
    fn same_queue_compares_everything_but_the_time() {
        let doc = |pending: usize, now: f64| build_document(None, Vec::new(), pending, now);
        let a = doc(3, 1_000_000.0);
        assert!(
            same_queue(&a, &doc(3, 1_000_500.0)),
            "only generated_at moved"
        );
        assert!(
            !same_queue(&a, &doc(4, 1_000_000.0)),
            "pending_volumes moved"
        );
        let mut b = a.clone();
        b.insert("next_check_after".into(), json!(42));
        assert!(!same_queue(&a, &b), "next_check_after moved");
        let mut c = a.clone();
        c.insert("extra".into(), json!(1));
        assert!(!same_queue(&a, &c), "a new field");
    }

    #[test]
    fn series_queue_paths() {
        let f = |p: &str| series_queue_folder(p);
        assert_eq!(
            f("/mokuro-reader/S/.mokuro-series-queue.json").as_deref(),
            Some("S")
        );
        assert_eq!(
            f("/mokuro-reader/S/.MOKURO-SERIES-QUEUE.JSON/").as_deref(),
            Some("S")
        );
        assert_eq!(
            f("/mokuro-reader/x/../S/./.mokuro-series-queue.json").as_deref(),
            Some("S")
        );
        assert_eq!(f("/mokuro-reader/.mokuro-series-queue.json"), None);
        assert_eq!(f("/mokuro-reader/S/T/.mokuro-series-queue.json"), None);
        assert_eq!(f("/mokuro-reader/S/.mokuro-queue.json"), None);
        assert_eq!(f("/S/.mokuro-series-queue.json"), None);
        assert!(
            refusal(
                &Method::PUT,
                "/mokuro-reader/S/.mokuro-series-queue.json",
                None
            )
            .is_some()
        );
        assert!(
            refusal(
                &Method::GET,
                "/mokuro-reader/S/.mokuro-series-queue.json",
                None
            )
            .is_none()
        );
        let propfind = Method::from_bytes(b"PROPFIND").unwrap();
        assert!(
            refusal(
                &propfind,
                "/mokuro-reader/S/.mokuro-series-queue.json",
                None
            )
            .is_none()
        );
    }
}
