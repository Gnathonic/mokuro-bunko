//! The Rust API the C ABI wraps: initialise, list devices, load a recognizer, read crops.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::Instant;

use bunko_vlm::Rgb;

use crate::TorchError;
use crate::abi::{
    BT_ABI_VERSION, CharsRead, DevicesReport, InitConfig, LoadInfo, LoadOptions, TEXTBOX_IO,
};
use crate::aoti::{Placement, TPrec, set_threads};
use crate::hayai::{HayaiFiles, PACKAGE_BUDGET, TorchHayai};
use crate::package::{GraphSet, PackageSet, default_weight_files};
use crate::paddle::{PaddleFiles, TorchPaddle};
use crate::textbox::{NAR_ROLE, TorchNar};

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
    let mut gpu_off = None;
    if let Some(pack) = dir.and_then(Path::parent)
        && !cfg.cpu_only
        && dir.is_some_and(|d| d.join("libtorch_hip.so").exists())
    {
        // No folder for the runtime's kernel compiles: its first GPU copy would crash
        // the process, so the GPU half stays unloaded and the devices report says why.
        let env = crate::abi::prepare_rocm_env(pack, &cfg.gpu_archs);
        gpu_off = env.gpu_off;
        // What this check worked around (e.g. a moved temporary folder) is reported in
        // every devices report, so it reaches the log and `doctor` even when an older
        // loader did not run the check itself.
        let _ = ENV_WARNINGS.set(env.warnings);
    }
    let r = crate::gpu::init(dir, cfg.cpu_only || gpu_off.is_some());
    let _ = INIT_WARNING.set(gpu_off.or(r.err()));
    // fp32 graphs compute in fp32 on every card (libtorch lets cuDNN use TF32 on Ampere
    // and newer by default), for every engine. Process-wide; a no-op on the CPU.
    crate::aoti::tf32_off();
    Ok(())
}

static INIT_WARNING: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
static ENV_WARNINGS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

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
    if let Some(env) = ENV_WARNINGS.get() {
        warnings.extend(env.iter().cloned());
    }
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
    Nar(TorchNar),
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

/// `.pt2` zips unpacked for one load: once into the cache, then loaded in place
/// (libtorch would extract them to the temp directory at every load and leave them there
/// on failure).
struct Unpacked {
    cache: PathBuf,
    dirs: Vec<PathBuf>,
}

impl Unpacked {
    fn new(model_dir: &Path, opts: &LoadOptions) -> Self {
        let cache = opts
            .cache_dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| model_dir.join(".unpacked"));
        Self {
            cache,
            dirs: Vec::new(),
        }
    }

    /// `p` itself when it is a directory, else its unpacked copy.
    fn in_place(&mut self, p: PathBuf) -> Result<PathBuf, TorchError> {
        if p.is_file() {
            let d = crate::unpack::ensure_unpacked(&p, &self.cache)?;
            self.dirs.push(d.clone());
            Ok(d)
        } else {
            Ok(p)
        }
    }

    /// Do not keep (or reuse) what failed to load.
    fn discard(&self) {
        for d in &self.dirs {
            crate::unpack::discard(d);
        }
    }
}

/// Loads `engine` (`hayai-nova` / `paddle-manga` from the package set in `model_dir`,
/// `{vision,prefill,step}.pt2` or the paths in `opts`; `nar-small` from
/// `<model_dir>/model.pt2` bound to `opts.weights`) at `precision` on `device`
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
    if !matches!(engine, "hayai-nova" | "paddle-manga" | "nar-small") {
        return Err(TorchError::Arg(format!(
            "unknown engine {engine:?} (hayai-nova, paddle-manga, nar-small)"
        )));
    }
    // Loading runs ATen ops too (casts, copies): give it this handle's width.
    set_threads_here(opts.threads);
    let mut unpacked = Unpacked::new(model_dir, opts);
    let (engine_, batch, io, bound) = if engine == "nar-small" {
        let n = load_nar(model_dir, prec, at, opts, &mut unpacked)?;
        let (b, w) = (n.batch(), n.bound());
        (Engine::Nar(n), b, 0, w)
    } else {
        load_recognizer(engine, model_dir, prec, at, opts, &mut unpacked)?
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

/// hayai-nova or paddle-manga: (engine, batch, `bunko.io`, weights bound).
fn load_recognizer(
    engine: &str,
    model_dir: &Path,
    prec: TPrec,
    at: Placement,
    opts: &LoadOptions,
    unpacked: &mut Unpacked,
) -> Result<(Engine, usize, u32, usize), TorchError> {
    let vision = graph(model_dir, &opts.vision, "vision");
    let prefill = graph(model_dir, &opts.prefill, "prefill");
    let step = graph(model_dir, &opts.step, "step");
    let weight_files: Vec<PathBuf> = match &opts.weights {
        Some(w) => w.iter().map(PathBuf::from).collect(),
        None => default_weight_files(model_dir),
    };
    let tokenizer = Path::new(&opts.tokenizer);
    let graphs = [
        unpacked.in_place(vision)?,
        unpacked.in_place(prefill)?,
        unpacked.in_place(step)?,
    ];
    let pkgs = match PackageSet::load([&graphs[0], &graphs[1], &graphs[2]], &weight_files, at) {
        Ok(p) => p,
        Err(e) => {
            unpacked.discard();
            return Err(e);
        }
    };
    register_exit_hook();
    let (io, bound) = (pkgs.io, pkgs.weights.as_ref().map_or(0, |w| w.len()));
    if engine == "hayai-nova" {
        let files = HayaiFiles {
            pos_table: need(&opts.pos_table, "pos_table")?,
            token_embeddings: need(&opts.embeddings, "embeddings")?,
            tokenizer,
        };
        let budget = opts.patch_budget.map_or(PACKAGE_BUDGET, |b| b as usize);
        let h = TorchHayai::load(pkgs, &files, prec, at, budget)?;
        let b = h.batch();
        Ok((Engine::Hayai(h), b, io, bound))
    } else {
        let files = PaddleFiles {
            embeddings: opts.embeddings.as_deref().map(Path::new),
            tokenizer,
        };
        let p = TorchPaddle::load(pkgs, &files, prec, at)?;
        let b = p.batch();
        Ok((Engine::Paddle(p), b, io, bound))
    }
}

/// nar-small: the graph `model` (`<model_dir>/model.pt2`, or the installer's unpacked
/// `model/`), bound to the authors' `model.safetensors` (`opts.weights`, required), the
/// vocabulary in `opts.tokenizer`.
fn load_nar(
    model_dir: &Path,
    prec: TPrec,
    at: Placement,
    opts: &LoadOptions,
    unpacked: &mut Unpacked,
) -> Result<TorchNar, TorchError> {
    if prec != TPrec::Fp32 {
        return Err(TorchError::Arg(format!(
            "nar-small runs at fp32 in this pack, not {}",
            prec.as_str()
        )));
    }
    let weight_files: Vec<PathBuf> = opts.weights.iter().flatten().map(PathBuf::from).collect();
    if weight_files.is_empty() {
        return Err(TorchError::Arg("nar-small needs its weights file".into()));
    }
    let model = unpacked.in_place(graph(model_dir, &None, NAR_ROLE))?;
    let set = match GraphSet::load(&[(NAR_ROLE, &model)], &weight_files, at, TEXTBOX_IO) {
        Ok(s) => s,
        Err(e) => {
            unpacked.discard();
            return Err(e);
        }
    };
    register_exit_hook();
    Ok(
        TorchNar::load(set, Path::new(&opts.tokenizer), at, opts.max_batch)?
            .with_threads(opts.threads),
    )
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

    /// Reads text-box crops: one [`CharsRead`] per crop, in crop order. An empty crop
    /// (zero width or height) reads `""` without a model call. Only the text-box engines
    /// read characters.
    pub fn read_chars(&self, crops: &[&Rgb]) -> Result<Vec<CharsRead>, TorchError> {
        let Engine::Nar(n) = &self.engine else {
            return Err(TorchError::Arg(format!(
                "bt_read_chars: {} reads no characters",
                self.info.engine
            )));
        };
        if crops.is_empty() {
            return Ok(Vec::new());
        }
        let _running = InFlight::enter()?;
        set_threads_here(self.threads);
        n.read_chars(crops)
    }

    /// Reads crops; one text per crop (Python `strip()`ed, no NFKC). `caps[i]` caps
    /// crop i's generated tokens (paddle-manga; hayai-nova and nar-small ignore caps).
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
            Engine::Nar(n) => Ok(n.read_chars(crops)?.into_iter().map(|c| c.text).collect()),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn init_turns_tf32_off() {
        super::init(&crate::abi::InitConfig {
            cpu_only: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(crate::aoti::tf32_state(), 0);
    }
}
