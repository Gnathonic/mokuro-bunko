//! Whole archives off the library: downloaded, resumed, verified, held in the spool
//! (spec remote-processors §8.3, 0.5.2 `ArchiveFetcher`).
//!
//! An archive arrives whole, at full network speed, before the pipeline sees a byte of
//! it. A broken download resumes from the byte it reached (`Range` + `If-Range`
//! against the strong `ETag`, so a file replaced mid-download starts over rather than
//! splicing). A copy that fails its zip CRCs is diagnosed with a second full
//! download: corrupted in transit (the second is used), or damaged at the library
//! (the same bytes twice: delivered as it is, and the pipeline decides exactly as it
//! would locally). Anything that cannot be delivered goes back to the library with a
//! class ([`TransferFault`]) — never as a failure of the volume.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::StatusCode;
use http::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, RANGE, RETRY_AFTER,
};
use serde_json::{Value, json};

use crate::client::{Credentials, describe_reqwest, quote_path};
use crate::pipeline::CancelToken;
use crate::spool::{ArchiveSpool, MIB, Placement, PlacementKind, SpoolError};
use crate::verify::{Verified, VerifyCancelled, describe_damaged, verify_archive};

/// How patient a download is. Tests shrink these; nothing else should.
#[derive(Debug, Clone, PartialEq)]
pub struct FetchTiming {
    pub connect_timeout: Duration,
    /// Silence on one request (headers or body) before it is given up and resumed.
    pub read_timeout: Duration,
    /// Waits between failed requests; the last repeats; back to the first after any
    /// attempt that received new bytes.
    pub retry_delays: Vec<Duration>,
    /// Silence, not slowness: no new byte for this long across every attempt → the
    /// claim goes back as `stalled`.
    pub stall: Duration,
    pub max_restarts: u32,
    /// The longest `Retry-After` honoured.
    pub retry_after_cap: Duration,
    /// No progress is offered for a download shorter than this...
    pub progress_after: Duration,
    /// ...and at most this often while one runs.
    pub progress_every: Duration,
}

impl Default for FetchTiming {
    fn default() -> Self {
        FetchTiming {
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(30),
            retry_delays: [1, 2, 4, 8, 15, 30]
                .into_iter()
                .map(Duration::from_secs)
                .collect(),
            stall: Duration::from_secs(120),
            max_restarts: 3,
            retry_after_cap: Duration::from_secs(30),
            progress_after: Duration::from_secs(3),
            progress_every: Duration::from_millis(500),
        }
    }
}

/// An archive this processor could not deliver, and why — by class
/// (`bunko_proto::return_class`). Never a failure of the volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferFault {
    pub kind: &'static str,
    pub message: String,
    pub status: Option<u16>,
    pub received: u64,
    pub total: Option<u64>,
    pub requests: u32,
}

impl TransferFault {
    pub fn new(kind: &'static str, message: impl Into<String>) -> TransferFault {
        TransferFault {
            kind,
            message: message.into(),
            status: None,
            received: 0,
            total: None,
            requests: 0,
        }
    }

    /// The counters a `volume_returned` event carries.
    pub fn detail(&self) -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        out.insert("bytes".to_string(), json!(self.received));
        out.insert("total".to_string(), json!(self.total));
        out.insert("requests".to_string(), json!(self.requests));
        if let Some(status) = self.status {
            out.insert("status".to_string(), json!(status));
        }
        out
    }
}

impl std::fmt::Display for TransferFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

/// How a fetch ended without an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The session or the processor ended meanwhile: say nothing.
    Cancelled,
    /// The library refused the account (401/403/407): the whole processor steps away
    /// and registers again.
    LostLibrary(String),
    /// Give the claim back with this class.
    Fault(TransferFault),
}

impl From<TransferFault> for FetchError {
    fn from(f: TransferFault) -> Self {
        FetchError::Fault(f)
    }
}

/// A verified archive (or one proven damaged at the library), held until its claim's
/// terminal event. Dropping it releases the placement.
#[derive(Debug)]
pub struct FetchedArchive {
    pub placement: Placement,
    pub size: u64,
    pub crc32: u32,
    pub requests: u32,
    pub restarts: u32,
    pub repairs: u32,
    pub seconds: f64,
    pub verify_seconds: f64,
    pub members: u32,
    pub damaged: Vec<String>,
    pub structural: Option<String>,
    pub verdict: Option<String>,
}

/// Appended to the pipeline's own failure for a volume whose archive was proven
/// damaged at the library.
pub const DAMAGED_AT_LIBRARY_NOTE: &str = " (the library's copy: the same bytes on two downloads)";

impl FetchedArchive {
    pub fn path(&self) -> &Path {
        self.placement.path()
    }

    /// The detail of a `fetch {state: "ready"}` event.
    pub fn summary(&self) -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        out.insert("bytes".into(), json!(self.size));
        out.insert("seconds".into(), json!(round3(self.seconds)));
        let rate = (self.seconds > 0.0)
            .then(|| ((self.size as f64 / 1e6 / self.seconds) * 10.0).round() / 10.0);
        out.insert("mb_per_s".into(), json!(rate));
        out.insert("requests".into(), json!(self.requests));
        out.insert("restarts".into(), json!(self.restarts));
        out.insert("repairs".into(), json!(self.repairs));
        out.insert("verify_seconds".into(), json!(round3(self.verify_seconds)));
        out.insert("placement".into(), json!(self.placement.kind().as_str()));
        out.insert("crc32".into(), json!(format!("{:08x}", self.crc32)));
        out.insert("members".into(), json!(self.members));
        if let Some(verdict) = &self.verdict {
            out.insert("verdict".into(), json!(verdict));
            out.insert("damaged".into(), json!(self.damaged));
            if let Some(s) = &self.structural {
                out.insert(
                    "structural".into(),
                    json!(s.chars().take(200).collect::<String>()),
                );
            }
        }
        out
    }

    /// The note for a failed volume whose archive was damaged at the library.
    pub fn damaged_note(&self) -> Option<String> {
        self.verdict.as_ref()?;
        if self.damaged.is_empty() {
            return Some(DAMAGED_AT_LIBRARY_NOTE.to_string());
        }
        let base = &DAMAGED_AT_LIBRARY_NOTE[..DAMAGED_AT_LIBRARY_NOTE.len() - 1];
        Some(format!("{base}; {})", describe_damaged(&self.damaged)))
    }
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// `1234567` → `1,234,567` (Python's `{:,}`).
pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn mb(v: Option<u64>) -> String {
    v.map(|v| format!("{:.1}", v as f64 / 1e6))
        .unwrap_or_else(|| "?".into())
}

/// Where fetch progress goes (a `fetch` event's detail, with `state`). Never blocks.
pub type Progress<'a> = &'a (dyn Fn(BTreeMap<String, Value>) + Send + Sync);

/// One processor's downloader. Each session's feeder runs its own downloads through
/// it, one at a time; a cancel token cuts the ones a stopping session is running.
pub struct ArchiveFetcher {
    http: reqwest::Client,
    base: String,
    creds: Arc<Credentials>,
    spool: Arc<ArchiveSpool>,
    timing: FetchTiming,
}

/// One fetch's counters and its silence clock.
struct Download<'a> {
    label: &'a str,
    shown: String,
    cancel: &'a CancelToken,
    progress: Option<Progress<'a>>,
    timing: &'a FetchTiming,
    started: Instant,
    /// When a new byte last arrived, across every attempt of every copy.
    last_progress: Instant,
    requests: u32,
    restarts: u32,
    no_range_restarts: u32,
    repairs: u32,
    last_error: String,
    offered_at: Option<Instant>,
}

impl Download<'_> {
    fn check(&self) -> Result<(), FetchError> {
        if self.cancel.is_cancelled() {
            Err(FetchError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn offer(&mut self, head: BTreeMap<String, Value>, force: bool) {
        let Some(progress) = self.progress else {
            return;
        };
        let now = Instant::now();
        if now.duration_since(self.started) < self.timing.progress_after {
            return;
        }
        if !force
            && self
                .offered_at
                .is_some_and(|t| now.duration_since(t) < self.timing.progress_every)
        {
            return;
        }
        self.offered_at = Some(now);
        progress(head);
    }

    async fn wait(&self, seconds: Duration) -> Result<(), FetchError> {
        tokio::select! {
            _ = self.cancel.cancelled() => Err(FetchError::Cancelled),
            _ = tokio::time::sleep(seconds) => Ok(()),
        }
    }

    fn fault(&self, kind: &'static str, message: String, copy: &Copy) -> TransferFault {
        TransferFault {
            kind,
            message,
            status: None,
            received: copy.received,
            total: copy.total,
            requests: self.requests,
        }
    }
}

/// One full copy of the file, as far as it has arrived.
#[derive(Default)]
struct Copy {
    placement: Option<Placement>,
    received: u64,
    total: Option<u64>,
    etag: Option<String>,
    crc: crc32fast::Hasher,
}

impl Copy {
    fn crc(&self) -> u32 {
        self.crc.clone().finalize()
    }

    async fn reset(&mut self) -> Result<(), FetchError> {
        if let Some(mut placement) = self.placement.take() {
            let (placement, result) = tokio::task::spawn_blocking(move || {
                let r = placement.reset();
                (placement, r)
            })
            .await
            .map_err(|e| {
                FetchError::Fault(TransferFault::new(
                    "local",
                    format!("the spool task failed: {e}"),
                ))
            })?;
            result.map_err(|e| {
                FetchError::Fault(TransferFault::new(
                    "local",
                    format!("could not reset a download: {e}"),
                ))
            })?;
            self.placement = Some(placement);
        }
        self.received = 0;
        self.crc = crc32fast::Hasher::new();
        Ok(())
    }
}

enum Attempt {
    /// Retry on the backoff.
    Transport {
        message: String,
        retry_after: Option<Duration>,
        server_error: bool,
    },
    /// This copy starts over with a plain GET.
    Restart(String),
    /// Moved from RAM to disk: go again at once.
    Replaced,
    Fail(FetchError),
}

impl From<FetchError> for Attempt {
    fn from(e: FetchError) -> Self {
        Attempt::Fail(e)
    }
}

impl From<TransferFault> for Attempt {
    fn from(f: TransferFault) -> Self {
        Attempt::Fail(FetchError::Fault(f))
    }
}

fn transport(message: impl Into<String>) -> Attempt {
    Attempt::Transport {
        message: message.into(),
        retry_after: None,
        server_error: false,
    }
}

/// The ETag exactly as sent, when it is a strong one.
fn strong_etag(value: Option<&str>) -> Option<String> {
    let v = value?.trim();
    if v.starts_with("W/") || !(v.len() >= 2 && v.starts_with('"') && v.ends_with('"')) {
        return None;
    }
    Some(v.to_string())
}

/// `bytes START-END/TOTAL` → (start, end, total).
fn content_range(value: Option<&str>) -> Option<(u64, u64, Option<u64>)> {
    let rest = value?.strip_prefix("bytes ")?;
    let (span, total) = rest.split_once('/').unwrap_or((rest, ""));
    let (first, last) = span.split_once('-')?;
    let total = match total.trim() {
        "" | "*" => None,
        t => Some(t.parse().ok()?),
    };
    Some((first.trim().parse().ok()?, last.trim().parse().ok()?, total))
}

fn header(response: &reqwest::Response, name: http::header::HeaderName) -> Option<&str> {
    response.headers().get(name).and_then(|v| v.to_str().ok())
}

impl ArchiveFetcher {
    /// `base` is the library URL (no trailing slash, any path prefix kept).
    pub fn new(
        http: reqwest::Client,
        base: &str,
        creds: Arc<Credentials>,
        spool: Arc<ArchiveSpool>,
        timing: FetchTiming,
    ) -> ArchiveFetcher {
        ArchiveFetcher {
            http,
            base: base.trim_end_matches('/').to_string(),
            creds,
            spool,
            timing,
        }
    }

    pub fn timing(&self) -> &FetchTiming {
        &self.timing
    }

    /// Download, verify and hold one archive — or say why not. Whatever it returns,
    /// nothing it placed is left held but the archive it hands over.
    pub async fn fetch(
        &self,
        url_path: &str,
        size: Option<u64>,
        cancel: &CancelToken,
        progress: Option<Progress<'_>>,
        label: &str,
    ) -> Result<FetchedArchive, FetchError> {
        let url = format!("{}{}", self.base, quote_path(url_path));
        let parts: Vec<&str> = url_path.split('/').filter(|p| !p.is_empty()).collect();
        let shown = if parts.is_empty() {
            url_path.to_string()
        } else {
            parts[parts.len().saturating_sub(2)..].join("/")
        };
        let now = Instant::now();
        let mut dl = Download {
            label,
            shown,
            cancel,
            progress,
            timing: &self.timing,
            started: now,
            last_progress: now,
            requests: 0,
            restarts: 0,
            no_range_restarts: 0,
            repairs: 0,
            last_error: String::new(),
            offered_at: None,
        };
        let mut copy = self.download(&url, size, &mut dl).await?;
        let seconds = dl.started.elapsed().as_secs_f64();
        let mut verified = self.verify(&copy, &dl).await?;
        while !verified.ok() {
            tracing::warn!(
                "{}: {}; downloading a second copy to tell damage from a bad transfer",
                dl.label,
                verified.describe()
            );
            let second = self.download(&url, size, &mut dl).await?;
            if second.etag != copy.etag || second.total != copy.total {
                // The file changed between the copies: the second IS the file now.
                let reason = format!(
                    "the library's copy changed ({:?} -> {:?})",
                    copy.etag, second.etag
                );
                copy = second;
                self.restart(&mut dl, &reason, false, &copy)?;
                verified = self.verify(&copy, &dl).await?;
                continue;
            }
            let again = self.verify(&second, &dl).await?;
            if again.ok() {
                tracing::warn!(
                    "{}: the second copy is clean: the first was corrupted in transit",
                    dl.label
                );
                copy = second;
                verified = again;
                dl.repairs += 1;
                break;
            }
            if second.received == copy.received && second.crc() == copy.crc() {
                drop(second);
                tracing::warn!(
                    "{}: damaged at the library (the same bytes on two downloads): {}; the pipeline gets it as it is",
                    dl.label,
                    verified.describe()
                );
                return self.held(
                    copy,
                    &dl,
                    seconds,
                    &verified,
                    Some("damaged at the library"),
                );
            }
            return Err(FetchError::Fault(TransferFault {
                kind: bunko_proto::return_class::DIFFERS,
                message: format!(
                    "two downloads failed their CRC checks with different bytes ({}; then {})",
                    verified.describe(),
                    again.describe()
                ),
                status: None,
                received: second.received,
                total: second.total,
                requests: dl.requests,
            }));
        }
        self.held(copy, &dl, seconds, &verified, None)
    }

    fn held(
        &self,
        mut copy: Copy,
        dl: &Download<'_>,
        seconds: f64,
        verified: &Verified,
        verdict: Option<&str>,
    ) -> Result<FetchedArchive, FetchError> {
        let crc32 = copy.crc();
        // A complete copy always has a placement (`attempt` places before reading).
        let placement = copy.placement.take().ok_or_else(|| {
            FetchError::Fault(TransferFault::new(
                "local",
                "a complete copy without a placement",
            ))
        })?;
        let fetched = FetchedArchive {
            placement,
            size: copy.received,
            crc32,
            requests: dl.requests,
            restarts: dl.restarts,
            repairs: dl.repairs,
            seconds,
            verify_seconds: verified.seconds,
            members: verified.members,
            damaged: if verdict.is_some() {
                verified.damaged.clone()
            } else {
                Vec::new()
            },
            structural: if verdict.is_some() {
                verified.structural.clone()
            } else {
                None
            },
            verdict: verdict.map(str::to_string),
        };
        tracing::info!(
            "fetched {}: {} MB in {:.2} s to {}, {} request(s){}{}; {} members verified in {:.2} s (crc32 {:08x})",
            dl.label,
            mb(Some(copy.received)),
            seconds,
            fetched.placement.kind().as_str(),
            dl.requests,
            if dl.restarts > 0 {
                format!(", {} restart(s)", dl.restarts)
            } else {
                String::new()
            },
            if dl.repairs > 0 {
                format!(", {} repair(s)", dl.repairs)
            } else {
                String::new()
            },
            verified.members,
            verified.seconds,
            crc32
        );
        Ok(fetched)
    }

    async fn verify(&self, copy: &Copy, dl: &Download<'_>) -> Result<Verified, FetchError> {
        let path = copy
            .placement
            .as_ref()
            .map(|p| p.path().to_path_buf())
            .ok_or_else(|| {
                FetchError::Fault(TransferFault::new("local", "a copy without a placement"))
            })?;
        let cancel = dl.cancel.clone();
        let result = tokio::task::spawn_blocking(move || verify_archive(&path, &cancel))
            .await
            .map_err(|e| {
                FetchError::Fault(TransferFault::new(
                    "local",
                    format!("verification failed: {e}"),
                ))
            })?;
        result.map_err(|VerifyCancelled| FetchError::Cancelled)
    }

    /// Count one restart; give the claim back past `max_restarts`.
    fn restart(
        &self,
        dl: &mut Download<'_>,
        reason: &str,
        no_range: bool,
        copy: &Copy,
    ) -> Result<(), FetchError> {
        dl.restarts += 1;
        if no_range {
            dl.no_range_restarts += 1;
        }
        if dl.restarts > self.timing.max_restarts {
            if dl.no_range_restarts == dl.restarts {
                return Err(dl
                    .fault(
                        bunko_proto::return_class::NO_RANGE,
                        "the library (or a proxy in front of it) ignores Range, so a broken download cannot resume"
                            .to_string(),
                        copy,
                    )
                    .into());
            }
            return Err(dl
                .fault(
                    bunko_proto::return_class::CHANGED,
                    format!("the library's copy kept changing while it was read ({reason})"),
                    copy,
                )
                .into());
        }
        tracing::warn!(
            "{}: {reason}; starting over ({} of {})",
            dl.label,
            dl.restarts,
            self.timing.max_restarts
        );
        let mut head = BTreeMap::new();
        head.insert("state".to_string(), json!("restarting"));
        head.insert(
            "error".to_string(),
            json!(reason.chars().take(300).collect::<String>()),
        );
        head.insert("restarts".to_string(), json!(dl.restarts));
        dl.offer(head, true);
        Ok(())
    }

    /// One full copy of the file, across as many requests as it takes.
    async fn download(
        &self,
        url: &str,
        size: Option<u64>,
        dl: &mut Download<'_>,
    ) -> Result<Copy, FetchError> {
        let mut copy = Copy::default();
        let mut backoff = 0usize;
        let mut server_errors = 0u32;
        loop {
            dl.check()?;
            let heard_from = dl.last_progress;
            let (message, retry_after, server_error) =
                match self.attempt(url, size, &mut copy, dl).await {
                    Ok(()) => return Ok(copy),
                    Err(Attempt::Replaced) => continue,
                    Err(Attempt::Restart(reason)) => {
                        self.restart(dl, &reason, false, &copy)?;
                        copy.reset().await?;
                        copy.etag = None;
                        copy.total = None;
                        backoff = 0;
                        continue;
                    }
                    Err(Attempt::Fail(e)) => return Err(e),
                    Err(Attempt::Transport {
                        message,
                        retry_after,
                        server_error,
                    }) => (message, retry_after, server_error),
                };
            dl.last_error = message;
            server_errors = if server_error { server_errors + 1 } else { 0 };
            if server_errors >= 2 {
                let status = dl
                    .last_error
                    .rsplit(' ')
                    .next()
                    .unwrap_or("500")
                    .to_string();
                let mut fault = dl.fault(
                    bunko_proto::return_class::STALLED,
                    format!(
                        "the library answered {status} twice in a row for {}",
                        dl.shown
                    ),
                    &copy,
                );
                fault.status = Some(500);
                return Err(fault.into());
            }
            if dl.last_progress > heard_from {
                backoff = 0;
            }
            let delays = &self.timing.retry_delays;
            let mut wait = match retry_after {
                Some(r) => r.min(self.timing.retry_after_cap),
                None if delays.is_empty() => Duration::from_secs(1),
                None => delays[backoff.min(delays.len() - 1)],
            };
            backoff += 1;
            let silent = dl.last_progress.elapsed();
            if silent >= self.timing.stall {
                tracing::error!(
                    "{}: no new byte for {:.0} s after {} requests (last: {}); giving it back (stalled)",
                    dl.label,
                    silent.as_secs_f64(),
                    dl.requests,
                    dl.last_error
                );
                return Err(dl
                    .fault(
                        bunko_proto::return_class::STALLED,
                        format!(
                            "no new byte for {:.0} s at byte {} of {} (last: {})",
                            silent.as_secs_f64(),
                            thousands(copy.received),
                            copy.total
                                .map(|t| t.to_string())
                                .unwrap_or_else(|| "?".into()),
                            dl.last_error
                        ),
                        &copy,
                    )
                    .into());
            }
            wait = wait.min(self.timing.stall - silent);
            tracing::warn!(
                "{}: request {} ended after {} of {} MB ({}); resuming in {:.1} s",
                dl.label,
                dl.requests,
                mb(Some(copy.received)),
                mb(copy.total),
                dl.last_error,
                wait.as_secs_f64()
            );
            let mut head = BTreeMap::new();
            head.insert("state".to_string(), json!("retrying"));
            head.insert("bytes".to_string(), json!(copy.received));
            head.insert("total".to_string(), json!(copy.total));
            head.insert("requests".to_string(), json!(dl.requests));
            head.insert(
                "error".to_string(),
                json!(dl.last_error.chars().take(300).collect::<String>()),
            );
            head.insert(
                "retry_in".to_string(),
                json!((wait.as_secs_f64() * 10.0).round() / 10.0),
            );
            dl.offer(head, true);
            dl.wait(wait).await?;
        }
    }

    /// One request, read to its end. `Ok` once the copy is complete.
    async fn attempt(
        &self,
        url: &str,
        size: Option<u64>,
        copy: &mut Copy,
        dl: &mut Download<'_>,
    ) -> Result<(), Attempt> {
        let resuming = copy.received > 0;
        let mut request = self
            .http
            .get(url)
            .header(AUTHORIZATION, self.creds.header());
        if resuming {
            request = request.header(RANGE, format!("bytes={}-", copy.received));
            if let Some(etag) = &copy.etag {
                request = request.header(IF_RANGE, etag.as_str());
            }
        }
        dl.requests += 1;
        let deadline = self.timing.connect_timeout + self.timing.read_timeout;
        let response = tokio::select! {
            _ = dl.cancel.cancelled() => return Err(FetchError::Cancelled.into()),
            sent = tokio::time::timeout(deadline, request.send()) => match sent {
                Err(_) => return Err(transport("timed out")),
                Ok(Err(e)) => {
                    dl.check()?;
                    return Err(transport(describe_reqwest(&e)));
                }
                Ok(Ok(r)) => r,
            },
        };
        let status = response.status().as_u16();
        let requests = dl.requests;
        let fault = move |kind: &'static str, message: String| TransferFault {
            kind,
            message,
            status: Some(status),
            received: 0,
            total: None,
            requests,
        };
        if response.headers().contains_key("x-accel-redirect") {
            return Err(fault(
                bunko_proto::return_class::MISMATCH,
                format!(
                    "the library sent an X-Accel-Redirect for {}: an offload with no nginx in front of it (MOKURO_NGINX_ACCEL without the proxy)",
                    dl.shown
                ),
            )
            .into());
        }
        if matches!(status, 401 | 403 | 407) {
            return Err(FetchError::LostLibrary(format!(
                "the library answered {status} for {}",
                dl.shown
            ))
            .into());
        }
        if matches!(status, 404 | 410) {
            return Err(fault(
                bunko_proto::return_class::MISSING,
                format!("the library has no {} ({status})", dl.shown),
            )
            .into());
        }
        if matches!(status, 412 | 416) {
            return Err(Attempt::Restart(format!(
                "the library answered {status} to a resume"
            )));
        }
        if matches!(status, 408 | 429) || status >= 500 {
            let retry_after = header(&response, RETRY_AFTER)
                .and_then(|v| v.trim().parse::<i64>().ok())
                .map(|s| Duration::from_secs(s.max(0) as u64));
            return Err(Attempt::Transport {
                message: format!("the library answered {status}"),
                retry_after,
                // 500 twice in a row is usually THIS file; 502/503/504 are the path's.
                server_error: status >= 500 && !matches!(status, 502..=504),
            });
        }
        if status != StatusCode::OK.as_u16() && status != StatusCode::PARTIAL_CONTENT.as_u16() {
            return Err(fault(
                bunko_proto::return_class::REJECTED,
                format!("the library answered {status} for {}", dl.shown),
            )
            .into());
        }
        let etag = strong_etag(header(&response, ETAG));
        let length = header(&response, CONTENT_LENGTH).and_then(|v| v.trim().parse::<u64>().ok());
        let expected = if status == 206 {
            let span = content_range(header(&response, CONTENT_RANGE));
            let Some((start, _end, total)) = span.filter(|_| resuming) else {
                return Err(Attempt::Restart(
                    "the library sent a partial answer to a plain GET".into(),
                ));
            };
            let total_differs = matches!((total, copy.total), (Some(a), Some(b)) if a != b);
            let etag_differs = matches!((&etag, &copy.etag), (Some(a), Some(b)) if a != b);
            if start != copy.received || total_differs || etag_differs {
                return Err(Attempt::Restart(format!(
                    "the library's copy changed while it was read ({:?} -> {:?})",
                    copy.etag, etag
                )));
            }
            total.or(copy.total)
        } else {
            let total = length.or(size);
            if resuming {
                // The file changed (If-Range did not match), or something ignored Range:
                // THIS body is the whole file, taken as the new copy from byte 0.
                let changed = matches!((&etag, &copy.etag), (Some(a), Some(b)) if a != b);
                let reason = if changed {
                    format!(
                        "the library's copy changed while it was read ({:?} -> {:?})",
                        copy.etag, etag
                    )
                } else {
                    "the library answered a resume with the whole file".to_string()
                };
                self.restart(dl, &reason, !changed, copy)?;
                copy.reset().await?;
            }
            if let (Some(placement), Some(total)) = (&copy.placement, total)
                && placement.size() != total
            {
                // A different file than the one placed for: placed again, so the
                // reservation is always the real size.
                copy.placement = None;
            }
            copy.total = total;
            copy.etag = etag;
            total
        };
        if let (Some(size), Some(expected)) = (size, expected)
            && size != expected
        {
            let mut f = fault(
                bunko_proto::return_class::MISMATCH,
                format!(
                    "the library sent {} bytes for {}, which it said is {} bytes",
                    thousands(expected),
                    dl.shown,
                    thousands(size)
                ),
            );
            f.total = Some(expected);
            return Err(f.into());
        }
        if copy.placement.is_none() {
            let known = expected.or(size);
            copy.placement = Some(
                self.place(known.unwrap_or(0), known.is_some(), dl, copy)
                    .await?,
            );
        }
        self.read_body(response, copy, dl).await?;
        if let Some(expected) = expected
            && copy.received != expected
        {
            if copy.received > expected {
                return Err(Attempt::Restart(format!(
                    "the library sent {} bytes of a {}-byte file",
                    thousands(copy.received),
                    thousands(expected)
                )));
            }
            return Err(transport(format!(
                "the connection closed at byte {} of {}",
                thousands(copy.received),
                thousands(expected)
            )));
        }
        copy.total = Some(copy.received);
        Ok(())
    }

    async fn place(
        &self,
        size: u64,
        memory: bool,
        dl: &Download<'_>,
        copy: &Copy,
    ) -> Result<Placement, Attempt> {
        let spool = self.spool.clone();
        let placed = tokio::task::spawn_blocking(move || spool.place(size, memory))
            .await
            .map_err(|e| {
                Attempt::Fail(FetchError::Fault(TransferFault::new(
                    "local",
                    format!("the spool task failed: {e}"),
                )))
            })?;
        placed.map_err(|e| match e {
            SpoolError::NoRoom(message) => dl
                .fault(bunko_proto::return_class::NO_ROOM, message, copy)
                .into(),
            other => dl
                .fault(
                    bunko_proto::return_class::LOCAL,
                    format!("the spool failed: {other}"),
                    copy,
                )
                .into(),
        })
    }

    /// Read the body into the copy until it ends. The end is not completion: the
    /// caller checks the count it was promised.
    async fn read_body(
        &self,
        mut response: reqwest::Response,
        copy: &mut Copy,
        dl: &mut Download<'_>,
    ) -> Result<(), Attempt> {
        let mut buffer: Vec<u8> = Vec::with_capacity(MIB as usize);
        loop {
            let next = tokio::select! {
                _ = dl.cancel.cancelled() => return Err(FetchError::Cancelled.into()),
                next = tokio::time::timeout(self.timing.read_timeout, response.chunk()) => next,
            };
            match next {
                Ok(Ok(Some(chunk))) => {
                    dl.last_progress = Instant::now();
                    buffer.extend_from_slice(&chunk);
                    if buffer.len() as u64 >= MIB {
                        self.flush(&mut buffer, copy, dl).await?;
                    }
                }
                Ok(Ok(None)) => {
                    self.flush(&mut buffer, copy, dl).await?;
                    return Ok(());
                }
                Ok(Err(e)) => {
                    // The bytes that arrived before the cut are good: kept, resumed after.
                    self.flush(&mut buffer, copy, dl).await?;
                    dl.check()?;
                    return Err(transport(describe_reqwest(&e)));
                }
                Err(_) => {
                    self.flush(&mut buffer, copy, dl).await?;
                    return Err(transport("timed out"));
                }
            }
        }
    }

    async fn flush(
        &self,
        buffer: &mut Vec<u8>,
        copy: &mut Copy,
        dl: &mut Download<'_>,
    ) -> Result<(), Attempt> {
        if buffer.is_empty() {
            return Ok(());
        }
        let data = std::mem::replace(buffer, Vec::with_capacity(MIB as usize));
        let Some(mut placement) = copy.placement.take() else {
            return Err(FetchError::Fault(TransferFault::new(
                "local",
                "a download without a placement",
            ))
            .into());
        };
        let (placement, data, written) = tokio::task::spawn_blocking(move || {
            let r = placement.write(&data);
            (placement, data, r)
        })
        .await
        .map_err(|e| {
            Attempt::Fail(FetchError::Fault(TransferFault::new(
                "local",
                format!("the spool task failed: {e}"),
            )))
        })?;
        match written {
            Ok(()) => {
                copy.placement = Some(placement);
                copy.crc.update(&data);
                copy.received += data.len() as u64;
                let mut head = BTreeMap::new();
                head.insert("state".to_string(), json!("downloading"));
                head.insert("bytes".to_string(), json!(copy.received));
                head.insert("total".to_string(), json!(copy.total));
                head.insert("requests".to_string(), json!(dl.requests));
                dl.offer(head, false);
                Ok(())
            }
            Err(SpoolError::Full(e)) if placement.kind() == PlacementKind::Memory => {
                tracing::warn!(
                    "{}: memory is full ({e}) at {} of {} MB; downloading again to disk",
                    dl.label,
                    mb(Some(copy.received)),
                    mb(copy.total)
                );
                let known = placement.size();
                drop(placement);
                copy.placement = Some(self.place(known, false, dl, copy).await?);
                copy.received = 0;
                copy.crc = crc32fast::Hasher::new();
                Err(Attempt::Replaced)
            }
            Err(SpoolError::Full(e)) => Err(dl
                .fault(
                    bunko_proto::return_class::NO_ROOM,
                    format!("the processor's storage filled up: {e}"),
                    copy,
                )
                .into()),
            Err(other) => Err(dl
                .fault(
                    bunko_proto::return_class::LOCAL,
                    format!("writing the archive failed: {other}"),
                    copy,
                )
                .into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_parsers() {
        assert_eq!(strong_etag(Some("\"abc\"")), Some("\"abc\"".into()));
        assert_eq!(strong_etag(Some("W/\"abc\"")), None);
        assert_eq!(strong_etag(Some("abc")), None);
        assert_eq!(
            content_range(Some("bytes 10-19/20")),
            Some((10, 19, Some(20)))
        );
        assert_eq!(content_range(Some("bytes 10-19/*")), Some((10, 19, None)));
        assert_eq!(content_range(Some("items 1-2/3")), None);
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(12), "12");
    }
}
