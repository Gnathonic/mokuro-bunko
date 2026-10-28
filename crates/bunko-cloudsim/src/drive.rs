//! Google Drive v3: the endpoints Bunko's driver and the reader's provider call, under
//! `{base}/google` (so a driver's `api_base` is `{base}/google` and uploads go to
//! `{base}/google/upload/drive/v3/...`). One [`DriveState`] per account.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use md5::Md5;
use parking_lot::{Mutex, MutexGuard};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::violations::{Bad, need};
use crate::{SimState, rfc3339};

pub const FOLDER: &str = "application/vnd.google-apps.folder";
/// The path prefix every Drive URL lives under.
pub const PREFIX: &str = "/google";

pub struct MFile {
    pub name: String,
    pub mime: String,
    pub parents: Vec<String>,
    pub data: Bytes,
    pub created: i64,
    pub modified: i64,
    pub version: i64,
    pub head_rev: String,
    pub revisions: HashMap<String, Bytes>,
    pub trashed: bool,
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

/// One request as the account saw it (path without the `/google` prefix).
#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub content_range: Option<String>,
}

/// An injected error: the next `times` requests matching `method` (any when `None`)
/// and containing `path_has` get `status` with Drive's error body for `reason`.
#[derive(Debug, Clone)]
pub struct Fault {
    pub method: Option<String>,
    pub path_has: String,
    pub status: u16,
    pub reason: String,
    pub retry_after: Option<u64>,
    pub times: usize,
}

/// One account's drive and its knobs.
pub struct DriveState {
    pub files: BTreeMap<String, MFile>,
    pub seq: u64,
    pub clock: i64,
    /// The change log `changes.list` pages through (one entry per changed id).
    pub log: Vec<String>,
    pub page_size: usize,
    /// List newest first (so children arrive before their parents).
    pub reverse_listing: bool,
    pub sha256: bool,
    pub keep_revisions: bool,
    pub revisions_forbidden: bool,
    sessions: HashMap<String, Sess>,
    /// A fixed bearer token this account accepts besides the ones the simulator issues.
    pub valid_token: String,
    pub faults: Vec<Fault>,
    pub requests: Vec<Req>,
    /// Fail this (1-based) chunk PUT after taking half its bytes.
    pub interrupt_chunk: Option<usize>,
    /// After the interrupt, every session PUT fails until this is cleared.
    pub down_after_interrupt: bool,
    pub session_down: bool,
    pub chunk_puts: usize,
    pub quota_limit: i64,
}

fn hex_md5(b: &[u8]) -> String {
    hex::encode(Md5::digest(b))
}

fn hex_sha256(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

fn err_msg(status: u16, reason: &str, message: &str) -> Response {
    let body = json!({"error": {"code": status, "message": message,
        "errors": [{"domain": "global", "reason": reason, "message": message}]}});
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
        axum::Json(body),
    )
        .into_response()
}

fn err(status: u16, reason: &str) -> Response {
    err_msg(status, reason, reason)
}

fn ok_json(v: Value) -> Response {
    axum::Json(v).into_response()
}

impl Default for DriveState {
    fn default() -> Self {
        DriveState {
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
        }
    }
}

impl DriveState {
    fn tick(&mut self) -> i64 {
        self.clock += 1000;
        self.clock
    }

    fn new_id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("{prefix}{:05}", self.seq)
    }

    /// Trashed itself or under a trashed folder.
    pub fn trashed(&self, id: &str) -> bool {
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

    pub fn json(&self, id: &str) -> Value {
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

    pub fn insert(
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

    pub fn rewrite(&mut self, id: &str, data: Bytes) {
        let now = self.tick();
        let rev = self.new_id("r");
        let keep = self.keep_revisions;
        let Some(f) = self.files.get_mut(id) else {
            return;
        };
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

    pub fn delete(&mut self, id: &str) {
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

    /// The first live child of `parent` named exactly `name`.
    pub fn child(&self, parent: &str, name: &str) -> Option<String> {
        self.files
            .iter()
            .find(|(id, f)| {
                f.name == name && f.parents.iter().any(|p| p == parent) && !self.trashed(id)
            })
            .map(|(id, _)| id.clone())
    }

    /// Create the folders along `path` (under My Drive) and put `data` at its end,
    /// replacing a file already there.
    pub fn seed(&mut self, path: &str, data: Bytes) -> String {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut parent = "root".to_string();
        for dir in &parts[..parts.len().saturating_sub(1)] {
            parent = match self.child(&parent, dir) {
                Some(id) => id,
                None => self.insert(None, &parent, dir, FOLDER, Bytes::new()),
            };
        }
        let Some(name) = parts.last() else {
            return parent;
        };
        match self.child(&parent, name) {
            Some(id) => {
                self.rewrite(&id, data);
                id
            }
            None => self.insert(None, &parent, name, "application/octet-stream", data),
        }
    }

    /// Every live file under My Drive: (path, bytes).
    pub fn files(&self) -> Vec<(String, Bytes)> {
        let mut out = Vec::new();
        for (id, f) in &self.files {
            if f.mime == FOLDER || self.trashed(id) {
                continue;
            }
            let mut parts = vec![f.name.clone()];
            let mut cur = f.parents.first().cloned();
            let mut rooted = false;
            for _ in 0..64 {
                match cur {
                    Some(p) if p == "root" => {
                        rooted = true;
                        break;
                    }
                    Some(p) => match self.files.get(&p) {
                        Some(pf) => {
                            parts.push(pf.name.clone());
                            cur = pf.parents.first().cloned();
                        }
                        None => break,
                    },
                    None => break,
                }
            }
            if rooted {
                parts.reverse();
                out.push((parts.join("/"), f.data.clone()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The first fault matching this request, used up by one.
    fn take_fault(&mut self, method: &str, path: &str) -> Option<Fault> {
        let i = self.faults.iter().position(|f| {
            f.method
                .as_deref()
                .is_none_or(|m| m.eq_ignore_ascii_case(method))
                && path.contains(&f.path_has)
        })?;
        let f = self.faults[i].clone();
        if f.times <= 1 {
            self.faults.remove(i);
        } else {
            self.faults[i].times -= 1;
        }
        Some(f)
    }
}

fn unquote(s: &str) -> Result<String, Bad> {
    let s = s.trim();
    if s.len() < 2 || !s.starts_with('\'') || !s.ends_with('\'') {
        return Err(format!("expected a quoted string, got {s:?}"));
    }
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
    Ok(out)
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// (metadata, media) of a multipart/related body.
fn parse_multipart(headers: &HeaderMap, body: &[u8]) -> Result<(Value, Bytes), Bad> {
    let ct = need(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        "multipart upload without a Content-Type",
    )?;
    let boundary = need(
        ct.split("boundary=").nth(1),
        "multipart Content-Type without a boundary",
    )?
    .trim()
    .trim_matches('"');
    let delim = format!("--{boundary}");
    let h1 = need(find(body, b"\r\n\r\n", 0), "multipart: no metadata part")? + 4;
    let j_end = need(
        find(body, format!("\r\n{delim}\r\n").as_bytes(), h1),
        "multipart: no media part",
    )?;
    let meta: Value = serde_json::from_slice(&body[h1..j_end])
        .map_err(|e| format!("multipart: metadata is not JSON: {e}"))?;
    let h2 = need(
        find(body, b"\r\n\r\n", j_end + 2),
        "multipart: media part has no headers",
    )? + 4;
    let closing = format!("\r\n{delim}--");
    let m_end = need(
        body.windows(closing.len())
            .rposition(|w| w == closing.as_bytes()),
        "multipart: no closing delimiter",
    )?;
    if m_end < h2 {
        return Err("multipart: media part ends before it starts".into());
    }
    Ok((meta, Bytes::copy_from_slice(&body[h2..m_end])))
}

fn media(data: &Bytes, headers: &HeaderMap) -> Result<Response, Bad> {
    let Some(r) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) else {
        return Ok((StatusCode::OK, data.clone()).into_response());
    };
    let bad = || format!("unparsable Range {r:?}");
    let spec = r.strip_prefix("bytes=").ok_or_else(bad)?;
    let (a, b) = spec.split_once('-').ok_or_else(bad)?;
    let a: usize = a.parse().map_err(|_| bad())?;
    if a >= data.len() {
        return Ok(err(416, "requestedRangeNotSatisfiable"));
    }
    let end = if b.is_empty() {
        data.len()
    } else {
        (b.parse::<usize>().map_err(|_| bad())? + 1).min(data.len())
    };
    Ok((
        StatusCode::PARTIAL_CONTENT,
        [(
            header::CONTENT_RANGE,
            format!("bytes {a}-{}/{}", end - 1, data.len()),
        )],
        data.slice(a..end),
    )
        .into_response())
}

fn range_308(len: usize) -> Response {
    let mut r = Response::builder().status(308);
    if len > 0 {
        r = r.header(header::RANGE, format!("bytes=0-{}", len - 1));
    }
    r.body(Body::empty()).unwrap_or_default()
}

fn json_body(body: &[u8]) -> Result<Value, Bad> {
    serde_json::from_slice(body).map_err(|e| format!("body is not JSON: {e}"))
}

fn first_parent(meta: &Value) -> Result<String, Bad> {
    Ok(need(meta["parents"][0].as_str(), "metadata without parents[0]")?.to_string())
}

fn meta_name(meta: &Value) -> Result<String, Bad> {
    Ok(need(meta["name"].as_str(), "metadata without a name")?.to_string())
}

/// `Bearer <token>` from the request, if any.
pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

pub async fn handle(
    State(sim): State<SimState>,
    method: Method,
    uri: Uri,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri
        .path()
        .strip_prefix(PREFIX)
        .unwrap_or(uri.path())
        .to_string();
    let Some((_account, acct)) = bearer(&headers).and_then(|t| sim.drive_for_token(t)) else {
        return err(401, "authError");
    };
    let res = {
        let mut st = acct.lock();
        st.requests.push(Req {
            method: method.to_string(),
            path: path.clone(),
            query: q.clone(),
            content_range: headers
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
        });
        if let Some(f) = st.take_fault(method.as_str(), &path) {
            let mut r = err(f.status, &f.reason);
            if let Some(s) = f.retry_after {
                r.headers_mut()
                    .insert(header::RETRY_AFTER, s.to_string().parse().expect("digits"));
            }
            return r;
        }
        let api = format!("{}{PREFIX}", sim.base());
        route(&mut st, &api, &method, &path, &q, &headers, body)
    };
    res.unwrap_or_else(|bad| {
        sim.violation(format!("google {method} {path}: {bad}"));
        err_msg(400, "badRequest", &bad)
    })
}

fn route(
    st: &mut DriveState,
    api: &str,
    method: &Method,
    path: &str,
    q: &HashMap<String, String>,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, Bad> {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    Ok(match (method.as_str(), segs.as_slice()) {
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
                return Ok(err(400, "invalid"));
            };
            if at > st.log.len() {
                return Ok(err(404, "notFound"));
            }
            if q.get("includeRemoved").map(String::as_str) != Some("true") {
                return Err("changes.list without includeRemoved=true".into());
            }
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
                    name = Some(unquote(v)?);
                } else if let Some(v) = c.strip_prefix("mimeType=") {
                    mime = Some(unquote(v)?);
                } else if let Some(v) = c.strip_suffix(" in parents") {
                    parent = Some(unquote(v)?);
                } else {
                    return Err(format!("unsupported q clause {c:?} in {query:?}"));
                }
            }
            if !(owners || name.is_some() || parent.is_some()) {
                return Err(format!("q {query:?} would list the whole drive"));
            }
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
                .unwrap_or(0)
                .min(ids.len());
            let end = (at + st.page_size).min(ids.len());
            let files: Vec<Value> = ids[at..end].iter().map(|id| st.json(id)).collect();
            let mut v = json!({"files": files});
            if end < ids.len() {
                v["nextPageToken"] = json!(format!("p{end}"));
            }
            ok_json(v)
        }
        ("POST", ["drive", "v3", "files"]) => {
            let meta = json_body(&body)?;
            let parent = meta["parents"][0].as_str().unwrap_or("root").to_string();
            let mime = meta["mimeType"]
                .as_str()
                .unwrap_or("application/octet-stream")
                .to_string();
            let name = meta_name(&meta)?;
            let id = st.insert(None, &parent, &name, &mime, Bytes::new());
            ok_json(st.json(&id))
        }
        ("POST", ["drive", "v3", "files", id, "copy"]) => {
            let Some(src) = st.files.get(*id) else {
                return Ok(err(404, "notFound"));
            };
            let (data, mime) = (src.data.clone(), src.mime.clone());
            let meta = json_body(&body)?;
            let (parent, name) = (first_parent(&meta)?, meta_name(&meta)?);
            let new = st.insert(None, &parent, &name, &mime, data);
            ok_json(st.json(&new))
        }
        ("GET", ["drive", "v3", "files", id, "revisions", rev]) => {
            if q.get("alt").map(String::as_str) != Some("media") {
                return Err("revisions.get without alt=media".into());
            }
            if st.revisions_forbidden {
                return Ok(err(403, "insufficientFilePermissions"));
            }
            match st.files.get(*id).and_then(|f| f.revisions.get(*rev)) {
                Some(d) => media(&d.clone(), headers)?,
                None => err(404, "notFound"),
            }
        }
        ("GET", ["drive", "v3", "files", id]) => {
            if !st.files.contains_key(*id) {
                return Ok(err(404, "notFound"));
            }
            if q.get("alt").map(String::as_str) == Some("media") {
                media(&st.files[*id].data.clone(), headers)?
            } else {
                ok_json(st.json(id))
            }
        }
        ("PATCH", ["drive", "v3", "files", id]) => {
            let id = id.to_string();
            if !st.files.contains_key(&id) {
                return Ok(err(404, "notFound"));
            }
            let meta: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let f = st.files.get_mut(&id).expect("checked above");
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
                return Ok(err(404, "notFound"));
            }
            st.delete(id);
            StatusCode::NO_CONTENT.into_response()
        }
        (m @ ("POST" | "PATCH"), ["upload", "drive", "v3", "files", rest @ ..]) => {
            let target = rest.first().map(|s| s.to_string());
            if m == "PATCH" {
                let Some(t) = target.as_deref() else {
                    return Err("an upload PATCH without a file id".into());
                };
                if q.get("keepRevisionForever").map(String::as_str) != Some("false") {
                    return Err("an upload PATCH without keepRevisionForever=false".into());
                }
                if !st.files.contains_key(t) {
                    return Ok(err(404, "notFound"));
                }
            }
            match q.get("uploadType").map(String::as_str) {
                Some("multipart") => {
                    let (meta, data) = parse_multipart(headers, &body)?;
                    match target {
                        Some(id) => {
                            st.rewrite(&id, data);
                            ok_json(st.json(&id))
                        }
                        None => {
                            let id = meta["id"].as_str().map(str::to_string);
                            if id.as_ref().is_some_and(|i| st.files.contains_key(i)) {
                                return Ok(err(409, "duplicate"));
                            }
                            let (parent, name) = (first_parent(&meta)?, meta_name(&meta)?);
                            let mime = meta["mimeType"]
                                .as_str()
                                .unwrap_or("application/octet-stream")
                                .to_string();
                            let id = st.insert(id, &parent, &name, &mime, data);
                            ok_json(st.json(&id))
                        }
                    }
                }
                Some("resumable") => {
                    let size: usize = need(
                        headers
                            .get("x-upload-content-length")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse().ok()),
                        "a resumable upload without X-Upload-Content-Length",
                    )?;
                    let kind = match target {
                        Some(id) => SessKind::Replace(id),
                        None => {
                            let meta = json_body(&body)?;
                            first_parent(&meta)?;
                            meta_name(&meta)?;
                            SessKind::New(meta)
                        }
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
                    let loc =
                        format!("{api}/upload/drive/v3/files?uploadType=resumable&upload_id={sid}");
                    Response::builder()
                        .status(200)
                        .header(header::LOCATION, loc)
                        .body(Body::empty())
                        .unwrap_or_default()
                }
                other => return Err(format!("uploadType {other:?}")),
            }
        }
        ("PUT", ["upload", "drive", "v3", "files"]) => {
            if st.session_down {
                return Ok(err(503, "backendError"));
            }
            let sid = need(q.get("upload_id"), "a session PUT without upload_id")?.clone();
            let cr = need(
                headers
                    .get(header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok()),
                "a session PUT without Content-Range",
            )?
            .to_string();
            let bad = || format!("unparsable Content-Range {cr:?}");
            let spec = cr.strip_prefix("bytes ").ok_or_else(bad)?;
            let (range, total) = spec.split_once('/').ok_or_else(bad)?;
            let total: usize = total.parse().map_err(|_| bad())?;
            let Some(sess) = st.sessions.get(&sid) else {
                return Ok(err(404, "notFound"));
            };
            if total != sess.size {
                return Err(format!(
                    "Content-Range total {total} but the session was opened for {}",
                    sess.size
                ));
            }
            if let Some(id) = sess.done.clone() {
                return Ok(ok_json(st.json(&id)));
            }
            if range == "*" {
                return Ok(range_308(sess.buf.len()));
            }
            let (a, _b) = range.split_once('-').ok_or_else(bad)?;
            let a: usize = a.parse().map_err(|_| bad())?;
            st.chunk_puts += 1;
            let n = st.chunk_puts;
            let interrupt = st.interrupt_chunk == Some(n);
            let sess = st.sessions.get_mut(&sid).expect("checked above");
            if a != sess.buf.len() {
                return Ok(range_308(sess.buf.len()));
            }
            if interrupt {
                sess.buf.extend_from_slice(&body[..body.len() / 2]);
                st.session_down = st.down_after_interrupt;
                return Ok(err(503, "backendError"));
            }
            sess.buf.extend_from_slice(&body);
            if sess.buf.len() < sess.size {
                return Ok(range_308(sess.buf.len()));
            }
            let data = Bytes::from(std::mem::take(&mut sess.buf));
            let kind = std::mem::replace(&mut sess.kind, SessKind::Replace(String::new()));
            let id = match kind {
                SessKind::Replace(id) => {
                    st.rewrite(&id, data);
                    id
                }
                SessKind::New(meta) => {
                    let mime = meta["mimeType"]
                        .as_str()
                        .unwrap_or("application/octet-stream")
                        .to_string();
                    st.insert(
                        meta["id"].as_str().map(str::to_string),
                        &first_parent(&meta)?,
                        &meta_name(&meta)?,
                        &mime,
                        data,
                    )
                }
            };
            if let Some(s) = st.sessions.get_mut(&sid) {
                s.done = Some(id.clone());
            }
            (StatusCode::OK, axum::Json(st.json(&id))).into_response()
        }
        _ => return Err("no such endpoint".into()),
    })
}

/// One Drive account, for tests: its state behind [`DriveHandle::lock`], seeding and
/// fault helpers, and the requests it received.
#[derive(Clone)]
pub struct DriveHandle(pub(crate) Arc<Mutex<DriveState>>);

impl DriveHandle {
    pub fn lock(&self) -> MutexGuard<'_, DriveState> {
        self.0.lock()
    }

    pub fn mkdir(&self, parent: &str, name: &str) -> String {
        self.lock().insert(None, parent, name, FOLDER, Bytes::new())
    }

    pub fn put(&self, parent: &str, name: &str, data: &[u8]) -> String {
        self.lock().insert(
            None,
            parent,
            name,
            "application/octet-stream",
            Bytes::copy_from_slice(data),
        )
    }

    pub fn data(&self, id: &str) -> Vec<u8> {
        self.lock().files[id].data.to_vec()
    }

    pub fn head(&self, id: &str) -> String {
        self.lock().files[id].head_rev.clone()
    }

    pub fn fault(&self, method: Option<&str>, path_has: &str, status: u16, reason: &str) {
        self.fault_n(method, path_has, status, reason, None, 1);
    }

    pub fn fault_n(
        &self,
        method: Option<&str>,
        path_has: &str,
        status: u16,
        reason: &str,
        retry_after: Option<u64>,
        times: usize,
    ) {
        self.lock().faults.push(Fault {
            method: method.map(str::to_string),
            path_has: path_has.into(),
            status,
            reason: reason.into(),
            retry_after,
            times,
        });
    }

    pub fn requests(&self) -> Vec<Req> {
        self.lock().requests.clone()
    }

    /// Requests with exactly this method and path (and query pair, if given).
    pub fn count_path(&self, method: &str, path: &str, query: Option<(&str, &str)>) -> usize {
        self.requests()
            .iter()
            .filter(|r| {
                r.method == method
                    && r.path == path
                    && query.is_none_or(|(k, v)| r.query.get(k).map(String::as_str) == Some(v))
            })
            .count()
    }

    /// Resumable status queries (`Content-Range: bytes */N`).
    pub fn status_queries(&self) -> usize {
        self.requests()
            .iter()
            .filter(|r| {
                r.content_range
                    .as_deref()
                    .is_some_and(|c| c.starts_with("bytes */"))
            })
            .count()
    }

    /// Requests with this method whose path contains `path_has`.
    pub fn count(&self, method: &str, path_has: &str, query: Option<(&str, &str)>) -> usize {
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
