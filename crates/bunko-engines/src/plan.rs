//! How wide each stage runs and how deep its queue is (0.5.2 `STAGE_GRAPHS`,
//! `plan_stage_workers`, `stage_widths`, `stage_capacities`, `host_worker_budget`;
//! spec ocr-recognizers §2.3, §7.3).
//!
//! Widths are derived from per-page stage costs against the stage that sets the pace
//! (the device-bound engine stage where there is one), with 2× headroom for slow
//! pages, clamped to the host's worker budget ([`host_worker_budget`]) and to
//! [`CPU_WORKERS_MAX`] (4 on the ppocr-manga road, as 0.5.2); an explicit
//! `pools.stage_workers` entry always wins. The engine stage is one worker, two on a
//! fast GPU (native bf16 class) of a host with 16+ physical cores, where a second thread
//! overlaps the host side of one batch with the device work of the other (4090 +57%,
//! 9070 XT +21% with the detect stage widened to match); an explicit width up to 8 runs
//! that many threads over the one shared session; on the CPU it stays 1. Width 0
//! (0.5.2's fused serial fallback) runs as 1.

use std::collections::BTreeMap;

use bunko_proto::PoolsSpec;

pub const STAGE_DETECT: &str = "detect";
pub const STAGE_ENGINE: &str = "engine";
pub const STAGE_POST: &str = "post";
pub const STAGE_LAYOUT: &str = "layout";

/// Pooled CPU workers at most (the detect workers share one PP-OCR session, so they
/// scale past 0.5.2's plateau of 4 separate sessions: 8 measured on a 24-core host).
pub const CPU_WORKERS_MAX: u32 = 8;
/// 0.5.2's `CPU_WORKERS_MAX`, kept for the ppocr-manga road (measured there).
pub const LINE_WORKERS_MAX: u32 = 4;
/// `SESSION_THREADS`: ORT intra-op threads of the shared PP-OCR session with one
/// detect worker; each further worker adds [`SESSION_THREADS_PER_WORKER`].
pub const SESSION_THREADS: usize = 4;
pub const SESSION_THREADS_PER_WORKER: usize = 2;
/// Logical CPUs kept for the engine's host thread, the post stage and the feeder.
pub const RESERVED_THREADS: usize = 2;
/// The engine stage's default width on a fast GPU of a large host.
pub const FAST_GPU_ENGINE_THREADS: u32 = 2;
/// Physical cores a host needs before the engine stage gets two threads.
pub const FAST_GPU_MIN_CORES: usize = 16;
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

/// `ENGINE_STAGE_SECONDS` (GPU figures), the CPU engine figures, else the road's.
///
/// 0.5.2 only knew the GPU figures, so a CPU engine stage was sized as if it took
/// 0.177 s a page and got three detect workers. On the CPU the engine is 10-50x
/// slower (measured on a 16-core desktop, fp32: hayai-nova 3.6 s a page; paddle-manga
/// took 27 s, which is why it runs on a GPU only now), one detect worker keeps it fed,
/// and each extra one only holds another decoded page and another detector working set
/// in memory.
fn stage_seconds(engine: &str, key: &str, declared: f64, gpu: bool) -> f64 {
    match (engine, key, gpu) {
        ("paddle-manga", STAGE_ENGINE, true) => 0.915,
        ("hayai-nova", STAGE_ENGINE, true) => 0.177,
        ("hayai-nova", STAGE_ENGINE, false) => 3.6,
        _ => declared,
    }
}

/// Threads of a CPU recognizer (libtorch intra-op / OpenMP, or an ONNX Runtime
/// session) for one of `jobs` concurrent jobs.
///
/// On a host with performance and efficiency cores (Apple silicon) it is the
/// performance cores alone: the recognizer's OpenMP regions split work evenly and wait
/// at a barrier for the slowest thread, so a share on an efficiency core holds the
/// whole region back, and the efficiency cores are left to the detect stage. libtorch
/// makes the same choice for its own default (`TaskThreadPoolBase::defaultNumThreads`),
/// and so 0.5.2 ran with it. M2 Pro (8P + 4E), hayai-nova fp32, 50 dense pages: 8
/// threads 80.1 s, 11 (all cores but one) 81.3 s, 6 84.5 s. Elsewhere all physical
/// cores but one, the one left to the pipeline's other stages.
pub fn cpu_engine_threads(physical: usize, performance: Option<usize>, jobs: usize) -> usize {
    let jobs = jobs.max(1);
    match performance {
        Some(p) if p > 0 && p < physical => (p / jobs).max(1),
        _ => (physical.saturating_sub(1) / jobs).max(1),
    }
}

/// Performance cores of a hybrid CPU, when the OS reports them (macOS
/// `hw.perflevel0.physicalcpu`; None on a CPU of one kind or elsewhere).
pub fn performance_core_count() -> Option<usize> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::{c_char, c_int, c_void};
        unsafe extern "C" {
            fn sysctlbyname(
                name: *const c_char,
                oldp: *mut c_void,
                oldlenp: *mut usize,
                newp: *mut c_void,
                newlen: usize,
            ) -> c_int;
        }
        let read = |name: &std::ffi::CStr| {
            let mut n: c_int = 0;
            let mut len = std::mem::size_of::<c_int>();
            // SAFETY: a sysctl of type int read into an int of the given length.
            let rc = unsafe {
                sysctlbyname(
                    name.as_ptr(),
                    (&raw mut n).cast(),
                    &mut len,
                    std::ptr::null_mut(),
                    0,
                )
            };
            (rc == 0 && n > 0).then_some(n as usize)
        };
        // perflevel1 exists only on a CPU with a second (efficiency) level.
        read(c"hw.perflevel1.physicalcpu")?;
        read(c"hw.perflevel0.physicalcpu")
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
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

/// Logical CPUs (SMT threads included).
pub fn logical_cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// This session's share of the host, in detect workers: the most workers whose shared
/// PP-OCR session (`SESSION_THREADS` + 2 per extra worker) fits in its share of the
/// logical CPUs after [`RESERVED_THREADS`]. 0.5.2 counted 4 cores per worker (one
/// session each), which left 8-core hosts at one detect worker with the GPU 23-59%
/// busy; three workers there measured +78-90% (RTX 4060, RX 6900 XT).
pub fn host_worker_budget(logical_cpus: usize, jobs: usize) -> u32 {
    let share = logical_cpus / jobs.max(1);
    let spare = share.saturating_sub(RESERVED_THREADS + SESSION_THREADS);
    (spare / SESSION_THREADS_PER_WORKER + 1).max(1) as u32
}

/// What the planner needs to know of the host and the engine's device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Host {
    /// Detect workers this session may have ([`host_worker_budget`]).
    pub budget: u32,
    pub physical_cores: usize,
    /// The engine's GPU is of the fast class (`precision::bf16_native`).
    pub fast_gpu: bool,
}

/// The stages of a road with their widths and capacities for this run.
pub fn plan(
    engine: &str,
    road: Road,
    engine_device: &str,
    pools: &PoolsSpec,
    host: &Host,
) -> Vec<StagePlan> {
    let gpu = engine_device.starts_with("gpu");
    let budget = host.budget;
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
        .map(|d| stage_seconds(engine, d.0, d.4, d.2.starts_with("gpu")))
        .collect();
    let cap = if road == Road::Line {
        LINE_WORKERS_MAX
    } else {
        CPU_WORKERS_MAX
    };
    let ceiling = budget.clamp(1, cap);
    // The engine stage's default width (only the fast-GPU hayai-nova case is 2: its
    // device time per page is small next to its host side).
    let engine_threads = |cost: f64| {
        if gpu && host.fast_gpu && host.physical_cores >= FAST_GPU_MIN_CORES && cost < 0.3 {
            FAST_GPU_ENGINE_THREADS
        } else {
            1
        }
    };
    let bound = declared
        .iter()
        .zip(&costs)
        .filter(|(d, _)| d.3)
        .map(|(_, c)| *c / f64::from(engine_threads(*c)))
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
            let derived = if d.3 {
                engine_threads(cost)
            } else if d.2.starts_with("gpu") {
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

/// How wide each stage of a planned session may be made by a benchmark's width search:
/// the widths the planner itself allows on this host. Pooled CPU stages up to the host
/// budget and the road's cap; the engine stage on a GPU up to [`MAX_ENGINE_THREADS`]
/// (bounded by the budget: each engine thread needs a host thread), on the CPU 1. Never
/// below the width the plan already gave a stage.
pub fn width_ceilings(stages: &[StagePlan], host: &Host) -> BTreeMap<String, u32> {
    let line = !stages.iter().any(|s| s.key == STAGE_ENGINE);
    let cap = if line {
        LINE_WORKERS_MAX
    } else {
        CPU_WORKERS_MAX
    };
    stages
        .iter()
        .map(|s| {
            let ceiling = if s.device_bound {
                if s.device.starts_with("gpu") {
                    host.budget.clamp(1, MAX_ENGINE_THREADS)
                } else {
                    1
                }
            } else {
                host.budget.clamp(1, cap)
            };
            (s.key.to_string(), ceiling.max(s.workers))
        })
        .collect()
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

    fn host(budget: u32) -> Host {
        Host {
            budget,
            physical_cores: 8,
            fast_gpu: false,
        }
    }

    #[test]
    fn derived_widths_match_the_measured_ones() {
        let none = PoolsSpec::default();
        // hayai-nova on a GPU: detect 3 (25.7% idle at 1, 0.8% at 3), engine 1, post 1.
        let p = plan("hayai-nova", Road::Reconciled, "gpu:0", &none, &host(3));
        assert_eq!(widths(&p), vec![(3, 4), (1, 1), (1, 1)]);
        assert_eq!(
            graph_line(&p),
            "detect (cpu x3, queue 4) -> engine (gpu:0 x1, queue 1) -> post (cpu x1, queue 1)"
        );
        // paddle-manga: the card is the bottleneck, one detect worker is enough.
        let p = plan("paddle-manga", Road::Reconciled, "gpu:0", &none, &host(6));
        assert_eq!(widths(&p), vec![(1, 4), (1, 1), (1, 1)]);
        // A CPU engine is slow enough that one detect worker keeps it fed.
        let p = plan("hayai-nova", Road::Reconciled, "cpu", &none, &host(6));
        assert_eq!(widths(&p), vec![(1, 4), (1, 1), (1, 1)]);
        // ppocr-manga: detect takes the ceiling (0.5.2's 4), the layout one worker.
        let p = plan("ppocr-manga", Road::Line, "cpu", &none, &host(7));
        assert_eq!(widths(&p), vec![(4, 4), (1, 1)]);
        let p = plan("ppocr-manga", Road::Line, "cpu", &none, &host(1));
        assert_eq!(widths(&p), vec![(1, 1), (1, 1)]);
    }

    #[test]
    fn fleet_defaults() {
        let none = PoolsSpec::default();
        let line = |logical: usize, physical: usize, fast: bool, engine: &str| {
            let h = Host {
                budget: host_worker_budget(logical, 1),
                physical_cores: physical,
                fast_gpu: fast,
            };
            graph_line(&plan(engine, Road::Reconciled, "gpu:0", &none, &h))
        };
        // 8-core/16-thread hosts (lily 4060, server/patrick RDNA2): detect 3, was 1.
        assert!(
            line(16, 8, true, "hayai-nova")
                .starts_with("detect (cpu x3, queue 4) -> engine (gpu:0 x1")
        );
        assert!(
            line(16, 8, false, "hayai-nova")
                .starts_with("detect (cpu x3, queue 4) -> engine (gpu:0 x1")
        );
        // 6-core/12-thread (steven): detect 3.
        assert!(line(12, 6, false, "hayai-nova").starts_with("detect (cpu x3"));
        // 16-core desktop 9070 XT, 24-core beast 4090: engine 2, detect 6.
        assert!(
            line(32, 16, true, "hayai-nova")
                .starts_with("detect (cpu x6, queue 6) -> engine (gpu:0 x2, queue 2)")
        );
        assert!(
            line(48, 24, true, "hayai-nova")
                .starts_with("detect (cpu x6, queue 6) -> engine (gpu:0 x2, queue 2)")
        );
        // ...but not with a slow-bf16 card or for paddle-manga (GPU-bound).
        assert!(line(32, 16, false, "hayai-nova").contains("engine (gpu:0 x1"));
        assert!(
            line(32, 16, true, "paddle-manga")
                .starts_with("detect (cpu x1, queue 4) -> engine (gpu:0 x1")
        );
        // 4-core hosts stay small: 4 threads = 1 worker, 8 threads = 2.
        assert!(line(4, 4, false, "hayai-nova").starts_with("detect (cpu x1"));
        assert!(line(8, 4, true, "hayai-nova").starts_with("detect (cpu x2"));
    }

    #[test]
    fn explicit_pools_win_within_structural_limits() {
        let mut pools = PoolsSpec::default();
        pools.stage_workers.insert("detect".into(), 6);
        pools.stage_workers.insert("engine".into(), 12);
        pools.queue_capacity.insert("post".into(), 8);
        let p = plan("hayai-nova", Road::Reconciled, "gpu:0", &pools, &host(3));
        assert_eq!(widths(&p), vec![(6, 6), (8, 8), (1, 8)]);
        let p = plan("hayai-nova", Road::Reconciled, "cpu", &pools, &host(3));
        assert_eq!(p[1].workers, 1, "the CPU engine stage is one worker");
        assert_eq!(p[0].workers, 6, "an explicit width still wins on the CPU");
        pools.stage_workers.insert("detect".into(), 0);
        let p = plan("hayai-nova", Road::Reconciled, "cpu", &pools, &host(3));
        assert_eq!(p[0].workers, 1);
    }

    #[test]
    fn width_ceilings_follow_the_planner() {
        let none = PoolsSpec::default();
        let h = Host {
            budget: host_worker_budget(16, 1),
            physical_cores: 8,
            fast_gpu: false,
        };
        let p = plan("hayai-nova", Road::Reconciled, "gpu:0", &none, &h);
        let c = width_ceilings(&p, &h);
        assert_eq!((c["detect"], c["engine"], c["post"]), (6, 6, 6));
        let p = plan("hayai-nova", Road::Reconciled, "cpu", &none, &h);
        assert_eq!(
            width_ceilings(&p, &h)["engine"],
            1,
            "a CPU engine is never widened"
        );
        let big = Host {
            budget: host_worker_budget(48, 1),
            physical_cores: 24,
            fast_gpu: true,
        };
        let p = plan("hayai-nova", Road::Reconciled, "gpu:0", &none, &big);
        let c = width_ceilings(&p, &big);
        assert_eq!(
            (c["detect"], c["engine"]),
            (CPU_WORKERS_MAX, MAX_ENGINE_THREADS)
        );
        let p = plan("ppocr-manga", Road::Line, "cpu", &none, &big);
        assert_eq!(width_ceilings(&p, &big)["detect"], LINE_WORKERS_MAX);
        // a small host: never below what the plan already runs
        let small = Host {
            budget: 1,
            physical_cores: 2,
            fast_gpu: false,
        };
        let mut pools = PoolsSpec::default();
        pools.stage_workers.insert("detect".into(), 3);
        let p = plan("hayai-nova", Road::Reconciled, "gpu:0", &pools, &small);
        assert_eq!(width_ceilings(&p, &small)["detect"], 3);
    }

    #[test]
    fn budget() {
        assert_eq!(host_worker_budget(16, 1), 6);
        assert_eq!(host_worker_budget(12, 1), 4);
        assert_eq!(host_worker_budget(8, 1), 2);
        assert_eq!(host_worker_budget(4, 1), 1);
        assert_eq!(host_worker_budget(16, 2), 2);
        assert_eq!(host_worker_budget(2, 1), 1);
        assert!(physical_cpu_count() >= 1);
    }

    #[test]
    fn cpu_engine_threads_prefer_performance_cores() {
        // M2 Pro: 8 performance + 4 efficiency cores.
        assert_eq!(cpu_engine_threads(12, Some(8), 1), 8);
        assert_eq!(cpu_engine_threads(12, Some(8), 2), 4);
        // One kind of core (or none reported): all physical cores but one.
        assert_eq!(cpu_engine_threads(16, None, 1), 15);
        assert_eq!(cpu_engine_threads(16, Some(16), 1), 15);
        assert_eq!(cpu_engine_threads(16, None, 2), 7);
        assert_eq!(cpu_engine_threads(1, None, 1), 1);
        assert_eq!(cpu_engine_threads(12, Some(8), 16), 1);
        if let Some(p) = performance_core_count() {
            assert!(p < physical_cpu_count());
        }
    }
}
