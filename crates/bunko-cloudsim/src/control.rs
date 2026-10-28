//! The control API under `{base}/_sim/`, JSON in and out: tokens, MEGA accounts,
//! seeding, faults, knobs, the request log, violations and reset. Mistakes in a control
//! call are a `400 {"error": ...}` (they are the test's, not a provider client's, so
//! they are not violations).

use std::collections::HashMap;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use serde_json::{Value, json};

use crate::accounts::Provider;
use crate::{DriveState, OneDriveState, SimState, drive, mega, onedrive};

type Body = Result<Json<Value>, JsonRejection>;

/// A control call the simulator cannot carry out: `400 {"error": ...}`.
pub struct Ctl(String);

impl IntoResponse for Ctl {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, Json(json!({"error": self.0}))).into_response()
    }
}

fn ctl_err(msg: impl Into<String>) -> Ctl {
    Ctl(msg.into())
}

type Answer = Result<Response, Ctl>;

fn body(b: Body) -> Result<Value, Ctl> {
    b.map(|Json(v)| v)
        .map_err(|e| ctl_err(format!("not a JSON body: {e}")))
}

fn provider(v: &Value) -> Result<Provider, Ctl> {
    let p = v["provider"].as_str().unwrap_or_default();
    Provider::parse(p).ok_or_else(|| ctl_err(format!("provider {p:?}: google, microsoft or mega")))
}

fn account(v: &Value) -> String {
    v["account"].as_str().unwrap_or("default").to_string()
}

fn string(v: &Value, k: &str) -> Result<String, Ctl> {
    v[k].as_str()
        .map(str::to_string)
        .ok_or_else(|| ctl_err(format!("missing {k}")))
}

pub fn router() -> Router<SimState> {
    Router::new()
        .route("/health", get(health))
        .route("/token", post(token))
        .route("/token/revoke", post(revoke))
        .route("/token/expire", post(expire))
        .route("/mega/account", post(mega_account))
        .route("/seed", post(seed))
        .route("/files", get(files))
        .route("/fault", post(fault))
        .route("/knobs", post(knobs))
        .route("/log", get(log).delete(clear_log))
        .route("/violations", get(violations))
        .route("/reset", post(reset))
}

async fn health() -> Response {
    Json(json!({"ok": true, "providers": ["google", "microsoft", "mega"]})).into_response()
}

async fn token(State(sim): State<SimState>, b: Body) -> Answer {
    let v = body(b)?;
    let p = provider(&v)?;
    if p == Provider::Mega {
        return Err(ctl_err(
            "MEGA has no OAuth tokens: create the account with /_sim/mega/account",
        ));
    }
    Ok(Json(sim.tokens().issue(p, &account(&v))).into_response())
}

async fn revoke(State(sim): State<SimState>, b: Body) -> Answer {
    let t = string(&body(b)?, "token")?;
    Ok(Json(json!({"ok": sim.tokens().revoke(&t)})).into_response())
}

async fn expire(State(sim): State<SimState>, b: Body) -> Answer {
    let t = string(&body(b)?, "token")?;
    Ok(Json(json!({"ok": sim.tokens().expire(&t)})).into_response())
}

async fn mega_account(State(sim): State<SimState>, b: Body) -> Answer {
    let v = body(b)?;
    let (email, password) = (string(&v, "email")?, string(&v, "password")?);
    sim.mega(&account(&v))
        .set_account(&email, &password, v["mfa"].as_str());
    Ok(Json(json!({"ok": true})).into_response())
}

async fn seed(State(sim): State<SimState>, b: Body) -> Answer {
    let v = body(b)?;
    let p = provider(&v)?;
    let mut files = Vec::new();
    for f in v["files"]
        .as_array()
        .ok_or_else(|| ctl_err("missing files"))?
    {
        let path = string(f, "path")?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(f["content_base64"].as_str().unwrap_or_default())
            .map_err(|e| ctl_err(format!("{path}: content_base64: {e}")))?;
        files.push((path, data));
    }
    sim.seed(p, &account(&v), &files);
    Ok(Json(json!({"seeded": files.len()})).into_response())
}

async fn files(State(sim): State<SimState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let p = q.get("provider").map(String::as_str).unwrap_or_default();
    let Some(p) = Provider::parse(p) else {
        return ctl_err(format!("provider {p:?}: google, microsoft or mega")).into_response();
    };
    let account = q.get("account").map(String::as_str).unwrap_or("default");
    Json(sim.files(p, account)).into_response()
}

async fn fault(State(sim): State<SimState>, b: Body) -> Answer {
    let v = body(b)?;
    let p = provider(&v)?;
    let method = v["method"].as_str().map(str::to_string);
    let path_has = v["path_has"].as_str().unwrap_or_default().to_string();
    let status = v["status"]
        .as_u64()
        .and_then(|s| u16::try_from(s).ok())
        .ok_or_else(|| ctl_err("missing status"))?;
    let reason = v["reason"].as_str().unwrap_or("backendError").to_string();
    let retry_after = v["retry_after"].as_u64();
    let times = v["times"].as_u64().unwrap_or(1) as usize;
    match p {
        Provider::Google => sim.drive(&account(&v)).lock().faults.push(drive::Fault {
            method,
            path_has,
            status,
            reason,
            retry_after,
            times,
        }),
        Provider::Microsoft => sim
            .onedrive(&account(&v))
            .lock()
            .faults
            .push(onedrive::Fault {
                method,
                path_has,
                status,
                code: reason,
                retry_after,
                times,
            }),
        Provider::Mega => {
            return Err(ctl_err(
                "MEGA answers with error codes, not HTTP faults: use /_sim/knobs",
            ));
        }
    }
    Ok(Json(json!({"ok": true})).into_response())
}

fn flag(v: &Value, k: &str) -> Result<bool, String> {
    v.as_bool()
        .ok_or_else(|| format!("knob {k} takes true or false"))
}

fn count(v: &Value, k: &str) -> Result<usize, String> {
    v.as_u64()
        .map(|n| n as usize)
        .ok_or_else(|| format!("knob {k} takes a non-negative number"))
}

fn opt_count(v: &Value, k: &str) -> Result<Option<usize>, String> {
    if v.is_null() {
        return Ok(None);
    }
    count(v, k).map(Some)
}

fn signed(v: &Value, k: &str) -> Result<i64, String> {
    v.as_i64().ok_or_else(|| format!("knob {k} takes a number"))
}

fn drive_knob(st: &mut DriveState, k: &str, v: &Value) -> Result<(), String> {
    match k {
        "page_size" => st.page_size = count(v, k)?.max(1),
        "reverse_listing" => st.reverse_listing = flag(v, k)?,
        "sha256" => st.sha256 = flag(v, k)?,
        "keep_revisions" => st.keep_revisions = flag(v, k)?,
        "revisions_forbidden" => st.revisions_forbidden = flag(v, k)?,
        "interrupt_chunk" => st.interrupt_chunk = opt_count(v, k)?,
        "down_after_interrupt" => st.down_after_interrupt = flag(v, k)?,
        "session_down" => st.session_down = flag(v, k)?,
        "quota_limit" => st.quota_limit = signed(v, k)?,
        _ => return Err(format!("no Drive knob {k:?}")),
    }
    Ok(())
}

fn onedrive_knob(st: &mut OneDriveState, k: &str, v: &Value) -> Result<(), String> {
    match k {
        "page_size" => st.page_size = count(v, k)?.max(1),
        "interrupt_fragment" => st.interrupt_fragment = opt_count(v, k)?,
        "down_after_interrupt" => st.down_after_interrupt = flag(v, k)?,
        "session_down" => st.session_down = flag(v, k)?,
        "send_ctag" => st.send_ctag = flag(v, k)?,
        "storage_ctag" => st.storage_ctag = flag(v, k)?,
        "ignore_range" => st.ignore_range = flag(v, k)?,
        "permanent_delete" => st.permanent_delete = flag(v, k)?,
        "quota_total" => st.quota_total = signed(v, k)?,
        "lost_below" => st.lost_below = count(v, k)?,
        _ => return Err(format!("no OneDrive knob {k:?}")),
    }
    Ok(())
}

async fn knobs(State(sim): State<SimState>, b: Body) -> Answer {
    let v = body(b)?;
    let p = provider(&v)?;
    let name = account(&v);
    let obj = v.as_object().ok_or_else(|| ctl_err("not an object"))?;
    let knobs = obj
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "provider" | "account"));
    for (k, val) in knobs {
        match p {
            Provider::Google => drive_knob(&mut sim.drive(&name).lock(), k, val),
            Provider::Microsoft => onedrive_knob(&mut sim.onedrive(&name).lock(), k, val),
            Provider::Mega => mega::knob(&sim.mega(&name), k, val),
        }
        .map_err(ctl_err)?;
    }
    Ok(Json(json!({"ok": true})).into_response())
}

async fn log(State(sim): State<SimState>) -> Response {
    Json(sim.log().list()).into_response()
}

async fn clear_log(State(sim): State<SimState>) -> Response {
    sim.log().clear();
    StatusCode::NO_CONTENT.into_response()
}

async fn violations(State(sim): State<SimState>) -> Response {
    Json(sim.violations()).into_response()
}

async fn reset(State(sim): State<SimState>, _b: Bytes) -> Response {
    sim.reset();
    StatusCode::NO_CONTENT.into_response()
}
