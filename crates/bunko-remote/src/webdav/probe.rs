//! The capability probe run at connect (spec §6 "WebDAV gotchas").
//!
//! Everything it writes lives in `.bunko-probe-d/` inside the folder and is removed at
//! the end. A missing folder leaves the write probes for later: `create_root` runs the
//! probe again once the folder exists.

use super::{WebDavBackend, xml};
use crate::backend::{BErr, HashKind};

/// What a WebDAV server can do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    /// The folder URL exists (otherwise nothing below was probed).
    pub folder_exists: bool,
    /// The probe could create files there.
    pub writable: bool,
    /// `X-Mokuro-Put` on OPTIONS: a Bunko server.
    pub bunko: bool,
    /// PROPFIND `Depth: infinity` answered 207.
    pub depth_infinity: bool,
    /// `Range: bytes=0-0` answered 206.
    pub range: bool,
    /// Files carry strong ETags.
    pub strong_etags: bool,
    /// A wrong `If-Match` on PUT gives 412 and the right one succeeds.
    pub if_match: bool,
    /// `If-None-Match: *` on PUT gives 412 for an existing name and succeeds for a new one.
    pub if_none_match: bool,
    /// `Some(false)` when `.bunko-probe-aa` found `.bunko-probe-Aa`.
    pub case_sensitive: Option<bool>,
    /// A create and an in-place write two levels down both change the folder's ETag.
    pub nested_propagation: bool,
    /// The hash `oc:checksums` reported for the probe file.
    pub hash_kind: Option<HashKind>,
}

const DIR: &str = ".bunko-probe-d";

pub(super) async fn run(b: &WebDavBackend) -> Result<Probe, BErr> {
    let mut p = Probe::default();
    let opts = b.send("OPTIONS", &b.url("", true), &[], None).await?;
    p.bunko = opts.headers().contains_key("x-mokuro-put");
    if p.bunko && !b.allow_bunko {
        // Before anything is written there.
        return Err(BErr::Fatal(super::BUNKO_BACKEND_REFUSED.into()));
    }
    match b.stat_node("", true).await? {
        (Some(n), _) if n.is_dir => p.folder_exists = true,
        _ => return Ok(p),
    }
    let mut status = mkcol(b, DIR).await?;
    if status == 405 {
        // A leftover from an interrupted probe.
        b.delete_quietly(DIR).await;
        status = mkcol(b, DIR).await?;
    }
    if !(200..300).contains(&status) {
        return Ok(p);
    }
    let r = write_probes(b, &mut p).await;
    b.delete_quietly(DIR).await;
    r?;
    Ok(p)
}

async fn mkcol(b: &WebDavBackend, pid: &str) -> Result<u16, BErr> {
    Ok(b.send("MKCOL", &b.url(pid, true), &[], None)
        .await?
        .status()
        .as_u16())
}

/// PUT `body` to `pid` with `headers`; the status.
async fn put(
    b: &WebDavBackend,
    pid: &str,
    body: &'static [u8],
    headers: &[(&str, String)],
) -> Result<u16, BErr> {
    let mut h: Vec<(&str, String)> = vec![("content-length", body.len().to_string())];
    h.extend(headers.iter().cloned());
    let resp = b
        .send("PUT", &b.url(pid, false), &h, Some(body.into()))
        .await?;
    if resp.headers().contains_key("x-mokuro-put") {
        // Bunko answers .cbz PUTs with it; harmless to notice on any PUT.
        tracing::debug!("webdav probe: X-Mokuro-Put on PUT");
    }
    Ok(resp.status().as_u16())
}

/// The ETag the server reports for `pid` (no fallback).
async fn etag(b: &WebDavBackend, pid: &str) -> Result<Option<String>, BErr> {
    Ok(raw_entry(b, pid).await?.and_then(|e| e.etag))
}

async fn raw_entry(b: &WebDavBackend, pid: &str) -> Result<Option<xml::DavEntry>, BErr> {
    let mut first = None;
    match b
        .propfind(pid, false, "0", xml::PROPFIND_NODE, &mut |e| {
            if first.is_none() {
                first = Some(e);
            }
        })
        .await
    {
        Ok(_) => Ok(first),
        Err(BErr::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

async fn write_probes(b: &WebDavBackend, p: &mut Probe) -> Result<(), BErr> {
    // Depth: infinity, on the (tiny) probe folder.
    let mut n = 0usize;
    p.depth_infinity = b
        .propfind(DIR, true, "infinity", xml::PROPFIND_NODE, &mut |_| n += 1)
        .await
        .is_ok()
        && n >= 1;

    let file = format!("{DIR}/.bunko-probe-Aa");
    if !(200..300).contains(&put(b, &file, b"bunko-probe", &[]).await?) {
        return Ok(());
    }
    p.writable = true;
    let Some(entry) = raw_entry(b, &file).await? else {
        return Ok(());
    };
    let tag = entry.etag.clone();
    p.strong_etags = tag.as_deref().is_some_and(|t| !t.starts_with("W/"));
    p.hash_kind = entry
        .checksums
        .as_deref()
        .and_then(xml::best_checksum)
        .map(|(k, _)| k);

    // Range.
    let r = b
        .send(
            "GET",
            &b.url(&file, false),
            &[("range", "bytes=0-0".into())],
            None,
        )
        .await?;
    p.range = r.status().as_u16() == 206;
    drop(r);

    // If-Match: a wrong tag must fail, the right one must pass.
    let wrong = put(
        b,
        &file,
        b"bunko-probe-2",
        &[("if-match", "\"bunko-probe-wrong-tag\"".into())],
    )
    .await?;
    if wrong == 412
        && p.strong_etags
        && let Some(cur) = etag(b, &file).await?
    {
        let right = put(
            b,
            &file,
            b"bunko-probe-3",
            &[("if-match", super::quote_tag(&cur))],
        )
        .await?;
        p.if_match = (200..300).contains(&right);
    }

    // If-None-Match: * must refuse an existing name and accept a new one.
    let existing = put(b, &file, b"bunko-probe-4", &[("if-none-match", "*".into())]).await?;
    if existing == 412 {
        let fresh = put(
            b,
            &format!("{DIR}/.bunko-probe-new"),
            b"bunko-probe",
            &[("if-none-match", "*".into())],
        )
        .await?;
        p.if_none_match = (200..300).contains(&fresh);
    }

    // Case: does the other spelling find the file?
    let other = format!("{DIR}/.bunko-probe-aa");
    let resp = b
        .send(
            "PROPFIND",
            &b.url(&other, false),
            &[("depth", "0".into())],
            Some(xml::PROPFIND_NODE.into()),
        )
        .await?;
    p.case_sensitive = match resp.status().as_u16() {
        207 => Some(false),
        404 => Some(true),
        _ => None,
    };
    drop(resp);

    // Nested and in-place change propagation to the folder's ETag.
    let nested = format!("{DIR}/n");
    if mkcol(b, &nested).await? == 201 {
        let f = format!("{nested}/f");
        let e0 = etag(b, DIR).await?;
        let created = put(b, &f, b"one", &[]).await?;
        let e1 = etag(b, DIR).await?;
        let rewritten = put(b, &f, b"two!", &[]).await?;
        let e2 = etag(b, DIR).await?;
        p.nested_propagation = (200..300).contains(&created)
            && (200..300).contains(&rewritten)
            && e0.is_some()
            && e0 != e1
            && e1 != e2;
    }
    Ok(())
}
