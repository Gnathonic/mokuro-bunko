//! Security and cache headers added to every response (0.5.2 `security_headers.py`).
//! Headers an inner handler already set are left alone.

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use http::{HeaderValue, Method, header};

const ALWAYS: &[(&str, &str)] = &[
    ("x-content-type-options", "nosniff"),
    ("x-frame-options", "DENY"),
    ("referrer-policy", "no-referrer"),
    ("x-xss-protection", "1; mode=block"),
    ("x-robots-tag", "noindex, nofollow"),
];

pub async fn security_headers(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path_lower = req.uri().path().to_ascii_lowercase();
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    for (name, value) in ALWAYS {
        if !headers.contains_key(*name) {
            headers.insert(*name, HeaderValue::from_static(value));
        }
    }
    if !headers.contains_key(header::CACHE_CONTROL) {
        let media = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| {
                v.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();
        let value = if media == "application/json" {
            Some("no-store")
        } else if media.starts_with("image/") {
            Some("private, max-age=86400")
        } else if (method == Method::GET || method == Method::HEAD)
            && (path_lower.ends_with(".mokuro")
                || path_lower.ends_with(".mokuro.gz")
                || path_lower.ends_with(".cbz"))
        {
            Some("no-cache")
        } else {
            None
        };
        if let Some(v) = value {
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(v));
        }
    }
    resp
}

/// `MOKURO_DEBUG` (any value but empty/0/false): one line per request on stderr,
/// `[REQUEST] <METHOD> <path> -> <status> (<seconds>s)` (0.5.2 `request_log.py`). The
/// time excludes body streaming, as in 0.5.2.
pub fn request_log_enabled() -> bool {
    // Decided once, as 0.5.2 did at construction.
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("MOKURO_DEBUG")
            .is_ok_and(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false"))
    })
}

pub async fn request_log(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let started = std::time::Instant::now();
    let resp = next.run(req).await;
    eprintln!(
        "[REQUEST] {method} {path} -> {} ({:.3}s)",
        resp.status().as_u16(),
        started.elapsed().as_secs_f64()
    );
    resp
}

/// Refuse request paths whose percent-decoded form has a `..` segment, a NUL byte or
/// invalid UTF-8 (400). Authorisation and WebDAV then always see the same normalised
/// path; no client sends such paths legitimately.
pub async fn path_guard(req: Request, next: Next) -> Response {
    if !path_is_clean(req.uri().path()) {
        let mut resp = Response::new(axum::body::Body::from("Bad Request: invalid path"));
        *resp.status_mut() = http::StatusCode::BAD_REQUEST;
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        return resp;
    }
    next.run(req).await
}

pub fn path_is_clean(raw: &str) -> bool {
    let Ok(decoded) = percent_encoding::percent_decode_str(raw).decode_utf8() else {
        return false;
    };
    !decoded.contains('\0') && !decoded.split(['/', '\\']).any(|seg| seg == "..")
}

#[cfg(test)]
mod tests {
    use super::path_is_clean;

    #[test]
    fn dotdot_and_garbage_refused() {
        assert!(path_is_clean("/mokuro-reader/Series%20A/Vol%201.cbz"));
        assert!(path_is_clean("/mokuro-reader/a..b/c.cbz"));
        assert!(!path_is_clean("/mokuro-reader/x/../Vol.cbz"));
        assert!(!path_is_clean("/mokuro-reader/x/%2e%2e/Vol.cbz"));
        assert!(!path_is_clean("/mokuro-reader/x/%2E%2E%2fVol.cbz"));
        assert!(!path_is_clean("/mokuro-reader/x/..%5cVol.cbz"));
        assert!(!path_is_clean("/a%00b"));
        assert!(!path_is_clean("/a%ff"));
    }
}
