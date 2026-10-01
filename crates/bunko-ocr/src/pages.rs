//! Which images of a volume are its pages, in what order, and reading them one at a
//! time (spec §2.1–§2.3, §9).
//!
//! Archives are read with a small zip reader of our own rather than the `zip`
//! crate, because the names must decode exactly as Python's `zipfile` decodes them:
//! UTF-8 only when general-purpose flag bit 11 is set, cp437 otherwise, with the
//! Info-ZIP Unicode-path extra field (0x7075) ignored — `zip` honours that field and
//! rewrites the raw name with it, and it collapses duplicate names. Entries are
//! stored or deflated (others fail to read, and such a page takes the blank-page
//! path like any unreadable page); zip64 is supported; CRC-32 is checked like
//! `ZipFile.read` does. Pages stream from the file: only the member being read is
//! ever in memory.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use flate2::read::DeflateDecoder;

use crate::error::{Error, Result};
use crate::natsort;

/// `IMAGE_EXTENSIONS` of the 0.5.2 runner.
pub const IMAGE_EXTENSIONS: [&str; 5] = [".jpg", ".jpeg", ".png", ".webp", ".avif"];
/// Extensions the cover thumbnail considers (§9; differs from the page rule).
pub const COVER_EXTENSIONS: [&str; 8] = [
    ".jpg", ".jpeg", ".png", ".gif", ".bmp", ".webp", ".tiff", ".tif",
];

const CP437_HIGH: [u16; 128] = [
    0x00C7, 0x00FC, 0x00E9, 0x00E2, 0x00E4, 0x00E0, 0x00E5, 0x00E7, 0x00EA, 0x00EB, 0x00E8, 0x00EF,
    0x00EE, 0x00EC, 0x00C4, 0x00C5, 0x00C9, 0x00E6, 0x00C6, 0x00F4, 0x00F6, 0x00F2, 0x00FB, 0x00F9,
    0x00FF, 0x00D6, 0x00DC, 0x00A2, 0x00A3, 0x00A5, 0x20A7, 0x0192, 0x00E1, 0x00ED, 0x00F3, 0x00FA,
    0x00F1, 0x00D1, 0x00AA, 0x00BA, 0x00BF, 0x2310, 0x00AC, 0x00BD, 0x00BC, 0x00A1, 0x00AB, 0x00BB,
    0x2591, 0x2592, 0x2593, 0x2502, 0x2524, 0x2561, 0x2562, 0x2556, 0x2555, 0x2563, 0x2551, 0x2557,
    0x255D, 0x255C, 0x255B, 0x2510, 0x2514, 0x2534, 0x252C, 0x251C, 0x2500, 0x253C, 0x255E, 0x255F,
    0x255A, 0x2554, 0x2569, 0x2566, 0x2560, 0x2550, 0x256C, 0x2567, 0x2568, 0x2564, 0x2565, 0x2559,
    0x2558, 0x2552, 0x2553, 0x256B, 0x256A, 0x2518, 0x250C, 0x2588, 0x2584, 0x258C, 0x2590, 0x2580,
    0x03B1, 0x00DF, 0x0393, 0x03C0, 0x03A3, 0x03C3, 0x00B5, 0x03C4, 0x03A6, 0x0398, 0x03A9, 0x03B4,
    0x221E, 0x03C6, 0x03B5, 0x2229, 0x2261, 0x00B1, 0x2265, 0x2264, 0x2320, 0x2321, 0x00F7, 0x2248,
    0x00B0, 0x2219, 0x00B7, 0x221A, 0x207F, 0x00B2, 0x25A0, 0x00A0,
];

/// Python's `bytes.decode("cp437")`.
pub fn decode_cp437(raw: &[u8]) -> String {
    raw.iter()
        .map(|&b| {
            if b < 128 {
                b as char
            } else {
                char::from_u32(CP437_HIGH[(b - 128) as usize] as u32).unwrap_or('?')
            }
        })
        .collect()
}

/// One central-directory entry.
#[derive(Debug, Clone)]
pub struct ZipEntry {
    /// The name as Python's `zipfile` decodes it (cut at the first NUL).
    pub name: String,
    pub flags: u16,
    pub method: u16,
    pub crc32: u32,
    pub compressed_size: u64,
    pub size: u64,
    header_offset: u64,
}

impl ZipEntry {
    pub fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

/// A zip archive's directory, with the file kept open for reading members.
pub struct ZipArchive {
    path: PathBuf,
    file: File,
    entries: Vec<ZipEntry>,
    /// Bytes prepended to the archive (self-extractor stubs): Python's `concat`.
    concat: u64,
}

impl ZipArchive {
    pub fn open(path: &Path) -> Result<Self> {
        let bad = |msg: &str| Error::Archive {
            path: path.to_path_buf(),
            msg: msg.to_string(),
        };
        let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
        let len = file.metadata().map_err(|e| Error::io(path, e))?.len();
        // The end-of-central-directory record is in the last 22 + 65535 bytes.
        let tail_len = len.min(22 + 65535);
        file.seek(SeekFrom::Start(len - tail_len))
            .map_err(|e| Error::io(path, e))?;
        let mut tail = vec![0u8; tail_len as usize];
        file.read_exact(&mut tail).map_err(|e| Error::io(path, e))?;
        let eocd = (0..tail.len().saturating_sub(21))
            .rev()
            .find(|&i| tail[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])
            .ok_or_else(|| bad("not a zip file (no end of central directory)"))?;
        let eocd_pos = len - tail_len + eocd as u64;
        let mut count = le16(&tail, eocd + 10) as u64;
        let mut cd_size = le32(&tail, eocd + 12) as u64;
        let mut cd_offset = le32(&tail, eocd + 16) as u64;
        let mut cd_end = eocd_pos;
        // zip64 locator just before the EOCD.
        if eocd >= 20 && tail[eocd - 20..eocd - 16] == [0x50, 0x4b, 0x06, 0x07] {
            let rec_off = le64(&tail, eocd - 20 + 8);
            let mut rec = [0u8; 56];
            file.seek(SeekFrom::Start(rec_off))
                .map_err(|e| Error::io(path, e))?;
            file.read_exact(&mut rec).map_err(|e| Error::io(path, e))?;
            if rec[0..4] != [0x50, 0x4b, 0x06, 0x06] {
                return Err(bad("corrupt zip64 end of central directory"));
            }
            count = le64(&rec, 32);
            cd_size = le64(&rec, 40);
            cd_offset = le64(&rec, 48);
            cd_end = eocd_pos - 20 - 56;
        }
        let concat = cd_end
            .checked_sub(cd_size)
            .and_then(|v| v.checked_sub(cd_offset))
            .ok_or_else(|| bad("bad central directory offset"))?;
        let mut cd = vec![0u8; cd_size as usize];
        file.seek(SeekFrom::Start(cd_offset + concat))
            .map_err(|e| Error::io(path, e))?;
        file.read_exact(&mut cd).map_err(|e| Error::io(path, e))?;
        let mut entries = Vec::with_capacity(count as usize);
        let mut p = 0usize;
        while p + 46 <= cd.len() {
            if cd[p..p + 4] != [0x50, 0x4b, 0x01, 0x02] {
                return Err(bad("bad central directory entry"));
            }
            let flags = le16(&cd, p + 8);
            let method = le16(&cd, p + 10);
            let crc32 = le32(&cd, p + 16);
            let mut compressed_size = le32(&cd, p + 20) as u64;
            let mut size = le32(&cd, p + 24) as u64;
            let name_len = le16(&cd, p + 28) as usize;
            let extra_len = le16(&cd, p + 30) as usize;
            let comment_len = le16(&cd, p + 32) as usize;
            let mut header_offset = le32(&cd, p + 42) as u64;
            let name_start = p + 46;
            if name_start + name_len + extra_len > cd.len() {
                return Err(bad("truncated central directory"));
            }
            let raw = &cd[name_start..name_start + name_len];
            let raw = raw.iter().position(|&b| b == 0).map_or(raw, |n| &raw[..n]);
            let name = if flags & 0x800 != 0 {
                String::from_utf8_lossy(raw).into_owned()
            } else {
                decode_cp437(raw)
            };
            // zip64 extended information extra field
            let extra = &cd[name_start + name_len..name_start + name_len + extra_len];
            let mut e = 0usize;
            while e + 4 <= extra.len() {
                let id = le16(extra, e);
                let len = le16(extra, e + 2) as usize;
                let body = &extra[e + 4..(e + 4 + len).min(extra.len())];
                if id == 0x0001 {
                    let mut q = 0usize;
                    let mut next = |v: &mut u64| {
                        if *v == 0xFFFF_FFFF && q + 8 <= body.len() {
                            *v = le64(body, q);
                            q += 8;
                        }
                    };
                    next(&mut size);
                    next(&mut compressed_size);
                    next(&mut header_offset);
                }
                e += 4 + len;
            }
            entries.push(ZipEntry {
                name,
                flags,
                method,
                crc32,
                compressed_size,
                size,
                header_offset,
            });
            p = name_start + name_len + extra_len + comment_len;
        }
        Ok(Self {
            path: path.to_path_buf(),
            file,
            entries,
            concat,
        })
    }

    /// Entries in central-directory order (Python's `infolist()`).
    pub fn entries(&self) -> &[ZipEntry] {
        &self.entries
    }

    /// A streaming reader of entry `index`'s uncompressed bytes; the CRC is checked
    /// when the end is reached.
    pub fn reader(&mut self, index: usize) -> Result<impl Read + '_> {
        let entry = self
            .entries
            .get(index)
            .cloned()
            .ok_or_else(|| Error::Archive {
                path: self.path.clone(),
                msg: format!("no entry #{index}"),
            })?;
        let bad = |msg: String| Error::Archive {
            path: self.path.clone(),
            msg,
        };
        if entry.flags & 0x1 != 0 {
            return Err(bad(format!("{}: encrypted", entry.name)));
        }
        let mut lh = [0u8; 30];
        self.file
            .seek(SeekFrom::Start(entry.header_offset + self.concat))
            .map_err(|e| Error::io(&self.path, e))?;
        self.file
            .read_exact(&mut lh)
            .map_err(|e| Error::io(&self.path, e))?;
        if lh[0..4] != [0x50, 0x4b, 0x03, 0x04] {
            return Err(bad(format!("{}: bad local header", entry.name)));
        }
        let skip = le16(&lh, 26) as i64 + le16(&lh, 28) as i64;
        self.file
            .seek(SeekFrom::Current(skip))
            .map_err(|e| Error::io(&self.path, e))?;
        let raw = BufReader::new((&self.file).take(entry.compressed_size));
        let inner: Box<dyn Read + '_> = match entry.method {
            0 => Box::new(raw),
            8 => Box::new(DeflateDecoder::new(raw)),
            m => {
                return Err(bad(format!(
                    "{}: compression method {m} not supported",
                    entry.name
                )));
            }
        };
        Ok(CrcReader {
            inner,
            hasher: crc32fast::Hasher::new(),
            expected: entry.crc32,
            size: entry.size,
            seen: 0,
            checked: false,
            name: entry.name,
        })
    }

    /// Entry `index` read whole.
    pub fn read(&mut self, index: usize) -> Result<Vec<u8>> {
        let size = self.entries.get(index).map_or(0, |e| e.size);
        let path = self.path.clone();
        let mut out = Vec::with_capacity(size.min(1 << 30) as usize);
        self.reader(index)?
            .read_to_end(&mut out)
            .map_err(|e| Error::io(path, e))?;
        Ok(out)
    }
}

struct CrcReader<R> {
    inner: R,
    hasher: crc32fast::Hasher,
    expected: u32,
    size: u64,
    seen: u64,
    checked: bool,
    name: String,
}

impl<R: Read> Read for CrcReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.seen += n as u64;
        let corrupt = || {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("bad CRC-32 or size for '{}'", self.name),
            )
        };
        if self.seen > self.size {
            return Err(corrupt());
        }
        if n == 0 && !buf.is_empty() && !self.checked {
            self.checked = true;
            if self.seen != self.size || self.hasher.clone().finalize() != self.expected {
                return Err(corrupt());
            }
        }
        Ok(n)
    }
}

/// `ZipFile._extract_member`'s target path: separators normalized, empty, `.` and
/// `..` components dropped (Linux rules: only `/` separates).
pub fn extracted_name(member: &str) -> String {
    member
        .split('/')
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .collect::<Vec<_>>()
        .join("/")
}

/// Python `PurePath(name).suffix.lower()`.
pub fn suffix_lower(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rfind('.') {
        Some(i) if i > 0 && i < base.len() - 1 => base[i..].to_lowercase(),
        _ => String::new(),
    }
}

/// `member_map`: landed page path → the member name to read, later duplicates win.
/// Returned in first-insertion order (Python dict order).
pub fn member_map<'a>(
    names: impl IntoIterator<Item = &'a str>,
    stem: &str,
) -> Vec<(String, String)> {
    let thumbnail = format!("{stem}.webp");
    let mut out: Vec<(String, String)> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for member in names {
        let landed = extracted_name(member);
        if landed.is_empty() || !IMAGE_EXTENSIONS.contains(&suffix_lower(&landed).as_str()) {
            continue;
        }
        if landed == thumbnail {
            continue;
        }
        match at.get(&landed) {
            Some(&i) => out[i].1 = member.to_string(),
            None => {
                at.insert(landed.clone(), out.len());
                out.push((landed, member.to_string()));
            }
        }
    }
    out
}

/// The pages of one archive, in reading order, readable one at a time.
pub struct ArchivePages {
    zip: ZipArchive,
    pages: Vec<String>,
    /// page → entry index (the last entry with the member's name, as
    /// `ZipFile.read(name)` resolves it).
    index: HashMap<String, usize>,
}

impl ArchivePages {
    /// Open `archive`; `stem` is the library archive's stem (the thumbnail rule is
    /// keyed on it), defaulting to the file's own stem.
    pub fn open(archive: &Path, stem: Option<&str>) -> Result<Self> {
        let zip = ZipArchive::open(archive)?;
        let own_stem = archive
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let stem = stem.unwrap_or(&own_stem);
        let names: Vec<&str> = zip
            .entries()
            .iter()
            .filter(|e| !e.is_dir())
            .map(|e| e.name.as_str())
            .collect();
        let map = member_map(names, stem);
        let mut by_name: HashMap<&str, usize> = HashMap::new();
        for (i, e) in zip.entries().iter().enumerate() {
            by_name.insert(e.name.as_str(), i);
        }
        let mut index = HashMap::new();
        for (landed, member) in &map {
            if let Some(&i) = by_name.get(member.as_str()) {
                index.insert(landed.clone(), i);
            }
        }
        let mut pages: Vec<String> = map.into_iter().map(|(l, _)| l).collect();
        natsort::natsort(&mut pages, |s| s.as_str());
        Ok(Self { zip, pages, index })
    }

    /// Page paths (POSIX, relative), in reading order: the sidecar's `img_path`s.
    pub fn pages(&self) -> &[String] {
        &self.pages
    }

    /// The bytes of one page. An error means the page is unreadable (it then takes
    /// the blank-page path); it never affects other pages.
    pub fn read(&mut self, page: &str) -> Result<Vec<u8>> {
        let i = *self.index.get(page).ok_or_else(|| Error::Archive {
            path: self.zip.path.clone(),
            msg: format!("no page '{page}'"),
        })?;
        self.zip.read(i)
    }
}

/// The pages of an extracted directory: every file below `dir` with an image
/// extension (hidden files included), natsorted by relative POSIX path.
pub fn directory_pages(dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).map_err(|e| Error::io(&d, e))? {
            let entry = entry.map_err(|e| Error::io(&d, e))?;
            let path = entry.path();
            let ft = entry.file_type().map_err(|e| Error::io(&path, e))?;
            if ft.is_dir() || (ft.is_symlink() && path.is_dir()) {
                stack.push(path);
            } else if path.is_file() {
                let rel = path
                    .strip_prefix(dir)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if IMAGE_EXTENSIONS.contains(&suffix_lower(&rel).as_str()) {
                    out.push(rel);
                }
            }
        }
    }
    // glob order is not stable across filesystems; natsort ties are rare.
    out.sort();
    natsort::natsort(&mut out, |s| s.as_str());
    Ok(out)
}

/// The cover image of an archive for the thumbnail (§9): the first name in plain
/// code-point order whose suffix is a cover extension; directories, `__MACOSX` and
/// the embedded `<stem>.webp` are NOT excluded (0.5.2 quirk, kept).
pub fn cover_member(zip: &ZipArchive) -> Option<usize> {
    let mut names: Vec<(&str, usize)> = zip
        .entries()
        .iter()
        .enumerate()
        .map(|(i, e)| (e.name.as_str(), i))
        .collect();
    // `sorted(namelist())` is stable; `getinfo` of a duplicate name is the last entry.
    names.sort_by(|a, b| a.0.cmp(b.0));
    let first = names
        .iter()
        .find(|(n, _)| COVER_EXTENSIONS.contains(&suffix_lower(n).as_str()))?
        .0;
    zip.entries().iter().rposition(|e| e.name == first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_suffixes() {
        assert_eq!(extracted_name("a//./b/../c.jpg"), "a/b/c.jpg");
        assert_eq!(extracted_name("../../etc/x.webp"), "etc/x.webp");
        assert_eq!(suffix_lower("a/B.JPG"), ".jpg");
        assert_eq!(suffix_lower("a/.jpg"), "");
        assert_eq!(suffix_lower("a/x."), "");
        assert_eq!(decode_cp437(&[0x41, 0x82, 0xff]), "Aé\u{a0}");
    }

    #[test]
    fn member_map_rules() {
        let names = [
            "vol.webp",
            "a/1.jpg",
            "a//1.jpg",
            "notes.txt",
            "sub/vol.webp",
            "dir/",
        ];
        let m = member_map(names, "vol");
        assert_eq!(
            m,
            vec![
                ("a/1.jpg".to_string(), "a//1.jpg".to_string()),
                ("sub/vol.webp".to_string(), "sub/vol.webp".to_string())
            ]
        );
    }
}
