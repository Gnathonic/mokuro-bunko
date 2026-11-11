//! Sidecar file names and contents: the primary `<stem>.mokuro[.gz]`, OCR
//! layers `<stem>.<layer>.mokuro[.gz]`, covers and markers
//! (`ocr/generations.py` helpers and `metadata/compiler.py::_load_sidecar`).

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use bunko_core::sidecar_names;
use bunko_store::fs;
use sha2::{Digest, Sha256};

use crate::pyjson::{self, JsonObject, JsonValue};

/// Archive extensions a volume can have (`VOLUME_ARCHIVE_EXTENSIONS`). Only `.cbz`
/// anchors sidecars (rule v1, [`sidecar_names`]).
pub const VOLUME_ARCHIVE_EXTENSIONS: &[&str] = &sidecar_names::ARCHIVE_EXTENSIONS;

/// Suffixes of the files that belong to a volume besides layers.
pub const VOLUME_SIDE_SUFFIXES: &[&str] = &[".mokuro", ".mokuro.gz", ".webp", ".nocover"];

/// A volume archive's file name: ends `.cbz` in any case (`Vol 1.CBZ` is one too).
/// Everything that recognises archives (covers, the catalog index, OCR, uploads) asks
/// this; 0.5's directory walks (`rglob("*.cbz")`) were case-sensitive by accident while
/// its other checks lowered the name first.
pub fn is_cbz_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() >= 4 && b[b.len() - 4..].eq_ignore_ascii_case(b".cbz")
}

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

/// `<stem>.webp` beside the archive (`OCRProcessor.get_cover_path`) as the folder
/// lists it ([`listed_file_in`]), `None` when it lists none: a cover's size and time are
/// read only from a listed name, never from a constructed one a host might open in
/// another spelling. For reads: a writer asks [`sibling`].
pub fn cover_path(cbz_path: &Path) -> Option<PathBuf> {
    listed_own_file(cbz_path, ".webp")
}

/// `<stem>.nocover` beside the archive, "thumbnailing was tried and failed", as the
/// folder lists it (`None` when it lists none), as [`cover_path`].
pub fn nocover_marker_path(cbz_path: &Path) -> Option<PathBuf> {
    listed_own_file(cbz_path, ".nocover")
}

/// [`listed_file_in`] over a fresh listing of the archive's folder.
fn listed_own_file(cbz_path: &Path, suffix: &str) -> Option<PathBuf> {
    let directory = cbz_path.parent().unwrap_or_else(|| Path::new(""));
    listed_file_in(cbz_path, suffix, &Listed::of(directory)?)
}

/// The archive's `<stem><suffix>` when `listed` lists it ([`listed_sibling_in`]: in
/// any spelling, rule v1 giving it to this archive), `None` otherwise: whether such a
/// file exists, decided from the listing alone.
pub fn listed_file_in(cbz_path: &Path, suffix: &str, listed: &Listed) -> Option<PathBuf> {
    listed_sibling_in(cbz_path, suffix, listed).filter(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| listed.files.iter().any(|f| f == n))
    })
}

/// One listing of a folder: the names of its files, sorted (what rule v1 and a
/// [`sidecar_names::Folder`] read: a folder is never an archive nor a sidecar, rule 2),
/// and the names of its folders (a folder spelled like a sidecar or a cover still takes
/// that name: nothing is written beside it in another spelling).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listed {
    pub files: Vec<String>,
    pub dirs: Vec<String>,
}

impl Listed {
    /// `directory`'s UTF-8 entries (`None` when it cannot be listed).
    pub fn of(directory: &Path) -> Option<Listed> {
        let mut out = Listed::default();
        for entry in fs::read_dir(directory).ok()?.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                out.dirs.push(name);
            } else {
                out.files.push(name);
            }
        }
        out.files.sort();
        out.dirs.sort();
        Some(out)
    }

    /// Files only (a caller that listed them; no folder known).
    pub fn files(files: Vec<String>) -> Listed {
        Listed {
            files,
            dirs: Vec::new(),
        }
    }

    /// Every entry as `(name, is_dir)`.
    pub fn entries(&self) -> impl Iterator<Item = (&str, bool)> {
        self.files
            .iter()
            .map(|n| (n.as_str(), false))
            .chain(self.dirs.iter().map(|n| (n.as_str(), true)))
    }
}

/// The file `<stem><suffix>` of the archive `cbz_path` as Bunko reads and writes it:
/// the name the folder lists in any spelling (`V.WEBP`, `v.mokuro`), else the
/// constructed `<stem><suffix>` (a brand-new name keeps the archive's spelling). Bunko
/// never creates a second spelling of a name.
///
/// `None` when nothing may be written for it: the folder's case-variant groups freeze
/// the archive or the name ([`bunko_core::fold_groups`]: the archive is a member of a
/// group, its own files hold one, or the name is a file rule v1 gives a frozen
/// archive), a folder there takes the name in some spelling, or every listed spelling
/// of the name belongs to another archive (rule v1: an exact stem is that archive's, a
/// shadowed archive's included), so writing it would overwrite that volume's file.
/// Such a volume is owed nothing until the case variants are merged by hand (doctor
/// reports them). The freeze of a folder above this one is the walk's
/// ([`crate::fsutil::frozen`]).
pub fn sibling(cbz_path: &Path, suffix: &str) -> Option<PathBuf> {
    let directory = cbz_path.parent().unwrap_or_else(|| Path::new(""));
    match Listed::of(directory) {
        Some(listed) => sibling_in(cbz_path, suffix, &listed),
        None => Some(with_suffix(cbz_path, suffix)),
    }
}

/// [`sibling`] for reads: the file the folder lists for the archive (a frozen one's
/// included, the reachable winner being served as ever), `None` when the archive is
/// shadowed or the name is another archive's or a folder's.
pub fn listed_sibling(cbz_path: &Path, suffix: &str) -> Option<PathBuf> {
    let directory = cbz_path.parent().unwrap_or_else(|| Path::new(""));
    match Listed::of(directory) {
        Some(listed) => listed_sibling_in(cbz_path, suffix, &listed),
        None => Some(with_suffix(cbz_path, suffix)),
    }
}

/// [`sibling`] over `listed`, the archive folder's listing (taken once by a caller that
/// resolves several): `None` where the freeze holds the archive or the name, else
/// [`listed_sibling_in`].
pub fn sibling_in(cbz_path: &Path, suffix: &str, listed: &Listed) -> Option<PathBuf> {
    let name = cbz_path.file_name()?.to_str()?;
    let directory = cbz_path.parent().unwrap_or_else(|| Path::new(""));
    let groups = crate::fsutil::fold_groups(directory, listed.entries());
    let wanted = format!("{}{suffix}", py_stem(name));
    if groups.is_frozen(name) || groups.is_frozen(&wanted) {
        return None;
    }
    listed_sibling_in(cbz_path, suffix, listed)
}

/// [`listed_sibling`] over `listed`.
///
/// The listed names that are `<stem><suffix>` by the filename fold
/// ([`sidecar_names::fold`]) are that one name: a new file there would be a second
/// spelling of it. A folder among them takes the name: `None`. Of the files, only those
/// rule v1 gives to this archive ([`Folder::volume_files`] over the whole listing,
/// shadowed archives included) are its own; of several, the one a request reaches (the
/// store's case winner, else the byte-smallest). When every listed spelling is another
/// archive's, or no archive's by rule v1 (`V.FAST.mokuro`, whose layer id rule v1
/// matches exactly; `STRASSE.mokuro` beside `Straße.cbz`, whose stems rule v1 tells
/// apart), nothing may be written for it: `None`.
///
/// [`Folder::volume_files`]: sidecar_names::Folder::volume_files
pub fn listed_sibling_in(cbz_path: &Path, suffix: &str, listed: &Listed) -> Option<PathBuf> {
    let names = &listed.files;
    let name = cbz_path.file_name()?.to_str()?;
    let directory = cbz_path.parent().unwrap_or_else(|| Path::new(""));
    let stem = py_stem(name);
    let wanted = format!("{stem}{suffix}");
    let losers = crate::fsutil::case_losers(directory, names.iter().map(String::as_str));
    if losers.contains(name) {
        return None;
    }
    let fold = sidecar_names::fold(&wanted);
    if listed
        .dirs
        .iter()
        .any(|d| *d == wanted || sidecar_names::fold(d) == fold)
    {
        return None;
    }
    let candidates: Vec<&String> = names
        .iter()
        .filter(|n| **n == wanted || sidecar_names::fold(n) == fold)
        .collect();
    if candidates.is_empty() {
        return Some(directory.join(wanted));
    }
    // Rule v1 over the whole listing (the archive itself counted even when a caller
    // asks for one that is not listed yet).
    let mut listing: Vec<&str> = names.iter().map(String::as_str).collect();
    if !listing.contains(&name) {
        listing.push(name);
    }
    let folder = sidecar_names::Folder::new(&listing);
    let own: Vec<&str> = folder
        .volume_files(name)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    let ours: Vec<String> = candidates
        .into_iter()
        .filter(|c| own.contains(&c.as_str()))
        .cloned()
        .collect();
    crate::fsutil::pick_winner(directory, &fold, ours).map(|c| directory.join(c))
}

/// `split_layer_sidecar`: `("Vol 1", "hayai-nova")` for
/// `Vol 1.hayai-nova.mokuro[.gz]`; `None` for the primary, non-sidecars and
/// postfixes that are not reader layer ids. Splits on the LAST dot.
///
/// The 0.5.2 listing-free split, suffixes exact as Python had them (the golden
/// vectors pin it); the catalog index's layer lists and the manifest still pair
/// through it. Anything that decides what a listed file IS goes through
/// [`sidecar_names::classify`].
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

/// The names of the FILES in `directory` (`None` when it cannot be listed), sorted:
/// what a [`sidecar_names::Folder`] is built from. A folder is never an archive nor a
/// sidecar (rule 2), whatever it is named.
pub fn list_names(directory: &Path) -> Option<Vec<String>> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                return None;
            }
            entry.file_name().into_string().ok()
        })
        .collect();
    names.sort();
    Some(names)
}

/// The names in the archive's directory.
fn listing(cbz_path: &Path) -> Option<Vec<String>> {
    list_names(cbz_path.parent().unwrap_or_else(|| Path::new("")))
}

/// `sidecar_siblings(cbz_path)`: every file that goes when the archive goes —
/// primary, gzip primary, cover, no-cover marker (whether or not they exist),
/// then the rest of [`sidecar_names::sweep_set`] (other spellings, layers; sorted).
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
    let Some(names) = listing(cbz_path) else {
        return siblings;
    };
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    for entry_name in sidecar_names::sweep_set(name, &names) {
        let path = directory.join(entry_name);
        if !siblings.contains(&path) {
            siblings.push(path);
        }
    }
    siblings
}

/// `_sidecar_for`: the archive's primary sidecar as the folder lists it, by rule v1
/// ([`sidecar_names::primary_of`]: an exact stem before a folded one, `.mokuro` before
/// `.mokuro.gz`, so `V.MOKURO` for `V.cbz`). Always resolved from the listing, never by
/// opening a constructed name: a case-insensitive disk would find `V.mokuro` and hand
/// back that spelling, a case-sensitive one would not, so the answer would depend on the
/// host. Lists the folder once per call.
pub fn primary_sidecar(cbz_path: &Path) -> Option<PathBuf> {
    let names = listing(cbz_path)?;
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    listed_file(cbz_path, |name| {
        sidecar_names::Folder::new(&names).primary_of(name)
    })
}

/// [`primary_sidecar`] with the archive's folder already listed: a pass over a whole
/// series lists it once.
pub fn primary_sidecar_in(cbz_path: &Path, folder: &sidecar_names::Folder) -> Option<PathBuf> {
    listed_file(cbz_path, |name| folder.primary_of(name))
}

/// The archive's layer `id`: `<stem>.<id>.mokuro[.gz]` in any spelling of the stem the
/// folder lists (rule v1, [`sidecar_names::Folder::layer_of`]), the listed name.
pub fn layer_sidecar_in(
    cbz_path: &Path,
    id: &str,
    folder: &sidecar_names::Folder,
) -> Option<PathBuf> {
    listed_file(cbz_path, |name| folder.layer_of(name, id))
}

/// The listed name `find` gives for the archive's name, when it is a file.
fn listed_file<'a>(cbz_path: &Path, find: impl FnOnce(&str) -> Option<&'a str>) -> Option<PathBuf> {
    let found = cbz_path.with_file_name(find(cbz_path.file_name()?.to_str()?)?);
    is_file(&found).then_some(found)
}

fn is_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file())
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
    // The hash covers the stored bytes, BOM included: what the reader hashes.
    match pyjson::parse(pyjson::strip_bom(text)) {
        Ok(JsonValue::Object(object)) => LoadedSidecar {
            data: Some(object),
            sha256: Some(hex::encode(Sha256::digest(&raw))),
        },
        _ => unreadable,
    }
}

/// The sidecar's JSON starts with a UTF-8 BOM (after gunzip). Reads only the head: a
/// compile asks it of cache rows that recorded the sidecar as unreadable.
pub fn starts_with_bom(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let gz = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| crate::pyunicode::lower(name).ends_with(".gz"));
    let mut head = [0u8; 3];
    let read = if gz {
        flate2::read::GzDecoder::new(file).read_exact(&mut head)
    } else {
        file.take(3).read_exact(&mut head)
    };
    read.is_ok() && head == *b"\xef\xbb\xbf"
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
    }

    #[test]
    fn stems() {
        assert_eq!(py_stem("Vol 1.5.cbz"), "Vol 1.5");
        assert_eq!(py_stem(".cbz"), ".cbz");
        assert_eq!(py_stem("a."), "a.");
    }

    fn touch(dir: &Path, names: &[&str]) {
        for name in names {
            fs::write(dir.join(name), b"{}").unwrap();
        }
    }

    fn sibling_names(cbz: &Path) -> Vec<String> {
        sidecar_siblings(cbz)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// Rule v1 over the listing: the primary is found in any spelling, the exact stem
    /// first, then `.mokuro` before `.mokuro.gz`, and the listed name comes back. Only
    /// one spelling of each name is ever written, so every host lists the same thing
    /// (listings only a case-sensitive store can hold are bunko-core's vectors).
    #[test]
    fn primary_in_any_case() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        touch(d, &["V.cbz", "V.MOKURO"]);
        assert_eq!(primary_sidecar(&d.join("V.cbz")), Some(d.join("V.MOKURO")));
        touch(d, &["V.mokuro.gz"]);
        assert_eq!(primary_sidecar(&d.join("V.cbz")), Some(d.join("V.MOKURO")));

        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        touch(d, &["W.cbz", "w.mokuro"]);
        assert_eq!(primary_sidecar(&d.join("W.cbz")), Some(d.join("w.mokuro")));
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        touch(d, &["W.cbz", "w.mokuro.gz"]);
        assert_eq!(
            primary_sidecar(&d.join("W.cbz")),
            Some(d.join("w.mokuro.gz"))
        );
    }

    /// A sibling is found in the spelling the folder lists, and a new one takes the
    /// archive's: no second spelling is ever made.
    #[test]
    fn siblings_resolve_to_the_listed_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        touch(d, &["V.cbz", "V.WEBP", "W.cbz"]);
        assert_eq!(sibling(&d.join("V.cbz"), ".webp"), Some(d.join("V.WEBP")));
        assert_eq!(cover_path(&d.join("V.cbz")), Some(d.join("V.WEBP")));
        assert_eq!(sibling(&d.join("W.cbz"), ".webp"), Some(d.join("W.webp")));
        // Nothing listed: no marker, whatever a host would open for the built name.
        assert_eq!(nocover_marker_path(&d.join("V.cbz")), None);
        assert_eq!(cover_path(&d.join("W.cbz")), None);
    }

    /// A cover or marker is read only from a name the folder lists for the archive:
    /// `STRASSE 1.webp` beside `Straße 1.cbz`, `Vol2\u{FE0F}.webp` beside `Vol2.cbz`
    /// (one name by the filename fold, another archive's stem by rule v1) are no
    /// covers, on a host that would open them for the built name too.
    #[test]
    fn covers_and_markers_are_listed_names_only() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        touch(
            d,
            &[
                "Straße 1.cbz",
                "STRASSE 1.webp",
                "Vol2.cbz",
                "Vol2\u{FE0F}.webp",
                "Vol1.cbz",
                "Vol1\u{200B}.nocover",
            ],
        );
        for (cbz, suffix) in [
            ("Straße 1.cbz", ".webp"),
            ("Vol2.cbz", ".webp"),
            ("Vol1.cbz", ".nocover"),
        ] {
            assert_eq!(listed_own_file(&d.join(cbz), suffix), None, "{cbz}");
            // Nothing is written beside them either: the name is taken.
            assert_eq!(sibling(&d.join(cbz), suffix), None, "{cbz}");
        }
    }

    /// A folder spelled like a sidecar or a cover takes the name: nothing is written
    /// beside it in another spelling, and it is no sidecar or cover.
    #[test]
    fn a_folder_takes_a_sidecars_name() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        touch(d, &["V.cbz"]);
        fs::create_dir(d.join("v.MOKURO")).unwrap();
        fs::create_dir(d.join("v.WEBP")).unwrap();
        for suffix in [".mokuro", ".webp"] {
            assert_eq!(sibling(&d.join("V.cbz"), suffix), None, "{suffix}");
            assert_eq!(listed_sibling(&d.join("V.cbz"), suffix), None, "{suffix}");
        }
        assert_eq!(cover_path(&d.join("V.cbz")), None);
        assert_eq!(
            sibling(&d.join("V.cbz"), ".nocover"),
            Some(d.join("V.nocover"))
        );
    }

    #[test]
    fn a_bom_is_seen_through_gzip() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::write(d.join("a.mokuro"), "\u{feff}{}").unwrap();
        fs::write(d.join("b.mokuro"), "{}").unwrap();
        fs::write(d.join("c.mokuro"), "").unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all("\u{feff}{}".as_bytes()).unwrap();
        fs::write(d.join("d.mokuro.gz"), gz.finish().unwrap()).unwrap();
        assert!(starts_with_bom(&d.join("a.mokuro")));
        assert!(!starts_with_bom(&d.join("b.mokuro")));
        assert!(!starts_with_bom(&d.join("c.mokuro")));
        assert!(!starts_with_bom(&d.join("missing.mokuro")));
        assert!(starts_with_bom(&d.join("d.mokuro.gz")));
    }

    /// A listed folder answers like a fresh listing, and a layer is found in any
    /// spelling of its stem (its id exact).
    #[test]
    fn lookups_in_a_listed_folder() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        touch(
            d,
            &["V.cbz", "v.MOKURO", "v.gcv.mokuro.gz", "W.cbz", "W.mokuro"],
        );
        // A folder named like an archive is none (rule 2).
        fs::create_dir(d.join("x.cbz")).unwrap();
        let names = list_names(d).unwrap();
        assert!(!names.iter().any(|n| n == "x.cbz"));
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let folder = sidecar_names::Folder::new(&names);
        assert_eq!(
            primary_sidecar_in(&d.join("V.cbz"), &folder),
            Some(d.join("v.MOKURO"))
        );
        assert_eq!(
            primary_sidecar_in(&d.join("W.cbz"), &folder),
            Some(d.join("W.mokuro"))
        );
        assert_eq!(primary_sidecar_in(&d.join("X.cbz"), &folder), None);
        assert_eq!(
            layer_sidecar_in(&d.join("V.cbz"), "gcv", &folder),
            Some(d.join("v.gcv.mokuro.gz"))
        );
        assert_eq!(layer_sidecar_in(&d.join("V.cbz"), "GCV", &folder), None);
        assert_eq!(layer_sidecar_in(&d.join("W.cbz"), "gcv", &folder), None);
    }

    #[test]
    fn siblings_follow_rule_v1() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        touch(
            d,
            &[
                "Vol 1.cbz",
                "Vol 1.5.zip",
                "Vol 1.5.mokuro",
                "Vol 1.2024.mokuro",
                "vol 1.gcv.MOKURO",
                "Vol 1.Upper.mokuro",
            ],
        );
        let got = sibling_names(&d.join("Vol 1.cbz"));
        assert_eq!(
            got,
            [
                "Vol 1.mokuro",
                "Vol 1.mokuro.gz",
                "Vol 1.webp",
                "Vol 1.nocover",
                "Vol 1.2024.mokuro",
                "vol 1.gcv.MOKURO",
            ]
        );
        // Rule v1 reads `Vol 1.5.mokuro` as layer `5` here (a .zip defines no stem),
        // but a sweep never takes a file whose base is any archive's stem.
        let stems = bunko_core::sidecar_names::volume_stems(["Vol 1.cbz", "Vol 1.5.zip"]);
        assert!(matches!(
            bunko_core::sidecar_names::classify("Vol 1.5.mokuro", &stems),
            bunko_core::sidecar_names::SidecarClass::Layer { id: "5", .. }
        ));
    }

    /// Rule v1: a leading BOM is tolerated on read and the hash still covers the
    /// stored bytes, BOM included (what the reader hashes).
    #[test]
    fn a_bom_sidecar_loads_and_hashes_its_raw_bytes() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let raw = "\u{feff}{\"pages\": []}".as_bytes();
        let plain = dir.path().join("V.mokuro");
        fs::write(&plain, raw).unwrap();
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(raw).unwrap();
        let gz = dir.path().join("W.mokuro.gz");
        fs::write(&gz, enc.finish().unwrap()).unwrap();
        let want = hex::encode(Sha256::digest(raw));
        for path in [&plain, &gz] {
            let loaded = load_sidecar(path);
            assert!(loaded.data.is_some(), "{}", path.display());
            assert_eq!(loaded.sha256.as_deref(), Some(want.as_str()));
        }
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
