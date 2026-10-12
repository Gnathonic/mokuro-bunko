//! Hooks into the OCR runtime for `doctor` (full build only): the linked ONNX Runtime's
//! version and the execution providers that are usable on this machine (compiled in and
//! reporting available). No model is loaded.

pub struct RuntimeInfo {
    pub version: String,
    pub providers: Vec<String>,
}

pub fn probe() -> Result<RuntimeInfo, String> {
    let probed = std::panic::catch_unwind(|| {
        bunko_engines::runtime::init();
        let version = bunko_engines::runtime::ort_version();
        let providers = bunko_ocr::runtime::ep_compiled()
            .into_iter()
            .map(|p| match p {
                "cpu" => "CPU".to_string(),
                "cuda" => "CUDA".to_string(),
                "webgpu" => "WebGPU".to_string(),
                "directml" => "DirectML".to_string(),
                "coreml" => "CoreML".to_string(),
                other => other.to_ascii_uppercase(),
            })
            .collect();
        RuntimeInfo { version, providers }
    });
    probed.map_err(|_| "onnxruntime could not be initialised".to_string())
}
