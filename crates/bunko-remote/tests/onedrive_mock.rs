//! OneDriveBackend against an in-memory mock of the Microsoft Graph v1.0 endpoints it
//! uses (plus the pre-authenticated download, upload-session and copy-monitor hosts).
#![cfg(feature = "onedrive")]

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
use bunko_remote::onedrive::api::units;
use bunko_remote::onedrive::quickxor::quickxor_base64;
use bunko_remote::onedrive::{
    FRAGMENT_MULTIPLE, OneDriveBackend, OneDriveConfig, StaticToken, TokenSourceDyn,
};
use futures_util::StreamExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------- the mock

const ROOT: &str = "ROOT";

struct MItem {
    name: String,
    parent: Option<String>,
    folder: bool,
    data: Bytes,
    created: i64,
    modified: i64,
    etag_n: u64,
    ctag_n: u64,
    deleted: bool,
}

enum SessKind {
    New { parent: String, name: String },
    Replace(String),
}

struct Sess {
    kind: SessKind,
    size: usize,
    buf: Vec<u8>,
}

struct Monitor {
    polls_left: usize,
    src: String,
    parent: String,
    name: String,
}

#[derive(Debug, Clone)]
struct Req {
    method: String,
    /// Decoded, without the `/v1.0` prefix.
    path: String,
    query: HashMap<String, String>,
    auth: Option<String>,
    if_match: Option<String>,
    content_range: Option<String>,
    user_agent: Option<String>,
}

#[derive(Clone)]
struct Fault {
    method: Option<&'static str>,
    path_has: String,
    status: u16,
    code: &'static str,
    retry_after: Option<u64>,
    times: usize,
}

struct St {
    host: String,
    items: BTreeMap<String, MItem>,
    seq: u64,
    clock: i64,
    log: Vec<String>,
    /// Delta tokens below this answer 410.
    lost_below: usize,
    page_size: usize,
    sessions: HashMap<String, Sess>,
    valid_token: String,
    faults: Vec<Fault>,
    requests: Vec<Req>,
    /// Fail this (1-based) fragment PUT after taking half its bytes.
    interrupt_fragment: Option<usize>,
    down_after_interrupt: bool,
    session_down: bool,
    fragment_puts: usize,
    monitors: HashMap<String, Monitor>,
    send_ctag: bool,
    ignore_range: bool,
    permanent_delete: bool,
    quota_total: i64,
    /// A pre-authenticated host (download, upload session, monitor) saw a token.
    preauth_saw_auth: bool,
}

#[derive(Clone)]
struct Mock(Arc<Mutex<St>>);

fn rfc3339(ms: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000).unwrap();
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

fn err(status: u16, code: &str) -> Response {
    let body = json!({"error": {"code": code, "message": code, "innerError": {"code": code}}});
    (StatusCode::from_u16(status).unwrap(), axum::Json(body)).into_response()
}

fn ok_json(v: Value) -> Response {
    axum::Json(v).into_response()
}

fn status_json(s: u16, v: Value) -> Response {
    (StatusCode::from_u16(s).unwrap(), axum::Json(v)).into_response()
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = std::str::from_utf8(&b[i + 1..i + 3]).unwrap();
            out.push(u8::from_str_radix(h, 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn bare(t: &str) -> &str {
    t.trim().trim_matches('"')
}

impl St {
    fn base(&self) -> String {
        format!("{}/v1.0", self.host)
    }

    fn tick(&mut self) -> i64 {
        self.clock += 1000;
        self.clock
    }

    fn new_id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("{prefix}{:05}", self.seq)
    }

    fn etag(&self, id: &str) -> String {
        format!("\"{{{id}}},{}\"", self.items[id].etag_n)
    }

    fn ctag(&self, id: &str) -> String {
        format!("\"c:{{{id}}},{}\"", self.items[id].ctag_n)
    }

    fn live(&self, id: &str) -> bool {
        self.items.get(id).is_some_and(|i| !i.deleted)
    }

    fn json(&self, id: &str) -> Value {
        let f = &self.items[id];
        if f.deleted {
            // Consumer accounts: no name on a delete.
            return json!({"id": id, "deleted": {"state": "deleted"},
                "parentReference": {"id": f.parent, "driveId": "drive1"}});
        }
        let mut v = json!({
            "id": id, "name": f.name, "eTag": self.etag(id), "cTag": self.ctag(id),
            "createdDateTime": rfc3339(f.created),
            "lastModifiedDateTime": rfc3339(f.modified),
            // Client-settable and deliberately different: the driver must not use it.
            "fileSystemInfo": {"lastModifiedDateTime": rfc3339(f.modified - 31_536_000_000)},
        });
        if let Some(p) = &f.parent {
            v["parentReference"] = json!({"id": p, "driveId": "drive1"});
        } else {
            v["root"] = json!({});
        }
        if f.folder {
            v["folder"] = json!({"childCount": self.kids(id).len()});
            v["size"] = json!(0);
        } else {
            v["size"] = json!(f.data.len());
            v["file"] = json!({"mimeType": "application/octet-stream",
                "hashes": {"quickXorHash": quickxor_base64(&f.data)}});
        }
        v
    }

    fn kids(&self, id: &str) -> Vec<String> {
        self.items
            .iter()
            .filter(|(_, f)| f.parent.as_deref() == Some(id) && !f.deleted)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// OneDrive is case-insensitive: the live child of `parent` folding to `name`.
    fn child(&self, parent: &str, name: &str) -> Option<String> {
        self.kids(parent)
            .into_iter()
            .find(|k| self.items[k].name.to_lowercase() == name.to_lowercase())
    }

    /// Log a change of `id`; its ancestors show up in delta too.
    fn touch(&mut self, id: &str) {
        self.log.push(id.to_string());
        let mut cur = self.items[id].parent.clone();
        while let Some(p) = cur {
            self.log.push(p.clone());
            cur = self.items[&p].parent.clone();
        }
    }

    fn insert(&mut self, parent: &str, name: &str, folder: bool, data: Bytes) -> String {
        let id = self.new_id(if folder { "D" } else { "F" });
        let now = self.tick();
        self.items.insert(
            id.clone(),
            MItem {
                name: name.into(),
                parent: Some(parent.into()),
                folder,
                data,
                created: now,
                modified: now,
                etag_n: 1,
                ctag_n: 1,
                deleted: false,
            },
        );
        self.touch(&id);
        id
    }

    fn rewrite(&mut self, id: &str, data: Bytes) {
        let now = self.tick();
        let f = self.items.get_mut(id).unwrap();
        f.data = data;
        f.etag_n += 1;
        f.ctag_n += 1;
        f.modified = now;
        self.touch(id);
    }

    fn delete(&mut self, id: &str) {
        for k in self.kids(id) {
            self.delete(&k);
        }
        self.items.get_mut(id).unwrap().deleted = true;
        self.log.push(id.to_string());
    }

    /// `If-Match` against the eTag or the cTag.
    fn matches(&self, id: &str, if_match: Option<&str>) -> bool {
        if_match.is_none_or(|t| bare(t) == bare(&self.etag(id)) || bare(t) == bare(&self.ctag(id)))
    }
}

fn media(data: &Bytes, headers: &HeaderMap, ignore_range: bool) -> Response {
    let r = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let Some(r) = r.filter(|_| !ignore_range) else {
        return (StatusCode::OK, data.clone()).into_response();
    };
    let spec = r.strip_prefix("bytes=").unwrap();
    let (a, b) = spec.split_once('-').unwrap();
    let a: usize = a.parse().unwrap();
    if a >= data.len() {
        return err(416, "invalidRange");
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

fn hdr(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
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
    let raw_path = uri.path().to_string();
    let graph = raw_path.starts_with("/v1.0/");
    let segs: Vec<String> = raw_path
        .trim_start_matches("/v1.0")
        .trim_start_matches('/')
        .split('/')
        .map(pct_decode)
        .collect();
    let path = format!("/{}", segs.join("/"));
    let auth = hdr(&headers, header::AUTHORIZATION);
    let if_match = hdr(&headers, header::IF_MATCH);
    st.requests.push(Req {
        method: method.to_string(),
        path: path.clone(),
        query: q.clone(),
        auth: auth.clone(),
        if_match: if_match.clone(),
        content_range: hdr(&headers, header::CONTENT_RANGE),
        user_agent: hdr(&headers, header::USER_AGENT),
    });
    if graph {
        if auth.as_deref() != Some(&format!("Bearer {}", st.valid_token)) {
            return err(401, "InvalidAuthenticationToken");
        }
    } else if auth.is_some() {
        st.preauth_saw_auth = true;
        return err(401, "tokenNotExpected");
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
        let mut r = err(f.status, f.code);
        if let Some(s) = f.retry_after {
            r.headers_mut()
                .insert(header::RETRY_AFTER, s.to_string().parse().unwrap());
        }
        return r;
    }
    let base = st.base();
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    if !graph {
        return match (method.as_str(), segs.as_slice()) {
            ("GET", ["preauth", id]) => {
                if !st.live(id) {
                    return err(404, "itemNotFound");
                }
                let mut r = media(&st.items[*id].data.clone(), &headers, st.ignore_range);
                let etag = st.etag(id);
                r.headers_mut().insert(header::ETAG, etag.parse().unwrap());
                if st.send_ctag {
                    let ctag = st.ctag(id);
                    r.headers_mut().insert("ctag", ctag.parse().unwrap());
                }
                r
            }
            ("GET", ["upload", sid]) => match st.sessions.get(*sid) {
                Some(s) => ok_json(json!({
                    "expirationDateTime": "2099-01-01T00:00:00Z",
                    "nextExpectedRanges": [format!("{}-{}", s.buf.len(), s.size - 1)],
                })),
                None => err(404, "itemNotFound"),
            },
            ("PUT", ["upload", sid]) => upload_fragment(&mut st, sid, &headers, &body),
            ("GET", ["monitor", mid]) => {
                let Some(mon) = st.monitors.get_mut(*mid) else {
                    return err(404, "itemNotFound");
                };
                if mon.polls_left > 0 {
                    mon.polls_left -= 1;
                    return status_json(
                        202,
                        json!({"operation": "itemCopy", "status": "inProgress", "percentageComplete": 50.0}),
                    );
                }
                let mon = st.monitors.remove(*mid).unwrap();
                if st.child(&mon.parent, &mon.name).is_some() {
                    return ok_json(
                        json!({"status": "failed", "error": {"code": "nameAlreadyExists", "message": "exists"}}),
                    );
                }
                let (folder, data) = {
                    let s = &st.items[&mon.src];
                    (s.folder, s.data.clone())
                };
                let id = st.insert(&mon.parent, &mon.name, folder, data);
                ok_json(json!({"status": "completed", "resourceId": id}))
            }
            _ => panic!("mock: unhandled {method} {raw_path}"),
        };
    }
    match (method.as_str(), segs.as_slice()) {
        ("GET", ["me", "drive"]) => {
            let used: usize = st
                .items
                .values()
                .filter(|f| !f.deleted)
                .map(|f| f.data.len())
                .sum();
            ok_json(json!({
                "id": "drive1", "driveType": "personal",
                "owner": {"user": {"id": "user-1", "displayName": "Me", "email": "me@example.com"}},
                "quota": {"total": st.quota_total, "used": used, "remaining": st.quota_total - used as i64, "state": "normal"},
            }))
        }
        ("GET", ["me", "drive", "root:", name]) => match st.child(ROOT, name) {
            Some(id) => ok_json(st.json(&id)),
            None => err(404, "itemNotFound"),
        },
        ("POST", ["me", "drive", "root", "children"]) => mkdir(&mut st, ROOT, &body),
        ("GET", ["me", "drive", "root", "delta"]) => {
            let tok = q.get("token").cloned().unwrap_or_default();
            if tok == "latest" {
                return ok_json(json!({"value": [],
                    "@odata.deltaLink": format!("{base}/me/drive/root/delta?token={}", st.log.len())}));
            }
            let at: usize = tok.parse().unwrap();
            if at < st.lost_below || at > st.log.len() {
                let mut r = err(410, "resyncRequired");
                *r.body_mut() = axum::body::Body::from(
                    json!({"error": {"code": "resyncRequired", "message": "resync",
                        "innerError": {"code": "resyncChangesApplyDifferences"}}})
                    .to_string(),
                );
                r.headers_mut().insert(
                    header::LOCATION,
                    format!("{base}/me/drive/root/delta").parse().unwrap(),
                );
                return r;
            }
            let end = (at + st.page_size).min(st.log.len());
            let value: Vec<Value> = st.log[at..end].iter().map(|id| st.json(id)).collect();
            let mut v = json!({"value": value});
            let link = format!("{base}/me/drive/root/delta?token={end}");
            if end < st.log.len() {
                v["@odata.nextLink"] = json!(link);
            } else {
                v["@odata.deltaLink"] = json!(link);
            }
            ok_json(v)
        }
        // Path addressing: /me/drive/items/{parent}:/{name}[:/action]
        (m, ["me", "drive", "items", pref, rest @ ..]) if pref.ends_with(':') => {
            let parent = pref.trim_end_matches(':').to_string();
            let name = rest[0].trim_end_matches(':').to_string();
            let action = rest.get(1).copied();
            if !st.live(&parent) {
                return err(404, "itemNotFound");
            }
            match (m, action) {
                ("GET", None) => match st.child(&parent, &name) {
                    Some(id) => ok_json(st.json(&id)),
                    None => err(404, "itemNotFound"),
                },
                ("PUT", Some("content")) => {
                    assert_eq!(
                        q.get("@microsoft.graph.conflictBehavior")
                            .map(String::as_str),
                        Some("fail")
                    );
                    if st.child(&parent, &name).is_some() {
                        return err(409, "nameAlreadyExists");
                    }
                    let id = st.insert(&parent, &name, false, body);
                    status_json(201, st.json(&id))
                }
                ("POST", Some("createUploadSession")) => {
                    let b: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(b["item"]["@microsoft.graph.conflictBehavior"], "fail");
                    if st.child(&parent, &name).is_some() {
                        return err(409, "nameAlreadyExists");
                    }
                    new_session(&mut st, SessKind::New { parent, name }, &headers)
                }
                _ => panic!("mock: unhandled {method} {raw_path}"),
            }
        }
        ("GET", ["me", "drive", "items", id]) => {
            if !st.items.contains_key(*id) || st.items[*id].deleted {
                return err(404, "itemNotFound");
            }
            ok_json(st.json(id))
        }
        ("PATCH", ["me", "drive", "items", id]) => {
            let id = id.to_string();
            if !st.live(&id) {
                return err(404, "itemNotFound");
            }
            if !st.matches(&id, if_match.as_deref()) {
                return err(412, "preconditionFailed");
            }
            let b: Value = serde_json::from_slice(&body).unwrap();
            let new_parent = b["parentReference"]["id"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| st.items[&id].parent.clone().unwrap());
            let new_name = b["name"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| st.items[&id].name.clone());
            if !st.live(&new_parent) {
                return err(404, "itemNotFound");
            }
            if st.child(&new_parent, &new_name).is_some_and(|c| c != id) {
                return err(409, "nameAlreadyExists");
            }
            let f = st.items.get_mut(&id).unwrap();
            f.parent = Some(new_parent);
            f.name = new_name;
            f.etag_n += 1;
            st.touch(&id);
            ok_json(st.json(&id))
        }
        ("DELETE", ["me", "drive", "items", id]) => {
            let id = id.to_string();
            if !st.live(&id) {
                return err(404, "itemNotFound");
            }
            if !st.matches(&id, if_match.as_deref()) {
                return err(412, "preconditionFailed");
            }
            st.delete(&id);
            StatusCode::NO_CONTENT.into_response()
        }
        ("POST", ["me", "drive", "items", id, "permanentDelete"]) => {
            if !st.permanent_delete {
                return err(400, "invalidRequest");
            }
            let id = id.to_string();
            if !st.live(&id) {
                return err(404, "itemNotFound");
            }
            st.delete(&id);
            StatusCode::NO_CONTENT.into_response()
        }
        ("GET", ["me", "drive", "items", id, "children"]) => {
            let id = id.to_string();
            if !st.live(&id) {
                return err(404, "itemNotFound");
            }
            assert!(q.contains_key("$top"));
            let kids = st.kids(&id);
            let at: usize = q.get("$skiptoken").map_or(0, |s| s.parse().unwrap());
            let end = (at + st.page_size).min(kids.len());
            let value: Vec<Value> = kids[at..end].iter().map(|k| st.json(k)).collect();
            let mut v = json!({"value": value});
            if end < kids.len() {
                v["@odata.nextLink"] = json!(format!(
                    "{base}/me/drive/items/{id}/children?$top=2&$skiptoken={end}"
                ));
            }
            ok_json(v)
        }
        ("POST", ["me", "drive", "items", id, "children"]) => {
            let id = id.to_string();
            if !st.live(&id) {
                return err(404, "itemNotFound");
            }
            mkdir(&mut st, &id, &body)
        }
        ("GET", ["me", "drive", "items", id, "content"]) => {
            if !st.live(id) || st.items[*id].folder {
                return err(404, "itemNotFound");
            }
            Response::builder()
                .status(302)
                .header(header::LOCATION, format!("{}/preauth/{id}", st.host))
                .body(axum::body::Body::empty())
                .unwrap()
        }
        ("PUT", ["me", "drive", "items", id, "content"]) => {
            let id = id.to_string();
            if !st.live(&id) {
                return err(404, "itemNotFound");
            }
            if !st.matches(&id, if_match.as_deref()) {
                return err(412, "preconditionFailed");
            }
            st.rewrite(&id, body);
            ok_json(st.json(&id))
        }
        ("POST", ["me", "drive", "items", id, "createUploadSession"]) => {
            let id = id.to_string();
            if !st.live(&id) {
                return err(404, "itemNotFound");
            }
            let b: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(b["item"]["@microsoft.graph.conflictBehavior"], "replace");
            if !st.matches(&id, if_match.as_deref()) {
                return err(412, "preconditionFailed");
            }
            new_session(&mut st, SessKind::Replace(id), &headers)
        }
        ("POST", ["me", "drive", "items", id, "copy"]) => {
            if !st.live(id) {
                return err(404, "itemNotFound");
            }
            let b: Value = serde_json::from_slice(&body).unwrap();
            let mid = st.new_id("m");
            st.monitors.insert(
                mid.clone(),
                Monitor {
                    polls_left: 1,
                    src: id.to_string(),
                    parent: b["parentReference"]["id"].as_str().unwrap().into(),
                    name: b["name"].as_str().unwrap().into(),
                },
            );
            Response::builder()
                .status(202)
                .header(header::LOCATION, format!("{}/monitor/{mid}", st.host))
                .body(axum::body::Body::empty())
                .unwrap()
        }
        _ => panic!("mock: unhandled {method} {raw_path}"),
    }
}

fn mkdir(st: &mut St, parent: &str, body: &[u8]) -> Response {
    let b: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(b["@microsoft.graph.conflictBehavior"], "fail");
    assert!(b["folder"].is_object());
    let name = b["name"].as_str().unwrap();
    if st.child(parent, name).is_some() {
        return err(409, "nameAlreadyExists");
    }
    let id = st.insert(parent, name, true, Bytes::new());
    status_json(201, st.json(&id))
}

fn new_session(st: &mut St, kind: SessKind, _headers: &HeaderMap) -> Response {
    let sid = st.new_id("s");
    st.sessions.insert(
        sid.clone(),
        Sess {
            kind,
            size: 0,
            buf: Vec::new(),
        },
    );
    ok_json(json!({
        "uploadUrl": format!("{}/upload/{sid}", st.host),
        "expirationDateTime": "2099-01-01T00:00:00Z",
        "nextExpectedRanges": ["0-"],
    }))
}

fn upload_fragment(st: &mut St, sid: &str, headers: &HeaderMap, body: &Bytes) -> Response {
    if st.session_down {
        return err(503, "serviceNotAvailable");
    }
    let cr = hdr(headers, header::CONTENT_RANGE).unwrap();
    let spec = cr.strip_prefix("bytes ").unwrap();
    let (range, total) = spec.split_once('/').unwrap();
    let total: usize = total.parse().unwrap();
    let (a, b) = range.split_once('-').unwrap();
    let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
    assert_eq!(b + 1 - a, body.len());
    let Some(sess) = st.sessions.get_mut(sid) else {
        return err(404, "itemNotFound");
    };
    if sess.size == 0 {
        sess.size = total;
    }
    assert_eq!(total, sess.size, "every fragment declares the same total");
    if b + 1 < total {
        assert_eq!(
            body.len() as u64 % FRAGMENT_MULTIPLE,
            0,
            "non-final fragments are multiples of 320 KiB"
        );
    }
    if a != sess.buf.len() {
        return err(416, "invalidRange");
    }
    st.fragment_puts += 1;
    let n = st.fragment_puts;
    let interrupt = st.interrupt_fragment == Some(n);
    let sess = st.sessions.get_mut(sid).unwrap();
    if interrupt {
        sess.buf.extend_from_slice(&body[..body.len() / 2]);
        st.session_down = st.down_after_interrupt;
        return err(503, "serviceNotAvailable");
    }
    sess.buf.extend_from_slice(body);
    if sess.buf.len() < sess.size {
        let next = sess.buf.len();
        return status_json(
            202,
            json!({"expirationDateTime": "2099-01-01T00:00:00Z", "nextExpectedRanges": [format!("{next}-")]}),
        );
    }
    let sess = st.sessions.remove(sid).unwrap();
    let data = Bytes::from(sess.buf);
    let id = match sess.kind {
        SessKind::Replace(id) => {
            st.rewrite(&id, data);
            id
        }
        SessKind::New { parent, name } => {
            if st.child(&parent, &name).is_some() {
                return err(409, "nameAlreadyExists");
            }
            st.insert(&parent, &name, false, data)
        }
    };
    status_json(201, st.json(&id))
}

impl Mock {
    async fn start() -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let mut items = BTreeMap::new();
        items.insert(
            ROOT.to_string(),
            MItem {
                name: "root".into(),
                parent: None,
                folder: true,
                data: Bytes::new(),
                created: 1_700_000_000_000,
                modified: 1_700_000_000_000,
                etag_n: 1,
                ctag_n: 1,
                deleted: false,
            },
        );
        let m = Mock(Arc::new(Mutex::new(St {
            host,
            items,
            seq: 0,
            clock: 1_790_000_000_000,
            log: Vec::new(),
            lost_below: 0,
            page_size: 1000,
            sessions: HashMap::new(),
            valid_token: "tok".into(),
            faults: Vec::new(),
            requests: Vec::new(),
            interrupt_fragment: None,
            down_after_interrupt: false,
            session_down: false,
            fragment_puts: 0,
            monitors: HashMap::new(),
            send_ctag: true,
            ignore_range: false,
            permanent_delete: true,
            quota_total: 1 << 30,
            preauth_saw_auth: false,
        })));
        let app = Router::new().fallback(handle).with_state(m.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        m
    }

    fn base(&self) -> String {
        self.0.lock().base()
    }

    fn mkdir(&self, parent: &str, name: &str) -> String {
        self.0.lock().insert(parent, name, true, Bytes::new())
    }

    fn put(&self, parent: &str, name: &str, data: &[u8]) -> String {
        self.0
            .lock()
            .insert(parent, name, false, Bytes::copy_from_slice(data))
    }

    fn data(&self, id: &str) -> Vec<u8> {
        self.0.lock().items[id].data.to_vec()
    }

    fn rewrite(&self, id: &str, data: &[u8]) {
        self.0.lock().rewrite(id, Bytes::copy_from_slice(data));
    }

    fn mv(&self, id: &str, parent: &str) {
        let mut st = self.0.lock();
        st.items.get_mut(id).unwrap().parent = Some(parent.into());
        st.items.get_mut(id).unwrap().etag_n += 1;
        st.touch(id);
    }

    fn rename(&self, id: &str, name: &str) {
        let mut st = self.0.lock();
        st.items.get_mut(id).unwrap().name = name.into();
        st.items.get_mut(id).unwrap().etag_n += 1;
        st.touch(id);
    }

    fn delete(&self, id: &str) {
        self.0.lock().delete(id);
    }

    fn fault(&self, method: Option<&'static str>, path_has: &str, status: u16, code: &'static str) {
        self.fault_ra(method, path_has, status, code, None);
    }

    fn fault_ra(
        &self,
        method: Option<&'static str>,
        path_has: &str,
        status: u16,
        code: &'static str,
        retry_after: Option<u64>,
    ) {
        self.0.lock().faults.push(Fault {
            method,
            path_has: path_has.into(),
            status,
            code,
            retry_after,
            times: 1,
        });
    }

    fn requests(&self) -> Vec<Req> {
        self.0.lock().requests.clone()
    }

    fn count(&self, method: &str, path_has: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.method == method && r.path.contains(path_has))
            .count()
    }
}

// ---------------------------------------------------------------- helpers

struct Env {
    mock: Mock,
    be: OneDriveBackend,
}

fn cfg_for(mock: &Mock) -> OneDriveConfig {
    OneDriveConfig {
        api_base: mock.base(),
        copy_poll: Duration::from_millis(10),
        ..OneDriveConfig::default()
    }
}

async fn env_with(tune: impl FnOnce(&mut OneDriveConfig)) -> Env {
    let mock = Mock::start().await;
    let mut cfg = cfg_for(&mock);
    tune(&mut cfg);
    let be = OneDriveBackend::new(Arc::new(StaticToken::new("tok")), cfg).unwrap();
    Env { mock, be }
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

async fn new_file(
    be: &OneDriveBackend,
    parent: &str,
    nm: &str,
    data: &[u8],
) -> Result<RNode, BErr> {
    let s = src(data);
    be.upload(
        UploadTarget::New { parent, name: nm },
        s.source(),
        &mut op(),
    )
    .await
}

async fn drain(be: &OneDriveBackend, mut c: Cursor) -> (Vec<ChangeItem>, Cursor, usize) {
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

fn upserts(items: &[ChangeItem]) -> HashMap<String, RNode> {
    items
        .iter()
        .filter_map(|i| match i {
            ChangeItem::Upsert(n) => Some((n.pid.clone(), n.clone())),
            _ => None,
        })
        .collect()
}

fn removed(items: &[ChangeItem]) -> HashSet<String> {
    items
        .iter()
        .filter_map(|i| match i {
            ChangeItem::Removed { pid } => Some(pid.clone()),
            _ => None,
        })
        .collect()
}

/// `mokuro-reader/{A/{v1.cbz, v1.mokuro, sub/x.webp}, B/, catalog.json}` and
/// `Documents/secret.txt` outside.
struct Lib {
    root: String,
    a: String,
    b: String,
    sub: String,
    v1: String,
    v1m: String,
    x: String,
    catalog: String,
    docs: String,
    secret: String,
}

fn library(m: &Mock) -> Lib {
    let root = m.mkdir(ROOT, "mokuro-reader");
    let a = m.mkdir(&root, "A");
    let v1 = m.put(&a, "v1.cbz", &pattern(3000, 1));
    let v1m = m.put(&a, "v1.mokuro", b"{}");
    let sub = m.mkdir(&a, "sub");
    let x = m.put(&sub, "x.webp", b"webp");
    let b = m.mkdir(&root, "B");
    let catalog = m.put(&root, "catalog.json", b"[]");
    let docs = m.mkdir(ROOT, "Documents");
    let secret = m.put(&docs, "secret.txt", b"private");
    Lib {
        root,
        a,
        b,
        sub,
        v1,
        v1m,
        x,
        catalog,
        docs,
        secret,
    }
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn roots_are_located_and_created() {
    let e = env().await;
    assert_eq!(e.be.locate_roots("mokuro-reader").await.unwrap(), vec![]);
    let r = e.be.create_root("mokuro-reader").await.unwrap();
    assert_eq!(name(&r), "mokuro-reader");
    assert!(r.is_dir);
    assert_eq!(r.parent.as_deref(), Some(ROOT));
    let found = e.be.locate_roots("mokuro-reader").await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].pid, r.pid);
    assert_eq!(e.mock.count("GET", "/me/drive/root:/mokuro-reader"), 2);
    assert_eq!(
        e.be.create_root("mokuro-reader").await,
        Err(BErr::NameConflict)
    );
    // A file at the root path is not a root.
    e.mock.put(ROOT, "notes", b"x");
    assert_eq!(e.be.locate_roots("notes").await.unwrap(), vec![]);
    assert_eq!(e.be.locate_roots("a:b").await, Err(BErr::InvalidName));
}

#[tokio::test]
async fn list_all_walks_children_with_paging() {
    let e = env().await;
    e.mock.0.lock().page_size = 2;
    let l = library(&e.mock);
    let mut nodes = Vec::new();
    e.be.list_all(std::slice::from_ref(&l.root), &mut |n| nodes.push(n))
        .await
        .unwrap();
    // Parents before children, each once, roots excluded.
    let mut seen: HashSet<String> = HashSet::from([l.root.clone()]);
    for n in &nodes {
        assert!(
            seen.contains(n.parent.as_ref().unwrap()),
            "{} early",
            name(n)
        );
        assert!(seen.insert(n.pid.clone()), "{} twice", name(n));
    }
    let mut names: Vec<String> = nodes.iter().map(name).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "A",
            "B",
            "catalog.json",
            "sub",
            "v1.cbz",
            "v1.mokuro",
            "x.webp"
        ]
    );
    assert!(!seen.contains(&l.secret) && !seen.contains(&l.docs));
    assert!(
        e.mock
            .requests()
            .iter()
            .any(|r| r.query.contains_key("$skiptoken"))
    );

    // Field mapping.
    let v1 = nodes.iter().find(|n| n.pid == l.v1).unwrap();
    let (etag, ctag, created, modified, data) = {
        let st = e.mock.0.lock();
        let f = &st.items[&l.v1];
        (
            st.etag(&l.v1),
            st.ctag(&l.v1),
            f.created,
            f.modified,
            f.data.clone(),
        )
    };
    assert_eq!(v1.parent.as_deref(), Some(l.a.as_str()));
    assert_eq!(v1.version, etag);
    assert_eq!(v1.cver, ctag);
    assert_eq!(v1.pin.as_deref(), Some(ctag.as_str()));
    assert_eq!(v1.size, 3000);
    assert_eq!(v1.created_ms, created);
    assert_eq!(v1.base_mtime_ms, modified, "top-level lastModifiedDateTime");
    assert_eq!(v1.hash, Some((HashKind::QuickXor, quickxor_base64(&data))));
    let a = nodes.iter().find(|n| n.pid == l.a).unwrap();
    assert!(a.is_dir);
    assert_eq!(a.cver, e.mock.0.lock().etag(&l.a), "folders use the eTag");
    assert_eq!(a.pin, None);

    let mut kids = Vec::new();
    e.be.list_dir(&l.root, &mut |n| kids.push(name(&n)))
        .await
        .unwrap();
    kids.sort();
    assert_eq!(kids, ["A", "B", "catalog.json"]);

    // find_named: exact names only, though Graph resolves case-insensitively.
    let (hit, at) = e.be.find_named(&l.a, "v1.cbz").await.unwrap();
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].pid, l.v1);
    assert!(at.0 > 0);
    assert!(e.be.find_named(&l.a, "V1.CBZ").await.unwrap().0.is_empty());
    assert!(e.be.find_named(&l.a, "nope").await.unwrap().0.is_empty());
}

#[tokio::test]
async fn delta_is_filtered_to_the_subtree() {
    let e = env().await;
    let l = library(&e.mock);
    e.be.locate_roots("mokuro-reader").await.unwrap();
    let mut nodes = Vec::new();
    e.be.list_all(std::slice::from_ref(&l.root), &mut |n| nodes.push(n))
        .await
        .unwrap();
    let c0 = e.be.start_cursor().await.unwrap();
    assert!(c0.0.contains("token="));
    e.mock.0.lock().page_size = 3;

    let newf = e.mock.put(&l.a, "new.cbz", b"new");
    let private = e.mock.put(&l.docs, "private.txt", b"secret");
    e.mock.rewrite(&l.catalog, b"[1]");
    e.mock.rename(&l.b, "B2");
    let c = e.mock.mkdir(&l.root, "C");
    let c1 = e.mock.put(&c, "c1.cbz", b"c1");
    e.mock.delete(&l.v1m);
    // A folder moves out, then its file changes while outside.
    e.mock.mv(&l.sub, &l.docs);
    e.mock.rewrite(&l.x, b"webp2");
    // A folder (with a file) moves in from outside.
    let incoming = e.mock.mkdir(&l.docs, "In");
    let inner = e.mock.put(&incoming, "inner.cbz", b"in");
    e.mock.mv(&incoming, &l.root);

    let (items, c1_cursor, pages) = drain(&e.be, c0).await;
    assert!(pages > 2, "pages: {pages}");
    let up = upserts(&items);
    let gone = removed(&items);
    for pid in [&newf, &l.catalog, &l.b, &c, &c1, &incoming, &l.root] {
        assert!(up.contains_key(pid), "{pid} should be upserted");
    }
    assert_eq!(name(&up[&l.b]), "B2");
    assert_eq!(up[&incoming].parent.as_deref(), Some(l.root.as_str()));
    // Nothing outside ever surfaces as a node.
    for pid in [&private, &l.docs, &l.secret, &l.sub, &l.x] {
        assert!(!up.contains_key(pid), "{pid} must not surface");
    }
    for n in up.values() {
        assert!(!["private.txt", "Documents", "secret.txt", "root"].contains(&name(n).as_str()));
    }
    // Deletes, the move out and the change while outside reach the index as ids.
    for pid in [&l.v1m, &l.sub, &l.x] {
        assert!(gone.contains(pid), "{pid} should be removed");
    }
    // The last upsert of an item wins; deleted items carry no name in the mock.
    assert!(!up.contains_key(&l.v1m));

    // `inner.cbz` was created outside; delta reports the latest state, in which it is
    // inside. A listing of the moved-in folder (the engine's batch-edge rule) finds it.
    assert!(up.contains_key(&inner));
    let mut kids = Vec::new();
    e.be.list_dir(&incoming, &mut |n| kids.push(n.pid))
        .await
        .unwrap();
    assert_eq!(kids, vec![inner.clone()]);
    e.mock.rewrite(&inner, b"in2");
    let (items, c2, _) = drain(&e.be, c1_cursor).await;
    assert!(upserts(&items).contains_key(&inner));

    // A restarted backend knows only the roots: unknown parents are fetched once.
    let be2 = OneDriveBackend::new(Arc::new(StaticToken::new("tok")), cfg_for(&e.mock)).unwrap();
    be2.locate_roots("mokuro-reader").await.unwrap();
    let deep = e.mock.mkdir(&l.a, "deep");
    let new2 = e.mock.put(&deep, "new2.cbz", b"n2");
    let outside2 = e.mock.put(&l.sub, "y.webp", b"y");
    let before = e.mock.requests().len();
    let (items, _, _) = drain(&be2, c2).await;
    let up = upserts(&items);
    assert!(up.contains_key(&new2) && up.contains_key(&deep));
    assert!(!up.contains_key(&outside2));
    let walks: Vec<Req> = e.mock.requests()[before..]
        .iter()
        .filter(|r| r.query.contains_key("$select"))
        .cloned()
        .collect();
    assert!(!walks.is_empty());
    let walked: HashSet<&str> = walks.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(walked.len(), walks.len(), "each folder is fetched once");
}

#[tokio::test]
async fn a_lost_delta_token_is_cursor_lost() {
    let e = env().await;
    let l = library(&e.mock);
    e.be.locate_roots("mokuro-reader").await.unwrap();
    let c = e.be.start_cursor().await.unwrap();
    e.mock.put(&l.a, "n.cbz", b"n");
    e.mock.0.lock().lost_below = usize::MAX;
    assert_eq!(e.be.changes(&c).await, Ok(ChangePage::CursorLost));
    // Before locate_roots the feed cannot be filtered: refused, not guessed.
    let be2 = OneDriveBackend::new(Arc::new(StaticToken::new("tok")), cfg_for(&e.mock)).unwrap();
    assert!(matches!(be2.changes(&c).await, Err(BErr::Fatal(_))));
    // A cursor pointing anywhere but the API base never gets the token.
    let foreign = Cursor("https://evil.example/v1.0/me/drive/root/delta?token=1".into());
    assert!(matches!(e.be.changes(&foreign).await, Err(BErr::Fatal(_))));
}

#[tokio::test]
async fn downloads_follow_the_redirect_without_the_token() {
    let e = env().await;
    let l = library(&e.mock);
    let data = pattern(3000, 1);
    let got = read_to_vec(e.be.read(&l.v1, None, None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got, data);
    let reqs = e.mock.requests();
    let content = reqs.iter().find(|r| r.path.ends_with("/content")).unwrap();
    assert_eq!(content.auth.as_deref(), Some("Bearer tok"));
    let pre = reqs
        .iter()
        .find(|r| r.path.starts_with("/preauth/"))
        .unwrap();
    assert_eq!(
        pre.auth, None,
        "the pre-authenticated URL never sees the token"
    );
    assert!(!e.mock.0.lock().preauth_saw_auth);

    let mid = read_to_vec(e.be.read(&l.v1, None, Some((10, Some(19)))).await.unwrap())
        .await
        .unwrap();
    assert_eq!(mid, data[10..20]);
    let tail = read_to_vec(e.be.read(&l.v1, None, Some((2990, None))).await.unwrap())
        .await
        .unwrap();
    assert_eq!(tail, data[2990..]);
    // The download host "may" ignore Range and send 200: sliced locally.
    e.mock.0.lock().ignore_range = true;
    let mid = read_to_vec(
        e.be.read(&l.v1, None, Some((100, Some(1099))))
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(mid, data[100..1100]);
    // The provider hash checks a whole download.
    let n = e.be.stat(&l.v1).await.unwrap().unwrap();
    assert_eq!(n.hash.unwrap().1, quickxor_base64(&got));
    assert_eq!(
        e.be.read("missing", None, None).await.err(),
        Some(BErr::NotFound)
    );
}

#[tokio::test]
async fn pinned_reads_compare_the_ctag() {
    let e = env().await;
    let l = library(&e.mock);
    let n = e.be.stat(&l.v1).await.unwrap().unwrap();
    let pin = n.pin.clone().unwrap();
    let got = read_to_vec(e.be.read(&l.v1, Some(&pin), None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got, pattern(3000, 1));
    e.mock.rewrite(&l.v1, b"changed");
    assert_eq!(
        e.be.read(&l.v1, Some(&pin), Some((0, Some(3)))).await.err(),
        Some(BErr::PreconditionFailed)
    );

    // Without a CTag header (the ETag is the eTag): re-stat after the last byte.
    e.mock.0.lock().send_ctag = false;
    let cur = e.be.stat(&l.v1).await.unwrap().unwrap();
    let got = read_to_vec(e.be.read(&l.v1, cur.pin.as_deref(), None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got, b"changed");
    let mut body = e.be.read(&l.v1, Some(&pin), None).await.unwrap();
    let mut last = None;
    while let Some(chunk) = body.next().await {
        last = Some(chunk);
    }
    let e2 = last.unwrap().unwrap_err();
    assert_eq!(
        e2.get_ref().unwrap().downcast_ref::<BErr>(),
        Some(&BErr::PreconditionFailed)
    );
}

#[tokio::test]
async fn small_creates_are_exclusive() {
    let e = env().await;
    let l = library(&e.mock);
    let n = new_file(&e.be, &l.a, "v2.mokuro", b"{\"v\":2}")
        .await
        .unwrap();
    assert_eq!(name(&n), "v2.mokuro");
    assert_eq!(n.parent.as_deref(), Some(l.a.as_str()));
    assert_eq!(e.mock.data(&n.pid), b"{\"v\":2}");
    assert_eq!(
        n.hash,
        Some((HashKind::QuickXor, quickxor_base64(b"{\"v\":2}")))
    );
    let put = e
        .mock
        .requests()
        .into_iter()
        .find(|r| r.method == "PUT")
        .unwrap();
    assert!(put.path.ends_with("/v2.mokuro:/content"), "{}", put.path);
    assert_eq!(
        put.query
            .get("@microsoft.graph.conflictBehavior")
            .map(String::as_str),
        Some("fail")
    );
    // Different bytes under the name: a conflict, nothing overwritten.
    assert_eq!(
        new_file(&e.be, &l.a, "v2.mokuro", b"other").await,
        Err(BErr::NameConflict)
    );
    // Names fold: a case variant is the same name on OneDrive.
    assert_eq!(
        new_file(&e.be, &l.a, "V2.MOKURO", b"other").await,
        Err(BErr::NameConflict)
    );
    assert_eq!(e.mock.data(&n.pid), b"{\"v\":2}");
    // A retry of a create that already landed returns it.
    let again = new_file(&e.be, &l.a, "v2.mokuro", b"{\"v\":2}")
        .await
        .unwrap();
    assert_eq!(again.pid, n.pid);
    // Empty files go up in one PUT too.
    let empty = new_file(&e.be, &l.a, "empty", b"").await.unwrap();
    assert_eq!(empty.size, 0);
}

#[tokio::test]
async fn large_uploads_resume_after_an_interrupted_fragment() {
    let e = env_with(|c| c.fragment_size = FRAGMENT_MULTIPLE + 1000).await;
    let l = library(&e.mock);
    let data = pattern((5 << 20) + 12_345, 7);
    e.mock.0.lock().interrupt_fragment = Some(3);
    let n = new_file(&e.be, &l.a, "big.cbz", &data).await.unwrap();
    assert_eq!(e.mock.data(&n.pid), data);
    assert_eq!(n.size, data.len() as u64);
    assert_eq!(e.mock.count("POST", "createUploadSession"), 1);
    // The interrupted fragment was followed by a status query and a resume from the
    // byte the session asked for.
    assert!(e.mock.count("GET", "/upload/") >= 1);
    let puts: Vec<Req> = e
        .mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PUT" && r.path.starts_with("/upload/"))
        .collect();
    assert!(puts.iter().all(|r| r.auth.is_none()));
    assert!(puts.len() >= 17);
    // The fragment after the interrupt starts where the session said (half-way into
    // the interrupted one), not at a fragment boundary.
    let starts: Vec<u64> = puts
        .iter()
        .map(|r| {
            let cr = r.content_range.as_deref().unwrap();
            cr["bytes ".len()..]
                .split('-')
                .next()
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    assert_eq!(starts[3], 2 * FRAGMENT_MULTIPLE + FRAGMENT_MULTIPLE / 2);
    assert!(!e.mock.0.lock().preauth_saw_auth);

    // The service stays down after the interrupt: the call fails with the session kept,
    // and the next call (a restart) resumes it instead of starting over.
    {
        let mut st = e.mock.0.lock();
        st.interrupt_fragment = Some(st.fragment_puts + 2);
        st.down_after_interrupt = true;
    }
    let s = src(&data);
    let mut ctx = op();
    let r =
        e.be.upload(
            UploadTarget::New {
                parent: &l.a,
                name: "big2.cbz",
            },
            s.source(),
            &mut ctx,
        )
        .await;
    assert!(
        matches!(r, Err(BErr::Transient(_)) | Err(BErr::Throttled(_))),
        "{r:?}"
    );
    assert!(
        ctx.resume
            .as_deref()
            .is_some_and(|r| r.contains("/upload/"))
    );
    {
        let mut st = e.mock.0.lock();
        st.session_down = false;
        st.down_after_interrupt = false;
    }
    let n2 =
        e.be.upload(
            UploadTarget::New {
                parent: &l.a,
                name: "big2.cbz",
            },
            s.source(),
            &mut ctx,
        )
        .await
        .unwrap();
    assert_eq!(e.mock.data(&n2.pid), data);
    assert_eq!(e.mock.count("POST", "createUploadSession"), 2);
    assert_eq!(ctx.resume, None);
}

#[tokio::test]
async fn replaces_use_a_session_with_if_match() {
    let e = env().await;
    let l = library(&e.mock);
    let old = e.be.stat(&l.catalog).await.unwrap().unwrap();
    let s = src(b"[\"new\"]");
    let n =
        e.be.upload(
            UploadTarget::Replace {
                pid: &l.catalog,
                if_cver: Some(&old.cver),
            },
            s.source(),
            &mut op(),
        )
        .await
        .unwrap();
    assert_eq!(n.pid, l.catalog, "the id survives a replace");
    assert_ne!(n.cver, old.cver);
    assert_eq!(e.mock.data(&l.catalog), b"[\"new\"]");
    let sess = e
        .mock
        .requests()
        .into_iter()
        .find(|r| r.path.ends_with("/createUploadSession"))
        .unwrap();
    assert_eq!(sess.if_match.as_deref(), Some(old.cver.as_str()));
    assert_eq!(e.mock.count("PUT", "/upload/"), 1, "one fragment");
    assert_eq!(e.mock.count("PUT", "/content"), 0, "never a simple PUT");

    // A stale base: 412, nothing written.
    let s2 = src(b"[\"stale\"]");
    assert_eq!(
        e.be.upload(
            UploadTarget::Replace {
                pid: &l.catalog,
                if_cver: Some(&old.cver),
            },
            s2.source(),
            &mut op(),
        )
        .await,
        Err(BErr::PreconditionFailed)
    );
    assert_eq!(e.mock.data(&l.catalog), b"[\"new\"]");
    // The same bytes again under the old base: an earlier attempt that landed.
    let again =
        e.be.upload(
            UploadTarget::Replace {
                pid: &l.catalog,
                if_cver: Some(&old.cver),
            },
            s.source(),
            &mut op(),
        )
        .await
        .unwrap();
    assert_eq!(again.cver, n.cver);
    // Unconditional and empty replaces.
    e.be.upload(
        UploadTarget::Replace {
            pid: &l.catalog,
            if_cver: None,
        },
        s2.source(),
        &mut op(),
    )
    .await
    .unwrap();
    assert_eq!(e.mock.data(&l.catalog), b"[\"stale\"]");
    let cur = e.be.stat(&l.catalog).await.unwrap().unwrap();
    let empty = src(b"");
    let n =
        e.be.upload(
            UploadTarget::Replace {
                pid: &l.catalog,
                if_cver: Some(&cur.cver),
            },
            empty.source(),
            &mut op(),
        )
        .await
        .unwrap();
    assert_eq!(n.size, 0);
    assert_eq!(
        e.be.upload(
            UploadTarget::Replace {
                pid: &l.catalog,
                if_cver: Some(&cur.cver),
            },
            empty.source(),
            &mut op(),
        )
        .await,
        Err(BErr::PreconditionFailed)
    );
}

#[tokio::test]
async fn folders_rename_and_move_by_patch() {
    let e = env().await;
    let l = library(&e.mock);
    let d = e.be.mkdir(&l.root, "D", &op()).await.unwrap();
    assert!(d.is_dir);
    assert_eq!(
        e.be.mkdir(&l.root, "d", &op()).await,
        Err(BErr::NameConflict)
    );

    let cur = e.be.stat(&l.v1).await.unwrap().unwrap();
    let n =
        e.be.update(&l.v1, None, Some("Vol 1.cbz"), Some(&cur.cver))
            .await
            .unwrap();
    assert_eq!(name(&n), "Vol 1.cbz");
    assert_eq!(n.pid, l.v1);
    let patch = e
        .mock
        .requests()
        .into_iter()
        .find(|r| r.method == "PATCH")
        .unwrap();
    assert_eq!(patch.if_match.as_deref(), Some(cur.cver.as_str()));
    // Case-only rename by id.
    let n =
        e.be.update(&l.v1, None, Some("VOL 1.cbz"), None)
            .await
            .unwrap();
    assert_eq!(name(&n), "VOL 1.cbz");
    // Move.
    let n = e.be.update(&l.v1, Some(&d.pid), None, None).await.unwrap();
    assert_eq!(n.parent.as_deref(), Some(d.pid.as_str()));
    // Onto an existing name: conflict.
    e.mock.put(&l.a, "taken.cbz", b"t");
    assert_eq!(
        e.be.update(&l.v1, Some(&l.a), Some("taken.cbz"), None)
            .await,
        Err(BErr::NameConflict)
    );
    // A stale If-Match.
    e.mock.rewrite(&l.catalog, b"[2]");
    let stale = "\"c:{stale},1\"";
    assert_eq!(
        e.be.update(&l.catalog, None, Some("c2.json"), Some(stale))
            .await,
        Err(BErr::PreconditionFailed)
    );
    assert_eq!(
        e.be.update("missing", None, Some("x"), None).await,
        Err(BErr::NotFound)
    );
}

#[tokio::test]
async fn copy_waits_for_the_monitor() {
    let e = env().await;
    let l = library(&e.mock);
    let n = e.be.copy(&l.v1, &l.b, "copy.cbz").await.unwrap();
    assert_ne!(n.pid, l.v1);
    assert_eq!(name(&n), "copy.cbz");
    assert_eq!(n.parent.as_deref(), Some(l.b.as_str()));
    assert_eq!(e.mock.data(&n.pid), pattern(3000, 1));
    let polls: Vec<Req> = e
        .mock
        .requests()
        .into_iter()
        .filter(|r| r.path.starts_with("/monitor/"))
        .collect();
    assert_eq!(polls.len(), 2);
    assert!(polls.iter().all(|r| r.auth.is_none()));
    assert_eq!(
        e.be.copy(&l.v1, &l.b, "copy.cbz").await,
        Err(BErr::NameConflict)
    );
}

#[tokio::test]
async fn trash_and_permanent_delete() {
    let e = env().await;
    let l = library(&e.mock);
    let cur = e.be.stat(&l.v1).await.unwrap().unwrap();
    e.mock.rewrite(&l.v1, b"newer");
    assert_eq!(
        e.be.trash(&l.v1, Some(&cur.cver)).await,
        Err(BErr::PreconditionFailed)
    );
    let cur = e.be.stat(&l.v1).await.unwrap().unwrap();
    e.be.trash(&l.v1, Some(&cur.cver)).await.unwrap();
    assert_eq!(e.be.stat(&l.v1).await, Ok(None));
    let del = e
        .mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "DELETE")
        .next_back()
        .unwrap();
    assert_eq!(del.if_match.as_deref(), Some(cur.cver.as_str()));
    assert_eq!(e.be.trash(&l.v1, None).await, Err(BErr::NotFound));

    e.be.delete_permanent(&l.v1m).await.unwrap();
    assert_eq!(e.be.stat(&l.v1m).await, Ok(None));
    assert_eq!(e.mock.count("POST", "permanentDelete"), 1);

    // OneDrive Personal: permanentDelete is refused; fall back to DELETE, and remember.
    e.mock.0.lock().permanent_delete = false;
    let deletes = e.mock.count("DELETE", "/me/drive/items/");
    e.be.delete_permanent(&l.x).await.unwrap();
    e.be.delete_permanent(&l.catalog).await.unwrap();
    assert_eq!(e.mock.count("POST", "permanentDelete"), 2);
    assert_eq!(e.mock.count("DELETE", "/me/drive/items/"), deletes + 2);
    assert_eq!(e.be.stat(&l.catalog).await, Ok(None));
}

#[tokio::test]
async fn forbidden_names_are_refused_before_any_request() {
    let e = env().await;
    let l = library(&e.mock);
    let before = e.mock.requests().len();
    for bad in [
        "a\"b", "a*b", "a:b", "a<b", "a>b", "a?b", "a/b", "a\\b", "a|b", " lead", "trail ", "",
    ] {
        assert_eq!(
            new_file(&e.be, &l.a, bad, b"x").await,
            Err(BErr::InvalidName),
            "{bad:?}"
        );
        assert_eq!(e.be.mkdir(&l.a, bad, &op()).await, Err(BErr::InvalidName));
        assert_eq!(
            e.be.update(&l.v1, None, Some(bad), None).await,
            Err(BErr::InvalidName)
        );
        assert_eq!(e.be.copy(&l.v1, &l.a, bad).await, Err(BErr::InvalidName));
        assert_eq!(
            e.be.find_named(&l.a, bad).await.err(),
            Some(BErr::InvalidName)
        );
    }
    assert_eq!(e.mock.requests().len(), before, "no request was sent");
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
    let r = mock.mkdir(ROOT, "mokuro-reader");
    mock.0.lock().valid_token = "tok-1".into();
    let tokens = Arc::new(Rotating {
        n: AtomicUsize::new(0),
        invalidated: AtomicUsize::new(0),
    });
    let be = OneDriveBackend::new(tokens.clone(), cfg_for(&mock)).unwrap();
    assert!(be.stat(&r).await.unwrap().is_some());
    assert_eq!(tokens.invalidated.load(Ordering::SeqCst), 1);
    assert!(be.stat(&r).await.unwrap().is_some());
    assert_eq!(tokens.invalidated.load(Ordering::SeqCst), 1);
    assert_eq!(be.units_used(), 3 * units::GET);
    mock.0.lock().valid_token = "something-else".into();
    assert_eq!(be.stat(&r).await, Err(BErr::AuthExpired));
    assert_eq!(tokens.invalidated.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn errors_map_to_backend_errors() {
    let e = env().await;
    let l = library(&e.mock);
    let path = format!("/me/drive/items/{}", l.v1);

    e.mock
        .fault_ra(None, &path, 429, "activityLimitReached", Some(7));
    assert_eq!(
        e.be.stat(&l.v1).await,
        Err(BErr::Throttled(Some(Duration::from_secs(7))))
    );
    e.mock
        .fault_ra(None, &path, 503, "serviceNotAvailable", Some(3));
    assert_eq!(
        e.be.stat(&l.v1).await,
        Err(BErr::Throttled(Some(Duration::from_secs(3))))
    );
    e.mock.fault(None, &path, 429, "activityLimitReached");
    assert!(matches!(
        e.be.stat(&l.v1).await,
        Err(BErr::Throttled(Some(_)))
    ));
    e.mock.fault(None, &path, 503, "serviceNotAvailable");
    assert!(matches!(e.be.stat(&l.v1).await, Err(BErr::Transient(_))));
    e.mock.fault(None, &path, 500, "generalException");
    assert!(matches!(e.be.stat(&l.v1).await, Err(BErr::Transient(_))));
    e.mock.fault(None, &path, 403, "accessDenied");
    assert_eq!(e.be.stat(&l.v1).await, Err(BErr::AuthRevoked));
    e.mock.fault(None, &path, 423, "resourceLocked");
    assert!(matches!(
        e.be.update(&l.v1, None, Some("z"), None).await,
        Err(BErr::Transient(_))
    ));
    e.mock.fault(None, &path, 412, "preconditionFailed");
    assert_eq!(e.be.trash(&l.v1, None).await, Err(BErr::PreconditionFailed));
    e.mock
        .fault(None, ":/v2.cbz:/content", 507, "insufficientStorage");
    assert_eq!(
        new_file(&e.be, &l.a, "v2.cbz", b"x").await,
        Err(BErr::StorageFull)
    );
    e.mock
        .fault(None, ":/v2.cbz:/content", 413, "maxFileSizeExceeded");
    assert_eq!(
        new_file(&e.be, &l.a, "v2.cbz", b"x").await,
        Err(BErr::TooLarge)
    );
    assert_eq!(e.be.stat("missing").await, Ok(None));
    // Everything still works afterwards.
    assert!(e.be.stat(&l.v1).await.unwrap().is_some());
}

#[tokio::test]
async fn units_user_agent_quota_and_account() {
    let e = env().await;
    let l = library(&e.mock);
    let u0 = e.be.units_used();
    e.be.stat(&l.v1).await.unwrap();
    assert_eq!(e.be.units_used() - u0, units::GET);
    let u0 = e.be.units_used();
    e.be.list_dir(&l.root, &mut |_| {}).await.unwrap();
    assert_eq!(e.be.units_used() - u0, units::LIST);
    let u0 = e.be.units_used();
    read_to_vec(e.be.read(&l.v1, None, None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(e.be.units_used() - u0, units::DOWNLOAD);
    let u0 = e.be.units_used();
    e.be.update(&l.v1, None, Some("w.cbz"), None).await.unwrap();
    assert_eq!(e.be.units_used() - u0, units::UPDATE);
    e.be.locate_roots("mokuro-reader").await.unwrap();
    let u0 = e.be.units_used();
    let c = e.be.start_cursor().await.unwrap();
    e.be.changes(&c).await.unwrap();
    assert_eq!(e.be.units_used() - u0, 2 * units::DELTA_TOKEN);

    let ua = format!("ISV|Mokuro|Bunko/{}", env!("CARGO_PKG_VERSION"));
    for r in e.mock.requests() {
        assert_eq!(
            r.user_agent.as_deref(),
            Some(ua.as_str()),
            "{} {}",
            r.method,
            r.path
        );
    }

    let q = e.be.quota().await.unwrap().unwrap();
    assert_eq!(q.total, 1 << 30);
    assert!(q.available < q.total);
    let acct = e.be.account().await.unwrap();
    assert_eq!(acct.id, "user-1");
    assert_eq!(acct.label, "me@example.com");
    let caps = e.be.caps();
    assert!(caps.cas_create && caps.cas_overwrite && caps.atomic_replace && caps.pinned_read);
    assert_eq!(caps.hash_kind, Some(HashKind::QuickXor));
    assert_eq!(caps.mtime_granularity_ns, 1_000_000_000);
    assert_eq!(caps.chunk_multiple, 320 << 10);
    assert_eq!(
        e.be.wait_change(&c, Duration::from_millis(1)).await,
        Err(BErr::Unsupported)
    );
}
