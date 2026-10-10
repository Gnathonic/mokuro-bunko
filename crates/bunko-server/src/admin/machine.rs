//! "This server" (`/_admin/api/machine/*`, new in 0.7): the machine the server runs on.
//! Its OCR backend (the preference, install with progress, reinstall, removal), its
//! engines and models, the doctor and the server log, through the binary's
//! [`crate::machine::Machine`]. Admin only, behind the panel's CSRF check like every
//! other write. Installs come from the signed release (or the folder shipped next to
//! the program); a page never names a folder or a file to install from.

use super::{AdminState, ApiRequest, blocking, error, ok, parse_qs, query_one, save_config};
use crate::machine::Machine;
use axum::response::Response;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Between two starts of the same long-running action.
const COOLDOWN: Duration = Duration::from_secs(3);

/// The backends a page may choose (`skip` is "OCR on this machine" off).
pub const CHOOSABLE_BACKENDS: [&str; 4] = ["auto", "cpu", "cuda", "rocm"];

fn machine(s: &AdminState) -> Result<Arc<dyn Machine>, Response> {
    s.deps
        .machine
        .clone()
        .ok_or_else(|| error(404, "This server's machine settings are not available here"))
}

/// One start of `action` per [`COOLDOWN`].
fn cooldown(s: &AdminState, action: &'static str) -> Result<(), Response> {
    let mut last = s.machine_actions.lock();
    let now = Instant::now();
    if let Some(t) = last.get(action)
        && now.duration_since(*t) < COOLDOWN
    {
        return Err(error(429, "Too soon: wait a few seconds and try again"));
    }
    last.insert(action, now);
    Ok(())
}

fn result(r: Result<Value, String>) -> Response {
    match r {
        Ok(v) => ok(v),
        Err(e) => error(409, e),
    }
}

pub(super) async fn overview(s: &AdminState) -> Response {
    let m = match machine(s) {
        Ok(m) => m,
        Err(r) => return r,
    };
    let (local, backend) = {
        let c = s.core().config.read();
        (c.ocr.local_processing, c.ocr.backend.clone())
    };
    match blocking(move || m.overview()).await {
        Ok(mut v) => {
            if let Value::Object(o) = &mut v {
                o.insert(
                    "config".into(),
                    json!({"local_processing": local, "backend": backend}),
                );
            }
            ok(v)
        }
        Err(r) => r,
    }
}

/// `PUT /api/machine/ocr {local_processing?, backend?}`: OCR on this machine, and the
/// backend it prefers (auto, cpu, cuda, rocm). Saved, then applied: the install starts
/// when something is missing, and a backend switch restarts this server's OCR (the
/// whole server when another backend is already loaded in it).
pub(super) async fn ocr(s: &AdminState, req: &ApiRequest) -> Response {
    let m = match machine(s) {
        Ok(m) => m,
        Err(r) => return r,
    };
    let data = match req.json() {
        Ok(d) => d.clone(),
        Err(r) => return r,
    };
    let local = match data.get("local_processing") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return error(400, "local_processing must be true or false"),
    };
    let backend = match data.get("backend") {
        None | Some(Value::Null) => None,
        Some(Value::String(b)) if CHOOSABLE_BACKENDS.contains(&b.as_str()) => Some(b.clone()),
        Some(_) => return error(400, "backend must be auto, cpu, cuda or rocm"),
    };
    if local.is_none() && backend.is_none() {
        return error(400, "Nothing to change");
    }
    if let Err(r) = cooldown(s, "ocr") {
        return r;
    }
    let s2 = s.clone();
    let actor = req.actor().unwrap_or_default();
    blocking(move || {
        if backend.is_some()
            && let Some(why) = m.backend_locked()
        {
            return error(409, why);
        }
        if local.is_some() && std::env::var_os("MOKURO_OCR_LOCAL_PROCESSING").is_some() {
            return error(409, "OCR on this machine is set by MOKURO_OCR_LOCAL_PROCESSING");
        }
        let (snapshot, local_changed, backend_changed) = {
            let _guard = s2.config_lock.lock();
            let (lc, bc) = {
                let mut cfg = s2.core().config.write();
                let lc = local.is_some_and(|v| v != cfg.ocr.local_processing);
                let bc = backend.as_ref().is_some_and(|b| *b != cfg.ocr.backend);
                if let Some(v) = local {
                    cfg.ocr.local_processing = v;
                }
                if let Some(b) = &backend {
                    cfg.ocr.backend = b.clone();
                }
                (lc, bc)
            };
            if let Err(r) = save_config(&s2) {
                return r;
            }
            (s2.core().config.read().clone(), lc, bc)
        };
        tracing::info!(
            "admin '{actor}': OCR on this machine {}, backend {}",
            snapshot.ocr.local_processing,
            snapshot.ocr.backend
        );
        let mut outcome = json!({"installing": false, "restarting": false, "message": ""});
        if local_changed {
            outcome = s2.ocr().apply(&snapshot).unwrap_or(outcome);
        }
        if backend_changed && snapshot.ocr.local_processing {
            outcome = m.ocr_changed(&snapshot, true, false);
        }
        ok(json!({
            "success": true,
            "config": {"local_processing": snapshot.ocr.local_processing, "backend": snapshot.ocr.backend},
            "result": outcome,
        }))
    })
    .await
    .unwrap_or_else(|r| r)
}

/// `POST /api/machine/install {reinstall?}`: install (or retry) now.
pub(super) async fn install(s: &AdminState, req: &ApiRequest) -> Response {
    let m = match machine(s) {
        Ok(m) => m,
        Err(r) => return r,
    };
    let reinstall = match req.json() {
        Ok(d) => d.get("reinstall").and_then(Value::as_bool).unwrap_or(false),
        Err(r) => return r,
    };
    if let Err(r) = cooldown(s, "install") {
        return r;
    }
    if let Some(a) = req.actor() {
        tracing::info!(
            "admin '{a}': {} the OCR backend",
            if reinstall { "reinstall" } else { "install" }
        );
    }
    blocking(move || result(m.install(reinstall)))
        .await
        .unwrap_or_else(|r| r)
}

/// `POST /api/machine/remove`: delete the installed packs.
pub(super) async fn remove(s: &AdminState) -> Response {
    let m = match machine(s) {
        Ok(m) => m,
        Err(r) => return r,
    };
    if let Err(r) = cooldown(s, "remove") {
        return r;
    }
    blocking(move || result(m.remove()))
        .await
        .unwrap_or_else(|r| r)
}

/// `POST /api/machine/jobs {kind, engine?}`: doctor, models-download, models-verify.
pub(super) async fn start_job(s: &AdminState, req: &ApiRequest) -> Response {
    let m = match machine(s) {
        Ok(m) => m,
        Err(r) => return r,
    };
    let data = match req.json() {
        Ok(d) => d.clone(),
        Err(r) => return r,
    };
    let Some(kind) = data.get("kind").and_then(Value::as_str).map(str::to_string) else {
        return error(400, "kind is required");
    };
    let engine = data
        .get("engine")
        .and_then(Value::as_str)
        .filter(|e| !e.is_empty())
        .map(str::to_string);
    if let Err(r) = cooldown(s, "job") {
        return r;
    }
    blocking(move || result(m.start_job(&kind, engine.as_deref())))
        .await
        .unwrap_or_else(|r| r)
}

/// `GET /api/machine/jobs/{id}?from=N`.
pub(super) async fn job(s: &AdminState, req: &ApiRequest, id: &str) -> Response {
    let m = match machine(s) {
        Ok(m) => m,
        Err(r) => return r,
    };
    let Ok(id) = id.parse::<u64>() else {
        return error(404, "No such job");
    };
    let from = query_one(&parse_qs(&req.query), "from")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    match blocking(move || m.job(id, from)).await {
        Ok(Some(v)) => ok(v),
        Ok(None) => error(404, "No such job"),
        Err(r) => r,
    }
}

/// `GET /api/machine/logs?lines=N` (at most 2000).
pub(super) async fn logs(s: &AdminState, req: &ApiRequest) -> Response {
    let m = match machine(s) {
        Ok(m) => m,
        Err(r) => return r,
    };
    let lines = query_one(&parse_qs(&req.query), "lines")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(1, 2000);
    blocking(move || ok(m.logs(lines)))
        .await
        .unwrap_or_else(|r| r)
}
