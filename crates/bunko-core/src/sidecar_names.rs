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
    /// `pre-upgrade`, `pre-upgrade-<n>`: bunko's copy of a replaced main; bunko only.
    ServerKeepsake,
    /// Reserved, but no client writes it here: `curated-<n>`, the rest of the
    /// `pre-upgrade` family, `previous-ocr*` (a reader's device-local keepsake).
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
    } else if is_counted(id, "pre-upgrade") {
        LayerClass::ServerKeepsake
    } else if is_counted(id, "curated")
        || id.starts_with("pre-upgrade")
        || id.starts_with("previous-ocr")
    {
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

/// The listed `.cbz` archives whose owner controls the file `name` of a folder listing
/// FILES `names` (series owners, 0.8): among the archives `name` belongs to, the one of
/// the SHORTEST stem, so a file takes its control from the outermost volume and never
/// from an archive nested under it (`V.curated.cbz` and `V.curated.mokuro` are
/// `V.cbz`'s while `V.cbz` is listed). Empty when no listed archive governs it.
///
/// `name` belongs to the stem `s` when it is `s.cbz`; a sidecar `s.mokuro[.gz]`,
/// `s.webp`, `s.nocover`; a `.mokuro[.gz]` rule v1 reads as `s`'s primary or layer
/// ([`classify`], all-digit ids included); or any other name nested under it,
/// `s.<id>.<rest>`, `<id>` a layer id with a letter once ASCII-lowercased (the
/// heuristic of `bunko_db::layer_sidecar_volume_path`, so `Vol 01.5.cbz` stays its own
/// volume; folded, because rule v1 folds stems: `V.EDIT.cbz` still turns `V.edit.mokuro`
/// into its primary). Stems match folded (rule 3): an archive spelled exactly like the
/// stem wins; otherwise every listed spelling governs (rule 6), and all of them must be
/// controlled.
pub fn governing_archives<'a>(name: &str, names: &[&'a str]) -> Vec<&'a str> {
    let archives: Vec<(&'a str, &'a str)> = names
        .iter()
        .filter_map(|&n| cbz_stem(n).map(|stem| (stem, n)))
        .collect();
    if archives.is_empty() {
        return Vec::new();
    }
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
    for (i, _) in name.match_indices('.').filter(|(i, _)| *i > 0) {
        let rest = &name[i + 1..];
        let Some((id, tail)) = rest.split_once('.') else {
            break;
        };
        let id = id.to_ascii_lowercase();
        if !tail.is_empty() && is_layer_id(&id) && id.bytes().any(|b| b.is_ascii_lowercase()) {
            candidates.push(&name[..i]);
        }
    }
    candidates.sort_by_key(|c| c.len());
    for candidate in candidates {
        let want = fold(candidate);
        let folded: Vec<(&str, &'a str)> = archives
            .iter()
            .copied()
            .filter(|(stem, _)| fold(stem) == want)
            .collect();
        if folded.is_empty() {
            continue;
        }
        return match folded.iter().find(|(stem, _)| *stem == candidate) {
            Some((_, exact)) => vec![*exact],
            None => folded.into_iter().map(|(_, n)| n).collect(),
        };
    }
    Vec::new()
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
                "server-keepsake" => LayerClass::ServerKeepsake,
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
            "V.PRE-UPGRADE.cbz",
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
    }

    #[test]
    fn suffixes_never_split_a_character() {
        assert_eq!(strip_suffix_ci("é", ".cbz"), None);
        assert_eq!(strip_suffix_ci("aé.CBZ", ".cbz"), Some("aé"));
        assert_eq!(cbz_stem(".cbz"), Some(""));
    }
}
