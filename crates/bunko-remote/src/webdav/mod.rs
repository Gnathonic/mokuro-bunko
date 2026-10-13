//! The WebDAV driver (spec §5.3, §5.7, §6; research `prov-webdav`): hand-rolled on
//! reqwest + quick-xml, Basic auth.
//!
//! **Ids.** A pid is the decoded path relative to the folder URL (`<url>/<folder>/`),
//! `""` being the folder itself; a move changes it (`caps.path_ids`). Every href a
//! server returns is percent-decoded and must lie under the folder URL, anything else is
//! ignored. The root named like the folder's last segment is the folder itself (pid
//! `""`); any other root name is a child of it.
//!
//! **Versions.** `version` = `cver` = `getetag` (or `<lastmod ms>-<size>` without one);
//! `pin` = the ETag when it is strong and the probe showed `If-Match` is honoured.
//!
//! **Change feed.** WebDAV has none that every server serves, so the cursor is a
//! generation of a snapshot (pid → version, is_dir) persisted in
//! `<state_dir>/webdav-snapshot.json`. [`Backend::changes`] re-lists the tracked roots
//! (Depth: infinity, or Depth 1 into the folders whose ETag changed when the probe showed
//! that nested and in-place writes propagate to ancestor ETags, else every folder) and
//! diffs against the snapshot. The snapshot of the cursor passed in is kept until the
//! caller moves on to the returned cursor, so a repeated call (delivery is at least
//! once) diffs from the same base again.
//!
//! **Writes.** With the probe's blessing, a create sends `If-None-Match: *` and a
//! replace with `if_cver` sends `If-Match`; otherwise the bytes go to
//! `.<name>.bunko-<op>.tmp` first and a MOVE (`Overwrite: T` for a replace, `F` for a
//! create) puts them in place. Every write ends with a PROPFIND Depth 0 of the target
//! (a PUT response may lack an ETag).

mod probe;
mod xml;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use futures_util::StreamExt;
use parking_lot::{Mutex, RwLock};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use reqwest::{Method, Response, StatusCode};
use serde::{Deserialize, Serialize};

use crate::backend::{
    AccountInfo, BErr, Backend, BackendCaps, ChangeItem, ChangePage, Cursor, OpCtx, Pid, RNode,
    RemoteBody, ServerTime, UploadSource, UploadTarget,
};

pub use probe::Probe;
use xml::DavEntry;

/// Path segment characters sent unescaped (RFC 3986 unreserved).
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// The refusal for a Bunko server as the backend (D15).
pub const BUNKO_BACKEND_REFUSED: &str = "That server is a Bunko library: connect Mokuro Reader to it directly, or make this machine its processor.";

const SNAPSHOT_FILE: &str = "webdav-snapshot.json";

/// The version of a folder the server gives no ETag.
const DIR_NO_ETAG: &str = "dir";

/// How to reach the WebDAV library.
#[derive(Clone, Default)]
pub struct WebDavConfig {
    /// The server URL (`https://host/remote.php/dav/files/user/`).
    pub url: String,
    pub username: String,
    pub password: String,
    /// The folder under `url` this backend is confined to (usually `mokuro-reader`;
    /// may be empty or several segments).
    pub folder: String,
    /// Accept `http://` (Basic auth in the clear).
    pub allow_http: bool,
    /// Accept a Bunko server as the backend (tests only, D15).
    pub allow_bunko_backend_for_tests: bool,
    /// Where the change-feed snapshot lives.
    pub state_dir: PathBuf,
    /// Never use `If-Match` / `If-None-Match`, whatever the probe found (tests, and
    /// servers that misreport them).
    pub disable_cas: bool,
}

impl std::fmt::Debug for WebDavConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebDavConfig")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("folder", &self.folder)
            .field("allow_http", &self.allow_http)
            .field(
                "allow_bunko_backend_for_tests",
                &self.allow_bunko_backend_for_tests,
            )
            .field("state_dir", &self.state_dir)
            .field("disable_cas", &self.disable_cas)
            .finish()
    }
}

/// One generation of the change-feed snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Generation {
    gen_no: u64,
    roots: Vec<Pid>,
    /// pid → (version, is_dir), the roots included.
    entries: BTreeMap<Pid, (String, bool)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapFile {
    /// Changes whenever a new feed starts, so cursors of an older one never match.
    epoch: String,
    base: Generation,
    /// The generation the last `changes` call returned, until the caller uses it.
    next: Option<Generation>,
}

/// A WebDAV server as a [`Backend`].
pub struct WebDavBackend {
    client: reqwest::Client,
    /// `scheme://authority`.
    origin: String,
    /// Decoded path segments of the folder URL.
    base: Vec<String>,
    username: String,
    password: String,
    state_dir: PathBuf,
    disable_cas: bool,
    allow_bunko: bool,
    probe: RwLock<Probe>,
    caps: RwLock<BackendCaps>,
    /// The roots last located, created or listed (what the snapshot covers).
    roots: Mutex<Vec<Pid>>,
    snap: tokio::sync::Mutex<Option<SnapFile>>,
}

static UNIQUE: AtomicU64 = AtomicU64::new(0);

fn unique_token() -> String {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{t:x}{:x}{:x}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    )
}

/// An entity-tag for a conditional header: some servers list `getetag` unquoted.
fn quote_tag(t: &str) -> String {
    if t.starts_with('"') || t.starts_with("W/") || t == "*" {
        t.to_string()
    } else {
        format!("\"{t}\"")
    }
}

fn method(m: &str) -> Method {
    Method::from_bytes(m.as_bytes()).expect("a valid method token")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn net_err(e: reqwest::Error) -> BErr {
    BErr::Transient(format!("network: {e}"))
}

fn retry_after(resp: &Response) -> Option<Duration> {
    let v = resp.headers().get("retry-after")?.to_str().ok()?.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(v).ok()?;
    Some(at.duration_since(SystemTime::now()).unwrap_or_default())
}

/// The error a non-2xx answer means (spec §5.11 and the research's G).
fn status_err(resp: &Response) -> BErr {
    let s = resp.status();
    match s.as_u16() {
        401 => BErr::AuthExpired,
        403 => BErr::AuthRevoked,
        404 | 410 => BErr::NotFound,
        // A missing parent (MKCOL, PUT, MOVE destination).
        409 => BErr::NotFound,
        412 => BErr::PreconditionFailed,
        413 => BErr::TooLarge,
        423 => BErr::Transient("locked (423)".into()),
        429 => BErr::Throttled(retry_after(resp)),
        503 => match retry_after(resp) {
            Some(d) => BErr::Throttled(Some(d)),
            None => BErr::Transient("service unavailable (503)".into()),
        },
        507 => BErr::StorageFull,
        408 => BErr::Transient("request timeout (408)".into()),
        c if s.is_server_error() => BErr::Transient(format!("server error ({c})")),
        c if s.is_redirection() => BErr::Fatal(format!(
            "the server redirected ({c}) to {}; use the final URL",
            resp.headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("?")
        )),
        c => BErr::Fatal(format!("unexpected answer {c}")),
    }
}

fn ok_or_err(resp: Response) -> Result<Response, BErr> {
    if resp.status().is_success() {
        Ok(resp)
    } else {
        Err(status_err(&resp))
    }
}

fn server_time(resp: &Response) -> ServerTime {
    let ms = resp
        .headers()
        .get("date")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| httpdate::parse_http_date(v).ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_else(now_ms);
    ServerTime(ms)
}

fn http_date_ms(s: &str) -> Option<i64> {
    let t = httpdate::parse_http_date(s.trim()).ok()?;
    Some(t.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64)
}

fn rfc3339_ms(s: &str) -> Option<i64> {
    let t = time::OffsetDateTime::parse(s.trim(), &time::format_description::well_known::Rfc3339)
        .ok()?;
    Some((t.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// `a/b/c` -> (`a/b`, `c`); `c` -> (``, `c`).
fn split_pid(pid: &str) -> (&str, &str) {
    match pid.rsplit_once('/') {
        Some((p, n)) => (p, n),
        None => ("", pid),
    }
}

fn join_pid(parent: &str, name: &str) -> Pid {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn check_name(name: &str) -> Result<(), BErr> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(BErr::InvalidName);
    }
    Ok(())
}

/// Is `pid` inside `root` (or the root itself)?
fn under(pid: &str, root: &str) -> bool {
    root.is_empty()
        || pid == root
        || (pid.len() > root.len() && pid.starts_with(root) && pid.as_bytes()[root.len()] == b'/')
}

/// Bunko's own transient names: probe leftovers and staged uploads. Never listed.
fn is_internal(pid: &str) -> bool {
    pid.split('/').any(|s| {
        s.starts_with(".bunko-probe-")
            || (s.starts_with('.') && s.ends_with(".tmp") && s.contains(".bunko-"))
    })
}

/// A file name safe for the staging name.
fn sanitize_op(op: &str) -> String {
    let s: String = op
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if s.is_empty() { unique_token() } else { s }
}

/// Skip `skip` bytes, then pass at most `take` (a server that answered a Range request
/// with the whole body).
fn body_stream(resp: Response, skip: u64, take: Option<u64>) -> RemoteBody {
    let s = resp
        .bytes_stream()
        .map(|r| r.map_err(std::io::Error::other));
    if skip == 0 && take.is_none() {
        return Box::pin(s);
    }
    Box::pin(
        s.scan((skip, take), |(skip, take), item| {
            let out = match item {
                Err(e) => Some(Some(Err(e))),
                Ok(_) if *take == Some(0) => None,
                Ok(mut b) => {
                    if *skip > 0 {
                        let n = (*skip).min(b.len() as u64);
                        b = b.slice(n as usize..);
                        *skip -= n;
                    }
                    if let Some(t) = take.as_mut() {
                        if b.len() as u64 > *t {
                            b.truncate(*t as usize);
                        }
                        *t -= b.len() as u64;
                    }
                    Some((!b.is_empty()).then_some(Ok(b)))
                }
            };
            futures_util::future::ready(out)
        })
        .filter_map(futures_util::future::ready),
    )
}

fn caps_from(p: &Probe, disable_cas: bool) -> BackendCaps {
    let cas = !disable_cas && p.strong_etags;
    let cas_overwrite = cas && p.if_match;
    BackendCaps {
        cas_create: !disable_cas && p.if_none_match,
        cas_overwrite,
        // A plain PUT can be listed mid-write; Bunko's is staged (X-Mokuro-Put), and
        // without CAS a replace goes through a temporary name and a MOVE.
        atomic_replace: p.bunko || !cas_overwrite,
        server_copy: true,
        range: p.range,
        pinned_read: cas_overwrite,
        push: false,
        hash_kind: p.hash_kind,
        mtime_granularity_ns: 1_000_000_000,
        simple_upload_max: u64::MAX,
        chunk_multiple: 1,
        path_ids: true,
    }
}

impl WebDavBackend {
    /// Check the URL, run the capability probe and refuse what the config does not
    /// allow (`http://`, a Bunko server).
    pub async fn connect(cfg: WebDavConfig) -> Result<Self, BErr> {
        let url = cfg.url.trim();
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| BErr::Fatal(format!("not a URL: {url}")))?;
        match scheme.to_ascii_lowercase().as_str() {
            "https" => {}
            "http" if cfg.allow_http => {}
            "http" => {
                return Err(BErr::Fatal(
                    "refusing http:// (the password would travel in the clear); use https:// or allow plain http".into(),
                ));
            }
            other => return Err(BErr::Fatal(format!("unsupported URL scheme {other}"))),
        }
        if rest.contains(['?', '#']) {
            return Err(BErr::Fatal("the WebDAV URL may not have a query".into()));
        }
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(BErr::Fatal(format!("no host in {url}")));
        }
        let mut base = Vec::new();
        for seg in path.split('/').chain(cfg.folder.split('/')) {
            if seg.is_empty() {
                continue;
            }
            let seg = percent_decode_str(seg)
                .decode_utf8()
                .map_err(|_| BErr::Fatal("the URL path is not UTF-8".into()))?
                .into_owned();
            if seg == "." || seg == ".." {
                return Err(BErr::Fatal("the URL path may not contain . or ..".into()));
            }
            base.push(seg);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(300))
            .user_agent(concat!("mokuro-bunko/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| BErr::Fatal(format!("http client: {e}")))?;
        let backend = WebDavBackend {
            client,
            origin: format!("{}://{authority}", scheme.to_ascii_lowercase()),
            base,
            username: cfg.username,
            password: cfg.password,
            state_dir: cfg.state_dir,
            disable_cas: cfg.disable_cas,
            allow_bunko: cfg.allow_bunko_backend_for_tests,
            probe: RwLock::new(Probe::default()),
            caps: RwLock::new(caps_from(&Probe::default(), true)),
            roots: Mutex::new(Vec::new()),
            snap: tokio::sync::Mutex::new(None),
        };
        let p = probe::run(&backend).await?;
        backend.set_probe(p);
        Ok(backend)
    }

    /// What the capability probe found.
    pub fn probe(&self) -> Probe {
        self.probe.read().clone()
    }

    fn set_probe(&self, p: Probe) {
        tracing::info!(?p, "webdav capability probe");
        *self.caps.write() = caps_from(&p, self.disable_cas);
        *self.probe.write() = p;
    }

    /// The encoded absolute path of `pid` (a trailing slash for a collection).
    fn path_of(&self, pid: &str, dir: bool) -> String {
        let mut out = String::new();
        for seg in self
            .base
            .iter()
            .map(String::as_str)
            .chain(pid.split('/').filter(|s| !s.is_empty()))
        {
            out.push('/');
            out.extend(utf8_percent_encode(seg, SEGMENT));
        }
        if dir || out.is_empty() {
            out.push('/');
        }
        out
    }

    fn url(&self, pid: &str, dir: bool) -> String {
        format!("{}{}", self.origin, self.path_of(pid, dir))
    }

    /// The pid an href names, or `None` when it is outside the folder (or not a path).
    pub(crate) fn pid_from_href(&self, href: &str) -> Option<Pid> {
        let path = if let Some((_, rest)) = href.split_once("://") {
            &rest[rest.find('/')?..]
        } else if href.starts_with('/') {
            href
        } else {
            return None;
        };
        let path = path.split(['?', '#']).next().unwrap_or("");
        let mut segs = Vec::new();
        for raw in path.split('/').filter(|s| !s.is_empty()) {
            let seg = percent_decode_str(raw).decode_utf8().ok()?;
            if seg == "." || seg == ".." || seg.contains('\0') {
                return None;
            }
            segs.push(seg.into_owned());
        }
        if segs.len() < self.base.len() {
            return None;
        }
        // Case-insensitive hosts may answer in another spelling of the folder path.
        let same = self
            .base
            .iter()
            .zip(&segs)
            .all(|(a, b)| a == b || a.to_lowercase() == b.to_lowercase());
        same.then(|| segs[self.base.len()..].join("/"))
    }

    fn node_from(&self, e: &DavEntry) -> Option<RNode> {
        if e.failed {
            return None;
        }
        let pid = self.pid_from_href(&e.href)?;
        let (parent, name) = if pid.is_empty() {
            (None, self.base.last().cloned().unwrap_or_default())
        } else {
            let (p, n) = split_pid(&pid);
            (Some(p.to_string()), n.to_string())
        };
        let base_mtime_ms = e
            .last_modified
            .as_deref()
            .and_then(http_date_ms)
            .unwrap_or(0);
        let size = if e.is_dir { 0 } else { e.size.unwrap_or(0) };
        let version = match &e.etag {
            Some(t) => t.clone(),
            // A folder's last-modified is no change token (virtual folders report *now*).
            None if e.is_dir => DIR_NO_ETAG.to_string(),
            None => format!("{base_mtime_ms}-{size}"),
        };
        let strong = e.etag.as_deref().is_some_and(|t| !t.starts_with("W/"));
        let pinned = self.caps.read().pinned_read;
        Some(RNode {
            pid,
            parent,
            name_raw: name.into_bytes(),
            is_dir: e.is_dir,
            size,
            base_mtime_ms,
            created_ms: e.creation_date.as_deref().and_then(rfc3339_ms).unwrap_or(0),
            cver: version.clone(),
            pin: (!e.is_dir && strong && pinned).then(|| version.clone()),
            version,
            hash: if e.is_dir {
                None
            } else {
                e.checksums.as_deref().and_then(xml::best_checksum)
            },
            trashed: false,
        })
    }

    async fn send(
        &self,
        m: &str,
        url: &str,
        headers: &[(&str, String)],
        body: Option<reqwest::Body>,
    ) -> Result<Response, BErr> {
        let mut req = self
            .client
            .request(method(m), url)
            .basic_auth(&self.username, Some(&self.password));
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        if let Some(b) = body {
            req = req.body(b);
        }
        tracing::trace!(m, url, "webdav request");
        req.send().await.map_err(net_err)
    }

    /// PROPFIND `pid`, streaming each response to `on`. Errors on a non-2xx answer.
    async fn propfind(
        &self,
        pid: &str,
        dir: bool,
        depth: &str,
        body: &'static str,
        on: &mut (dyn FnMut(DavEntry) + Send),
    ) -> Result<ServerTime, BErr> {
        let resp = self
            .send(
                "PROPFIND",
                &self.url(pid, dir),
                &[
                    ("depth", depth.to_string()),
                    ("content-type", "application/xml; charset=utf-8".into()),
                ],
                Some(body.into()),
            )
            .await?;
        let resp = ok_or_err(resp)?;
        let t = server_time(&resp);
        let stream = resp
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other));
        let reader = tokio_util::io::StreamReader::new(Box::pin(stream));
        xml::parse_multistatus(reader, on)
            .await
            .map_err(|e| BErr::Transient(format!("bad multistatus: {}", e.0)))?;
        Ok(t)
    }

    /// PROPFIND Depth 0: the node at `pid`, `None` when absent (or outside the folder).
    async fn stat_node(&self, pid: &str, dir: bool) -> Result<(Option<RNode>, ServerTime), BErr> {
        let mut first: Option<DavEntry> = None;
        let r = self
            .propfind(pid, dir, "0", xml::PROPFIND_NODE, &mut |e| {
                if first.is_none() {
                    first = Some(e);
                }
            })
            .await;
        match r {
            Ok(t) => Ok((first.and_then(|e| self.node_from(&e)), t)),
            Err(BErr::NotFound) => Ok((None, ServerTime(now_ms()))),
            Err(e) => Err(e),
        }
    }

    /// Stream `root`'s subtree (the root itself when `include_root`). With `prune`, a
    /// folder whose version equals `prune`'s is not listed again: its subtree is copied
    /// from there into `copied` instead.
    async fn walk(
        &self,
        root: &str,
        include_root: bool,
        prune: Option<&Generation>,
        sink: &mut (dyn FnMut(RNode) + Send),
        copied: &mut BTreeMap<Pid, (String, bool)>,
    ) -> Result<(), BErr> {
        let infinity = self.probe.read().depth_infinity;
        if infinity && prune.is_none() {
            let mut saw_root = false;
            self.propfind(root, true, "infinity", xml::PROPFIND_NODE, &mut |e| {
                let Some(n) = self.node_from(&e) else { return };
                if !under(&n.pid, root) || is_internal(&n.pid) {
                    return;
                }
                if n.pid == root {
                    saw_root = true;
                    if include_root {
                        sink(n);
                    }
                    return;
                }
                sink(n);
            })
            .await?;
            if !saw_root {
                return Err(BErr::NotFound);
            }
            return Ok(());
        }
        // Only a real ETag vouches for a subtree.
        let unchanged = |n: &RNode| {
            prune.is_some_and(|g| {
                n.is_dir
                    && n.version != DIR_NO_ETAG
                    && g.entries.get(&n.pid) == Some(&(n.version.clone(), true))
            })
        };
        if let Some(g) = prune {
            // The root first: when its ETag is unchanged, so is everything below it.
            let (node, _) = self.stat_node(root, true).await?;
            let node = node.filter(|n| n.is_dir).ok_or(BErr::NotFound)?;
            if unchanged(&node) {
                copy_subtree(g, root, copied);
                return Ok(());
            }
        }
        let mut stack: Vec<Pid> = vec![root.to_string()];
        while let Some(dir) = stack.pop() {
            let mut subdirs = Vec::new();
            let mut self_dir = None;
            let r = self
                .propfind(&dir, true, "1", xml::PROPFIND_NODE, &mut |e| {
                    let Some(n) = self.node_from(&e) else { return };
                    if n.pid == dir {
                        self_dir = Some(n.is_dir);
                        if dir == root && include_root {
                            sink(n);
                        }
                        return;
                    }
                    if n.parent.as_deref() != Some(dir.as_str()) || is_internal(&n.pid) {
                        return;
                    }
                    if n.is_dir {
                        if unchanged(&n) {
                            if let Some(g) = prune {
                                copy_subtree(g, &n.pid, copied);
                            }
                        } else {
                            subdirs.push(n.pid.clone());
                        }
                    }
                    sink(n);
                })
                .await;
            match r {
                Ok(_) if self_dir == Some(false) && dir == root => return Err(BErr::NotFound),
                Ok(_) => {}
                // A folder removed while we walk: the next cycle sees it gone.
                Err(BErr::NotFound) if dir != root => continue,
                Err(e) => return Err(e),
            }
            subdirs.reverse();
            stack.extend(subdirs);
        }
        Ok(())
    }

    /// The pid of the root named `name`: the folder itself when it is named so, else a
    /// child of it.
    fn root_pid(&self, name: &str) -> Pid {
        if self.base.last().is_some_and(|l| l == name) {
            String::new()
        } else {
            name.to_string()
        }
    }

    fn track_roots(&self, roots: &[Pid]) {
        *self.roots.lock() = roots.to_vec();
    }

    fn tracked_roots(&self) -> Vec<Pid> {
        let r = self.roots.lock().clone();
        if r.is_empty() { vec![String::new()] } else { r }
    }

    async fn put_file(
        &self,
        pid: &str,
        src: UploadSource<'_>,
        cond: &[(&str, String)],
    ) -> Result<(), BErr> {
        let UploadSource::File { path, size, sha256 } = src;
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|e| BErr::Fatal(format!("cannot read {}: {e}", path.display())))?;
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
        let mut headers: Vec<(&str, String)> = vec![
            ("content-length", size.to_string()),
            ("content-type", "application/octet-stream".into()),
            (
                "content-digest",
                format!(
                    "sha-256=:{}:",
                    base64::engine::general_purpose::STANDARD.encode(sha256)
                ),
            ),
        ];
        headers.extend(cond.iter().cloned());
        let resp = self
            .send("PUT", &self.url(pid, false), &headers, Some(body))
            .await?;
        if resp.status() == StatusCode::METHOD_NOT_ALLOWED {
            // RFC 4918 §9.7.2: a PUT onto a collection.
            return Err(BErr::NameConflict);
        }
        ok_or_err(resp).map(drop)
    }

    async fn delete_quietly(&self, pid: &str) {
        if let Err(e) = self.send("DELETE", &self.url(pid, false), &[], None).await {
            tracing::debug!(pid, %e, "webdav: could not remove a staging file");
        }
    }

    async fn move_or_copy(
        &self,
        m: &str,
        from: &str,
        to: &str,
        overwrite: bool,
        if_match: Option<&str>,
    ) -> Result<(), BErr> {
        let mut headers = vec![
            ("destination", self.url(to, false)),
            ("overwrite", if overwrite { "T" } else { "F" }.to_string()),
        ];
        if let Some(t) = if_match {
            headers.push(("if-match", quote_tag(t)));
        }
        let resp = self.send(m, &self.url(from, false), &headers, None).await?;
        ok_or_err(resp).map(drop)
    }

    /// The bytes to `.<name>.bunko-<op>.tmp`, then MOVE into place.
    async fn put_via_temp(
        &self,
        parent: &str,
        name: &str,
        src: UploadSource<'_>,
        op: &OpCtx,
        overwrite: bool,
    ) -> Result<(), BErr> {
        let tmp = join_pid(
            parent,
            &format!(".{name}.bunko-{}.tmp", sanitize_op(&op.op_id)),
        );
        let dest = join_pid(parent, name);
        if let Err(e) = self.put_file(&tmp, src, &[]).await {
            self.delete_quietly(&tmp).await;
            return Err(e);
        }
        if let Err(e) = self
            .move_or_copy("MOVE", &tmp, &dest, overwrite, None)
            .await
        {
            self.delete_quietly(&tmp).await;
            return Err(e);
        }
        Ok(())
    }

    async fn written(&self, pid: &str) -> Result<RNode, BErr> {
        self.stat_node(pid, false)
            .await?
            .0
            .ok_or_else(|| BErr::Transient(format!("{pid} is not visible after the write")))
    }

    async fn load_snap(&self, slot: &mut Option<SnapFile>) {
        if slot.is_some() {
            return;
        }
        let path = self.state_dir.join(SNAPSHOT_FILE);
        let loaded = tokio::task::spawn_blocking(move || {
            let data = std::fs::read(&path).ok()?;
            serde_json::from_slice::<SnapFile>(&data).ok()
        })
        .await
        .ok()
        .flatten();
        *slot = loaded;
    }

    async fn save_snap(&self, snap: &SnapFile) -> Result<(), BErr> {
        let dir = self.state_dir.clone();
        let data = serde_json::to_vec(snap).map_err(|e| BErr::Fatal(e.to_string()))?;
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            let tmp = dir.join(format!("{SNAPSHOT_FILE}.tmp"));
            std::fs::write(&tmp, &data)?;
            std::fs::rename(&tmp, dir.join(SNAPSHOT_FILE))
        })
        .await
        .map_err(|e| BErr::Fatal(e.to_string()))?
        .map_err(|e| BErr::Fatal(format!("cannot save the WebDAV snapshot: {e}")))
    }

    /// List `roots` (themselves included) into a generation, calling `on_new` for each
    /// node that differs from `prev`.
    async fn snapshot(
        &self,
        roots: &[Pid],
        prev: Option<&Generation>,
        on_new: &mut (dyn FnMut(RNode) + Send),
    ) -> Result<BTreeMap<Pid, (String, bool)>, BErr> {
        let probe = self.probe();
        let prune = prev.filter(|_| !probe.depth_infinity && probe.nested_propagation);
        let mut entries = BTreeMap::new();
        for root in roots {
            let mut copied = BTreeMap::new();
            let r = self
                .walk(
                    root,
                    true,
                    prune,
                    &mut |n| {
                        let v = (n.version.clone(), n.is_dir);
                        if prev.is_none_or(|g| g.entries.get(&n.pid) != Some(&v)) {
                            on_new(n.clone());
                        }
                        entries.insert(n.pid, v);
                    },
                    &mut copied,
                )
                .await;
            match r {
                Ok(()) => entries.extend(copied),
                // A root that is gone: its entries come out as removals.
                Err(BErr::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(entries)
    }
}

fn copy_subtree(g: &Generation, root: &str, into: &mut BTreeMap<Pid, (String, bool)>) {
    if root.is_empty() {
        into.extend(g.entries.iter().map(|(k, v)| (k.clone(), v.clone())));
        return;
    }
    if let Some(v) = g.entries.get(root) {
        into.insert(root.to_string(), v.clone());
    }
    let prefix = format!("{root}/");
    for (k, v) in g.entries.range(prefix.clone()..) {
        if !k.starts_with(&prefix) {
            break;
        }
        into.insert(k.clone(), v.clone());
    }
}

fn cursor_of(epoch: &str, gen_no: u64) -> Cursor {
    Cursor(format!("{epoch}:{gen_no}"))
}

impl Backend for WebDavBackend {
    fn caps(&self) -> BackendCaps {
        self.caps.read().clone()
    }

    async fn account(&self) -> Result<AccountInfo, BErr> {
        // Checks the credentials; a missing folder is fine here.
        self.stat_node("", true).await?;
        let host = self.origin.split_once("://").map_or("", |(_, h)| h);
        Ok(AccountInfo {
            id: format!(
                "{}@{}{}",
                self.username,
                self.origin,
                self.path_of("", true)
            ),
            label: format!("{} on {host}", self.username),
        })
    }

    async fn locate_roots(&self, name: &str) -> Result<Vec<RNode>, BErr> {
        check_name(name)?;
        let pid = self.root_pid(name);
        let (node, _) = self.stat_node(&pid, true).await?;
        let found: Vec<RNode> = node
            .filter(|n| n.is_dir && n.pid == pid)
            .into_iter()
            .collect();
        if !found.is_empty() {
            self.track_roots(&[pid]);
        }
        Ok(found)
    }

    async fn create_root(&self, name: &str) -> Result<RNode, BErr> {
        check_name(name)?;
        let pid = self.root_pid(name);
        let resp = self.send("MKCOL", &self.url(&pid, true), &[], None).await?;
        match resp.status().as_u16() {
            405 => return Err(BErr::NameConflict),
            _ => {
                ok_or_err(resp)?;
            }
        }
        if !self.probe.read().folder_exists {
            // The probe had nowhere to write: run it again in the new folder.
            let p = probe::run(self).await?;
            self.set_probe(p);
        }
        let node = self
            .stat_node(&pid, true)
            .await?
            .0
            .ok_or_else(|| BErr::Transient("the new root is not visible".into()))?;
        self.track_roots(std::slice::from_ref(&pid));
        Ok(node)
    }

    async fn start_cursor(&self) -> Result<Cursor, BErr> {
        let mut slot = self.snap.lock().await;
        let roots = self.tracked_roots();
        let entries = self.snapshot(&roots, None, &mut |_| {}).await?;
        let snap = SnapFile {
            epoch: unique_token(),
            base: Generation {
                gen_no: 1,
                roots,
                entries,
            },
            next: None,
        };
        self.save_snap(&snap).await?;
        let c = cursor_of(&snap.epoch, snap.base.gen_no);
        *slot = Some(snap);
        Ok(c)
    }

    async fn list_all(
        &self,
        roots: &[Pid],
        sink: &mut (dyn FnMut(RNode) + Send),
    ) -> Result<(), BErr> {
        self.track_roots(roots);
        for root in roots {
            let mut unused = BTreeMap::new();
            self.walk(root, false, None, sink, &mut unused).await?;
        }
        Ok(())
    }

    async fn list_dir(&self, dir: &str, sink: &mut (dyn FnMut(RNode) + Send)) -> Result<(), BErr> {
        let mut self_dir = None;
        self.propfind(dir, true, "1", xml::PROPFIND_NODE, &mut |e| {
            let Some(n) = self.node_from(&e) else { return };
            if n.pid == dir {
                self_dir = Some(n.is_dir);
                return;
            }
            if n.parent.as_deref() == Some(dir) && !is_internal(&n.pid) {
                sink(n);
            }
        })
        .await?;
        if self_dir == Some(false) {
            return Err(BErr::NotFound);
        }
        Ok(())
    }

    async fn find_named(&self, parent: &str, name: &str) -> Result<(Vec<RNode>, ServerTime), BErr> {
        check_name(name)?;
        let pid = join_pid(parent, name);
        let (node, t) = self.stat_node(&pid, false).await?;
        match node {
            Some(n) => Ok((
                (n.name_raw == name.as_bytes() && n.parent.as_deref() == Some(parent))
                    .then_some(n)
                    .into_iter()
                    .collect(),
                t,
            )),
            None => {
                let (p, t) = self.stat_node(parent, true).await?;
                match p {
                    Some(p) if p.is_dir => Ok((Vec::new(), t)),
                    _ => Err(BErr::NotFound),
                }
            }
        }
    }

    async fn changes(&self, c: &Cursor) -> Result<ChangePage, BErr> {
        let mut slot = self.snap.lock().await;
        self.load_snap(&mut slot).await;
        let Some(snap) = slot.as_mut() else {
            return Ok(ChangePage::CursorLost);
        };
        let Some((epoch, gen_no)) = c.0.rsplit_once(':') else {
            return Ok(ChangePage::CursorLost);
        };
        let Ok(gen_no) = gen_no.parse::<u64>() else {
            return Ok(ChangePage::CursorLost);
        };
        if epoch != snap.epoch {
            return Ok(ChangePage::CursorLost);
        }
        if snap.next.as_ref().is_some_and(|n| n.gen_no == gen_no) {
            snap.base = snap.next.take().expect("checked");
        } else if snap.base.gen_no != gen_no {
            return Ok(ChangePage::CursorLost);
        }
        let base = snap.base.clone();
        let mut items = Vec::new();
        let entries = self
            .snapshot(&base.roots, Some(&base), &mut |n| {
                items.push(ChangeItem::Upsert(n))
            })
            .await?;
        let mut removed: Vec<&Pid> = base
            .entries
            .keys()
            .filter(|p| !entries.contains_key(*p))
            .collect();
        // Children before their folders.
        removed.sort_by_key(|p| {
            std::cmp::Reverse(p.matches('/').count() + usize::from(!p.is_empty()))
        });
        items.extend(
            removed
                .into_iter()
                .map(|p| ChangeItem::Removed { pid: p.clone() }),
        );
        let next = Generation {
            gen_no: base.gen_no + 1,
            roots: base.roots.clone(),
            entries,
        };
        let cursor = cursor_of(&snap.epoch, next.gen_no);
        snap.next = Some(next);
        let to_save = snap.clone();
        self.save_snap(&to_save).await?;
        Ok(ChangePage::Page {
            items,
            next: cursor,
            more: false,
        })
    }

    async fn wait_change(&self, _c: &Cursor, _max: Duration) -> Result<(), BErr> {
        Err(BErr::Unsupported)
    }

    async fn stat(&self, pid: &str) -> Result<Option<RNode>, BErr> {
        Ok(self.stat_node(pid, false).await?.0)
    }

    async fn read(
        &self,
        pid: &str,
        pin: Option<&str>,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<RemoteBody, BErr> {
        let mut headers = Vec::new();
        if let Some((first, last)) = range {
            let v = match last {
                Some(l) => format!("bytes={first}-{l}"),
                None => format!("bytes={first}-"),
            };
            headers.push(("range", v));
        }
        if let Some(p) = pin {
            headers.push(("if-match", quote_tag(p)));
        }
        let resp = self
            .send("GET", &self.url(pid, false), &headers, None)
            .await?;
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => Ok(body_stream(resp, 0, None)),
            StatusCode::OK => match range {
                // The server ignored the Range: skip to the offset ourselves.
                Some((first, last)) => Ok(body_stream(
                    resp,
                    first,
                    last.map(|l| l.saturating_sub(first) + 1),
                )),
                None => Ok(body_stream(resp, 0, None)),
            },
            StatusCode::RANGE_NOT_SATISFIABLE => Err(BErr::Fatal(format!(
                "range {range:?} not satisfiable for {pid}"
            ))),
            _ => Err(status_err(&resp)),
        }
    }

    async fn upload(
        &self,
        to: UploadTarget<'_>,
        src: UploadSource<'_>,
        op: &mut OpCtx,
    ) -> Result<RNode, BErr> {
        let caps = self.caps();
        let pid = match to {
            UploadTarget::New { parent, name } => {
                check_name(name)?;
                let pid = join_pid(parent, name);
                let r = if caps.cas_create {
                    self.put_file(&pid, src, &[("if-none-match", "*".into())])
                        .await
                } else {
                    self.put_via_temp(parent, name, src, op, false).await
                };
                match r {
                    Ok(()) => pid,
                    // A folder in the way is a name conflict, a file a lost race.
                    Err(BErr::PreconditionFailed) => {
                        return Err(match self.stat_node(&pid, false).await?.0 {
                            Some(n) if n.is_dir => BErr::NameConflict,
                            _ => BErr::PreconditionFailed,
                        });
                    }
                    Err(e) => return Err(e),
                }
            }
            UploadTarget::Replace { pid, if_cver } => {
                match if_cver.filter(|_| caps.cas_overwrite) {
                    Some(tag) => {
                        self.put_file(pid, src, &[("if-match", quote_tag(tag))])
                            .await?
                    }
                    None => {
                        let (parent, name) = split_pid(pid);
                        self.put_via_temp(parent, name, src, op, true).await?
                    }
                }
                pid.to_string()
            }
        };
        self.written(&pid).await
    }

    async fn mkdir(&self, parent: &str, name: &str, _op: &OpCtx) -> Result<RNode, BErr> {
        check_name(name)?;
        let pid = join_pid(parent, name);
        let resp = self.send("MKCOL", &self.url(&pid, true), &[], None).await?;
        if resp.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Err(BErr::NameConflict);
        }
        ok_or_err(resp)?;
        self.stat_node(&pid, true)
            .await?
            .0
            .ok_or_else(|| BErr::Transient(format!("{pid} is not visible after MKCOL")))
    }

    async fn update(
        &self,
        pid: &str,
        parent: Option<&str>,
        name: Option<&str>,
        if_cver: Option<&str>,
    ) -> Result<RNode, BErr> {
        if pid.is_empty() {
            return Err(BErr::Unsupported);
        }
        let (cur_parent, cur_name) = split_pid(pid);
        let new_name = name.unwrap_or(cur_name);
        check_name(new_name)?;
        let dest = join_pid(parent.unwrap_or(cur_parent), new_name);
        if dest == pid {
            return self.stat_node(pid, false).await?.0.ok_or(BErr::NotFound);
        }
        let tag = if_cver.filter(|_| self.caps.read().cas_overwrite);
        match self.move_or_copy("MOVE", pid, &dest, false, tag).await {
            Ok(()) => {}
            Err(BErr::PreconditionFailed) => {
                // Overwrite: F (the destination exists) or If-Match (the source changed).
                let taken = tag.is_none()
                    || (dest.to_lowercase() != pid.to_lowercase()
                        && self.stat_node(&dest, false).await?.0.is_some());
                return Err(if taken {
                    BErr::NameConflict
                } else {
                    BErr::PreconditionFailed
                });
            }
            Err(e) => return Err(e),
        }
        self.written(&dest).await
    }

    async fn copy(&self, pid: &str, parent: &str, name: &str) -> Result<RNode, BErr> {
        check_name(name)?;
        let dest = join_pid(parent, name);
        match self.move_or_copy("COPY", pid, &dest, false, None).await {
            Ok(()) => {}
            Err(BErr::PreconditionFailed) => return Err(BErr::NameConflict),
            Err(e) => return Err(e),
        }
        self.written(&dest).await
    }

    async fn trash(&self, pid: &str, if_cver: Option<&str>) -> Result<(), BErr> {
        if pid.is_empty() {
            return Err(BErr::Unsupported);
        }
        let mut headers = Vec::new();
        if let Some(t) = if_cver.filter(|_| self.caps.read().cas_overwrite) {
            headers.push(("if-match", quote_tag(t)));
        }
        let resp = self
            .send("DELETE", &self.url(pid, false), &headers, None)
            .await?;
        ok_or_err(resp).map(drop)
    }

    async fn delete_permanent(&self, pid: &str) -> Result<(), BErr> {
        if pid.is_empty() {
            return Err(BErr::Unsupported);
        }
        let resp = self
            .send("DELETE", &self.url(pid, false), &[], None)
            .await?;
        ok_or_err(resp).map(drop)
    }

    async fn quota(&self) -> Result<Option<bunko_store::Capacity>, BErr> {
        let root = self.tracked_roots().remove(0);
        let mut first: Option<DavEntry> = None;
        self.propfind(&root, true, "0", xml::PROPFIND_QUOTA, &mut |e| {
            if first.is_none() {
                first = Some(e);
            }
        })
        .await?;
        let Some(e) = first else { return Ok(None) };
        // Nextcloud reports -1/-2/-3 for unknown, unlimited or not computed.
        Ok(match (e.quota_available, e.quota_used) {
            (Some(avail), Some(used)) if avail >= 0 && used >= 0 => Some(bunko_store::Capacity {
                total: (avail as u64).saturating_add(used as u64),
                available: avail as u64,
            }),
            _ => None,
        })
    }
}

/// For tests: the path the snapshot is kept at.
pub fn snapshot_path(state_dir: &Path) -> PathBuf {
    state_dir.join(SNAPSHOT_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(url: &str, folder: &str) -> WebDavBackend {
        let mut base = Vec::new();
        let (_, rest) = url.split_once("://").unwrap();
        let path = &rest[rest.find('/').unwrap_or(rest.len())..];
        for s in path.split('/').chain(folder.split('/')) {
            if !s.is_empty() {
                base.push(percent_decode_str(s).decode_utf8().unwrap().into_owned());
            }
        }
        WebDavBackend {
            client: reqwest::Client::new(),
            origin: "https://h".into(),
            base,
            username: "u".into(),
            password: "p".into(),
            state_dir: PathBuf::new(),
            disable_cas: false,
            allow_bunko: false,
            probe: RwLock::new(Probe::default()),
            caps: RwLock::new(caps_from(&Probe::default(), true)),
            roots: Mutex::new(Vec::new()),
            snap: tokio::sync::Mutex::new(None),
        }
    }

    #[test]
    fn hrefs_map_to_pids_inside_the_folder_only() {
        let b = backend("https://h/remote.php/dav/files/u%20x/", "mokuro-reader");
        assert_eq!(
            b.pid_from_href("/remote.php/dav/files/u%20x/mokuro-reader/"),
            Some(String::new())
        );
        assert_eq!(
            b.pid_from_href("https://h/remote.php/dav/files/u%20x/mokuro-reader/S/v%201.cbz"),
            Some("S/v 1.cbz".into())
        );
        assert_eq!(
            b.pid_from_href("/remote.php/dav/files/U%20X/Mokuro-Reader/a"),
            Some("a".into())
        );
        assert_eq!(b.pid_from_href("/remote.php/dav/files/u%20x/other/a"), None);
        assert_eq!(b.pid_from_href("/remote.php/dav/files/u%20x/"), None);
        assert_eq!(
            b.pid_from_href("/remote.php/dav/files/u%20x/mokuro-reader/../x"),
            None
        );
        assert_eq!(b.pid_from_href("relative/x"), None);
        assert_eq!(
            b.path_of("S/v 1#.cbz", false),
            "/remote.php/dav/files/u%20x/mokuro-reader/S/v%201%23.cbz"
        );
        assert_eq!(
            b.path_of("", true),
            "/remote.php/dav/files/u%20x/mokuro-reader/"
        );
        assert_eq!(b.root_pid("mokuro-reader"), "");
        assert_eq!(
            backend("https://h/", "").root_pid("mokuro-reader"),
            "mokuro-reader"
        );
        assert_eq!(backend("https://h", "").path_of("", true), "/");
    }

    #[test]
    fn internal_names_and_paths() {
        assert!(is_internal(".bunko-probe-d/x"));
        assert!(is_internal("S/.v.cbz.bunko-op1.tmp"));
        assert!(!is_internal("S/.hidden"));
        assert!(under("a/b", "a"));
        assert!(!under("ab", "a"));
        assert!(under("x", ""));
        assert_eq!(split_pid("a/b/c"), ("a/b", "c"));
        assert_eq!(join_pid("", "c"), "c");
    }
}
