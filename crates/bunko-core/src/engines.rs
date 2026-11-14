//! Static registry of OCR engines and detectors (no model code; safe for the lite build).
//!
//! The default engines and detector are Apache-2.0. Models under other licences are
//! listed here too, each with its [`LicenseClass`]: a processor offers one only when it
//! was built with that class's Cargo feature (`bunko-engines`), so a build made without
//! the feature neither downloads nor runs it. This registry is the same in every build:
//! a lite server must accept the rows its processors can run.
//!
//! mokuro (manga-ocr), AnimeText and rtdetr were removed in 0.7; configs that still name
//! them are migrated at load (see [`crate::generations`]).

/// What a model's weights are licensed as, as far as shipping them goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LicenseClass {
    /// Apache-2.0 and the like: in every build with OCR.
    Permissive,
    /// GPL: only in builds with the `gpl-models` feature.
    Gpl,
    /// Non-commercial terms (CC BY-NC-SA): only in builds with the `nc-models` feature.
    NonCommercial,
}

impl LicenseClass {
    pub fn as_str(self) -> &'static str {
        match self {
            LicenseClass::Permissive => "permissive",
            LicenseClass::Gpl => "gpl",
            LicenseClass::NonCommercial => "non-commercial",
        }
    }

    /// The Cargo feature of `bunko-engines` (and `mokuro-bunko`) a build needs to offer
    /// models of this class.
    pub fn feature(self) -> Option<&'static str> {
        match self {
            LicenseClass::Permissive => None,
            LicenseClass::Gpl => Some("gpl-models"),
            LicenseClass::NonCommercial => Some("nc-models"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineSpec {
    pub id: &'static str,
    pub label: &'static str,
    /// The short display name a sidecar's `ocr_engine.parts` carries for readers.
    pub name: &'static str,
    /// The model's version for display (`2.5`); empty when it has none.
    pub version: &'static str,
    /// Hugging Face id of the source model the ONNX export was made from.
    pub recognizer: &'static str,
    /// SPDX id of the weights' licence.
    pub license: &'static str,
    pub class: LicenseClass,
    /// The engine's own detector, when it was trained as a pair with one.
    pub detector: Option<&'static str>,
    pub patch_budget: bool,
    /// Non-empty when the recognizer may not be put on a GPU.
    pub cpu_only_reason: &'static str,
    /// Non-empty when the recognizer runs on the CPU but another engine is the better
    /// CPU choice: shown wherever the CPU is picked for it (setup, settings, the admin
    /// pools table).
    pub cpu_note: &'static str,
    /// Precision modes apply (`auto-accuracy`, `fp16`, ...).
    pub precision: bool,
    /// The formats a row may force (`fp32`, `bf16`, `fp16`); empty when precision
    /// modes do not apply. The auto modes resolve among these too.
    pub precision_formats: &'static [&'static str],
    /// The recognizer reads whole text blocks: it may be paired with a block detector
    /// (it then finds the lines itself), as well as with a line detector.
    pub blocks: bool,
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

/// paddle-manga on the CPU (it runs there, as in 0.5.2): what to know before picking it.
/// web/app/app.js carries the same text for the setup and settings pages.
pub const PADDLE_CPU_NOTE: &str = "paddle-manga on the CPU is slow (about 40 s a page on 16 threads) and downloads 3.6 GB of weights; hayai-nova is the CPU engine of choice.";

pub const ENGINES: &[EngineSpec] = &[
    EngineSpec {
        id: "hayai-nova",
        label: "hayai-ocr v2.5 Nova",
        name: "Hayai Nova",
        version: "2.5",
        recognizer: "JustANormalTinkerer/hayai-ocr-v2.5-nova",
        license: "Apache-2.0",
        class: LicenseClass::Permissive,
        detector: None,
        patch_budget: true,
        cpu_only_reason: "",
        cpu_note: "",
        precision: true,
        precision_formats: &["fp32", "bf16", "fp16"],
        blocks: false,
    },
    EngineSpec {
        id: "paddle-manga",
        label: "PaddleOCR-VL 1.6 manga LoRA",
        name: "Paddle Manga",
        version: "3",
        recognizer: "sorryhyun/paddleocr-vl-1.6-manga-lora",
        license: "Apache-2.0",
        class: LicenseClass::Permissive,
        detector: None,
        patch_budget: false,
        cpu_only_reason: "",
        cpu_note: PADDLE_CPU_NOTE,
        precision: true,
        precision_formats: &["fp32", "bf16", "fp16"],
        blocks: false,
    },
    EngineSpec {
        id: "ppocr-manga",
        label: "PP-OCRv6 manga (CTC)",
        name: "PP-OCR Manga",
        version: "0.3",
        recognizer: "Kellenok/PP-OCRv6_manga",
        license: "Apache-2.0",
        class: LicenseClass::Permissive,
        detector: Some("ppocr-manga"),
        patch_budget: false,
        cpu_only_reason: "PP-OCRv6's CTC recognizer runs on the CPU",
        cpu_note: "",
        precision: false,
        precision_formats: &[],
        blocks: false,
    },
    // The text-box recognizers (muscgab): small networks without a decoder loop. On the
    // CPU they run on ONNX Runtime; nar-small also runs on a GPU through the libtorch
    // backend pack (the authors' weights bound to a weightless graph of ours), with the
    // CPU as its fallback; there it reads at fp32 or fp16 (the authors' fp32 weights
    // cast at load), never bf16. Code Apache-2.0, weights CC BY-NC-SA 4.0.
    EngineSpec {
        id: "charspot-small",
        label: "manga-ocr-charspot-small (muscgab)",
        name: "Charspot Small",
        version: "",
        recognizer: "muscgab/manga-ocr-charspot-small",
        license: "CC-BY-NC-SA-4.0",
        class: LicenseClass::NonCommercial,
        detector: None,
        patch_budget: false,
        cpu_only_reason: "manga-ocr-charspot-small runs on the CPU",
        cpu_note: "",
        precision: false,
        precision_formats: &[],
        blocks: true,
    },
    EngineSpec {
        id: "nar-small",
        label: "manga-ocr-nar-small r16 (muscgab)",
        name: "NAR Small",
        version: "r16",
        recognizer: "muscgab/manga-ocr-nar-small",
        license: "CC-BY-NC-SA-4.0",
        class: LicenseClass::NonCommercial,
        detector: None,
        patch_budget: false,
        cpu_only_reason: "",
        cpu_note: "",
        precision: true,
        precision_formats: &["fp32", "fp16"],
        blocks: true,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectorSpec {
    pub id: &'static str,
    pub label: &'static str,
    /// The short display name a sidecar's `ocr_engine.parts` carries for readers.
    pub name: &'static str,
    /// The model's version for display (`0.3`); empty when it has none.
    pub version: &'static str,
    /// SPDX id of the weights' licence.
    pub license: &'static str,
    pub class: LicenseClass,
    pub line_level: bool,
    pub cpu_only_reason: &'static str,
}

pub const DETECTORS: &[DetectorSpec] = &[
    DetectorSpec {
        id: "ppocr-manga",
        label: "PP-OCRv6 manga line detector (Kellenok)",
        name: "PP-OCR Manga",
        version: "0.3",
        license: "Apache-2.0",
        class: LicenseClass::Permissive,
        line_level: true,
        cpu_only_reason: "the PP-OCRv6 detector runs on the CPU",
    },
    // comic-text-detector's line map only: the lines are read, laid out and ordered
    // like ppocr-manga's. Not the 0.5.x `ctd` (which used the model's own blocks): that
    // id stays removed, so a config written for it never starts fetching GPL weights.
    DetectorSpec {
        id: "ctd-lines",
        label: "comic-text-detector line detector (dmMaze)",
        name: "Comic Text Detector",
        version: "",
        license: "GPL-3.0",
        class: LicenseClass::Gpl,
        line_level: true,
        cpu_only_reason: "comic-text-detector runs on the CPU",
    },
    // A block detector: it boxes whole pieces of text, and the engine finds the lines
    // in each. Only for the engines that read blocks ([`EngineSpec::blocks`]). Not the
    // 0.5.x `animetext` (one line per block, whatever the engine): that id stays removed.
    DetectorSpec {
        id: "animetext-blocks",
        label: "AnimeText YOLO12-x block detector (deepghs)",
        name: "AnimeText",
        version: "12x",
        license: "GPL-3.0",
        class: LicenseClass::Gpl,
        line_level: false,
        cpu_only_reason: "AnimeText runs on the CPU",
    },
];

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

/// Detectors that existed in 0.5.x and were removed in 0.7. A row naming one reads with
/// the default detector; picking a detector that came back under a new id is a choice
/// the admin makes again.
pub const REMOVED_DETECTORS: &[(&str, &str)] = &[
    (
        "ctd",
        "the ctd detector of 0.5 (GPL-3.0) was removed in 0.7; comic-text-detector's lines are available again as the ctd-lines detector",
    ),
    (
        "animetext",
        "the animetext detector of 0.5 (GPL-3.0) was removed in 0.7; AnimeText is available again as the animetext-blocks detector, for the engines that read blocks",
    ),
    (
        "rtdetr",
        "rtdetr was removed in 0.7 (poor detection quality)",
    ),
];

pub const PATCH_BUDGETS: &[u32] = &[256, 384, 512];
pub const DEFAULT_PATCH_BUDGET: u32 = 512;

/// One part of a generation's OCR as a reader names it: `role` is `recognizer` or
/// `detector`, `version` is empty when the model has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part {
    pub role: &'static str,
    pub name: &'static str,
    pub version: &'static str,
}

/// What a sidecar's `ocr_engine.parts` lists for `engine` on `detector`, in display
/// order: the recognizer, then the detector, unless the engine is its own detector
/// (PP-OCR read alone). Unknown ids are left out.
///
/// The versions are the shipped models': `ppocr-manga`'s follows
/// `bunko_ocr::models::PPOCR_REVISION`, `hayai-nova`'s the recognizer repo.
pub fn parts(engine_id: &str, detector_id: &str) -> Vec<Part> {
    let mut out = Vec::new();
    let spec = engine(engine_id);
    if let Some(e) = spec {
        out.push(Part {
            role: "recognizer",
            name: e.name,
            version: e.version,
        });
    }
    if spec.is_some_and(|e| e.detector == Some(detector_id)) {
        return out;
    }
    if let Some(d) = detector(detector_id) {
        out.push(Part {
            role: "detector",
            name: d.name,
            version: d.version,
        });
    }
    out
}

pub fn engine(id: &str) -> Option<&'static EngineSpec> {
    ENGINES.iter().find(|e| e.id == id)
}

pub fn detector(id: &str) -> Option<&'static DetectorSpec> {
    DETECTORS.iter().find(|d| d.id == id)
}

/// Why `engine` cannot read with `detector`, when it cannot: a block detector finds no
/// lines, so only an engine that reads blocks goes with it.
pub fn mismatch(engine: &EngineSpec, detector: &DetectorSpec) -> Option<String> {
    if detector.line_level || engine.blocks {
        return None;
    }
    let readers: Vec<&str> = ENGINES.iter().filter(|e| e.blocks).map(|e| e.id).collect();
    Some(format!(
        "the {} detector finds text blocks, and {} reads lines (the engines that read blocks: {})",
        detector.id,
        engine.id,
        readers.join(", ")
    ))
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

/// The modes a row of engine `id` may ask for, in [`PRECISION_MODES`] order: the auto
/// modes and the formats it runs at (nar-small has no bf16). Empty for an engine whose
/// precision is fixed, or an unknown one.
pub fn engine_modes(id: &str) -> Vec<&'static str> {
    let Some(spec) = engine(id).filter(|e| e.precision) else {
        return Vec::new();
    };
    PRECISION_MODES
        .iter()
        .copied()
        .filter(|m| !is_forced_precision(m) || spec.precision_formats.contains(m))
        .collect()
}

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

#[cfg(test)]
mod tests {

    /// What a reader labels a generation's OCR by: recognizer then detector, each
    /// with its version; one part when the engine is its own detector.
    #[test]
    fn a_generation_names_its_parts_with_versions() {
        let shown = |e: &str, d: &str| -> Vec<String> {
            parts(e, d)
                .iter()
                .map(|p| format!("{}:{}:{}", p.role, p.name, p.version))
                .collect()
        };
        assert_eq!(
            shown("hayai-nova", "ppocr-manga"),
            ["recognizer:Hayai Nova:2.5", "detector:PP-OCR Manga:0.3"]
        );
        assert_eq!(
            shown("ppocr-manga", "ppocr-manga"),
            ["recognizer:PP-OCR Manga:0.3"]
        );
        assert_eq!(
            shown("charspot-small", "animetext-blocks"),
            ["recognizer:Charspot Small:", "detector:AnimeText:12x"]
        );
        assert!(shown("gone", "gone").is_empty());
        for e in ENGINES {
            assert!(!e.name.is_empty(), "{}", e.id);
        }
        for d in DETECTORS {
            assert!(!d.name.is_empty(), "{}", d.id);
        }
    }

    use super::*;

    /// paddle-manga runs on the CPU (0.5.2 parity) with a note; the setup and settings
    /// pages carry the same text as the admin panel's catalog.
    #[test]
    fn paddle_manga_runs_on_the_cpu_with_a_note() {
        let paddle = engine("paddle-manga").unwrap();
        assert!(!paddle.cpu_only() && paddle.precision);
        assert_eq!(paddle.cpu_note, PADDLE_CPU_NOTE);
        assert!(engine("hayai-nova").unwrap().cpu_note.is_empty());
        let app_js = include_str!("../../../web/app/app.js");
        assert!(
            app_js.contains(&format!("const PADDLE_CPU_NOTE = '{PADDLE_CPU_NOTE}';")),
            "web/app/app.js carries another PADDLE_CPU_NOTE"
        );
    }

    /// A precision engine runs at fp32 at least (the always-supported fallback); an
    /// engine with a fixed precision lists no format and offers no mode.
    #[test]
    fn precision_formats_follow_the_precision_flag() {
        for e in ENGINES {
            assert_eq!(e.precision, !e.precision_formats.is_empty(), "{}", e.id);
            assert!(
                !e.precision || e.precision_formats.contains(&PRECISION_FP32),
                "{}",
                e.id
            );
            assert!(
                e.precision_formats.iter().all(|f| PRECISIONS.contains(f)),
                "{}",
                e.id
            );
        }
        assert_eq!(engine_modes("hayai-nova"), PRECISION_MODES);
        assert_eq!(
            engine_modes("nar-small"),
            [MODE_ACCURACY, MODE_BALANCED, MODE_SPEED, "fp32", "fp16"]
        );
        assert!(engine_modes("ppocr-manga").is_empty());
        assert!(engine_modes("no-such-engine").is_empty());
    }

    /// nar-small may leave the CPU (the libtorch GPU route), with no caution when the
    /// CPU is picked for it; charspot-small stays on the CPU until its route ships.
    #[test]
    fn nar_small_may_run_on_a_gpu() {
        let nar = engine("nar-small").unwrap();
        assert!(!nar.cpu_only());
        assert_eq!(nar.cpu_note, "");
        assert!(engine("charspot-small").unwrap().cpu_only());
    }
}
