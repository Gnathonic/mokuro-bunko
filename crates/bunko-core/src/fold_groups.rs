//! Case-variant groups, and the freeze that keeps Bunko's hands off them (design 0.8
//! D5).
//!
//! A fold group is two or more entries listed in one folder whose names fold alike
//! under the one filename fold ([`crate::names::fold_name`]). Only a case-sensitive
//! store can hold one (a Linux disk without casefold, a remote store), made outside
//! Bunko or by a release before 0.5.3: Bunko itself never makes a second spelling.
//!
//! Requests reach one member of a group (the winner: the store's case winner, else the
//! byte-smallest); the others, and the files rule v1 gives a hidden archive, are hidden
//! ([`FoldGroups::is_hidden`]). Until an admin merges the group by hand, it is FROZEN:
//! nothing Bunko does on its own, and no client write, creates, changes, moves, deletes
//! or re-keys anything in it ([`FoldGroups::freezing`]):
//!
//! * every member, the winner included (and everything inside a member folder);
//! * every file rule v1 gives, or would give once it is written, to an archive of the
//!   group (`v.mokuro`, `V.hayai.mokuro`, `V.webp`, `v.nocover` beside `V.cbz` and
//!   `v.cbz`), and, the same way, to an archive whose own files hold a group
//!   (`Vol.cbz` beside `VOL.mokuro` and `vol.mokuro`): deleting or moving that archive
//!   would take a member with it. A file tied to such an archive only by the wider
//!   filename fold (`STRASSE.mokuro` beside `Straße.cbz` in a group) is frozen too.
//!
//! Reads still serve the winner. Every check (WebDAV, OCR, covers, the sweeps, the
//! hooks) asks this one type, built from a folder listing, so the answer is the same on
//! every host and a new code path gets it by asking.

// Rule v1's stem fold is a deprecated rule-2 shim (`fold`) until B2 removes it here.
#![allow(deprecated)]

use std::collections::{HashMap, HashSet};

use crate::names::fold_name;
use crate::sidecar_names::{self, Folder};

/// One fold group of a folder listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// The fold every member shares.
    pub fold: String,
    /// The member a request reaches.
    pub winner: String,
    /// Every member, the winner included, sorted.
    pub members: Vec<String>,
    /// The other listed files the group freezes (the files rule v1 gives its archives),
    /// sorted.
    pub files: Vec<String>,
}

impl Group {
    /// `a`, `b` and `c` as `"a", "b", "c"`.
    pub fn members_quoted(&self) -> String {
        self.members
            .iter()
            .map(|m| format!("{m:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The fold groups of one folder listing, with what they hide and freeze.
#[derive(Debug, Clone, Default)]
pub struct FoldGroups {
    groups: Vec<Group>,
    /// fold -> index into `groups`.
    by_fold: HashMap<String, usize>,
    /// The listed name of every fold that is no group (one entry).
    single: HashMap<String, String>,
    /// The listed file names (what rule v1 reads), sorted.
    files: Vec<String>,
    /// Every listed name the freeze holds or hides (members, group files, hidden
    /// files) -> index into `groups`: an archive whose arrival would take one of them
    /// as its own file (rule 4) is frozen too.
    held: HashMap<String, usize>,
    /// Stems of the archives a group freezes, by rule v1's stem fold and by the
    /// filename fold, -> index into `groups`.
    stem_folds: HashMap<String, usize>,
    name_folds: HashMap<String, usize>,
    /// Rule v1's folded stems of every listed `.cbz` (rule 4: a primary of another
    /// listed volume is never read as a frozen archive's layer).
    cbz_stem_folds: HashSet<String>,
    hidden: HashSet<String>,
}

impl FoldGroups {
    /// The groups of a folder listing, `entries` being every listed `(name, is_dir)`.
    /// The winner of a group is `preferred(fold)` when that name is listed (a remote
    /// store's case winner, the spelling its case merge keeps), else the byte-smallest.
    pub fn new<'a>(
        entries: impl IntoIterator<Item = (&'a str, bool)>,
        preferred: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let entries: Vec<(&str, bool)> = entries.into_iter().collect();
        let mut folds: HashMap<String, Vec<&str>> = HashMap::new();
        for (name, _) in &entries {
            folds.entry(fold_name(name)).or_default().push(name);
        }
        let mut out = FoldGroups::default();
        let mut sorted: Vec<(String, Vec<&str>)> = folds.into_iter().collect();
        sorted.sort();
        for (fold, mut members) in sorted {
            if members.len() < 2 {
                out.single.insert(fold, members[0].to_string());
                continue;
            }
            members.sort_unstable();
            members.dedup();
            let winner = preferred(&fold)
                .filter(|w| members.contains(&w.as_str()))
                .unwrap_or_else(|| members[0].to_string());
            out.by_fold.insert(fold.clone(), out.groups.len());
            out.groups.push(Group {
                fold,
                winner,
                members: members.iter().map(|m| m.to_string()).collect(),
                files: Vec::new(),
            });
        }
        if out.groups.is_empty() {
            return out;
        }
        let mut files: Vec<&str> = entries
            .iter()
            .filter(|(_, dir)| !dir)
            .map(|(n, _)| *n)
            .collect();
        files.sort_unstable();
        out.files = files.iter().map(|n| n.to_string()).collect();
        out.cbz_stem_folds = sidecar_names::volume_stems(files.iter().copied());
        let folder = Folder::new(&files);
        // Hidden: every member but the winner, and the files rule v1 gives a hidden
        // archive.
        for group in &out.groups {
            out.hidden.extend(
                group
                    .members
                    .iter()
                    .filter(|m| **m != group.winner)
                    .cloned(),
            );
        }
        for group in &out.groups {
            for member in group.members.iter().filter(|m| **m != group.winner) {
                if files.contains(&member.as_str()) && sidecar_names::cbz_stem(member).is_some() {
                    out.hidden.extend(
                        folder
                            .volume_files(member)
                            .into_iter()
                            .map(|(n, _)| n.to_string()),
                    );
                }
            }
        }
        // Frozen archives: an archive member, and an archive whose own files (rule v1)
        // hold a member.
        let mut frozen_stems: Vec<(&str, usize)> = Vec::new();
        for &archive in files
            .iter()
            .filter(|n| sidecar_names::cbz_stem(n).is_some())
        {
            let stem = sidecar_names::cbz_stem(archive).unwrap_or(archive);
            if let Some(&g) = out.by_fold.get(&fold_name(archive)) {
                frozen_stems.push((stem, g));
                continue;
            }
            if let Some(g) = folder
                .volume_files(archive)
                .into_iter()
                .find_map(|(n, _)| out.by_fold.get(&fold_name(n)).copied())
            {
                frozen_stems.push((stem, g));
            }
        }
        for (stem, g) in frozen_stems {
            out.stem_folds
                .entry(sidecar_names::stem_fold(stem))
                .or_insert(g);
            out.name_folds.entry(fold_name(stem)).or_insert(g);
        }
        let mut group_files: Vec<Vec<String>> = vec![Vec::new(); out.groups.len()];
        for (name, _) in &entries {
            if out.by_fold.contains_key(&fold_name(name)) {
                continue;
            }
            if let Some(g) = out.frozen_file(name) {
                group_files[g].push(name.to_string());
            }
        }
        for (group, mut files) in out.groups.iter_mut().zip(group_files) {
            files.sort();
            group.files = files;
        }
        for (index, group) in out.groups.iter().enumerate() {
            for name in group.members.iter().chain(group.files.iter()) {
                out.held.entry(name.clone()).or_insert(index);
            }
        }
        let hidden_files: Vec<(String, usize)> = out
            .hidden
            .iter()
            .filter(|n| !out.held.contains_key(*n))
            .filter_map(|n| out.frozen_file(n).map(|g| (n.clone(), g)))
            .collect();
        out.held.extend(hidden_files);
        out
    }

    /// No two listed names fold alike: nothing is hidden or frozen here.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// The groups, by fold.
    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    /// The listed names no request reaches: every member of a group but its winner,
    /// and every file rule v1 gives a hidden archive.
    pub fn hidden(&self) -> &HashSet<String> {
        &self.hidden
    }

    pub fn is_hidden(&self, name: &str) -> bool {
        self.hidden.contains(name)
    }

    /// The group that freezes `name`, listed or not (a name a write would create
    /// included): `name` folds like a group, or is a file rule v1 gives, or would
    /// give, to an archive the group freezes. `None` when nothing here freezes it.
    pub fn freezing(&self, name: &str) -> Option<&Group> {
        if self.groups.is_empty() {
            return None;
        }
        let index = self
            .by_fold
            .get(&fold_name(name))
            .copied()
            .or_else(|| self.frozen_file(name))
            .or_else(|| self.taking_archive(name))?;
        self.groups.get(index)
    }

    /// The group an archive `name` that is not listed yet would take a file from: once
    /// written, rule v1 (rule 4: an exact stem is the archive's own primary) would give
    /// it a member, a file the freeze holds or a hidden file (`V.edit.cbz` takes
    /// `V.edit.mokuro`, a frozen `V.cbz`'s correction, as its primary; `v.hayai.cbz`
    /// takes the hidden `v.cbz`'s `v.hayai.mokuro`). Writing it would move that file out
    /// of the group, and deleting it then would delete the file.
    fn taking_archive(&self, name: &str) -> Option<usize> {
        sidecar_names::cbz_stem(name)?;
        if self.files.iter().any(|f| f == name) {
            return None;
        }
        let mut listing: Vec<&str> = self.files.iter().map(String::as_str).collect();
        listing.push(name);
        Folder::new(&listing)
            .volume_files(name)
            .into_iter()
            .find_map(|(file, _)| self.held.get(file).copied())
    }

    /// [`Self::freezing`] is some.
    pub fn is_frozen(&self, name: &str) -> bool {
        self.freezing(name).is_some()
    }

    /// The entry a request for `name` reaches when `name` is in no group: the one
    /// listed name that folds like it (`None` when there is none). A group's winner
    /// for a grouped name.
    pub fn reach(&self, name: &str) -> Option<&str> {
        let fold = fold_name(name);
        match self.by_fold.get(&fold) {
            Some(&g) => Some(self.groups[g].winner.as_str()),
            None => self.single.get(&fold).map(String::as_str),
        }
    }

    /// The group a file `name` (not a member) is frozen by as a frozen archive's file.
    fn frozen_file(&self, name: &str) -> Option<usize> {
        if self.stem_folds.is_empty() {
            return None;
        }
        let of_stem = |stem: &str| {
            self.stem_folds
                .get(&sidecar_names::stem_fold(stem))
                .or_else(|| self.name_folds.get(&fold_name(stem)))
                .copied()
        };
        if let Some((base, _)) = sidecar_names::split_mokuro(name) {
            if let Some(g) = of_stem(base) {
                return Some(g);
            }
            // Rule 4: the primary of another listed volume is that volume's.
            if self
                .cbz_stem_folds
                .contains(&sidecar_names::stem_fold(base))
            {
                return None;
            }
            return sidecar_names::split_layer(name).and_then(|(stem, _, _)| of_stem(stem));
        }
        [".cbz", ".webp", ".nocover"]
            .iter()
            .find_map(|suffix| sidecar_names::strip_suffix_ci(name, suffix))
            .and_then(of_stem)
    }
}

/// Where a library path meets the freeze ([`frozen_along`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frozen {
    /// A segment is frozen by the group of the folder `folder` (library-relative, `""`
    /// for the root, in listed spelling).
    Group { folder: String, group: Group },
    /// The first segment is a reader's per-user name in some spelling (`History/`,
    /// `Goals.json`): never a library entry, never touched as one.
    Reserved { name: String },
    /// A `..` segment: the walk cannot tell where it leads, so it is held (fail closed;
    /// the request edge refuses such paths before they get here).
    Unresolved { rel: String },
}

impl Frozen {
    /// One line for a log, a refusal or doctor: what is frozen and what to do.
    pub fn describe(&self) -> String {
        match self {
            Frozen::Group { folder, group } => {
                let at = if folder.is_empty() {
                    "the library root".to_string()
                } else {
                    format!("{folder:?}")
                };
                format!(
                    "{at} lists case variants of one name: {}. Bunko changes none of them, \
                     nor their files, until they are merged by hand (run `mokuro-bunko \
                     doctor`; MIGRATING-0.7.md, \"Library paths are case-insensitive\", has \
                     the steps).",
                    group.members_quoted()
                )
            }
            Frozen::Reserved { name } => format!(
                "{name:?} at the library root is spelled like a reader's own file or folder \
                 and is no library entry: rename it (doctor names it)."
            ),
            Frozen::Unresolved { rel } => {
                format!("{rel:?} steps out of a folder (`..`): nothing is written there.")
            }
        }
    }
}

/// Walk `rel` (library-relative, `/`-separated; `\\` too on Windows, as the handlers
/// split it) from the library root through the folder listings: the first segment the
/// freeze holds, or `None`. A `.` segment stays in its folder, as every handler resolves
/// it; a `..` segment is [`Frozen::Unresolved`] (never "not frozen"). `groups_of(folder)`
/// gives the [`FoldGroups`] of a library folder (`""` for the root, the per-user names
/// left out of it), `None` when it cannot be listed (nothing below it is listed either:
/// the walk stops there). `top_reserved`: a per-user name as the first segment is
/// [`Frozen::Reserved`]. Each segment is followed through the entry a request reaches.
pub fn frozen_along(
    rel: &str,
    top_reserved: bool,
    mut groups_of: impl FnMut(&str) -> Option<FoldGroups>,
) -> Option<Frozen> {
    let mut folder = String::new();
    #[cfg(windows)]
    let segments = rel.split(['/', '\\']);
    #[cfg(not(windows))]
    let segments = rel.split('/');
    let segments: Vec<&str> = segments.filter(|s| !s.is_empty() && *s != ".").collect();
    if segments.contains(&"..") {
        return Some(Frozen::Unresolved {
            rel: rel.to_string(),
        });
    }
    for (position, segment) in segments.into_iter().enumerate() {
        if position == 0 && top_reserved && crate::reader_tree::is_per_user_root_name(segment) {
            return Some(Frozen::Reserved {
                name: segment.to_string(),
            });
        }
        let groups = groups_of(&folder)?;
        if let Some(group) = groups.freezing(segment) {
            return Some(Frozen::Group {
                folder,
                group: group.clone(),
            });
        }
        let next = groups.reach(segment)?.to_string();
        if !folder.is_empty() {
            folder.push('/');
        }
        folder.push_str(&next);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<String> {
        None
    }

    fn files(names: &[&'static str]) -> Vec<(&'static str, bool)> {
        names.iter().map(|n| (*n, false)).collect()
    }

    #[test]
    fn a_folder_without_variants_freezes_nothing() {
        let g = FoldGroups::new(files(&["V.cbz", "V.mokuro", "W.cbz"]), none);
        assert!(g.is_empty());
        assert!(!g.is_frozen("V.cbz"));
        assert!(!g.is_frozen("v.mokuro"));
        assert_eq!(g.reach("v.CBZ"), Some("V.cbz"));
        assert_eq!(g.reach("X.cbz"), None);
    }

    #[test]
    fn archive_variants_freeze_every_file_rule_v1_gives_either() {
        let g = FoldGroups::new(
            files(&[
                "V.cbz",
                "v.cbz",
                "V.mokuro",
                "v.hayai.mokuro.gz",
                "V.webp",
                "W.cbz",
                "W.mokuro",
                "STRASSE.cbz",
            ]),
            none,
        );
        assert_eq!(g.groups().len(), 1);
        let group = &g.groups()[0];
        assert_eq!(group.winner, "V.cbz");
        assert_eq!(group.members, ["V.cbz", "v.cbz"]);
        assert_eq!(group.files, ["V.mokuro", "V.webp", "v.hayai.mokuro.gz"]);
        for name in [
            "V.cbz",
            "v.cbz",
            "V.CBZ",
            "V.mokuro",
            "v.mokuro",
            // Names a write would create.
            "v.mokuro.gz",
            "V.hayai.mokuro",
            "v.fast.mokuro",
            "V.edit.mokuro",
            "v.webp",
            "V.nocover",
            "V\u{200B}.cbz",
        ] {
            assert!(g.is_frozen(name), "{name}");
        }
        for name in [
            "W.cbz",
            "W.mokuro",
            "w.webp",
            "X.mokuro",
            "STRASSE.cbz",
            "V",
        ] {
            assert!(!g.is_frozen(name), "{name}");
        }
        let mut hidden: Vec<&String> = g.hidden().iter().collect();
        hidden.sort();
        assert_eq!(hidden, ["v.cbz", "v.hayai.mokuro.gz"]);
    }

    /// Variants of a sidecar freeze the archive rule v1 gives them to, and so all of
    /// that archive's files: deleting it would delete a hidden one.
    #[test]
    fn sidecar_variants_freeze_their_archive() {
        let g = FoldGroups::new(files(&["Vol.cbz", "VOL.mokuro", "vol.mokuro"]), none);
        for name in [
            "Vol.cbz",
            "VOL.mokuro",
            "vol.mokuro",
            "Vol.webp",
            "Vol.fast.mokuro",
        ] {
            assert!(g.is_frozen(name), "{name}");
        }
        assert_eq!(g.groups()[0].files, ["Vol.cbz"]);
    }

    /// Rule 4: a decimal volume's primary stays its own beside a frozen whole volume.
    #[test]
    fn another_volumes_primary_is_not_a_frozen_archives_layer() {
        let g = FoldGroups::new(
            files(&["Vol 1.cbz", "vol 1.cbz", "Vol 1.5.cbz", "Vol 1.5.mokuro"]),
            none,
        );
        assert!(!g.is_frozen("Vol 1.5.cbz"));
        assert!(!g.is_frozen("Vol 1.5.mokuro"));
        assert!(g.is_frozen("Vol 1.6.mokuro"), "a layer `6` of Vol 1");
    }

    /// A new archive whose own files (rule 4) would include a member, a file the freeze
    /// holds or a hidden file is frozen: writing it would take that file from the group.
    #[test]
    fn an_archive_that_would_take_a_frozen_file_is_frozen() {
        let g = FoldGroups::new(
            files(&[
                "V.cbz",
                "v.cbz",
                "V.edit.mokuro",
                "v.hayai.mokuro",
                "W.cbz",
                "W.x.mokuro",
            ]),
            none,
        );
        assert!(g.is_hidden("v.hayai.mokuro"));
        for name in ["V.edit.cbz", "v.hayai.cbz", "V.HAYAI.cbz", "v.edit.CBZ"] {
            assert!(g.is_frozen(name), "{name}");
        }
        // Nothing of the group's would be its own.
        for name in ["W.x.cbz", "X.cbz", "V.other.cbz"] {
            assert!(!g.is_frozen(name), "{name}");
        }
    }

    #[test]
    fn the_wider_fold_freezes_too() {
        let g = FoldGroups::new(files(&["Straße.cbz", "STRASSE.cbz"]), none);
        for name in [
            "strasse.mokuro",
            "Straße.webp",
            "STRASSE.fast.mokuro",
            "Strasse.cbz",
        ] {
            assert!(g.is_frozen(name), "{name}");
        }
        let g = FoldGroups::new(files(&["Vol\u{200B}1.cbz", "Vol1.cbz"]), none);
        assert_eq!(g.groups().len(), 1);
        assert!(g.is_frozen("vol1.mokuro"));
    }

    #[test]
    fn folder_variants_and_the_store_winner() {
        let g = FoldGroups::new(
            [("Kingdom", true), ("kingdom", true), ("Other", true)],
            |fold| (fold == "kingdom").then(|| "kingdom".to_string()),
        );
        assert_eq!(g.groups()[0].winner, "kingdom");
        assert!(g.is_hidden("Kingdom"));
        assert!(g.is_frozen("KINGDOM"));
        assert!(!g.is_frozen("Other"));
        assert_eq!(g.reach("KINGDOM"), Some("kingdom"));
    }

    #[test]
    fn walking_a_path_meets_the_freeze_of_any_folder_on_the_way() {
        let listing = |folder: &str| -> Option<FoldGroups> {
            let entries: Vec<(&str, bool)> = match folder {
                "" => vec![
                    ("Kingdom", true),
                    ("kingdom", true),
                    ("Other", true),
                    ("History", true),
                ],
                "Kingdom" => vec![("V.cbz", false)],
                "Other" => vec![("S", true)],
                "Other/S" => vec![("A.cbz", false), ("a.cbz", false)],
                _ => return None,
            };
            Some(FoldGroups::new(entries, none))
        };
        let at = |rel: &str| frozen_along(rel, true, listing);
        assert!(
            matches!(at("kingdom/V.cbz"), Some(Frozen::Group { folder, .. }) if folder.is_empty())
        );
        assert!(
            matches!(at("OTHER/s/a.mokuro"), Some(Frozen::Group { folder, .. }) if folder == "Other/S")
        );
        assert_eq!(at("Other/S/B.cbz"), None);
        assert_eq!(at("Other/New/x.cbz"), None);
        assert_eq!(
            at("History/x"),
            Some(Frozen::Reserved {
                name: "History".into()
            })
        );
        assert_eq!(frozen_along("History/x", false, listing), None);
        // `.` stays in its folder, as every handler resolves it; `..` never passes.
        assert!(
            matches!(at("./kingdom/./V.cbz"), Some(Frozen::Group { folder, .. }) if folder.is_empty())
        );
        assert!(
            matches!(at("Other/./S/./a.mokuro"), Some(Frozen::Group { folder, .. }) if folder == "Other/S")
        );
        assert!(matches!(
            at("Other/New/../S/a.cbz"),
            Some(Frozen::Unresolved { .. })
        ));
        assert_eq!(at("Other/./S/./B.cbz"), None);
    }
}
