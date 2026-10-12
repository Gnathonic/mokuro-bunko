//! The device catalog a processor reports (`bunko_proto::Device` shape).

use ort::environment::Environment;
#[allow(unused_imports)]
use ort::ep::{self, ExecutionProvider};
use ort::memory::DeviceType;
use serde::{Deserialize, Serialize};

/// One usable device. Serializes exactly like `bunko_proto::Device`, so the
/// processor can forward it without this crate depending on the protocol crate.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// `cpu` or `gpu:<n>`.
    pub id: String,
    pub label: String,
    /// Precisions this device runs: subset of `fp32`, `fp16`, `bf16`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<String>,
    /// The execution provider behind a GPU id (`cuda`, `webgpu`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Execution providers this build's onnxruntime was compiled with
/// (`cpu`, `cuda`, `webgpu`, `directml`, `coreml`, `nnapi`).
pub fn ep_compiled() -> Vec<&'static str> {
    #[allow(unused_mut)]
    let mut out = vec!["cpu"];
    #[cfg(feature = "cuda")]
    if ep::CUDA::default().is_available().unwrap_or(false) {
        out.push("cuda");
    }
    #[cfg(feature = "webgpu")]
    if ep::WebGPU::default().is_available().unwrap_or(false) {
        out.push("webgpu");
    }
    #[cfg(feature = "directml")]
    if ep::DirectML::default().is_available().unwrap_or(false) {
        out.push("directml");
    }
    #[cfg(feature = "coreml")]
    if ep::CoreML::default().is_available().unwrap_or(false) {
        out.push("coreml");
    }
    #[cfg(feature = "nnapi")]
    if ep::NNAPI::default().is_available().unwrap_or(false) {
        out.push("nnapi");
    }
    out
}

fn provider_of(ep_name: &str) -> Option<&'static str> {
    Some(match ep_name {
        "CUDAExecutionProvider"
        | "NvTensorRTRTXExecutionProvider"
        | "TensorrtExecutionProvider" => "cuda",
        "WebGpuExecutionProvider" => "webgpu",
        "DmlExecutionProvider" => "directml",
        "CoreMLExecutionProvider" => "coreml",
        "NnapiExecutionProvider" => "nnapi",
        _ => return None,
    })
}

/// Probe what can run here: always the CPU, plus every GPU/NPU that a compiled-in
/// execution provider reports through ORT's device enumeration. A provider that is
/// compiled in but enumerates nothing (older drivers, providers without device
/// discovery) is listed as `gpu:0` with its provider name, since the session
/// builder will still try it and fall back to CPU with a reason.
pub fn device_catalog() -> Vec<DeviceInfo> {
    let cpu_label = std::thread::available_parallelism()
        .map(|n| format!("CPU ({n} threads)"))
        .unwrap_or_else(|_| "CPU".into());
    let mut out = vec![DeviceInfo {
        id: "cpu".into(),
        label: cpu_label,
        formats: vec!["fp32".into()],
        provider: Some("cpu".into()),
    }];
    let compiled = ep_compiled();
    let mut seen_providers: Vec<&'static str> = Vec::new();
    if let Ok(env) = Environment::current() {
        for dev in env.devices() {
            let hw = dev.hardware_device();
            if hw.ty() == DeviceType::CPU {
                continue;
            }
            let Some(provider) = dev.ep().ok().and_then(provider_of) else {
                continue;
            };
            if !compiled.contains(&provider) {
                continue;
            }
            let index = out.len() - 1;
            let vendor = hw
                .vendor()
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or(provider);
            let kind = if hw.ty() == DeviceType::NPU {
                "NPU"
            } else {
                "GPU"
            };
            out.push(DeviceInfo {
                id: format!("gpu:{index}"),
                label: format!("{vendor} {kind} (device {:#06x})", hw.id()),
                formats: vec!["fp32".into(), "fp16".into()],
                provider: Some(provider.into()),
            });
            seen_providers.push(provider);
        }
    }
    for provider in compiled.into_iter().filter(|p| *p != "cpu") {
        if !seen_providers.contains(&provider) {
            let index = out.len() - 1;
            out.push(DeviceInfo {
                id: format!("gpu:{index}"),
                label: format!("{provider} device 0"),
                formats: vec!["fp32".into(), "fp16".into()],
                provider: Some(provider.into()),
            });
        }
    }
    out
}
