//! DBNet post-processing, the dense-page pass policy and tiling (`ppocr.py:504-866`,
//! spec §3.2–§3.6). Everything here is pure; the network runs in [`super::PpOcr`].

use super::contours::find_contours;
use super::fillpoly::fill_poly;
use super::geometry::{
    Quad, bounds, convex_intersection_area, order_quad, polygon_area, quad_size, rect_to_quad,
    same_line, unclip_distance, union_quad,
};
use super::minrect::{bounding_rect, min_area_rect};
use crate::image::{ImageView, resize_linear};
use crate::py;

pub const DEFAULT_SIDE: u32 = 1120;
pub const SIDE_MULTIPLE: usize = 32;
pub const MAX_UPSCALE: f64 = 1.5;
pub const IMAGENET_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
pub const IMAGENET_STD: [f32; 3] = [0.229, 0.224, 0.225];
pub const DB_THRESH: f32 = 0.15;
pub const DB_BOX_THRESH: f64 = 0.25;
pub const DB_UNCLIP_RATIO: f64 = 1.4;
pub const DB_MIN_SIDE: f64 = 3.0;
pub const DB_MAX_CANDIDATES: usize = 3000;
pub const FINE_THICKNESS_PX: f64 = 24.0;
pub const DENSE_LENGTH_RATIO: f64 = 10.0;
pub const FINE_MIN_LINES: usize = 6;
pub const FUSED_THICKNESS_RATIO: f64 = 1.7;
pub const FUSED_MIN_COUNT: usize = 2;
pub const FINE_TARGET_THICKNESS_PX: f64 = 32.0;
pub const MAX_DETECTOR_PIXELS: usize = 2816 * 2048;
pub const TILE_SIZE: f64 = 1536.0;
pub const TILE_OVERLAP: f64 = 256.0;
pub const TILE_EDGE_MARGIN: f64 = 8.0;
pub const STRIP_ASPECT: f64 = 2.5;

/// `(width, height)` the page is resized to: longest side ~`side`, multiples of 32.
pub fn detector_input_size(width: usize, height: usize, side: u32) -> (usize, usize) {
    let scale = (side as f64 / width.max(height) as f64).min(MAX_UPSCALE);
    scaled_size(width, height, scale)
}

/// `_scaled_size`.
pub fn scaled_size(width: usize, height: usize, scale: f64) -> (usize, usize) {
    let m = SIDE_MULTIPLE as f64;
    let w = (py::round_int(width as f64 * scale / m) as usize * SIDE_MULTIPLE).max(SIDE_MULTIPLE);
    let h = (py::round_int(height as f64 * scale / m) as usize * SIDE_MULTIPLE).max(SIDE_MULTIPLE);
    (w, h)
}

/// Resize + ImageNet-normalise (BGR order, RGB-named constants) into `[3, H, W]`.
pub fn detector_tensor(bgr: ImageView<'_>, size: (usize, usize)) -> Vec<f32> {
    let resized = resize_linear(bgr, size.0, size.1);
    let plane = size.0 * size.1;
    let mut x = vec![0.0f32; 3 * plane];
    for (i, px) in resized.as_raw().as_chunks::<3>().0.iter().enumerate() {
        for c in 0..3 {
            x[c * plane + i] = (px[c] as f32 / 255.0 - IMAGENET_MEAN[c]) / IMAGENET_STD[c];
        }
    }
    x
}

/// A probability map in detector pixels.
pub struct ProbMap<'a> {
    pub width: usize,
    pub height: usize,
    pub data: &'a [f32],
}

/// DBNet map → `[(ordered quad, score)]` in map pixels.
pub fn db_postprocess(prob: &ProbMap<'_>) -> Vec<(Quad, f64)> {
    let mask: Vec<bool> = prob.data.iter().map(|&p| p > DB_THRESH).collect();
    let contours = find_contours(&mask, prob.width, prob.height);
    let mut out = Vec::new();
    for contour in contours.iter().take(DB_MAX_CANDIDATES) {
        let r = min_area_rect(contour);
        let (w, h) = (r.w as f64, r.h as f64);
        if w.min(h) < DB_MIN_SIDE {
            continue;
        }
        let score = contour_score(prob, contour);
        if score < DB_BOX_THRESH {
            continue;
        }
        let grow = 2.0 * unclip_distance(w, h, DB_UNCLIP_RATIO);
        let (w, h) = (w + grow, h + grow);
        if w.min(h) < DB_MIN_SIDE + 2.0 {
            continue;
        }
        out.push((
            order_quad(&rect_to_quad(
                r.cx as f64,
                r.cy as f64,
                w,
                h,
                r.angle as f64,
            )),
            score,
        ));
    }
    out
}

/// Mean probability inside the filled contour polygon (`cv2.mean` with mask).
fn contour_score(prob: &ProbMap<'_>, contour: &[[i32; 2]]) -> f64 {
    let (x0, y0, bw, bh) = bounding_rect(contour);
    let (bw, bh) = (bw as usize, bh as usize);
    let mut mask = vec![0u8; bw * bh];
    let shifted: Vec<[i32; 2]> = contour.iter().map(|p| [p[0] - x0, p[1] - y0]).collect();
    fill_poly(&mut mask, bw, bh, &shifted);
    let mut sum = 0.0f64;
    let mut n = 0usize;
    for y in 0..bh {
        let row = (y0 as usize + y) * prob.width + x0 as usize;
        for x in 0..bw {
            if mask[y * bw + x] != 0 {
                sum += prob.data[row + x] as f64;
                n += 1;
            }
        }
    }
    if n == 0 { 0.0 } else { sum / n as f64 }
}

/// Median thickness of a page's long lines (the upper median), if enough.
pub fn dense_median(thick: &[f64], lengths: &[f64]) -> Option<f64> {
    let mut body: Vec<f64> = thick
        .iter()
        .zip(lengths)
        .filter(|(t, l)| **l >= DENSE_LENGTH_RATIO * **t)
        .map(|(t, _)| *t)
        .collect();
    if body.len() < FINE_MIN_LINES {
        return None;
    }
    body.sort_by(f64::total_cmp);
    Some(body[body.len() / 2])
}

/// Why a first pass calls for a finer one, or `None`.
pub fn needs_fine_pass(thick: &[f64], lengths: &[f64]) -> Option<String> {
    let median = dense_median(thick, lengths)?;
    if median < FINE_THICKNESS_PX {
        return Some(format!(
            "median column thickness {median:.1}px < {FINE_THICKNESS_PX:.0}px"
        ));
    }
    let fused = thick
        .iter()
        .zip(lengths)
        .filter(|(t, l)| {
            **l >= DENSE_LENGTH_RATIO * median && **t >= FUSED_THICKNESS_RATIO * median
        })
        .count();
    if fused >= FUSED_MIN_COUNT {
        return Some(format!(
            "{fused} boxes >= {FUSED_THICKNESS_RATIO}x median thickness {median:.1}px"
        ));
    }
    None
}

/// Page → detector scale of the fine pass.
pub fn fine_scale(median: f64, first_scale: f64) -> f64 {
    if median <= 0.0 {
        1.0
    } else {
        (first_scale * FINE_TARGET_THICKNESS_PX / median).min(1.0)
    }
}

/// Overlapping `(x0, y0, x1, y1)` tiles, row-major.
pub fn tile_grid(
    width: usize,
    height: usize,
    tile: usize,
    overlap: usize,
) -> Vec<(usize, usize, usize, usize)> {
    let starts = |extent: usize| -> Vec<usize> {
        if extent <= tile {
            return vec![0];
        }
        let count = ((extent - overlap) as f64 / (tile - overlap) as f64).ceil() as usize;
        let step = (extent - tile) as f64 / (count - 1) as f64;
        (0..count)
            .map(|i| py::round_int(i as f64 * step) as usize)
            .collect()
    };
    let mut out = Vec::new();
    for y in starts(height) {
        for x in starts(width) {
            out.push((x, y, (x + tile).min(width), (y + tile).min(height)));
        }
    }
    out
}

/// A box the tile border cut in a way its neighbour tile repairs.
pub fn clipped_by_tile(
    q: &Quad,
    tile: (usize, usize, usize, usize),
    page: (usize, usize),
    overlap: f64,
    margin: f64,
) -> bool {
    let (x0, y0, x1, y1) = (tile.0 as f64, tile.1 as f64, tile.2 as f64, tile.3 as f64);
    let (w, h) = (page.0 as f64, page.1 as f64);
    let (lo_x, lo_y, hi_x, hi_y) = bounds(q);
    let (reach_x, reach_y) = (hi_x - lo_x, hi_y - lo_y);
    (x0 > 0.0 && lo_x <= x0 + margin && reach_x < overlap / 2.0)
        || (x1 < w && hi_x >= x1 - margin && reach_x < overlap / 2.0)
        || (y0 > 0.0 && lo_y <= y0 + margin && reach_y < overlap / 2.0)
        || (y1 < h && hi_y >= y1 - margin && reach_y < overlap / 2.0)
}

/// Merge per-tile detections into page lines (union-find over different tiles).
pub fn merge_tile_lines(quads: &[Quad], scores: &[f64], tiles: &[usize]) -> Vec<(Quad, f64)> {
    let n = quads.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let bb: Vec<(f64, f64, f64, f64)> = quads.iter().map(bounds).collect();
    for i in 0..n {
        for j in i + 1..n {
            if tiles[i] == tiles[j] {
                continue;
            }
            let (bi, bj) = (bb[i], bb[j]);
            if bi.2 < bj.0 || bj.2 < bi.0 || bi.3 < bj.1 || bj.3 < bi.1 {
                continue;
            }
            if find(&mut parent, i) != find(&mut parent, j) && same_line(&quads[i], &quads[j]) {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                parent[rj] = ri;
            }
        }
    }
    // Groups in order of first member (dict insertion order of roots).
    let mut order: Vec<usize> = Vec::new();
    let mut groups: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups
            .entry(r)
            .or_insert_with(|| {
                order.push(r);
                Vec::new()
            })
            .push(i);
    }
    let mut merged: Vec<(Quad, f64, Vec<usize>)> = Vec::new();
    for r in order {
        let members = &groups[&r];
        let mut seen: Vec<usize> = members.iter().map(|&m| tiles[m]).collect();
        seen.sort_unstable();
        seen.dedup();
        if members.len() == 1 {
            merged.push((quads[members[0]], scores[members[0]], seen));
        } else {
            let lengths: Vec<f64> = members
                .iter()
                .map(|&m| {
                    let (w, h) = quad_size(&quads[m]);
                    w.max(h)
                })
                .collect();
            let ss: Vec<f64> = members.iter().map(|&m| scores[m]).collect();
            let score = py::np_average(&ss, &lengths);
            let qs: Vec<Quad> = members.iter().map(|&m| quads[m]).collect();
            merged.push((union_quad(&qs), score, seen));
        }
    }
    drop_fragments(merged)
        .into_iter()
        .map(|(q, s, _)| (q, s))
        .collect()
}

/// Remove boxes that lie inside a line another tile saw whole.
fn drop_fragments(lines: Vec<(Quad, f64, Vec<usize>)>) -> Vec<(Quad, f64, Vec<usize>)> {
    const INSIDE: f64 = 0.6;
    let areas: Vec<f64> = lines.iter().map(|l| polygon_area(&l.0)).collect();
    let subset = |a: &[usize], b: &[usize]| a.iter().all(|x| b.contains(x));
    let mut keep = Vec::new();
    for i in 0..lines.len() {
        let mut fragment = false;
        for j in 0..lines.len() {
            if i == j || areas[j] <= areas[i] || areas[i] <= 0.0 || subset(&lines[j].2, &lines[i].2)
            {
                continue;
            }
            let shared = convex_intersection_area(&lines[i].0, &lines[j].0);
            if shared / areas[i] >= INSIDE {
                fragment = true;
                break;
            }
        }
        if !fragment {
            keep.push(i);
        }
    }
    let mut lines: Vec<Option<(Quad, f64, Vec<usize>)>> = lines.into_iter().map(Some).collect();
    keep.into_iter().filter_map(|i| lines[i].take()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(detector_input_size(1777, 2800, 1120), (704, 1120));
        assert_eq!(detector_input_size(100, 50, 1120), (160, 64));
        assert_eq!(
            tile_grid(1000, 3000, 1536, 256),
            vec![
                (0, 0, 1000, 1536),
                (0, 732, 1000, 2268),
                (0, 1464, 1000, 3000)
            ]
        );
        assert_eq!(
            dense_median(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], &[100.0; 6]),
            Some(4.0)
        );
    }

    #[test]
    fn postprocess_finds_a_box() {
        let (w, h) = (40, 20);
        let mut data = vec![0.0f32; w * h];
        for y in 5..12 {
            for x in 4..30 {
                data[y * w + x] = 0.9;
            }
        }
        let found = db_postprocess(&ProbMap {
            width: w,
            height: h,
            data: &data,
        });
        assert_eq!(found.len(), 1);
        let (q, s) = found[0];
        assert!((s - 0.9).abs() < 1e-6);
        let (qw, qh) = quad_size(&q);
        assert!(qw > 26.0 && qh > 7.0);
    }
}
