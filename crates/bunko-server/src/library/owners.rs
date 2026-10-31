//! Series folder owners (0.8): the records of `bunko_db::folders` kept in step with the
//! library folders, and given once to the folders that existed before them.
//!
//! * The request that CREATES a top-level folder (a WebDAV MKCOL; a PUT never makes a
//!   folder) owns it ([`folder_created`]). One that adds a spelling of a series already
//!   listed joins it: the record, or its absence, is unchanged.
//! * A record follows its folder through a rename and goes when the last spelling of
//!   its series does ([`folder_moved`], [`folder_removed`]): WebDAV's and the cloud's
//!   own changes alike. A cloud case merge only joins spellings of one series key, so
//!   it changes no record.
//! * Folders that existed before the records get one at startup, once
//!   ([`adopt_series_folder_owners`]): the one uploader of their tracked volumes, when
//!   no archive in them is untracked. Every other folder (mixed, untracked, empty) gets
//!   none, and stays shared.
//!
//! Records are read only through [`series_folder_is_live`]: a folder that went without
//! a hook (a crash, a delete in the cloud while the server was off) leaves a record
//! that nothing acts on, and the next creator of the folder replaces it.
//!
//! Bunko's own writes (the processor, the compiler, covers, keepsakes, the inbox, a
//! duplicate merge) never call these: they assign no owner.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use bunko_core::reader_tree;
use bunko_core::sidecar_names::{ARCHIVE_EXTENSIONS, strip_suffix_ci};
use bunko_db::{Database, fold_series_title_key};

/// What one startup adoption did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FolderAdoption {
    /// Series now owned by the one uploader of their tracked volumes.
    pub owned: usize,
    /// Series left without an owner (mixed, untracked or empty).
    pub unowned: usize,
}

/// A top-level library name that can be a series folder: not the cloud-root owner's
/// own tree, the cover cache, or a dot name (staging).
fn is_series_folder_name(name: &str) -> bool {
    !reader_tree::is_per_user_root_name(name) && name != "thumbnails" && !name.starts_with('.')
}

/// The series folders listed at the library root (through the store: an index read on
/// a cloud library); `None` when the root cannot be listed. Blocking.
fn series_folders(library: &Path) -> Option<Vec<String>> {
    let entries = bunko_store::fs::read_dir(library).ok()?;
    Some(
        entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| is_series_folder_name(n))
            .collect(),
    )
}

/// One segment, no more: a series folder's library rel.
fn top_level(rel: &str) -> Option<&str> {
    let rel = rel.trim_matches('/');
    (!rel.is_empty() && !rel.contains('/')).then_some(rel)
}

/// The series keys of the folders listed at the library root. Blocking.
pub fn live_series_keys(library: &Path) -> Option<HashSet<String>> {
    Some(
        series_folders(library)?
            .iter()
            .map(|n| fold_series_title_key(n))
            .collect(),
    )
}

/// Is a folder of `folder`'s series listed at the library root? The folder itself is
/// one lookup; any other spelling takes the root listing. Blocking.
pub fn series_folder_is_live(library: &Path, folder: &str) -> bool {
    let Some(folder) = top_level(folder) else {
        return false;
    };
    if is_series_folder_name(folder) && bunko_store::fs::is_dir(library.join(folder)) {
        return true;
    }
    let key = fold_series_title_key(folder);
    live_series_keys(library).is_some_and(|keys| keys.contains(&key))
}

/// A folder of the series `key` other than the names in `except` is listed.
fn variant_listed(library: &Path, key: &str, except: &[&str]) -> bool {
    series_folders(library).is_some_and(|names| {
        names
            .iter()
            .any(|n| !except.contains(&n.as_str()) && fold_series_title_key(n) == key)
    })
}

fn log<T>(what: &str, r: bunko_db::Result<T>) {
    if let Err(e) = r {
        tracing::warn!("[LIBRARY] series folder owner: {what} failed: {e}");
    }
}

/// A client's MKCOL created the series folder `rel`: `actor` owns it, replacing a
/// record its series kept from a folder that is gone. A new spelling of a series
/// still listed joins it and owns nothing. Blocking.
pub fn folder_created(db: &Database, library: &Path, rel: &str, actor: &str) {
    let Some(folder) = top_level(rel) else {
        return;
    };
    if variant_listed(library, &fold_series_title_key(folder), &[folder]) {
        return;
    }
    log("record", db.record_series_folder_owner(folder, actor));
}

/// The library folder `rel` went: its series' record goes with its last spelling.
/// Blocking.
pub fn folder_removed(db: &Database, library: &Path, rel: &str) {
    let Some(folder) = top_level(rel) else {
        return;
    };
    if variant_listed(library, &fold_series_title_key(folder), &[folder]) {
        return;
    }
    log("forget", db.forget_series_folder_owner(folder));
}

/// The library folder `old` is now `new` (both library rels). Blocking.
pub fn folder_moved(db: &Database, library: &Path, old: &str, new: &str) {
    match (top_level(old), top_level(new)) {
        (Some(old), Some(new)) => {
            let others = [old, new];
            let old_lives_on = variant_listed(library, &fold_series_title_key(old), &others);
            let joins = variant_listed(library, &fold_series_title_key(new), &others);
            log(
                "move",
                db.move_series_folder_owner(old, new, old_lives_on, joins),
            );
        }
        (Some(old), None) => folder_removed(db, library, old),
        // A folder moved up to the top: a record its new name kept from a folder that
        // is gone does not make it anyone's.
        (None, Some(new)) => {
            if !variant_listed(library, &fold_series_title_key(new), &[new]) {
                log("forget", db.forget_series_folder_owner(new));
            }
        }
        (None, None) => {}
    }
}

/// Every archive below `dir`, as library rels (`<folder>/.../<name>`). Blocking.
fn archives_under(library: &Path, folder: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![folder.to_string()];
    while let Some(rel) = stack.pop() {
        let Ok(entries) = bunko_store::fs::read_dir(library.join(&rel)) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(name) = e.file_name().into_string() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let child = format!("{rel}/{name}");
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(child),
                Ok(_)
                    if ARCHIVE_EXTENSIONS
                        .iter()
                        .any(|ext| strip_suffix_ci(&name, ext).is_some()) =>
                {
                    out.push(child)
                }
                _ => {}
            }
        }
    }
    out
}

/// Give each series folder that existed before the owner records the one uploader of
/// its tracked volumes, when no archive in it is untracked (any archive type counts:
/// only `.cbz` uploads are tracked, so a folder holding another is left shared). Once:
/// `None` when it was done already, or cannot be done now (the root cannot be listed,
/// the database failed), in which case the next start tries again. Blocking.
pub fn adopt_series_folder_owners(db: &Database, library: &Path) -> Option<FolderAdoption> {
    match db.series_folder_adoption_pending() {
        Ok(true) => {}
        Ok(false) => return None,
        Err(e) => {
            tracing::warn!("[LIBRARY] series folder adoption: {e}");
            return None;
        }
    }
    let Some(folders) = series_folders(library) else {
        tracing::warn!("[LIBRARY] series folder adoption waits: the library root cannot be listed");
        return None;
    };
    let tracked = match db.tracked_volume_uploaders() {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("[LIBRARY] series folder adoption: {e}");
            return None;
        }
    };
    let tracked_keys: HashSet<&str> = tracked.iter().map(|(k, _)| k.as_str()).collect();
    // Per series key: the uploaders (ignoring case, first spelling kept) and the
    // folder spellings that hold a tracked volume.
    let mut uploaders: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut holding: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (key, user) in &tracked {
        let Some((folder, _)) = key.split_once('/') else {
            continue;
        };
        let series = fold_series_title_key(folder);
        uploaders
            .entry(series.clone())
            .or_default()
            .entry(user.to_ascii_lowercase())
            .or_insert_with(|| user.clone());
        holding
            .entry(series)
            .or_default()
            .insert(folder.to_string());
    }
    let mut by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in folders {
        by_key
            .entry(fold_series_title_key(&name))
            .or_default()
            .push(name);
    }
    let mut owners: Vec<(String, String)> = Vec::new();
    let mut unowned = 0;
    for (key, names) in &by_key {
        let only = uploaders
            .get(key)
            .filter(|u| u.len() == 1)
            .and_then(|u| u.values().next());
        let spelling = holding
            .get(key)
            .and_then(|held| names.iter().filter(|n| held.contains(*n)).min());
        let all_tracked = || {
            names.iter().all(|n| {
                archives_under(library, n)
                    .iter()
                    .all(|a| tracked_keys.contains(a.as_str()))
            })
        };
        match (only, spelling) {
            (Some(user), Some(folder)) if all_tracked() => {
                owners.push((folder.clone(), user.clone()));
            }
            _ => unowned += 1,
        }
    }
    match db.finish_series_folder_adoption(&owners) {
        Ok(owned) => {
            tracing::info!(
                "[LIBRARY] {owned} series folders now belong to their only uploader; {unowned} have no owner"
            );
            Some(FolderAdoption { owned, unowned })
        }
        Err(e) => {
            tracing::warn!("[LIBRARY] series folder adoption: {e}");
            None
        }
    }
}
