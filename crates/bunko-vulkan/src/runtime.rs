//! The pack's state behind the C ABI: the device catalog, the runtimes built in, and
//! loaded recognizers.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use bunko_torch::abi::{
    BT_ABI_VERSION, DeviceEntry, DevicesReport, InitConfig, LoadInfo, LoadOptions,
};
use bunko_vlm::{Device, Precision, RecognizerInfo, Rgb};

use crate::backend::{DeviceKind, LoadRequest, VkBackend, VkDevice, VkRecognizer};
use crate::{VkError, probe};

pub struct State {
    pub catalog: Vec<VkDevice>,
    pub backends: Vec<Box<dyn VkBackend>>,
    /// Per backend (same order): the catalog entries it serves.
    pub served: Vec<Vec<usize>>,
    pub warnings: Vec<String>,
}

static STATE: OnceLock<State> = OnceLock::new();

/// The runtimes compiled into this library, in the default order of preference.
#[allow(clippy::vec_init_then_push, unused_mut)]
pub fn compiled_backends() -> Vec<Box<dyn VkBackend>> {
    let mut v: Vec<Box<dyn VkBackend>> = Vec::new();
    let only = only();
    #[cfg(feature = "ncnn")]
    if only.as_deref().is_none_or(|o| o == crate::backend::NCNN) {
        v.push(Box::new(crate::ncnn::Ncnn));
    }
    #[cfg(feature = "webgpu")]
    if only.as_deref().is_none_or(|o| o == crate::backend::WEBGPU) {
        v.push(Box::new(crate::webgpu::WebGpu::new()));
    }
    let _ = only;
    v
}

pub fn init(_cfg: &InitConfig) -> Result<(), VkError> {
    STATE.get_or_init(|| {
        let mut warnings = Vec::new();
        let catalog = probe::probe().unwrap_or_else(|e| {
            warnings.push(e);
            Vec::new()
        });
        let backends = compiled_backends();
        if backends.is_empty() {
            warnings.push("this Vulkan pack has no runtime built in".into());
        }
        let served = backends.iter().map(|b| b.serves(&catalog)).collect();
        State {
            catalog,
            backends,
            served,
            warnings,
        }
    });
    Ok(())
}

pub fn state() -> Result<&'static State, VkError> {
    STATE
        .get()
        .ok_or_else(|| VkError::Arg("bt_init was not called".into()))
}

/// See `bv_shutdown`.
pub fn shutdown() {
    let trace = std::env::var_os("BUNKO_VULKAN_TRACE").is_some();
    if trace {
        eprintln!("bv_shutdown: start");
    }
    #[cfg(feature = "webgpu")]
    if STATE
        .get()
        .is_some_and(|s| s.backends.iter().any(|b| b.id() == crate::backend::WEBGPU))
    {
        crate::webgpu::shutdown();
        if trace {
            eprintln!("bv_shutdown: webgpu contexts released");
        }
    }
    #[cfg(feature = "ncnn")]
    if STATE
        .get()
        .is_some_and(|s| s.backends.iter().any(|b| b.id() == crate::backend::NCNN))
    {
        bunko_ncnn::shutdown();
        if trace {
            eprintln!("bv_shutdown: ncnn instance destroyed");
        }
    }
}

/// `BUNKO_VULKAN_ONLY=<runtime>`: start only that runtime (diagnosis: which one a
/// driver problem comes from).
fn only() -> Option<String> {
    std::env::var("BUNKO_VULKAN_ONLY")
        .ok()
        .filter(|v| !v.is_empty())
}

/// The runtimes' names and versions: `ncnn 20260526; ONNX Runtime 1.28.0 WebGPU`.
pub fn versions(s: &State) -> String {
    s.backends
        .iter()
        .map(|b| b.version())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Every catalog GPU at least one runtime serves, as `gpu:<catalog index>`. No CPU
/// entry: the host's CPU recognizers come from its libtorch pack.
pub fn devices() -> Result<DevicesReport, VkError> {
    let s = state()?;
    let devices = s
        .catalog
        .iter()
        .filter(|d| d.kind != DeviceKind::Cpu)
        .filter_map(|d| {
            let backends: Vec<String> = s
                .backends
                .iter()
                .zip(&s.served)
                .filter(|(_, idx)| idx.contains(&d.index))
                .map(|(b, _)| b.id().to_string())
                .collect();
            (!backends.is_empty()).then(|| DeviceEntry {
                id: format!("gpu:{}", d.index),
                kind: "vulkan".into(),
                name: d.name.clone(),
                arch: d.pci_id(),
                vram_mb: Some(d.vram_mb),
                pci: d.pci.clone(),
                formats: vec!["fp32".into()],
                driver: Some(d.driver.clone()),
                uuid: Some(d.uuid.clone()),
                backends,
                ..Default::default()
            })
        })
        .collect();
    let mut warnings = s.warnings.clone();
    for d in s.catalog.iter().filter(|d| d.kind == DeviceKind::Cpu) {
        warnings.push(format!("{} is a software rasterizer: not used", d.name));
    }
    Ok(DevicesReport {
        abi: BT_ABI_VERSION,
        torch: versions(s),
        gpu_backend: Some("vulkan".into()),
        devices,
        warnings,
    })
}

pub struct Handle {
    rec: Box<dyn VkRecognizer>,
    info: LoadInfo,
    runtime: &'static str,
}

/// Test hook (the host's crash-safety tests): `BUNKO_VULKAN_ABORT=<runtime>` aborts the
/// process in that runtime's reads, as a crash in a driver would.
pub const ABORT_ENV: &str = "BUNKO_VULKAN_ABORT";

impl Handle {
    pub fn info(&self) -> &LoadInfo {
        &self.info
    }

    pub fn read(&self, crops: &[&Rgb], caps: Option<&[u32]>) -> Result<Vec<String>, VkError> {
        if std::env::var(ABORT_ENV).is_ok_and(|r| r == self.runtime) {
            eprintln!("{ABORT_ENV}={}: aborting in a read", self.runtime);
            std::process::abort();
        }
        let out = self.rec.read_crops(crops, caps).map_err(VkError::Run)?;
        if out.len() != crops.len() {
            return Err(VkError::Run(format!(
                "{} texts for {} crops",
                out.len(),
                crops.len()
            )));
        }
        Ok(out)
    }
}

/// What the recognizer of `engine` reports about itself on a Vulkan GPU.
pub fn recognizer_info(engine: &str, gpu: u8, budget: u32) -> Result<RecognizerInfo, VkError> {
    use bunko_vlm::{hayai, paddle};
    let device = Device::Gpu(gpu);
    Ok(match engine {
        "hayai-nova" => RecognizerInfo {
            engine: "hayai-nova",
            recognizer: hayai::REPO,
            repos: vec![
                (hayai::REPO, hayai::REVISION),
                (hayai::VISION_REPO, hayai::VISION_REVISION),
            ],
            precision: Precision::Fp32,
            device,
            second_read: false,
            token_caps: false,
            default_max_tokens: hayai::MAX_NEW_TOKENS as u32,
            batch: hayai::BATCH,
            patch_budget: Some(budget),
        },
        "paddle-manga" => RecognizerInfo {
            engine: "paddle-manga",
            recognizer: paddle::LORA_REPO,
            repos: vec![
                (paddle::BASE_REPO, paddle::BASE_REVISION),
                (paddle::LORA_REPO, paddle::LORA_REVISION),
            ],
            precision: Precision::Fp32,
            device,
            second_read: true,
            token_caps: true,
            default_max_tokens: paddle::DEFAULT_MAX_NEW_TOKENS,
            batch: 1,
            patch_budget: None,
        },
        other => return Err(VkError::Arg(format!("unknown engine {other}"))),
    })
}

/// `bt_load`: `device` is `gpu:<catalog index>`; `opts.backend` picks the runtime (the
/// first that serves the device when unset). fp32 only.
pub fn load(
    engine: &str,
    model_dir: &Path,
    precision: &str,
    device: &str,
    opts: &LoadOptions,
) -> Result<Handle, VkError> {
    let s = state()?;
    if precision != "fp32" {
        return Err(VkError::Arg(format!(
            "the Vulkan runtimes compute in fp32 only (asked for {precision})"
        )));
    }
    let index: usize = device
        .strip_prefix("gpu:")
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| VkError::Arg(format!("bad device {device} (want gpu:<n>)")))?;
    let dev = s
        .catalog
        .iter()
        .find(|d| d.index == index)
        .ok_or_else(|| VkError::Arg(format!("no Vulkan device {device}")))?;
    let pick = s
        .backends
        .iter()
        .zip(&s.served)
        .filter(|(b, idx)| {
            idx.contains(&index) && opts.backend.as_deref().is_none_or(|w| w == b.id())
        })
        .map(|(b, _)| b)
        .next()
        .ok_or_else(|| {
            VkError::Load(match &opts.backend {
                Some(b) if !s.backends.iter().any(|x| x.id() == b) => {
                    format!("this Vulkan pack has no {b} runtime")
                }
                Some(b) => format!("{b} cannot run on {} ({device})", dev.name),
                None => format!("no runtime of this pack runs on {} ({device})", dev.name),
            })
        })?;
    if !pick.engines().contains(&engine) {
        return Err(VkError::Load(format!(
            "{} has no {engine} loader",
            pick.id()
        )));
    }
    let missing = crate::backend::missing_files(pick.id(), engine, model_dir);
    if !missing.is_empty() {
        return Err(VkError::Load(format!(
            "{} {engine}: {} lacks {}",
            pick.id(),
            model_dir.display(),
            missing.join(", ")
        )));
    }
    let budget = opts.patch_budget.unwrap_or(512);
    let info = recognizer_info(engine, index.min(255) as u8, budget)?;
    let t0 = Instant::now();
    let rec = pick
        .load(&LoadRequest {
            engine,
            model_dir,
            device: dev,
            opts,
            info,
        })
        .map_err(VkError::Load)?;
    Ok(Handle {
        info: LoadInfo {
            engine: engine.into(),
            precision: "fp32".into(),
            device: device.into(),
            batch: rec.batch() as u32,
            load_seconds: t0.elapsed().as_secs_f64(),
            torch: pick.version(),
            io: 0,
            weights: 0,
        },
        rec,
        runtime: pick.id(),
    })
}
