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

/// The names in a library folder (through the store: an index read on a cloud
/// library), staging files left out. Blocking.
pub(crate) fn names_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
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
pub(crate) fn overlay_path(users: &Path, username: &str, rel: &str) -> Option<PathBuf> {
    let base = edits_base(users, username)?;
    let resolved = paths::resolve_under(users, &format!("{username}/{EDITS_DIR}/{rel}"))?;
    resolved.starts_with(&base).then_some(resolved)
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
    /// `edit-<n>` when that is taken (merge.rs `free_layer`: nothing is lost). The
    /// destination when it moved.
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
            // Free, or the source itself spelled differently (a case-insensitive disk).
            let free = |dest: &Path| !fs::exists(dest) || crate::path_case::same_file(from, dest);
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
            let names = names_in(&self.library.join(&rel));
            let stems = stems_of(&names);
            for name in &names {
                let child = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                };
                let path = self.library.join(&child);
                if fs::is_dir(&path) {
                    // The cloud-root owner's own tree holds no volume, and a symlinked
                    // folder is not walked (the index's scan does not either; a link up
                    // the tree would loop).
                    let linked =
                        fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
                    if !linked && !reader_tree::is_per_user_rel(&child) {
                        todo.push(child);
                    }
                } else if personal_name(name).is_some()
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
            match overlay_path(&self.users, username, folder) {
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
            let dir = base.join(old_folder);
            let owned = self.owned(&dir, old_folder, &old_name);
            for (name, id) in &owned {
                self.move_one(&dir.join(name), &base.join(new_folder), new_stem, id);
            }
            if !owned.is_empty() && !old_folder.is_empty() {
                self.drop_if_empty(&dir);
            }
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
            let dir = base.join(folder);
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
        for base in self.bases() {
            let (from, to) = (base.join(old), base.join(new));
            if !fs::is_dir(&from) {
                continue;
            }
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
                self.merge_dir(&path, &to.join(&name));
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
            let dir = base.join(rel);
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
