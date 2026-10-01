//! The web UIs, embedded at build time (0.5.2 served each module's `web/` directory).

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use http::{HeaderValue, StatusCode, header};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../web/"]
pub struct WebAssets;

pub fn content_type(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// `module/file` from the embedded tree, or None. Rejects traversal outright.
pub fn asset(module: &str, file: &str) -> Option<rust_embed::EmbeddedFile> {
    if file.is_empty() || file.contains("..") || file.starts_with('/') || file.contains('\\') {
        return None;
    }
    WebAssets::get(&format!("{module}/{file}"))
}

/// Serve an embedded asset with the given Cache-Control (None = no header).
pub fn serve(module: &str, file: &str, cache_control: Option<&'static str>) -> Option<Response> {
    let f = asset(module, file)?;
    let mut resp = Response::new(Body::from(f.data.into_owned()));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type(file)));
    if let Some(cc) = cache_control {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cc));
    }
    Some(resp)
}

pub fn not_found_text() -> Response {
    (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain")], "Not Found").into_response()
}

pub const ROBOTS_TXT: &str = "User-agent: *\nDisallow: /\n";

pub async fn robots() -> Response {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], ROBOTS_TXT).into_response()
}

/// `GET /_static/<file>` with `Cache-Control: public, max-age=3600`.
pub async fn shared_static(axum::extract::Path(file): axum::extract::Path<String>) -> Response {
    serve("_static", &file, Some("public, max-age=3600")).unwrap_or_else(not_found_text)
}
