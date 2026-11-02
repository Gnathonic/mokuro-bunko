//! What a file beside a volume archive IS: sidecar names rule v1, shared with
//! mokuro-reader (`classifyMokuroSidecar` in `src/lib/util/sync/syncable-file.ts`).
//!
//! 1. The suffixes `.cbz`, `.mokuro` and `.mokuro.gz` match in any ASCII case.
//! 2. Only `.cbz` archives define volume stems.
//! 3. Stems are compared folded: NFC, then Unicode lowercase ([`fold`]).
//! 4. A primary sidecar wins over a layer reading of the same name
//!    (`Vol 1.5.mokuro` is volume 1.5's when `Vol 1.5.cbz` is listed).
//! 5. A layer id is matched exactly as written against `^[a-z0-9-]{1,32}$`
//!    ([`generations::is_layer_id`]); all-digit ids are layers.
//! 6. When sweeping, an exact-case stem wins over a folded one, and a folded-only
//!    match belongs to no archive when two archives' stems fold alike.
//! 7. A leading UTF-8 BOM is tolerated on read and never written; hashes cover
//!    the raw bytes (`bunko_library::pyjson::strip_bom`).
//!
//! A bare file name cannot tell a decimal volume's primary from a layer, so every
//! decision here takes the folder listing. Both sides run the vectors in
//! `interop/contract/sidecar-names.json`.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use unicode_normalization::UnicodeNormalization;

use crate::generations::is_layer_id;

/// Archive extensions a volume can have. Only `.cbz` anchors sidecars (rule 2); the
/// others still keep a sweep off their files ([`sweep_set`]) and count as volumes
/// where a volume is all that is asked (the storage Check, failure records).
pub const ARCHIVE_EXTENSIONS: [&str; 4] = [".cbz", ".cbr", ".zip", ".rar"];

/// The spelling-insensitive form two stems collide under: NFC, then a full Unicode
/// lowercase.
pub fn fold(name: &str) -> String {
    name.nfc().collect::<String>().to_lowercase()
}

/// `name` minus `suffix`, the suffix matched in any ASCII case.
pub fn strip_suffix_ci<'a>(name: &'a str, suffix: &str) -> Option<&'a str> {
    let cut = name.len().checked_sub(suffix.len())?;
    (name.is_char_boundary(cut) && name[cut..].eq_ignore_ascii_case(suffix)).then(|| &name[..cut])
}

/// `Vol 1` for `Vol 1.cbz` (any case); `None` for any other name.
pub fn cbz_stem(name: &str) -> Option<&str> {
    strip_suffix_ci(name, ".cbz")
}

/// `(base, gz)` of `<base>.mokuro` / `<base>.mokuro.gz` (any case).
pub fn split_mokuro(name: &str) -> Option<(&str, bool)> {
    if let Some(base) = strip_suffix_ci(name, ".mokuro.gz") {
        return Some((base, true));
    }
    strip_suffix_ci(name, ".mokuro").map(|base| (base, false))
}

/// `(stem, id, gz)` of `<stem>.<id>.mokuro[.gz]`, split on the LAST dot, the id
/// exact. Listing-free: says nothing about whether `<stem>` is a volume, nor whether
/// the whole name is a decimal volume's primary — [`classify`] decides that.
pub fn split_layer(name: &str) -> Option<(&str, &str, bool)> {
    let (base, gz) = split_mokuro(name)?;
    let (stem, id) = base.rsplit_once('.')?;
    (!stem.is_empty() && is_layer_id(id)).then_some((stem, id, gz))
}

/// What a listed `.mokuro` is. `stem` is spelled as in the name, not as the archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarClass<'a> {
    Primary {
        stem: &'a str,
        gz: bool,
    },
    Layer {
        stem: &'a str,
        id: &'a str,
        gz: bool,
    },
    /// No archive here owns it, or it is no `.mokuro` at all.
    Orphan,
}

/// The folded stems of the `.cbz` archives among `names` (rule 2).
pub fn volume_stems<'a>(names: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
    names.into_iter().filter_map(cbz_stem).map(fold).collect()
}

/// Rule v1 for one name, given the folder's [`volume_stems`].
pub fn classify<'a>(name: &'a str, folded_cbz_stems: &HashSet<String>) -> SidecarClass<'a> {
    let Some((base, gz)) = split_mokuro(name) else {
        return SidecarClass::Orphan;
    };
    if folded_cbz_stems.contains(&fold(base)) {
        return SidecarClass::Primary { stem: base, gz };
    }
    match split_layer(name) {
        Some((stem, id, gz)) if folded_cbz_stems.contains(&fold(stem)) => {
            SidecarClass::Layer { stem, id, gz }
        }
        _ => SidecarClass::Orphan,
    }
}

/// Who may write a layer id. Exact-case, like the id itself (rule 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerClass {
    /// `edit`, `edit-<n>`: one account's own edits.
    Personal,
    /// `curated`: the shared edit, published by accounts with the right to.
    Curated,
    /// Reserved, but no client writes it here: `curated-<n>`.
    Reserved,
    /// Everything else: engine generations, `original`, `gcv`, `updated-ocr`,
    /// `tr-*`, hand-made ids — governed as before.
    Other,
}

/// `<prefix>` or `<prefix>-<digits>`.
fn is_counted(id: &str, prefix: &str) -> bool {
    match id.strip_prefix(prefix) {
        Some("") => true,
        Some(rest) => rest
            .strip_prefix('-')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
        None => false,
    }
}

/// The personal layer id (D27): `edit` and `edit-<n>` are each account's own.
pub const PERSONAL_LAYER: &str = "edit";

/// The layer ids a server keeps per account, as `/login/api/me` lists them
/// (`per_user_layers`; each covers `<id>-<n>` too). A reader pushes its corrections
/// only when its id is listed.
pub fn per_user_layers() -> Vec<String> {
    vec![PERSONAL_LAYER.to_string()]
}

/// The [`LayerClass`] of a layer id, exactly as written.
pub fn layer_class(id: &str) -> LayerClass {
    if is_counted(id, PERSONAL_LAYER) {
        LayerClass::Personal
    } else if id == "curated" {
        LayerClass::Curated
    } else if is_counted(id, "curated") {
        LayerClass::Reserved
    } else {
        LayerClass::Other
    }
}

/// The [`LayerClass`] of a listed name when it is a layer, else `None`.
pub fn layer_class_of_name(name: &str, folded_cbz_stems: &HashSet<String>) -> Option<LayerClass> {
    match classify(name, folded_cbz_stems) {
        SidecarClass::Layer { id, .. } => Some(layer_class(id)),
        _ => None,
    }
}

/// One file of a volume, as [`volume_files`] finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeFile<'a> {
    Primary { gz: bool },
    Cover,
    NoCover,
    Layer { id: &'a str, gz: bool },
}

/// A folder listing prepared once for every archive in it (a compile pass, the OCR
/// walk and the catalog index ask about each volume of a folder): the archive stems,
/// and the folded stems its `.mokuro` names could belong to, so an archive none of them
/// folds to is answered without folding the listing again.
pub struct Folder<'a> {
    names: Vec<&'a str>,
    /// Stems of every archive of [`ARCHIVE_EXTENSIONS`], as written.
    archive_stems: Vec<&'a str>,
    /// Folded bases of the `.mokuro[.gz]` names, and of their layer readings' stems.
    sidecar_folds: HashSet<String>,
    /// The archive stems as a set, and folded with how many spellings fold to each:
    /// made on first use, so a folder without sidecars folds no archive name.
    stems: OnceLock<(HashSet<&'a str>, HashMap<String, usize>)>,
}

impl<'a> Folder<'a> {
    pub fn new(names: &[&'a str]) -> Self {
        let mut archive_stems = Vec::new();
        let mut sidecar_folds = HashSet::new();
        for &name in names {
            if let Some(stem) = ARCHIVE_EXTENSIONS
                .iter()
                .find_map(|ext| strip_suffix_ci(name, ext))
            {
                archive_stems.push(stem);
            } else if let Some((base, _)) = split_mokuro(name) {
                sidecar_folds.insert(fold(base));
                if let Some((stem, _, _)) = split_layer(name) {
                    sidecar_folds.insert(fold(stem));
                }
            }
        }
        Folder {
            names: names.to_vec(),
            archive_stems,
            sidecar_folds,
            stems: OnceLock::new(),
        }
    }

    fn stems(&self) -> &(HashSet<&'a str>, HashMap<String, usize>) {
        self.stems.get_or_init(|| {
            let exact: HashSet<&str> = self.archive_stems.iter().copied().collect();
            let mut folded: HashMap<String, usize> = HashMap::new();
            for stem in &exact {
                *folded.entry(fold(stem)).or_default() += 1;
            }
            (exact, folded)
        })
    }

    /// The listed files that belong to the archive `cbz_name`: its primary, cover,
    /// no-cover marker and layers, in listing order.
    ///
    /// Stricter than [`classify`], because what it returns may be deleted:
    /// * a stem matches exactly, or folded only when it is the one archive here of that
    ///   folded stem (rule 6: `vol 01.mokuro` is `vol 01.cbz`'s when both spellings are
    ///   listed; beside `Vol 01.cbz` and `VOL 01.cbz` it is neither's);
    /// * a name whose base is any archive's stem, of any of the
    ///   [`ARCHIVE_EXTENSIONS`], is that archive's and is never a layer here
    ///   (`Vol 01.5.mokuro` beside `Vol 01.5.zip` classifies as layer `5`, but a sweep
    ///   leaves it).
    pub fn volume_files(&self, cbz_name: &str) -> Vec<(&'a str, VolumeFile<'a>)> {
        let Some(anchor) = cbz_stem(cbz_name) else {
            return Vec::new();
        };
        let (exact, folded) = self.stems();
        let anchor_fold = fold(anchor);
        let alone = folded.get(&anchor_fold).is_none_or(|&n| n <= 1);
        let owns = |stem: &str| {
            stem == anchor || (alone && !exact.contains(stem) && fold(stem) == anchor_fold)
        };
        let mut out = Vec::new();
        for &name in &self.names {
            let (base, file) = if let Some((base, gz)) = split_mokuro(name) {
                (base, VolumeFile::Primary { gz })
            } else if let Some(base) = strip_suffix_ci(name, ".webp") {
                (base, VolumeFile::Cover)
            } else if let Some(base) = strip_suffix_ci(name, ".nocover") {
                (base, VolumeFile::NoCover)
            } else {
                continue;
            };
            if owns(base) {
                out.push((name, file));
            } else if matches!(file, VolumeFile::Primary { .. })
                && !folded.contains_key(&fold(base))
                && let Some((stem, id, gz)) = split_layer(name)
                && owns(stem)
            {
                out.push((name, VolumeFile::Layer { id, gz }));
            }
        }
        out
    }

    /// No `.mokuro` here can be `cbz_name`'s, in any spelling: checked without walking
    /// the listing.
    fn no_sidecars_for(&self, cbz_name: &str) -> bool {
        cbz_stem(cbz_name).is_none_or(|anchor| !self.sidecar_folds.contains(&fold(anchor)))
    }

    /// The primary sidecar of `cbz_name`: an exact stem before a folded one,
    /// `.mokuro` before `.mokuro.gz`.
    pub fn primary_of(&self, cbz_name: &str) -> Option<&'a str> {
        if self.no_sidecars_for(cbz_name) {
            return None;
        }
        let anchor = cbz_stem(cbz_name)?;
        let rank = |name: &str, gz: bool| {
            let exact = split_mokuro(name).is_some_and(|(base, _)| base == anchor);
            (!exact, gz)
        };
        self.volume_files(cbz_name)
            .into_iter()
            .filter_map(|(name, file)| match file {
                VolumeFile::Primary { gz } => Some((rank(name, gz), name)),
                _ => None,
            })
            .min()
            .map(|(_, name)| name)
    }

    /// `cbz_name`'s layer `id` (exact, rule 5) in any spelling of its stem, `.mokuro`
    /// before `.mokuro.gz`.
    pub fn layer_of(&self, cbz_name: &str, id: &str) -> Option<&'a str> {
        if self.no_sidecars_for(cbz_name) {
            return None;
        }
        self.volume_files(cbz_name)
            .into_iter()
            .filter_map(|(name, file)| match file {
                VolumeFile::Layer { id: found, gz } if found == id => Some((gz, name)),
                _ => None,
            })
            .min()
            .map(|(_, name)| name)
    }
}

/// [`Folder::volume_files`] for a single archive.
pub fn volume_files<'a>(cbz_name: &str, names: &[&'a str]) -> Vec<(&'a str, VolumeFile<'a>)> {
    Folder::new(names).volume_files(cbz_name)
}

/// The names [`volume_files`] finds: everything that goes when `cbz_name` goes.
pub fn sweep_set(cbz_name: &str, names: &[&str]) -> Vec<String> {
    volume_files(cbz_name, names)
        .into_iter()
        .map(|(name, _)| name.to_string())
        .collect()
}

/// [`Folder::primary_of`] for a single archive.
pub fn primary_of<'a>(cbz_name: &str, names: &[&'a str]) -> Option<&'a str> {
    Folder::new(names).primary_of(cbz_name)
}

/// Is `id`, a dot-separated segment of a name, the id of a name nested under a volume
/// (`V.<id>.<rest>`)? A layer id with a letter once FOLDED (the heuristic of
/// `bunko_db::layer_sidecar_volume_path`, so `Vol 01.5.cbz` stays its own volume).
/// Folded as rule v1 folds stems (rule 3): `V.EDIT.cbz` and `V.\u{212A}anji.cbz` (a
/// Kelvin sign, NFC `K`) still turn `V.edit.mokuro` and `V.kanji.mokuro` into their
/// primaries.
fn is_nested_id(id: &str) -> bool {
    let id = fold(id);
    is_layer_id(&id) && id.bytes().any(|b| b.is_ascii_lowercase())
}

/// Every stem `name` is nested under, `<stem>.<id>.<rest>` with a nested id
/// ([`is_nested_id`]), shortest first.
fn nested_stems(name: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for (i, _) in name.match_indices('.').filter(|(i, _)| *i > 0) {
        let Some((id, tail)) = name[i + 1..].split_once('.') else {
            break;
        };
        if !tail.is_empty() && is_nested_id(id) {
            out.push(&name[..i]);
        }
    }
    out
}

/// The listed `.cbz` archives whose owner controls the file `name` of a folder listing
/// FILES `names` (series owners, 0.8). Empty when no listed archive governs it.
///
/// `name` belongs to the stem `s` when it is `s.cbz`; a sidecar `s.mokuro[.gz]`,
/// `s.webp`, `s.nocover`; a `.mokuro[.gz]` rule v1 reads as `s`'s primary or layer
/// ([`classify`], all-digit ids included); or any other name nested under it,
/// `s.<id>.<rest>`, `<id>` a layer id with a letter once folded ([`is_nested_id`]).
///
/// Of the listed archives it belongs to, the NEAREST (longest stem) governs it: a
/// listed `Berserk.v01.cbz` is its own uploader's volume, whoever uploads `Berserk.cbz`
/// later (OWN-1), and a name not listed yet takes the nearest listed volume above it
/// (creating `Berserk.v01.cbz` beside `Berserk.cbz` needs `Berserk.cbz`'s control).
/// Except that an archive hiding a reserved layer of the volume above it
/// (`V.curated.cbz`, `V.edit.cbz` beside `V.cbz`: its primary would be that layer)
/// passes control up to that volume, in either upload order: those are the names that
/// hijack `V`'s layers, and `V`'s arrival takes them back.
///
/// Stems match folded (rule 3): an archive spelled exactly like the stem wins;
/// otherwise every listed spelling governs (rule 6), and all of them must be
/// controlled.
pub fn governing_archives<'a>(name: &str, names: &[&'a str]) -> Vec<&'a str> {
    let archives: Vec<(&'a str, &'a str)> = names
        .iter()
        .filter_map(|&n| cbz_stem(n).map(|stem| (stem, n)))
        .collect();
    if archives.is_empty() {
        return Vec::new();
    }
    let listed = |stem: &str| -> Vec<(&'a str, &'a str)> {
        let want = fold(stem);
        archives
            .iter()
            .copied()
            .filter(|(s, _)| fold(s) == want)
            .collect()
    };
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(base) = [".cbz", ".mokuro.gz", ".mokuro", ".webp", ".nocover"]
        .iter()
        .find_map(|s| strip_suffix_ci(name, s))
    {
        candidates.push(base);
    }
    // Rule v1's own reading of a `.mokuro` name (C7: `Vol 01.5.mokuro` is layer `5`).
    match classify(name, &volume_stems(names.iter().copied())) {
        SidecarClass::Primary { stem, .. } | SidecarClass::Layer { stem, .. } => {
            candidates.push(stem)
        }
        SidecarClass::Orphan => {}
    }
    candidates.extend(nested_stems(name));
    candidates.sort_by_key(|c| std::cmp::Reverse(c.len()));
    let Some((mut stem, mut found)) = candidates
        .into_iter()
        .map(|c| (c, listed(c)))
        .find(|(_, found)| !found.is_empty())
    else {
        return Vec::new();
    };
    while let Some(up) = hidden_layer_of(stem) {
        let above = listed(up);
        if above.is_empty() {
            break;
        }
        (stem, found) = (up, above);
    }
    match found.iter().find(|(s, _)| *s == stem) {
        Some((_, exact)) => vec![*exact],
        None => found.into_iter().map(|(_, n)| n).collect(),
    }
}

/// `s` when an archive stem is `<s>.<id>` with `<id>` (folded) a layer id of a
/// reserved class: the archive whose primary would be `s`'s layer `<id>`.
fn hidden_layer_of(stem: &str) -> Option<&str> {
    let (s, id) = stem.rsplit_once('.')?;
    let id = fold(id);
    (!s.is_empty() && is_layer_id(&id) && layer_class(&id) != LayerClass::Other).then_some(s)
}

/// Would a NEW archive `name` hide a layer of a listed volume? `V.edit.cbz` beside
/// `V.cbz` turns `V.edit.mokuro`, an account's own correction of `V`, into the primary
/// of `V.edit.cbz` (rule 4), and `V.curated.cbz`, `V.curated-2.cbz` do the same to
/// the shared layer and its merge copies: true when `name` is `<s>.<id>.cbz`, `<id>` a
/// layer id of a reserved class (folded, rule 3), and `<s>.cbz` is listed. Only `.cbz`
/// names define stems (rule 2).
pub fn hides_reserved_layer(name: &str, names: &[&str]) -> bool {
    cbz_stem(name)
        .and_then(hidden_layer_of)
        .is_some_and(|s| volume_stems(names.iter().copied()).contains(&fold(s)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn vectors() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../interop/contract/sidecar-names.json");
        let text = std::fs::read_to_string(&path).expect("interop/contract/sidecar-names.json");
        serde_json::from_str(&text).expect("valid JSON")
    }

    fn strs(v: &Value) -> Vec<&str> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect()
    }

    #[test]
    fn classify_vectors() {
        let v = vectors();
        assert_eq!(v["rule"], 1);
        let cases = v["classify"].as_array().unwrap();
        assert!(cases.len() >= 25);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let stems = volume_stems(strs(&case["folder"]));
            let e = &case["expect"];
            let want = match e["kind"].as_str().unwrap() {
                "primary" => SidecarClass::Primary {
                    stem: e["stem"].as_str().unwrap(),
                    gz: e["gz"].as_bool().unwrap(),
                },
                "layer" => SidecarClass::Layer {
                    stem: e["stem"].as_str().unwrap(),
                    id: e["layer"].as_str().unwrap(),
                    gz: e["gz"].as_bool().unwrap(),
                },
                "orphan" => SidecarClass::Orphan,
                other => panic!("unknown kind {other}"),
            };
            assert_eq!(
                classify(name, &stems),
                want,
                "{name:?} in {:?}",
                case["folder"]
            );
        }
    }

    #[test]
    fn layer_class_vectors() {
        for case in vectors()["layer_class"].as_array().unwrap() {
            let id = case["id"].as_str().unwrap();
            let want = match case["class"].as_str().unwrap() {
                "personal" => LayerClass::Personal,
                "curated" => LayerClass::Curated,
                "reserved" => LayerClass::Reserved,
                "other" => LayerClass::Other,
                other => panic!("unknown class {other}"),
            };
            assert_eq!(layer_class(id), want, "{id:?}");
        }
    }

    #[test]
    fn sweep_vectors() {
        for case in vectors()["sweep"].as_array().unwrap() {
            let archive = case["archive"].as_str().unwrap();
            let mut got = sweep_set(archive, &strs(&case["folder"]));
            got.sort();
            assert_eq!(got, strs(&case["expect"]), "{archive:?}: {}", case["why"]);
        }
    }

    #[test]
    fn layer_class_of_a_listed_name() {
        let stems = volume_stems(["Vol 1.cbz"]);
        assert_eq!(
            layer_class_of_name("Vol 1.edit-2.mokuro", &stems),
            Some(LayerClass::Personal)
        );
        assert_eq!(
            layer_class_of_name("Vol 1.curated.MOKURO", &stems),
            Some(LayerClass::Curated)
        );
        assert_eq!(layer_class_of_name("Vol 1.EDIT.mokuro", &stems), None);
        assert_eq!(layer_class_of_name("Vol 1.mokuro", &stems), None);
        assert_eq!(layer_class_of_name("Vol 2.edit.mokuro", &stems), None);
    }

    #[test]
    fn primary_prefers_the_exact_stem_then_plain() {
        let names = ["V.cbz", "v.mokuro", "V.mokuro.gz"];
        assert_eq!(primary_of("V.cbz", &names), Some("V.mokuro.gz"));
        assert_eq!(
            primary_of("V.cbz", &["V.cbz", "V.MOKURO"]),
            Some("V.MOKURO")
        );
        assert_eq!(
            primary_of("V.cbz", &["V.cbz", "v.mokuro.gz", "v.MOKURO"]),
            Some("v.MOKURO")
        );
        assert_eq!(primary_of("V.cbz", &["V.cbz", "v.cbz", "v.mokuro"]), None);
        assert_eq!(primary_of("V.cbr", &["V.cbr", "V.mokuro"]), None);
    }

    #[test]
    fn two_archives_of_one_folded_stem_claim_no_folded_sidecar() {
        let names = ["Vol 01.cbz", "VOL 01.cbz", "vol 01.mokuro"];
        assert_eq!(primary_of("Vol 01.cbz", &names), None);
        assert_eq!(primary_of("VOL 01.cbz", &names), None);
        // An exact stem is still its own.
        let names = ["Vol 01.cbz", "VOL 01.cbz", "VOL 01.mokuro"];
        assert_eq!(primary_of("VOL 01.cbz", &names), Some("VOL 01.mokuro"));
        assert_eq!(primary_of("Vol 01.cbz", &names), None);
    }

    #[test]
    fn a_layer_in_any_spelling_of_the_stem() {
        let folder = Folder::new(&["V.cbz", "v.gcv.mokuro.gz", "V.gcv.MOKURO", "V.5.cbz"]);
        assert_eq!(folder.layer_of("V.cbz", "gcv"), Some("V.gcv.MOKURO"));
        assert_eq!(folder.layer_of("V.cbz", "hayai"), None);
        assert_eq!(folder.layer_of("W.cbz", "gcv"), None);
        assert_eq!(
            Folder::new(&["V.cbz", "v.gcv.mokuro.gz"]).layer_of("V.cbz", "gcv"),
            Some("v.gcv.mokuro.gz")
        );
        assert_eq!(folder.primary_of("V.cbz"), None);
    }

    #[test]
    fn the_outermost_listed_archive_governs_a_file() {
        let gov = governing_archives;
        let v = ["V.cbz", "W.cbz"];
        for name in [
            "V.cbz",
            "V.mokuro",
            "V.MOKURO.gz",
            "V.webp",
            "V.nocover",
            "V.edit.cbz",
            "V.hayai.webp",
            "V.curated.mokuro",
            "V.edit.curated.mokuro",
        ] {
            assert_eq!(gov(name, &v), ["V.cbz"], "{name}");
        }
        // Shortest stem wins, in either upload order; alone, a nested archive is its own.
        let both = ["V.cbz", "V.curated.cbz"];
        assert_eq!(gov("V.curated.cbz", &both), ["V.cbz"]);
        assert_eq!(gov("V.curated.mokuro", &both), ["V.cbz"]);
        assert_eq!(gov("V.curated.cbz", &["V.curated.cbz"]), ["V.curated.cbz"]);
        assert_eq!(
            gov("V.curated.mokuro", &["V.curated.cbz"]),
            ["V.curated.cbz"]
        );
        // Folded ids (C1): rule v1 folds the stem `V.EDIT` onto `V.edit.mokuro`.
        for name in [
            "V.EDIT.cbz",
            "V.Curated.cbz",
            "V.CURATED-2.cbz",
            "v.curated.cbz",
        ] {
            assert_eq!(gov(name, &v), ["V.cbz"], "{name}");
        }
        // A decimal volume is its own; its sidecars too.
        let dec = ["Vol 01.cbz", "Vol 01.5.cbz"];
        assert_eq!(gov("Vol 01.5.cbz", &dec), ["Vol 01.5.cbz"]);
        assert_eq!(gov("Vol 01.5.mokuro", &dec), ["Vol 01.5.cbz"]);
        assert_eq!(gov("Vol 01.5.webp", &dec), ["Vol 01.5.cbz"]);
        // Without it, rule v1 reads `Vol 01.5.mokuro` as layer `5` of Vol 01 (C7) and
        // `V.2024.mokuro` as layer `2024` of V; other names need a letter in the id.
        assert_eq!(gov("Vol 01.5.mokuro", &["Vol 01.cbz"]), ["Vol 01.cbz"]);
        assert_eq!(gov("V.2024.mokuro", &v), ["V.cbz"]);
        assert!(gov("Vol 01.5.cbz", &["Vol 01.cbz"]).is_empty());
        assert!(gov("Vol 01.5.webp", &["Vol 01.cbz"]).is_empty());
        assert!(gov("Dr. Stone v1.cbz", &["Dr.cbz"]).is_empty());
        assert_eq!(
            gov("Dr. Stone v1.mokuro", &["Dr. Stone v1.cbz"]),
            ["Dr. Stone v1.cbz"]
        );
        // A new volume, an orphan, staging and dot names: nothing governs them.
        for name in [
            "X.cbz",
            "X.mokuro",
            "notes.txt",
            ".V.cbz.upload-1.tmp",
            ".V.cbz",
        ] {
            assert!(gov(name, &v).is_empty(), "{name}");
        }
        assert!(gov("V.mokuro", &[]).is_empty());
        // Only `.cbz` archives govern (rule 2).
        assert!(gov("V.mokuro", &["V.zip"]).is_empty());
        // Case variants: the exact spelling, else every one of them.
        let variants = ["Vol.cbz", "VOL.CBZ"];
        assert_eq!(gov("VOL.mokuro", &variants), ["VOL.CBZ"]);
        assert_eq!(gov("Vol.edit.cbz", &variants), ["Vol.cbz"]);
        let mut all = gov("vol.mokuro", &variants);
        all.sort();
        assert_eq!(all, ["VOL.CBZ", "Vol.cbz"]);
        // Ids fold like stems, NFC first: a Kelvin sign is a `k` (F1).
        for name in [
            "V.\u{212A}anji.cbz",
            "V.\u{212A}r.webp",
            "V.\u{212A}anji.mokuro",
        ] {
            assert_eq!(gov(name, &v), ["V.cbz"], "{name}");
        }
    }

    /// The nearest listed volume governs (OWN-1): `Berserk.cbz` arriving later takes
    /// nothing of another account's listed `Berserk.v01.cbz`; only the names hiding a
    /// reserved layer of a volume pass control up to it.
    #[test]
    fn the_nearest_listed_volume_governs_but_hijack_names_go_up() {
        let gov = governing_archives;
        let listed = [
            "Berserk.cbz",
            "Berserk.v01.cbz",
            "Berserk.Omake.cbz",
            "Berserk.curated.cbz",
            "Berserk.EDIT.cbz",
            "Berserk.pre-upgrade.cbz",
            "Berserk.previous-ocr.cbz",
            "Berserk.\u{212A}anji.cbz",
            "Berserk.edit.curated.cbz",
        ];
        for (name, want) in [
            ("Berserk.v01.cbz", "Berserk.v01.cbz"),
            ("Berserk.v01.mokuro", "Berserk.v01.cbz"),
            ("Berserk.v01.webp", "Berserk.v01.cbz"),
            ("Berserk.v01.curated.mokuro", "Berserk.v01.cbz"),
            ("Berserk.v01.x.cbz", "Berserk.v01.cbz"),
            ("Berserk.Omake.cbz", "Berserk.Omake.cbz"),
            ("Berserk.\u{212A}anji.cbz", "Berserk.\u{212A}anji.cbz"),
            ("Berserk.curated.cbz", "Berserk.cbz"),
            ("Berserk.curated.mokuro", "Berserk.cbz"),
            ("Berserk.EDIT.cbz", "Berserk.cbz"),
            // Hand-made layer ids like any other (neither id is reserved).
            ("Berserk.pre-upgrade.webp", "Berserk.pre-upgrade.cbz"),
            ("Berserk.previous-ocr.cbz", "Berserk.previous-ocr.cbz"),
            ("Berserk.edit.curated.cbz", "Berserk.cbz"),
            ("Berserk.mokuro", "Berserk.cbz"),
            ("Berserk.v02.cbz", "Berserk.cbz"),
        ] {
            assert_eq!(gov(name, &listed), [want], "{name}");
        }
        // Not listed yet: the nearest listed volume above it.
        assert_eq!(gov("Berserk.v01.cbz", &["Berserk.cbz"]), ["Berserk.cbz"]);
        assert_eq!(
            gov("Berserk.v01.x.mokuro", &["Berserk.cbz", "Berserk.v01.cbz"]),
            ["Berserk.v01.cbz"]
        );
        // A hijack name goes up only to a listed volume.
        assert_eq!(
            gov("V.curated.mokuro", &["V.curated.cbz"]),
            ["V.curated.cbz"]
        );
    }

    #[test]
    fn a_new_archive_may_hide_a_reserved_layer_only_of_no_volume() {
        let hides = hides_reserved_layer;
        let v = ["V.cbz"];
        for name in [
            "V.edit.cbz",
            "V.edit-2.cbz",
            "V.EDIT.cbz",
            "v.Curated.cbz",
            "V.curated-2.cbz",
        ] {
            assert!(hides(name, &v), "{name}");
            assert!(!hides(name, &["W.cbz"]), "{name}");
        }
        for name in [
            "V.hayai.cbz",
            "V.5.cbz",
            "V.edit.x.cbz",
            "V.edit.mokuro",
            "V.edit.zip",
            "V.cbz",
            "V.pre-upgrade.cbz",
            "V.previous-ocr.cbz",
        ] {
            assert!(!hides(name, &v), "{name}");
        }
        // Only `.cbz` archives define stems (rule 2).
        assert!(!hides("V.edit.cbz", &["V.zip"]));
    }

    #[test]
    fn suffixes_never_split_a_character() {
        assert_eq!(strip_suffix_ci("é", ".cbz"), None);
        assert_eq!(strip_suffix_ci("aé.CBZ", ".cbz"), Some("aé"));
        assert_eq!(cbz_stem(".cbz"), Some(""));
    }
}
