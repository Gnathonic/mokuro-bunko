//! comic-text-detector (dmMaze; GPL-3.0 weights) as a line detector.
//!
//! The network has three heads: text blocks, a text mask and a DBNet line map. Only the
//! line map is used here. Its lines go through Bunko's own CTC read, page passes and
//! layout, as PP-OCR's do, so nothing of comic-text-detector's own block grouping or
//! ordering is involved.
//!
//! This file is written against the model's tensors, not its source: `images` is
//! float32 `[1, 3, 1024, 1024]` (the page scaled to fit and placed in the top-left
//! corner on black, blue channel first, values 0..1) and `det` is float32
//! `[1, 2, 1024, 1024]`, whose first channel is the probability that a pixel lies
//! inside a text line's shrunk kernel.

use std::path::Path;
use std::sync::Arc;

use super::detect::{DbParams, ProbMap, db_postprocess_with};
use super::{LineDetector, Quad};
use crate::error::{Error, Result};
use crate::image::{BgrImage, resize_linear};
use crate::lines::{DetectorInfo, DetectorPass};
use crate::py;
use crate::runtime::{Model, RuntimeOptions, TensorBuf};

/// The network's fixed input side.
pub const SIDE: usize = 1024;
const INPUT: &str = "images";
const LINE_MAP: &str = "det";

/// The thresholds the model is run with by its authors: text above 0.3, boxes from a
/// mean of 0.6, grown back by 1.5.
pub const CTD_DB: DbParams = DbParams {
    thresh: 0.3,
    box_thresh: 0.6,
    unclip_ratio: 1.5,
    min_side: 2.0,
    max_candidates: 1000,
};

/// The loaded detector; share it between workers with an [`Arc`].
#[derive(Debug)]
pub struct Ctd {
    model: Arc<Model>,
}

impl Ctd {
    pub fn load(path: &Path, opts: &RuntimeOptions) -> Result<Self> {
        Ok(Self {
            model: Arc::new(Model::load(path, opts)?),
        })
    }
}

/// `(width, height)` the page takes inside the square input: scaled to fit, both ways.
pub fn fitted_size(width: usize, height: usize) -> (usize, usize) {
    let r = (SIDE as f64 / width.max(1) as f64).min(SIDE as f64 / height.max(1) as f64);
    let fit = |v: usize| (py::round_int(v as f64 * r).max(1) as usize).min(SIDE);
    (fit(width), fit(height))
}

/// `[3, SIDE, SIDE]`: the page at `size` in the top-left corner, zeros elsewhere.
fn input_tensor(bgr: &BgrImage, size: (usize, usize)) -> Vec<f32> {
    let resized = resize_linear(bgr.view(), size.0, size.1);
    let plane = SIDE * SIDE;
    let mut x = vec![0.0f32; 3 * plane];
    for (y, row) in resized.as_raw().chunks_exact(size.0 * 3).enumerate() {
        for (col, px) in row.as_chunks::<3>().0.iter().enumerate() {
            for c in 0..3 {
                x[c * plane + y * SIDE + col] = px[c] as f32 / 255.0;
            }
        }
    }
    x
}

/// The line quads of a `SIDE` x `SIDE` line map (row-major) in page pixels. `size` is
/// what the page took of the input ([`fitted_size`]): the padding beside it is not page
/// and is left out.
pub fn lines_from_map(map: &[f32], size: (usize, usize), page: (usize, usize)) -> Vec<(Quad, f64)> {
    let (nw, nh) = size;
    let mut region = Vec::with_capacity(nw * nh);
    for y in 0..nh {
        region.extend_from_slice(&map[y * SIDE..y * SIDE + nw]);
    }
    let prob = ProbMap {
        width: nw,
        height: nh,
        data: &region,
    };
    let (sx, sy) = (page.0 as f32 / nw as f32, page.1 as f32 / nh as f32);
    db_postprocess_with(&prob, &CTD_DB)
        .into_iter()
        .map(|(mut q, score)| {
            for p in &mut q {
                p[0] *= sx;
                p[1] *= sy;
            }
            (q, score)
        })
        .collect()
}

impl LineDetector for Ctd {
    fn detect(&self, bgr: &BgrImage) -> Result<(Vec<(Quad, f64)>, DetectorInfo)> {
        let (w, h) = (bgr.width(), bgr.height());
        let mut info = DetectorInfo {
            side: SIDE as u32,
            tile: "off".into(),
            ..Default::default()
        };
        if w == 0 || h == 0 {
            return Ok((Vec::new(), info));
        }
        let size = fitted_size(w, h);
        let out = self.model.run_named(
            vec![(
                INPUT,
                TensorBuf::f32(&[1, 3, SIDE, SIDE], input_tensor(bgr, size)),
            )],
            &[LINE_MAP],
        )?;
        let map = out
            .first()
            .filter(|t| matches!(t.shape.as_slice(), [1, c, s1, s2] if *c >= 1 && *s1 == SIDE && *s2 == SIDE))
            .and_then(|t| t.as_f32())
            .ok_or_else(|| {
                Error::ModelOutput(format!(
                    "comic-text-detector line map has shape {:?}",
                    out.first().map(|t| t.shape.clone())
                ))
            })?;
        let lines = lines_from_map(&map[..SIDE * SIDE], size, (w, h));
        info.passes.push(DetectorPass {
            scale: py::round_to(size.0 as f64 / w as f64, 4),
            tiles: 1,
            lines: lines.len(),
            why: "ctd".into(),
        });
        Ok((lines, info))
    }
}

#[cfg(test)]
mod tests {
    use super::super::geometry::bounds;
    use super::*;

    #[test]
    fn the_page_fits_the_square_both_ways() {
        assert_eq!(fitted_size(1777, 2800), (650, 1024));
        assert_eq!(fitted_size(2048, 1024), (1024, 512));
        // A small page is scaled up, as a large one is scaled down.
        assert_eq!(fitted_size(512, 256), (1024, 512));
        assert_eq!(fitted_size(1, 5000), (1, 1024));
    }

    #[test]
    fn a_bar_on_the_map_is_a_line_in_page_pixels() {
        // A page twice as wide as tall takes the top half of the input, at half scale.
        let (page, size) = ((2048, 1024), (1024, 512));
        let mut map = vec![0.0f32; SIDE * SIDE];
        for y in 100..112 {
            for x in 200..400 {
                map[y * SIDE + x] = 0.95;
            }
        }
        // The same bar in the padding below the page: not page, never a line.
        for y in 700..712 {
            for x in 200..400 {
                map[y * SIDE + x] = 0.95;
            }
        }
        let lines = lines_from_map(&map, size, page);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let (quad, score) = lines[0];
        assert!((score - 0.95).abs() < 1e-5);
        let (x0, y0, x1, y1) = bounds(&quad);
        // Grown back from the kernel, then doubled into page pixels.
        assert!(x0 < 400.0 && x0 > 360.0, "{x0}");
        assert!(x1 > 800.0 && x1 < 840.0, "{x1}");
        assert!(y0 < 200.0 && y0 > 160.0, "{y0}");
        assert!(y1 > 222.0 && y1 < 262.0, "{y1}");
    }

    #[test]
    fn a_faint_bar_is_no_line() {
        let mut map = vec![0.0f32; SIDE * SIDE];
        for y in 100..112 {
            for x in 200..400 {
                map[y * SIDE + x] = 0.5; // text, but under the box threshold
            }
        }
        assert!(lines_from_map(&map, (1024, 1024), (1024, 1024)).is_empty());
    }

    #[test]
    fn the_tensor_is_the_page_in_the_corner_on_black() {
        let page = BgrImage::from_raw(2, 1, vec![255, 0, 51, 0, 255, 102]).unwrap();
        let x = input_tensor(&page, (2, 1));
        let plane = SIDE * SIDE;
        assert_eq!(x.len(), 3 * plane);
        // Blue plane first, then green, then red.
        assert_eq!((x[0], x[1]), (1.0, 0.0));
        assert_eq!((x[plane], x[plane + 1]), (0.0, 1.0));
        assert!((x[2 * plane] - 0.2).abs() < 1e-6 && (x[2 * plane + 1] - 0.4).abs() < 1e-6);
        assert_eq!(x[2], 0.0);
        assert_eq!(x[SIDE], 0.0);
    }
}
