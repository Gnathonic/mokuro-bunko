//! The desktop app's local pages (`/app/...`) and their backend (`/app/api/...`): the
//! first-launch chooser (library server or processor), the processor's pairing,
//! settings and status pages (docs/rust-port/GUI.md §1, §4). A library server's setup
//! and settings are its own web pages (`/setup`, the admin panel), on any machine.
//!
//! The router ([`app`]) is mounted on the local control listener of every long-running
//! instance (`gui`, `serve`, `processor serve`; bunko-control's `ControlListener::serve`
//! takes it): 127.0.0.1 only, same origin as `/control/...`. Every page and API route
//! needs the control token, as `Authorization: Bearer` (the tray) or the cookie that
//! `/app/login?c=<code>` sets (`bunko_control_<port>`, HttpOnly, SameSite=Strict); the
//! code is single use, from `POST /control/login-code` (bearer only).
//! The guard (bunko-control's `check_request`) is applied here too, so the pages do
//! not rely on how they are mounted:
//!
//! * `Host` must name a loopback address (no DNS rebinding from a web page);
//! * a state-changing request authenticated by the cookie must carry an `Origin` equal
//!   to this listener's own origin (SameSite does not separate localhost ports, so a
//!   page on another local port would otherwise be "same site");
//! * POST bodies are JSON (`axum::Json` refuses other content types: no form CSRF).
//!
//! Long work (install-ocr, models, doctor, update apply, ssl, ...) runs as this same
//! executable in a child process ([`jobs`]), so the GUI shows exactly what the CLI does,
//! streamed line by line over Server-Sent Events.

pub mod api;
pub mod jobs;
pub mod paths;
pub mod setup;
pub mod spawn;
pub mod tray;

#[cfg(test)]
mod coverage;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub use bunko_control::Role;
use bunko_control::http::{LoginCodes, check_request};

/// What the pages' backend works with.
pub struct AppState {
    pub role: Role,
    /// The control token (also the cookie's value).
    pub token: String,
    /// The listener's sign-in codes (`/app/login?c=`).
    pub codes: LoginCodes,
    /// This executable (child processes run it).
    pub exe: PathBuf,
    /// The library server's `config.yaml` (resolved: `-c`, `MOKURO_CONFIG`, default;
    /// child processes get it as `-c`).
    pub config_path: PathBuf,
    /// This machine's `processor.yaml`.
    pub processor_config: PathBuf,
    pub jobs: jobs::Jobs,
    /// Unix seconds of the last authenticated request (the `gui` idle exit).
    pub last_seen: AtomicU64,
    /// Set by `/app/api/handoff`: the `gui` command exits once the browser moved on.
    pub handoff: tokio::sync::watch::Sender<bool>,
}

impl AppState {
    /// `processor_config`: the one a running processor uses (None: this machine's,
    /// see [`paths::processor_config_path`]).
    pub fn new(
        role: Role,
        token: String,
        codes: LoginCodes,
        config_path: PathBuf,
        processor_config: Option<PathBuf>,
    ) -> AppState {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mokuro-bunko"));
        AppState {
            role,
            token,
            codes,
            exe,
            config_path,
            processor_config: processor_config
                .map(|p| std::path::absolute(&p).unwrap_or(p))
                .unwrap_or_else(paths::processor_config_path),
            jobs: jobs::Jobs::default(),
            last_seen: AtomicU64::new(now_secs()),
            handoff: tokio::sync::watch::channel(false).0,
        }
    }

    pub fn touch(&self) {
        self.last_seen.store(now_secs(), Ordering::Relaxed);
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The app router for a control listener (what `serve` and `processor serve` mount):
/// `token` and `codes` are the listener's (`ControlListener::token`, `login_codes`),
/// `config_path` the library's config.yaml, `processor_config` the processor.yaml in
/// use (processor role).
// Mounted by `serve` and `processor serve` on their control listeners (control.rs).
pub fn app(
    role: Role,
    token: &str,
    codes: LoginCodes,
    config_path: PathBuf,
    processor_config: Option<PathBuf>,
) -> Router {
    router(Arc::new(AppState::new(
        role,
        token.to_string(),
        codes,
        config_path,
        processor_config,
    )))
}

/// `/app/login`, the shared assets (`/_static/...`, public as on the library server),
/// the pages and `/app/api/...`, all else behind [`guard`].
pub fn router(state: Arc<AppState>) -> Router {
    let guarded = Router::new()
        .merge(api::routes())
        .route("/app", get(|| async { Redirect::to("/app/") }))
        .route("/app/", get(page))
        .route("/app/{*path}", get(page))
        .layer(middleware::from_fn_with_state(state.clone(), guard));
    Router::new()
        .route("/app/login", get(login))
        .route(
            "/_static/{file}",
            get(bunko_server::http::static_files::shared_static),
        )
        .merge(guarded)
        .with_state(state)
}

async fn guard(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if let Some((status, why)) = check_request(req.method(), req.headers(), &state.token) {
        if req.uri().path().starts_with("/app/api/") {
            return (status, axum::Json(serde_json::json!({"error": why}))).into_response();
        }
        return (
            status,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            format!(
                "<!DOCTYPE html><meta charset=utf-8><title>Mokuro Bunko</title>\
                 <link rel=stylesheet href=\"/_static/shared.css\">\
                 <body style=\"padding:2rem\"><h1>Mokuro Bunko</h1><p>{why}.</p></body>"
            ),
        )
            .into_response();
    }
    state.touch();
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    h.insert("X-Frame-Options", header::HeaderValue::from_static("DENY"));
    h.insert(
        "Referrer-Policy",
        header::HeaderValue::from_static("no-referrer"),
    );
    resp
}

/// `GET /app/login?c=<code>&next=/app/...`: use up the code, set the cookie, go to the
/// page (bunko-control's [`bunko_control::http::login`]).
async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<bunko_control::http::LoginQuery>,
) -> Response {
    let resp = bunko_control::http::login(&headers, &q, &state.token, &state.codes);
    if resp.status().is_redirection() {
        state.touch();
    }
    resp
}

/// The embedded file for an `/app/...` path: `/app/` is `index.html`, a page path
/// `/app/setup/processor` is `setup-processor.html`, anything with an extension is an asset.
pub fn page_file(path: &str) -> Option<String> {
    let rest = path.trim_start_matches('/');
    if rest.is_empty() {
        return Some("index.html".into());
    }
    if rest.contains("..") || rest.contains('\\') || rest.starts_with('/') {
        return None;
    }
    let rest = rest.trim_end_matches('/');
    if rest.contains('.') {
        return (!rest.contains('/')).then(|| rest.to_string());
    }
    if !rest
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '-')
    {
        return None;
    }
    // `/app/settings/<section>` is the settings page opened at that section.
    if rest == "settings" || rest.starts_with("settings/") {
        return Some("settings.html".into());
    }
    Some(format!("{}.html", rest.replace('/', "-")))
}

async fn page(path: Option<axum::extract::Path<String>>) -> Response {
    let path = path.map(|p| p.0).unwrap_or_default();
    if path.starts_with("api/") {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "no such API"})),
        )
            .into_response();
    }
    page_file(&path)
        .and_then(|f| bunko_server::http::static_files::serve("app", &f, None))
        .unwrap_or_else(bunko_server::http::static_files::not_found_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use bunko_control::http::safe_next;

    #[test]
    fn page_paths() {
        assert_eq!(page_file("").as_deref(), Some("index.html"));
        assert_eq!(
            page_file("setup/processor").as_deref(),
            Some("setup-processor.html")
        );
        assert_eq!(page_file("dashboard/").as_deref(), Some("dashboard.html"));
        assert_eq!(
            page_file("settings/update").as_deref(),
            Some("settings.html")
        );
        assert_eq!(page_file("app.css").as_deref(), Some("app.css"));
        assert_eq!(page_file("../admin/admin.js"), None);
        assert_eq!(page_file("x/y.js"), None);
        assert_eq!(page_file("a b"), None);
        assert_eq!(safe_next(Some("//evil.example/app/")), "/app/");
        assert_eq!(safe_next(Some("/app/settings#ocr")), "/app/settings#ocr");
        assert_eq!(safe_next(Some("https://x/app/")), "/app/");
    }

    /// `/app/api/info` tells the pages when the library folder cannot be written (a
    /// `~/.local` owned by root), with the folder in the way and the fix.
    #[cfg(unix)]
    #[tokio::test]
    async fn info_reports_an_unwritable_storage() {
        use std::os::unix::fs::PermissionsExt;
        use tower::ServiceExt;
        if unsafe { libc::getuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join(".local");
        std::fs::create_dir_all(&local).unwrap();
        let storage = local.join("share/mokuro-bunko");
        let config = dir.path().join("config.yaml");
        std::fs::write(
            &config,
            format!("storage:\n  base_path: {}\n", storage.display()),
        )
        .unwrap();
        std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o555)).unwrap();
        let state = Arc::new(AppState::new(
            Role::Gui,
            "secret-token".into(),
            LoginCodes::default(),
            config,
            Some(dir.path().join("processor.yaml")),
        ));
        let req = Request::builder()
            .uri("/app/api/info")
            .header("host", "127.0.0.1:4567")
            .header("authorization", "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let resp = router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o755)).unwrap();
        let check = &v["server_storage_check"];
        assert_eq!(check["writable"], false, "{v}");
        assert_eq!(check["blocker"], local.display().to_string());
        assert!(
            check["problem"]
                .as_str()
                .unwrap()
                .ends_with("isn't writable by you (its permissions do not allow writing)"),
            "{check}"
        );
    }

    /// The guard on every page and API route: loopback Host, the token (bearer or
    /// cookie), and a same-origin `Origin` on cookie-authenticated POSTs.
    #[tokio::test]
    async fn every_route_is_guarded() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let codes = LoginCodes::default();
        let state = Arc::new(AppState::new(
            Role::Gui,
            "secret-token".into(),
            codes.clone(),
            dir.path().join("config.yaml"),
            Some(dir.path().join("processor.yaml")),
        ));
        let app = router(state);
        let send = |method: &str, uri: &str, headers: &[(&str, &str)], body: &str| {
            let mut b = Request::builder().method(method).uri(uri);
            for (k, v) in headers {
                b = b.header(*k, *v);
            }
            let req = b.body(Body::from(body.to_string())).unwrap();
            let app = app.clone();
            async move { app.oneshot(req).await.unwrap() }
        };
        let host = ("host", "127.0.0.1:4567");
        let cookie = ("cookie", "bunko_control_4567=secret-token");
        let json = ("content-type", "application/json");

        // No token: refused, pages and APIs alike.
        for uri in [
            "/app/api/info",
            "/app/api/logs/tail?id=server:x",
            "/app/",
            "/app/settings/logs",
        ] {
            let r = send("GET", uri, &[host], "").await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }
        // A wrong token, or the right one through a non-loopback Host (DNS rebinding).
        let r = send(
            "GET",
            "/app/api/info",
            &[host, ("authorization", "Bearer nope")],
            "",
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let r = send(
            "GET",
            "/app/api/info",
            &[
                ("host", "evil.example:4567"),
                ("authorization", "Bearer secret-token"),
            ],
            "",
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        // The bearer token (the tray) and the cookie (the browser) both work for GETs.
        let r = send(
            "GET",
            "/app/api/info",
            &[host, ("authorization", "Bearer secret-token")],
            "",
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()["cache-control"], "no-store");
        assert_eq!(r.headers()["x-frame-options"], "DENY");
        let r = send("GET", "/app/settings/logs", &[host, cookie], "").await;
        assert_eq!(r.status(), StatusCode::OK);

        // A cookie-authenticated POST needs this listener's own Origin.
        let body = r#"{"url":"/app/"}"#;
        let r = send("POST", "/app/api/handoff", &[host, cookie, json], body).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = send(
            "POST",
            "/app/api/handoff",
            &[host, cookie, json, ("origin", "http://127.0.0.1:9999")],
            body,
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = send(
            "POST",
            "/app/api/handoff",
            &[host, cookie, json, ("origin", "http://127.0.0.1:4567")],
            body,
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        // JSON only: a form post (what a cross-site form could send) is refused.
        let r = send(
            "POST",
            "/app/api/jobs",
            &[
                host,
                ("authorization", "Bearer secret-token"),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            "kind=doctor",
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // Login: a code (single use) sets the per-port cookie and goes to an /app page;
        // the token itself, a wrong code or a used one do not.
        let r = send("GET", "/app/login?c=nope&next=/app/dashboard", &[host], "").await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let r = send("GET", "/app/login?t=secret-token", &[host], "").await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert!(!r.headers().contains_key("set-cookie"));
        let code = codes.mint();
        let r = send(
            "GET",
            &format!("/app/login?c={code}&next=/app/dashboard"),
            &[("host", "evil.example:4567")],
            "",
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let link = format!("/app/login?c={code}&next=//evil.example/");
        let r = send("GET", &link, &[host], "").await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(r.headers()["location"], "/app/");
        let set = r.headers()["set-cookie"].to_str().unwrap().to_string();
        assert!(set.starts_with("bunko_control_4567=secret-token;"), "{set}");
        assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict"));
        let r = send("GET", &link, &[host], "").await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }
}
