//! OPTIONS, PROPFIND, GET and HEAD.

use std::io::SeekFrom;
use std::sync::Arc;

use axum::body::Body;
use http::header::{
    ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, HeaderValue, LAST_MODIFIED,
};
use http::{Method, Response, StatusCode};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use super::{Req, XML_BODY_LIMIT, blocking, eval_if, lookup, read_body};
use crate::conditional::{self, RangeOutcome};
use crate::paths::Target;
use crate::propfind::{self, Mode, PropCtx};
use crate::resource::{FileKind, Resource, Stat, content_type};
use crate::response::{self, DavError, DavResult, Resp};
use crate::xml::{MULTISTATUS_CLOSE, MULTISTATUS_OPEN};
use crate::{DavInfo, DavTarget, Inner, Served, paths};
use bunko_core::{Perm, Role, has_perm, reader_tree};

const ALLOW_COLLECTION: &str =
    "OPTIONS, HEAD, GET, PROPFIND, DELETE, COPY, MOVE, PROPPATCH, LOCK, UNLOCK";
const ALLOW_COLLECTION_WRITE: &str =
    "OPTIONS, HEAD, GET, PROPFIND, PUT, MKCOL, DELETE, COPY, MOVE, PROPPATCH, LOCK, UNLOCK";
const ALLOW_COLLECTION_PUT: &str =
    "OPTIONS, HEAD, GET, PROPFIND, PUT, DELETE, COPY, MOVE, PROPPATCH, LOCK, UNLOCK";
const ALLOW_FILE: &str =
    "OPTIONS, HEAD, GET, PROPFIND, PUT, DELETE, COPY, MOVE, PROPPATCH, LOCK, UNLOCK";
const ALLOW_UNMAPPED: &str = "OPTIONS, PUT, MKCOL";

/// Would a GET of this file be offloaded to nginx (spec §8.10)? Only where the file is
/// on this disk (`caps().local_paths`): nginx cannot read a remote store.
fn accel_path(inner: &Inner, req: &Req, res: &Resource) -> Option<String> {
    if !req.ctx.nginx_accel {
        return None;
    }
    match res {
        Resource::File {
            phys,
            kind: FileKind::Library,
            ..
        } if bunko_store::store_for(phys).is_none_or(|s| s.caps().local_paths) => {
            let rel = inner.roots.library_rel(phys)?;
            Some(format!("/internal-library/{}", paths::quote_path(&rel)))
        }
        _ => None,
    }
}

/// The Allow of a collection for this account (plan X9). Advisory only: `authorize()`
/// still decides every request. It exists so a client that reads Allow (the reader's
/// generic WebDAV check) does not take a writable account for a read-only one.
/// - AddFiles: `PUT, MKCOL` on every collection at or under `/mokuro-reader`.
/// - WriteProgress alone: `PUT, MKCOL` on the reader root and its own `history/**`;
///   `PUT` on library collections, where its personal corrections go (D27).
/// - Anyone else, and `/`, `/inbox`: the base list.
fn collection_allow(path: &str, role: Role) -> &'static str {
    let adds_files = has_perm(role, Perm::AddFiles);
    let writes_own = has_perm(role, Perm::WriteProgress);
    match paths::classify(path) {
        Target::ReaderRoot | Target::Progress(_) | Target::Library(_) if adds_files => {
            ALLOW_COLLECTION_WRITE
        }
        Target::ReaderRoot | Target::Progress(_) if writes_own => ALLOW_COLLECTION_WRITE,
        Target::Library(_) if writes_own => ALLOW_COLLECTION_PUT,
        _ => ALLOW_COLLECTION,
    }
}

pub(crate) async fn options(inner: &Arc<Inner>, req: &Req) -> DavResult<Resp> {
    let mut resp = response::empty(StatusCode::OK);
    response::set_header(&mut resp, "content-type", "text/html; charset=utf-8");
    response::set_header(&mut resp, "dav", "1,2");
    response::set_header(&mut resp, "ms-author-via", "DAV");
    if req.path == "*" {
        return Ok(resp);
    }
    let res = lookup(inner, &req.path, req).await?;
    match &res {
        Some(r) if r.is_collection() => {
            let allow = collection_allow(&req.path, req.ctx.role);
            response::set_header(&mut resp, "allow", allow);
        }
        Some(r) => {
            response::set_header(&mut resp, "allow", ALLOW_FILE);
            if accel_path(inner, req, r).is_none() {
                response::set_header(&mut resp, "accept-ranges", "bytes");
            }
        }
        None => {
            let parent = match paths::uri_parent(&req.path) {
                Some(p) => lookup(inner, &p, req).await?,
                None => None,
            };
            if parent.is_some_and(|p| p.is_collection()) {
                response::set_header(&mut resp, "allow", ALLOW_UNMAPPED);
            } else {
                resp = response::status_page(StatusCode::NOT_FOUND, &req.path);
            }
        }
    }
    if crate::is_dav_path(&req.path) {
        response::set_header(&mut resp, "x-mokuro-put", "verified");
    }
    Ok(resp)
}

pub(crate) async fn propfind(inner: &Arc<Inner>, req: &Req, body: Body) -> DavResult<Resp> {
    let raw_depth = req.header("depth").map(str::to_ascii_lowercase);
    let depth = match raw_depth.as_deref() {
        None | Some("infinity") => None,
        Some("0") => Some(0),
        Some("1") => Some(1),
        Some(other) => {
            return Err(DavError::new(
                StatusCode::BAD_REQUEST,
                format!("Invalid Depth header: '{other}'."),
            ));
        }
    };
    let res = lookup(inner, &req.path, req)
        .await?
        .ok_or_else(|| DavError::new(StatusCode::NOT_FOUND, &req.path))?;
    eval_if(inner, req, &res)?;
    let body = read_body(body, XML_BODY_LIMIT).await?;

    // Depth: infinity, in any spelling or implied by a missing header (RFC 4918 9.1), is
    // answered from the shared cache, allprop. Walking it live per request let a burst of
    // header-less PROPFINDs each build a whole-library listing (one per request, in RAM).
    // A file (one response, whatever the depth) is still answered live: caching each
    // file would only evict the listings worth keeping.
    let norm = paths::normalize(&req.path);
    let gzip = req
        .header("accept-encoding")
        .is_some_and(|v| v.contains("gzip"));
    if depth.is_none()
        && res.is_collection()
        && !matches!(paths::classify(&norm), paths::Target::Progress(_))
    {
        if let Some(hit) = inner.cache.get(&norm).await {
            let mut tail = injected_progress(inner, req, &norm).await?;
            tail.push_str(&injected_personal(inner, req, &res, &hit).await?);
            return Ok(crate::cache::respond(hit, &tail, gzip));
        }
        return Err(DavError::new(StatusCode::NOT_FOUND, &req.path));
    }

    let mode =
        propfind::parse_mode(&body).ok_or_else(|| DavError::status(StatusCode::BAD_REQUEST))?;
    let inner2 = inner.clone();
    let dctx = req.ctx.clone();
    if depth == Some(1) && res.is_collection() {
        // A folder's members: generated compressed in one of the cache's bounded
        // generation slots and streamed out, so a 30k-member folder costs its compressed
        // size per request and at most a few are built at once.
        let hit = inner
            .cache
            .live(move |feed| {
                let ctx = PropCtx {
                    caller: dctx.caller(),
                    locks: Some(&inner2.locks),
                    dead: Some(&inner2.dead),
                    hidden_personal: None,
                };
                propfind::walk(&inner2.roots, &res, depth, &mode, &ctx, feed);
            })
            .await
            .ok_or_else(|| DavError::status(crate::SERVER_ERROR))?;
        return Ok(crate::cache::respond(hit, "", gzip));
    }
    let xml = blocking(move || {
        let ctx = PropCtx {
            caller: dctx.caller(),
            locks: Some(&inner2.locks),
            dead: Some(&inner2.dead),
            hidden_personal: None,
        };
        let mut out = String::from(MULTISTATUS_OPEN);
        propfind::walk(&inner2.roots, &res, depth, &mode, &ctx, &mut |chunk| {
            out.push_str(chunk)
        });
        out.push_str(MULTISTATUS_CLOSE);
        out
    })
    .await?;
    Ok(response::bytes_response(
        StatusCode::MULTI_STATUS,
        "application/xml; charset=utf-8",
        xml.into_bytes(),
    ))
}

/// The caller's own tree, appended to a shared `/` or `/mokuro-reader` listing (spec
/// §10.1 item 4): its root files, then its whole `history/` (a Depth-infinity listing is
/// allprop at every level). The cached shared body itself stays anonymous.
async fn injected_progress(inner: &Arc<Inner>, req: &Req, norm: &str) -> DavResult<String> {
    if req.caller().is_none() {
        return Ok(String::new());
    }
    if !matches!(
        paths::classify(norm),
        paths::Target::Root | paths::Target::ReaderRoot
    ) {
        return Ok(String::new());
    }
    let inner = inner.clone();
    let dctx = req.ctx.clone();
    blocking(move || {
        let mut out = String::new();
        let ctx = PropCtx {
            caller: dctx.caller(),
            locks: None,
            dead: None,
            hidden_personal: None,
        };
        for name in paths::PER_USER_FILES {
            if let Some(res) = inner
                .roots
                .lookup(&format!("/{}/{name}", paths::READER_ROOT), ctx.caller)
            {
                propfind::write_response(&mut out, &res, &Mode::AllProp, &ctx);
            }
        }
        let history = format!("/{}/{}", paths::READER_ROOT, reader_tree::HISTORY_DIR);
        if let Some(res) = inner.roots.lookup(&history, ctx.caller)
            && res.is_collection()
        {
            propfind::walk(
                &inner.roots,
                &res,
                None,
                &Mode::AllProp,
                &ctx,
                &mut |chunk| out.push_str(chunk),
            );
        }
        out
    })
    .await
}

/// The caller's own corrections (D27) below a cached collection, appended to its
/// shared listing: the cloud-root owner's physical ones the cache set aside, or anyone
/// else's overlays, each beside its archive. Anonymous has none.
async fn injected_personal(
    inner: &Arc<Inner>,
    req: &Req,
    res: &Resource,
    hit: &crate::cache::Hit,
) -> DavResult<String> {
    let Some(caller) = req.caller() else {
        return Ok(String::new());
    };
    if caller.owns_cloud_root {
        return Ok(hit.owner_tail().to_string());
    }
    let folder_rel = match res.phys() {
        Some(phys) => match inner.roots.library_rel(phys) {
            Some(rel) => rel,
            None => return Ok(String::new()),
        },
        // `/` and `/mokuro-reader` list the whole library.
        None => String::new(),
    };
    let inner = inner.clone();
    let dctx = req.ctx.clone();
    blocking(move || {
        let ctx = PropCtx {
            caller: dctx.caller(),
            locks: None,
            dead: None,
            hidden_personal: None,
        };
        let mut out = String::new();
        if let Some(caller) = ctx.caller {
            for res in inner.roots.overlays_under(caller, &folder_rel) {
                propfind::write_response(&mut out, &res, &Mode::AllProp, &ctx);
            }
        }
        out
    })
    .await
}

/// A GET's failed open or first read: a gone (or locally unreadable) file is a 404, a
/// file changed behind the index a 503 retry ([`DavError::from_read_io`], never 412).
fn read_error(e: std::io::Error) -> DavError {
    match bunko_store::classify(&e) {
        bunko_store::ErrorClass::NotFound | bunko_store::ErrorClass::Other => {
            DavError::new(StatusCode::NOT_FOUND, e.to_string())
        }
        _ => DavError::from_read_io(&e),
    }
}

pub(crate) async fn get(inner: &Arc<Inner>, req: &Req) -> DavResult<Resp> {
    let head = req.method == Method::HEAD;
    if req.content_length() != 0 {
        return Err(DavError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "The server does not handle any body content.",
        ));
    }
    if req.header("depth").is_some_and(|d| d != "0") {
        return Err(DavError::new(
            StatusCode::BAD_REQUEST,
            "Only Depth: 0 supported.",
        ));
    }
    let res = lookup(inner, &req.path, req)
        .await?
        .ok_or_else(|| DavError::new(StatusCode::NOT_FOUND, &req.path))?;
    let Resource::File { phys, kind, .. } = &res else {
        return Err(DavError::new(
            StatusCode::FORBIDDEN,
            "Directory browsing is not enabled.",
        ));
    };
    // What the path resolved to (a correction layer's name alone does not say whether
    // it is the caller's overlay or a library primary), for the request accounting.
    let info = DavInfo {
        target: match kind {
            FileKind::Library => DavTarget::Library,
            FileKind::Progress => DavTarget::Progress,
            FileKind::Personal => DavTarget::Personal,
        },
        rel: crate::analytics::dav_target_of(&req.path).1,
        depth: None,
    };
    let annotate = |resp: &mut Resp, served: Option<Served>| {
        resp.extensions_mut().insert(info.clone());
        if let Some(served) = served {
            resp.extensions_mut().insert(served);
        }
    };
    // Open first and describe what was opened: the validators match the bytes sent even
    // if an upload renames a new file into place meanwhile.
    // A remote store's file has no local handle: its body streams from the store (spool
    // or pinned ranged reads, spec §10), memory flat, with the same validators.
    let file = bunko_store::afs::File::open(phys)
        .await
        .map_err(read_error)?;
    let stat = Stat::from_meta(&file.metadata().await?);
    let res = match res {
        Resource::File {
            path, phys, kind, ..
        } => Resource::File {
            path,
            phys,
            stat,
            kind,
        },
        other => other,
    };
    let etag = stat.file_etag();
    let last_modified = stat.mtime_int();
    if let Err(e) = eval_if(inner, req, &res) {
        if e.status == StatusCode::NOT_MODIFIED {
            // RFC 9110 15.4.5: a 304 carries the validators (0.5.2 sent none).
            let mut resp = response::empty(StatusCode::NOT_MODIFIED);
            response::set_header(&mut resp, "etag", &format!("\"{etag}\""));
            response::set_header(&mut resp, "last-modified", &stat.last_modified_http());
            let served = Served {
                size: stat.size,
                len: 0,
                accel: false,
            };
            annotate(&mut resp, Some(served));
            return Ok(resp);
        }
        return Err(e);
    }
    let name = paths::file_name(res.phys().unwrap_or(std::path::Path::new("")));
    let mut resp = Response::new(Body::empty());
    let headers = resp.headers_mut();
    headers.insert(
        LAST_MODIFIED,
        HeaderValue::from_str(&stat.last_modified_http())
            .map_err(|_| DavError::status(crate::SERVER_ERROR))?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type(&name)));
    headers.insert(
        ETAG,
        HeaderValue::from_str(&format!("\"{etag}\""))
            .map_err(|_| DavError::status(crate::SERVER_ERROR))?,
    );

    if let Some(accel) = accel_path(inner, req, &res) {
        // What nginx will send: the range it honours (an `If-Range` that no longer
        // holds gets the whole file), else the whole file. A range past the end is
        // answered 416 here, as without offload: nginx would send no body, and the
        // accounting must not count one.
        let honoured = req
            .header("if-range")
            .is_none_or(|cond| conditional::if_range_holds(cond, &etag, last_modified));
        let len = match req.header("range") {
            Some(value) if stat.size > 0 && !head && honoured => {
                match conditional::parse_range(value, stat.size) {
                    Ok(RangeOutcome::Partial { start, end }) => end - start + 1,
                    Ok(RangeOutcome::Full) => stat.size,
                    Err(e) => {
                        let mut r =
                            DavError::new(StatusCode::RANGE_NOT_SATISFIABLE, "").into_response();
                        response::set_header(&mut r, "content-range", &e.context);
                        annotate(&mut r, None);
                        return Ok(r);
                    }
                }
            }
            _ if head => 0,
            _ => stat.size,
        };
        // nginx serves the bytes (and any Range); Content-Length 0 keeps the upstream
        // connection reusable.
        let headers = resp.headers_mut();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
        response::set_header(&mut resp, "x-accel-redirect", &accel);
        let served = Served {
            size: stat.size,
            len,
            accel: true,
        };
        annotate(&mut resp, Some(served));
        return Ok(resp);
    }
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    let size = stat.size;
    let mut range = RangeOutcome::Full;
    if let Some(value) = req.header("range")
        && size > 0
    {
        let honoured = match req.header("if-range") {
            Some(cond) => conditional::if_range_holds(cond, &etag, last_modified),
            None => true,
        };
        if honoured {
            range = match conditional::parse_range(value, size) {
                Ok(r) => r,
                Err(e) => {
                    let mut r =
                        DavError::new(StatusCode::RANGE_NOT_SATISFIABLE, "").into_response();
                    response::set_header(&mut r, "content-range", &e.context);
                    annotate(&mut r, None);
                    return Ok(r);
                }
            };
        }
    }
    let (start, len) = match range {
        RangeOutcome::Full => (0, size),
        RangeOutcome::Partial { start, end } => {
            *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
            let v = format!("bytes {start}-{end}/{size}");
            resp.headers_mut().insert(
                CONTENT_RANGE,
                HeaderValue::from_str(&v).map_err(|_| DavError::status(crate::SERVER_ERROR))?,
            );
            (start, end - start + 1)
        }
    };
    resp.headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from(len));
    annotate(
        &mut resp,
        Some(Served {
            size,
            len: if head { 0 } else { len },
            accel: false,
        }),
    );
    if head {
        return Ok(resp);
    }
    match file {
        bunko_store::afs::File::Tokio(mut file) => {
            if start > 0 {
                file.seek(SeekFrom::Start(start)).await?;
            }
            let stream = ReaderStream::with_capacity(file.take(len), 64 * 1024);
            *resp.body_mut() = Body::from_stream(stream);
        }
        remote => {
            use futures_util::StreamExt;
            let mut stream = remote.into_stream(start, len);
            // A remote file over the blob cache streams from ranged reads, the first of
            // them only once the body is polled: make it before the headers go out, so a
            // file changed behind the index answers 503 (as a small one does at open)
            // instead of a 200 whose body breaks off. Only this chunk (at most
            // `STREAM_CHUNK`) is held; a change DURING the transfer still ends the body.
            let first = match stream.next().await {
                Some(Ok(chunk)) => Some(chunk),
                Some(Err(e)) => return Err(read_error(e)),
                None => None,
            };
            let head = futures_util::stream::iter(first.map(Ok::<_, std::io::Error>));
            *resp.body_mut() = Body::from_stream(head.chain(stream));
        }
    }
    Ok(resp)
}
