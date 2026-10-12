//! The WebGPU runtime: ONNX Runtime's WebGPU execution provider over Dawn
//! (`libwebgpu_dawn` in the pack's `lib/`), running `bunko-vlm`'s GPU-resident
//! recognizers: per engine a vision graph and one shared decoder graph that serves the
//! prefill and every decode step (`tools/webgpu_export`); only token ids cross to the host
//! per step.
//!
//! VRAM (fp32, peak over idle, measured): hayai-nova 1.7 GB with a 16-crop vision batch,
//! less with fewer vision rows (picked from the device's VRAM here); paddle-manga up to
//! 5 GB on its biggest crops (weights alone 3.2 GB), so it is refused below
//! [`PADDLE_MIN_VRAM_MB`] and the host falls back to the next runtime or the CPU.
//!
//! Device choice: the EP opens Dawn's high-performance adapter, i.e. the first discrete
//! GPU in the loader's order, else the first integrated one. The runtime serves that
//! device only (a second GPU of a machine is ncnn's alone).

use std::ffi::CStr;
use std::path::{Path, PathBuf};

use bunko_vlm::{
    CropSet, Device, HayaiAssets, HayaiGpuGraphs, HayaiNova, OrtSessionFactory, PaddleAssets,
    PaddleManga, Precision, Provider, Recognizer, Rgb, SessionOptions, Sharing,
};

use crate::backend::{DeviceKind, LoadRequest, VkBackend, VkDevice, VkRecognizer, WEBGPU};

pub struct WebGpu;

/// Release ONNX Runtime's WebGPU contexts (and Dawn's instance) now: idempotent; ONNX
/// Runtime's own cleanup later finds nothing left.
pub fn shutdown() {
    unsafe extern "C" {
        #[cfg_attr(
            target_env = "msvc",
            link_name = "?CleanupWebGpuContexts@webgpu@onnxruntime@@YAXXZ"
        )]
        #[cfg_attr(
            not(target_env = "msvc"),
            link_name = "_ZN11onnxruntime6webgpu21CleanupWebGpuContextsEv"
        )]
        fn ort_cleanup_webgpu_contexts();
    }
    let _g = bunko_vlm::runtime::WebGpuGuard::lock();
    // SAFETY: a plain C++ function without arguments; it locks its own state.
    unsafe { ort_cleanup_webgpu_contexts() };
}

impl WebGpu {
    pub fn new() -> Self {
        let _ = ort::init().with_name("mokuro-bunko-vulkan").commit();
        if let Ok(env) = ort::environment::Environment::current() {
            env.set_log_level(ort::logging::LogLevel::Warning);
        }
        WebGpu
    }
}

/// The linked ONNX Runtime's version.
fn ort_version() -> String {
    // SAFETY: ONNX Runtime's C entry point; the struct and string are static.
    unsafe {
        let base = ort::sys::OrtGetApiBase();
        if base.is_null() {
            return "unknown".into();
        }
        let s = ((*base).GetVersionString)();
        if s.is_null() {
            return "unknown".into();
        }
        CStr::from_ptr(s).to_string_lossy().into_owned()
    }
}

/// The catalog entry Dawn's high-performance adapter is.
pub fn dawn_pick(catalog: &[VkDevice]) -> Option<usize> {
    catalog
        .iter()
        .find(|d| d.kind == DeviceKind::Discrete)
        .or_else(|| catalog.iter().find(|d| d.kind == DeviceKind::Integrated))
        .map(|d| d.index)
}

/// Below this much VRAM paddle-manga is not loaded: its fp32 weights alone are 3.2 GB, and
/// a volume's biggest crops (5000 patches) peak at 5 GB.
pub const PADDLE_MIN_VRAM_MB: u64 = 7_000;

/// hayai-nova's vision rows per run for a device's VRAM: the whole 16-crop batch from
/// 6 GB, 8 from 3 GB, else 4 (rows are independent: the texts do not change).
pub fn hayai_vision_rows(vram_mb: u64) -> usize {
    match vram_mb {
        6_000.. => 16,
        3_000.. => 8,
        _ => 4,
    }
}

struct Rec<R>(R, usize);

impl<R: Recognizer + Send + Sync> VkRecognizer for Rec<R> {
    fn read_crops(&self, crops: &[&Rgb], caps: Option<&[u32]>) -> Result<Vec<String>, String> {
        let lines: Vec<CropSet> = crops.iter().map(|c| CropSet::one((*c).clone())).collect();
        self.0.read(&lines, caps).map_err(|e| e.to_string())
    }

    fn batch(&self) -> usize {
        self.1
    }
}

fn need(p: &Option<String>, what: &str) -> Result<PathBuf, String> {
    p.as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| format!("the WebGPU runtime needs the {what}"))
}

impl VkBackend for WebGpu {
    fn id(&self) -> &'static str {
        WEBGPU
    }

    fn version(&self) -> String {
        format!("ONNX Runtime {} WebGPU", ort_version())
    }

    fn engines(&self) -> &'static [&'static str] {
        &["hayai-nova", "paddle-manga"]
    }

    fn serves(&self, catalog: &[VkDevice]) -> Vec<usize> {
        dawn_pick(catalog).into_iter().collect()
    }

    fn load(&self, req: &LoadRequest<'_>) -> Result<Box<dyn VkRecognizer>, String> {
        let opts = SessionOptions {
            // The EP's context 0 (its own adapter choice, see the module docs).
            device: Device::Gpu(0),
            provider: Provider::WebGpu,
            intra_threads: 4,
            spinning: true,
            sharing: Sharing::Auto,
        };
        match req.engine {
            "hayai-nova" => {
                let gpu =
                    HayaiGpuGraphs::in_dir(req.model_dir, Precision::Fp32).ok_or_else(|| {
                        format!(
                            "no {} / {} in {}",
                            HayaiGpuGraphs::VISION,
                            HayaiGpuGraphs::DECODER,
                            req.model_dir.display()
                        )
                    })?;
                let assets = HayaiAssets {
                    // Unused on a GPU when the GPU graphs are given.
                    vision: gpu.vision.clone(),
                    decoder: gpu.decoder.clone(),
                    pos_table: need(&req.opts.pos_table, "position table")?,
                    token_embeddings: need(&req.opts.embeddings, "token embeddings")?,
                    tokenizer: Path::new(&req.opts.tokenizer).to_path_buf(),
                    gpu: Some(gpu),
                };
                let budget = req.opts.patch_budget.unwrap_or(512) as usize;
                let mut h = HayaiNova::load(&OrtSessionFactory, &assets, &opts, budget)
                    .map_err(|e| e.to_string())?;
                h.set_gpu_vision_rows(hayai_vision_rows(req.device.vram_mb));
                Ok(Box::new(Rec(h, bunko_vlm::hayai::BATCH)))
            }
            "paddle-manga" => {
                if req.device.vram_mb < PADDLE_MIN_VRAM_MB {
                    return Err(format!(
                        "paddle-manga on WebGPU needs {PADDLE_MIN_VRAM_MB} MB of VRAM, {} has {} MB",
                        req.device.name, req.device.vram_mb
                    ));
                }
                if !bunko_vlm::paddle::webgpu_complete(req.model_dir) {
                    return Err(format!(
                        "paddle-manga on WebGPU: {} lacks some of {:?}",
                        req.model_dir.display(),
                        bunko_vlm::paddle::webgpu_files()
                    ));
                }
                let dir = req.model_dir.to_path_buf();
                let assets = PaddleAssets {
                    // Unused on a GPU when the WebGPU graphs are given.
                    vision: dir.clone(),
                    decoder: dir.clone(),
                    embed: need(&req.opts.embeddings, "token embeddings")?,
                    tokenizer: Path::new(&req.opts.tokenizer).to_path_buf(),
                    gpu_dir: Some(dir),
                };
                let p = PaddleManga::load(&OrtSessionFactory, &assets, &opts)
                    .map_err(|e| e.to_string())?;
                Ok(Box::new(Rec(p, bunko_vlm::paddle::BATCH)))
            }
            other => Err(format!("the WebGPU runtime has no {other} recognizer")),
        }
    }
}
