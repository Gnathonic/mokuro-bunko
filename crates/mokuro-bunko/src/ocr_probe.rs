//! Hooks into the OCR runtime for `doctor` (full build only).
//!
//! TODO(orchestrator): wire to bunko-ocr once it exposes its runtime probe. Return the
//! ONNX Runtime version plus the execution providers that actually initialise on this
//! machine (e.g. `["CPU", "CUDA"]`), or an error string such as "onnxruntime library
//! not found". Keep it cheap: no model loading.

pub struct RuntimeInfo {
    pub version: String,
    pub providers: Vec<String>,
}

pub fn probe() -> Result<RuntimeInfo, String> {
    Err("not wired".into())
}
