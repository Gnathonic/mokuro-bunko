//! PUT: staged, verified, atomically published, answered with a verdict (spec §8.3.4,
//! §8.8, §9).

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderValue};
use http::{Response, StatusCode};
use http_body_util::BodyExt;

/// Longest silence tolerated in the middle of an upload body.
const BODY_STALL: std::time::Duration = std::time::Duration::from_secs(120);

use super::{
    Effects, Req, audit, check_dav_locks, eval_if, file_audit_target, forget_ocr_records, is_cbz,
    lookup, write_lock_conflict,
};
use crate::propfind::resource_key;
use crate::resource::{Resource, Stat};
use crate::response::{self, DavError, Resp};
use crate::upload::{self, Rejection, Staging, Stored};
use crate::{Inner, paths};

enum PutError {
    /// The writer's own verdict (truncated, corrupted-in-transit, archive-*, disk-full,
    /// server-error).
    Rejected(Rejection),
    /// A protocol answer (405, 409, 412, 423, ...).
    Dav(DavError),
}

impl From<DavError> for PutError {
    fn from(e: DavError) -> Self {
        PutError::Dav(e)
    }
}

impl From<Rejection> for PutError {
    fn from(r: Rejection) -> Self {
        PutError::Rejected(r)
    }
}

pub(crate) async fn put(inner: &Arc<Inner>, req: &Req, body: Body) -> Resp {
    let verdict = crate::put_gives_verdict(&req.path);
    let mut resp = match put_inner(inner, req, body).await {
        Ok(resp) => resp,
        Err(PutError::Rejected(rej)) if verdict => {
            failure_response(rej.status, rej.reason, &rej.detail, rej.retry)
        }
        Err(PutError::Rejected(rej)) => response::status_page(
            StatusCode::from_u16(rej.status).unwrap_or(crate::SERVER_ERROR),
            &rej.detail,
        ),
        Err(PutError::Dav(e)) if verdict => {
            let (status, reason, detail, retry) = upload::failure_for_status(e.status.as_u16());
            failure_response(status, reason, &detail, retry)
        }
        Err(PutError::Dav(e)) => e.into_response(),
    };
    if req.path.to_lowercase().ends_with(".cbz") {
        response::set_header(&mut resp, "x-mokuro-put", "verified");
    }
    resp
}

fn failure_response(status: u16, reason: &str, detail: &str, retry: bool) -> Resp {
    let body = upload::failure_json(reason, detail, retry).into_bytes();
    let len = body.len();
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(crate::SERVER_ERROR);
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp.headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from(len));
    resp
}

/// See [`crate::put_refusal_verdict`].
pub(crate) fn refusal_verdict(path: &str, mut resp: Resp) -> Resp {
    let cbz = path.to_lowercase().ends_with(".cbz");
    if crate::put_gives_verdict(path) && !resp.status().is_success() {
        let (status, reason, detail, retry) = upload::failure_for_status(resp.status().as_u16());
        let body = upload::failure_json(reason, &detail, retry).into_bytes();
        let len = body.len();
        let mut headers = std::mem::take(resp.headers_mut());
        headers.remove(CONTENT_TYPE);
        headers.remove(CONTENT_LENGTH);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(CONTENT_LENGTH, HeaderValue::from(len));
        let mut out = Response::new(Body::from(body));
        *out.status_mut() = StatusCode::from_u16(status).unwrap_or(crate::SERVER_ERROR);
        *out.headers_mut() = headers;
        resp = out;
    }
    if cbz {
        response::set_header(&mut resp, "x-mokuro-put", "verified");
    }
    resp
}

/// `Content-Length` as the expected body size (`None`: absent, blank, unparsable).
fn expected_size(req: &Req) -> Option<u64> {
    req.header("content-length")
        .and_then(|v| v.trim().parse::<u64>().ok())
}

async fn put_inner(inner: &Arc<Inner>, req: &Req, body: Body) -> Result<Resp, PutError> {
    if req.headers.contains_key("content-encoding") {
        return Err(DavError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Content-Encoding header is not supported.",
        )
        .into());
    }
    if req.headers.contains_key("content-range") {
        return Err(DavError::new(
            StatusCode::BAD_REQUEST,
            "Content-Range header is not supported.",
        )
        .into());
    }
    let user = req.username();
    let res = lookup(inner, &req.path, user).await?;
    if res.as_ref().is_some_and(Resource::is_collection) {
        return Err(
            DavError::new(StatusCode::METHOD_NOT_ALLOWED, "Cannot PUT to a collection").into(),
        );
    }
    let parent = match paths::uri_parent(&req.path) {
        Some(p) => lookup(inner, &p, user).await?,
        None => None,
    };
    let Some(parent) = parent.filter(Resource::is_collection) else {
        return Err(DavError::new(StatusCode::CONFLICT, "PUT parent must be a collection").into());
    };

    let dest = match &res {
        Some(existing) => {
            eval_if(inner, req, existing)?;
            check_dav_locks(inner, req, &resource_key(existing, user), false)?;
            existing
                .phys()
                .map(Path::to_path_buf)
                .ok_or_else(|| DavError::status(StatusCode::METHOD_NOT_ALLOWED))?
        }
        None => {
            // RFC 9110: If-Match fails when there is no current representation.
            if req.headers.contains_key("if-match") {
                return Err(DavError::new(
                    StatusCode::PRECONDITION_FAILED,
                    "If-Match header condition failed",
                )
                .into());
            }
            check_dav_locks(inner, req, &resource_key(&parent, user), false)?;
            let name = paths::uri_name(&req.path).to_string();
            let inner2 = inner.clone();
            let (parent2, user2) = (parent.clone(), user.map(str::to_string));
            let (phys, kind) = super::blocking(move || {
                inner2.roots.member_path(&parent2, &name, user2.as_deref())
            })
            .await?
            .ok_or_else(|| DavError::new(StatusCode::FORBIDDEN, "Forbidden"))?;
            // A lock-null resource (LOCK before the first PUT) is locked by its own URL.
            let pseudo = Resource::File {
                path: paths::normalize(&req.path),
                phys: phys.clone(),
                stat: Stat {
                    size: 0,
                    mtime_secs: 0,
                    mtime_nanos: 0,
                    ctime_secs: 0,
                    is_dir: false,
                },
                kind,
            };
            check_dav_locks(inner, req, &resource_key(&pseudo, user), false)?;
            phys
        }
    };

    let Some(_guard) = inner.write_locks.try_lock(&dest) else {
        return Err(write_lock_conflict(
            req,
            file_audit_target(inner, &req.path, &dest),
            "write",
            None,
        )
        .into());
    };
    let existed_before = tokio::fs::metadata(&dest).await.is_ok();
    let expected = expected_size(req);
    let digest = upload::parse_content_digest(req.header("content-digest"));
    let mut staging = Staging::create(&dest, expected, digest).await?;
    if existed_before && is_ocr_sidecar(&dest) {
        // A client re-sending an OCR file unchanged (a backup, a re-upload) edits nothing:
        // the file, its provenance and its history stay as they were.
        staging.keep_if_identical();
    }
    let mut body = body;
    loop {
        // A client that stops sending for two minutes is gone: drop the upload rather
        // than hold the connection, the staging file and the path lock forever.
        let next = match tokio::time::timeout(BODY_STALL, body.frame()).await {
            Ok(next) => next.map(|r| r.map_err(|_| ())),
            Err(_) => Some(Err(())),
        };
        match next {
            None => break,
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    staging.write(&data).await?;
                }
            }
            Some(Err(_)) => {
                // The client went away mid-body: nothing is published.
                let got = staging.written();
                let detail = match expected {
                    Some(m) => format!("Received {got} of {m} bytes; the upload was cut short."),
                    None => format!("Received {got} bytes; the upload was cut short."),
                };
                return Err(Rejection {
                    status: 422,
                    reason: "truncated",
                    detail,
                    retry: true,
                }
                .into());
            }
        }
    }
    let stored: Stored = staging.commit(&inner.damage).await?;

    // Committed: the rest reports it and never fails the request.
    let (inner2, req2, dest2) = (inner.clone(), req.clone(), dest.clone());
    let unchanged = stored.unchanged;
    let follow_up =
        super::blocking(move || after_commit(&inner2, &req2, &dest2, existed_before, unchanged))
            .await
            .unwrap_or(None);

    let etag = tokio::fs::metadata(&dest)
        .await
        .map(|m| Stat::from_meta(&m).file_etag())
        .ok();
    let mut resp = if existed_before {
        response::empty(StatusCode::NO_CONTENT)
    } else {
        response::status_page(StatusCode::CREATED, "")
    };
    if let Some(etag) = etag {
        response::set_header(&mut resp, "etag", &format!("\"{etag}\""));
    }
    if crate::put_gives_verdict(&req.path) {
        response::set_header(&mut resp, "x-mokuro-upload", stored.verdict);
        response::set_header(&mut resp, "x-mokuro-size", &stored.size.to_string());
        if let Some(algo) = stored.digest_verified {
            response::set_header(&mut resp, "x-mokuro-digest-verified", algo);
        }
    }
    if let Some(f) = follow_up {
        response::set_header(&mut resp, "x-mokuro-manifest", &f.manifest);
        response::set_header(
            &mut resp,
            "x-mokuro-recheck-after",
            &f.recheck_after.to_string(),
        );
    }
    Ok(resp)
}

/// A `.mokuro` / `.mokuro.gz` OCR file.
fn is_ocr_sidecar(path: &Path) -> bool {
    let name = paths::file_name(path).to_lowercase();
    name.ends_with(".mokuro") || name.ends_with(".mokuro.gz")
}

/// 0.5.2 `_on_write_committed` + `UploadMiddleware._archive_arrived`. Blocking.
fn after_commit(
    inner: &Inner,
    req: &Req,
    dest: &Path,
    existed_before: bool,
    unchanged: bool,
) -> Option<crate::PutFollowUp> {
    let rel = inner.roots.library_rel(dest);
    if unchanged {
        // Nothing on disk changed: only the request is on record (as an `edit` the
        // generation upgrade does not count as one, `generation-upgrade.md` §3).
        audit(
            req,
            "edit",
            file_audit_target(inner, &req.path, dest),
            Some(serde_json::json!({ "existed_before": true, "unchanged": true })),
        );
        return None;
    }
    if let (Some(rel), Some(actor)) = (rel.as_deref(), req.username()) {
        req.ctx
            .hooks
            .record_volume_upload(rel, actor, existed_before);
    }
    forget_ocr_records(req, rel.as_deref(), false);
    audit(
        req,
        if existed_before { "edit" } else { "upload" },
        file_audit_target(inner, &req.path, dest),
        Some(serde_json::json!({ "existed_before": existed_before })),
    );
    let mut effects = Effects::default();
    effects.note_arrived(inner, dest);
    effects.touch(&req.path, Some(dest));
    let arrived = !effects.arrived.is_empty();
    effects.fire(inner, req);
    if !arrived {
        return None;
    }
    let rel = rel?;
    let series = match rel.rfind('/') {
        Some(i) => rel[..i].to_string(),
        None => ".".to_string(),
    };
    let volume = paths::py_stem(&paths::file_name(dest)).to_string();
    if !is_cbz(dest) {
        return None;
    }
    req.ctx.hooks.put_follow_up(dest, &series, &volume)
}
