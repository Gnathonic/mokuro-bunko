//! Quad geometry (`ppocr.py:408-496`, `:701-866`, `:896-930`), in float32 like the
//! numpy original. Python floats that enter numpy f32 expressions are cast to f32
//! first (numpy 2 "weak scalar" rules), and results that Python reads back with
//! `float(...)` are widened to f64 at the same points.

/// Four corners `[x, y]`: TL, TR, BR, BL of the line's upright reading frame.
pub type Quad = [[f32; 2]; 4];

#[inline]
fn sub(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

#[inline]
fn add(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] + b[0], a[1] + b[1]]
}

#[inline]
fn scale(a: [f32; 2], s: f32) -> [f32; 2] {
    [a[0] * s, a[1] * s]
}

#[inline]
fn norm(a: [f32; 2]) -> f32 {
    (a[0] * a[0] + a[1] * a[1]).sqrt()
}

#[inline]
fn dot(a: [f32; 2], b: [f32; 2]) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}

/// `v / (np.linalg.norm(v) or 1.0)`.
#[inline]
fn unit(v: [f32; 2]) -> [f32; 2] {
    let n = norm(v);
    let d = if n != 0.0 { n } else { 1.0 };
    [v[0] / d, v[1] / d]
}

/// `pts.mean(axis=0)` for float32 points (sequential sum, then divide).
pub fn centre(pts: &[[f32; 2]]) -> [f32; 2] {
    let mut s = [0.0f32; 2];
    for p in pts {
        s[0] += p[0];
        s[1] += p[1];
    }
    let n = pts.len() as f32;
    [s[0] / n, s[1] / n]
}

/// `order_quad`: sort the corners clockwise on screen around their centre, then
/// start at the corner whose outgoing edge points most nearly along +x.
pub fn order_quad(points: &Quad) -> Quad {
    let c = centre(points);
    let angles: Vec<f32> = points
        .iter()
        .map(|p| (p[1] - c[1]).atan2(p[0] - c[0]))
        .collect();
    let mut order = [0usize, 1, 2, 3];
    // numpy argsort on 4 elements is an insertion sort: stable.
    order.sort_by(|&a, &b| angles[a].total_cmp(&angles[b]));
    let pts: Vec<[f32; 2]> = order.iter().map(|&i| points[i]).collect();
    let mut best = 0;
    let mut best_dx = f64::NEG_INFINITY;
    for start in 0..4 {
        let edge = sub(pts[(start + 1) % 4], pts[start]);
        let length = edge[0].hypot(edge[1]) as f64;
        let dx = if length > 0.0 {
            edge[0] as f64 / length
        } else {
            f64::NEG_INFINITY
        };
        if dx > best_dx + 1e-6 {
            best = start;
            best_dx = dx;
        }
    }
    [
        pts[best],
        pts[(best + 1) % 4],
        pts[(best + 2) % 4],
        pts[(best + 3) % 4],
    ]
}

/// `(width, height)` of an ordered quad in its own frame.
pub fn quad_size(q: &Quad) -> (f64, f64) {
    let w = (norm(sub(q[1], q[0])) + norm(sub(q[2], q[3]))) / 2.0;
    let h = (norm(sub(q[3], q[0])) + norm(sub(q[2], q[1]))) / 2.0;
    (w as f64, h as f64)
}

/// Tilt in degrees, positive clockwise on screen.
pub fn quad_angle(q: &Quad) -> f64 {
    let top = add(sub(q[1], q[0]), sub(q[2], q[3]));
    (top[1] as f64).atan2(top[0] as f64).to_degrees()
}

pub fn quad_is_vertical(q: &Quad) -> bool {
    let (w, h) = quad_size(q);
    h > w
}

/// Short side: the glyph size.
pub fn quad_thickness(q: &Quad) -> f64 {
    let (w, h) = quad_size(q);
    w.min(h)
}

/// Long side.
pub fn quad_length(q: &Quad) -> f64 {
    let (w, h) = quad_size(q);
    w.max(h)
}

/// `rect_to_quad`: corners of a rotated rectangle (f64 math, cast to f32).
pub fn rect_to_quad(cx: f64, cy: f64, w: f64, h: f64, angle_deg: f64) -> Quad {
    let (hw, hh) = (w / 2.0, h / 2.0);
    let r = angle_deg.to_radians();
    let (c, s) = (r.cos(), r.sin());
    let corners = [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)];
    let mut out = [[0.0f32; 2]; 4];
    for (k, (x, y)) in corners.iter().enumerate() {
        out[k] = [(cx + x * c - y * s) as f32, (cy + x * s + y * c) as f32];
    }
    out
}

/// DB's unclip offset for a rectangle: `area * ratio / perimeter`.
pub fn unclip_distance(width: f64, height: f64, ratio: f64) -> f64 {
    let perimeter = 2.0 * (width + height);
    if perimeter > 0.0 {
        width * height * ratio / perimeter
    } else {
        0.0
    }
}

/// Axis-aligned bounds `(min x, min y, max x, max y)`.
pub fn bounds(q: &Quad) -> (f64, f64, f64, f64) {
    let mut b = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in q {
        b.0 = b.0.min(p[0]);
        b.1 = b.1.min(p[1]);
        b.2 = b.2.max(p[0]);
        b.3 = b.3.max(p[1]);
    }
    (b.0 as f64, b.1 as f64, b.2 as f64, b.3 as f64)
}

/// A quad in its own axes (`_frame`).
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    pub centre: [f32; 2],
    /// Unit vector along the long side.
    pub axis: [f32; 2],
    pub normal: [f32; 2],
    pub half_len: f64,
    pub half_thick: f64,
}

pub fn frame(q: &Quad) -> Frame {
    let (w, h) = quad_size(q);
    let across = add(sub(q[1], q[0]), sub(q[2], q[3]));
    let down = add(sub(q[3], q[0]), sub(q[2], q[1]));
    let (axis, normal) = if h >= w {
        (down, across)
    } else {
        (across, down)
    };
    Frame {
        centre: centre(q),
        axis: unit(axis),
        normal: unit(normal),
        half_len: w.max(h) / 2.0,
        half_thick: w.min(h) / 2.0,
    }
}

/// `cv2.contourArea` of a float quad (absolute shoelace in f64).
pub fn polygon_area(pts: &[[f32; 2]]) -> f64 {
    let n = pts.len();
    if n == 0 {
        return 0.0;
    }
    let mut a = 0.0f64;
    let mut prev = pts[n - 1];
    for &p in pts {
        a += prev[0] as f64 * p[1] as f64 - prev[1] as f64 * p[0] as f64;
        prev = p;
    }
    (a * 0.5).abs()
}

/// Area of the intersection of two convex polygons (Sutherland–Hodgman, f64), what
/// `cv2.intersectConvexConvex` returns as its first value.
pub fn convex_intersection_area(a: &[[f32; 2]], b: &[[f32; 2]]) -> f64 {
    let to64 = |p: &[f32; 2]| [p[0] as f64, p[1] as f64];
    let mut poly: Vec<[f64; 2]> = a.iter().map(to64).collect();
    let clip: Vec<[f64; 2]> = b.iter().map(to64).collect();
    // Orientation of the clip polygon decides which side is "inside".
    let signed = |p: &[[f64; 2]]| {
        let n = p.len();
        let mut s = 0.0;
        for i in 0..n {
            let (u, v) = (p[i], p[(i + 1) % n]);
            s += u[0] * v[1] - u[1] * v[0];
        }
        s
    };
    let orient = if signed(&clip) >= 0.0 { 1.0 } else { -1.0 };
    let n = clip.len();
    for i in 0..n {
        if poly.is_empty() {
            break;
        }
        let (c0, c1) = (clip[i], clip[(i + 1) % n]);
        let side = |p: [f64; 2]| {
            orient * ((c1[0] - c0[0]) * (p[1] - c0[1]) - (c1[1] - c0[1]) * (p[0] - c0[0]))
        };
        let input = std::mem::take(&mut poly);
        let m = input.len();
        for k in 0..m {
            let cur = input[k];
            let prev = input[(k + m - 1) % m];
            let (sc, sp) = (side(cur), side(prev));
            if sc >= 0.0 {
                if sp < 0.0 {
                    poly.push(intersect(prev, cur, sp, sc));
                }
                poly.push(cur);
            } else if sp >= 0.0 {
                poly.push(intersect(prev, cur, sp, sc));
            }
        }
    }
    let n = poly.len();
    if n < 3 {
        return 0.0;
    }
    let mut s = 0.0;
    for i in 0..n {
        let (u, v) = (poly[i], poly[(i + 1) % n]);
        s += u[0] * v[1] - u[1] * v[0];
    }
    (s * 0.5).abs()
}

fn intersect(p: [f64; 2], q: [f64; 2], sp: f64, sq: f64) -> [f64; 2] {
    let t = sp / (sp - sq);
    [p[0] + (q[0] - p[0]) * t, p[1] + (q[1] - p[1]) * t]
}

/// IoU of two convex quads.
pub fn quad_iou(a: &Quad, b: &Quad) -> f64 {
    let (aa, ab) = (polygon_area(a), polygon_area(b));
    if aa <= 0.0 || ab <= 0.0 {
        return 0.0;
    }
    let inter = convex_intersection_area(a, b);
    let union = aa + ab - inter;
    if union > 0.0 { inter / union } else { 0.0 }
}

/// `same_line`: two quads from different tiles are one printed line.
pub fn same_line(a: &Quad, b: &Quad) -> bool {
    if quad_iou(a, b) >= 0.5 {
        return true;
    }
    let (mut fa, mut fb) = (frame(a), frame(b));
    if fa.half_len < fb.half_len {
        std::mem::swap(&mut fa, &mut fb);
    }
    let thick_a = fa.half_thick * 2.0;
    let thick_b = fb.half_thick * 2.0;
    let ratio = thick_a / thick_b.max(1e-6);
    if !(0.7..=1.0 / 0.7).contains(&ratio) {
        return false;
    }
    if fb.half_len < 1.5 * fb.half_thick {
        return false;
    }
    if (dot(fa.axis, fb.axis) as f64).abs() < 6.0f64.to_radians().cos() {
        return false;
    }
    let offset = sub(fb.centre, fa.centre);
    if (dot(offset, fa.normal) as f64).abs() > 0.35 * thick_a.min(thick_b) {
        return false;
    }
    let along = (dot(offset, fa.axis) as f64).abs();
    let overlap = fa.half_len + fb.half_len - along;
    overlap > 0.5 * thick_a.min(thick_b)
}

/// `union_quad`: the smallest rectangle in the longest quad's frame holding all of
/// them, with the members' length-weighted thickness.
pub fn union_quad(quads: &[Quad]) -> Quad {
    let frames: Vec<Frame> = quads.iter().map(frame).collect();
    // max(..., key=) keeps the first maximum.
    let mut ref_i = 0;
    for (i, f) in frames.iter().enumerate() {
        if f.half_len > frames[ref_i].half_len {
            ref_i = i;
        }
    }
    let r = frames[ref_i];
    let mut lo = f32::MAX;
    let mut hi = f32::MIN;
    for q in quads {
        for p in q {
            let along = dot(sub(*p, r.centre), r.axis);
            lo = lo.min(along);
            hi = hi.max(along);
        }
    }
    // weights are a float32 array in numpy; np.average then works in f64.
    let weights: Vec<f64> = frames.iter().map(|f| f.half_len as f32 as f64).collect();
    let centres: Vec<f64> = frames
        .iter()
        .map(|f| dot(sub(f.centre, r.centre), r.normal) as f64)
        .collect();
    let mid_n = crate::py::np_average(&centres, &weights);
    let thick: Vec<f64> = frames.iter().map(|f| f.half_thick).collect();
    let half_thick = crate::py::np_average(&thick, &weights);
    let (lo, hi) = (lo as f64, hi as f64);
    let c = add(
        add(
            r.centre,
            [
                r.axis[0] * (lo + hi) as f32 / 2.0,
                r.axis[1] * (lo + hi) as f32 / 2.0,
            ],
        ),
        scale(r.normal, mid_n as f32),
    );
    let a = [
        r.axis[0] * (hi - lo) as f32 / 2.0,
        r.axis[1] * (hi - lo) as f32 / 2.0,
    ];
    let n = scale(r.normal, half_thick as f32);
    order_quad(&[
        sub(sub(c, a), n),
        sub(add(c, a), n),
        add(add(c, a), n),
        add(sub(c, a), n),
    ])
}

/// `slice_quad`: the stretch `[start, end]` along the reading axis, from its start
/// edge; may reach outside the quad.
pub fn slice_quad(q: &Quad, start: f64, end: f64) -> Quad {
    let (w, h) = quad_size(q);
    let (s, e) = (start as f32, end as f32);
    if h > w {
        let axis = unit(scale(add(sub(q[3], q[0]), sub(q[2], q[1])), 0.5));
        [
            add(q[0], scale(axis, s)),
            add(q[1], scale(axis, s)),
            add(q[1], scale(axis, e)),
            add(q[0], scale(axis, e)),
        ]
    } else {
        let axis = unit(scale(add(sub(q[1], q[0]), sub(q[2], q[3])), 0.5));
        [
            add(q[0], scale(axis, s)),
            add(q[0], scale(axis, e)),
            add(q[3], scale(axis, e)),
            add(q[3], scale(axis, s)),
        ]
    }
}

/// `widen_quad`: grown across the reading axis by `share` of its thickness per side
/// (corners not re-ordered).
pub fn widen_quad(q: &Quad, share: f64) -> Quad {
    let (w, h) = quad_size(q);
    if h > w {
        let side = scale(
            scale(unit(add(sub(q[1], q[0]), sub(q[2], q[3]))), w as f32),
            share as f32,
        );
        [
            sub(q[0], side),
            add(q[1], side),
            add(q[2], side),
            sub(q[3], side),
        ]
    } else {
        let side = scale(
            scale(unit(add(sub(q[3], q[0]), sub(q[2], q[1]))), h as f32),
            share as f32,
        );
        [
            sub(q[0], side),
            sub(q[1], side),
            add(q[2], side),
            add(q[3], side),
        ]
    }
}

/// Projection of a quad's corners on `frame.axis`: `(min, max)`.
pub fn project(q: &Quad, f: &Frame) -> (f64, f64) {
    let mut lo = f32::MAX;
    let mut hi = f32::MIN;
    for p in q {
        let v = dot(sub(*p, f.centre), f.axis);
        lo = lo.min(v);
        hi = hi.max(v);
    }
    (lo as f64, hi as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_quad_starts_top_left() {
        let q = [[10.0, 0.0], [10.0, 5.0], [0.0, 5.0], [0.0, 0.0]];
        assert_eq!(
            order_quad(&q),
            [[0.0, 0.0], [10.0, 0.0], [10.0, 5.0], [0.0, 5.0]]
        );
        assert_eq!(quad_size(&order_quad(&q)), (10.0, 5.0));
        assert!(!quad_is_vertical(&order_quad(&q)));
    }

    #[test]
    fn iou_and_union() {
        let a: Quad = [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        let b: Quad = [[5.0, 0.0], [15.0, 0.0], [15.0, 10.0], [5.0, 10.0]];
        assert!((quad_iou(&a, &b) - 50.0 / 150.0).abs() < 1e-9);
        // Squares take the vertical axis (h >= w); thickness is the weighted mean.
        // Python: [[2.5, 0], [12.5, 0], [12.5, 10], [2.5, 10]].
        assert_eq!(
            union_quad(&[a, b]),
            [[2.5, 0.0], [12.5, 0.0], [12.5, 10.0], [2.5, 10.0]]
        );
    }

    #[test]
    fn slices() {
        let q: Quad = [[0.0, 0.0], [10.0, 0.0], [10.0, 100.0], [0.0, 100.0]];
        let s = slice_quad(&q, -5.0, 20.0);
        assert_eq!(s, [[0.0, -5.0], [10.0, -5.0], [10.0, 20.0], [0.0, 20.0]]);
        let w = widen_quad(&q, 0.1);
        assert_eq!(w[0], [-1.0, 0.0]);
    }
}
