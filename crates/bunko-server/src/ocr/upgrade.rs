//! Generation upgrade (spec generation-upgrade.md): bring a volume's bare `.mokuro` up
//! to the current primary recipe, by a direct replace when a matching layer is already
//! there, else by an upgrade job whose result is swapped in. The bare file is model
//! output (D4): it is never skipped for looking edited. One that carries Bunko's own
//! `ocr_engine` stamp is replaced outright; any other (stock mokuro, adopted, or a
//! reader's correction) is first kept as the layer `<Vol>.pre-upgrade.mokuro`, and the
//! swap waits until that layer is committed. The same code runs on every library store.
//! Layers an earlier 0.7 beta kept of the old file (stamped `upgraded_from_primary`)
//! are removed once the volume is upgraded.
//!
//! Deviation: weight pins are not part of the compared recipe (the server cannot know
//! the pins a processor's export carries before it runs); listing the primary's own
//! engine in `replace` therefore re-runs nothing.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bunko_core::config::UpgradeConfig;
use bunko_core::generations::Generation;
use bunko_db::{AuditDetails, Database, NewAuditEvent};
use bunko_store::{Durability, Mode, Origin, Version, WriteIntent, fs};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::collect::{CollectRequest, Outcome};
use super::owed::UpgradeProbe;
use super::types::{LibraryFacts, PathLocks, mtime_ns, rel_of};

/// A plain sidecar's bytes, refused past `MAX_SIDECAR_BYTES` (never read whole).
fn read_sidecar(path: &Path) -> std::io::Result<Vec<u8>> {
    bunko_library::sidecar::read_capped(path, bunko_library::sidecar::MAX_SIDECAR_BYTES)
}

/// The family of a file no recipe can be read from.
pub const UNKNOWN: &str = "unknown";
pub const LEGACY: &str = "mokuro-legacy";

/// What produced a bare sidecar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipe {
    /// Engine id, `mokuro-legacy` or `unknown`.
    pub family: String,
    pub engine: String,
    pub detector: Option<String>,
    pub patch_budget: Option<u32>,
}

impl Recipe {
    fn unknown() -> Recipe {
        Recipe {
            family: UNKNOWN.into(),
            engine: String::new(),
            detector: None,
            patch_budget: None,
        }
    }

    pub fn matches(&self, target: &(String, String, Option<u32>)) -> bool {
        self.family != UNKNOWN
            && self.family != LEGACY
            && self.engine == target.0
            && self.detector.as_deref() == Some(target.1.as_str())
            && self.patch_budget == target.2
    }

    pub fn to_value(&self) -> Value {
        json!([self.engine, self.detector, self.patch_budget])
    }
}

/// `classify`: the recipe of a sidecar, from its provenance row, else its `ocr_engine`
/// block, else mokuro's top-level `version` (legacy), else unknown.
pub fn classify(path: &Path, row: Option<&bunko_db::OcrSidecar>) -> Recipe {
    let loaded = bunko_library::sidecar::load_sidecar(path);
    let Some(data) = loaded.data else {
        return Recipe::unknown();
    };
    let block = data.get("ocr_engine").and_then(|b| b.as_object());
    let block_str = |k: &str| {
        block
            .and_then(|b| b.get(k))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let block_budget = block
        .and_then(|b| b.get("patch_budget"))
        .and_then(|v| v.as_num())
        .map(|n| n.as_f64() as u32);
    if let Some(r) = row
        && let Some(engine) = r.engine.clone().filter(|e| !e.is_empty())
    {
        let budget = if engine == "hayai-nova" {
            block_budget.or(Some(bunko_core::engines::DEFAULT_PATCH_BUDGET))
        } else {
            None
        };
        return Recipe {
            family: engine.clone(),
            engine,
            detector: r.detector.clone().or_else(|| block_str("detector")),
            patch_budget: budget,
        };
    }
    if let Some(engine) = block_str("id") {
        if engine == "mokuro" {
            return Recipe {
                family: LEGACY.into(),
                engine: LEGACY.into(),
                detector: Some("ctd".into()),
                patch_budget: None,
            };
        }
        let budget = if engine == "hayai-nova" {
            block_budget.or(Some(bunko_core::engines::DEFAULT_PATCH_BUDGET))
        } else {
            None
        };
        let detector = block_str("detector").or_else(|| {
            bunko_core::engines::engine(&engine)
                .and_then(|e| e.detector)
                .map(str::to_string)
        });
        return Recipe {
            family: engine.clone(),
            engine,
            detector,
            patch_budget: budget,
        };
    }
    if block.is_none()
        && data
            .get("version")
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.is_empty())
    {
        return Recipe {
            family: LEGACY.into(),
            engine: LEGACY.into(),
            detector: Some("ctd".into()),
            patch_budget: None,
        };
    }
    Recipe::unknown()
}

/// Why a volume is not upgraded, or what happens to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Policy says no, or the recipe already matches: nothing to do.
    Current,
    /// A matching layer can be swapped in.
    Ready(PathBuf),
    /// It needs an upgrade job.
    NeedsOcr,
    SkippedMissingPages,
}

#[derive(Clone, Debug)]
struct CensusEntry {
    stamp: (u64, i128),
    /// The bare file's content version when it was judged (remote stores; `None` on a
    /// local library, where `stamp` pins it). The swap replaces only this version.
    version: Option<Version>,
    family: String,
    verdict: Verdict,
}

/// A test hook run inside a swap (see [`Upgrade::set_swap_hook`]).
pub type SwapHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// What the preserve step kept of the old bare file: the layer's library-relative path
/// and the write lock on it, held until the swap is done.
struct Preserved {
    rel: String,
    _guard: Box<dyn Send>,
}

/// The layer id the old bare file is kept under.
pub const PRE_UPGRADE: &str = "pre-upgrade";

/// The `pre-upgrade` layer ids: `pre-upgrade`, then `pre-upgrade-2`, `-3`…
fn is_pre_upgrade_id(id: &str) -> bool {
    let id = id.to_lowercase();
    id == PRE_UPGRADE
        || id
            .strip_prefix("pre-upgrade-")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The `ocr_engine.id` a sidecar's own bytes carry at top level (Bunko's stamp), if any.
fn stamp_id(data: &Value) -> Option<String> {
    data.get("ocr_engine")
        .and_then(Value::as_object)
        .and_then(|b| b.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// The `ocr_engine.id` the file at `path` carries (read capped), if any.
fn file_stamp_id(path: &Path) -> Option<String> {
    let bytes = read_sidecar(path).ok()?;
    stamp_id(&serde_json::from_slice::<Value>(&bytes).ok()?)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Write `bytes` to a staging file at `tmp` (on a remote store: local until published;
/// `fs::write` would publish the temporary name itself), then publish it as `dest`.
fn stage_and_publish(
    tmp: &Path,
    dest: &Path,
    bytes: &[u8],
    intent: &WriteIntent,
) -> std::io::Result<()> {
    let staged = || -> std::io::Result<()> {
        use std::io::Write as _;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        f.sync_all()
    };
    staged()
        .and_then(|()| fs::publish(tmp, dest, intent).map(|_| ()))
        .inspect_err(|_| {
            let _ = fs::remove_file(tmp);
        })
}

/// `.<name>.<what>.tmp` beside `path`.
fn tmp_beside(path: &Path, what: &str) -> PathBuf {
    path.with_file_name(format!(
        ".{}.{what}.tmp",
        path.file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default()
    ))
}

/// The census over the library's bare files, cached against each file's stat.
pub struct Upgrade {
    pub config: Mutex<UpgradeConfig>,
    pub primary: Mutex<Option<Generation>>,
    pub rows: Mutex<Vec<Generation>>,
    pub library: PathBuf,
    pub db: Option<Arc<Database>>,
    pub facts: Arc<dyn LibraryFacts>,
    pub locks: PathLocks,
    pub generator: String,
    census: Mutex<HashMap<String, CensusEntry>>,
    direct: Mutex<DirectProgress>,
    halted: AtomicBool,
    swap_hook: Mutex<Option<SwapHook>>,
}

/// The direct replaces in progress: volumes the census found a matching layer for are
/// swapped in by one paced job ([`Upgrade::run_direct`]), never by the walk itself.
#[derive(Debug, Clone, Default)]
pub struct DirectProgress {
    pub running: bool,
    pub total: u64,
    pub done: u64,
    pub failed: u64,
}

impl Upgrade {
    pub fn new(
        library: PathBuf,
        db: Option<Arc<Database>>,
        facts: Arc<dyn LibraryFacts>,
        locks: PathLocks,
        generator: String,
    ) -> Upgrade {
        Upgrade {
            config: Mutex::new(UpgradeConfig::default()),
            primary: Mutex::new(None),
            rows: Mutex::new(Vec::new()),
            library,
            db,
            facts,
            locks,
            generator,
            census: Mutex::new(HashMap::new()),
            direct: Mutex::new(DirectProgress::default()),
            halted: AtomicBool::new(false),
            swap_hook: Mutex::new(None),
        }
    }

    pub fn configure(&self, config: &UpgradeConfig, rows: &[Generation]) {
        *self.config.lock() = config.clone();
        *self.primary.lock() = bunko_core::generations::primary_generation(rows).cloned();
        *self.rows.lock() = rows.to_vec();
        self.census.lock().clear();
    }

    fn bare_rel(rel: &str) -> String {
        format!("{}.mokuro", &rel[..rel.len() - 4])
    }

    /// The volume's layer sidecars (plain `.mokuro`), sorted, as the directory lists them.
    fn layer_files(cbz: &Path) -> Vec<PathBuf> {
        let Some(dir) = cbz.parent() else {
            return Vec::new();
        };
        let stem = cbz
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Ok(rd) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let names: Vec<String> = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let others = bunko_library::sidecar::volume_stems(names.iter().map(String::as_str));
        let mut out: Vec<PathBuf> = names
            .iter()
            .filter(|n| {
                !n.ends_with(".gz")
                    && bunko_library::sidecar::layer_id_of_sidecar(n, &stem, &others).is_some()
            })
            .map(|n| dir.join(n))
            .collect();
        out.sort();
        out
    }

    /// A layer file's id (`<stem>.<id>.mokuro`).
    fn layer_id(layer: &Path) -> Option<String> {
        layer
            .file_name()
            .and_then(|f| f.to_str())
            .and_then(|f| f.strip_suffix(".mokuro"))
            .and_then(|s| s.rsplit_once('.'))
            .map(|(_, g)| g.to_string())
    }

    /// The layers the upgrade may read (D4): those that carry an `ocr_engine` stamp or
    /// have an `ocr_sidecars` row, with that row. A layer with neither (a reader's
    /// edit, a `pre-upgrade` copy) is never a candidate, nor ever deleted.
    fn layer_candidates(&self, cbz: &Path) -> Vec<(PathBuf, Option<bunko_db::OcrSidecar>)> {
        Self::layer_files(cbz)
            .into_iter()
            .filter(|l| !Self::layer_id(l).is_some_and(|id| is_pre_upgrade_id(&id)))
            .filter_map(|layer| {
                let layer_rel = rel_of(&self.library, &layer).unwrap_or_default();
                let row = self
                    .db
                    .as_ref()
                    .and_then(|db| db.get_ocr_sidecar(&layer_rel).ok().flatten());
                (row.is_some() || file_stamp_id(&layer).is_some()).then_some((layer, row))
            })
            .collect()
    }

    /// Was this layer produced from the archive as it is now?
    fn read_from_current_archive(
        &self,
        layer: &Path,
        cbz: &Path,
        row: Option<&bunko_db::OcrSidecar>,
    ) -> bool {
        let stamp = super::types::stamp_of(cbz);
        if let (Some(r), Some(s)) = (row, stamp)
            && r.archive_size.is_some()
        {
            return r.archive_size == Some(s.0 as i64) && r.archive_mtime_ns == Some(s.1 as i64);
        }
        let loaded = bunko_library::sidecar::load_sidecar(layer);
        let Some(data) = loaded.data else {
            return false;
        };
        let Some(pages) = data.get("pages").and_then(|p| p.as_array()) else {
            return false;
        };
        let Some(names) = bunko_library::archive::reader_image_names(cbz) else {
            return false;
        };
        if pages.len() != names.len() {
            return false;
        }
        let set: HashSet<&str> = names.iter().map(String::as_str).collect();
        pages.iter().all(|p| {
            p.as_object()
                .and_then(|o| o.get("img_path"))
                .and_then(|v| v.as_str())
                .is_some_and(|i| set.contains(i))
        })
    }

    /// The census verdict for one volume (cached by the bare file's stat).
    pub fn judge(&self, cbz: &Path, rel: &str) -> (String, Verdict) {
        let bare = cbz.with_extension("mokuro");
        let Some(meta) = fs::metadata(&bare).ok().filter(|m| m.is_file()) else {
            return (UNKNOWN.into(), Verdict::Current);
        };
        let stamp = (meta.len(), mtime_ns(&meta));
        let version = meta.version().cloned();
        if let Some(hit) = self
            .census
            .lock()
            .get(rel)
            .filter(|e| e.stamp == stamp && e.version == version)
        {
            return (hit.family.clone(), hit.verdict.clone());
        }
        let bare_rel = Self::bare_rel(rel);
        let row = self
            .db
            .as_ref()
            .and_then(|db| db.get_ocr_sidecar(&bare_rel).ok().flatten());
        let recipe = classify(&bare, row.as_ref());
        let upgraded = self
            .primary
            .lock()
            .as_ref()
            .is_some_and(|p| recipe.matches(&p.output_affecting()));
        if upgraded {
            self.sweep_kept(cbz);
        }
        let verdict = self.verdict(cbz, &recipe);
        self.census.lock().insert(
            rel.to_string(),
            CensusEntry {
                stamp,
                version,
                family: recipe.family.clone(),
                verdict: verdict.clone(),
            },
        );
        (recipe.family, verdict)
    }

    fn verdict(&self, cbz: &Path, recipe: &Recipe) -> Verdict {
        let config = self.config.lock().clone();
        let Some(primary) = self.primary.lock().clone() else {
            return Verdict::Current;
        };
        if !config.enabled || recipe.family == UNKNOWN || !config.replace.contains(&recipe.family) {
            return Verdict::Current;
        }
        let target = primary.output_affecting();
        if recipe.matches(&target) {
            return Verdict::Current;
        }
        if self.facts.missing_pages(cbz) > 0 {
            return Verdict::SkippedMissingPages;
        }
        for (layer, lrow) in self.layer_candidates(cbz) {
            // The layer's own file must carry the matching stamp: a row alone is not
            // enough (a reader's push of a layer drops the stamp, and `classify` would
            // read the row first).
            if Self::own_stamp_matches(&layer, &target)
                && self.read_from_current_archive(&layer, cbz, lrow.as_ref())
            {
                return Verdict::Ready(layer);
            }
        }
        Verdict::NeedsOcr
    }

    /// The file's own `ocr_engine` stamp (never its row) is the target recipe.
    fn own_stamp_matches(layer: &Path, target: &(String, String, Option<u32>)) -> bool {
        file_stamp_id(layer).is_some() && classify(layer, None).matches(target)
    }

    /// `{enabled, replace, direct, families: {family: n}, ready, needs_ocr,
    /// skipped_missing_pages}` over the volumes judged so far.
    pub fn census(&self) -> Value {
        let census = self.census.lock();
        let mut families: BTreeMap<String, u64> = BTreeMap::new();
        let (mut ready, mut needs, mut missing) = (0u64, 0u64, 0u64);
        for e in census.values() {
            *families.entry(e.family.clone()).or_insert(0) += 1;
            match &e.verdict {
                Verdict::Ready(_) => ready += 1,
                Verdict::NeedsOcr => needs += 1,
                Verdict::SkippedMissingPages => missing += 1,
                Verdict::Current => {}
            }
        }
        let config = self.config.lock().clone();
        let direct = self.direct.lock().clone();
        json!({
            "enabled": config.enabled,
            "replace": config.replace,
            "direct": {
                "running": direct.running,
                "total": direct.total,
                "done": direct.done,
                "failed": direct.failed,
            },
            "families": families,
            "ready": ready,
            "needs_ocr": needs,
            "skipped_missing_pages": missing,
        })
    }

    /// The volumes the census found a matching layer for (sorted by path).
    pub fn ready_volumes(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .census
            .lock()
            .iter()
            .filter(|(_, e)| matches!(e.verdict, Verdict::Ready(_)))
            .map(|(rel, _)| rel.clone())
            .collect();
        out.sort();
        out
    }

    /// Stop a running [`run_direct`](Self::run_direct) after its current volume (server
    /// shutdown).
    pub fn halt(&self) {
        self.halted.store(true, Ordering::SeqCst);
    }

    pub fn direct_progress(&self) -> DirectProgress {
        self.direct.lock().clone()
    }

    /// Swap the matching layer in for each of `volumes`, one at a time with `pace`
    /// between them, re-judging each first (a volume may have changed, or the policy
    /// moved, since the census). Every swap is an ordinary library write the metadata
    /// worker coalesces. Returns `(replaced, failed)`.
    pub fn run_direct(&self, volumes: Vec<String>, pace: Duration) -> (u64, u64) {
        *self.direct.lock() = DirectProgress {
            running: true,
            total: volumes.len() as u64,
            done: 0,
            failed: 0,
        };
        let (mut done, mut failed) = (0u64, 0u64);
        for (i, rel) in volumes.iter().enumerate() {
            if self.halted.load(Ordering::SeqCst) || !self.config.lock().enabled {
                break;
            }
            if i > 0 && !pace.is_zero() {
                std::thread::sleep(pace);
            }
            let cbz = self.library.join(rel);
            if let Verdict::Ready(layer) = self.judge(&cbz, rel).1 {
                match self.direct_replace(&cbz, &layer) {
                    Ok(_) => {
                        done += 1;
                        // The census sees the new bare file now, not at the next walk.
                        self.judge(&cbz, rel);
                    }
                    Err(e) => {
                        failed += 1;
                        tracing::warn!("could not upgrade {rel} directly: {e}");
                    }
                }
            }
            let mut progress = self.direct.lock();
            progress.done = done;
            progress.failed = failed;
        }
        self.direct.lock().running = false;
        (done, failed)
    }

    /// Where 0.7.0-beta.3 saved the unstamped original of a layer it kept (removed with
    /// that layer).
    fn backup_of(&self, kept: &Path) -> PathBuf {
        use sha2::Digest;
        let rel = rel_of(&self.library, kept).unwrap_or_default();
        let digest = hex::encode(sha2::Sha256::digest(rel.as_bytes()));
        self.backups_dir().join(format!("{}.mokuro", &digest[..32]))
    }

    fn backups_dir(&self) -> PathBuf {
        self.library
            .parent()
            .unwrap_or(&self.library)
            .join(".upgrade-originals")
    }

    /// Tests only: `hook` runs inside every swap with the bare path, under the swap's
    /// write locks, after the preserve step and just before the install (and before
    /// the LocalStore re-check of the bare file's stat).
    #[doc(hidden)]
    pub fn set_swap_hook(&self, hook: Option<SwapHook>) {
        *self.swap_hook.lock() = hook;
    }

    /// The census entry a swap pins: the bare file as it was judged (judged now when
    /// nothing is cached). An error when the file changed since (it is judged again at
    /// the next census) or no longer needs an upgrade.
    fn judged(&self, cbz: &Path, rel: &str, bare: &Path) -> Result<CensusEntry, String> {
        let cached = self.census.lock().get(rel).cloned();
        let entry = match cached {
            Some(e) => e,
            None => {
                self.judge(cbz, rel);
                self.census
                    .lock()
                    .get(rel)
                    .cloned()
                    .ok_or_else(|| format!("{} could not be judged", bare.display()))?
            }
        };
        let now =
            fs::metadata(bare).map_err(|e| format!("could not read {}: {e}", bare.display()))?;
        if (now.len(), mtime_ns(&now)) != entry.stamp || now.version() != entry.version.as_ref() {
            self.census.lock().remove(rel);
            return Err(format!(
                "{} changed since it was judged; the next census judges it again",
                bare.display()
            ));
        }
        if !matches!(entry.verdict, Verdict::Ready(_) | Verdict::NeedsOcr) {
            return Err(format!("{} no longer needs an upgrade", bare.display()));
        }
        Ok(entry)
    }

    /// The bytes of the bare file the swap replaces: on a remote store a read pinned to
    /// the judged version; on a local library a read whose stat still matches it.
    fn read_judged(bare: &Path, entry: &CensusEntry) -> Result<Vec<u8>, String> {
        let unreadable = |e: std::io::Error| format!("could not read {}: {e}", bare.display());
        let remote = bunko_store::store_for(bare).filter(|s| s.caps().remote);
        if let (Some(store), Some(version)) = (remote, entry.version.as_ref()) {
            let pinned = store.local_path(bare, Some(version)).map_err(unreadable)?;
            return read_sidecar(pinned.path()).map_err(unreadable);
        }
        let bytes = read_sidecar(bare).map_err(unreadable)?;
        if super::types::stamp_of(bare) != Some(entry.stamp) {
            return Err(format!("{} changed while it was read", bare.display()));
        }
        Ok(bytes)
    }

    /// The preserve step (D4): keep the old bare file, which Bunko did not stamp, as the
    /// layer `<Vol>.pre-upgrade.mokuro` (`-2`, `-3`… when that id holds other bytes),
    /// with a `preserved` block and no `ocr_engine`, published `CreateNew` + Sync: the
    /// layer has committed when this returns. A `pre-upgrade*` layer that already
    /// preserves these exact bytes (a retried swap) is reused. No `ocr_sidecars` row is
    /// recorded for it. The layer's write lock is held by the returned value.
    fn preserve(
        &self,
        cbz: &Path,
        mut doc: Value,
        old: &[u8],
        replaced_by: &str,
    ) -> Result<Preserved, String> {
        let sha = sha256_hex(old);
        let dir = cbz
            .parent()
            .ok_or_else(|| format!("{} has no folder", cbz.display()))?;
        let stem = cbz
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let locked = |p: &Path| format!("{} is locked by a WebDAV write", p.display());
        let existing: Vec<(String, PathBuf)> = Self::layer_files(cbz)
            .into_iter()
            .filter_map(|l| {
                Self::layer_id(&l)
                    .filter(|id| is_pre_upgrade_id(id))
                    .map(|id| (id.to_lowercase(), l))
            })
            .collect();
        for (_, layer) in &existing {
            let same = read_sidecar(layer)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| {
                    v.get("preserved")?
                        .get("sha256")?
                        .as_str()
                        .map(str::to_string)
                })
                .is_some_and(|s| s.eq_ignore_ascii_case(&sha));
            if same {
                let guard = self.locks.try_lock(layer).ok_or_else(|| locked(layer))?;
                return Ok(Preserved {
                    rel: rel_of(&self.library, layer).unwrap_or_default(),
                    _guard: guard,
                });
            }
        }
        let taken: HashSet<&str> = existing.iter().map(|(id, _)| id.as_str()).collect();
        let layer = std::iter::once(PRE_UPGRADE.to_string())
            .chain((2u32..).map(|n| format!("{PRE_UPGRADE}-{n}")))
            .filter(|id| !taken.contains(id.as_str()))
            .map(|id| dir.join(format!("{stem}.{id}.mokuro")))
            .find(|p| !fs::exists(p))
            .unwrap_or_default();
        let guard = self.locks.try_lock(&layer).ok_or_else(|| locked(&layer))?;
        if fs::exists(&layer) {
            return Err(format!("{} appeared meanwhile", layer.display()));
        }
        let Some(obj) = doc.as_object_mut() else {
            return Err("the old OCR file is not a JSON object".into());
        };
        obj.shift_remove("ocr_engine");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        obj.insert(
            "preserved".into(),
            json!({
                "by": "mokuro-bunko",
                "reason": "generation-upgrade",
                "replaced_at": bunko_sched::py::iso_utc(now),
                "sha256": sha,
                "replaced_by": replaced_by,
            }),
        );
        let bytes = serde_json::to_vec(&doc).map_err(|e| e.to_string())?;
        let rel = rel_of(&self.library, &layer).unwrap_or_default();
        let intent = WriteIntent::new(Mode::CreateNew, Origin::Upgrade, Durability::Sync);
        stage_and_publish(&tmp_beside(&layer, "upgrade"), &layer, &bytes, &intent)
            .map_err(|e| format!("could not keep the old OCR as {rel}: {e}"))?;
        tracing::info!("Kept the old OCR file as {rel} before its upgrade");
        Ok(Preserved { rel, _guard: guard })
    }

    /// The swap (§5, D4), with the new content already validated and normalised at
    /// `new`. Under the write lock of the bare file (and of the layer it is kept as):
    /// the bare file must still be the version the census judged; one Bunko did not
    /// stamp is first kept as a `pre-upgrade` layer, committed before anything else;
    /// then the new file replaces exactly that version. Any failure leaves the bare
    /// file as it was, and the next census retries. `mode`: `direct` or `generated`.
    /// Returns the bare path.
    pub fn swap(&self, cbz: &Path, new: &Path, mode: &str) -> Result<PathBuf, String> {
        let bare = cbz.with_extension("mokuro");
        let Some(_guard) = self.locks.try_lock(&bare) else {
            return Err(format!("{} is locked by a WebDAV write", bare.display()));
        };
        let rel = rel_of(&self.library, cbz).unwrap_or_default();
        let result = self.swap_locked(cbz, &bare, &rel, new, mode);
        if result.is_err() {
            self.census.lock().remove(&rel);
        }
        result
    }

    fn swap_locked(
        &self,
        cbz: &Path,
        bare: &Path,
        rel: &str,
        new: &Path,
        mode: &str,
    ) -> Result<PathBuf, String> {
        let bare_rel = Self::bare_rel(rel);
        let row = self
            .db
            .as_ref()
            .and_then(|db| db.get_ocr_sidecar(&bare_rel).ok().flatten());
        let target = self.primary.lock().clone().map(|p| p.output_affecting());
        let replaced_by = file_stamp_id(new)
            .or_else(|| target.as_ref().map(|t| t.0.clone()))
            .unwrap_or_default();
        let (judged, recipe, preserved) = if fs::exists(bare) {
            let entry = self.judged(cbz, rel, bare)?;
            let recipe = classify(bare, row.as_ref());
            let old = Self::read_judged(bare, &entry)?;
            let text = old.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&old);
            let doc: Value = serde_json::from_slice(text)
                .map_err(|e| format!("{} is not readable JSON: {e}", bare.display()))?;
            let preserved = if stamp_id(&doc).is_some() {
                None
            } else {
                Some(self.preserve(cbz, doc, &old, &replaced_by)?)
            };
            (Some(entry), recipe, preserved)
        } else {
            (None, Recipe::unknown(), None)
        };
        let hook = self.swap_hook.lock().clone();
        if let Some(hook) = hook {
            hook(bare);
        }
        let mode_of = match &judged {
            Some(entry) => {
                // LocalStore: nothing pins the replace, so the stat is checked again
                // right before it (a mismatch: skip, the layer kept stays).
                if entry.version.is_none() && super::types::stamp_of(bare) != Some(entry.stamp) {
                    return Err(format!(
                        "{} changed during its upgrade; the next census judges it again",
                        bare.display()
                    ));
                }
                Mode::Replace {
                    base: entry.version.clone(),
                }
            }
            None => Mode::CreateNew,
        };
        let intent = WriteIntent::new(mode_of, Origin::Upgrade, Durability::Sync);
        let installed = if mode == "direct" {
            read_sidecar(new).and_then(|bytes| {
                stage_and_publish(&tmp_beside(bare, "upgrade"), bare, &bytes, &intent)
            })
        } else {
            super::collect::move_into_place(new, bare, &intent)
        };
        installed.map_err(|e| format!("could not install {}: {e}", bare.display()))?;
        // Bookkeeping: the old file's provenance row goes with it (the caller records the
        // new file's), or the next census would read the old recipe off it.
        if let (Some(db), Some(_)) = (&self.db, &row) {
            let _ = db.forget_ocr_sidecar(&bare_rel);
        }
        if let Some(db) = &self.db {
            let mut details = AuditDetails::new()
                .with("from_recipe", recipe.to_value())
                .with(
                    "to_recipe",
                    target.map_or(Value::Null, |t| json!([t.0, t.1, t.2])),
                )
                .with("replaced", true)
                .with("mode", mode.to_string());
            if let Some(p) = &preserved {
                details = details.with("preserved_as", p.rel.clone());
            }
            let path = format!("{}{bare_rel}", bunko_proto::ARCHIVES_ROOT);
            let event = NewAuditEvent::new("ocr_sidecar_upgraded")
                .target_type("sidecar")
                .target_path(&path)
                .details(details);
            let _ = db.log_audit_event(&event);
        }
        self.census.lock().remove(rel);
        Ok(bare.to_path_buf())
    }

    /// Direct replace: copy a matching layer in as the bare file. Only a layer whose own
    /// file carries the primary's `ocr_engine` stamp qualifies (its row alone does
    /// not). The layer is then the primary's output twice over, so it is removed when it
    /// is byte-identical to the new bare file, unless an enabled row writes it (removing
    /// it would only queue that row to write it again).
    pub fn direct_replace(&self, cbz: &Path, layer: &Path) -> Result<PathBuf, String> {
        let target = self
            .primary
            .lock()
            .clone()
            .map(|p| p.output_affecting())
            .ok_or("no primary generation")?;
        if !Self::own_stamp_matches(layer, &target) {
            return Err(format!(
                "{} does not carry the primary recipe's ocr_engine stamp",
                layer.display()
            ));
        }
        let rel = rel_of(&self.library, cbz).unwrap_or_default();
        let bare = self.swap(cbz, layer, "direct")?;
        let layer_rel = rel_of(&self.library, layer).unwrap_or_default();
        let lrow = self
            .db
            .as_ref()
            .and_then(|db| db.get_ocr_sidecar(&layer_rel).ok().flatten());
        if let (Some(db), Some(mut lr)) = (&self.db, lrow.clone()) {
            lr.sidecar_path = Self::bare_rel(&rel);
            if let Some(p) = self.primary.lock().clone() {
                lr.generation_id = p.id;
                lr.generation_name = p.name;
            }
            let _ = db.record_ocr_sidecar(&lr);
        }
        let layer_name = Self::layer_id(layer);
        // The row that writes this layer (a non-primary row of that name).
        let enabled = self
            .rows
            .lock()
            .iter()
            .any(|r| r.runnable() && !r.primary && Some(&r.name) == layer_name.as_ref());
        let identical = matches!(
            (read_sidecar(layer), read_sidecar(&bare)),
            (Ok(a), Ok(b)) if a == b
        );
        if !enabled && identical {
            let _ = fs::remove_file(layer);
            if let Some(db) = &self.db {
                let _ = db.forget_ocr_sidecar(&layer_rel);
            }
        }
        self.facts.sidecar_installed(cbz);
        Ok(bare)
    }

    /// A layer an earlier 0.7 beta's upgrade kept: stamped
    /// `ocr_engine.upgraded_from_primary: true`. A person's layer, another generation's,
    /// a `pre-upgrade` copy, and what a 0.7 beta's forced upgrade kept (all unstamped) never
    /// carry it.
    fn kept_by_upgrade(layer: &Path) -> bool {
        bunko_library::sidecar::load_sidecar(layer)
            .data
            .as_ref()
            .and_then(|d| d.get("ocr_engine"))
            .and_then(|b| b.as_object())
            .and_then(|b| b.get("upgraded_from_primary"))
            .is_some_and(|v| v.is_truthy())
    }

    /// Is the bare file already the upgraded output (the current primary's recipe)?
    fn bare_upgraded(&self, bare: &Path) -> bool {
        let Some(target) = self.primary.lock().clone().map(|p| p.output_affecting()) else {
            return false;
        };
        if !fs::is_file(bare) {
            return false;
        }
        let bare_rel = rel_of(&self.library, bare).unwrap_or_default();
        let row = self
            .db
            .as_ref()
            .and_then(|db| db.get_ocr_sidecar(&bare_rel).ok().flatten());
        classify(bare, row.as_ref()).matches(&target)
    }

    /// Remove `layer` when an earlier beta's upgrade kept it (stamped) and the caller
    /// found the bare file already the upgraded output. One log line per file.
    fn drop_kept_layer(&self, bare: &Path, layer: &Path) -> bool {
        if !fs::is_file(layer) || !Self::kept_by_upgrade(layer) {
            return false;
        }
        let Some(_guard) = self.locks.try_lock(layer) else {
            return false;
        };
        let layer_rel = rel_of(&self.library, layer).unwrap_or_default();
        if let Err(e) = fs::remove_file(layer) {
            tracing::warn!("could not remove {layer_rel}, the old OCR an upgrade kept: {e}");
            return false;
        }
        if let Some(db) = &self.db {
            let _ = db.forget_ocr_sidecar(&layer_rel);
        }
        let _ = fs::remove_file(self.backup_of(layer));
        let _ = fs::remove_dir(self.backups_dir());
        tracing::info!(
            "Removed {layer_rel}: the old OCR an earlier beta's upgrade kept as a layer; {} has replaced it",
            rel_of(&self.library, bare).unwrap_or_default()
        );
        true
    }

    /// The census side of the clean-up: this volume's layers an earlier beta's upgrade
    /// kept, once its bare file is the upgraded output. Only layer names such an upgrade
    /// gave (`mokuro`, `mokuro-old`, `-prev…`) are read; the stamp decides.
    fn sweep_kept(&self, cbz: &Path) -> usize {
        let bare = cbz.with_extension("mokuro");
        let named: Vec<PathBuf> = self
            .layer_candidates(cbz)
            .into_iter()
            .map(|(l, _)| l)
            .filter(|l| {
                let id = l
                    .file_name()
                    .and_then(|f| f.to_str())
                    .and_then(|f| f.strip_suffix(".mokuro"))
                    .and_then(|s| s.rsplit_once('.'))
                    .map_or("", |(_, g)| g);
                id == "mokuro" || id.starts_with("mokuro-old") || id.contains("-prev")
            })
            .collect();
        if named.is_empty() || !self.bare_upgraded(&bare) {
            return 0;
        }
        let n = named
            .iter()
            .filter(|l| self.drop_kept_layer(&bare, l))
            .count();
        if n > 0 {
            self.facts.sidecar_installed(cbz);
        }
        n
    }

    /// The startup side of the clean-up: every layer an earlier beta's upgrade kept, as
    /// its audit (`ocr_sidecar_upgraded`, `kept_as`) names it, is removed when it still
    /// carries the stamp and its volume's bare file is the upgraded output. Returns how
    /// many were removed.
    pub fn sweep_kept_from_audit(&self) -> usize {
        let Some(db) = &self.db else { return 0 };
        let mut q = bunko_db::AuditQuery {
            actions: vec!["ocr_sidecar_upgraded".into()],
            limit: 200,
            ..Default::default()
        };
        let plain = |rel: &str| {
            !rel.is_empty()
                && Path::new(rel)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
        };
        let mut seen: HashSet<(String, String)> = HashSet::new();
        loop {
            let Ok(page) = db.query_audit_events(&q) else {
                break;
            };
            for e in &page.events {
                let kept = e
                    .details
                    .as_deref()
                    .and_then(|d| serde_json::from_str::<Value>(d).ok())
                    .and_then(|v| v.get("kept_as").and_then(Value::as_str).map(String::from));
                let bare = e
                    .target_path
                    .as_deref()
                    .and_then(|t| t.strip_prefix(bunko_proto::ARCHIVES_ROOT))
                    .map(String::from);
                if let (Some(kept), Some(bare)) = (kept, bare)
                    && plain(&kept)
                    && plain(&bare)
                {
                    seen.insert((bare, kept));
                }
            }
            match page.next_cursor {
                Some(c) => q.cursor = Some(c),
                None => break,
            }
        }
        let mut removed = 0;
        let mut touched: HashSet<PathBuf> = HashSet::new();
        for (bare_rel, kept_rel) in seen {
            let bare = self.library.join(&bare_rel);
            let layer = self.library.join(&kept_rel);
            if fs::is_file(&layer)
                && self.bare_upgraded(&bare)
                && self.drop_kept_layer(&bare, &layer)
            {
                removed += 1;
                touched.insert(bare.with_extension("cbz"));
            }
        }
        for cbz in touched {
            if fs::is_file(&cbz) {
                self.facts.sidecar_installed(&cbz);
            }
            if let Some(rel) = rel_of(&self.library, &cbz) {
                self.census.lock().remove(&rel);
            }
        }
        removed
    }
}

impl UpgradeProbe for Upgrade {
    /// Read-only: a volume with a matching layer (`Ready`) is only recorded in the
    /// census, and the scheduler's paced job swaps it in after the walk. A walk that
    /// wrote files set off a burst of library changes as it went.
    fn wants_upgrade(&self, cbz: &Path, rel: &str) -> bool {
        matches!(self.judge(cbz, rel).1, Verdict::NeedsOcr)
    }
}

/// An upgrade job's result: swapped in as the bare file (called by the collector).
pub fn swap_in_generated(req: &CollectRequest, cbz: &Path) -> Result<PathBuf, Outcome> {
    let Some(upgrade) = req.upgrade.clone() else {
        return Err(Outcome::Failed(
            "generation upgrades are not configured".into(),
        ));
    };
    upgrade.swap(cbz, &req.result, "generated").map_err(|e| {
        if e.contains("locked") {
            Outcome::Busy(e)
        } else {
            Outcome::Failed(e)
        }
    })
}

impl super::sched::Scheduler {
    /// The census probe the walk uses, when upgrades are enabled.
    pub fn upgrade_probe(&self) -> Option<Arc<Upgrade>> {
        let up = self.deps.upgrade.clone()?;
        let enabled = up.config.lock().enabled;
        enabled.then_some(up)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // fixtures are written with std::fs
mod tests {
    use super::*;

    #[test]
    fn recipes() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("a.mokuro");
        std::fs::write(&legacy, r#"{"version":"0.2.1","pages":[]}"#).unwrap();
        assert_eq!(classify(&legacy, None).family, LEGACY);
        let composed = dir.path().join("b.mokuro");
        std::fs::write(&composed, r#"{"version":"0.2.5","ocr_engine":{"id":"hayai-nova","detector":"ppocr-manga","patch_budget":512},"pages":[]}"#).unwrap();
        let r = classify(&composed, None);
        assert!(r.matches(&("hayai-nova".into(), "ppocr-manga".into(), Some(512))));
        let junk = dir.path().join("c.mokuro");
        std::fs::write(&junk, b"nope").unwrap();
        assert_eq!(classify(&junk, None).family, UNKNOWN);
    }
}
