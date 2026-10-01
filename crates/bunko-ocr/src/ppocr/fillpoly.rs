//! `cv2.fillPoly(mask, [pts], 1)` with the default `LINE_8` and `shift = 0`
//! (spec B.4): OpenCV's `CollectPolyEdges` (every edge also drawn as an 8-connected
//! line) followed by `FillEdgeCollection` (the scanline fill with its active edge
//! list), ported step by step. For outer contours the result equals the
//! 8-connected component with its holes filled, but for hole contours only the
//! scanline algorithm itself reproduces OpenCV's mask.

const XY_SHIFT: u32 = 16;

#[derive(Clone, Copy, Default)]
struct PolyEdge {
    y0: i64,
    y1: i64,
    x: i64,
    dx: i64,
    next: Option<usize>,
}

/// OpenCV `LineIterator` (8-connectivity, left to right) drawing value 1.
fn draw_line(mask: &mut [u8], w: usize, h: usize, p0: [i64; 2], p1: [i64; 2]) {
    let (mut a, mut b) = (p0, p1);
    // All points of a contour lie inside the mask; OpenCV clips otherwise.
    let inside = |p: [i64; 2]| p[0] >= 0 && p[1] >= 0 && (p[0] as usize) < w && (p[1] as usize) < h;
    if !inside(a) || !inside(b) {
        return;
    }
    let mut dx = b[0] - a[0];
    let mut dy = b[1] - a[1];
    if dx < 0 {
        dx = -dx;
        dy = -dy;
        std::mem::swap(&mut a, &mut b);
    }
    let mut delta_x = 1i64;
    let mut delta_y = 1i64;
    if dy < 0 {
        dy = -dy;
        delta_y = -1;
    }
    let vert = dy > dx;
    if vert {
        std::mem::swap(&mut dx, &mut dy);
        std::mem::swap(&mut delta_x, &mut delta_y);
    }
    let mut err = dx - (dy + dy);
    let plus_delta = dx + dx;
    let minus_delta = -(dy + dy);
    let (mut minus_shift, mut plus_shift, mut minus_step, mut plus_step) =
        (delta_x, 0i64, 0i64, delta_y);
    if vert {
        std::mem::swap(&mut plus_step, &mut plus_shift);
        std::mem::swap(&mut minus_step, &mut minus_shift);
    }
    let count = dx + 1;
    let mut p = a;
    for _ in 0..count {
        mask[p[1] as usize * w + p[0] as usize] = 1;
        let m = if err < 0 { -1i64 } else { 0 };
        err += minus_delta + (plus_delta & m);
        p[0] += minus_shift + (plus_shift & m);
        p[1] += minus_step + (plus_step & m);
    }
}

/// Fill polygon `pts` into a `w × h` u8 mask with value 1.
pub fn fill_poly(mask: &mut [u8], w: usize, h: usize, pts: &[[i32; 2]]) {
    let n = pts.len();
    if n == 0 {
        return;
    }
    // CollectPolyEdges
    let mut edges: Vec<PolyEdge> = Vec::with_capacity(n + 1);
    let mut pt0 = [(pts[n - 1][0] as i64) << XY_SHIFT, pts[n - 1][1] as i64];
    for p in pts {
        let pt1 = [(p[0] as i64) << XY_SHIFT, p[1] as i64];
        let half = 1i64 << (XY_SHIFT - 1);
        let t0 = [(pt0[0] + half) >> XY_SHIFT, pt0[1]];
        let t1 = [(pt1[0] + half) >> XY_SHIFT, pt1[1]];
        draw_line(mask, w, h, t0, t1);
        let pt0c = [t0[0] << XY_SHIFT, pt0[1]];
        let pt1c = [t1[0] << XY_SHIFT, pt1[1]];
        if pt0c[1] != pt1c[1] {
            let mut e = PolyEdge::default();
            if pt0c[1] < pt1c[1] {
                e.y0 = pt0c[1];
                e.y1 = pt1c[1];
                e.x = pt0c[0];
            } else {
                e.y0 = pt1c[1];
                e.y1 = pt0c[1];
                e.x = pt1c[0];
            }
            e.dx = (pt1c[0] - pt0c[0]) / (pt1c[1] - pt0c[1]);
            edges.push(e);
        }
        pt0 = pt1;
    }
    fill_edge_collection(mask, w, h, edges);
}

fn fill_edge_collection(mask: &mut [u8], w: usize, h: usize, mut edges: Vec<PolyEdge>) {
    let total = edges.len();
    if total < 2 {
        return;
    }
    let mut y_max = i64::MIN;
    let mut y_min = i64::MAX;
    let mut x_max = i64::MIN;
    let mut x_min = i64::MAX;
    for e in &edges {
        let x1 = e.x + (e.y1 - e.y0) * e.dx;
        y_min = y_min.min(e.y0);
        y_max = y_max.max(e.y1);
        x_min = x_min.min(e.x).min(x1);
        x_max = x_max.max(e.x).max(x1);
    }
    if y_max < 0 || y_min >= h as i64 || x_max < 0 || x_min >= ((w as i64) << XY_SHIFT) {
        return;
    }
    edges.sort_by(|a, b| a.y0.cmp(&b.y0).then(a.x.cmp(&b.x)).then(a.dx.cmp(&b.dx)));
    // Sentinel; index `total` is the "e" past the end, index `total + 1` is `tmp`.
    edges.push(PolyEdge {
        y0: i64::MAX,
        ..Default::default()
    });
    edges.push(PolyEdge::default());
    let tmp = total + 1;
    let mut i = 0usize;
    let mut e = 0usize;
    let y_max = y_max.min(h as i64);
    let mut y = edges[e].y0;
    while y < y_max {
        let mut draw = false;
        let clipline = y < 0;
        let mut prelast = tmp;
        let mut last = edges[tmp].next;
        loop {
            if !(last.is_some() || edges[e].y0 == y) {
                break;
            }
            if let Some(l) = last
                && edges[l].y1 == y
            {
                // exclude edge if y reaches its lower point
                edges[prelast].next = edges[l].next;
                last = edges[l].next;
                continue;
            }
            let keep_prelast = prelast;
            if let Some(l) = last
                && (edges[e].y0 > y || edges[l].x < edges[e].x)
            {
                // go to the next edge in the active list
                prelast = l;
                last = edges[l].next;
            } else if i < total {
                // insert a new edge into the active list
                edges[prelast].next = Some(e);
                edges[e].next = last;
                prelast = e;
                i += 1;
                e = i;
            } else {
                break;
            }
            if draw {
                if !clipline {
                    let (x1, x2) = if edges[keep_prelast].x > edges[prelast].x {
                        (
                            edges[prelast].x >> XY_SHIFT,
                            edges[keep_prelast].x >> XY_SHIFT,
                        )
                    } else {
                        (
                            edges[keep_prelast].x >> XY_SHIFT,
                            edges[prelast].x >> XY_SHIFT,
                        )
                    };
                    if x1 < w as i64 && x2 >= 0 {
                        let x1 = x1.max(0) as usize;
                        let x2 = x2.min(w as i64 - 1) as usize;
                        let row = y as usize * w;
                        mask[row + x1..=row + x2].fill(1);
                    }
                }
                let kd = edges[keep_prelast].dx;
                edges[keep_prelast].x += kd;
                let pd = edges[prelast].dx;
                edges[prelast].x += pd;
            }
            draw = !draw;
        }
        // bubble sort the active list by x
        let mut keep_prelast: Option<usize> = None;
        loop {
            let mut prelast = tmp;
            let mut last = edges[tmp].next;
            let mut last_exchange: Option<usize> = None;
            while let Some(l) = last {
                if Some(l) == keep_prelast {
                    break;
                }
                let Some(te) = edges[l].next else {
                    break;
                };
                if edges[l].x > edges[te].x {
                    edges[prelast].next = Some(te);
                    edges[l].next = edges[te].next;
                    edges[te].next = Some(l);
                    prelast = te;
                    last_exchange = Some(prelast);
                } else {
                    prelast = l;
                    last = Some(te);
                }
            }
            let Some(lx) = last_exchange else {
                break;
            };
            keep_prelast = Some(lx);
            if keep_prelast == edges[tmp].next || lx == tmp {
                break;
            }
        }
        y += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangle_fills_inclusive() {
        let mut m = vec![0u8; 6 * 5];
        fill_poly(&mut m, 6, 5, &[[1, 1], [4, 1], [4, 3], [1, 3]]);
        let filled: usize = m.iter().map(|&v| v as usize).sum();
        assert_eq!(filled, 4 * 3);
        assert_eq!(m[6 + 1], 1);
        assert_eq!(m[3 * 6 + 4], 1);
        assert_eq!(m[0], 0);
    }

    #[test]
    fn single_point() {
        let mut m = vec![0u8; 9];
        fill_poly(&mut m, 3, 3, &[[1, 1]]);
        assert_eq!(m.iter().map(|&v| v as usize).sum::<usize>(), 1);
    }
}
