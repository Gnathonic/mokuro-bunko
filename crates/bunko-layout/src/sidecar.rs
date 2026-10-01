//! The `.mokuro` sidecar: page dicts, the volume dict, the `ocr_engine`
//! block, the runner's write and the server's normalising rewrite (spec §8).
//!
//! Byte compatibility: the runner writes `json.dump(volume, ensure_ascii=False)`
//! (separators `", "`/`": "`, no indent) to `<out>.tmp` and renames it; the
//! server then rewrites every installed sidecar compact
//! (`separators=(",", ":")`), keeping each existing key where it was.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::json::{ParseError, Separators, Value};
use crate::layout::{Block, PageLayout, layout_page};
use crate::py;
use crate::records::{RawLine, RawPage};

/// `MOKURO_FORMAT_VERSION`.
pub const MOKURO_FORMAT_VERSION: &str = "0.2.5";

/// `RECOGNIZER_REPOS` (engine -> recognizer repo).
pub const RECOGNIZER_REPOS: [(&str, &str); 3] = [
    ("hayai-nova", "JustANormalTinkerer/hayai-ocr-v2.5-nova"),
    ("paddle-manga", "sorryhyun/paddleocr-vl-1.6-manga-lora"),
    ("ppocr-manga", "Kellenok/PP-OCRv6_manga"),
];

/// `REPO_REVISIONS`: the pinned commit of every VLM repo the runner resolves.
pub const REPO_REVISIONS: [(&str, &str); 4] = [
    ("JustANormalTinkerer/hayai-ocr-v2.5-nova", "e46d79138499600564f810d44ab6bdea7230dee1"),
    ("google/siglip2-base-patch16-naflex", "b53b807d3a2d5e2b3911292f2d69e5341cdc064c"),
    ("sorryhyun/paddleocr-vl-1.6-manga-lora", "26292839d1469c14212a12a1e01b5b1fe01bff15"),
    ("PaddlePaddle/PaddleOCR-VL-1.6", "c5630abae1d940eafe0697512a0325494b02ab42"),
];

/// Engines whose sidecar carries `patch_budget` (`PATCH_BUDGET_ENGINES`).
pub const PATCH_BUDGET_ENGINES: [&str; 1] = ["hayai-nova"];
pub const DEFAULT_PATCH_BUDGET: i64 = 512;

/// Errors reading or writing a sidecar.
#[derive(Debug, thiserror::Error)]
pub enum SidecarError {
    #[error("sidecar I/O on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("sidecar {path} is not valid JSON: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: ParseError,
    },
    #[error("sidecar {path} is not a JSON object")]
    NotAnObject { path: PathBuf },
}

impl Block {
    /// The block dict, keys in the runner's order.
    pub fn to_value(&self) -> Value {
        Value::Object(vec![
            ("box".into(), Value::Array(self.bbox.iter().map(|v| Value::Int(*v)).collect())),
            ("vertical".into(), Value::Bool(self.vertical)),
            ("font_size".into(), Value::Int(self.font_size)),
            ("lines".into(), Value::Array(self.lines.iter().map(|l| Value::Str(l.clone())).collect())),
            (
                "lines_coords".into(),
                Value::Array(
                    self.lines_coords
                        .iter()
                        .map(|q| {
                            Value::Array(q.iter().map(|p| Value::Array(vec![Value::Int(p[0]), Value::Int(p[1])])).collect())
                        })
                        .collect(),
                ),
            ),
        ])
    }
}

/// One mokuro page (`layout_page_dict`).
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub version: String,
    pub img_width: i64,
    pub img_height: i64,
    pub blocks: Vec<Block>,
}

impl Page {
    /// A page with no blocks (a page the engine failed on, `_blank_from`).
    pub fn blank(img_width: i64, img_height: i64) -> Page {
        Page { version: MOKURO_FORMAT_VERSION.to_string(), img_width, img_height, blocks: Vec::new() }
    }

    /// The page dict; with `img_path` as the volume carries it.
    pub fn to_value(&self, img_path: Option<&str>) -> Value {
        let mut items = vec![
            ("version".to_string(), Value::Str(self.version.clone())),
            ("img_width".to_string(), Value::Int(self.img_width)),
            ("img_height".to_string(), Value::Int(self.img_height)),
            ("blocks".to_string(), Value::Array(self.blocks.iter().map(Block::to_value).collect())),
        ];
        if let Some(p) = img_path {
            items.push(("img_path".to_string(), Value::Str(p.replace('\\', "/"))));
        }
        Value::Object(items)
    }
}

/// One mokuro page from a rounded raw page: noise blocks are left out.
pub fn layout_page_dict(raw: &RawPage, version: &str) -> (Page, PageLayout) {
    let result = layout_page(raw);
    let blocks = result
        .blocks
        .iter()
        .zip(&result.kinds)
        .filter(|(_, k)| **k != crate::layout::Kind::Noise)
        .map(|(b, _)| b.clone())
        .collect();
    (
        Page { version: version.to_string(), img_width: raw.width, img_height: raw.height, blocks },
        result,
    )
}

/// What a finished page carries: the sidecar page, the full layout, the raw
/// dump (`detect_dir/<rel>.json`) and, on the reconciled road, the lines for
/// `review.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct FinishedPage {
    pub page: Page,
    pub layout: PageLayout,
    pub raw: Value,
    pub doubtful: Option<Vec<Value>>,
}

/// `PPOcrPageReader.finish`: round the lines, lay them out, and keep the
/// removed ruby with the raw dump.
pub fn finish_page(lines: &[RawLine], width: i64, height: i64, detector: Option<Value>, version: &str) -> FinishedPage {
    let raw_page = RawPage { detector, ..RawPage::new(width, height, lines.to_vec()) }.rounded();
    let (page, layout) = layout_page_dict(&raw_page, version);
    let mut raw = raw_page.to_value();
    let ruby = layout
        .ruby
        .iter()
        .map(|r| {
            Value::Object(vec![
                ("line".into(), Value::Int(r.line as i64)),
                ("base".into(), Value::Int(r.base as i64)),
                ("text".into(), Value::Str(r.text.clone())),
                ("chars".into(), Value::Array(vec![Value::Int(r.chars.0), Value::Int(r.chars.1)])),
            ])
        })
        .collect();
    raw.set("ruby", Value::Array(ruby));
    FinishedPage { page, layout, raw, doubtful: None }
}

/// The `ocr_engine` block of a composed engine's sidecar.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrEngine {
    pub id: String,
    pub recognizer: String,
    pub detector: String,
    /// `--generator` (the server passes `"mokuro-bunko <version>"`), else
    /// `"mokuro-bunko"`.
    pub generator: String,
    /// Only for `PATCH_BUDGET_ENGINES`.
    pub patch_budget: Option<i64>,
    /// Repo -> commit, insertion order kept; omitted when empty.
    pub weights: Vec<(String, String)>,
    /// What the recognizer computed in; omitted when none loaded (ppocr-manga).
    pub precision: Option<String>,
}

impl OcrEngine {
    /// The block the runner writes for `engine` on the `ppocr-manga` detector.
    pub fn new(engine: &str, generator: Option<&str>, patches: i64) -> OcrEngine {
        OcrEngine {
            id: engine.to_string(),
            recognizer: RECOGNIZER_REPOS
                .iter()
                .find(|(e, _)| *e == engine)
                .map(|(_, r)| r.to_string())
                .unwrap_or_default(),
            detector: "ppocr-manga".to_string(),
            generator: generator.filter(|g| !g.is_empty()).unwrap_or("mokuro-bunko").to_string(),
            patch_budget: PATCH_BUDGET_ENGINES.contains(&engine).then_some(patches),
            weights: Vec::new(),
            precision: None,
        }
    }

    pub fn to_value(&self) -> Value {
        let mut items = vec![
            ("id".to_string(), Value::Str(self.id.clone())),
            ("recognizer".to_string(), Value::Str(self.recognizer.clone())),
            ("detector".to_string(), Value::Str(self.detector.clone())),
            ("generator".to_string(), Value::Str(self.generator.clone())),
        ];
        if let Some(p) = self.patch_budget {
            items.push(("patch_budget".to_string(), Value::Int(p)));
        }
        if !self.weights.is_empty() {
            items.push((
                "weights".to_string(),
                Value::Object(self.weights.iter().map(|(k, v)| (k.clone(), Value::Str(v.clone()))).collect()),
            ));
        }
        if let Some(p) = self.precision.as_ref().filter(|p| !p.is_empty()) {
            items.push(("precision".to_string(), Value::Str(p.clone())));
        }
        Value::Object(items)
    }
}

/// The header of a volume sidecar.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeHeader {
    pub version: String,
    pub title: String,
    pub title_uuid: String,
    pub volume: String,
    pub volume_uuid: String,
    /// `None` leaves the key out (pure upstream mokuro shape).
    pub ocr_engine: Option<Value>,
}

/// `build_volume`: the volume dict from `(img_path, page)` pairs in input order.
pub fn build_volume(header: &VolumeHeader, pages: &[(String, Page)]) -> Value {
    let mut items = vec![
        ("version".to_string(), Value::Str(header.version.clone())),
        ("title".to_string(), Value::Str(header.title.clone())),
        ("title_uuid".to_string(), Value::Str(header.title_uuid.clone())),
        ("volume".to_string(), Value::Str(header.volume.clone())),
        ("volume_uuid".to_string(), Value::Str(header.volume_uuid.clone())),
    ];
    if let Some(engine) = &header.ocr_engine {
        items.push(("ocr_engine".to_string(), engine.clone()));
    }
    items.push((
        "pages".to_string(),
        Value::Array(pages.iter().map(|(path, page)| page.to_value(Some(path))).collect()),
    ));
    Value::Object(items)
}

/// Write `contents` to `<path>.tmp` beside `path`, then rename it over `path`.
pub fn write_atomic(path: &Path, contents: &str) -> Result<(), SidecarError> {
    let io = |source| SidecarError::Io { path: path.to_path_buf(), source };
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(io)?;
    }
    let mut f = fs::File::create(&tmp).map_err(io)?;
    f.write_all(contents.as_bytes()).map_err(io)?;
    f.flush().map_err(io)?;
    drop(f);
    fs::rename(&tmp, path).map_err(io)
}

/// The runner's write of a volume sidecar (default separators, atomic).
pub fn write_sidecar(path: &Path, volume: &Value) -> Result<(), SidecarError> {
    write_atomic(path, &volume.dumps(Separators::Default))
}

/// The stamp a non-primary generation's sidecar gets (`_stamp_ocr_engine`).
#[derive(Debug, Clone, PartialEq)]
pub struct LayerStamp {
    /// The generation's engine id (`setdefault`).
    pub engine: String,
    /// `"mokuro-bunko <version>"` (`setdefault`).
    pub generator: String,
    /// The generation row's name (always set).
    pub generation: String,
}

/// What the server writes into every installed sidecar.
#[derive(Debug, Clone, PartialEq)]
pub struct Normalization {
    /// `_derive_series_name(cbz)`.
    pub series_name: String,
    /// The cbz stem.
    pub volume: String,
    /// `volume_uuid_for(cbz, generation)`.
    pub volume_uuid: String,
    /// `Some` for non-primary rows only.
    pub stamp: Option<LayerStamp>,
}

/// `_normalize_mokuro_metadata` on a parsed sidecar, in place. A non-object
/// root is left alone (the server skips it) and `false` returned.
pub fn normalize_sidecar(data: &mut Value, norm: &Normalization) -> bool {
    if !data.is_object() {
        return false;
    }
    data.set("title", Value::Str(norm.series_name.clone()));
    data.set("volume", Value::Str(norm.volume.clone()));
    data.set("title_uuid", Value::Str(title_uuid(&norm.series_name)));
    data.set("volume_uuid", Value::Str(norm.volume_uuid.clone()));
    if let Some(stamp) = &norm.stamp {
        let mut stamped = match data.get("ocr_engine") {
            Some(block @ Value::Object(_)) => block.clone(),
            _ => Value::object(),
        };
        stamped.set_default("id", Value::Str(stamp.engine.clone()));
        stamped.set_default("generator", Value::Str(stamp.generator.clone()));
        stamped.set("generation", Value::Str(stamp.generation.clone()));
        data.set("ocr_engine", stamped);
    }
    true
}

/// Read, normalise and rewrite a sidecar compact (atomically). `Ok(false)`
/// when the root is not an object (left untouched, as the server does).
pub fn normalize_sidecar_file(path: &Path, norm: &Normalization) -> Result<bool, SidecarError> {
    let text = fs::read_to_string(path).map_err(|source| SidecarError::Io { path: path.to_path_buf(), source })?;
    let mut data = Value::parse(&text).map_err(|source| SidecarError::Json { path: path.to_path_buf(), source })?;
    if !normalize_sidecar(&mut data, norm) {
        return Ok(false);
    }
    write_atomic(path, &data.dumps(Separators::Compact))?;
    Ok(true)
}

/// `str(uuid.uuid5(uuid.NAMESPACE_DNS, series_name))`.
pub fn title_uuid(series_name: &str) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, series_name.as_bytes()).to_string()
}

/// `_derive_series_name`: the parent folder's name, stripped, or the cbz stem
/// when the parent is the library root or the inbox (or the name is blank).
pub fn derive_series_name(cbz: &Path, library_root: &Path, inbox: &Path) -> String {
    let stem = cbz.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let Some(parent) = cbz.parent() else {
        return stem;
    };
    if parent == library_root || parent == inbox {
        return stem;
    }
    let name = parent.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let name = py::strip(&name);
    if name.is_empty() { stem } else { name.to_string() }
}

/// Port of the reader's `generateDeterministicUUID` (djb2-xor pair over UTF-16
/// units of the lower-cased, stripped value; 8-4-4-4-8 shape, quirks kept).
pub fn deterministic_uuid(value: &str) -> String {
    let normalized = value.to_lowercase();
    let normalized = py::strip(&normalized);
    let mut h1: i32 = 5381;
    let mut h2: i32 = 52711;
    for unit in normalized.encode_utf16() {
        h1 = h1.wrapping_mul(33) ^ i32::from(unit);
        h2 = h2.wrapping_mul(33) ^ i32::from(unit);
    }
    let (h1, h2) = (h1 as u32, h2 as u32);
    let hex1 = format!("{h1:08x}");
    let hex2 = format!("{h2:08x}");
    let hash3 = format!("{:08x}", h1 ^ h2);
    let hash4 = format!("{:08x}", h1.wrapping_add(h2));
    let lead = u32::from_str_radix(&hash3[..1], 16).unwrap_or(0);
    let variant = format!("{:x}", 8 + lead % 4);
    format!("{}-{}-4{}-{}{}-{}{}", hex1, &hex2[..4], &hex2[5..8], variant, &hash3[1..4], &hash3[4..], &hash4[..4])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid5_of_dns_namespace() {
        // python: str(uuid.uuid5(uuid.NAMESPACE_DNS, "python.org"))
        assert_eq!(title_uuid("python.org"), "886313e1-3b8a-5372-9b90-0c9aee199e5d");
    }

    #[test]
    fn series_name_rules() {
        let lib = Path::new("/lib");
        let inbox = Path::new("/lib/inbox");
        assert_eq!(derive_series_name(Path::new("/lib/Vol 1.cbz"), lib, inbox), "Vol 1");
        assert_eq!(derive_series_name(Path::new("/lib/inbox/Vol 1.cbz"), lib, inbox), "Vol 1");
        assert_eq!(derive_series_name(Path::new("/lib/ Series /Vol 1.cbz"), lib, inbox), "Series");
    }
}
