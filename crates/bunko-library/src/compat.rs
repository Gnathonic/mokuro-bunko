//! Ports of the reader client's pure helpers (`metadata/reader_compat.py`),
//! kept byte-identical with 0.5.2: the compiled files must be
//! indistinguishable from what the client itself writes.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::isodate;
use crate::pyjson::JsonValue;
use crate::pyunicode;

/// The client's `countChars` class `[○◯々-〇〻ぁ-ゖゝ-ゞァ-ヺー\p{Script=Hiragana}
/// \p{Script=Katakana}\p{Script=Han}]`, expanded to code point ranges (copied
/// verbatim from 0.5.2's `_COUNTED_RANGES`).
#[rustfmt::skip]
const COUNTED_RANGES: &[(u32, u32)] = &[
    (0x25CB, 0x25CB), (0x25EF, 0x25EF), (0x2E80, 0x2E99), (0x2E9B, 0x2EF3),
    (0x2F00, 0x2FD5), (0x3005, 0x3007), (0x3021, 0x3029), (0x3038, 0x303B),
    (0x3041, 0x3096), (0x309D, 0x309F), (0x30A1, 0x30FA), (0x30FC, 0x30FF),
    (0x31F0, 0x31FF), (0x32D0, 0x32FE), (0x3300, 0x3357), (0x3400, 0x4DBF),
    (0x4E00, 0x9FFF), (0xF900, 0xFA6D), (0xFA70, 0xFAD9), (0xFF66, 0xFF6F),
    (0xFF71, 0xFF9D), (0x16FE2, 0x16FE3), (0x16FF0, 0x16FF6), (0x1AFF0, 0x1AFF3),
    (0x1AFF5, 0x1AFFB), (0x1AFFD, 0x1AFFE), (0x1B000, 0x1B122), (0x1B132, 0x1B132),
    (0x1B150, 0x1B152), (0x1B155, 0x1B155), (0x1B164, 0x1B167), (0x1F200, 0x1F200),
    (0x20000, 0x2A6DF), (0x2A700, 0x2B81D), (0x2B820, 0x2CEAD), (0x2CEB0, 0x2EBE0),
    (0x2EBF0, 0x2EE5D), (0x2F800, 0x2FA1D), (0x30000, 0x3134A), (0x31350, 0x33479),
];

/// Merge stamps more than this far in the future are clamped to "now".
pub const FUTURE_TOLERANCE_SECONDS: f64 = 5.0 * 60.0;

fn is_counted(c: char) -> bool {
    let cp = c as u32;
    let index = COUNTED_RANGES.partition_point(|&(start, _)| start <= cp);
    index > 0 && cp <= COUNTED_RANGES[index - 1].1
}

/// Japanese characters in one OCR line (client: `countChars`).
pub fn count_chars(text: &str) -> u64 {
    text.chars().filter(|&c| is_counted(c)).count() as u64
}

/// Total over every line of every block (client: `getCharCount`); malformed
/// pieces are skipped, never fatal.
pub fn count_page_chars(pages: Option<&JsonValue>) -> u64 {
    let Some(JsonValue::Array(pages)) = pages else {
        return 0;
    };
    let mut total = 0;
    for page in pages {
        let Some(blocks) = page
            .as_object()
            .and_then(|p| p.get("blocks"))
            .and_then(JsonValue::as_array)
        else {
            continue;
        };
        for block in blocks {
            let Some(lines) = block
                .as_object()
                .and_then(|b| b.get("lines"))
                .and_then(JsonValue::as_array)
            else {
                continue;
            };
            for line in lines {
                if let JsonValue::Str(text) = line {
                    total += count_chars(text);
                }
            }
        }
    }
    total
}

/// Port of the client's `generateDeterministicUUID` (djb2-xor pair over UTF-16
/// code units). The 8-4-4-4-8 shape and the `hex2[5..8]` skip are the client's.
pub fn deterministic_uuid(value: &str) -> String {
    let lowered = pyunicode::lower(value);
    let normalized = pyunicode::strip(&lowered);
    let mut hash1: i32 = 5381;
    let mut hash2: i32 = 52711;
    for unit in normalized.encode_utf16() {
        hash1 = hash1.wrapping_mul(33) ^ i32::from(unit);
        hash2 = hash2.wrapping_mul(33) ^ i32::from(unit);
    }
    let h1 = hash1 as u32;
    let h2 = hash2 as u32;
    let hex1 = format!("{h1:08x}");
    let hex2 = format!("{h2:08x}");
    let hash3 = format!("{:08x}", h1 ^ h2);
    let hash4 = format!("{:08x}", h1.wrapping_add(h2));
    let first = u32::from_str_radix(&hash3[..1], 16).unwrap_or(0);
    let variant = format!("{:x}", 8 + first % 4);
    format!(
        "{hex1}-{}-4{}-{variant}{}-{}{}",
        &hex2[..4],
        &hex2[5..8],
        &hash3[1..4],
        &hash3[4..],
        &hash4[..4]
    )
}

/// Client `normalizeSeriesKey`: trim, collapse whitespace, lowercase (with
/// Python's whitespace and lowercase tables).
pub fn normalize_series_key(title: &str) -> String {
    pyunicode::lower(&pyunicode::collapse_whitespace(pyunicode::strip(title)))
}

/// Client `normalizeVolumeTitleKey`: NFC, then the series fold. The identity
/// of `series_facts` rows (and the `series_key` column of `catalog_folders`).
pub fn normalize_volume_title_key(title: &str) -> String {
    normalize_series_key(&pyunicode::nfc(title))
}

/// One component of a [`NaturalKey`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NaturalPart {
    /// A digit run, by value (canonical ASCII digits, arbitrary length).
    Number(String),
    /// Text, NFKD-decomposed, combining marks dropped, casefolded.
    Text(String),
}

impl Ord for NaturalPart {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (NaturalPart::Number(a), NaturalPart::Number(b)) => {
                pyunicode::cmp_decimal_strings(a, b)
            }
            (NaturalPart::Number(_), NaturalPart::Text(_)) => Ordering::Less,
            (NaturalPart::Text(_), NaturalPart::Number(_)) => Ordering::Greater,
            (NaturalPart::Text(a), NaturalPart::Text(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for NaturalPart {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// `natural_sort_key(title)`: a total preorder (distinct titles can tie);
/// callers tiebreak on the raw title.
pub type NaturalKey = Vec<NaturalPart>;

/// Split `text` into runs of decimal / non-decimal characters
/// (`re.split(r"(\d+)", text)` minus the empty strings).
fn decimal_runs(text: &str) -> Vec<(bool, &str)> {
    let mut runs = Vec::new();
    let mut start = 0;
    let mut current: Option<bool> = None;
    for (index, c) in text.char_indices() {
        let decimal = pyunicode::is_decimal(c);
        match current {
            Some(kind) if kind == decimal => {}
            Some(kind) => {
                runs.push((kind, &text[start..index]));
                start = index;
                current = Some(decimal);
            }
            None => current = Some(decimal),
        }
    }
    if let Some(kind) = current {
        runs.push((kind, &text[start..]));
    }
    runs
}

/// Order volume titles the way 0.5.2 did (`reader_compat.natural_sort_key`).
pub fn natural_sort_key(title: &str) -> NaturalKey {
    decimal_runs(title)
        .into_iter()
        .map(|(decimal, part)| {
            if decimal {
                NaturalPart::Number(pyunicode::decimal_run_value(part))
            } else {
                let stripped: String = pyunicode::nfkd(part)
                    .chars()
                    .filter(|&c| !pyunicode::is_combining(c))
                    .collect();
                NaturalPart::Text(pyunicode::casefold(&stripped))
            }
        })
        .collect()
}

/// Compare two titles by `(natural_sort_key(title), title)`.
pub fn natural_title_cmp(a: &str, b: &str) -> Ordering {
    natural_sort_key(a)
        .cmp(&natural_sort_key(b))
        .then_with(|| a.cmp(b))
}

/// Client `normalizeUpdatedAt`: a comparable ISO stamp, or `None` (reject).
/// `now` is `time.time()`. Stamps more than five minutes ahead clamp to `now`.
pub fn normalize_updated_at(value: Option<&JsonValue>, now: f64) -> Option<String> {
    let JsonValue::Str(raw) = value? else {
        return None;
    };
    let text = pyunicode::strip(raw);
    if text.is_empty() {
        return None;
    }
    let rewritten;
    let text = match text.strip_suffix('Z') {
        Some(head) => {
            rewritten = format!("{head}+00:00");
            rewritten.as_str()
        }
        None => text,
    };
    let micros = isodate::fromisoformat_utc_micros(text)?;
    let mut seconds = isodate::micros_to_seconds(micros);
    if seconds > now + FUTURE_TOLERANCE_SECONDS {
        seconds = now;
    }
    // CPython would raise (not reject) for a UTC instant outside years
    // 1..=9999; rejecting is the only sane mapping of that crash.
    isodate::iso_stamp(seconds)
}

/// `iso_stamp(seconds)` — see [`isodate::iso_stamp`].
pub fn iso_stamp(seconds: f64) -> Option<String> {
    isodate::iso_stamp(seconds)
}

// --- page/image matching ----------------------------------------------------

/// Client `IMAGE_EXTENSIONS` (extensions, without the dot).
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "bmp", "avif", "tif", "tiff", "jxl",
];

const EXCLUDED_SYSTEM_PATTERNS: &[&str] = &[
    "__MACOSX",
    ".DS_Store",
    ".Trashes",
    ".Spotlight-V100",
    ".fseventsd",
    ".TemporaryItems",
    ".Trash",
    "System Volume Information",
    "$RECYCLE.BIN",
    "Thumbs.db",
    "desktop.ini",
    "Desktop.ini",
    "RECYCLER",
    "RECYCLED",
    ".Trash-1000",
    ".thumbnails",
    ".directory",
    ".dropbox",
    ".dropbox.cache",
    ".git",
    ".svn",
];

const EXCLUDED_EXTENSIONS: &[&str] = &["bak", "tmp", "temp"];

/// OS junk the client refuses to import (client: `isSystemFile`).
pub fn is_system_file(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let segments: Vec<&str> = normalized.split('/').collect();
    for segment in &segments {
        if segment.is_empty() {
            continue;
        }
        if segment.starts_with("._")
            || segment.ends_with('~')
            || EXCLUDED_SYSTEM_PATTERNS.contains(segment)
        {
            return true;
        }
    }
    let filename = segments.last().copied().unwrap_or("");
    if let Some(dot) = filename.rfind('.') {
        let extension = pyunicode::lower(&filename[dot + 1..]);
        if EXCLUDED_EXTENSIONS.contains(&extension.as_str()) {
            return true;
        }
    }
    false
}

/// Client `isImageExtension`.
pub fn is_image_extension(extension: &str) -> bool {
    IMAGE_EXTENSIONS.contains(&pyunicode::lower(extension).as_str())
}

/// The client's `path.split('.').pop()` of the WHOLE path, lowercased (quirk
/// preserved: `chapter.1/page001` yields `1/page001`).
pub fn trailing_extension(path: &str) -> String {
    pyunicode::lower(path.rsplit('.').next().unwrap_or(path))
}

fn normalize_page_path(path: &str) -> String {
    pyunicode::lower(path).replace('\\', "/")
}

/// Client `getStem`: basename minus its last extension (`lastDot > 0`).
fn page_stem(path: &str) -> &str {
    let last = path.rsplit('/').next().unwrap_or(path);
    let filename = if last.is_empty() { path } else { last };
    match filename.rfind('.') {
        Some(dot) if dot > 0 => &filename[..dot],
        _ => filename,
    }
}

/// How many of a `.mokuro`'s pages have an image in the archive: the counting
/// half of the client's `matchImagesToPages` (exact normalized path, then stem,
/// then the whole-volume positional fallback). `None` entries are pages
/// without a usable `img_path`.
pub fn count_matched_pages(page_paths: &[Option<String>], file_paths: &[String]) -> u64 {
    let mut seen = HashSet::new();
    let unique_files: Vec<&str> = file_paths
        .iter()
        .map(String::as_str)
        .filter(|path| seen.insert(*path))
        .collect();

    let mut normalized_files: HashMap<String, &str> = HashMap::new();
    let mut stem_files: HashMap<String, &str> = HashMap::new();
    for &file in &unique_files {
        normalized_files.insert(normalize_page_path(file), file);
        stem_files.insert(pyunicode::lower(page_stem(file)), file);
    }

    let mut used: HashSet<&str> = HashSet::new();
    let mut matched: u64 = 0;
    let mut missing: u64 = 0;
    for page in page_paths {
        let Some(page) = page else {
            missing += 1;
            continue;
        };
        if let Some(&actual) = normalized_files.get(&normalize_page_path(page)) {
            matched += 1;
            used.insert(actual);
            continue;
        }
        if let Some(&actual) = stem_files.get(&pyunicode::lower(page_stem(page)))
            && !used.contains(actual)
        {
            matched += 1;
            used.insert(actual);
            continue;
        }
        missing += 1;
    }
    let extra = unique_files
        .iter()
        .filter(|file| !used.contains(**file))
        .count() as u64;
    // `matched / len(page_paths) < 0.5`, in exact integer arithmetic.
    if missing > 0 && missing == extra && matched * 2 < page_paths.len() as u64 {
        matched += missing;
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_shape() {
        let id = deterministic_uuid("Bakemonogatari/v01");
        assert_eq!(id.len(), 32);
        assert_eq!(&id[14..15], "4");
    }

    #[test]
    fn natural_order() {
        let mut titles = vec!["Vol 10", "vol 2", "Vol 1.5", "Vol 1", "Vol 01"];
        titles.sort_by(|a, b| natural_title_cmp(a, b));
        assert_eq!(
            titles,
            vec!["Vol 01", "Vol 1", "Vol 1.5", "vol 2", "Vol 10"]
        );
    }

    #[test]
    fn matched_pages() {
        let pages = vec![
            Some("a/p1.png".to_owned()),
            Some("a/P2.JPG".to_owned()),
            None,
        ];
        let files = vec!["a/p1.png".to_owned(), "a/p2.webp".to_owned()];
        assert_eq!(count_matched_pages(&pages, &files), 2);
    }
}
