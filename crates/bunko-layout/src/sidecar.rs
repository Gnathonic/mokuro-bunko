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
pub const RECOGNIZER_REPOS: [(&str, &str); 5] = [
    ("hayai-nova", "JustANormalTinkerer/hayai-ocr-v2.5-nova"),
    ("paddle-manga", "sorryhyun/paddleocr-vl-1.6-manga-lora"),
    ("ppocr-manga", "Kellenok/PP-OCRv6_manga"),
    ("charspot-small", "muscgab/manga-ocr-charspot-small"),
    ("nar-small", "muscgab/manga-ocr-nar-small"),
];

/// Each composed engine's output revision, written as `ocr_engine.revision`. Bump an
/// engine's number only when its output changes (new weights or export, a layout
/// change): with upgrades on, every older sealed file of that engine is OCR'd again,
/// and processors announcing an older revision are offered none of its work.
///
/// nar-small 2: the model of its repo's `r16` branch (0.8 pre-releases read with the
/// first release).
pub const ENGINE_REVISIONS: [(&str, u32); 4] = [
    ("hayai-nova", 2),
    ("paddle-manga", 2),
    ("ppocr-manga", 2),
    ("nar-small", 2),
];

/// `engine`'s current output revision; 1 for an engine not in [`ENGINE_REVISIONS`].
/// An absent `ocr_engine.revision` reads as 1 (what 0.7 wrote): current for an engine
/// still at 1, old output for one that has moved on.
pub fn engine_revision(engine: &str) -> u32 {
    ENGINE_REVISIONS
        .iter()
        .find(|(e, _)| *e == engine)
        .map_or(1, |(_, r)| *r)
}

/// `REPO_REVISIONS`: the pinned commit of every VLM repo the runner resolves.
pub const REPO_REVISIONS: [(&str, &str); 4] = [
    (
        "JustANormalTinkerer/hayai-ocr-v2.5-nova",
        "e46d79138499600564f810d44ab6bdea7230dee1",
    ),
    (
        "google/siglip2-base-patch16-naflex",
        "b53b807d3a2d5e2b3911292f2d69e5341cdc064c",
    ),
    (
        "sorryhyun/paddleocr-vl-1.6-manga-lora",
        "26292839d1469c14212a12a1e01b5b1fe01bff15",
    ),
    (
        "PaddlePaddle/PaddleOCR-VL-1.6",
        "c5630abae1d940eafe0697512a0325494b02ab42",
    ),
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
            (
                "box".into(),
                Value::Array(self.bbox.iter().map(|v| Value::Int(*v)).collect()),
            ),
            ("vertical".into(), Value::Bool(self.vertical)),
            ("font_size".into(), Value::Int(self.font_size)),
            (
                "lines".into(),
                Value::Array(self.lines.iter().map(|l| Value::Str(l.clone())).collect()),
            ),
            (
                "lines_coords".into(),
                Value::Array(
                    self.lines_coords
                        .iter()
                        .map(|q| {
                            Value::Array(
                                q.iter()
                                    .map(|p| Value::Array(vec![Value::Int(p[0]), Value::Int(p[1])]))
                                    .collect(),
                            )
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
        Page {
            version: MOKURO_FORMAT_VERSION.to_string(),
            img_width,
            img_height,
            blocks: Vec::new(),
        }
    }

    /// The page dict; with `img_path` as the volume carries it.
    pub fn to_value(&self, img_path: Option<&str>) -> Value {
        let mut items = vec![
            ("version".to_string(), Value::Str(self.version.clone())),
            ("img_width".to_string(), Value::Int(self.img_width)),
            ("img_height".to_string(), Value::Int(self.img_height)),
            (
                "blocks".to_string(),
                Value::Array(self.blocks.iter().map(Block::to_value).collect()),
            ),
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
        Page {
            version: version.to_string(),
            img_width: raw.width,
            img_height: raw.height,
            blocks,
        },
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
pub fn finish_page(
    lines: &[RawLine],
    width: i64,
    height: i64,
    detector: Option<Value>,
    version: &str,
) -> FinishedPage {
    let raw_page = RawPage {
        detector,
        ..RawPage::new(width, height, lines.to_vec())
    }
    .rounded();
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
                (
                    "chars".into(),
                    Value::Array(vec![Value::Int(r.chars.0), Value::Int(r.chars.1)]),
                ),
            ])
        })
        .collect();
    raw.set("ruby", Value::Array(ruby));
    FinishedPage {
        page,
        layout,
        raw,
        doubtful: None,
    }
}

/// One mokuro page from the blocks of a block detector: `blocks[k]` holds the lines the
/// block reader found in block `k`, in its reading order. The blocks and their lines
/// are kept as they are (no furigana filter, no roles, no merging: the detector and
/// the reader decided all that); the layout only puts the blocks in reading order and
/// builds them. A line says itself which way it runs.
pub fn blocks_page(blocks: &[Vec<RawLine>], width: i64, height: i64, version: &str) -> Page {
    use crate::layout::{Kind, Line, build_block, measure, order_blocks};

    let mut measured: Vec<Vec<Line>> = Vec::new();
    let mut index = 0usize;
    for block in blocks {
        let mut lines = Vec::new();
        for raw in block {
            let raw = raw.rounded();
            if let Some(mut line) = measure(index, &raw).filter(|l| !l.text.is_empty()) {
                line.vertical = raw.vertical;
                lines.push(line);
            }
            index += 1;
        }
        if !lines.is_empty() {
            measured.push(lines);
        }
    }
    let groups: Vec<Vec<&Line>> = measured.iter().map(|b| b.iter().collect()).collect();
    let kinds = vec![Kind::Text; groups.len()];
    let order = order_blocks(&groups, &kinds, &[]);
    Page {
        version: version.to_string(),
        img_width: width,
        img_height: height,
        blocks: order
            .iter()
            .map(|&i| build_block(&groups[i], width as f64, height as f64, 0.0))
            .collect(),
    }
}

/// The `ocr_engine` block of a composed engine's sidecar.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrEngine {
    pub id: String,
    /// [`engine_revision`] of `id`: written right after it.
    pub revision: u32,
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
    /// The parts a reader names the OCR by, in display order: `(role, name,
    /// version)`, the version empty when the model has none. Omitted when empty.
    pub parts: Vec<(String, String, String)>,
}

impl OcrEngine {
    /// The block the runner writes for `engine` on the `ppocr-manga` detector.
    pub fn new(engine: &str, generator: Option<&str>, patches: i64) -> OcrEngine {
        OcrEngine {
            id: engine.to_string(),
            revision: engine_revision(engine),
            recognizer: RECOGNIZER_REPOS
                .iter()
                .find(|(e, _)| *e == engine)
                .map(|(_, r)| r.to_string())
                .unwrap_or_default(),
            detector: "ppocr-manga".to_string(),
            generator: generator
                .filter(|g| !g.is_empty())
                .unwrap_or("mokuro-bunko")
                .to_string(),
            patch_budget: PATCH_BUDGET_ENGINES.contains(&engine).then_some(patches),
            weights: Vec::new(),
            precision: None,
            parts: Vec::new(),
        }
    }

    pub fn to_value(&self) -> Value {
        let mut items = vec![
            ("id".to_string(), Value::Str(self.id.clone())),
            ("revision".to_string(), Value::Int(i64::from(self.revision))),
            (
                "recognizer".to_string(),
                Value::Str(self.recognizer.clone()),
            ),
            ("detector".to_string(), Value::Str(self.detector.clone())),
            ("generator".to_string(), Value::Str(self.generator.clone())),
        ];
        if let Some(p) = self.patch_budget {
            items.push(("patch_budget".to_string(), Value::Int(p)));
        }
        if !self.weights.is_empty() {
            items.push((
                "weights".to_string(),
                Value::Object(
                    self.weights
                        .iter()
                        .map(|(k, v)| (k.clone(), Value::Str(v.clone())))
                        .collect(),
                ),
            ));
        }
        if let Some(p) = self.precision.as_ref().filter(|p| !p.is_empty()) {
            items.push(("precision".to_string(), Value::Str(p.clone())));
        }
        if !self.parts.is_empty() {
            let part = |(role, name, version): &(String, String, String)| {
                let mut fields = vec![
                    ("role".to_string(), Value::Str(role.clone())),
                    ("name".to_string(), Value::Str(name.clone())),
                ];
                if !version.is_empty() {
                    fields.push(("version".to_string(), Value::Str(version.clone())));
                }
                Value::Object(fields)
            };
            items.push((
                "parts".to_string(),
                Value::Array(self.parts.iter().map(part).collect()),
            ));
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
        (
            "title_uuid".to_string(),
            Value::Str(header.title_uuid.clone()),
        ),
        ("volume".to_string(), Value::Str(header.volume.clone())),
        (
            "volume_uuid".to_string(),
            Value::Str(header.volume_uuid.clone()),
        ),
    ];
    if let Some(engine) = &header.ocr_engine {
        items.push(("ocr_engine".to_string(), engine.clone()));
    }
    items.push((
        "pages".to_string(),
        Value::Array(
            pages
                .iter()
                .map(|(path, page)| page.to_value(Some(path)))
                .collect(),
        ),
    ));
    Value::Object(items)
}

/// Write `contents` to `<path>.tmp` beside `path`, then rename it over `path`.
pub fn write_atomic(path: &Path, contents: &str) -> Result<(), SidecarError> {
    let io = |source| SidecarError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
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
    /// The install time (ISO UTC) written as `ocr_engine.created` into a file that has
    /// a block; `None` writes none.
    pub created: Option<String>,
}

/// Stamp a layer's `ocr_engine` block (`_stamp_ocr_engine`): the engine and generator
/// unless the runner named them, the generation always. A file without a block gets one.
pub fn stamp_layer(data: &mut Value, stamp: &LayerStamp) {
    let mut stamped = match data.get("ocr_engine") {
        Some(block @ Value::Object(_)) => block.clone(),
        _ => Value::object(),
    };
    stamped.set_default("id", Value::Str(stamp.engine.clone()));
    stamped.set_default("generator", Value::Str(stamp.generator.clone()));
    stamped.set("generation", Value::Str(stamp.generation.clone()));
    data.set("ocr_engine", stamped);
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
        stamp_layer(data, stamp);
    }
    // Only a block is dated: an upstream-shaped primary stays in upstream shape. A
    // runner's own revision stands; one that wrote none ran revision 1 (the scheduler
    // offers no work to a processor older than this library's revision).
    if let Some(block @ Value::Object(_)) = data.get_mut("ocr_engine") {
        block.set_default("revision", Value::Int(1));
        if let Some(created) = &norm.created {
            block.set("created", Value::Str(created.clone()));
        }
    }
    true
}

/// Read, normalise and rewrite a sidecar compact (atomically). `Ok(false)`
/// when the root is not an object (left untouched, as the server does).
pub fn normalize_sidecar_file(path: &Path, norm: &Normalization) -> Result<bool, SidecarError> {
    let text = fs::read_to_string(path).map_err(|source| SidecarError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut data = Value::parse(&text).map_err(|source| SidecarError::Json {
        path: path.to_path_buf(),
        source,
    })?;
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
    let stem = cbz
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(parent) = cbz.parent() else {
        return stem;
    };
    if parent == library_root || parent == inbox {
        return stem;
    }
    let name = parent
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = py::strip(&name);
    if name.is_empty() {
        stem
    } else {
        name.to_string()
    }
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
    format!(
        "{}-{}-4{}-{}{}-{}{}",
        hex1,
        &hex2[..4],
        &hex2[5..8],
        variant,
        &hash3[1..4],
        &hash3[4..],
        &hash4[..4]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid5_of_dns_namespace() {
        // python: str(uuid.uuid5(uuid.NAMESPACE_DNS, "python.org"))
        assert_eq!(
            title_uuid("python.org"),
            "886313e1-3b8a-5372-9b90-0c9aee199e5d"
        );
    }

    fn read_line(text: &str, x0: f64, y0: f64, x1: f64, y1: f64, vertical: bool) -> RawLine {
        RawLine {
            quad: vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]],
            score: 0.9,
            text: text.to_string(),
            conf: 0.9,
            vertical,
            angle: 0.0,
            char_confs: Vec::new(),
        }
    }

    /// A block detector's blocks stand as they were found: every line kept (a short
    /// line beside a long one is no furigana here), in the reader's order, and the
    /// blocks in reading order (right to left).
    #[test]
    fn a_block_detectors_blocks_stand_and_are_put_in_reading_order() {
        let left = vec![
            read_line("こんにちは", 160.0, 100.0, 200.0, 400.0, true),
            read_line("ね", 110.0, 100.0, 150.0, 140.0, true),
        ];
        let right = vec![
            read_line("あ", 800.0, 100.0, 840.0, 140.0, true),
            read_line("", 700.0, 100.0, 740.0, 140.0, true),
        ];
        let page = blocks_page(&[left, right, Vec::new()], 1000, 1400, "0.2.5");
        assert_eq!((page.img_width, page.img_height), (1000, 1400));
        let lines: Vec<&[String]> = page.blocks.iter().map(|b| b.lines.as_slice()).collect();
        assert_eq!(
            lines,
            [
                &["あ".to_string()][..],
                &["こんにちは".into(), "ね".into()][..]
            ]
        );
        assert!(page.blocks.iter().all(|b| b.vertical));
        assert_eq!(page.blocks[1].lines_coords.len(), 2);
        // The one-character line is as wide as it is tall: it runs the way its
        // reader said, not the way its box looks.
        let across = vec![read_line("OK", 100.0, 100.0, 180.0, 140.0, false)];
        assert!(!blocks_page(&[across], 1000, 1400, "0.2.5").blocks[0].vertical);
        assert!(blocks_page(&[], 10, 10, "0.2.5").blocks.is_empty());
    }

    /// The parts a reader labels the OCR by close the block; a part without a version
    /// has no `version` key.
    #[test]
    fn engine_block_lists_its_parts_last() {
        let mut e = OcrEngine::new("charspot-small", None, 0);
        e.detector = "animetext-blocks".into();
        e.parts = vec![
            ("recognizer".into(), "Charspot Small".into(), String::new()),
            ("detector".into(), "AnimeText".into(), "12x".into()),
        ];
        let v = e.to_value();
        let Value::Object(items) = &v else {
            panic!("{v:?}")
        };
        assert_eq!(items.last().map(|(k, _)| k.as_str()), Some("parts"));
        let Some(Value::Array(parts)) = v.get("parts") else {
            panic!("{v:?}")
        };
        assert_eq!(parts.len(), 2);
        assert!(parts[0].get("version").is_none());
        assert_eq!(parts[1].get("version"), Some(&Value::Str("12x".into())));
        assert_eq!(
            parts[0].get("name"),
            Some(&Value::Str("Charspot Small".into()))
        );
        // None: no key at all, as before.
        assert!(
            OcrEngine::new("hayai-nova", None, 512)
                .to_value()
                .get("parts")
                .is_none()
        );
    }

    #[test]
    fn engine_block_carries_its_revision_after_the_id() {
        let block = OcrEngine::new("hayai-nova", None, 512).to_value();
        let Value::Object(items) = &block else {
            panic!("an object");
        };
        let keys: Vec<&str> = items.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(&keys[..3], ["id", "revision", "recognizer"]);
        let current = engine_revision("hayai-nova");
        assert_eq!(block.get("revision"), Some(&Value::Int(i64::from(current))));
        // 0.8's line detector (PP-OCR manga v0.3) made every engine's output new.
        assert!(ENGINE_REVISIONS.iter().all(|(_, r)| *r >= 2));
        // nar-small's r16 model replaced the one 0.8 pre-releases read with.
        assert_eq!(engine_revision("nar-small"), 2);
        assert_eq!(engine_revision("charspot-small"), 1);
        assert_eq!(
            engine_revision("no-such-engine"),
            1,
            "unknown engines are 1"
        );
    }

    #[test]
    fn normalisation_dates_blocks_and_keeps_their_revision() {
        let norm = |stamp: Option<LayerStamp>| Normalization {
            series_name: "S".into(),
            volume: "V".into(),
            volume_uuid: "u".into(),
            stamp,
            created: Some("2026-10-29T00:00:00Z".into()),
        };
        // Upstream mokuro shape (no block): stays without one.
        let mut plain = Value::parse(r#"{"version":"0.2.5","pages":[]}"#).unwrap();
        assert!(normalize_sidecar(&mut plain, &norm(None)));
        assert!(plain.get("ocr_engine").is_none());
        // A block without a revision reads as 1 and says so; the install time is set.
        let mut bare = Value::parse(r#"{"ocr_engine":{"id":"x"},"pages":[]}"#).unwrap();
        assert!(normalize_sidecar(&mut bare, &norm(None)));
        let block = bare.get("ocr_engine").unwrap();
        assert_eq!(block.get("revision"), Some(&Value::Int(1)));
        assert_eq!(
            block.get("created").and_then(Value::as_str),
            Some("2026-10-29T00:00:00Z")
        );
        // A revision the runner wrote is never overwritten, on a stamped layer too.
        let mut layer =
            Value::parse(r#"{"ocr_engine":{"id":"x","revision":3},"pages":[]}"#).unwrap();
        let stamp = LayerStamp {
            engine: "x".into(),
            generator: "mokuro-bunko".into(),
            generation: "fast".into(),
        };
        assert!(normalize_sidecar(&mut layer, &norm(Some(stamp))));
        let block = layer.get("ocr_engine").unwrap();
        assert_eq!(block.get("revision"), Some(&Value::Int(3)));
        assert!(block.get("created").is_some());
        // No install time given: nothing dated.
        let mut undated = Value::parse(r#"{"ocr_engine":{"id":"x"},"pages":[]}"#).unwrap();
        let mut n = norm(None);
        n.created = None;
        assert!(normalize_sidecar(&mut undated, &n));
        assert!(undated.get("ocr_engine").unwrap().get("created").is_none());
    }

    #[test]
    fn series_name_rules() {
        let lib = Path::new("/lib");
        let inbox = Path::new("/lib/inbox");
        assert_eq!(
            derive_series_name(Path::new("/lib/Vol 1.cbz"), lib, inbox),
            "Vol 1"
        );
        assert_eq!(
            derive_series_name(Path::new("/lib/inbox/Vol 1.cbz"), lib, inbox),
            "Vol 1"
        );
        assert_eq!(
            derive_series_name(Path::new("/lib/ Series /Vol 1.cbz"), lib, inbox),
            "Series"
        );
    }
}
