//! Generation upgrade (spec generation-upgrade.md): bring a volume's bare `.mokuro` up
//! to the current primary recipe, by a direct replace when a matching layer is already
//! on disk, else by an upgrade job whose result is swapped in. The new file replaces the
//! old one outright; volumes a person edited are skipped. Layers an earlier 0.7 beta kept
//! of the old file (stamped `upgraded_from_primary`) are removed once the volume is
//! upgraded.
//!
//! Deviation: weight pins are not part of the compared recipe (the server cannot know
//! the pins a processor's export carries before it runs); listing the primary's own
//! engine in `replace` therefore re-runs nothing.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bunko_core::config::UpgradeConfig;
use bunko_core::generations::Generation;
use bunko_db::{AuditDetails, Database, NewAuditEvent};
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
    SkippedEdited,
}

#[derive(Clone, Debug)]
struct CensusEntry {
    stamp: (u64, i128),
    family: String,
    verdict: Verdict,
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
    forced: Mutex<HashSet<String>>,
}

/// An audit row's details say the write left the bytes as they were.
fn audit_unchanged(details: Option<&str>) -> bool {
    details
        .and_then(|d| serde_json::from_str::<Value>(d).ok())
        .and_then(|v| v.get("unchanged").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn sqlite_utc_to_epoch(text: &str) -> Option<f64> {
    // `YYYY-MM-DD HH:MM:SS` (UTC).
    let t = text.replace(' ', "T");
    bunko_sched::py::parse_iso_timestamp(&format!("{t}Z"))
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
            forced: Mutex::new(HashSet::new()),
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

    /// Evidence a person changed the file: a user's WebDAV overwrite of it that changed
    /// its bytes (`edit`), a revert, or a provenance row older than the file by more than
    /// 2 s. Adding the file (`upload`: it did not exist before) is not an edit, nor is
    /// re-sending its exact bytes (`edit` with `unchanged`).
    fn edited(&self, bare: &Path, bare_rel: &str, row: Option<&bunko_db::OcrSidecar>) -> bool {
        if let (Some(r), Ok(meta)) = (row, std::fs::metadata(bare))
            && let Some(written) = sqlite_utc_to_epoch(&r.written_at)
        {
            let mtime = mtime_ns(&meta) as f64 / 1e9;
            if mtime > written + 2.0 {
                return true;
            }
        }
        let Some(db) = &self.db else { return false };
        let target = format!("{}{bare_rel}", bunko_proto::ARCHIVES_ROOT);
        let mut q = bunko_db::AuditQuery {
            actions: vec!["edit".into(), "ocr_sidecar_reverted".into()],
            search: Some(bare_rel.to_string()),
            limit: 200,
            ..Default::default()
        };
        // Unchanged re-sends can be many: read every page, not just the newest.
        loop {
            let Ok(page) = db.query_audit_events(&q) else {
                return false;
            };
            if page.events.iter().any(|e| {
                e.target_path.as_deref() == Some(target.as_str())
                    && e.actor_username.is_some()
                    && !(e.action == "edit" && audit_unchanged(e.details.as_deref()))
            }) {
                return true;
            }
            match page.next_cursor {
                Some(c) => q.cursor = Some(c),
                None => return false,
            }
        }
    }

    /// The rows this volume's layers were written by, read from the directory.
    fn layer_candidates(cbz: &Path) -> Vec<PathBuf> {
        let Some(dir) = cbz.parent() else {
            return Vec::new();
        };
        let stem = cbz
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Ok(rd) = std::fs::read_dir(dir) else {
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
        let Some(stamp) = super::types::stamp_of(&bare) else {
            return (UNKNOWN.into(), Verdict::Current);
        };
        let forced = self.forced.lock().contains(rel);
        if !forced && let Some(hit) = self.census.lock().get(rel).filter(|e| e.stamp == stamp) {
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
        let verdict = self.verdict(cbz, &bare, &bare_rel, &recipe, row.as_ref(), forced);
        self.census.lock().insert(
            rel.to_string(),
            CensusEntry {
                stamp,
                family: recipe.family.clone(),
                verdict: verdict.clone(),
            },
        );
        (recipe.family, verdict)
    }

    fn verdict(
        &self,
        cbz: &Path,
        bare: &Path,
        bare_rel: &str,
        recipe: &Recipe,
        row: Option<&bunko_db::OcrSidecar>,
        forced: bool,
    ) -> Verdict {
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
        if !forced && self.edited(bare, bare_rel, row) {
            return Verdict::SkippedEdited;
        }
        for layer in Self::layer_candidates(cbz) {
            let layer_rel = rel_of(&self.library, &layer).unwrap_or_default();
            let lrow = self
                .db
                .as_ref()
                .and_then(|db| db.get_ocr_sidecar(&layer_rel).ok().flatten());
            if classify(&layer, lrow.as_ref()).matches(&target)
                && self.read_from_current_archive(&layer, cbz, lrow.as_ref())
                && !self.edited(&layer, &layer_rel, lrow.as_ref())
            {
                return Verdict::Ready(layer);
            }
        }
        Verdict::NeedsOcr
    }

    /// `{families: {family: n}, ready, needs_ocr, skipped_missing_pages, skipped_edited:
    /// [volumes]}` over the volumes judged so far.
    pub fn census(&self) -> Value {
        let census = self.census.lock();
        let mut families: BTreeMap<String, u64> = BTreeMap::new();
        let (mut ready, mut needs, mut missing) = (0u64, 0u64, 0u64);
        let mut edited: Vec<String> = Vec::new();
        for (rel, e) in census.iter() {
            *families.entry(e.family.clone()).or_insert(0) += 1;
            match &e.verdict {
                Verdict::Ready(_) => ready += 1,
                Verdict::NeedsOcr => needs += 1,
                Verdict::SkippedMissingPages => missing += 1,
                Verdict::SkippedEdited => edited.push(rel.clone()),
                Verdict::Current => {}
            }
        }
        edited.sort();
        let config = self.config.lock().clone();
        json!({
            "enabled": config.enabled,
            "replace": config.replace,
            "families": families,
            "ready": ready,
            "needs_ocr": needs,
            "skipped_missing_pages": missing,
            "skipped_edited": edited.len(),
            "edited_volumes": edited,
        })
    }

    pub fn force(&self, rel: &str) {
        self.forced.lock().insert(rel.to_string());
        self.census.lock().remove(rel);
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

    /// The swap (§5), with the new content already validated and normalised at `new`:
    /// it replaces the bare file atomically, and the old file is gone. `mode`: `direct`
    /// or `generated`. Returns the bare path.
    pub fn swap(&self, cbz: &Path, new: &Path, mode: &str) -> Result<PathBuf, String> {
        let bare = cbz.with_extension("mokuro");
        let Some(_guard) = self.locks.try_lock(&bare) else {
            return Err(format!("{} is locked by a WebDAV write", bare.display()));
        };
        let rel = rel_of(&self.library, cbz).unwrap_or_default();
        let bare_rel = Self::bare_rel(&rel);
        let row = self
            .db
            .as_ref()
            .and_then(|db| db.get_ocr_sidecar(&bare_rel).ok().flatten());
        let recipe = if bare.exists() {
            classify(&bare, row.as_ref())
        } else {
            Recipe::unknown()
        };
        if mode == "direct" {
            let tmp = bare.with_file_name(format!(
                ".{}.upgrade.tmp",
                bare.file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ));
            std::fs::copy(new, &tmp)
                .map_err(|e| format!("could not copy {}: {e}", new.display()))?;
            std::fs::rename(&tmp, &bare)
                .map_err(|e| format!("could not install {}: {e}", bare.display()))?;
        } else {
            super::collect::move_into_place(new, &bare)
                .map_err(|e| format!("could not install {}: {e}", bare.display()))?;
        }
        // Bookkeeping: the old file's provenance row goes with it (the caller records the
        // new file's), or the next census would read the old recipe off it.
        if let (Some(db), Some(_)) = (&self.db, &row) {
            let _ = db.forget_ocr_sidecar(&bare_rel);
        }
        let target = self.primary.lock().clone().map(|p| p.output_affecting());
        if let Some(db) = &self.db {
            let details = AuditDetails::new()
                .with("from_recipe", recipe.to_value())
                .with(
                    "to_recipe",
                    target.map_or(Value::Null, |t| json!([t.0, t.1, t.2])),
                )
                .with("replaced", true)
                .with("mode", mode.to_string());
            let path = format!("{}{bare_rel}", bunko_proto::ARCHIVES_ROOT);
            let event = NewAuditEvent::new("ocr_sidecar_upgraded")
                .target_type("sidecar")
                .target_path(&path)
                .details(details);
            let _ = db.log_audit_event(&event);
        }
        self.forced.lock().remove(&rel);
        self.census.lock().remove(&rel);
        Ok(bare)
    }

    /// Direct replace: copy a matching layer in as the bare file. The layer is then the
    /// primary's output twice over, so it is removed when it is byte-identical to the new
    /// bare file, unless an enabled row writes it (removing it would only queue that row
    /// to write it again).
    pub fn direct_replace(&self, cbz: &Path, layer: &Path) -> Result<PathBuf, String> {
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
        let layer_name = layer
            .file_name()
            .and_then(|f| f.to_str())
            .and_then(|f| f.strip_suffix(".mokuro"))
            .and_then(|s| s.rsplit_once('.'))
            .map(|(_, g)| g.to_string());
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
            let _ = std::fs::remove_file(layer);
            if let Some(db) = &self.db {
                let _ = db.forget_ocr_sidecar(&layer_rel);
            }
        }
        self.facts.sidecar_installed(cbz);
        Ok(bare)
    }

    /// A layer an earlier 0.7 beta's upgrade kept: stamped
    /// `ocr_engine.upgraded_from_primary: true`. A person's layer, another generation's,
    /// and a forced upgrade's (left unstamped) never carry the stamp.
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
        if !bare.is_file() {
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
        if !layer.is_file() || !Self::kept_by_upgrade(layer) {
            return false;
        }
        let Some(_guard) = self.locks.try_lock(layer) else {
            return false;
        };
        let layer_rel = rel_of(&self.library, layer).unwrap_or_default();
        if let Err(e) = std::fs::remove_file(layer) {
            tracing::warn!("could not remove {layer_rel}, the old OCR an upgrade kept: {e}");
            return false;
        }
        if let Some(db) = &self.db {
            let _ = db.forget_ocr_sidecar(&layer_rel);
        }
        let _ = std::fs::remove_file(self.backup_of(layer));
        let _ = std::fs::remove_dir(self.backups_dir());
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
        let named: Vec<PathBuf> = Self::layer_candidates(cbz)
            .into_iter()
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
            if layer.is_file() && self.bare_upgraded(&bare) && self.drop_kept_layer(&bare, &layer) {
                removed += 1;
                touched.insert(bare.with_extension("cbz"));
            }
        }
        for cbz in touched {
            if cbz.is_file() {
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
    fn wants_upgrade(&self, cbz: &Path, rel: &str) -> bool {
        match self.judge(cbz, rel).1 {
            Verdict::NeedsOcr => true,
            Verdict::Ready(layer) => {
                if let Err(e) = self.direct_replace(cbz, &layer) {
                    tracing::warn!("could not upgrade {rel} directly: {e}");
                }
                false
            }
            _ => false,
        }
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
