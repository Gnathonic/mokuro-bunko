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
//! 6. When sweeping, an exact-case stem wins over a folded one.
//! 7. A leading UTF-8 BOM is tolerated on read and never written; hashes cover
//!    the raw bytes (`bunko_library::pyjson::strip_bom`).
//!
//! A bare file name cannot tell a decimal volume's primary from a layer, so every
//! decision here takes the folder listing. Both sides run the vectors in
//! `interop/contract/sidecar-names.json`.

use std::collections::HashSet;

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

/// The [`LayerClass`] of a layer id, exactly as written.
pub fn layer_class(id: &str) -> LayerClass {
    if is_counted(id, "edit") {
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

/// The listed files that belong to the archive `cbz_name`: its primary, cover,
/// no-cover marker and layers, in listing order.
///
/// Stricter than [`classify`], because what it returns may be deleted:
/// * a stem matches exactly, or folded only when no archive here has that exact
///   stem (rule 6: `vol 01.mokuro` is `vol 01.cbz`'s when both spellings are listed);
/// * a name whose base is any archive's stem, of any of the
///   [`ARCHIVE_EXTENSIONS`], is that archive's and is never a layer here
///   (`Vol 01.5.mokuro` beside `Vol 01.5.zip` classifies as layer `5`, but a sweep
///   leaves it).
pub fn volume_files<'a>(cbz_name: &str, names: &[&'a str]) -> Vec<(&'a str, VolumeFile<'a>)> {
    let Some(anchor) = cbz_stem(cbz_name) else {
        return Vec::new();
    };
    let mut exact: HashSet<&str> = HashSet::new();
    let mut folded: HashSet<String> = HashSet::new();
    for name in names {
        if let Some(stem) = ARCHIVE_EXTENSIONS
            .iter()
            .find_map(|ext| strip_suffix_ci(name, ext))
        {
            exact.insert(stem);
            folded.insert(fold(stem));
        }
    }
    let anchor_fold = fold(anchor);
    let owns = |stem: &str| stem == anchor || (!exact.contains(stem) && fold(stem) == anchor_fold);
    let mut out = Vec::new();
    for &name in names {
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
            && !folded.contains(&fold(base))
            && let Some((stem, id, gz)) = split_layer(name)
            && owns(stem)
        {
            out.push((name, VolumeFile::Layer { id, gz }));
        }
    }
    out
}

/// The names [`volume_files`] finds: everything that goes when `cbz_name` goes.
pub fn sweep_set(cbz_name: &str, names: &[&str]) -> Vec<String> {
    volume_files(cbz_name, names)
        .into_iter()
        .map(|(name, _)| name.to_string())
        .collect()
}

/// The primary sidecar of `cbz_name` among `names`: an exact stem before a folded
/// one, `.mokuro` before `.mokuro.gz`.
pub fn primary_of<'a>(cbz_name: &str, names: &[&'a str]) -> Option<&'a str> {
    let anchor = cbz_stem(cbz_name)?;
    let files = volume_files(cbz_name, names);
    let rank = |name: &str, gz: bool| {
        let exact = split_mokuro(name).is_some_and(|(base, _)| base == anchor);
        (!exact, gz)
    };
    files
        .into_iter()
        .filter_map(|(name, file)| match file {
            VolumeFile::Primary { gz } => Some((rank(name, gz), name)),
            _ => None,
        })
        .min()
        .map(|(_, name)| name)
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
    fn suffixes_never_split_a_character() {
        assert_eq!(strip_suffix_ci("é", ".cbz"), None);
        assert_eq!(strip_suffix_ci("aé.CBZ", ".cbz"), Some("aé"));
        assert_eq!(cbz_stem(".cbz"), Some(""));
    }
}
