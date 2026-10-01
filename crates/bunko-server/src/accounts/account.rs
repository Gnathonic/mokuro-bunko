//! The account page and its API (0.5.2 `account/api.py`, spec db-auth-admin §17):
//! personal stats (placeholder zeros), password change, self-service deletion.
//!
//! Deviation (spec §24 open question 7): 0.5.2 checked passwords here with no rate
//! limiter, an unthrottled password oracle. Basic credentials and the
//! current-password/confirmation checks now count in `core.login_limiter` under the same
//! `ip:username` key as the login page; a blocked key answers 429
//! `{"error": "Too many failed attempts. Retry in Ns"}`.

use super::AccountsDeps;
use super::login::{AuthHeader, auth_header};
use super::util::{
    Client, JsonBody, blocking, db_failed, json_error, json_response, limited_message, options_response, parse_object,
    read_body, serve_page_text_errors,
};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, options, post};
use bunko_db::{DbError, NewAuditEvent, User, validate_password};
use http::HeaderMap;
use serde_json::{Map, Value, json};
use std::path::Path as FsPath;
use tracing::warn;

pub fn routes() -> Router<AccountsDeps> {
    Router::new()
        .route("/api/account/stats", get(stats).options(preflight))
        .route("/api/account/password", post(change_password).options(preflight))
        .route("/api/account/delete", post(delete_account).options(preflight))
        .route("/api/account/{*rest}", options(preflight))
        .route("/account", get(index))
        .route("/account/", get(index))
        .route("/account/{*file}", get(file))
}

async fn index() -> Response {
    serve_page_text_errors("account", "index.html")
}

async fn file(Path(file): Path<String>) -> Response {
    serve_page_text_errors("account", &file)
}

async fn preflight() -> Response {
    options_response("GET, POST, OPTIONS")
}

/// 0.5.2 `PersonalStats._format_time`.
pub fn format_reading_time(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h {}m", minutes % 60);
    }
    format!("{}d {}h", hours / 24, hours % 24)
}

/// Bearer or Basic (0.5.2 `authenticate_basic_header`); anything else is 401
/// `Authentication required`.
async fn authenticate(d: &AccountsDeps, client: &Client, headers: &HeaderMap) -> Result<User, Response> {
    let refused = || json_error(401, "Authentication required");
    let db = d.db.clone();
    match auth_header(headers) {
        AuthHeader::Bearer(token) => match blocking(move || db.resolve_auth_token(&token)).await? {
            Ok(Some(user)) => Ok(user),
            Ok(None) => Err(refused()),
            Err(e) => Err(db_failed("token lookup", &e)),
        },
        AuthHeader::Basic(username, password) => {
            let key = client.limiter_key(&username);
            if let Err(retry) = d.core.login_limiter.allow(&key) {
                return Err(json_error(429, &limited_message(retry)));
            }
            match blocking(move || db.authenticate_user(&username, &password)).await? {
                Ok(Some(user)) => {
                    d.core.login_limiter.record_success(&key);
                    Ok(user)
                }
                Ok(None) => {
                    d.core.login_limiter.record_failure(&key);
                    Err(refused())
                }
                Err(e) => Err(db_failed("password check", &e)),
            }
        }
        AuthHeader::None | AuthHeader::Malformed => Err(refused()),
    }
}

/// Re-check the account's own password (current password / delete confirmation),
/// rate limited like a login. `Ok(false)` is a wrong password.
async fn confirm_password(d: &AccountsDeps, client: &Client, username: &str, password: &str) -> Result<bool, Response> {
    let key = client.limiter_key(username);
    if let Err(retry) = d.core.login_limiter.allow(&key) {
        return Err(json_error(429, &limited_message(retry)));
    }
    let (db, u, p) = (d.db.clone(), username.to_string(), password.to_string());
    match blocking(move || db.authenticate_user(&u, &p)).await? {
        Ok(Some(_)) => {
            d.core.login_limiter.record_success(&key);
            Ok(true)
        }
        Ok(None) => {
            d.core.login_limiter.record_failure(&key);
            Ok(false)
        }
        Err(e) => Err(db_failed("password check", &e)),
    }
}

/// The JSON object body of the password/delete calls, with their 0.5.2 errors.
async fn object_body(headers: &HeaderMap, body: Body) -> Result<Map<String, Value>, Response> {
    match read_body(headers, body).await {
        JsonBody::Empty => Err(json_error(400, "Missing request body")),
        JsonBody::TooLarge => Err(json_error(413, "Request body too large")),
        JsonBody::Bytes(b) => parse_object(&b).ok_or_else(|| json_error(400, "Invalid request")),
    }
}

/// A non-empty string field (0.5.2 tested truthiness; a non-string counts as missing).
fn nonempty<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    m.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn drop_processors(d: &AccountsDeps, username: &str, reason: &str) {
    if let Some(hook) = &d.hooks.drop_processor_account {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(username, reason)));
    }
}

/// `GET /api/account/stats`: all zeros (0.5.2 never computed them).
async fn stats(State(d): State<AccountsDeps>, client: Client, headers: HeaderMap) -> Response {
    if let Err(resp) = authenticate(&d, &client, &headers).await {
        return resp;
    }
    json_response(
        200,
        json!({
            "volumes": 0,
            "pages_read": 0,
            "characters_read": 0,
            "reading_time_seconds": 0,
            "reading_time_formatted": format_reading_time(0),
        }),
    )
}

/// `POST /api/account/password` `{current_password, new_password}`. Changing the
/// password revokes every token of the account, the caller's included.
async fn change_password(State(d): State<AccountsDeps>, client: Client, headers: HeaderMap, body: Body) -> Response {
    let user = match authenticate(&d, &client, &headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let data = match object_body(&headers, body).await {
        Ok(m) => m,
        Err(resp) => return resp,
    };
    let (Some(current), Some(new)) = (nonempty(&data, "current_password"), nonempty(&data, "new_password")) else {
        return json_error(400, "Missing required fields");
    };
    match confirm_password(&d, &client, &user.username, current).await {
        Ok(true) => {}
        Ok(false) => return json_error(401, "Current password is incorrect"),
        Err(resp) => return resp,
    }
    if let Some(msg) = validate_password(new) {
        return json_error(400, msg);
    }
    let (db, u, p) = (d.db.clone(), user.username.clone(), new.to_string());
    match blocking(move || db.update_user_password(&u, &p)).await {
        Ok(Ok(_)) => {}
        Ok(Err(DbError::Invalid(msg))) => return json_error(400, &msg),
        Ok(Err(e)) => return db_failed("password change", &e),
        Err(resp) => return resp,
    }
    drop_processors(&d, &user.username, "password changed");
    json_response(200, json!({ "success": true }))
}

/// `POST /api/account/delete` `{password}`: audit, soft-delete (tokens wiped), then
/// remove the account's progress directory.
async fn delete_account(State(d): State<AccountsDeps>, client: Client, headers: HeaderMap, body: Body) -> Response {
    let user = match authenticate(&d, &client, &headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let data = match object_body(&headers, body).await {
        Ok(m) => m,
        Err(resp) => return resp,
    };
    let Some(password) = nonempty(&data, "password") else {
        return json_error(400, "Password confirmation required");
    };
    match confirm_password(&d, &client, &user.username, password).await {
        Ok(true) => {}
        Ok(false) => return json_error(401, "Password is incorrect"),
        Err(resp) => return resp,
    }
    let users_root = d.core.layout.users();
    let (db, username) = (d.db.clone(), user.username.clone());
    let outcome = blocking(move || -> bunko_db::Result<bool> {
        // Logged before the deletion, as 0.5.2 did.
        db.log_audit_event(
            &NewAuditEvent::new("self_delete_account")
                .actor(Some(&username))
                .target_type("user")
                .target_username(&username),
        )?;
        db.delete_user(&username)?;
        Ok(remove_user_dir(&users_root, &username))
    })
    .await;
    let path_ok = match outcome {
        Ok(Ok(ok)) => ok,
        Ok(Err(e)) => return db_failed("account delete", &e),
        Err(resp) => return resp,
    };
    drop_processors(&d, &user.username, "account deleted");
    if !path_ok {
        // 0.5.2 order: the soft delete has already happened.
        return json_error(400, "Invalid username path");
    }
    json_response(200, json!({ "success": true }))
}

/// `rmtree(<storage>/users/<username>)`, refusing a name or symlink that leads outside
/// the users directory (`false`). Removal errors are ignored, as 0.5.2's
/// `ignore_errors=True`.
fn remove_user_dir(users_root: &FsPath, username: &str) -> bool {
    if username.is_empty() || username == "." || username.contains(['/', '\\']) || username.split('/').any(|s| s == "..") {
        return false;
    }
    let dir = users_root.join(username);
    if !dir.exists() {
        return true;
    }
    let (Ok(root), Ok(real)) = (users_root.canonicalize(), dir.canonicalize()) else { return true };
    if !real.starts_with(&root) || real == root {
        return false;
    }
    if let Err(e) = std::fs::remove_dir_all(&real) {
        warn!("could not remove {}: {e}", real.display());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_time() {
        assert_eq!(format_reading_time(0), "0s");
        assert_eq!(format_reading_time(59), "59s");
        assert_eq!(format_reading_time(60), "1m");
        assert_eq!(format_reading_time(3599), "59m");
        assert_eq!(format_reading_time(3600 + 120), "1h 2m");
        assert_eq!(format_reading_time(86400 * 2 + 3600 * 5 + 7), "2d 5h");
    }

    #[test]
    fn user_dir_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let users = tmp.path().join("users");
        std::fs::create_dir_all(users.join("alice/x")).unwrap();
        assert!(remove_user_dir(&users, "alice"));
        assert!(!users.join("alice").exists());
        assert!(remove_user_dir(&users, "nobody"));
        assert!(!remove_user_dir(&users, ".."));
        assert!(!remove_user_dir(&users, "a/b"));
        #[cfg(unix)]
        {
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, users.join("sneaky")).unwrap();
            assert!(!remove_user_dir(&users, "sneaky"));
            assert!(outside.exists());
        }
    }
}
