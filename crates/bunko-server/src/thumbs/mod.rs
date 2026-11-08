//! Library housekeeping that 0.5.2's OCR worker did beside OCR (spec ocr-ppocr-layout
//! §9, ocr-scheduling §5.1; `watcher.py` `_run_thumbnail_loop`,
//! `_remove_corrupt_sidecars`; `processor.py` `ensure_thumbnail`):
//!
//! * **Cover thumbnails.** Every poll interval (`ocr.poll_interval`, shared with the
//!   OCR scan) every `*.cbz` under the library that has neither `<stem>.webp` nor
//!   `<stem>.nocover` gets a cover. A cover beside an archive is never remade, however
//!   old: a client uploads a volume's cover before its archive, so a cover older than
//!   its archive is the usual case, and a volume is replaced by deleting it (which takes
//!   its cover and marker) and uploading it again. The cover made is the first name of
//!   the archive's plain sorted namelist with an image suffix, `ImageOps.contain(250×350, LANCZOS)`, lossy WebP
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
//! Neither pass enters a frozen case-variant group (`bunko_core::fold_groups`: its
//! members, the files rule v1 gives their archives, member folders) nor a series
//! spelled like a reader's per-user name; a cover or marker is written under the
//! WebDAV write lock of its name.
//!
//! **A cloud library** (0.8 §8, D13, D14): covers are made as archives arrive (the
//! store's change events, plus one walk of its index at start and after a `Rescan`),
//! never by a periodic walk; there is no corrupt-sidecar sweep (report-only on
//! remote); and `.nocover` is not a file in the cloud but a local-only record,
//! [`NoCoverSet`], kept in `<base>/cloud/<store-id>/nocover.json` until the store's
//! durable `local_row` table offers `nocover` rows (it keeps only staging rows today).
//! A `.nocover` file a copied-in library brought is still honoured, except beside an
//! archive of JPEG XL pages (a build before 0.7.5 could not read those).
//! An archive an open case merge is moving gets its cover only after the merge (it
//! is looked at again every [`MERGE_RETRY`]): the merge moves the archive before its
//! cover, and a cover made in between would meet the moving one as a duplicate.
//!
//! **A cover never outlives its archive.** A reader renaming a volume moves its cover
//! first and the archive last; a cover made in between, from the archive still under
//! the old name, would be left beside nothing once the archive moves (and the reader
//! then takes it for a volume it does not have). So a cover this process made lately
//! ([`MADE_KEPT`]) goes with its archive: a file MOVE of the archive (DAV, or a move in
//! the cloud) takes it along to the new name, or removes it when the new name has a
//! cover already, and a deleted archive takes it too. A WebDAV request does this
//! before it answers; a change seen in the cloud, just after, on one thread that runs
//! these in order (never on the store's events thread). A cover a client wrote, or
//! replaced since, stays where it is (sidecars are pinned on a MOVE). While a MOVE is
//! on its way (from just before its rename until that follow-up ran) the cover worker
//! leaves the archive's new name alone, and looks at it again after: a cover it made
//! there would meet the one moving along.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use bunko_core::Config;
use bunko_db::Database;
use bunko_store::{Durability, Mode, Origin, WriteIntent, fs};
use parking_lot::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Suffixes a cover may have: `_extract_cover_image_data`'s (no `.avif`), plus JPEG XL.
pub const COVER_EXTENSIONS: [&str; 9] = [
    ".jpg", ".jpeg", ".png", ".gif", ".bmp", ".webp", ".tiff", ".tif", ".jxl",
];
/// Decode budget of one cover (pixel buffers).
pub const MAX_DECODE_BYTES: u64 = 192 * 1024 * 1024;
/// Largest cover member read into memory.
pub const MAX_MEMBER_BYTES: u64 = 64 * 1024 * 1024;
/// Longest cover side accepted.
pub const MAX_SIDE: u32 = 20_000;
/// How soon a cloud library looks again at an archive a case merge was moving.
const MERGE_RETRY: Duration = Duration::from_secs(5);

/// How long a cover this process made follows its archive. A reader renaming a volume
/// moves what it listed before it began, the cover before the archive, so a cover made
/// since is one it does not know to move; a rename takes seconds, not minutes.
const MADE_KEPT: Duration = Duration::from_secs(10 * 60);

/// The longest a MOVE's notice keeps the cover worker off the archive's new name. The
/// notice is withdrawn as soon as the MOVE is over (its hook ran, or its rename
/// failed); this only bounds one that somehow never is.
const MOVING_KEPT: Duration = Duration::from_secs(10 * 60);

/// A case merge is moving `p` ([`crate::ocr::owed::merge_busy`]).
type Busy<'a> = &'a dyn Fn(&Path) -> bool;

/// The covers this process published lately: cover path -> (size, when). Its lock is
/// held from the archive's last check to the record, and by the archive's MOVE and
/// DELETE hooks, so a move lands either before the cover is published (no cover) or
/// after it is recorded (the hook takes it along). Beside them, the new names of the
/// archives a MOVE is bringing in ([`Moving`]).
#[derive(Default)]
pub struct MadeCovers(
    Mutex<HashMap<PathBuf, (u64, Instant)>>,
    Moving,
    /// The WebDAV write locks (the folded per-name lock a PUT and the OCR install
    /// take), once the server has set them ([`Thumbnails::set_locks`]).
    std::sync::OnceLock<crate::ocr::types::PathLocks>,
);

/// A write lock on a cover or marker name could not be had: a WebDAV write holds it.
struct NameHeld;

/// The new names of the archives a MOVE is bringing in, from just before the rename
/// until the move's cover hook ran (or the rename failed). The archive is under its
/// new name before that hook runs, and the store may already have announced it there
/// (a cloud library's rename emits the change before it returns), which wakes the
/// cover worker: a cover made then would meet the one the hook moves along, and on a
/// cloud library one of the two nodes would be lost. So the worker passes such a name
/// over and is sent back to it once the move is over (the hook may have brought no
/// cover). Its locks are leaves: nothing is taken while one is held, so the store's
/// events thread may take them, and the cover lock may be held around them.
#[derive(Default)]
struct Moving {
    names: Mutex<HashMap<PathBuf, MovingName>>,
    /// Looks at an archive again: a cloud library's cover loop
    /// ([`Thumbnails::spawn_remote`]). Locally the next scan does.
    again: Mutex<Option<Again>>,
}

type Again = Arc<dyn Fn(PathBuf) + Send + Sync>;

/// One new name being moved to, keyed like a cover record ([`MadeCovers::key`]).
struct MovingName {
    /// Moves to it still on their way.
    moves: usize,
    /// The cover worker came for it meanwhile.
    passed: bool,
    since: Instant,
}

impl Moving {
    /// `cbz` is the new name of an archive a MOVE is bringing in: the cover worker
    /// passes it over, and is sent back to it when the move is over. Called after the
    /// archive was found there: the MOVE registered before its rename began.
    fn passes_over(&self, cbz: &Path) -> bool {
        if self.names.lock().is_empty() {
            return false;
        }
        let key = MadeCovers::key(cbz);
        match self.names.lock().get_mut(&key) {
            Some(n) if n.since.elapsed() < MOVING_KEPT => {
                n.passed = true;
                true
            }
            _ => false,
        }
    }
}

/// A MOVE's notice ([`MadeCovers::moving`]): withdrawn when dropped, and then the cover
/// worker is sent back to the archive if it came for it meanwhile.
pub struct MoveGuard {
    made: Arc<MadeCovers>,
    key: PathBuf,
    to: PathBuf,
}

impl Drop for MoveGuard {
    fn drop(&mut self) {
        let passed = {
            let mut names = self.made.1.names.lock();
            match names.get_mut(&self.key) {
                Some(n) if n.moves > 1 => {
                    n.moves -= 1;
                    false
                }
                Some(_) => names.remove(&self.key).is_some_and(|n| n.passed),
                None => false,
            }
        };
        let again = self.made.1.again.lock().clone();
        if passed && let Some(again) = again {
            again(self.to.clone());
        }
    }
}

impl MadeCovers {
    /// The WebDAV write lock on `path` (folded: every spelling of the name), so a cover
    /// or marker is never written while a client writes that name in any spelling.
    /// `Ok(None)` when no locks were set (a test, a tool).
    fn write_lock(&self, path: &Path) -> Result<Option<Box<dyn Send>>, NameHeld> {
        match self.2.get() {
            None => Ok(None),
            Some(locks) => locks.try_lock(path).map(Some).ok_or(NameHeld),
        }
    }

    /// The record of `cover` is kept under its folder as the disk names it, plus its
    /// name. The cover worker finds archives under the library root as configured; a
    /// WebDAV MOVE or DELETE names them under the root bunko-dav canonicalized. The two
    /// differ on Windows always (the `\\?\` final path, 8.3 names expanded) and on any
    /// host whose root goes through a link (macOS temp folders: `/var` is
    /// `/private/var`): keyed as given, the hook never found the cover it made. A cloud
    /// library's paths are mount paths, one spelling already, and are kept as given
    /// (so no store lookup runs under the lock).
    fn key(cover: &Path) -> PathBuf {
        if bunko_store::store_for(cover).is_some_and(|s| s.caps().remote) {
            return cover.to_path_buf();
        }
        match (cover.parent(), cover.file_name()) {
            (Some(dir), Some(name)) if !dir.as_os_str().is_empty() => {
                fs::canonicalize(dir).map_or_else(|_| cover.to_path_buf(), |d| d.join(name))
            }
            _ => cover.to_path_buf(),
        }
    }

    /// Forget the covers made longer ago than [`MADE_KEPT`].
    fn prune(made: &mut HashMap<PathBuf, (u64, Instant)>) {
        made.retain(|_, (_, at)| at.elapsed() < MADE_KEPT);
    }

    /// The cover made for `cbz`, now gone, and its size, if it is still the one made
    /// (by size): its record is taken. `None` while the archive is there (a replaced
    /// one), and for a folder (`Series/` is not the archive `Series.cbz`).
    fn take_left(
        made: &mut HashMap<PathBuf, (u64, Instant)>,
        cbz: &Path,
    ) -> Option<(PathBuf, u64)> {
        Self::prune(made);
        if !is_cbz(cbz) {
            return None;
        }
        let cover = cover_path(cbz)?;
        let key = Self::key(&cover);
        // Still listed under exactly this name (an archive replaced, not gone). A listing,
        // not a lookup: after a case-only rename the old name still opens on a
        // case-insensitive disk.
        if !made.contains_key(&key) || listed_exactly(cbz) {
            return None;
        }
        let (size, _) = made.remove(&key)?;
        fs::metadata(&cover)
            .is_ok_and(|m| m.is_file() && m.len() == size)
            .then_some((cover, size))
    }

    /// A MOVE is about to bring an archive in under `to`: until the guard is dropped the
    /// cover worker leaves `to` alone ([`Moving`]). Never waits for the cover lock (the
    /// store's events thread calls it).
    pub fn moving(self: &Arc<Self>, to: &Path) -> MoveGuard {
        let key = Self::key(to);
        let now = Instant::now();
        let mut names = self.1.names.lock();
        names.retain(|_, n| now.duration_since(n.since) < MOVING_KEPT);
        let n = names.entry(key.clone()).or_insert(MovingName {
            moves: 0,
            passed: false,
            since: now,
        });
        n.moves += 1;
        n.since = now;
        drop(names);
        MoveGuard {
            made: self.clone(),
            key,
            to: to.to_path_buf(),
        }
    }

    /// When `cover` is one this process made and is still as made (by size), it is
    /// removed and its record taken; anything else stays. On a cloud library a removal
    /// cancels the cover's upload if it has not left, and waits out one on its way (then
    /// deletes what it left): the name is free when it returns.
    fn remove_own(made: &mut HashMap<PathBuf, (u64, Instant)>, cover: &Path) {
        let key = Self::key(cover);
        let Some(&(size, _)) = made.get(&key) else {
            return;
        };
        if !fs::metadata(cover).is_ok_and(|m| m.is_file() && m.len() == size) {
            return;
        }
        match fs::remove_file(cover) {
            Ok(()) => {
                made.remove(&key);
            }
            Err(e) => warn!(
                "could not replace the cover {} with the one of the archive moved there: {e}",
                cover.display()
            ),
        }
    }

    /// The archive `from` was moved to `to`: the cover made for it goes along, or goes
    /// when `to` has a cover already (the reader moved its own there first). A case
    /// merge moves covers itself.
    ///
    /// A cover at `to` that this process made gives way to the one moving along: the
    /// move replaced an archive Bunko had covered, or (a move seen in the cloud, whose
    /// notice comes only as its hooks run) the cover worker, woken by the archive's
    /// arrival under `to`, covered it first. Either way that cover is Bunko's, not the
    /// reader's: on a cloud library keeping it deleted the node the reader knew and
    /// uploaded the same picture again as a new one, and a replaced archive's cover
    /// would stay beside the archive moved over it. For a WebDAV MOVE the worker never
    /// covers `to` before this hook ran ([`Moving`]).
    pub fn archive_moved(&self, from: &Path, to: &Path) {
        if crate::ocr::owed::merge_busy(from) || crate::ocr::owed::merge_busy(to) {
            return;
        }
        let mut made = self.0.lock();
        let Some((cover, size)) = Self::take_left(&mut made, from) else {
            return;
        };
        // The cover `to` may have, as Bunko writes it (the listed spelling, else the
        // archive's): none when every listed spelling is another archive's (a hidden
        // case variant's own cover). Then the made cover goes: a second spelling is
        // never made beside that file.
        let Some(dest) = bunko_library::sidecar::sibling(to, ".webp") else {
            if let Err(e) = fs::remove_file(&cover) {
                warn!(
                    "could not remove the cover of a moved archive {}: {e}",
                    cover.display()
                );
            }
            return;
        };
        // A case-only rename (`vol 1.cbz` to `Vol 1.cbz`): the cover is the one the new
        // name resolves to. It is respelled like the archive, never removed.
        if dest == cover || fs::same_file(&cover, &dest) {
            let respelled = bunko_library::sidecar::with_suffix(to, ".webp");
            let at = if respelled != cover && fs::rename(&cover, &respelled).is_ok() {
                respelled
            } else {
                cover
            };
            made.insert(Self::key(&at), (size, Instant::now()));
            return;
        }
        // The name is taken under the WebDAV write lock of the cover's name, every
        // spelling (as a cover is published): a client's PUT of it in any spelling cannot
        // interleave. Held, a client is writing that cover, and the client's wins.
        let Ok(_guard) = self.write_lock(&dest) else {
            if let Err(e) = fs::remove_file(&cover) {
                warn!(
                    "could not remove the cover of a moved archive {}: {e}",
                    cover.display()
                );
            }
            return;
        };
        // A cover Bunko made at `to`, still as made, gives way; under the lock the name
        // is looked up again from the listing (one listed since is the cover's name).
        Self::remove_own(&mut made, &dest);
        let dest = bunko_library::sidecar::sibling(to, ".webp");
        if let Some(dest) = dest
            && !listed_exactly(&dest)
            && fs::rename(&cover, &dest).is_ok()
        {
            made.insert(Self::key(&dest), (size, Instant::now()));
            return;
        }
        if let Err(e) = fs::remove_file(&cover) {
            warn!(
                "could not remove the cover of a moved archive {}: {e}",
                cover.display()
            );
        }
    }

    /// Archives a DELETE or MOVE took away: the covers made for them go too.
    pub fn archives_removed(&self, paths: &[PathBuf]) {
        for cbz in paths {
            if crate::ocr::owed::merge_busy(cbz) {
                continue;
            }
            let Some((cover, _)) = Self::take_left(&mut self.0.lock(), cbz) else {
                continue;
            };
            if let Err(e) = fs::remove_file(&cover) {
                warn!(
                    "could not remove the cover of a removed archive {}: {e}",
                    cover.display()
                );
            }
        }
    }
}

/// `p`'s folder lists exactly its name.
fn listed_exactly(p: &Path) -> bool {
    let (Some(dir), Some(name)) = (p.parent(), p.file_name()) else {
        return false;
    };
    match fs::read_dir(dir) {
        Ok(entries) => entries.flatten().any(|e| e.file_name() == name),
        // Unlistable: an archive that opens is there.
        Err(_) => fs::exists(p),
    }
}

/// A `.cbz` name, any case.
fn is_cbz(p: &Path) -> bool {
    p.extension()
        .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("cbz"))
}

/// `<stem>.webp` as the folder lists it for the archive (`V.WEBP`;
/// [`bunko_library::sidecar::cover_path`]), `None` when it lists none: whether a cover
/// is there is decided from the listing, never by opening a built name a host might
/// resolve to another spelling. For reads only: every writer asks
/// [`bunko_library::sidecar::sibling`] itself and writes nothing on `None`.
pub fn cover_path(cbz: &Path) -> Option<PathBuf> {
    bunko_library::sidecar::cover_path(cbz)
}

/// `<stem>.nocover` as the folder lists it for the archive: a cover was looked for
/// and there is none. `None` when no marker is listed for it (as [`cover_path`]).
pub fn nocover_path(cbz: &Path) -> Option<PathBuf> {
    bunko_library::sidecar::nocover_marker_path(cbz)
}

/// `needs_thumbnail`.
pub fn needs_thumbnail(cbz: &Path) -> bool {
    needs_thumbnail_with(cbz, None)
}

/// [`needs_thumbnail`], with a cloud library's local `.nocover` records.
pub fn needs_thumbnail_with(cbz: &Path, nocover: Option<&NoCoverSet>) -> bool {
    fs::is_file(cbz)
        && is_cbz(cbz)
        // Shadowed by another spelling of itself, or its cover's name is another
        // volume's: nothing is written for it.
        && bunko_library::sidecar::sibling(cbz, ".webp").is_some()
        && !cover_path(cbz).is_some_and(|cover| written_for(&cover, cbz))
        && !marked_nocover(cbz, nocover.is_some())
        && nocover.is_none_or(|n| !n.contains(cbz))
}

/// A `<stem>.nocover` file beside the archive. In a cloud folder
/// such a file never comes from this build (its markers are [`NoCoverSet`] records,
/// D14) but from a library copied in, and one a build before JPEG XL covers left for
/// an archive holding `.jxl` images is not believed: the local library's marker is
/// removed at start ([`clear_jxl_nocovers`]), the cloud one is left where it is and
/// a missing cover is recorded locally as usual.
fn marked_nocover(cbz: &Path, cloud: bool) -> bool {
    nocover_path(cbz).is_some_and(|marker| written_for(&marker, cbz)) && !(cloud && holds_jxl(cbz))
}

/// `side` (a cover or a `.nocover` marker) is there for `cbz`, whatever its age. Never
/// judged by time: a client uploads a volume's cover before its archive, so a cover
/// older than its archive is the usual case (a reader's own, often made by hand), and
/// a volume is replaced by deleting it, which takes its cover and marker along.
fn written_for(side: &Path, _cbz: &Path) -> bool {
    fs::metadata(side).is_ok()
}

/// D14: the `.nocover` markers of a cloud library, as local-only records keyed by the
/// archive's library path and version `(size, mtime_ns)` (a new version of the archive
/// is looked at again). Persisted as JSON in the store's own directory; a stand-in for
/// the store's `local_row` table, which holds only staging rows so far.
pub struct NoCoverSet {
    file: PathBuf,
    library: PathBuf,
    set: Mutex<HashSet<(String, u64, i128)>>,
    /// Paths of the records found in `file` at load whose archive [`Self::clear_jxl`]
    /// has not looked at yet (an earlier build's records; one this build writes is
    /// its own verdict).
    unchecked: Mutex<HashSet<String>>,
}

impl NoCoverSet {
    /// The set kept in `file` for the library at `library` (empty when unreadable).
    // unmounted: `file` is in the store's local directory, not in the library.
    #[allow(clippy::disallowed_methods)]
    pub fn load(file: PathBuf, library: PathBuf) -> NoCoverSet {
        let set: HashSet<(String, u64, i128)> = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<(String, u64, String)>>(&b).ok())
            .map(|rows| {
                rows.into_iter()
                    .filter_map(|(p, s, m)| Some((p, s, m.parse().ok()?)))
                    .collect()
            })
            .unwrap_or_default();
        let unchecked = set.iter().map(|(p, _, _)| p.clone()).collect();
        NoCoverSet {
            file,
            library,
            set: Mutex::new(set),
            unchecked: Mutex::new(unchecked),
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
        self.unchecked.lock().remove(&k.0);
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
        self.unchecked.lock().remove(&rel);
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

    /// [`clear_jxl_nocovers`] for a cloud library: drop the record an earlier build
    /// kept for an archive that holds a `.jxl` image (a build before JPEG XL covers
    /// recorded it as having none), so this pass makes its cover. Each loaded record
    /// is looked at once, when its archive is in the library; one whose archive is
    /// not (yet: a store still syncing) waits for a later pass, so the cover loop runs
    /// this before every pass. Returns the archives whose record went.
    pub fn clear_jxl(&self) -> Vec<PathBuf> {
        let pending: Vec<String> = {
            let unchecked = self.unchecked.lock();
            if unchecked.is_empty() {
                return Vec::new();
            }
            let mut paths: Vec<String> = unchecked.iter().cloned().collect();
            paths.sort();
            paths
        };
        let mut gone = HashSet::new();
        for rel in pending {
            let cbz = self.library.join(&rel);
            // Listed under exactly the record's name: a record keyed by another
            // spelling matches no archive ([`Self::contains`]), and a cover made
            // for it would be a second spelling of the volume's.
            if !fs::is_file(&cbz) || !listed_exactly(&cbz) {
                continue;
            }
            self.unchecked.lock().remove(&rel);
            if holds_jxl(&cbz) {
                gone.insert(rel);
            }
        }
        if gone.is_empty() {
            return Vec::new();
        }
        for rel in &gone {
            info!("Looking for a JPEG XL cover again: {rel}");
        }
        let rows = {
            let mut set = self.set.lock();
            set.retain(|(p, _, _)| !gone.contains(p));
            Self::rows(&set)
        };
        if let Err(e) = self.save(&rows) {
            warn!("could not save the .nocover records: {e}");
        }
        let mut cleared: Vec<PathBuf> = gone.iter().map(|rel| self.library.join(rel)).collect();
        cleared.sort();
        cleared
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
/// needed). On a cloud library a missing cover is recorded in `nocover` instead of a
/// `.nocover` file (D14). The cover is published only while `busy` is false, and
/// recorded in `made`.
fn ensure_cover(
    cbz: &Path,
    nocover: Option<&NoCoverSet>,
    busy: Busy,
    made: &MadeCovers,
) -> Result<bool, String> {
    if !needs_thumbnail_with(cbz, nocover) {
        return Ok(true);
    }
    // Not read while a MOVE brings it in (the publish below checks again, decisively).
    if made.1.passes_over(cbz) {
        return Ok(false);
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
                // Never a marker another archive's listed `.nocover` would be, nor one
                // in a frozen case-variant group; written under the WebDAV write lock
                // of its name, the name looked up again under it.
                None => {
                    if let Some(marker) = bunko_library::sidecar::sibling(cbz, ".nocover") {
                        let Ok(_guard) = made.write_lock(&marker) else {
                            return Ok(false);
                        };
                        if bunko_library::sidecar::sibling(cbz, ".nocover").as_ref()
                            == Some(&marker)
                        {
                            fs::File::create(marker)
                                .map_err(|e| format!("could not write the .nocover marker: {e}"))?;
                        }
                    }
                }
            }
            return Ok(false);
        }
    };
    let webp = bunko_thumb::make_thumbnail_limited(&bytes, MAX_DECODE_BYTES, MAX_SIDE)
        .map_err(|e| format!("Failed to generate thumbnail for {name}: {e}"))?;
    drop(bytes);
    if !publish_cover(cbz, &webp, busy, made)? {
        return Ok(false);
    }
    // A marker the archive no longer needs: one that landed while the cover was made,
    // or a cloud folder's marker beside JPEG XL pages, which is not believed.
    match nocover {
        Some(set) => {
            if let Err(e) = set.remove(cbz) {
                warn!(
                    "could not drop the .nocover record of {}: {e}",
                    cbz.display()
                );
            }
        }
        None => {
            // Under the write lock, the marker is looked up again and must still be the
            // one the folder lists for the archive (decided from the listing, never by
            // opening the name).
            if let Some(marker) = bunko_library::sidecar::sibling(cbz, ".nocover")
                && let Ok(_guard) = made.write_lock(&marker)
                && bunko_library::sidecar::sibling(cbz, ".nocover").as_ref() == Some(&marker)
                && listed_exactly(&marker)
                && let Err(e) = fs::remove_file(&marker)
            {
                warn!("could not remove the stale {}: {e}", marker.display());
            }
        }
    }
    info!(
        "Created thumbnail: {}",
        cover_path(cbz)
            .and_then(|c| c.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default()
    );
    Ok(true)
}

/// Write `webp` as `cbz`'s cover; false when the archive was gone. A reader renaming a
/// volume moves its cover first and the archive last, and its series rename then moves
/// both again: a cover made from the archive while it moved would land where nothing
/// is left, an orphan that keeps the old folder from ever emptying. False too when a
/// case merge opened on it meanwhile (`busy`): the merge moves its own cover in; and
/// while a MOVE brings the archive in under this name ([`Moving`]): the move takes its
/// cover along. A published cover is recorded in `made`, for the archive's move to
/// take along.
fn publish_cover(cbz: &Path, webp: &[u8], busy: Busy, made: &MadeCovers) -> Result<bool, String> {
    publish_cover_with(cbz, webp, busy, made, &|_| {})
}

/// [`publish_cover`], calling `before_record` between the archive's last check and the
/// record (tests land a MOVE or DELETE there).
fn publish_cover_with(
    cbz: &Path,
    webp: &[u8],
    busy: Busy,
    made: &MadeCovers,
    before_record: &dyn Fn(&Path),
) -> Result<bool, String> {
    let covers = made;
    let moving = &made.1;
    let mut made = made.0.lock();
    if !fs::exists(cbz) || busy(cbz) {
        return Ok(false);
    }
    // After the archive was found: a MOVE that brought it here registered before its
    // rename began, so it is seen; looked at before, it could be missed.
    if moving.passes_over(cbz) {
        return Ok(false);
    }
    let name = cbz
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(out) = bunko_library::sidecar::sibling(cbz, ".webp") else {
        return Ok(false);
    };
    // The WebDAV write lock of the cover's name, every spelling, as the OCR install
    // takes it for a sidecar: a client's PUT of the cover in another spelling cannot
    // interleave (it would be a second spelling on a case-sensitive disk, or written
    // over on an insensitive one). Held, the next scan tries again. Under it the name
    // is looked up again: one listed since is the cover's name.
    let Ok(_guard) = covers.write_lock(&out) else {
        return Ok(false);
    };
    if bunko_library::sidecar::sibling(cbz, ".webp").as_ref() != Some(&out) {
        return Ok(false);
    }
    // A cover there (a client's, landed meanwhile) stays, whatever its age.
    // Whether a cover is there is the listing's (a built name is never opened).
    if listed_exactly(&out) && fs::metadata(&out).is_ok() {
        return Ok(false);
    }
    let mode = Mode::CreateNew;
    let mut tmp = out.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let intent = WriteIntent::new(mode, Origin::Thumbs, Durability::Deferred);
    // A staging file (on a remote store: local until published; `fs::write` would
    // publish the temporary name itself), then one publish onto the cover's name.
    let staged = || -> std::io::Result<()> {
        use std::io::Write as _;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(webp)?;
        f.flush()?;
        f.sync_all()
    };
    staged()
        .and_then(|()| fs::publish(&tmp, &out, &intent).map(|_| ()))
        .map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("Failed to generate thumbnail for {name}: {e}")
        })?;
    // Moved away while the cover was published: take it back.
    if !fs::exists(cbz) {
        let _ = fs::remove_file(&out);
        return Ok(false);
    }
    before_record(cbz);
    MadeCovers::prune(&mut made);
    made.insert(MadeCovers::key(&out), (webp.len() as u64, Instant::now()));
    Ok(true)
}

/// Every file below `dir`, leaving out what a case-variant group freezes (its members,
/// the winner included, the files rule v1 gives their archives, everything inside a
/// member folder: [`bunko_core::fold_groups`]) and, at the library root (`top`), the
/// per-user names (a cloud progress user's `history/`, a series spelled like one). No
/// cover is made, no marker written and no sidecar deleted there.
fn walk(dir: &Path, top: bool, keep: &mut dyn FnMut(&Path)) {
    let Some((entries, groups)) = bunko_library::fsutil::listed_groups(dir, top) else {
        return;
    };
    for (name, is_dir) in entries {
        if groups.is_frozen(&name) {
            continue;
        }
        let path = dir.join(&name);
        if is_dir {
            walk(&path, false, keep);
        } else {
            keep(&path);
        }
    }
}

/// Every library `.cbz` still waiting for a cover (`_thumbnail_candidates`).
pub fn thumbnail_candidates(library: &Path) -> Vec<PathBuf> {
    thumbnail_candidates_with(library, None)
}

/// [`thumbnail_candidates`] under `dir`, with a cloud library's `.nocover` records.
/// Never an archive in the per-user tree (a cloud progress user's `history/`): that
/// account's file, no volume, and no cover is written beside it.
pub fn thumbnail_candidates_with(dir: &Path, nocover: Option<&NoCoverSet>) -> Vec<PathBuf> {
    let library = nocover.map_or(dir, |n| n.library.as_path());
    let mut out = Vec::new();
    walk(dir, dir == library, &mut |p| {
        if is_cbz(p)
            && !crate::ocr::types::is_per_user_path(library, p)
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
    /// The covers made lately, which follow their archive.
    made: Arc<MadeCovers>,
    follow: FollowUps,
}

type FollowUp = Box<dyn FnOnce() + Send>;

/// Where the cover follow-ups of the WebDAV hooks run. A WebDAV request runs its own,
/// before it answers: a client listing right after a MOVE never sees a cover beside
/// nothing. The cloud's hooks run on the store's events thread, which must never wait
/// for a cover being published (a publish may wait for that thread): theirs are handed
/// to ONE thread, started on first use, which runs them in order, so a MOVE's "the
/// cover goes along" always comes before its "the archive is gone".
#[derive(Default)]
struct FollowUps(Mutex<Option<std::sync::mpsc::Sender<FollowUp>>>);

impl FollowUps {
    fn run(&self, f: impl FnOnce() + Send + 'static) {
        if !bunko_remote::store::on_events_thread() {
            return f();
        }
        let mut tx = self.0.lock();
        if tx.is_none() {
            let (send, recv) = std::sync::mpsc::channel::<FollowUp>();
            let started = std::thread::Builder::new()
                .name("bunko-cover-follow".into())
                .spawn(move || {
                    while let Ok(f) = recv.recv() {
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
                            tracing::error!("a cover follow-up panicked");
                        }
                    }
                });
            match started {
                Ok(_) => *tx = Some(send),
                Err(e) => {
                    warn!("could not start the cover follow-ups of moved archives: {e}");
                    return;
                }
            }
        }
        if let Some(tx) = &*tx {
            let _ = tx.send(Box::new(f));
        }
    }

    /// Wait until every follow-up handed over so far has run.
    #[cfg(test)]
    fn flush(&self) {
        let Some(tx) = self.0.lock().clone() else {
            return;
        };
        let (done, ran) = std::sync::mpsc::sync_channel(1);
        let _ = tx.send(Box::new(move || {
            let _ = done.send(());
        }));
        let _ = ran.recv();
    }
}

/// The cover side of the WebDAV hooks (and the cloud's, which run them): add it with
/// `ServerDavHooks::add_listener`.
impl bunko_dav::DavHooks for Thumbnails {
    fn archive_moving(&self, _from: &Path, to: &Path) -> bunko_dav::HookGuard {
        bunko_dav::HookGuard::new(self.made.moving(to))
    }
    fn archive_moved(&self, from: &Path, to: &Path) {
        // A move seen in the cloud is followed up later, on another thread: until then
        // the cover worker leaves the new name to it, as during a WebDAV MOVE. Its
        // archive is in the index already, so a worker that got there first is met by
        // the follow-up (`MadeCovers::archive_moved`).
        let moving = bunko_remote::store::on_events_thread().then(|| self.made.moving(to));
        let (made, f, t) = (self.made.clone(), from.to_path_buf(), to.to_path_buf());
        self.follow.run(move || {
            made.archive_moved(&f, &t);
            drop(moving);
        });
    }
    fn archives_removed(&self, paths: &[PathBuf]) {
        // Only an archive can have a cover made for it.
        let paths: Vec<PathBuf> = paths.iter().filter(|p| is_cbz(p)).cloned().collect();
        if paths.is_empty() {
            return;
        }
        let made = self.made.clone();
        self.follow.run(move || made.archives_removed(&paths));
    }
}

impl Thumbnails {
    /// The WebDAV write locks, taken for every cover and `.nocover` marker written (as
    /// the OCR install takes them for a sidecar). Set once, by the server.
    pub fn set_locks(&self, locks: crate::ocr::types::PathLocks) {
        let _ = self.made.2.set(locks);
    }

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
        self.make_covers_with(candidates, None, stop, &crate::ocr::owed::merge_busy)
            .0
    }

    /// Covers for `candidates` (minus those that would not decode at this version):
    /// how many were written, and the archives left for after a case merge (`busy`).
    fn make_covers_with(
        &self,
        candidates: Vec<PathBuf>,
        nocover: Option<&NoCoverSet>,
        stop: &CancellationToken,
        busy: Busy,
    ) -> (usize, Vec<PathBuf>) {
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
            return (0, Vec::new());
        }
        info!("Found {} CBZ files missing thumbnails", candidates.len());
        let (mut made, mut deferred) = (0, Vec::new());
        for cbz in candidates {
            if stop.is_cancelled() {
                break;
            }
            if busy(&cbz) {
                deferred.push(cbz);
                self.pending.fetch_sub(1, Ordering::Relaxed);
                continue;
            }
            match ensure_cover(&cbz, nocover, busy, &self.made) {
                Ok(true) => made += 1,
                // The merge opened while the cover was being made.
                Ok(false) if busy(&cbz) => deferred.push(cbz.clone()),
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
        (made, deferred)
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
        // A new name passed over while a MOVE brought the archive in: looked at again
        // once the move is over.
        *self.made.1.again.lock() = Some({
            let q = queue.clone();
            Arc::new(move |p: PathBuf| {
                q.0.lock().paths.insert(p);
                q.1.notify_one();
            })
        });
        let guard = {
            let (q, root) = (queue.clone(), library.clone());
            let sink: bunko_store::WatchSink = Arc::new(move |ev: WatchEvent| {
                let mut a = q.0.lock();
                match ev {
                    WatchEvent::Added { path, .. }
                        if crate::ocr::types::is_per_user_path(&root, &path) =>
                    {
                        return;
                    }
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
                    // JPEG XL volumes an older build recorded as having no cover get
                    // one (each record once its archive is in the index), in this
                    // pass whatever it was woken for.
                    let cleared = n.clear_jxl();
                    let candidates = if work.full {
                        thumbnail_candidates_with(&lib, Some(&n))
                    } else {
                        let mut out = BTreeSet::new();
                        for p in work.paths.iter().chain(&cleared) {
                            // Nothing in a frozen case-variant group, in any folder on
                            // the way (the walk below leaves out the rest).
                            if bunko_library::fsutil::frozen_path(&lib, p).is_some() {
                                continue;
                            }
                            if fs::is_dir(p) {
                                out.extend(thumbnail_candidates_with(p, Some(&n)));
                            } else if needs_thumbnail_with(p, Some(&n)) {
                                out.insert(p.clone());
                            }
                        }
                        out.into_iter().collect()
                    };
                    m.make_covers_with(candidates, Some(&n), &s, &crate::ocr::owed::merge_busy)
                });
                let deferred = match scan.await {
                    Ok((_, deferred)) => deferred,
                    Err(e) => {
                        warn!("Thumbnail scan error: {e}");
                        Vec::new()
                    }
                };
                if stop.is_cancelled() {
                    return;
                }
                // Looked at again once the merge is likely done; no change event
                // announces that.
                if !deferred.is_empty() {
                    queue.0.lock().paths.extend(deferred);
                    tokio::select! {
                        _ = stop.cancelled() => return,
                        _ = tokio::time::sleep(MERGE_RETRY) => {}
                    }
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

/// Upgrade from 0.7.4 and older, which could not read JPEG XL and so marked a volume
/// of JPEG XL pages `.nocover`: drop the marker of every archive holding a `.jxl`
/// image, so the next scan makes its cover. One-shot at startup; returns how many
/// markers went. A cloud library's records are [`NoCoverSet::clear_jxl`]'s. The marker
/// is the one the folder lists in any spelling (`V.NOCOVER`); nothing is removed for
/// an archive another spelling shadows, nor a marker another archive's name owns
/// ([`bunko_library::sidecar::sibling`]).
pub fn clear_jxl_nocovers(library: &Path) -> usize {
    let mut marked = Vec::new();
    walk(library, true, &mut |p| {
        if is_cbz(p)
            && let Some(marker) = bunko_library::sidecar::sibling(p, ".nocover")
            && listed_exactly(&marker)
            && fs::is_file(&marker)
        {
            marked.push((p.to_path_buf(), marker));
        }
    });
    marked.sort();
    let mut cleared = 0;
    for (cbz, marker) in marked {
        if !holds_jxl(&cbz) {
            continue;
        }
        match fs::remove_file(&marker) {
            Ok(()) => {
                cleared += 1;
                info!("Looking for a JPEG XL cover again: {}", cbz.display());
            }
            Err(e) => warn!(
                "could not remove the .nocover marker of {}: {e}",
                cbz.display()
            ),
        }
    }
    cleared
}

/// Whether the archive opens and has a `.jxl` member. Read through its central
/// directory only (on a cloud library, the store's cached tail).
fn holds_jxl(cbz: &Path) -> bool {
    bunko_library::archive::zipdir::list_entries(cbz)
        .is_ok_and(|entries| entries.iter().any(|e| suffix_lower(&e.filename) == ".jxl"))
}

/// `_remove_corrupt_sidecars`: delete every invalid library sidecar; returns how many.
pub fn remove_corrupt_sidecars(library: &Path, db: Option<&Database>) -> usize {
    let mut found = Vec::new();
    walk(library, true, &mut |p| {
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

    /// The cover's name as these fixtures spell it (no other spelling is listed).
    fn cover_of(cbz: &Path) -> PathBuf {
        cbz.with_extension("webp")
    }

    /// The `.nocover` marker's name as these fixtures spell it.
    fn marker(cbz: &Path) -> PathBuf {
        cbz.with_extension("nocover")
    }
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

    fn mtime(p: &Path) -> std::time::SystemTime {
        std::fs::metadata(p).unwrap().modified().unwrap()
    }

    fn set_mtime(p: &Path, t: std::time::SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    /// A client uploads a volume's cover before its archive, so the cover is older than
    /// the archive it belongs to. It is that archive's cover: never remade.
    #[test]
    fn a_cover_uploaded_before_its_archive_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        let series = lib.join("Series");
        std::fs::create_dir_all(&series).unwrap();
        let vol = series.join("Vol 1.cbz");
        std::fs::write(series.join("Vol 1.webp"), b"a hand-made cover").unwrap();
        cbz(&vol, &[("p.png", &png(500, 700))]);
        set_mtime(&cover_of(&vol), mtime(&vol) - Duration::from_secs(600));
        assert!(thumbnail_candidates(&lib).is_empty());
        assert_eq!(
            Thumbnails::default().scan_once(&lib, &CancellationToken::new()),
            0
        );
        assert_eq!(std::fs::read(cover_of(&vol)).unwrap(), b"a hand-made cover");
    }

    /// A cover written after the archive (a reader's own upload): kept.
    #[test]
    fn a_cover_newer_than_its_archive_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        let series = lib.join("Series");
        std::fs::create_dir_all(&series).unwrap();
        let vol = series.join("Vol 1.cbz");
        cbz(&vol, &[("p.png", &png(500, 700))]);
        std::fs::write(cover_of(&vol), b"a client's cover").unwrap();
        set_mtime(&cover_of(&vol), mtime(&vol) + Duration::from_secs(5));
        assert!(thumbnail_candidates(&lib).is_empty());
        assert_eq!(
            Thumbnails::default().scan_once(&lib, &CancellationToken::new()),
            0
        );
        assert_eq!(std::fs::read(cover_of(&vol)).unwrap(), b"a client's cover");
    }

    /// The cover and the `.nocover` marker are written under the WebDAV write lock of
    /// their name (folded: every spelling), as the OCR install writes a sidecar: while a
    /// client's write of `v.WEBP` (or `V.NOCOVER`) holds it, nothing is written, and the
    /// next scan makes it.
    #[test]
    fn covers_and_markers_take_the_webdav_write_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        let series = lib.join("Series");
        std::fs::create_dir_all(&series).unwrap();
        let vol = series.join("V.cbz");
        cbz(&vol, &[("p.png", &png(500, 700))]);
        let bad = series.join("W.cbz");
        cbz(&bad, &[("notes.txt", b"x")]);
        let locks = bunko_dav::PathWriteLocks::new();
        let thumbs = Thumbnails::default();
        thumbs.set_locks(Arc::new(crate::ocr::types::DavLocks(locks.clone())));
        let stop = CancellationToken::new();
        {
            let _cover = locks.try_lock(&series.join("v.WEBP")).unwrap();
            let _marker = locks.try_lock(&series.join("w.NOCOVER")).unwrap();
            assert_eq!(thumbs.scan_once(&lib, &stop), 0);
            let mut names: Vec<String> = std::fs::read_dir(&series)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            assert_eq!(names, ["V.cbz", "W.cbz"]);
        }
        assert_eq!(thumbs.scan_once(&lib, &stop), 1);
        assert!(series.join("V.webp").is_file());
        assert!(series.join("W.nocover").is_file());
    }

    /// A `.nocover` marker older than its archive still stands: the archive is not
    /// looked at again (a volume is replaced by deleting it, which takes the marker).
    #[test]
    fn a_nocover_marker_older_than_its_archive_stands() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        let series = lib.join("Series");
        std::fs::create_dir_all(&series).unwrap();
        let vol = series.join("Vol 1.cbz");
        cbz(&vol, &[("notes.txt", b"x")]);
        let thumbs = Thumbnails::default();
        let stop = CancellationToken::new();
        assert_eq!(thumbs.scan_once(&lib, &stop), 0);
        assert!(marker(&vol).is_file());

        set_mtime(&marker(&vol), mtime(&vol) - Duration::from_secs(600));
        assert!(thumbnail_candidates(&lib).is_empty());
        assert_eq!(thumbs.scan_once(&lib, &stop), 0);
        assert!(marker(&vol).is_file());
        assert!(!series.join("Vol 1.webp").exists());
    }

    /// An archive dated in the future (a fast clock, a copied mtime): its cover, older
    /// than it, is made once and not again on every scan.
    #[test]
    fn an_archive_dated_in_the_future_is_not_covered_again_and_again() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        let series = lib.join("Series");
        std::fs::create_dir_all(&series).unwrap();
        let vol = series.join("Vol 1.cbz");
        cbz(&vol, &[("p.png", &png(500, 700))]);
        set_mtime(
            &vol,
            std::time::SystemTime::now() + Duration::from_secs(3600),
        );
        let thumbs = Thumbnails::default();
        let stop = CancellationToken::new();
        assert_eq!(thumbs.scan_once(&lib, &stop), 1);
        assert_eq!(thumbs.scan_once(&lib, &stop), 0);
        assert!(thumbnail_candidates(&lib).is_empty());
    }

    /// A client's cover landed after the archive was judged coverless: publishing never
    /// overwrites it, whether it is newer or older than the archive.
    #[test]
    fn a_fresh_cover_written_meanwhile_is_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let series = tmp.path().join("Series");
        std::fs::create_dir_all(&series).unwrap();
        let vol = series.join("Vol 1.cbz");
        cbz(&vol, &[("p.png", &png(10, 10))]);
        std::fs::write(cover_of(&vol), b"a client's cover").unwrap();
        set_mtime(&cover_of(&vol), mtime(&vol) + Duration::from_secs(5));
        let webp =
            bunko_thumb::make_thumbnail_limited(&png(10, 10), MAX_DECODE_BYTES, MAX_SIDE).unwrap();
        assert!(!publish_cover(&vol, &webp, &|_| false, &MadeCovers::default()).unwrap());
        assert_eq!(std::fs::read(cover_of(&vol)).unwrap(), b"a client's cover");

        // Older than the archive (uploaded before it): still the client's, kept.
        set_mtime(&cover_of(&vol), mtime(&vol) - Duration::from_secs(5));
        assert!(!publish_cover(&vol, &webp, &|_| false, &MadeCovers::default()).unwrap());
        assert_eq!(std::fs::read(cover_of(&vol)).unwrap(), b"a client's cover");
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

    /// The archive moved away while its cover was being made (a reader renaming the
    /// volume, then its series): the cover is not written beside nothing, where it
    /// would keep the old folder from ever emptying.
    #[test]
    fn a_cover_is_not_published_for_an_archive_gone_meanwhile() {
        let tmp = tempfile::tempdir().unwrap();
        let series = tmp.path().join("Series");
        std::fs::create_dir_all(&series).unwrap();
        let here = series.join("Vol 1.cbz");
        cbz(&here, &[("a.png", &png(10, 10))]);
        let webp =
            bunko_thumb::make_thumbnail_limited(&png(10, 10), MAX_DECODE_BYTES, MAX_SIDE).unwrap();
        assert!(publish_cover(&here, &webp, &|_| false, &MadeCovers::default()).unwrap());
        assert!(cover_of(&here).exists());

        let gone = series.join("Vol 2.cbz");
        assert!(!publish_cover(&gone, &webp, &|_| false, &MadeCovers::default()).unwrap());
        assert!(!cover_of(&gone).exists());
        let names: Vec<_> = std::fs::read_dir(&series)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(!names.iter().any(|n| n.starts_with("Vol 2")), "{names:?}");
    }

    /// A case merge moving the archive (it has just come into the winning spelling's
    /// folder, its cover still in the loser's): no cover is made for it, or the one
    /// the merge moves in next would land beside it as a duplicate; it is handed back
    /// for after the merge.
    #[test]
    fn an_archive_a_case_merge_is_moving_gets_its_cover_after_the_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let series = tmp.path().join("Kingdom");
        std::fs::create_dir_all(&series).unwrap();
        let moving = series.join("Vol 2.cbz");
        let other = series.join("Vol 1.cbz");
        cbz(&moving, &[("a.png", &png(10, 10))]);
        cbz(&other, &[("a.png", &png(10, 10))]);
        let thumbs = Thumbnails::default();
        let stop = CancellationToken::new();

        let busy = |p: &Path| p.ends_with("Vol 2.cbz");
        let (made, deferred) =
            thumbs.make_covers_with(vec![other.clone(), moving.clone()], None, &stop, &busy);
        assert_eq!((made, &deferred), (1, &vec![moving.clone()]));
        assert!(cover_of(&other).exists());
        assert!(!cover_of(&moving).exists());
        assert_eq!(thumbs.pending(), 0);

        // The merge opened while the cover was being made: it is not published.
        let webp =
            bunko_thumb::make_thumbnail_limited(&png(10, 10), MAX_DECODE_BYTES, MAX_SIDE).unwrap();
        assert!(!publish_cover(&moving, &webp, &busy, &MadeCovers::default()).unwrap());
        assert!(!cover_of(&moving).exists());

        // The merge done: the archive gets its cover.
        let (made, deferred) = thumbs.make_covers_with(deferred, None, &stop, &|_| false);
        assert_eq!((made, deferred), (1, vec![]));
        assert!(cover_of(&moving).exists());
    }

    /// A reader's MOVE of the archive lands while its cover is being published: the
    /// move's hook waits for the publish, which sees the archive gone, and no cover is
    /// left under either name. One that lands after the publish takes the cover along.
    #[test]
    fn a_move_landing_mid_publish_leaves_no_cover_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let series = tmp.path().join("Series");
        std::fs::create_dir_all(&series).unwrap();
        let (from, to) = (series.join("Vol 1.cbz"), series.join("Vol 1 renamed.cbz"));
        cbz(&from, &[("a.png", &png(10, 10))]);
        let webp =
            bunko_thumb::make_thumbnail_limited(&png(10, 10), MAX_DECODE_BYTES, MAX_SIDE).unwrap();
        let made = Arc::new(MadeCovers::default());

        let hook = Mutex::new(None);
        let mid_publish = |cbz: &Path| {
            // The archive's last check passed: the MOVE renames it, and its hook runs.
            std::fs::rename(cbz, &to).unwrap();
            let (made, from, to) = (made.clone(), from.clone(), to.clone());
            *hook.lock() = Some(std::thread::spawn(move || made.archive_moved(&from, &to)));
            false
        };
        assert!(!publish_cover(&from, &webp, &mid_publish, &made).unwrap());
        hook.lock().take().unwrap().join().unwrap();
        let names: Vec<_> = std::fs::read_dir(&series)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["Vol 1 renamed.cbz"]);

        // Published, then moved: the cover follows the archive.
        assert!(publish_cover(&to, &webp, &|_| false, &made).unwrap());
        let back = series.join("Vol 1.cbz");
        std::fs::rename(&to, &back).unwrap();
        made.archive_moved(&to, &back);
        let mut names: Vec<_> = std::fs::read_dir(&series)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["Vol 1.cbz", "Vol 1.webp"]);
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// A MOVE or DELETE of the archive lands after the check that follows the publish,
    /// before the cover is recorded: its hook waits for the record, then takes the
    /// cover along (or away). Without the lock the hook would find nothing recorded and
    /// leave the cover beside nothing.
    #[test]
    fn a_move_or_delete_landing_before_the_record_takes_the_cover() {
        for delete in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let series = tmp.path().join("Series");
            std::fs::create_dir_all(&series).unwrap();
            let (from, to) = (series.join("Vol 1.cbz"), series.join("Vol 1 renamed.cbz"));
            cbz(&from, &[("a.png", &png(10, 10))]);
            let webp =
                bunko_thumb::make_thumbnail_limited(&png(10, 10), MAX_DECODE_BYTES, MAX_SIDE)
                    .unwrap();
            let made = Arc::new(MadeCovers::default());
            let hook = Mutex::new(None);
            let before_record = |cbz: &Path| {
                if delete {
                    std::fs::remove_file(cbz).unwrap();
                } else {
                    std::fs::rename(cbz, &to).unwrap();
                }
                let (made, from, to) = (made.clone(), from.clone(), to.clone());
                let (done, finished) = std::sync::mpsc::channel();
                *hook.lock() = Some(std::thread::spawn(move || {
                    if delete {
                        made.archives_removed(&[from]);
                    } else {
                        made.archive_moved(&from, &to);
                    }
                    let _ = done.send(());
                }));
                // Unlocked, the hook would be done by now, before the record.
                let _ = finished.recv_timeout(Duration::from_millis(200));
            };
            assert!(publish_cover_with(&from, &webp, &|_| false, &made, &before_record).unwrap());
            hook.lock().take().unwrap().join().unwrap();
            let want: &[&str] = if delete {
                &[]
            } else {
                &["Vol 1 renamed.cbz", "Vol 1 renamed.webp"]
            };
            assert_eq!(names_in(&series), want, "delete: {delete}");
        }
    }

    /// The archive's arrival under its new name woke the cover worker, which covered
    /// it before the move's hook ran: a move seen in the cloud, whose notice comes only
    /// with its hooks (a WebDAV MOVE's comes before its rename, and the worker waits:
    /// `a_moving_archives_new_name_is_left_to_its_move`; on Windows CI it used to win
    /// that race). The cover that came with the archive replaces that one and still
    /// goes along on the next move; on a cloud library keeping the worker's re-uploaded
    /// the cover as a new node and deleted the moved one (`cloud_bridge_put.rs`). A
    /// cover Bunko did not make at the new name (the reader moved its own first) stays.
    #[test]
    fn a_cover_made_under_the_new_name_before_the_hook_gives_way() {
        let tmp = tempfile::tempdir().unwrap();
        let series = tmp.path().join("Series");
        std::fs::create_dir_all(&series).unwrap();
        let (from, to) = (series.join("Vol 1.cbz"), series.join("Vol 1 renamed.cbz"));
        cbz(&from, &[("a.png", &png(10, 10))]);
        let thumb = |w| {
            bunko_thumb::make_thumbnail_limited(&png(w, 10), MAX_DECODE_BYTES, MAX_SIDE).unwrap()
        };
        let (moving, worker) = (thumb(10), thumb(12));
        assert_ne!(moving, worker);
        let made = MadeCovers::default();
        assert!(publish_cover(&from, &moving, &|_| false, &made).unwrap());
        std::fs::rename(&from, &to).unwrap();
        // The worker runs before the hook.
        assert!(publish_cover(&to, &worker, &|_| false, &made).unwrap());
        made.archive_moved(&from, &to);
        assert_eq!(
            names_in(&series),
            ["Vol 1 renamed.cbz", "Vol 1 renamed.webp"]
        );
        assert_eq!(
            std::fs::read(cover_path(&to).unwrap()).unwrap(),
            moving,
            "the moved cover"
        );
        let back = series.join("Vol 1.cbz");
        std::fs::rename(&to, &back).unwrap();
        made.archive_moved(&to, &back);
        assert_eq!(names_in(&series), ["Vol 1.cbz", "Vol 1.webp"]);
        assert_eq!(std::fs::read(cover_path(&back).unwrap()).unwrap(), moving);

        // The reader's own cover at the new name wins over the one Bunko made.
        std::fs::write(
            bunko_library::sidecar::with_suffix(&to, ".webp"),
            b"the reader's",
        )
        .unwrap();
        std::fs::rename(&back, &to).unwrap();
        made.archive_moved(&back, &to);
        assert_eq!(
            names_in(&series),
            ["Vol 1 renamed.cbz", "Vol 1 renamed.webp"]
        );
        assert_eq!(
            std::fs::read(cover_path(&to).unwrap()).unwrap(),
            b"the reader's"
        );
    }

    /// The move's hook takes the new name's cover under the WebDAV write lock of that
    /// name, in any spelling, as a published cover is: while a client writes it
    /// (`VOL 1 RENAMED.WEBP` locked), neither the cover Bunko made there gives way nor
    /// the moving one lands there; the client's write wins and the made cover goes.
    #[test]
    fn a_moves_cover_waits_for_no_client_writing_the_new_name() {
        let tmp = tempfile::tempdir().unwrap();
        let series = tmp.path().join("Series");
        std::fs::create_dir_all(&series).unwrap();
        let (from, to) = (series.join("Vol 1.cbz"), series.join("Vol 1 renamed.cbz"));
        cbz(&from, &[("a.png", &png(10, 10))]);
        let (moving, worker) = (thumb(10), thumb(12));
        let locks = bunko_dav::PathWriteLocks::new();
        let made = MadeCovers::default();
        let _ = made
            .2
            .set(Arc::new(crate::ocr::types::DavLocks(locks.clone())));
        assert!(publish_cover(&from, &moving, &|_| false, &made).unwrap());
        std::fs::rename(&from, &to).unwrap();
        assert!(publish_cover(&to, &worker, &|_| false, &made).unwrap());
        {
            let _client = locks.try_lock(&series.join("VOL 1 RENAMED.WEBP")).unwrap();
            made.archive_moved(&from, &to);
        }
        assert_eq!(
            names_in(&series),
            ["Vol 1 renamed.cbz", "Vol 1 renamed.webp"]
        );
        assert_eq!(
            std::fs::read(cover_path(&to).unwrap()).unwrap(),
            worker,
            "nothing at the locked name was touched"
        );
    }

    fn thumb(w: u32) -> Vec<u8> {
        bunko_thumb::make_thumbnail_limited(&png(w, 10), MAX_DECODE_BYTES, MAX_SIDE).unwrap()
    }

    /// A MOVE's notice keeps the cover worker off the archive's new name until the move
    /// is over: the store has the archive there before the move's hook runs (and may
    /// have announced it, waking the worker), and a cover made then would meet the one
    /// moving along. Windows CI showed it: the worker covered the new name first, and
    /// on a cloud library the moved cover's node was deleted for a new upload. The
    /// worker is sent back to a name it passed over once the notice is withdrawn,
    /// whether the hook ran or the rename failed; never for one it did not come for. A
    /// notice somehow never withdrawn lapses.
    #[test]
    fn a_moving_archives_new_name_is_left_to_its_move() {
        let tmp = tempfile::tempdir().unwrap();
        let series = tmp.path().join("Series");
        std::fs::create_dir_all(&series).unwrap();
        let (from, to) = (series.join("Vol 1.cbz"), series.join("Vol 1 renamed.cbz"));
        cbz(&from, &[("a.png", &png(10, 10))]);
        let (moving, worker) = (thumb(10), thumb(12));
        let made = Arc::new(MadeCovers::default());
        let again = Arc::new(Mutex::new(Vec::new()));
        *made.1.again.lock() = Some({
            let again = again.clone();
            Arc::new(move |p: PathBuf| again.lock().push(p))
        });
        assert!(publish_cover(&from, &moving, &|_| false, &made).unwrap());

        let notice = made.moving(&to);
        std::fs::rename(&from, &to).unwrap();
        // The worker, woken by the arrival, passes the new name over.
        assert!(!publish_cover(&to, &worker, &|_| false, &made).unwrap());
        assert!(!ensure_cover(&to, None, &|_| false, &made).unwrap());
        assert_eq!(names_in(&series), ["Vol 1 renamed.cbz", "Vol 1.webp"]);
        made.archive_moved(&from, &to);
        drop(notice);
        assert_eq!(
            names_in(&series),
            ["Vol 1 renamed.cbz", "Vol 1 renamed.webp"]
        );
        assert_eq!(
            std::fs::read(cover_path(&to).unwrap()).unwrap(),
            moving,
            "moved"
        );
        // Sent back once: the cover came with the archive, and stays.
        assert_eq!(*again.lock(), std::slice::from_ref(&to));
        assert!(!publish_cover(&to, &worker, &|_| false, &made).unwrap());
        assert_eq!(std::fs::read(cover_path(&to).unwrap()).unwrap(), moving);

        // A rename that failed drops its notice without the hook: sent back, the
        // worker covers the name.
        let other = series.join("Vol 2.cbz");
        cbz(&other, &[("a.png", &png(10, 10))]);
        let notice = made.moving(&other);
        assert!(!publish_cover(&other, &worker, &|_| false, &made).unwrap());
        drop(notice);
        assert_eq!(again.lock().last(), Some(&other));
        assert!(publish_cover(&other, &worker, &|_| false, &made).unwrap());

        // A notice the worker never met sends it nowhere.
        again.lock().clear();
        drop(made.moving(&series.join("Vol 3.cbz")));
        assert!(again.lock().is_empty());

        // A notice never withdrawn lapses.
        let third = series.join("Vol 3.cbz");
        cbz(&third, &[("a.png", &png(10, 10))]);
        let _kept = made.moving(&third);
        let long_ago = Instant::now()
            .checked_sub(MOVING_KEPT + Duration::from_secs(1))
            .expect("the clock goes back that far");
        for n in made.1.names.lock().values_mut() {
            n.since = long_ago;
        }
        assert!(publish_cover(&third, &worker, &|_| false, &made).unwrap());
    }

    /// The hooks' notice: a WebDAV MOVE's is up from `archive_moving` until its guard
    /// goes. A move seen in the cloud is followed up later on another thread: its notice
    /// goes up as the store's events thread hands the hook over (without waiting for
    /// the cover lock) and comes down once the follow-up ran.
    #[test]
    fn a_moves_notice_lasts_until_its_cover_hook_ran() {
        use bunko_dav::DavHooks as _;
        let tmp = tempfile::tempdir().unwrap();
        let series = tmp.path().join("Series");
        std::fs::create_dir_all(&series).unwrap();
        let (from, to) = (series.join("Vol 1.cbz"), series.join("Vol 1 renamed.cbz"));
        cbz(&from, &[("a.png", &png(10, 10))]);
        let thumbs = Arc::new(Thumbnails::default());
        assert!(publish_cover(&from, &thumb(10), &|_| false, &thumbs.made).unwrap());

        let notice = thumbs.archive_moving(&from, &to);
        assert!(thumbs.made.1.passes_over(&to));
        drop(notice);
        assert!(!thumbs.made.1.passes_over(&to));

        std::fs::rename(&from, &to).unwrap();
        {
            // Held: the follow-up cannot run yet.
            let _held = thumbs.made.0.lock();
            let (t, f, d) = (thumbs.clone(), from.clone(), to.clone());
            let (done, handed) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name(bunko_remote::store::EVENTS_THREAD.into())
                .spawn(move || {
                    t.archive_moved(&f, &d);
                    let _ = done.send(());
                })
                .unwrap();
            handed
                .recv_timeout(Duration::from_secs(5))
                .expect("the events thread does not wait for a cover");
            assert!(thumbs.made.1.passes_over(&to), "left to the follow-up");
        }
        thumbs.follow.flush();
        assert!(!thumbs.made.1.passes_over(&to), "withdrawn after it");
        assert_eq!(
            names_in(&series),
            ["Vol 1 renamed.cbz", "Vol 1 renamed.webp"]
        );
    }

    /// A cloud library over the in-memory provider, mounted, its first sync done.
    /// Deferred writes leave while `open` holds.
    struct Cloud {
        root: PathBuf,
        fake: Arc<bunko_remote::fake::FakeBackend>,
        store: Arc<bunko_remote::RemoteStore<bunko_remote::fake::FakeBackend>>,
        open: Arc<std::sync::atomic::AtomicBool>,
        _mount: bunko_store::MountGuard,
        _tmp: tempfile::TempDir,
    }

    const FOLDER: &str = "mokuro-reader";

    fn cloud(files: &[(&str, &[u8])]) -> Cloud {
        let fake = bunko_remote::fake::FakeBackend::drive_like();
        fake.reader_mkdir(FOLDER);
        for (rel, bytes) in files {
            fake.reader_put(&format!("{FOLDER}/{rel}"), bytes);
        }
        let fake = Arc::new(fake);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cloud/s/library");
        let mut cfg =
            bunko_remote::RemoteStoreConfig::new(&root, tmp.path().join("cloud/s"), FOLDER);
        cfg.poll_active = Duration::from_secs(3600);
        cfg.poll_idle = Duration::from_secs(3600);
        cfg.lease = false;
        cfg.retry_base = Duration::from_millis(20);
        cfg.mount_id = 7;
        let store = bunko_remote::RemoteStore::open_shared(cfg, fake.clone()).unwrap();
        let open = Arc::new(std::sync::atomic::AtomicBool::new(true));
        {
            let open = open.clone();
            store.set_lease_gate(Arc::new(move || open.load(Ordering::Acquire)));
        }
        let mount = bunko_store::mount(store.clone() as Arc<dyn bunko_store::LibraryStore>);
        assert!(store.wait_steady(Duration::from_secs(20)));
        Cloud {
            root,
            fake,
            store,
            open,
            _mount: mount,
            _tmp: tmp,
        }
    }

    impl Cloud {
        fn node(&self, rel: &str) -> Option<bunko_remote::RNode> {
            self.fake.node_at(&format!("{FOLDER}/{rel}"))
        }

        fn idle(&self) {
            assert!(
                self.store.wait_outbox_idle(Duration::from_secs(20)),
                "{:?}",
                self.store.outbox()
            );
        }
    }

    /// Holds the provider's uploads at their start until opened (or dropped);
    /// `entered` hears of each one that started.
    struct UploadGate {
        shut: Arc<(Mutex<bool>, parking_lot::Condvar)>,
        entered: std::sync::mpsc::Receiver<()>,
    }

    impl UploadGate {
        fn shut(fake: &bunko_remote::fake::FakeBackend) -> UploadGate {
            let shut = Arc::new((Mutex::new(true), parking_lot::Condvar::new()));
            let (tx, entered) = std::sync::mpsc::channel();
            let (tx, held) = (Mutex::new(tx), shut.clone());
            fake.set_call_hook(Some(Arc::new(move |op| {
                if op != bunko_remote::fake::FakeOp::Upload {
                    return;
                }
                let _ = tx.lock().send(());
                let mut shut = held.0.lock();
                while *shut {
                    held.1.wait(&mut shut);
                }
            })));
            UploadGate { shut, entered }
        }

        fn open(&self) {
            *self.shut.0.lock() = false;
            self.shut.1.notify_all();
        }
    }

    impl Drop for UploadGate {
        fn drop(&mut self) {
            self.open();
        }
    }

    /// On a cloud library the cover worker covered the archive's new name before the
    /// move's hook ran (a move seen in the cloud; or the cover of a volume a MOVE then
    /// replaced), and that cover is still staged, on its way to the provider (the
    /// worker took it and the upload has begun), or landed: in each case it gives way
    /// and the moved cover's node takes the name, moved by id. The cover on its way was
    /// the Windows CI failure's last form (e97faa0's fix covered the other two): the
    /// store's remove left it pending, so the hook took the name for taken and deleted
    /// the moved node, and the worker's upload then landed as a new one.
    #[test]
    fn a_cover_bunko_made_under_the_new_name_gives_way_on_a_cloud_library() {
        for state in ["landed", "staged", "on its way"] {
            let c = cloud(&[("Series/Vol 1.cbz", b"an archive")]);
            let (from, to) = (
                c.root.join("Series/Vol 1.cbz"),
                c.root.join("Series/Vol 1 renamed.cbz"),
            );
            let (moving, worker) = (thumb(10), thumb(12));
            let made = Arc::new(MadeCovers::default());
            assert!(publish_cover(&from, &moving, &|_| false, &made).unwrap());
            c.idle();
            let cover = c.node("Series/Vol 1.webp").unwrap().pid;
            fs::rename(&from, &to).unwrap();
            let gate = (state == "on its way").then(|| UploadGate::shut(&c.fake));
            if state == "staged" {
                c.open.store(false, Ordering::Release);
            }
            // The worker covers the new name before the hook runs.
            assert!(publish_cover(&to, &worker, &|_| false, &made).unwrap());
            match &gate {
                Some(g) => g.entered.recv_timeout(Duration::from_secs(20)).unwrap(),
                None if state == "landed" => c.idle(),
                None => {}
            }
            let (tx, rx) = std::sync::mpsc::channel::<&str>();
            let settle = Mutex::new(tx.clone());
            c.store.set_failpoint(Some(Arc::new(move |at: &str| {
                if at == "settle" {
                    let _ = settle.lock().send("settle");
                }
                false
            })));
            let hook = {
                let (made, from, to) = (made.clone(), from.clone(), to.clone());
                std::thread::spawn(move || {
                    made.archive_moved(&from, &to);
                    let _ = tx.send("done");
                })
            };
            if let Some(g) = &gate {
                // The hook waits for the upload on its way before it frees the name.
                let first = rx.recv_timeout(Duration::from_secs(20)).unwrap();
                assert_eq!(first, "settle", "{state}");
                g.open();
            }
            hook.join().unwrap();
            c.open.store(true, Ordering::Release);
            c.idle();
            let named = c.node("Series/Vol 1 renamed.webp");
            assert_eq!(
                named.as_ref().map(|n| &n.pid),
                Some(&cover),
                "{state}: the moved cover's node"
            );
            assert_eq!(c.fake.content(&cover), Some(moving.clone()), "{state}");
            assert!(c.node("Series/Vol 1.webp").is_none(), "{state}");
            let uploads = c.fake.upload_log();
            if state == "staged" {
                assert_eq!(uploads, ["Vol 1.webp"], "never sent");
            } else {
                assert_eq!(uploads, ["Vol 1.webp", "Vol 1 renamed.webp"], "{state}");
                assert_eq!(c.fake.deleted_log().len(), 1, "{state}: the worker's went");
            }
            assert_eq!(
                fs::read(cover_path(&to).unwrap()).unwrap(),
                moving,
                "{state}: as the library reads it"
            );
        }
    }

    /// A folder removed or moved away is no archive: the cover made for a loose archive
    /// of the folder's name beside it (`Series.cbz` next to `Series/`) stays.
    #[test]
    fn a_folder_removed_leaves_the_cover_of_an_archive_named_like_it() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        std::fs::create_dir_all(lib.join("Series")).unwrap();
        std::fs::create_dir_all(lib.join("Vol.1")).unwrap();
        let loose = lib.join("Series.cbz");
        let dotted = lib.join("Vol.cbz");
        cbz(&loose, &[("a.png", &png(10, 10))]);
        cbz(&dotted, &[("a.png", &png(10, 10))]);
        let webp =
            bunko_thumb::make_thumbnail_limited(&png(10, 10), MAX_DECODE_BYTES, MAX_SIDE).unwrap();
        let made = MadeCovers::default();
        assert!(publish_cover(&loose, &webp, &|_| false, &made).unwrap());
        assert!(publish_cover(&dotted, &webp, &|_| false, &made).unwrap());
        std::fs::remove_dir_all(lib.join("Series")).unwrap();
        std::fs::remove_dir_all(lib.join("Vol.1")).unwrap();
        made.archives_removed(&[lib.join("Series"), lib.join("Vol.1")]);
        assert_eq!(
            names_in(&lib),
            ["Series.cbz", "Series.webp", "Vol.cbz", "Vol.webp"]
        );
    }

    /// A case-only rename of an archive (`vol 1.cbz` to `Vol 1.cbz`) takes the cover
    /// Bunko made along, respelled like it, never removed: the same listing on every
    /// host. A move reported through another path form of the library (a symlinked
    /// root; macOS `/private/var`, Windows `\\?\`) finds the record too.
    #[test]
    fn a_case_only_rename_respells_the_cover_made_for_it() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let lib = real.join("library");
        std::fs::create_dir_all(lib.join("S")).unwrap();
        let from = lib.join("S/vol 1.cbz");
        cbz(&from, &[("a.png", &png(10, 10))]);
        let webp =
            bunko_thumb::make_thumbnail_limited(&png(10, 10), MAX_DECODE_BYTES, MAX_SIDE).unwrap();
        let made = MadeCovers::default();
        assert!(publish_cover(&from, &webp, &|_| false, &made).unwrap());
        // The hooks report canonical paths.
        let canon = std::fs::canonicalize(&lib).unwrap();
        let to = canon.join("S/Vol 1.cbz");
        bunko_store::fs::rename(canon.join("S/vol 1.cbz"), &to).unwrap();
        made.archive_moved(&canon.join("S/vol 1.cbz"), &to);
        assert_eq!(names_in(&lib.join("S")), ["Vol 1.cbz", "Vol 1.webp"]);
        // And it goes along again, under its new name.
        let renamed = canon.join("S/Vol 2.cbz");
        bunko_store::fs::rename(&to, &renamed).unwrap();
        made.archive_moved(&to, &renamed);
        assert_eq!(names_in(&lib.join("S")), ["Vol 2.cbz", "Vol 2.webp"]);
    }

    /// An archive Bunko made a cover for moves onto a name whose cover is a hidden case
    /// variant's own (`v.webp` beside the hidden `v.cbz`): the made cover goes, and no
    /// second spelling (`V.webp`) is written beside the hidden one's. Where the disk
    /// keeps one spelling, `v.webp` is the moved archive's own and stays as it was.
    #[test]
    fn a_moved_archive_never_gets_a_cover_beside_a_hidden_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(lib.join("S")).unwrap();
        std::fs::create_dir_all(lib.join("A")).unwrap();
        std::fs::write(lib.join("S/v.cbz"), b"hidden").unwrap();
        std::fs::write(lib.join("S/v.webp"), b"hidden cover").unwrap();
        let from = lib.join("A/Vol.cbz");
        cbz(&from, &[("a.png", &png(10, 10))]);
        let webp =
            bunko_thumb::make_thumbnail_limited(&png(10, 10), MAX_DECODE_BYTES, MAX_SIDE).unwrap();
        let made = MadeCovers::default();
        assert!(publish_cover(&from, &webp, &|_| false, &made).unwrap());
        let to = lib.join("S/V.cbz");
        let two = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&to)
            .is_ok();
        let two = two && names_in(&lib.join("S")).contains(&"v.cbz".to_string());
        if two {
            std::fs::remove_file(&to).unwrap();
            bunko_store::fs::rename(&from, &to).unwrap();
        } else {
            std::fs::remove_file(lib.join("S/v.cbz")).unwrap();
            bunko_store::fs::rename(&from, &to).unwrap();
        }
        made.archive_moved(&from, &to);
        if two {
            assert_eq!(names_in(&lib.join("S")), ["V.cbz", "v.cbz", "v.webp"]);
        } else {
            assert_eq!(names_in(&lib.join("S")), ["V.cbz", "v.webp"]);
        }
        assert_eq!(
            std::fs::read(lib.join("S/v.webp")).unwrap(),
            b"hidden cover"
        );
        assert_eq!(names_in(&lib.join("A")), Vec::<String>::new());
    }

    /// The record of covers made is pruned as covers are made too, not only when an
    /// archive moves: a first scan of a large library does not keep one entry per
    /// volume for good.
    #[test]
    fn old_cover_records_are_pruned_as_covers_are_made() {
        let tmp = tempfile::tempdir().unwrap();
        let cbz_path = tmp.path().join("Vol 1.cbz");
        cbz(&cbz_path, &[("a.png", &png(10, 10))]);
        let webp =
            bunko_thumb::make_thumbnail_limited(&png(10, 10), MAX_DECODE_BYTES, MAX_SIDE).unwrap();
        let made = MadeCovers::default();
        let long_ago = Instant::now()
            .checked_sub(MADE_KEPT + Duration::from_secs(1))
            .expect("the clock goes back that far");
        made.0
            .lock()
            .insert(tmp.path().join("Old.webp"), (3, long_ago));
        assert!(publish_cover(&cbz_path, &webp, &|_| false, &made).unwrap());
        let kept: Vec<PathBuf> = made.0.lock().keys().cloned().collect();
        assert_eq!(kept, [MadeCovers::key(&cover_of(&cbz_path))]);
    }

    /// The cloud's hooks (on its events thread) hand a MOVE's two follow-ups over in
    /// order, and they run in that order: the cover follows the archive, never removed
    /// by the "archive gone" step first. The events thread never waits for the lock.
    #[test]
    fn the_cloud_hooks_follow_ups_run_in_order() {
        use bunko_dav::DavHooks as _;
        for round in 0..10 {
            let tmp = tempfile::tempdir().unwrap();
            let series = tmp.path().join("Series");
            std::fs::create_dir_all(&series).unwrap();
            let (from, to) = (series.join("Vol 1.cbz"), series.join("Vol 1 renamed.cbz"));
            cbz(&from, &[("a.png", &png(10, 10))]);
            let webp =
                bunko_thumb::make_thumbnail_limited(&png(10, 10), MAX_DECODE_BYTES, MAX_SIDE)
                    .unwrap();
            let thumbs = Arc::new(Thumbnails::default());
            assert!(publish_cover(&from, &webp, &|_| false, &thumbs.made).unwrap());
            let cover = std::fs::read(cover_of(&from)).unwrap();
            std::fs::rename(&from, &to).unwrap();
            {
                // Held: both follow-ups are handed over before either can run.
                let _held = thumbs.made.0.lock();
                let (t, from, to) = (thumbs.clone(), from.clone(), to.clone());
                let (done, handed) = std::sync::mpsc::channel();
                std::thread::Builder::new()
                    .name(bunko_remote::store::EVENTS_THREAD.into())
                    .spawn(move || {
                        t.archive_moved(&from, &to);
                        t.archives_removed(&[from]);
                        let _ = done.send(());
                    })
                    .unwrap();
                handed
                    .recv_timeout(Duration::from_secs(5))
                    .expect("the events thread does not wait for a cover");
            }
            thumbs.follow.flush();
            assert_eq!(
                names_in(&series),
                ["Vol 1 renamed.cbz", "Vol 1 renamed.webp"],
                "round {round}"
            );
            assert_eq!(std::fs::read(cover_of(&to)).unwrap(), cover, "followed");
        }
    }

    /// Anywhere but on the events thread (a WebDAV request) the follow-up is done when
    /// the hook returns, so the MOVE answers with the cover already in place.
    #[test]
    fn a_dav_hook_follows_up_before_it_returns() {
        use bunko_dav::DavHooks as _;
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("Vol 1.cbz"), tmp.path().join("Vol 2.cbz"));
        cbz(&from, &[("a.png", &png(10, 10))]);
        let webp =
            bunko_thumb::make_thumbnail_limited(&png(10, 10), MAX_DECODE_BYTES, MAX_SIDE).unwrap();
        let thumbs = Thumbnails::default();
        assert!(publish_cover(&from, &webp, &|_| false, &thumbs.made).unwrap());
        std::fs::rename(&from, &to).unwrap();
        thumbs.archive_moved(&from, &to);
        thumbs.archives_removed(&[from]);
        assert_eq!(names_in(tmp.path()), ["Vol 2.cbz", "Vol 2.webp"]);
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

    /// The per-user tree at the root (a cloud progress user's `history/`) is no volume:
    /// no cover is written into it. A series folder's own `history/` still is one.
    #[test]
    fn the_per_user_tree_gets_no_covers() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        std::fs::create_dir_all(lib.join("history/dev-1")).unwrap();
        std::fs::create_dir_all(lib.join("Series/history")).unwrap();
        cbz(&lib.join("history/dev-1/x.cbz"), &[("p.png", &png(20, 30))]);
        cbz(
            &lib.join("Series/history/v.cbz"),
            &[("p.png", &png(20, 30))],
        );
        assert_eq!(
            thumbnail_candidates(&lib),
            vec![lib.join("Series/history/v.cbz")]
        );
        // A cloud library's walk of an arrived folder knows the library from its records.
        let nocover = NoCoverSet::load(tmp.path().join("nocover.json"), lib.clone());
        assert!(thumbnail_candidates_with(&lib.join("history"), Some(&nocover)).is_empty());
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

    /// Regression: a volume whose pages are all JPEG XL got a `.nocover` marker
    /// ("no image") instead of a cover.
    #[test]
    fn jxl_volumes_get_covers() {
        let page = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../bunko-ocr/tests/fixtures/jxl/page-rgb.jxl"),
        )
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        let series = lib.join("Series");
        std::fs::create_dir_all(&series).unwrap();
        cbz(
            &series.join("Vol 1.cbz"),
            &[("v/002.jxl", &page), ("v/001.jxl", &page)],
        );
        let thumbs = Thumbnails::default();
        assert_eq!(thumbs.scan_once(&lib, &CancellationToken::new()), 1);
        assert!(!series.join("Vol 1.nocover").exists());
        let img =
            image::load_from_memory(&std::fs::read(series.join("Vol 1.webp")).unwrap()).unwrap();
        assert_eq!(
            (img.width(), img.height()),
            bunko_thumb::contain_size(24, 16, bunko_thumb::THUMBNAIL_BOX)
        );
    }

    /// Upgrade: 0.7.4 and older could not read JPEG XL, so they marked a volume of JPEG
    /// XL pages `.nocover` ("no image"), and the marker stops every later look. Those
    /// markers go; the others stay.
    #[test]
    fn jxl_nocover_markers_are_cleared() {
        let page = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../bunko-ocr/tests/fixtures/jxl/page-rgb.jxl"),
        )
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        let series = lib.join("Series");
        std::fs::create_dir_all(&series).unwrap();
        cbz(&series.join("Vol 1.cbz"), &[("v/001.jxl", &page)]);
        cbz(&series.join("Vol 2.CBZ"), &[("v/001.JXL", &page)]);
        cbz(&series.join("Vol 3.cbz"), &[("notes.txt", b"x")]);
        std::fs::write(series.join("Vol 4.cbz"), b"not a zip").unwrap();
        for stem in ["Vol 1", "Vol 2", "Vol 3", "Vol 4", "Orphan"] {
            std::fs::write(series.join(format!("{stem}.nocover")), b"").unwrap();
        }
        assert_eq!(clear_jxl_nocovers(&lib), 2);
        assert!(!series.join("Vol 1.nocover").exists());
        assert!(!series.join("Vol 2.nocover").exists());
        for stem in ["Vol 3", "Vol 4", "Orphan"] {
            assert!(series.join(format!("{stem}.nocover")).is_file(), "{stem}");
        }
        let thumbs = Thumbnails::default();
        assert_eq!(thumbs.scan_once(&lib, &CancellationToken::new()), 2);
        assert!(series.join("Vol 1.webp").is_file());
        assert!(series.join("Vol 2.webp").is_file());
        assert_eq!(clear_jxl_nocovers(&lib), 0);
    }

    /// The marker cleared is the one the folder lists in any spelling (`V.NOCOVER`),
    /// and the cover made after it is that volume's only cover: no second spelling.
    #[test]
    fn jxl_nocover_markers_are_cleared_in_any_spelling() {
        let page = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../bunko-ocr/tests/fixtures/jxl/page-rgb.jxl"),
        )
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        let series = lib.join("Series");
        std::fs::create_dir_all(&series).unwrap();
        cbz(&series.join("Vol 1.cbz"), &[("v/001.jxl", &page)]);
        std::fs::write(series.join("Vol 1.NOCOVER"), b"").unwrap();
        assert_eq!(clear_jxl_nocovers(&lib), 1);
        let thumbs = Thumbnails::default();
        assert_eq!(thumbs.scan_once(&lib, &CancellationToken::new()), 1);
        let mut names: Vec<String> = std::fs::read_dir(&series)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["Vol 1.cbz", "Vol 1.webp"]);
    }

    /// A record kept under another spelling of its archive's name matches no archive:
    /// it is not taken for the volume listed now (whose cover pass already looks).
    #[test]
    fn a_jxl_nocover_record_of_another_spelling_is_left() {
        let page = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../bunko-ocr/tests/fixtures/jxl/page-rgb.jxl"),
        )
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        let series = lib.join("S");
        std::fs::create_dir_all(&series).unwrap();
        let file = tmp.path().join("nocover.json");
        let old = series.join("old.cbz");
        cbz(&old, &[("v/001.jxl", &page)]);
        let earlier = NoCoverSet::load(file.clone(), lib.clone());
        earlier.insert(&old).unwrap();
        drop(earlier);
        std::fs::remove_file(&old).unwrap();
        cbz(&series.join("OLD.cbz"), &[("v/001.jxl", &page)]);
        let set = NoCoverSet::load(file, lib);
        assert!(set.clear_jxl().is_empty());
        assert_eq!(set.len(), 1);
    }

    /// [`NoCoverSet::clear_jxl`] looks at each record an earlier run kept once its
    /// archive is in the library: a record whose archive is not there yet (a cloud
    /// store still syncing) waits for a later pass, and a record this run wrote is
    /// its own verdict.
    #[test]
    fn jxl_nocover_records_wait_for_their_archive() {
        let page = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../bunko-ocr/tests/fixtures/jxl/page-rgb.jxl"),
        )
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        let series = lib.join("S");
        std::fs::create_dir_all(&series).unwrap();
        let file = tmp.path().join("nocover.json");
        let (old, plain, late) = (
            series.join("old.cbz"),
            series.join("plain.cbz"),
            series.join("late.cbz"),
        );
        cbz(&old, &[("v/001.jxl", &page)]);
        cbz(&plain, &[("notes.txt", b"x")]);
        cbz(&late, &[("v/001.jxl", &page)]);
        let earlier = NoCoverSet::load(file.clone(), lib.clone());
        for a in [&old, &plain, &late] {
            earlier.insert(a).unwrap();
        }
        drop(earlier);
        std::fs::remove_file(&late).unwrap();

        let set = NoCoverSet::load(file.clone(), lib.clone());
        assert_eq!(set.clear_jxl(), std::slice::from_ref(&old));
        assert_eq!(set.len(), 2);
        assert!(
            set.clear_jxl().is_empty(),
            "the late archive is not there yet"
        );
        cbz(&late, &[("v/001.jxl", &page)]);
        assert_eq!(set.clear_jxl(), std::slice::from_ref(&late));
        assert_eq!(set.len(), 1);
        assert!(set.contains(&plain));
        // This run found no cover in `old` either: that record stays.
        set.insert(&old).unwrap();
        assert!(set.clear_jxl().is_empty());
        assert!(set.contains(&old));
        assert_eq!(NoCoverSet::load(file, lib).len(), 2, "saved");
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

    /// An uploaded sidecar of 100k nested `[` holding a `NaN` (which serde refuses and
    /// Python allows) went on to the layout parser, which recursed per level: the sweep
    /// at every start overflowed the stack and aborted the server, until the file was
    /// removed by hand. Now it is removed as corrupt; a moderately nested one with a
    /// `NaN` is kept. Run on a 2 MiB stack, a tokio blocking thread's.
    #[test]
    fn deeply_nested_sidecars_are_removed_not_a_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        std::fs::create_dir_all(lib.join("S")).unwrap();
        let nest = |depth: usize| format!("{}NaN{}", "[".repeat(depth), "]".repeat(depth));
        std::fs::write(lib.join("S/deep.mokuro"), nest(100_000)).unwrap();
        std::fs::write(lib.join("S/nested.mokuro"), nest(200)).unwrap();
        let lib2 = lib.clone();
        let removed = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || remove_corrupt_sidecars(&lib2, None))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(removed, 1);
        assert!(!lib.join("S/deep.mokuro").exists());
        assert!(lib.join("S/nested.mokuro").is_file());
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
