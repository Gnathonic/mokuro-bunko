//! Generation upgrade (spec generation-upgrade.md): bring a volume's bare `.mokuro` up
//! to the current primary recipe, by a direct replace when a matching layer is already
//! on disk, else by an upgrade job whose result is swapped in. The old file is kept as a
//! layer, so the upgrade is reversible.
//!
//! Deviation: weight pins are not part of the compared recipe (the server cannot know
//! the pins a processor's export carries before it runs); listing the primary's own
//! engine in `replace` therefore re-runs nothing.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bunko_core::config::UpgradeConfig;
use bunko_core::generations::{Generation, is_layer_id, name_rejection};
use bunko_db::{AuditDetails, Database, NewAuditEvent};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::collect::{CollectRequest, Outcome, lock_with_patience};
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

    /// Evidence a person edited the bare file: an audit-logged WebDAV write by an account
    /// to that path, or a provenance row older than the file by more than 2 s.
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
        let q = bunko_db::AuditQuery {
            actions: vec![
                "upload".into(),
                "edit".into(),
                "ocr_sidecar_reverted".into(),
            ],
            search: Some(bare_rel.to_string()),
            limit: 50,
            ..Default::default()
        };
        match db.query_audit_events(&q) {
            Ok(page) => page.events.iter().any(|e| {
                e.target_path.as_deref() == Some(target.as_str()) && e.actor_username.is_some()
            }),
            Err(_) => false,
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

    /// The name the old bare file is kept under: its provenance row's generation name,
    /// `mokuro` for a legacy file, made a valid, unreserved, free layer name.
    fn kept_name(recipe: &Recipe, row: Option<&bunko_db::OcrSidecar>) -> String {
        let base = row
            .map(|r| r.generation_name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| {
                if recipe.family == LEGACY {
                    "mokuro".into()
                } else {
                    recipe.engine.clone()
                }
            });
        if is_layer_id(&base) && name_rejection(&base).is_none() {
            base
        } else {
            "mokuro-old".into()
        }
    }

    /// Step 1 of the swap: keep the old bare file as `<Volume>.<old-name>.mokuro`
    /// (`-prev`, `-prev2`… when that holds a different file). The path kept.
    fn keep_old(
        &self,
        cbz: &Path,
        bare: &Path,
        recipe: &Recipe,
        row: Option<&bunko_db::OcrSidecar>,
        stamp: bool,
    ) -> std::io::Result<PathBuf> {
        let old = read_sidecar(bare)?;
        let base = Self::kept_name(recipe, row);
        let mut n = 0;
        let kept = loop {
            let name = match n {
                0 => base.clone(),
                1 => format!("{base}-prev"),
                k => format!("{base}-prev{k}"),
            };
            let candidate = bunko_library::sidecar::with_suffix(cbz, &format!(".{name}.mokuro"));
            match read_sidecar(&candidate) {
                Err(_) => break candidate,
                Ok(existing) if existing == old || stamped_equal(&existing, &old) => {
                    break candidate;
                }
                Ok(_) => n += 1,
            }
        };
        let mut bytes = old.clone();
        if stamp
            && let Ok(mut data) = bunko_layout::json::Value::parse(&String::from_utf8_lossy(&old))
            && data.is_object()
            && data.get("ocr_engine").is_none()
        {
            let mut block = bunko_layout::json::Value::object();
            let engine = if recipe.family == LEGACY {
                "mokuro".to_string()
            } else {
                recipe.engine.clone()
            };
            let kept_name = kept
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            let generation = kept_name
                .strip_suffix(".mokuro")
                .and_then(|s| s.rsplit_once('.').map(|(_, g)| g.to_string()))
                .unwrap_or_default();
            block.set("id", bunko_layout::json::Value::Str(engine));
            block.set("generation", bunko_layout::json::Value::Str(generation));
            block.set(
                "generator",
                bunko_layout::json::Value::Str(self.generator.clone()),
            );
            block.set(
                "upgraded_from_primary",
                bunko_layout::json::Value::Bool(true),
            );
            data.set("ocr_engine", block);
            bytes = data
                .dumps(bunko_layout::json::Separators::Compact)
                .into_bytes();
        }
        if bytes != old {
            // The stamp changed the bytes: keep the original for a byte-exact revert.
            let backup = self.backup_of(&kept);
            if let Some(dir) = backup.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&backup, &old)?;
        }
        let tmp = kept.with_file_name(format!(
            ".{}.tmp",
            kept.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &kept)?;
        Ok(kept)
    }

    /// Where the unstamped original of a kept layer is saved.
    fn backup_of(&self, kept: &Path) -> PathBuf {
        use sha2::Digest;
        let rel = rel_of(&self.library, kept).unwrap_or_default();
        let digest = hex::encode(sha2::Sha256::digest(rel.as_bytes()));
        self.library
            .parent()
            .unwrap_or(&self.library)
            .join(".upgrade-originals")
            .join(format!("{}.mokuro", &digest[..32]))
    }

    /// The swap (§5), with the new content already validated and normalised at `new`.
    /// `mode`: `direct` or `generated`. Returns the bare path.
    pub fn swap(
        &self,
        cbz: &Path,
        new: &Path,
        mode: &str,
        forced_edit: bool,
    ) -> Result<PathBuf, String> {
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
        let kept = if bare.exists() {
            Some(
                self.keep_old(cbz, &bare, &recipe, row.as_ref(), !forced_edit)
                    .map_err(|e| format!("could not keep the old sidecar: {e}"))?,
            )
        } else {
            None
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
        // Bookkeeping: the old row follows the kept file.
        if let (Some(db), Some(kept), Some(mut old)) = (&self.db, &kept, row.clone()) {
            old.sidecar_path = rel_of(&self.library, kept).unwrap_or_default();
            if let Some(name) = kept
                .file_name()
                .and_then(|f| f.to_str())
                .and_then(|f| f.strip_suffix(".mokuro"))
                .and_then(|s| s.rsplit_once('.'))
                .map(|(_, g)| g.to_string())
            {
                old.generation_name = name;
            }
            let _ = db.record_ocr_sidecar(&old);
        }
        let target = self.primary.lock().clone().map(|p| p.output_affecting());
        if let Some(db) = &self.db {
            let details = AuditDetails::new()
                .with("from_recipe", recipe.to_value())
                .with(
                    "to_recipe",
                    target.map_or(Value::Null, |t| json!([t.0, t.1, t.2])),
                )
                .with(
                    "kept_as",
                    kept.as_ref()
                        .and_then(|k| rel_of(&self.library, k))
                        .map_or(Value::Null, Value::String),
                )
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

    /// Direct replace: copy a matching layer in as the bare file; drop the layer when
    /// its row is disabled or gone (it would otherwise be kept twice).
    pub fn direct_replace(&self, cbz: &Path, layer: &Path) -> Result<PathBuf, String> {
        let rel = rel_of(&self.library, cbz).unwrap_or_default();
        let forced = self.forced.lock().contains(&rel);
        let bare = self.swap(cbz, layer, "direct", forced)?;
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
        if !enabled {
            let _ = std::fs::remove_file(layer);
            if let Some(db) = &self.db {
                let _ = db.forget_ocr_sidecar(&layer_rel);
            }
        }
        self.facts.sidecar_installed(cbz);
        Ok(bare)
    }

    /// Revert: the newest kept layer (stamped `upgraded_from_primary`, or named in the
    /// last upgrade's audit) goes back to the bare file; the current one is kept as a
    /// layer. Audited as a person's edit, so the census leaves the volume alone after.
    pub fn revert(&self, cbz: &Path, actor: Option<&str>) -> Result<PathBuf, String> {
        let bare = cbz.with_extension("mokuro");
        let mut kept: Vec<(i128, PathBuf)> = Vec::new();
        for layer in Self::layer_candidates(cbz) {
            let loaded = bunko_library::sidecar::load_sidecar(&layer);
            let stamped = loaded
                .data
                .as_ref()
                .and_then(|d| d.get("ocr_engine"))
                .and_then(|b| b.as_object())
                .and_then(|b| b.get("upgraded_from_primary"))
                .is_some_and(|v| v.is_truthy());
            let named = layer.file_name().is_some_and(|f| {
                let f = f.to_string_lossy();
                f.contains(".mokuro-prev")
                    || f.ends_with(".mokuro.mokuro")
                    || f.ends_with(".mokuro-old.mokuro")
            });
            if stamped || named {
                let m = std::fs::metadata(&layer).map(|m| mtime_ns(&m)).unwrap_or(0);
                kept.push((m, layer));
            }
        }
        kept.sort();
        let Some((_, layer)) = kept.pop() else {
            return Err("there is no kept sidecar to revert to".into());
        };
        let Some(_guard) = lock_with_patience(&self.locks, &bare) else {
            return Err(format!("{} is locked by a WebDAV write", bare.display()));
        };
        let current =
            read_sidecar(&bare).map_err(|e| format!("could not read {}: {e}", bare.display()))?;
        let primary = self.primary.lock().clone();
        let back_name = primary
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "upgraded".into());
        let mut slot = bunko_library::sidecar::with_suffix(cbz, &format!(".{back_name}.mokuro"));
        let mut n = 1;
        while slot.exists() && read_sidecar(&slot).ok().as_deref() != Some(current.as_slice()) {
            slot = bunko_library::sidecar::with_suffix(
                cbz,
                &format!(
                    ".{back_name}-prev{}.mokuro",
                    if n == 1 { String::new() } else { n.to_string() }
                ),
            );
            n += 1;
        }
        std::fs::write(&slot, &current)
            .map_err(|e| format!("could not keep {}: {e}", slot.display()))?;
        let backup = self.backup_of(&layer);
        let original = match read_sidecar(&backup) {
            Ok(bytes) => bytes,
            Err(_) => read_sidecar(&layer)
                .map_err(|e| format!("could not read {}: {e}", layer.display()))?,
        };
        let tmp = bare.with_file_name(format!(
            ".{}.revert.tmp",
            bare.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        std::fs::write(&tmp, &original)
            .map_err(|e| format!("could not restore {}: {e}", bare.display()))?;
        std::fs::rename(&tmp, &bare)
            .map_err(|e| format!("could not restore {}: {e}", bare.display()))?;
        let _ = std::fs::remove_file(&layer);
        let _ = std::fs::remove_file(&backup);
        let rel = rel_of(&self.library, cbz).unwrap_or_default();
        if let Some(db) = &self.db {
            let path = format!("{}{}", bunko_proto::ARCHIVES_ROOT, Self::bare_rel(&rel));
            let details = AuditDetails::new()
                .with(
                    "restored_from",
                    rel_of(&self.library, &layer).unwrap_or_default(),
                )
                .with("kept_as", rel_of(&self.library, &slot).unwrap_or_default());
            let event = NewAuditEvent::new("ocr_sidecar_reverted")
                .actor(actor)
                .target_type("sidecar")
                .target_path(&path)
                .details(details);
            let _ = db.log_audit_event(&event);
        }
        self.census.lock().remove(&rel);
        self.facts.sidecar_installed(cbz);
        Ok(bare)
    }
}

fn stamped_equal(a: &[u8], b: &[u8]) -> bool {
    // A kept copy differs from the bare file only by the stamp we added.
    let strip = |bytes: &[u8]| -> Option<bunko_layout::json::Value> {
        let mut v = bunko_layout::json::Value::parse(&String::from_utf8_lossy(bytes)).ok()?;
        if v.get("ocr_engine")
            .and_then(|b| b.get("upgraded_from_primary"))
            .is_some()
            && let bunko_layout::json::Value::Object(items) = &mut v
        {
            items.retain(|(k, _)| k != "ocr_engine");
        }
        Some(v)
    };
    matches!((strip(a), strip(b)), (Some(x), Some(y)) if x.dumps(bunko_layout::json::Separators::Compact) == y.dumps(bunko_layout::json::Separators::Compact))
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
    let rel = rel_of(&req.library, cbz).unwrap_or_default();
    let forced = upgrade.forced.lock().contains(&rel);
    upgrade
        .swap(cbz, &req.result, "generated", forced)
        .map_err(|e| {
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
