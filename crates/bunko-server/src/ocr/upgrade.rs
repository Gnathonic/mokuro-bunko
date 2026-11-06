//! Generation upgrade (spec generation-upgrade.md): bring a volume's bare `.mokuro` up
//! to the current primary recipe, by a direct replace when a matching layer is already
//! there, else by an upgrade job whose result is swapped in. The bare file is model
//! output (D4): it is never skipped for looking edited, and it is replaced outright,
//! with no copy kept (owner decision, 2026-11-01). On a remote store the replace is
//! never a permanent delete (D16): a provider whose replace makes a new node has the
//! old one trashed; on a local library the atomic rename replaces it. A layer is
//! swapped in only on proof that it is Bunko's own output (its content seal). The same
//! code runs on every library store.
//! Outdated means another recipe listed under `replace`, or the primary's own recipe at
//! a lower `ocr_engine.revision` than the engine's (absent = 1). A file a reader marked
//! EDITED (an `edited` block) is never replaced nor swapped in, whatever it carries: its
//! newer OCR goes beside it as `<Vol>.<primary name>.mokuro` (critique C4-B6). A
//! generation layer Bunko wrote at a lower revision is owed again (critique C10).
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
use bunko_core::sidecar_names::{self, VolumeFile};
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
/// The family of a file a reader marked EDITED (an `edited` block): a person's text,
/// never replaced, never swapped in (D4: edits never touch the model's main file).
pub const EDITED: &str = "edited";

/// What produced a bare sidecar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipe {
    /// Engine id, `mokuro-legacy`, `edited` or `unknown`.
    pub family: String,
    pub engine: String,
    pub detector: Option<String>,
    pub patch_budget: Option<u32>,
    /// `ocr_engine.revision` (1 when absent, as in every 0.7 file).
    pub revision: u32,
    /// The file carries an `edited` block.
    pub edited: bool,
    /// An EDITED file's recipe beneath the edit (what the model made before the reader
    /// changed it): whether newer OCR is owed beside it (critique C4-B6). A reader's
    /// file names none (no `ocr_engine` beside `edited`, and its PUT forgot the row), so
    /// this is `mokuro-legacy` by its `version`, or `unknown`: one primary-layer run is
    /// owed under the default `replace`, whatever the base was.
    pub under: Option<Box<Recipe>>,
}

impl Recipe {
    fn unknown() -> Recipe {
        Recipe {
            family: UNKNOWN.into(),
            engine: String::new(),
            detector: None,
            patch_budget: None,
            revision: 1,
            edited: false,
            under: None,
        }
    }

    pub fn matches(&self, target: &(String, String, Option<u32>)) -> bool {
        !self.edited
            && self.family != UNKNOWN
            && self.family != LEGACY
            && self.engine == target.0
            && self.detector.as_deref() == Some(target.1.as_str())
            && self.patch_budget == target.2
    }

    /// The target recipe at its engine's current revision (`target_rev`) or newer.
    pub fn is_current(&self, target: &(String, String, Option<u32>), target_rev: u32) -> bool {
        self.matches(target) && self.revision >= target_rev
    }

    pub fn to_value(&self) -> Value {
        json!([self.engine, self.detector, self.patch_budget])
    }
}

/// `classify`: the recipe of a sidecar, from its provenance row, else its `ocr_engine`
/// block, else mokuro's top-level `version` (legacy), else unknown.
pub fn classify(path: &Path, row: Option<&bunko_db::OcrSidecar>) -> Recipe {
    match bunko_library::sidecar::load_sidecar(path).data {
        Some(data) => classify_data(&data, row),
        None => Recipe::unknown(),
    }
}

/// [`classify`] of a plain sidecar's bytes already read.
fn classify_bytes(bytes: &[u8], row: Option<&bunko_db::OcrSidecar>) -> Recipe {
    let parsed = std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| bunko_library::pyjson::parse(bunko_library::pyjson::strip_bom(text)).ok());
    match parsed {
        Some(bunko_library::pyjson::JsonValue::Object(data)) => classify_data(&data, row),
        _ => Recipe::unknown(),
    }
}

fn classify_data(
    data: &bunko_library::pyjson::JsonObject,
    row: Option<&bunko_db::OcrSidecar>,
) -> Recipe {
    // Provenance precedence (interop §1): EDITED, then GENERATED, then LEGACY.
    if data.get("edited").and_then(|v| v.as_object()).is_some() {
        return Recipe {
            family: EDITED.into(),
            edited: true,
            under: Some(Box::new(classify_unedited(data, row))),
            ..Recipe::unknown()
        };
    }
    classify_unedited(data, row)
}

/// [`classify_data`] with any `edited` block ignored.
fn classify_unedited(
    data: &bunko_library::pyjson::JsonObject,
    row: Option<&bunko_db::OcrSidecar>,
) -> Recipe {
    let block = data.get("ocr_engine").and_then(|b| b.as_object());
    let revision = block
        .and_then(|b| b.get("revision"))
        .and_then(|v| v.as_num())
        .and_then(|n| match n {
            bunko_library::pyjson::JsonNum::Int(i) if *i >= 0 => {
                Some(u32::try_from(*i).unwrap_or(u32::MAX))
            }
            _ => None,
        })
        .unwrap_or(1);
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
            revision,
            edited: false,
            under: None,
        };
    }
    if let Some(engine) = block_str("id") {
        if engine == "mokuro" {
            return Recipe {
                family: LEGACY.into(),
                engine: LEGACY.into(),
                detector: Some("ctd".into()),
                ..Recipe::unknown()
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
            revision,
            edited: false,
            under: None,
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
            ..Recipe::unknown()
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
    /// A reader marked it EDITED: never replaced (a newer OCR goes beside it as a layer).
    Edited,
}

#[derive(Clone, Debug)]
struct CensusEntry {
    stamp: (u64, i128),
    /// The bare file's content version when it was judged (remote stores; `None` on a
    /// local library, where `stamp` pins it). The swap replaces only this version.
    version: Option<Version>,
    family: String,
    verdict: Verdict,
    /// [`Verdict::Edited`] over an outdated recipe: newer OCR is owed beside it.
    edited_outdated: bool,
}

/// A layer's [`Upgrade::layer_outdated`] verdict and the stat `(size, mtime_ns)` it was
/// judged at.
type LayerVerdict = ((u64, i128), bool);

/// A test hook run inside a swap (see [`Upgrade::set_swap_hook`]).
pub type SwapHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// The `ocr_engine.id` a sidecar's own bytes carry at top level (Bunko's stamp), if any.
fn stamp_id(data: &Value) -> Option<String> {
    data.get("ocr_engine")
        .and_then(Value::as_object)
        .and_then(|b| b.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// `ocr_engine.revision` of a block: a non-negative integer, else 1 (absent in every 0.7
/// file; [`bunko_layout::sidecar::engine_revision`]).
pub fn block_revision(block: &serde_json::Map<String, Value>) -> u32 {
    block
        .get("revision")
        .and_then(Value::as_u64)
        .and_then(|r| u32::try_from(r).ok())
        .unwrap_or(1)
}

/// `(ocr_engine.id, revision)` of a parsed sidecar, when it carries a block with an id.
pub fn engine_of(data: &Value) -> Option<(String, u32)> {
    let block = data.get("ocr_engine")?.as_object()?;
    Some((stamp_id(data)?, block_revision(block)))
}

/// The plain sidecar `<stem><suffix>` of `cbz` as Bunko reads and writes it: the
/// spelling the folder lists ([`super::owed::listed_sidecar_paths`]: `vol 1.hayai.mokuro`
/// for `Vol 1.cbz`, the winner of several), the one an install replaces, so whether a
/// layer is owed again is judged on the very file it would write, on every host. `None`
/// when nothing may be written for the archive (it owes nothing).
fn existing_plain(cbz: &Path, suffix: &str) -> Option<PathBuf> {
    super::owed::listed_sidecar_paths(cbz, suffix).map(|(plain, _)| plain)
}

/// Bunko's own output of `engine` (sealed, no `edited` block) at a revision below
/// `revision`: the one file an install of `engine` at `revision` replaces in place.
/// Anything else at its destination (a person's file, an edit, another engine's, an
/// unsealed or a same or newer revision) is kept.
pub fn superseded_own_output(bytes: &[u8], engine: &str, revision: u32) -> bool {
    let Ok(data) = serde_json::from_slice::<Value>(strip_bom(bytes)) else {
        return false;
    };
    data.get("edited").is_none()
        && engine_of(&data).is_some_and(|(id, r)| id == engine && r < revision)
        && bunko_layout::seal::is_sealed(bytes)
}

/// Write `bytes` to a staging file at `tmp` (on a remote store: local until published;
/// `fs::write` would publish the temporary name itself), then publish it as `dest`.
pub(crate) fn stage_and_publish(
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

/// The bytes without a leading UTF-8 BOM.
pub(super) fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes)
}

fn strip_bom_str(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// What a swap installs as the main sidecar.
enum Install<'a> {
    /// An upgrade job's result, validated and normalised: moved into place.
    Generated(&'a Path),
    /// A layer's bytes, proven Bunko's own ([`Upgrade::proven_layer`]): exactly these
    /// are published, never the layer file read again.
    Direct(Vec<u8>),
}

impl Install<'_> {
    fn mode(&self) -> &'static str {
        match self {
            Install::Generated(_) => "generated",
            Install::Direct(_) => "direct",
        }
    }
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
    /// [`Self::layer_outdated`]'s verdicts.
    layers: Mutex<HashMap<PathBuf, LayerVerdict>>,
    /// Tests only: engine revisions in place of `ENGINE_REVISIONS`.
    revisions: Mutex<BTreeMap<String, u32>>,
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
            layers: Mutex::new(HashMap::new()),
            revisions: Mutex::new(BTreeMap::new()),
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
        self.layers.lock().clear();
    }

    /// `engine`'s current output revision ([`bunko_layout::sidecar::engine_revision`]).
    pub fn target_revision(&self, engine: &str) -> u32 {
        self.revisions
            .lock()
            .get(engine)
            .copied()
            .unwrap_or_else(|| bunko_layout::sidecar::engine_revision(engine))
    }

    /// Tests only: `engine` at `revision`, as a build that bumped it would have it.
    #[doc(hidden)]
    pub fn set_engine_revision(&self, engine: &str, revision: u32) {
        self.revisions.lock().insert(engine.to_string(), revision);
        self.census.lock().clear();
        self.layers.lock().clear();
    }

    /// Critique C10: `row`'s generation layer is Bunko's own output of the row's engine
    /// at a lower revision than the engine's ([`superseded_own_output`]), so the row is
    /// owed again and its result replaces the file in place. A file that is not Bunko's
    /// own is never owed: its result could only be discarded, every walk. Cached by the
    /// layer's stat (one capped read per change).
    pub fn layer_outdated(&self, cbz: &Path, row: &Generation) -> bool {
        if row.primary {
            return false;
        }
        existing_plain(cbz, &row.sidecar_suffix())
            .is_some_and(|plain| self.own_outdated(&plain, &row.engine))
    }

    /// The file at `plain` is Bunko's own output of `engine` at a revision below the
    /// engine's ([`superseded_own_output`]), cached by its stat. False when absent.
    fn own_outdated(&self, plain: &Path, engine: &str) -> bool {
        let Some(meta) = fs::metadata(plain).ok().filter(|m| m.is_file()) else {
            return false;
        };
        let stamp = (meta.len(), mtime_ns(&meta));
        if let Some((at, hit)) = self.layers.lock().get(plain)
            && *at == stamp
        {
            return *hit;
        }
        let target = self.target_revision(engine);
        let outdated =
            read_sidecar(plain).is_ok_and(|bytes| superseded_own_output(&bytes, engine, target));
        self.layers
            .lock()
            .insert(plain.to_path_buf(), (stamp, outdated));
        outdated
    }

    /// The primary generation as the layer an EDITED main's new OCR goes to:
    /// `<Vol>.<primary.name>.mokuro` (critique C4-B6).
    fn edited_main_layer(&self) -> Option<Generation> {
        let mut layer = self.primary.lock().clone()?;
        layer.primary = false;
        Some(layer)
    }

    /// An EDITED main over an outdated recipe ([`Recipe::under`]) is owed newer OCR
    /// beside it, as the layer [`Self::edited_main_layer`], while that layer is missing in
    /// every spelling or is Bunko's own output at an older revision. A file of anyone else's at that name
    /// stays, and owes nothing: the result could only be discarded, every walk.
    fn edited_owes_layer(&self, cbz: &Path, rel: &str) -> bool {
        let outdated = self
            .census
            .lock()
            .get(rel)
            .is_some_and(|e| e.verdict == Verdict::Edited && e.edited_outdated);
        let Some(layer) = self.edited_main_layer().filter(|_| outdated) else {
            return false;
        };
        let Some(plain) = existing_plain(cbz, &layer.sidecar_suffix()) else {
            return false;
        };
        if !super::owed::row_done(cbz, &layer, None) {
            return true;
        }
        self.own_outdated(&plain, &layer.engine)
    }

    /// An upgrade job's result for a volume whose main a reader marked EDITED (D4: edits
    /// never touch the model's main file): stamped as the layer
    /// [`Self::edited_main_layer`] (`ocr_engine.generation` = the primary's name),
    /// sealed again, and published beside the main under the collector's reconcile
    /// rules (a missing name is created; Bunko's own sealed older revision there is
    /// replaced; anything else stays and the result is discarded). Under the write locks
    /// of the main and the layer, the main is judged again first: still EDITED, or the
    /// result is discarded (the next walk judges it afresh). The main is never touched.
    pub fn install_for_edited(&self, cbz: &Path, result: &Path) -> Result<PathBuf, Outcome> {
        let layer_row = self
            .edited_main_layer()
            .ok_or_else(|| Outcome::Failed("no primary generation".into()))?;
        let rel = rel_of(&self.library, cbz).unwrap_or_default();
        let bare = bunko_library::sidecar::sibling(cbz, ".mokuro")
            .ok_or_else(|| Outcome::Failed("the volume is shadowed by another spelling".into()))?;
        let (plain, gz) = super::owed::listed_sidecar_paths(cbz, &layer_row.sidecar_suffix())
            .ok_or_else(|| Outcome::Superseded(super::collect::SHADOWED.into()))?;
        let locked =
            |p: &Path| Outcome::Busy(format!("{} is locked by a WebDAV write", p.display()));
        let _bare_guard = self.locks.try_lock(&bare).ok_or_else(|| locked(&bare))?;
        let _guard = self.locks.try_lock(&plain).ok_or_else(|| locked(&plain))?;
        self.census.lock().remove(&rel);
        if self.judge(cbz, &rel).1 != Verdict::Edited {
            return Err(Outcome::Superseded(format!(
                "{} is no longer marked edited; the next walk judges it again",
                bare.display()
            )));
        }
        let failed = |e: String| Outcome::Failed(format!("the new OCR could not be stamped: {e}"));
        // unmounted: the result is in the `.processing` spool.
        #[allow(clippy::disallowed_methods)]
        let text = std::fs::read_to_string(result).map_err(|e| failed(e.to_string()))?;
        let mut data = bunko_layout::json::Value::parse(strip_bom_str(&text))
            .map_err(|e| failed(e.to_string()))?;
        if !data.is_object() {
            return Err(failed("not a JSON object".into()));
        }
        bunko_layout::sidecar::stamp_layer(
            &mut data,
            &bunko_layout::sidecar::LayerStamp {
                engine: layer_row.engine.clone(),
                generator: self.generator.clone(),
                generation: layer_row.name.clone(),
            },
        );
        if let Some(block) = data.get_mut("ocr_engine") {
            block.set_default("revision", bunko_layout::json::Value::Int(1));
        }
        // The block moved, so the seal is taken again (it covers the block).
        let sealed = bunko_layout::seal::seal(&mut data)
            .ok_or_else(|| failed("it has no JSON form to seal".into()))?;
        bunko_layout::sidecar::write_atomic(result, &sealed).map_err(|e| failed(e.to_string()))?;
        let facts = serde_json::from_str::<Value>(&sealed)
            .ok()
            .as_ref()
            .and_then(engine_of);
        let mode = match super::collect::reconcile_existing(&plain, &gz, facts.as_ref()) {
            super::collect::Existing::Absent => Mode::CreateNew,
            super::collect::Existing::Keep => {
                return Err(Outcome::Superseded(super::collect::SUPERSEDED.into()));
            }
            super::collect::Existing::Replace(base) => Mode::Replace { base },
        };
        let intent = WriteIntent::new(mode, Origin::Upgrade, Durability::Sync);
        if let Err(e) = super::collect::move_into_place(result, &plain, &intent) {
            let what = format!("could not install {}: {e}", plain.display());
            return Err(
                if bunko_store::store_for(&plain).is_some_and(|s| s.caps().remote) {
                    Outcome::Held(what)
                } else {
                    Outcome::Failed(what)
                },
            );
        }
        tracing::info!(
            "Wrote the new OCR of {rel} beside its edited main sidecar, as {}",
            rel_of(&self.library, &plain).unwrap_or_default()
        );
        Ok(plain)
    }

    fn bare_rel(rel: &str) -> String {
        format!("{}.mokuro", &rel[..rel.len() - 4])
    }

    /// The volume's layer sidecars (plain `.mokuro`), sorted, as the directory lists them
    /// and rule v1 sweeps them ([`sidecar_names::volume_files`]).
    fn layer_files(cbz: &Path) -> Vec<PathBuf> {
        let Some(dir) = cbz.parent() else {
            return Vec::new();
        };
        let Some(name) = cbz.file_name().and_then(|n| n.to_str()) else {
            return Vec::new();
        };
        let Ok(rd) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let names: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        let listed: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut out: Vec<PathBuf> = sidecar_names::volume_files(name, &listed)
            .into_iter()
            .filter(|(_, file)| matches!(file, VolumeFile::Layer { gz: false, .. }))
            .map(|(n, _)| dir.join(n))
            .collect();
        out.sort();
        out
    }

    /// A layer file's id (`<stem>.<id>.mokuro`, the suffix in any case).
    fn layer_id(layer: &Path) -> Option<String> {
        layer
            .file_name()
            .and_then(|f| f.to_str())
            .and_then(|f| sidecar_names::strip_suffix_ci(f, ".mokuro"))
            .and_then(|s| s.rsplit_once('.'))
            .map(|(_, g)| g.to_string())
    }

    /// The layers the upgrade may read (D4): those that carry an `ocr_engine` stamp or
    /// have an `ocr_sidecars` row, with that row. A layer with neither (a reader's
    /// edit, any hand-made layer) is never a candidate, nor ever deleted. Being a
    /// candidate is not enough to be swapped in: see [`Self::proven_layer`].
    fn layer_candidates(&self, cbz: &Path) -> Vec<(PathBuf, Option<bunko_db::OcrSidecar>)> {
        Self::layer_files(cbz)
            .into_iter()
            .filter_map(|layer| {
                let layer_rel = rel_of(&self.library, &layer).unwrap_or_default();
                let row = self
                    .db
                    .as_ref()
                    .and_then(|db| db.get_ocr_sidecar(&layer_rel).ok().flatten());
                let data = read_sidecar(&layer)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(strip_bom(&b)).ok());
                // A reader's EDITED layer is a person's text, whatever stamp it kept.
                if data.as_ref().is_some_and(|d| d.get("edited").is_some()) {
                    return None;
                }
                (row.is_some() || data.as_ref().and_then(stamp_id).is_some())
                    .then_some((layer, row))
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
        // The bare file in the spelling the folder lists (`V.MOKURO`); nothing for a
        // volume shadowed by another spelling of itself.
        let Some(bare) = bunko_library::sidecar::sibling(cbz, ".mokuro") else {
            return (UNKNOWN.into(), Verdict::Current);
        };
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
        let upgraded = self.primary.lock().as_ref().is_some_and(|p| {
            recipe.is_current(&p.output_affecting(), self.target_revision(&p.engine))
        });
        if upgraded {
            self.sweep_kept(cbz);
        }
        let verdict = self.verdict(cbz, &recipe);
        let edited_outdated =
            verdict == Verdict::Edited && recipe.under.as_deref().is_some_and(|u| self.outdated(u));
        self.census.lock().insert(
            rel.to_string(),
            CensusEntry {
                stamp,
                version,
                family: recipe.family.clone(),
                verdict: verdict.clone(),
                edited_outdated,
            },
        );
        (recipe.family, verdict)
    }

    /// The policy: `recipe` is the primary's own at a lower revision (the engine's
    /// output changed, whatever `replace` lists), or another recipe `replace` lists.
    /// Never an EDITED file, an unknown one, or a current one; never with upgrades off.
    fn outdated(&self, recipe: &Recipe) -> bool {
        let config = self.config.lock().clone();
        let Some(primary) = self.primary.lock().clone() else {
            return false;
        };
        if !config.enabled || recipe.edited || recipe.family == UNKNOWN {
            return false;
        }
        let target = primary.output_affecting();
        if recipe.is_current(&target, self.target_revision(&primary.engine)) {
            return false;
        }
        recipe.matches(&target) || config.replace.contains(&recipe.family)
    }

    fn verdict(&self, cbz: &Path, recipe: &Recipe) -> Verdict {
        let enabled = self.config.lock().enabled;
        let Some(primary) = self.primary.lock().clone() else {
            return Verdict::Current;
        };
        if !enabled {
            return Verdict::Current;
        }
        if recipe.edited {
            return Verdict::Edited;
        }
        if !self.outdated(recipe) {
            return Verdict::Current;
        }
        let target = primary.output_affecting();
        if self.facts.missing_pages(cbz) > 0 {
            return Verdict::SkippedMissingPages;
        }
        for (layer, lrow) in self.layer_candidates(cbz) {
            if self.read_from_current_archive(&layer, cbz, lrow.as_ref())
                && self.proven_layer(&layer, &target).is_some()
            {
                return Verdict::Ready(layer);
            }
        }
        Verdict::NeedsOcr
    }

    /// D4's proof that `bytes` hold the OCR Bunko installed: their content seal
    /// ([`bunko_layout::seal`], `ocr_engine.seal`) checks. An `ocr_engine` stamp alone
    /// proves nothing: anyone who can write the file (a reader's edit, a WebDAV PUT, a
    /// cloud upload) can add one, and a re-upload of edited text can keep one. The seal
    /// covers the pages and the block themselves, so it travels with the file (another
    /// Bunko, a moved library, a new instance on the same cloud folder, a rename) and an
    /// edit of either breaks it. A layer without proof is treated as a person's: never
    /// copied in as the main sidecar.
    fn proven_own(bytes: &[u8]) -> bool {
        bunko_layout::seal::is_sealed(bytes)
    }

    /// A layer that may become the main sidecar: its bytes (on a remote store, one
    /// pinned version), when they are proven Bunko's own ([`Self::proven_own`]) and
    /// their own `ocr_engine` block (never the row) names the target recipe. A row alone
    /// is not enough (a reader's push of a layer drops the stamp, and `classify` would
    /// read the row first); a stamp alone is not either (a forged one would make a
    /// person's file the model output).
    fn proven_layer(
        &self,
        layer: &Path,
        target: &(String, String, Option<u32>),
    ) -> Option<Vec<u8>> {
        let meta = fs::metadata(layer).ok().filter(|m| m.is_file())?;
        let remote = bunko_store::store_for(layer).filter(|s| s.caps().remote);
        let bytes = match (remote, meta.version()) {
            (Some(store), Some(version)) => {
                let pinned = store.local_path(layer, Some(version)).ok()?;
                read_sidecar(pinned.path()).ok()?
            }
            (Some(_), None) => return None,
            (None, _) => read_sidecar(layer).ok()?,
        };
        let data = serde_json::from_slice::<Value>(strip_bom(&bytes)).ok();
        let stamped = data.as_ref().and_then(stamp_id).is_some();
        let edited = data.as_ref().is_some_and(|d| d.get("edited").is_some());
        let current =
            classify_bytes(&bytes, None).is_current(target, self.target_revision(&target.0));
        (stamped && !edited && current && Self::proven_own(&bytes)).then_some(bytes)
    }

    /// `{enabled, replace, direct, families: {family: n}, ready, needs_ocr,
    /// skipped_missing_pages, edited}` over the volumes judged so far.
    pub fn census(&self) -> Value {
        let census = self.census.lock();
        let mut families: BTreeMap<String, u64> = BTreeMap::new();
        let (mut ready, mut needs, mut missing, mut edited) = (0u64, 0u64, 0u64, 0u64);
        for e in census.values() {
            *families.entry(e.family.clone()).or_insert(0) += 1;
            match &e.verdict {
                Verdict::Ready(_) => ready += 1,
                Verdict::NeedsOcr => needs += 1,
                Verdict::SkippedMissingPages => missing += 1,
                Verdict::Edited => edited += 1,
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
            "edited": edited,
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
    /// write lock, just before the install (and before the LocalStore re-check of the
    /// bare file's stat).
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

    /// The swap (§5, D4), with the new content already validated and normalised at
    /// `new`. Under the write lock of the bare file, which must still be the version
    /// the census judged, the new file replaces exactly that version outright, with no
    /// copy kept. Any failure leaves the bare file as it was, and the next census
    /// retries. `mode`:
    /// `direct` (`new` is a layer, which must be proven: [`Self::proven_layer`]) or
    /// `generated`. Returns the bare path.
    pub fn swap(&self, cbz: &Path, new: &Path, mode: &str) -> Result<PathBuf, String> {
        if mode == "direct" {
            let bytes = self.layer_source(new)?;
            return self.swap_install(cbz, Install::Direct(bytes));
        }
        self.swap_install(cbz, Install::Generated(new))
    }

    /// The bytes of `layer` a direct replace may install, or why it may not.
    fn layer_source(&self, layer: &Path) -> Result<Vec<u8>, String> {
        let target = self
            .primary
            .lock()
            .clone()
            .map(|p| p.output_affecting())
            .ok_or("no primary generation")?;
        self.proven_layer(layer, &target).ok_or_else(|| {
            format!(
                "{} is not proven to be Bunko's own output carrying the primary recipe's ocr_engine stamp",
                layer.display()
            )
        })
    }

    fn swap_install(&self, cbz: &Path, install: Install<'_>) -> Result<PathBuf, String> {
        let bare = bunko_library::sidecar::sibling(cbz, ".mokuro")
            .ok_or_else(|| "the volume is shadowed by another spelling".to_string())?;
        let Some(_guard) = self.locks.try_lock(&bare) else {
            return Err(format!("{} is locked by a WebDAV write", bare.display()));
        };
        let rel = rel_of(&self.library, cbz).unwrap_or_default();
        let result = self.swap_locked(cbz, &bare, &rel, &install);
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
        install: &Install<'_>,
    ) -> Result<PathBuf, String> {
        let mode = install.mode();
        let bare_rel = Self::bare_rel(rel);
        let row = self
            .db
            .as_ref()
            .and_then(|db| db.get_ocr_sidecar(&bare_rel).ok().flatten());
        let target = self.primary.lock().clone().map(|p| p.output_affecting());
        let (judged, recipe) = if fs::exists(bare) {
            let entry = self.judged(cbz, rel, bare)?;
            (Some(entry), classify(bare, row.as_ref()))
        } else {
            (None, Recipe::unknown())
        };
        let hook = self.swap_hook.lock().clone();
        if let Some(hook) = hook {
            hook(bare);
        }
        let mode_of = match &judged {
            Some(entry) => {
                // LocalStore: nothing pins the replace, so the stat is checked again
                // right before it (a mismatch: skip).
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
        let installed = match install {
            Install::Direct(bytes) => {
                stage_and_publish(&tmp_beside(bare, "upgrade"), bare, bytes, &intent)
            }
            Install::Generated(new) => super::collect::move_into_place(new, bare, &intent),
        };
        installed.map_err(|e| format!("could not install {}: {e}", bare.display()))?;
        // Bookkeeping: the old file's provenance row goes with it (the caller records the
        // new file's), or the next census would read the old recipe off it.
        if let (Some(db), Some(_)) = (&self.db, &row) {
            let _ = db.forget_ocr_sidecar(&bare_rel);
        }
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
        self.census.lock().remove(rel);
        Ok(bare.to_path_buf())
    }

    /// Direct replace: copy a matching layer in as the bare file. Only a layer proven to
    /// be Bunko's own bytes whose own file carries the primary's `ocr_engine` stamp
    /// qualifies ([`Self::proven_layer`]; its row alone, or a stamp alone, does not), and
    /// exactly the bytes proven are installed. The layer is then the primary's output
    /// twice over, so it is removed when it is byte-identical to the new bare file,
    /// unless an enabled row writes it (removing it would only queue that row to write
    /// it again).
    pub fn direct_replace(&self, cbz: &Path, layer: &Path) -> Result<PathBuf, String> {
        let bytes = self.layer_source(layer)?;
        let rel = rel_of(&self.library, cbz).unwrap_or_default();
        let bare = self.swap_install(cbz, Install::Direct(bytes))?;
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
            // The proof moves with the bytes: the new bare file is exactly them, seal
            // included (it covers the block and pages, which a promotion leaves as they
            // are, so it still checks on the main sidecar).
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
    /// and what a 0.7 beta's forced upgrade kept (all unstamped) never carry it.
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
        let Some(primary) = self.primary.lock().clone() else {
            return false;
        };
        let (target, target_rev) = (
            primary.output_affecting(),
            self.target_revision(&primary.engine),
        );
        if !fs::is_file(bare) {
            return false;
        }
        let bare_rel = rel_of(&self.library, bare).unwrap_or_default();
        let row = self
            .db
            .as_ref()
            .and_then(|db| db.get_ocr_sidecar(&bare_rel).ok().flatten());
        classify(bare, row.as_ref()).is_current(&target, target_rev)
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
        let Some(bare) = bunko_library::sidecar::sibling(cbz, ".mokuro") else {
            return 0;
        };
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
        match self.judge(cbz, rel).1 {
            Verdict::NeedsOcr => true,
            Verdict::Edited => self.edited_owes_layer(cbz, rel),
            _ => false,
        }
    }

    fn layer_outdated(&self, cbz: &Path, row: &Generation) -> bool {
        Upgrade::layer_outdated(self, cbz, row)
    }
}

/// What a swap wrote for an upgrade job's result: the main sidecar, or the layer an
/// EDITED main's new OCR goes to ([`Upgrade::install_for_edited`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swapped {
    Main,
    EditedMainLayer,
}

/// An upgrade job's result: swapped in as the bare file, or, when a reader marked that
/// file EDITED, written beside it as a layer (called by the collector).
pub fn swap_in_generated(req: &CollectRequest, cbz: &Path) -> Result<(PathBuf, Swapped), Outcome> {
    let Some(upgrade) = req.upgrade.clone() else {
        return Err(Outcome::Failed(
            "generation upgrades are not configured".into(),
        ));
    };
    let rel = rel_of(&upgrade.library, cbz).unwrap_or_default();
    if upgrade.judge(cbz, &rel).1 == Verdict::Edited {
        return upgrade
            .install_for_edited(cbz, &req.result)
            .map(|p| (p, Swapped::EditedMainLayer));
    }
    upgrade
        .swap(cbz, &req.result, "generated")
        .map(|p| (p, Swapped::Main))
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
        // A leading BOM (rule v1) hides neither the stamp nor the recipe.
        let bom = dir.path().join("d.mokuro");
        let stamped = r#"{"ocr_engine":{"id":"hayai-nova","detector":"ppocr-manga","patch_budget":512},"pages":[]}"#;
        std::fs::write(&bom, format!("\u{feff}{stamped}")).unwrap();
        assert!(classify(&bom, None).matches(&(
            "hayai-nova".into(),
            "ppocr-manga".into(),
            Some(512)
        )));
        let bytes = std::fs::read(&bom).unwrap();
        assert!(classify_bytes(&bytes, None).matches(&(
            "hayai-nova".into(),
            "ppocr-manga".into(),
            Some(512)
        )));
        let data: Value = serde_json::from_slice(strip_bom(&bytes)).unwrap();
        assert_eq!(stamp_id(&data).as_deref(), Some("hayai-nova"));
    }

    /// Provenance precedence: `edited` wins over everything; a revision reads off the
    /// block (1 when absent), also when a provenance row names the recipe.
    #[test]
    fn recipes_carry_revision_and_edited() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, text: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, text).unwrap();
            p
        };
        let target = (
            "hayai-nova".to_string(),
            "ppocr-manga".to_string(),
            Some(512),
        );
        let block =
            r#""ocr_engine":{"id":"hayai-nova","detector":"ppocr-manga","patch_budget":512"#;
        let old = write("a.mokuro", &format!(r#"{{{block}}},"pages":[]}}"#));
        let r = classify(&old, None);
        assert_eq!((r.revision, r.edited), (1, false));
        assert!(r.is_current(&target, 1));
        assert!(
            !r.is_current(&target, 2),
            "a newer registry revision outdates it"
        );
        let two = write(
            "b.mokuro",
            &format!(r#"{{{block},"revision":2}},"pages":[]}}"#),
        );
        assert_eq!(classify(&two, None).revision, 2);
        let row = bunko_db::OcrSidecar {
            engine: Some("hayai-nova".into()),
            detector: Some("ppocr-manga".into()),
            ..Default::default()
        };
        assert_eq!(
            classify(&two, Some(&row)).revision,
            2,
            "the row names no revision"
        );
        let edited = write(
            "c.mokuro",
            &format!(r#"{{{block},"revision":2}},"edited":{{"by":"mokuro-reader"}},"pages":[]}}"#),
        );
        for r in [classify(&edited, None), classify(&edited, Some(&row))] {
            assert!(r.edited);
            assert_eq!(r.family, EDITED);
            assert!(!r.matches(&target));
        }
    }

    /// The `preserved` block is no part of the schema any more (owner decision,
    /// 2026-11-01): a stray one is ignored, and the file classifies by its other
    /// blocks (EDITED, then GENERATED, then LEGACY).
    #[test]
    fn a_stray_preserved_block_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, text: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, text).unwrap();
            p
        };
        let stray = r#""preserved":{"by":"mokuro-bunko","sha256":"00"}"#;
        let legacy = write(
            "a.mokuro",
            &format!(r#"{{"version":"0.2.1",{stray},"pages":[]}}"#),
        );
        assert_eq!(classify(&legacy, None).family, LEGACY);
        let generated = write(
            "b.mokuro",
            &format!(
                r#"{{"version":"0.2.5",{stray},"ocr_engine":{{"id":"hayai-nova","detector":"ppocr-manga"}},"pages":[]}}"#
            ),
        );
        let r = classify(&generated, None);
        assert_eq!((r.family.as_str(), r.edited), ("hayai-nova", false));
        let edited = write(
            "c.mokuro",
            &format!(
                r#"{{"version":"0.2.1",{stray},"edited":{{"by":"mokuro-reader"}},"pages":[]}}"#
            ),
        );
        let r = classify(&edited, None);
        assert!(r.edited);
        assert_eq!(r.family, EDITED);
        assert_eq!(r.under.as_deref().map(|u| u.family.as_str()), Some(LEGACY));
        let nothing_else = write("d.mokuro", &format!(r#"{{{stray},"pages":[]}}"#));
        assert_eq!(classify(&nothing_else, None).family, UNKNOWN);
    }

    /// Rule v1: suffixes in any case, folded stems, all-digit ids; a file another
    /// archive (any extension) owns is never this volume's layer.
    #[test]
    fn layer_files_follow_rule_v1() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for n in [
            "Vol 1.cbz",
            "Vol 1.5.zip",
            "Vol 1.5.mokuro",
            "Vol 1.2024.mokuro",
            "vol 1.gcv.MOKURO",
            "Vol 1.hayai.mokuro.gz",
            "Vol 1.Upper.mokuro",
        ] {
            std::fs::write(d.join(n), b"{}").unwrap();
        }
        let layers = Upgrade::layer_files(&d.join("Vol 1.cbz"));
        assert_eq!(
            layers,
            [d.join("Vol 1.2024.mokuro"), d.join("vol 1.gcv.MOKURO")]
        );
        let ids: Vec<Option<String>> = layers.iter().map(|l| Upgrade::layer_id(l)).collect();
        assert_eq!(ids, [Some("2024".into()), Some("gcv".into())]);
    }
}
