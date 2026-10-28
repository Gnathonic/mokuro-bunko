//! Microsoft Graph v1.0 (OneDrive): the endpoints Bunko's driver and the reader's
//! provider call, under `{base}/graph/v1.0`, plus the pre-authenticated download,
//! upload-session and copy-monitor URLs under `{base}/graph/{preauth|upload|monitor}/
//! {account}/{id}` (they carry no token, so the account is in the URL, hex-encoded).
//! Those URLs use the other loopback name (`localhost` for `127.0.0.1` and back): a
//! different origin, as Microsoft's storage hosts are, so a browser or `fetch`
//! following the download redirect drops the `Authorization` header as it would for
//! the real thing.
//! One [`OneDriveState`] per account.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use parking_lot::{Mutex, MutexGuard};
use serde_json::{Value, json};

use crate::accounts::Provider;
use crate::drive::bearer;
use crate::log::LogTag;
use crate::violations::{Bad, need};
use crate::{SimState, rfc3339};

pub const ROOT: &str = "ROOT";
/// The path prefix every OneDrive URL lives under.
pub const PREFIX: &str = "/graph";
/// Non-final upload fragments must be multiples of this (320 KiB).
pub const FRAGMENT_MULTIPLE: u64 = 320 << 10;

pub struct MItem {
    pub name: String,
    pub parent: Option<String>,
    pub folder: bool,
    pub data: Bytes,
    pub created: i64,
    pub modified: i64,
    pub etag_n: u64,
    pub ctag_n: u64,
    pub deleted: bool,
}

enum SessKind {
    New {
        parent: String,
        name: String,
    },
    /// A path upload with `conflictBehavior: replace`: creates, or rewrites what is there.
    Upsert {
        parent: String,
        name: String,
    },
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

/// One request as the account saw it.
#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    /// Decoded, without the `/graph` and `/v1.0` prefixes.
    pub path: String,
    pub query: HashMap<String, String>,
    pub auth: Option<String>,
    pub if_match: Option<String>,
    pub content_range: Option<String>,
    pub user_agent: Option<String>,
}

/// An injected error: the next `times` requests matching `method` (any when `None`)
/// and containing `path_has` get `status` with Graph's error body for `code`.
#[derive(Debug, Clone)]
pub struct Fault {
    pub method: Option<String>,
    pub path_has: String,
    pub status: u16,
    pub code: String,
    pub retry_after: Option<u64>,
    pub times: usize,
}

/// One account's drive and its knobs.
pub struct OneDriveState {
    pub items: BTreeMap<String, MItem>,
    pub seq: u64,
    pub clock: i64,
    pub log: Vec<String>,
    /// Delta tokens below this answer 410.
    pub lost_below: usize,
    pub page_size: usize,
    sessions: HashMap<String, Sess>,
    /// A fixed bearer token this account accepts besides the ones the simulator issues.
    pub valid_token: String,
    pub faults: Vec<Fault>,
    pub requests: Vec<Req>,
    /// Fail this (1-based) fragment PUT after taking half its bytes.
    pub interrupt_fragment: Option<usize>,
    pub down_after_interrupt: bool,
    pub session_down: bool,
    pub fragment_puts: usize,
    monitors: HashMap<String, Monitor>,
    pub send_ctag: bool,
    /// The download's CTag in the storage host's own form, `{id},N,1` (personal
    /// OneDrive, seen live 2026-10-14), not the JSON `c:{id},N`.
    pub storage_ctag: bool,
    pub ignore_range: bool,
    pub permanent_delete: bool,
    pub quota_total: i64,
    /// A pre-authenticated host (download, upload session, monitor) saw a token.
    pub preauth_saw_auth: bool,
}

fn err_msg(status: u16, code: &str, message: &str) -> Response {
    let body = json!({"error": {"code": code, "message": message, "innerError": {"code": code}}});
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
        axum::Json(body),
    )
        .into_response()
}

fn err(status: u16, code: &str) -> Response {
    err_msg(status, code, code)
}

fn ok_json(v: Value) -> Response {
    axum::Json(v).into_response()
}

fn status_json(s: u16, v: Value) -> Response {
    (
        StatusCode::from_u16(s).unwrap_or(StatusCode::OK),
        axum::Json(v),
    )
        .into_response()
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%')
            .then(|| b.get(i + 1..i + 3))
            .flatten()
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(v) => {
                out.push(v);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn bare(t: &str) -> &str {
    t.trim().trim_matches('"')
}

impl Default for OneDriveState {
    fn default() -> Self {
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
        OneDriveState {
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
            storage_ctag: false,
            ignore_range: false,
            permanent_delete: true,
            quota_total: 1 << 30,
            preauth_saw_auth: false,
        }
    }
}

impl OneDriveState {
    fn tick(&mut self) -> i64 {
        self.clock += 1000;
        self.clock
    }

    fn new_id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("{prefix}{:05}", self.seq)
    }

    pub fn etag(&self, id: &str) -> String {
        format!("\"{{{id}}},{}\"", self.items[id].etag_n)
    }

    pub fn ctag(&self, id: &str) -> String {
        format!("\"c:{{{id}}},{}\"", self.items[id].ctag_n)
    }

    pub fn live(&self, id: &str) -> bool {
        self.items.get(id).is_some_and(|i| !i.deleted)
    }

    pub fn json(&self, id: &str) -> Value {
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

    pub fn kids(&self, id: &str) -> Vec<String> {
        self.items
            .iter()
            .filter(|(_, f)| f.parent.as_deref() == Some(id) && !f.deleted)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// OneDrive is case-insensitive: the live child of `parent` folding to `name`.
    pub fn child(&self, parent: &str, name: &str) -> Option<String> {
        self.kids(parent)
            .into_iter()
            .find(|k| self.items[k].name.to_lowercase() == name.to_lowercase())
    }

    /// Log a change of `id`; its ancestors show up in delta too.
    pub fn touch(&mut self, id: &str) {
        self.log.push(id.to_string());
        let mut cur = self.items.get(id).and_then(|i| i.parent.clone());
        while let Some(p) = cur {
            self.log.push(p.clone());
            cur = self.items.get(&p).and_then(|i| i.parent.clone());
        }
    }

    pub fn insert(&mut self, parent: &str, name: &str, folder: bool, data: Bytes) -> String {
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

    pub fn rewrite(&mut self, id: &str, data: Bytes) {
        let now = self.tick();
        let Some(f) = self.items.get_mut(id) else {
            return;
        };
        f.data = data;
        f.etag_n += 1;
        f.ctag_n += 1;
        f.modified = now;
        self.touch(id);
    }

    pub fn delete(&mut self, id: &str) {
        for k in self.kids(id) {
            self.delete(&k);
        }
        if let Some(i) = self.items.get_mut(id) {
            i.deleted = true;
        }
        self.log.push(id.to_string());
    }

    /// `If-Match` against the eTag or the cTag.
    pub fn matches(&self, id: &str, if_match: Option<&str>) -> bool {
        if_match.is_none_or(|t| bare(t) == bare(&self.etag(id)) || bare(t) == bare(&self.ctag(id)))
    }

    /// Create the folders along `path` and put `data` at its end, replacing a file
    /// already there.
    pub fn seed(&mut self, path: &str, data: Bytes) -> String {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut parent = ROOT.to_string();
        for dir in &parts[..parts.len().saturating_sub(1)] {
            parent = match self.child(&parent, dir) {
                Some(id) => id,
                None => self.insert(&parent, dir, true, Bytes::new()),
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
            None => self.insert(&parent, name, false, data),
        }
    }

    /// Every live file: (path, bytes).
    pub fn files(&self) -> Vec<(String, Bytes)> {
        let mut out = Vec::new();
        for f in self.items.values() {
            if f.folder || f.deleted {
                continue;
            }
            let mut parts = vec![f.name.clone()];
            let mut cur = f.parent.clone();
            while let Some(p) = cur {
                if p == ROOT {
                    break;
                }
                let Some(pf) = self.items.get(&p) else { break };
                parts.push(pf.name.clone());
                cur = pf.parent.clone();
            }
            parts.reverse();
            out.push((parts.join("/"), f.data.clone()));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

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

/// OneDrive's QuickXorHash, base64, written plainly from the published algorithm (the
/// n-th byte XORed in at bit `11n mod 160`, the length XORed into the last 8 bytes) so
/// it checks the driver's faster one rather than sharing it.
pub fn quickxor_base64(data: &[u8]) -> String {
    use base64::Engine;
    let mut bits = [0u8; 20];
    for (n, &b) in data.iter().enumerate() {
        let pos = (n * 11) % 160;
        for k in 0..8 {
            if b >> k & 1 == 1 {
                let bit = (pos + k) % 160;
                bits[bit / 8] ^= 1 << (bit % 8);
            }
        }
    }
    for (o, l) in bits[12..].iter_mut().zip((data.len() as u64).to_le_bytes()) {
        *o ^= l;
    }
    base64::engine::general_purpose::STANDARD.encode(bits)
}

fn media(data: &Bytes, headers: &HeaderMap, ignore_range: bool) -> Result<Response, Bad> {
    let r = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let Some(r) = r.filter(|_| !ignore_range) else {
        return Ok((StatusCode::OK, data.clone()).into_response());
    };
    let bad = || format!("unparsable Range {r:?}");
    let spec = r.strip_prefix("bytes=").ok_or_else(bad)?;
    let (a, b) = spec.split_once('-').ok_or_else(bad)?;
    let a: usize = a.parse().map_err(|_| bad())?;
    if a >= data.len() {
        return Ok(err(416, "invalidRange"));
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

fn hdr(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn json_body(body: &[u8]) -> Result<Value, Bad> {
    serde_json::from_slice(body).map_err(|e| format!("body is not JSON: {e}"))
}

/// The account a pre-authenticated URL names (`{kind}/{hex account}/{id}`).
pub(crate) fn preauth_account(hexed: &str) -> Option<String> {
    String::from_utf8(hex::decode(hexed).ok()?).ok()
}

/// The log class of a OneDrive request (`segs` without the prefixes).
fn class(method: &str, graph: bool, segs: &[String]) -> &'static str {
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    if !graph {
        return match segs.first() {
            Some(&"preauth") => "download",
            Some(&"upload") => "upload",
            _ => "metadata",
        };
    }
    match (method, segs.as_slice()) {
        (_, [.., "createUploadSession"]) | ("PUT", [.., "content"]) => "upload",
        ("GET", [.., "content"]) => "download",
        ("GET", [.., "delta"]) => "changes",
        ("GET", [.., "children"]) => "list",
        ("DELETE", _) | (_, [.., "permanentDelete"]) => "delete",
        ("POST" | "PATCH" | "PUT", _) => "write",
        _ => "metadata",
    }
}

pub async fn handle(
    State(sim): State<SimState>,
    method: Method,
    uri: Uri,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut tag = LogTag {
        provider: Provider::Microsoft,
        account: String::new(),
        class: "metadata",
        bytes_in: body.len() as u64,
    };
    let mut r = answer(&sim, &method, &uri, &q, &headers, body, &mut tag);
    r.extensions_mut().insert(tag);
    r
}

fn answer(
    sim: &SimState,
    method: &Method,
    uri: &Uri,
    q: &HashMap<String, String>,
    headers: &HeaderMap,
    body: Bytes,
    tag: &mut LogTag,
) -> Response {
    let raw_path = uri
        .path()
        .strip_prefix(PREFIX)
        .unwrap_or(uri.path())
        .to_string();
    let graph = raw_path.starts_with("/v1.0/");
    let segs: Vec<String> = raw_path
        .trim_start_matches("/v1.0")
        .trim_start_matches('/')
        .split('/')
        .map(pct_decode)
        .collect();
    let path = format!("/{}", segs.join("/"));
    tag.class = class(method.as_str(), graph, &segs);
    let auth = hdr(headers, header::AUTHORIZATION);
    let acct = if graph {
        bearer(headers).and_then(|t| sim.onedrive_for_token(t))
    } else {
        segs.get(1)
            .and_then(|a| preauth_account(a))
            .and_then(|a| sim.onedrive_existing(&a))
    };
    let Some((account, acct)) = acct else {
        if graph {
            return err(401, "InvalidAuthenticationToken");
        }
        return err(404, "itemNotFound");
    };
    tag.account = account.clone();
    let if_match = hdr(headers, header::IF_MATCH);
    let res = {
        let mut st = acct.lock();
        st.requests.push(Req {
            method: method.to_string(),
            path: path.clone(),
            query: q.clone(),
            auth: auth.clone(),
            if_match: if_match.clone(),
            content_range: hdr(headers, header::CONTENT_RANGE),
            user_agent: hdr(headers, header::USER_AGENT),
        });
        if !graph && auth.is_some() {
            st.preauth_saw_auth = true;
            return err(401, "tokenNotExpected");
        }
        if let Some(f) = st.take_fault(method.as_str(), &path) {
            let mut r = err(f.status, &f.code);
            if let Some(s) = f.retry_after {
                r.headers_mut()
                    .insert(header::RETRY_AFTER, s.to_string().parse().expect("digits"));
            }
            return r;
        }
        let root = format!("{}{PREFIX}", sim.base());
        let preauth_root = format!("{}{PREFIX}", sim.preauth_base());
        let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
        let cx = Cx {
            root: &root,
            preauth_root: &preauth_root,
            account_hex: &hex::encode(&account),
            method,
            q,
            headers,
            if_match: if_match.as_deref(),
        };
        if graph {
            route_graph(&mut st, &cx, &segs, body)
        } else {
            route_preauth(&mut st, &cx, &segs, body)
        }
    };
    res.unwrap_or_else(|bad| {
        sim.violation(format!("microsoft {method} {path}: {bad}"));
        err_msg(400, "invalidRequest", &bad)
    })
}

struct Cx<'a> {
    /// `{base}/graph`.
    root: &'a str,
    /// `{base}/graph` on the other loopback name, for the pre-authenticated URLs.
    preauth_root: &'a str,
    account_hex: &'a str,
    method: &'a Method,
    q: &'a HashMap<String, String>,
    headers: &'a HeaderMap,
    if_match: Option<&'a str>,
}

impl Cx<'_> {
    fn base(&self) -> String {
        format!("{}/v1.0", self.root)
    }

    fn preauth(&self, kind: &str, id: &str) -> String {
        format!("{}/{kind}/{}/{id}", self.preauth_root, self.account_hex)
    }
}

fn route_preauth(
    st: &mut OneDriveState,
    cx: &Cx,
    segs: &[&str],
    body: Bytes,
) -> Result<Response, Bad> {
    Ok(match (cx.method.as_str(), segs) {
        ("GET", ["preauth", _, id]) => {
            if !st.live(id) {
                return Ok(err(404, "itemNotFound"));
            }
            let mut r = media(&st.items[*id].data.clone(), cx.headers, st.ignore_range)?;
            let etag = st.etag(id);
            r.headers_mut()
                .insert(header::ETAG, etag.parse().expect("ascii"));
            if st.send_ctag {
                let ctag = if st.storage_ctag {
                    format!("{{{id}}},{},1", st.items[*id].ctag_n)
                } else {
                    st.ctag(id)
                };
                r.headers_mut().insert("ctag", ctag.parse().expect("ascii"));
            }
            r
        }
        ("GET", ["upload", _, sid]) => match st.sessions.get(*sid) {
            Some(s) => ok_json(json!({
                "expirationDateTime": "2099-01-01T00:00:00Z",
                "nextExpectedRanges": [format!("{}-{}", s.buf.len(), s.size.saturating_sub(1))],
            })),
            None => err(404, "itemNotFound"),
        },
        ("PUT", ["upload", _, sid]) => upload_fragment(st, sid, cx.headers, &body)?,
        ("GET", ["monitor", _, mid]) => {
            let Some(mon) = st.monitors.get_mut(*mid) else {
                return Ok(err(404, "itemNotFound"));
            };
            if mon.polls_left > 0 {
                mon.polls_left -= 1;
                return Ok(status_json(
                    202,
                    json!({"operation": "itemCopy", "status": "inProgress", "percentageComplete": 50.0}),
                ));
            }
            let mon = st.monitors.remove(*mid).expect("checked above");
            if st.child(&mon.parent, &mon.name).is_some() {
                return Ok(ok_json(
                    json!({"status": "failed", "error": {"code": "nameAlreadyExists", "message": "exists"}}),
                ));
            }
            let Some((folder, data)) = st.items.get(&mon.src).map(|s| (s.folder, s.data.clone()))
            else {
                return Ok(ok_json(
                    json!({"status": "failed", "error": {"code": "itemNotFound", "message": "gone"}}),
                ));
            };
            let id = st.insert(&mon.parent, &mon.name, folder, data);
            ok_json(json!({"status": "completed", "resourceId": id}))
        }
        _ => return Err("no such endpoint".into()),
    })
}

fn route_graph(
    st: &mut OneDriveState,
    cx: &Cx,
    segs: &[&str],
    body: Bytes,
) -> Result<Response, Bad> {
    let base = cx.base();
    let q = cx.q;
    Ok(match (cx.method.as_str(), segs) {
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
        (_, ["me", "drive", "root:", rest @ ..]) => route_root_path(st, cx, rest, body)?,
        ("GET", ["me", "drive", "root"]) => ok_json(st.json(ROOT)),
        ("GET", ["me", "drive", "root", "children"]) => children(st, cx, ROOT)?,
        ("POST", ["me", "drive", "root", "children"]) => mkdir(st, ROOT, &body)?,
        ("GET", ["me", "drive", "root", "delta"]) => {
            let tok = q.get("token").cloned().unwrap_or_default();
            if tok == "latest" {
                return Ok(ok_json(json!({"value": [],
                    "@odata.deltaLink": format!("{base}/me/drive/root/delta?token={}", st.log.len())})));
            }
            let at: usize = tok
                .parse()
                .map_err(|_| format!("delta token {tok:?} is not one this simulator issued"))?;
            if at < st.lost_below || at > st.log.len() {
                let mut r = err(410, "resyncRequired");
                *r.body_mut() = Body::from(
                    json!({"error": {"code": "resyncRequired", "message": "resync",
                        "innerError": {"code": "resyncChangesApplyDifferences"}}})
                    .to_string(),
                );
                r.headers_mut().insert(
                    header::LOCATION,
                    format!("{base}/me/drive/root/delta")
                        .parse()
                        .expect("ascii"),
                );
                return Ok(r);
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
            let name = need(rest.first(), "a path address without a name")?
                .trim_end_matches(':')
                .to_string();
            let action = rest.get(1).copied();
            if !st.live(&parent) {
                return Ok(err(404, "itemNotFound"));
            }
            match (m, action) {
                ("GET", None) => match st.child(&parent, &name) {
                    Some(id) => ok_json(st.json(&id)),
                    None => err(404, "itemNotFound"),
                },
                ("PUT", Some("content")) => {
                    if q.get("@microsoft.graph.conflictBehavior")
                        .map(String::as_str)
                        != Some("fail")
                    {
                        return Err("a create by path without conflictBehavior=fail".into());
                    }
                    if st.child(&parent, &name).is_some() {
                        return Ok(err(409, "nameAlreadyExists"));
                    }
                    let id = st.insert(&parent, &name, false, body);
                    status_json(201, st.json(&id))
                }
                ("POST", Some("createUploadSession")) => {
                    let b = json_body(&body)?;
                    if b["item"]["@microsoft.graph.conflictBehavior"] != "fail" {
                        return Err("a new upload session without conflictBehavior fail".into());
                    }
                    if st.child(&parent, &name).is_some() {
                        return Ok(err(409, "nameAlreadyExists"));
                    }
                    new_session(st, cx, SessKind::New { parent, name })
                }
                _ => return Err("no such endpoint".into()),
            }
        }
        ("GET", ["me", "drive", "items", id]) => {
            if !st.live(id) {
                return Ok(err(404, "itemNotFound"));
            }
            ok_json(st.json(id))
        }
        ("PATCH", ["me", "drive", "items", id]) => {
            let id = id.to_string();
            if !st.live(&id) {
                return Ok(err(404, "itemNotFound"));
            }
            if !st.matches(&id, cx.if_match) {
                return Ok(err(412, "preconditionFailed"));
            }
            let b = json_body(&body)?;
            let new_parent = b["parentReference"]["id"]
                .as_str()
                .map(str::to_string)
                .or_else(|| st.items[&id].parent.clone())
                .unwrap_or_else(|| ROOT.into());
            let new_name = b["name"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| st.items[&id].name.clone());
            if !st.live(&new_parent) {
                return Ok(err(404, "itemNotFound"));
            }
            if st.child(&new_parent, &new_name).is_some_and(|c| c != id) {
                return Ok(err(409, "nameAlreadyExists"));
            }
            let f = st.items.get_mut(&id).expect("checked above");
            f.parent = Some(new_parent);
            f.name = new_name;
            f.etag_n += 1;
            st.touch(&id);
            ok_json(st.json(&id))
        }
        ("DELETE", ["me", "drive", "items", id]) => {
            let id = id.to_string();
            if !st.live(&id) {
                return Ok(err(404, "itemNotFound"));
            }
            if !st.matches(&id, cx.if_match) {
                return Ok(err(412, "preconditionFailed"));
            }
            st.delete(&id);
            StatusCode::NO_CONTENT.into_response()
        }
        ("POST", ["me", "drive", "items", id, "permanentDelete"]) => {
            if !st.permanent_delete {
                return Ok(err(400, "invalidRequest"));
            }
            let id = id.to_string();
            if !st.live(&id) {
                return Ok(err(404, "itemNotFound"));
            }
            st.delete(&id);
            StatusCode::NO_CONTENT.into_response()
        }
        ("GET", ["me", "drive", "items", id, "children"]) => {
            if !st.live(id) {
                return Ok(err(404, "itemNotFound"));
            }
            children(st, cx, id)?
        }
        ("POST", ["me", "drive", "items", id, "children"]) => {
            let id = id.to_string();
            if !st.live(&id) {
                return Ok(err(404, "itemNotFound"));
            }
            mkdir(st, &id, &body)?
        }
        ("GET", ["me", "drive", "items", id, "content"]) => {
            if !st.live(id) || st.items[*id].folder {
                return Ok(err(404, "itemNotFound"));
            }
            Response::builder()
                .status(302)
                .header(header::LOCATION, cx.preauth("preauth", id))
                .body(Body::empty())
                .unwrap_or_default()
        }
        ("PUT", ["me", "drive", "items", id, "content"]) => {
            let id = id.to_string();
            if !st.live(&id) {
                return Ok(err(404, "itemNotFound"));
            }
            if !st.matches(&id, cx.if_match) {
                return Ok(err(412, "preconditionFailed"));
            }
            st.rewrite(&id, body);
            ok_json(st.json(&id))
        }
        ("POST", ["me", "drive", "items", id, "createUploadSession"]) => {
            let id = id.to_string();
            if !st.live(&id) {
                return Ok(err(404, "itemNotFound"));
            }
            let b = json_body(&body)?;
            if b["item"]["@microsoft.graph.conflictBehavior"] != "replace" {
                return Err("a replacing upload session without conflictBehavior replace".into());
            }
            if !st.matches(&id, cx.if_match) {
                return Ok(err(412, "preconditionFailed"));
            }
            new_session(st, cx, SessKind::Replace(id))
        }
        ("POST", ["me", "drive", "items", id, "copy"]) => {
            if !st.live(id) {
                return Ok(err(404, "itemNotFound"));
            }
            let b = json_body(&body)?;
            let parent = need(
                b["parentReference"]["id"].as_str(),
                "a copy without parentReference.id",
            )?
            .to_string();
            let name = need(b["name"].as_str(), "a copy without a name")?.to_string();
            let mid = st.new_id("m");
            st.monitors.insert(
                mid.clone(),
                Monitor {
                    polls_left: 1,
                    src: id.to_string(),
                    parent,
                    name,
                },
            );
            Response::builder()
                .status(202)
                .header(header::LOCATION, cx.preauth("monitor", &mid))
                .body(Body::empty())
                .unwrap_or_default()
        }
        _ => return Err("no such endpoint".into()),
    })
}

/// One page of a folder's live children (`$top` is optional, as in Graph; the page
/// size is the account's `page_size`).
fn children(st: &OneDriveState, cx: &Cx, id: &str) -> Result<Response, Bad> {
    let kids = st.kids(id);
    let at: usize = match cx.q.get("$skiptoken") {
        Some(s) => s
            .parse()
            .map_err(|_| format!("$skiptoken {s:?} is not one this simulator issued"))?,
        None => 0,
    }
    .min(kids.len());
    let end = (at + st.page_size).min(kids.len());
    let value: Vec<Value> = kids[at..end].iter().map(|k| st.json(k)).collect();
    let mut v = json!({"value": value});
    if end < kids.len() {
        v["@odata.nextLink"] = json!(format!(
            "{}/me/drive/items/{id}/children?$top=2&$skiptoken={end}",
            cx.base()
        ));
    }
    Ok(ok_json(v))
}

/// The live item at `path` (names fold, as on OneDrive) under the drive root.
fn at_path(st: &OneDriveState, path: &[&str]) -> Option<String> {
    let mut cur = ROOT.to_string();
    for name in path.iter().filter(|n| !n.is_empty()) {
        cur = st.child(&cur, name)?;
    }
    Some(cur)
}

/// Root-relative path addressing: `/me/drive/root:/a/b/c[:/action]`.
fn route_root_path(
    st: &mut OneDriveState,
    cx: &Cx,
    rest: &[&str],
    body: Bytes,
) -> Result<Response, Bad> {
    let end = rest.iter().position(|s| s.ends_with(':'));
    let (path, action): (Vec<&str>, Option<&str>) = match end {
        Some(i) => (
            rest[..=i].iter().map(|s| s.trim_end_matches(':')).collect(),
            rest.get(i + 1).copied(),
        ),
        None => (rest.to_vec(), None),
    };
    if path.iter().all(|s| s.is_empty()) {
        return Err("a root path address without a path".into());
    }
    let (dirs, name) = path.split_at(path.len() - 1);
    let name = name[0];
    let item = at_path(st, &path);
    Ok(match (cx.method.as_str(), action) {
        ("GET", None) => match item {
            Some(id) => ok_json(st.json(&id)),
            None => err(404, "itemNotFound"),
        },
        ("GET", Some("children")) => match item {
            Some(id) if st.items[&id].folder => children(st, cx, &id)?,
            _ => err(404, "itemNotFound"),
        },
        ("POST", Some("children")) => match item {
            Some(id) if st.items[&id].folder => mkdir(st, &id, &body)?,
            _ => err(404, "itemNotFound"),
        },
        ("POST", Some("createUploadSession")) => {
            let Some(parent) = at_path(st, dirs) else {
                return Ok(err(404, "itemNotFound"));
            };
            let b = json_body(&body)?;
            let kind = match b["item"]["@microsoft.graph.conflictBehavior"].as_str() {
                Some("fail") => {
                    if item.is_some() {
                        return Ok(err(409, "nameAlreadyExists"));
                    }
                    SessKind::New {
                        parent,
                        name: name.to_string(),
                    }
                }
                Some("replace") | None => SessKind::Upsert {
                    parent,
                    name: name.to_string(),
                },
                Some(other) => return Err(format!("conflictBehavior {other:?}")),
            };
            new_session(st, cx, kind)
        }
        ("PUT", Some("content")) => {
            let Some(parent) = at_path(st, dirs) else {
                return Ok(err(404, "itemNotFound"));
            };
            match (
                item,
                cx.q.get("@microsoft.graph.conflictBehavior")
                    .map(String::as_str),
            ) {
                (Some(_), Some("fail")) => err(409, "nameAlreadyExists"),
                (Some(id), _) => {
                    st.rewrite(&id, body);
                    ok_json(st.json(&id))
                }
                (None, _) => {
                    let id = st.insert(&parent, name, false, body);
                    status_json(201, st.json(&id))
                }
            }
        }
        _ => return Err("no such endpoint".into()),
    })
}

fn mkdir(st: &mut OneDriveState, parent: &str, body: &[u8]) -> Result<Response, Bad> {
    let b = json_body(body)?;
    if b["@microsoft.graph.conflictBehavior"] != "fail" {
        return Err("a folder create without conflictBehavior fail".into());
    }
    if !b["folder"].is_object() {
        return Err("a folder create without a folder facet".into());
    }
    let name = need(b["name"].as_str(), "a folder create without a name")?;
    if st.child(parent, name).is_some() {
        return Ok(err(409, "nameAlreadyExists"));
    }
    let id = st.insert(parent, name, true, Bytes::new());
    Ok(status_json(201, st.json(&id)))
}

fn new_session(st: &mut OneDriveState, cx: &Cx, kind: SessKind) -> Response {
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
        "uploadUrl": cx.preauth("upload", &sid),
        "expirationDateTime": "2099-01-01T00:00:00Z",
        "nextExpectedRanges": ["0-"],
    }))
}

fn upload_fragment(
    st: &mut OneDriveState,
    sid: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Response, Bad> {
    if st.session_down {
        return Ok(err(503, "serviceNotAvailable"));
    }
    let cr = need(
        hdr(headers, header::CONTENT_RANGE),
        "a fragment without Content-Range",
    )?;
    let bad = || format!("unparsable Content-Range {cr:?}");
    let spec = cr.strip_prefix("bytes ").ok_or_else(bad)?;
    let (range, total) = spec.split_once('/').ok_or_else(bad)?;
    let total: usize = total.parse().map_err(|_| bad())?;
    let (a, b) = range.split_once('-').ok_or_else(bad)?;
    let (a, b): (usize, usize) = (a.parse().map_err(|_| bad())?, b.parse().map_err(|_| bad())?);
    if b + 1 < a || b + 1 - a != body.len() {
        return Err(format!(
            "Content-Range {cr:?} does not match the {} bytes sent",
            body.len()
        ));
    }
    let Some(sess) = st.sessions.get_mut(sid) else {
        return Ok(err(404, "itemNotFound"));
    };
    if sess.size == 0 {
        sess.size = total;
    }
    if total != sess.size {
        return Err(format!(
            "every fragment declares the same total: {total} after {}",
            sess.size
        ));
    }
    if b + 1 < total && !(body.len() as u64).is_multiple_of(FRAGMENT_MULTIPLE) {
        return Err(format!(
            "a non-final fragment of {} bytes, not a multiple of 320 KiB",
            body.len()
        ));
    }
    if a != sess.buf.len() {
        return Ok(err(416, "invalidRange"));
    }
    st.fragment_puts += 1;
    let n = st.fragment_puts;
    let interrupt = st.interrupt_fragment == Some(n);
    let sess = st.sessions.get_mut(sid).expect("checked above");
    if interrupt {
        sess.buf.extend_from_slice(&body[..body.len() / 2]);
        st.session_down = st.down_after_interrupt;
        return Ok(err(503, "serviceNotAvailable"));
    }
    sess.buf.extend_from_slice(body);
    if sess.buf.len() < sess.size {
        let next = sess.buf.len();
        return Ok(status_json(
            202,
            json!({"expirationDateTime": "2099-01-01T00:00:00Z", "nextExpectedRanges": [format!("{next}-")]}),
        ));
    }
    let sess = st.sessions.remove(sid).expect("checked above");
    let data = Bytes::from(sess.buf);
    let id = match sess.kind {
        SessKind::Replace(id) => {
            st.rewrite(&id, data);
            id
        }
        SessKind::New { parent, name } => {
            if st.child(&parent, &name).is_some() {
                return Ok(err(409, "nameAlreadyExists"));
            }
            st.insert(&parent, &name, false, data)
        }
        SessKind::Upsert { parent, name } => match st.child(&parent, &name) {
            Some(id) => {
                st.rewrite(&id, data);
                id
            }
            None => st.insert(&parent, &name, false, data),
        },
    };
    Ok(status_json(201, st.json(&id)))
}

/// One OneDrive account, for tests: its state behind [`OneDriveHandle::lock`],
/// seeding and fault helpers, and the requests it received.
#[derive(Clone)]
pub struct OneDriveHandle(pub(crate) Arc<Mutex<OneDriveState>>);

impl OneDriveHandle {
    pub fn lock(&self) -> MutexGuard<'_, OneDriveState> {
        self.0.lock()
    }

    pub fn mkdir(&self, parent: &str, name: &str) -> String {
        self.lock().insert(parent, name, true, Bytes::new())
    }

    pub fn put(&self, parent: &str, name: &str, data: &[u8]) -> String {
        self.lock()
            .insert(parent, name, false, Bytes::copy_from_slice(data))
    }

    pub fn data(&self, id: &str) -> Vec<u8> {
        self.lock().items[id].data.to_vec()
    }

    pub fn rewrite(&self, id: &str, data: &[u8]) {
        self.lock().rewrite(id, Bytes::copy_from_slice(data));
    }

    /// Move `id` under `parent` as another client would.
    pub fn mv(&self, id: &str, parent: &str) {
        let mut st = self.lock();
        if let Some(i) = st.items.get_mut(id) {
            i.parent = Some(parent.into());
            i.etag_n += 1;
        }
        st.touch(id);
    }

    pub fn rename(&self, id: &str, name: &str) {
        let mut st = self.lock();
        if let Some(i) = st.items.get_mut(id) {
            i.name = name.into();
            i.etag_n += 1;
        }
        st.touch(id);
    }

    pub fn delete(&self, id: &str) {
        self.lock().delete(id);
    }

    pub fn fault(&self, method: Option<&str>, path_has: &str, status: u16, code: &str) {
        self.fault_ra(method, path_has, status, code, None);
    }

    pub fn fault_ra(
        &self,
        method: Option<&str>,
        path_has: &str,
        status: u16,
        code: &str,
        retry_after: Option<u64>,
    ) {
        self.lock().faults.push(Fault {
            method: method.map(str::to_string),
            path_has: path_has.into(),
            status,
            code: code.into(),
            retry_after,
            times: 1,
        });
    }

    pub fn requests(&self) -> Vec<Req> {
        self.lock().requests.clone()
    }

    /// Requests with this method whose path contains `path_has`.
    pub fn count(&self, method: &str, path_has: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.method == method && r.path.contains(path_has))
            .count()
    }
}
