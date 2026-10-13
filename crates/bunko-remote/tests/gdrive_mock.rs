//! GDriveBackend against an in-memory mock of the Drive v3 endpoints it uses.
#![cfg(feature = "gdrive")]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use bunko_remote::backend::{
    BErr, Backend, ChangeItem, ChangePage, Cursor, HashKind, OpCtx, RNode, UploadSource,
    UploadTarget, read_to_vec,
};
use bunko_remote::gdrive::api::units;
use bunko_remote::gdrive::{GDriveBackend, GDriveConfig, StaticToken, TokenSourceDyn};
use md5::Md5;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------- the mock

const FOLDER: &str = "application/vnd.google-apps.folder";

struct MFile {
    name: String,
    mime: String,
    parents: Vec<String>,
    data: Bytes,
    created: i64,
    modified: i64,
    version: i64,
    head_rev: String,
    revisions: HashMap<String, Bytes>,
    trashed: bool,
}

enum SessKind {
    New(Value),
    Replace(String),
}

struct Sess {
    kind: SessKind,
    size: usize,
    buf: Vec<u8>,
    done: Option<String>,
}

#[derive(Debug, Clone)]
struct Req {
    method: String,
    path: String,
    query: HashMap<String, String>,
    content_range: Option<String>,
}

#[derive(Clone)]
struct Fault {
    method: Option<&'static str>,
    path_has: String,
    status: u16,
    reason: &'static str,
    retry_after: Option<u64>,
    times: usize,
}

struct St {
    base: String,
    files: BTreeMap<String, MFile>,
    seq: u64,
    clock: i64,
    log: Vec<String>,
    page_size: usize,
    reverse_listing: bool,
    sha256: bool,
    keep_revisions: bool,
    revisions_forbidden: bool,
    sessions: HashMap<String, Sess>,
    valid_token: String,
    faults: Vec<Fault>,
    requests: Vec<Req>,
    /// Fail this (1-based) chunk PUT after taking half its bytes.
    interrupt_chunk: Option<usize>,
    /// After the interrupt, every session PUT fails until this is cleared.
    down_after_interrupt: bool,
    session_down: bool,
    chunk_puts: usize,
    quota_limit: i64,
}

#[derive(Clone)]
struct Mock(Arc<Mutex<St>>);

fn hex_md5(b: &[u8]) -> String {
    hex::encode(Md5::digest(b))
}

fn hex_sha256(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

fn rfc3339(ms: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000).unwrap();
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

fn err(status: u16, reason: &str) -> Response {
    let body = json!({"error": {"code": status, "message": reason,
        "errors": [{"domain": "global", "reason": reason, "message": reason}]}});
    (StatusCode::from_u16(status).unwrap(), axum::Json(body)).into_response()
}

fn ok_json(v: Value) -> Response {
    axum::Json(v).into_response()
}

impl St {
    fn tick(&mut self) -> i64 {
        self.clock += 1000;
        self.clock
    }

    fn new_id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("{prefix}{:05}", self.seq)
    }

    fn trashed(&self, id: &str) -> bool {
        let mut cur = id;
        for _ in 0..64 {
            let Some(f) = self.files.get(cur) else {
                return false;
            };
            if f.trashed {
                return true;
            }
            match f.parents.first() {
                Some(p) => cur = p,
                None => return false,
            }
        }
        false
    }

    fn json(&self, id: &str) -> Value {
        let f = &self.files[id];
        let mut v = json!({
            "id": id, "name": f.name, "mimeType": f.mime, "parents": f.parents,
            "version": f.version.to_string(), "modifiedTime": rfc3339(f.modified),
            "createdTime": rfc3339(f.created), "trashed": self.trashed(id),
        });
        if f.mime != FOLDER {
            v["size"] = json!(f.data.len().to_string());
            v["md5Checksum"] = json!(hex_md5(&f.data));
            v["headRevisionId"] = json!(f.head_rev);
            if self.sha256 {
                v["sha256Checksum"] = json!(hex_sha256(&f.data));
            }
        }
        v
    }

    fn insert(
        &mut self,
        id: Option<String>,
        parent: &str,
        name: &str,
        mime: &str,
        data: Bytes,
    ) -> String {
        let id = id.unwrap_or_else(|| self.new_id("f"));
        let now = self.tick();
        let rev = self.new_id("r");
        let mut revisions = HashMap::new();
        revisions.insert(rev.clone(), data.clone());
        self.files.insert(
            id.clone(),
            MFile {
                name: name.into(),
                mime: mime.into(),
                parents: vec![parent.into()],
                data,
                created: now,
                modified: now,
                version: 1,
                head_rev: rev,
                revisions,
                trashed: false,
            },
        );
        self.log.push(id.clone());
        id
    }

    fn rewrite(&mut self, id: &str, data: Bytes) {
        let now = self.tick();
        let rev = self.new_id("r");
        let keep = self.keep_revisions;
        let f = self.files.get_mut(id).unwrap();
        if !keep {
            f.revisions.clear();
        }
        f.revisions.insert(rev.clone(), data.clone());
        f.data = data;
        f.head_rev = rev;
        f.version += 1;
        f.modified = now;
        self.log.push(id.to_string());
    }

    fn delete(&mut self, id: &str) {
        let kids: Vec<String> = self
            .files
            .iter()
            .filter(|(_, f)| f.parents.iter().any(|p| p == id))
            .map(|(k, _)| k.clone())
            .collect();
        for k in kids {
            self.delete(&k);
        }
        self.files.remove(id);
        self.log.push(id.to_string());
    }
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let inner = &s[1..s.len() - 1];
    let mut out = String::new();
    let mut esc = false;
    for c in inner.chars() {
        if esc {
            out.push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else {
            out.push(c);
        }
    }
    out
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// (metadata, media) of a multipart/related body.
fn parse_multipart(headers: &HeaderMap, body: &[u8]) -> (Value, Bytes) {
    let ct = headers[header::CONTENT_TYPE].to_str().unwrap();
    let boundary = ct.split("boundary=").nth(1).unwrap().trim();
    let delim = format!("--{boundary}");
    let h1 = find(body, b"\r\n\r\n", 0).unwrap() + 4;
    let j_end = find(body, format!("\r\n{delim}\r\n").as_bytes(), h1).unwrap();
    let meta: Value = serde_json::from_slice(&body[h1..j_end]).unwrap();
    let h2 = find(body, b"\r\n\r\n", j_end + 2).unwrap() + 4;
    let closing = format!("\r\n{delim}--");
    let m_end = body
        .windows(closing.len())
        .rposition(|w| w == closing.as_bytes())
        .unwrap();
    (meta, Bytes::copy_from_slice(&body[h2..m_end]))
}

fn media(data: &Bytes, headers: &HeaderMap) -> Response {
    let Some(r) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) else {
        return (StatusCode::OK, data.clone()).into_response();
    };
    let spec = r.strip_prefix("bytes=").unwrap();
    let (a, b) = spec.split_once('-').unwrap();
    let a: usize = a.parse().unwrap();
    if a >= data.len() {
        return err(416, "requestedRangeNotSatisfiable");
    }
    let end = if b.is_empty() {
        data.len()
    } else {
        (b.parse::<usize>().unwrap() + 1).min(data.len())
    };
    (
        StatusCode::PARTIAL_CONTENT,
        [(
            header::CONTENT_RANGE,
            format!("bytes {a}-{}/{}", end - 1, data.len()),
        )],
        data.slice(a..end),
    )
        .into_response()
}

fn range_308(len: usize) -> Response {
    let mut r = Response::builder().status(308);
    if len > 0 {
        r = r.header(header::RANGE, format!("bytes=0-{}", len - 1));
    }
    r.body(axum::body::Body::empty()).unwrap()
}

async fn handle(
    State(m): State<Mock>,
    method: Method,
    uri: Uri,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut st = m.0.lock();
    let path = uri.path().to_string();
    st.requests.push(Req {
        method: method.to_string(),
        path: path.clone(),
        query: q.clone(),
        content_range: headers
            .get(header::CONTENT_RANGE)
            .map(|v| v.to_str().unwrap().to_string()),
    });
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth != format!("Bearer {}", st.valid_token) {
        return err(401, "authError");
    }
    if let Some(i) = st
        .faults
        .iter()
        .position(|f| f.method.is_none_or(|mm| mm == method.as_str()) && path.contains(&f.path_has))
    {
        let f = st.faults[i].clone();
        if f.times <= 1 {
            st.faults.remove(i);
        } else {
            st.faults[i].times -= 1;
        }
        let mut r = err(f.status, f.reason);
        if let Some(s) = f.retry_after {
            r.headers_mut()
                .insert(header::RETRY_AFTER, s.to_string().parse().unwrap());
        }
        return r;
    }
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method.as_str(), segs.as_slice()) {
        ("GET", ["drive", "v3", "about"]) => {
            let used: usize = st.files.values().map(|f| f.data.len()).sum();
            ok_json(json!({
                "storageQuota": {"limit": st.quota_limit.to_string(), "usage": used.to_string()},
                "user": {"permissionId": "perm-1", "emailAddress": "me@example.com", "displayName": "Me"},
            }))
        }
        ("GET", ["drive", "v3", "changes", "startPageToken"]) => {
            ok_json(json!({"startPageToken": format!("t{}", st.log.len())}))
        }
        ("GET", ["drive", "v3", "changes"]) => {
            let tok = q.get("pageToken").cloned().unwrap_or_default();
            let Some(at) = tok.strip_prefix('t').and_then(|n| n.parse::<usize>().ok()) else {
                return err(400, "invalid");
            };
            if at > st.log.len() {
                return err(404, "notFound");
            }
            assert_eq!(q.get("includeRemoved").map(String::as_str), Some("true"));
            let end = (at + st.page_size).min(st.log.len());
            let changes: Vec<Value> = st.log[at..end]
                .iter()
                .map(|id| {
                    if st.files.contains_key(id) {
                        json!({"changeType": "file", "fileId": id, "removed": false, "file": st.json(id)})
                    } else {
                        json!({"changeType": "file", "fileId": id, "removed": true})
                    }
                })
                .collect();
            let mut v = json!({"changes": changes});
            if end < st.log.len() {
                v["nextPageToken"] = json!(format!("t{end}"));
            } else {
                v["newStartPageToken"] = json!(format!("t{end}"));
            }
            ok_json(v)
        }
        ("GET", ["drive", "v3", "files", "generateIds"]) => {
            let id = st.new_id("g");
            ok_json(json!({"ids": [id], "space": "drive", "kind": "drive#generatedIds"}))
        }
        ("GET", ["drive", "v3", "files"]) => {
            let query = q.get("q").cloned().unwrap_or_default();
            let mut owners = false;
            let mut no_trash = false;
            let mut name: Option<String> = None;
            let mut mime: Option<String> = None;
            let mut parent: Option<String> = None;
            for clause in query.split(" and ") {
                let c = clause.trim();
                if c == "'me' in owners" {
                    owners = true;
                } else if c == "trashed=false" {
                    no_trash = true;
                } else if let Some(v) = c.strip_prefix("name=") {
                    name = Some(unquote(v));
                } else if let Some(v) = c.strip_prefix("mimeType=") {
                    mime = Some(unquote(v));
                } else if let Some(v) = c.strip_suffix(" in parents") {
                    parent = Some(unquote(v));
                } else {
                    panic!("mock: unsupported q clause {c:?}");
                }
            }
            assert!(owners || name.is_some() || parent.is_some(), "q {query}");
            let mut ids: Vec<String> = st
                .files
                .iter()
                .filter(|(id, f)| {
                    (!no_trash || !st.trashed(id))
                        // Drive's `name =` match is case-insensitive.
                        && name.as_ref().is_none_or(|n| n.eq_ignore_ascii_case(&f.name))
                        && mime.as_ref().is_none_or(|mm| *mm == f.mime)
                        && parent.as_ref().is_none_or(|p| f.parents.contains(p))
                })
                .map(|(id, _)| id.clone())
                .collect();
            if st.reverse_listing {
                ids.reverse();
            }
            let at: usize = q
                .get("pageToken")
                .and_then(|t| t.strip_prefix('p'))
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            let end = (at + st.page_size).min(ids.len());
            let files: Vec<Value> = ids[at..end].iter().map(|id| st.json(id)).collect();
            let mut v = json!({"files": files});
            if end < ids.len() {
                v["nextPageToken"] = json!(format!("p{end}"));
            }
            ok_json(v)
        }
        ("POST", ["drive", "v3", "files"]) => {
            let meta: Value = serde_json::from_slice(&body).unwrap();
            let parent = meta["parents"][0].as_str().unwrap_or("root").to_string();
            let mime = meta["mimeType"]
                .as_str()
                .unwrap_or("application/octet-stream");
            let id = st.insert(
                None,
                &parent,
                meta["name"].as_str().unwrap(),
                mime,
                Bytes::new(),
            );
            ok_json(st.json(&id))
        }
        ("POST", ["drive", "v3", "files", id, "copy"]) => {
            let Some(src) = st.files.get(*id) else {
                return err(404, "notFound");
            };
            let (data, mime) = (src.data.clone(), src.mime.clone());
            let meta: Value = serde_json::from_slice(&body).unwrap();
            let new = st.insert(
                None,
                meta["parents"][0].as_str().unwrap(),
                meta["name"].as_str().unwrap(),
                &mime,
                data,
            );
            ok_json(st.json(&new))
        }
        ("GET", ["drive", "v3", "files", id, "revisions", rev]) => {
            assert_eq!(q.get("alt").map(String::as_str), Some("media"));
            if st.revisions_forbidden {
                return err(403, "insufficientFilePermissions");
            }
            match st.files.get(*id).and_then(|f| f.revisions.get(*rev)) {
                Some(d) => media(&d.clone(), &headers),
                None => err(404, "notFound"),
            }
        }
        ("GET", ["drive", "v3", "files", id]) => {
            if !st.files.contains_key(*id) {
                return err(404, "notFound");
            }
            if q.get("alt").map(String::as_str) == Some("media") {
                media(&st.files[*id].data.clone(), &headers)
            } else {
                ok_json(st.json(id))
            }
        }
        ("PATCH", ["drive", "v3", "files", id]) => {
            let id = id.to_string();
            if !st.files.contains_key(&id) {
                return err(404, "notFound");
            }
            let meta: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let f = st.files.get_mut(&id).unwrap();
            if let Some(n) = meta["name"].as_str() {
                f.name = n.into();
            }
            if meta["trashed"].as_bool() == Some(true) {
                f.trashed = true;
            }
            if let Some(rm) = q.get("removeParents") {
                let rm: HashSet<&str> = rm.split(',').collect();
                f.parents.retain(|p| !rm.contains(p.as_str()));
            }
            if let Some(add) = q.get("addParents") {
                f.parents.push(add.clone());
            }
            f.version += 1;
            st.log.push(id.clone());
            ok_json(st.json(&id))
        }
        ("DELETE", ["drive", "v3", "files", id]) => {
            if !st.files.contains_key(*id) {
                return err(404, "notFound");
            }
            st.delete(id);
            StatusCode::NO_CONTENT.into_response()
        }
        (m @ ("POST" | "PATCH"), ["upload", "drive", "v3", "files", rest @ ..]) => {
            let target = rest.first().map(|s| s.to_string());
            if m == "PATCH" {
                assert_eq!(
                    q.get("keepRevisionForever").map(String::as_str),
                    Some("false")
                );
                if !st.files.contains_key(target.as_deref().unwrap()) {
                    return err(404, "notFound");
                }
            }
            match q.get("uploadType").map(String::as_str) {
                Some("multipart") => {
                    let (meta, data) = parse_multipart(&headers, &body);
                    match target {
                        Some(id) => {
                            st.rewrite(&id, data);
                            ok_json(st.json(&id))
                        }
                        None => {
                            let id = meta["id"].as_str().map(str::to_string);
                            if id.as_ref().is_some_and(|i| st.files.contains_key(i)) {
                                return err(409, "duplicate");
                            }
                            let id = st.insert(
                                id,
                                meta["parents"][0].as_str().unwrap(),
                                meta["name"].as_str().unwrap(),
                                "application/octet-stream",
                                data,
                            );
                            ok_json(st.json(&id))
                        }
                    }
                }
                Some("resumable") => {
                    let size: usize = headers["x-upload-content-length"]
                        .to_str()
                        .unwrap()
                        .parse()
                        .unwrap();
                    let kind = match target {
                        Some(id) => SessKind::Replace(id),
                        None => SessKind::New(serde_json::from_slice(&body).unwrap()),
                    };
                    let sid = st.new_id("s");
                    st.sessions.insert(
                        sid.clone(),
                        Sess {
                            kind,
                            size,
                            buf: Vec::new(),
                            done: None,
                        },
                    );
                    let loc = format!(
                        "{}/upload/drive/v3/files?uploadType=resumable&upload_id={sid}",
                        st.base
                    );
                    Response::builder()
                        .status(200)
                        .header(header::LOCATION, loc)
                        .body(axum::body::Body::empty())
                        .unwrap()
                }
                other => panic!("mock: uploadType {other:?}"),
            }
        }
        ("PUT", ["upload", "drive", "v3", "files"]) => {
            if st.session_down {
                return err(503, "backendError");
            }
            let sid = q["upload_id"].clone();
            let cr = headers[header::CONTENT_RANGE].to_str().unwrap().to_string();
            let spec = cr.strip_prefix("bytes ").unwrap();
            let (range, total) = spec.split_once('/').unwrap();
            let Some(sess) = st.sessions.get(&sid) else {
                return err(404, "notFound");
            };
            assert_eq!(total.parse::<usize>().unwrap(), sess.size);
            if let Some(id) = sess.done.clone() {
                return ok_json(st.json(&id));
            }
            if range == "*" {
                return range_308(sess.buf.len());
            }
            st.chunk_puts += 1;
            let n = st.chunk_puts;
            let (a, _b) = range.split_once('-').unwrap();
            let a: usize = a.parse().unwrap();
            let interrupt = st.interrupt_chunk == Some(n);
            let sess = st.sessions.get_mut(&sid).unwrap();
            if a != sess.buf.len() {
                return range_308(sess.buf.len());
            }
            if interrupt {
                sess.buf.extend_from_slice(&body[..body.len() / 2]);
                st.session_down = st.down_after_interrupt;
                return err(503, "backendError");
            }
            sess.buf.extend_from_slice(&body);
            if sess.buf.len() < sess.size {
                return range_308(sess.buf.len());
            }
            let data = Bytes::from(std::mem::take(&mut sess.buf));
            let kind = std::mem::replace(&mut sess.kind, SessKind::Replace(String::new()));
            let id = match kind {
                SessKind::Replace(id) => {
                    st.rewrite(&id, data);
                    id
                }
                SessKind::New(meta) => st.insert(
                    meta["id"].as_str().map(str::to_string),
                    meta["parents"][0].as_str().unwrap(),
                    meta["name"].as_str().unwrap(),
                    "application/octet-stream",
                    data,
                ),
            };
            st.sessions.get_mut(&sid).unwrap().done = Some(id.clone());
            (StatusCode::OK, axum::Json(st.json(&id))).into_response()
        }
        _ => panic!("mock: unhandled {method} {path}"),
    }
}

impl Mock {
    async fn start() -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let m = Mock(Arc::new(Mutex::new(St {
            base,
            files: BTreeMap::new(),
            seq: 0,
            clock: 1_790_000_000_000,
            log: Vec::new(),
            page_size: 1000,
            reverse_listing: true,
            sha256: true,
            keep_revisions: true,
            revisions_forbidden: false,
            sessions: HashMap::new(),
            valid_token: "tok".into(),
            faults: Vec::new(),
            requests: Vec::new(),
            interrupt_chunk: None,
            down_after_interrupt: false,
            session_down: false,
            chunk_puts: 0,
            quota_limit: 1 << 30,
        })));
        let app = Router::new().fallback(handle).with_state(m.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        m
    }

    fn base(&self) -> String {
        self.0.lock().base.clone()
    }

    fn mkdir(&self, parent: &str, name: &str) -> String {
        self.0
            .lock()
            .insert(None, parent, name, FOLDER, Bytes::new())
    }

    fn put(&self, parent: &str, name: &str, data: &[u8]) -> String {
        self.0.lock().insert(
            None,
            parent,
            name,
            "application/octet-stream",
            Bytes::copy_from_slice(data),
        )
    }

    fn data(&self, id: &str) -> Vec<u8> {
        self.0.lock().files[id].data.to_vec()
    }

    fn head(&self, id: &str) -> String {
        self.0.lock().files[id].head_rev.clone()
    }

    fn fault(
        &self,
        method: Option<&'static str>,
        path_has: &str,
        status: u16,
        reason: &'static str,
    ) {
        self.fault_n(method, path_has, status, reason, None, 1);
    }

    fn fault_n(
        &self,
        method: Option<&'static str>,
        path_has: &str,
        status: u16,
        reason: &'static str,
        retry_after: Option<u64>,
        times: usize,
    ) {
        self.0.lock().faults.push(Fault {
            method,
            path_has: path_has.into(),
            status,
            reason,
            retry_after,
            times,
        });
    }

    fn requests(&self) -> Vec<Req> {
        self.0.lock().requests.clone()
    }

    fn count_path(&self, method: &str, path: &str, query: Option<(&str, &str)>) -> usize {
        self.requests()
            .iter()
            .filter(|r| {
                r.method == method
                    && r.path == path
                    && query.is_none_or(|(k, v)| r.query.get(k).map(String::as_str) == Some(v))
            })
            .count()
    }

    fn status_queries(&self) -> usize {
        self.requests()
            .iter()
            .filter(|r| {
                r.content_range
                    .as_deref()
                    .is_some_and(|c| c.starts_with("bytes */"))
            })
            .count()
    }

    fn count(&self, method: &str, path_has: &str, query: Option<(&str, &str)>) -> usize {
        self.requests()
            .iter()
            .filter(|r| {
                r.method == method
                    && r.path.contains(path_has)
                    && query.is_none_or(|(k, v)| r.query.get(k).map(String::as_str) == Some(v))
            })
            .count()
    }
}

// ---------------------------------------------------------------- helpers

struct Env {
    mock: Mock,
    be: GDriveBackend,
    _state: tempfile::TempDir,
    state_dir: PathBuf,
}

async fn env_with(tune: impl FnOnce(&mut GDriveConfig)) -> Env {
    let mock = Mock::start().await;
    let state = tempfile::tempdir().unwrap();
    let mut cfg = GDriveConfig::new(state.path());
    cfg.api_base = mock.base();
    tune(&mut cfg);
    let be = GDriveBackend::new(Arc::new(StaticToken::new("tok")), cfg).unwrap();
    Env {
        mock,
        be,
        state_dir: state.path().to_path_buf(),
        _state: state,
    }
}

async fn env() -> Env {
    env_with(|_| {}).await
}

struct Src {
    _dir: tempfile::TempDir,
    path: PathBuf,
    size: u64,
    sha256: [u8; 32],
}

fn src(bytes: &[u8]) -> Src {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("src.bin");
    std::fs::write(&path, bytes).unwrap();
    Src {
        _dir: dir,
        path,
        size: bytes.len() as u64,
        sha256: Sha256::digest(bytes).into(),
    }
}

impl Src {
    fn source(&self) -> UploadSource<'_> {
        UploadSource::File {
            path: &self.path,
            size: self.size,
            sha256: self.sha256,
        }
    }
}

fn op() -> OpCtx {
    OpCtx {
        op_id: "op-1".into(),
        resume: None,
    }
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[1] ^ seed)
        .collect()
}

fn name(n: &RNode) -> String {
    String::from_utf8_lossy(&n.name_raw).into_owned()
}

async fn list_all(be: &GDriveBackend, roots: &[String]) -> Vec<RNode> {
    let mut out = Vec::new();
    be.list_all(roots, &mut |n| out.push(n)).await.unwrap();
    out
}

/// Parents are emitted before their children; returns pid → node.
fn check_order(nodes: &[RNode], roots: &[String]) -> HashMap<String, RNode> {
    let mut seen: HashSet<String> = roots.iter().cloned().collect();
    for n in nodes {
        let p = n.parent.clone().unwrap();
        assert!(seen.contains(&p), "{} listed before its parent", name(n));
        assert!(seen.insert(n.pid.clone()), "{} listed twice", name(n));
    }
    nodes.iter().map(|n| (n.pid.clone(), n.clone())).collect()
}

async fn drain(be: &GDriveBackend, mut c: Cursor) -> (Vec<ChangeItem>, Cursor, usize) {
    let mut all = Vec::new();
    let mut pages = 0;
    loop {
        match be.changes(&c).await.unwrap() {
            ChangePage::Page { items, next, more } => {
                pages += 1;
                all.extend(items);
                c = next;
                if !more {
                    return (all, c, pages);
                }
            }
            ChangePage::CursorLost => panic!("cursor lost"),
        }
    }
}

/// A library: two roots (one nested under an unrelated folder), a case variant and a
/// trashed copy that are not roots, and a stray file outside.
struct Lib {
    roots: Vec<String>,
    inside: Vec<String>,
    outside: Vec<String>,
}

fn library(m: &Mock) -> Lib {
    let r1 = m.mkdir("root", "mokuro-reader");
    let other = m.mkdir("root", "Other");
    let r2 = m.mkdir(&other, "mokuro-reader");
    let variant = m.mkdir("root", "Mokuro-Reader");
    let trashed = m.mkdir("root", "mokuro-reader");
    m.0.lock().files.get_mut(&trashed).unwrap().trashed = true;
    let sa = m.mkdir(&r1, "Series A");
    let v1 = m.put(&sa, "Vol 1.cbz", b"cbz-1");
    let v1m = m.put(&sa, "Vol 1.mokuro", b"{}");
    let sb = m.mkdir(&r2, "Series B");
    let deep = m.mkdir(&sb, "Extras");
    let v2 = m.put(&deep, "Vol 2.cbz", b"cbz-2");
    let stray = m.put(&other, "stray.txt", b"x");
    let in_variant = m.put(&variant, "nope.cbz", b"n");
    let in_trash = m.put(&trashed, "gone.cbz", b"g");
    Lib {
        roots: vec![r1, r2],
        inside: vec![sa, v1, v1m, sb, deep, v2],
        outside: vec![other, variant, trashed, stray, in_variant, in_trash],
    }
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn locate_roots_finds_every_exact_visible_folder_oldest_first() {
    let e = env().await;
    let lib = library(&e.mock);
    e.mock.0.lock().page_size = 2;
    let roots = e.be.locate_roots("mokuro-reader").await.unwrap();
    let pids: Vec<_> = roots.iter().map(|r| r.pid.clone()).collect();
    assert_eq!(
        pids, lib.roots,
        "exact name, not trashed, any parent, oldest first"
    );
    assert!(roots.iter().all(|r| r.is_dir && !r.trashed));
    assert_eq!(roots[0].parent.as_deref(), Some("root"));
    assert!(roots[0].created_ms < roots[1].created_ms);
    // Paged: 3 live folder candidates (the mock's `name =` ignores case), 2 per page.
    let lists = e.mock.count("GET", "/drive/v3/files", None);
    assert_eq!(lists, 2);
    assert_eq!(e.be.units_used(), 2 * units::LIST);

    let made = e.be.create_root("mokuro-reader").await.unwrap();
    assert_eq!(made.parent.as_deref(), Some("root"));
    assert_eq!(e.be.locate_roots("mokuro-reader").await.unwrap().len(), 3);
}

#[tokio::test]
async fn list_all_keeps_descendants_only_buffering_orphans_across_pages() {
    let e = env().await;
    let lib = library(&e.mock);
    // Newest first, two per page: every child arrives before its parent.
    e.mock.0.lock().page_size = 2;
    let nodes = list_all(&e.be, &lib.roots).await;
    let by = check_order(&nodes, &lib.roots);
    let mut got: Vec<_> = by.keys().cloned().collect();
    got.sort();
    let mut want = lib.inside.clone();
    want.sort();
    assert_eq!(got, want);
    for o in &lib.outside {
        assert!(!by.contains_key(o));
    }
    let v1 = &by[&lib.inside[1]];
    assert_eq!(name(v1), "Vol 1.cbz");
    assert_eq!(v1.parent.as_deref(), Some(lib.inside[0].as_str()));
    assert_eq!(v1.size, 5);
    assert_eq!(v1.cver, hex_md5(b"cbz-1"));
    assert_eq!(v1.hash, Some((HashKind::Sha256, hex_sha256(b"cbz-1"))));
    assert_eq!(v1.version, "1");
    assert_eq!(
        v1.pin.as_deref(),
        Some(e.mock.head(&lib.inside[1]).as_str())
    );
    assert!(v1.base_mtime_ms > 0 && v1.created_ms == v1.base_mtime_ms);
    let sa = &by[&lib.inside[0]];
    assert!(sa.is_dir);
    assert_eq!(sa.cver, sa.version, "a folder's cver is its version");
    // 12 owned live files at 2 per page.
    let q = e.mock.requests();
    let pages: Vec<_> = q
        .iter()
        .filter(|r| {
            r.query.get("q").map(String::as_str) == Some("'me' in owners and trashed=false")
        })
        .collect();
    assert_eq!(pages.len(), 6);
    assert!(pages.iter().all(|r| r.query["pageSize"] == "1000"));
    assert!(pages[0].query["fields"].contains("sha256Checksum"));
}

#[tokio::test]
async fn list_all_spills_orphans_to_disk_past_the_bound() {
    let e = env_with(|c| c.max_orphans = 1).await;
    let lib = library(&e.mock);
    e.mock.0.lock().page_size = 3;
    let mut nodes = Vec::new();
    let mut spilled = false;
    let dir = e.state_dir.clone();
    e.be.list_all(&lib.roots, &mut |n| {
        spilled |= std::fs::read_dir(&dir).unwrap().next().is_some();
        nodes.push(n);
    })
    .await
    .unwrap();
    assert!(spilled, "the spill file existed while the listing ran");
    let by = check_order(&nodes, &lib.roots);
    let mut got: Vec<_> = by.keys().cloned().collect();
    got.sort();
    let mut want = lib.inside.clone();
    want.sort();
    assert_eq!(got, want);
    let left: Vec<_> = std::fs::read_dir(&e.state_dir).unwrap().collect();
    assert!(left.is_empty(), "spill files removed: {left:?}");
}

#[tokio::test]
async fn md5_is_the_hash_when_drive_has_no_sha256() {
    let e = env().await;
    e.mock.0.lock().sha256 = false;
    let r = e.mock.mkdir("root", "mokuro-reader");
    let f = e.mock.put(&r, "a.cbz", b"abc");
    let n = e.be.stat(&f).await.unwrap().unwrap();
    assert_eq!(n.hash, Some((HashKind::Md5, hex_md5(b"abc"))));
}

#[tokio::test]
async fn changes_page_and_report_removed_and_trashed_files() {
    let e = env().await;
    let r = e.mock.mkdir("root", "mokuro-reader");
    let keep = e.mock.put(&r, "keep.cbz", b"1");
    let trash = e.mock.put(&r, "trash.cbz", b"2");
    let del = e.mock.put(&r, "del.cbz", b"3");
    let c0 = e.be.start_cursor().await.unwrap();
    e.mock.0.lock().page_size = 2;
    let added = e.mock.put(&r, "new.cbz", b"4");
    e.mock.0.lock().rewrite(&keep, Bytes::from_static(b"1b"));
    e.mock.0.lock().files.get_mut(&trash).unwrap().trashed = true;
    e.mock.0.lock().log.push(trash.clone());
    e.mock.0.lock().delete(&del);

    let (items, c1, pages) = drain(&e.be, c0).await;
    assert_eq!(pages, 2);
    assert_eq!(items.len(), 4);
    match &items[0] {
        ChangeItem::Upsert(n) => {
            assert_eq!(n.pid, added);
            assert_eq!(n.parent.as_deref(), Some(r.as_str()));
        }
        other => panic!("{other:?}"),
    }
    match &items[1] {
        ChangeItem::Upsert(n) => {
            assert_eq!(n.pid, keep);
            assert_eq!(n.cver, hex_md5(b"1b"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(items[2], ChangeItem::Removed { pid: trash });
    assert_eq!(items[3], ChangeItem::Removed { pid: del });
    let req = e.mock.requests();
    let ch: Vec<_> = req
        .iter()
        .filter(|r| r.path == "/drive/v3/changes")
        .collect();
    assert!(ch.iter().all(|r| r.query["pageSize"] == "1000"));

    // Caught up: an empty page at the same token.
    match e.be.changes(&c1).await.unwrap() {
        ChangePage::Page { items, next, more } => {
            assert!(items.is_empty() && !more);
            assert_eq!(next, c1);
        }
        other => panic!("{other:?}"),
    }
    // Invalid tokens (400 and 404) mean a full resync.
    assert_eq!(
        e.be.changes(&Cursor("garbage".into())).await.unwrap(),
        ChangePage::CursorLost
    );
    assert_eq!(
        e.be.changes(&Cursor("t99999".into())).await.unwrap(),
        ChangePage::CursorLost
    );
    // A throttle on the feed is not a lost cursor.
    e.mock.fault(
        Some("GET"),
        "/drive/v3/changes",
        403,
        "userRateLimitExceeded",
    );
    assert!(matches!(
        e.be.changes(&c1).await,
        Err(BErr::Throttled(Some(_)))
    ));
}

#[tokio::test]
async fn ranged_reads_send_http_inclusive_ranges() {
    let e = env().await;
    let r = e.mock.mkdir("root", "mokuro-reader");
    let data = pattern(1000, 7);
    let f = e.mock.put(&r, "a.cbz", &data);
    let body = read_to_vec(e.be.read(&f, None, Some((10, Some(19)))).await.unwrap())
        .await
        .unwrap();
    assert_eq!(body, &data[10..20]);
    let tail = read_to_vec(e.be.read(&f, None, Some((990, None))).await.unwrap())
        .await
        .unwrap();
    assert_eq!(tail, &data[990..]);
    let all = read_to_vec(e.be.read(&f, None, None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(all, data);
    assert!(matches!(
        e.be.read(&f, None, Some((5000, None))).await,
        Err(BErr::Fatal(_))
    ));
    assert!(matches!(
        e.be.read("missing", None, None).await,
        Err(BErr::NotFound)
    ));
    let r = e.mock.requests();
    assert!(
        r.iter()
            .any(|q| q.query.get("alt").map(String::as_str) == Some("media"))
    );
}

#[tokio::test]
async fn pinned_reads_use_revisions_and_detect_a_changed_file() {
    let e = env().await;
    let r = e.mock.mkdir("root", "mokuro-reader");
    let f = e.mock.put(&r, "a.cbz", b"version one");
    let pin1 = e.mock.head(&f);

    // Pinned through revisions/{rev}, with the range.
    let got = read_to_vec(
        e.be.read(&f, Some(&pin1), Some((0, Some(6))))
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(got, b"version");
    assert_eq!(e.mock.count("GET", &format!("/revisions/{pin1}"), None), 1);

    // After a rewrite, the old pin still reads the old bytes (Drive keeps revisions).
    e.mock
        .0
        .lock()
        .rewrite(&f, Bytes::from_static(b"version two!"));
    let got = read_to_vec(e.be.read(&f, Some(&pin1), None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got, b"version one");

    // The revision is gone: a stale pin fails before any download.
    e.mock.0.lock().keep_revisions = false;
    e.mock
        .0
        .lock()
        .rewrite(&f, Bytes::from_static(b"version three"));
    let media_path = format!("/drive/v3/files/{f}");
    let before = e
        .mock
        .count_path("GET", &media_path, Some(("alt", "media")));
    assert!(matches!(
        e.be.read(&f, Some(&pin1), None).await,
        Err(BErr::PreconditionFailed)
    ));
    let after = e
        .mock
        .count_path("GET", &media_path, Some(("alt", "media")));
    assert_eq!(before, after);

    // Revisions refused (drive.file): alt=media, then a re-stat at the end.
    e.mock.0.lock().revisions_forbidden = true;
    let head = e.mock.head(&f);
    let got = read_to_vec(e.be.read(&f, Some(&head), None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got, b"version three");
    let stale = e.be.read(&f, Some(&pin1), None).await.unwrap();
    let err = read_to_vec(stale).await.unwrap_err();
    let inner = err.get_ref().and_then(|e| e.downcast_ref::<BErr>());
    assert_eq!(inner, Some(&BErr::PreconditionFailed));
    // The 403 is remembered: no more revision requests.
    let revs = e.mock.count("GET", "/revisions/", None);
    let _ = read_to_vec(e.be.read(&f, Some(&head), None).await.unwrap()).await;
    assert_eq!(e.mock.count("GET", "/revisions/", None), revs);
}

#[tokio::test]
async fn small_uploads_are_multipart_with_a_preallocated_id() {
    let e = env().await;
    let r = e.mock.mkdir("root", "mokuro-reader");
    let s = src(b"hello sidecar");
    let mut o = op();
    let n =
        e.be.upload(
            UploadTarget::New {
                parent: &r,
                name: "a.mokuro",
            },
            s.source(),
            &mut o,
        )
        .await
        .unwrap();
    assert!(n.pid.starts_with('g'), "the generateIds id: {}", n.pid);
    assert_eq!(n.parent.as_deref(), Some(r.as_str()));
    assert_eq!(name(&n), "a.mokuro");
    assert_eq!(n.cver, hex_md5(b"hello sidecar"));
    assert_eq!(n.hash, Some((HashKind::Sha256, hex::encode(s.sha256))));
    assert_eq!(e.mock.data(&n.pid), b"hello sidecar");
    assert_eq!(
        e.mock.count(
            "POST",
            "/upload/drive/v3/files",
            Some(("uploadType", "multipart"))
        ),
        1
    );
    assert!(o.resume.as_deref().unwrap().contains(&n.pid));

    // A retry with the same op (the response was lost) stats the id and stops there.
    let again =
        e.be.upload(
            UploadTarget::New {
                parent: &r,
                name: "a.mokuro",
            },
            s.source(),
            &mut o,
        )
        .await
        .unwrap();
    assert_eq!(again.pid, n.pid);
    assert_eq!(e.mock.count("POST", "/upload/", None), 1);
    assert_eq!(e.mock.count("GET", "generateIds", None), 1);
}

#[tokio::test]
async fn large_uploads_are_resumable_and_survive_an_interrupted_chunk() {
    let e = env_with(|c| c.chunk_size = 256 << 10).await;
    let r = e.mock.mkdir("root", "mokuro-reader");
    let data = pattern((5 << 20) + 300_000, 3);
    let s = src(&data);
    e.mock.0.lock().interrupt_chunk = Some(4);
    let mut o = op();
    let n =
        e.be.upload(
            UploadTarget::New {
                parent: &r,
                name: "big.cbz",
            },
            s.source(),
            &mut o,
        )
        .await
        .unwrap();
    assert_eq!(e.mock.data(&n.pid), data);
    assert_eq!(n.size, data.len() as u64);
    assert_eq!(
        e.mock.count(
            "POST",
            "/upload/drive/v3/files",
            Some(("uploadType", "resumable"))
        ),
        1
    );
    assert_eq!(
        e.mock
            .count("POST", "/upload/", Some(("uploadType", "multipart"))),
        0
    );
    let resume = o.resume.clone().unwrap();
    assert!(resume.contains("upload_id"), "session persisted: {resume}");
    // 22 chunks' worth, resumed mid-way after one status query.
    assert_eq!(e.mock.status_queries(), 1);
    assert!(e.mock.count("PUT", "/upload/", None) > 22);
}

#[tokio::test]
async fn a_failed_upload_resumes_its_session_on_the_next_call() {
    let e = env_with(|c| c.chunk_size = 256 << 10).await;
    let r = e.mock.mkdir("root", "mokuro-reader");
    let data = pattern((6 << 20) + 1, 9);
    let s = src(&data);
    let mut o = op();
    // Chunk 6 half-lands, then the session endpoint is down (the status query too):
    // the call gives up with the session kept.
    {
        let mut st = e.mock.0.lock();
        st.interrupt_chunk = Some(6);
        st.down_after_interrupt = true;
    }
    let res =
        e.be.upload(
            UploadTarget::New {
                parent: &r,
                name: "big.cbz",
            },
            s.source(),
            &mut o,
        )
        .await;
    assert!(matches!(res, Err(BErr::Transient(_))), "{res:?}");
    let resume = o.resume.clone().unwrap();
    assert!(resume.contains("upload_id"));
    e.mock.0.lock().session_down = false;
    let n =
        e.be.upload(
            UploadTarget::New {
                parent: &r,
                name: "big.cbz",
            },
            s.source(),
            &mut o,
        )
        .await
        .unwrap();
    assert_eq!(e.mock.data(&n.pid), data);
    assert_eq!(
        e.mock.count("POST", "/upload/", None),
        1,
        "one session only"
    );
}

#[tokio::test]
async fn replace_keeps_the_id_and_skips_identical_bytes() {
    let e = env_with(|c| c.chunk_size = 256 << 10).await;
    let r = e.mock.mkdir("root", "mokuro-reader");
    let f = e.mock.put(&r, "series.json", b"{\"v\":1}");
    let base = e.be.stat(&f).await.unwrap().unwrap();

    let s2 = src(b"{\"v\":2}");
    let n =
        e.be.upload(
            UploadTarget::Replace {
                pid: &f,
                if_cver: Some(&base.cver),
            },
            s2.source(),
            &mut op(),
        )
        .await
        .unwrap();
    assert_eq!(n.pid, f);
    assert_ne!(n.cver, base.cver);
    assert_eq!(e.mock.data(&f), b"{\"v\":2}");
    assert_eq!(
        e.mock.count(
            "PATCH",
            &format!("/upload/drive/v3/files/{f}"),
            Some(("uploadType", "multipart"))
        ),
        1
    );

    // Same bytes again: nothing is uploaded (no new revision), the node comes back.
    let patches = e.mock.count("PATCH", "/upload/", None);
    let same =
        e.be.upload(
            UploadTarget::Replace {
                pid: &f,
                if_cver: Some(&base.cver),
            },
            s2.source(),
            &mut op(),
        )
        .await
        .unwrap();
    assert_eq!(same.cver, n.cver);
    assert_eq!(e.mock.count("PATCH", "/upload/", None), patches);

    // And by md5 when Drive has no sha256 for the file.
    e.mock.0.lock().sha256 = false;
    e.be.upload(
        UploadTarget::Replace {
            pid: &f,
            if_cver: None,
        },
        s2.source(),
        &mut op(),
    )
    .await
    .unwrap();
    assert_eq!(e.mock.count("PATCH", "/upload/", None), patches);
    e.mock.0.lock().sha256 = true;

    // A stale base is refused.
    let s3 = src(b"{\"v\":3}");
    assert_eq!(
        e.be.upload(
            UploadTarget::Replace {
                pid: &f,
                if_cver: Some(&base.cver)
            },
            s3.source(),
            &mut op()
        )
        .await,
        Err(BErr::PreconditionFailed)
    );
    assert_eq!(e.mock.data(&f), b"{\"v\":2}");

    // Large replaces are resumable PATCH sessions on the same id.
    let big = pattern((5 << 20) + 10, 1);
    let sb = src(&big);
    let n =
        e.be.upload(
            UploadTarget::Replace {
                pid: &f,
                if_cver: Some(&same.cver),
            },
            sb.source(),
            &mut op(),
        )
        .await
        .unwrap();
    assert_eq!(n.pid, f);
    assert_eq!(e.mock.data(&f), big);
    assert_eq!(
        e.mock.count(
            "PATCH",
            &format!("/upload/drive/v3/files/{f}"),
            Some(("uploadType", "resumable"))
        ),
        1
    );

    // Replacing a folder or a missing file is NotFound.
    assert_eq!(
        e.be.upload(
            UploadTarget::Replace {
                pid: &r,
                if_cver: None
            },
            s3.source(),
            &mut op()
        )
        .await,
        Err(BErr::NotFound)
    );
    assert_eq!(
        e.be.upload(
            UploadTarget::Replace {
                pid: "nope",
                if_cver: None
            },
            s3.source(),
            &mut op()
        )
        .await,
        Err(BErr::NotFound)
    );
}

#[tokio::test]
async fn metadata_writes_and_queries() {
    let e = env().await;
    let r = e.mock.mkdir("root", "mokuro-reader");
    let a = e.be.mkdir(&r, "A", &op()).await.unwrap();
    let b = e.be.mkdir(&r, "B", &op()).await.unwrap();
    assert!(a.is_dir);
    let f = e.mock.put(&a.pid, "v.cbz", b"zz");
    let cur = e.be.stat(&f).await.unwrap().unwrap();

    // Move + rename keeps the id.
    let moved =
        e.be.update(&f, Some(&b.pid), Some("w.cbz"), Some(&cur.cver))
            .await
            .unwrap();
    assert_eq!(moved.pid, f);
    assert_eq!(moved.parent.as_deref(), Some(b.pid.as_str()));
    assert_eq!(name(&moved), "w.cbz");
    let req = e.mock.requests();
    let patch = req.iter().rfind(|q| q.method == "PATCH").unwrap();
    assert_eq!(patch.query["addParents"], b.pid);
    assert_eq!(patch.query["removeParents"], a.pid);
    // Case-only rename, no preflight.
    let gets = e.mock.count("GET", &format!("/drive/v3/files/{f}"), None);
    let renamed = e.be.update(&f, None, Some("W.cbz"), None).await.unwrap();
    assert_eq!(name(&renamed), "W.cbz");
    assert_eq!(
        e.mock.count("GET", &format!("/drive/v3/files/{f}"), None),
        gets
    );
    assert_eq!(
        e.be.update(&f, None, Some("x"), Some("stale")).await,
        Err(BErr::PreconditionFailed)
    );

    // find_named matches exactly (Drive's `name =` ignores case) and reports the clock.
    let (found, at) = e.be.find_named(&b.pid, "W.cbz").await.unwrap();
    assert_eq!(found.len(), 1);
    let (none, _) = e.be.find_named(&b.pid, "w.cbz").await.unwrap();
    assert!(none.is_empty());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!((at.0 - now).abs() < 5_000, "Date header: {at:?}");

    let copy = e.be.copy(&f, &a.pid, "copy.cbz").await.unwrap();
    assert_ne!(copy.pid, f);
    assert_eq!(e.mock.data(&copy.pid), b"zz");

    let mut kids = Vec::new();
    e.be.list_dir(&b.pid, &mut |n| kids.push(n)).await.unwrap();
    assert_eq!(kids.len(), 1);
    assert_eq!(
        e.be.trash(&f, Some("stale")).await,
        Err(BErr::PreconditionFailed)
    );
    e.be.trash(&f, None).await.unwrap();
    assert!(e.be.stat(&f).await.unwrap().unwrap().trashed);
    let mut kids = Vec::new();
    e.be.list_dir(&b.pid, &mut |n| kids.push(n)).await.unwrap();
    assert!(kids.is_empty());

    e.be.delete_permanent(&copy.pid).await.unwrap();
    assert_eq!(e.be.stat(&copy.pid).await.unwrap(), None);
    assert_eq!(e.be.delete_permanent(&copy.pid).await, Err(BErr::NotFound));

    let q = e.be.quota().await.unwrap().unwrap();
    assert_eq!(q.total, 1 << 30);
    assert_eq!(q.available, (1 << 30) - 2);
    let acct = e.be.account().await.unwrap();
    assert_eq!(acct.id, "perm-1");
    assert_eq!(acct.label, "me@example.com");
    assert_eq!(
        e.be.wait_change(&Cursor("t0".into()), Duration::from_millis(1))
            .await,
        Err(BErr::Unsupported)
    );
    assert!(!e.mock.requests().iter().any(|q| q.path.contains("revoke")));
}

/// Hands out `tok-0`, `tok-1`, … advancing on `invalidate`.
struct Rotating {
    n: AtomicUsize,
    invalidated: AtomicUsize,
}

impl TokenSourceDyn for Rotating {
    fn access_token(&self) -> Pin<Box<dyn Future<Output = Result<String, BErr>> + Send + '_>> {
        Box::pin(async move { Ok(format!("tok-{}", self.n.load(Ordering::SeqCst))) })
    }
    fn invalidate(&self) {
        self.n.fetch_add(1, Ordering::SeqCst);
        self.invalidated.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_401_refreshes_the_token_once() {
    let mock = Mock::start().await;
    let r = mock.mkdir("root", "mokuro-reader");
    mock.0.lock().valid_token = "tok-1".into();
    let tokens = Arc::new(Rotating {
        n: AtomicUsize::new(0),
        invalidated: AtomicUsize::new(0),
    });
    let state = tempfile::tempdir().unwrap();
    let mut cfg = GDriveConfig::new(state.path());
    cfg.api_base = mock.base();
    let be = GDriveBackend::new(tokens.clone(), cfg).unwrap();
    assert!(be.stat(&r).await.unwrap().is_some());
    assert_eq!(tokens.invalidated.load(Ordering::SeqCst), 1);
    assert!(be.stat(&r).await.unwrap().is_some());
    assert_eq!(tokens.invalidated.load(Ordering::SeqCst), 1);
    // Units count every request sent, the refused one included.
    assert_eq!(be.units_used(), 3 * units::GET);

    // The fresh token is refused too: authorization has expired.
    mock.0.lock().valid_token = "something-else".into();
    assert_eq!(be.stat(&r).await, Err(BErr::AuthExpired));
    assert_eq!(tokens.invalidated.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn errors_map_to_backend_errors() {
    let e = env().await;
    let r = e.mock.mkdir("root", "mokuro-reader");
    let f = e.mock.put(&r, "a.cbz", b"abc");
    let path = format!("/drive/v3/files/{f}");

    e.mock
        .fault_n(None, &path, 403, "rateLimitExceeded", Some(7), 1);
    assert_eq!(
        e.be.stat(&f).await,
        Err(BErr::Throttled(Some(Duration::from_secs(7))))
    );
    e.mock.fault(None, &path, 403, "userRateLimitExceeded");
    match e.be.stat(&f).await {
        Err(BErr::Throttled(Some(d))) => {
            assert!(d >= Duration::from_secs(1) && d < Duration::from_secs(70))
        }
        other => panic!("{other:?}"),
    }
    e.mock.fault(None, &path, 429, "rateLimitExceeded");
    assert!(matches!(e.be.stat(&f).await, Err(BErr::Throttled(Some(_)))));
    e.mock.fault(None, &path, 403, "dailyLimitExceeded");
    assert_eq!(
        e.be.stat(&f).await,
        Err(BErr::Throttled(Some(Duration::from_secs(3600))))
    );
    e.mock.fault(None, "/upload/", 403, "storageQuotaExceeded");
    let s = src(b"new bytes");
    assert_eq!(
        e.be.upload(
            UploadTarget::Replace {
                pid: &f,
                if_cver: None
            },
            s.source(),
            &mut op()
        )
        .await,
        Err(BErr::StorageFull)
    );
    e.mock.fault(None, &path, 403, "insufficientPermissions");
    assert_eq!(e.be.stat(&f).await, Err(BErr::AuthRevoked));
    e.mock.fault(None, &path, 403, "appNotAuthorized");
    assert_eq!(e.be.stat(&f).await, Err(BErr::AuthRevoked));
    e.mock.fault(None, &path, 403, "appNotAuthorizedToFile");
    assert_eq!(e.be.stat(&f).await, Ok(None));
    e.mock.fault(None, &path, 412, "conditionNotMet");
    assert_eq!(e.be.trash(&f, None).await, Err(BErr::PreconditionFailed));
    e.mock.fault(None, &path, 500, "backendError");
    assert!(matches!(e.be.stat(&f).await, Err(BErr::Transient(_))));
    e.mock.fault(None, &path, 503, "backendError");
    assert!(matches!(
        e.be.read(&f, None, None).await,
        Err(BErr::Transient(_))
    ));
    e.mock.fault(Some("PATCH"), &path, 404, "notFound");
    assert_eq!(e.be.trash(&f, None).await, Err(BErr::NotFound));
    assert_eq!(e.be.stat("missing").await, Ok(None));
    assert_eq!(
        e.be.update("missing", None, Some("x"), None).await,
        Err(BErr::NotFound)
    );
    // Everything still works afterwards.
    assert!(e.be.stat(&f).await.unwrap().is_some());
}

#[tokio::test]
async fn unit_costs_follow_the_limits_page() {
    let e = env().await;
    let r = e.mock.mkdir("root", "mokuro-reader");
    let f = e.mock.put(&r, "a.cbz", b"abc");
    let u0 = e.be.units_used();
    e.be.stat(&f).await.unwrap();
    assert_eq!(e.be.units_used() - u0, units::GET);
    let u0 = e.be.units_used();
    let mut v = Vec::new();
    e.be.list_dir(&r, &mut |n| v.push(n)).await.unwrap();
    assert_eq!(e.be.units_used() - u0, units::LIST);
    let u0 = e.be.units_used();
    read_to_vec(e.be.read(&f, None, None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(e.be.units_used() - u0, units::DOWNLOAD);
    let u0 = e.be.units_used();
    e.be.update(&f, None, Some("b.cbz"), None).await.unwrap();
    assert_eq!(e.be.units_used() - u0, units::UPDATE);
    let u0 = e.be.units_used();
    e.be.mkdir(&r, "d", &op()).await.unwrap();
    assert_eq!(e.be.units_used() - u0, units::CREATE);
    let u0 = e.be.units_used();
    let c = e.be.start_cursor().await.unwrap();
    e.be.changes(&c).await.unwrap();
    assert_eq!(e.be.units_used() - u0, units::GET + units::CHANGES);
}
