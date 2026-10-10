//! `/app/api/...`: the pages' backend. Every route sits behind `super::guard`.

use super::jobs;
use super::setup::{self, ProcessorForm, ServerSetup};
use super::spawn;
use super::{AppState, Role, paths, service, tray};
use axum::Json;
use axum::Router;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

type S = State<Arc<AppState>>;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/app/api/info", get(info))
        .route("/app/api/ping", get(|| async { Json(json!({"ok": true})) }))
        .route("/app/api/instances", get(instances))
        .route(
            "/app/api/instances/{slot}/dashboard",
            get(instance_dashboard),
        )
        .route("/app/api/instances/{slot}/status", get(instance_status))
        .route("/app/api/ocr/request", post(ocr_request))
        .route("/app/api/fs", get(fs_list))
        .route(
            "/app/api/server/config",
            get(server_config).post(server_config_set),
        )
        .route("/app/api/server/setup", post(server_setup))
        .route("/app/api/server/start", post(server_start))
        .route("/app/api/server/health", get(server_health))
        .route("/app/api/ssl", get(ssl_status))
        .route("/app/api/processor/test", post(processor_test))
        .route("/app/api/processor/setup", post(processor_setup))
        .route(
            "/app/api/processor/config",
            get(processor_config).post(processor_config_set),
        )
        .route("/app/api/processor/start", post(processor_start))
        .route("/app/api/processor/status", get(processor_status))
        .route("/app/api/ocr/hardware", get(ocr_hardware))
        .route("/app/api/service", get(service_get).post(service_post))
        .route("/app/api/tray", get(tray_get).post(tray_post))
        .route("/app/api/logs", get(logs_list))
        .route("/app/api/logs/tail", get(logs_tail))
        .route("/app/api/update", get(update_check))
        .route("/app/api/jobs", get(jobs_list).post(job_start))
        .route("/app/api/jobs/{id}", get(job_get))
        .route("/app/api/jobs/{id}/events", get(job_events))
        .route("/app/api/jobs/{id}/cancel", post(job_cancel))
        .route("/app/api/handoff", post(handoff))
}

fn fail(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (status, Json(json!({"error": msg.to_string()}))).into_response()
}

fn bad(msg: impl std::fmt::Display) -> Response {
    fail(StatusCode::BAD_REQUEST, msg)
}

fn ok(v: Value) -> Response {
    Json(v).into_response()
}

#[cfg(not(feature = "ocr"))]
fn lite() -> Response {
    fail(
        StatusCode::NOT_IMPLEMENTED,
        "this is the lite build: OCR and the processor need the full build",
    )
}

/// Blocking work off the async workers.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

fn library_url(state: &AppState) -> Option<(String, bunko_core::Config)> {
    let c = bunko_core::config::load_config(Some(&state.config_path)).ok()?;
    Some((spawn::local_server_url(&c), c))
}

async fn info(State(state): S) -> Response {
    let st = state.clone();
    let v = blocking(move || {
        let config_exists = st.config_path.is_file();
        let processor_exists = st.processor_config.is_file();
        let lib = library_url(&st);
        let kind = service::platform_kind();
        json!({
            "version": bunko_core::VERSION,
            "flavor": crate::FLAVOR,
            "target": bunko_update::TARGET,
            "os": std::env::consts::OS,
            "role": st.role.as_str(),
            "ocr_build": cfg!(feature = "ocr"),
            "config_path": st.config_path,
            "config_exists": config_exists,
            "processor_config": st.processor_config,
            "processor_config_exists": processor_exists,
            "server_storage": paths::server_storage(&st.config_path),
            "processor_storage": paths::processor_storage(&st.processor_config),
            // Can this user write them? (A root-owned ~/.local cannot.) With a
            // writable alternative and the one-line fix for the pages to offer.
            "server_storage_check": paths::storage_check(&paths::server_storage(&st.config_path)),
            "processor_storage_check": paths::storage_check(&paths::processor_storage(&st.processor_config)),
            "library_url": lib.as_ref().map(|l| l.0.clone()),
            "admin_path": lib.as_ref().map(|l| l.1.admin.path.clone()),
            "service_kind": kind.as_str(),
            "service_describe": kind.describe(),
            "service_can_start": service::can_start(kind),
        })
    })
    .await
    .unwrap_or(Value::Null);
    ok(v)
}

/// Running instances on this machine (their `.control.json`), with a link to each
/// one's own dashboard ([`instance_dashboard`]: the browser never gets their token).
async fn instances(State(state): S) -> Response {
    let st = state.clone();
    let found = blocking(move || {
        let mut out = Vec::new();
        for (slot, storage) in [
            ("server", paths::server_storage(&st.config_path)),
            ("processor", paths::processor_storage(&st.processor_config)),
        ] {
            let Some(f) = bunko_control::read_control_file(&storage) else {
                continue;
            };
            let alive = f.pid != 0
                && spawn::pid_alive(f.pid)
                && std::net::TcpStream::connect_timeout(
                    &std::net::SocketAddr::from(([127, 0, 0, 1], f.port)),
                    Duration::from_millis(300),
                )
                .is_ok();
            out.push(json!({
                "role": f.role.as_str(),
                "pid": f.pid,
                "port": f.port,
                "version": f.version,
                "started_at": f.started_at,
                "alive": alive,
                "dashboard": alive.then(|| format!("/app/api/instances/{slot}/dashboard")),
            }));
        }
        out
    })
    .await
    .unwrap_or_default();
    let library = match library_url(&state) {
        Some((url, _)) => json!({"url": url, "up": spawn::healthy(&url).await}),
        None => Value::Null,
    };
    ok(json!({"instances": found, "library": library}))
}

/// `GET /app/api/instances/{server|processor}/dashboard`: sign the browser in to the
/// instance running on that storage. A single-use code is asked of it with its bearer
/// token (from its `.control.json`), and the browser is sent to its
/// `/app/login?c=<code>` on `http://127.0.0.1:<port>`.
async fn instance_dashboard(State(state): S, UrlPath(slot): UrlPath<String>) -> Response {
    let role = match slot.as_str() {
        "server" => Role::Server,
        "processor" => Role::Processor,
        _ => return fail(StatusCode::NOT_FOUND, "no such instance"),
    };
    let storage = role_storage(&state, role);
    let Some(f) = blocking(move || bunko_control::read_control_file(&storage))
        .await
        .flatten()
    else {
        return fail(StatusCode::NOT_FOUND, "that instance is not running");
    };
    match login_code(f.port, &f.token).await {
        Some(code) => axum::response::Redirect::to(&format!(
            "http://127.0.0.1:{}/app/login?c={code}&next=/app/dashboard",
            f.port
        ))
        .into_response(),
        None => fail(
            StatusCode::BAD_GATEWAY,
            "that instance did not answer; open its dashboard from the tray",
        ),
    }
}

/// `GET /app/api/instances/{server|processor}/status`: the running instance's
/// `/control/status` (the setup's last page shows its OCR backend install).
async fn instance_status(State(state): S, UrlPath(slot): UrlPath<String>) -> Response {
    let role = match slot.as_str() {
        "server" => Role::Server,
        "processor" => Role::Processor,
        _ => return fail(StatusCode::NOT_FOUND, "no such instance"),
    };
    let storage = role_storage(&state, role);
    let Some(f) = blocking(move || bunko_control::read_control_file(&storage))
        .await
        .flatten()
    else {
        return fail(StatusCode::NOT_FOUND, "that instance is not running");
    };
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
    else {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "no HTTP client");
    };
    match client
        .get(format!("http://127.0.0.1:{}/control/status", f.port))
        .bearer_auth(&f.token)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.json::<Value>().await {
            Ok(v) => ok(v),
            Err(e) => fail(StatusCode::BAD_GATEWAY, e),
        },
        _ => fail(StatusCode::BAD_GATEWAY, "that instance did not answer"),
    }
}

/// The OCR step's choices (`POST /app/api/ocr/request`).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OcrRequestBody {
    role: Option<String>,
    variant: Option<String>,
    from: Option<String>,
    no_models: bool,
    force: bool,
}

/// `POST /app/api/ocr/request`: keep the setup's OCR choices for the background
/// install the started server or processor runs (`<backends>/.install-request.json`;
/// it removes the file once an install with them succeeded).
async fn ocr_request(State(state): S, Json(b): Json<OcrRequestBody>) -> Response {
    #[cfg(feature = "ocr")]
    {
        let role = match role_of(b.role.as_deref()) {
            Ok(r) => r,
            Err(e) => return *e,
        };
        let storage = role_storage(&state, role);
        let backends =
            bunko_engines::EngineConfig::new(storage.join("models"), bunko_engines::Backend::Auto)
                .backends_dir();
        let req = crate::ocr_install::InstallRequest {
            variant: b.variant.filter(|v| !v.is_empty() && v != "auto"),
            from: b
                .from
                .filter(|f| !f.trim().is_empty())
                .map(|f| PathBuf::from(f.trim())),
            no_models: b.no_models,
            force: b.force,
        };
        if let Some(v) = &req.variant
            && !["cpu", "cu130", "rocm7.1"].contains(&v.as_str())
        {
            return bad(format!("variant: {v}?"));
        }
        let plain = req == crate::ocr_install::InstallRequest::default();
        let path = crate::ocr_install::InstallRequest::path(&backends);
        let written = blocking(move || {
            if plain {
                // Nothing beyond what the configuration decides.
                match std::fs::remove_file(&path) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                    _ => Ok(None),
                }
            } else {
                req.write(&backends)
                    .map(|()| Some(path))
                    .map_err(|e| e.to_string())
            }
        })
        .await;
        match written {
            Some(Ok(p)) => ok(json!({"saved": p})),
            Some(Err(e)) => bad(e),
            None => fail(StatusCode::INTERNAL_SERVER_ERROR, "could not save"),
        }
    }
    #[cfg(not(feature = "ocr"))]
    {
        let _ = (state, b);
        lite()
    }
}

/// When a tray runs for this user, the setup hands it the instance it starts: `role`
/// goes into tray.json (the tray notices within seconds, starts it and supervises it;
/// its Quit stops it). Returns what was done, None without a tray.
async fn hand_to_tray(state: &AppState, role: Role) -> Option<String> {
    let exe = state.exe.clone();
    let cfg = service_config(state, role);
    blocking(move || -> Option<String> {
        if tray::tray_command(&exe).is_none() || !tray::tray_running(&exe) {
            return None;
        }
        let path = tray::config_path(&exe);
        let mut conf = tray::load(&path).ok()?;
        let entry = tray::entry(role, &cfg);
        if conf.managed.contains(&entry) {
            // Listed already: the tray starts it unless something else runs it.
            return Some(format!("the tray runs the {} ({})", role.as_str(), path.display()));
        }
        tray::set_role(&mut conf, role.as_str(), Some(entry));
        tray::save(&path, &conf).ok()?;
        Some(format!(
            "Handed the {} to the running tray ({}): it starts it, restarts it if it stops, and Quit stops it.",
            role.as_str(),
            path.display()
        ))
    })
    .await
    .flatten()
}

/// A sign-in code from the control listener on `port` (`POST /control/login-code`).
async fn login_code(port: u16, token: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
        .ok()?;
    let resp = client
        .post(format!("http://127.0.0.1:{port}/control/login-code"))
        .bearer_auth(token)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    let code = v["code"].as_str()?;
    (code.len() == 32 && code.bytes().all(|b| b.is_ascii_hexdigit())).then(|| code.to_string())
}

#[derive(Deserialize)]
struct FsQuery {
    path: Option<String>,
    /// Comma-separated extensions of files to list too (`pem,crt`).
    exts: Option<String>,
}

/// The folder picker: subfolders (and chosen files) of a folder on this machine.
async fn fs_list(Query(q): Query<FsQuery>) -> Response {
    let dir = q
        .path
        .filter(|p| !p.trim().is_empty())
        .map(|p| bunko_core::storage::expand_user(Path::new(p.trim())))
        .unwrap_or_else(bunko_core::storage::home_dir);
    let exts: Vec<String> = q
        .exts
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().trim_start_matches('.').to_string())
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric()))
        .collect();
    match blocking(move || setup::list_dir(&dir, &exts)).await {
        Some(Ok(v)) => ok(v),
        Some(Err(e)) => bad(e),
        None => fail(StatusCode::INTERNAL_SERVER_ERROR, "listing failed"),
    }
}

/// config.yaml: the file's own values (what is saved), the effective values (with
/// `MOKURO_*` applied) and the keys `config set` accepts.
async fn server_config(State(state): S) -> Response {
    let path = state.config_path.clone();
    let v = blocking(move || {
        let file = crate::cfgfile::load_for_write(&path);
        let effective = bunko_core::config::load_config(Some(&path));
        let env: Vec<String> = std::env::vars()
            .map(|(k, _)| k)
            .filter(|k| k.starts_with("MOKURO_"))
            .collect();
        match (file, effective) {
            (Ok(f), Ok(e)) => Ok(json!({
                "path": path,
                "exists": path.is_file(),
                "file": f.to_value(),
                "effective": e.to_value(),
                "yaml": e.to_yaml(),
                "warnings": e.warnings,
                "keys": bunko_core::Config::KEYS,
                "env": env,
                "ocr_build": cfg!(feature = "ocr"),
            })),
            (Err(e), _) | (_, Err(e)) => Err(e.to_string()),
        }
    })
    .await;
    match v {
        Some(Ok(v)) => ok(v),
        Some(Err(e)) => bad(e),
        None => fail(StatusCode::INTERNAL_SERVER_ERROR, "read failed"),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ConfigSet {
    /// Dotted key → value text, as `config set` takes them.
    set: serde_json::Map<String, Value>,
    cors_add: Option<String>,
    cors_remove: Option<String>,
    /// `config init`: write a default file (`force` over an existing one).
    init: bool,
    force: bool,
}

async fn server_config_set(State(state): S, Json(body): Json<ConfigSet>) -> Response {
    let path = state.config_path.clone();
    let res = blocking(move || -> Result<Value, String> {
        if body.init {
            if path.exists() && !body.force {
                return Err(format!("{} already exists", path.display()));
            }
            crate::cfgfile::save(&bunko_core::Config::default(), &path)
                .map_err(|e| e.to_string())?;
            return Ok(json!({"saved": path, "changed": ["(defaults)"]}));
        }
        let mut c = crate::cfgfile::load_for_write(&path).map_err(|e| e.to_string())?;
        let mut changed = Vec::new();
        for (k, v) in &body.set {
            if !bunko_core::Config::KEYS.contains(&k.as_str()) {
                return Err(format!("{k}: no such setting"));
            }
            let text = match v {
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                Value::Array(_) | Value::Object(_) => v.to_string(),
                Value::Null => String::new(),
            };
            c.set_by_dotted_key(k, &text)
                .map_err(|e| format!("{k}: {e}"))?;
            changed.push(k.clone());
        }
        if let Some(o) = body
            .cors_add
            .as_deref()
            .map(str::trim)
            .filter(|o| !o.is_empty())
        {
            if !c.cors.allowed_origins.iter().any(|x| x == o) {
                c.cors.allowed_origins.push(o.to_string());
            }
            changed.push("cors.allowed_origins".into());
        }
        if let Some(o) = body.cors_remove.as_deref().map(str::trim) {
            c.cors.allowed_origins.retain(|x| x != o);
            changed.push("cors.allowed_origins".into());
        }
        crate::cfgfile::save(&c, &path).map_err(|e| e.to_string())?;
        Ok(json!({"saved": path, "changed": changed, "restart_needed": true}))
    })
    .await;
    match res {
        Some(Ok(v)) => ok(v),
        Some(Err(e)) => bad(e),
        None => fail(StatusCode::INTERNAL_SERVER_ERROR, "save failed"),
    }
}

async fn server_setup(State(state): S, Json(form): Json<ServerSetup>) -> Response {
    let path = state.config_path.clone();
    match blocking(move || setup::write_server(&form, &path)).await {
        Some(Ok(v)) => ok(v),
        Some(Err(e)) => bad(e),
        None => fail(StatusCode::INTERNAL_SERVER_ERROR, "setup failed"),
    }
}

/// Start `serve` in the background and wait for it to answer.
async fn server_start(State(state): S) -> Response {
    let Some((url, config)) = library_url(&state) else {
        return bad(format!(
            "{} does not load: run the setup first",
            state.config_path.display()
        ));
    };
    if spawn::healthy(&url).await {
        return ok(json!({"url": url, "already_running": true, "up": true}));
    }
    let log = spawn::stdout_log(&config.storage.base_path, "serve-console.log");
    // A running tray takes it over (tray.json); it starts it within seconds.
    let tray = hand_to_tray(&state, Role::Server).await;
    if let Some(note) = &tray
        && spawn::wait_healthy(&url, Duration::from_secs(25)).await
    {
        return ok(json!({"url": url, "up": true, "by_tray": true, "note": note}));
    }
    let args = vec![
        "-c".to_string(),
        state.config_path.display().to_string(),
        "serve".to_string(),
    ];
    // Started for the tray when there is one (it adopts it: Quit stops it).
    let envs: &[(&str, &str)] = if tray.is_some() {
        &[(bunko_control::MANAGED_ENV, "1")]
    } else {
        &[]
    };
    let pid = match spawn::spawn_detached_env(&state.exe, &args, &log, envs) {
        Ok(p) => p,
        Err(e) => return bad(format!("could not start the server: {e}")),
    };
    let up = spawn::wait_healthy(&url, Duration::from_secs(45)).await;
    let tail = if up {
        String::new()
    } else {
        spawn::tail_file(&log, 30).unwrap_or_default()
    };
    ok(json!({"url": url, "pid": pid, "up": up, "log": log, "output": tail}))
}

async fn server_health(State(state): S) -> Response {
    match library_url(&state) {
        Some((url, _)) => ok(json!({"url": url, "up": spawn::healthy(&url).await})),
        None => ok(json!({"url": null, "up": false})),
    }
}

/// HTTPS: what the config says and the certificate's details.
async fn ssl_status(State(state): S) -> Response {
    let path = state.config_path.clone();
    let v = blocking(move || {
        let c = bunko_core::config::load_config(Some(&path)).unwrap_or_default();
        let (cert, key) = if c.ssl.auto_cert || c.ssl.cert_file.is_empty() {
            let (c2, k2) = bunko_server::tls::default_cert_paths();
            (c2, k2)
        } else {
            (
                PathBuf::from(&c.ssl.cert_file),
                PathBuf::from(&c.ssl.key_file),
            )
        };
        let details = if cert.is_file() {
            crate::cmd::ssl::describe_cert(&cert).unwrap_or_else(|e| vec![e])
        } else {
            vec![]
        };
        json!({"enabled": c.ssl.enabled, "auto_cert": c.ssl.auto_cert,
               "cert_file": cert, "key_file": key, "cert_exists": cert.is_file(),
               "details": details})
    })
    .await
    .unwrap_or(Value::Null);
    ok(v)
}

async fn processor_test(Json(form): Json<ProcessorForm>) -> Response {
    #[cfg(feature = "ocr")]
    {
        match setup::test_connection(&form).await {
            Ok(v) => ok(v),
            Err(e) => bad(e),
        }
    }
    #[cfg(not(feature = "ocr"))]
    {
        let _ = form;
        lite()
    }
}

async fn processor_setup(State(state): S, Json(form): Json<ProcessorForm>) -> Response {
    #[cfg(feature = "ocr")]
    {
        match setup::write_processor(&form, &state.processor_config).await {
            Ok(v) => ok(v),
            Err(e) => bad(e),
        }
    }
    #[cfg(not(feature = "ocr"))]
    {
        let _ = (form, state);
        lite()
    }
}

async fn processor_config(State(state): S) -> Response {
    #[cfg(feature = "ocr")]
    {
        let p = state.processor_config.clone();
        match blocking(move || setup::read_processor(&p)).await {
            Some(Ok(v)) => ok(v),
            Some(Err(e)) => bad(e),
            None => fail(StatusCode::INTERNAL_SERVER_ERROR, "read failed"),
        }
    }
    #[cfg(not(feature = "ocr"))]
    {
        let _ = state;
        lite()
    }
}

async fn processor_config_set(State(state): S, Json(form): Json<ProcessorForm>) -> Response {
    #[cfg(feature = "ocr")]
    {
        match setup::update_processor(&form, &state.processor_config).await {
            Ok(v) => ok(v),
            Err(e) => bad(e),
        }
    }
    #[cfg(not(feature = "ocr"))]
    {
        let _ = (form, state);
        lite()
    }
}

/// Start `processor serve` in the background; wait for its first status.
async fn processor_start(State(state): S) -> Response {
    #[cfg(feature = "ocr")]
    {
        let cfg = state.processor_config.clone();
        let loaded = match bunko_processor::load_processor_config(&cfg) {
            Ok(c) => c,
            Err(e) => return bad(format!("{}: {e}", cfg.display())),
        };
        let storage = loaded.processor.storage.clone();
        let before = bunko_processor::status::read_status(&storage)
            .get("updated_at")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let log = spawn::stdout_log(&storage, "processor-console.log");
        // A running tray takes it over (tray.json); it starts it within seconds.
        let tray = hand_to_tray(&state, Role::Processor).await;
        if let Some(note) = &tray {
            let end = tokio::time::Instant::now() + Duration::from_secs(25);
            while tokio::time::Instant::now() < end {
                if let Some(f) = bunko_control::read_control_file(&storage)
                    && spawn::pid_alive(f.pid)
                {
                    let status = bunko_processor::status::read_status(&storage);
                    return ok(json!({"pid": f.pid, "by_tray": true, "note": note,
                        "state": status.get("state").and_then(Value::as_str).unwrap_or("starting"),
                        "status": status, "running": true, "log": log, "output": ""}));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        let args = vec![
            "processor".to_string(),
            "serve".to_string(),
            "--config".to_string(),
            cfg.display().to_string(),
        ];
        let envs: &[(&str, &str)] = if tray.is_some() {
            &[(bunko_control::MANAGED_ENV, "1")]
        } else {
            &[]
        };
        let pid = match spawn::spawn_detached_env(&state.exe, &args, &log, envs) {
            Ok(p) => p,
            Err(e) => return bad(format!("could not start the processor: {e}")),
        };
        let end = tokio::time::Instant::now() + Duration::from_secs(60);
        let mut status = serde_json::Map::new();
        while tokio::time::Instant::now() < end {
            status = bunko_processor::status::read_status(&storage);
            let fresh = status
                .get("updated_at")
                .and_then(Value::as_f64)
                .is_some_and(|t| t > before);
            if fresh && status.get("state").and_then(Value::as_str) != Some("stopped") {
                break;
            }
            if !spawn::pid_alive(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let state_s = status
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("starting")
            .to_string();
        let output = if state_s == "connected" {
            String::new()
        } else {
            spawn::tail_file(&log, 30).unwrap_or_default()
        };
        ok(json!({"pid": pid, "state": state_s, "status": status,
                  "running": spawn::pid_alive(pid), "log": log, "output": output}))
    }
    #[cfg(not(feature = "ocr"))]
    {
        let _ = state;
        lite()
    }
}

async fn processor_status(State(state): S) -> Response {
    let storage = paths::processor_storage(&state.processor_config);
    #[cfg(feature = "ocr")]
    {
        let s = bunko_processor::status::read_status(&storage);
        ok(
            json!({"status": s, "line": bunko_processor::status::describe(&s),
                  "storage": storage}),
        )
    }
    #[cfg(not(feature = "ocr"))]
    {
        let _ = storage;
        lite()
    }
}

/// `install-ocr --list` as data: the hardware, the pack it would pick, what is
/// installed for each role.
async fn ocr_hardware(State(state): S) -> Response {
    #[cfg(feature = "ocr")]
    {
        let st = state.clone();
        let v = blocking(move || {
            let hw = crate::hwdetect::detect();
            let choice = crate::hwdetect::choose(&hw, bunko_update::TARGET);
            let mut packs = Vec::new();
            let roots = [
                (
                    "server",
                    paths::server_storage(&st.config_path).join("backends"),
                ),
                (
                    "processor",
                    paths::processor_storage(&st.processor_config).join("backends"),
                ),
            ];
            let mut seen: Vec<PathBuf> = Vec::new();
            for (role, dir) in roots
                .into_iter()
                .chain(crate::cmd::install_ocr::bundled_dir().map(|d| ("bundled", d)))
            {
                for (p, m) in bunko_update::backend::installed(&dir) {
                    if seen.contains(&p) {
                        continue;
                    }
                    seen.push(p.clone());
                    packs.push(json!({"role": role, "dir": p, "name": m.name,
                        "variant": m.variant, "target": m.target,
                        "version": m.bunko_version,
                        "complete": crate::cmd::install_ocr::pack_complete(&p, &m)}));
                }
            }
            json!({
                "target": bunko_update::TARGET,
                "nvidia_driver": hw.nvidia_driver,
                "nvidia_gpus": hw.nvidia_gpus,
                "amd_gfx": hw.amd_gfx,
                "hidden": hw.hidden,
                "auto_variant": choice.variant,
                "reason": choice.reason,
                "hint": choice.hint,
                "variants": ["cpu", "cu130", "rocm7.1"],
                "packs": packs,
                // The offline OCR files shipped with this app (macOS disk image):
                // install-ocr takes them when no folder is given.
                "bundled_offline": crate::cmd::install_ocr::bundled_offline_dir(),
                "engines": bunko_engines::models::ENGINES,
            })
        })
        .await
        .unwrap_or(Value::Null);
        ok(v)
    }
    #[cfg(not(feature = "ocr"))]
    {
        let _ = state;
        lite()
    }
}

#[derive(Deserialize)]
struct RoleQuery {
    role: Option<String>,
}

fn role_of(s: Option<&str>) -> Result<Role, Box<Response>> {
    match s.unwrap_or("server") {
        "server" => Ok(Role::Server),
        "processor" => Ok(Role::Processor),
        other => Err(Box::new(bad(format!("role: {other}?")))),
    }
}

fn service_config(state: &AppState, role: Role) -> PathBuf {
    match role {
        Role::Processor => state.processor_config.clone(),
        _ => state.config_path.clone(),
    }
}

async fn service_get(State(state): S, Query(q): Query<RoleQuery>) -> Response {
    let role = match role_of(q.role.as_deref()) {
        Ok(r) => r,
        Err(e) => return *e,
    };
    let st = state.clone();
    let v = blocking(move || {
        let cfg = service_config(&st, role);
        match service::render(role, &cfg, &st.exe) {
            Ok(r) => {
                let (written, enabled) = service::state(&r);
                let tray_manages = tray::load(&tray::config_path(&st.exe))
                    .is_ok_and(|c| tray::manages(&c, role.as_str()));
                Ok(json!({"role": role.as_str(), "kind": r.kind.as_str(),
                    "describe": r.kind.describe(), "path": r.path, "name": r.name,
                    "text": r.text, "written": written, "enabled": enabled,
                    "can_start": service::can_start(r.kind),
                    "tray_manages": tray_manages,
                    "config": cfg, "config_exists": cfg.is_file()}))
            }
            Err(e) => Err(e),
        }
    })
    .await;
    match v {
        Some(Ok(v)) => ok(v),
        Some(Err(e)) => bad(e),
        None => fail(StatusCode::INTERNAL_SERVER_ERROR, "failed"),
    }
}

#[derive(Deserialize)]
struct ServiceBody {
    role: Option<String>,
    /// `install` or `remove`.
    action: String,
    /// With `install`: also enable and start it now.
    #[serde(default)]
    start: bool,
    /// With `install`: take the role out of tray.json first (and stop the copy the
    /// tray runs), so two copies never run.
    #[serde(default)]
    remove_tray: bool,
}

async fn service_post(State(state): S, Json(b): Json<ServiceBody>) -> Response {
    let role = match role_of(b.role.as_deref()) {
        Ok(r) => r,
        Err(e) => return *e,
    };
    let mut before = Vec::new();
    if b.action == "install" && b.remove_tray {
        match untray(&state, role).await {
            Ok(m) => before = m,
            Err(e) => return bad(e),
        }
    }
    let st = state.clone();
    let v = blocking(move || -> Result<Value, String> {
        let cfg = service_config(&st, role);
        if b.action == "install" && !cfg.is_file() {
            return Err(format!(
                "{} does not exist yet: finish the {} setup first",
                cfg.display(),
                if role == Role::Processor {
                    "processor"
                } else {
                    "library server"
                }
            ));
        }
        let r = service::render(role, &cfg, &st.exe)?;
        let mut messages = before;
        messages.extend(match b.action.as_str() {
            "install" => service::install(&r, b.start)?,
            "remove" => service::remove(&r)?,
            other => return Err(format!("action: {other}?")),
        });
        let (written, enabled) = service::state(&r);
        Ok(
            json!({"messages": messages, "path": r.path, "written": written,
                  "enabled": enabled}),
        )
    })
    .await;
    match v {
        Some(Ok(v)) => ok(v),
        Some(Err(e)) => bad(e),
        None => fail(StatusCode::INTERNAL_SERVER_ERROR, "failed"),
    }
}

/// The storage a role's running instance keeps its `.control.json` in.
fn role_storage(state: &AppState, role: Role) -> PathBuf {
    match role {
        Role::Processor => paths::processor_storage(&state.processor_config),
        _ => paths::server_storage(&state.config_path),
    }
}

/// Stop the copy of `role` the tray started (`POST /control/stop` answers only for
/// tray-managed instances), and wait for it to exit. A message when one was stopped.
async fn stop_tray_instance(state: &AppState, role: Role) -> Option<String> {
    let f = bunko_control::read_control_file(&role_storage(state, role))?;
    if !f.managed || !spawn::pid_alive(f.pid) {
        return None;
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
        .ok()?;
    let sent = client
        .post(format!("http://127.0.0.1:{}/control/stop", f.port))
        .bearer_auth(&f.token)
        .send()
        .await
        .is_ok_and(|r| r.status().is_success());
    if !sent {
        return Some(format!(
            "Could not ask the tray's {} (pid {}) to stop; quit it from the tray menu.",
            role.as_str(),
            f.pid
        ));
    }
    for _ in 0..60 {
        if !spawn::pid_alive(f.pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Some(format!(
        "Stopped the {} the tray was running (pid {}).",
        role.as_str(),
        f.pid
    ))
}

/// Start the tray (`mokuro-bunko tray`; Windows: the app `mokuro-bunko.exe`) in the background;
/// its pid.
fn start_tray(state: &AppState, tray: &(PathBuf, Vec<String>)) -> Result<u32, String> {
    let log = paths::server_storage(&state.config_path)
        .join("logs")
        .join("tray-start-console.log");
    spawn::spawn_detached(&tray.0, &tray.1, &log)
        .map_err(|e| format!("could not start {}: {e}", tray.0.display()))
}

/// Take `role` out of tray.json and stop the copy the tray runs. A running tray reads
/// tray.json only when it starts (and would start the copy again), so it is stopped
/// first and started again afterwards.
async fn untray(state: &Arc<AppState>, role: Role) -> Result<Vec<String>, String> {
    let path = tray::config_path(&state.exe);
    let mut conf = tray::load(&path)?;
    let mut messages = Vec::new();
    if !tray::manages(&conf, role.as_str()) {
        return Ok(messages);
    }
    tray::set_role(&mut conf, role.as_str(), None);
    tray::save(&path, &conf)?;
    messages.push(format!(
        "The tray no longer runs the {} ({}).",
        role.as_str(),
        path.display()
    ));
    let st = state.clone();
    let stopped = blocking(move || tray::stop_tray(&st.exe))
        .await
        .unwrap_or_else(|| Err("could not stop the tray".into()))?;
    if let Some(m) = &stopped {
        messages.push(m.clone());
    }
    if let Some(m) = stop_tray_instance(state, role).await {
        messages.push(m);
    }
    if stopped.is_some()
        && let Some(cmd) = tray::tray_command(&state.exe)
    {
        let pid = start_tray(state, &cmd)?;
        messages.push(format!(
            "Started the tray again (pid {pid}) with the new tray.json."
        ));
    }
    Ok(messages)
}

async fn tray_get(State(state): S) -> Response {
    let st = state.clone();
    let v = blocking(move || {
        let exe = tray::tray_command(&st.exe).map(|(p, _)| p);
        let path = tray::config_path(&st.exe);
        let conf = tray::load(&path);
        let headless = tray::headless();
        let autostart = tray::autostart_path();
        json!({
            "tray_exe": exe,
            "available": exe.is_some(),
            "headless": headless,
            "recommended": if exe.is_some() && !headless { "tray" } else { "service" },
            "config_path": path,
            "managed": conf.as_ref().map(|c| c.managed.clone()).unwrap_or_default(),
            "config_error": conf.err(),
            "autostart_path": autostart,
            "autostart": autostart.as_ref().is_some_and(|p| p.exists()),
            "running": tray::tray_running(&st.exe),
        })
    })
    .await
    .unwrap_or(Value::Null);
    ok(v)
}

#[derive(Deserialize)]
struct TrayBody {
    role: Option<String>,
    /// `enable` (the tray runs this role) or `disable`.
    action: String,
    /// Add (true) / leave alone (absent) / remove (false) the tray's login item.
    #[serde(default)]
    autostart: Option<bool>,
    /// Start the tray now when it is not running.
    #[serde(default)]
    start_now: bool,
    /// Remove this role's service first, so two copies never run.
    #[serde(default)]
    remove_service: bool,
}

async fn tray_post(State(state): S, Json(b): Json<TrayBody>) -> Response {
    let role = match role_of(b.role.as_deref()) {
        Ok(r) => r,
        Err(e) => return *e,
    };
    let cfg = service_config(&state, role);
    if b.action == "disable" {
        let mut messages = match untray(&state, role).await {
            Ok(m) => m,
            Err(e) => return bad(e),
        };
        if b.autostart == Some(false) {
            match tray::set_autostart(Path::new(""), false) {
                Ok(Some(p)) => messages.push(format!("Removed {}", p.display())),
                Ok(None) => {}
                Err(e) => return bad(e),
            }
        }
        return ok(json!({"messages": messages}));
    }
    if b.action != "enable" {
        return bad(format!("action: {}?", b.action));
    }
    if !cfg.is_file() {
        return bad(format!(
            "{} does not exist yet: finish the {} setup first",
            cfg.display(),
            if role == Role::Processor {
                "processor"
            } else {
                "library server"
            }
        ));
    }
    let Some(tray_cmd) = tray::tray_command(&state.exe) else {
        return bad(format!(
            "{} has no tray (the lite build): use a service instead",
            state.exe.display()
        ));
    };
    let tray_exe = tray_cmd.0.clone();
    let st = state.clone();
    let v = blocking(move || -> Result<Value, String> {
        let mut messages = Vec::new();
        if b.remove_service {
            let r = service::render(role, &cfg, &st.exe)?;
            if service::state(&r).0 {
                messages.extend(service::remove(&r)?);
            }
        }
        let path = tray::config_path(&st.exe);
        let mut conf = tray::load(&path)?;
        tray::set_role(&mut conf, role.as_str(), Some(tray::entry(role, &cfg)));
        tray::save(&path, &conf)?;
        messages.push(format!(
            "Wrote {}: the tray runs the {} and restarts it if it stops.",
            path.display(),
            role.as_str()
        ));
        if let Some(on) = b.autostart
            && let Some(p) = tray::set_autostart(&st.exe, on)?
        {
            messages.push(format!(
                "{} {}",
                if on {
                    "The tray starts when you log in:"
                } else {
                    "Removed"
                },
                p.display()
            ));
        }
        let mut started = None;
        if b.start_now {
            // A running tray reads tray.json only when it starts: restart it.
            if let Some(m) = tray::stop_tray(&st.exe)? {
                messages.push(m);
            }
            let pid = start_tray(&st, &tray_cmd)?;
            started = Some(pid);
            messages.push(format!("Started the tray (pid {pid})."));
        } else if tray::tray_running(&st.exe) {
            messages.push(
                "The running tray reads tray.json when it starts: choose Quit in its menu and start it again."
                    .into(),
            );
        }
        Ok(json!({"messages": messages, "config_path": path, "tray_exe": tray_exe,
                  "started": started, "autostart_path": tray::autostart_path()}))
    })
    .await;
    match v {
        Some(Ok(v)) => ok(v),
        Some(Err(e)) => bad(e),
        None => fail(StatusCode::INTERNAL_SERVER_ERROR, "failed"),
    }
}

/// The log directories the pages may read: `<storage>/logs` of each role.
fn log_dirs(state: &AppState) -> Vec<(&'static str, PathBuf)> {
    vec![
        (
            "server",
            paths::server_storage(&state.config_path).join("logs"),
        ),
        (
            "processor",
            paths::processor_storage(&state.processor_config).join("logs"),
        ),
    ]
}

async fn logs_list(State(state): S) -> Response {
    let st = state.clone();
    let v = blocking(move || {
        let mut files = Vec::new();
        for (role, dir) in log_dirs(&st) {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let Ok(md) = e.metadata() else { continue };
                if !md.is_file() {
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                if !paths::plain_file_name(&name) {
                    continue;
                }
                let modified = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                files.push(json!({"id": format!("{role}:{name}"), "role": role,
                    "name": name, "size": md.len(), "modified": modified,
                    "path": dir.join(&name)}));
            }
        }
        files.sort_by(|a, b| b["modified"].as_u64().cmp(&a["modified"].as_u64()));
        let dirs: Vec<Value> = log_dirs(&st)
            .into_iter()
            .map(|(r, d)| json!({"role": r, "dir": d}))
            .collect();
        json!({"files": files, "dirs": dirs})
    })
    .await
    .unwrap_or(Value::Null);
    ok(v)
}

#[derive(Deserialize)]
struct TailQuery {
    id: String,
    lines: Option<usize>,
}

/// The end of one log file, named by the id `logs_list` gave it. The file must be a
/// regular file directly in that role's log directory (no `..`, no symlink out).
pub fn resolve_log(dirs: &[(&str, PathBuf)], id: &str) -> Result<PathBuf, String> {
    let (role, name) = id.split_once(':').ok_or("no such log")?;
    if !paths::plain_file_name(name) {
        return Err("no such log".into());
    }
    let dir = dirs
        .iter()
        .find(|(r, _)| *r == role)
        .map(|(_, d)| d)
        .ok_or("no such log")?;
    let dir = std::fs::canonicalize(dir).map_err(|_| "no such log")?;
    let file = std::fs::canonicalize(dir.join(name)).map_err(|_| "no such log")?;
    if file.parent() != Some(dir.as_path()) || !file.is_file() {
        return Err("no such log".into());
    }
    Ok(file)
}

async fn logs_tail(State(state): S, Query(q): Query<TailQuery>) -> Response {
    let st = state.clone();
    let v = blocking(move || {
        let file = resolve_log(&log_dirs(&st), &q.id)?;
        let n = q.lines.unwrap_or(300).clamp(1, 5000);
        spawn::tail_file(&file, n)
            .map(|text| json!({"id": q.id, "path": file, "text": text}))
            .map_err(|e| e.to_string())
    })
    .await;
    match v {
        Some(Ok(v)) => ok(v),
        Some(Err(e)) => fail(StatusCode::NOT_FOUND, e),
        None => fail(StatusCode::INTERNAL_SERVER_ERROR, "failed"),
    }
}

async fn update_check(State(state): S) -> Response {
    let c = bunko_core::config::load_config(Some(&state.config_path)).unwrap_or_default();
    let updater = bunko_update::Updater::new(
        c.update.manifest_url.clone(),
        c.update.channel.clone(),
        crate::update_flavor(),
    );
    let status = updater.check().await;
    ok(json!({
        "current": status.current,
        "latest": status.latest,
        "available": status.available,
        "notes_url": status.notes_url,
        "install": crate::cmd::update::describe_install(&status.install),
        "can_apply": status.can_apply,
        "docker_image": status.docker_image,
        "error": status.error,
        "channel": updater.channel(),
        "channel_setting": c.update.channel,
        "check": c.update.check,
    }))
}

async fn jobs_list(State(state): S) -> Response {
    let list: Vec<Value> = state.jobs.all().iter().map(|j| j.summary()).collect();
    ok(json!({"jobs": list}))
}

async fn job_get(State(state): S, UrlPath(id): UrlPath<u64>) -> Response {
    match state.jobs.get(id) {
        Some(j) => {
            let mut v = j.summary();
            v["output"] = json!(j.output());
            ok(v)
        }
        None => fail(StatusCode::NOT_FOUND, "no such job"),
    }
}

#[derive(Deserialize)]
struct FromQuery {
    from: Option<u64>,
}

async fn job_events(
    State(state): S,
    UrlPath(id): UrlPath<u64>,
    Query(q): Query<FromQuery>,
) -> Response {
    match state.jobs.get(id) {
        Some(j) => jobs::events(j, q.from.unwrap_or(0)).into_response(),
        None => fail(StatusCode::NOT_FOUND, "no such job"),
    }
}

async fn job_cancel(State(state): S, UrlPath(id): UrlPath<u64>) -> Response {
    match state.jobs.get(id) {
        Some(j) => {
            j.cancel();
            ok(j.summary())
        }
        None => fail(StatusCode::NOT_FOUND, "no such job"),
    }
}

/// A job request: which CLI command, with which options.
#[derive(Deserialize, Default, Debug)]
#[serde(default)]
pub struct JobRequest {
    pub kind: String,
    pub processor: bool,
    pub variant: Option<String>,
    pub from: Option<String>,
    pub dir: Option<String>,
    pub no_models: bool,
    pub force: bool,
    pub engine: Option<String>,
    pub auto_cert: bool,
    pub cert: Option<String>,
    pub key: Option<String>,
    pub hostname: Option<String>,
    pub days: Option<u32>,
}

fn existing_dir(p: &str, what: &str) -> Result<String, String> {
    let p = bunko_core::storage::expand_user(Path::new(p.trim()));
    if !p.is_absolute() || !p.is_dir() {
        return Err(format!("{what}: {} is not a folder", p.display()));
    }
    Ok(p.display().to_string())
}

fn existing_file(p: &str, what: &str) -> Result<String, String> {
    let p = bunko_core::storage::expand_user(Path::new(p.trim()));
    if !p.is_absolute() || !p.is_file() {
        return Err(format!("{what}: {} is not a file", p.display()));
    }
    Ok(p.display().to_string())
}

/// The CLI arguments (after the global `-c`) and title of a job. Every value is
/// checked; nothing goes through a shell.
pub fn job_args(r: &JobRequest, processor_config: &Path) -> Result<(Vec<String>, String), String> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    let proc_flag = |mut a: Vec<String>| {
        if r.processor {
            a.push("--processor".into());
        }
        a
    };
    let pc = processor_config.display().to_string();
    Ok(match r.kind.as_str() {
        "doctor" => (proc_flag(s(&["doctor"])), "Diagnostics".into()),
        "install-ocr" | "install-ocr-list" => {
            let mut a = s(&["install-ocr"]);
            if r.kind == "install-ocr-list" {
                a.push("--list".into());
            }
            if let Some(v) = r.variant.as_deref().filter(|v| !v.is_empty()) {
                if !["auto", "cpu", "cu130", "rocm7.1"].contains(&v) {
                    return Err(format!("variant: {v}?"));
                }
                a.extend(s(&["--variant", v]));
            }
            if let Some(f) = r.from.as_deref().filter(|f| !f.trim().is_empty()) {
                a.push("--from".into());
                a.push(existing_dir(f, "install from")?);
            }
            if let Some(d) = r.dir.as_deref().filter(|d| !d.trim().is_empty()) {
                let d = bunko_core::storage::expand_user(Path::new(d.trim()));
                if !d.is_absolute() {
                    return Err("install into: give a full path".into());
                }
                a.push("--dir".into());
                a.push(d.display().to_string());
            }
            if r.no_models {
                a.push("--no-models".into());
            }
            if r.force {
                a.push("--force".into());
            }
            (proc_flag(a), "Install the OCR backend".into())
        }
        "models-list" => (proc_flag(s(&["models", "list"])), "Models".into()),
        "models-verify" => (
            proc_flag(s(&["models", "verify"])),
            "Verify the models".into(),
        ),
        "models-download" => {
            let mut a = s(&["models", "download"]);
            if let Some(e) = r.engine.as_deref().filter(|e| !e.is_empty()) {
                if !["hayai-nova", "paddle-manga", "ppocr-manga"].contains(&e) {
                    return Err(format!("engine: {e}?"));
                }
                a.extend(s(&["--engine", e]));
            }
            (proc_flag(a), "Download the models".into())
        }
        "ssl-status" => (s(&["ssl", "status"]), "HTTPS status".into()),
        "ssl-disable" => (s(&["ssl", "disable"]), "Turn HTTPS off".into()),
        "ssl-enable" => {
            let mut a = s(&["ssl", "enable"]);
            if r.auto_cert {
                a.push("--auto-cert".into());
            } else {
                a.push("--cert".into());
                a.push(existing_file(
                    r.cert.as_deref().unwrap_or(""),
                    "certificate",
                )?);
                a.push("--key".into());
                a.push(existing_file(
                    r.key.as_deref().unwrap_or(""),
                    "private key",
                )?);
            }
            (a, "Turn HTTPS on".into())
        }
        "ssl-generate" => {
            let host = r.hostname.as_deref().unwrap_or("localhost").trim();
            if host.is_empty()
                || !host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-:".contains(c))
            {
                return Err(format!("hostname: {host}?"));
            }
            let days = r.days.unwrap_or(365).clamp(1, 36500);
            (
                s(&[
                    "ssl",
                    "generate",
                    "--hostname",
                    host,
                    "--days",
                    &days.to_string(),
                ]),
                "Make a self-signed certificate".into(),
            )
        }
        "update-check" => (s(&["update", "check"]), "Check for updates".into()),
        "update-apply" => (s(&["update", "apply", "-y"]), "Install the update".into()),
        "config-show" => (s(&["config", "show"]), "Configuration".into()),
        "config-path" => (s(&["config", "path"]), "Where the files are".into()),
        "processor-status" => (
            s(&["processor", "status", "--config", &pc]),
            "Processor status".into(),
        ),
        "processor-service" => (
            s(&["processor", "service", "--config", &pc]),
            "Processor service file".into(),
        ),
        "dyndns-status" => (s(&["dyndns", "status"]), "Dynamic DNS".into()),
        "dyndns-update" => (s(&["dyndns", "update"]), "Update the DNS record now".into()),
        "dyndns-enable" => (s(&["dyndns", "enable"]), "Turn Dynamic DNS on".into()),
        "dyndns-disable" => (s(&["dyndns", "disable"]), "Turn Dynamic DNS off".into()),
        "tunnel-status" => (s(&["tunnel", "status"]), "Cloudflare tunnel".into()),
        "healthcheck" => (s(&["healthcheck"]), "Health check".into()),
        other => return Err(format!("unknown job kind: {other}")),
    })
}

/// Heavy jobs that must not run twice at once.
const EXCLUSIVE: [&str; 3] = ["install-ocr", "models-download", "update-apply"];

async fn job_start(State(state): S, Json(r): Json<JobRequest>) -> Response {
    let (args, title) = match job_args(&r, &state.processor_config) {
        Ok(x) => x,
        Err(e) => return bad(e),
    };
    if EXCLUSIVE.contains(&r.kind.as_str())
        && let Some(j) = EXCLUSIVE.iter().find_map(|k| state.jobs.running(k))
    {
        return fail(
            StatusCode::CONFLICT,
            format!("{} is still running (job {})", j.title, j.id),
        );
    }
    let mut full = vec!["-c".to_string(), state.config_path.display().to_string()];
    full.extend(args);
    let envs = vec![(
        "MOKURO_PROCESSOR_CONFIG".to_string(),
        state.processor_config.display().to_string(),
    )];
    let job = state.jobs.start(&r.kind, &title, &state.exe, full, envs);
    ok(job.summary())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct HandoffBody {
    /// Where the browser goes next (for the log).
    url: String,
}

/// The wizard is done and the browser moved on to a started instance: the `gui`
/// command exits shortly (other roles ignore it).
async fn handoff(State(state): S, Json(b): Json<HandoffBody>) -> Response {
    if state.role != Role::Gui {
        return ok(json!({"exiting": false}));
    }
    tracing::info!("handing off to {}", b.url);
    let st = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let _ = st.handoff.send(true);
    });
    ok(json!({"exiting": true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_arguments_are_checked() {
        let pc = Path::new("/c/processor.yaml");
        let r = JobRequest {
            kind: "install-ocr".into(),
            variant: Some("cpu".into()),
            no_models: true,
            processor: true,
            ..JobRequest::default()
        };
        let (a, _) = job_args(&r, pc).unwrap();
        assert_eq!(
            a,
            vec![
                "install-ocr",
                "--variant",
                "cpu",
                "--no-models",
                "--processor"
            ]
        );
        let r = JobRequest {
            kind: "install-ocr".into(),
            variant: Some("cpu; rm -rf /".into()),
            ..JobRequest::default()
        };
        assert!(job_args(&r, pc).is_err());
        let r = JobRequest {
            kind: "install-ocr".into(),
            from: Some("/definitely/not/here".into()),
            ..JobRequest::default()
        };
        assert!(job_args(&r, pc).is_err());
        let r = JobRequest {
            kind: "ssl-generate".into(),
            hostname: Some("$(id)".into()),
            ..JobRequest::default()
        };
        assert!(job_args(&r, pc).is_err());
        let r = JobRequest {
            kind: "rm".into(),
            ..JobRequest::default()
        };
        assert!(job_args(&r, pc).is_err());
        let (a, _) = job_args(
            &JobRequest {
                kind: "processor-status".into(),
                ..JobRequest::default()
            },
            pc,
        )
        .unwrap();
        assert_eq!(
            a,
            vec!["processor", "status", "--config", "/c/processor.yaml"]
        );
    }

    #[test]
    fn logs_stay_in_their_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tmp.path().join("logs");
        std::fs::create_dir(&logs).unwrap();
        std::fs::write(logs.join("server.log"), "hi\n").unwrap();
        std::fs::write(tmp.path().join("secret.txt"), "no\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.path().join("secret.txt"), logs.join("link.log")).unwrap();
        let dirs = vec![("server", logs.clone())];
        assert!(resolve_log(&dirs, "server:server.log").is_ok());
        for bad in [
            "server:../secret.txt",
            "server:..",
            "processor:server.log",
            "server:link.log",
            "server.log",
            "server:/etc/passwd",
        ] {
            assert!(resolve_log(&dirs, bad).is_err(), "{bad}");
        }
    }
}
