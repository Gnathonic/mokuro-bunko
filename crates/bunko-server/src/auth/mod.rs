//! Authentication and authorisation (0.5.2 `middleware/auth.py`).
//!
//! `authenticate` turns the `Authorization` header into an [`Identity`]; `authorize`
//! applies the exact decision table of spec http-webdav §4.3. UI/API routes outside the
//! DAV tree use `authenticate` directly and check roles themselves, as in 0.5.2.

pub mod paths;

use crate::http::limiter::AuthLimiter;
use base64::Engine as _;
use bunko_core::Role;
use bunko_core::reader_tree::Caller;
use bunko_core::sidecar_names::{self, LayerClass};
// Moved to bunko-core so bunko-dav can ask what an account may write (its Allow header).
pub use bunko_core::{Perm, has_perm};
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};

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
    /// The account owns a cloud library's root (its progress user), so its per-user
    /// files are the cloud root's own. Decided per request from the live config, after
    /// authentication; never true for anonymous.
    pub owns_cloud_root: bool,
}

impl Identity {
    pub fn role(&self) -> Role {
        self.user
            .as_ref()
            .map(|u| u.role)
            .unwrap_or(Role::Anonymous)
    }
    pub fn username(&self) -> Option<&str> {
        self.user.as_ref().map(|u| u.username.as_str())
    }
    pub fn authenticated(&self) -> bool {
        self.user.is_some()
    }
    /// Who per-user paths resolve for (`None`: anonymous).
    pub fn caller(&self) -> Option<Caller<'_>> {
        self.username().map(|username| Caller {
            username,
            owns_cloud_root: self.owns_cloud_root,
        })
    }
}

/// What authentication and the ownership rules need from the database.
pub trait AuthBackend: Send + Sync {
    /// Live user for a bearer token (disabled/deleted/expired → None).
    fn resolve_token(&self, token: &str) -> Option<AuthUser>;
    /// Password check for an active user.
    fn check_password(&self, username: &str, password: &str) -> Option<AuthUser>;
    /// A recently verified `(username, password)` pair, answered without bcrypt or the
    /// limiter (WebDAV clients send Basic credentials on every request).
    fn cached_login(&self, _username: &str, _password: &str) -> Option<AuthUser> {
        None
    }
    /// Uploader ownership of a library file (and its sidecars/layers).
    fn can_user_delete_library_path(&self, username: &str, virtual_path: &str) -> bool;
    /// Uploader owns every tracked volume of the series.
    fn can_user_edit_series(&self, username: &str, series_title: &str) -> bool;
    /// Does the virtual path map to an existing physical file?
    fn physical_exists(&self, virtual_path: &str, caller: Option<Caller<'_>>) -> bool;
    /// Is this library file the primary sidecar of an archive listed beside it
    /// (sidecar names rule 4: `V.edit.mokuro` is volume `V.edit`'s when `V.edit.cbz`
    /// is listed), and so no layer at all?
    fn is_volume_primary(&self, _virtual_path: &str) -> bool {
        false
    }
}

/// A `Basic` header that is present but unusable (bad base64, not UTF-8, no colon).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MalformedBasic;

/// Parse `Basic` credentials: `Ok(None)` for no Basic header, `Err` for a malformed one.
pub fn parse_basic(header: &str) -> Result<Option<(String, String)>, MalformedBasic> {
    let Some(b64) = header.strip_prefix("Basic ") else {
        return Ok(None);
    };
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| MalformedBasic)?;
    let text = String::from_utf8(raw).map_err(|_| MalformedBasic)?;
    let (u, p) = text.split_once(':').ok_or(MalformedBasic)?;
    Ok(Some((u.to_string(), p.to_string())))
}

/// [`authenticate`] when it needs neither the database nor bcrypt: no `Authorization`
/// header, a header that is not Basic or Bearer, a malformed Basic header, or Basic
/// credentials verified recently ([`AuthBackend::cached_login`]). `None` means the full
/// check has to run on a blocking thread (a bearer token, an unverified password).
pub fn authenticate_without_io(headers: &HeaderMap, backend: &dyn AuthBackend) -> Option<Identity> {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return Some(Identity::default());
    };
    if value.starts_with("Bearer ") {
        return None;
    }
    match parse_basic(value) {
        Ok(None) => Some(Identity::default()),
        Err(MalformedBasic) => Some(Identity {
            error: Some("Invalid authorization header".into()),
            ..Default::default()
        }),
        Ok(Some((username, password))) => {
            backend
                .cached_login(&username, &password)
                .map(|user| Identity {
                    user: Some(user),
                    ..Default::default()
                })
        }
    }
}

/// 0.5.2 `AuthMiddleware.authenticate`.
pub fn authenticate(
    headers: &HeaderMap,
    client_ip: &str,
    backend: &dyn AuthBackend,
    limiter: &AuthLimiter,
) -> Identity {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return Identity::default();
    };
    if let Some(token) = value.strip_prefix("Bearer ") {
        return match backend.resolve_token(token.trim()) {
            Some(user) => Identity {
                user: Some(user),
                ..Default::default()
            },
            None => Identity {
                error: Some("Invalid or expired token".into()),
                bearer_failed: true,
                ..Default::default()
            },
        };
    }
    match parse_basic(value) {
        Ok(None) => Identity::default(),
        Err(MalformedBasic) => Identity {
            error: Some("Invalid authorization header".into()),
            ..Default::default()
        },
        Ok(Some((username, password))) => {
            if let Some(user) = backend.cached_login(&username, &password) {
                return Identity {
                    user: Some(user),
                    ..Default::default()
                };
            }
            let key = format!("{client_ip}:{username}");
            if let Err(retry) = limiter.allow_blocking(&key) {
                return Identity {
                    error: Some(format!("Too many failed attempts. Retry in {retry}s")),
                    attempted_username: Some(username),
                    ..Default::default()
                };
            }
            match backend.check_password(&username, &password) {
                Some(user) => {
                    limiter.record_success(&key);
                    Identity {
                        user: Some(user),
                        ..Default::default()
                    }
                }
                None => {
                    limiter.record_failure(&key);
                    Identity {
                        error: Some("Invalid credentials".into()),
                        attempted_username: Some(username),
                        ..Default::default()
                    }
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
        Self {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN),
            message: message.into(),
            bearer: false,
        }
    }

    /// 0.5.2 `_error_response`: text/plain body, WWW-Authenticate on 401.
    pub fn into_response(self) -> axum::response::Response {
        let mut resp = axum::response::Response::new(axum::body::Body::from(self.message));
        *resp.status_mut() = self.status;
        let h = resp.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
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
        Denied::new(
            403,
            "Permission denied: this file is compiled by the server",
        )
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
    if id.username().is_some() {
        Ok(())
    } else {
        Err(Denied::new(403, "Cannot write to other users' progress"))
    }
}

/// What an allowed request was allowed as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    /// The decision table's rules for the path.
    Table,
    /// Only as the caller's own correction layer (D27), decided from a listing of the
    /// folder: DAV resolves the path again where it writes and must find a correction
    /// there too (`bunko_dav::DavContext::personal_only`).
    Personal,
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
    authorize_grant(method, path, destination, id, anon, backend).map(drop)
}

/// [`authorize`], with what the request was allowed as.
pub fn authorize_grant(
    method: &Method,
    path: &str,
    destination: Option<&str>,
    id: &Identity,
    anon: AnonymousAccess,
    backend: &dyn AuthBackend,
) -> Result<Grant, Denied> {
    let mut grant = Grant::Table;
    decide(method, path, destination, id, anon, backend, &mut grant).map(|()| grant)
}

fn decide(
    method: &Method,
    path: &str,
    destination: Option<&str>,
    id: &Identity,
    anon: AnonymousAccess,
    backend: &dyn AuthBackend,
    grant: &mut Grant,
) -> Result<(), Denied> {
    use paths::*;
    if *method == Method::OPTIONS {
        return Ok(());
    }
    if let (Some(err), false) = (&id.error, id.authenticated()) {
        let status = if err.contains("Too many failed attempts") {
            429
        } else {
            401
        };
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
    if matches!(
        m,
        "DELETE" | "MOVE" | "COPY" | "PROPPATCH" | "MKCOL" | "LOCK" | "UNLOCK"
    ) && is_compiled_metadata_path(path)
    {
        return Err(compiled_denied(id));
    }
    if matches!(m, "MOVE" | "COPY")
        && destination
            .and_then(destination_path)
            .is_some_and(|d| is_compiled_metadata_path(&d))
    {
        return Err(compiled_denied(id));
    }
    // The reading history is written file by file and never rearranged, whoever asks.
    if matches!(m, "MOVE" | "COPY" | "LOCK" | "UNLOCK" | "PROPPATCH")
        && (is_history_path(path)
            || destination
                .and_then(destination_path)
                .is_some_and(|d| is_history_path(&d)))
    {
        if !id.authenticated() {
            return Err(Denied::new(401, "Authentication required"));
        }
        return Err(Denied::new(
            403,
            "Permission denied: not supported for reading history",
        ));
    }
    if matches!(
        m,
        "PUT" | "DELETE" | "MOVE" | "COPY" | "LOCK" | "UNLOCK" | "PROPPATCH"
    ) && let Some(decision) = authorize_layer(m, path, destination, id, backend)
    {
        return decision.map(|g| *grant = g);
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
            if refuse {
                Err(Denied::new(401, "Authentication required"))
            } else {
                Ok(())
            }
        }
        "PUT" => authorize_put(path, id, backend),
        "MKCOL" => {
            // The reader creates its own history collections; DAV decides which.
            if is_progress_path(path) {
                return progress_write(id);
            }
            if is_library_path(path) {
                need(
                    id,
                    Perm::AddFiles,
                    "Permission denied: cannot create directories",
                )
            } else {
                Err(Denied::new(
                    403,
                    "Permission denied: unsupported target path",
                ))
            }
        }
        "DELETE" => {
            if is_progress_path(path) {
                return progress_write(id);
            }
            if id.role() == Role::Uploader
                && is_library_path(path)
                && id
                    .username()
                    .is_some_and(|u| backend.can_user_delete_library_path(u, path))
            {
                return Ok(());
            }
            need(
                id,
                Perm::ModifyDelete,
                "Permission denied: cannot modify or delete files",
            )
        }
        "MOVE" | "COPY" => {
            if is_progress_path(path) {
                return progress_write(id);
            }
            need(
                id,
                Perm::ModifyDelete,
                "Permission denied: cannot modify or delete files",
            )
        }
        "LOCK" | "UNLOCK" => {
            if is_progress_path(path) {
                return progress_write(id);
            }
            need(id, Perm::ModifyDelete, "Permission denied")
        }
        "PROPPATCH" => need(id, Perm::ModifyDelete, "Permission denied"),
        _ => Ok(()),
    }
}

/// The class of a library file named as a layer with a reserved id (D27); `None` for
/// any other file, an `Other` layer included, which the table decides as before.
fn reserved_layer_at(path: &str, backend: &dyn AuthBackend) -> Option<LayerClass> {
    let class = paths::layer_class_of_path(path).filter(|c| *c != LayerClass::Other)?;
    (!backend.is_volume_primary(path)).then_some(class)
}

fn denied_unless_signed_in(id: &Identity, message: &str) -> Denied {
    if id.authenticated() {
        Denied::new(403, message)
    } else {
        Denied::new(401, "Authentication required")
    }
}

/// Writes of correction, curated and reserved layer files (D27), or `None` when the
/// rest of the table decides. A personal file is every progress writer's own (DAV
/// resolves it per account), so it moves only between personal names; nothing a
/// client sends lands in a server keepsake or a reserved id no client writes here.
fn authorize_layer(
    m: &str,
    path: &str,
    destination: Option<&str>,
    id: &Identity,
    backend: &dyn AuthBackend,
) -> Option<Result<Grant, Denied>> {
    const BETWEEN: &str = "Permission denied: corrections can only move between correction files";
    const SERVER: &str = "Permission denied: this file is written by the server";
    const RESERVED: &str = "Permission denied: this layer name is reserved";
    let moving = matches!(m, "MOVE" | "COPY");
    let source = reserved_layer_at(path, backend);
    let dest_path = destination
        .filter(|_| moving)
        .and_then(paths::destination_path);
    let dest = dest_path
        .as_deref()
        .and_then(|d| reserved_layer_at(d, backend));
    if source == Some(LayerClass::Personal) {
        return Some(progress_write(id).and_then(|()| {
            if moving && dest != Some(LayerClass::Personal) {
                Err(Denied::new(403, BETWEEN))
            } else if m == "PUT" && sidecar_names::strip_suffix_ci(path, ".gz").is_some() {
                Err(Denied::new(
                    403,
                    "Permission denied: corrections are saved as plain .mokuro",
                ))
            } else {
                Ok(Grant::Personal)
            }
        }));
    }
    // A MOVE that keeps the layer id is a volume rename carrying its layer (the reader
    // moves every listed one): nothing new is written, so the 0.7 rule decides.
    let renamed = m == "MOVE"
        && source.is_some()
        && source == dest
        && dest_path.as_deref().and_then(paths::layer_id_of_path) == paths::layer_id_of_path(path);
    let written = if renamed {
        None
    } else if moving {
        dest
    } else if m == "PUT" {
        source
    } else {
        None
    };
    match (dest, written) {
        (Some(LayerClass::Personal), _) => Some(Err(denied_unless_signed_in(id, BETWEEN))),
        (_, Some(LayerClass::ServerKeepsake)) => Some(Err(denied_unless_signed_in(id, SERVER))),
        (_, Some(LayerClass::Reserved)) => Some(Err(denied_unless_signed_in(id, RESERVED))),
        (_, Some(LayerClass::Curated)) if !moving => {
            Some(authorize_curated(path, id, backend).map(|()| Grant::Table))
        }
        // Deleting or moving a curated or keepsake file keeps the 0.7 rules.
        _ => None,
    }
}

/// Publishing the shared `curated` layer: ModifyDelete roles on any volume, an
/// uploader on the volumes it uploaded, the first create included.
fn authorize_curated(path: &str, id: &Identity, backend: &dyn AuthBackend) -> Result<(), Denied> {
    if !id.authenticated() {
        return Err(Denied::new(401, "Authentication required"));
    }
    if has_perm(id.role(), Perm::ModifyDelete)
        || (id.role() == Role::Uploader
            && id
                .username()
                .is_some_and(|u| backend.can_user_delete_library_path(u, path)))
    {
        return Ok(());
    }
    Err(Denied::new(
        403,
        "Permission denied: cannot publish shared corrections for this volume",
    ))
}

fn authorize_put(path: &str, id: &Identity, backend: &dyn AuthBackend) -> Result<(), Denied> {
    use paths::*;
    if is_progress_path(path) {
        return progress_write(id);
    }
    if let Some(series) = series_title_from_series_file_path(path) {
        if !id.authenticated() {
            return Err(Denied::new(401, "Authentication required"));
        }
        if has_perm(id.role(), Perm::ModifyDelete) {
            return Ok(());
        }
        if id.role() == Role::Uploader
            && id
                .username()
                .is_some_and(|u| backend.can_user_edit_series(u, &series))
        {
            return Ok(());
        }
        return Err(Denied::new(
            403,
            "Permission denied: cannot submit metadata updates for this series",
        ));
    }
    if is_compiled_metadata_path(path) {
        return Err(compiled_denied(id));
    }
    if is_library_path(path) {
        need(id, Perm::AddFiles, "Permission denied: cannot add files")?;
        if !has_perm(id.role(), Perm::ModifyDelete) {
            let user = id.username();
            if backend.physical_exists(path, id.caller())
                && !user.is_some_and(|u| backend.can_user_delete_library_path(u, path))
            {
                return Err(Denied::new(
                    403,
                    "Permission denied: cannot replace a file another account uploaded",
                ));
            }
        }
        return Ok(());
    }
    Err(Denied::new(
        403,
        "Permission denied: unsupported target path",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct B;
    impl AuthBackend for B {
        fn resolve_token(&self, t: &str) -> Option<AuthUser> {
            (t == "good").then(|| AuthUser {
                id: 1,
                username: "u".into(),
                role: Role::Uploader,
            })
        }
        fn check_password(&self, u: &str, p: &str) -> Option<AuthUser> {
            (p == "pw").then(|| AuthUser {
                id: 2,
                username: u.into(),
                role: Role::Editor,
            })
        }
        fn can_user_delete_library_path(&self, u: &str, p: &str) -> bool {
            u == "u" && p.ends_with("mine.cbz")
        }
        fn can_user_edit_series(&self, _: &str, s: &str) -> bool {
            s == "Mine"
        }
        fn physical_exists(&self, p: &str, _: Option<Caller<'_>>) -> bool {
            p.ends_with(".cbz")
        }
    }

    fn id(role: Option<Role>) -> Identity {
        Identity {
            user: role.map(|r| AuthUser {
                id: 1,
                username: "u".into(),
                role: r,
            }),
            ..Default::default()
        }
    }
    const OPEN: AnonymousAccess = AnonymousAccess {
        browse: true,
        download: true,
    };

    /// A backend whose `cached_login` knows `ed:pw`.
    struct Cached;
    impl AuthBackend for Cached {
        fn resolve_token(&self, _: &str) -> Option<AuthUser> {
            unreachable!("a token needs the database")
        }
        fn check_password(&self, _: &str, _: &str) -> Option<AuthUser> {
            unreachable!("a password check needs bcrypt")
        }
        fn cached_login(&self, u: &str, p: &str) -> Option<AuthUser> {
            (u == "ed" && p == "pw").then(|| AuthUser {
                id: 2,
                username: u.into(),
                role: Role::Editor,
            })
        }
        fn can_user_delete_library_path(&self, _: &str, _: &str) -> bool {
            false
        }
        fn can_user_edit_series(&self, _: &str, _: &str) -> bool {
            false
        }
        fn physical_exists(&self, _: &str, _: Option<Caller<'_>>) -> bool {
            false
        }
    }

    #[test]
    fn requests_that_need_no_io_are_decided_without_it() {
        let mut h = HeaderMap::new();
        let anonymous = authenticate_without_io(&h, &Cached).unwrap();
        assert!(!anonymous.authenticated() && anonymous.error.is_none());
        let basic = base64::engine::general_purpose::STANDARD.encode("ed:pw");
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {basic}").parse().unwrap(),
        );
        assert_eq!(
            authenticate_without_io(&h, &Cached).unwrap().role(),
            Role::Editor
        );
        h.insert(header::AUTHORIZATION, "Basic !!!".parse().unwrap());
        assert_eq!(
            authenticate_without_io(&h, &Cached)
                .unwrap()
                .error
                .as_deref(),
            Some("Invalid authorization header")
        );
        h.insert(header::AUTHORIZATION, "Digest x".parse().unwrap());
        assert!(
            !authenticate_without_io(&h, &Cached)
                .unwrap()
                .authenticated()
        );
        // These go to a blocking thread (Cached would panic if they were checked here).
        let other = base64::engine::general_purpose::STANDARD.encode("ed:other");
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {other}").parse().unwrap(),
        );
        assert!(authenticate_without_io(&h, &Cached).is_none());
        h.insert(header::AUTHORIZATION, "Bearer t".parse().unwrap());
        assert!(authenticate_without_io(&h, &Cached).is_none());
    }

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
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {basic}").parse().unwrap(),
        );
        assert_eq!(authenticate(&h, "1.1.1.1", &B, &lim).role(), Role::Editor);
        h.insert(header::AUTHORIZATION, "Basic !!!".parse().unwrap());
        assert_eq!(
            authenticate(&h, "1.1.1.1", &B, &lim).error.as_deref(),
            Some("Invalid authorization header")
        );
        let bad = base64::engine::general_purpose::STANDARD.encode("ed:nope");
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {bad}").parse().unwrap(),
        );
        for _ in 0..10 {
            assert_eq!(
                authenticate(&h, "1.1.1.1", &B, &lim).error.as_deref(),
                Some("Invalid credentials")
            );
        }
        assert!(
            authenticate(&h, "1.1.1.1", &B, &lim)
                .error
                .unwrap()
                .starts_with("Too many failed attempts. Retry in 900s")
        );
    }

    #[test]
    fn table() {
        let get = Method::GET;
        let put = Method::PUT;
        let del = Method::DELETE;
        let anon = id(None);
        assert!(authorize(&get, "/mokuro-reader/a.cbz", None, &anon, OPEN, &B).is_ok());
        let closed = AnonymousAccess {
            browse: false,
            download: false,
        };
        assert_eq!(
            authorize(&get, "/mokuro-reader/a.cbz", None, &anon, closed, &B)
                .unwrap_err()
                .status,
            401
        );
        assert!(authorize(&get, "/_static/x", None, &anon, closed, &B).is_ok());
        assert_eq!(
            authorize(&get, "/mokuro-reader", None, &anon, closed, &B)
                .unwrap_err()
                .status,
            401
        );
        assert_eq!(
            authorize(&put, "/", None, &anon, OPEN, &B)
                .unwrap_err()
                .status,
            403
        );
        assert_eq!(
            authorize(
                &put,
                "/mokuro-reader/a.cbz",
                None,
                &id(Some(Role::Registered)),
                OPEN,
                &B
            )
            .unwrap_err()
            .message,
            "Permission denied: cannot add files"
        );
        assert_eq!(
            authorize(
                &put,
                "/mokuro-reader/other.cbz",
                None,
                &id(Some(Role::Uploader)),
                OPEN,
                &B
            )
            .unwrap_err()
            .message,
            "Permission denied: cannot replace a file another account uploaded"
        );
        assert!(
            authorize(
                &put,
                "/mokuro-reader/mine.cbz",
                None,
                &id(Some(Role::Uploader)),
                OPEN,
                &B
            )
            .is_ok()
        );
        assert!(
            authorize(
                &put,
                "/mokuro-reader/new.mokuro",
                None,
                &id(Some(Role::Uploader)),
                OPEN,
                &B
            )
            .is_ok()
        );
        assert!(
            authorize(
                &del,
                "/mokuro-reader/mine.cbz",
                None,
                &id(Some(Role::Uploader)),
                OPEN,
                &B
            )
            .is_ok()
        );
        assert_eq!(
            authorize(
                &del,
                "/mokuro-reader/x.cbz",
                None,
                &id(Some(Role::Uploader)),
                OPEN,
                &B
            )
            .unwrap_err()
            .status,
            403
        );
        assert_eq!(
            authorize(
                &del,
                "/mokuro-reader/catalog.json",
                None,
                &id(Some(Role::Admin)),
                OPEN,
                &B
            )
            .unwrap_err()
            .status,
            403
        );
        assert!(
            authorize(
                &put,
                "/mokuro-reader/Mine/series.json",
                None,
                &id(Some(Role::Uploader)),
                OPEN,
                &B
            )
            .is_ok()
        );
        assert_eq!(
            authorize(
                &put,
                "/mokuro-reader/Theirs/series.json",
                None,
                &id(Some(Role::Uploader)),
                OPEN,
                &B
            )
            .unwrap_err()
            .status,
            403
        );
        assert_eq!(
            authorize(
                &put,
                "/mokuro-reader/volume-data.json",
                None,
                &anon,
                OPEN,
                &B
            )
            .unwrap_err()
            .message,
            "Authentication required to save progress"
        );
        assert!(
            authorize(
                &put,
                "/mokuro-reader/volume-data.json",
                None,
                &id(Some(Role::Registered)),
                OPEN,
                &B
            )
            .is_ok()
        );
        let mv = Method::from_bytes(b"MOVE").unwrap();
        assert_eq!(
            authorize(
                &mv,
                "/mokuro-reader/a.json",
                Some("http://h/mokuro-reader/catalog.json"),
                &id(Some(Role::Admin)),
                OPEN,
                &B
            )
            .unwrap_err()
            .status,
            403
        );
        assert_eq!(
            authorize(
                &get,
                "/_processor/x",
                None,
                &id(Some(Role::Admin)),
                OPEN,
                &B
            )
            .unwrap_err()
            .message,
            "Processor access required"
        );
        assert!(authorize(&get, "/_admin/", None, &anon, OPEN, &B).is_ok());
        assert_eq!(
            authorize(&get, "/_admin/api/users", None, &anon, OPEN, &B)
                .unwrap_err()
                .status,
            401
        );
        assert!(
            authorize(
                &get,
                "/_admin/api/invites",
                None,
                &id(Some(Role::Inviter)),
                OPEN,
                &B
            )
            .is_ok()
        );
        let failed = Identity {
            error: Some("Invalid credentials".into()),
            ..Default::default()
        };
        assert_eq!(
            authorize(&get, "/mokuro-reader/a.cbz", None, &failed, OPEN, &B)
                .unwrap_err()
                .status,
            401
        );
        assert!(authorize(&Method::OPTIONS, "/x", None, &failed, OPEN, &B).is_ok());
    }

    /// The reading-history tree (0.8) is the account's own: any account that may save
    /// progress writes it, and the methods that would rearrange it are refused.
    #[test]
    fn history_needs_write_progress() {
        let m = |name: &str| Method::from_bytes(name.as_bytes()).unwrap();
        let file = "/mokuro-reader/history/d/2026-10.events";
        let dir = "/mokuro-reader/history";
        for (method, path) in [
            ("PUT", file),
            ("MKCOL", dir),
            ("MKCOL", "/mokuro-reader/history/d"),
            ("DELETE", file),
            ("DELETE", dir),
        ] {
            assert!(
                authorize(
                    &m(method),
                    path,
                    None,
                    &id(Some(Role::Registered)),
                    OPEN,
                    &B
                )
                .is_ok(),
                "{method} {path}"
            );
            let refused = authorize(&m(method), path, None, &id(Some(Role::Processor)), OPEN, &B);
            assert_eq!(
                refused.unwrap_err().status,
                403,
                "processor {method} {path}"
            );
            let refused = authorize(&m(method), path, None, &id(None), OPEN, &B);
            assert_eq!(
                refused.unwrap_err().status,
                401,
                "anonymous {method} {path}"
            );
        }
        for method in ["MOVE", "COPY", "LOCK", "UNLOCK", "PROPPATCH"] {
            for path in [file, dir] {
                let d = authorize(
                    &m(method),
                    path,
                    Some("http://h/mokuro-reader/history/d/x"),
                    &id(Some(Role::Uploader)),
                    OPEN,
                    &B,
                )
                .unwrap_err();
                assert_eq!(
                    (d.status.as_u16(), d.message.as_str()),
                    (403, "Permission denied: not supported for reading history"),
                    "{method} {path}"
                );
            }
        }
        let anon = authorize(&m("MOVE"), file, None, &id(None), OPEN, &B).unwrap_err();
        assert_eq!(anon.status, 401);
        // Into the history from a top-level per-user file: refused the same way.
        let d = authorize(
            &m("COPY"),
            "/mokuro-reader/goals.json",
            Some("http://h/mokuro-reader/history/d/x"),
            &id(Some(Role::Registered)),
            OPEN,
            &B,
        )
        .unwrap_err();
        assert_eq!(d.status, 403);
        // A top-level per-user file keeps its 0.5.2 rules.
        assert!(
            authorize(
                &m("LOCK"),
                "/mokuro-reader/goals.json",
                None,
                &id(Some(Role::Registered)),
                OPEN,
                &B
            )
            .is_ok()
        );
        // history/series.json is the account's, not compiled metadata; a library
        // `History/` series is still the library's.
        assert!(
            authorize(
                &m("PUT"),
                "/mokuro-reader/history/series.json",
                None,
                &id(Some(Role::Registered)),
                OPEN,
                &B
            )
            .is_ok()
        );
        assert_eq!(
            authorize(
                &m("PUT"),
                "/mokuro-reader/History/v.cbz",
                None,
                &id(Some(Role::Registered)),
                OPEN,
                &B
            )
            .unwrap_err()
            .message,
            "Permission denied: cannot add files"
        );
    }

    /// Owns `Mine/`'s volumes; `S/V.edit.cbz` is a listed archive.
    struct Layers;
    impl AuthBackend for Layers {
        fn resolve_token(&self, _: &str) -> Option<AuthUser> {
            None
        }
        fn check_password(&self, _: &str, _: &str) -> Option<AuthUser> {
            None
        }
        fn can_user_delete_library_path(&self, u: &str, p: &str) -> bool {
            u == "u" && p.starts_with("/mokuro-reader/Mine/")
        }
        fn can_user_edit_series(&self, _: &str, _: &str) -> bool {
            false
        }
        fn physical_exists(&self, p: &str, _: Option<Caller<'_>>) -> bool {
            p.ends_with(".curated.mokuro") && p.contains("/Theirs/")
        }
        fn is_volume_primary(&self, p: &str) -> bool {
            p == "/mokuro-reader/S/V.edit.mokuro"
        }
    }

    /// What a request was allowed as: a correction write only as the caller's own file
    /// (DAV checks it again where it writes), anything else by the table.
    #[test]
    fn a_correction_write_is_granted_as_the_callers_own() {
        let m = |name: &str| Method::from_bytes(name.as_bytes()).unwrap();
        let grant = |method: &str, path: &str, dest: Option<&str>, role: Role| {
            authorize_grant(&m(method), path, dest, &id(Some(role)), OPEN, &Layers)
        };
        let edit = "/mokuro-reader/S/W.edit.mokuro";
        for method in ["PUT", "DELETE", "LOCK", "UNLOCK", "PROPPATCH"] {
            for role in [Role::Registered, Role::Admin] {
                assert_eq!(grant(method, edit, None, role), Ok(Grant::Personal));
            }
        }
        let dest = Some("http://h/mokuro-reader/T/X.edit.mokuro");
        assert_eq!(
            grant("MOVE", edit, dest, Role::Registered),
            Ok(Grant::Personal)
        );
        // A dotted volume's primary, an archive, a progress file: the table's.
        for (method, path, role) in [
            ("PUT", "/mokuro-reader/S/V.edit.mokuro", Role::Editor),
            ("PUT", "/mokuro-reader/S/V.cbz", Role::Uploader),
            ("PUT", "/mokuro-reader/volume-data.json", Role::Registered),
            ("GET", edit, Role::Registered),
        ] {
            assert_eq!(
                grant(method, path, None, role),
                Ok(Grant::Table),
                "{method} {path}"
            );
        }
        assert!(grant("PUT", "/mokuro-reader/S/V.cbz", None, Role::Registered).is_err());
    }

    /// D27: personal corrections are any progress writer's own; the curated layer is
    /// published by ModifyDelete roles and an uploader on its own volumes; the server's
    /// keepsakes and the other reserved ids take no client write.
    #[test]
    fn correction_layers() {
        let m = |name: &str| Method::from_bytes(name.as_bytes()).unwrap();
        let ask = |method: &str, path: &str, dest: Option<&str>, role: Option<Role>| {
            authorize(&m(method), path, dest, &id(role), OPEN, &Layers)
                .map_err(|d| (d.status.as_u16(), d.message))
        };
        let reg = Some(Role::Registered);
        let up = Some(Role::Uploader);
        let edit = "/mokuro-reader/S/W.edit.mokuro";

        for method in ["PUT", "DELETE", "LOCK", "UNLOCK", "PROPPATCH"] {
            for path in [edit, "/mokuro-reader/S/W.edit-2.mokuro"] {
                assert_eq!(ask(method, path, None, reg), Ok(()), "{method} {path}");
                assert_eq!(ask(method, path, None, None).unwrap_err().0, 401);
                assert_eq!(
                    ask(method, path, None, Some(Role::Processor))
                        .unwrap_err()
                        .0,
                    403
                );
            }
        }
        // Reads are the resolver's business.
        assert_eq!(ask("GET", edit, None, None), Ok(()));
        // Plain `.mokuro` only.
        assert_eq!(
            ask("PUT", "/mokuro-reader/S/W.edit.mokuro.gz", None, reg),
            Err((
                403,
                "Permission denied: corrections are saved as plain .mokuro".into()
            ))
        );
        // Exact-case ids (rule v1): `EDIT` is a shared library file, as before.
        assert_eq!(
            ask("PUT", "/mokuro-reader/S/W.EDIT.mokuro", None, reg),
            Err((403, "Permission denied: cannot add files".into()))
        );
        // `V.edit.mokuro` beside `V.edit.cbz` is that volume's primary (rule 4).
        assert_eq!(
            ask("PUT", "/mokuro-reader/S/V.edit.mokuro", None, reg),
            Err((403, "Permission denied: cannot add files".into()))
        );

        let between = "Permission denied: corrections can only move between correction files";
        for method in ["MOVE", "COPY"] {
            assert_eq!(
                ask(
                    method,
                    edit,
                    Some("http://h/mokuro-reader/T/X.edit.mokuro"),
                    reg
                ),
                Ok(())
            );
            assert_eq!(
                ask(
                    method,
                    edit,
                    Some("http://h/mokuro-reader/T/X.foo.mokuro"),
                    reg
                ),
                Err((403, between.into()))
            );
            assert_eq!(
                ask(
                    method,
                    "/mokuro-reader/T/X.foo.mokuro",
                    Some("http://h/mokuro-reader/S/W.edit.mokuro"),
                    Some(Role::Admin)
                ),
                Err((403, between.into()))
            );
            assert_eq!(
                ask(
                    method,
                    edit,
                    Some("http://h/mokuro-reader/T/X.edit.mokuro"),
                    None
                )
                .unwrap_err()
                .0,
                401
            );
        }

        let curated = "Permission denied: cannot publish shared corrections for this volume";
        assert_eq!(
            ask("PUT", "/mokuro-reader/Mine/V.curated.mokuro", None, up),
            Ok(())
        );
        // Another account's volume, whether its curated file exists yet or not.
        for path in [
            "/mokuro-reader/Theirs/V.curated.mokuro",
            "/mokuro-reader/Other/V.curated.mokuro",
        ] {
            assert_eq!(
                ask("PUT", path, None, up),
                Err((403, curated.into())),
                "{path}"
            );
            assert_eq!(ask("PUT", path, None, Some(Role::Editor)), Ok(()));
            assert_eq!(ask("PUT", path, None, reg), Err((403, curated.into())));
            assert_eq!(ask("PUT", path, None, None).unwrap_err().0, 401);
        }

        let server = "Permission denied: this file is written by the server";
        for path in [
            "/mokuro-reader/Mine/V.pre-upgrade.mokuro",
            "/mokuro-reader/Mine/V.pre-upgrade-2.mokuro",
        ] {
            assert_eq!(
                ask("PUT", path, None, Some(Role::Admin)),
                Err((403, server.into()))
            );
            assert_eq!(
                ask(
                    "MOVE",
                    "/mokuro-reader/Mine/V.hayai.mokuro",
                    Some(&format!("http://h{path}")),
                    Some(Role::Admin)
                ),
                Err((403, server.into()))
            );
            // Deleting one keeps its 0.7 rule.
            assert_eq!(ask("DELETE", path, None, up), Ok(()));
        }
        let reserved = "Permission denied: this layer name is reserved";
        for path in [
            "/mokuro-reader/Mine/V.curated-2.mokuro",
            "/mokuro-reader/Mine/V.previous-ocr.mokuro",
            "/mokuro-reader/Mine/V.pre-upgraded.mokuro",
        ] {
            assert_eq!(
                ask("PUT", path, None, Some(Role::Admin)),
                Err((403, reserved.into())),
                "{path}"
            );
            assert_eq!(ask("PUT", path, None, None).unwrap_err().0, 401);
        }
        // A volume rename carries its keepsakes and reserved layers under the same id:
        // the 0.7 MOVE rule decides. Anything else into those names stays refused.
        let edited = Some(Role::Editor);
        for id in ["pre-upgrade", "pre-upgrade-2", "curated-2", "previous-ocr"] {
            let from = format!("/mokuro-reader/Mine/V.{id}.mokuro");
            let to = format!("http://h/mokuro-reader/Mine/W.{id}.mokuro");
            assert_eq!(ask("MOVE", &from, Some(&to), edited), Ok(()), "{id}");
            assert_eq!(
                ask("MOVE", &from, Some(&to), reg),
                Err((
                    403,
                    "Permission denied: cannot modify or delete files".into()
                )),
                "{id}"
            );
            assert!(ask("COPY", &from, Some(&to), edited).is_err(), "{id}");
        }
        assert_eq!(
            ask(
                "MOVE",
                "/mokuro-reader/Mine/V.pre-upgrade.mokuro",
                Some("http://h/mokuro-reader/Mine/W.pre-upgrade-2.mokuro"),
                edited
            ),
            Err((403, server.into()))
        );
        assert_eq!(
            ask(
                "MOVE",
                "/mokuro-reader/Mine/V.curated-2.mokuro",
                Some("http://h/mokuro-reader/Mine/W.curated-3.mokuro"),
                edited
            ),
            Err((403, reserved.into()))
        );
        // The reader's own reserved ids that Bunko stores as before.
        for path in [
            "/mokuro-reader/Mine/V.original.mokuro",
            "/mokuro-reader/Mine/V.updated-ocr.mokuro",
            "/mokuro-reader/Mine/V.my-fix.mokuro",
        ] {
            assert_eq!(ask("PUT", path, None, up), Ok(()), "{path}");
        }
    }
}
