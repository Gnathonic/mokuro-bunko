//! The accounting layer (design §4.1): the second-outermost layer of the router, so it
//! sees every request, WebDAV and web alike, including the ones answered early (a
//! `path_guard` 400, a CORS preflight, the catalog, `GET /`).
//!
//! Per request: a little string work, two counting body wrappers and, when the
//! response body ends, one short lock in the collector. No I/O, no `await` of its own.

use super::body::{CountingBody, CountingReqBody, Pending};
use super::classify::{self, Input};
use super::collector::RequestRecord;
use super::principal::{self, PrincipalKind};
use super::{Analytics, AnalyticsSlot, LoginAttempt};
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use bunko_dav::{DavInfo, ListingInfo, Served};
use http::{HeaderMap, header};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

/// Longest `User-Agent` / `Origin` / `Host` kept.
const HEADER_MAX: usize = 256;

fn header_text(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    let v = headers.get(name)?.to_str().ok()?.trim();
    (!v.is_empty()).then(|| v.chars().take(HEADER_MAX).collect())
}

/// A MOVE's `Destination` in the caller's own tree: its per-user-relative path.
fn per_user_destination(headers: &HeaderMap) -> Option<String> {
    let dest = headers.get("destination")?.to_str().ok()?;
    let path = crate::auth::paths::destination_path(dest)?;
    match bunko_dav::dav_target_of(&path) {
        (bunko_dav::DavTarget::Progress, rel) => rel,
        _ => None,
    }
}

pub async fn layer(State(a): State<Analytics>, mut req: Request, next: Next) -> Response {
    if !a.enabled() {
        return next.run(req).await;
    }
    let started = Instant::now();
    let at_ms = super::now_ms();
    let method = classify::method_name(req.method().as_str());
    let path = percent_encoding::percent_decode_str(req.uri().path())
        .decode_utf8_lossy()
        .into_owned();
    // Only the catalog's JSON names a series in its query.
    let query = req
        .uri()
        .query()
        .filter(|_| path.starts_with("/catalog/api/"))
        .map(str::to_string);
    let headers = req.headers();
    let depth = (method == "PROPFIND")
        .then(|| {
            headers
                .get("depth")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        })
        .flatten();
    let range = headers.contains_key(header::RANGE);
    let user_agent = header_text(headers, header::USER_AGENT);
    let origin = header_text(headers, header::ORIGIN);
    let host = header_text(headers, header::HOST);
    let destination_rel = (method == "MOVE")
        .then(|| per_user_destination(headers))
        .flatten();
    // The address as the server resolves it (trusted proxies honoured), without
    // authenticating: used when the request never resolves an identity.
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let ip_text = a.core().proxies.read().client_ip_text(peer, req.headers());
    let slot = AnalyticsSlot::default();
    req.extensions_mut().insert(slot.clone());
    let bytes_in = Arc::new(AtomicU64::new(0));
    let req = req.map(|b| Body::new(CountingReqBody::new(b, bytes_in.clone())));
    let inflight = a.gauges().request();

    let mut resp = next.run(req).await;

    let headers_ms = started.elapsed().as_millis() as u64;
    let status = resp.status().as_u16();
    // Taken, not cloned: nothing outside this layer reads them (never on the wire).
    let dav = resp.extensions_mut().remove::<DavInfo>();
    let served = resp.extensions().get::<Served>().copied();
    let listing = resp.extensions().get::<ListingInfo>().copied();
    let login_attempt = resp.extensions_mut().remove::<LoginAttempt>().map(|l| l.0);
    let accel = served.is_some_and(|s| s.accel) || resp.headers().contains_key("x-accel-redirect");
    let declared = resp
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());

    let ident = slot.get();
    let ip: Arc<str> = principal::canonical_ip_text(
        ident
            .map(|i| i.client_ip.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&ip_text),
    )
    .into();
    let principal = principal::principal_of(
        ident.and_then(|i| i.username.as_deref()),
        ident.map_or(bunko_core::Role::Anonymous, |i| i.role),
        &ip,
        a.db().salt(),
    );
    let class = classify::classify(&Input {
        method,
        path: &path,
        query: query.as_deref(),
        dav: dav.as_ref(),
        depth: depth.as_deref(),
        processor: principal.kind == PrincipalKind::Processor,
        status,
        range,
        accel,
    });
    let login_refused = method == "POST"
        && matches!(path.as_str(), "/login/api/token" | "/login/api/check")
        && matches!(status, 401 | 429);
    let auth_failure = match ident {
        Some(i) if i.auth_error.is_some() => Some(i.attempted_username.clone().unwrap_or_default()),
        _ if login_refused => Some(login_attempt.unwrap_or_default()),
        _ => None,
    };
    let est_bytes_out = if accel {
        served.map_or(0, |s| s.len)
    } else {
        0
    };
    let no_body = method == "HEAD" || matches!(status, 100..=199 | 204 | 304);
    let record = RequestRecord {
        at_ms,
        method,
        status,
        class,
        bytes_out: 0,
        est_bytes_out,
        bytes_in: 0,
        headers_ms,
        total_ms: 0,
        aborted: false,
        range,
        principal,
        ip,
        user_agent,
        origin,
        host,
        path,
        served,
        listing,
        auth_failure,
        destination_rel,
    };
    let (parts, body) = resp.into_parts();
    let pending = Pending {
        analytics: a.0.clone(),
        record,
        started,
        bytes_in,
        declared,
        no_body,
        _inflight: inflight,
    };
    Response::from_parts(parts, Body::new(CountingBody::new(body, pending)))
}
