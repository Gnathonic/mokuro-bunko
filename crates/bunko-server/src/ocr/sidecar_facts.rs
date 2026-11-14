//! What the OCR census needs of a sidecar file's bytes ([`SidecarFacts`]: its recipe
//! block, its revision, its pages, its seal), read once per version of the file, ever.
//!
//! The census ([`super::upgrade`]) asks every walk whether each volume's main sidecar is
//! outdated and whether each generation layer is Bunko's own output at an older
//! revision. Its verdicts are kept in memory against each file's stat, but they depend
//! on the settings (a settings change clears them) and die with the process, while what
//! they are made from depends only on the file's bytes. On a cloud library every read is
//! a provider download, and a large library has more sidecar bytes than the store's
//! small-file cache holds: each walk after a start or a settings change downloaded every
//! sidecar again. The facts are kept here instead, in `mokuro.db`'s `sidecar_facts`
//! ([`bunko_db::sidecar_facts`]), keyed by the file's library-relative path and valid for
//! exactly the version they were read at: its size, its mtime in ns and the store's
//! content version (a cloud store's `cver`; a local file has none, and its size and mtime
//! pin it, as everywhere else in the census, once it is older than [`RACY`]). Any other
//! version of the file (a reader's replace, MEGA's delete-then-upload, a write of Bunko's
//! own) is read again, once. The facts are stored only when the file still is the
//! version read after the read (a read racing a write is used once and not kept), never
//! for an I/O error (the network), and never for a cloud file the store gives no
//! version.
//!
//! A version names a file's bytes only on a store whose versions are content versions
//! ([`bunko_store::Caps::content_versions`]: Drive's `md5Checksum`, OneDrive's `cTag`,
//! MEGA's node handle). There a file renamed or moved keeps its version, and its new
//! name finds the facts by it. Elsewhere the version is the file's mtime and size, or a
//! host's ETag made of the same (WebDAV, a connected local folder), and a local file has
//! none: two files can share one, so another path's facts are never taken (a new name
//! is read once), and a rewrite at the same size within one tick of the clock keeps it,
//! so a file written less than [`RACY`] ago is used, never kept (git's racy files).
//!
//! The facts are a hint, never a proof: every step that deletes, replaces or promotes a
//! file reads the bytes it acts on again, under the file's write lock, and decides on
//! those ([`SidecarFactsCache::fresh`]), keeping what it read in place of what was kept.
//! Rows of files that are gone or changed are pruned (never while the library folder
//! itself is missing).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bunko_db::{Database, SidecarFactsKey};
use bunko_library::pyjson::{self, JsonNum, JsonObject, JsonValue};
use bunko_store::{Meta, Version, fs};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;

use super::types::{mtime_ns, rel_of};

/// The shape of the stored facts: a build that reads more (or otherwise) bumps it, and
/// every row of another format is read again.
pub const FACTS_FORMAT: u32 = 1;

/// What the census reads off a sidecar's bytes. Pure: two files with the same bytes have
/// the same facts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SidecarFacts {
    /// The bytes as `load_sidecar` reads them (UTF-8, BOM stripped, Python's JSON), when
    /// they are a JSON object.
    pub object: Option<ObjectFacts>,
    /// The bytes as `serde_json` reads them (BOM stripped), when they parse.
    pub json: Option<JsonFacts>,
    /// The content seal checks ([`bunko_layout::seal::is_sealed`]).
    pub sealed: bool,
}

/// [`SidecarFacts::object`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ObjectFacts {
    /// An `edited` object.
    pub edited: bool,
    /// The `ocr_engine` object.
    pub block: Option<BlockFacts>,
    /// A non-empty string `version` (mokuro's own).
    pub version: bool,
    /// The `pages` array.
    pub pages: Option<PageFacts>,
}

/// The `ocr_engine` object of [`ObjectFacts`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BlockFacts {
    /// A string `id`.
    pub id: Option<String>,
    /// A string `detector`.
    pub detector: Option<String>,
    /// A number `patch_budget`, as `u32` (`as f64 as u32`).
    pub patch_budget: Option<u32>,
    /// `revision`: a non-negative integer, else 1.
    pub revision: u32,
    /// `upgraded_from_primary` is truthy (a layer an earlier beta's upgrade kept).
    pub upgraded_from_primary: bool,
}

/// The `pages` array of [`ObjectFacts`]: enough to tell whether they are the pages of
/// an archive ([`PageFacts::name_the_images_of`]) without keeping every name.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PageFacts {
    pub count: usize,
    /// Every page is an object with a string `img_path`.
    pub all_named: bool,
    /// How many distinct `img_path`s.
    pub distinct: usize,
    /// SHA-256 over the distinct `img_path`s, sorted, each followed by a NUL.
    pub digest: String,
    /// The distinct `img_path`s themselves, kept only when some name repeats.
    pub repeated: Option<Vec<String>>,
}

/// [`SidecarFacts::json`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct JsonFacts {
    /// An `edited` key, whatever its value.
    pub edited: bool,
    /// `(ocr_engine.id, revision)` ([`super::upgrade::engine_of`]).
    pub engine: Option<(String, u32)>,
}

fn names_digest<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let mut h = sha2::Sha256::new();
    for n in names {
        h.update(n.as_bytes());
        h.update([0u8]);
    }
    hex::encode(h.finalize())
}

impl PageFacts {
    fn of(pages: &[JsonValue]) -> PageFacts {
        let named: Vec<Option<&str>> = pages
            .iter()
            .map(|p| {
                p.as_object()
                    .and_then(|o| o.get("img_path"))
                    .and_then(|v| v.as_str())
            })
            .collect();
        let distinct: BTreeSet<&str> = named.iter().flatten().copied().collect();
        PageFacts {
            count: pages.len(),
            all_named: named.iter().all(Option::is_some),
            distinct: distinct.len(),
            digest: names_digest(distinct.iter().copied()),
            repeated: (distinct.len() < pages.len())
                .then(|| distinct.iter().map(|n| n.to_string()).collect()),
        }
    }

    /// One page per image of `names` (an archive's reader image names), and every page
    /// names one of them: as many pages as names, each page's `img_path` among them.
    pub fn name_the_images_of(&self, names: &[String]) -> bool {
        if self.count != names.len() || !self.all_named {
            return false;
        }
        let set: BTreeSet<&str> = names.iter().map(String::as_str).collect();
        match &self.repeated {
            Some(listed) => listed.iter().all(|n| set.contains(n.as_str())),
            // As many distinct page names as pages as names: every page name among the
            // names exactly when the two sets are one.
            None => set.len() == self.distinct && names_digest(set) == self.digest,
        }
    }
}

impl BlockFacts {
    fn of(block: &JsonObject) -> BlockFacts {
        let text = |k: &str| block.get(k).and_then(|v| v.as_str()).map(str::to_string);
        BlockFacts {
            id: text("id"),
            detector: text("detector"),
            patch_budget: block
                .get("patch_budget")
                .and_then(|v| v.as_num())
                .map(|n| n.as_f64() as u32),
            revision: block
                .get("revision")
                .and_then(|v| v.as_num())
                .and_then(|n| match n {
                    JsonNum::Int(i) if *i >= 0 => Some(u32::try_from(*i).unwrap_or(u32::MAX)),
                    _ => None,
                })
                .unwrap_or(1),
            upgraded_from_primary: block
                .get("upgraded_from_primary")
                .is_some_and(|v| v.is_truthy()),
        }
    }
}

impl ObjectFacts {
    pub fn of(data: &JsonObject) -> ObjectFacts {
        ObjectFacts {
            edited: data.get("edited").and_then(|v| v.as_object()).is_some(),
            block: data
                .get("ocr_engine")
                .and_then(|b| b.as_object())
                .map(BlockFacts::of),
            version: data
                .get("version")
                .and_then(|v| v.as_str())
                .is_some_and(|v| !v.is_empty()),
            pages: data
                .get("pages")
                .and_then(|p| p.as_array())
                .map(PageFacts::of),
        }
    }
}

impl JsonFacts {
    fn of(data: &Value) -> JsonFacts {
        JsonFacts {
            edited: data.get("edited").is_some(),
            engine: super::upgrade::engine_of(data),
        }
    }
}

impl SidecarFacts {
    /// The facts of a sidecar's whole plain bytes (after gunzip).
    pub fn of(bytes: &[u8]) -> SidecarFacts {
        let object = std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| pyjson::parse(pyjson::strip_bom(text)).ok())
            .and_then(|v| match v {
                JsonValue::Object(data) => Some(ObjectFacts::of(&data)),
                _ => None,
            });
        let json = serde_json::from_slice::<Value>(super::upgrade::strip_bom(bytes))
            .ok()
            .map(|d| JsonFacts::of(&d));
        SidecarFacts {
            object,
            json,
            sealed: bunko_layout::seal::is_sealed(bytes),
        }
    }

    /// The facts of the file at `path`, read now (nothing cached): unreadable ones when
    /// it cannot be read.
    pub fn of_file(path: &Path) -> SidecarFacts {
        match bunko_library::sidecar::read_sidecar_bytes(path) {
            Ok(bytes) => SidecarFacts::of(&bytes),
            Err(_) => SidecarFacts::default(),
        }
    }

    /// Bunko's own output of `engine` (sealed, no `edited` key) at a revision below
    /// `revision` ([`super::upgrade::superseded_own_output`]).
    pub fn superseded(&self, engine: &str, revision: u32) -> bool {
        self.json.as_ref().is_some_and(|j| {
            !j.edited
                && j.engine
                    .as_ref()
                    .is_some_and(|(id, r)| id == engine && *r < revision)
        }) && self.sealed
    }

    /// The `ocr_engine.id` stamp of the bytes (`serde_json`'s reading), if any.
    pub fn stamped(&self) -> bool {
        self.json.as_ref().is_some_and(|j| j.engine.is_some())
    }

    /// An `edited` key (`serde_json`'s reading).
    pub fn edited_key(&self) -> bool {
        self.json.as_ref().is_some_and(|j| j.edited)
    }
}

impl SidecarFactsCache {
    /// The library-relative name `path`'s file has now, when it was renamed in case only:
    /// its folder lists another spelling of the name at exactly the version `key`, on a
    /// store whose version names the bytes and stays with its file (never where another
    /// file at that mtime and size could take the name, nor a local one, which has none).
    fn renamed_in_case(
        &self,
        path: &Path,
        key: &SidecarFactsKey,
        listings: &mut HashMap<PathBuf, Vec<String>>,
    ) -> Option<String> {
        if key.version.is_empty() || !content_versions(path) {
            return None;
        }
        let folder = path.parent()?;
        let name = path.file_name()?.to_str()?;
        let folded = bunko_core::names::fold_name(name);
        let names = listings
            .entry(folder.to_path_buf())
            .or_insert_with(|| bunko_library::sidecar::list_names(folder).unwrap_or_default());
        names
            .iter()
            .filter(|n| n.as_str() != name && bunko_core::names::fold_name(n) == folded)
            .find_map(|n| {
                let rel = rel_of(&self.library, &folder.join(n))?;
                let now = FileVersion::now(&folder.join(n))?.key(&rel)?;
                (now.size == key.size && now.mtime_ns == key.mtime_ns && now.version == key.version)
                    .then_some(rel)
            })
    }
}

/// The version of a file the facts were read at: `(size, mtime_ns, content version)`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileVersion {
    size: u64,
    mtime_ns: i128,
    version: Option<Version>,
}

impl FileVersion {
    fn of(meta: &Meta) -> FileVersion {
        FileVersion {
            size: meta.len(),
            mtime_ns: mtime_ns(meta),
            version: meta.version().cloned(),
        }
    }

    /// Too new to tell from its next version: a file at `path` whose version is no
    /// content version ([`content_versions`]: none at all on a local disk; the mtime and
    /// size on WebDAV or a connected local folder) whose mtime is less than [`RACY`] ago
    /// (or ahead of the clock) may be written again within one tick of its clock, at the
    /// same size, and nothing would tell the two apart (git's racy files). Its facts are
    /// used, not kept.
    fn racy(&self, path: &Path) -> bool {
        if self.version.is_some() && content_versions(path) {
            return false;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as i128);
        now - self.mtime_ns < RACY.as_nanos() as i128
    }

    fn now(path: &Path) -> Option<FileVersion> {
        fs::metadata(path)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| FileVersion::of(&m))
    }

    /// The database key of `rel` at this version (`None` when it does not fit one).
    fn key(&self, rel: &str) -> Option<SidecarFactsKey> {
        Some(SidecarFactsKey {
            sidecar_path: rel.to_string(),
            size: i64::try_from(self.size).ok()?,
            mtime_ns: i64::try_from(self.mtime_ns).ok()?,
            version: self
                .version
                .as_ref()
                .map_or_else(String::new, |v| v.as_str().to_string()),
        })
    }
}

/// The stored form of the facts.
#[derive(Serialize, Deserialize)]
struct Stored {
    format: u32,
    facts: SidecarFacts,
}

/// How old a file without a content version must be for its facts to be kept: more than
/// the coarsest file system clock (FAT's 2 s; a WebDAV host's `getlastmodified` has 1 s).
pub const RACY: Duration = Duration::from_secs(3);

/// The store of `path` gives content versions ([`bunko_store::Caps::content_versions`]):
/// one version is one content, wherever the file is. Never on a local library.
fn content_versions(path: &Path) -> bool {
    bunko_store::store_for(path).is_some_and(|s| s.caps().content_versions)
}

/// The bytes of the sidecar at `path` at the version `meta` names: on a cloud store
/// read pinned to it (a version since replaced is an error), else as the file is now.
fn read_at(path: &Path, meta: &Meta) -> std::io::Result<Vec<u8>> {
    let gz = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().ends_with(".gz"));
    let remote = bunko_store::store_for(path).filter(|s| s.caps().remote);
    match (remote, meta.version()) {
        (Some(store), Some(version)) if !gz => {
            let pinned = store.local_path(path, Some(version))?;
            bunko_library::sidecar::read_capped(
                pinned.path(),
                bunko_library::sidecar::MAX_SIDECAR_BYTES,
            )
        }
        _ => bunko_library::sidecar::read_sidecar_bytes(path),
    }
}

/// How often a walk may prune the rows of files that are gone or changed.
pub const PRUNE_EVERY: Duration = Duration::from_secs(600);

/// [`SidecarFacts`] by file, for the version of each file it was read at (see the module
/// doc). In `mokuro.db` when there is one; else in memory, for this process.
pub struct SidecarFactsCache {
    library: PathBuf,
    db: Option<Arc<Database>>,
    /// Without a database: the facts by path, with the version they were read at.
    memory: Mutex<HashMap<PathBuf, (FileVersion, Arc<SidecarFacts>)>>,
    /// Files read for their facts by this cache (tests and diagnostics).
    reads: AtomicU64,
    pruned_at: Mutex<Option<Instant>>,
}

impl SidecarFactsCache {
    pub fn new(library: PathBuf, db: Option<Arc<Database>>) -> SidecarFactsCache {
        SidecarFactsCache {
            library,
            db,
            memory: Mutex::new(HashMap::new()),
            reads: AtomicU64::new(0),
            pruned_at: Mutex::new(None),
        }
    }

    /// How many files this cache has read for their facts.
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// The facts of the file at `path` as it is now; `None` when it is no file.
    pub fn get(&self, path: &Path) -> Option<Arc<SidecarFacts>> {
        let meta = fs::metadata(path).ok().filter(|m| m.is_file())?;
        Some(self.get_at(path, &meta))
    }

    /// The facts of the file at `path`, whose metadata (taken now) is `meta`: those kept
    /// for exactly that version, else read now (and kept, when the file still is that
    /// version after the read and is not [racy](FileVersion::racy)). A hint: what
    /// deletes, replaces or promotes a file decides on [`Self::fresh`].
    pub fn get_at(&self, path: &Path, meta: &Meta) -> Arc<SidecarFacts> {
        let at = FileVersion::of(meta);
        if let Some(hit) = self.cached(path, &at) {
            return hit;
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.facts_of_read(
            path,
            meta,
            bunko_library::sidecar::read_sidecar_bytes(path).map(|b| SidecarFacts::of(&b)),
        )
    }

    /// The facts of one read of the file at `path` at the version `meta` names, kept
    /// as [`Self::keep_read`] keeps them: over the cap, a fact of the version (its
    /// declared size, never parsed); another error (the network, a file gone
    /// meanwhile) teaches nothing of the version, and its unreadable facts are not kept.
    fn facts_of_read(
        &self,
        path: &Path,
        meta: &Meta,
        read: std::io::Result<SidecarFacts>,
    ) -> Arc<SidecarFacts> {
        match read {
            Ok(facts) => {
                let facts = Arc::new(facts);
                self.keep_read(path, meta, &facts);
                facts
            }
            Err(e) if e.kind() == std::io::ErrorKind::FileTooLarge => {
                let facts = Arc::new(SidecarFacts::default());
                self.keep_read(path, meta, &facts);
                facts
            }
            Err(_) => Arc::new(SidecarFacts::default()),
        }
    }

    /// The facts of the file at `path` read now, whatever is kept: what a step that
    /// deletes, replaces or promotes the file decides on, under the file's write lock.
    /// They are kept in place of what was (a version that names no content can hide a
    /// rewrite, and the facts kept for it are the old bytes'). Unreadable facts when the
    /// file cannot be read; `None` when it is no file.
    pub fn fresh(&self, path: &Path) -> Option<Arc<SidecarFacts>> {
        let meta = fs::metadata(path).ok().filter(|m| m.is_file())?;
        Some(self.fresh_at(path, &meta))
    }

    /// [`Self::fresh`] of the file whose metadata (taken now) is `meta`: read at that
    /// version (a cloud file's read pinned to it).
    pub fn fresh_at(&self, path: &Path, meta: &Meta) -> Arc<SidecarFacts> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.facts_of_read(
            path,
            meta,
            read_at(path, meta).map(|b| SidecarFacts::of(&b)),
        )
    }

    /// The bytes of the file at `path` at the version `meta` (taken now) names (a cloud
    /// file's read pinned to it), with their facts, kept as [`Self::fresh`] keeps them:
    /// what a promotion installs, exactly the bytes it decided on.
    pub fn read_now(
        &self,
        path: &Path,
        meta: &Meta,
    ) -> std::io::Result<(Vec<u8>, Arc<SidecarFacts>)> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let bytes = read_at(path, meta)?;
        let facts = Arc::new(SidecarFacts::of(&bytes));
        self.keep_read(path, meta, &facts);
        Ok((bytes, facts))
    }

    /// Keep `facts`, read off the file at `path` at the version `meta` names, when the
    /// file still is that version after the read and is not [racy](FileVersion::racy).
    fn keep_read(&self, path: &Path, meta: &Meta, facts: &Arc<SidecarFacts>) {
        let at = FileVersion::of(meta);
        if !at.racy(path) && FileVersion::now(path).as_ref() == Some(&at) {
            self.remember(path, &at, facts);
        }
    }

    /// A cloud file with no content version: nothing pins what was read.
    fn unpinned(path: &Path, at: &FileVersion) -> bool {
        at.version.is_none() && bunko_store::store_for(path).is_some_and(|s| s.caps().remote)
    }

    fn cached(&self, path: &Path, at: &FileVersion) -> Option<Arc<SidecarFacts>> {
        let Some(db) = &self.db else {
            return self
                .memory
                .lock()
                .get(path)
                .filter(|(v, _)| v == at)
                .map(|(_, f)| f.clone());
        };
        if Self::unpinned(path, at) {
            return None;
        }
        let rel = rel_of(&self.library, path)?;
        let want = at.key(&rel)?;
        let stored = |row: bunko_db::SidecarFacts| {
            let stored: Stored = serde_json::from_str(&row.facts).ok()?;
            (stored.format == FACTS_FORMAT).then(|| Arc::new(stored.facts))
        };
        if let Some(row) = db.get_sidecar_facts(&rel).ok().flatten()
            && row.key == want
        {
            return stored(row);
        }
        // The same version under another name: the file renamed, on a store whose
        // version names the bytes and stays with its file; its facts are kept under this
        // name too. Never elsewhere: an mtime and a size (or an ETag made of them) are
        // no file's identity, and two files can share them.
        if !content_versions(path) {
            return None;
        }
        let row = db
            .sidecar_facts_of_version(&want.version, want.size, want.mtime_ns)
            .ok()
            .flatten()?;
        let facts = stored(row)?;
        self.remember(path, at, &facts);
        Some(facts)
    }

    fn remember(&self, path: &Path, at: &FileVersion, facts: &Arc<SidecarFacts>) {
        let Some(db) = &self.db else {
            self.memory
                .lock()
                .insert(path.to_path_buf(), (at.clone(), facts.clone()));
            return;
        };
        if Self::unpinned(path, at) {
            return;
        }
        let Some(key) = rel_of(&self.library, path).and_then(|rel| at.key(&rel)) else {
            return;
        };
        let stored = Stored {
            format: FACTS_FORMAT,
            facts: (**facts).clone(),
        };
        let Ok(text) = serde_json::to_string(&stored) else {
            return;
        };
        if let Err(e) = db.record_sidecar_facts(&bunko_db::SidecarFacts { key, facts: text }) {
            tracing::warn!("could not keep what a sidecar says: {e}");
        }
    }

    /// [`Self::prune`], when the last one ran `every` ago or longer (or never).
    pub fn prune_every(&self, every: Duration) -> usize {
        {
            let mut at = self.pruned_at.lock();
            if at.is_some_and(|t| t.elapsed() < every) {
                return 0;
            }
            *at = Some(Instant::now());
        }
        self.prune()
    }

    /// Drop the facts of every file that is gone or changed since: those whose path
    /// reaches no file, or a file at another version. A cloud file renamed in case only
    /// (the folder lists another spelling of its name at its very version) keeps its
    /// facts under the new spelling. Returns how many went. A library folder that is
    /// not there (a disk or share not mounted yet) tells nothing of its files: nothing
    /// goes, or every sidecar would be read again once it is back.
    pub fn prune(&self) -> usize {
        if !fs::metadata(&self.library).is_ok_and(|m| m.is_dir()) {
            return 0;
        }
        let Some(db) = &self.db else {
            let mut memory = self.memory.lock();
            let before = memory.len();
            memory.retain(|path, (at, _)| FileVersion::now(path).as_ref() == Some(at));
            return before - memory.len();
        };
        let keys = match db.sidecar_facts_keys() {
            Ok(keys) => keys,
            Err(e) => {
                tracing::warn!("could not list what sidecars said: {e}");
                return 0;
            }
        };
        let mut listings: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let mut stale = Vec::new();
        for key in keys {
            let path = self.library.join(&key.sidecar_path);
            match FileVersion::now(&path) {
                Some(now) if now.key(&key.sidecar_path).as_ref() == Some(&key) => continue,
                Some(_) => stale.push(key),
                None => match self.renamed_in_case(&path, &key, &mut listings) {
                    Some(new) if db.rename_sidecar_facts(&key, &new).unwrap_or(false) => {}
                    _ => stale.push(key),
                },
            }
        }
        match db.forget_sidecar_facts(&stale) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("could not prune what sidecars said: {e}");
                0
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // fixtures are written with std::fs
mod tests {
    use super::*;

    fn pages(names: &[&str]) -> PageFacts {
        let text = format!(
            r#"{{"pages":[{}]}}"#,
            names
                .iter()
                .map(|n| format!(r#"{{"img_path":"{n}"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        SidecarFacts::of(text.as_bytes())
            .object
            .unwrap()
            .pages
            .unwrap()
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// The page test the census made on the whole `pages` array: as many pages as the
    /// archive has images, each page's `img_path` among them (repeats included).
    #[test]
    fn pages_name_the_images_as_the_whole_array_did() {
        let abc = names(&["a.jpg", "b.jpg", "c.jpg"]);
        assert!(pages(&["a.jpg", "b.jpg", "c.jpg"]).name_the_images_of(&abc));
        assert!(pages(&["c.jpg", "a.jpg", "b.jpg"]).name_the_images_of(&abc));
        assert!(!pages(&["a.jpg", "b.jpg"]).name_the_images_of(&abc));
        assert!(!pages(&["a.jpg", "b.jpg", "d.jpg"]).name_the_images_of(&abc));
        // A repeated page: still each among the images, as many as the images.
        let p = pages(&["a.jpg", "a.jpg", "b.jpg"]);
        assert!(p.repeated.is_some());
        assert!(p.name_the_images_of(&abc));
        assert!(!pages(&["a.jpg", "a.jpg", "d.jpg"]).name_the_images_of(&abc));
        // An archive listing an image twice.
        let twice = names(&["a.jpg", "a.jpg", "b.jpg"]);
        assert!(!pages(&["a.jpg", "b.jpg", "c.jpg"]).name_the_images_of(&twice));
        assert!(pages(&["a.jpg", "b.jpg", "b.jpg"]).name_the_images_of(&twice));
        // A page without a name.
        let text = r#"{"pages":[{"img_path":"a.jpg"},{},{"img_path":"c.jpg"}]}"#;
        let unnamed = SidecarFacts::of(text.as_bytes())
            .object
            .unwrap()
            .pages
            .unwrap();
        assert!(!unnamed.name_the_images_of(&abc));
        // No pages, no images.
        assert!(pages(&[]).name_the_images_of(&[]));
    }

    /// The facts of an unreadable or non-object file are the census's "unknown".
    #[test]
    fn unreadable_bytes_have_no_facts() {
        for bytes in [&b"nope"[..], b"[1,2]", b"\xff\xfe", b""] {
            let f = SidecarFacts::of(bytes);
            assert_eq!(f.object, None, "{bytes:?}");
            assert!(!f.sealed);
            assert!(!f.stamped());
        }
        let f = SidecarFacts::of(br#"{"edited":1,"ocr_engine":{"id":"x"}}"#);
        assert!(f.edited_key(), "any edited key, for serde's reading");
        assert!(
            !f.object.unwrap().edited,
            "an edited object, for the recipe"
        );
    }

    /// `bytes` written at `path`, last modified `ago`.
    fn write_aged(path: &Path, bytes: &[u8], ago: Duration) {
        std::fs::write(path, bytes).unwrap();
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - ago).unwrap();
    }

    /// Without a database the facts are kept in memory, by version: a file read once is
    /// not read again until it changes; a prune drops a file that is gone.
    #[test]
    fn a_file_is_read_once_per_version() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path();
        let path = lib.join("v.mokuro");
        let hour = Duration::from_secs(3600);
        write_aged(&path, br#"{"version":"0.2.1","pages":[]}"#, hour);
        let cache = SidecarFactsCache::new(lib.to_path_buf(), None);
        let first = cache.get(&path).unwrap();
        assert_eq!(cache.get(&path).unwrap(), first);
        assert_eq!(cache.reads(), 1);
        write_aged(
            &path,
            br#"{"version":"","pages":[{"img_path":"a"}]}"#,
            hour / 2,
        );
        let second = cache.get(&path).unwrap();
        assert_ne!(second, first);
        assert_eq!(cache.reads(), 2);
        assert_eq!(cache.prune(), 0);
        std::fs::remove_file(&path).unwrap();
        assert!(cache.get(&path).is_none());
        assert_eq!(cache.prune(), 1);
    }

    /// A library folder that is not there (a share not mounted yet) prunes nothing: its
    /// files are not gone, and their facts are good again once it is back.
    #[test]
    fn a_library_that_is_not_there_prunes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("library");
        std::fs::create_dir_all(lib.join("S")).unwrap();
        let path = lib.join("S/v.mokuro");
        write_aged(
            &path,
            br#"{"version":"0.2.1","pages":[]}"#,
            Duration::from_secs(3600),
        );
        let db = Arc::new(Database::open(dir.path().join("mokuro.db")).unwrap());
        let cache = SidecarFactsCache::new(lib.clone(), Some(db.clone()));
        cache.get(&path).unwrap();
        assert_eq!(db.sidecar_facts_keys().unwrap().len(), 1);
        let away = dir.path().join("unmounted");
        std::fs::rename(&lib, &away).unwrap();
        assert_eq!(cache.prune(), 0);
        assert_eq!(db.sidecar_facts_keys().unwrap().len(), 1, "kept");
        std::fs::rename(&away, &lib).unwrap();
        let again = SidecarFactsCache::new(lib.clone(), Some(db.clone()));
        again.get(&path).unwrap();
        assert_eq!(again.reads(), 0, "not read again once it is back");
        // A file that is gone from a library that is there goes.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(again.prune(), 1);
    }

    /// A file written a moment ago may be written again within its clock's tick, at the
    /// same size: its facts are used, never kept, until it is older than that.
    #[test]
    fn a_file_just_written_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path();
        let path = lib.join("v.mokuro");
        std::fs::write(&path, br#"{"version":"0.2.1","pages":[]}"#).unwrap();
        let cache = SidecarFactsCache::new(lib.to_path_buf(), None);
        cache.get(&path).unwrap();
        cache.get(&path).unwrap();
        assert_eq!(cache.reads(), 2, "read each time while it is new");
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(SystemTime::now() - RACY * 2).unwrap();
        cache.get(&path).unwrap();
        cache.get(&path).unwrap();
        assert_eq!(cache.reads(), 3, "then kept");
    }
}
