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
use crate::resource::Resource;
use crate::response::{self, DavError, DavResult, Resp};
use crate::{DavContext, Inner, paths};

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

pub(crate) async fn dispatch(inner: Arc<Inner>, req: Request<Body>, ctx: DavContext) -> Resp {
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
    result.unwrap_or_else(DavError::into_response)
}

/// Run filesystem work off the async workers.
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> DavResult<T> {
    tokio::task::spawn_blocking(f)
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

pub(crate) async fn lookup(
    inner: &Arc<Inner>,
    path: &str,
    username: Option<&str>,
) -> DavResult<Option<Resource>> {
    let inner = inner.clone();
    let path = path.to_string();
    let user = username.map(str::to_string);
    blocking(move || inner.roots.lookup(&path, user.as_deref())).await
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
    let ty = if paths::is_per_user_name(paths::uri_name(&norm)) {
        "progress"
    } else {
        "webdav"
    };
    (ty, norm)
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
    let Some(stem) = crate::sidecars::primary_sidecar_stem(&name) else {
        return;
    };
    if phys.with_file_name(format!("{stem}.cbz")).is_file() {
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
        if phys.starts_with(&inner.roots.library) {
            self.removed.push(phys.to_path_buf());
        }
    }

    pub fn note_arrived(&mut self, inner: &Inner, phys: &Path) {
        if is_cbz(phys) && phys.starts_with(&inner.roots.library) {
            self.arrived.push(phys.to_path_buf());
        }
    }

    pub fn touch(&mut self, vpath: &str, phys: Option<&Path>) {
        self.listings.push(paths::normalize(vpath));
        if let Some(p) = phys {
            self.changed.push(p.to_path_buf());
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
            .filter(|p| p.starts_with(&inner.roots.library))
            .collect();
        if !changed.is_empty() {
            req.ctx.hooks.library_changed(&changed);
        }
    }
}
