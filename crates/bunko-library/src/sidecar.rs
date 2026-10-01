//! Sidecar file names and contents: the primary `<stem>.mokuro[.gz]`, OCR
//! layers `<stem>.<layer>.mokuro[.gz]`, covers and markers
//! (`ocr/generations.py` helpers and `metadata/compiler.py::_load_sidecar`).

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::pyjson::{self, JsonObject, JsonValue};

/// Archive extensions a volume can have (`VOLUME_ARCHIVE_EXTENSIONS`).
pub const VOLUME_ARCHIVE_EXTENSIONS: &[&str] = &[".cbz", ".cbr", ".zip", ".rar"];

/// Suffixes of the files that belong to a volume besides layers.
pub const VOLUME_SIDE_SUFFIXES: &[&str] = &[".mokuro", ".mokuro.gz", ".webp", ".nocover"];

/// `pathlib.PurePath.suffix` (3.12) of a file name.
pub fn py_suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(index) if index > 0 && index < name.len() - 1 => &name[index..],
        _ => "",
    }
}

/// `Path(name).stem` / `with_suffix("").name`: the name minus its last suffix
/// (`Vol 1.5.cbz` -> `Vol 1.5`).
pub fn py_stem(name: &str) -> &str {
    &name[..name.len() - py_suffix(name).len()]
}

/// `path.with_suffix(suffix)` for a path whose file name is valid UTF-8.
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{}{suffix}", py_stem(&name)))
}

/// `<stem>.webp` beside the archive (`OCRProcessor.get_cover_path`).
pub fn cover_path(cbz_path: &Path) -> PathBuf {
    with_suffix(cbz_path, ".webp")
}

/// `<stem>.nocover` beside the archive: "thumbnailing was tried and failed".
pub fn nocover_marker_path(cbz_path: &Path) -> PathBuf {
    with_suffix(cbz_path, ".nocover")
}

/// `split_layer_sidecar`: `("Vol 1", "hayai-nova")` for
/// `Vol 1.hayai-nova.mokuro[.gz]`; `None` for the primary, non-sidecars and
/// postfixes that are not reader layer ids. Splits on the LAST dot.
pub fn split_layer_sidecar(file_name: &str) -> Option<(&str, &str)> {
    let name = file_name.strip_suffix(".gz").unwrap_or(file_name);
    let middle = name.strip_suffix(".mokuro")?;
    let cut = middle.rfind('.')?;
    if cut == 0 {
        return None;
    }
    let layer = &middle[cut + 1..];
    if !bunko_core::generations::is_layer_id(layer) {
        return None;
    }
    Some((&middle[..cut], layer))
}

/// `layer_id_of_sidecar`: the layer id of `<stem>.<id>.mokuro[.gz]`, unless
/// `<stem>.<id>` is itself another volume here (decimal volume numbers:
/// `Volume 01.5.mokuro` is volume 1.5's primary when `Volume 01.5.cbz` exists).
pub fn layer_id_of_sidecar<'a>(
    file_name: &'a str,
    stem: &str,
    other_volumes: &HashSet<String>,
) -> Option<&'a str> {
    let (owner, layer) = split_layer_sidecar(file_name)?;
    if owner != stem || other_volumes.contains(&format!("{stem}.{layer}")) {
        return None;
    }
    Some(layer)
}

/// `volume_stems`: stems of the volume archives among these names.
pub fn volume_stems<'a>(file_names: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
    let mut stems = HashSet::new();
    for name in file_names {
        let lower = crate::pyunicode::lower(name);
        for ext in VOLUME_ARCHIVE_EXTENSIONS {
            if lower.ends_with(ext) {
                // `name[: -len(ext)]`: by character count, as Python slices.
                let keep = name.chars().count() - ext.len();
                stems.insert(name.chars().take(keep).collect());
                break;
            }
        }
    }
    stems
}

/// `sidecar_siblings(cbz_path)`: every file that goes when the archive goes —
/// primary, gzip primary, cover, no-cover marker (whether or not they exist),
/// then every layer sidecar of this stem found in the directory (sorted).
pub fn sidecar_siblings(cbz_path: &Path) -> Vec<PathBuf> {
    let Some(name) = cbz_path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let stem = py_stem(name);
    let directory = cbz_path.parent().unwrap_or_else(|| Path::new(""));
    let mut siblings: Vec<PathBuf> = VOLUME_SIDE_SUFFIXES
        .iter()
        .map(|suffix| directory.join(format!("{stem}{suffix}")))
        .collect();
    let Ok(listing) = fs::read_dir(directory) else {
        return siblings;
    };
    let mut names: Vec<String> = listing
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    let mut others = volume_stems(names.iter().map(String::as_str));
    others.remove(stem);
    for entry_name in &names {
        if layer_id_of_sidecar(entry_name, stem, &others).is_some() {
            siblings.push(directory.join(entry_name));
        }
    }
    siblings
}

/// `_sidecar_for`: `<stem>.mokuro` if it is a file, else `<stem>.mokuro.gz`.
pub fn primary_sidecar(cbz_path: &Path) -> Option<PathBuf> {
    [".mokuro", ".mokuro.gz"]
        .iter()
        .map(|suffix| with_suffix(cbz_path, suffix))
        .find(|candidate| fs::metadata(candidate).is_ok_and(|meta| meta.is_file()))
}

/// A sidecar read the way `_load_sidecar` reads it.
#[derive(Debug, Clone)]
pub struct LoadedSidecar {
    /// The parsed object; `None` when unreadable or not a JSON object.
    pub data: Option<JsonObject>,
    /// Lowercase hex SHA-256 of the stored JSON bytes (after gunzip), only
    /// when `data` parsed.
    pub sha256: Option<String>,
}

/// Decompress a gzip file the way Python's `gzip` module does: concatenated
/// members, zero padding between/after members tolerated, anything else after
/// a member is an error.
fn gunzip_python(mut data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        if data.is_empty() {
            return Ok(out);
        }
        if data.len() < 2 || data[..2] != [0x1f, 0x8b] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Not a gzipped file",
            ));
        }
        let mut decoder = flate2::bufread::GzDecoder::new(data);
        decoder.read_to_end(&mut out)?;
        let mut rest = decoder.into_inner();
        // Skip the zero padding gzip allows after a member.
        while let Some((&0, tail)) = rest.split_first() {
            rest = tail;
        }
        data = rest;
    }
}

/// Read the raw JSON bytes of a sidecar (gunzipped when the name ends `.gz`).
pub fn read_sidecar_bytes(path: &Path) -> io::Result<Vec<u8>> {
    let raw = fs::read(path)?;
    let gz = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| crate::pyunicode::lower(name).ends_with(".gz"));
    if gz { gunzip_python(&raw) } else { Ok(raw) }
}

/// `_load_sidecar`: parse a `.mokuro`/`.mokuro.gz` and hash the bytes it parsed.
pub fn load_sidecar(path: &Path) -> LoadedSidecar {
    let unreadable = LoadedSidecar {
        data: None,
        sha256: None,
    };
    let Ok(raw) = read_sidecar_bytes(path) else {
        return unreadable;
    };
    let Ok(text) = std::str::from_utf8(&raw) else {
        return unreadable;
    };
    match pyjson::parse(text) {
        Ok(JsonValue::Object(object)) => LoadedSidecar {
            data: Some(object),
            sha256: Some(hex::encode(Sha256::digest(&raw))),
        },
        _ => unreadable,
    }
}

/// `mokuro_sha256` of a sidecar if it parses as a JSON object.
pub fn sidecar_sha256(path: &Path) -> Option<String> {
    load_sidecar(path).sha256
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_names() {
        assert_eq!(
            split_layer_sidecar("Vol 1.hayai-nova.mokuro"),
            Some(("Vol 1", "hayai-nova"))
        );
        assert_eq!(
            split_layer_sidecar("Vol 1.hayai-nova.mokuro.gz"),
            Some(("Vol 1", "hayai-nova"))
        );
        assert_eq!(split_layer_sidecar("Vol 1.mokuro"), None);
        assert_eq!(split_layer_sidecar("Vol 1.Bad.mokuro"), None);
        assert_eq!(split_layer_sidecar(".x.mokuro"), None);
        let others: HashSet<String> = ["Volume 01.5".to_owned()].into();
        assert_eq!(
            layer_id_of_sidecar("Volume 01.5.mokuro", "Volume 01", &others),
            None
        );
        assert_eq!(
            layer_id_of_sidecar("Volume 01.5.mokuro", "Volume 01", &HashSet::new()),
            Some("5")
        );
    }

    #[test]
    fn stems() {
        assert_eq!(py_stem("Vol 1.5.cbz"), "Vol 1.5");
        assert_eq!(py_stem(".cbz"), ".cbz");
        assert_eq!(py_stem("a."), "a.");
        assert_eq!(
            volume_stems(["A.CBZ", "b.rar", "c.txt"]),
            ["A".to_owned(), "b".to_owned()].into()
        );
    }

    #[test]
    fn gzip_members_and_padding() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let member = |text: &[u8]| {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(text).unwrap();
            encoder.finish().unwrap()
        };
        let mut data = member(b"{\"a\":");
        data.extend(member(b"1}"));
        data.extend([0, 0, 0]);
        assert_eq!(gunzip_python(&data).unwrap(), b"{\"a\":1}");
        data.push(7);
        assert!(gunzip_python(&data).is_err());
        let truncated = &member(b"hello")[..10];
        assert!(gunzip_python(truncated).is_err());
    }
}
