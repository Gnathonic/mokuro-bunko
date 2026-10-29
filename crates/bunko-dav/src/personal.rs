//! Personal correction layers (D27): `<Vol>.edit[-n].mokuro` is one account's own.
//!
//! A library path whose name reads as a personal layer (exact id, sidecar names rule
//! v1) and is no listed archive's primary resolves per caller: the cloud-root owner's
//! is the physical file beside the archive (a cloud-direct reader shares it), any other
//! account's is its overlay `users/<u>/edits/<library rel>`. Nobody sees another
//! account's copy, anonymous sees none. A listing shows one only beside its `.cbz`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use bunko_core::reader_tree;
use bunko_core::sidecar_names::{self, LayerClass, SidecarClass};
use bunko_store::fs;

use crate::paths;

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
