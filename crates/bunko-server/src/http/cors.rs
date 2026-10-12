//! CORS (0.5.2 `middleware/cors.py`). A preflight is any `OPTIONS` carrying `Origin`; it is
//! answered here with 204 and never reaches the app. `OPTIONS` without `Origin` is an
//! ordinary request.

use crate::core::Core;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use http::{HeaderMap, HeaderValue, Method, StatusCode};

pub const EXPOSE_HEADERS: &str = "Content-Length, Content-Type, DAV, ETag, Last-Modified, Location, Lock-Token, \
WWW-Authenticate, X-Mokuro-Manifest, X-Mokuro-Recheck-After, X-Mokuro-Upload, X-Mokuro-Size, X-Mokuro-Put, \
X-Mokuro-Digest-Verified";
pub const ALLOW_METHODS: &str =
    "GET, HEAD, POST, PUT, DELETE, OPTIONS, PROPFIND, PROPPATCH, MKCOL, COPY, MOVE, LOCK, UNLOCK";
pub const ALLOW_HEADERS: &str = "Authorization, Content-Digest, Content-Type, Content-Length, Depth, Destination, If, \
If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since, Lock-Token, Overwrite, Range, Timeout, X-Requested-With";

/// Paths served by WebDAV, which advertise `X-Mokuro-Put: verified` on preflight.
pub fn is_dav_path(path: &str) -> bool {
    matches!(path, "" | "/" | "/mokuro-reader" | "/inbox")
        || path.starts_with("/mokuro-reader/")
        || path.starts_with("/inbox/")
}

pub async fn cors(State(state): State<Core>, req: Request, next: Next) -> Response {
    let (enabled, allowed, credentials) = {
        let cfg = state.config.read();
        let origin = req
            .headers()
            .get(http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let allowed = origin.filter(|o| cfg.cors.is_origin_allowed(o));
        (cfg.cors.enabled, allowed, cfg.cors.allow_credentials)
    };
    if !enabled {
        return next.run(req).await;
    }
    let has_origin = req.headers().contains_key(http::header::ORIGIN);
    if req.method() == Method::OPTIONS && has_origin {
        let dav = is_dav_path(req.uri().path());
        let private = req
            .headers()
            .get("access-control-request-private-network")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("true"));
        let mut resp = Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap_or_default();
        let h = resp.headers_mut();
        if let Some(origin) = &allowed {
            put_origin(h, origin, credentials);
            h.insert(
                "access-control-allow-methods",
                HeaderValue::from_static(ALLOW_METHODS),
            );
            h.insert(
                "access-control-allow-headers",
                HeaderValue::from_static(ALLOW_HEADERS),
            );
            h.insert("access-control-max-age", HeaderValue::from_static("3600"));
            if private {
                h.insert(
                    "access-control-allow-private-network",
                    HeaderValue::from_static("true"),
                );
            }
            if dav {
                h.insert(
                    "access-control-expose-headers",
                    HeaderValue::from_static(EXPOSE_HEADERS),
                );
            }
        }
        if dav {
            h.insert("x-mokuro-put", HeaderValue::from_static("verified"));
        }
        return resp;
    }
    let mut resp = next.run(req).await;
    if let Some(origin) = &allowed {
        let h = resp.headers_mut();
        put_origin(h, origin, credentials);
        h.insert(
            "access-control-expose-headers",
            HeaderValue::from_static(EXPOSE_HEADERS),
        );
    }
    resp
}

fn put_origin(h: &mut HeaderMap, origin: &str, credentials: bool) {
    if let Ok(v) = HeaderValue::from_str(origin) {
        h.insert("access-control-allow-origin", v);
    }
    if credentials {
        h.insert(
            "access-control-allow-credentials",
            HeaderValue::from_static("true"),
        );
    }
    h.append(http::header::VARY, HeaderValue::from_static("Origin"));
}
