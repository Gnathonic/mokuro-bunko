//! Authentication and authorisation (0.5.2 `middleware/auth.py`).
//!
//! `authenticate` turns the `Authorization` header into an [`Identity`]; `authorize`
//! applies the exact decision table of spec http-webdav §4.3. UI/API routes outside the
//! DAV tree use `authenticate` directly and check roles themselves, as in 0.5.2.

pub mod paths;

use crate::http::limiter::AuthLimiter;
use base64::Engine as _;
use bunko_core::Role;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};

/// Capabilities derived from a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Perm {
    Read,
    WriteProgress,
    AddFiles,
    ModifyDelete,
    ManageInvites,
    Admin,
    Process,
}

pub fn has_perm(role: Role, perm: Perm) -> bool {
    use Role::*;
    match perm {
        Perm::Read => true,
        Perm::WriteProgress => matches!(role, Registered | Uploader | Inviter | Editor | Admin),
        Perm::AddFiles => matches!(role, Uploader | Inviter | Editor | Admin),
        Perm::ModifyDelete => matches!(role, Inviter | Editor | Admin),
        Perm::ManageInvites => matches!(role, Inviter | Admin),
        Perm::Admin => role == Admin,
        Perm::Process => role == Processor,
    }
}

/// The authenticated user as the request sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthUser {
    pub id: i64,
    pub username: String,
    pub role: Role,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub user: Option<AuthUser>,
    /// A credential was presented and refused (401/429 text).
    pub error: Option<String>,
    /// The username of a refused Basic attempt (for processor refusal reports).
    pub attempted_username: Option<String>,
    /// The refused credential was a bearer token.
    pub bearer_failed: bool,
}

impl Identity {
    pub fn role(&self) -> Role {
        self.user.as_ref().map(|u| u.role).unwrap_or(Role::Anonymous)
    }
    pub fn username(&self) -> Option<&str> {
        self.user.as_ref().map(|u| u.username.as_str())
    }
    pub fn authenticated(&self) -> bool {
        self.user.is_some()
    }
}

/// What authentication and the ownership rules need from the database.
pub trait AuthBackend: Send + Sync {
    /// Live user for a bearer token (disabled/deleted/expired → None).
    fn resolve_token(&self, token: &str) -> Option<AuthUser>;
    /// Password check for an active user.
    fn check_password(&self, username: &str, password: &str) -> Option<AuthUser>;
    /// Uploader ownership of a library file (and its sidecars/layers).
    fn can_user_delete_library_path(&self, username: &str, virtual_path: &str) -> bool;
    /// Uploader owns every tracked volume of the series.
    fn can_user_edit_series(&self, username: &str, series_title: &str) -> bool;
    /// Does the virtual path map to an existing physical file?
    fn physical_exists(&self, virtual_path: &str, username: Option<&str>) -> bool;
}

/// A `Basic` header that is present but unusable (bad base64, not UTF-8, no colon).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MalformedBasic;

/// Parse `Basic` credentials: `Ok(None)` for no Basic header, `Err` for a malformed one.
pub fn parse_basic(header: &str) -> Result<Option<(String, String)>, MalformedBasic> {
    let Some(b64) = header.strip_prefix("Basic ") else { return Ok(None) };
    let raw = base64::engine::general_purpose::STANDARD.decode(b64.trim()).map_err(|_| MalformedBasic)?;
    let text = String::from_utf8(raw).map_err(|_| MalformedBasic)?;
    let (u, p) = text.split_once(':').ok_or(MalformedBasic)?;
    Ok(Some((u.to_string(), p.to_string())))
}

/// 0.5.2 `AuthMiddleware.authenticate`.
pub fn authenticate(headers: &HeaderMap, client_ip: &str, backend: &dyn AuthBackend, limiter: &AuthLimiter) -> Identity {
    let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return Identity::default();
    };
    if let Some(token) = value.strip_prefix("Bearer ") {
        return match backend.resolve_token(token.trim()) {
            Some(user) => Identity { user: Some(user), ..Default::default() },
            None => Identity { error: Some("Invalid or expired token".into()), bearer_failed: true, ..Default::default() },
        };
    }
    match parse_basic(value) {
        Ok(None) => Identity::default(),
        Err(MalformedBasic) => Identity { error: Some("Invalid authorization header".into()), ..Default::default() },
        Ok(Some((username, password))) => {
            let key = format!("{client_ip}:{username}");
            if let Err(retry) = limiter.allow(&key) {
                return Identity {
                    error: Some(format!("Too many failed attempts. Retry in {retry}s")),
                    attempted_username: Some(username),
                    ..Default::default()
                };
            }
            match backend.check_password(&username, &password) {
                Some(user) => {
                    limiter.record_success(&key);
                    Identity { user: Some(user), ..Default::default() }
                }
                None => {
                    limiter.record_failure(&key);
                    Identity { error: Some("Invalid credentials".into()), attempted_username: Some(username), ..Default::default() }
                }
            }
        }
    }
}

/// A refusal: status + plain-text message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    pub status: StatusCode,
    pub message: String,
    pub bearer: bool,
}

impl Denied {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status: StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN), message: message.into(), bearer: false }
    }

    /// 0.5.2 `_error_response`: text/plain body, WWW-Authenticate on 401.
    pub fn into_response(self) -> axum::response::Response {
        let mut resp = axum::response::Response::new(axum::body::Body::from(self.message));
        *resp.status_mut() = self.status;
        let h = resp.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
        if self.status == StatusCode::UNAUTHORIZED {
            let v = if self.bearer {
                "Bearer realm=\"mokuro-bunko\", error=\"invalid_token\""
            } else {
                "Basic realm=\"mokuro-bunko\", charset=\"UTF-8\""
            };
            h.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static(v));
        }
        resp
    }
}

/// Live anonymous-access flags (read from config on every request).
#[derive(Debug, Clone, Copy)]
pub struct AnonymousAccess {
    pub browse: bool,
    pub download: bool,
}

fn need(id: &Identity, perm: Perm, forbidden: &str) -> Result<(), Denied> {
    if has_perm(id.role(), perm) {
        Ok(())
    } else if !id.authenticated() {
        Err(Denied::new(401, "Authentication required"))
    } else {
        Err(Denied::new(403, forbidden))
    }
}

fn compiled_denied(id: &Identity) -> Denied {
    if id.authenticated() {
        Denied::new(403, "Permission denied: this file is compiled by the server")
    } else {
        Denied::new(401, "Authentication required")
    }
}

fn progress_write(id: &Identity) -> Result<(), Denied> {
    if !id.authenticated() {
        return Err(Denied::new(401, "Authentication required to save progress"));
    }
    if !has_perm(id.role(), Perm::WriteProgress) {
        return Err(Denied::new(403, "Permission denied: cannot save progress"));
    }
    if id.username().is_some() { Ok(()) } else { Err(Denied::new(403, "Cannot write to other users' progress")) }
}

/// The decision table of spec http-webdav §4.3, in order.
pub fn authorize(
    method: &Method,
    path: &str,
    destination: Option<&str>,
    id: &Identity,
    anon: AnonymousAccess,
    backend: &dyn AuthBackend,
) -> Result<(), Denied> {
    use paths::*;
    if *method == Method::OPTIONS {
        return Ok(());
    }
    if let (Some(err), false) = (&id.error, id.authenticated()) {
        let status = if err.contains("Too many failed attempts") { 429 } else { 401 };
        let mut d = Denied::new(status, err.clone());
        d.bearer = id.bearer_failed;
        return Err(d);
    }
    if is_processor_path(path) {
        return need(id, Perm::Process, "Processor access required");
    }
    if is_admin_path(path) {
        if (*method == Method::GET || *method == Method::HEAD) && !path.contains("/api/") {
            return Ok(());
        }
        if is_invites_admin_api_path(path) {
            return need(id, Perm::ManageInvites, "Invite management access required");
        }
        return need(id, Perm::Admin, "Admin access required");
    }
    let m = method.as_str();
    if matches!(m, "DELETE" | "MOVE" | "COPY" | "PROPPATCH" | "MKCOL" | "LOCK" | "UNLOCK") && is_compiled_metadata_path(path) {
        return Err(compiled_denied(id));
    }
    if matches!(m, "MOVE" | "COPY") && destination.and_then(destination_path).is_some_and(|d| is_compiled_metadata_path(&d)) {
        return Err(compiled_denied(id));
    }
    match m {
        "GET" | "HEAD" | "PROPFIND" => {
            if id.authenticated() {
                return Ok(());
            }
            let refuse = if m == "PROPFIND" {
                !anon.browse
            } else if is_library_path(path) {
                !anon.download
            } else {
                !anon.browse && (path == "/" || path == "/mokuro-reader")
            };
            if refuse { Err(Denied::new(401, "Authentication required")) } else { Ok(()) }
        }
        "PUT" => authorize_put(path, id, backend),
        "MKCOL" => {
            if is_library_path(path) {
                need(id, Perm::AddFiles, "Permission denied: cannot create directories")
            } else {
                Err(Denied::new(403, "Permission denied: unsupported target path"))
            }
        }
        "DELETE" => {
            if is_progress_file(path) {
                return progress_write(id);
            }
            if id.role() == Role::Uploader
                && is_library_path(path)
                && id.username().is_some_and(|u| backend.can_user_delete_library_path(u, path))
            {
                return Ok(());
            }
            need(id, Perm::ModifyDelete, "Permission denied: cannot modify or delete files")
        }
        "MOVE" | "COPY" => {
            if is_progress_file(path) {
                return progress_write(id);
            }
            need(id, Perm::ModifyDelete, "Permission denied: cannot modify or delete files")
        }
        "LOCK" | "UNLOCK" => {
            if is_progress_file(path) {
                return progress_write(id);
            }
            need(id, Perm::ModifyDelete, "Permission denied")
        }
        "PROPPATCH" => need(id, Perm::ModifyDelete, "Permission denied"),
        _ => Ok(()),
    }
}

fn authorize_put(path: &str, id: &Identity, backend: &dyn AuthBackend) -> Result<(), Denied> {
    use paths::*;
    if is_progress_file(path) {
        return progress_write(id);
    }
    if let Some(series) = series_title_from_series_file_path(path) {
        if !id.authenticated() {
            return Err(Denied::new(401, "Authentication required"));
        }
        if has_perm(id.role(), Perm::ModifyDelete) {
            return Ok(());
        }
        if id.role() == Role::Uploader && id.username().is_some_and(|u| backend.can_user_edit_series(u, &series)) {
            return Ok(());
        }
        return Err(Denied::new(403, "Permission denied: cannot submit metadata updates for this series"));
    }
    if is_compiled_metadata_path(path) {
        return Err(compiled_denied(id));
    }
    if is_library_path(path) {
        need(id, Perm::AddFiles, "Permission denied: cannot add files")?;
        if !has_perm(id.role(), Perm::ModifyDelete) {
            let user = id.username();
            if backend.physical_exists(path, user) && !user.is_some_and(|u| backend.can_user_delete_library_path(u, path)) {
                return Err(Denied::new(403, "Permission denied: cannot replace a file another account uploaded"));
            }
        }
        return Ok(());
    }
    Err(Denied::new(403, "Permission denied: unsupported target path"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct B;
    impl AuthBackend for B {
        fn resolve_token(&self, t: &str) -> Option<AuthUser> {
            (t == "good").then(|| AuthUser { id: 1, username: "u".into(), role: Role::Uploader })
        }
        fn check_password(&self, u: &str, p: &str) -> Option<AuthUser> {
            (p == "pw").then(|| AuthUser { id: 2, username: u.into(), role: Role::Editor })
        }
        fn can_user_delete_library_path(&self, u: &str, p: &str) -> bool {
            u == "u" && p.ends_with("mine.cbz")
        }
        fn can_user_edit_series(&self, _: &str, s: &str) -> bool {
            s == "Mine"
        }
        fn physical_exists(&self, p: &str, _: Option<&str>) -> bool {
            p.ends_with(".cbz")
        }
    }

    fn id(role: Option<Role>) -> Identity {
        Identity { user: role.map(|r| AuthUser { id: 1, username: "u".into(), role: r }), ..Default::default() }
    }
    const OPEN: AnonymousAccess = AnonymousAccess { browse: true, download: true };

    #[test]
    fn authenticate_paths() {
        let lim = AuthLimiter::default();
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer good".parse().unwrap());
        assert_eq!(authenticate(&h, "1.1.1.1", &B, &lim).role(), Role::Uploader);
        h.insert(header::AUTHORIZATION, "Bearer bad".parse().unwrap());
        let i = authenticate(&h, "1.1.1.1", &B, &lim);
        assert!(i.bearer_failed);
        let basic = base64::engine::general_purpose::STANDARD.encode("ed:pw");
        h.insert(header::AUTHORIZATION, format!("Basic {basic}").parse().unwrap());
        assert_eq!(authenticate(&h, "1.1.1.1", &B, &lim).role(), Role::Editor);
        h.insert(header::AUTHORIZATION, "Basic !!!".parse().unwrap());
        assert_eq!(authenticate(&h, "1.1.1.1", &B, &lim).error.as_deref(), Some("Invalid authorization header"));
        let bad = base64::engine::general_purpose::STANDARD.encode("ed:nope");
        h.insert(header::AUTHORIZATION, format!("Basic {bad}").parse().unwrap());
        for _ in 0..10 {
            assert_eq!(authenticate(&h, "1.1.1.1", &B, &lim).error.as_deref(), Some("Invalid credentials"));
        }
        assert!(authenticate(&h, "1.1.1.1", &B, &lim).error.unwrap().starts_with("Too many failed attempts. Retry in 900s"));
    }

    #[test]
    fn table() {
        let get = Method::GET;
        let put = Method::PUT;
        let del = Method::DELETE;
        let anon = id(None);
        assert!(authorize(&get, "/mokuro-reader/a.cbz", None, &anon, OPEN, &B).is_ok());
        let closed = AnonymousAccess { browse: false, download: false };
        assert_eq!(authorize(&get, "/mokuro-reader/a.cbz", None, &anon, closed, &B).unwrap_err().status, 401);
        assert!(authorize(&get, "/_static/x", None, &anon, closed, &B).is_ok());
        assert_eq!(authorize(&get, "/mokuro-reader", None, &anon, closed, &B).unwrap_err().status, 401);
        assert_eq!(authorize(&put, "/", None, &anon, OPEN, &B).unwrap_err().status, 403);
        assert_eq!(authorize(&put, "/mokuro-reader/a.cbz", None, &id(Some(Role::Registered)), OPEN, &B).unwrap_err().message, "Permission denied: cannot add files");
        assert_eq!(
            authorize(&put, "/mokuro-reader/other.cbz", None, &id(Some(Role::Uploader)), OPEN, &B).unwrap_err().message,
            "Permission denied: cannot replace a file another account uploaded"
        );
        assert!(authorize(&put, "/mokuro-reader/mine.cbz", None, &id(Some(Role::Uploader)), OPEN, &B).is_ok());
        assert!(authorize(&put, "/mokuro-reader/new.mokuro", None, &id(Some(Role::Uploader)), OPEN, &B).is_ok());
        assert!(authorize(&del, "/mokuro-reader/mine.cbz", None, &id(Some(Role::Uploader)), OPEN, &B).is_ok());
        assert_eq!(authorize(&del, "/mokuro-reader/x.cbz", None, &id(Some(Role::Uploader)), OPEN, &B).unwrap_err().status, 403);
        assert_eq!(authorize(&del, "/mokuro-reader/catalog.json", None, &id(Some(Role::Admin)), OPEN, &B).unwrap_err().status, 403);
        assert!(authorize(&put, "/mokuro-reader/Mine/series.json", None, &id(Some(Role::Uploader)), OPEN, &B).is_ok());
        assert_eq!(authorize(&put, "/mokuro-reader/Theirs/series.json", None, &id(Some(Role::Uploader)), OPEN, &B).unwrap_err().status, 403);
        assert_eq!(authorize(&put, "/mokuro-reader/volume-data.json", None, &anon, OPEN, &B).unwrap_err().message, "Authentication required to save progress");
        assert!(authorize(&put, "/mokuro-reader/volume-data.json", None, &id(Some(Role::Registered)), OPEN, &B).is_ok());
        let mv = Method::from_bytes(b"MOVE").unwrap();
        assert_eq!(
            authorize(&mv, "/mokuro-reader/a.json", Some("http://h/mokuro-reader/catalog.json"), &id(Some(Role::Admin)), OPEN, &B).unwrap_err().status,
            403
        );
        assert_eq!(authorize(&get, "/_processor/x", None, &id(Some(Role::Admin)), OPEN, &B).unwrap_err().message, "Processor access required");
        assert!(authorize(&get, "/_admin/", None, &anon, OPEN, &B).is_ok());
        assert_eq!(authorize(&get, "/_admin/api/users", None, &anon, OPEN, &B).unwrap_err().status, 401);
        assert!(authorize(&get, "/_admin/api/invites", None, &id(Some(Role::Inviter)), OPEN, &B).is_ok());
        let failed = Identity { error: Some("Invalid credentials".into()), ..Default::default() };
        assert_eq!(authorize(&get, "/mokuro-reader/a.cbz", None, &failed, OPEN, &B).unwrap_err().status, 401);
        assert!(authorize(&Method::OPTIONS, "/x", None, &failed, OPEN, &B).is_ok());
    }
}
