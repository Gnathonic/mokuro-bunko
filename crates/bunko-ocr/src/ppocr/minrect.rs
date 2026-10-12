//! `cv2.minAreaRect` and `cv2.boundingRect` on integer contour points (spec B.3):
//! the convex hull, then OpenCV's rotating calipers in float32 (`rotatingCalipers`,
//! `CALIPERS_MINAREARECT`), and the box built from its output as OpenCV does.
//! Ties between equal-area rectangles may resolve to another representation of the
//! same rectangle than OpenCV's; `order_quad` makes that irrelevant.

/// `((cx, cy), (w, h), angle_degrees)` as float32, like OpenCV's `RotatedRect`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RotatedRect {
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
    pub angle: f32,
}

/// `cv2.boundingRect` of integer points: `(x0, y0, w, h)` with inclusive extents.
pub fn bounding_rect(pts: &[[i32; 2]]) -> (i32, i32, i32, i32) {
    let mut x0 = i32::MAX;
    let mut y0 = i32::MAX;
    let mut x1 = i32::MIN;
    let mut y1 = i32::MIN;
    for p in pts {
        x0 = x0.min(p[0]);
        y0 = y0.min(p[1]);
        x1 = x1.max(p[0]);
        y1 = y1.max(p[1]);
    }
    (x0, y0, x1 - x0 + 1, y1 - y0 + 1)
}

fn cross(o: [i64; 2], a: [i64; 2], b: [i64; 2]) -> i64 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

/// Strict convex hull (no collinear points), starting at the point that comes first
/// in the input, like OpenCV's index-ordered output.
fn convex_hull(pts: &[[i32; 2]]) -> Vec<[i32; 2]> {
    let mut idx: Vec<usize> = (0..pts.len()).collect();
    idx.sort_by(|&a, &b| pts[a].cmp(&pts[b]).then(a.cmp(&b)));
    idx.dedup_by(|a, b| pts[*a] == pts[*b]);
    if idx.len() <= 2 {
        return idx.iter().map(|&i| pts[i]).collect();
    }
    let p = |i: usize| [pts[i][0] as i64, pts[i][1] as i64];
    let mut lower: Vec<usize> = Vec::new();
    for &i in &idx {
        while lower.len() >= 2
            && cross(p(lower[lower.len() - 2]), p(lower[lower.len() - 1]), p(i)) <= 0
        {
            lower.pop();
        }
        lower.push(i);
    }
    let mut upper: Vec<usize> = Vec::new();
    for &i in idx.iter().rev() {
        while upper.len() >= 2
            && cross(p(upper[upper.len() - 2]), p(upper[upper.len() - 1]), p(i)) <= 0
        {
            upper.pop();
        }
        upper.push(i);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    // Rotate so the hull starts at its smallest input index.
    if let Some(start) = lower
        .iter()
        .enumerate()
        .min_by_key(|(_, i)| **i)
        .map(|(k, _)| k)
    {
        lower.rotate_left(start);
    }
    lower.iter().map(|&i| pts[i]).collect()
}

/// OpenCV `rotatingCalipers(..., CALIPERS_MINAREARECT)`: `[corner, side1, side2]`.
fn rotating_calipers(points: &[[f32; 2]]) -> [[f32; 2]; 3] {
    let n = points.len();
    let mut minarea = f32::MAX;
    let mut inv_len = vec![0.0f32; n];
    let mut vect = vec![[0.0f32; 2]; n];
    let (mut left, mut bottom, mut right, mut top) = (0usize, 0usize, 0usize, 0usize);
    let mut pt0 = points[0];
    let (mut left_x, mut right_x, mut top_y, mut bottom_y) = (pt0[0], pt0[0], pt0[1], pt0[1]);
    for i in 0..n {
        if pt0[0] < left_x {
            left_x = pt0[0];
            left = i;
        }
        if pt0[0] > right_x {
            right_x = pt0[0];
            right = i;
        }
        if pt0[1] > top_y {
            top_y = pt0[1];
            top = i;
        }
        if pt0[1] < bottom_y {
            bottom_y = pt0[1];
            bottom = i;
        }
        let pt = points[if i + 1 < n { i + 1 } else { 0 }];
        let dx = pt[0] as f64 - pt0[0] as f64;
        let dy = pt[1] as f64 - pt0[1] as f64;
        vect[i] = [dx as f32, dy as f32];
        inv_len[i] = (1.0 / (dx * dx + dy * dy).sqrt()) as f32;
        pt0 = pt;
    }
    // convex hull orientation
    let mut orientation = 0.0f32;
    {
        let mut ax = vect[n - 1][0] as f64;
        let mut ay = vect[n - 1][1] as f64;
        for v in &vect {
            let bx = v[0] as f64;
            let by = v[1] as f64;
            let convexity = ax * by - ay * bx;
            if convexity != 0.0 {
                orientation = if convexity > 0.0 { 1.0 } else { -1.0 };
                break;
            }
            ax = bx;
            ay = by;
        }
    }
    let mut base_a = orientation;
    let mut base_b = 0.0f32;
    let mut seq = [bottom, right, top, left];
    let mut best = (0usize, 0.0f32, 0.0f32, 0.0f32, 0.0f32, 0usize); // seq3, a, width, b, height, seq0
    for _ in 0..n {
        let dp = [
            base_a * vect[seq[0]][0] + base_b * vect[seq[0]][1],
            -base_b * vect[seq[1]][0] + base_a * vect[seq[1]][1],
            -base_a * vect[seq[2]][0] - base_b * vect[seq[2]][1],
            base_b * vect[seq[3]][0] - base_a * vect[seq[3]][1],
        ];
        let mut maxcos = dp[0] * inv_len[seq[0]];
        let mut main = 0;
        for i in 1..4 {
            let cosalpha = dp[i] * inv_len[seq[i]];
            if cosalpha > maxcos {
                main = i;
                maxcos = cosalpha;
            }
        }
        let pindex = seq[main];
        let lead_x = vect[pindex][0] * inv_len[pindex];
        let lead_y = vect[pindex][1] * inv_len[pindex];
        match main {
            0 => {
                base_a = lead_x;
                base_b = lead_y;
            }
            1 => {
                base_a = lead_y;
                base_b = -lead_x;
            }
            2 => {
                base_a = -lead_x;
                base_b = -lead_y;
            }
            _ => {
                base_a = -lead_y;
                base_b = lead_x;
            }
        }
        seq[main] += 1;
        if seq[main] == n {
            seq[main] = 0;
        }
        let dx = points[seq[1]][0] - points[seq[3]][0];
        let dy = points[seq[1]][1] - points[seq[3]][1];
        let width = dx * base_a + dy * base_b;
        let dx = points[seq[2]][0] - points[seq[0]][0];
        let dy = points[seq[2]][1] - points[seq[0]][1];
        let height = -dx * base_b + dy * base_a;
        let area = width * height;
        if area <= minarea {
            minarea = area;
            best = (seq[3], base_a, width, base_b, height, seq[0]);
        }
    }
    let (s3, a1, width, b1, height, s0) = best;
    let a2 = -b1;
    let b2 = a1;
    let c1 = a1 * points[s3][0] + points[s3][1] * b1;
    let c2 = a2 * points[s0][0] + points[s0][1] * b2;
    let idet = 1.0f32 / (a1 * b2 - a2 * b1);
    let px = (c1 * b2 - c2 * b1) * idet;
    let py = (a1 * c2 - a2 * c1) * idet;
    [
        [px, py],
        [a1 * width, b1 * width],
        [a2 * height, b2 * height],
    ]
}

/// `cv2.minAreaRect(points)`.
pub fn min_area_rect(pts: &[[i32; 2]]) -> RotatedRect {
    let hull: Vec<[f32; 2]> = convex_hull(pts)
        .iter()
        .map(|p| [p[0] as f32, p[1] as f32])
        .collect();
    let n = hull.len();
    let mut rect = RotatedRect {
        cx: 0.0,
        cy: 0.0,
        w: 0.0,
        h: 0.0,
        angle: 0.0,
    };

    let angle_rad: f32 = if n > 2 {
        let out = rotating_calipers(&hull);
        rect.cx = out[0][0] + (out[1][0] + out[2][0]) * 0.5;
        rect.cy = out[0][1] + (out[1][1] + out[2][1]) * 0.5;
        rect.w = ((out[1][0] as f64).powi(2) + (out[1][1] as f64).powi(2)).sqrt() as f32;
        rect.h = ((out[2][0] as f64).powi(2) + (out[2][1] as f64).powi(2)).sqrt() as f32;
        (out[1][1] as f64).atan2(out[1][0] as f64) as f32
    } else if n == 2 {
        rect.cx = (hull[0][0] + hull[1][0]) * 0.5;
        rect.cy = (hull[0][1] + hull[1][1]) * 0.5;
        let dx = (hull[1][0] - hull[0][0]) as f64;
        let dy = (hull[1][1] - hull[0][1]) as f64;
        rect.w = (dx * dx + dy * dy).sqrt() as f32;
        rect.h = 0.0;
        dy.atan2(dx) as f32
    } else {
        if n == 1 {
            rect.cx = hull[0][0];
            rect.cy = hull[0][1];
        }
        0.0
    };
    // `box.angle*180/CV_PI`: float * int, then divided in double.
    rect.angle = ((angle_rad * 180.0) as f64 / std::f64::consts::PI) as f32;
    rect
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_aligned_box() {
        let r = min_area_rect(&[[0, 0], [0, 3], [4, 3], [4, 0]]);
        let (w, h) = if r.w > r.h { (r.w, r.h) } else { (r.h, r.w) };
        assert!((w - 4.0).abs() < 1e-5 && (h - 3.0).abs() < 1e-5);
        assert!((r.cx - 2.0).abs() < 1e-5 && (r.cy - 1.5).abs() < 1e-5);
    }

    #[test]
    fn diamond() {
        let r = min_area_rect(&[[2, 0], [4, 2], [2, 4], [0, 2]]);
        assert!((r.w * r.h - 8.0).abs() < 1e-4);
        assert_eq!(
            bounding_rect(&[[2, 0], [4, 2], [2, 4], [0, 2]]),
            (0, 0, 5, 5)
        );
    }
}
