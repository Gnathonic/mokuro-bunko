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
//! that many threads over the one shared session. A libtorch recognizer on the CPU
//! (hayai-nova, paddle-manga) stays at 1: one call already spreads over every core.
//!
//! The text-box recognizers (charspot-small, nar-small) run on the CPU through ONNX
//! Runtime, or on a GPU through the libtorch backend pack. On the CPU each call reads
//! one block crop, too little work to keep a wide intra-op pool busy, so their engine
//! stage is a pooled CPU stage there: many workers over one shared session of one or
//! two threads ([`textbox_session_threads`]). The job's cores ([`core_count`], SMT
//! threads aside) are split between it and the detect stage by what a page costs each
//! ([`textbox_engine_workers`], [`textbox_detect_workers`]); an explicit engine width
//! leaves detect at its share of that default split. A 24-core host reads nar-small on
//! AnimeText blocks about 3x faster that way than with one 8-thread worker, at half the
//! CPU time a block. On a GPU a page's blocks are one batched call, so the engine stage
//! is device-bound like hayai-nova's, sized by its GPU figure ([`textbox_gpu_seconds`]).
//! Width 0 (0.5.2's fused serial fallback) runs as 1.

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
/// An explicit width of a pooled CPU stage at most.
pub const MAX_POOLED_WORKERS: u32 = 64;
/// `Pipeline.SOURCE_EXTRA`: the feed queue holds the detect width plus this.
pub const SOURCE_EXTRA: u32 = 2;
/// CPU seconds the detect stage spends on a page beside a text-box recognizer: AnimeText
/// block detection, 0.8-1.0 on a 24-core host and 1.05-1.3 on a 16-core desktop
/// (a test volume, 9.4 blocks a page); it sizes the stage's share of the cores.
pub const DETECT_CPU_SECONDS: f64 = 0.9;
/// Seconds the detect stage spends on a page when the block detector runs on a GPU
/// (AnimeText on the pack, one page a call), which sizes its pace beside the engine. The
/// spike's figure on a 16-core desktop with an RX 9070 XT; replaced by the gated
/// targets' measured ones.
pub const DETECT_GPU_SECONDS: f64 = 0.045;
/// CPU seconds a page costs the host side of a GPU detect stage (decoding the page,
/// the letterboxed canvas, the blocks): what it takes from a text-box CPU pool's cores
/// in place of [`DETECT_CPU_SECONDS`].
pub const DETECT_GPU_HOST_SECONDS: f64 = 0.04;
/// Workers of a GPU detect stage by default: two keep the card fed while one prepares
/// its next page.
pub const DETECT_GPU_WORKERS: u32 = 2;
/// Workers of a GPU detect stage at most (an explicit width or a benchmark's search):
/// each one holds a page on the card.
pub const DETECT_GPU_WORKERS_MAX: u32 = 4;
/// Intra-op threads of a text-box recognizer's session at most (the models' own
/// packages stop at eight: more does not help them).
pub const TEXTBOX_THREADS_MAX: usize = 8;

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
            e if crate::models::textbox_engine(e) => Some(Road::Reconciled),
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
/// slower (measured on a 16-core desktop, fp32: hayai-nova 3.6 s, paddle-manga 27 s a
/// page), one detect worker keeps it fed, and each extra one only holds another
/// decoded page and another detector working set in memory.
///
/// The text-box recognizers' CPU figure is one worker's page at one intra-op thread,
/// which is also the CPU time the page costs ([`textbox_cpu_seconds`]); on a GPU it is
/// the page's one batched call ([`textbox_gpu_seconds`]).
fn stage_seconds(engine: &str, key: &str, declared: f64, gpu: bool) -> f64 {
    match (engine, key, gpu) {
        ("paddle-manga", STAGE_ENGINE, true) => 0.915,
        ("hayai-nova", STAGE_ENGINE, true) => 0.177,
        ("paddle-manga", STAGE_ENGINE, false) => 27.0,
        ("hayai-nova", STAGE_ENGINE, false) => 3.6,
        (e, STAGE_ENGINE, true) => textbox_gpu_seconds(e).unwrap_or(declared),
        (e, STAGE_ENGINE, false) => textbox_cpu_seconds(e).unwrap_or(declared),
        (_, STAGE_DETECT, true) => DETECT_GPU_SECONDS,
        _ => declared,
    }
}

/// Seconds a page costs a text-box recognizer on a GPU (libtorch, one batched call of
/// the page's blocks), None for any other engine. The spike's figures on a 16-core
/// desktop with an RX 9070 XT, 9.4 blocks a page; replaced by the gated targets'
/// measured ones once they are built. charspot-small's is gated: its engine stage on one
/// worker took 0.094 s a page on that desktop with AnimeText on the same GPU and 0.125 s
/// with PP-OCR's line boxes (a test volume, 8.3 and 8.6 blocks a page), the slower card
/// next to an RTX 4090 (which reads several times its crops a second).
pub fn textbox_gpu_seconds(engine: &str) -> Option<f64> {
    match engine {
        "nar-small" => Some(0.12),
        "charspot-small" => Some(0.1),
        _ => None,
    }
}

/// CPU seconds a page costs a text-box recognizer on the CPU on one intra-op thread,
/// None for any other engine. Measured on a 24-core host, 9.4 blocks a page: nar-small
/// 0.13-0.19 s a block, charspot-small 0.035-0.047 (a 16-core desktop: 0.23, 0.05).
pub fn textbox_cpu_seconds(engine: &str) -> Option<f64> {
    match engine {
        "nar-small" => Some(1.5),
        "charspot-small" => Some(0.4),
        _ => None,
    }
}

/// Physical cores of one of `jobs` concurrent jobs (at least 1).
pub fn core_share(physical_cores: usize, jobs: usize) -> usize {
    (physical_cores / jobs.max(1)).max(1)
}

/// The default width of a text-box recognizer's engine stage on the CPU: its part of
/// the job's `share` of physical cores, split with the detect stage by what a page
/// costs each on the CPU (`engine_cost` against `detect_cost`: [`DETECT_CPU_SECONDS`],
/// or [`DETECT_GPU_HOST_SECONDS`] when the detect stage runs on a GPU), at one worker a
/// core.
/// Measured nar-small widths at one thread over one session: 24-core host 82-89 blocks/s
/// at 16-20 workers against 18 with today's one 8-thread worker; the plateau sets in
/// at the free physical cores (SMT siblings add CPU time a block, not blocks).
pub fn textbox_engine_workers(engine_cost: f64, detect_cost: f64, share: usize) -> u32 {
    let share = share.max(1);
    let part = engine_cost / (engine_cost + detect_cost);
    ((share as f64 * part).round() as u32).clamp(1, share as u32)
}

/// Detect workers beside a text-box recognizer on the CPU: the shared detect session
/// ([`SESSION_THREADS`], [`SESSION_THREADS_PER_WORKER`] more for each further worker)
/// sized to the cores `engine_workers` leave of the job's `share` (rounded to the
/// nearest worker). With nar-small on AnimeText blocks: 4 on a 24-core host (6.0 pages/s
/// measured with 16 engine workers, 5.2 with 3 detect workers), 2 on a 16-core desktop,
/// 1 on an 8-core host.
pub fn textbox_detect_workers(engine_workers: u32, share: usize) -> u32 {
    let rest = share.saturating_sub(engine_workers as usize);
    let extra = rest
        .saturating_sub(SESSION_THREADS)
        .div_ceil(SESSION_THREADS_PER_WORKER);
    extra as u32 + 1
}

/// Intra-op threads of a text-box recognizer's one shared session, for an engine stage
/// of `workers`: `single` (at most 8) for a single worker, where `single` is the job's
/// [`cpu_engine_threads`] as before the stage was pooled (all physical cores but one,
/// or the performance cores of a hybrid CPU); 2 for two workers; 1 for a wider pool. Each call reads one block, too little to keep a pool busy: on a 24-core
/// host, 16 workers spent 0.19 CPU-s a block at 1 thread, 0.20 at 2, 0.23 at 4 and
/// 0.29 at 8, for the same blocks a second, while at one worker 8 threads read 2.3x
/// faster than 1 and at two, 2 threads read 30% faster than 1 (at four, 14%, for 10%
/// more CPU time). The text is the same at every width and thread count.
pub fn textbox_session_threads(single: usize, workers: u32) -> usize {
    match workers {
        0 | 1 => single.clamp(1, TEXTBOX_THREADS_MAX),
        2 => 2,
        _ => 1,
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

/// A sysctl of type int (macOS), None when it is missing or not positive.
#[cfg(target_os = "macos")]
fn sysctl_count(name: &std::ffi::CStr) -> Option<usize> {
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
}

/// Performance cores of a hybrid CPU, when the OS reports them (macOS
/// `hw.perflevel0.physicalcpu`; None on a CPU of one kind or elsewhere).
pub fn performance_core_count() -> Option<usize> {
    #[cfg(target_os = "macos")]
    {
        // perflevel1 exists only on a CPU with a second (efficiency) level.
        sysctl_count(c"hw.perflevel1.physicalcpu")?;
        sysctl_count(c"hw.perflevel0.physicalcpu")
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Cores without their SMT siblings on every OS: [`physical_cpu_count`] on Linux,
/// `hw.physicalcpu` on macOS, the processor-core records on Windows (logical CPUs
/// where none of these answers). The planner's [`Host::physical_cores`] is this: a
/// text-box recognizer's pool takes one worker a core, and past the cores a worker adds
/// CPU time a block, not blocks. The thread counts of the libtorch and ONNX recognizers
/// keep [`physical_cpu_count`] (logical CPUs outside Linux), which they were tuned on.
pub fn core_count() -> usize {
    #[cfg(target_os = "macos")]
    if let Some(n) = sysctl_count(c"hw.physicalcpu") {
        return n.min(logical_cpu_count());
    }
    #[cfg(windows)]
    if let Some(n) = windows_core_count() {
        return n.min(logical_cpu_count());
    }
    physical_cpu_count()
}

/// Processor cores as Windows lists them (`GetLogicalProcessorInformationEx`, one
/// `RelationProcessorCore` record a core, whatever its SMT threads).
#[cfg(windows)]
fn windows_core_count() -> Option<usize> {
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore,
    };
    let mut len: u32 = 0;
    // SAFETY: a null buffer of length 0 only asks for the length needed.
    unsafe {
        GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len)
    };
    if len == 0 {
        return None;
    }
    // u64 words: the records hold pointer-aligned fields.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: the buffer holds at least `len` bytes, and `len` says so.
    let ok = unsafe {
        GetLogicalProcessorInformationEx(RelationProcessorCore, buf.as_mut_ptr().cast(), &mut len)
    };
    if ok == 0 {
        return None;
    }
    // Each record starts with its relationship (i32) and its size in bytes (u32).
    let bytes: &[u8] =
        // SAFETY: a view of the u64 buffer's own bytes, of which `len` were written.
        unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), (len as usize).min(buf.len() * 8)) };
    let mut cores = 0;
    let mut at = 0;
    while at + 8 <= bytes.len() {
        let word = |i: usize| u32::from_ne_bytes(bytes[i..i + 4].try_into().unwrap_or_default());
        let (relation, size) = (word(at) as i32, word(at + 4) as usize);
        if size == 0 {
            break;
        }
        if relation == RelationProcessorCore {
            cores += 1;
        }
        at += size;
    }
    (cores > 0).then_some(cores)
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
    /// Cores without their SMT siblings ([`core_count`]).
    pub physical_cores: usize,
    /// Concurrent jobs sharing the host (a text-box engine takes its part of one share).
    pub jobs: usize,
    /// The engine's GPU is of the fast class (`precision::bf16_native`).
    pub fast_gpu: bool,
}

impl Host {
    /// Physical cores of this job ([`core_share`]).
    pub fn share(&self) -> usize {
        core_share(self.physical_cores, self.jobs)
    }
}

/// Whether a stage is a text-box recognizer's engine stage on the CPU: a pooled stage of
/// workers over one shared session rather than a device-bound one.
pub fn pooled_engine(stage: &StagePlan) -> bool {
    stage.key == STAGE_ENGINE && !stage.device_bound && !stage.device.starts_with("gpu")
}

/// The stages of a road with their widths and capacities for this run. `engine_device`
/// is where the recognizer runs, `detect_device` where the block detector does (`cpu`,
/// or the GPU it loaded on: AnimeText on the pack). A GPU detect stage is a small pool
/// of workers over the one detector ([`DETECT_GPU_WORKERS`], at most
/// [`DETECT_GPU_WORKERS_MAX`]), not device-bound; on the engine's own GPU its page adds
/// to the engine's period. The line road's detect stage also reads, so it stays on the
/// CPU whatever `detect_device` says.
pub fn plan(
    engine: &str,
    road: Road,
    engine_device: &str,
    detect_device: &str,
    pools: &PoolsSpec,
    host: &Host,
) -> Vec<StagePlan> {
    let gpu = engine_device.starts_with("gpu");
    let detect_gpu = road == Road::Reconciled && detect_device.starts_with("gpu");
    let budget = host.budget;
    // A text-box recognizer on the CPU: its engine stage is pooled, not device-bound.
    let textbox = if gpu {
        None
    } else {
        textbox_cpu_seconds(engine)
    };
    let declared: Vec<(&'static str, &'static str, String, bool, f64)> = match road {
        Road::Line => vec![
            (STAGE_DETECT, "detect + CTC read", "cpu".into(), false, 0.19),
            (STAGE_LAYOUT, "layout + dump", "cpu".into(), false, 0.011),
        ],
        Road::Reconciled => vec![
            (
                STAGE_DETECT,
                "detect + CTC read",
                if detect_gpu {
                    detect_device.to_string()
                } else {
                    "cpu".into()
                },
                false,
                0.225,
            ),
            (
                STAGE_ENGINE,
                "engine read + reconcile",
                engine_device.to_string(),
                textbox.is_none(),
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
    // The engine stage's default width (2 only for a cheap page on a fast GPU of a large
    // host, hayai-nova or a text-box recognizer: its device time per page is small next
    // to its host side).
    let engine_threads = |cost: f64| {
        if gpu && host.fast_gpu && host.physical_cores >= FAST_GPU_MIN_CORES && cost < 0.3 {
            FAST_GPU_ENGINE_THREADS
        } else {
            1
        }
    };
    let share = host.share();
    // A text-box engine stage's default width: its part of the cores.
    // Beside a GPU detect stage the CPU pool gives up only the detect stage's host side.
    let detect_cost = if detect_gpu {
        DETECT_GPU_HOST_SECONDS
    } else {
        DETECT_CPU_SECONDS
    };
    let textbox_default = textbox.map(|cost| textbox_engine_workers(cost, detect_cost, share));
    // Its width: the explicit one, else the default.
    let textbox_width = textbox_default.map(|w| {
        pools
            .stage_workers
            .get(STAGE_ENGINE)
            .copied()
            .unwrap_or(w)
            .clamp(1, MAX_POOLED_WORKERS)
    });
    // The pace: the device-bound stage's period, or the text-box engine pool's. A GPU
    // detect stage on the engine's own GPU shares its time: the page's detection adds
    // to the engine's period.
    let shared_gpu = if detect_gpu && gpu && detect_device == engine_device {
        DETECT_GPU_SECONDS
    } else {
        0.0
    };
    let bound = match (textbox, textbox_width) {
        (Some(cost), Some(w)) => cost / f64::from(w),
        _ => declared
            .iter()
            .zip(&costs)
            .filter(|(d, _)| d.3)
            .map(|(_, c)| *c / f64::from(engine_threads(*c)) + shared_gpu)
            .fold(0.0f64, f64::max),
    };
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
            let detect_on_gpu = d.0 == STAGE_DETECT && detect_gpu;
            let derived = if d.3 {
                engine_threads(cost)
            } else if detect_on_gpu {
                DETECT_GPU_WORKERS
            } else if d.2.starts_with("gpu") {
                1
            } else if let (STAGE_ENGINE, Some(w)) = (d.0, textbox_width) {
                w
            } else if let (STAGE_DETECT, Some(w)) = (d.0, textbox_default) {
                // Beside a text-box engine the detect stage takes the cores its default
                // width leaves. From the default, not an explicit width: each stage's
                // derived width stands alone, so a benchmark's winner pins only the
                // widths that differ from them and the next session runs what it ran.
                textbox_detect_workers(w, share).clamp(1, ceiling)
            } else if pace > 0.0 {
                ((STAGE_WIDTH_HEADROOM * cost / pace).ceil() as u32).clamp(1, ceiling)
            } else {
                ceiling
            };
            let explicit = pools.stage_workers.get(d.0).copied();
            let max = if d.3 {
                if gpu { MAX_ENGINE_THREADS } else { 1 }
            } else if detect_on_gpu {
                DETECT_GPU_WORKERS_MAX
            } else {
                MAX_POOLED_WORKERS
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
        // The queue feeding the engine stage keeps its load window whether that stage
        // is device-bound or a text-box recognizer's pool.
        if out
            .get(i + 1)
            .is_some_and(|n| n.device_bound || n.key == STAGE_ENGINE)
        {
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
/// (bounded by the budget: each engine thread needs a host thread), a libtorch one on the
/// CPU 1; a text-box recognizer's pooled engine stage up to the job's physical cores
/// (past them a worker adds CPU time a block, not blocks); a GPU detect stage up to
/// [`DETECT_GPU_WORKERS_MAX`] (and the budget). Never below the width the plan already
/// gave a stage.
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
            let ceiling = if pooled_engine(s) {
                host.share() as u32
            } else if s.key == STAGE_DETECT && s.device.starts_with("gpu") {
                host.budget.clamp(1, DETECT_GPU_WORKERS_MAX)
            } else if s.device_bound {
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
            jobs: 1,
            fast_gpu: false,
        }
    }

    #[test]
    fn derived_widths_match_the_measured_ones() {
        let none = PoolsSpec::default();
        // hayai-nova on a GPU: detect 3 (25.7% idle at 1, 0.8% at 3), engine 1, post 1.
        let p = plan(
            "hayai-nova",
            Road::Reconciled,
            "gpu:0",
            "cpu",
            &none,
            &host(3),
        );
        assert_eq!(widths(&p), vec![(3, 4), (1, 1), (1, 1)]);
        assert_eq!(
            graph_line(&p),
            "detect (cpu x3, queue 4) -> engine (gpu:0 x1, queue 1) -> post (cpu x1, queue 1)"
        );
        // paddle-manga: the card is the bottleneck, one detect worker is enough.
        let p = plan(
            "paddle-manga",
            Road::Reconciled,
            "gpu:0",
            "cpu",
            &none,
            &host(6),
        );
        assert_eq!(widths(&p), vec![(1, 4), (1, 1), (1, 1)]);
        // A CPU engine is slow enough that one detect worker keeps it fed.
        let p = plan(
            "hayai-nova",
            Road::Reconciled,
            "cpu",
            "cpu",
            &none,
            &host(6),
        );
        assert_eq!(widths(&p), vec![(1, 4), (1, 1), (1, 1)]);
        let p = plan(
            "paddle-manga",
            Road::Reconciled,
            "cpu",
            "cpu",
            &none,
            &host(6),
        );
        assert_eq!(widths(&p), vec![(1, 4), (1, 1), (1, 1)]);
        // ppocr-manga: detect takes the ceiling (0.5.2's 4), the layout one worker.
        let p = plan("ppocr-manga", Road::Line, "cpu", "cpu", &none, &host(7));
        assert_eq!(widths(&p), vec![(4, 4), (1, 1)]);
        let p = plan("ppocr-manga", Road::Line, "cpu", "cpu", &none, &host(1));
        assert_eq!(widths(&p), vec![(1, 1), (1, 1)]);
    }

    #[test]
    fn host_shape_defaults() {
        let none = PoolsSpec::default();
        let line = |logical: usize, physical: usize, fast: bool, engine: &str| {
            let h = Host {
                budget: host_worker_budget(logical, 1),
                physical_cores: physical,
                jobs: 1,
                fast_gpu: fast,
            };
            graph_line(&plan(engine, Road::Reconciled, "gpu:0", "cpu", &none, &h))
        };
        // 8-core/16-thread hosts (5800X + RTX 4060, 5800X + RDNA2): detect 3, was 1.
        assert!(
            line(16, 8, true, "hayai-nova")
                .starts_with("detect (cpu x3, queue 4) -> engine (gpu:0 x1")
        );
        assert!(
            line(16, 8, false, "hayai-nova")
                .starts_with("detect (cpu x3, queue 4) -> engine (gpu:0 x1")
        );
        // 6-core/12-thread (Ryzen 5500): detect 3.
        assert!(line(12, 6, false, "hayai-nova").starts_with("detect (cpu x3"));
        // 16-core desktop 9070 XT, 24-core RTX 4090 host: engine 2, detect 6.
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
        let p = plan(
            "hayai-nova",
            Road::Reconciled,
            "gpu:0",
            "cpu",
            &pools,
            &host(3),
        );
        assert_eq!(widths(&p), vec![(6, 6), (8, 8), (1, 8)]);
        let p = plan(
            "hayai-nova",
            Road::Reconciled,
            "cpu",
            "cpu",
            &pools,
            &host(3),
        );
        assert_eq!(p[1].workers, 1, "the CPU engine stage is one worker");
        assert_eq!(p[0].workers, 6, "an explicit width still wins on the CPU");
        pools.stage_workers.insert("detect".into(), 0);
        let p = plan(
            "hayai-nova",
            Road::Reconciled,
            "cpu",
            "cpu",
            &pools,
            &host(3),
        );
        assert_eq!(p[0].workers, 1);
    }

    #[test]
    fn width_ceilings_follow_the_planner() {
        let none = PoolsSpec::default();
        let h = Host {
            budget: host_worker_budget(16, 1),
            physical_cores: 8,
            jobs: 1,
            fast_gpu: false,
        };
        let p = plan("hayai-nova", Road::Reconciled, "gpu:0", "cpu", &none, &h);
        let c = width_ceilings(&p, &h);
        assert_eq!((c["detect"], c["engine"], c["post"]), (6, 6, 6));
        let p = plan("hayai-nova", Road::Reconciled, "cpu", "cpu", &none, &h);
        assert_eq!(
            width_ceilings(&p, &h)["engine"],
            1,
            "a CPU engine is never widened"
        );
        let big = Host {
            budget: host_worker_budget(48, 1),
            physical_cores: 24,
            jobs: 1,
            fast_gpu: true,
        };
        let p = plan("hayai-nova", Road::Reconciled, "gpu:0", "cpu", &none, &big);
        let c = width_ceilings(&p, &big);
        assert_eq!(
            (c["detect"], c["engine"]),
            (CPU_WORKERS_MAX, MAX_ENGINE_THREADS)
        );
        let p = plan("ppocr-manga", Road::Line, "cpu", "cpu", &none, &big);
        assert_eq!(width_ceilings(&p, &big)["detect"], LINE_WORKERS_MAX);
        // a small host: never below what the plan already runs
        let small = Host {
            budget: 1,
            physical_cores: 2,
            jobs: 1,
            fast_gpu: false,
        };
        let mut pools = PoolsSpec::default();
        pools.stage_workers.insert("detect".into(), 3);
        let p = plan(
            "hayai-nova",
            Road::Reconciled,
            "gpu:0",
            "cpu",
            &pools,
            &small,
        );
        assert_eq!(width_ceilings(&p, &small)["detect"], 3);
    }

    /// An SMT host of `physical` cores running `jobs` jobs.
    fn smt(physical: usize, jobs: usize) -> Host {
        Host {
            budget: host_worker_budget(2 * physical, jobs),
            physical_cores: physical,
            jobs,
            fast_gpu: false,
        }
    }

    #[test]
    fn textbox_engines_take_the_cpu_in_a_pool() {
        let none = PoolsSpec::default();
        // (physical cores, jobs, engine workers, detect workers)
        type Hosts = [(usize, usize, u32, u32); 8];
        let cases: [(&str, Hosts); 2] = [
            (
                "nar-small",
                [
                    (4, 1, 3, 1),
                    (4, 2, 1, 1),
                    (8, 1, 5, 1),
                    (8, 2, 3, 1),
                    (16, 1, 10, 2),
                    (16, 2, 5, 1),
                    (24, 1, 15, 4),
                    (24, 2, 8, 1),
                ],
            ),
            (
                "charspot-small",
                [
                    (4, 1, 1, 1),
                    (4, 2, 1, 1),
                    (8, 1, 2, 2),
                    (8, 2, 1, 1),
                    (16, 1, 5, 5),
                    (16, 2, 2, 2),
                    (24, 1, 7, 8),
                    (24, 2, 4, 3),
                ],
            ),
        ];
        for (engine, hosts) in cases {
            for (physical, jobs, engine_workers, detect_workers) in hosts {
                let h = smt(physical, jobs);
                let p = plan(engine, Road::Reconciled, "cpu", "cpu", &none, &h);
                let at = format!("{engine} on {physical} cores, {jobs} job(s)");
                assert_eq!(
                    widths(&p),
                    vec![
                        (detect_workers, detect_workers.max(LOAD_WINDOW_SLOTS)),
                        (engine_workers, engine_workers),
                        (1, 1)
                    ],
                    "{at}"
                );
                assert!(!p[1].device_bound, "{at}: a pool, not device-bound");
                assert!(pooled_engine(&p[1]), "{at}");
                let c = width_ceilings(&p, &h);
                assert_eq!(c["engine"], (physical / jobs) as u32, "{at}");
                assert_eq!(c["detect"], h.budget.clamp(1, CPU_WORKERS_MAX), "{at}");
                // One thread a worker once the pool is wide; a lone worker keeps the
                // threads it had before the stage was pooled.
                let single = cpu_engine_threads(physical, None, jobs);
                let threads = textbox_session_threads(single, engine_workers);
                let want = match engine_workers {
                    1 => ((physical - 1) / jobs).min(TEXTBOX_THREADS_MAX),
                    2 => 2,
                    _ => 1,
                };
                assert_eq!(threads, want, "{at}");
            }
        }
        assert_eq!(
            graph_line(&plan(
                "nar-small",
                Road::Reconciled,
                "cpu",
                "cpu",
                &none,
                &smt(24, 1)
            )),
            "detect (cpu x4, queue 4) -> engine (cpu x15, queue 15) -> post (cpu x1, queue 1)"
        );
        assert_eq!(
            graph_line(&plan(
                "nar-small",
                Road::Reconciled,
                "cpu",
                "cpu",
                &none,
                &smt(16, 1)
            )),
            "detect (cpu x2, queue 4) -> engine (cpu x10, queue 10) -> post (cpu x1, queue 1)"
        );
    }

    #[test]
    fn textbox_engine_widths_when_set() {
        let big = smt(24, 1);
        let mut pools = PoolsSpec::default();
        // An explicit engine width is honoured, and detect keeps the width the default
        // engine width gives it: a benchmark that widened only the engine pins only
        // the engine, and the next session runs the detect width it measured.
        let auto = plan("nar-small", Road::Reconciled, "cpu", "cpu", &pools, &big);
        for engine in [12, 16, 1] {
            pools.stage_workers.insert("engine".into(), engine);
            let p = plan("nar-small", Road::Reconciled, "cpu", "cpu", &pools, &big);
            assert_eq!((p[0].workers, p[1].workers), (auto[0].workers, engine));
        }
        let single = cpu_engine_threads(24, None, 1);
        assert_eq!(textbox_session_threads(single, 12), 1);
        assert_eq!(textbox_session_threads(single, 2), 2);
        // One worker: today's single 8-thread session.
        assert_eq!(textbox_session_threads(single, 1), TEXTBOX_THREADS_MAX);
        // The lone worker's threads: all cores but one, or a hybrid CPU's performance
        // cores, split across jobs, as before the stage was pooled.
        assert_eq!(
            textbox_session_threads(cpu_engine_threads(4, None, 1), 1),
            3
        );
        assert_eq!(
            textbox_session_threads(cpu_engine_threads(2, None, 1), 1),
            1
        );
        assert_eq!(
            textbox_session_threads(cpu_engine_threads(8, Some(4), 2), 1),
            2
        );
        assert_eq!(
            textbox_session_threads(cpu_engine_threads(4, None, 2), 1),
            1
        );
        pools.stage_workers.insert("engine".into(), 0);
        let p = plan("nar-small", Road::Reconciled, "cpu", "cpu", &pools, &big);
        assert_eq!(p[1].workers, 1, "0 runs as 1");
        // Past the cores up to the pooled maximum; the benchmark may go as far.
        pools.stage_workers.insert("engine".into(), 30);
        let p = plan(
            "charspot-small",
            Road::Reconciled,
            "cpu",
            "cpu",
            &pools,
            &big,
        );
        assert_eq!(p[1].workers, 30);
        assert_eq!(width_ceilings(&p, &big)["engine"], 30);
        pools.stage_workers.insert("engine".into(), 500);
        let p = plan(
            "charspot-small",
            Road::Reconciled,
            "cpu",
            "cpu",
            &pools,
            &big,
        );
        assert_eq!(p[1].workers, MAX_POOLED_WORKERS);
        // An explicit detect width still wins beside it.
        pools.stage_workers.insert("detect".into(), 3);
        pools.stage_workers.insert("engine".into(), 16);
        let p = plan("nar-small", Road::Reconciled, "cpu", "cpu", &pools, &big);
        assert_eq!(widths(&p), vec![(3, 4), (16, 16), (1, 1)]);
    }

    /// Cores count once whatever their SMT threads (Linux reads the topology the same
    /// way the libtorch thread counts do).
    #[test]
    fn core_count_leaves_out_smt_threads() {
        let cores = core_count();
        assert!(cores >= 1 && cores <= logical_cpu_count());
        #[cfg(target_os = "linux")]
        assert_eq!(cores, physical_cpu_count());
    }

    #[test]
    fn libtorch_engines_keep_their_plans_on_a_big_host() {
        let none = PoolsSpec::default();
        let big = smt(24, 1);
        for engine in ["hayai-nova", "paddle-manga"] {
            let p = plan(engine, Road::Reconciled, "cpu", "cpu", &none, &big);
            assert_eq!(widths(&p), vec![(1, 4), (1, 1), (1, 1)], "{engine}");
            assert!(p[1].device_bound && !pooled_engine(&p[1]));
            assert_eq!(width_ceilings(&p, &big)["engine"], 1, "{engine}");
            let mut pools = PoolsSpec::default();
            pools.stage_workers.insert("engine".into(), 12);
            let p = plan(engine, Road::Reconciled, "cpu", "cpu", &pools, &big);
            assert_eq!(
                p[1].workers, 1,
                "{engine}: the CPU engine stage is one worker"
            );
        }
        let fast = Host {
            fast_gpu: true,
            ..big
        };
        let p = plan("hayai-nova", Road::Reconciled, "gpu:0", "cpu", &none, &fast);
        assert_eq!(
            graph_line(&p),
            "detect (cpu x6, queue 6) -> engine (gpu:0 x2, queue 2) -> post (cpu x1, queue 1)"
        );
    }

    #[test]
    fn a_gpu_text_box_stage_uses_its_gpu_figure() {
        let h = Host {
            budget: 8,
            physical_cores: 24,
            jobs: 1,
            fast_gpu: true,
        };
        let p = plan(
            "nar-small",
            Road::Reconciled,
            "gpu:0",
            "cpu",
            &PoolsSpec::default(),
            &h,
        );
        let e = p.iter().find(|s| s.key == STAGE_ENGINE).unwrap();
        assert!(e.device_bound);
        assert_eq!((e.seconds, e.workers), (0.12, 2));
        let d = p.iter().find(|s| s.key == STAGE_DETECT).unwrap();
        assert_eq!(d.workers, 8); // ceil(2 * 0.225 / (0.12 / 2)) = 8, the ceiling
        let cpu = plan(
            "nar-small",
            Road::Reconciled,
            "cpu",
            "cpu",
            &PoolsSpec::default(),
            &h,
        );
        assert!(
            !cpu.iter()
                .find(|s| s.key == STAGE_ENGINE)
                .unwrap()
                .device_bound
        );
        // charspot-small's figure, and none for any other engine.
        assert_eq!(textbox_gpu_seconds("charspot-small"), Some(0.1));
        assert_eq!(textbox_gpu_seconds("hayai-nova"), None);
        let c = plan(
            "charspot-small",
            Road::Reconciled,
            "gpu:0",
            "cpu",
            &PoolsSpec::default(),
            &h,
        );
        assert_eq!(c[1].seconds, 0.1);
    }

    fn big() -> Host {
        Host {
            budget: 8,
            physical_cores: 24,
            jobs: 1,
            fast_gpu: true,
        }
    }

    #[test]
    fn a_gpu_detect_stage_beside_a_gpu_engine() {
        let p = plan(
            "nar-small",
            Road::Reconciled,
            "gpu:0",
            "gpu:0",
            &PoolsSpec::default(),
            &big(),
        );
        let d = p.iter().find(|s| s.key == STAGE_DETECT).unwrap();
        assert_eq!(
            (d.device.as_str(), d.device_bound, d.workers, d.seconds),
            ("gpu:0", false, 2, 0.045)
        );
    }

    #[test]
    fn a_gpu_detect_stage_leaves_the_cores_to_a_cpu_pool() {
        assert_eq!(textbox_engine_workers(1.5, DETECT_CPU_SECONDS, 24), 15);
        assert_eq!(textbox_engine_workers(1.5, DETECT_GPU_HOST_SECONDS, 24), 23);
        let p = plan(
            "nar-small",
            Road::Reconciled,
            "cpu",
            "gpu:0",
            &PoolsSpec::default(),
            &big(),
        );
        assert_eq!(
            p.iter().find(|s| s.key == STAGE_ENGINE).unwrap().workers,
            23
        );
        assert_eq!(p.iter().find(|s| s.key == STAGE_DETECT).unwrap().workers, 2);
    }

    #[test]
    fn a_gpu_detect_stage_widens_to_four_at_most() {
        let p = plan(
            "nar-small",
            Road::Reconciled,
            "gpu:0",
            "gpu:0",
            &PoolsSpec::default(),
            &big(),
        );
        assert_eq!(width_ceilings(&p, &big())["detect"], 4);
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
