//! The files that belong to a volume archive and go when it goes (0.5.2
//! `ocr.generations.sidecar_siblings`, spec §8.3.5 / §8.9).

use std::path::{Path, PathBuf};

use bunko_core::sidecar_names::{self, LayerClass};

use crate::paths;

/// Every sidecar of the archive `cbz` (listed from its directory): `<stem>.mokuro`,
/// `<stem>.mokuro.gz`, `<stem>.webp`, `<stem>.nocover`, and every other file
/// [`sidecar_names::sweep_set`] gives that volume (other spellings of those, its
/// layers). Folders are no sidecars and archive nothing (rule 2).
///
/// Never a file that, with `cbz` gone, is a correction or curated layer of
/// another listed archive: an archive nested under a volume's name (`V.edit.cbz` beside
/// `V.cbz`) would otherwise take `V`'s own `V.edit.mokuro` or `V.curated.mokuro` with
/// it, for good.
pub fn siblings(cbz: &Path) -> Vec<PathBuf> {
    let name = paths::file_name(cbz);
    let stem = paths::py_stem(&name).to_string();
    let Some(dir) = cbz.parent() else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = [".mokuro", ".mokuro.gz", ".webp", ".nocover"]
        .iter()
        .map(|s| dir.join(format!("{stem}{s}")))
        .collect();
    let Ok(entries) = bunko_store::fs::read_dir(dir) else {
        return out;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| !e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let listed: Vec<&str> = names.iter().map(String::as_str).collect();
    for entry in sidecar_names::sweep_set(&name, &listed) {
        let path = dir.join(&entry);
        if !out.contains(&path) {
            out.push(path);
        }
    }
    let others = sidecar_names::volume_stems(listed.iter().copied().filter(|n| *n != name));
    out.retain(|path| {
        let leaving = paths::file_name(path);
        !matches!(
            sidecar_names::layer_class_of_name(&leaving, &others),
            Some(LayerClass::Personal | LayerClass::Curated)
        )
    });
    out
}

/// `<stem>` when `name` is a bare primary sidecar `<stem>.mokuro[.gz]` (any case).
pub fn primary_sidecar_stem(name: &str) -> Option<&str> {
    sidecar_names::split_mokuro(name).map(|(stem, _)| stem)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // fixtures are written with std::fs
mod tests {
    use super::*;

    #[test]
    fn layers_and_other_volumes() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for n in [
            "Vol 01.cbz",
            "Vol 01.5.cbz",
            "Vol 01.mokuro",
            "Vol 01.5.mokuro",
            "Vol 01.hayai-nova.mokuro.gz",
            "Vol 01.backup.2024.mokuro",
            "Vol 01.Upper.mokuro",
            "Vol 02.hayai.mokuro",
        ] {
            std::fs::write(d.join(n), b"x").unwrap();
        }
        let got: Vec<String> = siblings(&d.join("Vol 01.cbz"))
            .iter()
            .map(|p| paths::file_name(p))
            .collect();
        assert!(got.contains(&"Vol 01.mokuro".to_string()));
        assert!(got.contains(&"Vol 01.hayai-nova.mokuro.gz".to_string()));
        assert!(!got.contains(&"Vol 01.5.mokuro".to_string()));
        assert!(!got.contains(&"Vol 01.backup.2024.mokuro".to_string()));
        assert!(!got.contains(&"Vol 01.Upper.mokuro".to_string()));
        assert!(!got.contains(&"Vol 02.hayai.mokuro".to_string()));
        assert!(!got.contains(&"Vol 01.cbz".to_string()));
        assert_eq!(primary_sidecar_stem("V.MOKURO"), Some("V"));
        assert_eq!(primary_sidecar_stem("V.mokuro.gz"), Some("V"));
        assert_eq!(primary_sidecar_stem("V.cbz"), None);
    }

    /// Rule v1's sweep: a folded stem is the archive's only while no archive here
    /// has that exact spelling (a case-sensitive store can hold both).
    #[test]
    fn folded_stems_only_without_an_exact_archive() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for n in ["Vol 01.cbz", "vol 01.hayai.mokuro", "VOL 01.MOKURO"] {
            std::fs::write(d.join(n), b"x").unwrap();
        }
        let names = |cbz: &str| -> Vec<String> {
            siblings(&d.join(cbz))
                .iter()
                .map(|p| paths::file_name(p))
                .collect()
        };
        let got = names("Vol 01.cbz");
        assert!(got.contains(&"vol 01.hayai.mokuro".to_string()));
        assert!(got.contains(&"VOL 01.MOKURO".to_string()));

        // Archives that differ only in case: a listing only a case-sensitive store can
        // hold. Rule v1 is a function of the listing, so it is checked on the listing
        // itself, the same on every host.
        let listing = [
            "Vol 01.cbz",
            "vol 01.cbz",
            "VOL 01.cbz",
            "vol 01.hayai.mokuro",
            "VOL 01.MOKURO",
        ];
        let got = sidecar_names::sweep_set("Vol 01.cbz", &listing);
        assert!(!got.contains(&"vol 01.hayai.mokuro".to_string()));
        assert!(!got.contains(&"VOL 01.MOKURO".to_string()));
        assert!(
            sidecar_names::sweep_set("vol 01.cbz", &listing)
                .contains(&"vol 01.hayai.mokuro".to_string())
        );
        assert!(
            sidecar_names::sweep_set("VOL 01.cbz", &listing).contains(&"VOL 01.MOKURO".to_string())
        );
    }

    /// C11: an archive nested under a volume's name leaves that volume's correction
    /// and curated layers; folders are no archives.
    #[test]
    fn a_nested_archive_leaves_the_outer_volumes_layers() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for n in [
            "V.cbz",
            "V.edit.cbz",
            "V.edit.mokuro",
            "V.curated.cbz",
            "V.curated.mokuro",
            "V.hayai.cbz",
            "V.hayai.mokuro",
            "V.hayai.webp",
        ] {
            std::fs::write(d.join(n), b"x").unwrap();
        }
        let names = |cbz: &str| -> Vec<String> {
            siblings(&d.join(cbz))
                .iter()
                .map(|p| paths::file_name(p))
                .collect()
        };
        assert!(!names("V.edit.cbz").contains(&"V.edit.mokuro".to_string()));
        assert!(!names("V.curated.cbz").contains(&"V.curated.mokuro".to_string()));
        // An engine id is no such layer: the nested volume's own primary goes with it.
        let hayai = names("V.hayai.cbz");
        assert!(hayai.contains(&"V.hayai.mokuro".to_string()));
        assert!(hayai.contains(&"V.hayai.webp".to_string()));
        // Alone, a dotted volume's primary goes with it.
        std::fs::remove_file(d.join("V.cbz")).unwrap();
        assert!(names("V.edit.cbz").contains(&"V.edit.mokuro".to_string()));
        // A folder named like an archive claims nothing.
        std::fs::create_dir(d.join("W.cbz")).unwrap();
        std::fs::write(d.join("W.curated.cbz"), b"x").unwrap();
        std::fs::write(d.join("W.curated.mokuro"), b"x").unwrap();
        assert!(names("W.curated.cbz").contains(&"W.curated.mokuro".to_string()));
    }

    #[test]
    fn every_archive_extension_keeps_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for n in [
            "Vol 01.cbz",
            "Vol 01.5.zip",
            "Vol 01.5.mokuro",
            "Vol 01.7.mokuro",
        ] {
            std::fs::write(d.join(n), b"x").unwrap();
        }
        let got: Vec<String> = siblings(&d.join("Vol 01.cbz"))
            .iter()
            .map(|p| paths::file_name(p))
            .collect();
        assert!(!got.contains(&"Vol 01.5.mokuro".to_string()));
        assert!(got.contains(&"Vol 01.7.mokuro".to_string()));
    }
}
