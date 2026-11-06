//! Login page and its API (0.5.2 `login/api.py`, spec db-auth-admin §15): the password
//! check, bearer tokens, `/login/api/me` and the shared nav config.
//!
//! Password checks count in `core.login_limiter` (key `ip:username`), separate from
//! the WebDAV limiter as in 0.5.2. Bearer tokens are never rate limited. No response
//! here carries `WWW-Authenticate`: the callers are `fetch()`es and a browser Basic
//! dialog must never pop up.

use super::AccountsDeps;
use super::util::{
    Client, JsonBody, blocking, db_failed, json_error, json_response, limited_message,
    parse_object, read_body, serve_page_text_errors, str_field,
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
use std::collections::{BTreeMap, HashSet};
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
    let Some(raw) = headers.get(header::AUTHORIZATION) else {
        return AuthHeader::None;
    };
    let Ok(value) = raw.to_str() else {
        // Non-ASCII header bytes: a Basic payload cannot be valid base64.
        return if raw.as_bytes().starts_with(b"Basic ") {
            AuthHeader::Malformed
        } else if raw.as_bytes().starts_with(b"Bearer ") {
            AuthHeader::Bearer(
                String::from_utf8_lossy(&raw.as_bytes()[7..])
                    .trim()
                    .to_string(),
            )
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
async fn check(
    State(d): State<AccountsDeps>,
    client: Client,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let data = match read_body(&headers, body).await {
        JsonBody::Empty => return json_error(400, "Missing credentials"),
        JsonBody::TooLarge => return json_error(413, "Request body too large"),
        JsonBody::Bytes(b) => match parse_object(&b) {
            Some(m) => m,
            None => return json_error(400, "Invalid request"),
        },
    };
    let (Some(username), Some(password)) =
        (str_field(&data, "username"), str_field(&data, "password"))
    else {
        return json_error(400, "Missing credentials");
    };
    if username.is_empty() || password.is_empty() {
        return json_error(400, "Missing credentials");
    }
    let key = client.limiter_key(username);
    if let Err(retry) = d.core.login_limiter.allow(&key) {
        return attempted(json_error(429, &limited_message(retry)), username);
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
            signed_in(
                json_response(
                    200,
                    json!({"success": true, "user": {"username": user.username, "role": user.role.as_str()}}),
                ),
                &user,
            )
        }
        None => {
            d.core.login_limiter.record_failure(&key);
            attempted(json_error(401, "Invalid credentials"), username)
        }
    }
}

/// A refused sign-in names its account for the request accounting (failed logins per
/// IP and username); a response extension, never on the wire.
fn attempted(mut resp: Response, username: &str) -> Response {
    resp.extensions_mut()
        .insert(crate::analytics::LoginAttempt(username.to_string()));
    resp
}

/// A sign-in that succeeded names its account for the request accounting, so the
/// sign-in counts for that account and not for an anonymous visitor at its IP.
fn signed_in(mut resp: Response, user: &User) -> Response {
    resp.extensions_mut().insert(crate::analytics::SignedIn {
        username: user.username.clone(),
        role: user.role,
    });
    resp
}

/// `POST /login/api/token` (§15.2): a password (JSON body or Basic header) buys a
/// bearer token. `kind` sets its lifetime, `label` names its holder.
async fn issue_token(
    State(d): State<AccountsDeps>,
    client: Client,
    headers: HeaderMap,
    body: Body,
) -> Response {
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
    let Some(username) = username.as_str().filter(|s| !s.is_empty()) else {
        return json_error(400, "Missing credentials");
    };
    // No password: only an account that has none, from this computer (`local`).
    let password = password.as_str().unwrap_or("");
    if password.is_empty() && !super::local::passwordless_sign_in(&d.core, &client, &headers) {
        return json_error(400, "Missing credentials");
    }
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
        return attempted(json_error(429, &limited_message(retry)), username);
    }
    let (db, u, p) = (d.db.clone(), username.to_string(), password.to_string());
    let issued = blocking(move || -> bunko_db::Result<Option<(User, String, f64)>> {
        let user = if p.is_empty() {
            db.passwordless_user(&u)?
        } else {
            db.authenticate_user(&u, &p)?
        };
        let Some(user) = user else {
            return Ok(None);
        };
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
            attempted(json_error(401, "Invalid credentials"), username)
        }
        Some((user, token, expires_at)) => {
            d.core.login_limiter.record_success(&key);
            let mut who = json!({"username": user.username, "role": user.role.as_str()});
            if password.is_empty() {
                // The account page then offers to set its first password.
                who["has_password"] = json!(false);
            }
            signed_in(
                json_response(
                    200,
                    json!({
                        "token": token,
                        "token_type": "Bearer",
                        "kind": kind.as_str(),
                        "expires_at": expires_at,
                        "user": who,
                    }),
                ),
                &user,
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

/// An uploader's series folders (0.8), each as recorded, by code point: the ones it
/// owns and the ones another account owns (closed to it). Only folders listed now, as
/// auth checks them: a record whose folder went without a hook closes nothing.
///
/// What cannot be read is never reported as a shorter list, which a reader would act
/// on as true: unsure, a series is closed and not owned. With the library root
/// unlistable, every record of another account closes its series and none is owned;
/// with the records unreadable, `closed` is `None`: the account may add nowhere. Auth
/// answers those requests 503 meanwhile, and the reader's next /me corrects it.
///
/// `volumes` are the archives it may publish the curated layer of outside the series it
/// owns ([`curated_volumes`]); there a shorter list only offers less, so whatever cannot
/// be read is left out.
struct SeriesFolders {
    owned: Vec<String>,
    closed: Option<Vec<String>>,
    volumes: Vec<CuratedVolume>,
}

/// A volume outside the account's own series whose curated layer it may publish: its
/// series folder as listed, and the archive's stem.
struct CuratedVolume {
    series: String,
    volume: String,
}

/// The client-facing permissions object (§15.4 `P`): `metadata` nests inside, which is
/// where the reader's `identity.ts` reads it. `addFiles` (0.8) says where the account
/// may add files: anywhere, every series but the `closedSeries` other accounts own, or
/// nowhere. Its presence tells a reader the server enforces series owners.
fn permissions(role: Role, folders: Option<SeriesFolders>) -> Value {
    let (owned, closed, volumes) = match folders {
        Some(f) => (Some(f.owned), f.closed, f.volumes),
        None => (None, Some(Vec::new()), Vec::new()),
    };
    let metadata = if has_perm(role, Perm::ModifyDelete) {
        json!({"scope": "all"})
    } else if let (Role::Uploader, Some(owned)) = (role, owned) {
        json!({"scope": "owned", "ownedSeries": owned})
    } else {
        json!({"scope": "none"})
    };
    let add_files = if has_perm(role, Perm::ModifyDelete) {
        json!({"scope": "all"})
    } else if let (true, Some(closed)) = (has_perm(role, Perm::AddFiles), closed) {
        json!({"scope": "open", "closedSeries": closed})
    } else {
        json!({"scope": "none"})
    };
    json!({
        "canWriteProgress": has_perm(role, Perm::WriteProgress),
        "canAddFiles": has_perm(role, Perm::AddFiles),
        "canModifyDelete": has_perm(role, Perm::ModifyDelete),
        "metadata": metadata,
        "curated": curated(role, volumes),
        "addFiles": add_files,
    })
}

/// `curated`: its scope, and with `owned` the `volumes` it controls outside the series
/// it owns (those are `metadata.ownedSeries`), so a reader can offer Publish per volume
/// in a shared series.
fn curated(role: Role, volumes: Vec<CuratedVolume>) -> Value {
    let scope = curated_scope(role);
    if scope != "owned" {
        return json!({ "scope": scope });
    }
    let volumes: Vec<Value> = volumes
        .into_iter()
        .map(|v| json!({"series": v.series, "volume": v.volume}))
        .collect();
    json!({"scope": scope, "volumes": volumes})
}

/// Who may publish a volume's shared `curated` layer (D27; auth enforces it): every
/// volume; the volumes it controls (`owned`: its uploads, and every volume of a series
/// it owns; a 403 on any other is final); or none.
fn curated_scope(role: Role) -> &'static str {
    if has_perm(role, Perm::ModifyDelete) {
        "all"
    } else if role == Role::Uploader {
        "owned"
    } else {
        "none"
    }
}

/// See [`SeriesFolders`]. Blocking (lists the library root).
fn series_folders(
    db: &bunko_db::Database,
    library: &std::path::Path,
    username: &str,
) -> SeriesFolders {
    let records = db
        .series_folders_owned_by(username)
        .and_then(|owned| Ok((owned, db.series_folders_closed_to(username)?)));
    let (owned, closed) = match records {
        Ok(records) => records,
        Err(e) => {
            tracing::warn!("series folder owners for /me: {e}");
            return SeriesFolders {
                owned: Vec::new(),
                closed: None,
                volumes: Vec::new(),
            };
        }
    };
    let live = crate::library::owners::live_series_keys(library)
        .inspect_err(|e| {
            tracing::warn!("series folders for /me: the library root cannot be listed: {e}")
        })
        .ok();
    let volumes = match &live {
        Some(live) => {
            let keys = |rows: &[bunko_db::SeriesFolderOwner]| -> HashSet<String> {
                rows.iter().map(|r| r.series_key.clone()).collect()
            };
            curated_volumes(db, library, username, live, &keys(&owned), &keys(&closed))
        }
        None => Vec::new(),
    };
    // A record whose folder may or may not be listed closes its series, and owns nothing.
    let listed = |rows: Vec<bunko_db::SeriesFolderOwner>, unsure: bool| -> Vec<String> {
        rows.into_iter()
            .filter(|r| live.as_ref().map_or(unsure, |l| l.contains(&r.series_key)))
            .map(|r| r.folder_name)
            .collect()
    };
    SeriesFolders {
        owned: listed(owned, false),
        closed: Some(listed(closed, true)),
        volumes,
    }
}

/// The listed archives `username` controls in the series folders it does not own (D1),
/// by series then stem: those auth lets it publish the curated layer of. A volume is
/// controlled when every archive governing its `<stem>.curated.mokuro` (the nearest
/// listed one, `sidecar_names::governing_archives`) is its recorded upload; in a series
/// another account owns, only while that layer is listed (a new one there is refused).
/// `owned` and `closed` are the series keys of its own and other accounts' records,
/// `live` those listed at the root. Archives directly in a series folder only; a folder
/// or record that cannot be read adds nothing. Blocking.
fn curated_volumes(
    db: &bunko_db::Database,
    library: &std::path::Path,
    username: &str,
    live: &HashSet<String>,
    owned: &HashSet<String>,
    closed: &HashSet<String>,
) -> Vec<CuratedVolume> {
    use bunko_core::sidecar_names::{cbz_stem, governing_archives};
    use bunko_db::uploads::fold_series_title_key;
    let uploads = match db.volume_uploads_by(username) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("volume uploads for /me: {e}");
            return Vec::new();
        }
    };
    let mut by_folder: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for rel in &uploads {
        if let Some((folder, name)) = rel.split_once('/')
            && !name.contains('/')
        {
            by_folder.entry(folder).or_default().push(name);
        }
    }
    let mut out = Vec::new();
    for (folder, archives) in by_folder {
        let key = fold_series_title_key(folder);
        if !live.contains(&key) || owned.contains(&key) {
            continue;
        }
        let Ok(entries) = bunko_store::fs::read_dir(library.join(folder)) else {
            continue;
        };
        let names: Vec<String> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        let listed: Vec<&str> = names.iter().map(String::as_str).collect();
        for archive in archives {
            let Some(stem) = cbz_stem(archive).filter(|_| listed.contains(&archive)) else {
                continue;
            };
            let layer = format!("{stem}.curated.mokuro");
            if closed.contains(&key) && !listed.contains(&layer.as_str()) {
                continue;
            }
            let governors = governing_archives(&layer, &listed);
            let mine = !governors.is_empty()
                && governors.iter().all(|g| {
                    let rel = format!("{folder}/{g}");
                    uploads.binary_search(&rel).is_ok()
                });
            if mine {
                out.push(CuratedVolume {
                    series: folder.to_string(),
                    volume: stem.to_string(),
                });
            }
        }
    }
    out.sort_by(|a, b| (&a.series, &a.volume).cmp(&(&b.series, &b.volume)));
    out
}

/// The 200 identity body for a signed-in user (an uploader's series folders are read
/// from the owner records and the library root).
fn identity_body(db: &bunko_db::Database, library: &std::path::Path, user: &User) -> Value {
    let folders =
        (user.role == Role::Uploader).then(|| series_folders(db, library, &user.username));
    json!({
        "authenticated": true,
        "username": user.username,
        "role": user.role.as_str(),
        "created_at": user.created_at,
        "permissions": permissions(user.role, folders),
        "per_user_files": bunko_core::reader_tree::advertised(),
        "per_user_layers": bunko_core::sidecar_names::per_user_layers(),
        "version": bunko_core::VERSION,
    })
}

fn unauthenticated(status: u16, error: &str) -> Response {
    json_response(status, json!({"authenticated": false, "error": error}))
}

/// `GET /login/api/me` (§15.4). `authenticated` is in every answer, 401/429 included.
async fn me(State(d): State<AccountsDeps>, client: Client, headers: HeaderMap) -> Response {
    let db = d.db.clone();
    let library = d.core.layout.library();
    match auth_header(&headers) {
        AuthHeader::Bearer(token) => {
            let result = blocking(move || -> bunko_db::Result<Option<Value>> {
                match db.resolve_auth_token(&token)? {
                    Some(user) => Ok(Some(identity_body(&db, &library, &user))),
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
            json!({
                "authenticated": false,
                "role": "anonymous",
                "permissions": permissions(Role::Anonymous, None),
                // What this server keeps per account (`history/`: a directory tree): a
                // reader uses a name (positions.json, history/) only once it is listed.
                "per_user_files": bunko_core::reader_tree::advertised(),
                // The layer ids each account keeps its own copy of (D27): a reader
                // pushes its corrections only when `edit` is listed.
                "per_user_layers": bunko_core::sidecar_names::per_user_layers(),
                // Reported only by builds that keep history per account (0.8.0 on): a
                // reader turns history sync on from the numeric core.
                "version": bunko_core::VERSION,
            }),
        ),
        AuthHeader::Basic(username, password) => {
            let key = client.limiter_key(&username);
            if let Err(retry) = d.core.login_limiter.allow(&key) {
                return unauthenticated(429, &limited_message(retry));
            }
            let result = blocking(move || -> bunko_db::Result<Option<Value>> {
                match db.authenticate_user(&username, &password)? {
                    Some(user) => Ok(Some(identity_body(&db, &library, &user))),
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
        "serve_readers": c.server.serve_readers,
    });
    drop(c);
    json_response(200, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_per_role() {
        let folders = SeriesFolders {
            owned: vec!["A".into()],
            closed: Some(vec!["B".into()]),
            volumes: vec![CuratedVolume {
                series: "C".into(),
                volume: "Vol 1".into(),
            }],
        };
        let p = permissions(Role::Uploader, Some(folders));
        assert_eq!(
            permissions(
                Role::Uploader,
                Some(SeriesFolders {
                    owned: Vec::new(),
                    closed: None,
                    volumes: Vec::new(),
                })
            )["addFiles"],
            json!({"scope": "none"})
        );
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            r#"{"canWriteProgress":true,"canAddFiles":true,"canModifyDelete":false,"metadata":{"scope":"owned","ownedSeries":["A"]},"curated":{"scope":"owned","volumes":[{"series":"C","volume":"Vol 1"}]},"addFiles":{"scope":"open","closedSeries":["B"]}}"#
        );
        // Where an account may add files (0.8; its presence is the capability).
        for (role, scope) in [
            (Role::Admin, json!({"scope": "all"})),
            (Role::Editor, json!({"scope": "all"})),
            (Role::Inviter, json!({"scope": "all"})),
            (Role::Uploader, json!({"scope": "open", "closedSeries": []})),
            (Role::Registered, json!({"scope": "none"})),
            (Role::Processor, json!({"scope": "none"})),
            (Role::Anonymous, json!({"scope": "none"})),
        ] {
            assert_eq!(permissions(role, None)["addFiles"], scope, "{role:?}");
        }
        assert_eq!(
            permissions(Role::Inviter, None)["metadata"],
            json!({"scope": "all"})
        );
        assert_eq!(
            permissions(Role::Processor, None)["metadata"],
            json!({"scope": "none"})
        );
        // Who may publish the shared `curated` layer (D27): an uploader only on what
        // it uploaded, even before it owns a series, its volumes outside the series it
        // owns listed (D1).
        for (role, curated) in [
            (Role::Admin, json!({"scope": "all"})),
            (Role::Editor, json!({"scope": "all"})),
            (Role::Inviter, json!({"scope": "all"})),
            (Role::Uploader, json!({"scope": "owned", "volumes": []})),
            (Role::Registered, json!({"scope": "none"})),
            (Role::Processor, json!({"scope": "none"})),
            (Role::Anonymous, json!({"scope": "none"})),
        ] {
            assert_eq!(permissions(role, None)["curated"], curated, "{role:?}");
        }
        assert_eq!(
            permissions(Role::Anonymous, None)["canWriteProgress"],
            json!(false)
        );
    }

    /// bob, an uploader, in a library where alice owns `S/` and bob owns `B/`.
    fn owners_library() -> (
        tempfile::TempDir,
        bunko_db::Database,
        std::path::PathBuf,
        User,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library");
        for folder in ["S", "B"] {
            bunko_store::fs::create_dir_all(library.join(folder)).unwrap();
        }
        let db = bunko_db::Database::open(dir.path().join("mokuro.db")).unwrap();
        db.record_series_folder_owner("S", "alice").unwrap();
        db.record_series_folder_owner("B", "bob").unwrap();
        let bob = User {
            id: 2,
            username: "bob".into(),
            role: Role::Uploader,
            status: bunko_db::UserStatus::Active,
            notes: String::new(),
            created_at: "2026-10-31 00:00:00".into(),
        };
        (dir, db, library, bob)
    }

    fn reported(db: &bunko_db::Database, library: &std::path::Path, user: &User) -> Value {
        identity_body(db, library, user)["permissions"].clone()
    }

    #[test]
    fn the_series_lists_are_the_live_records() {
        let (_dir, db, library, bob) = owners_library();
        let p = reported(&db, &library, &bob);
        assert_eq!(
            p["addFiles"],
            json!({"scope": "open", "closedSeries": ["S"]})
        );
        assert_eq!(
            p["metadata"],
            json!({"scope": "owned", "ownedSeries": ["B"]})
        );
    }

    /// The volumes an uploader may publish the curated layer of outside the series it
    /// owns (D1): its listed uploads in shared folders, as auth decides (the nearest
    /// listed archive governs), and in another account's series only where the curated
    /// layer is there already (a new one there is refused).
    #[test]
    fn curated_volumes_are_the_uploads_it_controls_outside_its_series() {
        let (_dir, db, library, bob) = owners_library();
        for folder in ["Shared", "Zeta"] {
            bunko_store::fs::create_dir_all(library.join(folder)).unwrap();
        }
        db.record_series_folder_owner("Zeta", "carol").unwrap();
        let files = [
            ("Shared/Vol 1.cbz", Some("BOB")),
            ("Shared/Vol 2.cbz", Some("carol")),
            ("Shared/Vol 1.5.cbz", Some("bob")),
            // Hides carol's `V`'s curated layer: control passes up to `V`.
            ("Shared/V.cbz", Some("carol")),
            ("Shared/V.curated.cbz", Some("bob")),
            ("B/Mine.cbz", Some("bob")),
            ("S/Old.cbz", Some("bob")),
            ("S/Kept.cbz", Some("bob")),
            ("S/Kept.curated.mokuro", None),
            ("Loose.cbz", Some("bob")),
        ];
        for (rel, uploader) in files {
            bunko_store::fs::write(library.join(rel), b"x").unwrap();
            if let Some(u) = uploader {
                db.record_volume_upload(rel, u).unwrap();
            }
        }
        // Recorded, but no longer listed.
        db.record_volume_upload("Shared/Gone.cbz", "bob").unwrap();
        db.record_volume_upload("Elsewhere/Vol 1.cbz", "bob")
            .unwrap();
        let p = reported(&db, &library, &bob);
        assert_eq!(
            p["curated"],
            json!({"scope": "owned", "volumes": [
                {"series": "S", "volume": "Kept"},
                {"series": "Shared", "volume": "Vol 1"},
                {"series": "Shared", "volume": "Vol 1.5"},
            ]})
        );
        // Unreadable upload records report none (the reader just offers no Publish).
        db.with_writer_connection(|c| c.execute("DROP TABLE volume_uploads", []))
            .unwrap();
        let p = reported(&db, &library, &bob);
        assert_eq!(p["curated"], json!({"scope": "owned", "volumes": []}));
    }

    /// Owner records that cannot be read are no empty list: /me still answers (a reader
    /// takes a failed /me for a server that enforces no owners), saying the account may
    /// add nowhere and owns nothing until it asks again.
    #[test]
    fn unreadable_owner_records_close_every_series() {
        let (_dir, db, library, bob) = owners_library();
        db.with_writer_connection(|c| c.execute("DROP TABLE series_folder_owners", []))
            .unwrap();
        let p = reported(&db, &library, &bob);
        assert_eq!(p["addFiles"], json!({"scope": "none"}));
        assert_eq!(p["metadata"], json!({"scope": "owned", "ownedSeries": []}));
        assert_eq!(p["canAddFiles"], json!(true));
    }

    /// A library root that cannot be listed cannot tell which records are live: every
    /// series another account has a record for is reported closed, and none owned.
    #[cfg(unix)]
    #[test]
    fn an_unlistable_root_reports_every_record_closed() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, db, library, bob) = owners_library();
        db.record_series_folder_owner("Gone", "carol").unwrap();
        std::fs::set_permissions(&library, std::fs::Permissions::from_mode(0o311)).unwrap();
        let listable = bunko_store::fs::read_dir(&library).is_ok(); // root reads anything
        let p = reported(&db, &library, &bob);
        std::fs::set_permissions(&library, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !listable {
            assert_eq!(
                p["addFiles"],
                json!({"scope": "open", "closedSeries": ["Gone", "S"]})
            );
            assert_eq!(p["metadata"], json!({"scope": "owned", "ownedSeries": []}));
        }
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
        h.insert(
            header::AUTHORIZATION,
            http::HeaderValue::from_bytes(b"Basic \xe4").unwrap(),
        );
        assert!(matches!(auth_header(&h), AuthHeader::Malformed));
    }
}
