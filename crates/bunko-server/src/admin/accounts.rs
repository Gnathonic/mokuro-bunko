//! Users and invites (spec db-auth-admin §18.3, §18.4).
//!
//! Changed from 0.5.2: deleting, disabling or demoting the last active admin is refused
//! with 409 and `bunko_db::LAST_ADMIN_MESSAGE` (checked in the transaction that makes the
//! change). The CLI `admin` commands still allow it: whoever runs them owns the server's
//! files and can always add an admin back with `admin add-user --role admin`.

use super::{AdminState, ApiRequest, blocking, error, internal, json_response, ok};
use axum::response::Response;
use bunko_core::Role;
use bunko_db::{
    AuditDetails, DbError, KeepAdmin, NewAuditEvent, normalize_role, pyfmt, validate_password,
    validate_username,
};
use serde_json::{Value, json};
use std::str::FromStr;

/// Roles `PUT /api/users/<u>/role` accepts, in 0.5.2's (unsorted) message order.
const CHANGEABLE_ROLES: [&str; 6] = [
    "registered",
    "uploader",
    "inviter",
    "editor",
    "admin",
    "processor",
];
const CHANGEABLE_ROLES_TEXT: &str =
    "['registered', 'uploader', 'inviter', 'editor', 'admin', 'processor']";
const INVITE_ROLES: [&str; 4] = ["editor", "inviter", "registered", "uploader"];
const INVITE_ROLES_TEXT: &str = "['editor', 'inviter', 'registered', 'uploader']";

/// A DB failure as 0.5.2's handlers answer it: caller mistakes 400 (409 for "already
/// exists"), anything else 500.
fn db_error(e: DbError) -> Response {
    match &e {
        DbError::Invalid(msg) | DbError::Conflict(msg) | DbError::AuditQuery(msg) => {
            let status = if matches!(e, DbError::Conflict(_))
                || msg.to_lowercase().contains("already exists")
            {
                409
            } else {
                400
            };
            error(status, msg.clone())
        }
        _ => internal("database error", e),
    }
}

fn user_event<'a>(action: &'a str, actor: Option<&'a str>, username: &'a str) -> NewAuditEvent<'a> {
    NewAuditEvent::new(action)
        .actor(actor)
        .target_type("user")
        .target_username(username)
}

/// A JSON value as Python's `str()` would print it in a message.
fn py_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

pub(super) async fn list_users(s: &AdminState) -> Response {
    let db = s.db();
    match blocking(move || db.list_users(None)).await {
        Ok(Ok(users)) => ok(json!({ "users": users })),
        Ok(Err(e)) => db_error(e),
        Err(r) => r,
    }
}

pub(super) async fn create_user(s: &AdminState, req: &ApiRequest) -> Response {
    let data = match req.json() {
        Ok(d) => d,
        Err(r) => return r,
    };
    let username =
        pyfmt::strip(data.get("username").and_then(Value::as_str).unwrap_or("")).to_string();
    let password = data
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let role_raw = data
        .get("role")
        .cloned()
        .unwrap_or_else(|| json!("registered"));
    let notes = match data.get("notes") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(n)) => n.clone(),
        // 0.5.2: an SQLite bind error (500); spec OQ5: 400.
        Some(_) => return error(400, "notes must be a string"),
    };
    if username.is_empty() {
        return error(400, "Username is required");
    }
    if let Some(msg) = validate_username(&username) {
        return error(400, msg);
    }
    if password.is_empty() {
        return error(400, "Password is required");
    }
    if let Some(msg) = validate_password(&password) {
        return error(400, msg);
    }
    let role = match normalize_role(&py_text(&role_raw)) {
        Ok(r) => r,
        Err(e) => return db_error(e),
    };
    let db = s.db();
    let actor = req.actor();
    let result = blocking(move || -> Result<Value, DbError> {
        db.create_user(
            &username,
            &password,
            role,
            bunko_db::UserStatus::Active,
            &notes,
        )?;
        let user = db.get_user(&username)?;
        db.log_audit_event(
            &user_event("admin_create_user", actor.as_deref(), &username)
                .details(AuditDetails::new().with("role", role_raw)),
        )?;
        Ok(json!({"success": true, "user": user}))
    })
    .await;
    match result {
        Ok(Ok(body)) => json_response(201, &body),
        Ok(Err(e)) => db_error(e),
        Err(r) => r,
    }
}

pub(super) async fn delete_user(s: &AdminState, req: &ApiRequest, username: &str) -> Response {
    let db = s.db();
    let u = username.to_string();
    let deleted = match blocking(move || db.delete_user_with(&u, KeepAdmin::Keep)).await {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return db_error(e),
        Err(r) => return r,
    };
    if !deleted {
        return error(404, format!("User '{username}' not found"));
    }
    s.drop_processors(username, "its account was deleted");
    if let Err(r) = audit(s, "admin_delete_user", req.actor(), username, None).await {
        return r;
    }
    ok(json!({"success": true, "message": format!("User '{username}' deleted")}))
}

async fn audit(
    s: &AdminState,
    action: &'static str,
    actor: Option<String>,
    username: &str,
    details: Option<AuditDetails>,
) -> Result<(), Response> {
    let db = s.db();
    let u = username.to_string();
    match blocking(move || {
        let mut ev = user_event(action, actor.as_deref(), &u);
        ev.details = details;
        db.log_audit_event(&ev)
    })
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(db_error(e)),
        Err(r) => Err(r),
    }
}

/// `{"success": true, "user": <user>}` after a successful change, with its audit row.
async fn user_after(
    s: &AdminState,
    req: &ApiRequest,
    action: &'static str,
    username: &str,
    details: Option<AuditDetails>,
) -> Response {
    let db = s.db();
    let u = username.to_string();
    let actor = req.actor();
    let result = blocking(move || -> Result<Value, DbError> {
        let user = db.get_user(&u)?;
        let mut ev = user_event(action, actor.as_deref(), &u);
        ev.details = details;
        db.log_audit_event(&ev)?;
        Ok(json!({"success": true, "user": user}))
    })
    .await;
    match result {
        Ok(Ok(body)) => ok(body),
        Ok(Err(e)) => db_error(e),
        Err(r) => r,
    }
}

pub(super) async fn update_notes(s: &AdminState, req: &ApiRequest, username: &str) -> Response {
    let data = match req.json() {
        Ok(d) => d,
        Err(r) => return r,
    };
    let notes = match data.get("notes") {
        None => String::new(),
        Some(Value::String(n)) => n.clone(),
        Some(_) => return error(400, "notes must be a string"),
    };
    let db = s.db();
    let u = username.to_string();
    match blocking(move || db.update_user_notes(&u, &notes)).await {
        Ok(Ok(true)) => user_after(s, req, "admin_update_notes", username, None).await,
        Ok(Ok(false)) => error(404, format!("User '{username}' not found")),
        Ok(Err(e)) => db_error(e),
        Err(r) => r,
    }
}

pub(super) async fn change_role(s: &AdminState, req: &ApiRequest, username: &str) -> Response {
    let data = match req.json() {
        Ok(d) => d,
        Err(r) => return r,
    };
    let raw = data.get("role").cloned().unwrap_or(Value::Null);
    if !pyfmt::truthy(&raw) {
        return error(400, "Role is required");
    }
    let Some(role_name) = raw
        .as_str()
        .filter(|r| CHANGEABLE_ROLES.contains(r))
        .map(str::to_string)
    else {
        return error(
            400,
            format!("Invalid role. Must be one of: {CHANGEABLE_ROLES_TEXT}"),
        );
    };
    let Ok(role) = Role::from_str(&role_name) else {
        return error(
            400,
            format!("Invalid role. Must be one of: {CHANGEABLE_ROLES_TEXT}"),
        );
    };
    let db = s.db();
    let u = username.to_string();
    match blocking(move || db.update_user_role_with(&u, role, KeepAdmin::Keep)).await {
        Ok(Ok(true)) => {
            if role != Role::Processor {
                s.drop_processors(
                    username,
                    &format!("its account's role was changed to {role_name}"),
                );
            }
            user_after(
                s,
                req,
                "admin_change_role",
                username,
                Some(AuditDetails::new().with("role", role_name)),
            )
            .await
        }
        Ok(Ok(false)) => error(404, format!("User '{username}' not found")),
        Ok(Err(e)) => db_error(e),
        Err(r) => r,
    }
}

pub(super) async fn approve_user(s: &AdminState, req: &ApiRequest, username: &str) -> Response {
    let db = s.db();
    let u = username.to_string();
    match blocking(move || db.approve_user(&u)).await {
        Ok(Ok(true)) => user_after(s, req, "admin_approve_user", username, None).await,
        Ok(Ok(false)) => error(404, format!("User '{username}' not found or not pending")),
        Ok(Err(e)) => db_error(e),
        Err(r) => r,
    }
}

pub(super) async fn disable_user(s: &AdminState, req: &ApiRequest, username: &str) -> Response {
    let db = s.db();
    let u = username.to_string();
    match blocking(move || db.disable_user_with(&u, KeepAdmin::Keep)).await {
        Ok(Ok(true)) => {
            s.drop_processors(username, "its account was disabled");
            user_after(s, req, "admin_disable_user", username, None).await
        }
        Ok(Ok(false)) => error(404, format!("User '{username}' not found")),
        Ok(Err(e)) => db_error(e),
        Err(r) => r,
    }
}

// --- invites -------------------------------------------------------------------------------

pub(super) async fn list_invites(s: &AdminState) -> Response {
    let db = s.db();
    match blocking(move || db.list_invite_infos()).await {
        Ok(Ok(invites)) => ok(json!({ "invites": invites })),
        Ok(Err(e)) => db_error(e),
        Err(r) => r,
    }
}

pub(super) async fn create_invite(s: &AdminState, req: &ApiRequest) -> Response {
    let data = match req.json() {
        Ok(d) => d,
        Err(r) => return r,
    };
    let role_raw = data
        .get("role")
        .cloned()
        .unwrap_or_else(|| json!("registered"));
    // `writer` is refused here although the database layer would accept it (0.5.2).
    let Some(role) = role_raw
        .as_str()
        .filter(|r| INVITE_ROLES.contains(r))
        .and_then(|r| Role::from_str(r).ok())
    else {
        return error(
            400,
            format!("Invalid role. Must be one of: {INVITE_ROLES_TEXT}"),
        );
    };
    let expires = match data.get("expires") {
        None => "7d".to_string(),
        Some(Value::String(e)) => e.clone(),
        // 0.5.2: TypeError → 500; spec §18.4: 400 with the duration message.
        Some(other) => return error(400, format!("Invalid duration format: {}", py_text(other))),
    };
    let db = s.db();
    let actor = req.actor();
    let result = blocking(move || -> Result<Value, DbError> {
        let code = db.create_invite(role, &expires, actor.as_deref())?;
        let info = db.invite_info(&code)?;
        Ok(json!({"success": true, "invite": info}))
    })
    .await;
    match result {
        Ok(Ok(body)) => json_response(201, &body),
        Ok(Err(e)) => db_error(e),
        Err(r) => r,
    }
}

pub(super) async fn delete_invite(s: &AdminState, req: &ApiRequest, code: &str) -> Response {
    let db = s.db();
    let c = code.to_string();
    let actor = req.actor();
    let result = blocking(move || -> Result<bool, DbError> {
        if !db.delete_invite(&c)? {
            return Ok(false);
        }
        db.log_audit_event(
            &NewAuditEvent::new("invite_deleted")
                .actor(actor.as_deref())
                .target_type("invite")
                .target_path(&c),
        )?;
        Ok(true)
    })
    .await;
    match result {
        Ok(Ok(true)) => ok(json!({"success": true, "message": "Invite deleted"})),
        Ok(Ok(false)) => error(404, "Invite not found"),
        Ok(Err(e)) => db_error(e),
        Err(r) => r,
    }
}
