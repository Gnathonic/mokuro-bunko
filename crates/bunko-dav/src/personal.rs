//! Personal correction layers (D27): `<Vol>.edit[-n].mokuro` is one account's own.
//!
//! A library path whose name reads as a personal layer (exact id, sidecar names rule
//! v1) and is no listed archive's primary resolves per caller: the cloud-root owner's
//! is the physical file beside the archive (a cloud-direct reader shares it), any other
//! account's is its overlay `users/<u>/edits/<library rel>`. Nobody sees another
//! account's copy, anonymous sees none. A listing shows one only beside its `.cbz`.
//!
//! Overlays follow their volume ([`PersonalOverlays`]): an archive or folder that moves
//! or merges takes every account's overlays along, a collision moving in as the next
//! free `edit-<n>`; a deleted archive or folder takes them with it. An archive renamed
//! over DAV also takes the cloud-root owner's physical correction along.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use bunko_core::reader_tree;
use bunko_core::sidecar_names::{self, LayerClass, SidecarClass};
use bunko_store::fs;

use crate::paths;
use crate::write_locks::PathWriteLocks;

/// Below `users/<u>/`: the account's overlays, laid out like the library.
pub const EDITS_DIR: &str = "edits";

/// A correction (or curated) layer PUT is at most this big.
pub const MAX_LAYER_BYTES: u64 = 32 << 20;

/// `(stem, id, gz)` when `name` reads as a personal layer, listing-free.
pub(crate) fn personal_name(name: &str) -> Option<(&str, &str, bool)> {
    sidecar_names::split_layer(name)
        .filter(|(_, id, _)| sidecar_names::layer_class(id) == LayerClass::Personal)
}

/// The names of a library folder's FILES (through the store: an index read on a cloud
/// library), staging files left out. A folder is never an archive nor a sidecar (rule
/// 2): a directory named `V.edit.cbz` makes no `V.edit.mokuro` a primary. Blocking.
pub(crate) fn names_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| !e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !paths::is_staging_name(n))
        .collect()
}

/// The folded `.cbz` stems of a folder's names (rule 2).
pub(crate) fn stems_of(names: &[String]) -> HashSet<String> {
    sidecar_names::volume_stems(names.iter().map(String::as_str))
}

/// `users/<u>/edits`, or `None` for a username that cannot be a directory.
pub(crate) fn edits_base(users: &Path, username: &str) -> Option<PathBuf> {
    reader_tree::safe_username(username).then(|| users.join(username).join(EDITS_DIR))
}

/// `users/<u>/edits/<rel>`, which must stay inside `users/<u>/edits` (symlinks followed).
/// `rel` is a library rel in the library's own spelling. Every segment that names an
/// existing overlay folder or file in any spelling takes its listed spelling, so one
/// volume's correction is one file on every host, whatever case the library or the
/// request spells it in; except an overlay spelling the library lists as an entry of its
/// own (a case variant a case-sensitive disk holds, shadowed): those are that entry's
/// corrections, never another's ([`resolve_existing`]).
pub(crate) fn overlay_path(
    library: &Path,
    users: &Path,
    username: &str,
    rel: &str,
) -> Option<PathBuf> {
    let base = edits_base(users, username)?;
    let rel = resolve_existing(library, &base, rel)?;
    let resolved = paths::resolve_under(users, &format!("{username}/{EDITS_DIR}/{rel}"))?;
    resolved.starts_with(&base).then_some(resolved)
}

/// `rel` (a library rel, each segment the library's listed spelling of the entry, or
/// of one gone) below the overlay base `base`, every segment that names an existing
/// overlay entry in its listed spelling (any case). An overlay folder spelled otherwise
/// than the segment that the library lists, beside the segment, as a folder of its own
/// belongs to that folder and is never taken: on a case-sensitive disk holding both
/// `Kingdom/` and `kingdom/`, `edits/kingdom/` stays `kingdom/`'s whatever happens to
/// `Kingdom/`, before or after it goes. Decided from listings only, so every host
/// answers alike.
///
/// `None` while such a folder is the only overlay folder of that name: the segment's
/// own would be a second spelling of it, which the disk under `users/` may fold into
/// that very folder (APFS, NTFS, casefold). Nothing is read, written, moved or deleted
/// there for the segment's entry until the shadowed folder is merged (or gone, when
/// its overlay folder becomes the name's).
fn resolve_existing(library: &Path, base: &Path, rel: &str) -> Option<String> {
    let mut parts: Vec<String> = rel.split('/').map(str::to_string).collect();
    if parts.iter().any(|p| p == "." || p == "..") {
        return Some(rel.to_string());
    }
    let mut library_dir = Some(library.to_path_buf());
    let mut overlay_dir = base.to_path_buf();
    for part in parts.iter_mut() {
        let segment = part.clone();
        if segment.is_empty() {
            continue;
        }
        // The library's folders here (an overlay folder mirrors one; overlay files are
        // matched to archives by rule v1, [`PersonalOverlays::owned`]).
        let listed_here: Vec<String> = library_dir
            .as_deref()
            .and_then(|d| fs::read_dir(d).ok())
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        let folded = crate::fold_name(&segment);
        let alike: Vec<String> = fs::read_dir(&overlay_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| *n == segment || crate::fold_name(n) == folded)
                    .collect()
            })
            .unwrap_or_default();
        let (mut ours, theirs): (Vec<String>, Vec<String>) = alike
            .into_iter()
            .partition(|n| *n == segment || !listed_here.contains(n));
        if ours.is_empty() && !theirs.is_empty() {
            return None;
        }
        ours.sort();
        let chosen = if ours.contains(&segment) {
            Some(segment.clone())
        } else {
            ours.into_iter().next()
        };
        library_dir = library_dir.map(|d| d.join(&segment));
        let Some(chosen) = chosen else {
            break;
        };
        overlay_dir.push(&chosen);
        *part = chosen;
    }
    Some(parts.join("/"))
}

/// A library file that is a personal correction.
#[derive(Debug, Clone)]
pub(crate) struct PersonalFile {
    /// Library-relative, the folder as resolved on disk. Its archive may be missing:
    /// such a correction is never listed nor written, but it is still the caller's.
    pub rel: String,
}

/// What a PUT of `name` into the library folder `names` must pass (D27, plan §1
/// "Overlay and curated PUT"): `None` for a file the rules do not cover.
pub(crate) fn layer_put_rule(name: &str, names: &[String]) -> Option<Result<(), LayerRefusal>> {
    let (_, id, gz) = sidecar_names::split_layer(name)?;
    if !matches!(
        sidecar_names::layer_class(id),
        LayerClass::Personal | LayerClass::Curated
    ) {
        return None;
    }
    let stems = stems_of(names);
    Some(match sidecar_names::classify(name, &stems) {
        // A dotted volume's own primary (rule 4).
        SidecarClass::Primary { .. } => return None,
        _ if gz => Err(LayerRefusal::Gzip),
        // Stored as written: one spelling per account and volume (critique C18).
        _ if !name.ends_with(".mokuro") => Err(LayerRefusal::NoVolume),
        SidecarClass::Layer { .. } => Ok(()),
        SidecarClass::Orphan => Err(LayerRefusal::NoVolume),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayerRefusal {
    Gzip,
    NoVolume,
}

/// Is this body a correction layer: a JSON object with a `pages` array and no
/// `ocr_engine` (an edit is never model output)? A leading BOM is tolerated (rule 7).
pub(crate) fn is_layer_body(bytes: &[u8]) -> bool {
    use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

    struct Probe {
        pages: bool,
        engine: bool,
    }
    struct Pages;
    impl<'de> serde::Deserialize<'de> for Pages {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = Pages;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("an array")
                }
                fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Pages, A::Error> {
                    while seq.next_element::<IgnoredAny>()?.is_some() {}
                    Ok(Pages)
                }
            }
            d.deserialize_seq(V)
        }
    }
    impl<'de> serde::Deserialize<'de> for Probe {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = Probe;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("an object")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Probe, A::Error> {
                    let mut probe = Probe {
                        pages: false,
                        engine: false,
                    };
                    while let Some(key) = map.next_key::<String>()? {
                        match key.as_str() {
                            "pages" => {
                                map.next_value::<Pages>()?;
                                probe.pages = true;
                            }
                            "ocr_engine" => {
                                map.next_value::<IgnoredAny>()?;
                                probe.engine = true;
                            }
                            _ => {
                                map.next_value::<IgnoredAny>()?;
                            }
                        }
                    }
                    Ok(probe)
                }
            }
            d.deserialize_map(V)
        }
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    serde_json::from_slice::<Probe>(bytes).is_ok_and(|p| p.pages && !p.engine)
}

/// Every account's overlays, kept with the volumes they correct. Blocking.
#[derive(Debug, Clone)]
pub struct PersonalOverlays {
    library: PathBuf,
    users: PathBuf,
    locks: PathWriteLocks,
}

impl PersonalOverlays {
    pub(crate) fn new(library: PathBuf, users: PathBuf, locks: PathWriteLocks) -> Self {
        Self {
            library,
            users,
            locks,
        }
    }

    /// Each account's `edits` directory.
    fn bases(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.users) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path().join(EDITS_DIR))
            .filter(|p| fs::is_dir(p))
            .collect()
    }

    /// `folder/stem` of an archive's library rel (`None`: no `.cbz`).
    fn split_archive(rel: &str) -> Option<(&str, &str)> {
        let rel = rel.trim_matches('/');
        let (folder, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        Some((folder, sidecar_names::cbz_stem(name)?))
    }

    /// The overlays in `dir` that belong to the archive `cbz_name` of the library
    /// folder `folder_rel`, as rule v1 sweeps them: an exact stem, or a folded one when
    /// no other archive there has that spelling.
    fn owned(&self, dir: &Path, folder_rel: &str, cbz_name: &str) -> Vec<(String, String)> {
        let overlays: Vec<String> = names_in(dir)
            .into_iter()
            .filter(|n| personal_name(n).is_some())
            .collect();
        if overlays.is_empty() {
            return Vec::new();
        }
        let folder = self.library.join(folder_rel);
        let mut names: Vec<String> = names_in(&folder)
            .into_iter()
            .filter(|n| {
                sidecar_names::ARCHIVE_EXTENSIONS
                    .iter()
                    .any(|e| sidecar_names::strip_suffix_ci(n, e).is_some())
            })
            .collect();
        // The archive itself may already have gone (a delete) or moved (a rename).
        if !names.iter().any(|n| n == cbz_name) {
            names.push(cbz_name.to_string());
        }
        names.extend(overlays.iter().cloned());
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let listing = sidecar_names::Folder::new(&refs);
        listing
            .volume_files(cbz_name)
            .into_iter()
            .filter_map(|(name, file)| match file {
                sidecar_names::VolumeFile::Layer { id, .. }
                    if overlays.iter().any(|o| o == name) =>
                {
                    Some((name.to_string(), id.to_string()))
                }
                _ => None,
            })
            .collect()
    }

    /// Move one overlay into `to_dir` as `<stem>.<id>.mokuro`, or the next free
    /// `edit-<n>` when that is taken in any spelling (merge.rs `free_layer`: nothing is
    /// lost). The destination when it moved, or the source itself when it is already
    /// there under that name.
    fn move_one(&self, from: &Path, to_dir: &Path, stem: &str, id: &str) -> Option<PathBuf> {
        if fs::create_dir_all(to_dir).is_err() {
            return None;
        }
        for n in 1u32.. {
            let id = match n {
                1 => id.to_string(),
                n => format!("edit-{n}"),
            };
            let dest = to_dir.join(format!("{stem}.{id}.mokuro"));
            // Already there: the same file under the same name.
            if dest.file_name() == from.file_name() && crate::path_case::same_file(from, &dest) {
                return Some(from.to_path_buf());
            }
            // Free in every spelling, or taken only by the source itself (a case-only
            // rename, which `fs::rename` carries out on every host).
            let free = |dest: &Path| {
                let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("");
                match crate::path_case::on_disk_name(to_dir, name, false) {
                    None => true,
                    Some(listed) => crate::path_case::same_file(from, &to_dir.join(listed)),
                }
            };
            if !free(&dest) {
                continue;
            }
            let Some(_guard) = self.locks.try_lock_all(&[from, &dest]) else {
                tracing::warn!(
                    "[DAV] a correction is being written: {} stays where it is",
                    from.display()
                );
                return None;
            };
            // Taken meanwhile (a PUT that held its lock): the next one.
            if !free(&dest) {
                continue;
            }
            return match fs::rename(from, &dest) {
                Ok(()) => Some(dest),
                Err(e) => {
                    tracing::warn!("[DAV] moving the correction {}: {e}", from.display());
                    None
                }
            };
        }
        None
    }

    /// The library rels of the physical correction files: names that read as a personal
    /// layer and are no listed archive's primary (rule 4). On a library on this disk,
    /// and in a cloud library with no progress user, nobody sees them. Blocking (an
    /// index walk on a cloud library).
    pub fn physical_corrections(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut todo = vec![String::new()];
        while let Some(rel) = todo.pop() {
            let dir = self.library.join(&rel);
            let names = names_in(&dir);
            let stems = stems_of(&names);
            let child_of = |name: &str| {
                if rel.is_empty() {
                    name.to_string()
                } else {
                    format!("{rel}/{name}")
                }
            };
            for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let child = child_of(&entry.file_name().to_string_lossy());
                let path = self.library.join(&child);
                // The cloud-root owner's own tree holds no volume, and a symlinked
                // folder is not walked (the index's scan does not either; a link up
                // the tree would loop).
                let linked = fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
                if !linked && !reader_tree::is_per_user_rel(&child) {
                    todo.push(child);
                }
            }
            for name in &names {
                let child = child_of(name);
                if personal_name(name).is_some()
                    && !matches!(
                        sidecar_names::classify(name, &stems),
                        SidecarClass::Primary { .. }
                    )
                {
                    out.push(child);
                }
            }
        }
        out.sort();
        out
    }

    /// Give the physical correction `rel` to `username`: it moves to the same place in
    /// that account's overlays, as the next free `edit-<n>` when the account already
    /// has one there. False when it stayed (a write in progress, a bad name).
    pub fn adopt(&self, rel: &str, username: &str) -> bool {
        let rel = rel.trim_matches('/');
        let (folder, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        let Some((stem, id, false)) = personal_name(name) else {
            return false;
        };
        let Some(base) = edits_base(&self.users, username) else {
            return false;
        };
        let to_dir = if folder.is_empty() {
            base
        } else {
            match overlay_path(&self.library, &self.users, username, folder) {
                Some(dir) => dir,
                None => return false,
            }
        };
        self.move_one(&self.library.join(rel), &to_dir, stem, id)
            .is_some()
    }

    /// The archive `old` (library rel) is now `new`: each account's overlays of it move
    /// along (renamed to the new stem).
    pub fn rename_volume(&self, old: &str, new: &str) {
        if old == new {
            return;
        }
        let (Some((old_folder, _)), Some((new_folder, new_stem))) =
            (Self::split_archive(old), Self::split_archive(new))
        else {
            return;
        };
        let old_name = paths::uri_name(old).to_string();
        for base in self.bases() {
            let Some(from_rel) = resolve_existing(&self.library, &base, old_folder) else {
                continue;
            };
            let dir = base.join(from_rel);
            let owned = self.owned(&dir, old_folder, &old_name);
            if owned.is_empty() {
                continue;
            }
            // Where the new folder's overlays would collide with a shadowed folder's,
            // they stay where they are (still found once that folder is gone).
            let Some(to_rel) = resolve_existing(&self.library, &base, new_folder) else {
                continue;
            };
            let to_dir = base.join(&to_rel);
            for (name, id) in &owned {
                self.move_one(&dir.join(name), &to_dir, new_stem, id);
            }
            if !old_folder.is_empty() && dir != to_dir {
                self.drop_if_empty(&dir);
            }
            // The receiving folder takes the library's spelling (a case merge or a case
            // fix of the folder): the overlay tree stays aligned with the library.
            self.respell(&base, &to_rel, new_folder);
        }
    }

    /// Rename the overlay folders of `have` (below `base`, listed spellings) whose
    /// spelling differs from `want`'s only in case, segment by segment, to `want`'s
    /// spelling. A distinct folder already spelled that way (a case-sensitive disk)
    /// takes the other's overlays instead ([`Self::merge_dir`]).
    fn respell(&self, base: &Path, have: &str, want: &str) {
        let have: Vec<&str> = have.split('/').filter(|s| !s.is_empty()).collect();
        let want: Vec<&str> = want.split('/').filter(|s| !s.is_empty()).collect();
        if have.len() != want.len() {
            return;
        }
        let mut parent = base.to_path_buf();
        for (h, w) in have.iter().zip(&want) {
            if h != w && crate::fold_name(h) == crate::fold_name(w) {
                let (from, to) = (parent.join(h), parent.join(w));
                if fs::exists(&to) && !crate::path_case::same_file(&from, &to) {
                    self.merge_dir(&from, &to);
                } else {
                    let Some(_guard) = self.locks.try_lock_all(&[&from, &to]) else {
                        tracing::warn!(
                            "[DAV] corrections are being written under {}: its spelling stays",
                            from.display()
                        );
                        return;
                    };
                    if let Err(e) = fs::rename(&from, &to) {
                        tracing::warn!(
                            "[DAV] respelling the corrections folder {}: {e}",
                            from.display()
                        );
                        return;
                    }
                }
            }
            parent.push(w);
        }
    }

    /// The archive `old` (library rel) was renamed to `new` over DAV: the cloud-root
    /// owner's corrections of it, the physical files beside it, move along like the
    /// overlays do. Nobody else is shown them, so the renamer's reader cannot move them,
    /// and sidecars stay pinned. Not for a rename made in the cloud itself: a
    /// cloud-direct owner's reader moves its own files there.
    pub fn rename_physical(&self, old: &str, new: &str) {
        if old == new {
            return;
        }
        let (Some((old_folder, _)), Some((new_folder, new_stem))) =
            (Self::split_archive(old), Self::split_archive(new))
        else {
            return;
        };
        let dir = self.library.join(old_folder);
        for (name, id) in self.owned(&dir, old_folder, paths::uri_name(old)) {
            self.move_one(
                &dir.join(&name),
                &self.library.join(new_folder),
                new_stem,
                &id,
            );
        }
    }

    /// Remove an overlay folder left empty (under its lock: a PUT into it holds its
    /// file's lock before it creates the folder).
    fn drop_if_empty(&self, dir: &Path) {
        if let Some(_guard) = self.locks.try_lock(dir) {
            let _ = fs::remove_dir(dir);
        }
    }

    /// The archive `rel` is gone: so are each account's overlays of it.
    pub fn forget_volume(&self, rel: &str) {
        let Some((folder, _)) = Self::split_archive(rel) else {
            return;
        };
        let name = paths::uri_name(rel).to_string();
        for base in self.bases() {
            let Some(rel) = resolve_existing(&self.library, &base, folder) else {
                continue;
            };
            let dir = base.join(rel);
            let owned = self.owned(&dir, folder, &name);
            for (file, _) in &owned {
                let path = dir.join(file);
                match self.locks.try_lock(&path) {
                    Some(_guard) => {
                        let _ = fs::remove_file(&path);
                    }
                    None => tracing::warn!(
                        "[DAV] a correction is being written: {} is kept",
                        path.display()
                    ),
                }
            }
            if !owned.is_empty() && !folder.is_empty() {
                self.drop_if_empty(&dir);
            }
        }
    }

    /// The folder `old` (library rel) is now `new`: each account's overlays below it
    /// move along, merging into what is already there.
    pub fn rename_prefix(&self, old: &str, new: &str) {
        let (old, new) = (old.trim_matches('/'), new.trim_matches('/'));
        if old == new || old.is_empty() || new.is_empty() {
            return;
        }
        let (new_parent, new_name) = new.rsplit_once('/').unwrap_or(("", new));
        for base in self.bases() {
            let Some(from) = resolve_existing(&self.library, &base, old) else {
                continue;
            };
            let from = base.join(from);
            if !fs::is_dir(&from) {
                continue;
            }
            // Into an existing folder of the new name in any spelling (never a shadowed
            // folder's), unless that is the source itself (a case fix: renamed to the
            // new spelling).
            let Some(to_parent) = resolve_existing(&self.library, &base, new_parent) else {
                continue;
            };
            let to_parent = base.join(to_parent);
            let to = match resolve_existing(&self.library, &base, new) {
                Some(to_rel) if !crate::path_case::same_file(&from, &base.join(&to_rel)) => {
                    base.join(to_rel)
                }
                Some(_) => to_parent.join(new_name),
                // Only the source's own folder holds the new name in another spelling
                // (a case fix, or a case merge whose losing folder is gone): renamed.
                None if from.parent() == Some(to_parent.as_path())
                    && from
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| crate::fold_name(n) == crate::fold_name(new_name)) =>
                {
                    to_parent.join(new_name)
                }
                None => continue,
            };
            // Free, or the same directory spelled differently (a case-insensitive disk):
            // one rename.
            if !fs::exists(&to) || crate::path_case::same_file(&from, &to) {
                let Some(_guard) = self.locks.try_lock_all(&[&from, &to]) else {
                    tracing::warn!(
                        "[DAV] corrections are being written under {}",
                        from.display()
                    );
                    continue;
                };
                if let Some(parent) = to.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if let Err(e) = fs::rename(&from, &to) {
                    tracing::warn!("[DAV] moving the corrections of {}: {e}", from.display());
                }
                continue;
            }
            self.merge_dir(&from, &to);
        }
    }

    /// Move every overlay below `from` into the same place below `to`, then drop the
    /// emptied folders.
    fn merge_dir(&self, from: &Path, to: &Path) {
        let Ok(entries) = fs::read_dir(from) else {
            return;
        };
        for entry in entries.flatten() {
            let (path, name) = (
                entry.path(),
                entry.file_name().to_string_lossy().into_owned(),
            );
            if fs::is_dir(&path) {
                let into = crate::path_case::on_disk_name(to, &name, false).unwrap_or(name);
                self.merge_dir(&path, &to.join(into));
            } else if let Some((stem, id, _)) = personal_name(&name) {
                self.move_one(&path, to, stem, id);
            }
        }
        let _ = fs::remove_dir(from);
    }

    /// The folder `rel` is gone: so is every account's overlay below it.
    pub fn forget_prefix(&self, rel: &str) {
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            return;
        }
        for base in self.bases() {
            let Some(dir) = resolve_existing(&self.library, &base, rel) else {
                continue;
            };
            let dir = base.join(dir);
            if !fs::is_dir(&dir) {
                continue;
            }
            match self.locks.try_lock(&dir) {
                Some(_guard) => {
                    let _ = fs::remove_dir_all(&dir);
                }
                None => tracing::warn!(
                    "[DAV] corrections are being written: {} is kept",
                    dir.display()
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_bodies() {
        assert!(is_layer_body(br#"{"pages": []}"#));
        assert!(is_layer_body(
            b"\xEF\xBB\xBF{\"pages\": [{\"x\": 1}], \"v\": {}}"
        ));
        for bad in [
            &b"[]"[..],
            b"{}",
            b"{\"pages\": {}}",
            b"{\"pages\": [], \"ocr_engine\": null}",
            b"{\"pages\": []",
            b"",
        ] {
            assert!(!is_layer_body(bad), "{}", String::from_utf8_lossy(bad));
        }
    }

    /// The startup walk never descends a symlinked folder (like the index's scan): a
    /// link back up the tree would otherwise loop until the path is too long.
    #[cfg(unix)]
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn the_correction_walk_skips_symlinked_folders() {
        let dir = tempfile::tempdir().unwrap();
        let (library, users) = (dir.path().join("library"), dir.path().join("users"));
        std::fs::create_dir_all(library.join("S")).unwrap();
        std::fs::create_dir_all(&users).unwrap();
        std::fs::write(library.join("S/V.cbz"), b"x").unwrap();
        std::fs::write(library.join("S/V.edit.mokuro"), b"{}").unwrap();
        std::os::unix::fs::symlink("..", library.join("S/all")).unwrap();
        let overlays = PersonalOverlays::new(library, users, PathWriteLocks::new());
        assert_eq!(overlays.physical_corrections(), ["S/V.edit.mokuro"]);
    }

    #[test]
    fn put_rules() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let here = names(&["V.cbz", "D.edit.cbz", "Z.zip"]);
        assert_eq!(layer_put_rule("V.edit.mokuro", &here), Some(Ok(())));
        assert_eq!(layer_put_rule("v.edit-3.mokuro", &here), Some(Ok(())));
        assert_eq!(layer_put_rule("V.curated.mokuro", &here), Some(Ok(())));
        assert_eq!(
            layer_put_rule("V.edit.mokuro.gz", &here),
            Some(Err(LayerRefusal::Gzip))
        );
        assert_eq!(
            layer_put_rule("V.edit.MOKURO", &here),
            Some(Err(LayerRefusal::NoVolume))
        );
        assert_eq!(
            layer_put_rule("Z.edit.mokuro", &here),
            Some(Err(LayerRefusal::NoVolume))
        );
        // Not a correction: a dotted volume's primary, another id, the exact-case rule.
        assert_eq!(layer_put_rule("D.edit.mokuro", &here), None);
        assert_eq!(layer_put_rule("V.hayai.mokuro", &here), None);
        assert_eq!(layer_put_rule("V.EDIT.mokuro", &here), None);
    }
}
