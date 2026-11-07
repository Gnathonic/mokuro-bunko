//! How the planner reads names: the one fold names pair under ([`pair_key`]), the
//! fold of the taken-name sets ([`taken_key`]), and the volume-group split of a file
//! name ([`split_name`], sidecar names rule v1).

use std::collections::HashSet;

use crate::sidecar_names::{self, SidecarClass, strip_suffix_ci as strip_ci};

use super::plan::Node;

/// The form two names pair under in a merge: two folders that are one series, two
/// volume-group stems, two sidecar suffixes, the folded scope segments. Every pairing
/// the planner makes goes through here, so a change of fold (Track B) is one change.
/// Today: the filename fold, [`crate::names::fold_name`] (the index's and PathCase's).
pub fn pair_key(name: &str) -> String {
    crate::names::fold_name(name)
}

/// The form a name is held in the planner's taken sets (`Plan::taken`): a name that
/// moves in, or a free layer id ([`super::rules::free_layer`]), never folds onto one
/// already there. Today the same fold as [`pair_key`]; it is its own seam because the
/// taken set must fold at least as much as every host the plan runs on.
pub fn taken_key(name: &str) -> String {
    crate::names::fold_name(name)
}

/// What one file of a volume group is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Archive,
    Main,
    Layer(String),
    Cover,
    Other,
}

/// A listed file: its NFC name split into stem and suffix.
#[derive(Debug, Clone)]
pub struct Named<X> {
    pub node: Node<X>,
    pub name: String,
    pub stem: String,
    pub suffix: String,
    pub part: Part,
    /// The volume group it belongs to: the paired stem, or (a file that is no volume
    /// part) `\0` and its paired name.
    pub key: String,
}

/// The folded stems of the `.cbz` archives among `names`: only they anchor sidecars
/// (rule v1). A `.zip` is still an [`Part::Archive`] for grouping moves.
pub fn archive_stems<'a>(names: impl Iterator<Item = &'a str>) -> HashSet<String> {
    sidecar_names::volume_stems(names)
}

/// `(stem, suffix, part)` of a file name, given the folder's archive stems.
pub fn split_name(name: &str, stems: &HashSet<String>) -> (String, String, Part) {
    if let Some(s) = strip_ci(name, ".cbz").or_else(|| strip_ci(name, ".zip")) {
        return (s.to_string(), name[s.len()..].to_string(), Part::Archive);
    }
    if let Some((base, _)) = sidecar_names::split_mokuro(name) {
        // The listing is at hand, so rule v1 decides: all-digit ids are layers.
        if let SidecarClass::Layer { stem, id, .. } = sidecar_names::classify(name, stems) {
            return (
                stem.to_string(),
                name[stem.len()..].to_string(),
                Part::Layer(id.to_string()),
            );
        }
        return (base.to_string(), name[base.len()..].to_string(), Part::Main);
    }
    if let Some(s) = strip_ci(name, ".webp") {
        return (s.to_string(), name[s.len()..].to_string(), Part::Cover);
    }
    (name.to_string(), String::new(), Part::Other)
}

/// A listed node as a volume-group member.
pub fn named<X>(n: Node<X>, stems: &HashSet<String>) -> Named<X> {
    let name = n.name.clone();
    let (stem, suffix, part) = split_name(&name, stems);
    let key = match part {
        Part::Other => format!("\u{0}{}", pair_key(&name)),
        _ => pair_key(&stem),
    };
    Named {
        node: n,
        name,
        stem,
        suffix,
        part,
        key,
    }
}

/// `(scope key, group key)` of a file variant set: the folded stem (a file that is
/// no volume part: its folded name), and the [`Named::key`] its files share.
pub fn file_key(name: &str, stems: &HashSet<String>) -> (String, String) {
    let (stem, _, part) = split_name(name, stems);
    match part {
        Part::Other => (pair_key(name), format!("\u{0}{}", pair_key(name))),
        _ => (pair_key(&stem), pair_key(&stem)),
    }
}
