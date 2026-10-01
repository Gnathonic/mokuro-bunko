//! How wide each stage runs and how deep its queue is (0.5.2 `STAGE_GRAPHS`,
//! `plan_stage_workers`, `stage_widths`, `stage_capacities`, `host_worker_budget`;
//! spec ocr-recognizers §2.3, §7.3).
//!
//! Widths are derived from per-page stage costs against the stage that sets the pace
//! (the device-bound engine stage where there is one), with 2× headroom for slow
//! pages, clamped to the host's worker budget and to 4; an explicit
//! `pools.stage_workers` entry always wins. The engine stage is one worker; on a GPU an
//! explicit width up to 8 runs that many threads over the one shared session (0.5.2's
//! "N model copies"); on the CPU it stays 1. Width 0 (0.5.2's fused serial fallback)
//! runs as 1.

use std::collections::BTreeMap;

use bunko_proto::PoolsSpec;

pub const STAGE_DETECT: &str = "detect";
pub const STAGE_ENGINE: &str = "engine";
pub const STAGE_POST: &str = "post";
pub const STAGE_LAYOUT: &str = "layout";

/// `CPU_WORKERS_MAX`: the measured plateau of pooled CPU stages.
pub const CPU_WORKERS_MAX: u32 = 4;
/// `SESSION_THREADS`: ORT intra-op threads a PP-OCR session takes (budgeting only).
pub const SESSION_THREADS: usize = 4;
/// `LOAD_WINDOW_SLOTS`: capacity of the queue feeding a device-bound stage.
pub const LOAD_WINDOW_SLOTS: u32 = 4;
/// `STAGE_WIDTH_HEADROOM`: p90/mean of the detect stage's page time.
pub const STAGE_WIDTH_HEADROOM: f64 = 2.0;
/// `MAX_ENGINE_COPIES`: engine threads over one GPU session at most.
pub const MAX_ENGINE_THREADS: u32 = 8;
/// `Pipeline.SOURCE_EXTRA`: the feed queue holds the detect width plus this.
pub const SOURCE_EXTRA: u32 = 2;

/// The two roads (spec ocr-ppocr-layout §7.0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Road {
    /// ppocr-manga alone: detect (+ CTC) → layout.
    Line,
    /// hayai-nova / paddle-manga over the ppocr-manga detector: detect → engine → post.
    Reconciled,
}

impl Road {
    pub fn of(engine: &str) -> Option<Road> {
        match engine {
            "ppocr-manga" => Some(Road::Line),
            "hayai-nova" | "paddle-manga" => Some(Road::Reconciled),
            _ => None,
        }
    }
}

/// One stage as run.
#[derive(Debug, Clone, PartialEq)]
pub struct StagePlan {
    pub key: &'static str,
    pub name: &'static str,
    /// `cpu` or `gpu:<n>`.
    pub device: String,
    pub device_bound: bool,
    pub workers: u32,
    /// Capacity of the queue this stage fills.
    pub capacity: u32,
    /// Seconds a page costs here (sizing only).
    pub seconds: f64,
}

/// `ENGINE_STAGE_SECONDS`, else the road's figure.
fn stage_seconds(engine: &str, key: &str, declared: f64) -> f64 {
    match (engine, key) {
        ("paddle-manga", STAGE_ENGINE) => 0.915,
        ("hayai-nova", STAGE_ENGINE) => 0.177,
        _ => declared,
    }
}

/// Physical cores (0.5.2 sized on those, not on SMT threads).
pub fn physical_cpu_count() -> usize {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    #[cfg(target_os = "linux")]
    {
        let mut cores = std::collections::BTreeSet::new();
        if let Ok(dir) = std::fs::read_dir("/sys/devices/system/cpu") {
            for entry in dir.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                let topo = entry.path().join("topology");
                let read = |f: &str| {
                    std::fs::read_to_string(topo.join(f))
                        .ok()
                        .map(|s| s.trim().to_string())
                };
                if let (Some(pkg), Some(core)) = (read("physical_package_id"), read("core_id")) {
                    cores.insert((pkg, core));
                }
            }
        }
        if !cores.is_empty() && cores.len() <= logical {
            return cores.len();
        }
    }
    logical
}

/// `host_worker_budget`: this session's share of the host, in pooled workers.
pub fn host_worker_budget(cpus: usize, jobs: usize) -> u32 {
    let share = (cpus.saturating_sub(1) / jobs.max(1)).max(1);
    (share / SESSION_THREADS).max(1) as u32
}

/// The stages of a road with their widths and capacities for this run.
pub fn plan(
    engine: &str,
    road: Road,
    engine_device: &str,
    pools: &PoolsSpec,
    budget: u32,
) -> Vec<StagePlan> {
    let gpu = engine_device.starts_with("gpu");
    let declared: Vec<(&'static str, &'static str, String, bool, f64)> = match road {
        Road::Line => vec![
            (STAGE_DETECT, "detect + CTC read", "cpu".into(), false, 0.19),
            (STAGE_LAYOUT, "layout + dump", "cpu".into(), false, 0.011),
        ],
        Road::Reconciled => vec![
            (
                STAGE_DETECT,
                "detect + CTC read",
                "cpu".into(),
                false,
                0.225,
            ),
            (
                STAGE_ENGINE,
                "engine read + reconcile",
                engine_device.to_string(),
                true,
                0.30,
            ),
            (STAGE_POST, "layout + dump", "cpu".into(), false, 0.011),
        ],
    };
    let costs: Vec<f64> = declared
        .iter()
        .map(|d| stage_seconds(engine, d.0, d.4))
        .collect();
    let ceiling = budget.clamp(1, CPU_WORKERS_MAX);
    let bound = declared
        .iter()
        .zip(&costs)
        .filter(|(d, _)| d.3)
        .map(|(_, c)| *c)
        .fold(0.0f64, f64::max);
    let pace = if bound > 0.0 {
        bound
    } else {
        // The leading pooled stage at its own ceiling sets the period.
        let lead = costs.iter().copied().fold(0.0f64, f64::max);
        if lead > 0.0 {
            lead / f64::from(ceiling)
        } else {
            0.0
        }
    };
    let mut out: Vec<StagePlan> = declared
        .iter()
        .zip(&costs)
        .map(|(d, &cost)| {
            let derived = if d.3 || d.2.starts_with("gpu") {
                1
            } else if pace > 0.0 {
                ((STAGE_WIDTH_HEADROOM * cost / pace).ceil() as u32).clamp(1, ceiling)
            } else {
                ceiling
            };
            let explicit = pools.stage_workers.get(d.0).copied();
            let max = if d.3 {
                if gpu { MAX_ENGINE_THREADS } else { 1 }
            } else {
                64
            };
            let workers = explicit.unwrap_or(derived).clamp(1, max);
            StagePlan {
                key: d.0,
                name: d.1,
                device: d.2.clone(),
                device_bound: d.3,
                workers,
                capacity: 1,
                seconds: cost,
            }
        })
        .collect();
    for i in 0..out.len() {
        let mut default = out[i].workers.max(1);
        if out.get(i + 1).is_some_and(|n| n.device_bound) {
            default = default.max(LOAD_WINDOW_SLOTS);
        }
        let asked = pools.queue_capacity.get(out[i].key).copied();
        out[i].capacity = asked.unwrap_or(default).max(1);
    }
    out
}

/// `graph_line`: `detect (cpu x3, queue 4) -> engine (gpu:0 x1, queue 1) -> ...`.
pub fn graph_line(stages: &[StagePlan]) -> String {
    stages
        .iter()
        .map(|s| {
            format!(
                "{} ({} x{}, queue {})",
                s.key, s.device, s.workers, s.capacity
            )
        })
        .collect::<Vec<_>>()
        .join(" -> ")
}

/// `ready.stage_workers` / `ready.queue_capacity`.
pub fn tables(stages: &[StagePlan]) -> (BTreeMap<String, u32>, BTreeMap<String, u32>) {
    (
        stages
            .iter()
            .map(|s| (s.key.to_string(), s.workers))
            .collect(),
        stages
            .iter()
            .map(|s| (s.key.to_string(), s.capacity))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widths(stages: &[StagePlan]) -> Vec<(u32, u32)> {
        stages.iter().map(|s| (s.workers, s.capacity)).collect()
    }

    #[test]
    fn derived_widths_match_the_measured_ones() {
        let none = PoolsSpec::default();
        // hayai-nova on a GPU: detect 3 (25.7% idle at 1, 0.8% at 3), engine 1, post 1.
        let p = plan("hayai-nova", Road::Reconciled, "gpu:0", &none, 3);
        assert_eq!(widths(&p), vec![(3, 4), (1, 1), (1, 1)]);
        assert_eq!(
            graph_line(&p),
            "detect (cpu x3, queue 4) -> engine (gpu:0 x1, queue 1) -> post (cpu x1, queue 1)"
        );
        // paddle-manga: the card is the bottleneck, one detect worker is enough.
        let p = plan("paddle-manga", Road::Reconciled, "gpu:0", &none, 3);
        assert_eq!(widths(&p), vec![(1, 4), (1, 1), (1, 1)]);
        // ppocr-manga: detect takes the ceiling, the layout one worker.
        let p = plan("ppocr-manga", Road::Line, "cpu", &none, 7);
        assert_eq!(widths(&p), vec![(4, 4), (1, 1)]);
        let p = plan("ppocr-manga", Road::Line, "cpu", &none, 1);
        assert_eq!(widths(&p), vec![(1, 1), (1, 1)]);
    }

    #[test]
    fn explicit_pools_win_within_structural_limits() {
        let mut pools = PoolsSpec::default();
        pools.stage_workers.insert("detect".into(), 6);
        pools.stage_workers.insert("engine".into(), 12);
        pools.queue_capacity.insert("post".into(), 8);
        let p = plan("hayai-nova", Road::Reconciled, "gpu:0", &pools, 3);
        assert_eq!(widths(&p), vec![(6, 6), (8, 8), (1, 8)]);
        let p = plan("hayai-nova", Road::Reconciled, "cpu", &pools, 3);
        assert_eq!(p[1].workers, 1, "the CPU engine stage is one worker");
        pools.stage_workers.insert("detect".into(), 0);
        let p = plan("hayai-nova", Road::Reconciled, "cpu", &pools, 3);
        assert_eq!(p[0].workers, 1);
    }

    #[test]
    fn budget() {
        assert_eq!(host_worker_budget(16, 1), 3);
        assert_eq!(host_worker_budget(16, 2), 1);
        assert_eq!(host_worker_budget(2, 1), 1);
        assert!(physical_cpu_count() >= 1);
    }
}
