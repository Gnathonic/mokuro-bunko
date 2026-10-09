//! The OCR half of the admin panel (spec ocr-generations-bench §8, db-auth-admin §18.8,
//! web-frontend-contract §5.8 "Settings > OCR Generations").
//!
//! The HTTP side (routing, body parsing, the config lock, saving `ocr.generations`, the
//! generic validation 0.5.2 did before reaching its OCR objects) lives here; everything
//! that needs the scheduler, processors, device probes or benchmarks goes through
//! [`OcrAdmin`]. [`NoOcr`] answers for a process that runs no OCR at all, with the
//! shapes 0.5.2 sent when it had no OCR control, so the panel renders and degrades.
//!
//! The helpers [`catalog`], [`generation_entry`], [`stage_rows`] and [`default_devices`]
//! build the registry-derived JSON from `bunko_core::engines`; a real implementation can
//! start from them and fill in what only it knows (counts, benches, processors).
//! Engines removed in 0.7 are never offered; a config row still naming one is shown with
//! its `retired` reason.

use ::http::Method;
use bunko_core::Config;
use bunko_core::engines::{self, Road};
use bunko_core::generations::{self, Generation, GenerationError, ParsedGenerations};
use serde_json::{Map, Value, json};

/// An OCR endpoint's refusal: the status and the whole JSON body (`{"error", ...}`;
/// generation errors also carry `row` and `field`).
#[derive(Debug, Clone, PartialEq)]
pub struct OcrError {
    pub status: u16,
    pub body: Value,
}

impl OcrError {
    /// `{"error": message}`.
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        OcrError {
            status,
            body: json!({"error": message.into()}),
        }
    }

    /// `{"error", "row", "field"}` (the generations editor puts the message on that row).
    pub fn at(
        status: u16,
        message: impl Into<String>,
        row: Option<usize>,
        field: Option<&str>,
    ) -> Self {
        OcrError {
            status,
            body: json!({"error": message.into(), "row": row, "field": field}),
        }
    }

    /// `404 {"error": "API endpoint not found"}`: what a server without the feature says
    /// (the page hides benchmark controls on a 404 for a real row).
    pub fn not_found() -> Self {
        OcrError::new(404, "API endpoint not found")
    }

    /// `400 {"error": "no processor called 'x' is known"}`.
    pub fn unknown_processor(name: &str) -> Self {
        OcrError::new(
            400,
            format!(
                "no processor called {} is known",
                bunko_db::pyfmt::repr_str(name)
            ),
        )
    }
}

impl From<GenerationError> for OcrError {
    fn from(e: GenerationError) -> Self {
        OcrError::at(400, e.message, e.row, e.field)
    }
}

/// `/api/ocr/generations/<key>/bench`, by method.
#[derive(Debug, Clone, PartialEq)]
pub enum BenchRequest {
    /// POST: `spec`/`pages` as sent (absent → None); `processor` defaults to `local`.
    Enqueue {
        spec: Option<Value>,
        pages: Option<Value>,
        processor: String,
    },
    /// GET `?processor=` (None → this server's own).
    Get { processor: Option<String> },
    /// DELETE `?processor=`.
    Cancel { processor: Option<String> },
}

/// What the admin panel asks of the OCR subsystem. Every method is synchronous and may
/// block (the HTTP side calls it from `spawn_blocking`); the `Config` passed in is a
/// snapshot of the live config. Returned values are the JSON bodies of the contracts in
/// spec ocr-generations-bench §8.
pub trait OcrAdmin: Send + Sync + 'static {
    /// `ocr_runtime` of `GET /api/settings` (also echoed after OCR settings changes).
    /// Must at least carry `available` and `local_processing`.
    fn runtime_status(&self, config: &Config) -> Value;

    /// `GET /api/processors`: `{processors, speed, failed_logins, last_disconnect,
    /// local_processing, processing_hold}`.
    fn processors(&self, config: &Config) -> Value;

    /// `GET /api/ocr/generations`: `{stats_pending, generations, catalog, processors,
    /// local_processing, autobench}`. Also the body of a successful PUT.
    fn generations_payload(&self, config: &Config) -> Value;

    /// `GET /api/ocr/generations/stats`: `{stats_pending: true}` or
    /// `{stats_pending: false, computed_at, generations: {id: {...}}}`.
    fn generation_stats(&self, config: &Config) -> Value;

    /// Validate a full replacement list (`PUT /api/ocr/generations`), against every
    /// machine's devices. The default is the registry-only check of `bunko_core`.
    fn parse_generations(
        &self,
        config: &Config,
        rows: &Value,
    ) -> Result<ParsedGenerations, GenerationError> {
        let _ = config;
        generations::parse_generation_list(rows)
    }

    /// Push saved OCR settings (generations, poll interval) into the running scheduler.
    /// Returns `{applied, installing, restart_required, reason}`; `None` means "nothing
    /// to push into" and the caller answers `restart_required: <changed>`.
    fn apply(&self, config: &Config) -> Option<Value>;

    /// Forget benchmarks and processor profiles of rows that no longer exist.
    fn prune(&self, known_ids: &[String]) {
        let _ = known_ids;
    }

    /// `POST /api/ocr/generations/derive`: `{road, stages}` for an unsaved spec, as this
    /// server (processor None) or the named processor would run it.
    fn derive(
        &self,
        config: &Config,
        spec: &Value,
        processor: Option<&str>,
    ) -> Result<Value, OcrError>;

    /// `PUT /api/ocr/generations/<id>/pools` after the generic checks (a processor is
    /// named, the row exists): unknown machine, `pools must be an object`, validation,
    /// storage. Returns `{"success": true, "pools": {...}}`.
    fn set_pools(
        &self,
        config: &Config,
        row: &Generation,
        processor: &str,
        pools: &Value,
    ) -> Result<Value, OcrError>;

    /// The bench endpoints: `(status, body)` (POST answers 202).
    fn bench(
        &self,
        config: &Config,
        key: &str,
        request: BenchRequest,
    ) -> Result<(u16, Value), OcrError>;

    /// `POST /api/ocr/devices/refresh`: `{"success": true, "devices": [...]}`.
    fn refresh_devices(&self) -> Value;

    /// `PUT /api/settings/queue` saved: the queue page's state must move.
    fn queue_changed(&self) {}

    /// Any other `/api/ocr/...` path (`path` = the segments after `/api/`), so the OCR
    /// subsystem can add endpoints without touching the admin router. `None` → 404.
    fn other(
        &self,
        config: &Config,
        method: &Method,
        path: &[&str],
        query: &str,
        body: &Value,
    ) -> Option<Result<(u16, Value), OcrError>> {
        let _ = (config, method, path, query, body);
        None
    }
}

// --- registry-derived JSON ------------------------------------------------------------------

/// `catalog.devices` when nothing was probed: `auto` (resolving to the CPU) and `cpu`.
pub fn default_devices() -> Value {
    json!([{"id": "auto", "label": "Auto — CPU"}, {"id": "cpu", "label": "CPU"}])
}

fn precision_label(mode: &str) -> &'static str {
    match mode {
        engines::MODE_ACCURACY => "Auto: accuracy",
        engines::MODE_BALANCED => "Auto: balanced",
        engines::MODE_SPEED => "Auto: speed",
        engines::PRECISION_FP32 => "fp32 only",
        engines::PRECISION_BF16 => "bf16 only (cards that support it)",
        engines::PRECISION_FP16 => "fp16 only (GPUs)",
        _ => "",
    }
}

/// The 0.5.2 `GENERATION_NAME_RE` source the page builds its name check from.
pub const NAME_PATTERN: &str = "^[a-z0-9][a-z0-9-]{0,31}$";

/// `catalog`: what a row may be set to. Only the engines and detectors 0.7 ships; the
/// 0.5.2 flags for the mokuro engine (`monolithic`, `served`, `own_environment`) are
/// always false and kept because `admin.js` reads them.
pub fn catalog(devices: Value) -> Value {
    let engines: Vec<Value> = engines::ENGINES
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "label": e.label,
                "monolithic": false,
                "served": false,
                "own_environment": false,
                "own_detector": e.detector,
                "patch_budget": e.patch_budget,
                "precision": e.precision,
                "precision_modes": if e.precision { engines::PRECISION_MODES.to_vec() } else { vec![] },
                // `["cpu"]`, `"gpu"` (a GPU only: paddle-manga) or `"any"`.
                "devices": if e.cpu_only() { json!(["cpu"]) } else if e.gpu_only() { json!("gpu") } else { json!("any") },
                "gpu_only_reason": (!e.gpu_only_reason.is_empty()).then_some(e.gpu_only_reason),
            })
        })
        .collect();
    let detectors: Vec<Value> = engines::DETECTORS
        .iter()
        .map(|d| {
            json!({
                "id": d.id,
                "label": d.label,
                "devices": if d.cpu_only_reason.is_empty() { json!("any") } else { json!(["cpu"]) },
            })
        })
        .collect();
    json!({
        "engines": engines,
        "detectors": detectors,
        "devices": devices,
        "patch_budgets": engines::PATCH_BUDGETS,
        "precision_modes": engines::PRECISION_MODES.iter().map(|m| json!({"id": m, "label": precision_label(m)})).collect::<Vec<_>>(),
        "precision_default": engines::DEFAULT_PRECISION_MODE,
        "name_pattern": NAME_PATTERN,
        "reserved_names": generations::RESERVED_NAMES,
        "reserved_prefixes": generations::RESERVED_PREFIXES,
    })
}

/// The stage names `STAGE_GRAPHS` declares (kept verbatim from 0.5.2).
fn stage_name(road: Road, key: &str) -> &'static str {
    match (road, key) {
        (_, engines::STAGE_DETECT) => "detect + CTC read",
        (Road::Line, engines::STAGE_LAYOUT) => "layout + dump",
        (Road::Reconciled, engines::STAGE_ENGINE) => "engine read + reconcile",
        (Road::Reconciled, engines::STAGE_POST) => "layout + dump",
        _ => "",
    }
}

/// Why a model stage may not leave the CPU, from the registry (0.5.2 `stage_lock_reason`
/// without the host's onnxruntime facts).
pub fn stage_lock_reason(row: &Generation, key: &str) -> Option<String> {
    let engine = row.engine_spec()?;
    let road = engine.road();
    if !road.device_stage_keys().contains(&key) {
        return None;
    }
    let text = if key == engines::STAGE_ENGINE {
        engine.cpu_only_reason
    } else if engine.detector.is_some() && !engine.cpu_only_reason.is_empty() {
        // The engine brings its own detector: its reason is the engine's.
        engine.cpu_only_reason
    } else {
        engines::detector(row.effective_detector())
            .map(|d| d.cpu_only_reason)
            .unwrap_or("")
    };
    (!text.is_empty()).then(|| text.to_string())
}

fn device_short(device: &str) -> String {
    match device {
        "cpu" => "CPU".into(),
        "gpu" => "GPU".into(),
        d => match d.strip_prefix("gpu:") {
            Some(i) => format!("GPU {i}"),
            None => d.to_string(),
        },
    }
}

/// `stages[]` of a row as the registry alone describes it: keys, names, which stages take
/// a device and which are locked to the CPU. With no host facts (`NoOcr`), `auto`
/// resolves to the CPU and the derived widths/capacities are unknown (null).
pub fn stage_rows(row: &Generation, devices: &Value) -> Vec<Value> {
    let Some(road) = row.road() else {
        return vec![];
    };
    let ids: Vec<String> = devices
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| d.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let label_of = |id: &str| -> String {
        devices
            .as_array()
            .and_then(|a| {
                a.iter()
                    .find(|d| d.get("id").and_then(Value::as_str) == Some(id))
            })
            .and_then(|d| d.get("label").and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| device_short(id))
    };
    road.stage_keys()
        .iter()
        .map(|key| {
            let takes_device = road.device_stage_keys().contains(key);
            let locked = stage_lock_reason(row, key);
            // A GPU-only engine (paddle-manga): its engine stage never offers the CPU,
            // and `auto` means the first GPU (or "a GPU" when none is known here).
            let needs_gpu = (*key == engines::STAGE_ENGINE)
                .then(|| row.engine_spec().filter(|e| e.gpu_only()))
                .flatten()
                .map(|e| e.gpu_only_reason);
            let allowed: Vec<String> = if !takes_device {
                vec![]
            } else if locked.is_some() {
                vec!["auto".into(), "cpu".into()]
            } else if needs_gpu.is_some() {
                ids.iter().filter(|id| *id != "cpu").cloned().collect()
            } else {
                ids.clone()
            };
            let device = match row.pools.stage_device.get(*key) {
                Some(d) if takes_device && locked.is_none() && d != "auto" => d.clone(),
                _ if needs_gpu.is_some() => ids
                    .iter()
                    .find(|id| id.starts_with("gpu:"))
                    .cloned()
                    .unwrap_or_else(|| "gpu".to_string()),
                _ => "cpu".to_string(),
            };
            let options: Vec<Value> = allowed
                .iter()
                .map(|id| {
                    let label = if id == "auto" && needs_gpu.is_some() {
                        // The first GPU (a pin does not change what `auto` means).
                        let first = ids.iter().find(|i| i.starts_with("gpu:"));
                        format!("Auto → {}", device_short(first.map_or("gpu", |s| s)))
                    } else if id == "auto" {
                        "Auto → CPU".to_string()
                    } else {
                        label_of(id)
                    };
                    json!({"id": id, "label": label})
                })
                .collect();
            let means = if *key == engines::STAGE_ENGINE && device.starts_with("gpu") {
                "copies"
            } else {
                "pool"
            };
            json!({
                "key": key,
                "name": stage_name(road, key),
                "device": device,
                "max_workers": null,
                "derived_workers": null,
                "derived_capacity": null,
                "devices_allowed": allowed,
                "device_options": options,
                "device_locked_reason": locked,
                "device_needs_gpu": needs_gpu,
                "workers_means": means,
            })
        })
        .collect()
}

/// One `generations[]` entry with everything the registry can derive, in 0.5.2's key
/// order; counts, benches and per-machine data are null/empty for the caller to fill in.
/// Adds `retired` (new in 0.7): why the row no longer runs, or null.
pub fn generation_entry(row: &Generation, devices: &Value) -> Map<String, Value> {
    let mut entry = match row.to_value() {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    entry.entry("detector").or_insert(Value::Null);
    let spec = row.engine_spec().filter(|_| row.retired.is_none());
    let mut put = |k: &str, v: Value| {
        entry.insert(k.to_string(), v);
    };
    put(
        "sidecar",
        json!(format!("<Volume>{}", row.sidecar_suffix())),
    );
    put(
        "effective_detector",
        if spec.is_some() {
            json!(row.effective_detector())
        } else {
            Value::Null
        },
    );
    put(
        "detector_locked",
        json!(spec.is_none_or(|e| e.detector.is_some())),
    );
    put(
        "patch_budget_applies",
        json!(spec.is_some() && row.patch_budget_applies()),
    );
    put(
        "precision_applies",
        json!(spec.is_some() && row.precision_applies()),
    );
    put(
        "road",
        spec.map(|e| json!(e.road().as_str()))
            .unwrap_or(Value::Null),
    );
    put(
        "stages",
        json!(if spec.is_some() {
            stage_rows(row, devices)
        } else {
            vec![]
        }),
    );
    put("volumes_done", Value::Null);
    put("volumes_total", Value::Null);
    put("congestion", Value::Null);
    put("processor_stages", json!({}));
    put("volumes_skipped", json!(0));
    put("volumes_by_machine", json!({}));
    put("bench", Value::Null);
    if spec.is_some() && row.precision_applies() {
        put("precision", json!(row.precision));
        put("precision_on", json!({}));
    }
    put("precision_hold", Value::Null);
    put("configured", json!(!row.pools.is_empty()));
    put("local_pools", Value::Null);
    put("local_bench", Value::Null);
    put("local_runs", Value::Null);
    put("processor_pools", json!({}));
    put("processor_bench", json!({}));
    put("processor_runs", json!({}));
    put("processor_congestion", json!({}));
    put("retired", json!(row.retired));
    entry
}

// --- NoOcr ---------------------------------------------------------------------------------

/// The admin side of a process that runs no OCR at all (the lite build before an OCR
/// scheduler is wired in): the generations list is still viewable and editable (it is
/// config), and every OCR-only feature answers the way 0.5.2 did without OCR control.
#[derive(Debug, Clone, Default)]
pub struct NoOcr;

/// Shown as the OCR environment hint when this process runs no OCR.
pub const NO_OCR_HINT: &str =
    "This server process runs no OCR itself; volumes are read by processors connected to it.";

impl OcrAdmin for NoOcr {
    fn runtime_status(&self, config: &Config) -> Value {
        json!({
            "available": false,
            "launch_only": true,
            "configured_backend": config.ocr.backend,
            "local_processing": false,
            "generations": config.ocr.generations.iter().map(Generation::to_value).collect::<Vec<_>>(),
            "detectors": [],
            "supported_backends": [],
            "cli_hint": NO_OCR_HINT,
            "driver_hint": "",
        })
    }

    fn processors(&self, _config: &Config) -> Value {
        json!({
            "processors": [],
            "failed_logins": [],
            "last_disconnect": null,
            "local_processing": false,
            "processing_hold": null,
            "speed": [],
        })
    }

    fn generations_payload(&self, config: &Config) -> Value {
        let devices = default_devices();
        let rows: Vec<Value> = config
            .ocr
            .generations
            .iter()
            .map(|g| Value::Object(generation_entry(g, &devices)))
            .collect();
        json!({
            "stats_pending": false,
            "generations": rows,
            "catalog": catalog(devices),
            "processors": [],
            "local_processing": false,
            "autobench": config.ocr.autobench,
        })
    }

    fn generation_stats(&self, config: &Config) -> Value {
        let generations: Map<String, Value> = config
            .ocr
            .generations
            .iter()
            .map(|g| {
                (g.id.clone(), json!({"volumes_done": null, "volumes_total": null, "volumes_skipped": 0, "volumes_by_machine": {}}))
            })
            .collect();
        json!({"stats_pending": false, "computed_at": crate::ops::dyndns::utc_stamp(), "generations": generations})
    }

    fn apply(&self, _config: &Config) -> Option<Value> {
        Some(json!({
            "applied": false,
            "installing": false,
            "restart_required": false,
            "reason": "Saved. This server process runs no OCR itself, so there is nothing to restart.",
        }))
    }

    fn derive(
        &self,
        _config: &Config,
        spec: &Value,
        processor: Option<&str>,
    ) -> Result<Value, OcrError> {
        if let Some(name) = processor {
            return Err(OcrError::unknown_processor(name));
        }
        let row = generations::parse_bench_spec(spec)?;
        let road = row.road().map(|r| r.as_str());
        Ok(json!({"road": road, "stages": stage_rows(&row, &default_devices())}))
    }

    fn set_pools(
        &self,
        _config: &Config,
        _row: &Generation,
        processor: &str,
        _pools: &Value,
    ) -> Result<Value, OcrError> {
        Err(OcrError::unknown_processor(processor))
    }

    fn bench(
        &self,
        _config: &Config,
        _key: &str,
        _request: BenchRequest,
    ) -> Result<(u16, Value), OcrError> {
        Err(OcrError::not_found())
    }

    fn refresh_devices(&self) -> Value {
        json!({"success": true, "devices": default_devices()})
    }
}

// --- HTTP ------------------------------------------------------------------------------------

pub(super) mod http {
    use super::{BenchRequest, OcrError};
    use crate::admin::settings::{default_outcome, merge};
    use crate::admin::{
        AdminState, ApiRequest, blocking, error, json_response, not_found, ok, parse_qs,
        save_config,
    };
    use axum::response::Response;
    use bunko_core::Config;
    use serde_json::{Map, Value, json};

    fn respond(r: Result<(u16, Value), OcrError>) -> Response {
        match r {
            Ok((status, body)) => json_response(status, &body),
            Err(e) => json_response(e.status, &e.body),
        }
    }

    fn snapshot(s: &AdminState) -> Config {
        s.core().config.read().clone()
    }

    /// Run `f(ocr, config snapshot)` off the async workers.
    async fn with_ocr<T: Send + 'static>(
        s: &AdminState,
        f: impl FnOnce(&dyn super::OcrAdmin, &Config) -> T + Send + 'static,
    ) -> Result<T, Response> {
        let ocr = s.ocr();
        let cfg = snapshot(s);
        blocking(move || f(ocr.as_ref(), &cfg)).await
    }

    pub async fn processors(s: &AdminState) -> Response {
        with_ocr(s, |o, c| o.processors(c))
            .await
            .map(ok)
            .unwrap_or_else(|r| r)
    }

    pub async fn list(s: &AdminState) -> Response {
        with_ocr(s, |o, c| o.generations_payload(c))
            .await
            .map(ok)
            .unwrap_or_else(|r| r)
    }

    pub async fn stats(s: &AdminState) -> Response {
        with_ocr(s, |o, c| o.generation_stats(c))
            .await
            .map(ok)
            .unwrap_or_else(|r| r)
    }

    pub async fn refresh_devices(s: &AdminState) -> Response {
        with_ocr(s, |o, _| o.refresh_devices())
            .await
            .map(ok)
            .unwrap_or_else(|r| r)
    }

    /// Python `str(value)` for an id in a message.
    fn id_text(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Bool(true) => "True".into(),
            Value::Bool(false) => "False".into(),
            other => other.to_string(),
        }
    }

    /// `PUT /api/ocr/generations`: the whole list, in run order.
    pub async fn replace(s: &AdminState, req: &ApiRequest) -> Response {
        let data = match &req.body {
            Ok(d) => d.clone(),
            Err(e) => return json_response(400, &json!({"error": e, "row": null, "field": null})),
        };
        let Some(rows) = data.get("generations").cloned() else {
            return json_response(
                400,
                &json!({"error": "generations is required (a list of rows, in run order)", "row": null, "field": null}),
            );
        };
        let s2 = s.clone();
        blocking(move || {
            let ocr = s2.ocr();
            let _guard = s2.config_lock.lock();
            let current = snapshot(&s2);
            if let Some(list) = rows.as_array() {
                for (index, row) in list.iter().enumerate() {
                    let Some(raw_id) = row.as_object().and_then(|r| r.get("id")) else { continue };
                    if raw_id.is_null() {
                        continue;
                    }
                    let text = id_text(raw_id);
                    let id = bunko_db::pyfmt::strip(&text);
                    if id.is_empty() {
                        continue;
                    }
                    if !current.ocr.generations.iter().any(|g| g.id == id) {
                        return json_response(
                            400,
                            &json!({
                                "error": format!(
                                    "ocr.generations[{index}]: id {} is not a generation this server knows; leave id out for a new row",
                                    bunko_db::pyfmt::repr_str(&text)
                                ),
                                "row": index,
                                "field": "id",
                            }),
                        );
                    }
                }
            }
            let parsed = match ocr.parse_generations(&current, &rows) {
                Ok(p) => p,
                Err(e) => {
                    let e = OcrError::from(e);
                    return json_response(e.status, &e.body);
                }
            };
            let as_values = |g: &[bunko_core::generations::Generation]| g.iter().map(|r| r.to_value()).collect::<Vec<_>>();
            let changed = as_values(&parsed.rows) != as_values(&current.ocr.generations);
            s2.core().config.write().ocr.generations = parsed.rows;
            if let Err(r) = save_config(&s2) {
                return r;
            }
            let saved = snapshot(&s2);
            let outcome = if changed { ocr.apply(&saved).unwrap_or_else(|| default_outcome(true)) } else { default_outcome(false) };
            if changed {
                let ids: Vec<String> = saved.ocr.generations.iter().map(|g| g.id.clone()).collect();
                ocr.prune(&ids);
            }
            let mut body = Map::new();
            body.insert("success".into(), json!(true));
            merge(&mut body, ocr.generations_payload(&saved));
            merge(&mut body, outcome);
            body.insert("ocr_runtime".into(), ocr.runtime_status(&saved));
            ok(Value::Object(body))
        })
        .await
        .unwrap_or_else(|r| r)
    }

    /// A processor named in a body: a non-empty string other than `local`.
    fn named_processor(v: Option<&Value>) -> Option<String> {
        v.and_then(Value::as_str)
            .filter(|n| !n.is_empty() && *n != "local")
            .map(str::to_string)
    }

    pub async fn derive(s: &AdminState, req: &ApiRequest) -> Response {
        let data = match req.json() {
            Ok(d) => d.clone(),
            Err(r) => return r,
        };
        let spec = data.get("spec").cloned().unwrap_or(Value::Null);
        let processor = named_processor(data.get("processor"));
        with_ocr(s, move |o, c| {
            o.derive(c, &spec, processor.as_deref()).map(|v| (200, v))
        })
        .await
        .map(respond)
        .unwrap_or_else(|r| r)
    }

    pub async fn pools(s: &AdminState, req: &ApiRequest, generation_id: &str) -> Response {
        let data = match req.json() {
            Ok(d) => d.clone(),
            Err(r) => return r,
        };
        let Some(processor) = named_processor(data.get("processor")) else {
            return error(400, "processor must name a processor");
        };
        let id = generation_id.to_string();
        let pools = data.get("pools").cloned().unwrap_or(Value::Null);
        with_ocr(s, move |o, c| {
            let Some(row) = c.ocr.generations.iter().find(|g| g.id == id) else {
                return Err(OcrError::new(
                    400,
                    format!("there is no generation {}", bunko_db::pyfmt::repr_str(&id)),
                ));
            };
            o.set_pools(c, row, &processor, &pools).map(|v| (200, v))
        })
        .await
        .map(respond)
        .unwrap_or_else(|r| r)
    }

    pub async fn bench(s: &AdminState, req: &ApiRequest, key: &str) -> Response {
        let query = parse_qs(&req.query);
        // `?processor=`: the first value as given (0.5.2 did not strip it).
        let asked = query
            .iter()
            .find(|(k, _)| k == "processor")
            .map(|(_, v)| v.clone());
        let request = match req.method.as_str() {
            "POST" => {
                // A body that will not parse is treated as `{}` (0.5.2).
                let body = req.body.clone().unwrap_or_default();
                let processor = match body.get("processor") {
                    None => "local".to_string(),
                    Some(v) if !bunko_db::pyfmt::truthy(v) => "local".to_string(),
                    Some(Value::String(p)) => p.clone(),
                    Some(other) => other.to_string(),
                };
                BenchRequest::Enqueue {
                    spec: body.get("spec").cloned(),
                    pages: body.get("pages").cloned(),
                    processor,
                }
            }
            "GET" => BenchRequest::Get { processor: asked },
            "DELETE" => BenchRequest::Cancel { processor: asked },
            _ => return not_found(),
        };
        let key = key.to_string();
        with_ocr(s, move |o, c| o.bench(c, &key, request))
            .await
            .map(respond)
            .unwrap_or_else(|r| r)
    }

    pub async fn other(s: &AdminState, req: &ApiRequest, path: &[&str]) -> Response {
        let method = req.method.clone();
        let query = req.query.clone();
        let body = Value::Object(req.body.clone().unwrap_or_default());
        let path: Vec<String> = path.iter().map(|p| p.to_string()).collect();
        match with_ocr(s, move |o, c| {
            let segs: Vec<&str> = path.iter().map(String::as_str).collect();
            o.other(c, &method, &segs, &query, &body)
        })
        .await
        {
            Ok(Some(r)) => respond(r),
            Ok(None) => not_found(),
            Err(r) => r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_offers_only_shipped_components() {
        let c = catalog(default_devices());
        let engines: Vec<&str> = c["engines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap())
            .collect();
        assert_eq!(engines, ["hayai-nova", "paddle-manga", "ppocr-manga"]);
        let detectors: Vec<&str> = c["detectors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap())
            .collect();
        assert_eq!(detectors, ["ppocr-manga"]);
        assert_eq!(c["engines"][2]["precision_modes"], json!([]));
        assert_eq!(c["engines"][2]["devices"], json!(["cpu"]));
        assert_eq!(c["engines"][0]["devices"], json!("any"));
        assert_eq!(c["name_pattern"], json!(NAME_PATTERN));
    }

    #[test]
    fn entry_of_default_row() {
        let row = generations::default_generation("g-1");
        let e = generation_entry(&row, &default_devices());
        assert_eq!(e["sidecar"], json!("<Volume>.mokuro"));
        assert_eq!(e["road"], json!("reconciled"));
        assert_eq!(e["detector_locked"], json!(false));
        assert_eq!(e["retired"], Value::Null);
        let stages = e["stages"].as_array().unwrap();
        let keys: Vec<&str> = stages.iter().map(|s| s["key"].as_str().unwrap()).collect();
        assert_eq!(keys, ["detect", "engine", "post"]);
        assert_eq!(
            stages[0]["device_locked_reason"],
            json!("the PP-OCRv6 detector runs on the CPU")
        );
        assert_eq!(stages[1]["devices_allowed"], json!(["auto", "cpu"]));
        assert_eq!(stages[1]["device_needs_gpu"], Value::Null);
        assert_eq!(stages[2]["devices_allowed"], json!([]));
        let first: Vec<&String> = e.keys().take(4).collect();
        assert_eq!(first, ["id", "name", "primary", "enabled"]);
    }

    /// paddle-manga runs on a GPU only: its engine stage never offers the CPU, and
    /// `auto` shows the GPU it means.
    #[test]
    fn a_gpu_only_engine_stage_offers_no_cpu() {
        let c = catalog(default_devices());
        assert_eq!(c["engines"][1]["id"], json!("paddle-manga"));
        assert_eq!(c["engines"][1]["devices"], json!("gpu"));
        assert!(
            c["engines"][1]["gpu_only_reason"]
                .as_str()
                .unwrap()
                .starts_with("paddle-manga needs a GPU")
        );
        assert_eq!(c["engines"][0]["gpu_only_reason"], Value::Null);
        let parsed = generations::parse_generation_list(&json!([
            {"name": "nova", "engine": "hayai-nova", "primary": true},
            {"name": "vl", "engine": "paddle-manga"},
        ]))
        .unwrap();
        let vl = &parsed.rows[1];
        // No host facts: only `auto`, meaning "a GPU".
        let stages = stage_rows(vl, &default_devices());
        assert_eq!(stages[1]["devices_allowed"], json!(["auto"]));
        assert_eq!(stages[1]["device"], json!("gpu"));
        assert_eq!(stages[1]["device_options"][0]["label"], json!("Auto → GPU"));
        assert!(
            stages[1]["device_needs_gpu"]
                .as_str()
                .unwrap()
                .contains("use hayai-nova on the CPU")
        );
        // A machine with cards: those, and `auto` on the first.
        let devices = json!([
            {"id": "auto", "label": "Auto"}, {"id": "cpu", "label": "CPU"},
            {"id": "gpu:0", "label": "RTX 4090"}, {"id": "gpu:1", "label": "RX 6600"},
        ]);
        let stages = stage_rows(vl, &devices);
        assert_eq!(
            stages[1]["devices_allowed"],
            json!(["auto", "gpu:0", "gpu:1"])
        );
        assert_eq!(stages[1]["device"], json!("gpu:0"));
        assert_eq!(
            stages[1]["device_options"][0]["label"],
            json!("Auto → GPU 0")
        );
        // The detect stage stays on the CPU.
        assert_eq!(stages[0]["devices_allowed"], json!(["auto", "cpu"]));
    }

    #[test]
    fn retired_row_is_shown_with_its_reason() {
        let parsed = generations::parse_generation_list(&json!([
            {"name": "legacy", "engine": "mokuro", "primary": true},
        ]))
        .unwrap();
        let retired = parsed.rows.iter().find(|g| g.retired.is_some()).unwrap();
        let e = generation_entry(retired, &default_devices());
        assert!(e["retired"].as_str().unwrap().contains("removed"));
        assert_eq!(e["stages"], json!([]));
        assert_eq!(e["road"], Value::Null);
    }
}
