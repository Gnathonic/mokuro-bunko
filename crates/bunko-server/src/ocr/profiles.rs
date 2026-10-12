//! Per-machine profiles: `<storage>/processors/<file>.json` (0.5.2 `remote/profiles.py`).
//!
//! One file per machine name (`@local.json` for this server): who owns the name, what
//! the machine is (`host`, `catalog`), and per generation row its pools, benchmark and
//! run evidence. Files keep the 0.5.2 layout so an upgraded library keeps name ownership,
//! benches and pools. Writes are read-modify-write under ONE process-wide lock (the admin
//! panel writes pools through its own handle), atomic via `.tmp` + rename,
//! `json.dumps(indent=2)` spelling.
//!
//! A stored benchmark counts only while it describes the row's precision mode on that
//! machine (`stale_bench_reason`, `bunko_sched::precision`): a stale one reads as absent
//! (`stale_bench`), is said once, and is dropped from the file at its next save.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde_json::{Map, Value, json};

pub const PROFILES_DIRNAME: &str = "processors";
/// This server's own profile key: no stored processor name starts with a space.
pub const LOCAL_PROFILE: &str = " local";
pub const LOCAL_PROFILE_FILENAME: &str = "@local.json";
pub const RUNS_KEPT: usize = 5;
pub const RECENT_VOLUMES: usize = bunko_sched::throughput::RECENT_VOLUMES;
pub const POOL_TABLES: [&str; 3] = ["stage_workers", "queue_capacity", "stage_device"];
pub const POOL_AUTO: &str = "auto";

static WRITE_LOCK: Mutex<()> = Mutex::new(());
/// Stale benchmarks seen by a read, dropped at the machine's next save:
/// `profile file -> {row id -> the bench's "at"}` (`_STALE_PENDING`, keyed by the file
/// rather than the name so two stores never mix).
static STALE_PENDING: Mutex<Option<HashMap<String, HashMap<String, String>>>> = Mutex::new(None);
/// `(name, row id, at)` already logged (`_STALE_LOGGED`).
static STALE_LOGGED: Mutex<Option<HashSet<(String, String, String)>>> = Mutex::new(None);

/// What a machine's model device computes in, for judging a stored benchmark.
#[derive(Clone, Copy, Debug)]
pub enum Formats<'a> {
    /// Read from what the machine registered (`recorded_formats`).
    Recorded,
    /// As the caller knows it (None: not reported).
    Known(Option<&'a BTreeSet<String>>),
}

/// `recorded_formats(bench, profile)`: a benchmark whose recognizer ran on the CPU says
/// fp32; else the machine's registered catalog, where the model sits by default.
pub fn recorded_formats(
    bench: &Map<String, Value>,
    profile: &Map<String, Value>,
    mode: &str,
) -> Option<BTreeSet<String>> {
    let engine = bench
        .get("host")
        .and_then(|h| h.get("devices"))
        .and_then(|d| d.get("engine"))
        .and_then(Value::as_str);
    if engine == Some("cpu") {
        return Some(BTreeSet::from(["fp32".to_string()]));
    }
    let catalog: bunko_proto::Catalog =
        serde_json::from_value(profile.get("catalog")?.clone()).ok()?;
    crate::ocr::sched::supported_for(&catalog, "auto", mode).map(|s| s.into_iter().collect())
}

/// The profile key of a machine name (`"local"` is this server).
pub fn profile_key(machine: &str) -> &str {
    if machine == crate::ocr::types::LOCAL {
        LOCAL_PROFILE
    } else {
        machine
    }
}

fn is_plain(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// `profile_filename(name)`.
pub fn profile_filename(name: &str) -> String {
    use sha2::Digest;
    if name == LOCAL_PROFILE {
        return LOCAL_PROFILE_FILENAME.to_string();
    }
    if is_plain(name) && name.chars().count() <= 64 {
        return format!("{name}.json");
    }
    let mut readable = String::new();
    let mut in_unsafe = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            readable.push(c);
            in_unsafe = false;
        } else if !in_unsafe {
            readable.push('_');
            in_unsafe = true;
        }
    }
    let trimmed: String = readable
        .trim_matches(|c| c == '.' || c == '_')
        .chars()
        .take(40)
        .collect();
    let readable = if trimmed.is_empty() {
        "processor".to_string()
    } else {
        trimmed
    };
    let digest = hex::encode(sha2::Sha256::digest(name.as_bytes()));
    format!("{readable}~{}.json", &digest[..16])
}

/// `recipe_key`: `[engine, effective_detector, patch_budget|null]`.
pub fn recipe_value(recipe: &(String, String, Option<u32>)) -> Value {
    json!([recipe.0, recipe.1, recipe.2])
}

/// One (machine, row) entry as the scheduler reads it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowProfile {
    pub pools: Map<String, Value>,
    /// None also when the stored one no longer describes the row's mode here.
    pub bench: Option<Map<String, Value>>,
    pub runs: Map<String, Value>,
    /// A benchmark is stored but stale (the pair is unmeasured there).
    pub stale_bench: bool,
}

impl RowProfile {
    pub fn bench_pages_per_second(&self) -> Option<f64> {
        self.bench
            .as_ref()
            .and_then(|b| b.get("pages_per_second"))
            .and_then(Value::as_f64)
            .filter(|p| *p > 0.0)
    }
    pub fn bench_startup_seconds(&self) -> Option<f64> {
        self.bench
            .as_ref()
            .and_then(|b| b.get("startup_seconds"))
            .and_then(Value::as_f64)
            .filter(|p| *p > 0.0)
    }
}

/// `holds_pools`: a stored pools object that names a stage in any table.
pub fn holds_pools(pools: Option<&Value>) -> bool {
    let Some(Value::Object(p)) = pools else {
        return false;
    };
    p.iter()
        .any(|(k, t)| k != "precision" && t.as_object().is_some_and(|t| !t.is_empty()))
}

/// `machine_pools(stored, own)`: table by table, a non-empty stored table wins whole.
pub fn machine_pools(stored: &Map<String, Value>, own: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    for key in POOL_TABLES {
        let table = match stored.get(key) {
            Some(Value::Object(t)) if !t.is_empty() => Value::Object(t.clone()),
            _ => own
                .get(key)
                .cloned()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({})),
        };
        out.insert(key.to_string(), table);
    }
    out
}

/// `runner_pools`: `"auto"` widths and capacities are left out (derived there).
pub fn runner_pools(pools: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for key in POOL_TABLES {
        let mut table = pools
            .get(key)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if key != "stage_device" {
            table.retain(|_, v| v.as_str() != Some(POOL_AUTO));
        }
        out.insert(key.to_string(), Value::Object(table));
    }
    out
}

/// The profile store over `<storage>/processors/`.
#[derive(Clone, Debug)]
pub struct Profiles {
    pub dir: PathBuf,
}

impl Profiles {
    pub fn new(storage: &Path) -> Profiles {
        Profiles {
            dir: storage.join(PROFILES_DIRNAME),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(profile_filename(name))
    }

    fn read(path: &Path) -> Map<String, Value> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| match v {
                Value::Object(m) => Some(m),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub fn load(&self, name: &str) -> Map<String, Value> {
        Self::read(&self.path(name))
    }

    /// The entry as stored (None when absent or measured for another recipe), its
    /// benchmark not judged: for reads that never look at the benchmark's rate.
    pub fn row(
        &self,
        name: &str,
        generation_id: &str,
        recipe: Option<&(String, String, Option<u32>)>,
    ) -> Option<RowProfile> {
        self.read_row(name, generation_id, recipe, None)
    }

    /// `row(name, id, recipe=..., mode=..., supported=...)`: the benchmark reads as
    /// absent (`stale_bench`) when it no longer describes `mode` on that machine.
    pub fn row_for(
        &self,
        name: &str,
        generation_id: &str,
        recipe: Option<&(String, String, Option<u32>)>,
        mode: &str,
        formats: Formats<'_>,
    ) -> Option<RowProfile> {
        self.read_row(name, generation_id, recipe, Some((mode, formats)))
    }

    fn read_row(
        &self,
        name: &str,
        generation_id: &str,
        recipe: Option<&(String, String, Option<u32>)>,
        judge: Option<(&str, Formats<'_>)>,
    ) -> Option<RowProfile> {
        let profile = self.load(name);
        let entry = profile
            .get("rows")?
            .get(generation_id)?
            .as_object()?
            .clone();
        let stored = entry.get("recipe");
        if let (Some(recipe), Some(stored)) = (recipe, stored)
            && !stored.is_null()
            && *stored != recipe_value(recipe)
        {
            return None;
        }
        let pools = entry.get("pools");
        let mut bench = entry.get("bench").and_then(Value::as_object).cloned();
        let mut stale = false;
        if let (Some(b), Some((mode, formats))) = (&bench, judge) {
            let engine = stored
                .and_then(Value::as_array)
                .and_then(|r| r.first())
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| recipe.map(|r| r.0.clone()));
            let wanted = bunko_sched::precision::normalize_mode(Some(mode))
                .unwrap_or(bunko_sched::precision::DEFAULT_MODE);
            let recorded;
            let supported = match formats {
                Formats::Known(s) => s,
                Formats::Recorded => {
                    recorded = recorded_formats(b, &profile, wanted);
                    recorded.as_ref()
                }
            };
            if let Some(reason) =
                bunko_sched::precision::stale_bench_reason(engine.as_deref(), b, wanted, supported)
            {
                note_stale(&self.path(name), name, generation_id, b, &reason);
                bench = None;
                stale = true;
            }
        }
        Some(RowProfile {
            pools: if holds_pools(pools) {
                pools
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|(k, _)| k != "precision")
                    .collect()
            } else {
                Map::new()
            },
            bench,
            runs: entry
                .get("runs")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            stale_bench: stale,
        })
    }

    /// Every processor with a profile, by the name inside the file.
    pub fn names(&self) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        paths.sort();
        let mut out = Vec::new();
        for p in paths {
            if let Some(Value::String(name)) = Self::read(&p).get("name")
                && !name.is_empty()
                && name != LOCAL_PROFILE
                && p.file_name()
                    .is_some_and(|f| f.to_string_lossy() == profile_filename(name))
            {
                out.push(name.clone());
            }
        }
        out
    }

    fn update(&self, name: &str, create: bool, edit: impl FnOnce(&mut Map<String, Value>)) {
        let _guard = WRITE_LOCK.lock();
        let path = self.path(name);
        if !create && !path.is_file() {
            return;
        }
        let mut profile = Self::read(&path);
        profile.insert("name".into(), Value::String(name.to_string()));
        if !profile.get("rows").is_some_and(Value::is_object) {
            profile.insert("rows".into(), json!({}));
        }
        drop_stale_benches(&path, &mut profile);
        edit(&mut profile);
        let text = ascii_escape(&bunko_sched::pyjson::dumps_indent2(&Value::Object(profile)));
        if let Err(e) = std::fs::create_dir_all(&self.dir)
            .and_then(|_| bunko_sched::pyjson::write_atomic(&path, &text))
        {
            tracing::error!("could not write {}: {e}", path.display());
        }
    }

    fn row_entry<'a>(
        profile: &'a mut Map<String, Value>,
        generation_id: &str,
        recipe: Option<&(String, String, Option<u32>)>,
    ) -> &'a mut Map<String, Value> {
        let rows = profile.entry("rows").or_insert_with(|| json!({}));
        if !rows.is_object() {
            *rows = json!({});
        }
        let rows = rows.as_object_mut().expect("rows was just made an object");
        let wanted = recipe.map(recipe_value);
        let reset = match rows.get(generation_id) {
            Some(Value::Object(e)) => match (&wanted, e.get("recipe")) {
                (Some(w), Some(stored)) => !stored.is_null() && stored != w,
                _ => false,
            },
            _ => true,
        };
        if reset {
            rows.insert(generation_id.to_string(), json!({}));
        }
        let entry = rows
            .get_mut(generation_id)
            .and_then(Value::as_object_mut)
            .expect("row entry was just made an object");
        if let Some(w) = wanted {
            entry.insert("recipe".into(), w);
        }
        entry
    }

    /// `claim(name, account)`: the first account to register a name owns it.
    pub fn claim(&self, name: &str, account: &str) -> bool {
        let mut taken = false;
        self.update(name, true, |p| match p.get("account") {
            Some(Value::String(owner)) if !owner.is_empty() && owner != account => taken = true,
            _ => {
                p.insert("account".into(), Value::String(account.to_string()));
            }
        });
        !taken
    }

    /// The account that owns `name`, if any.
    pub fn owner(&self, name: &str) -> Option<String> {
        self.load(name)
            .get("account")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    pub fn set_identity(&self, name: &str, host: &Value, catalog: &Value) {
        self.update(name, true, |p| {
            p.insert("host".into(), host.clone());
            p.insert("catalog".into(), catalog.clone());
        });
    }

    pub fn set_pools(
        &self,
        name: &str,
        generation_id: &str,
        pools: &Map<String, Value>,
        recipe: Option<&(String, String, Option<u32>)>,
        keep_existing: bool,
        autobench: bool,
    ) {
        self.update(name, true, |p| {
            let row = Self::row_entry(p, generation_id, recipe);
            if keep_existing && holds_pools(row.get("pools")) {
                return;
            }
            let pools: Map<String, Value> = pools
                .iter()
                .filter(|(k, _)| *k != "precision")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            row.insert("pools".into(), Value::Object(pools));
            if autobench {
                row.insert("pools_autobench".into(), json!({}));
            } else {
                row.remove("pools_autobench");
            }
        });
    }

    pub fn set_bench(
        &self,
        name: &str,
        generation_id: &str,
        bench: &Map<String, Value>,
        recipe: Option<&(String, String, Option<u32>)>,
    ) {
        self.update(name, true, |p| {
            let row = Self::row_entry(p, generation_id, recipe);
            row.insert("bench".into(), Value::Object(bench.clone()));
        });
    }

    /// `record_run`: one finished volume's evidence; a contended one is only counted.
    #[allow(clippy::too_many_arguments)]
    pub fn record_run(
        &self,
        name: &str,
        generation_id: &str,
        pages: i64,
        seconds: f64,
        congestion: Option<&Map<String, Value>>,
        recipe: Option<&(String, String, Option<u32>)>,
        contended: bool,
        now: f64,
    ) {
        if contended {
            self.update(name, true, |p| {
                let row = Self::row_entry(p, generation_id, recipe);
                let runs = row.entry("runs").or_insert_with(|| json!({}));
                if let Some(runs) = runs.as_object_mut() {
                    let n = runs.get("contended").and_then(Value::as_i64).unwrap_or(0) + 1;
                    runs.insert("contended".into(), json!(n));
                    runs.insert("contended_last_at".into(), json!(now));
                }
            });
            return;
        }
        if pages <= 0 || seconds <= 0.0 {
            return;
        }
        let keep = RUNS_KEPT;
        self.update(name, true, |p| {
            let row = Self::row_entry(p, generation_id, recipe);
            let runs = row.entry("runs").or_insert_with(|| json!({}));
            if !runs.is_object() {
                *runs = json!({});
            }
            let Some(runs) = runs.as_object_mut() else {
                return;
            };
            let volumes = runs.get("volumes").and_then(Value::as_i64).unwrap_or(0) + 1;
            let total_pages = runs.get("pages").and_then(Value::as_i64).unwrap_or(0) + pages;
            let total_seconds =
                runs.get("seconds").and_then(Value::as_f64).unwrap_or(0.0) + seconds;
            runs.insert("volumes".into(), json!(volumes));
            runs.insert("pages".into(), json!(total_pages));
            runs.insert("seconds".into(), json!(total_seconds));
            runs.insert(
                "pages_per_second".into(),
                json!(total_pages as f64 / total_seconds),
            );
            let mut recent: Vec<Value> = runs
                .get("recent")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(Value::is_object)
                .collect();
            recent.push(json!({"pages": pages, "seconds": seconds, "at": now}));
            let skip = recent.len().saturating_sub(RECENT_VOLUMES);
            runs.insert("recent".into(), Value::Array(recent.split_off(skip)));
            runs.insert("last_at".into(), json!(now));
            if let Some(c) = congestion {
                let mut history: Vec<Value> = runs
                    .get("congestion")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                history.push(Value::Object(c.clone()));
                let skip = history.len().saturating_sub(keep);
                runs.insert("congestion".into(), Value::Array(history.split_off(skip)));
            }
        });
    }

    /// Drop rows that no longer exist from every profile (and this server's own).
    pub fn prune(&self, generation_ids: &[String]) {
        let mut names = self.names();
        names.push(LOCAL_PROFILE.to_string());
        for name in names {
            self.update(&name, false, |p| {
                if let Some(rows) = p.get_mut("rows").and_then(Value::as_object_mut) {
                    rows.retain(|k, _| generation_ids.iter().any(|g| g == k));
                }
            });
        }
    }
}

/// `_note_stale`: say a stale benchmark once, and have it dropped at the next save.
fn note_stale(
    file: &Path,
    name: &str,
    generation_id: &str,
    bench: &Map<String, Value>,
    reason: &str,
) {
    let stamp = bench
        .get("at")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    STALE_PENDING
        .lock()
        .get_or_insert_with(HashMap::new)
        .entry(file.to_string_lossy().into_owned())
        .or_default()
        .insert(generation_id.to_string(), stamp.clone());
    let said = (name.to_string(), generation_id.to_string(), stamp.clone());
    if !STALE_LOGGED
        .lock()
        .get_or_insert_with(HashSet::new)
        .insert(said)
    {
        return;
    }
    tracing::info!(
        "Ignoring the stale benchmark of {generation_id} on {} ({}): {reason}; it is re-measured where autobench is on, and dropped from the profile at its next save",
        if name == LOCAL_PROFILE {
            crate::ocr::types::LOCAL_DISPLAY
        } else {
            name
        },
        if stamp.is_empty() { "undated" } else { &stamp }
    );
}

/// `_drop_stale_benches`: remove the benchmarks a read found stale, if they are still
/// the ones stored (matched by `at`: a re-measurement written since is kept).
fn drop_stale_benches(file: &Path, profile: &mut Map<String, Value>) {
    let key = file.to_string_lossy();
    let Some(pending) = STALE_PENDING
        .lock()
        .as_mut()
        .and_then(|p| p.remove(key.as_ref()))
    else {
        return;
    };
    let Some(rows) = profile.get_mut("rows").and_then(Value::as_object_mut) else {
        return;
    };
    for (generation_id, stamp) in pending {
        if let Some(Value::Object(entry)) = rows.get_mut(&generation_id) {
            let same = entry
                .get("bench")
                .and_then(Value::as_object)
                .is_some_and(|b| b.get("at").and_then(Value::as_str).unwrap_or("") == stamp);
            if same {
                entry.remove("bench");
            }
        }
    }
}

/// Python's default `ensure_ascii` over an already-serialised document: every non-ASCII
/// character can only be inside a string, so escaping it there is the same output.
pub fn ascii_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if (c as u32) > 0x7f {
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames() {
        assert_eq!(profile_filename("tower"), "tower.json");
        assert_eq!(profile_filename(LOCAL_PROFILE), "@local.json");
        let f = profile_filename("Tower 1");
        assert!(f.starts_with("Tower_1~") && f.ends_with(".json"), "{f}");
    }

    #[test]
    fn a_stale_benchmark_reads_as_absent_and_is_dropped_at_the_next_save() {
        let dir = tempfile::tempdir().unwrap();
        let p = Profiles::new(dir.path());
        let recipe = (
            "hayai-nova".to_string(),
            "ppocr-manga".to_string(),
            Some(512),
        );
        let bench = json!({
            "pages_per_second": 5.0, "at": "2026-10-01T12:00:00Z",
            "precision": "bf16", "precision_mode": "auto-balanced",
            "precision_trials": [{"precision": "bf16", "pages_per_second": 5.0},
                                 {"precision": "fp32", "pages_per_second": 2.0}],
        });
        p.set_bench("box", "g-1", bench.as_object().unwrap(), Some(&recipe));
        let gpu: BTreeSet<String> = ["fp32", "fp16", "bf16"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let fresh = p
            .row_for(
                "box",
                "g-1",
                Some(&recipe),
                "auto-balanced",
                Formats::Known(Some(&gpu)),
            )
            .unwrap();
        assert!(fresh.bench.is_some() && !fresh.stale_bench);
        // The row's mode changed: the benchmark no longer describes it.
        let stale = p
            .row_for(
                "box",
                "g-1",
                Some(&recipe),
                "auto-speed",
                Formats::Known(Some(&gpu)),
            )
            .unwrap();
        assert!(stale.bench.is_none() && stale.stale_bench);
        // A raw read still sees it; the next save of that machine drops it.
        assert!(p.row("box", "g-1", Some(&recipe)).unwrap().bench.is_some());
        p.record_run("box", "g-1", 10, 5.0, None, Some(&recipe), false, 100.0);
        assert!(p.row("box", "g-1", Some(&recipe)).unwrap().bench.is_none());
        // Judged on the formats the machine registered.
        p.set_identity(
            "box",
            &json!({}),
            &json!({"devices": [{"id": "gpu:0", "label": "GPU", "formats": ["fp32", "fp16", "bf16"], "provider": "cuda", "arch": "sm_86"}]}),
        );
        p.set_bench("box", "g-1", bench.as_object().unwrap(), Some(&recipe));
        let recorded = p
            .row_for(
                "box",
                "g-1",
                Some(&recipe),
                "auto-balanced",
                Formats::Recorded,
            )
            .unwrap();
        assert!(recorded.bench.is_some());
    }

    #[test]
    fn claim_and_runs() {
        let dir = tempfile::tempdir().unwrap();
        let p = Profiles::new(dir.path());
        assert!(p.claim("tower", "alice"));
        assert!(p.claim("tower", "alice"));
        assert!(!p.claim("tower", "bob"));
        let recipe = (
            "hayai-nova".to_string(),
            "ppocr-manga".to_string(),
            Some(512),
        );
        p.record_run("tower", "g-1", 10, 5.0, None, Some(&recipe), false, 100.0);
        let row = p.row("tower", "g-1", Some(&recipe)).unwrap();
        assert_eq!(row.runs["volumes"], 1);
        let other = ("paddle-manga".to_string(), "ppocr-manga".to_string(), None);
        assert!(p.row("tower", "g-1", Some(&other)).is_none());
        assert_eq!(p.names(), vec!["tower".to_string()]);
    }
}
