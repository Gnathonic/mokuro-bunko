//! Check a downloaded archive against its own central directory (0.5.2
//! `verify_archive`): every member a reader can reach — for each distinct name the
//! LAST entry, which is what a reader resolves — is read to its end, so the inflate
//! stream and the CRC-32 are checked. This only answers "do these bytes match their
//! own CRCs?"; what the pages mean is the pipeline's to decide.

use std::io::Read;
use std::path::Path;
use std::time::Instant;

use crate::pipeline::CancelToken;

const READ_CHUNK: usize = 1 << 20;

/// What the zip's own CRCs say about one copy of an archive.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Verified {
    /// Members checked (directories excluded).
    pub members: u32,
    /// Members whose bytes do not match their CRC, or will not inflate.
    pub damaged: Vec<String>,
    /// Members of a method or encryption this reader cannot read: not damage; the
    /// pipeline meets them in its own place, as locally.
    pub skipped: Vec<String>,
    /// Why the zip will not open at all.
    pub structural: Option<String>,
    pub seconds: f64,
}

impl Verified {
    pub fn ok(&self) -> bool {
        self.structural.is_none() && self.damaged.is_empty()
    }

    pub fn describe(&self) -> String {
        if let Some(s) = &self.structural {
            return format!("not a readable zip ({s})");
        }
        if !self.damaged.is_empty() {
            return describe_damaged(&self.damaged);
        }
        format!("{} members verified", self.members)
    }
}

/// Which members fail their CRC-32 check, by name (the first five).
pub fn describe_damaged(names: &[String]) -> String {
    let shown: Vec<String> = names.iter().take(5).map(|n| format!("'{n}'")).collect();
    let shown = shown.join(", ");
    if names.len() == 1 {
        return format!("{shown} fails its CRC-32 check");
    }
    let more = if names.len() > 5 {
        format!(" and {} more", names.len() - 5)
    } else {
        String::new()
    };
    format!("{shown}{more} fail their CRC-32 checks")
}

/// The check was abandoned because `cancel` was set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyCancelled;

/// Verify the archive at `path`; `cancel` is looked at between members and after
/// every MiB read.
pub fn verify_archive(path: &Path, cancel: &CancelToken) -> Result<Verified, VerifyCancelled> {
    let started = Instant::now();
    let mut result = Verified::default();
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            result.structural = Some(format!("OSError: {e}"));
            result.seconds = started.elapsed().as_secs_f64();
            return Ok(result);
        }
    };
    let mut zip = match zip::ZipArchive::new(std::io::BufReader::new(file)) {
        Ok(z) => z,
        Err(e) => {
            result.structural = Some(format!("BadZipFile: {e}"));
            result.seconds = started.elapsed().as_secs_f64();
            return Ok(result);
        }
    };
    let mut buffer = vec![0u8; READ_CHUNK];
    // The archive's name index keeps one entry per distinct name: the last one.
    for index in 0..zip.len() {
        if cancel.is_cancelled() {
            return Err(VerifyCancelled);
        }
        let name = match zip.name_for_index(index) {
            Some(Ok(name)) => name.into_owned(),
            // A name the zip crate cannot decode is a broken directory entry: the
            // member counts as damaged (under its raw bytes), never as a directory or
            // as a member with an empty name.
            Some(Err(_)) => {
                let raw = zip
                    .by_index_data(index)
                    .map(|e| String::from_utf8_lossy(e.name_raw()).into_owned())
                    .unwrap_or_default();
                result.members += 1;
                result.damaged.push(raw);
                continue;
            }
            None => continue,
        };
        let mut member = match zip.by_index(index) {
            Ok(m) => m,
            Err(zip::result::ZipError::UnsupportedArchive(_)) => {
                result.skipped.push(name);
                continue;
            }
            Err(_) => {
                if !name.ends_with('/') {
                    result.members += 1;
                    result.damaged.push(name);
                }
                continue;
            }
        };
        if member.is_dir() {
            continue;
        }
        result.members += 1;
        loop {
            match member.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {
                    if cancel.is_cancelled() {
                        return Err(VerifyCancelled);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                    result.members -= 1;
                    result.skipped.push(name.clone());
                    break;
                }
                Err(_) => {
                    result.damaged.push(name.clone());
                    break;
                }
            }
        }
    }
    result.seconds = started.elapsed().as_secs_f64();
    Ok(result)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// A stored (uncompressed) zip of `pages`, so bytes can be damaged at known places.
    pub(crate) fn stored_zip(pages: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut out);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, data) in pages {
                w.start_file(*name, opts).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        out.into_inner()
    }

    #[test]
    fn clean_damaged_and_structural() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelToken::new();
        let bytes = stored_zip(&[
            ("001.jpg", b"page one bytes"),
            ("002.jpg", b"page two bytes"),
        ]);
        let good = dir.path().join("good.cbz");
        std::fs::write(&good, &bytes).unwrap();
        let v = verify_archive(&good, &cancel).unwrap();
        assert!(v.ok(), "{v:?}");
        assert_eq!(v.members, 2);

        let mut bad = bytes.clone();
        let at = bad.windows(8).position(|w| w == b"page two").unwrap();
        bad[at] ^= 0xff;
        let damaged = dir.path().join("bad.cbz");
        std::fs::write(&damaged, &bad).unwrap();
        let v = verify_archive(&damaged, &cancel).unwrap();
        assert!(!v.ok());
        assert_eq!(v.damaged, vec!["002.jpg".to_string()]);
        assert_eq!(v.describe(), "'002.jpg' fails its CRC-32 check");

        let junk = dir.path().join("junk.cbz");
        std::fs::write(&junk, b"not a zip at all").unwrap();
        let v = verify_archive(&junk, &cancel).unwrap();
        assert!(v.structural.is_some());
        assert!(v.describe().starts_with("not a readable zip"));

        cancel.cancel();
        assert_eq!(verify_archive(&good, &cancel), Err(VerifyCancelled));
    }

    #[test]
    fn damaged_names_are_summarised() {
        let names: Vec<String> = (0..7).map(|i| format!("{i}.jpg")).collect();
        assert_eq!(
            describe_damaged(&names),
            "'0.jpg', '1.jpg', '2.jpg', '3.jpg', '4.jpg' and 2 more fail their CRC-32 checks"
        );
    }

    /// `data` with member `from` renamed to the raw bytes `to` (same length) in its local
    /// and central headers, and the UTF-8 flag (bit 11) set to `utf8`: the names real
    /// archives carry that the zip writer cannot write.
    fn renamed(mut data: Vec<u8>, from: &str, to: &[u8], utf8: bool) -> Vec<u8> {
        assert_eq!(from.len(), to.len());
        let (mut i, mut hits) = (0, 0);
        while i + 46 <= data.len() {
            let (flags, len, name) = match &data[i..i + 4] {
                b"PK\x03\x04" => (i + 6, i + 26, i + 30),
                b"PK\x01\x02" => (i + 8, i + 28, i + 46),
                _ => {
                    i += 1;
                    continue;
                }
            };
            let n = u16::from_le_bytes([data[len], data[len + 1]]) as usize;
            if &data[name..name + n] == from.as_bytes() {
                data[name..name + n].copy_from_slice(to);
                let f = u16::from_le_bytes([data[flags], data[flags + 1]]);
                let f = if utf8 { f | 0x800 } else { f & !0x800 };
                data[flags..flags + 2].copy_from_slice(&f.to_le_bytes());
                hits += 1;
            }
            i += 4;
        }
        assert_eq!(hits, 2, "{from}: one local and one central header");
        data
    }

    /// Member names in every encoding real archives use, none of them a valid flagged
    /// UTF-8 name: Shift-JIS and CP437 without the flag, UTF-8 without the flag (macOS
    /// Archive Utility), and bytes that are not UTF-8 under the flag.
    fn odd_names() -> [(&'static str, &'static [u8], bool); 4] {
        [
            ("sj.jpg", b"\x83\x41.jpg", false),
            ("cp.jpg", b"\x80\x81.jpg", false),
            ("mac.jpg", "\u{30a4}.jpg".as_bytes(), false),
            ("bd.jpg", b"\xff\xfe.jpg", true),
        ]
    }

    /// zip 9's `name()` is fallible; an undecodable name counts as a damaged member.
    /// Every real name encoding still decodes: such an archive verifies clean.
    #[test]
    fn member_names_in_any_encoding_verify() {
        let pages: Vec<(&str, &[u8])> = odd_names()
            .iter()
            .map(|(placeholder, _, _)| (*placeholder, b"page bytes".as_slice()))
            .collect();
        let mut bytes = stored_zip(&pages);
        for (placeholder, raw, utf8) in odd_names() {
            bytes = renamed(bytes, placeholder, raw, utf8);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.cbz");
        std::fs::write(&path, bytes).unwrap();
        let v = verify_archive(&path, &CancelToken::new()).unwrap();
        assert!(v.structural.is_none(), "{:?}", v.structural);
        assert!(v.damaged.is_empty(), "{:?}", v.damaged);
        assert!(v.skipped.is_empty(), "{:?}", v.skipped);
        assert_eq!(v.members, 4);
    }
}
