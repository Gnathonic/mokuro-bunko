//! A `.zip` central-directory reader that agrees with CPython 3.12's
//! `zipfile` on what an archive contains.
//!
//! Which entries exist, under which names, and which archives are "damaged"
//! decide page counts, missing pages and OCR page order, so this mirrors
//! `zipfile._EndRecData` / `_RealGetContents` / `ZipInfo._decodeExtra`
//! (zip64 records, prepended data, CP437 names, the Info-ZIP Unicode Path
//! field, NUL-truncated names, duplicate names kept) rather than relying on a
//! general-purpose zip crate whose choices differ (the `zip` crate collapses
//! duplicate names, for one). Only the central directory is read to list an
//! archive; members are streamed one at a time.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use flate2::read::DeflateDecoder;

const SIZE_END_CENT_DIR: u64 = 22;
const SIZE_END_CENT_DIR64: u64 = 56;
const SIZE_END_CENT_DIR64_LOCATOR: u64 = 20;
const SIZE_CENTRAL_DIR: usize = 46;
const SIZE_FILE_HEADER: usize = 30;
const ZIP_MAX_COMMENT: u64 = (1 << 16) - 1;
const MAX_EXTRACT_VERSION: u8 = 63;

const STRING_END_ARCHIVE: &[u8; 4] = b"PK\x05\x06";
const STRING_END_ARCHIVE64: &[u8; 4] = b"PK\x06\x06";
const STRING_END_ARCHIVE64_LOCATOR: &[u8; 4] = b"PK\x06\x07";
const STRING_CENTRAL_DIR: &[u8; 4] = b"PK\x01\x02";
const STRING_FILE_HEADER: &[u8; 4] = b"PK\x03\x04";

const MASK_ENCRYPTED: u16 = 1 << 0;
const MASK_COMPRESSED_PATCH: u16 = 1 << 5;
const MASK_STRONG_ENCRYPTION: u16 = 1 << 6;
const MASK_UTF_FILENAME: u16 = 1 << 11;

/// Why an archive (or a member) could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ZipError {
    /// `zipfile.BadZipFile` / `EOFError`: the archive is damaged.
    #[error("bad zip file: {0}")]
    BadZip(String),
    /// `NotImplementedError` / an unsupported feature (compression method,
    /// encryption, zip version): the reader cannot open it either.
    #[error("unsupported zip feature: {0}")]
    Unsupported(String),
    /// `OSError`: the file could not be read at all (permissions, a mount
    /// that went away). Not damage.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl ZipError {
    fn bad(message: impl Into<String>) -> Self {
        ZipError::BadZip(message.into())
    }
}

/// One central-directory entry, as `zipfile.ZipInfo` holds it.
#[derive(Debug, Clone)]
pub struct ZipEntry {
    /// `ZipInfo.filename`: decoded, NUL-truncated (or the Unicode Path field).
    pub filename: String,
    /// `ZipInfo.orig_filename`: the decoded name before sanitizing.
    pub orig_filename: String,
    pub flag_bits: u16,
    pub compress_type: u16,
    pub crc: u32,
    pub compress_size: u64,
    pub file_size: u64,
    pub header_offset: u64,
    end_offset: u64,
}

impl ZipEntry {
    /// `ZipInfo.is_dir()` on POSIX: the name ends with `/`.
    pub fn is_dir(&self) -> bool {
        self.filename.ends_with('/')
    }
}

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn u64_at(data: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/// Read up to `len` bytes (`file.read(n)` semantics: short at EOF).
fn read_up_to(reader: &mut impl Read, len: usize) -> io::Result<Vec<u8>> {
    let mut buffer = Vec::with_capacity(len.min(1 << 20));
    reader.take(len as u64).read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// The end-of-central-directory facts `_RealGetContents` uses.
struct EndRecord {
    size_cd: u64,
    offset_cd: u64,
    location: u64,
    is_zip64: bool,
}

fn end_rec_data64(
    file: &mut (impl Read + Seek),
    offset: u64,
    record: EndRecord,
) -> Result<EndRecord, ZipError> {
    let Some(offset) = offset.checked_sub(SIZE_END_CENT_DIR64_LOCATOR) else {
        return Ok(record);
    };
    file.seek(SeekFrom::Start(offset))?;
    let data = read_up_to(file, SIZE_END_CENT_DIR64_LOCATOR as usize)?;
    if data.len() != SIZE_END_CENT_DIR64_LOCATOR as usize {
        return Err(ZipError::Io(io::Error::other("Unknown I/O error")));
    }
    if &data[0..4] != STRING_END_ARCHIVE64_LOCATOR {
        return Ok(record);
    }
    let diskno = u32_at(&data, 4);
    let reloff = u64_at(&data, 8);
    let disks = u32_at(&data, 16);
    if diskno != 0 || disks > 1 {
        return Err(ZipError::bad(
            "zipfiles that span multiple disks are not supported",
        ));
    }
    let Some(offset) = offset.checked_sub(SIZE_END_CENT_DIR64) else {
        return Err(ZipError::bad(
            "Corrupt zip64 end of central directory locator",
        ));
    };
    if reloff > offset {
        return Err(ZipError::bad(
            "Corrupt zip64 end of central directory locator",
        ));
    }
    file.seek(SeekFrom::Start(reloff))?;
    let mut extrasz = offset - reloff;
    let mut data = read_up_to(file, SIZE_END_CENT_DIR64 as usize)?;
    if data.len() != SIZE_END_CENT_DIR64 as usize {
        return Err(ZipError::Io(io::Error::other("Unknown I/O error")));
    }
    if !data.starts_with(STRING_END_ARCHIVE64) && reloff != offset {
        file.seek(SeekFrom::Start(offset))?;
        extrasz = 0;
        data = read_up_to(file, SIZE_END_CENT_DIR64 as usize)?;
        if data.len() != SIZE_END_CENT_DIR64 as usize {
            return Err(ZipError::Io(io::Error::other("Unknown I/O error")));
        }
    }
    if !data.starts_with(STRING_END_ARCHIVE64) {
        return Err(ZipError::bad(
            "Zip64 end of central directory record not found",
        ));
    }
    let sz = u64_at(&data, 4);
    let dirsize = u64_at(&data, 40);
    let diroffset = u64_at(&data, 48);
    if diroffset.checked_add(dirsize) != Some(reloff)
        || sz.checked_add(12) != Some(SIZE_END_CENT_DIR64 + extrasz)
    {
        return Err(ZipError::bad(
            "Corrupt zip64 end of central directory record",
        ));
    }
    Ok(EndRecord {
        size_cd: dirsize,
        offset_cd: diroffset,
        location: offset - extrasz,
        is_zip64: true,
    })
}

fn end_rec_data(file: &mut (impl Read + Seek)) -> Result<Option<EndRecord>, ZipError> {
    let filesize = file.seek(SeekFrom::End(0))?;
    if filesize < SIZE_END_CENT_DIR {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(filesize - SIZE_END_CENT_DIR))?;
    let data = read_up_to(file, SIZE_END_CENT_DIR as usize)?;
    if data.len() == SIZE_END_CENT_DIR as usize
        && &data[0..4] == STRING_END_ARCHIVE
        && data[20..22] == [0, 0]
    {
        let record = EndRecord {
            size_cd: u64::from(u32_at(&data, 12)),
            offset_cd: u64::from(u32_at(&data, 16)),
            location: filesize - SIZE_END_CENT_DIR,
            is_zip64: false,
        };
        return end_rec_data64(file, filesize - SIZE_END_CENT_DIR, record).map(Some);
    }
    let max_comment_start = filesize.saturating_sub(ZIP_MAX_COMMENT + SIZE_END_CENT_DIR);
    file.seek(SeekFrom::Start(max_comment_start))?;
    let data = read_up_to(file, (ZIP_MAX_COMMENT + SIZE_END_CENT_DIR) as usize)?;
    let Some(start) = data
        .windows(4)
        .rposition(|window| window == STRING_END_ARCHIVE)
    else {
        return Ok(None);
    };
    let rec = &data[start..];
    if rec.len() < SIZE_END_CENT_DIR as usize {
        return Ok(None);
    }
    let record = EndRecord {
        size_cd: u64::from(u32_at(rec, 12)),
        offset_cd: u64::from(u32_at(rec, 16)),
        location: max_comment_start + start as u64,
        is_zip64: false,
    };
    end_rec_data64(file, max_comment_start + start as u64, record).map(Some)
}

/// IBM code page 437, as CPython's `cp437` codec decodes it.
#[rustfmt::skip]
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', //
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', //
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', //
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', //
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', //
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', //
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', //
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

fn decode_cp437(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&byte| {
            if byte < 0x80 {
                char::from(byte)
            } else {
                CP437_HIGH[usize::from(byte - 0x80)]
            }
        })
        .collect()
}

fn decode_name(raw: &[u8], flags: u16) -> Result<String, ZipError> {
    if flags & MASK_UTF_FILENAME != 0 {
        // Python raises UnicodeDecodeError here (not caught by 0.5.2, which
        // aborted the whole pass); a reader cannot open such an entry either,
        // so it is treated as damage.
        String::from_utf8(raw.to_vec()).map_err(|_| ZipError::bad("invalid UTF-8 file name"))
    } else {
        Ok(decode_cp437(raw))
    }
}

/// `_sanitize_filename` on POSIX: cut at the first NUL.
fn sanitize_filename(name: &str) -> String {
    match name.find('\0') {
        Some(index) => name[..index].to_owned(),
        None => name.to_owned(),
    }
}

/// `ZipInfo._decodeExtra`.
fn decode_extra(entry: &mut ZipEntry, extra: &[u8], filename_crc: u32) -> Result<(), ZipError> {
    let mut extra = extra;
    while extra.len() >= 4 {
        let tp = u16_at(extra, 0);
        let ln = usize::from(u16_at(extra, 2));
        if ln + 4 > extra.len() {
            return Err(ZipError::bad(format!(
                "Corrupt extra field {tp:04x} (size={ln})"
            )));
        }
        let data = &extra[4..ln + 4];
        if tp == 0x0001 {
            let mut data = data;
            let mut take = |field: &str| -> Result<u64, ZipError> {
                if data.len() < 8 {
                    return Err(ZipError::bad(format!(
                        "Corrupt zip64 extra field. {field} not found."
                    )));
                }
                let value = u64_at(data, 0);
                data = &data[8..];
                Ok(value)
            };
            if entry.file_size == 0xFFFF_FFFF_FFFF_FFFF || entry.file_size == 0xFFFF_FFFF {
                entry.file_size = take("File size")?;
            }
            if entry.compress_size == 0xFFFF_FFFF {
                entry.compress_size = take("Compress size")?;
            }
            if entry.header_offset == 0xFFFF_FFFF {
                entry.header_offset = take("Header offset")?;
            }
        } else if tp == 0x7075 {
            if data.len() < 5 {
                return Err(ZipError::bad("Corrupt unicode path extra field (0x7075)"));
            }
            let version = data[0];
            let name_crc = u32_at(data, 1);
            if version == 1 && name_crc == filename_crc {
                let name = std::str::from_utf8(&data[5..]).map_err(|_| {
                    ZipError::bad("Corrupt unicode path extra field (0x7075): invalid utf-8 bytes")
                })?;
                if !name.is_empty() {
                    entry.filename = sanitize_filename(name);
                }
            }
        }
        extra = &extra[ln + 4..];
    }
    Ok(())
}

/// The central directory of the archive at `path`, in archive order
/// (`ZipFile.infolist()`), duplicates included.
pub fn list_entries(path: &Path) -> Result<Vec<ZipEntry>, ZipError> {
    let mut file = File::open(path)?;
    read_central_directory(&mut file)
}

/// [`list_entries`] over any seekable reader.
pub fn read_central_directory(file: &mut (impl Read + Seek)) -> Result<Vec<ZipEntry>, ZipError> {
    // `_RealGetContents` turns any OSError while locating the end record
    // into BadZipFile.
    let record = match end_rec_data(file) {
        Ok(Some(record)) => record,
        Ok(None) | Err(ZipError::Io(_)) => return Err(ZipError::bad("File is not a zip file")),
        Err(error) => return Err(error),
    };
    let mut concat = record.location as i128 - record.size_cd as i128 - record.offset_cd as i128;
    if record.is_zip64 {
        concat -= i128::from(SIZE_END_CENT_DIR64 + SIZE_END_CENT_DIR64_LOCATOR);
    }
    let start_dir = record.offset_cd as i128 + concat;
    if start_dir < 0 {
        return Err(ZipError::bad("Bad offset for central directory"));
    }
    file.seek(SeekFrom::Start(start_dir as u64))?;
    let mut reader = BufReader::new(file.take(record.size_cd));
    let mut entries = Vec::new();
    let mut total: u64 = 0;
    while total < record.size_cd {
        let mut centdir = [0u8; SIZE_CENTRAL_DIR];
        let got = read_up_to(&mut reader, SIZE_CENTRAL_DIR)?;
        if got.len() != SIZE_CENTRAL_DIR {
            return Err(ZipError::bad("Truncated central directory"));
        }
        centdir.copy_from_slice(&got);
        if &centdir[0..4] != STRING_CENTRAL_DIR {
            return Err(ZipError::bad("Bad magic number for central directory"));
        }
        let extract_version = centdir[6];
        let flag_bits = u16_at(&centdir, 8);
        let compress_type = u16_at(&centdir, 10);
        let crc = u32_at(&centdir, 16);
        let compress_size = u64::from(u32_at(&centdir, 20));
        let file_size = u64::from(u32_at(&centdir, 24));
        let name_len = usize::from(u16_at(&centdir, 28));
        let extra_len = usize::from(u16_at(&centdir, 30));
        let comment_len = usize::from(u16_at(&centdir, 32));
        let header_offset = u64::from(u32_at(&centdir, 42));
        let raw_name = read_up_to(&mut reader, name_len)?;
        let filename_crc = crc32fast::hash(&raw_name);
        let orig_filename = decode_name(&raw_name, flag_bits)?;
        let extra = read_up_to(&mut reader, extra_len)?;
        let _comment = read_up_to(&mut reader, comment_len)?;
        if extract_version > MAX_EXTRACT_VERSION {
            return Err(ZipError::Unsupported(format!(
                "zip file version {:.1}",
                f64::from(extract_version) / 10.0
            )));
        }
        let mut entry = ZipEntry {
            filename: sanitize_filename(&orig_filename),
            orig_filename,
            flag_bits,
            compress_type,
            crc,
            compress_size,
            file_size,
            header_offset,
            end_offset: 0,
        };
        decode_extra(&mut entry, &extra, filename_crc)?;
        let offset = entry.header_offset as i128 + concat;
        if offset < 0 {
            return Err(ZipError::bad("Bad offset for local header"));
        }
        entry.header_offset = offset as u64;
        entries.push(entry);
        total += (SIZE_CENTRAL_DIR + name_len + extra_len + comment_len) as u64;
    }
    // `_end_offset`: where the next entry (by offset) starts, for the
    // overlapped-entries check on open.
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by(|&a, &b| entries[b].header_offset.cmp(&entries[a].header_offset));
    let mut end_offset = start_dir as u64;
    for index in order {
        entries[index].end_offset = end_offset;
        end_offset = entries[index].header_offset;
    }
    Ok(entries)
}

/// A member's decompressed bytes, streamed, with the CRC checked at the end
/// (`zipfile.ZipExtFile`).
pub struct MemberReader {
    inner: Box<dyn Read + Send>,
    hasher: crc32fast::Hasher,
    expected_crc: u32,
    expected_size: u64,
    read: u64,
    done: bool,
}

impl Read for MemberReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.read += n as u64;
        if self.read > self.expected_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "member is larger than declared",
            ));
        }
        if n == 0 {
            self.done = true;
            if self.read != self.expected_size {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Compressed file ended before the end-of-stream marker was reached",
                ));
            }
            let crc = std::mem::take(&mut self.hasher).finalize();
            if crc != self.expected_crc {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Bad CRC-32"));
            }
        }
        Ok(n)
    }
}

/// Open `entry` of the archive at `path` for streaming (`ZipFile.open`).
pub fn open_member(path: &Path, entry: &ZipEntry) -> Result<MemberReader, ZipError> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(entry.header_offset))?;
    let header = read_up_to(&mut file, SIZE_FILE_HEADER)?;
    if header.len() != SIZE_FILE_HEADER {
        return Err(ZipError::bad("Truncated file header"));
    }
    if &header[0..4] != STRING_FILE_HEADER {
        return Err(ZipError::bad("Bad magic number for file header"));
    }
    let local_flags = u16_at(&header, 6);
    let name_len = usize::from(u16_at(&header, 26));
    let extra_len = u64::from(u16_at(&header, 28));
    let raw_name = read_up_to(&mut file, name_len)?;
    if extra_len > 0 {
        file.seek(SeekFrom::Current(extra_len as i64))?;
    }
    if entry.flag_bits & MASK_COMPRESSED_PATCH != 0 {
        return Err(ZipError::Unsupported(
            "compressed patched data (flag bit 5)".into(),
        ));
    }
    if entry.flag_bits & MASK_STRONG_ENCRYPTION != 0 {
        return Err(ZipError::Unsupported(
            "strong encryption (flag bit 6)".into(),
        ));
    }
    let local_name = if local_flags & MASK_UTF_FILENAME != 0 {
        String::from_utf8(raw_name).map_err(|_| ZipError::bad("invalid UTF-8 local file name"))?
    } else {
        decode_cp437(&raw_name)
    };
    if local_name != entry.orig_filename {
        return Err(ZipError::bad(format!(
            "File name in directory {:?} and header {:?} differ.",
            entry.orig_filename, local_name
        )));
    }
    let data_start = file.stream_position()?;
    if data_start + entry.compress_size > entry.end_offset
        && entry.end_offset != entry.header_offset
    {
        return Err(ZipError::bad(format!(
            "Overlapped entries: {:?} (possible zip bomb)",
            entry.orig_filename
        )));
    }
    if entry.flag_bits & MASK_ENCRYPTED != 0 {
        return Err(ZipError::Unsupported("encrypted member".into()));
    }
    let raw = BufReader::new(file.take(entry.compress_size));
    let inner: Box<dyn Read + Send> = match entry.compress_type {
        0 => Box::new(raw),
        8 => Box::new(DeflateDecoder::new(raw)),
        method => {
            return Err(ZipError::Unsupported(format!(
                "compression method {method}"
            )));
        }
    };
    Ok(MemberReader {
        inner,
        hasher: crc32fast::Hasher::new(),
        expected_crc: entry.crc,
        expected_size: entry.file_size,
        read: 0,
        done: false,
    })
}
