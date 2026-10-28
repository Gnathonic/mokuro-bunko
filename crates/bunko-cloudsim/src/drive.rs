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

use crate::accounts::Provider;
use crate::log::LogTag;
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
    pub description: Option<String>,
}

enum SessKind {
    New(Value),
    Replace(String),
}

struct Sess {
    kind: SessKind,
    /// Declared at the start (`X-Upload-Content-Length`) or by the first PUT that
    /// names a total.
    size: Option<usize>,
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
    /// A fixed bearer token besides the ones the simulator issues (honoured for the
    /// `default` account only).
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
    /// Check, on every request, what Bunko's driver must always send and other clients
    /// may leave out (bunko-remote's tests turn this on; the reader leaves it off).
    pub strict_bunko: bool,
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
            strict_bunko: false,
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

    /// Set the metadata a create carries beyond name, parent and type.
    fn describe(&mut self, id: &str, meta: &Value) {
        if let (Some(d), Some(f)) = (meta["description"].as_str(), self.files.get_mut(id)) {
            f.description = Some(d.to_string());
        }
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
        if let Some(d) = &f.description {
            v["description"] = json!(d);
        }
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
                description: None,
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

/// The `q` of `files.list`: `and`, `or`, `not` and parentheses over `'me' in owners`,
/// `'<id>' in parents`, `name`, `mimeType` and `trashed` compared with `=` or `!=`.
/// Anything else is an error naming it, so a client's query the simulator cannot
/// answer is a violation rather than a wrong listing.
mod query {
    use super::DriveState;
    use crate::violations::Bad;

    #[derive(Debug, Clone, PartialEq)]
    enum Tok {
        Str(String),
        Word(String),
        Eq,
        Ne,
        Open,
        Close,
    }

    fn lex(q: &str) -> Result<Vec<Tok>, Bad> {
        let mut out = Vec::new();
        let mut it = q.chars().peekable();
        while let Some(&c) = it.peek() {
            match c {
                c if c.is_whitespace() => {
                    it.next();
                }
                '(' => {
                    it.next();
                    out.push(Tok::Open);
                }
                ')' => {
                    it.next();
                    out.push(Tok::Close);
                }
                '=' => {
                    it.next();
                    out.push(Tok::Eq);
                }
                '!' => {
                    it.next();
                    if it.next() != Some('=') {
                        return Err(format!("q {q:?}: `!` not followed by `=`"));
                    }
                    out.push(Tok::Ne);
                }
                '\'' => {
                    it.next();
                    let mut s = String::new();
                    loop {
                        match it.next() {
                            Some('\\') => match it.next() {
                                Some(e) => s.push(e),
                                None => return Err(format!("q {q:?}: unterminated string")),
                            },
                            Some('\'') => break,
                            Some(ch) => s.push(ch),
                            None => return Err(format!("q {q:?}: unterminated string")),
                        }
                    }
                    out.push(Tok::Str(s));
                }
                c if c.is_ascii_alphanumeric() || c == '_' => {
                    let mut w = String::new();
                    while let Some(&ch) = it.peek() {
                        if ch.is_ascii_alphanumeric() || ch == '_' {
                            w.push(ch);
                            it.next();
                        } else {
                            break;
                        }
                    }
                    out.push(Tok::Word(w));
                }
                other => return Err(format!("q {q:?}: unexpected {other:?}")),
            }
        }
        Ok(out)
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum Field {
        Name,
        MimeType,
        Trashed,
    }

    #[derive(Debug)]
    pub enum Expr {
        And(Box<Expr>, Box<Expr>),
        Or(Box<Expr>, Box<Expr>),
        Not(Box<Expr>),
        Owned,
        InParents(String),
        Cmp {
            field: Field,
            ne: bool,
            value: String,
        },
    }

    struct Parser<'a> {
        q: &'a str,
        toks: Vec<Tok>,
        at: usize,
    }

    impl Parser<'_> {
        fn peek(&self) -> Option<&Tok> {
            self.toks.get(self.at)
        }

        fn next(&mut self) -> Option<Tok> {
            let t = self.toks.get(self.at).cloned();
            self.at += 1;
            t
        }

        fn word(&self, w: &str) -> bool {
            matches!(self.peek(), Some(Tok::Word(x)) if x == w)
        }

        fn bad(&self, what: &str) -> Bad {
            format!("unsupported q clause in {:?}: {what}", self.q)
        }

        fn or(&mut self) -> Result<Expr, Bad> {
            let mut e = self.and()?;
            while self.word("or") {
                self.at += 1;
                e = Expr::Or(Box::new(e), Box::new(self.and()?));
            }
            Ok(e)
        }

        fn and(&mut self) -> Result<Expr, Bad> {
            let mut e = self.unary()?;
            while self.word("and") {
                self.at += 1;
                e = Expr::And(Box::new(e), Box::new(self.unary()?));
            }
            Ok(e)
        }

        fn unary(&mut self) -> Result<Expr, Bad> {
            if self.word("not") {
                self.at += 1;
                return Ok(Expr::Not(Box::new(self.unary()?)));
            }
            if self.peek() == Some(&Tok::Open) {
                self.at += 1;
                let e = self.or()?;
                if self.next() != Some(Tok::Close) {
                    return Err(self.bad("missing `)`"));
                }
                return Ok(e);
            }
            self.atom()
        }

        fn atom(&mut self) -> Result<Expr, Bad> {
            match self.next() {
                Some(Tok::Str(s)) => {
                    if !self.word("in") {
                        return Err(self.bad(&format!("'{s}' not followed by `in`")));
                    }
                    self.at += 1;
                    match self.next() {
                        Some(Tok::Word(w)) if w == "owners" && s == "me" => Ok(Expr::Owned),
                        Some(Tok::Word(w)) if w == "parents" => Ok(Expr::InParents(s)),
                        other => Err(self.bad(&format!("'{s}' in {other:?}"))),
                    }
                }
                Some(Tok::Word(w)) => {
                    let field = match w.as_str() {
                        "name" => Field::Name,
                        "mimeType" => Field::MimeType,
                        "trashed" => Field::Trashed,
                        _ => return Err(self.bad(&format!("field {w:?}"))),
                    };
                    let ne = match self.next() {
                        Some(Tok::Eq) => false,
                        Some(Tok::Ne) => true,
                        other => return Err(self.bad(&format!("{w} {other:?}"))),
                    };
                    let value = match (field, self.next()) {
                        (Field::Trashed, Some(Tok::Word(b))) if b == "true" || b == "false" => b,
                        (Field::Name | Field::MimeType, Some(Tok::Str(v))) => v,
                        (_, other) => return Err(self.bad(&format!("{w} compared with {other:?}"))),
                    };
                    Ok(Expr::Cmp { field, ne, value })
                }
                other => Err(self.bad(&format!("{other:?}"))),
            }
        }
    }

    pub fn parse(q: &str) -> Result<Expr, Bad> {
        let mut p = Parser {
            q,
            toks: lex(q)?,
            at: 0,
        };
        if p.toks.is_empty() {
            return Err("files.list without q".into());
        }
        let e = p.or()?;
        if p.at != p.toks.len() {
            return Err(p.bad("trailing tokens"));
        }
        Ok(e)
    }

    impl Expr {
        /// Narrowed to the user's files, a parent or a name (not the whole corpus).
        pub fn scoped(&self) -> bool {
            match self {
                Expr::And(a, b) => a.scoped() || b.scoped(),
                Expr::Or(a, b) => a.scoped() && b.scoped(),
                Expr::Not(_) => false,
                Expr::Owned | Expr::InParents(_) => true,
                Expr::Cmp { field, ne, .. } => *field == Field::Name && !ne,
            }
        }

        pub fn matches(&self, st: &DriveState, id: &str) -> bool {
            let f = &st.files[id];
            match self {
                Expr::And(a, b) => a.matches(st, id) && b.matches(st, id),
                Expr::Or(a, b) => a.matches(st, id) || b.matches(st, id),
                Expr::Not(e) => !e.matches(st, id),
                Expr::Owned => true,
                Expr::InParents(p) => f.parents.iter().any(|x| x == p),
                Expr::Cmp { field, ne, value } => {
                    let eq = match field {
                        // Drive's `name =` match is case-insensitive.
                        Field::Name => value.eq_ignore_ascii_case(&f.name),
                        Field::MimeType => *value == f.mime,
                        Field::Trashed => (value == "true") == st.trashed(id),
                    };
                    eq != *ne
                }
            }
        }
    }
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

/// The log class of a Drive request.
fn class(method: &str, path: &str, q: &HashMap<String, String>) -> &'static str {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let media = q.get("alt").map(String::as_str) == Some("media");
    match (method, segs.as_slice()) {
        (_, ["upload", ..]) => "upload",
        ("GET", ["drive", "v3", "changes", ..]) => "changes",
        ("GET", ["drive", "v3", "files"]) => "list",
        ("GET", ["drive", "v3", "files", _, "revisions", _]) => "download",
        ("GET", ["drive", "v3", "files", _]) if media => "download",
        ("DELETE", _) => "delete",
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
    let path = uri
        .path()
        .strip_prefix(PREFIX)
        .unwrap_or(uri.path())
        .to_string();
    let tag = LogTag {
        provider: Provider::Google,
        account: String::new(),
        class: class(method.as_str(), &path, &q),
        bytes_in: body.len() as u64,
    };
    let (account, mut r) = answer(&sim, &method, path, &q, &headers, body);
    r.extensions_mut().insert(LogTag { account, ..tag });
    r
}

fn answer(
    sim: &SimState,
    method: &Method,
    path: String,
    q: &HashMap<String, String>,
    headers: &HeaderMap,
    body: Bytes,
) -> (String, Response) {
    let session_put = *method == Method::PUT && path == "/upload/drive/v3/files";
    let acct = match bearer(headers) {
        Some(t) => sim.drive_for_token(t),
        // A resumable session's URL needs no token: its id names the account.
        None if session_put => q
            .get("upload_id")
            .and_then(|u| u.split_once('.'))
            .and_then(|(a, _)| hex::decode(a).ok())
            .and_then(|a| String::from_utf8(a).ok())
            .and_then(|a| sim.drive_existing(&a)),
        None => None,
    };
    let Some((account, acct)) = acct else {
        return (String::new(), err(401, "authError"));
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
            return (account, r);
        }
        let api = format!("{}{PREFIX}", sim.base());
        route(&mut st, &account, &api, method, &path, q, headers, body)
    };
    let r = res.unwrap_or_else(|bad| {
        sim.violation(format!("google {method} {path}: {bad}"));
        err_msg(400, "badRequest", &bad)
    });
    (account, r)
}

#[allow(clippy::too_many_arguments)]
fn route(
    st: &mut DriveState,
    account: &str,
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
            let expr = query::parse(&query)?;
            if !expr.scoped() {
                return Err(format!("q {query:?} would list the whole drive"));
            }
            let mut ids: Vec<String> = st
                .files
                .keys()
                .filter(|id| expr.matches(st, id))
                .cloned()
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
            st.describe(&id, &meta);
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
            if let Some(d) = meta["description"].as_str() {
                f.description = Some(d.to_string());
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
                if st.strict_bunko
                    && q.get("keepRevisionForever").map(String::as_str) != Some("false")
                {
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
                            st.describe(&id, &meta);
                            ok_json(st.json(&id))
                        }
                    }
                }
                Some("resumable") => {
                    // Optional: without it the size comes with the data.
                    let size: Option<usize> = match headers.get("x-upload-content-length") {
                        Some(v) => Some(
                            v.to_str()
                                .ok()
                                .and_then(|v| v.parse().ok())
                                .ok_or("an unparsable X-Upload-Content-Length")?,
                        ),
                        None => None,
                    };
                    let kind = match target {
                        Some(id) => SessKind::Replace(id),
                        None => {
                            let meta = json_body(&body)?;
                            first_parent(&meta)?;
                            meta_name(&meta)?;
                            SessKind::New(meta)
                        }
                    };
                    // The session URL is its own credential (Drive's needs no token):
                    // its id names the account.
                    let sid = format!("{}.{}", hex::encode(account), st.new_id("s"));
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
            // Without Content-Range the PUT is the whole file in one request.
            let cr = match headers
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
            {
                Some(cr) => cr.to_string(),
                None => format!("bytes 0-{}/{}", body.len().saturating_sub(1), body.len()),
            };
            let bad = || format!("unparsable Content-Range {cr:?}");
            let spec = cr.strip_prefix("bytes ").ok_or_else(bad)?;
            let (range, total) = spec.split_once('/').ok_or_else(bad)?;
            let total: Option<usize> = match total {
                "*" => None,
                t => Some(t.parse().map_err(|_| bad())?),
            };
            let Some(sess) = st.sessions.get_mut(&sid) else {
                return Ok(err(404, "notFound"));
            };
            match (total, sess.size) {
                (Some(t), Some(size)) if t != size => {
                    return Err(format!(
                        "Content-Range total {t} but the session was opened for {size}"
                    ));
                }
                (Some(t), None) => sess.size = Some(t),
                _ => {}
            }
            let sess = &*sess;
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
            if sess.size.is_none_or(|size| sess.buf.len() < size) {
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
                    let id = st.insert(
                        meta["id"].as_str().map(str::to_string),
                        &first_parent(&meta)?,
                        &meta_name(&meta)?,
                        &mime,
                        data,
                    );
                    st.describe(&id, &meta);
                    id
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
