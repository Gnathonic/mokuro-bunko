//! LOCK, UNLOCK (in-memory class-2 locks, spec §8.3.9) and PROPPATCH (in-memory dead
//! properties, spec §8.3.8).

use std::sync::Arc;

use axum::body::Body;
use http::StatusCode;

use super::{
    Req, XML_BODY_LIMIT, blocking, check_dav_locks, eval_if, is_virtual_root, lookup, read_body,
};
use crate::locks::{self, AcquireError, Scope};
use crate::propfind::{self, resource_key};
use crate::resource::Resource;
use crate::response::{self, DavError, DavResult, Resp};
use crate::xml::{self, DAV_NS};
use crate::{Inner, paths};

fn lockdiscovery_body(activelocks: &str) -> Vec<u8> {
    format!(
        "{}<D:prop xmlns:D=\"DAV:\"><D:lockdiscovery>{activelocks}</D:lockdiscovery></D:prop>",
        xml::XML_DECLARATION
    )
    .into_bytes()
}

/// The lock key and href for a URL that may not exist yet (lock-null).
fn key_for_unmapped(req: &Req) -> (String, String) {
    let norm = paths::normalize(&req.path);
    match paths::classify(&norm) {
        paths::Target::Progress(_) => (
            format!("{norm}\0{}", req.ctx.principal()),
            paths::href(&norm, false),
        ),
        _ => (norm.clone(), paths::href(&norm, false)),
    }
}

pub(crate) async fn lock(inner: &Arc<Inner>, req: &Req, body: Body) -> DavResult<Resp> {
    if is_virtual_root(&req.path) {
        return Err(DavError::new(
            StatusCode::FORBIDDEN,
            "The library root cannot be locked.",
        ));
    }
    let infinite = match req.header("depth") {
        None | Some("infinity") => true,
        Some("0") => false,
        Some(_) => {
            return Err(DavError::new(
                StatusCode::BAD_REQUEST,
                "Expected Depth: 'infinity' or '0'.",
            ));
        }
    };
    let user = req.username();
    let res = lookup(inner, &req.path, user).await?;
    if let Some(r) = &res {
        eval_if(inner, req, r)?;
    }
    let timeout = locks::parse_timeout(req.header("timeout"));
    let body = read_body(body, XML_BODY_LIMIT).await?;

    if body.iter().all(u8::is_ascii_whitespace) {
        // Refresh: exactly one submitted token, which must lock this URL.
        let key = match &res {
            Some(r) => resource_key(r, user),
            None => key_for_unmapped(req).0,
        };
        let tokens = req.if_tokens();
        if tokens.len() != 1 {
            return Err(DavError::new(
                StatusCode::BAD_REQUEST,
                "Expected a lock token (only one lock may be refreshed at a time).",
            ));
        }
        if !inner.locks.is_locked_by_token(&key, &tokens[0]) {
            return Err(DavError::new(
                StatusCode::PRECONDITION_FAILED,
                "Lock token does not match URL.",
            ));
        }
        let lock = inner
            .locks
            .refresh(&tokens[0], timeout)
            .ok_or_else(|| DavError::status(StatusCode::PRECONDITION_FAILED))?;
        return Ok(response::bytes_response(
            StatusCode::OK,
            "application/xml; charset=utf-8",
            lockdiscovery_body(&propfind::activelock(&lock)),
        ));
    }

    let info = xml::parse(&body).map_err(|_| DavError::status(StatusCode::BAD_REQUEST))?;
    if !info.is_dav("lockinfo") {
        return Err(DavError::status(StatusCode::BAD_REQUEST));
    }
    let mut scope = None;
    let mut write_type = false;
    let mut owner = String::new();
    for child in &info.children {
        if child.is_dav("lockscope") {
            scope = child.children.first().and_then(|c| {
                if c.is_dav("exclusive") {
                    Some(Scope::Exclusive)
                } else if c.is_dav("shared") {
                    Some(Scope::Shared)
                } else {
                    None
                }
            });
        } else if child.is_dav("locktype") {
            write_type = child.children.first().is_some_and(|c| c.is_dav("write"));
        } else if child.is_dav("owner") {
            let mut s = String::from("<D:owner>");
            s.push_str(&xml::escape_text(&child.text));
            for c in &child.children {
                c.to_xml(&mut s);
            }
            s.push_str("</D:owner>");
            owner = s;
        } else {
            return Err(DavError::new(
                StatusCode::BAD_REQUEST,
                format!("Invalid node '{}'.", child.clark()),
            ));
        }
    }
    let scope = scope
        .ok_or_else(|| DavError::new(StatusCode::BAD_REQUEST, "Missing or invalid lockscope."))?;
    if !write_type {
        return Err(DavError::new(
            StatusCode::BAD_REQUEST,
            "Missing or invalid locktype.",
        ));
    }

    let (key, href, created) = match &res {
        Some(r) => (resource_key(r, user), r.href(), false),
        None => {
            // Lock-null: the parent must be a collection the name can live in; nothing is
            // created on disk (the first PUT creates the file).
            let parent = match paths::uri_parent(&req.path) {
                Some(p) => lookup(inner, &p, user).await?,
                None => None,
            };
            let Some(parent) = parent.filter(Resource::is_collection) else {
                return Err(DavError::new(
                    StatusCode::CONFLICT,
                    "LOCK-0 parent must be a collection",
                ));
            };
            let name = paths::uri_name(&req.path).to_string();
            let inner2 = inner.clone();
            let user2 = user.map(str::to_string);
            if blocking(move || inner2.roots.member_path(&parent, &name, user2.as_deref()))
                .await?
                .is_none()
            {
                return Err(DavError::new(StatusCode::FORBIDDEN, "Forbidden"));
            }
            let (key, href) = key_for_unmapped(req);
            (key, href, true)
        }
    };
    let lock = match inner.locks.acquire(
        &key,
        &href,
        scope,
        infinite,
        owner,
        timeout,
        req.ctx.principal(),
    ) {
        Ok(l) => l,
        Err(AcquireError::Conflict(hrefs)) => return Err(DavError::locked_by(hrefs)),
        Err(AcquireError::TooMany) => {
            return Err(DavError::new(
                StatusCode::INSUFFICIENT_STORAGE,
                "Too many locks.",
            ));
        }
    };
    let activelocks: String = inner
        .locks
        .locks_on(&key)
        .iter()
        .map(propfind::activelock)
        .collect();
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let mut resp = response::bytes_response(
        status,
        "application/xml; charset=utf-8",
        lockdiscovery_body(&activelocks),
    );
    response::set_header(&mut resp, "lock-token", &format!("<{}>", lock.token));
    Ok(resp)
}

pub(crate) async fn unlock(inner: &Arc<Inner>, req: &Req) -> DavResult<Resp> {
    if req.content_length() != 0 {
        return Err(DavError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "The server does not handle any body content.",
        ));
    }
    let user = req.username();
    let res = lookup(inner, &req.path, user).await?;
    let key = match &res {
        Some(r) => resource_key(r, user),
        None => key_for_unmapped(req).0,
    };
    let token = req.header("lock-token").map(|t| {
        t.trim()
            .trim_start_matches('<')
            .trim_end_matches('>')
            .to_string()
    });
    // A lock-null resource (no file yet) can still be unlocked.
    if res.is_none() && inner.locks.locks_on(&key).is_empty() {
        return Err(DavError::new(StatusCode::NOT_FOUND, &req.path));
    }
    let Some(token) = token else {
        return Err(DavError::new(
            StatusCode::BAD_REQUEST,
            "Missing lock token.",
        ));
    };
    if let Some(r) = &res {
        eval_if(inner, req, r)?;
    }
    if !inner.locks.is_locked_by_token(&key, &token) {
        return Err(DavError::new(
            StatusCode::CONFLICT,
            "Resource is not locked by token.",
        ));
    }
    // 0.5.2's principal was "" for everyone, so anyone could unlock anyone's lock.
    if inner
        .locks
        .get(&token)
        .is_some_and(|l| l.principal != req.ctx.principal())
    {
        return Err(DavError::new(
            StatusCode::FORBIDDEN,
            "Token was created by another user.",
        ));
    }
    inner.locks.release(&token);
    Ok(response::empty(StatusCode::NO_CONTENT))
}

pub(crate) async fn proppatch(inner: &Arc<Inner>, req: &Req, body: Body) -> DavResult<Resp> {
    if is_virtual_root(&req.path) {
        return Err(DavError::new(
            StatusCode::FORBIDDEN,
            "The library root's properties cannot be changed.",
        ));
    }
    if req.header("depth").is_some_and(|d| d != "0") {
        return Err(DavError::new(StatusCode::BAD_REQUEST, "Depth must be '0'."));
    }
    let user = req.username();
    let res = lookup(inner, &req.path, user)
        .await?
        .ok_or_else(|| DavError::new(StatusCode::NOT_FOUND, &req.path))?;
    eval_if(inner, req, &res)?;
    let key = resource_key(&res, user);
    check_dav_locks(inner, req, &key, false)?;
    let body = read_body(body, XML_BODY_LIMIT).await?;
    let root = xml::parse(&body).map_err(|_| DavError::status(StatusCode::BAD_REQUEST))?;
    if !root.is_dav("propertyupdate") {
        return Err(DavError::status(StatusCode::BAD_REQUEST));
    }
    // (element, Some(value) to set / None to remove)
    let mut updates: Vec<(xml::Element, bool)> = Vec::new();
    for op in &root.children {
        let set = if op.is_dav("set") {
            true
        } else if op.is_dav("remove") {
            false
        } else {
            return Err(DavError::new(
                StatusCode::BAD_REQUEST,
                "Unknown tag (expected 'set' or 'remove').",
            ));
        };
        for prop in &op.children {
            if !prop.is_dav("prop") {
                return Err(DavError::new(
                    StatusCode::BAD_REQUEST,
                    "Unknown tag (expected 'prop').",
                ));
            }
            for p in &prop.children {
                if !set && !p.children.is_empty() {
                    return Err(DavError::new(
                        StatusCode::BAD_REQUEST,
                        "prop element must be empty for 'remove'.",
                    ));
                }
                updates.push((p.clone(), set));
            }
        }
    }
    // Dry run: live DAV: properties are protected; the store must have room.
    let verdicts: Vec<Option<StatusCode>> = updates
        .iter()
        .map(|(p, set)| {
            if p.ns.as_deref() == Some(DAV_NS) {
                Some(StatusCode::FORBIDDEN)
            } else if *set && !inner.dead.can_hold(&key) {
                Some(StatusCode::INSUFFICIENT_STORAGE)
            } else {
                None
            }
        })
        .collect();
    let failed = verdicts.iter().any(Option::is_some);
    let mut groups: Vec<(StatusCode, String)> = Vec::new();
    for ((p, set), verdict) in updates.iter().zip(&verdicts) {
        let status = match verdict {
            Some(s) => *s,
            None if failed => StatusCode::FAILED_DEPENDENCY,
            None => {
                let value = set.then(|| p.clone());
                if inner.dead.set(&key, p.ns.as_deref(), &p.local, value) {
                    StatusCode::OK
                } else {
                    StatusCode::INSUFFICIENT_STORAGE
                }
            }
        };
        let tag = xml::prop_open_tag(p.ns.as_deref(), &p.local, true);
        match groups.iter_mut().find(|(s, _)| *s == status) {
            Some((_, xmls)) => xmls.push_str(&tag),
            None => groups.push((status, tag)),
        }
    }
    let mut out = String::from(xml::MULTISTATUS_OPEN);
    out.push_str("<D:response><D:href>");
    out.push_str(&xml::escape_text(&res.href()));
    out.push_str("</D:href>");
    for (status, props) in &groups {
        propfind::push_propstat(&mut out, props, &response::status_text(*status));
    }
    out.push_str("</D:response>");
    out.push_str(xml::MULTISTATUS_CLOSE);
    Ok(response::bytes_response(
        StatusCode::MULTI_STATUS,
        "application/xml; charset=utf-8",
        out.into_bytes(),
    ))
}
