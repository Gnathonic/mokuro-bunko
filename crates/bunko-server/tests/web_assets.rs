//! The web pages work under a sub-path mount (`https://host/bunko/`, prefix stripped by
//! the proxy): every URL a page or script names is relative to the `<base href>` the
//! server injects, never root-absolute, and the whole server (`assemble`) injects it.

// Fixtures are set up with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use axum::Router;
use axum::body::Body;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use bunko_server::http::static_files::WebAssets;
use http::Request;
use tower::ServiceExt;

/// The server's own modules (`app/` is bunko-control's local UI, served at the root of
/// another listener).
const MODULES: [&str; 9] = [
    "home",
    "login",
    "registration",
    "account",
    "catalog",
    "queue",
    "admin",
    "setup",
    "_static",
];

/// First segments of the server's own routes: a literal starting `/<one of these>` is a
/// root-absolute URL whatever code passes it on.
const ROUTES: [&str; 14] = [
    "_static",
    "_home",
    "_admin",
    "_processor",
    "api",
    "login",
    "register",
    "catalog",
    "queue",
    "account",
    "setup",
    "mokuro-reader",
    "inbox",
    "robots.txt",
];

/// Where a literal is always a URL (the spec's pattern: `fetch(`, `href=`, `src=`,
/// `location.href =`, and `pushState`'s third argument), plus a form's `action=`.
fn url_context(before: &str) -> bool {
    let b = before.trim_end();
    ["fetch(", "href=", "src=", "action=", "location.href ="]
        .iter()
        .any(|c| b.ends_with(c))
        || b.ends_with("location.href=")
        || b.rfind("pushState(")
            .is_some_and(|i| b[i..].matches(',').count() == 2)
}

/// Root-absolute URL literals on one line: `(column, literal start)`.
fn absolute_urls(line: &str) -> Vec<(usize, String)> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, c) in line.char_indices() {
        if !matches!(c, '\'' | '"' | '`') || !line[i + 1..].starts_with('/') {
            continue;
        }
        let lit = &line[i + 1..];
        let end = lit[1..]
            .find([c, '/', '?', '#'])
            .map_or(lit.len(), |e| e + 1);
        let first = &lit[1..end];
        if url_context(&line[..i]) || ROUTES.contains(&first) {
            out.push((i, lit.chars().take(40).collect()));
        }
    }
    out
}

#[test]
fn the_guard_sees_what_it_guards() {
    for bad in [
        "fetch('/api/stats')",
        r#"<link rel="stylesheet" href="/_static/shared.css">"#,
        r#"<script src="/login/login.js"></script>"#,
        "window.location.href = '/';",
        "history.pushState(null, '', '/catalog');",
        "const API_BASE = '/_admin/api';",
        "await postJSON('/setup/api/storage/local', {});",
        r#"<form method="post" action="/setup/code">"#,
    ] {
        assert!(!absolute_urls(bad).is_empty(), "missed: {bad}");
    }
    for good in [
        "fetch('api/stats')",
        r#"<a href="./" class="x">"#,
        "await apiGet('/users');",
        "path.split('/')",
        "'<path d=\"M8 12h8\"/>'",
        "const reader = 'https://reader.mokuro.app/';",
        "// from `/_admin/api/storage`",
    ] {
        assert!(absolute_urls(good).is_empty(), "flagged: {good}");
    }
}

#[test]
fn no_page_names_a_root_absolute_url() {
    let mut found = Vec::new();
    for name in WebAssets::iter() {
        let module = name.split('/').next().unwrap_or("");
        if !MODULES.contains(&module) || !(name.ends_with(".js") || name.ends_with(".html")) {
            continue;
        }
        let file = WebAssets::get(&name).unwrap();
        let text = String::from_utf8_lossy(&file.data).into_owned();
        for (n, line) in text.lines().enumerate() {
            for (col, lit) in absolute_urls(line) {
                found.push(format!("{name}:{}:{}: {lit}", n + 1, col + 1));
            }
        }
    }
    assert!(
        found.is_empty(),
        "root-absolute URLs (break under a sub-path mount):\n{}",
        found.join("\n")
    );
}

struct Env {
    _dir: tempfile::TempDir,
    _services: Services,
    app: Router,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.storage.base_path = dir.path().to_path_buf();
    config.catalog.enabled = true;
    std::fs::create_dir_all(config.storage.layout().library()).unwrap();
    let opts = ServeOptions::default();
    let services = Services::new(config, None, &opts).unwrap();
    // An admin, so `/` is the home page rather than the setup redirect.
    services
        .db
        .create_user(
            "boss",
            "boss-password-1",
            Role::Admin,
            UserStatus::Active,
            "",
        )
        .unwrap();
    let app = assemble(&services, &opts);
    Env {
        _dir: dir,
        _services: services,
        app,
    }
}

async fn send(env: &Env, method: &str, path: &str) -> (u16, http::HeaderMap, String) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("accept", "text/html")
        .header("user-agent", "Mozilla/5.0")
        .header("depth", "1")
        .body(Body::empty())
        .unwrap();
    let resp = env.app.clone().oneshot(req).await.unwrap();
    let (status, headers) = (resp.status().as_u16(), resp.headers().clone());
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    if let Some(len) = headers.get("content-length") {
        assert_eq!(len.to_str().unwrap(), body.len().to_string(), "{path}");
    }
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn every_page_gets_a_base_leading_to_the_mount_root() {
    let env = env();
    for (path, base) in [
        ("/", "./"),
        ("/catalog", "./"),
        ("/catalog/", "../"),
        ("/catalog//", "../../"),
        ("/queue/", "../"),
        ("/_admin", "./"),
        ("/_admin/", "../"),
        ("/login/", "../"),
        ("/setup", "./"),
    ] {
        let (status, _, body) = send(&env, "GET", path).await;
        assert_eq!(status, 200, "{path}");
        let tag = format!(r#"<base href="{base}">"#);
        assert!(body.contains(&tag), "{path}: no {tag}");
        assert_eq!(body.matches("<base ").count(), 1, "{path}");
    }
}

#[tokio::test]
async fn webdav_and_api_answers_are_untouched() {
    let env = env();
    let (status, headers, body) = send(&env, "GET", "/api/health").await;
    assert_eq!(status, 200);
    assert!(headers["content-type"].to_str().unwrap().contains("json"));
    assert!(body.starts_with('{') && !body.contains("<base"));
    // WebDAV's own HTML (an error page, a listing) is the protocol's, not a page.
    for (method, path) in [
        ("PROPFIND", "/"),
        ("GET", "/mokuro-reader/"),
        ("GET", "/mokuro-reader/nothing.cbz"),
    ] {
        let (_, _, body) = send(&env, method, path).await;
        assert!(!body.contains("<base"), "{method} {path}: {body}");
    }
}
