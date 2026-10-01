//! Login page and its API (0.5.2 `login/api.py`, spec db-auth-admin §15): the password
//! check, bearer tokens, `/login/api/me` and the shared nav config.
//!
//! Password checks count in `core.login_limiter` (key `ip:username`), separate from
//! the WebDAV limiter as in 0.5.2. Bearer tokens are never rate limited. No response
//! here carries `WWW-Authenticate`: the callers are `fetch()`es and a browser Basic
//! dialog must never pop up.

use super::AccountsDeps;
use super::util::{
    Client, JsonBody, blocking, db_failed, json_error, json_response, limited_message, parse_object, read_body,
    serve_page_text_errors, str_field,
};
use crate::auth::{Perm, has_perm, parse_basic};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use bunko_core::Role;
use bunko_db::pyfmt::truthy;
use bunko_db::{TokenKind, User};
use http::{HeaderMap, header};
use serde_json::{Value, json};
use std::str::FromStr;

pub fn routes() -> Router<AccountsDeps> {
    Router::new()
        .route("/login/api/check", post(check))
        .route("/login/api/token", post(issue_token).delete(revoke_token))
        .route("/login/api/me", get(me))
        .route("/api/nav/config", get(nav_config))
        .route("/login", get(index))
        .route("/login/", get(index))
        .route("/login/{*file}", get(file))
}

async fn index() -> Response {
    serve_page_text_errors("login", "index.html")
}

async fn file(Path(file): Path<String>) -> Response {
    serve_page_text_errors("login", &file)
}

/// The `Authorization` header as the login API reads it.
pub(super) enum AuthHeader {
    None,
    /// `Bearer <t>` (trimmed; possibly empty, which fails as an invalid token).
    Bearer(String),
    /// Basic credentials.
    Basic(String, String),
    /// A `Basic ` header that does not decode to UTF-8 `user:password`.
    Malformed,
}

pub(super) fn auth_header(headers: &HeaderMap) -> AuthHeader {
    let Some(raw) = headers.get(header::AUTHORIZATION) else { return AuthHeader::None };
    let Ok(value) = raw.to_str() else {
        // Non-ASCII header bytes: a Basic payload cannot be valid base64.
        return if raw.as_bytes().starts_with(b"Basic ") {
            AuthHeader::Malformed
        } else if raw.as_bytes().starts_with(b"Bearer ") {
            AuthHeader::Bearer(String::from_utf8_lossy(&raw.as_bytes()[7..]).trim().to_string())
        } else {
            AuthHeader::None
        };
    };
    if let Some(token) = value.strip_prefix("Bearer ") {
        return AuthHeader::Bearer(token.trim().to_string());
    }
    match parse_basic(value) {
        Ok(Some((u, p))) => AuthHeader::Basic(u, p),
        Ok(None) => AuthHeader::None,
        Err(_) => AuthHeader::Malformed,
    }
}

/// `POST /login/api/check` (§15.1): the legacy credential check.
async fn check(State(d): State<AccountsDeps>, client: Client, headers: HeaderMap, body: Body) -> Response {
    let data = match read_body(&headers, body).await {
        JsonBody::Empty => return json_error(400, "Missing credentials"),
        JsonBody::TooLarge => return json_error(413, "Request body too large"),
        JsonBody::Bytes(b) => match parse_object(&b) {
            Some(m) => m,
            None => return json_error(400, "Invalid request"),
        },
    };
    let (Some(username), Some(password)) = (str_field(&data, "username"), str_field(&data, "password")) else {
        return json_error(400, "Missing credentials");
    };
    if username.is_empty() || password.is_empty() {
        return json_error(400, "Missing credentials");
    }
    let key = client.limiter_key(username);
    if let Err(retry) = d.core.login_limiter.allow(&key) {
        return json_error(429, &limited_message(retry));
    }
    let (db, u, p) = (d.db.clone(), username.to_string(), password.to_string());
    let user = match blocking(move || db.authenticate_user(&u, &p)).await {
        Ok(Ok(user)) => user,
        Ok(Err(e)) => return db_failed("password check", &e),
        Err(resp) => return resp,
    };
    match user {
        Some(user) => {
            d.core.login_limiter.record_success(&key);
            json_response(200, json!({"success": true, "user": {"username": user.username, "role": user.role.as_str()}}))
        }
        None => {
            d.core.login_limiter.record_failure(&key);
            json_error(401, "Invalid credentials")
        }
    }
}

/// `POST /login/api/token` (§15.2): a password (JSON body or Basic header) buys a
/// bearer token. `kind` sets its lifetime, `label` names its holder.
async fn issue_token(State(d): State<AccountsDeps>, client: Client, headers: HeaderMap, body: Body) -> Response {
    let data = match read_body(&headers, body).await {
        JsonBody::Empty => serde_json::Map::new(),
        JsonBody::TooLarge => return json_error(413, "Request body too large"),
        JsonBody::Bytes(b) => match parse_object(&b) {
            Some(m) => m,
            None => return json_error(400, "Invalid request"),
        },
    };
    let mut username = data.get("username").cloned().unwrap_or(Value::Null);
    let mut password = data.get("password").cloned().unwrap_or(Value::Null);
    if !truthy(&username) && !truthy(&password) {
        match auth_header(&headers) {
            AuthHeader::Malformed => return json_error(400, "Invalid credentials"),
            AuthHeader::Basic(u, p) => {
                username = Value::String(u);
                password = Value::String(p);
            }
            AuthHeader::None | AuthHeader::Bearer(_) => {}
        }
    }
    let (Some(username), Some(password)) = (username.as_str().filter(|s| !s.is_empty()), password.as_str().filter(|s| !s.is_empty()))
    else {
        return json_error(400, "Missing credentials");
    };
    let kind_value = data.get("kind").filter(|v| truthy(v));
    let kind = match kind_value {
        None => TokenKind::Web,
        Some(v) => match v.as_str().map(TokenKind::from_str) {
            Some(Ok(k)) => k,
            _ => return json_error(400, "kind must be one of web, reader, processor"),
        },
    };
    let label = str_field(&data, "label").unwrap_or("").to_string();

    let key = client.limiter_key(username);
    if let Err(retry) = d.core.login_limiter.allow(&key) {
        if kind == TokenKind::Processor {
            report_processor_refusal(&d, username, &client.ip);
        }
        return json_error(429, &limited_message(retry));
    }
    let (db, u, p) = (d.db.clone(), username.to_string(), password.to_string());
    let issued = blocking(move || -> bunko_db::Result<Option<(User, String, f64)>> {
        let Some(user) = db.authenticate_user(&u, &p)? else { return Ok(None) };
        db.prune_expired_auth_tokens()?;
        let (token, expires_at) = db.create_auth_token(&user.username, kind, &label, None)?;
        Ok(Some((user, token, expires_at)))
    })
    .await;
    let issued = match issued {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return db_failed("token issue", &e),
        Err(resp) => return resp,
    };
    match issued {
        None => {
            d.core.login_limiter.record_failure(&key);
            if kind == TokenKind::Processor {
                report_processor_refusal(&d, username, &client.ip);
            }
            json_error(401, "Invalid credentials")
        }
        Some((user, token, expires_at)) => {
            d.core.login_limiter.record_success(&key);
            json_response(
                200,
                json!({
                    "token": token,
                    "token_type": "Bearer",
                    "kind": kind.as_str(),
                    "expires_at": expires_at,
                    "user": {"username": user.username, "role": user.role.as_str()},
                }),
            )
        }
    }
}

fn report_processor_refusal(d: &AccountsDeps, username: &str, ip: &str) {
    if let Some(hook) = &d.hooks.on_processor_login_refused {
        // A listener never breaks a refusal (0.5.2 swallowed its exceptions).
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(username, ip)));
    }
}

/// `DELETE /login/api/token` (§15.3): sign out the presented bearer token.
async fn revoke_token(State(d): State<AccountsDeps>, headers: HeaderMap) -> Response {
    let token = match auth_header(&headers) {
        AuthHeader::Bearer(t) if !t.is_empty() => t,
        _ => return json_error(400, "No bearer token"),
    };
    let db = d.db.clone();
    match blocking(move || db.revoke_auth_token(&token)).await {
        Ok(Ok(revoked)) => json_response(200, json!({ "revoked": revoked })),
        Ok(Err(e)) => db_failed("token revoke", &e),
        Err(resp) => resp,
    }
}

/// The client-facing permissions object (§15.4 `P`): `metadata` nests inside, which is
/// where the reader's `identity.ts` reads it.
fn permissions(role: Role, owned_series: Option<Vec<String>>) -> Value {
    let metadata = if has_perm(role, Perm::ModifyDelete) {
        json!({"scope": "all"})
    } else if let (Role::Uploader, Some(owned)) = (role, owned_series) {
        json!({"scope": "owned", "ownedSeries": owned})
    } else {
        json!({"scope": "none"})
    };
    json!({
        "canWriteProgress": has_perm(role, Perm::WriteProgress),
        "canAddFiles": has_perm(role, Perm::AddFiles),
        "canModifyDelete": has_perm(role, Perm::ModifyDelete),
        "metadata": metadata,
    })
}

/// The 200 identity body for a signed-in user (the uploader's owned series are read
/// from the ownership table).
fn identity_body(db: &bunko_db::Database, user: &User) -> bunko_db::Result<Value> {
    let owned = if user.role == Role::Uploader { Some(db.list_series_owned_by(&user.username)?) } else { None };
    Ok(json!({
        "authenticated": true,
        "username": user.username,
        "role": user.role.as_str(),
        "created_at": user.created_at,
        "permissions": permissions(user.role, owned),
    }))
}

fn unauthenticated(status: u16, error: &str) -> Response {
    json_response(status, json!({"authenticated": false, "error": error}))
}

/// `GET /login/api/me` (§15.4). `authenticated` is in every answer, 401/429 included.
async fn me(State(d): State<AccountsDeps>, client: Client, headers: HeaderMap) -> Response {
    let db = d.db.clone();
    match auth_header(&headers) {
        AuthHeader::Bearer(token) => {
            let result = blocking(move || -> bunko_db::Result<Option<Value>> {
                match db.resolve_auth_token(&token)? {
                    Some(user) => identity_body(&db, &user).map(Some),
                    None => Ok(None),
                }
            })
            .await;
            match result {
                Ok(Ok(Some(body))) => json_response(200, body),
                Ok(Ok(None)) => unauthenticated(401, "Invalid or expired token"),
                Ok(Err(e)) => db_failed("token lookup", &e),
                Err(resp) => resp,
            }
        }
        // A garbled header: 401 without touching the limiter.
        AuthHeader::Malformed => unauthenticated(401, "Invalid credentials"),
        AuthHeader::None => json_response(
            200,
            json!({"authenticated": false, "role": "anonymous", "permissions": permissions(Role::Anonymous, None)}),
        ),
        AuthHeader::Basic(username, password) => {
            let key = client.limiter_key(&username);
            if let Err(retry) = d.core.login_limiter.allow(&key) {
                return unauthenticated(429, &limited_message(retry));
            }
            let result = blocking(move || -> bunko_db::Result<Option<Value>> {
                match db.authenticate_user(&username, &password)? {
                    Some(user) => identity_body(&db, &user).map(Some),
                    None => Ok(None),
                }
            })
            .await;
            match result {
                Ok(Ok(Some(body))) => {
                    d.core.login_limiter.record_success(&key);
                    json_response(200, body)
                }
                Ok(Ok(None)) => {
                    d.core.login_limiter.record_failure(&key);
                    unauthenticated(401, "Invalid credentials")
                }
                Ok(Err(e)) => db_failed("password check", &e),
                Err(resp) => resp,
            }
        }
    }
}

/// `GET /api/nav/config` (§15.5): header feature flags, from the live config.
async fn nav_config(State(d): State<AccountsDeps>) -> Response {
    let c = d.core.config.read();
    let body = json!({
        "home_enabled": !(c.catalog.enabled && c.catalog.use_as_homepage),
        "catalog_enabled": c.catalog.enabled,
        "queue_show_in_nav": c.queue.show_in_nav,
        "queue_public_access": c.queue.public_access,
        "registration_enabled": c.registration.mode != "disabled",
    });
    drop(c);
    json_response(200, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_per_role() {
        let p = permissions(Role::Uploader, Some(vec!["A".into()]));
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            r#"{"canWriteProgress":true,"canAddFiles":true,"canModifyDelete":false,"metadata":{"scope":"owned","ownedSeries":["A"]}}"#
        );
        assert_eq!(permissions(Role::Inviter, None)["metadata"], json!({"scope": "all"}));
        assert_eq!(permissions(Role::Processor, None)["metadata"], json!({"scope": "none"}));
        assert_eq!(permissions(Role::Anonymous, None)["canWriteProgress"], json!(false));
    }

    #[test]
    fn header_forms() {
        let mut h = HeaderMap::new();
        assert!(matches!(auth_header(&h), AuthHeader::None));
        h.insert(header::AUTHORIZATION, "Negotiate xyz".parse().unwrap());
        assert!(matches!(auth_header(&h), AuthHeader::None));
        h.insert(header::AUTHORIZATION, "Bearer  abc ".parse().unwrap());
        assert!(matches!(auth_header(&h), AuthHeader::Bearer(t) if t == "abc"));
        h.insert(header::AUTHORIZATION, "Basic !!!notb64!!!".parse().unwrap());
        assert!(matches!(auth_header(&h), AuthHeader::Malformed));
        h.insert(header::AUTHORIZATION, http::HeaderValue::from_bytes(b"Basic \xe4").unwrap());
        assert!(matches!(auth_header(&h), AuthHeader::Malformed));
    }
}
