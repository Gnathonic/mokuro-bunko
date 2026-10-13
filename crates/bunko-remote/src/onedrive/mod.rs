//! The OneDrive driver: Microsoft Graph v1.0, hand-rolled on reqwest (spec §5.3, §5.6,
//! §5.7, §5.9, §5.11, §6).
//!
//! - Ids are stable across rename, move and content replace; names are case-insensitive
//!   and case-preserving, and a folder never holds two names that differ only in case
//!   (so OneDrive never shows case variants; the index still compares names
//!   byte-exact). The root is `/me/drive/root:/mokuro-reader`.
//! - `version` is `eTag`, `cver` and `pin` are `cTag` (folders: `eTag`), the hash is
//!   `quickXorHash` ([`quickxor`]), else `sha1Hash`. The base mtime is the top-level
//!   `lastModifiedDateTime`, not the client-settable `fileSystemInfo` one.
//! - Writes are conditional: creates send `conflictBehavior=fail`; replaces, renames,
//!   moves and trashes send `If-Match`. Every replace, small ones included, goes through
//!   an upload session, because `If-Match` on a simple PUT is undocumented.
//! - The scope (`Files.ReadWrite`) covers the whole drive, so the change feed (root
//!   `delta`) is filtered to the roots' subtrees by id ancestry: nothing outside is ever
//!   reported except as an opaque `Removed { pid }`, which is how a move out of the
//!   subtree reaches the index (an id the index never had is a no-op there).
//! - Downloads follow the 302 to the pre-authenticated URL by hand, without the
//!   `Authorization` header; upload fragments and copy monitors are pre-authenticated
//!   too and never get the token.
//! - Requests carry the `ISV|Mokuro|Bunko/<version>` User-Agent, and each adds its
//!   resource-unit cost ([`api::units`]) to [`OneDriveBackend::units_used`].

pub mod api;
pub mod quickxor;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
pub use crate::token_dyn::{StaticToken, TokenSourceDyn};
use api::{ApiError, Item, PAGE_TOP, Page, UploadSession, bare_tag, check_name, seg, units};

/// Graph's v1.0 endpoint.
pub const DEFAULT_API_BASE: &str = "https://graph.microsoft.com/v1.0";
/// New files up to this size go up in one PUT; larger ones (and every replace) use an
/// upload session.
pub const SIMPLE_UPLOAD_MAX: u64 = 4 << 20;
/// Session fragments must be multiples of this (except the last).
pub const FRAGMENT_MULTIPLE: u64 = 320 << 10;
/// The default fragment: 32 × 320 KiB = 10 MiB (spec §5.7).
pub const DEFAULT_FRAGMENT: u64 = 32 * FRAGMENT_MULTIPLE;
/// Consecutive uncertain fragments one `upload` call retries (after a status query)
/// before handing the error to the outbox.
const FRAGMENT_RETRIES: u32 = 3;
/// Longest parent chain the ancestry filter walks before calling an item outside.
const MAX_DEPTH: usize = 256;

/// `ISV|Mokuro|Bunko/<crate version>`: decorated traffic is throttled last.
pub fn user_agent() -> String {
    format!("ISV|Mokuro|Bunko/{}", env!("CARGO_PKG_VERSION"))
}

/// Driver settings.
#[derive(Debug, Clone)]
pub struct OneDriveConfig {
    /// `https://graph.microsoft.com/v1.0`, or a mock server in tests.
    pub api_base: String,
    /// Upload-session fragment size, rounded down to a multiple of 320 KiB.
    pub fragment_size: u64,
    /// How often a copy's monitor is polled.
    pub copy_poll: Duration,
    /// How long `copy` waits for the monitor before giving up (Transient).
    pub copy_timeout: Duration,
}

impl Default for OneDriveConfig {
    fn default() -> Self {
        OneDriveConfig {
            api_base: DEFAULT_API_BASE.to_string(),
            fragment_size: DEFAULT_FRAGMENT,
            copy_poll: Duration::from_secs(1),
            copy_timeout: Duration::from_secs(300),
        }
    }
}

/// The OneDrive backend. Cheap to clone.
#[derive(Clone)]
pub struct OneDriveBackend(Arc<Inner>);

/// What the ancestry filter knows: the roots, and folder → parent for every folder
/// seen anywhere in the drive (`None`: the drive root, or a folder that is gone).
/// Membership is computed by walking this map, so a folder move re-files its whole
/// subtree at once; unknown links are fetched (one get each) and remembered.
#[derive(Default)]
struct Tree {
    roots: HashSet<Pid>,
    dirs: HashMap<Pid, Option<Pid>>,
}

struct Inner {
    http: reqwest::Client,
    token: Arc<dyn TokenSourceDyn>,
    base: String,
    fragment: u64,
    copy_poll: Duration,
    copy_timeout: Duration,
    units: AtomicU64,
    /// Consecutive throttles, for the exponential suggestion when there is no Retry-After.
    streak: AtomicU32,
    /// `permanentDelete` was refused as unsupported (OneDrive Personal): use DELETE.
    no_permanent_delete: AtomicBool,
    tree: Mutex<Tree>,
}

impl OneDriveBackend {
    pub fn new(token: Arc<dyn TokenSourceDyn>, cfg: OneDriveConfig) -> Result<Self, BErr> {
        // Redirects are followed by hand: a download's 302 goes to a pre-authenticated
        // URL that must not see the bearer token, and a copy monitor's 303 names the
        // new item.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .user_agent(user_agent())
            .build()
            .map_err(|e| BErr::Fatal(format!("onedrive: http client: {e}")))?;
        let fragment = (cfg.fragment_size / FRAGMENT_MULTIPLE).max(1) * FRAGMENT_MULTIPLE;
        Ok(OneDriveBackend(Arc::new(Inner {
            http,
            token,
            base: cfg.api_base.trim_end_matches('/').to_string(),
            fragment,
            copy_poll: cfg.copy_poll,
            copy_timeout: cfg.copy_timeout,
            units: AtomicU64::new(0),
            streak: AtomicU32::new(0),
            no_permanent_delete: AtomicBool::new(false),
            tree: Mutex::new(Tree::default()),
        })))
    }

    /// Graph resource units spent by this backend so far ([`api::units`]).
    pub fn units_used(&self) -> u64 {
        self.0.units.load(Ordering::Relaxed)
    }
}

enum Payload {
    Empty,
    Json(serde_json::Value),
    Raw(Bytes, &'static str),
}

/// One request, kept whole so a 401 can resend it with a fresh token.
struct Call {
    method: Method,
    url: String,
    headers: Vec<(HeaderName, String)>,
    body: Payload,
    units: u64,
    /// Send the bearer token (false for pre-authenticated URLs).
    auth: bool,
}

impl Call {
    fn new(method: Method, url: String, units: u64) -> Self {
        Call {
            method,
            url,
            headers: Vec::new(),
            body: Payload::Empty,
            units,
            auth: true,
        }
    }
    fn h(mut self, k: HeaderName, v: impl Into<String>) -> Self {
        self.headers.push((k, v.into()));
        self
    }
    fn if_match(self, tag: Option<&str>) -> Self {
        match tag {
            Some(t) => self.h(header::IF_MATCH, t),
            None => self,
        }
    }
    fn json(mut self, v: serde_json::Value) -> Self {
        self.body = Payload::Json(v);
        self
    }
    fn raw(mut self, b: Bytes) -> Self {
        self.body = Payload::Raw(b, "application/octet-stream");
        self
    }
    fn pre_authenticated(mut self) -> Self {
        self.auth = false;
        self
    }
}

fn io_err(e: std::io::Error) -> BErr {
    BErr::Fatal(format!("onedrive: local file: {e}"))
}

fn transient(e: reqwest::Error) -> BErr {
    BErr::Transient(format!("onedrive: {e}"))
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

fn header_str(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// `Retry-After` as seconds or an HTTP date.
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let v = resp
        .headers()
        .get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(s) = v.parse::<u64>() {
        return Some(Duration::from_secs(s));
    }
    let at = httpdate::parse_http_date(v).ok()?;
    Some(
        at.duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

fn range_header(range: Option<(u64, Option<u64>)>) -> Option<String> {
    range.map(|(first, last)| match last {
        Some(l) => format!("bytes={first}-{l}"),
        None => format!("bytes={first}-"),
    })
}

/// `skip` bytes dropped, then at most `limit` passed (a 200 answer to a Range request,
/// which the download host "may" send).
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

/// What `OpCtx::resume` holds for an upload: the session's `uploadUrl` and the size
/// it was made for.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Resume {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default)]
    size: u64,
}

impl Resume {
    fn load(op: &OpCtx) -> Resume {
        op.resume
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }
    fn save(&self, op: &mut OpCtx) {
        op.resume = if self.url.is_some() {
            serde_json::to_string(self).ok()
        } else {
            None
        };
    }
}

/// The staged file's QuickXorHash, computed on first use.
struct LocalHash {
    path: PathBuf,
    value: Option<String>,
}

impl LocalHash {
    async fn get(&mut self) -> Result<String, BErr> {
        if let Some(v) = &self.value {
            return Ok(v.clone());
        }
        let p = self.path.clone();
        let v = tokio::task::spawn_blocking(move || quickxor::file_quickxor(&p))
            .await
            .map_err(|e| BErr::Fatal(format!("onedrive: hash task: {e}")))?
            .map_err(io_err)?;
        self.value = Some(v.clone());
        Ok(v)
    }
}

enum SessionState {
    /// The next byte the session wants.
    At(u64),
    /// Unknown or expired (or already committed).
    Gone,
}

enum FragErr {
    /// The session is unknown or expired.
    Gone,
    /// The session is dead (a conflict, a 4xx): drop it.
    Restart(BErr),
    /// Retry later on the same session.
    Keep(BErr),
}

impl Inner {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn item_url(&self, id: &str) -> String {
        self.url(&format!("/me/drive/items/{}", seg(id)))
    }

    /// `/me/drive/items/{parent}:/{name}` (path addressing relative to an id).
    fn child_url(&self, parent: &str, name: &str) -> String {
        self.url(&format!("/me/drive/items/{}:/{}", seg(parent), seg(name)))
    }

    /// A link Graph handed back (nextLink, deltaLink) gets the bearer token only when
    /// it points under the API base.
    fn own_link(&self, link: &str) -> Result<(), BErr> {
        if link.starts_with(&format!("{}/", self.base)) {
            Ok(())
        } else {
            Err(BErr::Fatal(format!(
                "onedrive: refusing a link outside {}: {link}",
                self.base
            )))
        }
    }

    /// Send; with `auth`, a 401 invalidates the token and resends once. Any other
    /// status comes back to the caller.
    async fn send_raw(&self, c: &Call) -> Result<reqwest::Response, BErr> {
        let mut refreshed = false;
        loop {
            let mut rb = self.http.request(c.method.clone(), &c.url);
            if c.auth {
                let token = self.token.access_token().await?;
                rb = rb.bearer_auth(token);
            }
            for (k, v) in &c.headers {
                rb = rb.header(k, v);
            }
            rb = match &c.body {
                Payload::Empty if c.method == Method::GET || c.method == Method::DELETE => rb,
                // Graph wants a Content-Length on bodiless POSTs and PUTs.
                Payload::Empty => rb.body(Bytes::new()),
                Payload::Json(v) => rb.json(v),
                Payload::Raw(b, ct) => rb.header(header::CONTENT_TYPE, *ct).body(b.clone()),
            };
            self.units.fetch_add(c.units, Ordering::Relaxed);
            crate::governor::charge(c.units);
            let resp = rb.send().await.map_err(transient)?;
            if c.auth && resp.status() == StatusCode::UNAUTHORIZED {
                if refreshed {
                    return Err(BErr::AuthExpired);
                }
                tracing::debug!("onedrive: 401, refreshing the access token");
                self.token.invalidate();
                refreshed = true;
                continue;
            }
            if resp.status().is_success() {
                self.streak.store(0, Ordering::Relaxed);
            }
            return Ok(resp);
        }
    }

    async fn api_error(resp: reqwest::Response) -> (ApiError, Option<Duration>) {
        let status = resp.status().as_u16();
        let ra = retry_after(&resp);
        let body = resp.bytes().await.unwrap_or_default();
        (ApiError::parse(status, &body), ra)
    }

    fn classify(&self, e: &ApiError, ra: Option<Duration>) -> BErr {
        let backoff = if e.is_throttle(ra) {
            let n = self.streak.fetch_add(1, Ordering::Relaxed).min(6);
            Duration::from_secs(1 << n) + Duration::from_millis(jitter_ms())
        } else {
            Duration::ZERO
        };
        let err = e.classify(ra, backoff);
        if !matches!(
            err,
            BErr::NotFound | BErr::NameConflict | BErr::PreconditionFailed
        ) {
            tracing::debug!(status = e.status, codes = ?e.codes, message = %e.message, "onedrive: request failed");
        }
        err
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
        let body = resp.bytes().await.map_err(transient)?;
        serde_json::from_slice(&body)
            .map_err(|e| BErr::Fatal(format!("onedrive: bad response: {e}")))
    }

    async fn json<T: DeserializeOwned>(&self, c: Call) -> Result<T, BErr> {
        Self::parse(self.send_ok(&c).await?).await
    }

    /// Remember where a folder sits (for the ancestry filter).
    fn note(&self, it: &Item) {
        if it.is_dir() {
            let parent = if it.root.is_some() {
                None
            } else {
                it.parent_id()
            };
            self.tree.lock().dirs.insert(it.id.clone(), parent);
        }
    }

    fn node(&self, it: &Item) -> RNode {
        self.note(it);
        it.to_rnode(it.parent_id())
    }

    /// A GET that answers `None` on 404.
    async fn get_opt(&self, url: String) -> Result<Option<Item>, BErr> {
        let resp = self
            .send_raw(&Call::new(Method::GET, url, units::GET))
            .await?;
        match resp.status() {
            s if s.is_success() => Ok(Some(Self::parse(resp).await?)),
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(self.fail(resp).await),
        }
    }

    async fn get_item(&self, id: &str) -> Result<Option<Item>, BErr> {
        Ok(self
            .get_opt(self.item_url(id))
            .await?
            .filter(|it| it.deleted.is_none()))
    }

    /// The child of `parent` named `name` (Graph matches case-insensitively).
    async fn get_child(&self, parent: &str, name: &str) -> Result<Option<Item>, BErr> {
        Ok(self
            .get_opt(self.child_url(parent, name))
            .await?
            .filter(|it| it.deleted.is_none()))
    }

    /// Every live child of `dir`, page by page.
    async fn children(&self, dir: &str, f: &mut (dyn FnMut(Item) + Send)) -> Result<(), BErr> {
        let mut url = format!("{}/children?$top={PAGE_TOP}", self.item_url(dir));
        loop {
            let page: Page = self.json(Call::new(Method::GET, url, units::LIST)).await?;
            for it in page.value {
                if it.deleted.is_none() {
                    self.note(&it);
                    f(it);
                }
            }
            match page.next_link {
                Some(n) => {
                    self.own_link(&n)?;
                    url = n;
                }
                None => return Ok(()),
            }
        }
    }

    /// Is a node whose parent is `parent` inside one of the roots' subtrees?
    async fn in_subtree(&self, parent: Option<&str>) -> Result<bool, BErr> {
        let Some(mut cur) = parent.map(str::to_string) else {
            return Ok(false);
        };
        for _ in 0..MAX_DEPTH {
            let step = {
                let t = self.tree.lock();
                if t.roots.contains(&cur) {
                    return Ok(true);
                }
                t.dirs.get(&cur).cloned()
            };
            let next = match step {
                Some(n) => n,
                None => {
                    let url = format!(
                        "{}?$select=id,name,parentReference,folder,root",
                        self.item_url(&cur)
                    );
                    let up = match self.get_opt(url).await? {
                        Some(it) if it.root.is_none() && it.deleted.is_none() => it.parent_id(),
                        _ => None,
                    };
                    self.tree.lock().dirs.insert(cur.clone(), up.clone());
                    up
                }
            };
            match next {
                Some(p) => cur = p,
                None => return Ok(false),
            }
        }
        tracing::warn!(pid = %cur, "onedrive: parent chain too deep; treating the item as outside");
        Ok(false)
    }

    // ---- reads ----

    async fn read(
        self: &Arc<Self>,
        pid: &str,
        pin: Option<&str>,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<RemoteBody, BErr> {
        for attempt in 0..2 {
            let c = Call::new(
                Method::GET,
                format!("{}/content", self.item_url(pid)),
                units::DOWNLOAD,
            );
            let resp = self.send_raw(&c).await?;
            let resp = match resp.status() {
                s if s.is_redirection() => {
                    let loc = header_str(&resp, "location").ok_or_else(|| {
                        BErr::Transient("onedrive: download redirect without a Location".into())
                    })?;
                    // A fresh request: the pre-authenticated URL never sees the token.
                    let mut rb = self
                        .http
                        .get(&loc)
                        .header(header::ACCEPT_ENCODING, "identity");
                    if let Some(r) = range_header(range) {
                        rb = rb.header(header::RANGE, r);
                    }
                    let r2 = rb.send().await.map_err(transient)?;
                    match r2.status() {
                        s if s.is_success() => r2,
                        // An expired pre-authenticated URL: ask for a fresh one once.
                        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN if attempt == 0 => {
                            continue;
                        }
                        StatusCode::RANGE_NOT_SATISFIABLE => {
                            return Err(BErr::Fatal("onedrive: range not satisfiable".into()));
                        }
                        _ => return Err(self.fail(r2).await),
                    }
                }
                // Served directly (no redirect): any Range was not sent, so slice.
                s if s.is_success() => resp,
                _ => return Err(self.fail(resp).await),
            };
            let Some(pin) = pin else {
                return Ok(media_body(resp, range));
            };
            // Pinned read (D24): the download's CTag/ETag against the expected cTag.
            let ctag = header_str(&resp, "ctag");
            let etag = header_str(&resp, "etag");
            return match ctag {
                Some(c) if bare_tag(&c) == bare_tag(pin) => Ok(media_body(resp, range)),
                Some(_) => Err(BErr::PreconditionFailed),
                None if etag
                    .as_deref()
                    .is_some_and(|e| bare_tag(e) == bare_tag(pin)) =>
                {
                    Ok(media_body(resp, range))
                }
                // No comparable tag: re-stat after the last byte (spec §5.6 fallback).
                None => Ok(post_checked(
                    media_body(resp, range),
                    self.clone(),
                    pid.to_string(),
                    pin.to_string(),
                )),
            };
        }
        Err(BErr::Transient(
            "onedrive: the download URL was refused twice".into(),
        ))
    }

    // ---- uploads ----

    /// The earlier attempt's result, when the target already holds exactly the staged
    /// bytes (same size and QuickXor; for New, the exact name).
    async fn landed(
        &self,
        to: UploadTarget<'_>,
        size: u64,
        local: &mut LocalHash,
    ) -> Result<Option<Item>, BErr> {
        let cur = match to {
            UploadTarget::New { parent, name } => self
                .get_child(parent, name)
                .await?
                .filter(|it| it.name() == name),
            UploadTarget::Replace { pid, .. } => self.get_item(pid).await?,
        };
        let Some(cur) = cur.filter(|c| !c.is_dir() && c.size == Some(size as i64)) else {
            return Ok(None);
        };
        match cur.quickxor() {
            Some(q) if quickxor::same_hash(q, &local.get().await?) => Ok(Some(cur)),
            _ => Ok(None),
        }
    }

    async fn simple_new(
        &self,
        parent: &str,
        name: &str,
        path: &Path,
        size: u64,
        local: &mut LocalHash,
    ) -> Result<Item, BErr> {
        let data = tokio::fs::read(path).await.map_err(io_err)?;
        let url = format!(
            "{}:/content?@microsoft.graph.conflictBehavior=fail",
            self.child_url(parent, name)
        );
        let c = Call::new(Method::PUT, url, units::UPLOAD).raw(Bytes::from(data));
        let resp = self.send_raw(&c).await?;
        if resp.status().is_success() {
            return Self::parse(resp).await;
        }
        match self.fail(resp).await {
            // A retry whose earlier attempt landed.
            BErr::NameConflict => self
                .landed(UploadTarget::New { parent, name }, size, local)
                .await?
                .ok_or(BErr::NameConflict),
            e => Err(e),
        }
    }

    /// An empty replace: sessions cannot carry zero bytes, so a preflight `if_cver`
    /// check, then a simple PUT that also sends `If-Match` (honoured or not, the
    /// remaining window is one round trip).
    async fn empty_replace(&self, pid: &str, if_cver: Option<&str>) -> Result<Item, BErr> {
        if let Some(want) = if_cver {
            let cur = self.get_item(pid).await?.ok_or(BErr::NotFound)?;
            if bare_tag(&cur.cver()) != bare_tag(want) {
                return Err(BErr::PreconditionFailed);
            }
        }
        let c = Call::new(
            Method::PUT,
            format!("{}/content", self.item_url(pid)),
            units::UPLOAD,
        )
        .if_match(if_cver)
        .raw(Bytes::new());
        self.json(c).await
    }

    async fn create_session(&self, to: UploadTarget<'_>) -> Result<String, BErr> {
        let c = match to {
            UploadTarget::New { parent, name } => Call::new(
                Method::POST,
                format!("{}:/createUploadSession", self.child_url(parent, name)),
                units::UPLOAD,
            )
            .json(json!({"item": {"@microsoft.graph.conflictBehavior": "fail", "name": name}})),
            UploadTarget::Replace { pid, if_cver } => Call::new(
                Method::POST,
                format!("{}/createUploadSession", self.item_url(pid)),
                units::UPLOAD,
            )
            .if_match(if_cver)
            .json(json!({"item": {"@microsoft.graph.conflictBehavior": "replace"}})),
        };
        let s: UploadSession = self.json(c).await?;
        s.upload_url
            .ok_or_else(|| BErr::Fatal("onedrive: upload session without an uploadUrl".into()))
    }

    async fn session_status(&self, url: &str, size: u64) -> Result<SessionState, BErr> {
        let c = Call::new(Method::GET, url.to_string(), units::FRAGMENT).pre_authenticated();
        let resp = self.send_raw(&c).await?;
        match resp.status() {
            s if s.is_success() => {
                let s: UploadSession = Self::parse(resp).await?;
                Ok(SessionState::At(s.next_offset().unwrap_or(size)))
            }
            StatusCode::NOT_FOUND | StatusCode::GONE => Ok(SessionState::Gone),
            _ => Err(self.fail(resp).await),
        }
    }

    async fn send_fragments(
        &self,
        url: &str,
        file: &Path,
        size: u64,
        mut offset: u64,
    ) -> Result<Item, FragErr> {
        let keep = |e: std::io::Error| FragErr::Keep(io_err(e));
        let mut f = tokio::fs::File::open(file).await.map_err(keep)?;
        let mut failures = 0u32;
        loop {
            if offset >= size {
                return Err(FragErr::Keep(BErr::Transient(
                    "onedrive: the upload session holds every byte but did not finish".into(),
                )));
            }
            let len = self.fragment.min(size - offset);
            let mut buf = vec![0u8; len as usize];
            f.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(keep)?;
            f.read_exact(&mut buf).await.map_err(keep)?;
            let c = Call::new(Method::PUT, url.to_string(), units::FRAGMENT)
                .pre_authenticated()
                .h(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", offset, offset + len - 1, size),
                )
                .raw(Bytes::from(buf));
            let uncertain = match self.send_raw(&c).await {
                Ok(resp) => match resp.status().as_u16() {
                    200 | 201 => return Self::parse(resp).await.map_err(FragErr::Keep),
                    202 => {
                        let s: UploadSession = Self::parse(resp).await.map_err(FragErr::Keep)?;
                        offset = s.next_offset().unwrap_or(offset + len);
                        failures = 0;
                        continue;
                    }
                    404 => return Err(FragErr::Gone),
                    // Already received: ask the session where it stands.
                    416 => BErr::Transient("onedrive: fragment already received".into()),
                    s if s == 408 || s >= 500 => match self.fail(resp).await {
                        e @ BErr::Throttled(_) => return Err(FragErr::Keep(e)),
                        e => e,
                    },
                    429 => return Err(FragErr::Keep(self.fail(resp).await)),
                    _ => {
                        return Err(match self.fail(resp).await {
                            e @ (BErr::Throttled(_)
                            | BErr::AuthExpired
                            | BErr::AuthRevoked
                            | BErr::StorageFull) => FragErr::Keep(e),
                            e => FragErr::Restart(e),
                        });
                    }
                },
                Err(e @ BErr::Transient(_)) => e,
                Err(e) => return Err(FragErr::Keep(e)),
            };
            failures += 1;
            tracing::debug!(%uncertain, failures, "onedrive: fragment outcome uncertain; asking the session");
            if failures > FRAGMENT_RETRIES {
                return Err(FragErr::Keep(uncertain));
            }
            tokio::time::sleep(Duration::from_millis(100 << failures)).await;
            match self.session_status(url, size).await {
                Ok(SessionState::At(o)) => offset = o,
                Ok(SessionState::Gone) => return Err(FragErr::Gone),
                Err(e) => return Err(FragErr::Keep(e)),
            }
        }
    }

    async fn session_upload(
        &self,
        to: UploadTarget<'_>,
        path: &Path,
        size: u64,
        st: &mut Resume,
        op: &mut OpCtx,
        local: &mut LocalHash,
    ) -> Result<Item, BErr> {
        let mut restarts = 0;
        loop {
            let (url, offset) = match st.url.clone() {
                Some(u) => match self.session_status(&u, size).await? {
                    SessionState::At(o) => (u, o),
                    SessionState::Gone => {
                        // Expired, or committed by the fragment whose answer was lost.
                        st.url = None;
                        st.save(op);
                        if let Some(it) = self.landed(to, size, local).await? {
                            return Ok(it);
                        }
                        restarts += 1;
                        if restarts > 1 {
                            return Err(BErr::Transient("onedrive: upload session expired".into()));
                        }
                        continue;
                    }
                },
                None => {
                    let u = match self.create_session(to).await {
                        Ok(u) => u,
                        Err(e @ (BErr::PreconditionFailed | BErr::NameConflict)) => {
                            return match self.landed(to, size, local).await? {
                                Some(it) => Ok(it),
                                None => Err(e),
                            };
                        }
                        Err(e) => return Err(e),
                    };
                    st.url = Some(u.clone());
                    st.size = size;
                    st.save(op);
                    (u, 0)
                }
            };
            match self.send_fragments(&url, path, size, offset).await {
                Ok(it) => {
                    st.url = None;
                    st.save(op);
                    return Ok(it);
                }
                Err(FragErr::Keep(e)) => return Err(e),
                Err(FragErr::Restart(e)) => {
                    st.url = None;
                    st.save(op);
                    return Err(e);
                }
                Err(FragErr::Gone) => {
                    st.url = None;
                    st.save(op);
                    if let Some(it) = self.landed(to, size, local).await? {
                        return Ok(it);
                    }
                    restarts += 1;
                    if restarts > 1 {
                        return Err(BErr::Transient("onedrive: upload session expired".into()));
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
        let UploadSource::File { path, size, .. } = src;
        if let UploadTarget::New { name, .. } = to {
            check_name(name)?;
        }
        let mut local = LocalHash {
            path: path.to_path_buf(),
            value: None,
        };
        let mut st = Resume::load(op);
        if st.url.is_some() && st.size != size {
            st = Resume::default();
            st.save(op);
        }
        let it = match to {
            UploadTarget::New { parent, name } if st.url.is_none() && size <= SIMPLE_UPLOAD_MAX => {
                self.simple_new(parent, name, path, size, &mut local)
                    .await?
            }
            UploadTarget::Replace { pid, if_cver } if size == 0 => {
                self.empty_replace(pid, if_cver).await?
            }
            _ => {
                self.session_upload(to, path, size, &mut st, op, &mut local)
                    .await?
            }
        };
        // The service's hash of what it stored must be the staged bytes'.
        if let Some(q) = it.quickxor()
            && !quickxor::same_hash(q, &local.get().await?)
        {
            return Err(BErr::Fatal(format!(
                "onedrive: stored bytes of {} do not match the upload (quickXorHash)",
                it.id
            )));
        }
        Ok(self.node(&it))
    }

    // ---- copy ----

    async fn copy(&self, pid: &str, parent: &str, name: &str) -> Result<RNode, BErr> {
        check_name(name)?;
        let c = Call::new(
            Method::POST,
            format!(
                "{}/copy?@microsoft.graph.conflictBehavior=fail",
                self.item_url(pid)
            ),
            units::CREATE,
        )
        .json(json!({"parentReference": {"id": parent}, "name": name}));
        let resp = self.send_ok(&c).await?;
        if resp.status() != StatusCode::ACCEPTED {
            let it: Item = Self::parse(resp).await?;
            return Ok(self.node(&it));
        }
        let monitor = header_str(&resp, "location")
            .ok_or_else(|| BErr::Fatal("onedrive: copy accepted without a monitor".into()))?;
        let deadline = Instant::now() + self.copy_timeout;
        loop {
            // The monitor URL is pre-authenticated.
            let m = Call::new(Method::GET, monitor.clone(), 0).pre_authenticated();
            let r = self.send_raw(&m).await?;
            let status = r.status();
            if status.is_redirection() {
                let loc = header_str(&r, "location").unwrap_or_default();
                let id = loc
                    .rsplit_once("/items/")
                    .map(|(_, id)| id.split(['?', '/']).next().unwrap_or(id).to_string())
                    .ok_or_else(|| {
                        BErr::Fatal(format!(
                            "onedrive: copy finished at an unknown place: {loc}"
                        ))
                    })?;
                return self.copied(&id).await;
            }
            if status == StatusCode::NOT_FOUND {
                return Err(BErr::Transient("onedrive: copy monitor gone".into()));
            }
            if !status.is_success() {
                return Err(self.fail(r).await);
            }
            let v: serde_json::Value = Self::parse(r).await?;
            if v.get("status").is_none()
                && let Ok(it) = serde_json::from_value::<Item>(v.clone())
            {
                return Ok(self.node(&it));
            }
            let mon: api::Monitor = serde_json::from_value(v)
                .map_err(|e| BErr::Fatal(format!("onedrive: bad copy monitor: {e}")))?;
            match mon.status.as_deref() {
                Some("completed") => {
                    let id = mon.resource_id.ok_or_else(|| {
                        BErr::Fatal("onedrive: copy completed without a resourceId".into())
                    })?;
                    return self.copied(&id).await;
                }
                Some("failed") => {
                    let e = mon.error.unwrap_or_default();
                    return Err(match e.code.as_str() {
                        "nameAlreadyExists" => BErr::NameConflict,
                        "itemNotFound" => BErr::NotFound,
                        "quotaLimitReached" | "insufficientStorage" => BErr::StorageFull,
                        _ => {
                            BErr::Fatal(format!("onedrive: copy failed: {} {}", e.code, e.message))
                        }
                    });
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(BErr::Transient("onedrive: copy still running".into()));
            }
            tokio::time::sleep(self.copy_poll).await;
        }
    }

    async fn copied(&self, id: &str) -> Result<RNode, BErr> {
        let it = self
            .get_item(id)
            .await?
            .ok_or_else(|| BErr::Transient("onedrive: the copy is not visible yet".into()))?;
        Ok(self.node(&it))
    }
}

/// After the last byte, re-stat the item: a different cTag means the bytes were not the
/// pinned version, and the stream ends in `PreconditionFailed` (wrapped in the
/// `io::Error`) instead of a clean end.
fn post_checked(inner: RemoteBody, be: Arc<Inner>, pid: String, pin: String) -> RemoteBody {
    let s = futures_util::stream::unfold(Some((inner, be, pid, pin)), |st| async move {
        let (mut inner, be, pid, pin) = st?;
        match inner.next().await {
            Some(Ok(b)) => Some((Ok(b), Some((inner, be, pid, pin)))),
            Some(Err(e)) => Some((Err(e), None)),
            None => match be.get_item(&pid).await {
                Ok(Some(it)) if it.c_tag.as_deref().map(bare_tag) == Some(bare_tag(&pin)) => None,
                Ok(_) => Some((Err(std::io::Error::other(BErr::PreconditionFailed)), None)),
                Err(e) => Some((Err(std::io::Error::other(e)), None)),
            },
        }
    });
    Box::pin(s)
}

impl Backend for OneDriveBackend {
    /// 8 req/s, 2 transfers.
    fn limits(&self) -> crate::governor::Limits {
        crate::governor::Limits::onedrive()
    }

    /// OneDrive names are case-insensitive (no two siblings differ only in case) and
    /// case-preserving; `BackendCaps` has no field for it, and the index compares names
    /// byte-exact anyway.
    fn caps(&self) -> BackendCaps {
        BackendCaps {
            cas_create: true,
            cas_overwrite: true,
            atomic_replace: true,
            server_copy: true,
            range: true,
            pinned_read: true,
            push: false,
            hash_kind: Some(HashKind::QuickXor),
            mtime_granularity_ns: 1_000_000_000,
            simple_upload_max: SIMPLE_UPLOAD_MAX,
            chunk_multiple: FRAGMENT_MULTIPLE,
            path_ids: false,
        }
    }

    async fn account(&self) -> Result<AccountInfo, BErr> {
        let d: api::Drive = self
            .0
            .json(Call::new(Method::GET, self.0.url("/me/drive"), units::GET))
            .await?;
        let user = d.owner.and_then(|o| o.user).unwrap_or_default();
        Ok(AccountInfo {
            id: user.id.or(d.id).unwrap_or_default(),
            label: user.email.or(user.display_name).unwrap_or_default(),
        })
    }

    /// `/me/drive/root:/{name}` (the folder the reader uses), or nothing.
    async fn locate_roots(&self, name: &str) -> Result<Vec<RNode>, BErr> {
        check_name(name)?;
        let url = self.0.url(&format!("/me/drive/root:/{}", seg(name)));
        let Some(it) = self.0.get_opt(url).await? else {
            return Ok(Vec::new());
        };
        if !it.is_dir() || it.deleted.is_some() {
            return Ok(Vec::new());
        }
        let n = self.0.node(&it);
        let mut t = self.0.tree.lock();
        t.roots.insert(n.pid.clone());
        // Its parent is the drive root by construction.
        if let Some(p) = &n.parent {
            t.dirs.insert(p.clone(), None);
        }
        Ok(vec![n])
    }

    async fn create_root(&self, name: &str) -> Result<RNode, BErr> {
        check_name(name)?;
        let c = Call::new(
            Method::POST,
            self.0.url("/me/drive/root/children"),
            units::CREATE,
        )
        .json(json!({"name": name, "folder": {}, "@microsoft.graph.conflictBehavior": "fail"}));
        let it: Item = self.0.json(c).await?;
        let n = self.0.node(&it);
        let mut t = self.0.tree.lock();
        t.roots.insert(n.pid.clone());
        if let Some(p) = &n.parent {
            t.dirs.insert(p.clone(), None);
        }
        Ok(n)
    }

    /// `root/delta?token=latest`: the current head, with no enumeration.
    async fn start_cursor(&self) -> Result<Cursor, BErr> {
        let url = self.0.url("/me/drive/root/delta?token=latest");
        let units = api::delta_units(&url);
        let page: Page = self.0.json(Call::new(Method::GET, url, units)).await?;
        let link = page.delta_link.ok_or_else(|| {
            BErr::Fatal("onedrive: delta?token=latest without a deltaLink".into())
        })?;
        self.0.own_link(&link)?;
        Ok(Cursor(link))
    }

    /// Walks each root's `children`, folder by folder (parents before children).
    async fn list_all(
        &self,
        roots: &[Pid],
        sink: &mut (dyn FnMut(RNode) + Send),
    ) -> Result<(), BErr> {
        self.0.tree.lock().roots.extend(roots.iter().cloned());
        let mut seen: HashSet<Pid> = roots.iter().cloned().collect();
        let mut queue: VecDeque<Pid> = roots.iter().cloned().collect();
        while let Some(dir) = queue.pop_front() {
            let mut kids = Vec::new();
            self.0.children(&dir, &mut |it| kids.push(it)).await?;
            for it in kids {
                if !seen.insert(it.id.clone()) {
                    continue;
                }
                if it.is_dir() {
                    queue.push_back(it.id.clone());
                }
                sink(it.to_rnode(Some(dir.clone())));
            }
        }
        Ok(())
    }

    async fn list_dir(&self, dir: &str, sink: &mut (dyn FnMut(RNode) + Send)) -> Result<(), BErr> {
        let d = dir.to_string();
        self.0
            .children(dir, &mut |it| sink(it.to_rnode(Some(d.clone()))))
            .await
    }

    async fn find_named(&self, parent: &str, name: &str) -> Result<(Vec<RNode>, ServerTime), BErr> {
        check_name(name)?;
        let c = Call::new(Method::GET, self.0.child_url(parent, name), units::GET);
        let resp = self.0.send_raw(&c).await?;
        let at = server_time(&resp);
        let it: Item = match resp.status() {
            s if s.is_success() => Inner::parse(resp).await?,
            StatusCode::NOT_FOUND => return Ok((Vec::new(), at)),
            _ => return Err(self.0.fail(resp).await),
        };
        // Graph resolves the path case-insensitively; only the exact name is asked for.
        if it.deleted.is_some() || it.name() != name {
            return Ok((Vec::new(), at));
        }
        self.0.note(&it);
        Ok((vec![it.to_rnode(Some(parent.to_string()))], at))
    }

    /// One page of root `delta`, filtered to the roots' subtrees (module docs).
    async fn changes(&self, c: &Cursor) -> Result<ChangePage, BErr> {
        if self.0.tree.lock().roots.is_empty() {
            return Err(BErr::Fatal(
                "onedrive: changes before locate_roots (the feed is filtered to the roots)".into(),
            ));
        }
        self.0.own_link(&c.0)?;
        let call = Call::new(Method::GET, c.0.clone(), api::delta_units(&c.0));
        let resp = self.0.send_raw(&call).await?;
        match resp.status() {
            s if s.is_success() => {}
            StatusCode::GONE => {
                let (e, _) = Inner::api_error(resp).await;
                tracing::info!(codes = ?e.codes, "onedrive: delta token lost (410)");
                return Ok(ChangePage::CursorLost);
            }
            _ => return Err(self.0.fail(resp).await),
        }
        let page: Page = Inner::parse(resp).await?;
        let mut items = Vec::with_capacity(page.value.len());
        for it in page.value {
            if it.deleted.is_some() {
                self.0.tree.lock().dirs.remove(&it.id);
                items.push(ChangeItem::Removed { pid: it.id });
                continue;
            }
            if it.root.is_some() {
                self.0.note(&it);
                continue;
            }
            self.0.note(&it);
            let parent = it.parent_id();
            let is_root = self.0.tree.lock().roots.contains(&it.id);
            if is_root || self.0.in_subtree(parent.as_deref()).await? {
                items.push(ChangeItem::Upsert(it.to_rnode(parent)));
            } else {
                // Outside: never surfaced; an opaque id tells the index it left (if it
                // was ever in).
                items.push(ChangeItem::Removed { pid: it.id });
            }
        }
        match (page.next_link, page.delta_link) {
            (Some(n), _) => {
                self.0.own_link(&n)?;
                Ok(ChangePage::Page {
                    items,
                    next: Cursor(n),
                    more: true,
                })
            }
            (None, Some(d)) => {
                self.0.own_link(&d)?;
                Ok(ChangePage::Page {
                    items,
                    next: Cursor(d),
                    more: false,
                })
            }
            (None, None) => Err(BErr::Fatal("onedrive: delta page without a link".into())),
        }
    }

    async fn wait_change(&self, _c: &Cursor, _max: Duration) -> Result<(), BErr> {
        Err(BErr::Unsupported)
    }

    async fn stat(&self, pid: &str) -> Result<Option<RNode>, BErr> {
        Ok(self.0.get_item(pid).await?.map(|it| self.0.node(&it)))
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
        check_name(name)?;
        let c = Call::new(
            Method::POST,
            format!("{}/children", self.0.item_url(parent)),
            units::CREATE,
        )
        .json(json!({"name": name, "folder": {}, "@microsoft.graph.conflictBehavior": "fail"}));
        let it: Item = self.0.json(c).await?;
        Ok(self.0.node(&it))
    }

    async fn update(
        &self,
        pid: &str,
        parent: Option<&str>,
        name: Option<&str>,
        if_cver: Option<&str>,
    ) -> Result<RNode, BErr> {
        if let Some(n) = name {
            check_name(n)?;
        }
        let mut body = serde_json::Map::new();
        if let Some(n) = name {
            body.insert("name".into(), n.into());
        }
        if let Some(p) = parent {
            body.insert("parentReference".into(), json!({"id": p}));
        }
        let c = Call::new(Method::PATCH, self.0.item_url(pid), units::UPDATE)
            .if_match(if_cver)
            .json(serde_json::Value::Object(body));
        let it: Item = self.0.json(c).await?;
        Ok(self.0.node(&it))
    }

    /// `POST /items/{id}/copy`, then the monitor is polled until the copy exists.
    async fn copy(&self, pid: &str, parent: &str, name: &str) -> Result<RNode, BErr> {
        self.0.copy(pid, parent, name).await
    }

    /// DELETE moves the item to the recycle bin.
    async fn trash(&self, pid: &str, if_cver: Option<&str>) -> Result<(), BErr> {
        let c = Call::new(Method::DELETE, self.0.item_url(pid), units::DELETE).if_match(if_cver);
        self.0.send_ok(&c).await?;
        self.0.tree.lock().dirs.remove(pid);
        Ok(())
    }

    /// `permanentDelete` where the account has it (work or school); otherwise a DELETE,
    /// which leaves the item in the recycle bin.
    async fn delete_permanent(&self, pid: &str) -> Result<(), BErr> {
        if !self.0.no_permanent_delete.load(Ordering::Relaxed) {
            let c = Call::new(
                Method::POST,
                format!("{}/permanentDelete", self.0.item_url(pid)),
                units::DELETE,
            );
            let resp = self.0.send_raw(&c).await?;
            let status = resp.status();
            if status.is_success() {
                self.0.tree.lock().dirs.remove(pid);
                return Ok(());
            }
            let (e, ra) = Inner::api_error(resp).await;
            match status.as_u16() {
                400 | 405 | 501 => {
                    tracing::info!(codes = ?e.codes, "onedrive: permanentDelete unsupported; deleting to the recycle bin");
                    self.0.no_permanent_delete.store(true, Ordering::Relaxed);
                }
                // Unknown endpoint or unknown item: the DELETE below tells which.
                404 => {}
                _ if e.has("notSupported") => {
                    self.0.no_permanent_delete.store(true, Ordering::Relaxed);
                }
                _ => return Err(self.0.classify(&e, ra)),
            }
        }
        self.trash(pid, None).await
    }

    async fn quota(&self) -> Result<Option<bunko_store::Capacity>, BErr> {
        let d: api::Drive = self
            .0
            .json(Call::new(Method::GET, self.0.url("/me/drive"), units::GET))
            .await?;
        Ok(d.quota.and_then(|q| {
            let total = q.total?.max(0) as u64;
            let available = match q.remaining {
                Some(r) => r.max(0) as u64,
                None => total.saturating_sub(q.used.unwrap_or(0).max(0) as u64),
            };
            Some(bunko_store::Capacity { total, available })
        }))
    }
}
