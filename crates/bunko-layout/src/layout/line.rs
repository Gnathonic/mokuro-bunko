//! Per-line metrics and orientation (`line_layout.py:632-805`).

use crate::py::{self, hypot, max2, min2, round_digits};
use crate::records::RawLine;
use crate::script::glyph_count;

use super::consts::*;

/// A point `(x, y)` in page pixels.
pub type Point = (f64, f64);
/// A quad, TL, TR, BR, BL of the line's own frame.
pub type Quad = [Point; 4];
/// `(x0, x1, y0, y1)` or `(main0, main1, cross0, cross1)`.
pub type Spans = (f64, f64, f64, f64);

/// One detected line, measured in its own frame.
#[derive(Debug, Clone)]
pub struct Line {
    /// Position in the raw list.
    pub index: usize,
    pub quad: Quad,
    pub text: String,
    pub score: f64,
    pub conf: f64,
    pub width: f64,
    pub height: f64,
    pub angle: f64,
    pub vertical: bool,
    pub ambiguous: bool,
}

impl Line {
    pub fn thickness(&self) -> f64 {
        if self.vertical { self.width } else { self.height }
    }

    pub fn length(&self) -> f64 {
        if self.vertical { self.height } else { self.width }
    }

    pub fn aspect(&self) -> f64 {
        let short = min2(self.width, self.height);
        if short > 0.0 { max2(self.width, self.height) / short } else { f64::INFINITY }
    }

    pub fn angle_reliable(&self) -> bool {
        self.aspect() >= ANGLE_RELIABLE_ASPECT
    }

    pub fn centre(&self) -> Point {
        (py::sum(self.quad.iter().map(|p| p.0)) / 4.0, py::sum(self.quad.iter().map(|p| p.1)) / 4.0)
    }

    /// `(x0, x1, y0, y1)` of the quad in a frame turned `theta` degrees; the
    /// rotation uses `round(theta, 3)`.
    pub fn spans(&self, theta: f64) -> Spans {
        quad_spans_at(&self.quad, theta)
    }

    /// `(main0, main1, cross0, cross1)` for text running `vertical`-ly.
    pub fn main_cross(&self, theta: f64, vertical: bool) -> Spans {
        let (x0, x1, y0, y1) = self.spans(theta);
        if vertical { (y0, y1, x0, x1) } else { (x0, x1, y0, y1) }
    }
}

pub(crate) fn quad_spans_at(quad: &Quad, theta: f64) -> Spans {
    let key = round_digits(theta, 3);
    let (c, s) = (py::radians(key).cos(), py::radians(key).sin());
    let xs = quad.map(|p| p.0 * c + p.1 * s);
    let ys = quad.map(|p| -p.0 * s + p.1 * c);
    (py::min_of(xs), py::max_of(xs), py::min_of(ys), py::max_of(ys))
}

/// The quad as TL, TR, BR, BL of the line's frame, `None` if degenerate.
pub fn canonical_quad(quad: &[[f64; 2]]) -> Option<Quad> {
    if quad.len() < 4 {
        return None;
    }
    let mut pts: Vec<Point> = quad[..4].iter().map(|p| (p[0], p[1])).collect();
    let area2 = py::sum((0..4).map(|i| pts[i].0 * pts[(i + 1) % 4].1 - pts[(i + 1) % 4].0 * pts[i].1));
    if area2.abs() < 1e-6 {
        return None;
    }
    if area2 < 0.0 {
        pts = vec![pts[0], pts[3], pts[2], pts[1]];
    }
    let (mut best, mut best_dx) = (0usize, f64::NEG_INFINITY);
    for start in 0..4 {
        let ex = pts[(start + 1) % 4].0 - pts[start].0;
        let ey = pts[(start + 1) % 4].1 - pts[start].1;
        let norm = hypot(ex, ey);
        let dx = if norm > 0.0 { ex / norm } else { f64::NEG_INFINITY };
        if dx > best_dx + 1e-6 {
            best = start;
            best_dx = dx;
        }
    }
    Some([pts[best % 4], pts[(best + 1) % 4], pts[(best + 2) % 4], pts[(best + 3) % 4]])
}

/// `(width, height, angle_deg)` of a canonical quad (midpoint construction).
pub fn quad_frame(quad: &Quad) -> (f64, f64, f64) {
    let mids: Vec<Point> = (0..4)
        .map(|i| ((quad[i].0 + quad[(i + 1) % 4].0) / 2.0, (quad[i].1 + quad[(i + 1) % 4].1) / 2.0))
        .collect();
    let across = (mids[1].0 - mids[3].0, mids[1].1 - mids[3].1);
    let down = (mids[2].0 - mids[0].0, mids[2].1 - mids[0].1);
    let angle = py::degrees(across.1.atan2(across.0));
    (hypot(across.0, across.1), hypot(down.0, down.1), angle)
}

/// One raw line measured, `None` for a degenerate quad. Text may be empty.
pub fn measure(index: usize, raw: &RawLine) -> Option<Line> {
    let quad = canonical_quad(&raw.quad)?;
    let (width, height, angle) = quad_frame(&quad);
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    Some(Line {
        index,
        quad,
        text: py::strip(&raw.text).to_string(),
        score: raw.score,
        conf: raw.conf,
        width,
        height,
        angle,
        vertical: height > width,
        ambiguous: false,
    })
}

/// Measure every raw line: `(lines, dropped_indices)`.
pub fn measure_lines(raw: &[RawLine]) -> (Vec<Line>, Vec<usize>) {
    let mut lines = Vec::new();
    let mut dropped = Vec::new();
    for (index, r) in raw.iter().enumerate() {
        match measure(index, r) {
            Some(line) if !line.text.is_empty() => lines.push(line),
            _ => dropped.push(index),
        }
    }
    (lines, dropped)
}

pub fn is_ambiguous(line: &Line) -> bool {
    glyph_count(&line.text) <= 1 || line.aspect() < AMBIGUOUS_ASPECT
}

/// Page-level orientation by AREA vote; ties go to vertical.
pub fn dominant_vertical<'a, I: IntoIterator<Item = &'a Line>>(lines: I) -> bool {
    let mut vertical_area = 0.0f64;
    let mut horizontal_area = 0.0f64;
    for line in lines {
        let area = line.width * line.height;
        if line.height > VOTE_ASPECT * line.width {
            vertical_area += area;
        } else if line.width > VOTE_ASPECT * line.height {
            horizontal_area += area;
        }
    }
    vertical_area >= horizontal_area
}

pub fn box_distance(a: &Line, b: &Line) -> f64 {
    let (ax0, ax1, ay0, ay1) = a.spans(0.0);
    let (bx0, bx1, by0, by1) = b.spans(0.0);
    let dx = max2(0.0, max2(ax0, bx0) - min2(ax1, bx1));
    let dy = max2(0.0, max2(ay0, by0) - min2(ay1, by1));
    hypot(dx, dy)
}

/// Settle `vertical` for ambiguous lines from their neighbours.
pub fn decide_orientations(lines: &mut [Line]) {
    let clear: Vec<usize> = (0..lines.len()).filter(|&i| !is_ambiguous(&lines[i])).collect();
    let page_vertical = dominant_vertical(clear.iter().map(|&i| &lines[i]));
    for li in 0..lines.len() {
        let ambiguous = is_ambiguous(&lines[li]);
        lines[li].ambiguous = ambiguous;
        if !ambiguous {
            continue;
        }
        let line = &lines[li];
        let reach = NEIGHBOUR_REACH_EM * max2(line.width, line.height);
        let mut best: Option<(f64, usize)> = None;
        let mut vertical = line.vertical;
        for &oi in &clear {
            let other = &lines[oi];
            let dist = box_distance(line, other);
            let better = match best {
                None => true,
                Some((bd, bi)) => dist < bd || (dist == bd && other.index < bi),
            };
            if dist <= reach && better {
                best = Some((dist, other.index));
                vertical = other.vertical;
            }
        }
        lines[li].vertical = if best.is_none() { page_vertical } else { vertical };
    }
}

/// Common frame angle for comparing two lines, `None` if they disagree.
pub fn pair_theta(a: &Line, b: &Line) -> Option<f64> {
    let (ar, br) = (a.angle_reliable(), b.angle_reliable());
    if ar && br {
        if (a.angle - b.angle).abs() > ANGLE_TOLERANCE_DEG {
            return None;
        }
        return Some(if max2(a.width, a.height) >= max2(b.width, b.height) { a.angle } else { b.angle });
    }
    if ar {
        return Some(a.angle);
    }
    if br {
        return Some(b.angle);
    }
    Some(0.0)
}

/// Signed overlap of two intervals: negative = the gap between them.
pub fn overlap(a0: f64, a1: f64, b0: f64, b1: f64) -> f64 {
    min2(a1, b1) - max2(a0, b0)
}
