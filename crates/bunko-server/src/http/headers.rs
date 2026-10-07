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
///
/// The same rule covers the `Destination` of a MOVE/COPY: it names a second path the
/// request writes to (a `..` there let `COPY x` with `Destination: d/../x` resolve onto
/// its own source and delete it).
pub async fn path_guard(req: Request, next: Next) -> Response {
    let dest_ok = req
        .headers()
        .get("destination")
        .is_none_or(|v| v.to_str().is_ok_and(destination_is_clean));
    if !path_is_clean(req.uri().path()) || !dest_ok {
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

/// [`path_is_clean`] for a `Destination` header value: an absolute URL or a path; the
/// path part (before any query or fragment) must be clean.
pub fn destination_is_clean(raw: &str) -> bool {
    let path = match raw.find("://") {
        Some(i) => raw[i + 3..].find('/').map_or("/", |j| &raw[i + 3 + j..]),
        None => raw,
    };
    path_is_clean(path.split(['?', '#']).next().unwrap_or(""))
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

    /// Regression (review finding): MOVE/COPY destinations with `..` reached WebDAV.
    #[test]
    fn destinations_get_the_same_rule() {
        use super::destination_is_clean;
        assert!(destination_is_clean("/mokuro-reader/A/x.cbz"));
        assert!(destination_is_clean(
            "http://h:8080/mokuro-reader/A/x%20y.cbz?a=..#.."
        ));
        assert!(destination_is_clean("http://h"));
        assert!(!destination_is_clean("/mokuro-reader/A/B/../x.cbz"));
        assert!(!destination_is_clean(
            "http://h/mokuro-reader/A/B/%2e%2e/x.cbz"
        ));
        assert!(!destination_is_clean(
            "https://h/mokuro-reader/A/..%5cx.cbz"
        ));
        assert!(!destination_is_clean("/mokuro-reader/A/x%00.cbz"));
        assert!(!destination_is_clean("/mokuro-reader/A/x%ff.cbz"));
    }
}

/// 0.5.2 `test_security_headers.py`: volume files are revalidated every time.
#[cfg(test)]
mod cache_control_tests {
    use super::security_headers;
    use axum::Router;
    use axum::body::Body;
    use axum::response::IntoResponse;
    use http::{Request, header};
    use tower::ServiceExt;

    async fn cache_control(method: &str, path: &str, content_type: &'static str) -> Vec<String> {
        let app = Router::new()
            .fallback(move || async move {
                ([(header::CONTENT_TYPE, content_type)], "x").into_response()
            })
            .layer(axum::middleware::from_fn(security_headers));
        let req = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        resp.headers()
            .get_all(header::CACHE_CONTROL)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect()
    }

    /// A re-OCR rewrites `.mokuro` in place: a heuristically fresh cached copy would hand
    /// a reader the old OCR, so volume files must revalidate (the ETag keeps that a 304).
    #[tokio::test]
    async fn volume_files_are_revalidated_every_time() {
        for (path, content_type) in [
            (
                "/mokuro-reader/S/Vol%201.mokuro",
                "application/octet-stream",
            ),
            (
                "/mokuro-reader/S/Vol%201.hayai-nova.mokuro.gz",
                "application/gzip",
            ),
            ("/mokuro-reader/S/Vol%201.CBZ", "application/zip"),
        ] {
            for method in ["GET", "HEAD"] {
                assert_eq!(
                    cache_control(method, path, content_type).await,
                    ["no-cache"],
                    "{method} {path}"
                );
            }
        }
    }

    #[tokio::test]
    async fn other_downloads_keep_no_forced_cache_control() {
        let got = cache_control(
            "GET",
            "/mokuro-reader/S/notes.txt",
            "application/octet-stream",
        )
        .await;
        assert!(got.is_empty(), "{got:?}");
    }

    #[tokio::test]
    async fn a_preflight_or_put_answer_about_a_volume_file_is_left_alone() {
        for method in ["OPTIONS", "PUT"] {
            let got = cache_control(method, "/mokuro-reader/S/Vol%201.cbz", "text/plain").await;
            assert!(got.is_empty(), "{method}: {got:?}");
        }
    }
}
