//! DELETE, MKCOL, COPY and MOVE (spec §8.3.5–8.3.7), with the destructive 0.5.2 quirks
//! 14.1–14.3 fixed: cross-class moves/copies are refused (403), a file never replaces a
//! collection or vice versa (409), and the virtual roots are never moved, copied or
//! deleted (403).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bunko_store::fs;
use http::StatusCode;
use serde_json::json;

use super::{
    Effects, Req, audit, blocking, check_dav_locks, eval_if, file_audit_target,
    folder_audit_target, forget_ocr_records, is_cbz, is_virtual_root, lookup, ownership_holds,
    personal_audit_target, personal_grant_holds, primary_leaving, write_lock_conflict,
};
use crate::paths::{self, PathClass};
use crate::propfind::resource_key;
use crate::resource::{FileKind, Resource};
use crate::response::{self, DavError, DavResult, Resp};
use crate::{Inner, sidecars};
use bunko_core::reader_tree::{self, HistoryShape};
use bunko_core::sidecar_names;

pub(crate) async fn delete(inner: &Arc<Inner>, req: &Req) -> DavResult<Resp> {
    if is_virtual_root(&req.path) {
        return Err(DavError::new(
            StatusCode::FORBIDDEN,
            "The library root cannot be deleted.",
        ));
    }
    let res = lookup(inner, &req.path, req)
        .await?
        .ok_or_else(|| DavError::new(StatusCode::NOT_FOUND, &req.path))?;
    personal_grant_holds(req, res.kind())?;
    if req.content_length() != 0 {
        return Err(DavError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "The server does not handle any body content.",
        ));
    }
    match (res.is_collection(), req.header("depth")) {
        (true, None | Some("infinity")) | (false, None | Some("0") | Some("infinity")) => {}
        _ => {
            return Err(DavError::new(
                StatusCode::BAD_REQUEST,
                "Invalid Depth header.",
            ));
        }
    }
    eval_if(inner, req, &res)?;
    let key = resource_key(&res, req.username());
    check_dav_locks(inner, req, &key, true)?;
    if let Some(parent) = paths::uri_parent(&req.path) {
        check_dav_locks(inner, req, &paths::normalize(&parent), false)?;
    }
    let (inner2, req2) = (inner.clone(), req.clone());
    blocking(move || {
        ownership_holds(&req2)?;
        let mut effects = Effects::default();
        delete_resource(&inner2, &req2, &res, &mut effects)?;
        inner2.locks.remove_tree(&key);
        inner2.dead.remove_tree(&key);
        effects.fire(&inner2, &req2);
        Ok::<_, DavError>(())
    })
    .await??;
    Ok(response::empty(StatusCode::NO_CONTENT))
}

/// Delete one file or folder with every side effect (0.5.2 `MokuroFileResource.delete` /
/// `MokuroFolderResource.delete`). Blocking.
fn delete_resource(
    inner: &Inner,
    req: &Req,
    res: &Resource,
    effects: &mut Effects,
) -> DavResult<()> {
    match res {
        Resource::Virtual { .. } => Ok(()),
        Resource::File {
            path,
            phys,
            kind: FileKind::Personal,
            ..
        } => delete_personal(inner, req, path, phys, effects),
        Resource::File { path, phys, .. } => delete_file(inner, req, path, phys, effects),
        Resource::Folder { path, phys, .. } => {
            if !fs::exists(phys) {
                return Ok(());
            }
            let target = folder_audit_target(inner, path, Some(phys));
            let Some(_guard) = inner.write_locks.try_lock(phys) else {
                return Err(write_lock_conflict(req, target, "delete", None));
            };
            fs::remove_dir_all(phys)?;
            if let Some(rel) = inner.roots.library_rel(phys).filter(|r| !r.is_empty()) {
                req.ctx.hooks.forget_volume_uploads_under_prefix(&rel);
                req.ctx.hooks.forget_ocr_sidecars_under_prefix(&rel);
                req.ctx.hooks.forget_volume_uuids_under_prefix(&rel);
                req.ctx.hooks.series_folder_removed(&rel);
                inner.overlays().forget_prefix(&rel);
            }
            effects.note_removed(inner, phys);
            effects.touch(path, Some(phys));
            audit(req, "delete", target, None);
            Ok(())
        }
    }
}

fn delete_file(
    inner: &Inner,
    req: &Req,
    vpath: &str,
    phys: &Path,
    effects: &mut Effects,
) -> DavResult<()> {
    if !fs::exists(phys) {
        return Ok(());
    }
    let target = file_audit_target(inner, vpath, phys);
    let Some(_guard) = inner.write_locks.try_lock(phys) else {
        return Err(write_lock_conflict(req, target, "delete", None));
    };
    let rel = inner.roots.library_rel(phys);
    primary_leaving(inner, req, phys);
    // Only a library archive is a volume with sidecars and records.
    let cbz = rel.is_some() && is_cbz(phys);
    if cbz {
        for sidecar in sidecars::siblings(phys) {
            let _ = fs::remove_file(&sidecar);
        }
    }
    match fs::remove_file(phys) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if cbz {
        effects.note_removed(inner, phys);
        if let Some(rel) = rel.as_deref() {
            req.ctx.hooks.forget_volume_upload(rel);
            req.ctx.hooks.forget_volume_uuid(rel);
            // Every account's corrections of the volume go with it (the owner's
            // physical one went with the sidecars).
            inner.overlays().forget_volume(rel);
        }
    }
    forget_ocr_records(req, rel.as_deref(), true);
    effects.touch(vpath, Some(phys));
    audit(req, "delete", target, None);
    Ok(())
}

/// Delete the caller's own correction (D27): nothing else changes.
fn delete_personal(
    inner: &Inner,
    req: &Req,
    vpath: &str,
    phys: &Path,
    effects: &mut Effects,
) -> DavResult<()> {
    let target = personal_audit_target(vpath);
    let Some(_guard) = inner.write_locks.try_lock(phys) else {
        return Err(write_lock_conflict(req, target, "delete", None));
    };
    match fs::remove_file(phys) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    effects.touch_personal(inner, vpath, phys);
    audit(req, "delete", target, None);
    Ok(())
}

pub(crate) async fn mkcol(inner: &Arc<Inner>, req: &Req) -> DavResult<Resp> {
    if req.content_length() != 0 {
        return Err(DavError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "The server does not handle any body content.",
        ));
    }
    if req.header("depth").is_some_and(|d| d != "0") {
        return Err(DavError::new(StatusCode::BAD_REQUEST, "Depth must be '0'."));
    }
    if lookup(inner, &req.path, req).await?.is_some() {
        return Err(DavError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "MKCOL can only be executed on an unmapped URL.",
        ));
    }
    let parent = match paths::uri_parent(&req.path) {
        Some(p) => lookup(inner, &p, req).await?,
        None => None,
    };
    let Some(parent) = parent.filter(Resource::is_collection) else {
        return Err(DavError::new(
            StatusCode::CONFLICT,
            "Parent must be an existing collection.",
        ));
    };
    check_dav_locks(inner, req, &resource_key(&parent, req.username()), false)?;
    let name = paths::uri_name(&req.path).to_string();
    // In the caller's own tree only the history collections can be created: `history`
    // and one level of device folders below it.
    if let paths::Target::Progress(rel) = paths::classify(&req.path)
        && reader_tree::history_shape_of_rel(&rel) != Some(HistoryShape::Collection)
    {
        return Err(DavError::new(StatusCode::FORBIDDEN, "Forbidden"));
    }
    let (inner2, req2) = (inner.clone(), req.clone());
    // A series folder (a top-level library folder) is owned by the request that
    // created it (0.8).
    let series_folder = matches!(parent, Resource::Virtual { .. });
    blocking(move || {
        let Some((dir, kind)) = inner2
            .roots
            .member_path(&parent, &name, req2.caller())
            .filter(|(_, kind)| *kind != FileKind::Personal)
        else {
            return Err(DavError::new(StatusCode::FORBIDDEN, "Forbidden"));
        };
        if kind == FileKind::Library {
            // An owner rule admitted it: ask again where the folder is made (OWN-6).
            ownership_holds(&req2)?;
        }
        if series_folder && kind == FileKind::Library {
            create_series_folder(&inner2, &req2, &dir)?;
        } else if kind == FileKind::Library {
            // The parent was checked above, but may have gone since: a MKCOL never
            // makes it again (an unowned series folder, made by nobody's MKCOL), it
            // answers 409 as RFC 4918 has it (OWN-6, as C9 for a PUT). A concurrent
            // MKCOL of the same folder is not an error.
            let made = if other_spelling_listed(&dir, false) {
                Ok(())
            } else {
                fs::create_dir(&dir)
            };
            match made {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(DavError::new(
                        StatusCode::CONFLICT,
                        "Parent must be an existing collection.",
                    ));
                }
                Err(e) => return Err(e.into()),
            }
        } else {
            // Also creates `users/<u>/` on an account's first use; a concurrent MKCOL of
            // the same collection is not an error.
            fs::create_dir_all(&dir)?;
        }
        let member = paths::join_uri(parent.path(), &name);
        let target = match kind {
            FileKind::Progress => ("progress", paths::normalize(&member)),
            FileKind::Library | FileKind::Personal => {
                folder_audit_target(&inner2, parent.path(), parent.phys())
            }
        };
        audit(&req2, "mkdir", target, Some(json!({ "path": member })));
        let mut effects = Effects::default();
        effects.touch(&member, Some(&dir));
        effects.fire(&inner2, &req2);
        Ok(())
    })
    .await??;
    Ok(response::status_page(StatusCode::CREATED, ""))
}

/// One series folder MKCOL at a time, from its create to its owner record: a store
/// whose create is a lookup and then a make (a cloud index) would otherwise let two
/// concurrent requests both create it, and the later one take it.
static SERIES_FOLDER_CREATE: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// MKCOL of a top-level library folder. The request that creates it tells the hooks who
/// did; one that finds it there already (a concurrent MKCOL) is not an error and owns
/// nothing. Blocking.
fn create_series_folder(inner: &Inner, req: &Req, dir: &Path) -> DavResult<()> {
    let _one = SERIES_FOLDER_CREATE.lock();
    let made = if other_spelling_listed(dir, true) {
        Err(std::io::ErrorKind::AlreadyExists.into())
    } else {
        fs::create_dir(dir)
    };
    let created = match made {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(e.into()),
    };
    if created
        && let (Some(rel), Some(actor)) = (inner.roots.library_rel(dir), req.username())
        && !rel.is_empty()
    {
        req.ctx.hooks.series_folder_created(&rel, actor);
    }
    Ok(())
}

/// Another spelling of `dir`'s name is listed beside it now: a concurrent request made
/// it after PathCase resolved this one. That entry is this name (one name is one entry
/// on every host), so nothing is made: the MKCOL ends as one that found its folder there
/// already. `top`: `dir` is a series folder. Blocking.
fn other_spelling_listed(dir: &Path, top: bool) -> bool {
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name().and_then(|n| n.to_str()))
    else {
        return false;
    };
    crate::path_case::on_disk_name(parent, name, top).is_some_and(|listed| listed != name)
}

/// The `Destination` header as a decoded path (WsgiDAV rules; spec §8.3.7).
fn destination_path(req: &Req) -> DavResult<String> {
    let raw = req.header("destination").ok_or_else(|| {
        DavError::new(
            StatusCode::BAD_REQUEST,
            "Missing required Destination header.",
        )
    })?;
    let (netloc, path) = match raw.find("://") {
        Some(i) => {
            let rest = &raw[i + 3..];
            match rest.find('/') {
                Some(j) => (Some(&rest[..j]), &rest[j..]),
                None => (Some(rest), "/"),
            }
        }
        None => (None, raw),
    };
    // Query and fragment are not part of the path.
    let path = path.split(['?', '#']).next().unwrap_or("");
    if let Some(netloc) = netloc {
        let netloc = netloc.to_ascii_lowercase();
        let host = req
            .header("host")
            .map(str::to_ascii_lowercase)
            .or_else(|| req.uri.authority().map(|a| a.as_str().to_ascii_lowercase()));
        let fwd = req.header("x-forwarded-host").map(str::to_ascii_lowercase);
        if !netloc.is_empty() && Some(&netloc) != host.as_ref() && Some(&netloc) != fwd.as_ref() {
            return Err(DavError::new(
                StatusCode::BAD_GATEWAY,
                "Source and destination must have the same host name.",
            ));
        }
    }
    let decoded = paths::decode_request_path(path)
        .filter(|p| !p.split(['/', '\\']).any(|seg| seg == ".."))
        .ok_or_else(|| DavError::new(StatusCode::BAD_REQUEST, "Invalid Destination header."))?;
    if !decoded.starts_with('/') {
        return Err(DavError::new(
            StatusCode::BAD_GATEWAY,
            "Inter-realm copy/move is not supported.",
        ));
    }
    Ok(decoded)
}

pub(crate) async fn copy_move(inner: &Arc<Inner>, req: &Req, is_move: bool) -> DavResult<Resp> {
    let user = req.username();
    if is_virtual_root(&req.path) {
        return Err(DavError::new(
            StatusCode::FORBIDDEN,
            "The library root cannot be moved or copied.",
        ));
    }
    let src = lookup(inner, &req.path, req)
        .await?
        .ok_or_else(|| DavError::new(StatusCode::NOT_FOUND, &req.path))?;
    personal_grant_holds(req, src.kind())?;
    let mut dest_path = destination_path(req)?;
    let overwrite = match req
        .header("overwrite")
        .map(str::to_ascii_uppercase)
        .as_deref()
    {
        None | Some("T") => true,
        Some("F") => false,
        Some(_) => {
            return Err(DavError::new(
                StatusCode::BAD_REQUEST,
                "Invalid Overwrite header.",
            ));
        }
    };
    let depth_infinity = if src.is_collection() {
        match req.header("depth") {
            None | Some("infinity") => true,
            Some("0") if !is_move => false,
            Some("0") => {
                return Err(DavError::new(
                    StatusCode::BAD_REQUEST,
                    "Depth header for MOVE collection must be 'infinity'.",
                ));
            }
            Some(_) => {
                return Err(DavError::new(
                    StatusCode::BAD_REQUEST,
                    "Invalid Depth header.",
                ));
            }
        }
    } else {
        match req.header("depth") {
            None | Some("0") | Some("infinity") => false,
            Some(_) => {
                return Err(DavError::new(
                    StatusCode::BAD_REQUEST,
                    "Invalid Depth header.",
                ));
            }
        }
    };
    if src.is_collection() {
        dest_path = format!("{}/", dest_path.trim_end_matches('/'));
    }

    // The reading history is written file by file and never rearranged: nothing moves
    // or copies into, out of or inside it.
    if paths::is_history_path(&req.path) || paths::is_history_path(&dest_path) {
        return Err(DavError::new(
            StatusCode::FORBIDDEN,
            "Reading history cannot be moved or copied.",
        ));
    }
    // 14.1 / 14.3: both ends in the same class (library or progress), never a virtual root.
    let src_class = paths::class_of(&req.path);
    let dest_class = paths::class_of(&dest_path);
    if !matches!(src_class, PathClass::Library | PathClass::Progress) || dest_class != src_class {
        return Err(DavError::new(
            StatusCode::FORBIDDEN,
            "Source and destination must both be library paths, or both progress files.",
        ));
    }

    let src_norm = paths::normalize(&req.path);
    let dest_norm = paths::normalize(&dest_path);
    // A MOVE to the source's own path spelled in another case renames it (0.5.3). On a
    // case-insensitive filesystem the destination opens the source itself: reported as
    // existing, `Overwrite: T` would delete it first -- the very file or folder being
    // renamed (0.5.3 `_is_rename_of_source`).
    let renames_source = is_move
        && src_norm != dest_norm
        && crate::fold_name(&src_norm) == crate::fold_name(&dest_norm);
    let mut dest = lookup(inner, &dest_path, req).await?;
    if renames_source
        && let (Some(s), Some(d)) = (src.phys(), dest.as_ref().and_then(Resource::phys))
    {
        let (s, d) = (s.to_path_buf(), d.to_path_buf());
        if blocking(move || crate::path_case::same_file(&s, &d)).await? {
            dest = None;
        }
    }
    // A case-fixing MOVE whose destination still exists is another entry: a second
    // spelling of the name (only a case-sensitive disk or a remote store holds one).
    // Bunko keeps one spelling per name, and `Overwrite: T` would delete the other
    // spelling's content (or, where `same_file` cannot tell, the source itself).
    if renames_source && dest.is_some() {
        return Err(DavError::new(
            StatusCode::CONFLICT,
            "Another spelling of this name already exists here; Bunko keeps one spelling \
             per name: merge them first.",
        ));
    }
    let dest_parent = match paths::uri_parent(&dest_path) {
        Some(p) => lookup(inner, &p, req).await?,
        None => None,
    };
    let Some(dest_parent) = dest_parent.filter(Resource::is_collection) else {
        return Err(DavError::new(
            StatusCode::CONFLICT,
            "Destination parent must be a collection.",
        ));
    };
    eval_if(inner, req, &src)?;
    if let Some(d) = &dest {
        eval_if(inner, req, d)?;
    }
    if is_move {
        check_dav_locks(inner, req, &resource_key(&src, user), true)?;
        if let Some(p) = paths::uri_parent(&req.path) {
            check_dav_locks(inner, req, &paths::normalize(&p), false)?;
        }
    }
    match &dest {
        None => check_dav_locks(inner, req, &resource_key(&dest_parent, user), false)?,
        Some(d) => check_dav_locks(inner, req, &resource_key(d, user), true)?,
    }
    if src_norm == dest_norm {
        return Err(DavError::new(
            StatusCode::FORBIDDEN,
            "Cannot copy/move source onto itself",
        ));
    }
    if paths::is_equal_or_child(&src_norm, &dest_norm) {
        return Err(DavError::new(
            StatusCode::FORBIDDEN,
            "Cannot copy/move source below itself",
        ));
    }
    if dest.is_some() && !overwrite {
        return Err(DavError::new(
            StatusCode::PRECONDITION_FAILED,
            "Destination already exists and Overwrite is set to false",
        ));
    }
    // 14.2: a file never replaces a collection, nor a collection a file.
    if let Some(d) = &dest
        && d.is_collection() != src.is_collection()
    {
        return Err(DavError::new(
            StatusCode::CONFLICT,
            "Source and destination must both be files or both be collections.",
        ));
    }

    let dest_name = paths::uri_name(&dest_path).to_string();
    let (inner2, req2) = (inner.clone(), req.clone());
    let dest_exists = dest.is_some();
    let status = blocking(move || -> DavResult<StatusCode> {
        let Some((dest_phys, dest_kind)) =
            inner2
                .roots
                .destination_path(&dest_parent, &dest_name, req2.caller())
        else {
            return Err(DavError::new(StatusCode::FORBIDDEN, "Forbidden"));
        };
        personal_grant_holds(&req2, Some(dest_kind))?;
        // A correction is the caller's own (D27): it moves or copies to another
        // correction name only, and nothing else becomes one.
        if (src.kind() == Some(FileKind::Personal)) != (dest_kind == FileKind::Personal) {
            return Err(DavError::new(
                StatusCode::FORBIDDEN,
                "Corrections can only move between correction files.",
            ));
        }
        // Stored under the one spelling a PUT takes: a plain `.mokuro` (critique C18).
        if dest_kind == FileKind::Personal {
            if sidecar_names::strip_suffix_ci(&dest_name, ".gz").is_some() {
                return Err(DavError::new(
                    StatusCode::FORBIDDEN,
                    "Corrections are saved as plain .mokuro files.",
                ));
            }
            if !dest_name.ends_with(".mokuro") {
                return Err(DavError::new(
                    StatusCode::CONFLICT,
                    "A correction's name must end in .mokuro.",
                ));
            }
        }
        // The virtual paths differed; the files may not (`a//x`, `a/./x`, a symlink, a
        // case-insensitive filesystem). Overwriting the source with itself deletes it. A
        // rename that only changes the case is the source on a case-insensitive filesystem
        // and a rename does the right thing there.
        if let Some(src_phys) = src.phys()
            && !renames_source
        {
            if same_physical(src_phys, &dest_phys) {
                return Err(DavError::new(
                    StatusCode::FORBIDDEN,
                    "Cannot copy/move source onto itself",
                ));
            }
            if src.is_collection() && physically_below(&dest_phys, src_phys) {
                return Err(DavError::new(
                    StatusCode::FORBIDDEN,
                    "Cannot copy/move source below itself",
                ));
            }
        }
        let mut effects = Effects::default();
        let status = match &src {
            Resource::File {
                path,
                phys,
                kind: FileKind::Personal,
                ..
            } => {
                if let Some(d) = &dest {
                    delete_resource(&inner2, &req2, d, &mut effects)?;
                }
                move_personal(
                    &inner2,
                    &req2,
                    (path, phys),
                    (&dest_norm, &dest_phys),
                    is_move,
                    &mut effects,
                )?;
                // A file MOVE answers 204 like the library's (WsgiDAV `handle_move`).
                if is_move || dest_exists {
                    StatusCode::NO_CONTENT
                } else {
                    StatusCode::CREATED
                }
            }
            Resource::File { path, phys, .. } => {
                if is_move {
                    move_file(
                        &inner2,
                        &req2,
                        path,
                        phys,
                        &dest_norm,
                        &dest_phys,
                        &mut effects,
                    )?;
                    // A natively handled file MOVE answers 204 (WsgiDAV `handle_move`).
                    StatusCode::NO_CONTENT
                } else {
                    if let Some(d) = &dest {
                        delete_resource(&inner2, &req2, d, &mut effects)?;
                    }
                    copy_file(&inner2, &req2, phys, &dest_norm, &dest_phys, &mut effects)?;
                    if dest_exists {
                        StatusCode::NO_CONTENT
                    } else {
                        StatusCode::CREATED
                    }
                }
            }
            Resource::Folder { path, phys, .. } => {
                if is_move {
                    if let Some(d) = &dest {
                        delete_resource(&inner2, &req2, d, &mut effects)?;
                    }
                    move_folder(
                        &inner2,
                        &req2,
                        path,
                        phys,
                        &dest_norm,
                        &dest_phys,
                        &mut effects,
                    )?;
                } else {
                    copy_folder(
                        &inner2,
                        &req2,
                        &src,
                        &dest,
                        &dest_norm,
                        &dest_phys,
                        depth_infinity,
                        &mut effects,
                    )?;
                }
                if dest_exists {
                    StatusCode::NO_CONTENT
                } else {
                    StatusCode::CREATED
                }
            }
            Resource::Virtual { .. } => return Err(DavError::status(StatusCode::FORBIDDEN)),
        };
        if is_move {
            let key = resource_key(&src, req2.username());
            inner2.locks.remove_tree(&key);
        }
        effects.fire(&inner2, &req2);
        Ok(status)
    })
    .await??;
    Ok(if status == StatusCode::NO_CONTENT {
        response::empty(status)
    } else {
        response::status_page(status, "")
    })
}

/// Do `a` and `b` name the same file or folder on disk (equal paths, the same inode, or
/// the same canonical path)? Missing paths are only equal to themselves.
fn same_physical(a: &Path, b: &Path) -> bool {
    a == b || crate::path_case::same_file(a, b)
}

/// Is `dest` (which may not exist yet) inside the folder `src` on disk?
fn physically_below(dest: &Path, src: &Path) -> bool {
    let Ok(src) = fs::canonicalize(src) else {
        return dest.starts_with(src);
    };
    // The deepest existing ancestor of `dest`, canonical, with the rest re-attached.
    let mut probe = dest.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canon) = fs::canonicalize(&probe) {
            let mut full = canon;
            full.extend(tail.iter().rev());
            return full.starts_with(&src);
        }
        match (probe.file_name().map(|n| n.to_os_string()), probe.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                probe = parent.to_path_buf();
            }
            _ => return dest.starts_with(&src),
        }
    }
}

/// 0.5.2 `MokuroFileResource.handle_move`. Sidecars stay where they are (pinned).
fn move_file(
    inner: &Inner,
    req: &Req,
    src_v: &str,
    src: &Path,
    dest_v: &str,
    dest: &Path,
    effects: &mut Effects,
) -> DavResult<()> {
    let target = file_audit_target(inner, src_v, src);
    let Some(_guard) = inner.write_locks.try_lock_all(&[src, dest]) else {
        return Err(write_lock_conflict(req, target, "move", Some(dest_v)));
    };
    let old_rel = inner.roots.library_rel(src);
    let new_rel = inner.roots.library_rel(dest);
    primary_leaving(inner, req, src);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(src, dest)?;
    if is_cbz(src) && is_cbz(dest) && old_rel.is_some() && new_rel.is_some() {
        req.ctx.hooks.archive_moved(src, dest);
    }
    if is_cbz(src) {
        effects.note_removed(inner, src);
    }
    effects.note_arrived(inner, dest);
    if let (Some(old), Some(new)) = (old_rel.as_deref(), new_rel.as_deref()) {
        req.ctx.hooks.rename_volume_upload(old, new);
        // Every account's corrections follow the archive, the cloud-root owner's
        // physical one included. The reader's own MOVE of its `<Old>.edit.mokuro`
        // then finds it already there.
        if is_cbz(src) {
            let overlays = inner.overlays();
            overlays.rename_volume(old, new);
            overlays.rename_physical(old, new);
        }
    }
    forget_ocr_records(req, old_rel.as_deref(), true);
    forget_ocr_records(req, new_rel.as_deref(), false);
    effects.touch(src_v, Some(src));
    effects.touch(dest_v, Some(dest));
    audit(req, "move", target, Some(json!({ "destination": dest_v })));
    Ok(())
}

/// MOVE or COPY of the caller's own correction to another of its correction names
/// (D27): the file only, no record of any kind.
fn move_personal(
    inner: &Inner,
    req: &Req,
    (src_v, src): (&str, &Path),
    (dest_v, dest): (&str, &Path),
    is_move: bool,
    effects: &mut Effects,
) -> DavResult<()> {
    let target = personal_audit_target(src_v);
    let operation = if is_move { "move" } else { "copy" };
    let Some(_guard) = inner.write_locks.try_lock_all(&[src, dest]) else {
        return Err(write_lock_conflict(req, target, operation, Some(dest_v)));
    };
    if is_move {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(src, dest)?;
        effects.touch_personal(inner, src_v, src);
    } else {
        copy_one(src, dest)?;
    }
    effects.touch_personal(inner, dest_v, dest);
    audit(
        req,
        operation,
        target,
        Some(json!({ "destination": dest_v })),
    );
    Ok(())
}

/// 0.5.2 file `copy_move_single(is_move=False)`: the copy is untracked (no owner row);
/// sidecars are not copied. Written to a staging file and renamed (never a torn file).
fn copy_file(
    inner: &Inner,
    req: &Req,
    src: &Path,
    dest_v: &str,
    dest: &Path,
    effects: &mut Effects,
) -> DavResult<()> {
    let target = file_audit_target(inner, dest_v, dest);
    let Some(_guard) = inner.write_locks.try_lock(dest) else {
        return Err(write_lock_conflict(req, target, "copy", Some(dest_v)));
    };
    copy_one(src, dest)?;
    effects.note_arrived(inner, dest);
    forget_ocr_records(req, inner.roots.library_rel(dest).as_deref(), false);
    effects.touch(dest_v, Some(dest));
    audit(
        req,
        "copy",
        file_audit_target(inner, dest_v, dest),
        Some(json!({ "destination": dest_v })),
    );
    Ok(())
}

/// Copy bytes, permissions and times (Python `shutil.copy2`), atomically. A failed read
/// of the source is answered as a GET's is ([`DavError::from_read_io`]: a file changed
/// behind the index is a retry, not a 412).
fn copy_one(src: &Path, dest: &Path) -> DavResult<()> {
    let dir = dest
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent"))?;
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!(
        ".{}.upload-copy{}.tmp",
        paths::file_name(dest),
        std::process::id()
    ));
    let result = (|| -> DavResult<()> {
        fs::copy(src, &temp).map_err(|e| DavError::from_read_io(&e))?;
        let meta = fs::metadata(src)?;
        let file = fs::File::options().write(true).open(&temp)?;
        let mut times = std::fs::FileTimes::new();
        if let Ok(m) = meta.modified() {
            times = times.set_modified(m);
        }
        if let Some(a) = meta.as_std().and_then(|m| m.accessed().ok()) {
            times = times.set_accessed(a);
        }
        file.set_times(times)?;
        drop(file);
        Ok(fs::rename(&temp, dest)?)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// 0.5.2 `MokuroFolderResource.move_recursive`: one rename of the whole tree, sidecars and
/// ownership move with it.
fn move_folder(
    inner: &Inner,
    req: &Req,
    src_v: &str,
    src: &Path,
    dest_v: &str,
    dest: &Path,
    effects: &mut Effects,
) -> DavResult<()> {
    let target = folder_audit_target(inner, src_v, Some(src));
    let Some(_guard) = inner.write_locks.try_lock_all(&[src, dest]) else {
        return Err(write_lock_conflict(req, target, "move", Some(dest_v)));
    };
    let old_prefix = inner.roots.library_rel(src);
    let new_prefix = inner.roots.library_rel(dest);
    let volumes = cbz_under(src);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(src, dest)?;
    effects.note_removed(inner, src);
    match (old_prefix.as_deref(), new_prefix.as_deref()) {
        (Some(old_prefix), Some(new_prefix)) => {
            for old in &volumes {
                let Some(old_rel) = inner.roots.library_rel(old) else {
                    continue;
                };
                let suffix = old_rel[old_prefix.len().min(old_rel.len())..].trim_start_matches('/');
                let new_rel = if suffix.is_empty() {
                    new_prefix.to_string()
                } else {
                    format!("{new_prefix}/{suffix}")
                };
                req.ctx.hooks.rename_volume_upload(&old_rel, &new_rel);
            }
            req.ctx
                .hooks
                .rename_ocr_sidecars_under_prefix(old_prefix, new_prefix);
            req.ctx
                .hooks
                .rename_volume_uuids_under_prefix(old_prefix, new_prefix);
            req.ctx.hooks.series_folder_moved(old_prefix, new_prefix);
            inner.overlays().rename_prefix(old_prefix, new_prefix);
        }
        (Some(old_prefix), None) => {
            req.ctx.hooks.forget_ocr_sidecars_under_prefix(old_prefix);
            req.ctx.hooks.forget_volume_uuids_under_prefix(old_prefix);
            req.ctx.hooks.series_folder_removed(old_prefix);
            inner.overlays().forget_prefix(old_prefix);
        }
        _ => {}
    }
    for cbz in cbz_under(dest) {
        effects.note_arrived(inner, &cbz);
    }
    effects.touch(src_v, Some(src));
    effects.touch(dest_v, Some(dest));
    audit(req, "move", target, Some(json!({ "destination": dest_v })));
    Ok(())
}

/// Every `.cbz` below `dir` (not following symlinked folders).
fn cbz_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(_) if is_cbz(&p) => out.push(p),
                _ => {}
            }
        }
    }
    out
}

/// Generic folder COPY: `Depth: infinity` copies the tree (files as file COPY); a
/// destination collection is not merged into (its members missing from the source go
/// first, RFC 4918 9.8.4); `Depth: 0` creates only the folder.
#[allow(clippy::too_many_arguments)]
fn copy_folder(
    inner: &Inner,
    req: &Req,
    src: &Resource,
    dest: &Option<Resource>,
    dest_v: &str,
    dest_phys: &Path,
    infinity: bool,
    effects: &mut Effects,
) -> DavResult<()> {
    // Locked file by file, as 0.5.2 (each copy takes its destination's lock).
    let Some(src_phys) = src.phys() else {
        return Err(DavError::status(StatusCode::FORBIDDEN));
    };
    if let Some(existing) = dest
        && infinity
    {
        prune_unmatched(inner, req, existing, src_phys, dest_phys, effects)?;
    }
    fs::create_dir_all(dest_phys)?;
    effects.touch(dest_v, Some(dest_phys));
    if !infinity {
        return Ok(());
    }
    let mut stack: Vec<(PathBuf, PathBuf, String)> = vec![(
        src_phys.to_path_buf(),
        dest_phys.to_path_buf(),
        dest_v.trim_end_matches('/').to_string(),
    )];
    while let Some((from, to, vto)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&from) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if paths::is_staging_name(&name) {
                continue;
            }
            let (f, t, v) = (e.path(), to.join(&name), format!("{vto}/{name}"));
            match fs::metadata(&f) {
                Ok(m) if m.is_dir() => {
                    // Never descend into a symlink leading out of the library.
                    if fs::canonicalize(&f).is_ok_and(|c| {
                        c.starts_with(&inner.roots.library) && !dest_phys.starts_with(&c)
                    }) {
                        fs::create_dir_all(&t)?;
                        stack.push((f, t, v));
                    }
                }
                Ok(_) => {
                    if fs::exists(&t) {
                        delete_file(inner, req, &v, &t, effects)?;
                    }
                    copy_file(inner, req, &f, &v, &t, effects)?;
                }
                Err(_) => {}
            }
        }
    }
    Ok(())
}

/// Delete the members of an existing destination collection that the source lacks.
fn prune_unmatched(
    inner: &Inner,
    req: &Req,
    existing: &Resource,
    src: &Path,
    dest: &Path,
    effects: &mut Effects,
) -> DavResult<()> {
    let base_v = existing.path().trim_end_matches('/').to_string();
    let mut stack = vec![(src.to_path_buf(), dest.to_path_buf(), base_v)];
    while let Some((s, d, v)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            let (sp, dp, vp) = (
                s.join(&name),
                e.path(),
                format!("{v}/{}", name.to_string_lossy()),
            );
            let d_is_dir = fs::metadata(&dp).map(|m| m.is_dir()).unwrap_or(false);
            let s_meta = fs::metadata(&sp).ok();
            match s_meta {
                Some(m) if m.is_dir() == d_is_dir => {
                    if d_is_dir {
                        stack.push((sp, dp, vp));
                    }
                }
                _ => {
                    let stat = fs::metadata(&dp).map(|m| crate::resource::Stat::from_meta(&m));
                    if let Ok(stat) = stat {
                        let res = if d_is_dir {
                            Resource::Folder {
                                path: vp,
                                phys: dp,
                                stat,
                                kind: FileKind::Library,
                            }
                        } else {
                            Resource::File {
                                path: vp,
                                phys: dp,
                                stat,
                                kind: FileKind::Library,
                            }
                        };
                        delete_resource(inner, req, &res, effects)?;
                    }
                }
            }
        }
    }
    Ok(())
}
