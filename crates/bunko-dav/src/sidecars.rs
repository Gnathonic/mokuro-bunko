//! The files that belong to a volume archive and go when it goes (0.5.2
//! `ocr.generations.sidecar_siblings`, spec §8.3.5 / §8.9).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::paths;

/// Archive extensions a volume can have.
const VOLUME_EXTENSIONS: [&str; 4] = [".cbz", ".cbr", ".zip", ".rar"];

/// The reader's layer-id grammar `^[a-z0-9-]{1,32}$`.
fn is_layer_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `("Vol 1", "hayai-nova")` for `Vol 1.hayai-nova.mokuro[.gz]`; split on the LAST dot.
fn split_layer_sidecar(name: &str) -> Option<(&str, &str)> {
    let name = name.strip_suffix(".gz").unwrap_or(name);
    let middle = name.strip_suffix(".mokuro")?;
    let cut = middle.rfind('.')?;
    if cut == 0 {
        return None;
    }
    let layer = &middle[cut + 1..];
    is_layer_id(layer).then(|| (&middle[..cut], layer))
}

fn volume_stems<'a>(names: impl Iterator<Item = &'a str>) -> HashSet<String> {
    let mut stems = HashSet::new();
    for name in names {
        let lower = name.to_lowercase();
        for ext in VOLUME_EXTENSIONS {
            if lower.ends_with(ext) && lower.len() == name.len() {
                stems.insert(name[..name.len() - ext.len()].to_string());
                break;
            }
        }
    }
    stems
}

/// Every sidecar of the archive `cbz` (listed from its directory): `<stem>.mokuro`,
/// `<stem>.mokuro.gz`, `<stem>.webp`, `<stem>.nocover`, and every
/// `<stem>.<layer>.mokuro[.gz]` that is not the primary sidecar of another volume.
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
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut others = volume_stems(names.iter().map(String::as_str));
    others.remove(&stem);
    for entry in &names {
        if let Some((s, layer)) = split_layer_sidecar(entry)
            && s == stem
            && !others.contains(&format!("{stem}.{layer}"))
        {
            out.push(dir.join(entry));
        }
    }
    out
}

/// `<stem>` when `name` is a bare primary sidecar `<stem>.mokuro[.gz]` (any case).
pub fn primary_sidecar_stem(name: &str) -> Option<&str> {
    let lower = name.to_lowercase();
    if lower.len() != name.len() {
        return None;
    }
    if lower.ends_with(".mokuro.gz") {
        Some(&name[..name.len() - ".mokuro.gz".len()])
    } else if lower.ends_with(".mokuro") {
        Some(&name[..name.len() - ".mokuro".len()])
    } else {
        None
    }
}

#[cfg(test)]
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
        assert_eq!(primary_sidecar_stem("V.MOKURO"), Some("V"));
        assert_eq!(primary_sidecar_stem("V.mokuro.gz"), Some("V"));
        assert_eq!(primary_sidecar_stem("V.cbz"), None);
    }
}
