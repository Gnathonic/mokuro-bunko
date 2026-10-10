//! `impl admin::OcrAdmin for OcrControl`: the OCR half of the admin panel, answered
//! from the scheduler, the profiles and the stored benchmarks.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use bunko_core::Config;
use bunko_core::generations::{Generation, parse_bench_spec};
use http::Method;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use super::OcrControl;

/// One row's `precision_on`, and each connected machine's benchmark of it (judged for
/// the row's mode), by profile key.
type JudgedRow = (Map<String, Value>, HashMap<String, Value>);
use super::profiles::{LOCAL_PROFILE, Profiles, profile_key};
use super::sched::{BenchRequest as SchedBench, Scheduler};
use crate::admin::ocr::{
    BenchRequest, OcrAdmin, OcrError, catalog, default_devices, generation_entry, stage_rows,
};

const WAIT: Duration = Duration::from_secs(10);
pub const GEN_STATS_TTL_SECONDS: u64 = 60;

/// The last stats worked out, by what they were worked out for ([`stats_key`]): a
/// changed list of rows (or another library) is never answered with stale counts.
static STATS: Mutex<Option<HashMap<String, (Instant, Value)>>> = Mutex::new(None);

/// What the stats depend on besides the disk: the library and each row's id, name,
/// primary, enabled and files (0.5.3 keyed its cache on the rows the same way).
fn stats_key(ocr: &OcrControl, rows: &[Generation]) -> String {
    let mut key = ocr.core().layout.library().display().to_string();
    for r in rows {
        key.push_str(&format!(
            "\0{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
            r.id,
            r.name,
            r.primary,
            r.enabled,
            r.files_suffix()
        ));
    }
    key
}

fn cached_stats(key: &str) -> Option<(Instant, Value)> {
    STATS.lock().as_ref().and_then(|m| m.get(key).cloned())
}

/// Every device any connected or remembered machine could place a model on. Read live
/// from the machines' catalogs: this server's own is replaced each time its OCR (re)starts
/// (`LocalUp`), so a backend installed in the background shows here at once.
///
/// `auto` carries what it `resolves` to for the row's own (this server's) table: the
/// first card of this server's catalog while its OCR runs, else the first card any
/// machine reports.
fn merged_devices(s: &Scheduler) -> Value {
    let mut out: Vec<Value> = Vec::new();
    let mut gpus: BTreeMap<String, String> = BTreeMap::new();
    for m in s.machines.values() {
        for d in &m.catalog.devices {
            if d.id.starts_with("gpu:") {
                gpus.entry(d.id.clone()).or_insert_with(|| d.label.clone());
            }
        }
    }
    let resolves = match s.machines.values().find(|m| m.local) {
        Some(local) => local
            .catalog
            .devices
            .iter()
            .find(|d| d.id.starts_with("gpu:") && !d.formats.is_empty())
            .map_or_else(|| "cpu".to_string(), |d| d.id.clone()),
        None => gpus.keys().next().cloned().unwrap_or_else(|| "cpu".into()),
    };
    if gpus.is_empty() {
        return default_devices();
    }
    let auto_label = match resolves.strip_prefix("gpu:") {
        Some(i) => format!("Auto \u{2014} GPU {i}"),
        None => "Auto \u{2014} CPU".into(),
    };
    out.push(json!({"id": "auto", "label": auto_label, "resolves": resolves}));
    out.push(json!({"id": "cpu", "label": "CPU"}));
    for (id, label) in gpus {
        out.push(json!({"id": id, "label": label}));
    }
    Value::Array(out)
}

fn ask<T: Send + 'static>(
    ocr: &OcrControl,
    f: impl FnOnce(&mut Scheduler) -> T + Send + 'static,
) -> Option<T> {
    ocr.ask_blocking(WAIT, f)
}

/// The volumes the stats count, as 0.5.3 did (`_generation_volume_counts` over the
/// library index): archives inside a series folder that is not hidden. A loose archive
/// at the library root, or one under a `.folder`, is in no series the catalog shows (OCR
/// still reads it), so it is not one of the library's volumes here.
pub fn stats_volumes(library: &std::path::Path) -> Vec<std::path::PathBuf> {
    super::owed::list_archives(library)
        .into_iter()
        .filter(|cbz| {
            super::types::rel_of(library, cbz).is_some_and(|rel| {
                let parts: Vec<&str> = rel.split('/').collect();
                parts.len() > 1 && !parts[..parts.len() - 1].iter().any(|p| p.starts_with('.'))
            })
        })
        .collect()
}

/// `generations/stats`: done / total / skipped per row and who wrote them.
fn compute_stats(ocr: &OcrControl, rows: &[Generation]) -> Value {
    let library = ocr.core().layout.library();
    let archives = stats_volumes(&library);
    let mut out = Map::new();
    let skipped: HashMap<String, Vec<String>> = ask(ocr, |s| {
        s.owed
            .volumes
            .iter()
            .map(|(rel, v)| {
                (
                    rel.clone(),
                    v.skipped.iter().map(|g| g.to_string()).collect(),
                )
            })
            .collect()
    })
    .unwrap_or_default();
    let producers = ocr
        .db()
        .and_then(|db| db.ocr_sidecar_producers().ok())
        .unwrap_or_default();
    for row in rows {
        let mut done = 0u64;
        let mut present: std::collections::HashSet<String> = Default::default();
        let mut skip = 0u64;
        for cbz in &archives {
            let rel = super::types::rel_of(&library, cbz).unwrap_or_default();
            // The row's own files: a row retired from primary left bare ones.
            let (plain, gz) = super::owed::sidecar_paths(cbz, &row.files_suffix());
            if plain.exists() || gz.exists() {
                done += 1;
                present.insert(rel.clone());
            } else if !row.primary && skipped.get(&rel).is_some_and(|g| g.contains(&row.id)) {
                skip += 1;
            }
        }
        let mut latest: HashMap<String, String> = HashMap::new();
        for (gid, volume, machine) in &producers {
            if gid == &row.id {
                latest.insert(volume.clone(), machine.clone());
            }
        }
        let mut by_machine: BTreeMap<String, u64> = BTreeMap::new();
        for (volume, machine) in latest {
            if present.contains(&volume) {
                *by_machine.entry(machine).or_insert(0) += 1;
            }
        }
        out.insert(row.id.clone(), json!({"volumes_done": done, "volumes_total": archives.len(), "volumes_skipped": skip, "volumes_by_machine": by_machine}));
    }
    json!({"stats_pending": false, "computed_at": crate::ops::dyndns::utc_stamp(), "generations": out})
}

fn stats(ocr: &OcrControl, config: &Config) -> Value {
    let key = stats_key(ocr, &config.ocr.generations);
    if let Some((at, v)) = cached_stats(&key)
        && at.elapsed().as_secs() < GEN_STATS_TTL_SECONDS
    {
        return v;
    }
    let v = compute_stats(ocr, &config.ocr.generations);
    let mut cache = STATS.lock();
    let map = cache.get_or_insert_with(HashMap::new);
    map.retain(|_, (at, _)| at.elapsed().as_secs() < GEN_STATS_TTL_SECONDS);
    map.insert(key, (Instant::now(), v.clone()));
    v
}

impl OcrAdmin for OcrControl {
    fn runtime_status(&self, config: &Config) -> Value {
        let local = config.processes_locally(self.has_local());
        let engines: Vec<String> =
            bunko_core::generations::enabled_generations(&config.ocr.generations)
                .map(|g| g.engine.clone())
                .collect();
        json!({
            "available": self.has_local(),
            "launch_only": true,
            "configured_backend": config.ocr.backend,
            "local_processing": local,
            "generations": config.ocr.generations.iter().map(Generation::to_value).collect::<Vec<_>>(),
            "detectors": [bunko_core::engines::DEFAULT_DETECTOR],
            "supported_backends": [],
            "cli_hint": if self.has_local() { "" } else { crate::admin::ocr::NO_OCR_HINT },
            "driver_hint": "",
            "active_engines": engines,
            "install": self.install_view(),
        })
    }

    fn processors(&self, config: &Config) -> Value {
        let local = config.processes_locally(self.has_local());
        // 0.7: this server's background OCR backend install (null: none ran).
        let install = serde_json::to_value(self.install_view()).unwrap_or(Value::Null);
        let install2 = install.clone();
        ask(self, move |s| {
            let mut processors = s.processors();
            for p in &mut processors {
                let name = p.get("name").and_then(Value::as_str).map(|n| if p.get("local") == Some(&json!(true)) { super::types::LOCAL.to_string() } else { n.to_string() });
                if let (Some(name), Value::Object(m)) = (name, p) {
                    let host = m.get("host").cloned().unwrap_or(Value::Null);
                    let cpu = host.get("cpu").cloned().unwrap_or(Value::Null);
                    let gpu = host.get("gpu").cloned().unwrap_or(Value::Null);
                    if m.get("local") == Some(&json!(true)) {
                        // This server's `_local_host()`: its CPU with its core count.
                        let cpu = cpu
                            .as_str()
                            .map(|c| json!(super::sched::cpu_label(c, super::sched::physical_cores())))
                            .unwrap_or(cpu);
                        m.insert("host".into(), json!({"cpu": cpu, "gpu": gpu}));
                    }
                    m.insert("cannot_start".into(), Value::Array(s.start_backoffs(&name)));
                }
            }
            let local_host = s
                .machines
                .values()
                .find(|m| m.local)
                .map(|m| json!({"cpu": super::sched::cpu_label(&m.host.cpu, super::sched::physical_cores()), "gpu": m.host.gpu}));
            json!({
                "processors": processors,
                "speed": s.admin_speed(local_host.as_ref()),
                "failed_logins": s.failed_logins(),
                "last_disconnect": s.last_disconnect(),
                "local_processing": local,
                "processing_hold": s.processing_hold(),
                "local_install": install,
            })
        })
        .unwrap_or_else(|| json!({"processors": [], "speed": [], "failed_logins": [], "last_disconnect": null, "local_processing": local, "processing_hold": null, "local_install": install2}))
    }

    fn generations_payload(&self, config: &Config) -> Value {
        let rows = config.ocr.generations.clone();
        let asked_rows = rows.clone();
        let (devices, holds, processors, judged) = ask(self, move |s| {
            let holds: HashMap<String, String> = s.precision_holds().into_iter().collect();
            let processors: Vec<Value> = s
                .processors()
                .into_iter()
                .filter(|p| p.get("local") != Some(&json!(true)))
                .collect();
            // Per row: where each machine's precision stands, and each connected
            // machine's benchmark judged for the row's mode on its own device.
            let mut judged: HashMap<String, JudgedRow> = HashMap::new();
            for row in &asked_rows {
                let mut benches = HashMap::new();
                for m in s.machines.values() {
                    let key = profile_key(if m.local {
                        super::types::LOCAL
                    } else {
                        &m.name
                    })
                    .to_string();
                    let bench = s
                        .machine_profile(&m.name, row)
                        .and_then(|p| p.bench)
                        .map_or(Value::Null, Value::Object);
                    benches.insert(key, bench);
                }
                judged.insert(row.id.clone(), (s.precision_on(row), benches));
            }
            (merged_devices(s), holds, processors, judged)
        })
        .unwrap_or_else(|| {
            (
                default_devices(),
                HashMap::new(),
                Vec::new(),
                HashMap::new(),
            )
        });
        let storage = self.storage().to_path_buf();
        let profiles = Profiles::new(&storage);
        let names = profiles.names();
        let saved = bunko_sched::bench_file::BenchFile::new(&storage).load();
        let history = bunko_sched::congestion::CongestionHistory::new(&storage);
        let stats = cached_stats(&stats_key(self, &rows)).map(|(_, v)| v);
        let mut entries = Vec::new();
        for row in &rows {
            let mut e = generation_entry(row, &devices);
            if let Some(Value::Object(st)) = stats
                .as_ref()
                .and_then(|s| s.get("generations"))
                .and_then(|g| g.get(&row.id))
            {
                for (k, v) in st {
                    e.insert(k.clone(), v.clone());
                }
            }
            e.insert(
                "congestion".into(),
                history.summary(&row.id).map_or(Value::Null, Value::Object),
            );
            if let Some(Value::Object(b)) = saved.get(&row.id) {
                let mut b = b.clone();
                b.remove("trials");
                b.insert("progress".into(), Value::Null);
                e.insert("bench".into(), Value::Object(b));
            }
            e.insert(
                "precision_hold".into(),
                holds.get(&row.id).map_or(Value::Null, |r| json!(r)),
            );
            let (precision_on, benches) = judged.get(&row.id).cloned().unwrap_or_default();
            if row.precision_applies() {
                e.insert("precision_on".into(), Value::Object(precision_on));
            }
            let recipe = row.output_affecting();
            if let Some(local) = profiles.row(LOCAL_PROFILE, &row.id, Some(&recipe)) {
                e.insert(
                    "local_pools".into(),
                    if local.pools.is_empty() {
                        Value::Null
                    } else {
                        Value::Object(local.pools.clone())
                    },
                );
                e.insert(
                    "local_bench".into(),
                    benches
                        .get(LOCAL_PROFILE)
                        .cloned()
                        .unwrap_or_else(|| local.bench.clone().map_or(Value::Null, Value::Object)),
                );
                e.insert(
                    "local_runs".into(),
                    if local.runs.is_empty() {
                        Value::Null
                    } else {
                        Value::Object(local.runs.clone())
                    },
                );
            }
            let (mut pools, mut bench, mut runs, mut cong) =
                (Map::new(), Map::new(), Map::new(), Map::new());
            for name in &names {
                let Some(p) = profiles.row(name, &row.id, Some(&recipe)) else {
                    continue;
                };
                if !p.pools.is_empty() {
                    pools.insert(name.clone(), Value::Object(p.pools.clone()));
                }
                // A connected machine's benchmark as judged for the row's mode there.
                match benches.get(name.as_str()) {
                    Some(Value::Object(b)) => {
                        bench.insert(name.clone(), Value::Object(b.clone()));
                    }
                    Some(_) => {}
                    None => {
                        if let Some(b) = p.bench.clone() {
                            bench.insert(name.clone(), Value::Object(b));
                        }
                    }
                }
                if !p.runs.is_empty() {
                    if let Some(c) = p
                        .runs
                        .get("congestion")
                        .and_then(Value::as_array)
                        .and_then(|r| bunko_sched::congestion::average_runs(r))
                    {
                        cong.insert(name.clone(), Value::Object(c));
                    }
                    runs.insert(name.clone(), Value::Object(p.runs.clone()));
                }
            }
            e.insert("processor_pools".into(), Value::Object(pools));
            e.insert("processor_bench".into(), Value::Object(bench));
            e.insert("processor_runs".into(), Value::Object(runs));
            e.insert("processor_congestion".into(), Value::Object(cong));
            entries.push(Value::Object(e));
        }
        json!({
            "stats_pending": stats.is_none(),
            "generations": entries,
            "catalog": catalog(devices),
            "processors": processors,
            "local_processing": config.processes_locally(self.has_local()),
            "autobench": config.ocr.autobench,
        })
    }

    fn generation_stats(&self, config: &Config) -> Value {
        stats(self, config)
    }

    fn apply(&self, config: &Config) -> Option<Value> {
        STATS.lock().take();
        Some(self.apply_config(config))
    }

    fn prune(&self, known_ids: &[String]) {
        let storage = self.storage().to_path_buf();
        let _ = bunko_sched::bench_file::BenchFile::new(&storage).prune(known_ids);
        Profiles::new(&storage).prune(known_ids);
    }

    fn derive(
        &self,
        _config: &Config,
        spec: &Value,
        processor: Option<&str>,
    ) -> Result<Value, OcrError> {
        let devices = match processor.filter(|p| *p != "local") {
            Some(name) => {
                let n = name.to_string();
                let found = ask(self, move |s| {
                    s.machine_by_name(&n)
                        .map(|m| serde_json::to_value(&m.catalog.devices).unwrap_or(Value::Null))
                        .or_else(|| {
                            s.profiles
                                .load(&n)
                                .get("catalog")
                                .and_then(|c| c.get("devices"))
                                .cloned()
                        })
                })
                .flatten();
                match found {
                    Some(Value::Array(d)) if !d.is_empty() => {
                        let mut list = vec![
                            json!({"id": "auto", "label": "Auto"}),
                            json!({"id": "cpu", "label": "CPU"}),
                        ];
                        list.extend(d.into_iter().filter(|x| {
                            x.get("id")
                                .and_then(Value::as_str)
                                .is_some_and(|i| i.starts_with("gpu:"))
                        }));
                        Value::Array(list)
                    }
                    Some(_) => default_devices(),
                    None => return Err(OcrError::unknown_processor(name)),
                }
            }
            None => ask(self, |s| merged_devices(s)).unwrap_or_else(default_devices),
        };
        let row = parse_bench_spec(spec)?;
        Ok(json!({"road": row.road().map(|r| r.as_str()), "stages": stage_rows(&row, &devices)}))
    }

    fn set_pools(
        &self,
        _config: &Config,
        row: &Generation,
        processor: &str,
        pools: &Value,
    ) -> Result<Value, OcrError> {
        let storage = self.storage().to_path_buf();
        let profiles = Profiles::new(&storage);
        let name = processor.to_string();
        let connected = ask(self, move |s| s.machine_by_name(&name).is_some()).unwrap_or(false);
        if !connected && !profiles.names().iter().any(|n| n == processor) {
            return Err(OcrError::unknown_processor(processor));
        }
        let Value::Object(p) = pools else {
            return Err(OcrError {
                status: 400,
                body: json!({"error": "pools must be an object", "field": "pools"}),
            });
        };
        // `"auto"` widths/capacities are validated as 1 and stored as "auto".
        let mut check = p.clone();
        for table in ["stage_workers", "queue_capacity"] {
            if let Some(Value::Object(t)) = check.get_mut(table) {
                for v in t.values_mut() {
                    if v.as_str() == Some(super::profiles::POOL_AUTO) {
                        *v = json!(1);
                    }
                }
            }
        }
        let mut spec = match row.to_value() {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        spec.insert("pools".into(), Value::Object(check));
        parse_bench_spec(&Value::Object(spec))?;
        let mut stored = Map::new();
        for t in super::profiles::POOL_TABLES {
            stored.insert(
                t.into(),
                p.get(t)
                    .cloned()
                    .filter(Value::is_object)
                    .unwrap_or(json!({})),
            );
        }
        profiles.set_pools(
            profile_key(processor),
            &row.id,
            &stored,
            Some(&row.output_affecting()),
            false,
            false,
        );
        Ok(json!({"success": true, "pools": stored}))
    }

    fn bench(
        &self,
        _config: &Config,
        key: &str,
        request: BenchRequest,
    ) -> Result<(u16, Value), OcrError> {
        let key = key.to_string();
        let to_err = |(status, body): (u16, Value)| OcrError { status, body };
        match request {
            BenchRequest::Enqueue {
                spec,
                pages,
                processor,
            } => {
                let req = SchedBench {
                    key,
                    spec,
                    pages,
                    processor,
                    autobench: false,
                    precision_only: false,
                };
                match ask(self, move |s| s.bench_enqueue(req)) {
                    Some(Ok(v)) => Ok((202, v)),
                    Some(Err(e)) => Err(to_err(e)),
                    None => Err(OcrError::new(400, "OCR is disabled in this server process")),
                }
            }
            BenchRequest::Get { processor } => {
                let m = processor.unwrap_or_else(|| "local".into());
                ask(self, move |s| s.bench_get(&key, &m))
                    .map(|v| (200, v))
                    .ok_or_else(|| OcrError::new(400, "OCR is disabled in this server process"))
            }
            BenchRequest::Cancel { processor } => {
                let m = processor.unwrap_or_else(|| "local".into());
                match ask(self, move |s| s.bench_cancel(&key, &m)) {
                    Some(Ok(v)) => Ok((200, v)),
                    Some(Err(e)) => Err(to_err(e)),
                    None => Err(OcrError::new(400, "OCR is disabled in this server process")),
                }
            }
        }
    }

    fn refresh_devices(&self) -> Value {
        let devices = ask(self, |s| merged_devices(s)).unwrap_or_else(default_devices);
        json!({"success": true, "devices": devices})
    }

    fn queue_changed(&self) {
        let _ = ask(self, |s| s.bump_page_pub());
    }

    /// `/api/ocr/upgrade` (GET census), `/api/ocr/upgrade/<volume>` (POST, `force`),
    /// `/api/ocr/upgrade/<volume>/revert` (POST).
    fn other(
        &self,
        _config: &Config,
        method: &Method,
        path: &[&str],
        _query: &str,
        body: &Value,
    ) -> Option<Result<(u16, Value), OcrError>> {
        // 0.7: `GET /api/ocr/install` (where this server's background OCR backend
        // install stands) and `POST /api/ocr/install` (start it, or retry a failed one).
        if path == ["ocr", "install"] {
            return Some(match *method {
                Method::GET => Ok((200, json!({"install": self.install_view()}))),
                Method::POST => match self.start_install() {
                    Ok(running) => Ok((
                        202,
                        json!({"success": true, "installing": running, "install": self.install_view()}),
                    )),
                    Err(e) => Err(OcrError::new(409, e)),
                },
                _ => Err(OcrError::not_found()),
            });
        }
        if path.len() < 2 || path[0] != "ocr" || path[1] != "upgrade" {
            return None;
        }
        let up = self.upgrade().clone();
        if path.len() == 2 {
            if *method != Method::GET {
                return Some(Err(OcrError::not_found()));
            }
            return Some(Ok((200, up.census())));
        }
        if *method != Method::POST {
            return Some(Err(OcrError::not_found()));
        }
        let revert = path.last() == Some(&"revert");
        let parts = if revert {
            &path[2..path.len() - 1]
        } else {
            &path[2..]
        };
        let rel: String = parts
            .iter()
            .map(|p| {
                percent_encoding::percent_decode_str(p)
                    .decode_utf8_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>()
            .join("/");
        let rel = if rel.to_lowercase().ends_with(".cbz") {
            rel
        } else {
            format!("{rel}.cbz")
        };
        // Every part one plain component: on Windows `C:x` or `\\server\x` would make
        // the join below REPLACE the library path, and `a\..\b` would climb out.
        if rel.split('/').any(|p| p == ".." || p.is_empty())
            || !std::path::Path::new(&rel)
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
        {
            return Some(Err(OcrError::new(400, "not a volume of the library")));
        }
        let cbz = self.core().layout.library().join(&rel);
        if !cbz.is_file() {
            return Some(Err(OcrError::new(404, "no such volume")));
        }
        if revert {
            let actor = body
                .get("actor")
                .and_then(Value::as_str)
                .map(str::to_string);
            return Some(match up.revert(&cbz, actor.as_deref()) {
                Ok(bare) => Ok((
                    200,
                    json!({"success": true, "sidecar": super::types::rel_of(&self.core().layout.library(), &bare)}),
                )),
                Err(e) => Err(OcrError::new(409, e)),
            });
        }
        if bunko_sched::py::truthy(body.get("force")) {
            up.force(&rel);
        }
        let verdict = up.judge(&cbz, &rel).1;
        let answer = match verdict {
            super::upgrade::Verdict::Ready(layer) => match up.direct_replace(&cbz, &layer) {
                Ok(_) => json!({"success": true, "mode": "direct"}),
                Err(e) => return Some(Err(OcrError::new(409, e))),
            },
            super::upgrade::Verdict::NeedsOcr => {
                self.archive_arrived(&cbz);
                json!({"success": true, "mode": "generated"})
            }
            super::upgrade::Verdict::SkippedMissingPages => {
                return Some(Err(OcrError::new(
                    409,
                    "the archive is missing pages its sidecar names; replace it with a whole volume first",
                )));
            }
            super::upgrade::Verdict::SkippedEdited => {
                return Some(Err(OcrError::new(
                    409,
                    "its sidecar was edited by a person; send force to upgrade it anyway",
                )));
            }
            super::upgrade::Verdict::Current => json!({"success": true, "mode": "current"}),
        };
        Some(Ok((200, answer)))
    }
}

impl Scheduler {
    pub fn bump_page_pub(&mut self) {
        self.bump_page();
    }
}
