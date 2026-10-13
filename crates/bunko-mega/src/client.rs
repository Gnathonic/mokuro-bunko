//! The HTTP layer (new; mega-rs's `http/reqwest.rs` replaced): reqwest 0.12 with rustls
//! from the workspace, `cs` batches with pacing, MEGA's `X-Hashcash` challenge,
//! EAGAIN/server-busy retries with backoff, the `sc` endpoint, and a transfer
//! semaphore.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, DATE, HeaderMap, RETRY_AFTER};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::error::{Error, ErrorCode, Result};
use crate::hashcash;
use crate::protocol::Request;
use crate::session::Session;

/// MEGA's API origin.
pub const DEFAULT_API_ORIGIN: &str = "https://g.api.mega.co.nz/";

/// Client settings.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// The API origin (a mock server in tests), with or without a trailing `/`.
    pub api_origin: String,
    /// At most one `cs` batch per this interval (spec §5.11: 1 batch/s).
    pub min_cs_interval: Duration,
    /// Retries of EAGAIN, server-busy and network failures.
    pub max_retries: u32,
    /// The first retry delay; doubles per retry up to `retry_max`.
    pub retry_base: Duration,
    pub retry_max: Duration,
    /// Ask for `https://` transfer URLs (`ssl=2`).
    pub https_transfers: bool,
    /// Concurrent uploads/downloads (spec §5.11: 2).
    pub max_transfers: usize,
    /// Idle read timeout of a request or transfer.
    pub read_timeout: Duration,
    pub user_agent: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            api_origin: DEFAULT_API_ORIGIN.to_string(),
            min_cs_interval: Duration::from_secs(1),
            max_retries: 6,
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(32),
            https_transfers: true,
            max_transfers: 2,
            read_timeout: Duration::from_secs(120),
            user_agent: concat!("mokuro-bunko/", env!("CARGO_PKG_VERSION")).to_string(),
        }
    }
}

/// A reply of the `sc` endpoint. `Debug` leaves the wait URL out (it can carry a
/// token).
#[derive(Clone, PartialEq)]
pub enum ScReply {
    /// Action packets after the requested `sn`, and the `sn` that follows them.
    Packets { packets: Vec<Value>, sn: String },
    /// Nothing new: long-poll this URL, then ask again with the same `sn`.
    Wait { url: String },
}

impl std::fmt::Debug for ScReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScReply::Packets { packets, sn } => f
                .debug_struct("Packets")
                .field("packets", &packets.len())
                .field("sn", sn)
                .finish(),
            ScReply::Wait { .. } => f.write_str("Wait { url: <redacted> }"),
        }
    }
}

/// A MEGA API client. Cheap to clone; the session (if any) is fixed at construction.
#[derive(Clone)]
pub struct Client(pub(crate) Arc<Inner>);

pub(crate) struct Inner {
    pub(crate) http: reqwest::Client,
    pub(crate) cfg: ClientConfig,
    origin: String,
    pub(crate) session: Option<Session>,
    seq: AtomicU64,
    next_cs: tokio::sync::Mutex<Option<Instant>>,
    transfers: Arc<Semaphore>,
    clock: std::sync::Mutex<Option<(i64, Instant)>>,
}

enum Sink<'a> {
    Memory,
    File(&'a mut tokio::fs::File),
}

fn retry_after(h: &HeaderMap) -> Option<Duration> {
    h.get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The JSON type of `v`, for error messages that must not quote a body.
fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// A top-level error code (`-3`), as opposed to a JSON array or object.
fn bare_code(body: &[u8]) -> Option<ErrorCode> {
    let s = std::str::from_utf8(body).ok()?.trim();
    if s.starts_with('-') || s.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
        s.parse::<i64>().ok().map(ErrorCode::from_i64)
    } else {
        None
    }
}

impl Client {
    /// A client without a session (for login), or with one.
    pub fn new(cfg: ClientConfig, session: Option<Session>) -> Result<Client> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(cfg.read_timeout)
            .user_agent(cfg.user_agent.clone())
            .build()
            .map_err(|e| Error::Network(format!("http client: {e}")))?;
        let mut origin = cfg.api_origin.clone();
        if !origin.ends_with('/') {
            origin.push('/');
        }
        let seed: u32 = rand::random();
        Ok(Client(Arc::new(Inner {
            http,
            transfers: Arc::new(Semaphore::new(cfg.max_transfers.max(1))),
            cfg,
            origin,
            session,
            seq: AtomicU64::new(u64::from(seed)),
            next_cs: tokio::sync::Mutex::new(None),
            clock: std::sync::Mutex::new(None),
        })))
    }

    /// The same settings with `session`.
    pub fn with_session(&self, session: Session) -> Result<Client> {
        Client::new(self.0.cfg.clone(), Some(session))
    }

    pub fn session(&self) -> Option<&Session> {
        self.0.session.as_ref()
    }

    pub(crate) fn session_or_err(&self) -> Result<&Session> {
        self.0
            .session
            .as_ref()
            .ok_or_else(|| Error::InvalidSession("not logged in".into()))
    }

    pub fn config(&self) -> &ClientConfig {
        &self.0.cfg
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.0.http
    }

    pub(crate) fn ssl(&self) -> i32 {
        if self.0.cfg.https_transfers { 2 } else { 0 }
    }

    /// The server's clock (from the last `Date` header), unix ms; the local clock
    /// before any response.
    pub fn server_now_ms(&self) -> i64 {
        match *self.0.clock.lock().unwrap_or_else(|e| e.into_inner()) {
            Some((ms, at)) => ms + at.elapsed().as_millis() as i64,
            None => now_ms(),
        }
    }

    pub(crate) fn note_date(&self, h: &HeaderMap) {
        let Some(t) = h
            .get(DATE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| httpdate::parse_http_date(v).ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        else {
            return;
        };
        *self.0.clock.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((t.as_millis() as i64, Instant::now()));
    }

    /// Wait for a transfer slot (held until the permit drops).
    pub async fn transfer_permit(&self) -> OwnedSemaphorePermit {
        // The semaphore is never closed.
        self.0
            .transfers
            .clone()
            .acquire_owned()
            .await
            .expect("transfer semaphore is never closed")
    }

    async fn pace(&self) {
        let every = self.0.cfg.min_cs_interval;
        if every.is_zero() {
            return;
        }
        let mut next = self.0.next_cs.lock().await;
        if let Some(t) = *next
            && t > Instant::now()
        {
            tokio::time::sleep_until(t).await;
        }
        *next = Some(Instant::now() + every);
    }

    pub(crate) fn backoff(&self, attempt: u32) -> Duration {
        let base = self.0.cfg.retry_base.as_millis() as u64;
        let d = base.saturating_mul(1u64 << attempt.min(16));
        let jitter = if base == 0 {
            0
        } else {
            rand::random::<u64>() % (base / 4 + 1)
        };
        Duration::from_millis(d.min(self.0.cfg.retry_max.as_millis() as u64) + jitter)
    }

    fn query(&self, id: u64, extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut q = vec![("id".to_string(), id.to_string())];
        if let Some(s) = &self.0.session {
            q.push(("sid".into(), s.sid().to_string()));
        }
        q.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        q
    }

    /// Send one `cs` batch; the body lands in memory or in `sink`.
    async fn cs_raw(
        &self,
        reqs: &[Value],
        extra: &[(&str, &str)],
        mut sink: Sink<'_>,
    ) -> Result<Bytes> {
        // One id for every retry of this batch, so MEGA can recognise a retransmission.
        let id = self.0.seq.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::to_vec(reqs)?;
        let q = self.query(id, extra);
        let url = format!("{}cs", self.0.origin);
        let mut answer: Option<String> = None;
        let mut challenges = 0u32;
        let mut attempt = 0u32;
        loop {
            self.pace().await;
            let mut rb = self
                .0
                .http
                .post(&url)
                .query(&q)
                .header(CONTENT_TYPE, "application/json")
                .body(body.clone());
            if let Some(a) = &answer {
                rb = rb.header("X-Hashcash", a);
            }
            let resp = match rb.send().await {
                Ok(r) => r,
                Err(e) if attempt < self.0.cfg.max_retries => {
                    tracing::debug!(error = %Error::from(e), attempt, "mega: cs request failed, retrying");
                    tokio::time::sleep(self.backoff(attempt)).await;
                    attempt += 1;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            self.note_date(resp.headers());
            if let Some(ch) = resp.headers().get("x-hashcash") {
                let ch = ch
                    .to_str()
                    .map_err(|_| Error::Hashcash("non-ASCII challenge".into()))?
                    .to_string();
                challenges += 1;
                if challenges > 3 {
                    return Err(Error::Hashcash("challenge repeated after answers".into()));
                }
                tracing::info!(challenge = %ch, "mega: answering a hashcash challenge");
                let solved = tokio::task::spawn_blocking(move || hashcash::solve(&ch))
                    .await
                    .map_err(|e| Error::Hashcash(format!("solver: {e}")))??;
                answer = Some(solved);
                continue;
            }
            let status = resp.status();
            if matches!(
                status,
                StatusCode::INTERNAL_SERVER_ERROR
                    | StatusCode::BAD_GATEWAY
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT
            ) && attempt < self.0.cfg.max_retries
            {
                // "Server Too Busy" is answered as EAGAIN (megajs).
                tokio::time::sleep(self.backoff(attempt)).await;
                attempt += 1;
                continue;
            }
            if !status.is_success() {
                return Err(Error::Http {
                    status: status.as_u16(),
                    retry_after: retry_after(resp.headers()),
                });
            }
            let (head, bytes) = match &mut sink {
                Sink::Memory => {
                    let b = match resp.bytes().await {
                        Ok(b) => b,
                        Err(e) if attempt < self.0.cfg.max_retries => {
                            tracing::debug!(error = %Error::from(e), "mega: cs body failed, retrying");
                            tokio::time::sleep(self.backoff(attempt)).await;
                            attempt += 1;
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    };
                    (b.clone(), b)
                }
                Sink::File(f) => match spool(f, resp).await {
                    Ok(head) => (head, Bytes::new()),
                    Err(Error::Network(e)) if attempt < self.0.cfg.max_retries => {
                        // Already URL-free (From<reqwest::Error>).
                        tracing::debug!(error = %e, "mega: cs body failed, retrying");
                        tokio::time::sleep(self.backoff(attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                },
            };
            match bare_code(&head) {
                Some(ErrorCode::EAGAIN) if attempt < self.0.cfg.max_retries => {
                    tokio::time::sleep(self.backoff(attempt)).await;
                    attempt += 1;
                    continue;
                }
                Some(code) => return Err(Error::Api(code)),
                None => return Ok(bytes),
            }
        }
    }

    /// One `cs` batch. Each command answers a value or its own error code. When every
    /// command answered EAGAIN the batch is retried (nothing was altered).
    pub async fn cs_batch(
        &self,
        reqs: &[Request],
        extra: &[(&str, &str)],
    ) -> Result<Vec<std::result::Result<Value, ErrorCode>>> {
        let values: Vec<Value> = reqs.iter().map(Request::to_value).collect();
        let mut attempt = 0u32;
        loop {
            let body = self.cs_raw(&values, extra, Sink::Memory).await?;
            let parsed: Value = serde_json::from_slice(&body)?;
            let Value::Array(items) = parsed else {
                return Err(Error::Protocol(format!(
                    "cs answered a {}",
                    kind_of(&parsed)
                )));
            };
            if items.len() != values.len() {
                return Err(Error::Protocol(format!(
                    "{} answers to {} commands",
                    items.len(),
                    values.len()
                )));
            }
            let out: Vec<_> = items
                .into_iter()
                .map(|v| match ErrorCode::from_json(&v) {
                    Some(ErrorCode::OK) => Ok(Value::Null),
                    Some(code) if v.as_i64().is_some_and(|n| n < 0) => Err(code),
                    _ => Ok(v),
                })
                .collect();
            let all_again = out.iter().all(|r| matches!(r, Err(ErrorCode::EAGAIN)));
            if all_again && attempt < self.0.cfg.max_retries {
                tokio::time::sleep(self.backoff(attempt)).await;
                attempt += 1;
                continue;
            }
            return Ok(out);
        }
    }

    /// One command; its error code becomes `Error::Api`.
    pub async fn cs1(&self, req: Request) -> Result<Value> {
        self.cs1_with(req, &[]).await
    }

    pub(crate) async fn cs1_with(&self, req: Request, extra: &[(&str, &str)]) -> Result<Value> {
        let mut out = self.cs_batch(std::slice::from_ref(&req), extra).await?;
        match out.pop() {
            Some(Ok(v)) => Ok(v),
            Some(Err(code)) => Err(Error::Api(code)),
            None => Err(Error::Protocol("empty cs answer".into())),
        }
    }

    /// One command whose (possibly large) answer is spooled to `file` from offset 0.
    pub(crate) async fn cs1_to_file(&self, req: Request, file: &mut tokio::fs::File) -> Result<()> {
        self.cs_raw(&[req.to_value()], &[], Sink::File(file))
            .await
            .map(|_| ())
    }

    /// Ask `sc` for action packets after `sn`.
    pub async fn sc(&self, sn: &str) -> Result<ScReply> {
        let url = format!("{}sc", self.0.origin);
        let mut q = vec![
            ("sn".to_string(), sn.to_string()),
            ("ssl".into(), "1".into()),
        ];
        if let Some(s) = &self.0.session {
            q.push(("sid".into(), s.sid().to_string()));
        }
        let mut attempt = 0u32;
        loop {
            let res = self.0.http.post(&url).query(&q).send().await;
            let resp = match res {
                Ok(r) => r,
                Err(e) if attempt < self.0.cfg.max_retries => {
                    tracing::debug!(error = %Error::from(e), "mega: sc failed, retrying");
                    tokio::time::sleep(self.backoff(attempt)).await;
                    attempt += 1;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            self.note_date(resp.headers());
            let status = resp.status();
            if status.is_server_error() && attempt < self.0.cfg.max_retries {
                tokio::time::sleep(self.backoff(attempt)).await;
                attempt += 1;
                continue;
            }
            if !status.is_success() {
                return Err(Error::Http {
                    status: status.as_u16(),
                    retry_after: retry_after(resp.headers()),
                });
            }
            let body = resp.bytes().await?;
            match bare_code(&body) {
                Some(ErrorCode::EAGAIN) if attempt < self.0.cfg.max_retries => {
                    tokio::time::sleep(self.backoff(attempt)).await;
                    attempt += 1;
                    continue;
                }
                Some(code) => return Err(Error::Api(code)),
                None => {}
            }
            let v: Value = serde_json::from_slice(&body)?;
            if let Some(w) = v.get("w").and_then(Value::as_str) {
                return Ok(ScReply::Wait { url: w.to_string() });
            }
            let sn = v
                .get("sn")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    Error::Protocol(format!("sc answered a {} without sn", kind_of(&v)))
                })?
                .to_string();
            let packets = match v.get("a") {
                Some(Value::Array(a)) => a.clone(),
                None | Some(Value::Null) => Vec::new(),
                Some(other) => {
                    return Err(Error::Protocol(format!(
                        "sc packets are a {}",
                        kind_of(other)
                    )));
                }
            };
            return Ok(ScReply::Packets { packets, sn });
        }
    }

    /// Long-poll a wait URL from [`ScReply::Wait`] for at most `max`. Returns when MEGA
    /// answers (something changed), on timeout, or after a short pause on error.
    pub async fn sc_wait(&self, url: &str, max: Duration) -> Result<()> {
        let started = Instant::now();
        match tokio::time::timeout(max, self.0.http.post(url).send()).await {
            Ok(Ok(resp)) => {
                let _ = resp.bytes().await;
            }
            Ok(Err(e)) => {
                tracing::debug!(error = %Error::from(e), "mega: sc wait failed");
                let rest = max.saturating_sub(started.elapsed());
                tokio::time::sleep(rest.min(Duration::from_secs(1))).await;
            }
            Err(_) => {}
        }
        Ok(())
    }
}

/// Stream a response body into `f` (truncated first); returns its first bytes.
async fn spool(f: &mut tokio::fs::File, resp: reqwest::Response) -> Result<Bytes> {
    f.set_len(0).await?;
    f.seek(std::io::SeekFrom::Start(0)).await?;
    let mut body = resp.bytes_stream();
    while let Some(chunk) = body.next().await {
        f.write_all(&chunk?).await?;
    }
    f.flush().await?;
    f.seek(std::io::SeekFrom::Start(0)).await?;
    let mut head = vec![0u8; 32];
    let mut n = 0;
    while n < head.len() {
        let k = f.read(&mut head[n..]).await?;
        if k == 0 {
            break;
        }
        n += k;
    }
    head.truncate(n);
    f.seek(std::io::SeekFrom::Start(0)).await?;
    Ok(Bytes::from(head))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_of_urls_is_redacted() {
        let secret = "https://gfs.example/dl/TOKENsecret123";
        let reply = ScReply::Wait { url: secret.into() };
        let target = crate::DownloadTarget {
            url: secret.into(),
            size: 9,
        };
        let poll = crate::Poll::Idle {
            wait_url: secret.into(),
        };
        for s in [
            format!("{reply:?}"),
            format!("{target:?}"),
            format!("{poll:?}"),
        ] {
            assert!(!s.contains("TOKENsecret123"), "{s}");
        }
    }

    #[test]
    fn bare_codes() {
        assert_eq!(bare_code(b"-3"), Some(ErrorCode::EAGAIN));
        assert_eq!(bare_code(b" -15\n"), Some(ErrorCode::ESID));
        assert_eq!(bare_code(b"[-9]"), None);
        assert_eq!(bare_code(b"{\"a\":1}"), None);
        assert_eq!(bare_code(b"[{\"f\":[]"), None);
    }
}
