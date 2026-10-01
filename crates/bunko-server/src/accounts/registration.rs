//! Self-service registration (0.5.2 `registration/api.py`, spec db-auth-admin §16).
//!
//! register.js routes a 400's `error` to a form field by case-insensitive substring
//! (`invite`, `username`, `password`), so every message keeps 0.5.2's wording. The mode
//! and default role are read from the live config on every request.

use super::AccountsDeps;
use super::util::{
    JsonBody, blocking, db_failed, json_error, json_response, options_response, parse_object, read_body,
    serve_page_json_errors,
};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use bunko_core::Role;
use bunko_db::pyfmt::strip;
use bunko_db::{DbError, UserStatus, normalize_role, validate_password, validate_username};
use http::HeaderMap;
use serde_json::{Value, json};
use tracing::warn;

pub fn routes() -> Router<AccountsDeps> {
    Router::new()
        .route(
            "/api/register",
            get(info).post(register).options(preflight).fallback(method_not_allowed),
        )
        .route("/api/register/config", get(info).options(preflight).fallback(method_not_allowed))
        .route("/register", get(index).options(preflight))
        .route("/register/", get(index).options(preflight))
        .route("/register/{*file}", get(file))
}

async fn method_not_allowed() -> Response {
    json_error(405, "Method not allowed")
}

async fn preflight() -> Response {
    options_response("GET, POST, OPTIONS")
}

async fn index() -> Response {
    serve_page_json_errors("registration", "register.html", "File not found", None)
}

async fn file(Path(file): Path<String>) -> Response {
    serve_page_json_errors("registration", &file, "File not found", None)
}

/// `GET /api/register` and `/api/register/config` (§16.1).
async fn info(State(d): State<AccountsDeps>) -> Response {
    let mode = d.core.config.read().registration.mode.clone();
    let mut body = json!({ "mode": mode, "enabled": mode != "disabled" });
    if mode == "invite" {
        body["requires_invite"] = Value::Bool(true);
    }
    json_response(200, body)
}

/// Python `str(data.get("invite_code", ""))`: a JSON null is the string `None`.
fn invite_code_text(v: Option<&Value>) -> String {
    match v {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "None".into(),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.into(),
        Some(other) => other.to_string(),
    }
}

fn created(message: &str, username: &str, status: &str) -> Response {
    json_response(
        201,
        json!({ "success": true, "message": message, "username": username, "status": status }),
    )
}

/// `POST /api/register` (§16.2).
async fn register(State(d): State<AccountsDeps>, headers: HeaderMap, body: Body) -> Response {
    let (mode, default_role) = {
        let c = d.core.config.read();
        (c.registration.mode.clone(), c.registration.default_role.clone())
    };
    if mode == "disabled" {
        return json_error(403, "Registration is disabled");
    }
    let data = match read_body(&headers, body).await {
        JsonBody::TooLarge => return json_error(413, "Request body too large"),
        JsonBody::Empty => return json_error(400, "Invalid JSON body"),
        // A non-object body (a 500 in 0.5.2) is refused like bad JSON.
        JsonBody::Bytes(b) => match parse_object(&b) {
            Some(m) => m,
            None => return json_error(400, "Invalid JSON body"),
        },
    };
    // A non-string username/password (a 500 in 0.5.2) validates as missing.
    let username = strip(data.get("username").and_then(Value::as_str).unwrap_or("")).to_string();
    if let Some(msg) = validate_username(&username) {
        return json_error(400, msg);
    }
    let password = data.get("password").and_then(Value::as_str).unwrap_or("").to_string();
    if let Some(msg) = validate_password(&password) {
        return json_error(400, msg);
    }
    let invite_code = match mode.as_str() {
        "self" | "approval" => None,
        "invite" => {
            let code = invite_code_text(data.get("invite_code"));
            if code.is_empty() {
                return json_error(400, "Invite code is required");
            }
            Some(code)
        }
        _ => return json_error(500, "Invalid registration mode"),
    };
    let default_role = match normalize_role(&default_role) {
        Ok(r) => r,
        Err(e) => return json_error(400, &e.to_string()),
    };

    let db = d.db.clone();
    let approval = mode == "approval";
    let outcome = blocking(move || -> bunko_db::Result<Result<(), Response>> {
        let role: Role = match &invite_code {
            Some(code) => match db.validate_invite(code)? {
                Some(invite) => invite.role,
                None => return Ok(Err(json_error(400, "Invalid or expired invite code"))),
            },
            None => default_role,
        };
        if db.get_user(&username)?.is_some() {
            return Ok(Err(json_error(409, "Username already exists")));
        }
        let status = if approval { UserStatus::Pending } else { UserStatus::Active };
        match db.create_user(&username, &password, role, status, "") {
            Ok(_) => {}
            Err(DbError::Invalid(msg) | DbError::Conflict(msg)) => return Ok(Err(json_error(400, &msg))),
            Err(e) => return Err(e),
        }
        if let Some(code) = &invite_code {
            // 0.5.2 order: the account exists before the invite is consumed; a
            // concurrent registration that lost the race keeps its account.
            if !db.use_invite(code, &username)? {
                warn!("invite {code} was consumed concurrently; '{username}' registered anyway");
            }
        }
        Ok(Ok(()))
    })
    .await;
    let username = data.get("username").and_then(Value::as_str).map(strip).unwrap_or("");
    match outcome {
        Ok(Ok(Ok(()))) if approval => created("Registration submitted for approval", username, "pending"),
        Ok(Ok(Ok(()))) => created("Registration successful", username, "active"),
        Ok(Ok(Err(resp))) => resp,
        Ok(Err(e)) => db_failed("registration", &e),
        Err(resp) => resp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_code_spelling() {
        assert_eq!(invite_code_text(None), "");
        assert_eq!(invite_code_text(Some(&json!(null))), "None");
        assert_eq!(invite_code_text(Some(&json!("abc"))), "abc");
        assert_eq!(invite_code_text(Some(&json!(12))), "12");
    }
}
