//! `/app/api/...`: the pages' backend. Every route sits behind `super::guard`.

use super::jobs;
use super::setup::{self, ProcessorForm};
use super::spawn;
use super::{AppState, Role, paths, tray};
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
        .route("/app/api/server/start", post(server_start))
        .route("/app/api/server/new", post(server_new))
        .route("/app/api/server/health", get(server_health))
        .route("/app/api/processor/test", post(processor_test))
        .route("/app/api/processor/setup", post(processor_setup))
        .route(
            "/app/api/processor/config",
            get(processor_config).post(processor_config_set),
        )
        .route("/app/api/processor/start", post(processor_start))
        .route("/app/api/processor/status", get(processor_status))
        .route("/app/api/ocr/hardware", get(ocr_hardware))
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
    // Only a library this machine is set up as (something else may answer on the
    // default port).
    let library = match library_url(&state).filter(|_| state.config_path.is_file()) {
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

/// The tray runs `role` from now on: its entry goes into tray.json (so the next start
/// of the app runs it too), and a running tray picks it up within seconds; with no
/// tray running on a desktop, the tray is started (it starts the role). None: no tray
/// (the lite build, or headless): the caller starts the role itself.
async fn hand_to_tray(state: &AppState, role: Role) -> Option<String> {
    let exe = state.exe.clone();
    let cfg = service_config(state, role);
    let log = paths::server_storage(&state.config_path)
        .join("logs")
        .join("tray-start-console.log");
    blocking(move || -> Option<String> {
        let cmd = tray::tray_command(&exe)?;
        let path = tray::config_path(&exe);
        let mut conf = tray::load(&path).ok()?;
        let entry = tray::entry(role, &cfg);
        if !conf.managed.contains(&entry) {
            tray::set_role(&mut conf, role.as_str(), Some(entry));
            tray::save(&path, &conf).ok()?;
        }
        if tray::tray_running(&exe) {
            return Some(format!("the tray runs the {}", role.as_str()));
        }
        if tray::headless() {
            return None;
        }
        let _ = std::fs::create_dir_all(log.parent()?);
        match spawn::spawn_detached(&cmd.0, &cmd.1, &log) {
            Ok(pid) => Some(format!(
                "started the tray (pid {pid}); it runs the {}",
                role.as_str()
            )),
            Err(e) => {
                tracing::warn!("could not start the tray: {e}");
                None
            }
        }
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

#[derive(Deserialize, Default)]
#[serde(default)]
struct ServerNew {
    /// The library folder (empty: the default).
    storage: String,
}

/// The chooser's "Library server": write a config.yaml when there is none (the
/// folder, a free port), start the server (the tray runs it), and say where the
/// browser goes: its first-run `/setup`, or its admin panel once set up.
async fn server_new(State(state): S, Json(b): Json<ServerNew>) -> Response {
    let path = state.config_path.clone();
    let written = blocking(move || write_first_config(&path, b.storage.trim())).await;
    match written {
        Some(Ok(())) => {}
        Some(Err(e)) => return bad(e),
        None => {
            return fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not write the config",
            );
        }
    }
    let resp = start_server(&state).await;
    if !resp.0 {
        return ok(resp.1);
    }
    let mut v = resp.1;
    let url = v["url"].as_str().unwrap_or_default().to_string();
    let next = match setup_needed(&url).await {
        Some(false) => "/_admin",
        _ => "/setup",
    };
    v["open"] = json!(format!("{}{next}", url.trim_end_matches('/')));
    if state.role == Role::Gui {
        let st = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let _ = st.handoff.send(true);
        });
    }
    ok(v)
}

/// A config.yaml for a new library server, unless one exists: `storage` (empty: the
/// default folder), port 8080 or the next free one.
fn write_first_config(path: &Path, storage: &str) -> Result<(), String> {
    if path.is_file() {
        return Ok(());
    }
    let mut config = bunko_core::Config::default();
    if !storage.is_empty() {
        let p = bunko_core::storage::expand_user(Path::new(storage));
        if !p.is_absolute() {
            return Err("library folder: give its full path".into());
        }
        config.storage.base_path = p;
    }
    paths::ensure_writable(&config.storage.base_path)
        .map_err(|e| format!("{}: {e}", config.storage.base_path.display()))?;
    config.server.port = (8080..8100)
        .find(|p| std::net::TcpListener::bind(("0.0.0.0", *p)).is_ok())
        .ok_or("no free port between 8080 and 8099")?;
    crate::cfgfile::save(&config, path).map_err(|e| e.to_string())?;
    tracing::info!(
        "wrote {} (library folder {}, port {})",
        path.display(),
        config.storage.base_path.display(),
        config.server.port
    );
    Ok(())
}

/// Does the server at `url` still need its first-run setup? (None: no answer.)
async fn setup_needed(url: &str) -> Option<bool> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .danger_accept_invalid_certs(true)
        .build()
        .ok()?;
    let v: Value = client
        .get(format!("{}/setup/api/status", url.trim_end_matches('/')))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    v.get("needs_setup").and_then(Value::as_bool)
}

/// Start `serve` in the background and wait for it to answer.
async fn server_start(State(state): S) -> Response {
    let (_, v) = start_server(&state).await;
    ok(v)
}

/// (up, the answer).
async fn start_server(state: &Arc<AppState>) -> (bool, Value) {
    let Some((url, config)) = library_url(state) else {
        return (
            false,
            json!({"up": false, "error": format!("{} does not load", state.config_path.display())}),
        );
    };
    if spawn::healthy(&url).await {
        return (
            true,
            json!({"url": url, "already_running": true, "up": true}),
        );
    }
    let log = spawn::stdout_log(&config.storage.base_path, "serve-console.log");
    // A running tray takes it over (tray.json); it starts it within seconds.
    let tray = hand_to_tray(state, Role::Server).await;
    if let Some(note) = &tray
        && spawn::wait_healthy(&url, Duration::from_secs(40)).await
    {
        return (
            true,
            json!({"url": url, "up": true, "by_tray": true, "note": note}),
        );
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
        Err(e) => {
            return (
                false,
                json!({"up": false, "error": format!("could not start the server: {e}")}),
            );
        }
    };
    let up = spawn::wait_healthy(&url, Duration::from_secs(45)).await;
    let tail = if up {
        String::new()
    } else {
        spawn::tail_file(&log, 30).unwrap_or_default()
    };
    (
        up,
        json!({"url": url, "pid": pid, "up": up, "log": log, "output": tail}),
    )
}

async fn server_health(State(state): S) -> Response {
    match library_url(&state) {
        Some((url, _)) => ok(json!({"url": url, "up": spawn::healthy(&url).await})),
        None => ok(json!({"url": null, "up": false})),
    }
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

#[cfg_attr(not(feature = "ocr"), allow(dead_code))]
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

/// The storage a role's running instance keeps its `.control.json` in.
fn role_storage(state: &AppState, role: Role) -> PathBuf {
    match role {
        Role::Processor => paths::processor_storage(&state.processor_config),
        _ => paths::server_storage(&state.config_path),
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
    fn a_first_config_takes_the_folder_and_a_free_port() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        assert!(write_first_config(&path, "lib").is_err());
        let lib = dir.path().join("lib");
        // Port 8080 taken: the next free one.
        let held = std::net::TcpListener::bind(("0.0.0.0", 8080)).ok();
        write_first_config(&path, &lib.display().to_string()).unwrap();
        let c = bunko_core::config::load_config(Some(&path)).unwrap();
        assert_eq!(c.storage.base_path, lib);
        assert!(lib.is_dir());
        if held.is_some() {
            assert_ne!(c.server.port, 8080);
        }
        assert!((8080..8100).contains(&c.server.port));
        // An existing file is left alone.
        write_first_config(&path, "/elsewhere").unwrap();
        let again = bunko_core::config::load_config(Some(&path)).unwrap();
        assert_eq!(again.storage.base_path, lib);
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
