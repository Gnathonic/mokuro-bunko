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
