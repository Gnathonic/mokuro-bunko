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
    /// Does the virtual path map to an existing physical file?
    fn physical_exists(&self, virtual_path: &str, caller: Option<Caller<'_>>) -> bool;
    /// Is this library file the primary sidecar of an archive listed beside it
    /// (sidecar names rule 4: `V.edit.mokuro` is volume `V.edit`'s when `V.edit.cbz`
    /// is listed), and so no layer at all?
    fn is_volume_primary(&self, _virtual_path: &str) -> bool {
        false
    }
    /// The recorded owner of the series folder `series` (series owners, 0.8), while a
    /// folder of that series is listed: a record whose folder is gone owns nothing.
    fn series_owner(&self, _series: &str) -> Option<String> {
        None
    }
    /// The listed archives (library-relative, `S/V.cbz`) that govern a library file
    /// (`bunko_core::sidecar_names::governing_archives`, from its folder's listing).
    fn governing_archives(&self, _virtual_path: &str) -> Vec<String> {
        Vec::new()
    }
    /// Who uploaded the library archive `rel` (library-relative).
    fn volume_uploader(&self, _rel: &str) -> Option<String> {
        None
    }
    /// Is the library path an existing folder?
    fn is_library_folder(&self, _virtual_path: &str) -> bool {
        false
    }
    /// The folders that existed before series owners are still waiting for theirs
    /// (startup adoption has not run): an addition now could make a folder mixed that
    /// is about to be someone's.
    fn ownership_pending(&self) -> bool {
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
    /// Why, for a PUT's verdict `detail` (`bunko_dav::RefusalDetail`); `None` keeps the
    /// generic one.
    pub detail: Option<&'static str>,
    /// `Retry-After` seconds.
    pub retry_after: Option<u64>,
}

impl Denied {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN),
            message: message.into(),
            bearer: false,
            detail: None,
            retry_after: None,
        }
    }

    fn with_detail(mut self, detail: &'static str) -> Self {
        self.detail = Some(detail);
        self
    }

    /// 0.5.2 `_error_response`: text/plain body, WWW-Authenticate on 401.
    pub fn into_response(self) -> axum::response::Response {
        let mut resp = axum::response::Response::new(axum::body::Body::from(self.message));
        *resp.status_mut() = self.status;
        if let Some(detail) = self.detail {
            resp.extensions_mut()
                .insert(bunko_dav::RefusalDetail(detail));
        }
        let h = resp.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        if let Some(secs) = self.retry_after {
            h.insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
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
    /// By a series owner rule (0.8), decided from a folder listing and the database:
    /// DAV asks again just before it writes (`bunko_dav::DavContext::ownership_recheck`),
    /// so an archive that landed in between and governs the path refuses it.
    Owner,
}

/// A series another account owns (S3): nothing new goes into it.
const SERIES_OWNED: &str = "Permission denied: this series belongs to another account";
const SERIES_OWNED_DETAIL: &str = "This series belongs to another account.";
/// A file named after a volume this account does not control (S2). Neither text may
/// read as a sign-in failure to a reader (no "401", no "Unauthorized").
const VOLUME_OWNED: &str =
    "Permission denied: this file belongs to a volume this account did not upload";
const VOLUME_OWNED_DETAIL: &str = "This file belongs to a volume this account did not upload.";

/// Two names of one account: account names are unique ignoring ASCII case.
fn same_account(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The owner of the series folder a library path is inside, or (`itself`) is.
fn folder_owner_of(path: &str, itself: bool, backend: &dyn AuthBackend) -> Option<String> {
    let folder = paths::series_folder_of(path)
        .or_else(|| itself.then(|| paths::top_level_folder(path)).flatten())?;
    backend.series_owner(&folder)
}

fn owned_by_another(path: &str, itself: bool, user: &str, backend: &dyn AuthBackend) -> bool {
    folder_owner_of(path, itself, backend).is_some_and(|owner| !same_account(&owner, user))
}

/// Does `user` control the library FILE at `path` (0.8)? The owner of the series folder
/// it is in does (S3), and so does the uploader of the archive that governs it, of
/// every one when case variants share that (S2). What no listed archive governs is the
/// folder owner's only. Folders are never governed (C3): only their owner controls one.
fn controls(path: &str, user: &str, governors: &[String], backend: &dyn AuthBackend) -> bool {
    folder_owner_of(path, false, backend).is_some_and(|owner| same_account(&owner, user))
        || (!governors.is_empty()
            && governors.iter().all(|archive| {
                backend
                    .volume_uploader(archive)
                    .is_some_and(|u| same_account(&u, user))
            }))
}

/// An addition while the existing folders wait for their owners (C5).
fn adoption_pending() -> Denied {
    Denied {
        retry_after: Some(10),
        ..Denied::new(
            503,
            "The server is still giving its existing series their owners; try again shortly.",
        )
    }
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
        "PUT" => authorize_put(path, id, backend, grant),
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
                )?;
                if has_perm(id.role(), Perm::ModifyDelete) {
                    return Ok(());
                }
                let user = id.username().unwrap_or_default();
                let path = &normalized_library_path(path).unwrap_or_else(|| path.to_string());
                if backend.ownership_pending() {
                    return Err(adoption_pending());
                }
                // Inside another account's series, or a spelling of one (S3).
                if owned_by_another(path, true, user, backend) {
                    return Err(Denied::new(403, SERIES_OWNED).with_detail(SERIES_OWNED_DETAIL));
                }
                // A folder named after a volume is no archive (rule 2), but only the
                // series owner makes one (C2, C3).
                if !backend.governing_archives(path).is_empty()
                    && !folder_owner_of(path, false, backend)
                        .is_some_and(|o| same_account(&o, user))
                {
                    return Err(Denied::new(403, VOLUME_OWNED).with_detail(VOLUME_OWNED_DETAIL));
                }
                Ok(())
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
            // An uploader deletes what it controls (0.8): a file of a volume it uploaded,
            // anything in a series it owns, the series folder itself.
            if id.role() == Role::Uploader
                && is_library_path(path)
                && let Some(user) = id.username()
            {
                let path = &normalized_library_path(path).unwrap_or_else(|| path.to_string());
                let allowed = if backend.is_library_folder(path) {
                    folder_owner_of(path, true, backend).is_some_and(|o| same_account(&o, user))
                } else {
                    controls(path, user, &backend.governing_archives(path), backend)
                };
                if allowed {
                    *grant = Grant::Owner;
                    return Ok(());
                }
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
        (_, Some(LayerClass::Curated)) if !moving => Some(authorize_curated(path, id, backend)),
        // Deleting or moving a curated or keepsake file keeps the 0.7 rules.
        _ => None,
    }
}

/// Publishing the shared `curated` layer: ModifyDelete roles on any volume, an
/// uploader on the volumes it controls (its uploads, every volume of a series it
/// owns), the first create included, but never a new one in another account's series.
fn authorize_curated(
    path: &str,
    id: &Identity,
    backend: &dyn AuthBackend,
) -> Result<Grant, Denied> {
    if !id.authenticated() {
        return Err(Denied::new(401, "Authentication required"));
    }
    if has_perm(id.role(), Perm::ModifyDelete) {
        return Ok(Grant::Table);
    }
    if id.role() == Role::Uploader
        && let Some(user) = id.username()
    {
        let path = &paths::normalized_library_path(path).unwrap_or_else(|| path.to_string());
        if !backend.physical_exists(path, id.caller())
            && owned_by_another(path, false, user, backend)
        {
            return Err(Denied::new(403, SERIES_OWNED).with_detail(SERIES_OWNED_DETAIL));
        }
        if controls(path, user, &backend.governing_archives(path), backend) {
            return Ok(Grant::Owner);
        }
    }
    Err(Denied::new(
        403,
        "Permission denied: cannot publish shared corrections for this volume",
    ))
}

fn authorize_put(
    path: &str,
    id: &Identity,
    backend: &dyn AuthBackend,
    grant: &mut Grant,
) -> Result<(), Denied> {
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
        // The series' recorded owner (0.8); an unowned series takes no request below
        // ModifyDelete.
        if id.role() == Role::Uploader
            && let Some(user) = id.username()
            && backend
                .series_owner(&series)
                .is_some_and(|owner| same_account(&owner, user))
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
        if has_perm(id.role(), Perm::ModifyDelete) {
            return Ok(());
        }
        // Below ModifyDelete (0.8): nothing new in another account's series (S3);
        // creating, replacing or deleting a file named after a volume needs control of
        // that volume (S2); a new volume in a shared series is anyone's to add.
        let user = id.username().unwrap_or_default();
        let path = &normalized_library_path(path).unwrap_or_else(|| path.to_string());
        let governors = backend.governing_archives(path);
        if backend.physical_exists(path, id.caller()) {
            if !controls(path, user, &governors, backend) {
                return Err(Denied::new(
                    403,
                    "Permission denied: cannot replace a file another account uploaded",
                ));
            }
        } else {
            if backend.ownership_pending() {
                return Err(adoption_pending());
            }
            if owned_by_another(path, false, user, backend) {
                return Err(Denied::new(403, SERIES_OWNED).with_detail(SERIES_OWNED_DETAIL));
            }
            if !governors.is_empty() && !controls(path, user, &governors, backend) {
                return Err(Denied::new(403, VOLUME_OWNED).with_detail(VOLUME_OWNED_DETAIL));
            }
        }
        *grant = Grant::Owner;
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
        fn physical_exists(&self, p: &str, _: Option<Caller<'_>>) -> bool {
            p.ends_with(".cbz")
        }
        fn series_owner(&self, s: &str) -> Option<String> {
            (s == "Mine").then(|| "u".into())
        }
        /// Every archive is its own volume.
        fn governing_archives(&self, p: &str) -> Vec<String> {
            paths::library_rel(p)
                .filter(|r| r.ends_with(".cbz"))
                .into_iter()
                .collect()
        }
        fn volume_uploader(&self, rel: &str) -> Option<String> {
            rel.ends_with("mine.cbz").then(|| "u".into())
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

    /// `u` owns the series `Mine/`; `S/V.edit.cbz` is a listed archive.
    struct Layers;
    impl AuthBackend for Layers {
        fn resolve_token(&self, _: &str) -> Option<AuthUser> {
            None
        }
        fn check_password(&self, _: &str, _: &str) -> Option<AuthUser> {
            None
        }
        fn series_owner(&self, s: &str) -> Option<String> {
            (s == "Mine").then(|| "u".into())
        }
        fn physical_exists(&self, p: &str, _: Option<Caller<'_>>) -> bool {
            p.ends_with(".curated.mokuro") && p.contains("/Theirs/")
        }
        fn is_volume_primary(&self, p: &str) -> bool {
            p == "/mokuro-reader/S/V.edit.mokuro"
        }
    }

    /// A small library for the series owner rules (0.8): files and folders as library
    /// rels, archive uploaders, folder owner records.
    #[derive(Default)]
    struct Owners {
        files: Vec<String>,
        folders: Vec<String>,
        uploads: Vec<(String, String)>,
        records: Vec<(String, String)>,
        pending: bool,
    }

    impl Owners {
        fn folder(mut self, rel: &str) -> Self {
            let mut at = String::new();
            for seg in rel.split('/') {
                at = if at.is_empty() {
                    seg.to_string()
                } else {
                    format!("{at}/{seg}")
                };
                if !self.folders.contains(&at) {
                    self.folders.push(at.clone());
                }
            }
            self
        }
        fn file(mut self, rel: &str) -> Self {
            if let Some((dir, _)) = rel.rsplit_once('/') {
                self = self.folder(dir);
            }
            self.files.push(rel.to_string());
            self
        }
        fn upload(self, rel: &str, user: &str) -> Self {
            let mut me = self.file(rel);
            me.uploads.push((rel.to_string(), user.to_string()));
            me
        }
        fn owned(mut self, folder: &str, user: &str) -> Self {
            self.records.push((folder.to_string(), user.to_string()));
            self
        }
        fn names_in(&self, dir: &str) -> Vec<&str> {
            self.files
                .iter()
                .filter_map(|f| match f.rsplit_once('/') {
                    Some((d, n)) if d == dir => Some(n),
                    None if dir.is_empty() => Some(f.as_str()),
                    _ => None,
                })
                .collect()
        }
        fn split(p: &str) -> Option<(String, String)> {
            let rel = paths::library_rel(p)?;
            Some(match rel.rsplit_once('/') {
                Some((d, n)) => (d.to_string(), n.to_string()),
                None => (String::new(), rel),
            })
        }
    }

    impl AuthBackend for Owners {
        fn resolve_token(&self, _: &str) -> Option<AuthUser> {
            None
        }
        fn check_password(&self, _: &str, _: &str) -> Option<AuthUser> {
            None
        }
        fn physical_exists(&self, p: &str, _: Option<Caller<'_>>) -> bool {
            paths::library_rel(p).is_some_and(|r| self.files.contains(&r))
        }
        fn is_volume_primary(&self, p: &str) -> bool {
            let Some((dir, name)) = Self::split(p) else {
                return false;
            };
            let names = self.names_in(&dir);
            matches!(
                sidecar_names::classify(&name, &sidecar_names::volume_stems(names)),
                sidecar_names::SidecarClass::Primary { .. }
            )
        }
        fn series_owner(&self, series: &str) -> Option<String> {
            let key = bunko_db::fold_series_title_key(series);
            let live = self
                .folders
                .iter()
                .any(|f| !f.contains('/') && bunko_db::fold_series_title_key(f) == key);
            self.records
                .iter()
                .find(|(f, _)| bunko_db::fold_series_title_key(f) == key)
                .filter(|_| live)
                .map(|(_, u)| u.clone())
        }
        fn governing_archives(&self, p: &str) -> Vec<String> {
            let Some((dir, name)) = Self::split(p) else {
                return Vec::new();
            };
            sidecar_names::governing_archives(&name, &self.names_in(&dir))
                .into_iter()
                .map(|a| {
                    if dir.is_empty() {
                        a.to_string()
                    } else {
                        format!("{dir}/{a}")
                    }
                })
                .collect()
        }
        fn volume_uploader(&self, rel: &str) -> Option<String> {
            self.uploads
                .iter()
                .find(|(r, _)| r == rel)
                .map(|(_, u)| u.clone())
        }
        fn is_library_folder(&self, p: &str) -> bool {
            paths::library_rel(p).is_some_and(|r| self.folders.contains(&r))
        }
        fn ownership_pending(&self) -> bool {
            self.pending
        }
    }

    fn as_user(name: &str, role: Role) -> Identity {
        Identity {
            user: Some(AuthUser {
                id: 7,
                username: name.into(),
                role,
            }),
            ..Default::default()
        }
    }

    /// `(status, detail)` of a refusal, or the grant.
    fn decide_as(
        lib: &Owners,
        method: &str,
        path: &str,
        who: &Identity,
    ) -> Result<Grant, (u16, Option<&'static str>)> {
        let m = Method::from_bytes(method.as_bytes()).unwrap();
        authorize_grant(&m, path, None, who, OPEN, lib).map_err(|d| (d.status.as_u16(), d.detail))
    }

    const SERIES: Option<&str> = Some(SERIES_OWNED_DETAIL);
    const VOLUME: Option<&str> = Some(VOLUME_OWNED_DETAIL);

    /// The finding (authz-dotted-primary) in both upload orders: an archive nested
    /// under another account's volume is that volume's, so the account that is not its
    /// uploader can neither create it nor, once the volume arrives, keep writing it.
    #[test]
    fn a_nested_archive_beside_another_accounts_volume_is_refused() {
        let alice = as_user("alice", Role::Uploader);
        let mallory = as_user("mallory", Role::Uploader);
        let lib = Owners::default().upload("S/V.cbz", "alice");
        for name in [
            "V.curated.cbz",
            "V.edit.cbz",
            "V.EDIT.cbz",
            "V.Curated.cbz",
            "V.PRE-UPGRADE.cbz",
            "v.curated.cbz",
            "V.hayai.mokuro",
            "V.mokuro",
            "V.webp",
            "V.nocover",
        ] {
            let path = format!("/mokuro-reader/S/{name}");
            assert_eq!(
                decide_as(&lib, "PUT", &path, &mallory),
                Err((403, VOLUME)),
                "{name}"
            );
            assert_eq!(
                decide_as(&lib, "PUT", &path, &alice),
                Ok(Grant::Owner),
                "{name}"
            );
        }
        // V's curated layer: its uploader's to publish.
        let curated = "/mokuro-reader/S/V.curated.mokuro";
        assert_eq!(decide_as(&lib, "PUT", curated, &mallory), Err((403, None)));
        assert_eq!(decide_as(&lib, "PUT", curated, &alice), Ok(Grant::Owner));
        // The other order: mallory's V.curated.cbz was there first, V.cbz arrived.
        let lib = Owners::default()
            .upload("S/V.curated.cbz", "mallory")
            .file("S/V.curated.mokuro")
            .upload("S/V.cbz", "alice");
        for name in ["V.curated.cbz", "V.curated.mokuro"] {
            let path = format!("/mokuro-reader/S/{name}");
            for method in ["PUT", "DELETE"] {
                assert!(
                    matches!(decide_as(&lib, method, &path, &mallory), Err((403, _))),
                    "{method} {name}"
                );
                assert_eq!(
                    decide_as(&lib, method, &path, &alice),
                    Ok(Grant::Owner),
                    "{method} {name}"
                );
            }
        }
        // Alone, a nested archive is its own volume: its uploader's.
        let lib = Owners::default().upload("S/V.curated.cbz", "mallory");
        assert_eq!(
            decide_as(&lib, "PUT", "/mokuro-reader/S/V.curated.mokuro", &mallory),
            Ok(Grant::Owner)
        );
    }

    /// S2: a file named after a volume needs control of that volume; rule v1's own
    /// reading of a `.mokuro` name decides (C7: `Vol 01.5.mokuro` is a layer of Vol 01
    /// when no `Vol 01.5.cbz` is listed).
    #[test]
    fn a_volumes_files_are_its_controllers() {
        let alice = as_user("alice", Role::Uploader);
        let bob = as_user("bob", Role::Uploader);
        let lib = Owners::default()
            .upload("S/Vol 01.cbz", "alice")
            .upload("S/Vol 02.cbz", "bob")
            .file("S/Untracked.cbz")
            .file("S/notes.txt");
        for name in [
            "Vol 01.mokuro",
            "Vol 01.mokuro.gz",
            "Vol 01.webp",
            "Vol 01.my-fix.mokuro",
            "Vol 01.5.mokuro",
            "Vol 01.2024.mokuro",
        ] {
            let path = format!("/mokuro-reader/S/{name}");
            assert_eq!(
                decide_as(&lib, "PUT", &path, &bob),
                Err((403, VOLUME)),
                "{name}"
            );
            assert_eq!(
                decide_as(&lib, "PUT", &path, &alice),
                Ok(Grant::Owner),
                "{name}"
            );
        }
        // An untracked volume is only its series owner's (nobody's here).
        for who in [&alice, &bob] {
            assert_eq!(
                decide_as(&lib, "PUT", "/mokuro-reader/S/Untracked.mokuro", who),
                Err((403, VOLUME))
            );
            assert!(decide_as(&lib, "PUT", "/mokuro-reader/S/Untracked.cbz", who).is_err());
            assert!(decide_as(&lib, "DELETE", "/mokuro-reader/S/notes.txt", who).is_err());
        }
        // A new volume in a shared series is anyone's to add, a decimal one included.
        for name in [
            "Vol 03.cbz",
            "Vol 01.5.cbz",
            "Vol 03.mokuro",
            "Vol 01.5.webp",
        ] {
            let path = format!("/mokuro-reader/S/{name}");
            assert_eq!(
                decide_as(&lib, "PUT", &path, &bob),
                Ok(Grant::Owner),
                "{name}"
            );
        }
        // Replacing or deleting: the archive's uploader only.
        assert_eq!(
            decide_as(&lib, "PUT", "/mokuro-reader/S/Vol 01.cbz", &bob)
                .unwrap_err()
                .0,
            403
        );
        assert_eq!(
            decide_as(&lib, "DELETE", "/mokuro-reader/S/Vol 01.cbz", &bob)
                .unwrap_err()
                .0,
            403
        );
        assert_eq!(
            decide_as(&lib, "DELETE", "/mokuro-reader/S/Vol 01.cbz", &alice),
            Ok(Grant::Owner)
        );
        // An editor's is the table's, as before.
        let editor = as_user("ed", Role::Editor);
        assert_eq!(
            decide_as(&lib, "PUT", "/mokuro-reader/S/Vol 01.mokuro", &editor),
            Ok(Grant::Table)
        );
        // A registered account still adds nothing.
        assert!(
            decide_as(
                &lib,
                "PUT",
                "/mokuro-reader/S/Vol 01.mokuro",
                &as_user("r", Role::Registered)
            )
            .is_err()
        );
    }

    /// S3: the owner of a series folder controls everything in it; nobody else below
    /// ModifyDelete adds to it. Personal corrections stay every account's own.
    #[test]
    fn a_series_owner_controls_its_folder() {
        let alice = as_user("alice", Role::Uploader);
        let bob = as_user("bob", Role::Uploader);
        let lib = Owners::default()
            .upload("Alice/Vol 1.cbz", "alice")
            .upload("Alice/Vol 2.cbz", "bob")
            .file("Alice/Vol 3.cbz")
            .file("Alice/notes.txt")
            .folder("Alice/Extras")
            .owned("Alice", "ALICE");
        // Bob adds nothing there, not even under his own volume, by any spelling.
        for path in [
            "/mokuro-reader/Alice/Vol 9.cbz",
            "/mokuro-reader/Alice/Vol 2.mokuro",
            "/mokuro-reader//Alice/Vol 9.cbz",
            "/mokuro-reader/./Alice/Vol 9.cbz",
            "/mokuro-reader/alice/Vol 9.cbz",
            "/mokuro-reader/Alice/Extras/Vol 9.cbz",
        ] {
            assert_eq!(
                decide_as(&lib, "PUT", path, &bob),
                Err((403, SERIES)),
                "{path}"
            );
        }
        for path in [
            "/mokuro-reader/Alice/New",
            "/mokuro-reader//Alice/New",
            "/mokuro-reader/./Alice/New",
            "/mokuro-reader/ALICE",
        ] {
            assert_eq!(
                decide_as(&lib, "MKCOL", path, &bob),
                Err((403, SERIES)),
                "{path}"
            );
        }
        // His own volume's existing files are still his to replace and delete.
        assert_eq!(
            decide_as(&lib, "PUT", "/mokuro-reader/Alice/Vol 2.cbz", &bob),
            Ok(Grant::Owner)
        );
        // The owner (any case of her name) writes, replaces and deletes anything there.
        for (method, path) in [
            ("PUT", "/mokuro-reader/Alice/Vol 9.cbz"),
            ("PUT", "/mokuro-reader/Alice/Vol 2.mokuro"),
            ("PUT", "/mokuro-reader/Alice/Vol 2.cbz"),
            ("PUT", "/mokuro-reader/Alice/Vol 3.webp"),
            ("PUT", "/mokuro-reader/Alice/notes.txt"),
            ("DELETE", "/mokuro-reader/Alice/Vol 2.cbz"),
            ("DELETE", "/mokuro-reader/Alice/Extras"),
            ("DELETE", "/mokuro-reader/Alice"),
            ("DELETE", "/mokuro-reader//Alice/"),
        ] {
            assert_eq!(
                decide_as(&lib, method, path, &alice),
                Ok(Grant::Owner),
                "{method} {path}"
            );
        }
        assert_eq!(
            decide_as(&lib, "MKCOL", "/mokuro-reader/Alice/New", &alice),
            Ok(Grant::Table)
        );
        // Bob deletes neither the folder nor what is not his.
        for path in [
            "/mokuro-reader/Alice",
            "/mokuro-reader/Alice/Extras",
            "/mokuro-reader/Alice/Vol 1.cbz",
            "/mokuro-reader//Alice/./Vol 1.cbz",
        ] {
            assert_eq!(
                decide_as(&lib, "DELETE", path, &bob).unwrap_err().0,
                403,
                "{path}"
            );
        }
        // Personal corrections: anyone's own, owned folder or not.
        for who in [&bob, &as_user("r", Role::Registered)] {
            assert_eq!(
                decide_as(&lib, "PUT", "/mokuro-reader/Alice/Vol 1.edit.mokuro", who),
                Ok(Grant::Personal)
            );
        }
        // MOVE, COPY, LOCK, PROPPATCH stay ModifyDelete's, owner or not.
        for method in ["LOCK", "PROPPATCH"] {
            assert!(decide_as(&lib, method, "/mokuro-reader/Alice/Vol 1.cbz", &alice).is_err());
        }
        // An editor needs no ownership.
        let editor = as_user("ed", Role::Editor);
        assert_eq!(
            decide_as(&lib, "PUT", "/mokuro-reader/Alice/Vol 9.cbz", &editor),
            Ok(Grant::Table)
        );
        assert_eq!(
            decide_as(&lib, "MKCOL", "/mokuro-reader/Alice/New", &editor),
            Ok(Grant::Table)
        );
        // A record whose folder is gone owns nothing: the next creator may make it.
        let gone = Owners::default().owned("Gone", "alice");
        assert_eq!(
            decide_as(&gone, "MKCOL", "/mokuro-reader/Gone", &bob),
            Ok(Grant::Table)
        );
    }

    /// C2/C3: a folder is never governed by the volume it is named after, so only the
    /// series owner makes or deletes one; an uploader's control of a volume never
    /// reaches a dotted folder of other accounts' volumes.
    #[test]
    fn folders_are_their_series_owners() {
        let bob = as_user("bob", Role::Uploader);
        let lib = Owners::default()
            .upload("S/V.cbz", "alice")
            .upload("S/Vol 1.cbz", "bob")
            .upload("S/Vol 1.bonus.chapters/C1.cbz", "alice");
        assert_eq!(
            decide_as(&lib, "MKCOL", "/mokuro-reader/S/V.edit.cbz", &bob),
            Err((403, VOLUME))
        );
        assert_eq!(
            decide_as(&lib, "MKCOL", "/mokuro-reader/S/Vol 1.edit.cbz", &bob),
            Err((403, VOLUME)),
            "not even under its own volume: only the series owner"
        );
        assert_eq!(
            decide_as(
                &lib,
                "DELETE",
                "/mokuro-reader/S/Vol 1.bonus.chapters",
                &bob
            )
            .unwrap_err()
            .0,
            403
        );
        assert_eq!(
            decide_as(&lib, "MKCOL", "/mokuro-reader/S/Extras", &bob),
            Ok(Grant::Table)
        );
        let owned = Owners {
            records: vec![("S".into(), "bob".into())],
            ..lib
        };
        assert_eq!(
            decide_as(
                &owned,
                "DELETE",
                "/mokuro-reader/S/Vol 1.bonus.chapters",
                &bob
            ),
            Ok(Grant::Owner)
        );
    }

    /// The shared `curated` layer: a series owner publishes it for every volume there,
    /// an uploader for its own volumes in a shared series.
    #[test]
    fn curated_follows_control() {
        let alice = as_user("alice", Role::Uploader);
        let bob = as_user("bob", Role::Uploader);
        let curated = "Permission denied: cannot publish shared corrections for this volume";
        let lib = Owners::default()
            .upload("Shared/A.cbz", "alice")
            .upload("Shared/B.cbz", "bob")
            .upload("Mine/X.cbz", "bob")
            .file("Mine/Y.cbz")
            .owned("Mine", "alice");
        let ask = |path: &str, who: &Identity| {
            authorize_grant(&Method::PUT, path, None, who, OPEN, &lib)
                .map_err(|d| (d.status.as_u16(), d.message))
        };
        assert_eq!(
            ask("/mokuro-reader/Shared/A.curated.mokuro", &alice),
            Ok(Grant::Owner)
        );
        assert_eq!(
            ask("/mokuro-reader/Shared/B.curated.mokuro", &alice),
            Err((403, curated.into()))
        );
        for path in [
            "/mokuro-reader/Mine/X.curated.mokuro",
            "/mokuro-reader/Mine/Y.curated.mokuro",
        ] {
            assert_eq!(ask(path, &alice), Ok(Grant::Owner), "{path}");
        }
        assert_eq!(
            ask("/mokuro-reader/Mine/X.curated.mokuro", &bob),
            Err((403, SERIES_OWNED.into())),
            "a new file in another account's series"
        );
        assert_eq!(
            ask(
                "/mokuro-reader/Shared/A.curated.mokuro",
                &as_user("ed", Role::Editor)
            ),
            Ok(Grant::Table)
        );
        assert_eq!(
            ask(
                "/mokuro-reader/Shared/A.curated.mokuro",
                &as_user("r", Role::Registered)
            ),
            Err((403, curated.into()))
        );
        assert_eq!(
            ask("/mokuro-reader/Shared/A.curated.mokuro", &id(None))
                .unwrap_err()
                .0,
            401
        );
    }

    /// series.json update requests: the series' recorded owner or ModifyDelete; being
    /// the sole uploader is no longer enough.
    #[test]
    fn series_metadata_needs_the_recorded_owner() {
        let lib = Owners::default()
            .upload("Owned/V.cbz", "alice")
            .upload("Sole/V.cbz", "alice")
            .owned("Owned", "Alice");
        let alice = as_user("alice", Role::Uploader);
        assert_eq!(
            decide_as(&lib, "PUT", "/mokuro-reader/Owned/series.json", &alice),
            Ok(Grant::Table)
        );
        assert_eq!(
            decide_as(&lib, "PUT", "/mokuro-reader/Sole/series.json", &alice)
                .unwrap_err()
                .0,
            403
        );
        assert_eq!(
            decide_as(
                &lib,
                "PUT",
                "/mokuro-reader/Owned/series.json",
                &as_user("bob", Role::Uploader)
            )
            .unwrap_err()
            .0,
            403
        );
        assert_eq!(
            decide_as(
                &lib,
                "PUT",
                "/mokuro-reader/Sole/series.json",
                &as_user("ed", Role::Editor)
            ),
            Ok(Grant::Table)
        );
    }

    /// C5: until the existing folders have their owners, an uploader's additions wait
    /// (503, Retry-After); what it already controls does not.
    #[test]
    fn additions_wait_for_adoption() {
        let lib = Owners {
            pending: true,
            ..Owners::default().upload("S/V.cbz", "bob")
        };
        let bob = as_user("bob", Role::Uploader);
        for (method, path) in [
            ("PUT", "/mokuro-reader/S/W.cbz"),
            ("PUT", "/mokuro-reader/S/V.mokuro"),
            ("MKCOL", "/mokuro-reader/T"),
        ] {
            let m = Method::from_bytes(method.as_bytes()).unwrap();
            let d = authorize_grant(&m, path, None, &bob, OPEN, &lib).unwrap_err();
            assert_eq!(
                (d.status.as_u16(), d.retry_after),
                (503, Some(10)),
                "{method} {path}"
            );
            let resp = d.into_response();
            assert_eq!(resp.headers()["retry-after"], "10");
        }
        assert_eq!(
            decide_as(&lib, "PUT", "/mokuro-reader/S/V.cbz", &bob),
            Ok(Grant::Owner)
        );
        assert_eq!(
            decide_as(&lib, "DELETE", "/mokuro-reader/S/V.cbz", &bob),
            Ok(Grant::Owner)
        );
        assert_eq!(
            decide_as(
                &lib,
                "PUT",
                "/mokuro-reader/S/W.cbz",
                &as_user("ed", Role::Editor)
            ),
            Ok(Grant::Table)
        );
    }

    /// A refusal's detail rides on its response, for the PUT verdict; its text never
    /// reads as a sign-in failure.
    #[test]
    fn an_ownership_refusal_says_why() {
        let lib = Owners::default()
            .upload("S/V.cbz", "alice")
            .owned("S", "alice");
        let bob = as_user("bob", Role::Uploader);
        let d = authorize(
            &Method::PUT,
            "/mokuro-reader/S/W.cbz",
            None,
            &bob,
            OPEN,
            &lib,
        )
        .unwrap_err();
        let resp = d.into_response();
        assert_eq!(resp.status(), 403);
        assert_eq!(
            resp.extensions().get::<bunko_dav::RefusalDetail>(),
            Some(&bunko_dav::RefusalDetail(SERIES_OWNED_DETAIL))
        );
        for text in [
            SERIES_OWNED,
            SERIES_OWNED_DETAIL,
            VOLUME_OWNED,
            VOLUME_OWNED_DETAIL,
        ] {
            assert!(
                !text.contains("401") && !text.contains("Unauthorized"),
                "{text}"
            );
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
            ("PUT", "/mokuro-reader/S/V.cbz", Role::Editor),
            ("PUT", "/mokuro-reader/volume-data.json", Role::Registered),
            ("GET", edit, Role::Registered),
        ] {
            assert_eq!(
                grant(method, path, None, role),
                Ok(Grant::Table),
                "{method} {path}"
            );
        }
        // An uploader's library write is decided by a series owner rule (0.8): DAV
        // asks again before it writes.
        assert_eq!(
            grant("PUT", "/mokuro-reader/S/V.cbz", None, Role::Uploader),
            Ok(Grant::Owner)
        );
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
