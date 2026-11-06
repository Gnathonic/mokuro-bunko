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
//! that nothing acts on, and the next creator of the folder replaces it. So a hook
//! that cannot list the library root keeps a record rather than forget it, and gives
//! no folder an owner it could not check: a stale record is inert, a wrong one is not.
//!
//! Bunko's own writes (the processor, the compiler, covers, the inbox, a duplicate
//! merge) never call these: they assign no owner.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bunko_core::reader_tree;
use bunko_core::sidecar_names::{ARCHIVE_EXTENSIONS, strip_suffix_ci};
use bunko_db::{Database, fold_series_title_key};
use tokio_util::sync::CancellationToken;

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
    !reader_tree::is_per_user_root_name(name)
        && !name.eq_ignore_ascii_case("thumbnails")
        && !name.starts_with('.')
}

/// The series folders listed at the library root (through the store: an index read on
/// a cloud library); `Err` when the root, or any entry of it, cannot be read: a listing
/// missing a folder would read its series as unowned. Blocking.
fn series_folders(library: &Path) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in bunko_store::fs::read_dir(library)? {
        let entry = entry?;
        let is_dir = match entry.file_type() {
            Ok(t) => t.is_dir(),
            // Gone since the listing: no folder.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if let (true, Ok(name)) = (is_dir, entry.file_name().into_string())
            && is_series_folder_name(&name)
        {
            out.push(name);
        }
    }
    // A folder another listed spelling shadows is no series anyone reaches.
    let losers = bunko_library::fsutil::case_losers(library, out.iter().map(String::as_str));
    out.retain(|n| !losers.contains(n));
    Ok(out)
}

/// One segment, no more: a series folder's library rel.
fn top_level(rel: &str) -> Option<&str> {
    let rel = rel.trim_matches('/');
    (!rel.is_empty() && !rel.contains('/')).then_some(rel)
}

/// The series keys of the folders listed at the library root. Blocking.
pub fn live_series_keys(library: &Path) -> std::io::Result<HashSet<String>> {
    Ok(series_folders(library)?
        .iter()
        .map(|n| fold_series_title_key(n))
        .collect())
}

/// Is a folder of `folder`'s series listed at the library root? The folder itself is
/// one lookup; any other spelling takes the root listing, and `Err` when that cannot
/// be read: "not listed" would make an owned series anyone's. Blocking.
pub fn series_folder_is_live(library: &Path, folder: &str) -> std::io::Result<bool> {
    let Some(folder) = top_level(folder) else {
        return Ok(false);
    };
    if is_series_folder_name(folder) && bunko_store::fs::is_dir(library.join(folder)) {
        return Ok(true);
    }
    let key = fold_series_title_key(folder);
    Ok(live_series_keys(library)?.contains(&key))
}

/// A folder of the series `key` other than the names in `except` is listed (`Err`:
/// the root cannot be listed).
fn variant_listed(library: &Path, key: &str, except: &[&str]) -> std::io::Result<bool> {
    Ok(series_folders(library)?
        .iter()
        .any(|n| !except.contains(&n.as_str()) && fold_series_title_key(n) == key))
}

/// [`variant_listed`], logged when it cannot tell.
fn variant_known(library: &Path, key: &str, except: &[&str]) -> Option<bool> {
    variant_listed(library, key, except)
        .inspect_err(|e| {
            tracing::warn!("[LIBRARY] series folder owner: the library root cannot be listed: {e}")
        })
        .ok()
}

fn log<T>(what: &str, r: bunko_db::Result<T>) {
    if let Err(e) = r {
        tracing::warn!("[LIBRARY] series folder owner: {what} failed: {e}");
    }
}

/// A client's MKCOL created the series folder `rel`: `actor` owns it, replacing a
/// record its series kept from a folder that is gone. A new spelling of a series
/// still listed joins it and owns nothing, and so does one made while the root cannot
/// be listed: the series may be listed, and owning it would hand `actor` every other
/// account's volumes in it. Blocking.
pub fn folder_created(db: &Database, library: &Path, rel: &str, actor: &str) {
    let Some(folder) = top_level(rel) else {
        return;
    };
    if variant_known(library, &fold_series_title_key(folder), &[folder]) != Some(false) {
        return;
    }
    log("record", db.record_series_folder_owner(folder, actor));
}

/// The library folder `rel` went: its series' record goes with its last spelling. One
/// that cannot be told the last stays (inert while no spelling is listed). Blocking.
pub fn folder_removed(db: &Database, library: &Path, rel: &str) {
    let Some(folder) = top_level(rel) else {
        return;
    };
    if variant_known(library, &fold_series_title_key(folder), &[folder]) != Some(false) {
        return;
    }
    log("forget", db.forget_series_folder_owner(folder));
}

/// The uploaders of the tracked volumes under the library folder `rel`.
fn uploaders_under(db: &Database, rel: &str) -> Vec<String> {
    let prefix = format!("{}/", rel.trim_matches('/'));
    match db.tracked_volume_uploaders() {
        Ok(rows) => rows
            .into_iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(_, user)| user)
            .collect(),
        Err(e) => {
            tracing::warn!("[LIBRARY] series folder owner: tracked volumes: {e}");
            // Unknown: as if another account's, so a joined series keeps no owner.
            vec![String::new()]
        }
    }
}

/// The library folder `old` is now `new` (both library rels), its volumes' upload
/// records already moved with it. Joining a series still listed under another
/// spelling, it keeps that series' owner only when it was that account's too (S1,
/// OWN-5). Blocking.
///
/// When the root cannot be listed, `old`'s record stays (inert if no spelling is
/// listed) and `new` is taken to join a series of unknown uploaders: it keeps an owner
/// only when it was that owner's already, and never gets one it could not check.
pub fn folder_moved(db: &Database, library: &Path, old: &str, new: &str) {
    // An uploader no account is: a join with it agrees with no owner but a record.
    let unknown = || vec![String::new()];
    match (top_level(old), top_level(new)) {
        (Some(old), Some(new)) => {
            let others = [old, new];
            let old_lives_on =
                variant_known(library, &fold_series_title_key(old), &others) != Some(false);
            let joins = match variant_known(library, &fold_series_title_key(new), &others) {
                Some(true) => Some(uploaders_under(db, new)),
                Some(false) => None,
                None => Some(unknown()),
            };
            log(
                "move",
                db.move_series_folder_owner(old, new, old_lives_on, joins.as_deref()),
            );
        }
        (Some(old), None) => folder_removed(db, library, old),
        (None, Some(new)) => {
            if variant_known(library, &fold_series_title_key(new), &[new]) != Some(false) {
                log(
                    "join",
                    db.series_folder_joined(new, &uploaders_under(db, new)),
                );
            } else {
                // A record its new name kept from a folder that is gone does not make
                // it anyone's.
                log("forget", db.forget_series_folder_owner(new));
            }
        }
        (None, None) => {}
    }
}

/// What a series folder holds, for adoption.
#[derive(Default)]
struct Contents {
    /// Every archive below it, as library rels (`<folder>/.../<name>`).
    archives: Vec<String>,
    /// It holds something adoption cannot vouch for: a name that is not UTF-8, or a
    /// dot-named archive or folder (OWN-7). Bunko's own staging names are skipped.
    opaque: bool,
}

/// The staging files of in-flight uploads (`.<name>.upload-<random>.tmp`,
/// `bunko_dav::paths::is_staging_name`).
fn is_staging_name(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(".tmp") && name.contains(".upload-")
}

fn is_archive_name(name: &str) -> bool {
    ARCHIVE_EXTENSIONS
        .iter()
        .any(|ext| strip_suffix_ci(name, ext).is_some())
}

/// The [`Contents`] of a series folder, walked through the store. Blocking.
fn contents_of(library: &Path, folder: &str) -> Contents {
    let mut out = Contents::default();
    let mut stack = vec![folder.to_string()];
    while let Some(rel) = stack.pop() {
        let Ok(entries) = bunko_store::fs::read_dir(library.join(&rel)) else {
            out.opaque = true;
            continue;
        };
        for e in entries {
            let Ok(e) = e else {
                out.opaque = true;
                continue;
            };
            let Ok(name) = e.file_name().into_string() else {
                out.opaque = true;
                continue;
            };
            if is_staging_name(&name) {
                continue;
            }
            let is_dir = match e.file_type() {
                Ok(t) => t.is_dir(),
                Err(_) => {
                    out.opaque = true;
                    continue;
                }
            };
            if name.starts_with('.') {
                // A hidden file that is no archive (`.DS_Store`) holds no volume.
                out.opaque |= is_dir || is_archive_name(&name);
                continue;
            }
            let child = format!("{rel}/{name}");
            if is_dir {
                stack.push(child);
            } else if is_archive_name(&name) {
                out.archives.push(child);
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
    let folders = match series_folders(library) {
        Ok(folders) => folders,
        Err(e) => {
            tracing::warn!(
                "[LIBRARY] series folder adoption waits: the library root cannot be listed: {e}"
            );
            return None;
        }
    };
    let tracked = match db.tracked_volume_uploaders() {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("[LIBRARY] series folder adoption: {e}");
            return None;
        }
    };
    // Only rows whose archive is listed count (F3): a row left by a volume removed
    // outside WebDAV neither gives an empty folder away nor makes a folder mixed.
    let uploader_of: HashMap<&str, &str> = tracked
        .iter()
        .map(|(k, u)| (k.as_str(), u.as_str()))
        .collect();
    // An account's name as its first tracked row spells it.
    let mut spelled: HashMap<String, &str> = HashMap::new();
    for (_, user) in &tracked {
        spelled.entry(user.to_ascii_lowercase()).or_insert(user);
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
    for names in by_key.values() {
        // The uploaders (ignoring case, first spelling kept), the spellings holding a
        // tracked volume, and whether anything in them is not a tracked volume.
        let mut uploaders: BTreeMap<String, &str> = BTreeMap::new();
        let mut holding: BTreeSet<&str> = BTreeSet::new();
        let mut all_tracked = true;
        for name in names {
            let contents = contents_of(library, name);
            all_tracked &= !contents.opaque;
            for archive in &contents.archives {
                match uploader_of.get(archive.as_str()) {
                    Some(user) => {
                        let lower = user.to_ascii_lowercase();
                        let user = spelled.get(&lower).copied().unwrap_or(user);
                        uploaders.entry(lower).or_insert(user);
                        holding.insert(name.as_str());
                    }
                    None => all_tracked = false,
                }
            }
        }
        match (uploaders.len(), holding.first()) {
            (1, Some(folder)) if all_tracked => {
                let user = uploaders.values().next().copied().unwrap_or_default();
                owners.push((folder.to_string(), user.to_string()));
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

/// [`adopt_series_folder_owners`], tried again `every` while it cannot be done (the
/// root cannot be listed, the database failed): until it is, uploaders' additions wait
/// (auth answers 503), so a failed first try must not hold them for the life of the
/// process (F4). Returns once adoption is done, or on `stop`.
pub async fn adopt_series_folder_owners_until_done(
    db: Arc<Database>,
    library: PathBuf,
    every: Duration,
    stop: CancellationToken,
) {
    loop {
        let (d, lib) = (db.clone(), library.clone());
        let done = tokio::task::spawn_blocking(move || {
            adopt_series_folder_owners(&d, &lib);
            d.series_folder_adoption_pending()
                .is_ok_and(|pending| !pending)
        })
        .await
        .unwrap_or(false);
        if done {
            return;
        }
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(every) => {}
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Runs `f` with the library root searchable but not listable; `false` when the
    /// root could list it anyway (the root user reads anything).
    fn unlistable(library: &Path, f: impl FnOnce()) -> bool {
        std::fs::set_permissions(library, std::fs::Permissions::from_mode(0o311)).unwrap();
        let listable = bunko_store::fs::read_dir(library).is_ok();
        f();
        std::fs::set_permissions(library, std::fs::Permissions::from_mode(0o755)).unwrap();
        !listable
    }

    fn library(folders: &[&str]) -> (tempfile::TempDir, PathBuf, Database) {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library");
        for folder in folders {
            bunko_store::fs::create_dir_all(library.join(folder)).unwrap();
        }
        let db = Database::open(dir.path().join("mokuro.db")).unwrap();
        (dir, library, db)
    }

    /// A folder made while the root cannot be listed may be a spelling of a listed
    /// series: it takes no owner, and the series keeps its own.
    #[test]
    fn a_folder_made_unseen_owns_nothing() {
        let (_dir, library, db) = library(&["S", "s", "New"]);
        db.record_series_folder_owner("S", "alice").unwrap();
        let checked = unlistable(&library, || {
            folder_created(&db, &library, "s", "bob");
            folder_created(&db, &library, "New", "bob");
        });
        if checked {
            assert_eq!(
                db.series_folder_owner("S").unwrap().as_deref(),
                Some("alice")
            );
            assert_eq!(db.series_folder_owner("New").unwrap(), None);
        }
        // Seen, a new series is its creator's.
        folder_created(&db, &library, "New", "bob");
        assert_eq!(
            db.series_folder_owner("New").unwrap().as_deref(),
            Some("bob")
        );
    }

    /// A removal that cannot tell whether it took the last spelling keeps the record.
    #[test]
    fn a_folder_removed_unseen_keeps_its_record() {
        let (_dir, library, db) = library(&["S"]);
        db.record_series_folder_owner("S", "alice").unwrap();
        let checked = unlistable(&library, || folder_removed(&db, &library, "s"));
        if checked {
            assert_eq!(
                db.series_folder_owner("S").unwrap().as_deref(),
                Some("alice")
            );
        }
    }

    /// A move that cannot tell whether it joins a listed series hands that series no
    /// owner, and keeps the moved folder's record where it was.
    #[test]
    fn a_folder_moved_unseen_hands_over_nothing() {
        let (_dir, library, db) = library(&["t"]);
        db.record_series_folder_owner("S", "alice").unwrap();
        let checked = unlistable(&library, || folder_moved(&db, &library, "S", "T"));
        if checked {
            assert_eq!(db.series_folder_owner("T").unwrap(), None);
            assert_eq!(
                db.series_folder_owner("S").unwrap().as_deref(),
                Some("alice")
            );
        }
    }
}
