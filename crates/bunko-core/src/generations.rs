//! OCR generations: the configured list of named OCR recipes (`ocr.generations`).
//!
//! A generation is one row: an engine, the detector feeding it, the patch budget it reads
//! at, its precision mode and per-stage pool sizes. The queue's unit of work is
//! `(volume, generation)`. Each row writes its own sidecar: `<Volume>.mokuro` for the row
//! flagged `primary`, `<Volume>.<name>.mokuro` for the others.
//!
//! The name is a file-name postfix and the reader's grammar decides it
//! (`LAYER_ID_RE` in mokuro-reader `src/lib/util/sync/syncable-file.ts`).
//!
//! ## 0.7 migration of removed components
//!
//! 0.5.2 configs may name `mokuro`/`mokuro-fp16` engines and `ctd`/`animetext`/`rtdetr`
//! detectors. They still load:
//! * a row on a removed engine is kept (so its layer's name stays reserved and the admin
//!   sees it) but marked [`Generation::retired`] and never runs; it loses `primary`;
//! * a row on a removed detector is switched to the `ppocr-manga` detector;
//! * when no runnable enabled primary remains, the default `hayai-nova` primary row is
//!   added. Existing bare `<Volume>.mokuro` files are left alone (they are complete, so
//!   nothing re-OCRs them); the generation upgrade replaces them only when enabled.
//!
//! Every migration is reported in [`ParsedGenerations::warnings`].

use crate::engines::{self, Road};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

pub const MAX_GENERATION_NAME: usize = 32;
pub const RESERVED_NAMES: &[&str] = &["original", "gcv", "updated-ocr"];
pub const RESERVED_PREFIXES: &[&str] = &["tr-"];
pub const MAX_STAGE_WORKERS: u32 = 64;
pub const MAX_QUEUE_CAPACITY: u32 = 256;
pub const POOL_KEYS: &[&str] = &[
    "stage_workers",
    "queue_capacity",
    "stage_device",
    "precision",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationError {
    pub message: String,
    /// Index of the offending row, or None for the list as a whole.
    pub row: Option<usize>,
    pub field: Option<&'static str>,
}

impl fmt::Display for GenerationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for GenerationError {}

fn row_err(index: usize, field: &'static str, msg: impl fmt::Display) -> GenerationError {
    GenerationError {
        message: format!("ocr.generations[{index}]: {msg}"),
        row: Some(index),
        field: Some(field),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pools {
    pub stage_workers: BTreeMap<String, u32>,
    pub queue_capacity: BTreeMap<String, u32>,
    /// Stage → device id (`cpu`, `gpu:<n>`); absent means `auto`.
    pub stage_device: BTreeMap<String, String>,
}

impl Pools {
    pub fn is_empty(&self) -> bool {
        self.stage_workers.is_empty()
            && self.queue_capacity.is_empty()
            && self.stage_device.is_empty()
    }
    pub fn to_value(&self) -> Value {
        json!({
            "stage_workers": self.stage_workers,
            "queue_capacity": self.queue_capacity,
            "stage_device": self.stage_device,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    pub id: String,
    pub name: String,
    pub engine: String,
    pub primary: bool,
    pub enabled: bool,
    /// None when the engine brings its own detector (or the row is retired).
    pub detector: Option<String>,
    pub patch_budget: u32,
    pub pools: Pools,
    pub precision: String,
    /// Set only on a per-machine copy of the row, never stored.
    pub precision_pick: Option<String>,
    pub precision_why: String,
    /// Why this row no longer runs (its engine was removed in 0.7). Retired rows are
    /// serialized back exactly as loaded so a downgrade still finds them.
    pub retired: Option<String>,
    /// The row as it was read, kept for retired rows so saving does not lose fields.
    pub raw: Option<Map<String, Value>>,
}

impl Generation {
    pub fn engine_spec(&self) -> Option<&'static engines::EngineSpec> {
        engines::engine(&self.engine)
    }

    /// Runs at all: enabled and not retired.
    pub fn runnable(&self) -> bool {
        self.enabled && self.retired.is_none()
    }

    pub fn effective_detector(&self) -> &str {
        match self.engine_spec().and_then(|e| e.detector) {
            Some(own) => own,
            None => self
                .detector
                .as_deref()
                .unwrap_or(engines::DEFAULT_DETECTOR),
        }
    }

    pub fn road(&self) -> Option<Road> {
        self.engine_spec().map(|e| e.road())
    }

    pub fn stage_keys(&self) -> &'static [&'static str] {
        self.road().map(|r| r.stage_keys()).unwrap_or(&[])
    }

    pub fn device_stage_keys(&self) -> &'static [&'static str] {
        self.road().map(|r| r.device_stage_keys()).unwrap_or(&[])
    }

    pub fn patch_budget_applies(&self) -> bool {
        self.engine_spec().is_some_and(|e| e.patch_budget)
    }

    pub fn precision_applies(&self) -> bool {
        self.engine_spec().is_some_and(|e| e.precision)
    }

    /// `.mokuro` for the primary row, `.<name>.mokuro` otherwise.
    pub fn sidecar_suffix(&self) -> String {
        if self.primary {
            ".mokuro".to_string()
        } else {
            format!(".{}.mokuro", self.name)
        }
    }

    /// What must not change under a running job: engine, effective detector, and the
    /// patch budget when the engine reads it. Renames and pool sizes are excluded.
    pub fn output_affecting(&self) -> (String, String, Option<u32>) {
        (
            self.engine.clone(),
            self.effective_detector().to_string(),
            self.patch_budget_applies().then_some(self.patch_budget),
        )
    }

    /// The row as stored in `config.yaml` and sent over HTTP (0.5.2 `to_dict`).
    pub fn to_value(&self) -> Value {
        if let (Some(_), Some(raw)) = (&self.retired, &self.raw) {
            let mut out = raw.clone();
            out.insert("id".into(), Value::String(self.id.clone()));
            out.insert("name".into(), Value::String(self.name.clone()));
            out.insert("primary".into(), Value::Bool(false));
            out.insert("enabled".into(), Value::Bool(false));
            return Value::Object(out);
        }
        let mut m = Map::new();
        m.insert("id".into(), json!(self.id));
        m.insert("name".into(), json!(self.name));
        m.insert("primary".into(), json!(self.primary));
        m.insert("enabled".into(), json!(self.enabled));
        m.insert("engine".into(), json!(self.engine));
        if let Some(d) = &self.detector {
            m.insert("detector".into(), json!(d));
        }
        m.insert("patch_budget".into(), json!(self.patch_budget));
        if self.precision != engines::DEFAULT_PRECISION_MODE {
            m.insert("precision".into(), json!(self.precision));
        }
        m.insert("pools".into(), self.pools.to_value());
        if let Some(pick) = &self.precision_pick {
            m.insert("precision_pick".into(), json!(pick));
            m.insert("precision_why".into(), json!(self.precision_why));
        }
        Value::Object(m)
    }
}

/// The default primary row of a fresh 0.7 install.
pub fn default_generation(id: &str) -> Generation {
    Generation {
        id: id.to_string(),
        name: engines::DEFAULT_ENGINE.to_string(),
        engine: engines::DEFAULT_ENGINE.to_string(),
        primary: true,
        enabled: true,
        detector: Some(engines::DEFAULT_DETECTOR.to_string()),
        patch_budget: engines::DEFAULT_PATCH_BUDGET,
        pools: Pools::default(),
        precision: engines::DEFAULT_PRECISION_MODE.to_string(),
        precision_pick: None,
        precision_why: String::new(),
        retired: None,
        raw: None,
    }
}

pub fn default_generations() -> Vec<Generation> {
    vec![default_generation("g-1")]
}

#[derive(Debug, Clone, Default)]
pub struct ParsedGenerations {
    pub rows: Vec<Generation>,
    /// Human-readable notes about migrated rows; logged at start, shown in the admin panel.
    pub warnings: Vec<String>,
    /// True when the list on disk differs from `rows` (a save would change it).
    pub migrated: bool,
}

// --- naming ---------------------------------------------------------------------------

fn is_name_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
}

/// The reader's layer-id grammar: `[a-z0-9-]{1,32}`.
pub fn is_layer_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.chars().all(is_name_char)
}

/// Why `name` cannot be a generation name, or None when it can.
pub fn name_rejection(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some(
            "a name is required (it is this row's label and its file-name postfix)".into(),
        );
    }
    let first_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !first_ok || name.len() > MAX_GENERATION_NAME || !name.chars().all(is_name_char) {
        return Some(format!(
            "name '{name}' cannot be a file-name postfix: use lowercase letters, digits and hyphens only, \
             {MAX_GENERATION_NAME} characters at most, starting with a letter or a digit (a name outside this \
             is invisible to every reader)"
        ));
    }
    if RESERVED_NAMES.contains(&name) {
        return Some(format!(
            "name '{name}' is reserved by the reader for its own layer of that name"
        ));
    }
    for prefix in RESERVED_PREFIXES {
        if name.starts_with(prefix) {
            return Some(format!(
                "name '{name}' is reserved: the reader files any layer starting with '{prefix}' as a translation, \
                 whatever produced it"
            ));
        }
    }
    None
}

fn trim_name(stem: &str) -> String {
    let cut: String = stem.chars().take(MAX_GENERATION_NAME).collect();
    cut.trim_end_matches('-').to_string()
}

/// The name a new row gets: the engine, or `<engine>-<detector>`, de-duplicated.
pub fn seed_generation_name<'a>(
    engine: &str,
    detector: Option<&str>,
    taken: impl IntoIterator<Item = &'a str>,
) -> String {
    let used: HashSet<&str> = taken.into_iter().collect();
    let own_detector = engines::engine(engine).is_some_and(|e| e.detector.is_some());
    let stem = match detector {
        Some(d) if !own_detector && !d.is_empty() => format!("{engine}-{d}"),
        _ => engine.to_string(),
    };
    let stem = trim_name(&stem);
    let mut candidate = stem.clone();
    let mut counter = 1;
    while used.contains(candidate.as_str()) || name_rejection(&candidate).is_some() {
        counter += 1;
        let tail = format!("-{counter}");
        let room = MAX_GENERATION_NAME.saturating_sub(tail.len());
        candidate = trim_name(&stem.chars().take(room).collect::<String>()) + &tail;
    }
    candidate
}

fn is_valid_id(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && s.len() <= 32
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The next free `g-<n>` id.
pub fn mint_generation_id<'a>(taken: impl IntoIterator<Item = &'a str>) -> String {
    let used: HashSet<&str> = taken.into_iter().collect();
    let mut highest = used
        .iter()
        .filter_map(|v| v.strip_prefix("g-").and_then(|n| n.parse::<u64>().ok()))
        .max()
        .unwrap_or(0);
    loop {
        let candidate = format!("g-{}", highest + 1);
        if !used.contains(candidate.as_str()) {
            return candidate;
        }
        highest += 1;
    }
}

// --- parsing --------------------------------------------------------------------------

fn value_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(other) => other.to_string().trim_matches('"').trim().to_string(),
    }
}

fn value_bool(v: Option<&Value>, default: bool) -> bool {
    match v {
        None | Some(Value::Null) => default,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn coerce_rows(value: &Value) -> Result<Vec<Value>, GenerationError> {
    match value {
        Value::Null => Ok(vec![]),
        Value::String(s) => {
            let text = s.trim();
            if text.is_empty() {
                return Ok(vec![]);
            }
            let decoded: Value = serde_json::from_str(text).map_err(|e| GenerationError {
                message: format!(
                    "ocr.generations must be a list of generations, or the JSON text of one; could not read it as JSON ({e})"
                ),
                row: None,
                field: None,
            })?;
            coerce_rows(&decoded)
        }
        Value::Object(_) => Ok(vec![value.clone()]),
        Value::Array(a) => Ok(a.clone()),
        other => Err(GenerationError {
            message: format!(
                "ocr.generations must be a list of generations, got {}",
                json_type(other)
            ),
            row: None,
            field: None,
        }),
    }
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Validate a configured `ocr.generations` value. Accepts the parsed YAML/JSON list, a
/// JSON string (env var / `config set`), or nothing (→ the default list).
pub fn parse_generation_list(value: &Value) -> Result<ParsedGenerations, GenerationError> {
    let raw = coerce_rows(value)?;
    if raw.is_empty() {
        return Ok(ParsedGenerations {
            rows: default_generations(),
            warnings: vec![],
            migrated: false,
        });
    }
    let mut maps = Vec::with_capacity(raw.len());
    for (index, entry) in raw.into_iter().enumerate() {
        match entry {
            Value::Object(m) => maps.push(m),
            other => {
                return Err(GenerationError {
                    message: format!(
                        "ocr.generations[{index}]: each generation must be a mapping of fields, got {}",
                        json_type(&other)
                    ),
                    row: Some(index),
                    field: None,
                });
            }
        }
    }
    let ids = assign_ids(&maps)?;
    let mut out = ParsedGenerations::default();
    let mut names: HashMap<String, usize> = HashMap::new();
    for (index, row) in maps.iter().enumerate() {
        let spec = parse_row(index, row, &ids[index], &names, &mut out)?;
        names.insert(spec.name.clone(), index);
        out.rows.push(spec);
    }
    ensure_runnable_primary(&mut out);
    validate_primary(&out.rows)?;
    Ok(out)
}

/// Validate a benchmark spec (the row as edited in the admin UI) like a saved row.
pub fn parse_bench_spec(value: &Value) -> Result<Generation, GenerationError> {
    let Value::Object(m) = value else {
        return Err(GenerationError {
            message: format!(
                "spec must be a mapping of engine, detector, patch_budget and pools, got {}",
                json_type(value)
            ),
            row: None,
            field: None,
        });
    };
    let row: Map<String, Value> = m
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "name" | "primary" | "enabled" | "id"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut scratch = ParsedGenerations::default();
    let spec = parse_row(0, &row, "bench-spec", &HashMap::new(), &mut scratch).map_err(|e| {
        let msg = e
            .message
            .strip_prefix("ocr.generations[0]: ")
            .unwrap_or(&e.message)
            .to_string();
        GenerationError {
            message: format!("spec: {msg}"),
            row: None,
            field: e.field,
        }
    })?;
    if let Some(why) = spec.retired {
        return Err(GenerationError {
            message: format!("spec: {why}"),
            row: None,
            field: Some("engine"),
        });
    }
    Ok(spec)
}

fn assign_ids(rows: &[Map<String, Value>]) -> Result<Vec<String>, GenerationError> {
    let mut taken: HashSet<String> = HashSet::new();
    for (index, row) in rows.iter().enumerate() {
        let value = value_str(row.get("id"));
        if value.is_empty() {
            continue;
        }
        if !is_valid_id(&value) {
            return Err(row_err(
                index,
                "id",
                format!(
                    "id '{value}' is not usable — ids are letters, digits, '-' and '_' only (they name this row's \
                     working directories); leave it out and the server mints one"
                ),
            ));
        }
        if !taken.insert(value.clone()) {
            return Err(row_err(
                index,
                "id",
                format!(
                    "id '{value}' is already used by an earlier generation; ids identify a row for its whole life \
                     and must be unique"
                ),
            ));
        }
    }
    let mut assigned = Vec::with_capacity(rows.len());
    for row in rows {
        let value = value_str(row.get("id"));
        if value.is_empty() {
            let minted = mint_generation_id(taken.iter().map(String::as_str));
            taken.insert(minted.clone());
            assigned.push(minted);
        } else {
            assigned.push(value);
        }
    }
    Ok(assigned)
}

fn parse_row(
    index: usize,
    row: &Map<String, Value>,
    row_id: &str,
    names: &HashMap<String, usize>,
    out: &mut ParsedGenerations,
) -> Result<Generation, GenerationError> {
    let engine = value_str(row.get("engine"));
    if engine.is_empty() {
        let known: Vec<_> = engines::ENGINES.iter().map(|e| e.id).collect();
        return Err(row_err(
            index,
            "engine",
            format!("engine is required (one of {})", known.join(", ")),
        ));
    }

    let raw_name = value_str(row.get("name"));

    if let Some(why) = engines::removed_engine(&engine) {
        // Kept, never run. Its name stays taken so a new row cannot collide with its files.
        let name = if raw_name.is_empty() {
            engine.clone()
        } else {
            raw_name
        };
        if let Some(prev) = names.get(&name) {
            return Err(row_err(
                index,
                "name",
                format!(
                    "name '{name}' is already generation {prev}'s; every generation writes a file named after it, so names must be unique"
                ),
            ));
        }
        let was_primary = value_bool(row.get("primary"), false);
        out.warnings.push(format!(
            "ocr.generations[{index}] '{name}': {why}. The row is kept but no longer runs{}.",
            if was_primary {
                "; its existing <Volume>.mokuro files stay and are still served"
            } else {
                ""
            }
        ));
        out.migrated = true;
        return Ok(Generation {
            id: row_id.to_string(),
            name,
            engine,
            primary: false,
            enabled: false,
            detector: None,
            patch_budget: engines::DEFAULT_PATCH_BUDGET,
            pools: Pools::default(),
            precision: engines::DEFAULT_PRECISION_MODE.to_string(),
            precision_pick: None,
            precision_why: String::new(),
            retired: Some(why.to_string()),
            raw: Some(row.clone()),
        });
    }

    let Some(spec) = engines::engine(&engine) else {
        let known: Vec<_> = engines::ENGINES.iter().map(|e| e.id).collect();
        return Err(row_err(
            index,
            "engine",
            format!(
                "Unknown OCR engine '{engine}' (known: {})",
                known.join(", ")
            ),
        ));
    };

    let mut detector = None;
    if spec.detector.is_none() {
        let mut wanted = value_str(row.get("detector"));
        if wanted.is_empty() {
            wanted = engines::DEFAULT_DETECTOR.to_string();
        }
        if let Some(why) = engines::removed_detector(&wanted) {
            out.warnings.push(format!(
                "ocr.generations[{index}]: {why}; this row now reads with the {} detector",
                engines::DEFAULT_DETECTOR
            ));
            out.migrated = true;
            wanted = engines::DEFAULT_DETECTOR.to_string();
        }
        let Some(d) = engines::detector(&wanted) else {
            let known: Vec<_> = engines::DETECTORS.iter().map(|d| d.id).collect();
            return Err(row_err(
                index,
                "detector",
                format!(
                    "Unknown OCR detector '{wanted}' (known: {})",
                    known.join(", ")
                ),
            ));
        };
        detector = Some(d.id.to_string());
    }

    let name = if raw_name.is_empty() {
        seed_generation_name(
            &engine,
            detector.as_deref(),
            names.keys().map(String::as_str),
        )
    } else {
        raw_name
    };
    if let Some(rejection) = name_rejection(&name) {
        return Err(row_err(index, "name", rejection));
    }
    if let Some(prev) = names.get(&name) {
        return Err(row_err(
            index,
            "name",
            format!(
                "name '{name}' is already generation {prev}'s; every generation writes a file named after it, so names must be unique"
            ),
        ));
    }

    if row.contains_key("char_map") {
        return Err(row_err(
            index,
            "char_map",
            "char_map was removed with the character-map system (no per-character placement mode produced output \
             worth using; readers lay characters on a uniform grid) -- delete the key",
        ));
    }

    let patch_budget = match row.get("patch_budget") {
        None | Some(Value::Null) => engines::DEFAULT_PATCH_BUDGET,
        Some(v) => {
            let text = value_str(Some(v));
            let parsed: Option<u32> = text.parse().ok();
            match parsed {
                Some(p) if engines::PATCH_BUDGETS.contains(&p) => p,
                Some(_) => {
                    let known: Vec<String> =
                        engines::PATCH_BUDGETS.iter().map(u32::to_string).collect();
                    return Err(row_err(
                        index,
                        "patch_budget",
                        format!(
                            "Unknown OCR patch budget '{text}' (known: {})",
                            known.join(", ")
                        ),
                    ));
                }
                None => {
                    return Err(row_err(
                        index,
                        "patch_budget",
                        format!("Invalid OCR patch budget '{text}'"),
                    ));
                }
            }
        }
    };

    let pools_raw = row.get("pools");
    let legacy_precision = pools_raw
        .and_then(|p| p.get("precision"))
        .map(|v| value_str(Some(v)));
    let precision_raw = value_str(row.get("precision"));
    let precision_value = if precision_raw.is_empty() {
        legacy_precision.filter(|s| !s.is_empty())
    } else {
        Some(precision_raw)
    };
    let mut precision = engines::normalize_precision_mode(precision_value.as_deref())
        .map_err(|e| row_err(index, "precision", e))?
        .to_string();
    if !spec.precision {
        precision = engines::DEFAULT_PRECISION_MODE.to_string();
    }

    let pick = value_str(row.get("precision_pick"));
    let (precision_pick, precision_why) = if engines::PRECISIONS.contains(&pick.as_str()) {
        (Some(pick), value_str(row.get("precision_why")))
    } else {
        (None, String::new())
    };

    let mut row_spec = Generation {
        id: row_id.to_string(),
        name,
        engine: spec.id.to_string(),
        primary: value_bool(row.get("primary"), false),
        enabled: value_bool(row.get("enabled"), true),
        detector,
        patch_budget,
        pools: Pools::default(),
        precision,
        precision_pick,
        precision_why,
        retired: None,
        raw: None,
    };
    row_spec.pools = parse_pools(index, pools_raw, &row_spec, out)?;
    Ok(row_spec)
}

fn parse_pools(
    index: usize,
    raw: Option<&Value>,
    spec: &Generation,
    out: &mut ParsedGenerations,
) -> Result<Pools, GenerationError> {
    let raw = match raw {
        None | Some(Value::Null) => return Ok(Pools::default()),
        Some(Value::Object(m)) => m,
        Some(_) => {
            return Err(row_err(
                index,
                "pools",
                "pools must be a mapping with 'stage_workers', 'queue_capacity', 'stage_device', 'precision'",
            ));
        }
    };
    if let Some(unknown) = raw.keys().find(|k| !POOL_KEYS.contains(&k.as_str())) {
        return Err(row_err(
            index,
            "pools",
            format!(
                "pools has no '{unknown}' setting (the settings are {})",
                POOL_KEYS.join(", ")
            ),
        ));
    }
    let stage_keys = spec.stage_keys();
    let mut pools = Pools::default();
    for (pool_key, limit, floor) in [
        ("stage_workers", MAX_STAGE_WORKERS, 0u32),
        ("queue_capacity", MAX_QUEUE_CAPACITY, 1u32),
    ] {
        let Some(values) = raw.get(pool_key) else {
            continue;
        };
        if values.is_null() {
            continue;
        }
        let Value::Object(values) = values else {
            return Err(row_err(
                index,
                "pools",
                format!("pools.{pool_key} must map a stage name to a number"),
            ));
        };
        for (stage, number) in values {
            let key = stage.trim();
            if !stage_keys.contains(&key) {
                // 0.5.2's served/adapter roads had stage names 0.7 does not; a tuning for a
                // stage that no longer exists is dropped, not fatal.
                out.warnings.push(format!(
                    "ocr.generations[{index}]: pools.{pool_key}.{key} names a stage this row no longer has ({}); ignored",
                    stage_keys.join(", ")
                ));
                out.migrated = true;
                continue;
            }
            let width: i64 = match number {
                Value::Number(n) => n
                    .as_i64()
                    .or_else(|| n.as_f64().map(|f| f as i64))
                    .unwrap_or(-1),
                Value::String(s) => s.trim().parse().map_err(|_| {
                    row_err(
                        index,
                        "pools",
                        format!("pools.{pool_key}.{key} must be a whole number, got '{s}'"),
                    )
                })?,
                other => {
                    return Err(row_err(
                        index,
                        "pools",
                        format!("pools.{pool_key}.{key} must be a whole number, got {other}"),
                    ));
                }
            };
            if width < floor as i64 || width > limit as i64 {
                return Err(row_err(
                    index,
                    "pools",
                    format!(
                        "pools.{pool_key}.{key} is {width}; it must be between {floor} and {limit}"
                    ),
                ));
            }
            let map = if pool_key == "stage_workers" {
                &mut pools.stage_workers
            } else {
                &mut pools.queue_capacity
            };
            map.insert(key.to_string(), width as u32);
        }
    }
    if let Some(devs) = raw.get("stage_device").filter(|v| !v.is_null()) {
        let Value::Object(devs) = devs else {
            return Err(row_err(
                index,
                "pools",
                "pools.stage_device must map a stage name to a device (cpu or gpu:<n>)",
            ));
        };
        let allowed = spec.device_stage_keys();
        for (stage, value) in devs {
            let key = stage.trim();
            if !allowed.contains(&key) {
                out.warnings.push(format!(
                    "ocr.generations[{index}]: pools.stage_device.{key} names a stage without a model in 0.7 ({}); ignored",
                    allowed.join(", ")
                ));
                out.migrated = true;
                continue;
            }
            if value.is_null() {
                continue;
            }
            let device = value_str(Some(value));
            let device = parse_device(&device)
                .map_err(|e| row_err(index, "pools", format!("pools.stage_device.{key}: {e}")))?;
            let cpu_locked = match key {
                "detect" => true, // the PP-OCR detector is CPU-only
                _ => spec.engine_spec().is_some_and(|e| e.cpu_only()),
            };
            if cpu_locked && device != "auto" && device != "cpu" {
                return Err(row_err(
                    index,
                    "pools",
                    format!(
                        "pools.stage_device.{key} is '{device}', but that stage runs on the CPU; leave it on cpu"
                    ),
                ));
            }
            pools.stage_device.insert(key.to_string(), device);
        }
    }
    Ok(pools)
}

/// `auto`, `cpu` or `gpu:<n>`.
pub fn parse_device(value: &str) -> Result<String, String> {
    let v = value.trim();
    match v {
        "auto" | "cpu" => Ok(v.to_string()),
        "gpu" | "cuda" => Ok("gpu:0".to_string()),
        _ => {
            if let Some(n) = v.strip_prefix("gpu:").or_else(|| v.strip_prefix("cuda:"))
                && let Ok(i) = n.parse::<u32>()
            {
                return Ok(format!("gpu:{i}"));
            }
            Err(format!("'{v}' is not a device (use auto, cpu or gpu:<n>)"))
        }
    }
}

/// When migration left no runnable enabled primary, add the default one.
fn ensure_runnable_primary(out: &mut ParsedGenerations) {
    if out.rows.iter().any(|g| g.runnable() && g.primary) {
        return;
    }
    let any_runnable_enabled = out.rows.iter().any(|g| g.runnable());
    let lost_primary = out.rows.iter().any(|g| {
        g.retired.is_some()
            && g.raw.as_ref().is_some_and(|r| {
                value_bool(r.get("primary"), false) && value_bool(r.get("enabled"), true)
            })
    });
    if !lost_primary && any_runnable_enabled {
        // Leave the ordinary "no primary" error to validate_primary.
        return;
    }
    if !lost_primary {
        return;
    }
    let ids: Vec<String> = out.rows.iter().map(|g| g.id.clone()).collect();
    let names: Vec<String> = out.rows.iter().map(|g| g.name.clone()).collect();
    let mut row = default_generation(&mint_generation_id(ids.iter().map(String::as_str)));
    row.name = seed_generation_name(&row.engine, None, names.iter().map(String::as_str));
    // An existing hayai-nova layer row becomes the primary instead of adding a duplicate.
    if let Some(existing) = out
        .rows
        .iter_mut()
        .find(|g| g.runnable() && g.engine == engines::DEFAULT_ENGINE)
    {
        existing.primary = true;
        out.warnings.push(format!(
            "ocr.generations: the primary row was retired; '{}' is now the primary generation (new volumes get \
             <Volume>.mokuro from it)",
            existing.name
        ));
    } else {
        out.warnings.push(format!(
            "ocr.generations: the primary row was retired; added '{}' ({} + {} detector) as the primary generation",
            row.name,
            engines::DEFAULT_ENGINE,
            engines::DEFAULT_DETECTOR
        ));
        out.rows.insert(0, row);
    }
    out.migrated = true;
}

fn validate_primary(rows: &[Generation]) -> Result<(), GenerationError> {
    let enabled: Vec<&Generation> = rows.iter().filter(|g| g.runnable()).collect();
    if enabled.is_empty() {
        return Ok(());
    }
    let primaries: Vec<&Generation> = enabled.iter().copied().filter(|g| g.primary).collect();
    if primaries.is_empty() {
        return Err(GenerationError {
            message: "ocr.generations: no enabled generation is the primary one — exactly one must be, because it \
                      writes the bare <Volume>.mokuro every reader counts characters and inherits volume ids from"
                .into(),
            row: None,
            field: Some("primary"),
        });
    }
    if primaries.len() > 1 {
        let names: Vec<String> = primaries.iter().map(|g| format!("'{}'", g.name)).collect();
        let row = rows.iter().position(|g| g.id == primaries[1].id);
        return Err(GenerationError {
            message: format!(
                "ocr.generations: {} are all marked primary — exactly one enabled generation may write the bare <Volume>.mokuro",
                names.join(", ")
            ),
            row,
            field: Some("primary"),
        });
    }
    Ok(())
}

/// The rows the queue runs, in list order.
pub fn enabled_generations(rows: &[Generation]) -> impl Iterator<Item = &Generation> {
    rows.iter().filter(|g| g.runnable())
}

pub fn primary_generation(rows: &[Generation]) -> Option<&Generation> {
    enabled_generations(rows).find(|g| g.primary)
}

pub fn generation_by_id<'a>(rows: &'a [Generation], id: &str) -> Option<&'a Generation> {
    rows.iter().find(|g| g.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_default_hayai_primary() {
        let p = parse_generation_list(&Value::Null).unwrap();
        assert_eq!(p.rows.len(), 1);
        assert_eq!(p.rows[0].engine, "hayai-nova");
        assert!(p.rows[0].primary);
        assert_eq!(p.rows[0].sidecar_suffix(), ".mokuro");
    }

    #[test]
    fn mokuro_primary_is_retired_and_replaced() {
        let v = json!([{"name": "mokuro", "engine": "mokuro", "primary": true}]);
        let p = parse_generation_list(&v).unwrap();
        assert_eq!(p.rows.len(), 2);
        assert_eq!(p.rows[0].engine, "hayai-nova");
        assert!(p.rows[0].primary);
        assert!(p.rows[1].retired.is_some());
        assert_eq!(p.rows[1].name, "mokuro");
        assert!(p.migrated);
        // Retired row saves back with its original fields.
        let saved = p.rows[1].to_value();
        assert_eq!(saved["engine"], "mokuro");
        assert_eq!(saved["enabled"], false);
    }

    #[test]
    fn existing_hayai_layer_is_promoted() {
        let v = json!([
            {"name": "mokuro", "engine": "mokuro", "primary": true},
            {"name": "hayai-nova", "engine": "hayai-nova", "detector": "rtdetr"}
        ]);
        let p = parse_generation_list(&v).unwrap();
        assert_eq!(p.rows.len(), 2);
        let hayai = p.rows.iter().find(|g| g.engine == "hayai-nova").unwrap();
        assert!(hayai.primary);
        assert_eq!(hayai.detector.as_deref(), Some("ppocr-manga"));
    }

    #[test]
    fn names_follow_reader_grammar() {
        assert!(name_rejection("hayai-nova").is_none());
        assert!(name_rejection("Hayai").is_some());
        assert!(name_rejection("-x").is_some());
        assert!(name_rejection("original").is_some());
        assert!(name_rejection("tr-en").is_some());
        assert!(name_rejection(&"a".repeat(33)).is_some());
    }

    #[test]
    fn duplicate_names_refused() {
        let v = json!([
            {"name": "a", "engine": "hayai-nova", "primary": true},
            {"name": "a", "engine": "paddle-manga"}
        ]);
        let e = parse_generation_list(&v).unwrap_err();
        assert_eq!(e.row, Some(1));
        assert_eq!(e.field, Some("name"));
    }

    #[test]
    fn ids_minted() {
        let v = json!([
            {"id": "g-3", "engine": "hayai-nova", "primary": true},
            {"engine": "paddle-manga"}
        ]);
        let p = parse_generation_list(&v).unwrap();
        assert_eq!(p.rows[1].id, "g-4");
        assert_eq!(p.rows[1].name, "paddle-manga-ppocr-manga");
    }

    #[test]
    fn json_string_accepted() {
        let v = Value::String(r#"[{"engine":"ppocr-manga","primary":true}]"#.into());
        let p = parse_generation_list(&v).unwrap();
        assert_eq!(p.rows[0].name, "ppocr-manga");
        assert_eq!(p.rows[0].detector, None);
    }

    #[test]
    fn pools_validated() {
        let v = json!([{"engine": "hayai-nova", "primary": true,
            "pools": {"stage_workers": {"engine": 4, "crop": 2}, "stage_device": {"engine": "gpu:1", "detect": "cpu"}}}]);
        let p = parse_generation_list(&v).unwrap();
        assert_eq!(p.rows[0].pools.stage_workers.get("engine"), Some(&4));
        assert!(!p.rows[0].pools.stage_workers.contains_key("crop"));
        assert_eq!(
            p.rows[0]
                .pools
                .stage_device
                .get("engine")
                .map(String::as_str),
            Some("gpu:1")
        );
        let bad = json!([{"engine": "hayai-nova", "primary": true, "pools": {"stage_device": {"detect": "gpu:0"}}}]);
        assert!(parse_generation_list(&bad).is_err());
    }
}
