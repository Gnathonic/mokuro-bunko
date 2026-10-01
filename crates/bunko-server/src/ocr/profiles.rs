//! Per-machine profiles: `<storage>/processors/<file>.json` (0.5.2 `remote/profiles.py`).
//!
//! One file per machine name (`@local.json` for this server): who owns the name, what
//! the machine is (`host`, `catalog`), and per generation row its pools, benchmark and
//! run evidence. Files keep the 0.5.2 layout so an upgraded library keeps name ownership,
//! benches and pools. Writes are read-modify-write under ONE process-wide lock (the admin
//! panel writes pools through its own handle), atomic via `.tmp` + rename,
//! `json.dumps(indent=2)` spelling.
//!
//! Deviation: 0.5.2's precision staleness of a stored benchmark (`stale_bench_reason`) is
//! torch-shaped and not ported; a bench is current while its recipe matches.

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

/// The profile key of a machine name (`"local"` is this server).
pub fn profile_key(machine: &str) -> &str {
    if machine == crate::ocr::types::LOCAL { LOCAL_PROFILE } else { machine }
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
    let trimmed: String = readable.trim_matches(|c| c == '.' || c == '_').chars().take(40).collect();
    let readable = if trimmed.is_empty() { "processor".to_string() } else { trimmed };
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
    pub bench: Option<Map<String, Value>>,
    pub runs: Map<String, Value>,
}

impl RowProfile {
    pub fn bench_pages_per_second(&self) -> Option<f64> {
        self.bench.as_ref().and_then(|b| b.get("pages_per_second")).and_then(Value::as_f64).filter(|p| *p > 0.0)
    }
    pub fn bench_startup_seconds(&self) -> Option<f64> {
        self.bench.as_ref().and_then(|b| b.get("startup_seconds")).and_then(Value::as_f64).filter(|p| *p > 0.0)
    }
}

/// `holds_pools`: a stored pools object that names a stage in any table.
pub fn holds_pools(pools: Option<&Value>) -> bool {
    let Some(Value::Object(p)) = pools else { return false };
    p.iter().any(|(k, t)| k != "precision" && t.as_object().is_some_and(|t| !t.is_empty()))
}

/// `machine_pools(stored, own)`: table by table, a non-empty stored table wins whole.
pub fn machine_pools(stored: &Map<String, Value>, own: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    for key in POOL_TABLES {
        let table = match stored.get(key) {
            Some(Value::Object(t)) if !t.is_empty() => Value::Object(t.clone()),
            _ => own.get(key).cloned().filter(Value::is_object).unwrap_or_else(|| json!({})),
        };
        out.insert(key.to_string(), table);
    }
    out
}

/// `runner_pools`: `"auto"` widths and capacities are left out (derived there).
pub fn runner_pools(pools: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for key in POOL_TABLES {
        let mut table = pools.get(key).and_then(Value::as_object).cloned().unwrap_or_default();
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
        Profiles { dir: storage.join(PROFILES_DIRNAME) }
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

    /// `row(name, id, recipe=...)`: None when absent or measured for another recipe.
    pub fn row(&self, name: &str, generation_id: &str, recipe: Option<&(String, String, Option<u32>)>) -> Option<RowProfile> {
        let profile = self.load(name);
        let entry = profile.get("rows")?.get(generation_id)?.as_object()?.clone();
        let stored = entry.get("recipe");
        if let (Some(recipe), Some(stored)) = (recipe, stored)
            && !stored.is_null()
            && *stored != recipe_value(recipe)
        {
            return None;
        }
        let pools = entry.get("pools");
        Some(RowProfile {
            pools: if holds_pools(pools) {
                pools.and_then(Value::as_object).cloned().unwrap_or_default().into_iter().filter(|(k, _)| k != "precision").collect()
            } else {
                Map::new()
            },
            bench: entry.get("bench").and_then(Value::as_object).cloned(),
            runs: entry.get("runs").and_then(Value::as_object).cloned().unwrap_or_default(),
        })
    }

    /// Every processor with a profile, by the name inside the file.
    pub fn names(&self) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut paths: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
        paths.sort();
        let mut out = Vec::new();
        for p in paths {
            if let Some(Value::String(name)) = Self::read(&p).get("name")
                && !name.is_empty()
                && name != LOCAL_PROFILE
                && p.file_name().is_some_and(|f| f.to_string_lossy() == profile_filename(name))
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
        edit(&mut profile);
        let text = ascii_escape(&bunko_sched::pyjson::dumps_indent2(&Value::Object(profile)));
        if let Err(e) = std::fs::create_dir_all(&self.dir).and_then(|_| bunko_sched::pyjson::write_atomic(&path, &text)) {
            tracing::error!("could not write {}: {e}", path.display());
        }
    }

    fn row_entry<'a>(profile: &'a mut Map<String, Value>, generation_id: &str, recipe: Option<&(String, String, Option<u32>)>) -> &'a mut Map<String, Value> {
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
        let entry = rows.get_mut(generation_id).and_then(Value::as_object_mut).expect("row entry was just made an object");
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
        self.load(name).get("account").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
    }

    pub fn set_identity(&self, name: &str, host: &Value, catalog: &Value) {
        self.update(name, true, |p| {
            p.insert("host".into(), host.clone());
            p.insert("catalog".into(), catalog.clone());
        });
    }

    pub fn set_pools(&self, name: &str, generation_id: &str, pools: &Map<String, Value>, recipe: Option<&(String, String, Option<u32>)>, keep_existing: bool, autobench: bool) {
        self.update(name, true, |p| {
            let row = Self::row_entry(p, generation_id, recipe);
            if keep_existing && holds_pools(row.get("pools")) {
                return;
            }
            let pools: Map<String, Value> = pools.iter().filter(|(k, _)| *k != "precision").map(|(k, v)| (k.clone(), v.clone())).collect();
            row.insert("pools".into(), Value::Object(pools));
            if autobench {
                row.insert("pools_autobench".into(), json!({}));
            } else {
                row.remove("pools_autobench");
            }
        });
    }

    pub fn set_bench(&self, name: &str, generation_id: &str, bench: &Map<String, Value>, recipe: Option<&(String, String, Option<u32>)>) {
        self.update(name, true, |p| {
            let row = Self::row_entry(p, generation_id, recipe);
            row.insert("bench".into(), Value::Object(bench.clone()));
        });
    }

    /// `record_run`: one finished volume's evidence; a contended one is only counted.
    #[allow(clippy::too_many_arguments)]
    pub fn record_run(&self, name: &str, generation_id: &str, pages: i64, seconds: f64, congestion: Option<&Map<String, Value>>, recipe: Option<&(String, String, Option<u32>)>, contended: bool, now: f64) {
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
            let Some(runs) = runs.as_object_mut() else { return };
            let volumes = runs.get("volumes").and_then(Value::as_i64).unwrap_or(0) + 1;
            let total_pages = runs.get("pages").and_then(Value::as_i64).unwrap_or(0) + pages;
            let total_seconds = runs.get("seconds").and_then(Value::as_f64).unwrap_or(0.0) + seconds;
            runs.insert("volumes".into(), json!(volumes));
            runs.insert("pages".into(), json!(total_pages));
            runs.insert("seconds".into(), json!(total_seconds));
            runs.insert("pages_per_second".into(), json!(total_pages as f64 / total_seconds));
            let mut recent: Vec<Value> = runs.get("recent").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter(Value::is_object).collect();
            recent.push(json!({"pages": pages, "seconds": seconds, "at": now}));
            let skip = recent.len().saturating_sub(RECENT_VOLUMES);
            runs.insert("recent".into(), Value::Array(recent.split_off(skip)));
            runs.insert("last_at".into(), json!(now));
            if let Some(c) = congestion {
                let mut history: Vec<Value> = runs.get("congestion").and_then(Value::as_array).cloned().unwrap_or_default();
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
    fn claim_and_runs() {
        let dir = tempfile::tempdir().unwrap();
        let p = Profiles::new(dir.path());
        assert!(p.claim("tower", "alice"));
        assert!(p.claim("tower", "alice"));
        assert!(!p.claim("tower", "bob"));
        let recipe = ("hayai-nova".to_string(), "ppocr-manga".to_string(), Some(512));
        p.record_run("tower", "g-1", 10, 5.0, None, Some(&recipe), false, 100.0);
        let row = p.row("tower", "g-1", Some(&recipe)).unwrap();
        assert_eq!(row.runs["volumes"], 1);
        let other = ("paddle-manga".to_string(), "ppocr-manga".to_string(), None);
        assert!(p.row("tower", "g-1", Some(&other)).is_none());
        assert_eq!(p.names(), vec!["tower".to_string()]);
    }
}
