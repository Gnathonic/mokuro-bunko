//! Response and path helpers shared by the catalog routes and the PUT handler, written
//! to 0.5.2's byte shapes (`json.dumps` default spacing + ASCII escaping, gzip ≥ 512
//! bytes, `text/plain` errors).

use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use axum::body::Body;
use axum::response::Response;
use bunko_db::pyfmt::{self, JsonStyle};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use serde_json::Value;

/// Bodies below this stay identity-encoded (`_GZIP_MIN_BYTES`).
pub const GZIP_MIN_BYTES: usize = 512;

fn status(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("gzip"))
}

/// `gzip.compress(body, compresslevel=6)`.
fn gzip(body: &[u8]) -> Option<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(
        Vec::with_capacity(body.len() / 4 + 64),
        flate2::Compression::new(6),
    );
    encoder.write_all(body).ok()?;
    encoder.finish().ok()
}

/// `_json_response` with an already-serialised body. `gzip_from` = the request headers
/// when the endpoint may gzip (0.5.2: `/library` and `/manifest` only).
pub fn json_bytes(
    code: u16,
    body: Vec<u8>,
    gzip_from: Option<&HeaderMap>,
    extra: &[(&'static str, &'static str)],
) -> Response {
    let mut body = body;
    let mut encoded = false;
    if let Some(headers) = gzip_from
        && body.len() >= GZIP_MIN_BYTES
        && accepts_gzip(headers)
        && let Some(compressed) = gzip(&body)
    {
        body = compressed;
        encoded = true;
    }
    let length = body.len();
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = status(code);
    let h = resp.headers_mut();
    if encoded {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    for (name, value) in extra {
        h.insert(*name, HeaderValue::from_static(value));
    }
    resp
}

/// `json.dumps(value)` (Python defaults) as a response.
pub fn json(code: u16, value: &Value) -> Response {
    json_bytes(
        code,
        pyfmt::dumps(value, JsonStyle::DEFAULT_ASCII).into_bytes(),
        None,
        &[],
    )
}

/// `{"error": message}`.
pub fn json_error(code: u16, message: &str) -> Response {
    json(code, &serde_json::json!({ "error": message }))
}

/// `_error_response`: `text/plain` (no charset), as the catalog sends it.
pub fn text(code: u16, message: &str) -> Response {
    text_with(code, message, "text/plain", &[])
}

pub fn text_with(
    code: u16,
    message: &str,
    content_type: &'static str,
    extra: &[(&'static str, &'static str)],
) -> Response {
    let mut resp = Response::new(Body::from(message.to_owned()));
    *resp.status_mut() = status(code);
    let h = resp.headers_mut();
    for (name, value) in extra {
        h.insert(*name, HeaderValue::from_static(value));
    }
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(message.len()));
    resp
}

/// `urllib.parse.unquote`: percent-decoding, invalid UTF-8 replaced.
pub fn unquote(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .into_owned()
}

/// `urllib.parse.parse_qs(query).get(name, [""])[0]`: `+` is a space, blank values
/// are dropped (so `name=` reads as missing), pairs without `=` are ignored.
pub fn query_param(query: Option<&str>, name: &str) -> String {
    for pair in query.unwrap_or("").split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        if unquote(&key.replace('+', " ")) == name {
            return unquote(&value.replace('+', " "));
        }
    }
    String::new()
}

/// `Path.resolve()` (non-strict): `..`/`.` folded lexically, then the longest
/// existing prefix canonicalised (symlinks followed), the rest appended. `None` for a
/// path the OS cannot represent (a NUL byte: Python's `ValueError`).
pub fn resolve(path: &Path) -> Option<PathBuf> {
    if path.as_os_str().as_encoded_bytes().contains(&0) {
        return None;
    }
    let mut lexical = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                lexical.pop();
            }
            Component::CurDir => {}
            other => lexical.push(other.as_os_str()),
        }
    }
    let mut existing = lexical.clone();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(&existing) {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return Some(out);
        }
        match (
            existing.file_name().map(|n| n.to_os_string()),
            existing.parent().map(Path::to_path_buf),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return Some(lexical),
        }
    }
}

/// `is_within_path(path, base)` on resolved paths.
pub fn is_within(path: &Path, base: &Path) -> bool {
    path.starts_with(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parsing() {
        assert_eq!(query_param(Some("name=Dr+Stone"), "name"), "Dr Stone");
        assert_eq!(query_param(Some("name=Dr%20Stone&x=1"), "name"), "Dr Stone");
        assert_eq!(query_param(Some("name=&name=b"), "name"), "b");
        assert_eq!(query_param(Some("name"), "name"), "");
        assert_eq!(query_param(None, "name"), "");
        assert_eq!(
            query_param(Some("series=%E6%97%A5&volume=v%261"), "volume"),
            "v&1"
        );
    }

    #[test]
    fn resolving() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(base.join("a")).unwrap();
        assert_eq!(resolve(&base.join("a/../a/x")).unwrap(), base.join("a/x"));
        assert!(!is_within(&resolve(&base.join("../etc")).unwrap(), &base));
        assert!(resolve(Path::new("/tmp/a\0b")).is_none());
    }

    #[test]
    fn json_shapes() {
        let r = json(200, &serde_json::json!({"a": "日", "b": [1, 2.0]}));
        assert_eq!(r.headers()["content-type"], "application/json");
    }
}
