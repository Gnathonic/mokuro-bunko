//! Page image formats inside an archive, from its zip CENTRAL DIRECTORY only (design
//! §7): the end-of-central-directory record is found in the file's tail, the directory
//! is streamed one entry at a time (constant memory, whatever the entry count), and an
//! entry's data is touched only to sniff the first bytes of a page whose name has no
//! extension. Nothing is decompressed beyond those few bytes.

use std::collections::BTreeMap;
use std::io::{self, BufReader, Read, Seek, SeekFrom};

/// The formats reported; anything else that is still an image counts as `other`.
pub const FORMATS: [&str; 7] = ["jpeg", "png", "webp", "avif", "gif", "jxl", "other"];

/// Entries read from one directory at most (a hostile archive cannot make the scan
/// loop for ever; real volumes have a few hundred).
const MAX_ENTRIES: u64 = 1_000_000;
/// Extension-less entries sniffed per archive at most.
const MAX_SNIFFS: usize = 512;
/// The end record is 22 bytes plus a comment of at most 64 KiB; the zip64 locator sits
/// 20 bytes before it.
const TAIL: u64 = 22 + 0xFFFF + 20;

const EOCD_SIG: u32 = 0x0605_4b50;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;

/// What one archive holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageCount {
    pub pages: u64,
    /// Pages per format (keys from [`FORMATS`]).
    pub formats: BTreeMap<&'static str, u64>,
}

impl PageCount {
    /// `{format: pages}` as JSON.
    pub fn formats_json(&self) -> String {
        serde_json::to_string(&self.formats).unwrap_or_else(|_| "{}".into())
    }
}

/// The page format a lowercase extension names (`None`: not an image).
pub fn format_of_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "jpg" | "jpeg" | "jpe" | "jfif" => "jpeg",
        "png" => "png",
        "webp" => "webp",
        "avif" => "avif",
        "gif" => "gif",
        "jxl" => "jxl",
        "bmp" | "tif" | "tiff" | "heic" | "heif" => "other",
        _ => return None,
    })
}

/// The format a page's first bytes say (`None`: not an image we know).
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    let at = |i: usize, sig: &[u8]| head.len() >= i + sig.len() && &head[i..i + sig.len()] == sig;
    if at(0, &[0xFF, 0xD8, 0xFF]) {
        Some("jpeg")
    } else if at(0, b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if at(0, b"RIFF") && at(8, b"WEBP") {
        Some("webp")
    } else if at(4, b"ftypavif") || at(4, b"ftypavis") {
        Some("avif")
    } else if at(0, b"GIF87a") || at(0, b"GIF89a") {
        Some("gif")
    } else if at(0, &[0xFF, 0x0A]) || at(0, b"\0\0\0\x0cJXL \r\n\x87\n") {
        Some("jxl")
    } else if at(0, b"BM") || at(0, b"II*\0") || at(0, b"MM\0*") {
        Some("other")
    } else {
        None
    }
}

fn u16_at(b: &[u8], i: usize) -> u64 {
    u64::from(u16::from_le_bytes([b[i], b[i + 1]]))
}

fn u32_at(b: &[u8], i: usize) -> u64 {
    u64::from(u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]))
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[i..i + 8]);
    u64::from_le_bytes(a)
}

fn bad(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Where the central directory is: `(its offset in the file, its size, entries, the
/// offset every recorded offset is shifted by)`. The shift is non-zero for an archive
/// with data in front of it (a self-extractor), whose recorded offsets start at 0.
fn locate<R: Read + Seek>(r: &mut R) -> io::Result<(u64, u64, u64, u64)> {
    let len = r.seek(SeekFrom::End(0))?;
    if len < 22 {
        return Err(bad("too short for a zip"));
    }
    let tail_len = len.min(TAIL);
    r.seek(SeekFrom::Start(len - tail_len))?;
    let mut tail = vec![0u8; tail_len as usize];
    r.read_exact(&mut tail)?;
    let eocd = (0..=tail.len() - 22)
        .rev()
        .find(|&i| u32_at(&tail, i) == u64::from(EOCD_SIG))
        .ok_or_else(|| bad("no end of central directory"))?;
    let eocd_pos = len - tail_len + eocd as u64;
    let mut entries = u16_at(&tail, eocd + 10);
    let mut cd_size = u32_at(&tail, eocd + 12);
    let mut cd_offset = u32_at(&tail, eocd + 16);
    let mut cd_end = eocd_pos;
    if (entries == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF)
        && eocd >= 20
        && u32_at(&tail, eocd - 20) == u64::from(ZIP64_LOCATOR_SIG)
    {
        let z64_pos = u64_at(&tail, eocd - 20 + 8);
        r.seek(SeekFrom::Start(z64_pos))?;
        let mut rec = [0u8; 56];
        r.read_exact(&mut rec)?;
        if u32_at(&rec, 0) != u64::from(ZIP64_EOCD_SIG) {
            return Err(bad("bad zip64 end record"));
        }
        entries = u64_at(&rec, 32);
        cd_size = u64_at(&rec, 40);
        cd_offset = u64_at(&rec, 48);
        cd_end = z64_pos;
    }
    let cd_start = cd_end
        .checked_sub(cd_size)
        .ok_or_else(|| bad("central directory larger than the file"))?;
    let shift = cd_start
        .checked_sub(cd_offset)
        .ok_or_else(|| bad("central directory offset past its end"))?;
    Ok((cd_start, cd_size, entries, shift))
}

/// The lowercase extension of an entry's last path segment (`Some("")`: none at all;
/// `None`: a folder or a file that is never a page).
fn page_ext(name: &[u8]) -> Option<String> {
    if name.last().is_some_and(|&b| b == b'/' || b == b'\\') {
        return None;
    }
    // macOS resource forks (`__MACOSX/`, `._name`) are not pages.
    if name
        .split(|&b| b == b'/' || b == b'\\')
        .any(|seg| seg.eq_ignore_ascii_case(b"__MACOSX"))
    {
        return None;
    }
    let base = name
        .rsplit(|&b| b == b'/' || b == b'\\')
        .next()
        .unwrap_or(name);
    if base.is_empty() || base.starts_with(b".") {
        return None;
    }
    match base.iter().rposition(|&b| b == b'.') {
        None => Some(String::new()),
        Some(i) => Some(String::from_utf8_lossy(&base[i + 1..]).to_ascii_lowercase()),
    }
}

/// An extension-less entry to sniff once the directory is read.
struct Sniff {
    local_offset: u64,
    method: u64,
    compressed: u64,
}

/// Count the page images of the zip in `r` (see the module docs). An error means the
/// archive is not a zip, or its directory is damaged.
pub fn count_pages<R: Read + Seek>(r: &mut R) -> io::Result<PageCount> {
    let (cd_start, cd_size, entries, shift) = locate(r)?;
    r.seek(SeekFrom::Start(cd_start))?;
    let mut out = PageCount::default();
    let mut sniffs: Vec<Sniff> = Vec::new();
    {
        let mut cd = BufReader::with_capacity(32 * 1024, (&mut *r).take(cd_size));
        let mut head = [0u8; 46];
        let mut name: Vec<u8> = Vec::new();
        let mut extra: Vec<u8> = Vec::new();
        for _ in 0..entries.min(MAX_ENTRIES) {
            cd.read_exact(&mut head)?;
            if u32_at(&head, 0) != u64::from(CENTRAL_SIG) {
                return Err(bad("bad central directory entry"));
            }
            let flags = u16_at(&head, 8);
            let method = u16_at(&head, 10);
            let mut compressed = u32_at(&head, 20);
            let mut uncompressed = u32_at(&head, 24);
            let (name_len, extra_len, comment_len) =
                (u16_at(&head, 28), u16_at(&head, 30), u16_at(&head, 32));
            let mut local_offset = u32_at(&head, 42);
            name.resize(name_len as usize, 0);
            cd.read_exact(&mut name)?;
            extra.resize(extra_len as usize, 0);
            cd.read_exact(&mut extra)?;
            io::copy(&mut (&mut cd).take(comment_len), &mut io::sink())?;
            let Some(ext) = page_ext(&name) else { continue };
            if !ext.is_empty() {
                if let Some(f) = format_of_ext(&ext) {
                    out.pages += 1;
                    *out.formats.entry(f).or_default() += 1;
                }
                continue;
            }
            // Encrypted data cannot be sniffed; neither can more than the cap.
            if flags & 1 != 0 || sniffs.len() >= MAX_SNIFFS {
                continue;
            }
            // The zip64 extra field holds whichever sizes and offset overflowed, in
            // this order.
            let mut i = 0;
            while i + 4 <= extra.len() {
                let (id, size) = (u16_at(&extra, i), u16_at(&extra, i + 2) as usize);
                let body = extra.get(i + 4..i + 4 + size).unwrap_or(&[]);
                if id == 1 {
                    let mut at = 0;
                    let mut take = |v: &mut u64| {
                        if *v == 0xFFFF_FFFF && at + 8 <= body.len() {
                            *v = u64_at(body, at);
                            at += 8;
                        }
                    };
                    take(&mut uncompressed);
                    take(&mut compressed);
                    take(&mut local_offset);
                }
                i += 4 + size;
            }
            sniffs.push(Sniff {
                local_offset: local_offset + shift,
                method,
                compressed,
            });
        }
    }
    for s in sniffs {
        if let Some(f) = sniff_entry(r, &s).ok().flatten() {
            out.pages += 1;
            *out.formats.entry(f).or_default() += 1;
        }
    }
    Ok(out)
}

/// The first bytes of one entry's data (stored, or the start of a deflate stream).
fn sniff_entry<R: Read + Seek>(r: &mut R, s: &Sniff) -> io::Result<Option<&'static str>> {
    r.seek(SeekFrom::Start(s.local_offset))?;
    let mut local = [0u8; 30];
    r.read_exact(&mut local)?;
    if u32_at(&local, 0) != u64::from(LOCAL_SIG) {
        return Ok(None);
    }
    let skip = u16_at(&local, 26) + u16_at(&local, 28);
    r.seek(SeekFrom::Current(skip as i64))?;
    let data = (&mut *r).take(s.compressed.min(64 * 1024));
    let mut head = [0u8; 16];
    let n = match s.method {
        0 => read_up_to(data, &mut head)?,
        8 => read_up_to(flate2::read::DeflateDecoder::new(data), &mut head)?,
        _ => return Ok(None),
    };
    Ok(sniff(&head[..n]))
}

fn read_up_to(mut r: impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::CompressionMethod;
    use zip::write::SimpleFileOptions;

    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F'];
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    const WEBP: &[u8] = b"RIFF\x10\0\0\0WEBPVP8 ";
    const AVIF: &[u8] = b"\0\0\0\x1cftypavif\0\0\0\0";

    fn zip_of(files: &[(&str, &[u8], CompressionMethod)]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            for (name, data, method) in files {
                if name.ends_with('/') {
                    zw.add_directory(*name, SimpleFileOptions::default())
                        .unwrap();
                    continue;
                }
                zw.start_file(
                    *name,
                    SimpleFileOptions::default().compression_method(*method),
                )
                .unwrap();
                zw.write_all(data).unwrap();
            }
            zw.finish().unwrap();
        }
        buf.into_inner()
    }

    fn count(bytes: Vec<u8>) -> io::Result<PageCount> {
        count_pages(&mut Cursor::new(bytes))
    }

    #[test]
    fn pages_by_extension_from_the_directory() {
        let s = CompressionMethod::Stored;
        let d = CompressionMethod::Deflated;
        let z = zip_of(&[
            ("vol/", b"", s),
            ("vol/001.JPG", JPEG, d),
            ("vol/002.jpeg", JPEG, s),
            ("vol/003.png", PNG, d),
            ("vol/004.webp", WEBP, s),
            ("vol/005.avif", AVIF, s),
            ("vol/006.gif", b"GIF89a", s),
            ("vol/007.jxl", b"\xFF\x0A", s),
            ("vol/008.tiff", b"II*\0", s),
            ("vol/info.txt", b"hello", s),
            ("vol/ComicInfo.xml", b"<x/>", d),
            ("__MACOSX/vol/._001.JPG", JPEG, s),
            ("vol/._002.jpeg", JPEG, s),
        ]);
        let c = count(z).unwrap();
        assert_eq!(c.pages, 8);
        assert_eq!(
            c.formats,
            BTreeMap::from([
                ("avif", 1),
                ("gif", 1),
                ("jpeg", 2),
                ("jxl", 1),
                ("other", 1),
                ("png", 1),
                ("webp", 1)
            ])
        );
        assert_eq!(
            c.formats_json(),
            r#"{"avif":1,"gif":1,"jpeg":2,"jxl":1,"other":1,"png":1,"webp":1}"#
        );
    }

    #[test]
    fn extensionless_pages_are_sniffed_stored_or_deflated() {
        let z = zip_of(&[
            ("p1", PNG, CompressionMethod::Stored),
            ("p2", JPEG, CompressionMethod::Deflated),
            ("p3", WEBP, CompressionMethod::Deflated),
            (
                "README",
                b"not an image at all",
                CompressionMethod::Deflated,
            ),
        ]);
        let c = count(z).unwrap();
        assert_eq!(c.pages, 3);
        assert_eq!(
            c.formats,
            BTreeMap::from([("jpeg", 1), ("png", 1), ("webp", 1)])
        );
    }

    #[test]
    fn data_in_front_of_the_zip_shifts_every_offset() {
        let mut z = b"MZ self-extractor stub ....".to_vec();
        z.extend(zip_of(&[
            ("a.png", PNG, CompressionMethod::Stored),
            ("b", JPEG, CompressionMethod::Stored),
        ]));
        let c = count(z).unwrap();
        assert_eq!(c.formats, BTreeMap::from([("jpeg", 1), ("png", 1)]));
    }

    #[test]
    fn a_comment_after_the_directory_is_skipped() {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            zw.set_comment("x".repeat(4000));
            zw.start_file("a.webp", SimpleFileOptions::default())
                .unwrap();
            zw.write_all(WEBP).unwrap();
            zw.finish().unwrap();
        }
        assert_eq!(count(buf.into_inner()).unwrap().pages, 1);
    }

    #[test]
    fn not_a_zip_is_an_error() {
        assert!(count(b"Rar!\x1a\x07\x01\0 a rar volume, not a zip".to_vec()).is_err());
        assert!(count(Vec::new()).is_err());
        // A damaged directory: the end record points at garbage.
        let mut z = zip_of(&[("a.png", PNG, CompressionMethod::Stored)]);
        let n = z.len();
        let cd = u32::from_le_bytes([z[n - 6], z[n - 5], z[n - 4], z[n - 3]]) as usize;
        z[cd] = 0;
        assert!(count(z).is_err());
    }

    #[test]
    fn a_zip64_directory_of_many_entries() {
        // More than 65 535 entries forces the zip64 end record.
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            let o = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            for i in 0..66_000 {
                zw.start_file(format!("{i}.jpg"), o).unwrap();
            }
            zw.start_file("last", o).unwrap();
            zw.write_all(PNG).unwrap();
            zw.finish().unwrap();
        }
        let c = count(buf.into_inner()).unwrap();
        assert_eq!(c.pages, 66_001);
        assert_eq!(c.formats.get("png"), Some(&1));
    }

    #[test]
    fn sniffing_knows_the_formats() {
        assert_eq!(sniff(JPEG), Some("jpeg"));
        assert_eq!(sniff(PNG), Some("png"));
        assert_eq!(sniff(WEBP), Some("webp"));
        assert_eq!(sniff(AVIF), Some("avif"));
        assert_eq!(sniff(b"GIF87a.."), Some("gif"));
        assert_eq!(sniff(b"\0\0\0\x0cJXL \r\n\x87\n"), Some("jxl"));
        assert_eq!(sniff(b"BM...."), Some("other"));
        assert_eq!(sniff(b"PK\x03\x04"), None);
        assert_eq!(sniff(b""), None);
    }
}
