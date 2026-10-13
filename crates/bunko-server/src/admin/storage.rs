//! The admin Storage panel's API (0.8 §11; M6): `/_admin/api/storage...`, admins
//! only. What the panel shows is `library.storage` of `/api/status` (also answered
//! here alone); its actions are Connect (one button, the flow chosen from where the
//! request comes from), the account choice, "Create it?", Disconnect (Wait / Export /
//! Discard), "This computer", Take over and whether readers connect. No answer ever
//! carries a token, password or session blob.

use super::{AdminState, ApiRequest, blocking, error, ok};
use crate::storage::StorageControl;
use crate::storage::connect::{ConnectRequest, RequestOrigin};
use crate::storage::disconnect::DisconnectMode;
use axum::response::Response;
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) fn control(s: &AdminState) -> Arc<StorageControl> {
    s.storage_control.clone()
}

fn origin(req: &ApiRequest) -> RequestOrigin {
    let loopback = req.ctx.peer.is_loopback()
        && (req.ctx.client_ip == req.ctx.peer.to_string()
            || crate::http::client_ip::is_loopback(&req.ctx.client_ip));
    RequestOrigin::of(loopback, req.user_agent.as_deref())
}

/// `GET /api/storage`: the `library.storage` block.
pub(super) async fn get(s: &AdminState) -> Response {
    let c = control(s);
    match blocking(move || c.status_json()).await {
        Ok(v) => ok(v),
        Err(r) => r,
    }
}

/// `POST /api/storage/connect` `{provider, reader?, folder?, flow?, webdav?, mega?,
/// use_this_account?}`: start a sign-in; the server restarts onto the library once it
/// connected.
pub(super) async fn connect(s: &AdminState, req: &ApiRequest) -> Response {
    let body = match req.json() {
        Ok(b) => b,
        Err(r) => return r,
    };
    let creq: ConnectRequest = match serde_json::from_value(Value::Object(body.clone())) {
        Ok(r) => r,
        Err(e) => return error(400, format!("Invalid connect request: {e}")),
    };
    tracing::info!(
        "admin: {} connects the library to {}",
        req.actor().unwrap_or_default(),
        creq.provider
    );
    match control(s).connect(creq, origin(req), true) {
        Ok(st) => ok(st.as_json()),
        Err(e) => error(400, e),
    }
}

/// `GET /api/storage/connect`: the sign-in's state (the live code, the Check).
pub(super) fn connect_status(s: &AdminState) -> Response {
    ok(control(s).connector().status().as_json())
}

pub(super) fn connect_cancel(s: &AdminState) -> Response {
    let c = control(s);
    c.connector().cancel();
    ok(c.connector().status().as_json())
}

/// `POST /api/storage/connect/account` `{use_this_account: bool}`.
pub(super) async fn connect_account(s: &AdminState, req: &ApiRequest) -> Response {
    let body = match req.json() {
        Ok(b) => b,
        Err(r) => return r,
    };
    let yes = body
        .get("use_this_account")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match control(s).decide_account(yes, true).await {
        Ok(st) => ok(st.as_json()),
        Err(e) => error(400, e),
    }
}

/// `POST /api/storage/connect/create`: "Create it?" on the Check step.
pub(super) async fn connect_create(s: &AdminState) -> Response {
    match control(s).connector().create_folder().await {
        Ok(check) => ok(json!({"check": check})),
        Err(e) => error(400, e),
    }
}

/// `POST /api/storage/disconnect` `{mode: wait|export|discard, wait_s?, folder?}`.
/// 409 with `owed` when results are still owed and the mode is `wait`.
pub(super) async fn disconnect(s: &AdminState, req: &ApiRequest) -> Response {
    let body = match req.json() {
        Ok(b) => Value::Object(b.clone()),
        Err(r) => return r,
    };
    let mode = match DisconnectMode::from_json(&body) {
        Ok(m) => m,
        Err(e) => return error(400, e),
    };
    tracing::info!(
        "admin: {} disconnects the library ({mode:?})",
        req.actor().unwrap_or_default()
    );
    let c = control(s);
    let c2 = c.clone();
    match blocking(move || c2.disconnect(&mode)).await {
        Ok(Ok(report)) => {
            let restarting = c.restart_soon();
            ok(json!({"report": report, "restarting": restarting}))
        }
        Ok(Err(e @ crate::storage::disconnect::DisconnectError::Owed(_))) => {
            super::json_response(409, &e.as_json())
        }
        Ok(Err(e)) => super::json_response(500, &e.as_json()),
        Err(r) => r,
    }
}

/// `POST /api/storage/local`: keep the library on this disk.
pub(super) async fn local(s: &AdminState) -> Response {
    let c = control(s);
    let c2 = c.clone();
    match blocking(move || c2.use_local()).await {
        Ok(Ok(())) => {
            let restarting = c.restart_pending() && c.restart_soon();
            ok(json!({"backend": "local", "restarting": restarting}))
        }
        Ok(Err(e)) => error(409, e),
        Err(r) => r,
    }
}

/// `POST /api/storage/lease/take-over`.
pub(super) async fn take_over(s: &AdminState, req: &ApiRequest) -> Response {
    tracing::warn!(
        "admin: {} takes the library lease over",
        req.actor().unwrap_or_default()
    );
    let c = control(s);
    match blocking(move || c.take_over()).await {
        Ok(Ok(())) => ok(json!({"success": true})),
        Ok(Err(e)) => error(409, e),
        Err(r) => r,
    }
}

/// `PUT /api/storage/readers` `{serve_readers: bool, progress_user?: string|null}`:
/// whether readers connect to this Bunko, and (bridge) whose progress files are the
/// cloud root's. Read per request: no restart.
pub(super) async fn readers(s: &AdminState, req: &ApiRequest) -> Response {
    let body = match req.json() {
        Ok(b) => b.clone(),
        Err(r) => return r,
    };
    let Some(serve) = body.get("serve_readers").and_then(Value::as_bool) else {
        return error(400, "serve_readers (true or false) is required");
    };
    let progress_user = match body.get("progress_user") {
        None | Some(Value::Null) => None,
        Some(Value::String(u)) if u.trim().is_empty() => None,
        Some(Value::String(u)) => Some(u.trim().to_string()),
        Some(_) => return error(400, "progress_user: a username or null"),
    };
    if let Some(u) = &progress_user {
        let (db, name) = (s.db(), u.clone());
        match blocking(move || db.get_user(&name)).await {
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) => return error(400, format!("progress_user: no user {u:?}")),
            Ok(Err(e)) => return super::internal("user lookup", e),
            Err(r) => return r,
        }
    }
    let _lock = s.config_lock.lock();
    {
        let mut c = s.core().config.write();
        c.server.serve_readers = serve;
        c.storage.library.progress_user = progress_user.clone();
    }
    if let Err(e) = s.core().save_config() {
        return super::internal("saving the config", e);
    }
    ok(json!({"serve_readers": serve, "progress_user": progress_user}))
}
