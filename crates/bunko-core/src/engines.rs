//! Static registry of OCR engines and detectors (no model code; safe for the lite build).
//!
//! 0.7 ships Apache-2.0 components only. mokuro (manga-ocr + comic-text-detector),
//! AnimeText and rtdetr were removed; configs that still name them are migrated at load
//! (see [`crate::generations`]).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineSpec {
    pub id: &'static str,
    pub label: &'static str,
    /// Hugging Face id of the source model the ONNX export was made from.
    pub recognizer: &'static str,
    /// The engine's own detector, when it was trained as a pair with one.
    pub detector: Option<&'static str>,
    pub patch_budget: bool,
    /// Non-empty when the recognizer may not be put on a GPU.
    pub cpu_only_reason: &'static str,
    /// Precision modes apply (`auto-accuracy`, `fp16`, ...).
    pub precision: bool,
}

impl EngineSpec {
    pub fn cpu_only(&self) -> bool {
        !self.cpu_only_reason.is_empty()
    }
    /// The road through the pipeline: `line` (detector + CTC only) or `reconciled`.
    pub fn road(&self) -> Road {
        if self.detector.is_some() {
            Road::Line
        } else {
            Road::Reconciled
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Road {
    /// ppocr-manga alone: `detect` (det + CTC read), then `layout`.
    Line,
    /// An engine reading the ppocr-manga detector's lines: `detect`, `engine`, `post`.
    Reconciled,
}

pub const STAGE_DETECT: &str = "detect";
pub const STAGE_ENGINE: &str = "engine";
pub const STAGE_POST: &str = "post";
pub const STAGE_LAYOUT: &str = "layout";

impl Road {
    pub fn stage_keys(self) -> &'static [&'static str] {
        match self {
            Road::Line => &[STAGE_DETECT, STAGE_LAYOUT],
            Road::Reconciled => &[STAGE_DETECT, STAGE_ENGINE, STAGE_POST],
        }
    }
    /// Stages that hold a model and therefore take a device.
    pub fn device_stage_keys(self) -> &'static [&'static str] {
        match self {
            Road::Line => &[STAGE_DETECT],
            Road::Reconciled => &[STAGE_DETECT, STAGE_ENGINE],
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Road::Line => "line",
            Road::Reconciled => "reconciled",
        }
    }
}

pub const ENGINES: &[EngineSpec] = &[
    EngineSpec {
        id: "hayai-nova",
        label: "hayai-ocr v2.5 Nova",
        recognizer: "JustANormalTinkerer/hayai-ocr-v2.5-nova",
        detector: None,
        patch_budget: true,
        cpu_only_reason: "",
        precision: true,
    },
    EngineSpec {
        id: "paddle-manga",
        label: "PaddleOCR-VL 1.6 manga LoRA",
        recognizer: "sorryhyun/paddleocr-vl-1.6-manga-lora",
        detector: None,
        patch_budget: false,
        cpu_only_reason: "",
        precision: true,
    },
    EngineSpec {
        id: "ppocr-manga",
        label: "PP-OCRv6 manga (CTC)",
        recognizer: "Kellenok/PP-OCRv6_manga",
        detector: Some("ppocr-manga"),
        patch_budget: false,
        cpu_only_reason: "PP-OCRv6's CTC recognizer runs on the CPU",
        precision: false,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectorSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub license: &'static str,
    pub line_level: bool,
    pub cpu_only_reason: &'static str,
}

pub const DETECTORS: &[DetectorSpec] = &[DetectorSpec {
    id: "ppocr-manga",
    label: "PP-OCRv6 manga line detector (Kellenok)",
    license: "Apache-2.0",
    line_level: true,
    cpu_only_reason: "the PP-OCRv6 detector runs on the CPU",
}];

pub const DEFAULT_ENGINE: &str = "hayai-nova";
pub const DEFAULT_DETECTOR: &str = "ppocr-manga";

/// Engines that existed in 0.5.x and were removed in 0.7, with the reason shown to admins.
pub const REMOVED_ENGINES: &[(&str, &str)] = &[
    (
        "mokuro",
        "mokuro (manga-ocr + comic-text-detector) was removed in 0.7: GPL-licensed detector and weaker reading than hayai-nova/paddle-manga",
    ),
    (
        "mokuro-fp16",
        "mokuro was removed in 0.7: GPL-licensed detector and weaker reading than hayai-nova/paddle-manga",
    ),
];

/// Detectors that existed in 0.5.x and were removed in 0.7.
pub const REMOVED_DETECTORS: &[(&str, &str)] = &[
    (
        "ctd",
        "comic-text-detector is GPL-3.0 and was removed in 0.7",
    ),
    ("animetext", "AnimeText (GPL-3.0) was removed in 0.7"),
    (
        "rtdetr",
        "rtdetr was removed in 0.7 (poor detection quality)",
    ),
];

pub const PATCH_BUDGETS: &[u32] = &[256, 384, 512];
pub const DEFAULT_PATCH_BUDGET: u32 = 512;

pub fn engine(id: &str) -> Option<&'static EngineSpec> {
    ENGINES.iter().find(|e| e.id == id)
}

pub fn detector(id: &str) -> Option<&'static DetectorSpec> {
    DETECTORS.iter().find(|d| d.id == id)
}

pub fn removed_engine(id: &str) -> Option<&'static str> {
    REMOVED_ENGINES
        .iter()
        .find(|(e, _)| *e == id)
        .map(|(_, why)| *why)
}

pub fn removed_detector(id: &str) -> Option<&'static str> {
    REMOVED_DETECTORS
        .iter()
        .find(|(d, _)| *d == id)
        .map(|(_, why)| *why)
}

// --- precision ---------------------------------------------------------------

pub const PRECISION_FP32: &str = "fp32";
pub const PRECISION_FP16: &str = "fp16";
pub const PRECISION_BF16: &str = "bf16";
pub const PRECISIONS: &[&str] = &[PRECISION_BF16, PRECISION_FP16, PRECISION_FP32];

pub const MODE_ACCURACY: &str = "auto-accuracy";
pub const MODE_BALANCED: &str = "auto-balanced";
pub const MODE_SPEED: &str = "auto-speed";
pub const PRECISION_MODES: &[&str] = &[
    MODE_ACCURACY,
    MODE_BALANCED,
    MODE_SPEED,
    PRECISION_FP32,
    PRECISION_BF16,
    PRECISION_FP16,
];
pub const DEFAULT_PRECISION_MODE: &str = MODE_ACCURACY;

pub fn is_forced_precision(mode: &str) -> bool {
    PRECISIONS.contains(&mode)
}

/// `auto` (the old spelling) means the default mode.
pub fn normalize_precision_mode(value: Option<&str>) -> Result<&'static str, String> {
    let v = value.map(str::trim).unwrap_or("");
    if v.is_empty() || v == "auto" {
        return Ok(DEFAULT_PRECISION_MODE);
    }
    PRECISION_MODES
        .iter()
        .find(|m| **m == v)
        .copied()
        .ok_or_else(|| {
            format!(
                "precision is '{v}'; it must be one of {}",
                PRECISION_MODES.join(", ")
            )
        })
}
