//! The Google Drive v3 driver (spec §5.3, §5.6, §5.7, §5.11, §6), hand-rolled on reqwest.
//!
//! - Ids are opaque and stable across content updates; names may repeat in a folder.
//! - `cver` is `md5Checksum` (folders: `version`), `pin` is `headRevisionId`, the hash
//!   is `sha256Checksum` where Drive has one, else md5.
//! - Drive has no conditional writes: `if_cver` is a `files.get` preflight, leaving a
//!   window of about one round trip (spec §5.7).
//! - Creates are idempotent through a `files.generateIds` id kept in `OpCtx::resume`;
//!   large uploads are resumable and their session URI is kept there too.
//! - Never calls `/revoke` (it would revoke the whole project, Mokuro Reader included),
//!   and never rewrites a file whose bytes are already there.
//! - Every request adds its Drive quota-unit cost ([`api::units`]) to
//!   [`GDriveBackend::units_used`] for the governor.

pub mod api;

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::future::Future;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use parking_lot::Mutex;
use reqwest::header::{self, HeaderName};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::backend::{
    AccountInfo, BErr, Backend, BackendCaps, ChangeItem, ChangePage, Cursor, HashKind, OpCtx, Pid,
    RNode, RemoteBody, ServerTime, UploadSource, UploadTarget,
};
use api::{ApiError, ChangeList, DFile, FILE_FIELDS, FOLDER_MIME, FileList, PAGE_SIZE, units};

/// Drive's public API host.
pub const DEFAULT_API_BASE: &str = "https://www.googleapis.com";
/// Files up to this size go up in one multipart request; larger ones are resumable.
pub const SIMPLE_UPLOAD_MAX: u64 = 5 << 20;
/// Resumable chunks must be multiples of this, except the last.
pub const CHUNK_MULTIPLE: u64 = 256 << 10;
/// The default resumable chunk (spec §5.7).
pub const DEFAULT_CHUNK: u64 = 8 << 20;
/// Files buffered in memory while their parent has not been listed yet; past this they
/// spill to a file in `state_dir`.
pub const DEFAULT_MAX_ORPHANS: usize = 100_000;
/// Consecutive failed chunks one `upload` call retries (after a status query) before
/// handing the error to the outbox.
const CHUNK_RETRIES: u32 = 3;

/// A source of OAuth access tokens. Defined here so the driver does not depend on the
/// auth crate; that crate's token source implements it.
pub trait TokenSourceDyn: Send + Sync {
    /// A currently valid access token (refreshing it if due).
    fn access_token(&self) -> Pin<Box<dyn Future<Output = Result<String, BErr>> + Send + '_>>;
    /// The last token was refused (401): the next `access_token` must refresh.
    fn invalidate(&self);
}

/// A fixed access token (tests, the live test, short scripts).
#[derive(Debug, Clone)]
pub struct StaticToken(String);

impl StaticToken {
    pub fn new(token: impl Into<String>) -> Self {
        StaticToken(token.into())
    }
}

impl TokenSourceDyn for StaticToken {
    fn access_token(&self) -> Pin<Box<dyn Future<Output = Result<String, BErr>> + Send + '_>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
    fn invalidate(&self) {}
}

/// Driver settings.
#[derive(Debug, Clone)]
pub struct GDriveConfig {
    /// `https://www.googleapis.com`, or a mock server in tests.
    pub api_base: String,
    /// The store's state folder; full listings spill orphans here.
    pub state_dir: PathBuf,
    /// Resumable chunk size, rounded down to a multiple of 256 KiB.
    pub chunk_size: u64,
    /// See [`DEFAULT_MAX_ORPHANS`].
    pub max_orphans: usize,
}

impl GDriveConfig {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        GDriveConfig {
            api_base: DEFAULT_API_BASE.to_string(),
            state_dir: state_dir.into(),
            chunk_size: DEFAULT_CHUNK,
            max_orphans: DEFAULT_MAX_ORPHANS,
        }
    }
}

/// The Google Drive backend. Cheap to clone.
#[derive(Clone)]
pub struct GDriveBackend(Arc<Inner>);

struct Inner {
    http: reqwest::Client,
    token: Arc<dyn TokenSourceDyn>,
    base: String,
    state_dir: PathBuf,
    chunk: u64,
    max_orphans: usize,
    units: AtomicU64,
    /// Consecutive throttles, for the exponential suggestion when there is no Retry-After.
    streak: AtomicU32,
    /// `revisions.get` answered 403 once: pinned reads use the re-stat fallback.
    revisions_forbidden: AtomicBool,
    /// Roots seen by `locate_roots`/`create_root`/`list_all`: the parent of choice for
    /// a (legacy) file with several parents.
    roots: Mutex<HashSet<Pid>>,
}

impl GDriveBackend {
    pub fn new(token: Arc<dyn TokenSourceDyn>, cfg: GDriveConfig) -> Result<Self, BErr> {
        // Resumable uploads answer 308 "Resume Incomplete" without a Location: never
        // treat that as a redirect.
        let redirect = reqwest::redirect::Policy::custom(|a| {
            if a.status() == StatusCode::PERMANENT_REDIRECT || a.previous().len() >= 10 {
                a.stop()
            } else {
                a.follow()
            }
        });
        let http = reqwest::Client::builder()
            .redirect(redirect)
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .user_agent(concat!("mokuro-bunko/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| BErr::Fatal(format!("drive: http client: {e}")))?;
        let chunk = (cfg.chunk_size / CHUNK_MULTIPLE).max(1) * CHUNK_MULTIPLE;
        Ok(GDriveBackend(Arc::new(Inner {
            http,
            token,
            base: cfg.api_base.trim_end_matches('/').to_string(),
            state_dir: cfg.state_dir,
            chunk,
            max_orphans: cfg.max_orphans,
            units: AtomicU64::new(0),
            streak: AtomicU32::new(0),
            revisions_forbidden: AtomicBool::new(false),
            roots: Mutex::new(HashSet::new()),
        })))
    }

    /// Drive quota units spent by this backend so far ([`api::units`]).
    pub fn units_used(&self) -> u64 {
        self.0.units.load(Ordering::Relaxed)
    }
}

enum Payload {
    Empty,
    Json(serde_json::Value),
    Raw(Bytes, String),
}

/// One request, kept whole so a 401 can resend it with a fresh token.
struct Call {
    method: Method,
    url: String,
    query: Vec<(&'static str, String)>,
    headers: Vec<(HeaderName, String)>,
    body: Payload,
    units: u64,
}

impl Call {
    fn new(method: Method, url: String, units: u64) -> Self {
        Call {
            method,
            url,
            query: Vec::new(),
            headers: Vec::new(),
            body: Payload::Empty,
            units,
        }
    }
    fn q(mut self, k: &'static str, v: impl Into<String>) -> Self {
        self.query.push((k, v.into()));
        self
    }
    fn h(mut self, k: HeaderName, v: impl Into<String>) -> Self {
        self.headers.push((k, v.into()));
        self
    }
    fn json(mut self, v: serde_json::Value) -> Self {
        self.body = Payload::Json(v);
        self
    }
    fn raw(mut self, b: Bytes, content_type: impl Into<String>) -> Self {
        self.body = Payload::Raw(b, content_type.into());
        self
    }
}

/// A path segment, percent-encoded (Drive ids are URL-safe, `root` too).
fn seg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn io_err(e: std::io::Error) -> BErr {
    BErr::Fatal(format!("drive: local file: {e}"))
}

fn jitter_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()) % 1000)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn server_time(resp: &reqwest::Response) -> ServerTime {
    resp.headers()
        .get(header::DATE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| httpdate::parse_http_date(v).ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(ServerTime(now_ms()), |d| ServerTime(d.as_millis() as i64))
}

fn range_header(range: Option<(u64, Option<u64>)>) -> Option<String> {
    range.map(|(first, last)| match last {
        Some(l) => format!("bytes={first}-{l}"),
        None => format!("bytes={first}-"),
    })
}

/// `skip` bytes dropped, then at most `limit` passed (a 200 answer to a Range request).
fn slice_stream<S>(
    s: S,
    skip: u64,
    limit: Option<u64>,
) -> impl Stream<Item = std::io::Result<Bytes>>
where
    S: Stream<Item = std::io::Result<Bytes>> + Send + 'static,
{
    futures_util::stream::unfold(
        (Box::pin(s), skip, limit),
        |(mut s, mut skip, mut limit)| async move {
            loop {
                if limit == Some(0) {
                    return None;
                }
                let mut b = match s.next().await? {
                    Ok(b) => b,
                    Err(e) => return Some((Err(e), (s, 0, Some(0)))),
                };
                if skip > 0 {
                    let n = skip.min(b.len() as u64);
                    skip -= n;
                    b = b.slice(n as usize..);
                    if b.is_empty() {
                        continue;
                    }
                }
                if let Some(l) = limit {
                    if b.len() as u64 > l {
                        b.truncate(l as usize);
                    }
                    limit = Some(l - b.len() as u64);
                }
                return Some((Ok(b), (s, skip, limit)));
            }
        },
    )
}

fn media_body(resp: reqwest::Response, range: Option<(u64, Option<u64>)>) -> RemoteBody {
    let status = resp.status();
    let s = resp
        .bytes_stream()
        .map(|r| r.map_err(std::io::Error::other));
    match range {
        Some((first, last)) if status == StatusCode::OK => Box::pin(slice_stream(
            s,
            first,
            last.map(|l| (l + 1).saturating_sub(first)),
        )),
        _ => Box::pin(s),
    }
}

/// What `OpCtx::resume` holds for an upload.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Resume {
    /// New: the `generateIds` id; Replace: the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    /// A resumable session URI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<String>,
}

impl Resume {
    fn load(op: &OpCtx) -> Resume {
        op.resume
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }
    fn save(&self, op: &mut OpCtx) {
        op.resume = serde_json::to_string(self).ok();
    }
}

enum SessionState {
    Done(Box<DFile>),
    At(u64),
    Gone,
}

enum ChunkErr {
    /// The session is unknown or expired: start a new one.
    Gone,
    /// A 4xx the docs say to answer with a new session.
    Restart(BErr),
    /// Retry later on the same session.
    Keep(BErr),
}

impl Inner {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn file_url(&self, id: &str) -> String {
        self.url(&format!("/drive/v3/files/{}", seg(id)))
    }

    /// Send with the current token; a 401 invalidates it and resends once. Any other
    /// status comes back to the caller.
    async fn send_raw(&self, c: &Call) -> Result<reqwest::Response, BErr> {
        let mut refreshed = false;
        loop {
            let token = self.token.access_token().await?;
            let mut rb = self
                .http
                .request(c.method.clone(), &c.url)
                .bearer_auth(&token);
            if !c.query.is_empty() {
                rb = rb.query(&c.query);
            }
            for (k, v) in &c.headers {
                rb = rb.header(k, v);
            }
            rb = match &c.body {
                Payload::Empty => rb,
                Payload::Json(v) => rb.json(v),
                Payload::Raw(b, ct) => rb.header(header::CONTENT_TYPE, ct).body(b.clone()),
            };
            self.units.fetch_add(c.units, Ordering::Relaxed);
            let resp = rb
                .send()
                .await
                .map_err(|e| BErr::Transient(format!("drive: {e}")))?;
            if resp.status() == StatusCode::UNAUTHORIZED {
                if refreshed {
                    return Err(BErr::AuthExpired);
                }
                tracing::debug!("drive: 401, refreshing the access token");
                self.token.invalidate();
                refreshed = true;
                continue;
            }
            if resp.status().is_success() || resp.status() == StatusCode::PERMANENT_REDIRECT {
                self.streak.store(0, Ordering::Relaxed);
            }
            return Ok(resp);
        }
    }

    async fn api_error(resp: reqwest::Response) -> (ApiError, Option<Duration>) {
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = resp.bytes().await.unwrap_or_default();
        (ApiError::parse(status, &body), retry_after)
    }

    fn classify(&self, e: &ApiError, retry_after: Option<Duration>) -> BErr {
        let backoff = if e.is_throttle() {
            let n = self.streak.fetch_add(1, Ordering::Relaxed).min(6);
            Duration::from_secs(1 << n) + Duration::from_millis(jitter_ms())
        } else {
            Duration::ZERO
        };
        e.classify(retry_after, backoff)
    }

    async fn fail(&self, resp: reqwest::Response) -> BErr {
        let (e, ra) = Self::api_error(resp).await;
        self.classify(&e, ra)
    }

    async fn send_ok(&self, c: &Call) -> Result<reqwest::Response, BErr> {
        let resp = self.send_raw(c).await?;
        if resp.status().is_success() {
            Ok(resp)
        } else {
            Err(self.fail(resp).await)
        }
    }

    async fn parse<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, BErr> {
        let body = resp
            .bytes()
            .await
            .map_err(|e| BErr::Transient(format!("drive: {e}")))?;
        serde_json::from_slice(&body).map_err(|e| BErr::Fatal(format!("drive: bad response: {e}")))
    }

    async fn json<T: DeserializeOwned>(&self, c: Call) -> Result<T, BErr> {
        Self::parse(self.send_ok(&c).await?).await
    }

    fn pick_parent(&self, f: &DFile) -> Option<Pid> {
        if f.parents.len() > 1 {
            let roots = self.roots.lock();
            if let Some(p) = f.parents.iter().find(|p| roots.contains(*p)) {
                return Some(p.clone());
            }
        }
        f.parents.first().cloned()
    }

    fn node(&self, f: &DFile) -> RNode {
        f.to_rnode(self.pick_parent(f))
    }

    async fn get_file(&self, id: &str) -> Result<Option<DFile>, BErr> {
        let c = Call::new(Method::GET, self.file_url(id), units::GET).q("fields", FILE_FIELDS);
        let resp = self.send_raw(&c).await?;
        match resp.status() {
            s if s.is_success() => Ok(Some(Self::parse(resp).await?)),
            StatusCode::NOT_FOUND => Ok(None),
            _ => match self.fail(resp).await {
                BErr::NotFound => Ok(None),
                e => Err(e),
            },
        }
    }

    /// Every page of a `files.list` query; returns the last page's server clock.
    async fn list_q(
        &self,
        q: &str,
        f: &mut (dyn FnMut(DFile) -> Result<(), BErr> + Send),
    ) -> Result<ServerTime, BErr> {
        let mut token: Option<String> = None;
        loop {
            let mut c = Call::new(Method::GET, self.url("/drive/v3/files"), units::LIST)
                .q("q", q)
                .q("pageSize", PAGE_SIZE.to_string())
                .q("spaces", "drive")
                .q("fields", format!("nextPageToken,files({FILE_FIELDS})"));
            if let Some(t) = &token {
                c = c.q("pageToken", t.clone());
            }
            let resp = self.send_ok(&c).await?;
            let at = server_time(&resp);
            let page: FileList = Self::parse(resp).await?;
            for file in page.files {
                f(file)?;
            }
            match page.next_page_token {
                Some(t) if !t.is_empty() => token = Some(t),
                _ => return Ok(at),
            }
        }
    }

    async fn create_folder(&self, parent: &str, name: &str) -> Result<RNode, BErr> {
        let c = Call::new(Method::POST, self.url("/drive/v3/files"), units::CREATE)
            .q("fields", FILE_FIELDS)
            .json(json!({"name": name, "mimeType": FOLDER_MIME, "parents": [parent]}));
        let f: DFile = self.json(c).await?;
        Ok(self.node(&f))
    }

    /// A live (non-trashed) node for a write preflight, with the `if_cver` check.
    async fn preflight(&self, pid: &str, if_cver: Option<&str>) -> Result<DFile, BErr> {
        let cur = self
            .get_file(pid)
            .await?
            .filter(|f| !f.trashed)
            .ok_or(BErr::NotFound)?;
        if if_cver.is_some_and(|c| c != cur.cver()) {
            return Err(BErr::PreconditionFailed);
        }
        Ok(cur)
    }

    // ---- reads ----

    async fn read(
        self: &Arc<Self>,
        pid: &str,
        pin: Option<&str>,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<RemoteBody, BErr> {
        let range_h = range_header(range);
        let with_range = |mut c: Call| {
            c = c.h(header::ACCEPT_ENCODING, "identity");
            match &range_h {
                Some(r) => c.h(header::RANGE, r.clone()),
                None => c,
            }
        };
        let mut pre_check = false;
        if let Some(rev) = pin
            && !self.revisions_forbidden.load(Ordering::Relaxed)
        {
            let url = self.url(&format!(
                "/drive/v3/files/{}/revisions/{}",
                seg(pid),
                seg(rev)
            ));
            let c = with_range(Call::new(Method::GET, url, units::DOWNLOAD).q("alt", "media"));
            let resp = self.send_raw(&c).await?;
            let status = resp.status();
            if status.is_success() {
                return Ok(media_body(resp, range));
            }
            if status == StatusCode::RANGE_NOT_SATISFIABLE {
                return Err(BErr::Fatal("drive: range not satisfiable".into()));
            }
            let (e, ra) = Self::api_error(resp).await;
            match status {
                StatusCode::FORBIDDEN if !e.is_throttle() => {
                    tracing::info!(
                        reasons = ?e.reasons,
                        "drive: revision reads refused; pinning reads by re-stat instead"
                    );
                    self.revisions_forbidden.store(true, Ordering::Relaxed);
                }
                // The revision may be gone because the file changed since.
                StatusCode::NOT_FOUND => pre_check = true,
                _ => return Err(self.classify(&e, ra)),
            }
        }
        if let (Some(rev), true) = (pin, pre_check) {
            let cur = self.get_file(pid).await?.ok_or(BErr::NotFound)?;
            if cur.head_revision_id.as_deref() != Some(rev) {
                return Err(BErr::PreconditionFailed);
            }
        }
        let c = with_range(
            Call::new(Method::GET, self.file_url(pid), units::DOWNLOAD).q("alt", "media"),
        );
        let resp = self.send_raw(&c).await?;
        if resp.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(BErr::Fatal("drive: range not satisfiable".into()));
        }
        if !resp.status().is_success() {
            return Err(self.fail(resp).await);
        }
        let body = media_body(resp, range);
        Ok(match pin {
            Some(rev) => post_checked(body, self.clone(), pid.to_string(), rev.to_string()),
            None => body,
        })
    }

    // ---- uploads ----

    async fn generate_id(&self) -> Result<String, BErr> {
        let c = Call::new(
            Method::GET,
            self.url("/drive/v3/files/generateIds"),
            units::GET,
        )
        .q("count", "1")
        .q("space", "drive")
        .q("type", "files");
        let ids: api::GeneratedIds = self.json(c).await?;
        ids.ids
            .into_iter()
            .next()
            .ok_or_else(|| BErr::Fatal("drive: generateIds returned no id".into()))
    }

    async fn same_content(
        &self,
        cur: &DFile,
        path: &Path,
        size: u64,
        sha256: &[u8; 32],
    ) -> Result<bool, BErr> {
        if cur.size != Some(size as i64) {
            return Ok(false);
        }
        if let Some(s) = &cur.sha256_checksum {
            return Ok(s.eq_ignore_ascii_case(&hex::encode(sha256)));
        }
        let Some(md5) = &cur.md5_checksum else {
            return Ok(false);
        };
        let path = path.to_path_buf();
        let local = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
            use md5::Digest;
            let mut h = md5::Md5::new();
            std::io::copy(&mut File::open(path)?, &mut h)?;
            Ok(hex::encode(h.finalize()))
        })
        .await
        .map_err(|e| BErr::Fatal(format!("drive: md5 task: {e}")))?
        .map_err(io_err)?;
        Ok(md5.eq_ignore_ascii_case(&local))
    }

    async fn multipart(
        &self,
        method: Method,
        path: &str,
        meta: serde_json::Value,
        file: &Path,
        cost: u64,
    ) -> Result<DFile, BErr> {
        let data = tokio::fs::read(file).await.map_err(io_err)?;
        let boundary = {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(now_ms().to_le_bytes());
            h.update(jitter_ms().to_le_bytes());
            h.update(&data[..data.len().min(4096)]);
            format!("bunko{}", &hex::encode(h.finalize())[..32])
        };
        let body = api::multipart_related(&meta, &data, &boundary);
        let mut c = Call::new(method.clone(), self.url(path), cost)
            .q("uploadType", "multipart")
            .q("fields", FILE_FIELDS);
        if method == Method::PATCH {
            c = c.q("keepRevisionForever", "false");
        }
        let c = c.raw(
            Bytes::from(body),
            format!("multipart/related; boundary={boundary}"),
        );
        self.json(c).await
    }

    async fn start_session(
        &self,
        method: Method,
        path: &str,
        meta: &serde_json::Value,
        size: u64,
        cost: u64,
    ) -> Result<String, BErr> {
        let mut c = Call::new(method.clone(), self.url(path), cost)
            .q("uploadType", "resumable")
            .q("fields", FILE_FIELDS)
            .h(
                HeaderName::from_static("x-upload-content-length"),
                size.to_string(),
            )
            .h(
                HeaderName::from_static("x-upload-content-type"),
                "application/octet-stream",
            )
            .json(meta.clone());
        if method == Method::PATCH {
            c = c.q("keepRevisionForever", "false");
        }
        let resp = self.send_ok(&c).await?;
        let loc = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| BErr::Fatal("drive: resumable session without a Location".into()))?;
        Ok(if loc.starts_with('/') {
            self.url(loc)
        } else {
            loc.to_string()
        })
    }

    async fn session_status(&self, session: &str, size: u64) -> Result<SessionState, BErr> {
        let c = Call::new(Method::PUT, session.to_string(), units::GET)
            .h(header::CONTENT_RANGE, format!("bytes */{size}"))
            .raw(Bytes::new(), "application/octet-stream");
        let resp = self.send_raw(&c).await?;
        match resp.status().as_u16() {
            200 | 201 => Ok(SessionState::Done(Box::new(Self::parse(resp).await?))),
            308 => Ok(SessionState::At(api::next_offset(
                resp.headers()
                    .get(header::RANGE)
                    .and_then(|v| v.to_str().ok()),
            ))),
            404 | 410 => Ok(SessionState::Gone),
            _ => Err(self.fail(resp).await),
        }
    }

    async fn send_chunks(
        &self,
        session: &str,
        file: &Path,
        size: u64,
        mut offset: u64,
    ) -> Result<DFile, ChunkErr> {
        let keep = |e: std::io::Error| ChunkErr::Keep(io_err(e));
        let mut f = tokio::fs::File::open(file).await.map_err(keep)?;
        let mut failures = 0u32;
        loop {
            if offset >= size && size > 0 {
                return match self.session_status(session, size).await {
                    Ok(SessionState::Done(d)) => Ok(*d),
                    Ok(SessionState::Gone) => Err(ChunkErr::Gone),
                    Ok(SessionState::At(_)) => Err(ChunkErr::Restart(BErr::Transient(
                        "drive: upload session holds every byte but did not finish".into(),
                    ))),
                    Err(e) => Err(ChunkErr::Keep(e)),
                };
            }
            let len = self.chunk.min(size - offset);
            let mut buf = vec![0u8; len as usize];
            f.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(keep)?;
            f.read_exact(&mut buf).await.map_err(keep)?;
            let range = if size == 0 {
                "bytes */0".to_string()
            } else {
                format!("bytes {}-{}/{}", offset, offset + len - 1, size)
            };
            let c = Call::new(Method::PUT, session.to_string(), units::UPDATE)
                .h(header::CONTENT_RANGE, range)
                .raw(Bytes::from(buf), "application/octet-stream");
            let uncertain = match self.send_raw(&c).await {
                Ok(resp) => match resp.status().as_u16() {
                    200 | 201 => return Self::parse(resp).await.map_err(ChunkErr::Keep),
                    308 => {
                        offset = api::next_offset(
                            resp.headers()
                                .get(header::RANGE)
                                .and_then(|v| v.to_str().ok()),
                        );
                        failures = 0;
                        continue;
                    }
                    404 | 410 => return Err(ChunkErr::Gone),
                    s if s == 408 || s >= 500 => self.fail(resp).await,
                    _ => {
                        return Err(match self.fail(resp).await {
                            e @ (BErr::Throttled(_)
                            | BErr::AuthExpired
                            | BErr::AuthRevoked
                            | BErr::StorageFull) => ChunkErr::Keep(e),
                            e => ChunkErr::Restart(e),
                        });
                    }
                },
                Err(e @ BErr::Transient(_)) => e,
                Err(e) => return Err(ChunkErr::Keep(e)),
            };
            failures += 1;
            tracing::debug!(%uncertain, failures, "drive: chunk outcome uncertain; asking the session");
            if failures > CHUNK_RETRIES {
                return Err(ChunkErr::Keep(uncertain));
            }
            tokio::time::sleep(Duration::from_millis(250 << failures)).await;
            match self.session_status(session, size).await {
                Ok(SessionState::Done(d)) => return Ok(*d),
                Ok(SessionState::At(o)) => offset = o,
                Ok(SessionState::Gone) => return Err(ChunkErr::Gone),
                Err(e) => return Err(ChunkErr::Keep(e)),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn resumable(
        &self,
        method: Method,
        path: &str,
        meta: serde_json::Value,
        file: &Path,
        size: u64,
        st: &mut Resume,
        op: &mut OpCtx,
        cost: u64,
    ) -> Result<DFile, BErr> {
        let mut restarts = 0;
        loop {
            let (session, offset) = match st.session.clone() {
                Some(s) => match self.session_status(&s, size).await? {
                    SessionState::Done(f) => return Ok(*f),
                    SessionState::At(o) => (s, o),
                    SessionState::Gone => {
                        st.session = None;
                        st.save(op);
                        continue;
                    }
                },
                None => {
                    let s = self
                        .start_session(method.clone(), path, &meta, size, cost)
                        .await?;
                    st.session = Some(s.clone());
                    st.save(op);
                    (s, 0)
                }
            };
            match self.send_chunks(&session, file, size, offset).await {
                Ok(f) => return Ok(f),
                Err(ChunkErr::Keep(e)) => return Err(e),
                Err(ChunkErr::Restart(e)) => {
                    st.session = None;
                    st.save(op);
                    return Err(e);
                }
                Err(ChunkErr::Gone) => {
                    st.session = None;
                    st.save(op);
                    restarts += 1;
                    if restarts > 1 {
                        return Err(BErr::Transient("drive: upload session expired".into()));
                    }
                }
            }
        }
    }

    async fn upload(
        &self,
        to: UploadTarget<'_>,
        src: UploadSource<'_>,
        op: &mut OpCtx,
    ) -> Result<RNode, BErr> {
        let UploadSource::File { path, size, sha256 } = src;
        let mut st = Resume::load(op);
        let f = match to {
            UploadTarget::New { parent, name } => {
                let id = match st.id.clone() {
                    // A retry: the earlier attempt may have landed under its id.
                    Some(id) => {
                        if let Some(f) = self.get_file(&id).await? {
                            return Ok(self.node(&f));
                        }
                        id
                    }
                    None => {
                        let id = self.generate_id().await?;
                        st.id = Some(id.clone());
                        st.save(op);
                        id
                    }
                };
                let meta = json!({"id": id, "name": name, "parents": [parent]});
                let path_ = "/upload/drive/v3/files";
                if size <= SIMPLE_UPLOAD_MAX && st.session.is_none() {
                    match self
                        .multipart(Method::POST, path_, meta, path, units::CREATE)
                        .await
                    {
                        // The id is taken: an earlier attempt landed after all.
                        Err(BErr::NameConflict) => {
                            self.get_file(&id).await?.ok_or(BErr::NameConflict)?
                        }
                        r => r?,
                    }
                } else {
                    self.resumable(
                        Method::POST,
                        path_,
                        meta,
                        path,
                        size,
                        &mut st,
                        op,
                        units::CREATE,
                    )
                    .await?
                }
            }
            UploadTarget::Replace { pid, if_cver } => {
                let cur = self
                    .get_file(pid)
                    .await?
                    .filter(|f| !f.trashed && !f.is_dir())
                    .ok_or(BErr::NotFound)?;
                // Never rewrite unchanged bytes: every rewrite is a stored revision.
                // This also completes a retry whose earlier attempt landed.
                if self.same_content(&cur, path, size, &sha256).await? {
                    return Ok(self.node(&cur));
                }
                if if_cver.is_some_and(|c| c != cur.cver()) {
                    return Err(BErr::PreconditionFailed);
                }
                if st.id.as_deref() != Some(pid) {
                    st = Resume {
                        id: Some(pid.to_string()),
                        session: None,
                    };
                }
                let path_ = format!("/upload/drive/v3/files/{}", seg(pid));
                if size <= SIMPLE_UPLOAD_MAX && st.session.is_none() {
                    self.multipart(Method::PATCH, &path_, json!({}), path, units::UPDATE)
                        .await?
                } else {
                    self.resumable(
                        Method::PATCH,
                        &path_,
                        json!({}),
                        path,
                        size,
                        &mut st,
                        op,
                        units::UPDATE,
                    )
                    .await?
                }
            }
        };
        Ok(self.node(&f))
    }
}

/// After the last byte, re-stat the file: a different `headRevisionId` means the bytes
/// were not the pinned version, and the stream ends in `PreconditionFailed` (wrapped in
/// the `io::Error`) instead of a clean end.
fn post_checked(inner: RemoteBody, be: Arc<Inner>, pid: String, rev: String) -> RemoteBody {
    let s = futures_util::stream::unfold(Some((inner, be, pid, rev)), |st| async move {
        let (mut inner, be, pid, rev) = st?;
        match inner.next().await {
            Some(Ok(b)) => Some((Ok(b), Some((inner, be, pid, rev)))),
            Some(Err(e)) => Some((Err(e), None)),
            None => match be.get_file(&pid).await {
                Ok(Some(f)) if f.head_revision_id.as_deref() == Some(rev.as_str()) => None,
                Ok(_) => Some((Err(std::io::Error::other(BErr::PreconditionFailed)), None)),
                Err(e) => Some((Err(std::io::Error::other(e)), None)),
            },
        }
    });
    Box::pin(s)
}

/// Full-listing assembly: keeps only descendants of the roots, emitting parents before
/// children. A file whose parent has not been listed yet waits (in memory up to
/// `max`, then in a spill file) until it is.
struct Resolver<'s> {
    roots: HashSet<Pid>,
    desc: HashSet<Pid>,
    waiting: HashMap<Pid, Vec<DFile>>,
    waiting_n: usize,
    max: usize,
    multi_done: HashSet<Pid>,
    spill: Option<Spill>,
    spill_path: PathBuf,
    sink: &'s mut (dyn FnMut(RNode) + Send),
}

struct Spill {
    path: PathBuf,
    w: BufWriter<File>,
}

/// Removes the spill files however the listing ends.
struct SpillGuard(PathBuf);

impl Drop for SpillGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(self.0.with_extension("next"));
    }
}

impl<'s> Resolver<'s> {
    fn new(
        roots: &[Pid],
        max: usize,
        spill_path: PathBuf,
        sink: &'s mut (dyn FnMut(RNode) + Send),
    ) -> Self {
        let roots: HashSet<Pid> = roots.iter().cloned().collect();
        Resolver {
            desc: roots.clone(),
            roots,
            waiting: HashMap::new(),
            waiting_n: 0,
            max,
            multi_done: HashSet::new(),
            spill: None,
            spill_path,
            sink,
        }
    }

    fn offer(&mut self, f: DFile) -> Result<(), BErr> {
        if self.roots.contains(&f.id) || f.parents.is_empty() {
            return Ok(());
        }
        if let Some(p) = f.parents.iter().find(|p| self.desc.contains(*p)).cloned() {
            self.emit(f, p);
        } else if self.waiting_n < self.max {
            self.waiting_n += f.parents.len();
            for p in &f.parents {
                self.waiting.entry(p.clone()).or_default().push(f.clone());
            }
        } else {
            self.spill_one(&f)?;
        }
        Ok(())
    }

    fn spill_one(&mut self, f: &DFile) -> Result<(), BErr> {
        let fail = |e: std::io::Error| BErr::Fatal(format!("drive: listing spill: {e}"));
        if self.spill.is_none() {
            if let Some(dir) = self.spill_path.parent() {
                std::fs::create_dir_all(dir).map_err(fail)?;
            }
            tracing::info!(
                path = %self.spill_path.display(),
                "drive: many files outside known folders; spilling the listing to disk"
            );
            self.spill = Some(Spill {
                path: self.spill_path.clone(),
                w: BufWriter::new(File::create(&self.spill_path).map_err(fail)?),
            });
        }
        let sp = self.spill.as_mut().expect("just set");
        serde_json::to_writer(&mut sp.w, f).map_err(|e| fail(e.into()))?;
        sp.w.write_all(b"\n").map_err(fail)
    }

    fn emit(&mut self, f: DFile, parent: Pid) {
        let mut work = vec![(f, parent)];
        while let Some((f, parent)) = work.pop() {
            if self.roots.contains(&f.id) {
                continue;
            }
            if f.parents.len() > 1 && !self.multi_done.insert(f.id.clone()) {
                continue;
            }
            if f.is_dir() {
                self.desc.insert(f.id.clone());
                if let Some(kids) = self.waiting.remove(&f.id) {
                    self.waiting_n -= kids.len();
                    work.extend(kids.into_iter().map(|k| (k, f.id.clone())));
                }
            }
            (self.sink)(f.to_rnode(Some(parent)));
        }
    }

    /// Re-offer spilled files until a pass places none; what is left is outside.
    fn finish(mut self) -> Result<(), BErr> {
        let Some(mut sp) = self.spill.take() else {
            return Ok(());
        };
        let fail = |e: std::io::Error| BErr::Fatal(format!("drive: listing spill: {e}"));
        let _guard = SpillGuard(sp.path.clone());
        sp.w.flush().map_err(fail)?;
        drop(sp.w);
        let next = sp.path.with_extension("next");
        loop {
            let mut placed = false;
            let mut kept = 0usize;
            {
                let r = BufReader::new(File::open(&sp.path).map_err(fail)?);
                let mut w = BufWriter::new(File::create(&next).map_err(fail)?);
                for line in r.lines() {
                    let line = line.map_err(fail)?;
                    let f: DFile = serde_json::from_str(&line)
                        .map_err(|e| BErr::Fatal(format!("drive: listing spill: {e}")))?;
                    match f.parents.iter().find(|p| self.desc.contains(*p)).cloned() {
                        Some(p) => {
                            self.emit(f, p);
                            placed = true;
                        }
                        None => {
                            w.write_all(line.as_bytes()).map_err(fail)?;
                            w.write_all(b"\n").map_err(fail)?;
                            kept += 1;
                        }
                    }
                }
                w.flush().map_err(fail)?;
            }
            std::fs::rename(&next, &sp.path).map_err(fail)?;
            if !placed || kept == 0 {
                return Ok(());
            }
        }
    }
}

impl Backend for GDriveBackend {
    fn caps(&self) -> BackendCaps {
        BackendCaps {
            cas_create: false,
            // `if_cver` is honoured by a preflight, not atomically.
            cas_overwrite: false,
            atomic_replace: true,
            server_copy: true,
            range: true,
            pinned_read: true,
            push: false,
            hash_kind: Some(HashKind::Sha256),
            mtime_granularity_ns: 1_000_000,
            simple_upload_max: SIMPLE_UPLOAD_MAX,
            chunk_multiple: CHUNK_MULTIPLE,
            path_ids: false,
        }
    }

    async fn account(&self) -> Result<AccountInfo, BErr> {
        let c = Call::new(Method::GET, self.0.url("/drive/v3/about"), units::GET)
            .q("fields", "user(permissionId,emailAddress,displayName)");
        let about: api::About = self.0.json(c).await?;
        let u = about
            .user
            .ok_or_else(|| BErr::Fatal("drive: about without user".into()))?;
        Ok(AccountInfo {
            id: u
                .permission_id
                .or_else(|| u.email_address.clone())
                .unwrap_or_default(),
            label: u.email_address.or(u.display_name).unwrap_or_default(),
        })
    }

    /// Every visible, non-trashed folder named exactly `name`, under any parent
    /// (`drive.file` lists only what this app may see), oldest first.
    async fn locate_roots(&self, name: &str) -> Result<Vec<RNode>, BErr> {
        let q = format!(
            "name={} and mimeType='{FOLDER_MIME}' and trashed=false",
            api::q_str(name)
        );
        let mut found: Vec<DFile> = Vec::new();
        self.0
            .list_q(&q, &mut |f| {
                // `name =` may match case-insensitively; the reader keys by exact name.
                if f.name == name && f.is_dir() && !f.trashed {
                    found.push(f);
                }
                Ok(())
            })
            .await?;
        let mut nodes: Vec<RNode> = found
            .iter()
            .map(|f| f.to_rnode(f.parents.first().cloned()))
            .collect();
        nodes.sort_by(|a, b| (a.created_ms, &a.pid).cmp(&(b.created_ms, &b.pid)));
        self.0
            .roots
            .lock()
            .extend(nodes.iter().map(|n| n.pid.clone()));
        Ok(nodes)
    }

    async fn create_root(&self, name: &str) -> Result<RNode, BErr> {
        let n = self.0.create_folder("root", name).await?;
        self.0.roots.lock().insert(n.pid.clone());
        Ok(n)
    }

    async fn start_cursor(&self) -> Result<Cursor, BErr> {
        let c = Call::new(
            Method::GET,
            self.0.url("/drive/v3/changes/startPageToken"),
            units::GET,
        );
        let t: api::StartToken = self.0.json(c).await?;
        Ok(Cursor(t.start_page_token))
    }

    async fn list_all(
        &self,
        roots: &[Pid],
        sink: &mut (dyn FnMut(RNode) + Send),
    ) -> Result<(), BErr> {
        self.0.roots.lock().extend(roots.iter().cloned());
        let spill = self.0.state_dir.join(format!(
            "gdrive-listing-{}-{}.jsonl",
            std::process::id(),
            now_ms()
        ));
        let mut r = Resolver::new(roots, self.0.max_orphans, spill, sink);
        let res = self
            .0
            .list_q("'me' in owners and trashed=false", &mut |f| r.offer(f))
            .await;
        match res {
            Ok(_) => r.finish(),
            Err(e) => {
                if let Some(sp) = r.spill.take() {
                    let path = sp.path.clone();
                    drop(sp);
                    drop(SpillGuard(path));
                }
                Err(e)
            }
        }
    }

    async fn list_dir(&self, dir: &str, sink: &mut (dyn FnMut(RNode) + Send)) -> Result<(), BErr> {
        let q = format!("{} in parents and trashed=false", api::q_str(dir));
        let dir = dir.to_string();
        self.0
            .list_q(&q, &mut |f| {
                sink(f.to_rnode(Some(dir.clone())));
                Ok(())
            })
            .await?;
        Ok(())
    }

    async fn find_named(&self, parent: &str, name: &str) -> Result<(Vec<RNode>, ServerTime), BErr> {
        let q = format!(
            "{} in parents and name={} and trashed=false",
            api::q_str(parent),
            api::q_str(name)
        );
        let mut found = Vec::new();
        let at = self
            .0
            .list_q(&q, &mut |f| {
                if f.name == name {
                    found.push(f.to_rnode(Some(parent.to_string())));
                }
                Ok(())
            })
            .await?;
        Ok((found, at))
    }

    async fn changes(&self, c: &Cursor) -> Result<ChangePage, BErr> {
        let call = Call::new(Method::GET, self.0.url("/drive/v3/changes"), units::CHANGES)
            .q("pageToken", c.0.clone())
            .q("pageSize", PAGE_SIZE.to_string())
            .q("includeRemoved", "true")
            .q("spaces", "drive")
            .q(
                "fields",
                format!(
                    "nextPageToken,newStartPageToken,changes(changeType,removed,fileId,file({FILE_FIELDS}))"
                ),
            );
        let resp = self.0.send_raw(&call).await?;
        match resp.status() {
            s if s.is_success() => {}
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::GONE => {
                let (e, _) = Inner::api_error(resp).await;
                tracing::info!(status = e.status, message = %e.message, "drive: change cursor lost");
                return Ok(ChangePage::CursorLost);
            }
            _ => return Err(self.0.fail(resp).await),
        }
        let list: ChangeList = Inner::parse(resp).await?;
        let mut items = Vec::with_capacity(list.changes.len());
        for ch in list.changes {
            if ch.change_type.as_deref().is_some_and(|t| t != "file") {
                continue;
            }
            let Some(pid) = ch
                .file_id
                .clone()
                .or_else(|| ch.file.as_ref().map(|f| f.id.clone()))
            else {
                continue;
            };
            match ch.file {
                Some(f) if !ch.removed && !f.trashed => {
                    items.push(ChangeItem::Upsert(self.0.node(&f)))
                }
                _ => items.push(ChangeItem::Removed { pid }),
            }
        }
        match (list.next_page_token, list.new_start_page_token) {
            (Some(n), _) if !n.is_empty() => Ok(ChangePage::Page {
                items,
                next: Cursor(n),
                more: true,
            }),
            (_, Some(s)) => Ok(ChangePage::Page {
                items,
                next: Cursor(s),
                more: false,
            }),
            _ => Err(BErr::Fatal("drive: changes page without a token".into())),
        }
    }

    async fn wait_change(&self, _c: &Cursor, _max: Duration) -> Result<(), BErr> {
        Err(BErr::Unsupported)
    }

    async fn stat(&self, pid: &str) -> Result<Option<RNode>, BErr> {
        Ok(self.0.get_file(pid).await?.map(|f| self.0.node(&f)))
    }

    async fn read(
        &self,
        pid: &str,
        pin: Option<&str>,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<RemoteBody, BErr> {
        self.0.read(pid, pin, range).await
    }

    async fn upload(
        &self,
        to: UploadTarget<'_>,
        src: UploadSource<'_>,
        op: &mut OpCtx,
    ) -> Result<RNode, BErr> {
        self.0.upload(to, src, op).await
    }

    async fn mkdir(&self, parent: &str, name: &str, _op: &OpCtx) -> Result<RNode, BErr> {
        self.0.create_folder(parent, name).await
    }

    async fn update(
        &self,
        pid: &str,
        parent: Option<&str>,
        name: Option<&str>,
        if_cver: Option<&str>,
    ) -> Result<RNode, BErr> {
        let mut c =
            Call::new(Method::PATCH, self.0.file_url(pid), units::UPDATE).q("fields", FILE_FIELDS);
        if parent.is_some() || if_cver.is_some() {
            let cur = self.0.preflight(pid, if_cver).await?;
            if let Some(p) = parent
                && !cur.parents.iter().any(|x| x == p)
            {
                c = c.q("addParents", p);
                let old: Vec<&str> = cur.parents.iter().map(String::as_str).collect();
                if !old.is_empty() {
                    c = c.q("removeParents", old.join(","));
                }
            }
        }
        let mut meta = serde_json::Map::new();
        if let Some(n) = name {
            meta.insert("name".into(), n.into());
        }
        let f: DFile = self.0.json(c.json(serde_json::Value::Object(meta))).await?;
        Ok(self.0.node(&f))
    }

    async fn copy(&self, pid: &str, parent: &str, name: &str) -> Result<RNode, BErr> {
        let url = self.0.url(&format!("/drive/v3/files/{}/copy", seg(pid)));
        let c = Call::new(Method::POST, url, units::CREATE)
            .q("fields", FILE_FIELDS)
            .json(json!({"name": name, "parents": [parent]}));
        let f: DFile = self.0.json(c).await?;
        Ok(self.0.node(&f))
    }

    async fn trash(&self, pid: &str, if_cver: Option<&str>) -> Result<(), BErr> {
        if if_cver.is_some() {
            self.0.preflight(pid, if_cver).await?;
        }
        let c = Call::new(Method::PATCH, self.0.file_url(pid), units::UPDATE)
            .q("fields", "id")
            .json(json!({"trashed": true}));
        self.0.send_ok(&c).await?;
        Ok(())
    }

    async fn delete_permanent(&self, pid: &str) -> Result<(), BErr> {
        let c = Call::new(Method::DELETE, self.0.file_url(pid), units::UPDATE);
        self.0.send_ok(&c).await?;
        Ok(())
    }

    async fn quota(&self) -> Result<Option<bunko_store::Capacity>, BErr> {
        let c = Call::new(Method::GET, self.0.url("/drive/v3/about"), units::GET)
            .q("fields", "storageQuota(limit,usage)");
        let about: api::About = self.0.json(c).await?;
        Ok(about.storage_quota.and_then(|q| {
            let total = q.limit?.max(0) as u64;
            let used = q.usage.unwrap_or(0).max(0) as u64;
            Some(bunko_store::Capacity {
                total,
                available: total.saturating_sub(used),
            })
        }))
    }
}
