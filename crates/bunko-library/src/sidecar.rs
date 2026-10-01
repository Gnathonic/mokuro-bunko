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

/// The largest sidecar this build reads, after gunzip (and the largest plain one).
///
/// A real `.mokuro` is a few MiB (a 1000-page volume with dense text is ~30 MiB); without
/// a cap, a 1.5 MB gzip of whitespace inflated to 1.5 GiB in memory on every read (the
/// metadata pass, the upload hooks, the corrupt-sidecar sweep). Over the cap a sidecar is
/// unreadable ([`io::ErrorKind::FileTooLarge`]): never parsed, never "repaired".
pub const MAX_SIDECAR_BYTES: u64 = 256 * 1024 * 1024;

fn too_large(cap: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        format!("sidecar larger than {} MiB", cap / (1024 * 1024)),
    )
}

/// Read a whole file, refusing ([`io::ErrorKind::FileTooLarge`]) one over `cap` bytes
/// without reading past it.
pub fn read_capped(path: &Path, cap: u64) -> io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let declared = file.metadata()?.len();
    if declared > cap {
        return Err(too_large(cap));
    }
    let mut out = Vec::with_capacity(declared as usize);
    file.take(cap + 1).read_to_end(&mut out)?;
    if out.len() as u64 > cap {
        return Err(too_large(cap));
    }
    Ok(out)
}

/// Where inflated bytes go: kept (into a buffer that never grows past its limit) or only
/// counted.
enum Sink<'a> {
    Keep(&'a mut Vec<u8>),
    Count(u64),
}

impl Sink<'_> {
    fn len(&self) -> u64 {
        match self {
            Sink::Keep(v) => v.len() as u64,
            Sink::Count(n) => *n,
        }
    }
}

/// Decompress a gzip file the way Python's `gzip` module does: concatenated
/// members, zero padding between/after members tolerated, anything else after
/// a member is an error. More than `limit` inflated bytes is
/// [`io::ErrorKind::FileTooLarge`]: inflation stops there and a kept buffer never grows
/// past `limit` (no doubling on the way).
fn gunzip_members(mut data: &[u8], limit: u64, sink: &mut Sink<'_>) -> io::Result<()> {
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        if data.is_empty() {
            return Ok(());
        }
        if data.len() < 2 || data[..2] != [0x1f, 0x8b] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Not a gzipped file",
            ));
        }
        let mut decoder = flate2::bufread::GzDecoder::new(data);
        loop {
            let n = match decoder.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if sink.len() + n as u64 > limit {
                return Err(too_large(limit));
            }
            match sink {
                Sink::Keep(v) => v.extend_from_slice(&chunk[..n]),
                Sink::Count(total) => *total += n as u64,
            }
        }
        let mut rest = decoder.into_inner();
        // Skip the zero padding gzip allows after a member.
        while let Some((&0, tail)) = rest.split_first() {
            rest = tail;
        }
        data = rest;
    }
}

/// [`gunzip_members`] into memory, at most `cap` bytes, allocated once at the inflated
/// size: the gzip trailer's size is trusted as a first guess (one pass for an honest
/// single-member file); when it is wrong (several members, or a lie) the output is
/// counted first, without keeping it, and inflated again into an exact buffer. A bomb
/// therefore costs CPU up to the cap, never memory.
fn gunzip_python(data: &[u8], cap: u64) -> io::Result<Vec<u8>> {
    let trailer = data.len().checked_sub(4).map(|at| {
        u64::from(u32::from_le_bytes([
            data[at],
            data[at + 1],
            data[at + 2],
            data[at + 3],
        ]))
    });
    if let Some(hint) = trailer.filter(|h| *h <= cap) {
        let mut out = Vec::with_capacity(hint as usize);
        match gunzip_members(data, hint, &mut Sink::Keep(&mut out)) {
            Ok(()) => return Ok(out),
            Err(e) if e.kind() == io::ErrorKind::FileTooLarge => {}
            Err(e) => return Err(e),
        }
    }
    let mut counted = Sink::Count(0);
    gunzip_members(data, cap, &mut counted)?;
    let mut out = Vec::with_capacity(counted.len() as usize);
    gunzip_members(data, cap, &mut Sink::Keep(&mut out))?;
    Ok(out)
}

/// Read the raw JSON bytes of a sidecar (gunzipped when the name ends `.gz`), at most
/// [`MAX_SIDECAR_BYTES`] of them.
pub fn read_sidecar_bytes(path: &Path) -> io::Result<Vec<u8>> {
    read_sidecar_bytes_capped(path, MAX_SIDECAR_BYTES)
}

/// [`read_sidecar_bytes`] with an explicit cap (tests, callers with a tighter budget).
pub fn read_sidecar_bytes_capped(path: &Path, cap: u64) -> io::Result<Vec<u8>> {
    let raw = read_capped(path, cap)?;
    let gz = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| crate::pyunicode::lower(name).ends_with(".gz"));
    if gz {
        gunzip_python(&raw, cap)
    } else {
        Ok(raw)
    }
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
        assert_eq!(
            gunzip_python(&data, MAX_SIDECAR_BYTES).unwrap(),
            b"{\"a\":1}"
        );
        data.push(7);
        assert!(gunzip_python(&data, MAX_SIDECAR_BYTES).is_err());
        let truncated = &member(b"hello")[..10];
        assert!(gunzip_python(truncated, MAX_SIDECAR_BYTES).is_err());
    }

    /// Regression (review finding): a small gzip that inflates past the cap is refused
    /// once the cap is crossed (FileTooLarge), not inflated into memory; a multi-member
    /// file counts every member; a plain file over the cap is refused unread.
    #[test]
    fn inflation_and_plain_reads_are_capped() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let mut enc = GzEncoder::new(Vec::new(), Compression::best());
        let spaces = vec![b' '; 1 << 20];
        for _ in 0..8 {
            enc.write_all(&spaces).unwrap();
        }
        let bomb = enc.finish().unwrap();
        assert!(bomb.len() < 64 * 1024);
        let gz = dir.path().join("V.mokuro.gz");
        fs::write(&gz, &bomb).unwrap();
        let err = read_sidecar_bytes_capped(&gz, 4 << 20).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
        // Allocated once, at the inflated size: no doubling past it.
        let whole = read_sidecar_bytes_capped(&gz, 8 << 20).unwrap();
        assert_eq!((whole.len(), whole.capacity()), (8 << 20, 8 << 20));
        // A trailer that lies about the size is not trusted for the allocation.
        let mut liar = bomb.clone();
        let at = liar.len() - 4;
        liar[at..].copy_from_slice(&16u32.to_le_bytes());
        fs::write(&gz, &liar).unwrap();
        assert_eq!(
            read_sidecar_bytes_capped(&gz, 4 << 20).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
        // (The CRC/size check still rejects the liar once inflated, as Python would.)
        assert!(read_sidecar_bytes_capped(&gz, 8 << 20).is_err());
        // Two members of 3 MiB each: 6 MiB in all.
        let mut two = Vec::new();
        for _ in 0..2 {
            let mut e = GzEncoder::new(Vec::new(), Compression::fast());
            e.write_all(&vec![b' '; 3 << 20]).unwrap();
            two.extend(e.finish().unwrap());
        }
        fs::write(&gz, &two).unwrap();
        assert_eq!(
            read_sidecar_bytes_capped(&gz, 5 << 20).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
        let plain = dir.path().join("V.mokuro");
        fs::write(&plain, vec![b' '; 2 << 20]).unwrap();
        assert_eq!(
            read_sidecar_bytes_capped(&plain, 1 << 20)
                .unwrap_err()
                .kind(),
            io::ErrorKind::FileTooLarge
        );
        assert_eq!(read_capped(&plain, 2 << 20).unwrap().len(), 2 << 20);
        // A sidecar under the cap still loads.
        fs::write(&plain, b"{\"pages\": []}").unwrap();
        assert!(load_sidecar(&plain).data.is_some());
    }
}
