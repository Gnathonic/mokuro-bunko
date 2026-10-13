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
//!
//! **A cloud library** (0.8 §8, D13, D14): covers are made as archives arrive (the
//! store's change events, plus one walk of its index at start and after a `Rescan`),
//! never by a periodic walk; there is no corrupt-sidecar sweep (report-only on
//! remote); and `.nocover` is not a file in the cloud but a local-only record,
//! [`NoCoverSet`], kept in `<base>/cloud/<store-id>/nocover.json` until the store's
//! durable `local_row` table offers `nocover` rows (it keeps only staging rows today).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use bunko_core::Config;
use bunko_db::Database;
use bunko_store::{Durability, Mode, Origin, WriteIntent, fs};
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
    needs_thumbnail_with(cbz, None)
}

/// [`needs_thumbnail`], with a cloud library's local `.nocover` records.
pub fn needs_thumbnail_with(cbz: &Path, nocover: Option<&NoCoverSet>) -> bool {
    fs::is_file(cbz)
        && cbz
            .extension()
            .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("cbz"))
        && !fs::exists(cover_path(cbz))
        && !fs::exists(nocover_path(cbz))
        && nocover.is_none_or(|n| !n.contains(cbz))
}

/// D14: the `.nocover` markers of a cloud library, as local-only records keyed by the
/// archive's library path and version `(size, mtime_ns)` (a new version of the archive
/// is looked at again). Persisted as JSON in the store's own directory; a stand-in for
/// the store's `local_row` table, which holds only staging rows so far.
pub struct NoCoverSet {
    file: PathBuf,
    library: PathBuf,
    set: Mutex<HashSet<(String, u64, i128)>>,
}

impl NoCoverSet {
    /// The set kept in `file` for the library at `library` (empty when unreadable).
    // unmounted: `file` is in the store's local directory, not in the library.
    #[allow(clippy::disallowed_methods)]
    pub fn load(file: PathBuf, library: PathBuf) -> NoCoverSet {
        let set = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<(String, u64, String)>>(&b).ok())
            .map(|rows| {
                rows.into_iter()
                    .filter_map(|(p, s, m)| Some((p, s, m.parse().ok()?)))
                    .collect()
            })
            .unwrap_or_default();
        NoCoverSet {
            file,
            library,
            set: Mutex::new(set),
        }
    }

    fn key(&self, cbz: &Path) -> Option<(String, u64, i128)> {
        let rel = crate::ocr::types::rel_of(&self.library, cbz)?;
        let (size, mtime) = stamp(cbz)?;
        Some((rel, size, mtime))
    }

    pub fn contains(&self, cbz: &Path) -> bool {
        self.key(cbz).is_some_and(|k| self.set.lock().contains(&k))
    }

    pub fn len(&self) -> usize {
        self.set.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Record that `cbz` (this version of it) has no cover.
    pub fn insert(&self, cbz: &Path) -> std::io::Result<()> {
        let Some(k) = self.key(cbz) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the archive is not in the library",
            ));
        };
        let rows = {
            let mut set = self.set.lock();
            // A path keeps one record: its newest version.
            set.retain(|(p, _, _)| *p != k.0);
            set.insert(k);
            Self::rows(&set)
        };
        self.save(&rows)
    }

    /// Drop the record of `cbz` (whatever its version): a case merge trashed or moved
    /// the archive (spec §5.9).
    pub fn remove(&self, cbz: &Path) -> std::io::Result<()> {
        let Some(rel) = crate::ocr::types::rel_of(&self.library, cbz) else {
            return Ok(());
        };
        let rows = {
            let mut set = self.set.lock();
            let before = set.len();
            set.retain(|(p, _, _)| *p != rel);
            if set.len() == before {
                return Ok(());
            }
            Self::rows(&set)
        };
        self.save(&rows)
    }

    fn rows(set: &HashSet<(String, u64, i128)>) -> Vec<(String, u64, String)> {
        let mut rows: Vec<_> = set
            .iter()
            .map(|(p, s, m)| (p.clone(), *s, m.to_string()))
            .collect();
        rows.sort();
        rows
    }

    // unmounted: `file` is in the store's local directory, not in the library.
    #[allow(clippy::disallowed_methods)]
    fn save(&self, rows: &[(String, u64, String)]) -> std::io::Result<()> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.file.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec(rows).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(&tmp, &self.file)
    }
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
///
/// On a cloud library the archive is read with `bunko_library`'s central-directory
/// reader: the directory at the tail plus the one member, a few ranged reads. The
/// `zip` crate reads every member's local header while opening an archive, one ranged
/// read per page across the whole file.
pub fn cover_image(cbz: &Path) -> Cover {
    if bunko_store::store_for(cbz).is_some_and(|s| s.caps().remote) {
        return cover_image_zipdir(cbz);
    }
    let file = match fs::File::open(cbz) {
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

/// [`cover_image`] through the central directory only: the same member (the first
/// sorted image name; of duplicate names the last entry, as the `zip` crate keeps)
/// and the same size cap.
fn cover_image_zipdir(cbz: &Path) -> Cover {
    use bunko_library::archive::zipdir;
    let entries = match zipdir::list_entries(cbz) {
        Ok(e) => e,
        Err(e) => return Cover::None(e.to_string()),
    };
    let Some(first) = entries
        .iter()
        .map(|e| e.filename.as_str())
        .filter(|n| COVER_EXTENSIONS.contains(&suffix_lower(n).as_str()))
        .min()
    else {
        return Cover::None("no image".into());
    };
    let entry = entries
        .iter()
        .rfind(|e| e.filename == first)
        .expect("the name came from these entries");
    if entry.file_size > MAX_MEMBER_BYTES {
        return Cover::Image(Vec::new());
    }
    let member = match zipdir::open_member(cbz, entry) {
        Ok(m) => m,
        Err(e) => return Cover::None(e.to_string()),
    };
    let mut bytes = Vec::with_capacity(entry.file_size as usize);
    match member.take(MAX_MEMBER_BYTES + 1).read_to_end(&mut bytes) {
        Ok(_) => Cover::Image(bytes),
        Err(e) => Cover::None(e.to_string()),
    }
}

/// `ensure_thumbnail` for one archive: true when a cover was written (or was not
/// needed).
pub fn ensure_thumbnail(cbz: &Path) -> Result<bool, String> {
    ensure_thumbnail_with(cbz, None)
}

/// [`ensure_thumbnail`]; on a cloud library a missing cover is recorded in `nocover`
/// instead of a `.nocover` file (D14).
pub fn ensure_thumbnail_with(cbz: &Path, nocover: Option<&NoCoverSet>) -> Result<bool, String> {
    if !needs_thumbnail_with(cbz, nocover) {
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
            match nocover {
                Some(set) => set
                    .insert(cbz)
                    .map_err(|e| format!("could not record the missing cover: {e}"))?,
                None => {
                    fs::File::create(nocover_path(cbz))
                        .map_err(|e| format!("could not write the .nocover marker: {e}"))?;
                }
            }
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
    let intent = WriteIntent::new(Mode::CreateNew, Origin::Thumbs, Durability::Deferred);
    // A staging file (on a remote store: local until published; `fs::write` would
    // publish the temporary name itself), then one publish onto the cover's name.
    let staged = || -> std::io::Result<()> {
        use std::io::Write as _;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&webp)?;
        f.flush()?;
        f.sync_all()
    };
    staged()
        .and_then(|()| fs::publish(&tmp, &out, &intent).map(|_| ()))
        .map_err(|e| {
            let _ = fs::remove_file(&tmp);
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
    let Ok(entries) = fs::read_dir(dir) else {
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
    thumbnail_candidates_with(library, None)
}

/// [`thumbnail_candidates`] under `dir`, with a cloud library's `.nocover` records.
pub fn thumbnail_candidates_with(dir: &Path, nocover: Option<&NoCoverSet>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, &mut |p| {
        if p.extension()
            .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("cbz"))
            && needs_thumbnail_with(p, nocover)
        {
            out.push(p.to_path_buf());
        }
    });
    out.sort();
    out
}

/// What a cloud library's cover loop has been told to look at.
#[derive(Default)]
struct Arrivals {
    /// Walk the whole library (start, `Rescan`).
    full: bool,
    /// Archives (or folders) that appeared.
    paths: BTreeSet<PathBuf>,
}

type Stamp = (u64, i128);

fn stamp(path: &Path) -> Option<Stamp> {
    let m = fs::metadata(path).ok()?;
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
    /// A cloud library's `.nocover` records (set by [`Thumbnails::spawn_remote`]).
    nocover: std::sync::OnceLock<Arc<NoCoverSet>>,
}

impl Thumbnails {
    /// Forget the `.nocover` record of `cbz` on a cloud library (a case merge trashed
    /// or moved the archive); nothing on a local one, where the marker is a file.
    pub fn forget_nocover(&self, cbz: &Path) {
        if let Some(n) = self.nocover.get()
            && let Err(e) = n.remove(cbz)
        {
            tracing::warn!(
                "could not drop the .nocover record of {}: {e}",
                cbz.display()
            );
        }
    }

    /// Volumes still waiting for a cover, as of the last scan.
    pub fn pending(&self) -> i64 {
        self.pending.load(Ordering::Relaxed)
    }

    /// One pass over the library (`_scan_thumbnails_once`); returns covers written.
    pub fn scan_once(&self, library: &Path, stop: &CancellationToken) -> usize {
        let candidates = thumbnail_candidates(library);
        self.make_covers(candidates, None, stop)
    }

    /// Covers for `candidates` (minus those that would not decode at this version).
    fn make_covers(
        &self,
        candidates: Vec<PathBuf>,
        nocover: Option<&NoCoverSet>,
        stop: &CancellationToken,
    ) -> usize {
        let candidates: Vec<PathBuf> = {
            let failed = self.failed.lock();
            candidates
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
            match ensure_thumbnail_with(&cbz, nocover) {
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

    /// The cover loop of a cloud library mounted at `library` (no periodic walk): one
    /// walk of the store's index now and after every `Rescan`, then the archives its
    /// change events announce. `.nocover` records go to
    /// `<library's parent>/nocover.json` ([`NoCoverSet`]).
    pub fn spawn_remote(
        self: &Arc<Self>,
        library: PathBuf,
        stop: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        use bunko_store::WatchEvent;
        let file = library
            .parent()
            .map_or_else(|| PathBuf::from("nocover.json"), |d| d.join("nocover.json"));
        let nocover = Arc::new(NoCoverSet::load(file, library.clone()));
        let _ = self.nocover.set(nocover.clone());
        let queue = Arc::new((
            Mutex::new(Arrivals {
                full: true,
                ..Arrivals::default()
            }),
            tokio::sync::Notify::new(),
        ));
        let guard = {
            let q = queue.clone();
            let sink: bunko_store::WatchSink = Arc::new(move |ev: WatchEvent| {
                let mut a = q.0.lock();
                match ev {
                    WatchEvent::Added { path, is_dir } => {
                        let cbz = path
                            .extension()
                            .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("cbz"));
                        if is_dir || cbz {
                            a.paths.insert(path);
                        } else {
                            return;
                        }
                    }
                    WatchEvent::Rescan => a.full = true,
                    WatchEvent::Removed { .. } => return,
                }
                drop(a);
                q.1.notify_one();
            });
            match bunko_store::store_for(&library).map(|s| s.watch(sink)) {
                Some(Ok(g)) => Some(g),
                Some(Err(e)) => {
                    warn!("Thumbnails: the library's change events are unavailable: {e}");
                    None
                }
                None => None,
            }
        };
        let me = self.clone();
        tokio::spawn(async move {
            let _watching = guard;
            loop {
                let work = std::mem::take(&mut *queue.0.lock());
                if !work.full && work.paths.is_empty() {
                    tokio::select! {
                        _ = stop.cancelled() => return,
                        _ = queue.1.notified() => continue,
                    }
                }
                let (m, lib, s, n) = (me.clone(), library.clone(), stop.clone(), nocover.clone());
                let scan = tokio::task::spawn_blocking(move || {
                    let candidates = if work.full {
                        thumbnail_candidates_with(&lib, Some(&n))
                    } else {
                        let mut out = BTreeSet::new();
                        for p in &work.paths {
                            if fs::is_dir(p) {
                                out.extend(thumbnail_candidates_with(p, Some(&n)));
                            } else if needs_thumbnail_with(p, Some(&n)) {
                                out.insert(p.clone());
                            }
                        }
                        out.into_iter().collect()
                    };
                    m.make_covers(candidates, Some(&n), &s)
                });
                if let Err(e) = scan.await {
                    warn!("Thumbnail scan error: {e}");
                }
                if stop.is_cancelled() {
                    return;
                }
            }
        })
    }
}

/// What the corrupt-sidecar sweep learnt about one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarCheck {
    Valid,
    /// Not JSON as Python reads it (or not readable at all): safe to delete.
    Invalid,
    /// Over [`bunko_library::sidecar::MAX_SIDECAR_BYTES`] (inflated): unreadable here, but
    /// not provably damaged, so it is left alone.
    TooLarge,
}

/// A reader that fails with `FileTooLarge` once more than `left` bytes went through.
struct Capped<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n as u64 > self.left {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "sidecar larger than the cap",
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// Whether a sidecar parses as Python's `json.load` would (gzip-aware).
pub fn is_valid_sidecar(path: &Path) -> bool {
    check_sidecar(path) == SidecarCheck::Valid
}

/// [`is_valid_sidecar`], telling "damaged" from "too large to read" (at most
/// [`bunko_library::sidecar::MAX_SIDECAR_BYTES`] are inflated, streamed, never held).
pub fn check_sidecar(path: &Path) -> SidecarCheck {
    check_sidecar_capped(path, bunko_library::sidecar::MAX_SIDECAR_BYTES)
}

fn check_sidecar_capped(path: &Path, cap: u64) -> SidecarCheck {
    let too_large = |e: &std::io::Error| e.kind() == std::io::ErrorKind::FileTooLarge;
    let open = || -> std::io::Result<Box<dyn Read>> {
        let file = fs::File::open(path)?;
        let gz = path.to_string_lossy().ends_with(".mokuro.gz");
        let reader: Box<dyn Read> = if gz {
            Box::new(flate2::read::GzDecoder::new(std::io::BufReader::new(file)))
        } else {
            Box::new(std::io::BufReader::new(file))
        };
        Ok(Box::new(Capped {
            inner: reader,
            left: cap,
        }))
    };
    let Ok(reader) = open() else {
        return SidecarCheck::Invalid;
    };
    match serde_json::from_reader::<_, serde::de::IgnoredAny>(reader) {
        Ok(_) => return SidecarCheck::Valid,
        Err(e) if e.io_error_kind() == Some(std::io::ErrorKind::FileTooLarge) => {
            return SidecarCheck::TooLarge;
        }
        Err(_) => {}
    }
    // serde is stricter than Python in two ways: NaN/Infinity and lone surrogates. The
    // second look reads the whole text, allocated once at its size (never past the cap).
    let text = match bunko_library::sidecar::read_sidecar_bytes_capped(path, cap) {
        Ok(text) => text,
        Err(e) if too_large(&e) => return SidecarCheck::TooLarge,
        Err(_) => return SidecarCheck::Invalid,
    };
    let Ok(text) = String::from_utf8(text) else {
        return SidecarCheck::Invalid;
    };
    match bunko_layout::json::Value::parse(&text) {
        Ok(_) => SidecarCheck::Valid,
        Err(e) if e.to_string().contains("surrogate") => SidecarCheck::Valid,
        Err(_) => SidecarCheck::Invalid,
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
        if !fs::is_file(&path) {
            continue;
        }
        match check_sidecar(&path) {
            SidecarCheck::Valid => continue,
            SidecarCheck::TooLarge => {
                warn!(
                    "Sidecar too large to check (over {} MiB inflated), left alone: {}",
                    bunko_library::sidecar::MAX_SIDECAR_BYTES / (1024 * 1024),
                    path.display()
                );
                continue;
            }
            SidecarCheck::Invalid => {}
        }
        match fs::remove_file(&path) {
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
#[allow(clippy::disallowed_methods)] // fixtures are written with std::fs
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_nocover_record_is_dropped_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nocover.json");
        std::fs::write(
            &file,
            br#"[["kingdom/Vol 1.cbz", 3, "7"], ["Kingdom/Vol 1.cbz", 3, "7"]]"#,
        )
        .unwrap();
        let lib = dir.path().join("library");
        let set = NoCoverSet::load(file.clone(), lib.clone());
        assert_eq!(set.len(), 2);
        set.remove(&lib.join("kingdom/Vol 1.cbz")).unwrap();
        assert_eq!(set.len(), 1);
        let again = NoCoverSet::load(file, lib.clone());
        assert_eq!(again.len(), 1, "persisted");
        set.remove(&lib.join("Other/none.cbz")).unwrap();
        assert_eq!(set.len(), 1);
    }

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

    /// Regression (upgrade test): neither 0.5.3 nor 0.7 made a cover for `Vol.CBZ`: the
    /// walk asked for exactly `.cbz` though every other archive check ignores case.
    #[test]
    fn upper_case_archives_get_covers() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        let series = lib.join("Series");
        std::fs::create_dir_all(&series).unwrap();
        cbz(&series.join("Vol 1.CBZ"), &[("p.png", &png(20, 30))]);
        cbz(&series.join("Vol 2.Cbz"), &[("p.png", &png(20, 30))]);
        assert_eq!(thumbnail_candidates(&lib).len(), 2);
        let thumbs = Thumbnails::default();
        assert_eq!(thumbs.scan_once(&lib, &CancellationToken::new()), 2);
        assert!(series.join("Vol 1.webp").is_file());
        assert!(series.join("Vol 2.webp").is_file());
        assert!(thumbnail_candidates(&lib).is_empty());
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

    /// Regression (review finding): a 1.5 MB gzip of whitespace inflated to 1.5 GiB in
    /// memory here. Inflation stops at the cap, and an over-cap sidecar is left in place
    /// (it is not provably damaged), while small damaged ones are still removed.
    #[test]
    fn over_cap_sidecars_are_not_inflated_or_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("V.mokuro.gz");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(b"{\"pages\": [").unwrap();
        gz.write_all(&vec![b' '; 4 << 20]).unwrap();
        gz.write_all(b"]}").unwrap();
        std::fs::write(&path, gz.finish().unwrap()).unwrap();
        assert_eq!(check_sidecar_capped(&path, 1 << 20), SidecarCheck::TooLarge);
        assert_eq!(check_sidecar_capped(&path, 8 << 20), SidecarCheck::Valid);
        let plain = tmp.path().join("W.mokuro");
        std::fs::write(&plain, vec![b' '; 2 << 20]).unwrap();
        assert_eq!(
            check_sidecar_capped(&plain, 1 << 20),
            SidecarCheck::TooLarge
        );
        assert_eq!(check_sidecar_capped(&plain, 4 << 20), SidecarCheck::Invalid);
    }
}
