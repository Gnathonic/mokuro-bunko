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
use crate::propfind::{self, Mode, PropCtx};
use crate::resource::{FileKind, Resource, Stat, content_type};
use crate::response::{self, DavError, DavResult, Resp};
use crate::xml::{MULTISTATUS_CLOSE, MULTISTATUS_OPEN};
use crate::{Inner, paths};

const ALLOW_COLLECTION: &str =
    "OPTIONS, HEAD, GET, PROPFIND, DELETE, COPY, MOVE, PROPPATCH, LOCK, UNLOCK";
const ALLOW_FILE: &str =
    "OPTIONS, HEAD, GET, PROPFIND, PUT, DELETE, COPY, MOVE, PROPPATCH, LOCK, UNLOCK";
const ALLOW_UNMAPPED: &str = "OPTIONS, PUT, MKCOL";

/// Would a GET of this file be offloaded to nginx (spec §8.10)?
fn accel_path(inner: &Inner, req: &Req, res: &Resource) -> Option<String> {
    if !req.ctx.nginx_accel {
        return None;
    }
    match res {
        Resource::File {
            phys,
            kind: FileKind::Library,
            ..
        } => {
            let rel = inner.roots.library_rel(phys)?;
            Some(format!("/internal-library/{}", paths::quote_path(&rel)))
        }
        _ => None,
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
    let res = lookup(inner, &req.path, req.username()).await?;
    match &res {
        Some(r) if r.is_collection() => response::set_header(&mut resp, "allow", ALLOW_COLLECTION),
        Some(r) => {
            response::set_header(&mut resp, "allow", ALLOW_FILE);
            if accel_path(inner, req, r).is_none() {
                response::set_header(&mut resp, "accept-ranges", "bytes");
            }
        }
        None => {
            let parent = match paths::uri_parent(&req.path) {
                Some(p) => lookup(inner, &p, req.username()).await?,
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
    let raw_depth = req.header("depth").map(str::to_string);
    let depth = match raw_depth.as_deref().map(str::to_ascii_lowercase).as_deref() {
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
    let res = lookup(inner, &req.path, req.username())
        .await?
        .ok_or_else(|| DavError::new(StatusCode::NOT_FOUND, &req.path))?;
    eval_if(inner, req, &res)?;
    let body = read_body(body, XML_BODY_LIMIT).await?;

    // Depth: infinity (the exact header) is answered from the shared cache, allprop.
    let norm = paths::normalize(&req.path);
    if raw_depth.as_deref() == Some("infinity")
        && !matches!(paths::classify(&norm), paths::Target::Progress(_))
    {
        if let Some(hit) = inner.cache.get(&norm).await {
            let tail = injected_progress(inner, req, &norm).await?;
            let gzip = req
                .header("accept-encoding")
                .is_some_and(|v| v.contains("gzip"));
            return Ok(crate::cache::respond(hit, &tail, gzip));
        }
        return Err(DavError::new(StatusCode::NOT_FOUND, &req.path));
    }

    let mode =
        propfind::parse_mode(&body).ok_or_else(|| DavError::status(StatusCode::BAD_REQUEST))?;
    let inner2 = inner.clone();
    let user = req.username().map(str::to_string);
    let xml = blocking(move || {
        let ctx = PropCtx {
            username: user.as_deref(),
            locks: Some(&inner2.locks),
            dead: Some(&inner2.dead),
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

/// The caller's own progress files, appended to a shared `/` or `/mokuro-reader` listing
/// (spec §10.1 item 4).
async fn injected_progress(inner: &Arc<Inner>, req: &Req, norm: &str) -> DavResult<String> {
    let Some(user) = req.username().map(str::to_string) else {
        return Ok(String::new());
    };
    if !matches!(
        paths::classify(norm),
        paths::Target::Root | paths::Target::ReaderRoot
    ) {
        return Ok(String::new());
    }
    let inner = inner.clone();
    blocking(move || {
        let mut out = String::new();
        let ctx = PropCtx {
            username: Some(&user),
            locks: None,
            dead: None,
        };
        for name in paths::PER_USER_FILES {
            if let Some(res) = inner
                .roots
                .lookup(&format!("/{}/{name}", paths::READER_ROOT), Some(&user))
            {
                propfind::write_response(&mut out, &res, &Mode::AllProp, &ctx);
            }
        }
        out
    })
    .await
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
    let res = lookup(inner, &req.path, req.username())
        .await?
        .ok_or_else(|| DavError::new(StatusCode::NOT_FOUND, &req.path))?;
    let Resource::File { phys, .. } = &res else {
        return Err(DavError::new(
            StatusCode::FORBIDDEN,
            "Directory browsing is not enabled.",
        ));
    };
    // Open first and describe what was opened: the validators match the bytes sent even
    // if an upload renames a new file into place meanwhile.
    let mut file = tokio::fs::File::open(phys)
        .await
        .map_err(|e| DavError::new(StatusCode::NOT_FOUND, e.to_string()))?;
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
        // nginx serves the bytes (and any Range); Content-Length 0 keeps the upstream
        // connection reusable.
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
        response::set_header(&mut resp, "x-accel-redirect", &accel);
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
    if head {
        return Ok(resp);
    }
    if start > 0 {
        file.seek(SeekFrom::Start(start)).await?;
    }
    let stream = ReaderStream::with_capacity(file.take(len), 64 * 1024);
    *resp.body_mut() = Body::from_stream(stream);
    Ok(resp)
}
