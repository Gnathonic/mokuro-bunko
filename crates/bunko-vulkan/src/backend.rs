//! The contract every runtime of the Vulkan pack implements (docs/rust-port/VULKAN.md §2).
//!
//! The pack carries more than one runtime for the same engines (ncnn Vulkan, ONNX Runtime
//! WebGPU). They are interchangeable: each reads the same crops into the same fp32 texts
//! (the self-test proves it per device), so the host may pick either per (device,
//! engine) and fall back to the other when one fails on a machine.

use std::path::Path;

use bunko_torch::abi::LoadOptions;
use bunko_vlm::{RecognizerInfo, Rgb};
use serde::Serialize;

/// ncnn Vulkan (`bunko-ncnn`).
pub const NCNN: &str = "ncnn";
/// ONNX Runtime's WebGPU execution provider over Dawn (`bunko-vlm`'s GPU-resident hayai).
pub const WEBGPU: &str = "webgpu";
/// Every runtime this pack may carry, in the default order of preference.
pub const ALL: &[&str] = &[NCNN, WEBGPU];

/// The engines each runtime of the pack reads, with the graph files it loads for each
/// from its directory (`<models>/vulkan/<engine>/<runtime>/`): the release description
/// (bunko-ocr `vulkan_models.json`) must list exactly these (a test holds them
/// together), and the adapters refuse a directory that lacks one.
pub fn model_files(runtime: &str, engine: &str) -> Option<Vec<String>> {
    // ncnn graphs format 2 (tools/ncnn_export): prefill and step share decoder.bin.
    const NCNN_COMMON: [&str; 6] = [
        "vision.param",
        "vision.bin",
        "prefill.param",
        "step.param",
        "decoder.bin",
        "ncnn.json",
    ];
    let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match (runtime, engine) {
        (NCNN, "hayai-nova") => Some(own(&[&NCNN_COMMON[..], &["token_embeddings.f32"]].concat())),
        (NCNN, "paddle-manga") => Some(own(&[
            &NCNN_COMMON[..],
            &["embeddings.f32", "pos_table.f32"],
        ]
        .concat())),
        (WEBGPU, "hayai-nova") => Some(own(&[
            bunko_vlm::HayaiGpuGraphs::VISION,
            bunko_vlm::HayaiGpuGraphs::DECODER,
        ])),
        (WEBGPU, "paddle-manga") => Some(bunko_vlm::paddle::webgpu_files()),
        _ => None,
    }
}

/// The files of [`model_files`] missing from `dir`.
pub fn missing_files(runtime: &str, engine: &str, dir: &Path) -> Vec<String> {
    model_files(runtime, engine)
        .unwrap_or_default()
        .into_iter()
        .filter(|f| !dir.join(f).is_file())
        .collect()
}

/// `VkPhysicalDeviceType`, as the catalog reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    Discrete,
    Integrated,
    Virtual,
    /// A software rasterizer (llvmpipe, SwiftShader): never offered.
    Cpu,
    Other,
}

/// One GPU the Vulkan loader reports ([`crate::probe`]): the catalog every runtime maps
/// its own device numbering onto.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VkDevice {
    /// Position in `vkEnumeratePhysicalDevices`.
    pub index: usize,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub kind: DeviceKind,
    /// `radv Mesa 25.2.4`, `NVIDIA 580.95.05`, ... (driver name and info).
    pub driver: String,
    /// The device's Vulkan version, `1.4.318`.
    pub api_version: String,
    /// The largest device-local heap, MiB.
    pub vram_mb: u64,
    /// `VkPhysicalDeviceIDProperties::deviceUUID`, hex: stable across runs.
    pub uuid: String,
    /// `0000:03:00.0` when the driver reports its PCI address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci: Option<String>,
}

impl VkDevice {
    /// `1002:67df`.
    pub fn pci_id(&self) -> String {
        format!("{:04x}:{:04x}", self.vendor_id, self.device_id)
    }
}

/// What a runtime gets to load an engine.
pub struct LoadRequest<'a> {
    /// `hayai-nova` or `paddle-manga`.
    pub engine: &'a str,
    /// This runtime's graphs of the engine (`<models>/vulkan/<engine>/<runtime>/`).
    pub model_dir: &'a Path,
    pub device: &'a VkDevice,
    /// Host files (tokenizer, position table, embeddings) and the patch budget.
    pub opts: &'a LoadOptions,
    /// What the recognizer reports about itself (engine facts, fp32, device).
    pub info: RecognizerInfo,
}

/// A loaded recognizer of one runtime.
pub trait VkRecognizer: Send + Sync {
    /// One text per crop (decoded, Python `strip()`ed), in input order. `caps[i]` caps
    /// crop `i`'s generated tokens (engines with token caps only).
    fn read_crops(&self, crops: &[&Rgb], caps: Option<&[u32]>) -> Result<Vec<String>, String>;
    /// Crops per generation batch.
    fn batch(&self) -> usize;
}

/// One runtime of the pack.
pub trait VkBackend: Send + Sync {
    /// [`NCNN`] or [`WEBGPU`].
    fn id(&self) -> &'static str;
    /// The runtime and its version (`ncnn 20260526`, `ONNX Runtime 1.28.0 WebGPU`): part of
    /// the key the host caches self-test verdicts under.
    fn version(&self) -> String;
    /// The engines it has a loader for.
    fn engines(&self) -> &'static [&'static str];
    /// The catalog entries it can run on (indices into the catalog).
    fn serves(&self, catalog: &[VkDevice]) -> Vec<usize>;
    /// Load `req.engine` on `req.device` in fp32.
    fn load(&self, req: &LoadRequest<'_>) -> Result<Box<dyn VkRecognizer>, String>;
}
