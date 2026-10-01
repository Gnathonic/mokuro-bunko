//! The mokuro block and box growth over ruby (`line_layout.py:1602-1817`).

use std::collections::HashMap;

use crate::py::{self, hypot, median, min2, round_int};
use crate::script::normalize_text;

use super::consts::*;
use super::furigana::Ruby;
use super::line::{Line, Quad, Spans};
use super::order::{Kind, block_theta};

/// One mokuro block, as written to the sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// `[x0, y0, x1, y1]`, whole pixels, clamped to the page.
    pub bbox: [i64; 4],
    pub vertical: bool,
    pub font_size: i64,
    pub lines: Vec<String>,
    pub lines_coords: Vec<[[i64; 2]; 4]>,
}

/// `quad` grown by `margin` px on both sides ACROSS its reading axis.
fn widened(quad: Quad, vertical: bool, margin: f64) -> Quad {
    let [a, b, c, d] = quad;
    let far = if vertical { b } else { d };
    let (ux, uy) = (far.0 - a.0, far.1 - a.1);
    let norm = hypot(ux, uy);
    if margin <= 0.0 || norm <= 0.0 {
        return quad;
    }
    let (ux, uy) = (ux / norm * margin, uy / norm * margin);
    let lo = |p: (f64, f64)| (p.0 - ux, p.1 - uy);
    let hi = |p: (f64, f64)| (p.0 + ux, p.1 + uy);
    if vertical { [lo(a), hi(b), hi(c), lo(d)] } else { [lo(a), lo(b), hi(c), hi(d)] }
}

/// `[x0, y0, x1, y1]` in whole pixels around `spans`, clamped to the page.
pub fn page_box(spans: Spans, page_width: f64, page_height: f64) -> [i64; 4] {
    let (x0, x1, y0, y1) = spans;
    let w = (page_width.trunc() as i64).max(0);
    let h = (page_height.trunc() as i64).max(0);
    let clamp = |value: i64, limit: i64| if limit > 0 { value.max(0).min(limit) } else { value.max(0) };
    [
        clamp(x0.floor() as i64, w),
        clamp(y0.floor() as i64, h),
        clamp(x1.ceil() as i64, w),
        clamp(y1.ceil() as i64, h),
    ]
}

/// The line's quad, with a meaningless tilt taken out.
fn settled_quad(line: &Line, theta: f64) -> Quad {
    let near_square = line.aspect() < AMBIGUOUS_ASPECT;
    if line.angle_reliable() || (!near_square && (line.angle - theta).abs() > SETTLE_MAX_TILT_DEG) {
        return line.quad;
    }
    let (cx, cy) = line.centre();
    let (c, s) = (py::radians(theta).cos(), py::radians(theta).sin());
    let (hw, hh) = (line.width / 2.0, line.height / 2.0);
    let corners = [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)];
    corners.map(|(x, y)| (cx + x * c - y * s, cy + x * s + y * c))
}

/// One mokuro block from lines already in reading order.
pub fn build_block(group: &[&Line], page_width: f64, page_height: f64, margin_cap: f64) -> Block {
    let theta = block_theta(group);
    let margins: Vec<f64> = group.iter().map(|l| min2(BODY_QUAD_MARGIN_EM * l.thickness(), margin_cap)).collect();
    let quads: Vec<Quad> =
        group.iter().zip(&margins).map(|(l, &m)| widened(settled_quad(l, theta), l.vertical, m)).collect();
    let xs = quads.iter().flat_map(|q| q.iter().map(|p| p.0));
    let ys = quads.iter().flat_map(|q| q.iter().map(|p| p.1));
    let spans = (py::min_of(xs.clone()), py::max_of(xs), py::min_of(ys.clone()), py::max_of(ys));
    let font = median(group.iter().zip(&margins).map(|(l, &m)| l.thickness() + 2.0 * m)).unwrap_or(0.0);
    Block {
        bbox: page_box(spans, page_width, page_height),
        vertical: group[0].vertical,
        font_size: round_int(font),
        lines: group.iter().map(|l| normalize_text(&l.text)).collect(),
        lines_coords: quads.iter().map(|q| q.map(|p| [round_int(p.0), round_int(p.1)])).collect(),
    }
}

/// Do the rectangle `[x0, y0, x1, y1]` and a convex quad share any AREA?
fn rect_meets_quad(rect: [i64; 4], quad: &[[i64; 2]; 4]) -> bool {
    let [x0, y0, x1, y1] = rect.map(|v| v as f64);
    if x1 <= x0 || y1 <= y0 {
        return false;
    }
    let q: [(f64, f64); 4] = quad.map(|p| (p[0] as f64, p[1] as f64));
    let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
    let mut axes = vec![(1.0, 0.0), (0.0, 1.0)];
    for k in 0..2 {
        let (ex, ey) = (q[k + 1].0 - q[k].0, q[k + 1].1 - q[k].1);
        axes.push((-ey, ex));
    }
    for (ax, ay) in axes {
        let rect_proj = corners.map(|(x, y)| x * ax + y * ay);
        let quad_proj = q.map(|(x, y)| x * ax + y * ay);
        if py::max_of(rect_proj) <= py::min_of(quad_proj) || py::max_of(quad_proj) <= py::min_of(rect_proj) {
            return false;
        }
    }
    true
}

/// How far edge `side` of `box` (`[x0, y0, x1, y1]`) can move towards `goal`.
fn clear_reach(bx: [i64; 4], side: usize, goal: i64, obstacles: &[[[i64; 2]; 4]]) -> i64 {
    let [x0, y0, x1, y1] = bx;
    let clear = |value: i64| -> bool {
        let strip = match side {
            0 => [value, y0, x0, y1],
            1 => [x0, value, x1, y0],
            2 => [x1, y0, value, y1],
            _ => [x0, y1, x1, value],
        };
        !obstacles.iter().any(|q| rect_meets_quad(strip, q))
    };
    let (mut near, mut far) = (bx[side], goal);
    if far == near || clear(far) {
        return far;
    }
    while (far - near).abs() > 1 {
        let mid = (near + far).div_euclid(2);
        if clear(mid) {
            near = mid;
        } else {
            far = mid;
        }
    }
    near
}

fn quad_spans(quad: &Quad) -> Spans {
    let xs = quad.map(|p| p.0);
    let ys = quad.map(|p| p.1);
    (py::min_of(xs), py::max_of(xs), py::min_of(ys), py::max_of(ys))
}

/// Grow every block's box over the furigana of its lines, in place.
pub fn grow_boxes_over_ruby(
    blocks: &mut [Block],
    groups: &[Vec<usize>],
    kinds: &[Kind],
    ruby: &[Ruby],
    page_width: f64,
    page_height: f64,
) {
    let mut owner: HashMap<usize, usize> = HashMap::new();
    for (k, group) in groups.iter().enumerate() {
        for &index in group {
            owner.insert(index, k);
        }
    }
    let mut order: Vec<usize> = Vec::new();
    let mut runs: HashMap<usize, Vec<&Ruby>> = HashMap::new();
    for run in ruby {
        if let Some(&k) = owner.get(&run.base) {
            if !runs.contains_key(&k) {
                order.push(k);
            }
            runs.entry(k).or_default().push(run);
        }
    }
    for k in order {
        let obstacles: Vec<[[i64; 2]; 4]> = blocks
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != k && kinds[*other] != Kind::Noise)
            .flat_map(|(_, b)| b.lines_coords.iter().copied())
            .collect();
        let mut bx = blocks[k].bbox;
        let mut block_runs = runs.remove(&k).unwrap_or_default();
        block_runs.sort_by(|a, b| {
            let (sa, sb) = (quad_spans(&a.quad), quad_spans(&b.quad));
            py::fcmp(sa.0, sb.0)
                .then(py::fcmp(sa.1, sb.1))
                .then(py::fcmp(sa.2, sb.2))
                .then(py::fcmp(sa.3, sb.3))
                .then(a.text.cmp(&b.text))
        });
        for run in block_runs {
            let goal = page_box(quad_spans(&run.quad), page_width, page_height);
            for side in [1usize, 3, 0, 2] {
                let target = if side < 2 { bx[side].min(goal[side]) } else { bx[side].max(goal[side]) };
                bx[side] = clear_reach(bx, side, target, &obstacles);
            }
        }
        blocks[k].bbox = bx;
    }
}
