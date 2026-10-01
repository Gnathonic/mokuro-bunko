//! The input records: what the PP-OCR reader produces for a page.
//!
//! The same shape serves twice, as in 0.5.2:
//!
//! * **unrounded**, straight from the reader (`ppocr.Line`): float32 quad
//!   corners widened to f64, the detector score, the CTC text, confidence and
//!   per-character confidences, and the reader's own `vertical`/`angle`
//!   (computed on the float32 quad). The reconciled road
//!   ([`crate::road`]) measures these.
//! * **rounded** by [`RawPage::rounded`] (`ppocr.page_to_json`): corners to
//!   0.01 px, score/conf to 4 decimals, angle to 2. **The layout always runs
//!   on the rounded form** (spec §5.0), so `conf < 0.5` sees the rounded value.
//!
//! The serde form is the `ppocr-lines/1` JSON (`{"format","width","height",
//! "detector"?,"lines":[{quad,score,text,conf,vertical,angle,char_confs}]}`),
//! so the `bunko-ocr` crate's line structs interoperate through serde.

use serde::{Deserialize, Serialize};

use crate::json::Value;
use crate::py::round_digits;

/// `ppocr.FORMAT_ID`.
pub const FORMAT_ID: &str = "ppocr-lines/1";

fn default_format() -> String {
    FORMAT_ID.to_string()
}

/// One detected (and read) line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawLine {
    /// Corners TL, TR, BR, BL of the line's own upright frame, page pixels.
    pub quad: Vec<[f64; 2]>,
    /// DBNet's mean probability over the quad.
    #[serde(default)]
    pub score: f64,
    /// The CTC read (may be empty).
    #[serde(default)]
    pub text: String,
    /// The CTC read's confidence (mean of per-character confidences).
    #[serde(default)]
    pub conf: f64,
    /// The reader's orientation (`height > width` on the float32 quad).
    #[serde(default)]
    pub vertical: bool,
    /// The reader's tilt in degrees, (-45, 45], clockwise positive.
    #[serde(default)]
    pub angle: f64,
    /// One confidence per character of `text` (may be empty).
    #[serde(default)]
    pub char_confs: Vec<f64>,
}

/// One page of lines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawPage {
    #[serde(default = "default_format")]
    pub format: String,
    pub width: i64,
    pub height: i64,
    /// The reader's detector info, opaque here (key order kept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detector: Option<Value>,
    pub lines: Vec<RawLine>,
}

impl RawLine {
    /// `ppocr.page_to_json`'s rounding of one line.
    pub fn rounded(&self) -> RawLine {
        RawLine {
            quad: self.quad.iter().map(|p| [round_digits(p[0], 2), round_digits(p[1], 2)]).collect(),
            score: round_digits(self.score, 4),
            text: self.text.clone(),
            conf: round_digits(self.conf, 4),
            vertical: self.vertical,
            angle: round_digits(self.angle, 2),
            char_confs: self.char_confs.iter().map(|c| round_digits(*c, 4)).collect(),
        }
    }

    /// The line as `page_to_json` writes it (non-compact flavour).
    pub fn to_value(&self) -> Value {
        Value::Object(vec![
            (
                "quad".into(),
                Value::Array(
                    self.quad
                        .iter()
                        .map(|p| Value::Array(vec![Value::Float(p[0]), Value::Float(p[1])]))
                        .collect(),
                ),
            ),
            ("score".into(), Value::Float(self.score)),
            ("text".into(), Value::Str(self.text.clone())),
            ("conf".into(), Value::Float(self.conf)),
            ("vertical".into(), Value::Bool(self.vertical)),
            ("angle".into(), Value::Float(self.angle)),
            ("char_confs".into(), Value::Array(self.char_confs.iter().map(|c| Value::Float(*c)).collect())),
        ])
    }

    /// Read one line leniently, the way `line_layout` reads a raw dict
    /// (`raw.get("text") or ""`, `float(raw.get("score") or 0.0)`, ...).
    pub fn from_value(v: &Value) -> RawLine {
        let num = |k: &str| v.get(k).filter(|x| x.truthy()).and_then(Value::as_f64).unwrap_or(0.0);
        let quad = v
            .get("quad")
            .and_then(Value::as_array)
            .map(|pts| {
                pts.iter()
                    .filter_map(|p| {
                        let p = p.as_array()?;
                        Some([p.first()?.as_f64()?, p.get(1)?.as_f64()?])
                    })
                    .collect()
            })
            .unwrap_or_default();
        RawLine {
            quad,
            score: num("score"),
            text: match v.get("text") {
                Some(Value::Str(s)) => s.clone(),
                Some(x) if x.truthy() => x.to_string(),
                _ => String::new(),
            },
            conf: num("conf"),
            vertical: v.get("vertical").is_some_and(Value::truthy),
            angle: num("angle"),
            char_confs: v
                .get("char_confs")
                .and_then(Value::as_array)
                .map(|cs| cs.iter().filter_map(Value::as_f64).collect())
                .unwrap_or_default(),
        }
    }
}

impl RawPage {
    /// A page as the reader hands it over.
    pub fn new(width: i64, height: i64, lines: Vec<RawLine>) -> RawPage {
        RawPage { format: FORMAT_ID.to_string(), width, height, detector: None, lines }
    }

    /// `ppocr.page_to_json(lines, width, height, detector=...)`: the form the
    /// layout runs on.
    pub fn rounded(&self) -> RawPage {
        RawPage {
            format: FORMAT_ID.to_string(),
            width: self.width,
            height: self.height,
            detector: self.detector.clone(),
            lines: self.lines.iter().map(RawLine::rounded).collect(),
        }
    }

    /// The page as an ordered JSON object, keys in `page_to_json`'s order.
    pub fn to_value(&self) -> Value {
        let mut items = vec![
            ("format".to_string(), Value::Str(self.format.clone())),
            ("width".to_string(), Value::Int(self.width)),
            ("height".to_string(), Value::Int(self.height)),
        ];
        if let Some(d) = &self.detector {
            items.push(("detector".to_string(), d.clone()));
        }
        items.push(("lines".to_string(), Value::Array(self.lines.iter().map(RawLine::to_value).collect())));
        Value::Object(items)
    }

    /// Read a raw page JSON value leniently (extra keys ignored).
    pub fn from_value(v: &Value) -> RawPage {
        let int = |k: &str| v.get(k).filter(|x| x.truthy()).and_then(Value::as_f64).unwrap_or(0.0) as i64;
        RawPage {
            format: v.get("format").and_then(Value::as_str).unwrap_or(FORMAT_ID).to_string(),
            width: int("width"),
            height: int("height"),
            detector: v.get("detector").cloned(),
            lines: v
                .get("lines")
                .and_then(Value::as_array)
                .map(|ls| ls.iter().map(RawLine::from_value).collect())
                .unwrap_or_default(),
        }
    }
}
