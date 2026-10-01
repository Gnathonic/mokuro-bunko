//! Volume archives: list a volume's pages in 0.5.2's order and stream one page
//! at a time. Never loads a whole archive: listing reads the zip central
//! directory only, and a page is a streaming decompressor over its member.
//!
//! Supported sources (what 0.5.2's OCR could read without the GPL mokuro CLI):
//! `.cbz` / `.zip` archives and directories of images. `.cbr` / `.rar` are
//! read only when they are really zip files (a common mislabel, sniffed by
//! magic number); actual RAR data is refused with [`ArchiveError::Unsupported`]
//! because every RAR decoder is under a non-OSI licence. 0.5.2 only handled
//! RAR inbox uploads by handing them to the (GPL, removed) mokuro CLI.

pub mod natsort;
pub mod zipdir;

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::compat;
use crate::pyunicode;
use zipdir::{ZipEntry, ZipError};

/// Page image suffixes the OCR runner accepts (`engine_runner.IMAGE_EXTENSIONS`).
pub const PAGE_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".webp", ".avif"];

/// Why a volume or page could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    /// The archive is damaged (`BadZipFile`, truncated data, bad CRC).
    #[error("damaged archive: {0}")]
    Damaged(String),
    /// A format or feature this build cannot read (RAR, an exotic compression
    /// method, encryption).
    #[error("unsupported archive: {0}")]
    Unsupported(String),
    /// No such page.
    #[error("no page {0}")]
    NoSuchPage(usize),
    /// The file could not be read at all.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<ZipError> for ArchiveError {
    fn from(error: ZipError) -> Self {
        match error {
            ZipError::BadZip(message) => ArchiveError::Damaged(message),
            ZipError::Unsupported(message) => ArchiveError::Unsupported(message),
            ZipError::Io(error) => ArchiveError::Io(error),
        }
    }
}

/// `pathlib.PurePath.suffix` (3.12) of the last component of a `/` path.
fn path_suffix(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(index) if index > 0 && index < name.len() - 1 => &name[index..],
        _ => "",
    }
}

fn is_page_name(path: &str) -> bool {
    PAGE_EXTENSIONS.contains(&pyunicode::lower(path_suffix(path)).as_str())
}

/// `extracted_name(member)` on POSIX: where `ZipFile.extractall` would put a
/// member — empty, `.` and `..` components dropped.
pub fn extracted_name(member: &str) -> String {
    member
        .split('/')
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .collect::<Vec<_>>()
        .join("/")
}

/// One page of a volume.
#[derive(Debug, Clone)]
pub struct Page {
    /// The page's path relative to the volume root (`/`-separated), as the
    /// extracted road would name it; this is the sidecar's `img_path` tail.
    pub path: String,
    source: PageSource,
}

#[derive(Debug, Clone)]
enum PageSource {
    Zip(usize),
    File(PathBuf),
}

#[derive(Debug)]
enum Source {
    Zip(Vec<ZipEntry>),
    Directory,
}

/// An opened volume: its page list in reading order.
#[derive(Debug)]
pub struct Volume {
    path: PathBuf,
    source: Source,
    pages: Vec<Page>,
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn sniff_zip(path: &Path) -> io::Result<bool> {
    let mut magic = [0u8; 4];
    let mut file = File::open(path)?;
    let read = file.read(&mut magic)?;
    Ok(read >= 2 && &magic[..2] == b"PK")
}

impl Volume {
    /// Open a `.cbz`/`.zip` (or zip-in-disguise `.cbr`/`.rar`) archive or a
    /// directory of images. The embedded-thumbnail rule is keyed on this
    /// path's own stem.
    pub fn open(path: &Path) -> Result<Volume, ArchiveError> {
        Self::open_with_stem(path, &file_stem(path))
    }

    /// [`Volume::open`] with the LIBRARY archive's stem given explicitly
    /// (the thumbnail rule: a top-level `<stem>.webp` is not a page).
    pub fn open_with_stem(path: &Path, stem: &str) -> Result<Volume, ArchiveError> {
        let metadata = fs::metadata(path)?;
        if metadata.is_dir() {
            return Self::open_directory(path);
        }
        let extension = path
            .extension()
            .map(|ext| pyunicode::lower(&ext.to_string_lossy()))
            .unwrap_or_default();
        if (extension == "cbr" || extension == "rar") && !sniff_zip(path)? {
            return Err(ArchiveError::Unsupported(
                "RAR archives cannot be read (no OSI-licensed decoder)".into(),
            ));
        }
        let entries = zipdir::list_entries(path)?;
        let pages = zip_pages(&entries, stem);
        Ok(Volume {
            path: path.to_path_buf(),
            source: Source::Zip(entries),
            pages,
        })
    }

    fn open_directory(root: &Path) -> Result<Volume, ArchiveError> {
        let mut pages = Vec::new();
        let mut visited = Vec::new();
        walk_pages(root, "", &mut pages, &mut visited, 0)?;
        natsort::natsorted(&mut pages, |page: &Page| page.path.as_str());
        Ok(Volume {
            path: root.to_path_buf(),
            source: Source::Directory,
            pages,
        })
    }

    /// The volume's pages, in 0.5.2's OCR reading order.
    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    /// The zip central directory, in archive order (None for a directory).
    pub fn zip_entries(&self) -> Option<&[ZipEntry]> {
        match &self.source {
            Source::Zip(entries) => Some(entries),
            Source::Directory => None,
        }
    }

    /// Stream page `index`'s bytes (CRC-checked for zip members).
    pub fn open_page(&self, index: usize) -> Result<Box<dyn Read + Send>, ArchiveError> {
        let page = self
            .pages
            .get(index)
            .ok_or(ArchiveError::NoSuchPage(index))?;
        match (&page.source, &self.source) {
            (PageSource::Zip(entry), Source::Zip(entries)) => {
                Ok(Box::new(zipdir::open_member(&self.path, &entries[*entry])?))
            }
            (PageSource::File(path), _) => Ok(Box::new(File::open(path)?)),
            (PageSource::Zip(_), Source::Directory) => Err(ArchiveError::NoSuchPage(index)),
        }
    }

    /// Read page `index` into memory (one page, never the archive).
    pub fn read_page(&self, index: usize) -> Result<Vec<u8>, ArchiveError> {
        let mut reader = self.open_page(index)?;
        let mut buffer = Vec::new();
        reader
            .read_to_end(&mut buffer)
            .map_err(|error| match error.kind() {
                io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => {
                    ArchiveError::Damaged(error.to_string())
                }
                _ => ArchiveError::Io(error),
            })?;
        Ok(buffer)
    }
}

/// `member_map` + `reading_order`: pages of a zip, as extraction would lay
/// them out (later members landing on the same path win), natsorted.
fn zip_pages(entries: &[ZipEntry], stem: &str) -> Vec<Page> {
    let thumbnail = format!("{stem}.webp");
    let mut pages: Vec<Page> = Vec::new();
    let mut slots: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.is_dir() {
            continue;
        }
        let landed = extracted_name(&entry.filename);
        if landed.is_empty() || !is_page_name(&landed) || landed == thumbnail {
            continue;
        }
        // A dict assignment: the slot keeps its first position, the value
        // (which member is read) is the last one.
        match slots.get(&landed) {
            Some(&slot) => pages[slot].source = PageSource::Zip(index),
            None => {
                slots.insert(landed.clone(), pages.len());
                pages.push(Page {
                    path: landed,
                    source: PageSource::Zip(index),
                });
            }
        }
    }
    natsort::natsorted(&mut pages, |page: &Page| page.path.as_str());
    pages
}

const MAX_WALK_DEPTH: usize = 64;

/// `input_dir.glob("**/*")` filtered to page files, relative `/` paths.
fn walk_pages(
    dir: &Path,
    prefix: &str,
    pages: &mut Vec<Page>,
    visited: &mut Vec<PathBuf>,
    depth: usize,
) -> io::Result<()> {
    if depth > MAX_WALK_DEPTH {
        return Ok(());
    }
    // Symlinked directories are followed; a loop back into an ancestor is not.
    if let Ok(canonical) = dir.canonicalize() {
        if visited.contains(&canonical) {
            return Ok(());
        }
        visited.push(canonical);
    }
    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            tracing::warn!(path = %entry.path().display(), "skipping a non-UTF-8 file name");
            continue;
        };
        entries.push((name, entry.path()));
    }
    for (name, path) in entries {
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        match fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => walk_pages(&path, &relative, pages, visited, depth + 1)?,
            Ok(meta) if meta.is_file() && is_page_name(&relative) => {
                pages.push(Page {
                    path: relative,
                    source: PageSource::File(path),
                });
            }
            _ => {}
        }
    }
    visited.pop();
    Ok(())
}

/// The images a READER would treat as pages of a `.cbz`, in archive order —
/// the metadata compiler's `_archive_image_names`. `None` = the file could not
/// be read at all (unknown, not cacheable); `Some(vec![])` = damaged (every
/// page missing).
pub fn reader_image_names(cbz_path: &Path) -> Option<Vec<String>> {
    let stem = file_stem(cbz_path);
    let cover = format!("{}.webp", pyunicode::lower(&stem));
    let entries = match zipdir::list_entries(cbz_path) {
        Ok(entries) => entries,
        Err(ZipError::Io(_)) => return None,
        // BadZipFile/EOFError are damage. 0.5.2 crashed the whole pass on
        // NotImplementedError / undecodable UTF-8 names; a reader cannot open
        // those archives either, so they count as damaged too.
        Err(ZipError::BadZip(_) | ZipError::Unsupported(_)) => return Some(Vec::new()),
    };
    let mut names = Vec::new();
    for entry in entries {
        let name = entry.filename;
        if name.ends_with('/') || compat::is_system_file(&name) {
            continue;
        }
        if !compat::is_image_extension(&compat::trailing_extension(&name)) {
            continue;
        }
        let basename = match name.rsplit('/').next() {
            Some(last) if !last.is_empty() => last,
            _ => name.as_str(),
        };
        if pyunicode::lower(basename) == cover {
            continue;
        }
        names.push(name);
    }
    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-member stored zip (local header + central directory + EOCD).
    fn stored_zip(name: &str, data: &[u8], crc: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let n = name.len() as u16;
        let len = data.len() as u32;
        out.extend(b"PK\x03\x04");
        for v in [20u16, 0, 0, 0, 0] {
            out.extend(v.to_le_bytes());
        }
        for v in [crc, len, len] {
            out.extend(v.to_le_bytes());
        }
        out.extend(n.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(name.as_bytes());
        out.extend(data);
        let cd_offset = out.len() as u32;
        out.extend(b"PK\x01\x02");
        out.extend([20u8, 3, 20, 0]);
        for v in [0u16, 0, 0, 0] {
            out.extend(v.to_le_bytes());
        }
        for v in [crc, len, len] {
            out.extend(v.to_le_bytes());
        }
        for v in [n, 0, 0, 0, 0] {
            out.extend(v.to_le_bytes());
        }
        out.extend(0u32.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(name.as_bytes());
        let cd_size = out.len() as u32 - cd_offset;
        out.extend(b"PK\x05\x06");
        for v in [0u16, 0, 1, 1] {
            out.extend(v.to_le_bytes());
        }
        out.extend(cd_size.to_le_bytes());
        out.extend(cd_offset.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    #[test]
    fn pages_stream_and_crc_is_checked() {
        let dir = tempfile::tempdir().unwrap();
        let data = vec![7u8; 100_000];
        let good = dir.path().join("v.cbz");
        std::fs::write(&good, stored_zip("p1.jpg", &data, crc32fast::hash(&data))).unwrap();
        let volume = Volume::open(&good).unwrap();
        assert_eq!(volume.pages().len(), 1);
        let mut reader = volume.open_page(0).unwrap();
        let mut chunk = [0u8; 4096];
        let mut total = 0;
        loop {
            let n = reader.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(total, data.len());

        let bad = dir.path().join("bad.cbz");
        std::fs::write(&bad, stored_zip("p1.jpg", &data, 1234)).unwrap();
        let volume = Volume::open(&bad).unwrap();
        assert!(matches!(volume.read_page(0), Err(ArchiveError::Damaged(_))));
        assert!(matches!(
            volume.read_page(5),
            Err(ArchiveError::NoSuchPage(5))
        ));
    }

    #[test]
    fn rar_is_refused_but_a_zip_named_cbr_opens() {
        let dir = tempfile::tempdir().unwrap();
        let disguised = dir.path().join("v.cbr");
        std::fs::write(
            &disguised,
            stored_zip("p1.png", b"x", crc32fast::hash(b"x")),
        )
        .unwrap();
        assert_eq!(Volume::open(&disguised).unwrap().pages().len(), 1);
        let rar = dir.path().join("w.rar");
        std::fs::write(&rar, b"Rar!\x1a\x07\x01\x00rest").unwrap();
        assert!(matches!(
            Volume::open(&rar),
            Err(ArchiveError::Unsupported(_))
        ));
        let missing = dir.path().join("none.cbz");
        assert!(matches!(Volume::open(&missing), Err(ArchiveError::Io(_))));
        assert_eq!(reader_image_names(&missing), None);
        let junk = dir.path().join("junk.cbz");
        std::fs::write(&junk, b"short").unwrap();
        assert_eq!(reader_image_names(&junk), Some(Vec::new()));
    }

    #[test]
    fn suffix_rules() {
        assert_eq!(path_suffix("a/b.JPG"), ".JPG");
        assert_eq!(path_suffix(".jpg"), "");
        assert_eq!(path_suffix("a.jpg."), "");
        assert_eq!(extracted_name("../a//./b.jpg"), "a/b.jpg");
    }
}
