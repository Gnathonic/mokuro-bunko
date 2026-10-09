//! Precision modes as the library and the benchmark see them (0.5.2
//! `engine_runner.PRECISION_POLICY` / `resolve_mode` / `pick_precision`, `precision.py`
//! `bench_pick` / `resolution_entry` and `remote/profiles.py` `stale_bench_reason`;
//! spec ocr-generations-bench §3, §6.6).
//!
//! The policy table is the same one the engines resolve with (`bunko-engines`
//! `precision::candidates`); it is repeated here because this crate is pure and the
//! library (lite build) has no engines. A "supported" set is what a machine's model
//! device computes in, as it reported it; `None` means nobody reported it.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::py::{float_value, fmt_fixed};

pub const FP32: &str = "fp32";
pub const FP16: &str = "fp16";
pub const BF16: &str = "bf16";
/// The formats a recognizer computes in.
pub const PRECISIONS: [&str; 3] = [BF16, FP16, FP32];

pub const MODE_ACCURACY: &str = "auto-accuracy";
pub const MODE_BALANCED: &str = "auto-balanced";
pub const MODE_SPEED: &str = "auto-speed";
pub const DEFAULT_MODE: &str = MODE_ACCURACY;
/// Every mode, in the admin panel's order.
pub const MODES: [&str; 6] = [MODE_ACCURACY, MODE_BALANCED, MODE_SPEED, FP32, BF16, FP16];
/// The modes a per-machine benchmark decides.
pub const BENCHED_MODES: [&str; 2] = [MODE_BALANCED, MODE_SPEED];
/// Two candidates this close in pages/s are a tie, won by the earlier (more accurate).
pub const PRECISION_TIE: f64 = 0.05;

pub fn is_forced(mode: &str) -> bool {
    PRECISIONS.contains(&mode)
}

pub fn is_benched(mode: &str) -> bool {
    BENCHED_MODES.contains(&mode)
}

/// Engines whose row's mode decides a precision (`PRECISION_ENGINES`).
pub fn is_precision_engine(engine: &str) -> bool {
    matches!(engine, "hayai-nova" | "paddle-manga")
}

/// `normalize_precision_mode`: blank / `auto` → the default; None for an unknown mode.
pub fn normalize_mode(value: Option<&str>) -> Option<&'static str> {
    let text = value
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if text.is_empty() || text == "auto" {
        return Some(DEFAULT_MODE);
    }
    MODES.iter().find(|m| **m == text).copied()
}

/// Engines that run on a GPU only (NVIDIA CUDA or AMD ROCm): never placed on the CPU,
/// never offered by a machine without a GPU. paddle-manga: there are no CPU packages
/// for it (a CPU read ~27 s a page on a 16-core desktop, from a 4.65 GB package per
/// platform); a CPU-only machine runs hayai-nova. THE rule: the engines place and
/// offer with it, the library judges which machine may run a row with it.
pub fn gpu_only(engine: &str) -> bool {
    engine == "paddle-manga"
}

/// Why a [`gpu_only`] engine cannot run where no GPU may run it (no GPU, `ocr.backend:
/// cpu`): the error of a session, `models download` and `doctor`.
pub fn needs_gpu(engine: &str) -> String {
    format!("{engine} needs a GPU (NVIDIA CUDA or AMD ROCm); use hayai-nova on the CPU")
}

/// `engine_modes(engine)`: every mode for a precision engine, none otherwise.
pub fn engine_modes(engine: &str) -> &'static [&'static str] {
    if is_precision_engine(engine) {
        &MODES
    } else {
        &[]
    }
}

/// Whether a device computes bf16 natively and fast: NVIDIA sm_80+ (Ampere on), AMD
/// RDNA3/RDNA4 (`gfx11xx`, `gfx12xx`). Elsewhere bf16 is emulated: on RDNA2
/// (`gfx103x`) hayai-nova ran at about half its fp32 speed with the GPU 99% busy, and
/// read less accurately (6900 XT, default pools: fp32 2.72, bf16 2.19, fp16 2.72 p/s).
/// The CPU never counts (0.5.2 always ran the CPU in fp32; its probe refused bf16).
///
/// `kind` is the device's provider (`cuda`, `rocm`, `cpu`), `arch` its architecture
/// (`sm_89`, `gfx1201`). THE rule: the engines resolve a session's format with it, and
/// the library judges what each machine runs (precision, benchmarks, staleness) with it.
pub fn bf16_native(kind: &str, arch: &str) -> bool {
    match kind {
        "cuda" => arch
            .strip_prefix("sm_")
            .and_then(|n| n.parse::<u32>().ok())
            .is_some_and(|sm| sm >= 80),
        "rocm" => arch.starts_with("gfx11") || arch.starts_with("gfx12"),
        _ => false,
    }
}

/// The formats a row asking `requested` may resolve to on a device running `formats`:
/// the auto modes take bf16 only where the device runs it natively ([`bf16_native`]); a
/// row that forces `bf16` still gets it wherever the device has it.
pub fn auto_formats<T: AsRef<str>>(
    requested: &str,
    mut formats: Vec<T>,
    bf16_is_native: bool,
) -> Vec<T> {
    if !bf16_is_native && normalize_mode(Some(requested)) != Some(BF16) {
        formats.retain(|f| f.as_ref() != BF16);
    }
    formats
}

/// What a REPORTED device (a catalog entry: its runnable formats, provider and
/// architecture) runs a row asking `mode` at: fp32 always, then [`auto_formats`].
pub fn device_formats(
    mode: &str,
    formats: &[String],
    kind: Option<&str>,
    arch: Option<&str>,
) -> BTreeSet<String> {
    let native = bf16_native(kind.unwrap_or("cpu"), arch.unwrap_or(""));
    let mut all: Vec<String> = formats.to_vec();
    if !all.iter().any(|f| f == FP32) {
        all.push(FP32.to_string());
    }
    auto_formats(mode, all, native).into_iter().collect()
}

/// `mode_candidates(engine, mode)`: the formats `mode` may run `engine` at, preferred
/// first.
pub fn candidates(engine: &str, mode: &str) -> &'static [&'static str] {
    if !is_precision_engine(engine) {
        return &[];
    }
    match (engine, mode) {
        (_, FP32) => &[FP32],
        (_, BF16) => &[BF16],
        (_, FP16) => &[FP16],
        ("hayai-nova", MODE_ACCURACY | MODE_BALANCED) => &[BF16, FP32],
        ("hayai-nova", MODE_SPEED) => &[BF16, FP16, FP32],
        ("paddle-manga", MODE_ACCURACY) => &[FP32],
        ("paddle-manga", MODE_BALANCED) => &[BF16, FP32],
        ("paddle-manga", MODE_SPEED) => &[BF16, FP16, FP32],
        _ => &[],
    }
}

/// What one mode comes to on one device (`ModeResolution`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeResolution {
    /// None with `eligible`: the engine fixes its own, or the device never reported.
    pub precision: Option<String>,
    pub eligible: bool,
    pub why: String,
    /// The candidates this device supports, in order.
    pub usable: Vec<String>,
}

impl ModeResolution {
    fn new(precision: Option<&str>, eligible: bool, why: &str, usable: Vec<String>) -> Self {
        ModeResolution {
            precision: precision.map(str::to_string),
            eligible,
            why: why.to_string(),
            usable,
        }
    }
}

/// `resolve_mode(engine, mode, supported, pick, pick_why)`.
pub fn resolve_mode(
    engine: &str,
    mode: &str,
    supported: Option<&BTreeSet<String>>,
    pick: Option<&str>,
    pick_why: &str,
) -> ModeResolution {
    let mode = normalize_mode(Some(mode)).unwrap_or(DEFAULT_MODE);
    if !is_precision_engine(engine) {
        return ModeResolution::new(None, true, "fixed by the engine", Vec::new());
    }
    if is_forced(mode) {
        // A device nobody reported counts as fp32-only.
        let runs = match supported {
            Some(s) => s.contains(mode),
            None => mode == FP32,
        };
        if runs {
            return ModeResolution::new(Some(mode), true, mode, vec![mode.to_string()]);
        }
        if supported.is_none() {
            return ModeResolution::new(None, false, "card not reported", Vec::new());
        }
        return ModeResolution::new(None, false, &format!("{mode} not supported"), Vec::new());
    }
    let Some(supported) = supported else {
        return ModeResolution::new(
            None,
            true,
            "decided at start (card not reported)",
            Vec::new(),
        );
    };
    let usable: Vec<String> = candidates(engine, mode)
        .iter()
        .filter(|c| **c == FP32 || supported.contains(**c))
        .map(|c| c.to_string())
        .collect();
    let first = usable.first().cloned().unwrap_or_else(|| FP32.to_string());
    if !is_benched(mode) || usable.len() == 1 {
        return ModeResolution::new(Some(&first), true, mode, usable);
    }
    if let Some(p) = pick.filter(|p| usable.iter().any(|u| u == p)) {
        let why = if pick_why.is_empty() {
            "benchmark"
        } else {
            pick_why
        };
        return ModeResolution::new(Some(p), true, why, usable);
    }
    ModeResolution::new(
        Some(&first),
        true,
        "not benchmarked yet: first supported candidate",
        usable,
    )
}

/// `pick_precision(trials, usable)`: the fastest usable candidate, a 5% tie going to
/// the earlier one; `(format, why)`, or None when no usable candidate was measured.
pub fn pick_precision(trials: &[(String, f64)], usable: &[String]) -> Option<(String, String)> {
    // A dict comprehension: a later trial of the same format wins.
    let mut rates: Vec<(String, f64)> = Vec::new();
    for (fmt, rate) in trials {
        if !usable.contains(fmt) || *rate <= 0.0 {
            continue;
        }
        match rates.iter_mut().find(|(f, _)| f == fmt) {
            Some(slot) => slot.1 = *rate,
            None => rates.push((fmt.clone(), *rate)),
        }
    }
    let rate_of = |f: &str| rates.iter().find(|(g, _)| g == f).map(|(_, r)| *r);
    let fastest = rates
        .iter()
        .map(|(_, r)| *r)
        .fold(f64::NEG_INFINITY, f64::max);
    if rates.is_empty() {
        return None;
    }
    let chosen = usable
        .iter()
        .find(|f| rate_of(f).is_some_and(|r| r >= fastest * (1.0 - PRECISION_TIE)))?
        .clone();
    let mine = rate_of(&chosen).unwrap_or(0.0);
    let others: Vec<&String> = usable
        .iter()
        .filter(|f| **f != chosen && rate_of(f).is_some())
        .collect();
    let why = if others.is_empty() {
        format!("benchmark: {chosen} {} p/s", fmt_fixed(mine, 2))
    } else {
        let detail: Vec<String> = others
            .iter()
            .map(|f| format!("{f} {} p/s", fmt_fixed(rate_of(f).unwrap_or(0.0), 2)))
            .collect();
        let verb = if others
            .iter()
            .all(|f| mine > rate_of(f).unwrap_or(f64::INFINITY))
        {
            "beat"
        } else {
            "tied with"
        };
        format!(
            "benchmark: {chosen} {} p/s {verb} {}",
            fmt_fixed(mine, 2),
            detail.join(", ")
        )
    };
    Some((chosen, why))
}

fn is_number(v: &Value) -> bool {
    v.is_number()
}

/// `bench_trials(bench)`: `(format, pages/s)` of a stored benchmark's precision
/// trials, in the order run.
pub fn bench_trials(bench: Option<&Map<String, Value>>) -> Vec<(String, f64)> {
    let Some(Value::Array(trials)) = bench.and_then(|b| b.get("precision_trials")) else {
        return Vec::new();
    };
    trials
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|t| {
            let fmt = t.get("precision")?.as_str()?;
            let rate = t.get("pages_per_second").filter(|v| is_number(v))?;
            PRECISIONS
                .contains(&fmt)
                .then(|| (fmt.to_string(), rate.as_f64().unwrap_or(0.0)))
        })
        .collect()
}

/// `bench_pick(bench, mode)`: a machine's benchmarked pick for a balanced/speed mode
/// and why. Only a benchmark taken FOR this mode, with its precision trials, counts.
pub fn bench_pick(bench: Option<&Map<String, Value>>, mode: &str) -> (Option<String>, String) {
    let none = (None, String::new());
    let Some(b) = bench else { return none };
    if !is_benched(mode) || b.get("precision_mode").and_then(Value::as_str) != Some(mode) {
        return none;
    }
    let trials = bench_trials(bench);
    let Some(ran) = b.get("precision").and_then(Value::as_str) else {
        return none;
    };
    if trials.is_empty() || !trials.iter().any(|(f, _)| f == ran) {
        return none;
    }
    let formats: Vec<String> = trials.iter().map(|(f, _)| f.clone()).collect();
    let picked = pick_precision(&trials, &formats);
    let why = match b.get("precision_why").and_then(Value::as_str) {
        Some(w) if !w.is_empty() => w.to_string(),
        _ => picked.map_or_else(|| "benchmark".to_string(), |(_, w)| w),
    };
    (Some(ran.to_string()), why)
}

/// `bench_precision(bench)`: the precision a stored benchmark's recognizer ran at.
pub fn bench_precision(bench: &Map<String, Value>) -> Option<String> {
    if let Some(p) = bench
        .get("precision")
        .and_then(Value::as_str)
        .filter(|p| PRECISIONS.contains(p))
    {
        return Some(p.to_string());
    }
    let Some(Value::Array(trials)) = bench.get("precision_trials") else {
        return None;
    };
    trials
        .iter()
        .filter_map(Value::as_object)
        .find(|t| crate::py::truthy(t.get("chosen")))
        .and_then(|t| t.get("precision")?.as_str())
        .filter(|p| PRECISIONS.contains(p))
        .map(str::to_string)
}

fn set(formats: &[&str]) -> BTreeSet<String> {
    formats.iter().map(|s| s.to_string()).collect()
}

/// `stale_bench_reason(engine, bench, mode, supported)`: why a stored benchmark no
/// longer describes this machine for the row's mode, or None when it does (or when it
/// cannot be judged).
pub fn stale_bench_reason(
    engine: Option<&str>,
    bench: &Map<String, Value>,
    mode: &str,
    supported: Option<&BTreeSet<String>>,
) -> Option<String> {
    let engine = engine.filter(|e| is_precision_engine(e))?;
    let Some(ran) = bench_precision(bench) else {
        return Some("it records no precision (measured before the precision modes)".into());
    };
    let measured_for = bench
        .get("precision_mode")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .map(|m| normalize_mode(Some(m)));
    if let Some(Some(m)) = measured_for
        && m != mode
    {
        return Some(format!("it was measured for {m}; the row asks {mode} now"));
    }
    let Some(supported) = supported else {
        // Judged only when every kind of device gives the same single answer.
        let mut answers = BTreeSet::new();
        for device in [set(&[FP32]), set(&[FP32, FP16]), set(&[FP32, FP16, BF16])] {
            let r = resolve_mode(engine, mode, Some(&device), None, "");
            if r.eligible {
                answers.insert(r.precision);
            }
        }
        if is_benched(mode) || answers.len() != 1 {
            return None;
        }
        let now = answers.pop_first().flatten();
        return match now {
            Some(now) if now != ran => {
                Some(format!("it ran at {ran}; this machine runs {now} now"))
            }
            _ => None,
        };
    };
    let resolved = resolve_mode(engine, mode, Some(supported), None, "");
    if !resolved.eligible {
        return None;
    }
    if is_benched(mode) && resolved.usable.len() > 1 {
        let tried: BTreeSet<String> = bench_trials(Some(bench)).into_iter().map(|t| t.0).collect();
        let usable: BTreeSet<String> = resolved.usable.iter().cloned().collect();
        if measured_for != Some(Some(mode_static(mode))) || tried != usable {
            let tried_text = if tried.is_empty() {
                "none".to_string()
            } else {
                tried.into_iter().collect::<Vec<_>>().join(", ")
            };
            return Some(format!(
                "its precision trials were {tried_text}; this machine's candidates for {mode} are {}",
                resolved.usable.join(", ")
            ));
        }
        return None;
    }
    match resolved.precision {
        Some(p) if p != ran => Some(format!("it ran at {ran}; this machine runs {p} now")),
        _ => None,
    }
}

fn mode_static(mode: &str) -> &'static str {
    normalize_mode(Some(mode)).unwrap_or(DEFAULT_MODE)
}

/// `resolution_entry(resolved, bench, mode)`: one machine × mode for the admin panel.
pub fn resolution_entry(
    resolved: &ModeResolution,
    bench: Option<&Map<String, Value>>,
    mode: &str,
) -> Map<String, Value> {
    let mut entry = Map::new();
    entry.insert(
        "precision".into(),
        resolved
            .precision
            .as_ref()
            .map_or(Value::Null, |p| Value::String(p.clone())),
    );
    entry.insert("eligible".into(), Value::Bool(resolved.eligible));
    entry.insert("why".into(), Value::String(resolved.why.clone()));
    if is_benched(mode)
        && let Some(b) = bench
        && b.get("precision_mode").and_then(Value::as_str) == Some(mode)
    {
        let trials = bench_trials(bench);
        if !trials.is_empty() {
            entry.insert(
                "trials".into(),
                Value::Array(
                    trials
                        .into_iter()
                        .map(|(f, r)| {
                            let mut t = Map::new();
                            t.insert("precision".into(), Value::String(f));
                            t.insert("pages_per_second".into(), float_value(r));
                            Value::Object(t)
                        })
                        .collect(),
                ),
            );
        }
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn paddle_manga_alone_needs_a_gpu() {
        assert!(gpu_only("paddle-manga"));
        assert!(!gpu_only("hayai-nova") && !gpu_only("ppocr-manga"));
        assert_eq!(
            needs_gpu("paddle-manga"),
            "paddle-manga needs a GPU (NVIDIA CUDA or AMD ROCm); use hayai-nova on the CPU"
        );
    }

    #[test]
    fn resolve_follows_the_policy() {
        let gpu = set(&[FP32, FP16, BF16]);
        let r = resolve_mode("hayai-nova", "auto-accuracy", Some(&gpu), None, "");
        assert_eq!(r.precision.as_deref(), Some("bf16"));
        assert_eq!(r.why, "auto-accuracy");
        let r = resolve_mode("hayai-nova", "auto-speed", Some(&gpu), None, "");
        assert_eq!(r.usable, ["bf16", "fp16", "fp32"]);
        assert_eq!(r.why, "not benchmarked yet: first supported candidate");
        let r = resolve_mode("hayai-nova", "auto-speed", Some(&gpu), Some("fp16"), "");
        assert_eq!(
            (r.precision.as_deref(), r.why.as_str()),
            (Some("fp16"), "benchmark")
        );
        let cpu = set(&[FP32]);
        // A card with fp32 packages only: one candidate, no benchmark pick.
        let r = resolve_mode("paddle-manga", "auto-speed", Some(&cpu), Some("bf16"), "x");
        assert_eq!(
            (r.precision.as_deref(), r.why.as_str()),
            (Some("fp32"), "auto-speed")
        );
        let r = resolve_mode("hayai-nova", "bf16", Some(&cpu), None, "");
        assert!(!r.eligible);
        assert_eq!(r.why, "bf16 not supported");
        let r = resolve_mode("hayai-nova", "bf16", None, None, "");
        assert_eq!(r.why, "card not reported");
        let r = resolve_mode("hayai-nova", "fp32", None, None, "");
        assert!(r.eligible);
        let r = resolve_mode("hayai-nova", "auto-balanced", None, None, "");
        assert_eq!(r.why, "decided at start (card not reported)");
        let r = resolve_mode("ppocr-manga", "fp16", Some(&cpu), None, "");
        assert_eq!((r.precision, r.why.as_str()), (None, "fixed by the engine"));
    }

    #[test]
    fn pick_breaks_ties_toward_accuracy() {
        let usable: Vec<String> = vec!["bf16".into(), "fp32".into()];
        let (p, why) =
            pick_precision(&[("bf16".into(), 5.1157), ("fp32".into(), 1.5431)], &usable).unwrap();
        assert_eq!(p, "bf16");
        assert_eq!(why, "benchmark: bf16 5.12 p/s beat fp32 1.54 p/s");
        let (p, why) =
            pick_precision(&[("bf16".into(), 4.9), ("fp32".into(), 5.0)], &usable).unwrap();
        assert_eq!(p, "bf16");
        assert_eq!(why, "benchmark: bf16 4.90 p/s tied with fp32 5.00 p/s");
        let (p, why) = pick_precision(&[("fp32".into(), 3.0)], &usable).unwrap();
        assert_eq!(
            (p.as_str(), why.as_str()),
            ("fp32", "benchmark: fp32 3.00 p/s")
        );
        assert!(pick_precision(&[("fp16".into(), 3.0)], &usable).is_none());
    }

    #[test]
    fn bench_pick_wants_trials_for_this_mode() {
        let b = obj(json!({
            "precision": "bf16", "precision_mode": "auto-balanced",
            "precision_trials": [{"precision": "bf16", "pages_per_second": 5.0, "chosen": true},
                                 {"precision": "fp32", "pages_per_second": 2.0, "chosen": false}],
            "precision_why": "benchmark: bf16 5.00 p/s beat fp32 2.00 p/s"}));
        assert_eq!(
            bench_pick(Some(&b), "auto-balanced"),
            (
                Some("bf16".into()),
                "benchmark: bf16 5.00 p/s beat fp32 2.00 p/s".into()
            )
        );
        assert_eq!(bench_pick(Some(&b), "auto-speed").0, None);
        assert_eq!(bench_pick(Some(&b), "auto-accuracy").0, None);
        let mut no_why = b.clone();
        no_why.remove("precision_why");
        assert_eq!(
            bench_pick(Some(&no_why), "auto-balanced").1,
            "benchmark: bf16 5.00 p/s beat fp32 2.00 p/s"
        );
    }

    #[test]
    fn staleness_rules() {
        let gpu = set(&[FP32, FP16, BF16]);
        let balanced = obj(json!({
            "precision": "bf16", "precision_mode": "auto-balanced",
            "precision_trials": [{"precision": "bf16", "pages_per_second": 5.0},
                                 {"precision": "fp32", "pages_per_second": 2.0}]}));
        assert_eq!(
            stale_bench_reason(Some("hayai-nova"), &balanced, "auto-balanced", Some(&gpu)),
            None
        );
        assert_eq!(
            stale_bench_reason(Some("hayai-nova"), &balanced, "auto-speed", Some(&gpu)).unwrap(),
            "it was measured for auto-balanced; the row asks auto-speed now"
        );
        let mut speed = balanced.clone();
        speed.insert("precision_mode".into(), json!("auto-speed"));
        assert_eq!(
            stale_bench_reason(Some("hayai-nova"), &speed, "auto-speed", Some(&gpu)).unwrap(),
            "its precision trials were bf16, fp32; this machine's candidates for auto-speed are bf16, fp16, fp32"
        );
        let accuracy = obj(json!({"precision": "fp32", "precision_mode": "auto-accuracy"}));
        assert_eq!(
            stale_bench_reason(Some("hayai-nova"), &accuracy, "auto-accuracy", Some(&gpu)).unwrap(),
            "it ran at fp32; this machine runs bf16 now"
        );
        let cpu = set(&[FP32]);
        assert_eq!(
            stale_bench_reason(Some("hayai-nova"), &accuracy, "auto-accuracy", Some(&cpu)),
            None
        );
        assert_eq!(
            stale_bench_reason(Some("hayai-nova"), &obj(json!({})), "auto-accuracy", None).unwrap(),
            "it records no precision (measured before the precision modes)"
        );
        // Unreported device: paddle-manga's accuracy is fp32 everywhere.
        let bf = obj(json!({"precision": "bf16"}));
        assert_eq!(
            stale_bench_reason(Some("paddle-manga"), &bf, "auto-accuracy", None).unwrap(),
            "it ran at bf16; this machine runs fp32 now"
        );
        assert_eq!(
            stale_bench_reason(Some("hayai-nova"), &bf, "auto-accuracy", None),
            None
        );
        assert_eq!(
            stale_bench_reason(Some("ppocr-manga"), &obj(json!({})), "auto-accuracy", None),
            None
        );
    }

    #[test]
    fn a_candidate_reported_at_zero_counts_as_tried_and_never_wins() {
        let gpu = set(&[FP32, FP16, BF16]);
        let b = obj(json!({
            "precision": "bf16", "precision_mode": "auto-speed",
            "precision_trials": [{"precision": "bf16", "pages_per_second": 5.0, "chosen": true},
                                 {"precision": "fp16", "pages_per_second": 0.0, "chosen": false},
                                 {"precision": "fp32", "pages_per_second": 2.0, "chosen": false}]}));
        assert_eq!(
            stale_bench_reason(Some("hayai-nova"), &b, "auto-speed", Some(&gpu)),
            None
        );
        assert_eq!(
            bench_pick(Some(&b), "auto-speed").0.as_deref(),
            Some("bf16")
        );
    }

    #[test]
    fn bf16_counts_only_where_it_is_native() {
        assert!(bf16_native("cuda", "sm_80") && bf16_native("cuda", "sm_120"));
        assert!(!bf16_native("cuda", "sm_75") && !bf16_native("cuda", ""));
        assert!(bf16_native("rocm", "gfx1100") && bf16_native("rocm", "gfx1201"));
        assert!(!bf16_native("rocm", "gfx1030") && !bf16_native("rocm", "gfx1032"));
        assert!(!bf16_native("cpu", "x86_64"));
        let all: Vec<String> = vec!["fp32".into(), "fp16".into(), "bf16".into()];
        let rdna2 = |mode| device_formats(mode, &all, Some("rocm"), Some("gfx1030"));
        assert_eq!(rdna2("auto-accuracy"), set(&[FP32, FP16]));
        assert_eq!(rdna2("auto-speed"), set(&[FP32, FP16]));
        assert_eq!(
            rdna2("bf16"),
            set(&[FP32, FP16, BF16]),
            "forced: wherever it exists"
        );
        assert_eq!(
            device_formats("auto-accuracy", &all, Some("rocm"), Some("gfx1201")),
            set(&[FP32, FP16, BF16])
        );
        let cpu: Vec<String> = vec!["bf16".into()];
        assert_eq!(
            device_formats("auto-balanced", &cpu, Some("cpu"), None),
            set(&[FP32])
        );
        assert_eq!(device_formats("bf16", &cpu, None, None), set(&[FP32, BF16]));
        // What the engines resolve to on such devices is what the library expects.
        let gfx1030 = rdna2("auto-accuracy");
        let r = resolve_mode("hayai-nova", "auto-accuracy", Some(&gfx1030), None, "");
        assert_eq!(r.precision.as_deref(), Some("fp32"));
        let fp32 = obj(json!({"precision": "fp32", "precision_mode": "auto-accuracy"}));
        assert_eq!(
            stale_bench_reason(Some("hayai-nova"), &fp32, "auto-accuracy", Some(&gfx1030)),
            None
        );
    }

    #[test]
    fn entries_carry_trials_only_for_their_mode() {
        let gpu = set(&[FP32, FP16, BF16]);
        let b = obj(json!({
            "precision": "bf16", "precision_mode": "auto-balanced",
            "precision_trials": [{"precision": "bf16", "pages_per_second": 5.0}]}));
        let r = resolve_mode("hayai-nova", "auto-balanced", Some(&gpu), Some("bf16"), "w");
        let e = resolution_entry(&r, Some(&b), "auto-balanced");
        assert_eq!(
            Value::Object(e),
            json!({"precision": "bf16", "eligible": true, "why": "w",
                   "trials": [{"precision": "bf16", "pages_per_second": 5.0}]})
        );
        let r = resolve_mode("hayai-nova", "auto-speed", Some(&gpu), None, "");
        assert!(!resolution_entry(&r, Some(&b), "auto-speed").contains_key("trials"));
    }
}
