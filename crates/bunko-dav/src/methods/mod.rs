//! Method dispatch and what the methods share.

pub(crate) mod lock;
pub(crate) mod put;
pub(crate) mod read;
pub(crate) mod write;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use bytes::Bytes;
use http::{HeaderMap, Method, Request, StatusCode, Uri};
use http_body_util::{BodyExt, Limited};

use crate::conditional::{self, IfHeader};
use crate::hooks::AuditEvent;
use crate::propfind::resource_key;
use crate::resource::{FileKind, Resource};
use crate::response::{self, DavError, DavResult, Resp};
use crate::{Caller, DavContext, DavInfo, DavTarget, Inner, paths};

/// Request bodies DAV parses as XML are read whole up to this size.
pub(crate) const XML_BODY_LIMIT: usize = 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Req {
    pub method: Method,
    /// The request path, percent-decoded (not normalised).
    pub path: String,
    pub headers: HeaderMap,
    pub uri: Uri,
    pub ctx: DavContext,
    pub if_header: Option<IfHeader>,
}

impl Req {
    pub fn username(&self) -> Option<&str> {
        self.ctx.username.as_deref()
    }

    /// Who per-user paths resolve for.
    pub fn caller(&self) -> Option<Caller<'_>> {
        self.ctx.caller()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        crate::header_str(&self.headers, name)
    }

    /// `Content-Length` as WSGI saw it (absent or unparsable = 0).
    pub fn content_length(&self) -> u64 {
        self.header("content-length")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn if_tokens(&self) -> &[String] {
        self.if_header
            .as_ref()
            .map(|h| h.tokens.as_slice())
            .unwrap_or(&[])
    }
}

/// Answer one DAV request, with a [`DavInfo`] on every response (the server's request
/// accounting reads it, design analytics.md §4.2). A method that resolved the path
/// itself may have set a finer one ([`read::get`]); it is kept.
pub(crate) async fn dispatch(inner: Arc<Inner>, req: Request<Body>, ctx: DavContext) -> Resp {
    let info = request_info(&req);
    let mut resp = dispatch_method(inner, req, ctx).await;
    if resp.extensions().get::<DavInfo>().is_none() {
        resp.extensions_mut().insert(info);
    }
    resp
}

/// What the request addresses, from its path alone (no I/O).
fn request_info(req: &Request<Body>) -> DavInfo {
    let Some(path) = paths::decode_request_path(req.uri().path()) else {
        return DavInfo {
            target: DavTarget::Other,
            rel: None,
            depth: None,
        };
    };
    let (target, rel) = crate::analytics::dav_target_of(&path);
    let depth = (req.method().as_str() == "PROPFIND")
        .then(|| crate::analytics::propfind_depth(crate::header_str(req.headers(), "depth")))
        .flatten();
    DavInfo { target, rel, depth }
}

async fn dispatch_method(inner: Arc<Inner>, req: Request<Body>, ctx: DavContext) -> Resp {
    let (parts, body) = req.into_parts();
    let Some(path) = paths::decode_request_path(parts.uri.path()) else {
        return response::status_page(
            StatusCode::BAD_REQUEST,
            "The request path is not valid UTF-8.",
        );
    };
    let if_header = IfHeader::parse(&parts.headers);
    let r = Req {
        method: parts.method,
        path,
        headers: parts.headers,
        uri: parts.uri,
        ctx,
        if_header,
    };
    if let Some(resp) = per_user_spelled_otherwise(&inner, &r).await {
        return resp;
    }
    if let Some(resp) = frozen_refusal(&inner, &r).await {
        return resp;
    }
    if let Some(resp) = disputed_refusal(&inner, &r).await {
        return resp;
    }
    let result = match r.method.as_str() {
        "OPTIONS" => read::options(&inner, &r).await,
        "PROPFIND" => read::propfind(&inner, &r, body).await,
        "GET" | "HEAD" => read::get(&inner, &r).await,
        "PUT" => return put::put(&inner, &r, body).await,
        "DELETE" => write::delete(&inner, &r).await,
        "MKCOL" => write::mkcol(&inner, &r).await,
        "COPY" => write::copy_move(&inner, &r, false).await,
        "MOVE" => write::copy_move(&inner, &r, true).await,
        "LOCK" => lock::lock(&inner, &r, body).await,
        "UNLOCK" => lock::unlock(&inner, &r).await,
        "PROPPATCH" => lock::proppatch(&inner, &r, body).await,
        _ => {
            // 0.5.2 sent no `Allow` (spec 14.11); listing what the path accepts is harmless.
            let mut resp = response::status_page(StatusCode::METHOD_NOT_ALLOWED, "");
            response::set_header(
                &mut resp,
                "allow",
                "OPTIONS, HEAD, GET, PROPFIND, PUT, DELETE, COPY, MOVE, MKCOL, PROPPATCH, LOCK, UNLOCK",
            );
            return resp;
        }
    };
    match result {
        Ok(resp) => {
            if resp.status().is_server_error() {
                log_server_error(&r, resp.status().as_u16(), "");
            }
            resp
        }
        Err(e) => {
            if e.status.is_server_error() {
                log_server_error(&r, e.status.as_u16(), &e.context);
            }
            e.into_response()
        }
    }
}

/// A 5xx answer is the server's (or the library storage's) failure, so it is logged:
/// the method, the path and the cause (store and provider errors carry no
/// credentials; the request's own headers are never logged). A 503 is a wait the
/// client is told to sit out (WARN); anything else is a fault (ERROR).
pub(crate) fn log_server_error(req: &Req, status: u16, cause: &str) {
    let user = req.username().unwrap_or("-");
    let cause = if cause.is_empty() {
        "(no detail)"
    } else {
        cause
    };
    if status == 503 {
        tracing::warn!(
            "[WEBDAV] {} {} answered {status} (user {user}): {cause}",
            req.method,
            req.path
        );
    } else {
        tracing::error!(
            "[WEBDAV] {} {} failed with {status} (user {user}): {cause}",
            req.method,
            req.path
        );
    }
}

/// Run filesystem work off the async workers.
/// WebDAV work on the blocking pool. It answers a client, so a remote store treats it
/// as interactive: a provider pause fails fast with a retry-after error (503) instead
/// of holding the request (spec §3.1).
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> DavResult<T> {
    tokio::task::spawn_blocking(move || bunko_store::interactive(f))
        .await
        .map_err(|_| DavError::new(crate::SERVER_ERROR, "internal error"))
}

/// Read a (small) request body whole.
pub(crate) async fn read_body(body: Body, limit: usize) -> DavResult<Bytes> {
    match Limited::new(body, limit).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(e) if e.is::<http_body_util::LengthLimitError>() => Err(DavError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body too large",
        )),
        Err(_) => Err(DavError::new(
            StatusCode::BAD_REQUEST,
            "could not read the request body",
        )),
    }
}

/// Resolve `path` for the requesting account. Blocking work, off the async workers.
pub(crate) async fn lookup(
    inner: &Arc<Inner>,
    path: &str,
    req: &Req,
) -> DavResult<Option<Resource>> {
    let inner = inner.clone();
    let path = path.to_string();
    let ctx = req.ctx.clone();
    blocking(move || inner.roots.lookup(&path, ctx.caller())).await
}

/// A write auth admitted only as the caller's own correction
/// ([`DavContext::personal_only`]) must find one where it writes: refused (409) when
/// the path now resolves to anything else.
pub(crate) fn personal_grant_holds(req: &Req, kind: Option<FileKind>) -> DavResult<()> {
    if req.ctx.personal_only && kind != Some(FileKind::Personal) {
        return Err(DavError::new(
            StatusCode::CONFLICT,
            "The folder changed while this request was checked; try again.",
        ));
    }
    Ok(())
}

/// A write auth admitted by a series owner rule ([`DavContext::ownership_recheck`]) is
/// decided again where it lands: refused (403, the fresh refusal) when it no longer
/// would be, or 503 with `Retry-After` when that cannot be decided now. Blocking.
pub(crate) fn ownership_holds(req: &Req) -> DavResult<()> {
    match &req.ctx.ownership_recheck {
        Some(recheck) => recheck().map_err(|refusal| match refusal.retry_after {
            Some(secs) => DavError {
                retry_after: Some(secs),
                ..DavError::new(StatusCode::SERVICE_UNAVAILABLE, refusal.message)
            },
            None => DavError::new(StatusCode::FORBIDDEN, refusal.message),
        }),
        None => Ok(()),
    }
}

/// HTTP conditionals and the DAV `If:` header against an existing resource (WsgiDAV
/// `_evaluate_if_headers`).
pub(crate) fn eval_if(inner: &Inner, req: &Req, res: &Resource) -> DavResult<()> {
    let etag = res.etag();
    conditional::evaluate_http(
        &req.headers,
        &req.method,
        etag.as_deref(),
        res.last_modified(),
    )?;
    if let Some(ifh) = &req.if_header {
        let key = resource_key(res, req.username());
        let tokens = inner.locks.indirect_tokens(&key, req.ctx.principal());
        if !ifh.test(res.path(), etag.as_deref(), &tokens) {
            return Err(DavError::new(
                StatusCode::PRECONDITION_FAILED,
                "'If' header condition failed.",
            ));
        }
    }
    Ok(())
}

/// May this request modify `key` given the DAV locks (WsgiDAV `_check_write_permission`)?
pub(crate) fn check_dav_locks(
    inner: &Inner,
    req: &Req,
    key: &str,
    infinite: bool,
) -> DavResult<()> {
    inner
        .locks
        .check_write(key, infinite, req.if_tokens(), req.ctx.principal())
        .map_err(DavError::locked_by)
}

/// `/` or `/mokuro-reader` (any trailing-slash form): never moved, copied, deleted,
/// locked or PROPPATCHed (spec 14.3 FIX).
pub(crate) fn is_virtual_root(path: &str) -> bool {
    matches!(
        paths::classify(path),
        paths::Target::Root | paths::Target::ReaderRoot
    )
}

/// The audit target of a file (0.5.2 `MokuroFileResource._audit`).
pub(crate) fn file_audit_target(inner: &Inner, vpath: &str, phys: &Path) -> (&'static str, String) {
    if let Some(rel) = inner.roots.library_rel(phys) {
        return ("library", format!("/{}/{rel}", paths::READER_ROOT));
    }
    let norm = paths::normalize(vpath);
    let ty = if is_progress_path(&norm) {
        "progress"
    } else {
        "webdav"
    };
    (ty, norm)
}

/// The audit target of a personal correction (D27): the account's own file, at the
/// path it asked for, whether it is an overlay or the cloud-root owner's physical file.
pub(crate) fn personal_audit_target(vpath: &str) -> (&'static str, String) {
    ("personal", paths::normalize(vpath))
}

/// The audit target of a file of `kind`.
pub(crate) fn audit_target_of(
    inner: &Inner,
    kind: Option<FileKind>,
    vpath: &str,
    phys: &Path,
) -> (&'static str, String) {
    match kind {
        Some(FileKind::Personal) => personal_audit_target(vpath),
        _ => file_audit_target(inner, vpath, phys),
    }
}

/// A path of the caller's own tree (root files, `history/**`).
pub(crate) fn is_progress_path(vpath: &str) -> bool {
    matches!(paths::classify(vpath), paths::Target::Progress(_))
}

/// The audit target of a folder (0.5.2 `MokuroFolderResource._audit`).
pub(crate) fn folder_audit_target(
    inner: &Inner,
    vpath: &str,
    phys: Option<&Path>,
) -> (&'static str, String) {
    match phys
        .and_then(|p| inner.roots.library_rel(p))
        .filter(|r| !r.is_empty())
    {
        Some(rel) => ("library_folder", format!("/{}/{rel}", paths::READER_ROOT)),
        None if is_progress_path(vpath) => ("progress", paths::normalize(vpath)),
        None => ("webdav_folder", paths::normalize(vpath)),
    }
}

pub(crate) fn audit(
    req: &Req,
    action: &'static str,
    target: (&'static str, String),
    details: Option<serde_json::Value>,
) {
    req.ctx.hooks.audit(AuditEvent {
        action,
        actor: req.ctx.username.clone(),
        target_type: target.0,
        target_path: target.1,
        details,
    });
}

/// Audit a 423 from the per-path write locks and build the error.
pub(crate) fn write_lock_conflict(
    req: &Req,
    target: (&'static str, String),
    operation: &str,
    destination: Option<&str>,
) -> DavError {
    let mut details = serde_json::json!({ "operation": operation });
    if let Some(d) = destination {
        details["destination"] = serde_json::Value::String(d.to_string());
    }
    audit(req, "lock_conflict", target, Some(details));
    DavError::new(
        StatusCode::LOCKED,
        "Resource is locked by another write operation",
    )
}

/// The cloud-root owner's per-user entries carry their canonical names in the cloud
/// folder (`history/`, `goals.json`), shared with a reader that reads it directly. While
/// that folder lists the name only in another spelling (`History/`, `Goals.json`, from
/// 0.7 or a cloud app), that entry is the name: Bunko never writes the canonical
/// spelling beside it. A write there (or a COPY/MOVE onto it) is refused with 409 until
/// it is renamed (doctor names it). Reads find nothing there. Every other account's
/// tree is `users/<u>/` and never meets this.
async fn per_user_spelled_otherwise(inner: &Arc<Inner>, r: &Req) -> Option<Resp> {
    if !matches!(
        r.method.as_str(),
        "PUT" | "DELETE" | "MKCOL" | "COPY" | "MOVE" | "PROPPATCH" | "LOCK"
    ) {
        return None;
    }
    let caller = r.caller()?;
    if !caller.owns_cloud_root {
        return None;
    }
    let mut targets = vec![r.path.clone()];
    if matches!(r.method.as_str(), "COPY" | "MOVE")
        && let Ok(dest) = write::destination_path(r)
    {
        targets.push(dest);
    }
    let names: Vec<&'static str> = targets
        .iter()
        .filter_map(|p| match paths::classify(p) {
            paths::Target::Progress(rel) => bunko_core::reader_tree::canonical_per_user_rel(&rel),
            _ => None,
        })
        .filter_map(|rel| {
            let first = rel.split('/').next().unwrap_or("").to_string();
            bunko_core::reader_tree::PER_USER_FILES
                .iter()
                .chain(bunko_core::reader_tree::PER_USER_DIRS.iter())
                .find(|n| **n == first)
                .copied()
        })
        .collect();
    if names.is_empty() {
        return None;
    }
    let library = inner.roots.library.clone();
    let listed = blocking(move || -> Option<(String, String)> {
        let entries: Vec<String> = bunko_store::fs::read_dir(&library)
            .ok()?
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.iter().find_map(|canonical| {
            if entries.iter().any(|n| n == canonical) {
                return None;
            }
            entries
                .iter()
                .find(|n| crate::fold_name(n) == crate::fold_name(canonical))
                .map(|n| (n.clone(), canonical.to_string()))
        })
    })
    .await
    .ok()
    .flatten()?;
    let (listed, canonical) = listed;
    Some(response::status_page(
        StatusCode::CONFLICT,
        &format!(
            "The cloud folder holds {listed:?} where your {canonical:?} belongs, and Bunko \
             keeps one spelling per name: rename it to {canonical:?} (doctor and the \
             startup log name it), then try again."
        ),
    ))
}

/// The freeze (design 0.8 D5, [`bunko_core::fold_groups`]): a folder that lists two or
/// more spellings of one name (only a case-sensitive store can, made outside Bunko or
/// by a release before 0.5.3) holds a case-variant group, and until an admin merges it
/// no write touches it. A PUT, DELETE, MKCOL, COPY, MOVE, PROPPATCH or LOCK whose
/// target, COPY or MOVE source, or destination is a member of a group (the reachable
/// winner included), a file rule v1 gives or would give to an archive of one (or below
/// a member folder), answers 409 naming the group; so does a DELETE, COPY or MOVE of a
/// folder that holds one, or a COPY or MOVE onto such a folder. Reads still reach the
/// winner. Decided once, here, from the folder listings: the same on every host, for
/// every method.
async fn frozen_refusal(inner: &Arc<Inner>, r: &Req) -> Option<Resp> {
    let method = r.method.as_str();
    if !matches!(
        method,
        "PUT" | "DELETE" | "MKCOL" | "COPY" | "MOVE" | "PROPPATCH" | "LOCK"
    ) {
        return None;
    }
    let whole = matches!(method, "DELETE" | "COPY" | "MOVE");
    let mut targets: Vec<String> = Vec::new();
    if let paths::Target::Library(rel) = paths::classify(&r.path) {
        targets.push(rel);
    }
    if matches!(method, "COPY" | "MOVE")
        && let Ok(dest) = write::destination_path(r)
        && let paths::Target::Library(rel) = paths::classify(&dest)
    {
        targets.push(rel);
    }
    if targets.is_empty() {
        return None;
    }
    let library = inner.roots.library.clone();
    let roots = inner.roots.clone();
    let refusal = blocking(move || -> Option<String> {
        for rel in targets {
            // As spelled (`.` segments stay in their folder, `..` is held), and where
            // the handler will write: the physical path the request resolves to
            // (symlinks followed, `\` a separator on Windows), library-relative.
            if let Some(frozen) = crate::path_case::frozen(&library, &rel) {
                return Some(frozen.describe());
            }
            let Some(rel) = roots
                .library_member(&library, &rel)
                .and_then(|phys| roots.library_rel(&phys))
            else {
                continue;
            };
            if let Some(frozen) = crate::path_case::frozen(&library, &rel) {
                return Some(frozen.describe());
            }
            if whole
                && crate::path_case::reachable_as_spelled(&library, &rel, true)
                && let Some((dir, group)) = crate::path_case::frozen_inside(&library.join(&rel))
            {
                let folder = paths::relative_posix(&dir, &library).unwrap_or_default();
                return Some(bunko_core::fold_groups::Frozen::Group { folder, group }.describe());
            }
        }
        None
    })
    .await
    .ok()
    .flatten()?;
    Some(response::status_page(StatusCode::CONFLICT, &refusal))
}

/// Where the filename fold and sidecar names rule v1 part ways about a name
/// ([`bunko_core::sidecar_names::fold_disputed`]): a PUT, MKCOL, PROPPATCH or LOCK of
/// it, or a COPY or MOVE onto it, answers 409, for every account. Every host makes
/// `S/STRASSE.mokuro` one name with `S/Straße.mokuro`, the OCR file of a listed
/// `S/Straße.cbz`, but rule v1 (Mokuro Reader's) does not give it to that volume: such
/// a file would take the volume's name without being its file. The same for a name
/// only the fold gives a volume file suffix (`V.mo\u{212A}uro`). An archive is never
/// refused for such a name already listed beside it (`S/Straße.cbz` beside a
/// `S/STRASSE.mokuro` made first): the name is then the archive's for authorization,
/// and Bunko writes nothing for the volume in its place. Decided from the folder's
/// listing, the name in the spelling the folder lists it in (the one a write reaches),
/// so every host answers alike. Reads, DELETE and a MOVE away are left alone: they are
/// how such a name is cleared up.
///
/// A MOVE whose destination folds like its own source (in the same folder) respells
/// that entry, and is judged as the source renamed: the destination name as spelled,
/// over the listing without the source. Renaming a stray to the volume's own spelling
/// (`S/STRASSE.mokuro` to `S/Straße.mokuro`, the repair doctor names) is done;
/// respelling it as another stray is not. (The freeze ran before.)
///
/// Also refused: a name the fold makes one with a personal or curated layer name while
/// it is no such layer ([`bunko_core::sidecar_names::shadows_reserved_layer`]:
/// `X.ed\u{200B}it.mokuro`, `X.EDIT.mokuro`), its volume listed or not, and an archive
/// whose OCR file could only be such a name (`X.ed\u{200B}it.cbz`,
/// [`bunko_core::sidecar_names::shadowing_archive`]), `X.cbz` listed or not.
///
/// An account's own correction (an `edit`/`edit-<n>` layer name, written to its copy
/// under `users/`) is never refused here, whatever the library lists: nothing in the
/// library is written. A write of the shared layer, or of the cloud-root owner's
/// correction (written in the library), beside a listed stray that is one name with it
/// is never judged as the stray: it answers 409 naming the stray.
///
/// One volume name per fold ([`volume_name_refusal`]): no archive is created whose
/// volume name the fold joins with a listed archive's spelled otherwise
/// (`S/STRASSE.curated.cbz` beside `S/Straße.cbz`, either way round), and where two
/// such archives are listed already, no file of either volume is written.
async fn disputed_refusal(inner: &Arc<Inner>, r: &Req) -> Option<Resp> {
    let target = match r.method.as_str() {
        "PUT" | "MKCOL" | "PROPPATCH" | "LOCK" => r.path.clone(),
        "COPY" | "MOVE" => write::destination_path(r).ok()?,
        _ => return None,
    };
    let paths::Target::Library(rel) = paths::classify(&target) else {
        return None;
    };
    // A MOVE's source: a destination that folds like it respells that very entry.
    let source = match paths::classify(&r.path) {
        paths::Target::Library(src) if r.method == "MOVE" => Some(src),
        _ => None,
    };
    let owns_cloud_root = r.caller().is_some_and(|c| c.owns_cloud_root);
    let roots = inner.roots.clone();
    let library = roots.library.clone();
    let refusal = blocking(move || -> Option<String> {
        let rel = rel.trim_end_matches('/');
        let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        if name.is_empty() || name == "." || name == ".." {
            return None;
        }
        let resolved_dir =
            |dir: &str| crate::path_case::resolve_existing(&library, dir.trim_matches('/'), true);
        let parent = if dir.trim_matches('/').is_empty() {
            library.clone()
        } else {
            roots.library_member(&library, &resolved_dir(dir))?
        };
        let mut files = crate::personal::names_in(&parent);
        {
            let refs: Vec<&str> = files.iter().map(String::as_str).collect();
            if !owns_cloud_root
                && bunko_core::sidecar_names::reserved_layer_of(name, &refs)
                    == Some(bunko_core::sidecar_names::LayerClass::Personal)
            {
                // An account's own correction, written to its copy under `users/`
                // ([`crate::resource::Roots::personal_path`]): nothing in the library is
                // written, so nothing listed there can dispute it.
                return None;
            }
        }
        let folded = crate::fold_name(name);
        // A MOVE's source in this very folder: it leaves the listing.
        let source_here = source.as_deref().and_then(|src| {
            let src = src.trim_end_matches('/');
            let (src_dir, src_name) = src.rsplit_once('/').unwrap_or(("", src));
            (resolved_dir(src_dir) == resolved_dir(dir)).then(|| src_name.to_string())
        });
        let respelled = source_here
            .clone()
            .filter(|src_name| src_name != name && crate::fold_name(src_name) == folded);
        let spelled = if let Some(src_name) = respelled {
            // The source renamed (`S/STRASSE.mokuro` to `S/Stra\u{df}e.mokuro`, the
            // repair doctor names): judged as the name it takes, without the source.
            files.retain(|n| *n != src_name);
            name.to_string()
        } else {
            let refs: Vec<&str> = files.iter().map(String::as_str).collect();
            let layer = bunko_core::sidecar_names::reserved_layer_of(name, &refs);
            let listed = files
                .iter()
                .find(|n| *n == name)
                .or_else(|| files.iter().find(|n| crate::fold_name(n) == folded))
                .cloned();
            match (listed, layer) {
                // A personal or curated layer name, beside a listed entry that is one
                // name with it but not that layer (`X.ed\u{200B}it.mokuro`, made by hand
                // or before 0.8): never that entry.
                (Some(other), Some(class))
                    if bunko_core::sidecar_names::reserved_layer_of(&other, &refs)
                        != Some(class) =>
                {
                    return Some(format!(
                        "{other:?} is listed here, and is one name with {name:?} on \
                         every host (Bunko joins names as the case-insensitive \
                         filesystems do), but it is no such layer: Bunko writes no \
                         second spelling of a name. Rename or delete {other:?} first \
                         (doctor names it)."
                    ));
                }
                (listed, _) => listed.unwrap_or_else(|| name.to_string()),
            }
        };
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        if let Some(refusal) = volume_name_refusal(&spelled, &refs, source_here.as_deref()) {
            return Some(refusal);
        }
        let disputed = bunko_core::sidecar_names::fold_disputed(&spelled, &refs);
        if disputed.is_empty() {
            let class = bunko_core::sidecar_names::shadows_reserved_layer(&spelled, &refs)?;
            if bunko_core::sidecar_names::shadowing_archive(&spelled).is_some() {
                return Some(format!(
                    "{spelled:?} is, on every host (Bunko joins names as the case-insensitive \
                     filesystems do), one name with an archive whose OCR file would be an \
                     account's own correction or the curated layer of the volume of the \
                     shorter stem, but the sidecar naming rule Mokuro Reader shares reads \
                     no such layer in it: its OCR file could only ever stand in that \
                     layer's place, so Bunko creates or writes no such archive. Choose \
                     another name."
                ));
            }
            let layer = match class {
                bunko_core::sidecar_names::LayerClass::Personal => {
                    "an account's own correction (edit, edit-<n>)"
                }
                bunko_core::sidecar_names::LayerClass::Curated => "the shared curated layer",
                _ => "a reserved layer (curated-<n>)",
            };
            return Some(format!(
                "{spelled:?} is, on every host (Bunko joins names as the case-insensitive \
                 filesystems do), one name with {layer} of a volume of that stem, but the \
                 sidecar naming rule Mokuro Reader shares reads no such layer in it: such \
                 a name could only ever stand in that layer's place, so Bunko creates or \
                 writes none. Choose another name."
            ));
        }
        Some(if disputed == [spelled.clone()] {
            format!(
                "{spelled:?} is, on every host (Bunko joins names as the case-insensitive \
                 filesystems do), a name ending in a volume file suffix (.cbz, .mokuro, \
                 .mokuro.gz, .webp, .nocover) it does not spell itself: Bunko creates or \
                 writes no such name. Choose another name."
            )
        } else {
            let joined = disputed
                .iter()
                .map(|n| format!("{n:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{spelled:?} is tied to {joined} here on every host (Bunko joins names \
                 as the case-insensitive filesystems do), but the sidecar naming rule \
                 Mokuro Reader shares does not make them one volume's archive and file: \
                 Bunko creates or writes no such ambiguous name. Spell it exactly like the \
                 volume's stem, or choose another name."
            )
        })
    })
    .await
    .ok()
    .flatten()?;
    if r.method == "PUT" && crate::put_gives_verdict(&r.path) {
        return Some(put::failure_response(&crate::upload::Rejection::new(
            409,
            "ambiguous-name",
            refusal,
            false,
        )));
    }
    Some(response::status_page(StatusCode::CONFLICT, &refusal))
}

/// One volume name per filename fold
/// ([`bunko_core::sidecar_names::volume_name_ties`]), for a write of `name` (spelled
/// as the folder lists it, when it does) in a folder whose listed FILES are `files`
/// (`source`: a MOVE's own source in this folder, which leaves it):
///
/// * an archive not listed yet (a PUT, a COPY or MOVE onto it) whose volume name the
///   fold joins with a listed archive's spelled otherwise (`STRASSE.curated.cbz`
///   beside `Straße.cbz`, `Straße.cbz` beside `STRASSE.curated.cbz`,
///   `X\u{200B}.cbz` beside `X.5.cbz`) is refused, as a case-insensitive disk keeps
///   one spelling of a name: the first one listed keeps it;
/// * any other name a listed archive with such a tie governs, by either fold
///   ([`bunko_core::sidecar_names::governing_archives_by_either_fold`]: its sidecars,
///   layers, cover, a name nested under it) is refused naming the pair, while the
///   pair is listed (made before this release, or outside Bunko).
///
/// An archive already listed is replaced, deleted and moved away as ever: that is how
/// a pair is cleared up. Reads are never refused. Asked at dispatch, from the listing
/// before the body is read, and again right before an archive lands
/// ([`archive_name_check`]).
fn volume_name_refusal(name: &str, files: &[&str], source: Option<&str>) -> Option<String> {
    use bunko_core::sidecar_names::{
        cbz_stem, governing_archives_by_either_fold, volume_name_ties,
    };
    let quoted = |names: &[&str]| {
        names
            .iter()
            .map(|n| format!("{n:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if cbz_stem(name).is_some() {
        if files.contains(&name) {
            return None;
        }
        let others: Vec<&str> = files
            .iter()
            .copied()
            .filter(|n| Some(*n) != source)
            .collect();
        let ties = volume_name_ties(name, &others);
        if ties.is_empty() {
            return None;
        }
        return Some(format!(
            "{name:?} would be one volume name with {} here on every host (Bunko joins \
             names as the case-insensitive filesystems do, and a nested archive such as \
             <stem>.curated.cbz is the volume <stem>'s), spelled otherwise: Bunko keeps \
             one spelling of a volume name, the one listed first. Spell it exactly like \
             that volume, or choose another name.",
            quoted(&ties)
        ));
    }
    for archive in governing_archives_by_either_fold(name, files) {
        let ties = volume_name_ties(archive, files);
        if ties.is_empty() {
            continue;
        }
        return Some(format!(
            "{archive:?} and {} are one volume name on every host (Bunko joins names as \
             the case-insensitive filesystems do), spelled otherwise (made before this \
             release, or outside Bunko): no file of either volume, {name:?} included, is \
             written until one of those archives is renamed or deleted (doctor names \
             them). Reads still serve them.",
            quoted(&ties)
        ));
    }
    None
}

/// Does an archive landing at `dest` (physical) take a name in a library folder, so
/// one volume name per fold ([`archive_name_check`]) is asked again right before it
/// lands?
pub(crate) fn lands_library_archive(inner: &Inner, dest: &Path) -> bool {
    is_cbz(dest) && dest.parent().is_some() && inner.roots.library_rel(dest).is_some()
}

/// The [`Inner::archive_publishes`] key of the folder an archive lands in at `dest`:
/// its physical path, folded, so every spelling of one folder is one key.
pub(crate) fn archive_publish_key(dest: &Path) -> PathBuf {
    let dir = dest.parent().unwrap_or(dest);
    PathBuf::from(crate::fold_name(&dir.to_string_lossy()))
}

/// One volume name per filename fold ([`volume_name_refusal`]), asked again from a
/// fresh listing of the folder right before an archive lands at `dest` (a PUT's
/// publish, a COPY's or MOVE's rename; `source`: a MOVE's source, which leaves the
/// folder when it is the same one), the caller holding the folder's
/// [`Inner::archive_publishes`] lock through the publish. What dispatch checked was the
/// listing before the body was read or the file copied: an archive that landed since
/// (a racing upload, however long it was held open) is seen here, and of two archives
/// the fold makes one volume name only the first to land is published, the other
/// refused (`Some`: the 409 `ambiguous-name` detail), for every account. Blocking.
pub(crate) fn archive_name_check(dest: &Path, source: Option<&Path>) -> Option<String> {
    let (dir, name) = (dest.parent()?, dest.file_name()?.to_str()?);
    let files = crate::personal::names_in(dir);
    let refs: Vec<&str> = files.iter().map(String::as_str).collect();
    let source = source
        .filter(|src| src.parent() == Some(dir))
        .and_then(|src| src.file_name())
        .and_then(|n| n.to_str());
    volume_name_refusal(name, &refs, source)
}

/// 0.5.2 `_forget_ocr_records`.
pub(crate) fn forget_ocr_records(req: &Req, rel: Option<&str>, archive_too: bool) {
    let Some(rel) = rel else { return };
    let lower = rel.to_lowercase();
    if lower.ends_with(".cbz") {
        if archive_too {
            req.ctx.hooks.forget_ocr_sidecars_of_volume(rel);
        }
    } else if lower.ends_with(".mokuro") || lower.ends_with(".mokuro.gz") {
        req.ctx.hooks.forget_ocr_sidecar(rel);
    }
}

/// 0.5.2 `_remember_primary_uuid`: tell the server a volume's primary sidecar is leaving.
pub(crate) fn primary_leaving(inner: &Inner, req: &Req, phys: &Path) {
    let Some(rel) = inner.roots.library_rel(phys) else {
        return;
    };
    let name = paths::file_name(phys);
    if crate::sidecars::primary_sidecar_stem(&name).is_none() {
        return;
    }
    // The primary of a listed archive (rule v1 over the folder's listing: `v.mokuro` is
    // `V.CBZ`'s), never a constructed archive name opened on the host.
    let Some(dir) = phys.parent() else { return };
    let Ok(entries) = bunko_store::fs::read_dir(dir) else {
        return;
    };
    let names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let folder = bunko_core::sidecar_names::Folder::new(&names);
    if names
        .iter()
        .filter(|n| bunko_core::sidecar_names::cbz_stem(n).is_some())
        .any(|archive| folder.primary_of(archive) == Some(name.as_str()))
    {
        req.ctx.hooks.primary_sidecar_leaving(&rel, phys);
    }
}

pub(crate) fn is_cbz(path: &Path) -> bool {
    paths::py_suffix(&paths::file_name(path)).eq_ignore_ascii_case(".cbz")
}

/// What a committed write changed, reported once the operation is done (0.5.2
/// `UploadMiddleware` ran these after the DAV answer; order: removals, then arrivals).
#[derive(Default)]
pub(crate) struct Effects {
    pub removed: Vec<PathBuf>,
    pub arrived: Vec<PathBuf>,
    /// Virtual paths whose listings changed (PROPFIND cache).
    pub listings: Vec<String>,
    /// Physical library paths that changed.
    pub changed: Vec<PathBuf>,
}

impl Effects {
    pub fn note_removed(&mut self, inner: &Inner, phys: &Path) {
        if inner.roots.library_rel(phys).is_some() {
            self.removed.push(phys.to_path_buf());
        }
    }

    pub fn note_arrived(&mut self, inner: &Inner, phys: &Path) {
        if is_cbz(phys) && inner.roots.library_rel(phys).is_some() {
            self.arrived.push(phys.to_path_buf());
        }
    }

    /// A write changed `vpath`. The caller's own tree is in no shared listing and is no
    /// library change, so a progress or history write evicts nothing (a reader saving
    /// its goals used to drop the whole-library `Depth: infinity` listing).
    pub fn touch(&mut self, vpath: &str, phys: Option<&Path>) {
        if is_progress_path(vpath) {
            return;
        }
        self.listings.push(paths::normalize(vpath));
        if let Some(p) = phys {
            self.changed.push(p.to_path_buf());
        }
    }

    /// A personal correction changed. An overlay is in no shared listing; the cloud-root
    /// owner's physical file is, hidden, in the owner's part of the cached listings
    /// (which therefore go). Neither is a library change.
    pub fn touch_personal(&mut self, inner: &Inner, vpath: &str, phys: &Path) {
        if inner.roots.library_rel(phys).is_some() {
            self.listings.push(paths::normalize(vpath));
        }
    }

    /// Fire the hooks (blocking context) and drop the affected cached listings.
    pub fn fire(self, inner: &Inner, req: &Req) {
        if !self.listings.is_empty() {
            inner.cache.invalidate_paths(&self.listings);
        }
        if !self.removed.is_empty() {
            req.ctx.hooks.archives_removed(&self.removed);
        }
        for cbz in &self.arrived {
            req.ctx.hooks.archive_arrived(cbz);
        }
        let changed: Vec<PathBuf> = self
            .changed
            .into_iter()
            .filter(|p| inner.roots.library_rel(p).is_some())
            .collect();
        if !changed.is_empty() {
            req.ctx.hooks.library_changed(&changed);
        }
    }
}
