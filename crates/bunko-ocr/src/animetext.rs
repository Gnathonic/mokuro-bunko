//! AnimeText YOLO12 (deepghs; GPL-3.0 weights) as a block detector.
//!
//! Written against the model's tensors: `images` is float32 `[1, 3, H, W]` (the page
//! scaled so its longer side is 640, on a grey canvas whose sides are multiples of 32,
//! red channel first, values 0..1) and `output0` is float32 `[1, 5, N]`: per candidate
//! the box's centre, width and height in input pixels, and its score. There is one
//! class: text.

use std::path::Path;
use std::sync::Arc;

use crate::blocks::{BlockDetector, TextBlock, drop_containers, drop_parts, suppress};
use crate::error::{Error, Result};
use crate::image::{BgrImage, resize_linear};
use crate::lines::{DetectorInfo, DetectorPass};
use crate::py;
use crate::runtime::{Model, RuntimeOptions, TensorBuf};

/// The longer side of the network's input.
pub const SIDE: usize = 640;
const STRIDE: usize = 32;
/// The canvas colour around the page (the value the network was trained with).
const PAD: f32 = 114.0 / 255.0;
/// A candidate is a block from this score up.
pub const MIN_SCORE: f32 = 0.3;
/// Of two candidates overlapping by more than this, the better one stays.
pub const OVERLAP: f32 = 0.5;
/// A box holding two or more others (80% of each) is a box around several blocks.
const CONTAINER_PARTS: usize = 2;
const CONTAINER_INSIDE: f32 = 0.8;
/// Blocks thinner than this many page pixels are dropped.
const MIN_SIDE: f32 = 4.0;

/// The loaded detector.
#[derive(Debug)]
pub struct AnimeText {
    model: Arc<Model>,
}

impl AnimeText {
    pub fn load(path: &Path, opts: &RuntimeOptions) -> Result<Self> {
        Ok(Self {
            model: Arc::new(Model::load(path, opts)?),
        })
    }
}

/// The page shrunk to `(w, h)` by averaging the pixels each new pixel covers (what a
/// detector wants of a page four times its input), or stretched bilinearly when the
/// page is the smaller one.
fn fit(bgr: &BgrImage, w: usize, h: usize) -> BgrImage {
    let (sw, sh) = (bgr.width(), bgr.height());
    if w >= sw || h >= sh {
        return resize_linear(bgr.view(), w, h);
    }
    let (fx, fy) = (sw as f64 / w as f64, sh as f64 / h as f64);
    let src = bgr.as_raw();
    let mut out = vec![0u8; w * h * 3];
    // Per axis: the source span of each destination pixel, with the share of its
    // first and last source pixels.
    let spans = |n: usize, f: f64, limit: usize| -> Vec<(usize, usize, f64, f64)> {
        (0..n)
            .map(|i| {
                let (a, b) = (i as f64 * f, ((i + 1) as f64 * f).min(limit as f64));
                let (i0, i1) = (a.floor() as usize, (b.ceil() as usize).min(limit).max(1));
                (i0, i1, 1.0 - (a - i0 as f64), 1.0 - (i1 as f64 - b))
            })
            .collect()
    };
    let (xs, ys) = (spans(w, fx, sw), spans(h, fy, sh));
    for (y, &(y0, y1, wy0, wy1)) in ys.iter().enumerate() {
        for (x, &(x0, x1, wx0, wx1)) in xs.iter().enumerate() {
            let (mut sum, mut weight) = ([0.0f64; 3], 0.0f64);
            for sy in y0..y1 {
                let wy = if sy == y0 {
                    wy0
                } else if sy + 1 == y1 {
                    wy1
                } else {
                    1.0
                };
                for sx in x0..x1 {
                    let wx = if sx == x0 {
                        wx0
                    } else if sx + 1 == x1 {
                        wx1
                    } else {
                        1.0
                    };
                    let px = &src[(sy * sw + sx) * 3..][..3];
                    for c in 0..3 {
                        sum[c] += f64::from(px[c]) * wx * wy;
                    }
                    weight += wx * wy;
                }
            }
            for c in 0..3 {
                out[(y * w + x) * 3 + c] = (sum[c] / weight.max(f64::MIN_POSITIVE)).round() as u8;
            }
        }
    }
    BgrImage::from_raw(w, h, out).unwrap_or_else(|| BgrImage::zeros(w, h))
}

/// What the page becomes for the network: `(scale, scaled width, scaled height, canvas
/// width, canvas height)`.
pub fn input_size(width: usize, height: usize) -> (f64, usize, usize, usize, usize) {
    let scale = SIDE as f64 / width.max(height).max(1) as f64;
    let fit = |v: usize| (py::round_int(v as f64 * scale).max(1) as usize).min(SIDE);
    let (w, h) = (fit(width), fit(height));
    (
        scale,
        w,
        h,
        w.div_ceil(STRIDE) * STRIDE,
        h.div_ceil(STRIDE) * STRIDE,
    )
}

/// The network's candidates (`[5, n]` row-major: centre x, centre y, width, height,
/// score, in input pixels) as the page's blocks: scored, in page pixels, the better of
/// two overlapping ones, no box around several blocks and no box of a block's part.
pub fn blocks_from_output(
    out: &[f32],
    n: usize,
    scale: f64,
    page: (usize, usize),
) -> Vec<TextBlock> {
    let (pw, ph) = (page.0 as f32, page.1 as f32);
    let s = scale as f32;
    let mut found = Vec::new();
    for i in 0..n {
        let score = out[4 * n + i];
        if score < MIN_SCORE {
            continue;
        }
        let (cx, cy, w, h) = (out[i], out[n + i], out[2 * n + i], out[3 * n + i]);
        let bbox = [
            ((cx - w / 2.0) / s).clamp(0.0, pw),
            ((cy - h / 2.0) / s).clamp(0.0, ph),
            ((cx + w / 2.0) / s).clamp(0.0, pw),
            ((cy + h / 2.0) / s).clamp(0.0, ph),
        ];
        if bbox[2] - bbox[0] >= MIN_SIDE && bbox[3] - bbox[1] >= MIN_SIDE {
            found.push(TextBlock { bbox, score });
        }
    }
    drop_parts(
        drop_containers(suppress(found, OVERLAP), CONTAINER_PARTS, CONTAINER_INSIDE),
        CONTAINER_INSIDE,
    )
}

impl BlockDetector for AnimeText {
    fn detect(&self, bgr: &BgrImage) -> Result<(Vec<TextBlock>, DetectorInfo)> {
        let (pw, ph) = (bgr.width(), bgr.height());
        let mut info = DetectorInfo {
            side: SIDE as u32,
            tile: "off".into(),
            ..Default::default()
        };
        if pw == 0 || ph == 0 {
            return Ok((Vec::new(), info));
        }
        let (scale, w, h, cw, ch) = input_size(pw, ph);
        let small = fit(bgr, w, h);
        let plane = cw * ch;
        let mut x = vec![PAD; 3 * plane];
        for (row, line) in small.as_raw().chunks_exact(w * 3).enumerate() {
            for (col, px) in line.as_chunks::<3>().0.iter().enumerate() {
                let at = row * cw + col;
                // Blue, green, red in the page; red first in the tensor.
                x[at] = f32::from(px[2]) / 255.0;
                x[plane + at] = f32::from(px[1]) / 255.0;
                x[2 * plane + at] = f32::from(px[0]) / 255.0;
            }
        }
        let out = self.model.run_named(
            vec![("images", TensorBuf::f32(&[1, 3, ch, cw], x))],
            &["output0"],
        )?;
        let (n, data) = out
            .first()
            .and_then(|t| match t.shape.as_slice() {
                [1, 5, n] => t.as_f32().map(|d| (*n, d)),
                _ => None,
            })
            .ok_or_else(|| {
                Error::ModelOutput(format!(
                    "AnimeText output has shape {:?}",
                    out.first().map(|t| t.shape.clone())
                ))
            })?;
        let blocks = blocks_from_output(data, n, scale, (pw, ph));
        info.passes.push(DetectorPass {
            scale: py::round_to(scale, 4),
            tiles: 1,
            lines: blocks.len(),
            why: "animetext".into(),
        });
        Ok((blocks, info))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_fits_640_on_a_canvas_of_32s() {
        assert_eq!(input_size(1709, 2800), (640.0 / 2800.0, 391, 640, 416, 640));
        assert_eq!(input_size(640, 320), (1.0, 640, 320, 640, 320));
        // A small page is scaled up.
        let (scale, w, h, cw, ch) = input_size(100, 50);
        assert_eq!((scale, w, h, cw, ch), (6.4, 640, 320, 640, 320));
    }

    #[test]
    fn candidates_become_blocks_in_page_pixels() {
        // Three candidates at half scale: a block, a weaker copy of it, and noise.
        let n = 3;
        let mut out = vec![0.0f32; 5 * n];
        for (i, c) in [
            [100.0, 200.0, 40.0, 100.0, 0.9],
            [102.0, 200.0, 40.0, 100.0, 0.5],
            [300.0, 300.0, 50.0, 50.0, 0.1],
        ]
        .iter()
        .enumerate()
        {
            for k in 0..5 {
                out[k * n + i] = c[k];
            }
        }
        let blocks = blocks_from_output(&out, n, 0.5, (1000, 1000));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].bbox, [160.0, 300.0, 240.0, 500.0]);
        assert_eq!(blocks[0].score, 0.9);
    }

    #[test]
    fn shrinking_averages_what_each_pixel_covers() {
        // 4 x 2 → 2 x 1: each new pixel is the mean of a 2 x 2 square.
        let mut data = Vec::new();
        for v in [0u8, 100, 200, 40, 20, 80, 0, 160] {
            data.extend_from_slice(&[v, v, v]);
        }
        let page = BgrImage::from_raw(4, 2, data).unwrap();
        let small = fit(&page, 2, 1);
        assert_eq!(small.as_raw(), &[50, 50, 50, 100, 100, 100]);
    }
}
