//! Library housekeeping that 0.5.2's OCR worker did beside OCR (spec ocr-ppocr-layout
//! §9, ocr-scheduling §5.1; `watcher.py` `_run_thumbnail_loop`,
//! `_remove_corrupt_sidecars`; `processor.py` `ensure_thumbnail`):
//!
//! * **Cover thumbnails.** Every poll interval (`ocr.poll_interval`, shared with the
//!   OCR scan) every `*.cbz` under the library that has neither `<stem>.webp` nor
//!   `<stem>.nocover` gets a cover: the first name of the archive's plain sorted
//!   namelist with an image suffix, `ImageOps.contain(250×350, LANCZOS)`, lossy WebP
//!   q85 m6 (`bunko-thumb`). No image, an unreadable archive or a missing member writes
//!   the `<stem>.nocover` marker. One cover at a time, on a blocking thread; the decode
//!   is capped ([`MAX_DECODE_BYTES`]) so a hostile "cover" cannot take a small host's
//!   memory. The file is written to `<stem>.webp.tmp` and renamed (0.5.2 wrote it in
//!   place).
//!   A cover that does not decode is logged and, as in 0.5.2, gets no marker; unlike
//!   0.5.2 it is not retried every poll: it is remembered (by size and mtime) until
//!   the archive changes or the server restarts.
//! * **Corrupt sidecars.** Once at start, every `library/**/*.mokuro` /
//!   `*.mokuro.gz` that is not valid JSON (gzip-aware) is deleted and its provenance
//!   row forgotten. Validity is Python's `json.load`: a file serde rejects is checked
//!   again with `bunko-layout`'s Python-compatible parser (`NaN`/`Infinity`), and a
//!   lone-surrogate escape (which Python accepts) never gets a file deleted.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use bunko_core::Config;
use bunko_db::Database;
use parking_lot::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Suffixes a cover may have (`_extract_cover_image_data`): no `.avif`.
pub const COVER_EXTENSIONS: [&str; 8] = [
    ".jpg", ".jpeg", ".png", ".gif", ".bmp", ".webp", ".tiff", ".tif",
];
/// Decode budget of one cover (pixel buffers).
pub const MAX_DECODE_BYTES: u64 = 192 * 1024 * 1024;
/// Largest cover member read into memory.
pub const MAX_MEMBER_BYTES: u64 = 64 * 1024 * 1024;
/// Longest cover side accepted.
pub const MAX_SIDE: u32 = 20_000;

/// `<stem>.webp`.
pub fn cover_path(cbz: &Path) -> PathBuf {
    cbz.with_extension("webp")
}

/// `<stem>.nocover`: a cover was looked for and there is none.
pub fn nocover_path(cbz: &Path) -> PathBuf {
    cbz.with_extension("nocover")
}

/// `needs_thumbnail`.
pub fn needs_thumbnail(cbz: &Path) -> bool {
    cbz.is_file()
        && cbz
            .extension()
            .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("cbz"))
        && !cover_path(cbz).exists()
        && !nocover_path(cbz).exists()
}

fn suffix_lower(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rfind('.') {
        Some(i) if i > 0 && i + 1 < base.len() => base[i..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// What looking for a cover found.
#[derive(Debug)]
pub enum Cover {
    /// The cover member's bytes.
    Image(Vec<u8>),
    /// No image in the archive, or it cannot be read as a zip: `.nocover`.
    None(String),
}

/// `_extract_cover_image_data`: the first name in code-point order whose suffix is
/// an image suffix (directories, `__MACOSX` and the embedded `<stem>.webp` included,
/// as in 0.5.2).
pub fn cover_image(cbz: &Path) -> Cover {
    let file = match std::fs::File::open(cbz) {
        Ok(f) => f,
        Err(e) => return Cover::None(e.to_string()),
    };
    let mut zip = match zip::ZipArchive::new(std::io::BufReader::new(file)) {
        Ok(z) => z,
        Err(e) => return Cover::None(e.to_string()),
    };
    let mut names: Vec<String> = zip
        .file_names()
        .filter(|n| COVER_EXTENSIONS.contains(&suffix_lower(n).as_str()))
        .map(str::to_string)
        .collect();
    names.sort();
    let Some(first) = names.into_iter().next() else {
        return Cover::None("no image".into());
    };
    let mut member = match zip.by_name(&first) {
        Ok(m) => m,
        Err(e) => return Cover::None(e.to_string()),
    };
    if member.size() > MAX_MEMBER_BYTES {
        return Cover::Image(Vec::new());
    }
    let mut bytes = Vec::with_capacity(member.size() as usize);
    match (&mut member)
        .take(MAX_MEMBER_BYTES + 1)
        .read_to_end(&mut bytes)
    {
        Ok(_) => Cover::Image(bytes),
        Err(e) => Cover::None(e.to_string()),
    }
}

/// `ensure_thumbnail` for one archive: true when a cover was written (or was not
/// needed).
pub fn ensure_thumbnail(cbz: &Path) -> Result<bool, String> {
    if !needs_thumbnail(cbz) {
        return Ok(true);
    }
    let name = cbz
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let bytes = match cover_image(cbz) {
        Cover::Image(b) => b,
        Cover::None(why) => {
            info!("No cover image found in: {name} ({why})");
            std::fs::File::create(nocover_path(cbz))
                .map_err(|e| format!("could not write the .nocover marker: {e}"))?;
            return Ok(false);
        }
    };
    let webp = bunko_thumb::make_thumbnail_limited(&bytes, MAX_DECODE_BYTES, MAX_SIDE)
        .map_err(|e| format!("Failed to generate thumbnail for {name}: {e}"))?;
    drop(bytes);
    let out = cover_path(cbz);
    let mut tmp = out.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, &webp)
        .and_then(|()| std::fs::rename(&tmp, &out))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("Failed to generate thumbnail for {name}: {e}")
        })?;
    info!(
        "Created thumbnail: {}",
        out.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    );
    Ok(true)
}

fn walk(dir: &Path, keep: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => walk(&path, keep),
            Ok(t) if t.is_file() || t.is_symlink() => keep(&path),
            _ => {}
        }
    }
}

/// Every library `.cbz` still waiting for a cover (`_thumbnail_candidates`).
pub fn thumbnail_candidates(library: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(library, &mut |p| {
        if p.extension().is_some_and(|e| e == "cbz") && needs_thumbnail(p) {
            out.push(p.to_path_buf());
        }
    });
    out.sort();
    out
}

type Stamp = (u64, i128);

fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i128);
    Some((m.len(), mtime))
}

/// The cover loop's state, shared with the queue page's `pending_thumbnails`.
#[derive(Default)]
pub struct Thumbnails {
    pending: AtomicI64,
    /// Covers that would not decode, by archive stamp (not retried until it changes).
    failed: Mutex<HashMap<PathBuf, Stamp>>,
}

impl Thumbnails {
    /// Volumes still waiting for a cover, as of the last scan.
    pub fn pending(&self) -> i64 {
        self.pending.load(Ordering::Relaxed)
    }

    /// One pass over the library (`_scan_thumbnails_once`); returns covers written.
    pub fn scan_once(&self, library: &Path, stop: &CancellationToken) -> usize {
        let candidates: Vec<PathBuf> = {
            let failed = self.failed.lock();
            thumbnail_candidates(library)
                .into_iter()
                .filter(|p| failed.get(p).is_none_or(|s| stamp(p).as_ref() != Some(s)))
                .collect()
        };
        self.pending
            .store(candidates.len() as i64, Ordering::Relaxed);
        if candidates.is_empty() {
            return 0;
        }
        info!("Found {} CBZ files missing thumbnails", candidates.len());
        let mut made = 0;
        for cbz in candidates {
            if stop.is_cancelled() {
                break;
            }
            match ensure_thumbnail(&cbz) {
                Ok(true) => made += 1,
                Ok(false) => {}
                Err(e) => {
                    warn!("{e}");
                    if let Some(s) = stamp(&cbz) {
                        self.failed.lock().insert(cbz.clone(), s);
                    }
                }
            }
            self.pending.fetch_sub(1, Ordering::Relaxed);
        }
        made
    }

    /// Run the loop until `stop`: a scan, then the OCR poll interval.
    pub fn spawn(
        self: &Arc<Self>,
        config: Arc<RwLock<Config>>,
        library: PathBuf,
        stop: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                let (m, lib, s) = (me.clone(), library.clone(), stop.clone());
                let scan = tokio::task::spawn_blocking(move || m.scan_once(&lib, &s));
                if let Err(e) = scan.await {
                    warn!("Thumbnail scan error: {e}");
                }
                let wait = config.read().ocr.poll_interval.max(1);
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(u64::from(wait))) => {}
                }
            }
        })
    }
}

/// Whether a sidecar parses as Python's `json.load` would (gzip-aware).
pub fn is_valid_sidecar(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let gz = path.to_string_lossy().ends_with(".mokuro.gz");
    let reader: Box<dyn Read> = if gz {
        Box::new(flate2::read::GzDecoder::new(std::io::BufReader::new(file)))
    } else {
        Box::new(std::io::BufReader::new(file))
    };
    if serde_json::from_reader::<_, serde::de::IgnoredAny>(reader).is_ok() {
        return true;
    }
    // serde is stricter than Python in two ways: NaN/Infinity and lone surrogates.
    let mut text = Vec::new();
    let read = if gz {
        std::fs::File::open(path)
            .and_then(|f| flate2::read::GzDecoder::new(f).read_to_end(&mut text))
    } else {
        std::fs::File::open(path).and_then(|mut f| f.read_to_end(&mut text))
    };
    if read.is_err() {
        return false;
    }
    let Ok(text) = String::from_utf8(text) else {
        return false;
    };
    match bunko_layout::json::Value::parse(&text) {
        Ok(_) => true,
        Err(e) => e.to_string().contains("surrogate"),
    }
}

/// `_remove_corrupt_sidecars`: delete every invalid library sidecar; returns how many.
pub fn remove_corrupt_sidecars(library: &Path, db: Option<&Database>) -> usize {
    let mut found = Vec::new();
    walk(library, &mut |p| {
        let name = p.to_string_lossy();
        if name.ends_with(".mokuro") || name.ends_with(".mokuro.gz") {
            found.push(p.to_path_buf());
        }
    });
    found.sort();
    let mut removed = 0;
    for path in found {
        if !path.is_file() || is_valid_sidecar(&path) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                removed += 1;
                info!("Removed corrupt mokuro sidecar: {}", path.display());
                if let (Some(db), Ok(rel)) = (db, path.strip_prefix(library)) {
                    let rel = rel.to_string_lossy().replace('\\', "/");
                    if let Err(e) = db.forget_ocr_sidecar(&rel) {
                        warn!("could not forget the provenance of {rel}: {e}");
                    }
                }
            }
            Err(e) => warn!("Failed to remove corrupt sidecar {}: {e}", path.display()),
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn cbz(path: &Path, members: &[(&str, &[u8])]) {
        let f = std::fs::File::create(path).unwrap();
        let mut z = zip::ZipWriter::new(f);
        for (name, data) in members {
            z.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            z.write_all(data).unwrap();
        }
        z.finish().unwrap();
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::RgbImage::from_fn(w, h, |x, y| image::Rgb([x as u8, y as u8, 7]))
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn covers_markers_and_retries() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        let series = lib.join("Series");
        std::fs::create_dir_all(&series).unwrap();
        // Plain code-point order: "B/1.png" < "a.png"; the text file is skipped.
        cbz(
            &series.join("Vol 1.cbz"),
            &[
                ("a.png", &png(10, 10)),
                ("B/1.png", &png(500, 700)),
                ("0.txt", b"x"),
            ],
        );
        cbz(&series.join("Vol 2.cbz"), &[("notes.txt", b"x")]);
        std::fs::write(series.join("Vol 3.cbz"), b"not a zip").unwrap();
        cbz(&series.join("Vol 4.cbz"), &[("p.jpg", b"garbage")]);
        cbz(&series.join("Done.cbz"), &[("p.png", &png(10, 10))]);
        std::fs::write(series.join("Done.webp"), b"x").unwrap();

        let thumbs = Thumbnails::default();
        assert_eq!(thumbnail_candidates(&lib).len(), 4);
        let made = thumbs.scan_once(&lib, &CancellationToken::new());
        assert_eq!(made, 1);
        let webp = std::fs::read(series.join("Vol 1.webp")).unwrap();
        let img = image::load_from_memory(&webp).unwrap();
        assert_eq!((img.width(), img.height()), (250, 350), "the 500x700 page");
        assert!(series.join("Vol 2.nocover").is_file());
        assert!(series.join("Vol 3.nocover").is_file());
        // An undecodable cover: no marker, and not retried while unchanged.
        assert!(!series.join("Vol 4.nocover").exists());
        assert!(!series.join("Vol 4.webp").exists());
        assert_eq!(thumbs.pending(), 0);
        assert_eq!(thumbs.scan_once(&lib, &CancellationToken::new()), 0);
        assert_eq!(thumbnail_candidates(&lib).len(), 1);
        // Replaced by a good archive: tried again.
        std::thread::sleep(Duration::from_millis(20));
        cbz(&series.join("Vol 4.cbz"), &[("p.png", &png(300, 200))]);
        assert_eq!(thumbs.scan_once(&lib, &CancellationToken::new()), 1);
        assert!(!series.join("Vol 4.webp.tmp").exists());
    }

    #[test]
    fn corrupt_sidecars_are_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        std::fs::create_dir_all(lib.join("S")).unwrap();
        std::fs::write(lib.join("S/ok.mokuro"), br#"{"pages": []}"#).unwrap();
        std::fs::write(lib.join("S/nan.mokuro"), br#"{"x": NaN}"#).unwrap();
        std::fs::write(lib.join("S/sur.mokuro"), br#"{"x": "\ud800"}"#).unwrap();
        std::fs::write(lib.join("S/bad.mokuro"), br#"{"pages": ["#).unwrap();
        std::fs::write(lib.join("S/bad.layer.mokuro"), b"\xff\xfe").unwrap();
        std::fs::write(lib.join("S/other.mokuro.tmp"), b"{").unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(br#"{"a": 1}"#).unwrap();
        std::fs::write(lib.join("S/ok.mokuro.gz"), gz.finish().unwrap()).unwrap();
        std::fs::write(lib.join("S/bad.mokuro.gz"), b"not gzip").unwrap();
        assert_eq!(remove_corrupt_sidecars(&lib, None), 3);
        let mut left: Vec<String> = std::fs::read_dir(lib.join("S"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "nan.mokuro",
                "ok.mokuro",
                "ok.mokuro.gz",
                "other.mokuro.tmp",
                "sur.mokuro"
            ]
        );
    }
}
