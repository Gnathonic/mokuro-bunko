//! The preprocessing of the text-box recognizers, **manga-ocr-charspot-small** and
//! **manga-ocr-nar-small** (muscgab), shared by the ONNX Runtime route
//! (`bunko-engines`, CPU) and the libtorch route (`bunko-torch`, GPU), so both build
//! bit-identical tensors. It follows the pre- and post-processing of the Apache-2.0
//! inference code the models are published with.
//!
//! The crop is scaled to fit a grid (aspect kept, Pillow's bicubic), put in the grid's
//! top-left corner and the rest filled with the median colour of the scaled crop's
//! border.

use std::ops::Range;
use std::path::Path;

use crate::resample::{Filter, resize};
use crate::{Rgb, VlmError};

/// charspot: the side of a grid cell in pixels, and how many cells a crop may take.
pub const CHARSPOT_CELL: usize = 16;
pub const CHARSPOT_BUDGET: usize = 256;
/// charspot's classes: the vocabulary padded to this many, then the `!?` glyphs.
pub const CHARSPOT_CLASSES: usize = 6818;
/// nar-small: the side of a patch, how many patches a crop may take, and the output
/// positions (the last may be the end mark).
pub const NAR_PATCH: usize = 14;
pub const NAR_BUDGET: usize = 324;
pub const NAR_SLOTS: usize = 161;
/// The normalisation, digit for digit as the model's package writes it.
pub const NAR_MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
#[allow(clippy::excessive_precision)]
pub const NAR_STD: [f32; 3] = [0.268_629_54, 0.261_302_58, 0.275_777_11];

fn asset(what: &str, path: &Path, e: impl std::fmt::Display) -> VlmError {
    VlmError::Asset(format!("{what} {}: {e}", path.display()))
}

/// The `characters` list of a vocabulary file.
pub fn load_characters(path: &Path) -> Result<Vec<String>, VlmError> {
    let text =
        std::fs::read_to_string(path).map_err(|e| VlmError::Io(path.display().to_string(), e))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| asset("vocabulary", path, e))?;
    let chars: Option<Vec<String>> = value["characters"].as_array().map(|a| {
        a.iter()
            .map(|c| c.as_str().unwrap_or_default().to_string())
            .collect()
    });
    chars
        .filter(|c| !c.is_empty())
        .ok_or_else(|| asset("vocabulary", path, "no characters"))
}

/// The largest scale at which a `height` x `width` crop, cut into 14 px cells, takes
/// no more than `budget` cells: `(scale, rows, columns)`. Both models size their input
/// with this search (charspot then draws 16 px cells).
pub fn fit_grid(height: usize, width: usize, budget: usize) -> (f64, usize, usize) {
    let cells = |v: usize, s: f64| (v as f64 * s / 14.0).ceil() as usize;
    let (mut lo, mut hi) = (1e-4f64, 1000.0f64);
    for _ in 0..60 {
        let mid = (lo + hi) / 2.0;
        if cells(height, mid) * cells(width, mid) <= budget {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo, cells(height, lo), cells(width, lo))
}

/// Scale `crop` to `scaled` and put it in the top-left corner of a `canvas` sized
/// image filled with the median colour of the scaled crop's border.
pub fn on_canvas(crop: &Rgb, scaled: (usize, usize), canvas: (usize, usize)) -> Rgb {
    let (rw, rh) = scaled;
    let small = resize(crop, rw, rh, Filter::Bicubic);
    // The top and bottom rows and the left and right columns, corners twice.
    let mut fill = [0u8; 3];
    for (c, out) in fill.iter_mut().enumerate() {
        let mut border: Vec<u8> = Vec::with_capacity(2 * (rw + rh));
        for y in [0, rh - 1] {
            border.extend((0..rw).map(|x| small.data[(y * rw + x) * 3 + c]));
        }
        for x in [0, rw - 1] {
            border.extend((0..rh).map(|y| small.data[(y * rw + x) * 3 + c]));
        }
        border.sort_unstable();
        // An even count: the mean of the two middle values, cut to a whole number.
        let mid = border.len() / 2;
        *out = ((u16::from(border[mid - 1]) + u16::from(border[mid])) / 2) as u8;
    }
    let (cw, ch) = canvas;
    let mut data = Vec::with_capacity(cw * ch * 3);
    for _ in 0..cw * ch {
        data.extend_from_slice(&fill);
    }
    for y in 0..rh {
        let row = &small.data[y * rw * 3..(y + 1) * rw * 3];
        data[y * cw * 3..y * cw * 3 + rw * 3].copy_from_slice(row);
    }
    Rgb {
        width: cw,
        height: ch,
        data,
    }
}

fn round(x: f64) -> usize {
    x.round_ties_even().max(0.0) as usize
}

/// Whether a crop has no pixels (a block box of zero width or height): it reads `""`
/// without a model call.
pub fn empty(crop: &Rgb) -> bool {
    crop.width == 0 || crop.height == 0
}

// --- charspot ----------------------------------------------------------------

/// charspot's input for a crop: the canvas, and the factors that take canvas pixels
/// back to crop pixels (`x`, `y`).
pub fn charspot_canvas(crop: &Rgb) -> (Rgb, (f32, f32)) {
    let (s, rows, cols) = fit_grid(crop.height, crop.width, CHARSPOT_BUDGET);
    let s = s * CHARSPOT_CELL as f64 / 14.0;
    let rw = round(crop.width as f64 * s).clamp(1, cols * CHARSPOT_CELL);
    let rh = round(crop.height as f64 * s).clamp(1, rows * CHARSPOT_CELL);
    let canvas = on_canvas(crop, (rw, rh), (cols * CHARSPOT_CELL, rows * CHARSPOT_CELL));
    let back = (
        (crop.width as f64 / rw as f64) as f32,
        (crop.height as f64 / rh as f64) as f32,
    );
    (canvas, back)
}

// --- nar-small ---------------------------------------------------------------

/// nar-small's inputs for a crop: the patches `[324, 588]` (each a 14 x 14 patch, red
/// plane first, normalised), which of them are crop (`V`), their rows and columns, and
/// the grid's size `(rows, columns)`.
#[derive(Clone, Debug, PartialEq)]
pub struct NarInput {
    pub patches: Vec<f32>,
    pub valid: Vec<f32>,
    pub rows: Vec<i32>,
    pub cols: Vec<i32>,
    pub grid: (usize, usize),
}

/// nar-small's inputs for one non-empty crop.
pub fn nar_input(crop: &Rgb) -> NarInput {
    let (s, rows, cols) = fit_grid(crop.height, crop.width, NAR_BUDGET);
    let rw = round(crop.width as f64 * s).clamp(1, cols * NAR_PATCH);
    let rh = round(crop.height as f64 * s).clamp(1, rows * NAR_PATCH);
    let canvas = on_canvas(crop, (rw, rh), (cols * NAR_PATCH, rows * NAR_PATCH));
    let feat = 3 * NAR_PATCH * NAR_PATCH;
    let mut patches = vec![0.0f32; NAR_BUDGET * feat];
    let (mut valid, mut prow, mut pcol) = (
        vec![0.0f32; NAR_BUDGET],
        vec![0i32; NAR_BUDGET],
        vec![0i32; NAR_BUDGET],
    );
    for r in 0..rows {
        for c in 0..cols {
            let n = r * cols + c;
            valid[n] = 1.0;
            prow[n] = r as i32;
            pcol[n] = c as i32;
            let patch = &mut patches[n * feat..(n + 1) * feat];
            for ch in 0..3 {
                for py in 0..NAR_PATCH {
                    for px in 0..NAR_PATCH {
                        let v = canvas.data
                            [((r * NAR_PATCH + py) * canvas.width + c * NAR_PATCH + px) * 3 + ch];
                        patch[(ch * NAR_PATCH + py) * NAR_PATCH + px] =
                            (f32::from(v) / 255.0 - NAR_MEAN[ch]) / NAR_STD[ch];
                    }
                }
            }
        }
    }
    NarInput {
        patches,
        valid,
        rows: prow,
        cols: pcol,
        grid: (rows, cols),
    }
}

/// A batch of nar-small inputs, row-major: `patches` `[len, 324, 588]`, `valid`,
/// `rows`, `cols` `[len, 324]`, and each crop's grid as `heights` / `widths` `[len]`.
#[derive(Clone, Debug, PartialEq)]
pub struct NarBatch {
    pub len: usize,
    pub patches: Vec<f32>,
    pub valid: Vec<f32>,
    pub rows: Vec<i32>,
    pub cols: Vec<i32>,
    pub heights: Vec<i32>,
    pub widths: Vec<i32>,
}

/// The inputs back to back, in their order.
pub fn nar_batch(inputs: &[&NarInput]) -> NarBatch {
    let len = inputs.len();
    let feat = 3 * NAR_PATCH * NAR_PATCH;
    let mut b = NarBatch {
        len,
        patches: Vec::with_capacity(len * NAR_BUDGET * feat),
        valid: Vec::with_capacity(len * NAR_BUDGET),
        rows: Vec::with_capacity(len * NAR_BUDGET),
        cols: Vec::with_capacity(len * NAR_BUDGET),
        heights: Vec::with_capacity(len),
        widths: Vec::with_capacity(len),
    };
    for i in inputs {
        b.patches.extend_from_slice(&i.patches);
        b.valid.extend_from_slice(&i.valid);
        b.rows.extend_from_slice(&i.rows);
        b.cols.extend_from_slice(&i.cols);
        b.heights.push(i.grid.0 as i32);
        b.widths.push(i.grid.1 as i32);
    }
    b
}

/// The text of one crop's ids: the end mark is the id after the last character and the
/// text stops there; an id outside the vocabulary writes nothing.
pub fn nar_text(ids: &[i64], characters: &[String]) -> String {
    let end = characters.len() as i64;
    ids.iter()
        .take_while(|&&i| i != end)
        .filter_map(|&i| usize::try_from(i).ok().and_then(|i| characters.get(i)))
        .map(String::as_str)
        .collect()
}

/// `0..n` cut into consecutive ranges of at most `max` (a package's largest batch).
pub fn batches(n: usize, max: usize) -> Vec<Range<usize>> {
    let max = max.max(1);
    (0..n)
        .step_by(max)
        .map(|start| start..(start + max).min(n))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nar_text_stops_at_the_end_id() {
        let chars: Vec<String> = ["a", "b"].map(String::from).to_vec();
        assert_eq!(nar_text(&[1, 7, 0, 2, 1], &chars), "ba");
        assert_eq!(nar_text(&[], &chars), "");
    }

    #[test]
    fn batches_split_at_the_package_max() {
        assert_eq!(batches(130, 64), vec![0..64, 64..128, 128..130]);
        assert_eq!(batches(64, 64), vec![0..64]);
        assert!(batches(0, 64).is_empty());
    }

    #[test]
    fn a_batch_is_its_inputs_back_to_back() {
        let a = nar_input(&Rgb {
            width: 20,
            height: 40,
            data: vec![200; 20 * 40 * 3],
        });
        let b = nar_input(&Rgb {
            width: 90,
            height: 30,
            data: vec![10; 90 * 30 * 3],
        });
        let nb = nar_batch(&[&a, &b]);
        assert_eq!(nb.len, 2);
        assert_eq!(nb.patches.len(), 2 * NAR_BUDGET * 3 * NAR_PATCH * NAR_PATCH);
        assert_eq!(&nb.patches[..a.patches.len()], &a.patches[..]);
        assert_eq!(&nb.valid[NAR_BUDGET..], &b.valid[..]);
        assert_eq!(nb.heights, vec![a.grid.0 as i32, b.grid.0 as i32]);
        assert_eq!(nb.widths, vec![a.grid.1 as i32, b.grid.1 as i32]);
    }

    #[test]
    fn the_grid_is_the_largest_that_fits_the_budget() {
        // A square crop fills the budget exactly: 16 x 16 cells, 18 x 18 patches.
        let (_, rows, cols) = fit_grid(100, 100, CHARSPOT_BUDGET);
        assert_eq!((rows, cols), (16, 16));
        let (_, rows, cols) = fit_grid(100, 100, NAR_BUDGET);
        assert_eq!((rows, cols), (18, 18));
        // A column four times as tall as wide.
        let (s, rows, cols) = fit_grid(400, 100, CHARSPOT_BUDGET);
        assert!(
            rows * cols <= CHARSPOT_BUDGET && rows > 3 * cols,
            "{rows}x{cols}"
        );
        // One step further would not fit.
        let cells = |v: usize, s: f64| (v as f64 * s / 14.0).ceil() as usize;
        assert!(cells(400, s * 1.01) * cells(100, s * 1.01) > CHARSPOT_BUDGET);
    }

    #[test]
    fn the_canvas_holds_the_crop_in_the_corner_on_its_border_median() {
        // 4 x 2, all border: red 10/20/30/40 on top, 50/60/70/80 below.
        let mut data = Vec::new();
        for v in [10u8, 20, 30, 40, 50, 60, 70, 80] {
            data.extend_from_slice(&[v, 0, 255]);
        }
        let crop = Rgb {
            width: 4,
            height: 2,
            data,
        };
        let canvas = on_canvas(&crop, (4, 2), (6, 3));
        assert_eq!((canvas.width, canvas.height), (6, 3));
        assert_eq!(canvas.px(0, 0), [10, 0, 255]);
        assert_eq!(canvas.px(3, 1), [80, 0, 255]);
        // Rows 10..40 and 50..80, columns 10,50 and 40,80: the two middle values of the
        // twelve are 40 and 50.
        assert_eq!(canvas.px(5, 0), [45, 0, 255]);
        assert_eq!(canvas.px(0, 2), [45, 0, 255]);
    }

    #[test]
    fn charspot_boxes_come_back_in_crop_pixels() {
        let crop = Rgb::new(300, 600);
        let (canvas, (fx, fy)) = charspot_canvas(&crop);
        assert_eq!(canvas.width % CHARSPOT_CELL, 0);
        assert_eq!(canvas.height % CHARSPOT_CELL, 0);
        assert!(canvas.width / CHARSPOT_CELL * (canvas.height / CHARSPOT_CELL) <= CHARSPOT_BUDGET);
        // The crop keeps its shape and lies inside the canvas: a canvas pixel is the
        // same number of crop pixels both ways, within the rounding of the scaled size.
        assert!((fx / fy - 1.0).abs() < 0.02, "{fx} {fy}");
        assert!(300.0 / fx <= canvas.width as f32 + 0.5);
        assert!(600.0 / fy <= canvas.height as f32 + 0.5);
    }

    #[test]
    fn nar_patches_are_laid_out_row_by_row_and_padded() {
        let crop = Rgb::new(140, 70);
        let input = nar_input(&crop);
        let (rows, cols) = input.grid;
        assert!(
            rows * cols <= NAR_BUDGET && cols == 2 * rows,
            "{rows}x{cols}"
        );
        let n = rows * cols;
        assert_eq!(input.valid[..n], vec![1.0; n][..]);
        assert!(input.valid[n..].iter().all(|v| *v == 0.0));
        assert_eq!((input.rows[cols], input.cols[cols]), (1, 0));
        assert_eq!(
            (input.rows[cols - 1], input.cols[cols - 1]),
            (0, cols as i32 - 1)
        );
        // A black pixel, normalised, red first.
        assert!((input.patches[0] - (0.0 - NAR_MEAN[0]) / NAR_STD[0]).abs() < 1e-6);
        assert!(
            (input.patches[2 * NAR_PATCH * NAR_PATCH] - (0.0 - NAR_MEAN[2]) / NAR_STD[2]).abs()
                < 1e-6
        );
        assert!(input.patches[n * 588..].iter().all(|v| *v == 0.0));
    }
}
