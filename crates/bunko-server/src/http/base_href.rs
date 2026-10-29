//! Web pages that work under a sub-path mount (a reverse proxy serving the server at
//! `https://host/bunko/`, prefix stripped). Every page, script and redirect names its
//! URLs relative to the mount root, and each HTML page gets a `<base href>` that leads
//! from the request path back to that root. The server never needs to know the prefix:
//! `../` from `/catalog/` is the mount root whatever the proxy put in front of it.

use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::{HeaderValue, Method, StatusCode, header};

/// Embedded pages are a few hundred KB at most; anything bigger is not one of them.
const MAX_PAGE: usize = 8 << 20;

/// The way from `path` (a request path, as sent) back to the mount root: `./` from a
/// file at the root, one `../` per folder of the path's directory (the path itself when
/// it ends with `/`, else its parent).
pub fn relative_root(path: &str) -> String {
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    let depth = dir.split('/').filter(|s| !s.is_empty()).count();
    if depth == 0 {
        "./".to_string()
    } else {
        "../".repeat(depth)
    }
}

/// `target` (a root-relative path without its leading `/`) as seen from `path`: a
/// redirect Location that stays under the mount.
pub fn relative_to_root(path: &str, target: &str) -> String {
    match relative_root(path).as_str() {
        "./" if !target.is_empty() => target.to_string(),
        root => format!("{root}{target}"),
    }
}

/// Requests whose HTML (if any) is not one of the server's pages: WebDAV (its error
/// pages and listings belong to the protocol) and the JSON APIs.
fn is_page_path(path: &str, method: &Method, headers: &http::HeaderMap) -> bool {
    if path == "/" {
        // `/` is WebDAV's too; only a browser's GET gets the home page.
        return method == Method::GET && crate::accounts::is_browser_request(headers);
    }
    if super::cors::is_dav_path(path) || path.starts_with("/_processor") {
        return false;
    }
    // `/api/...` and every module API (`/login/api`, `/_admin/api`, `/catalog/api`, ...).
    let mut segs = path.trim_start_matches('/').split('/');
    let (first, second) = (segs.next(), segs.next());
    first != Some("api") && second != Some("api")
}

fn is_html(resp: &Response) -> bool {
    resp.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/html"))
}

/// `body` with `<base href="{root}">` right after its first `<head>` tag, or `None`
/// when it has none.
fn with_base(body: &[u8], root: &str) -> Option<Vec<u8>> {
    let lower = body.to_ascii_lowercase();
    let start = lower.windows(5).position(|w| w == b"<head")?;
    // `<head>` or `<head ...>`, never `<header>`.
    let after = *lower.get(start + 5)?;
    if after != b'>' && !after.is_ascii_whitespace() {
        return None;
    }
    let end = start + lower[start..].iter().position(|&b| b == b'>')? + 1;
    let tag = format!("<base href=\"{root}\">");
    let mut out = Vec::with_capacity(body.len() + tag.len());
    out.extend_from_slice(&body[..end]);
    out.extend_from_slice(tag.as_bytes());
    out.extend_from_slice(&body[end..]);
    Some(out)
}

/// The middleware: an HTML page answered for a page path gets its `<base href>`.
/// Error pages too (the setup code page answers 400/403 with its form).
pub async fn inject(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let page = req.method() != Method::HEAD && is_page_path(&path, req.method(), req.headers());
    let resp = next.run(req).await;
    if !page || !is_html(&resp) {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_PAGE).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Some(out) = with_base(&bytes, &relative_root(&path)) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    parts
        .headers
        .insert(header::CONTENT_LENGTH, HeaderValue::from(out.len()));
    Response::from_parts(parts, Body::from(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_root_counts_the_directory() {
        assert_eq!(relative_root("/"), "./");
        assert_eq!(relative_root("/catalog"), "./");
        assert_eq!(relative_root("/catalog/"), "../");
        assert_eq!(relative_root("/_admin/a/b"), "../../");
        assert_eq!(relative_root("/app/settings/logs"), "../../");
        assert_eq!(relative_root("/setup/code"), "../");
    }

    #[test]
    fn redirects_stay_under_the_mount() {
        assert_eq!(relative_to_root("/", "setup"), "setup");
        assert_eq!(relative_to_root("/", "catalog/"), "catalog/");
        assert_eq!(relative_to_root("/setup/code", "setup"), "../setup");
        assert_eq!(relative_to_root("/", ""), "./");
    }

    #[test]
    fn base_goes_right_after_head() {
        let page = b"<!DOCTYPE html>\n<html><HEAD lang=x>\n<title>t</title></head><header>";
        let out = String::from_utf8(with_base(page, "../").unwrap()).unwrap();
        assert_eq!(
            out,
            "<!DOCTYPE html>\n<html><HEAD lang=x><base href=\"../\">\n<title>t</title></head><header>"
        );
        assert!(with_base(b"<html><header></header>", "./").is_none());
        assert!(with_base(b"", "./").is_none());
    }

    #[test]
    fn only_pages_qualify() {
        let browser = {
            let mut h = http::HeaderMap::new();
            h.insert(header::ACCEPT, HeaderValue::from_static("text/html"));
            h
        };
        let dav = {
            let mut h = http::HeaderMap::new();
            h.insert(header::USER_AGENT, HeaderValue::from_static("davfs2/1.5"));
            h
        };
        let get = Method::GET;
        assert!(is_page_path("/", &get, &browser));
        assert!(!is_page_path("/", &get, &dav));
        assert!(!is_page_path(
            "/",
            &Method::from_bytes(b"PROPFIND").unwrap(),
            &browser
        ));
        for p in [
            "/catalog",
            "/catalog/",
            "/login",
            "/_admin/",
            "/setup/code",
            "/queue/",
        ] {
            assert!(is_page_path(p, &get, &browser), "{p}");
        }
        for p in [
            "/mokuro-reader/",
            "/mokuro-reader/S/v.cbz",
            "/inbox/x",
            "/api/health",
            "/login/api/me",
            "/_admin/api/status",
            "/catalog/api/library",
            "/_processor/register",
        ] {
            assert!(!is_page_path(p, &get, &browser), "{p}");
        }
    }
}
