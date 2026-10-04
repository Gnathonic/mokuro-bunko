//! The Rust API the C ABI wraps: initialise, list devices, load a recognizer, read crops.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::Instant;

use bunko_vlm::Rgb;

use crate::TorchError;
use crate::abi::{BT_ABI_VERSION, DevicesReport, InitConfig, LoadInfo, LoadOptions};
use crate::aoti::{Placement, TPrec, set_threads};
use crate::hayai::{HayaiFiles, PACKAGE_BUDGET, TorchHayai};
use crate::package::{PackageSet, default_weight_files};
use crate::paddle::{PaddleFiles, TorchPaddle};

/// The libtorch this library was built against, e.g. `2.13.0+rocm7.1`.
pub fn torch_version() -> &'static str {
    env!("BUNKO_TORCH_LIBTORCH_VERSION")
}

/// Loads the GPU half of libtorch (when the pack has one). Idempotent; a missing or
/// broken GPU half is not an error (the CPU still works): [`devices`] reports why.
pub fn init(cfg: &InitConfig) -> Result<(), TorchError> {
    let dir = cfg.lib_dir.as_deref().map(Path::new);
    register_exit_hook();
    // The ROCm runtime reads these when it starts (a no-op when the loader already did,
    // or for a non-ROCm pack on a host without AMD GPUs).
    if let Some(pack) = dir.and_then(Path::parent)
        && !cfg.cpu_only
        && dir.is_some_and(|d| d.join("libtorch_hip.so").exists())
    {
        crate::abi::prepare_rocm_env(pack, &cfg.gpu_archs);
    }
    let r = crate::gpu::init(dir, cfg.cpu_only);
    let _ = INIT_WARNING.set(r.err());
    Ok(())
}

static INIT_WARNING: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

// ---------------------------------------------------------------- process exit
//
// The server joins its OCR sessions and frees their recognizers before `main` returns
// (bounded). Should the process exit with a read still running anyway (a stop that
// outlasted that wait, an embedder that does not wait), libtorch's static destructors
// would run under that read and the compiled model's next kernel fail noisily
// ("Error: aoti_torch_cpu_... API call failed"). An `atexit` handler, registered after
// libtorch's (so it runs first), stops new reads, makes running ones stop at their
// next decode step, and waits for them and for running frees (bounded); a recognizer
// freed after that is leaked to the exit ([`free_handle`]).

static SHUTTING_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static IN_FLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Longest the exit waits for running reads (a CPU vision batch takes seconds).
const EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Whether the process is exiting: generation loops stop at the next step.
pub fn shutting_down() -> bool {
    SHUTTING_DOWN.load(std::sync::atomic::Ordering::Acquire)
}

extern "C" fn on_exit() {
    SHUTTING_DOWN.store(true, std::sync::atomic::Ordering::Release);
    let t0 = Instant::now();
    while IN_FLIGHT.load(std::sync::atomic::Ordering::Acquire) > 0 && t0.elapsed() < EXIT_WAIT {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Registers [`on_exit`] (again). `atexit` handlers run in reverse order of
/// registration, interleaved with the C++ static destructors of every library loaded
/// before: the compiled model libraries are loaded at each `bt_load`, so the hook is
/// registered after each load too, to run before their destructors.
fn register_exit_hook() {
    static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    unsafe extern "C" {
        fn atexit(cb: extern "C" fn()) -> std::ffi::c_int;
    }
    // The C runtime guarantees at least 32 registrations; loads are few.
    if COUNT.fetch_add(1, std::sync::atomic::Ordering::AcqRel) < 24 {
        // SAFETY: registering a plain `extern "C"` function with the C runtime.
        unsafe {
            atexit(on_exit);
        }
    }
}

/// Frees a recognizer -- unless the process is exiting. Then libtorch's and the GPU
/// runtime's static state may already be torn down (HIP's caching allocator aborts the
/// process on "invalid device pointer"; CUDA's crashes), so the handle is left to the
/// exit instead. A free that started first counts as in flight: the exit hook waits for
/// it before libtorch's destructors run.
pub(crate) fn free_handle(h: Box<Handle>) {
    match InFlight::enter() {
        Ok(_running) => drop(h),
        Err(_) => std::mem::forget(h),
    }
}

/// Counts a running read (or free) for the exit hook.
struct InFlight;

impl InFlight {
    fn enter() -> Result<InFlight, TorchError> {
        IN_FLIGHT.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if shutting_down() {
            IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            return Err(TorchError::Run("the process is exiting".into()));
        }
        Ok(InFlight)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// The CPU, then libtorch's GPUs.
pub fn devices() -> DevicesReport {
    let mut warnings = Vec::new();
    let mut devices = vec![crate::gpu::cpu_entry()];
    if crate::gpu::loaded() {
        let n = tch::Cuda::device_count().max(0) as usize;
        if n == 0 {
            warnings.push(format!(
                "{} libtorch is loaded but sees no GPU",
                crate::gpu::backend().unwrap_or("gpu")
            ));
        }
        let (gpus, w) = crate::gpu::probe(n);
        devices.extend(gpus);
        warnings.extend(w);
    } else if let Some(Some(why)) = INIT_WARNING.get() {
        warnings.push(format!("no GPU: {why}"));
    }
    warnings.extend(crate::gpu::notes().iter().cloned());
    DevicesReport {
        abi: BT_ABI_VERSION,
        torch: torch_version().into(),
        gpu_backend: crate::gpu::backend().map(str::to_string),
        devices,
        warnings,
    }
}

enum Engine {
    Hayai(TorchHayai),
    Paddle(TorchPaddle),
}

/// A loaded recognizer. Shared by any number of threads.
pub struct Handle {
    engine: Engine,
    threads: u32,
    info: LoadInfo,
}

thread_local! {
    /// The OpenMP/ATen width this thread was last set to (setting it clears oneDNN's
    /// primitive cache, so only on change).
    static THREADS: Cell<u32> = const { Cell::new(0) };
}

/// A graph: the given path, else `<model_dir>/<name>/` when the installer unpacked it
/// there (its `.unpacked` stamp or AOTInductor's `data/`), else `<model_dir>/<name>.pt2`.
fn graph(model_dir: &Path, given: &Option<String>, name: &str) -> PathBuf {
    if let Some(g) = given {
        return PathBuf::from(g);
    }
    let unpacked = model_dir.join(name);
    let has_data = |d: &Path| {
        d.join("data").is_dir()
            || std::fs::read_dir(d)
                .is_ok_and(|rd| rd.flatten().any(|e| e.path().join("data").is_dir()))
    };
    if unpacked.join(".unpacked").is_file() || (unpacked.is_dir() && has_data(&unpacked)) {
        return unpacked;
    }
    model_dir.join(format!("{name}.pt2"))
}

fn need<'a>(v: &'a Option<String>, what: &str) -> Result<&'a Path, TorchError> {
    v.as_deref()
        .map(Path::new)
        .ok_or_else(|| TorchError::Arg(format!("load options lack `{what}`")))
}

/// Loads `engine` (`hayai-nova` / `paddle-manga`) from the package set in `model_dir`
/// (`{vision,prefill,step}.pt2`, or the paths in `opts`) at `precision` on `device`
/// (`cpu` / `gpu:<n>`).
pub fn load(
    engine: &str,
    model_dir: &Path,
    precision: &str,
    device: &str,
    opts: &LoadOptions,
) -> Result<Handle, TorchError> {
    let t0 = Instant::now();
    let prec = TPrec::parse(precision)?;
    let at = Placement::parse(device)?;
    if engine != "hayai-nova" && engine != "paddle-manga" {
        return Err(TorchError::Arg(format!(
            "unknown engine {engine:?} (hayai-nova, paddle-manga)"
        )));
    }
    let vision = graph(model_dir, &opts.vision, "vision");
    let prefill = graph(model_dir, &opts.prefill, "prefill");
    let step = graph(model_dir, &opts.step, "step");
    let weight_files: Vec<PathBuf> = match &opts.weights {
        Some(w) => w.iter().map(PathBuf::from).collect(),
        None => default_weight_files(model_dir),
    };
    let tokenizer = Path::new(&opts.tokenizer);
    // Loading runs ATen ops too (casts, copies): give it this handle's width.
    set_threads_here(opts.threads);
    // `.pt2` zips: unpacked once into the cache and loaded in place (libtorch would
    // extract them to the temp directory at every load and leave them there on failure).
    let cache = opts
        .cache_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| model_dir.join(".unpacked"));
    let mut unpacked = Vec::new();
    let mut in_place = |p: PathBuf| -> Result<PathBuf, TorchError> {
        if p.is_file() {
            let d = crate::unpack::ensure_unpacked(&p, &cache)?;
            unpacked.push(d.clone());
            Ok(d)
        } else {
            Ok(p)
        }
    };
    let graphs = [in_place(vision)?, in_place(prefill)?, in_place(step)?];
    let pkgs = match PackageSet::load([&graphs[0], &graphs[1], &graphs[2]], &weight_files, at) {
        Ok(p) => p,
        Err(e) => {
            // Do not keep (or reuse) what failed to load.
            for d in &unpacked {
                crate::unpack::discard(d);
            }
            return Err(e);
        }
    };
    register_exit_hook();
    let (io, bound) = (pkgs.io, pkgs.weights.as_ref().map_or(0, |w| w.len()));
    let (engine_, batch) = if engine == "hayai-nova" {
        let files = HayaiFiles {
            pos_table: need(&opts.pos_table, "pos_table")?,
            token_embeddings: need(&opts.embeddings, "embeddings")?,
            tokenizer,
        };
        let budget = opts.patch_budget.map_or(PACKAGE_BUDGET, |b| b as usize);
        let h = TorchHayai::load(pkgs, &files, prec, at, budget)?;
        let b = h.batch();
        (Engine::Hayai(h), b)
    } else {
        let files = PaddleFiles {
            embeddings: opts.embeddings.as_deref().map(Path::new),
            tokenizer,
        };
        let p = TorchPaddle::load(pkgs, &files, prec, at)?;
        let b = p.batch();
        (Engine::Paddle(p), b)
    };
    Ok(Handle {
        engine: engine_,
        threads: opts.threads,
        info: LoadInfo {
            engine: engine.into(),
            precision: prec.as_str().into(),
            device: at.label(),
            batch: batch as u32,
            load_seconds: t0.elapsed().as_secs_f64(),
            torch: torch_version().into(),
            io,
            weights: bound as u32,
        },
    })
}

fn set_threads_here(n: u32) {
    if n > 0 && THREADS.with(Cell::get) != n {
        set_threads(n);
        THREADS.with(|t| t.set(n));
    }
    // The packages' own OpenMP runtime (MSVC's vcomp on Windows) is loaded with them,
    // after the first call above: set it on every read.
    crate::aoti::set_omp_threads(n);
}

impl Handle {
    pub fn info(&self) -> &LoadInfo {
        &self.info
    }

    /// Reads crops; one text per crop (Python `strip()`ed, no NFKC). `caps[i]` caps
    /// crop i's generated tokens (paddle-manga; hayai-nova ignores caps).
    pub fn read(&self, crops: &[&Rgb], caps: Option<&[u32]>) -> Result<Vec<String>, TorchError> {
        if let Some(c) = caps
            && c.len() != crops.len()
        {
            return Err(TorchError::Arg(format!(
                "{} token caps for {} crops",
                c.len(),
                crops.len()
            )));
        }
        if crops.is_empty() {
            return Ok(Vec::new());
        }
        let _running = InFlight::enter()?;
        set_threads_here(self.threads);
        match &self.engine {
            Engine::Hayai(h) => h.read_crops(crops),
            Engine::Paddle(p) => {
                let default;
                let caps = match caps {
                    Some(c) => c,
                    None => {
                        default = vec![bunko_vlm::paddle::DEFAULT_MAX_NEW_TOKENS; crops.len()];
                        &default
                    }
                };
                p.read_crops(crops, caps)
            }
        }
    }
}
