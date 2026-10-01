//! `cv2.findContours(bitmap, RETR_LIST, CHAIN_APPROX_SIMPLE)` (spec B.2): Suzuki–Abe
//! border following on 8-connected foreground, returning outer borders **and**
//! hole borders, as polygons of border-pixel coordinates where the chain changes
//! direction.
//!
//! The image is treated as surrounded by background (OpenCV pads it), so
//! foreground touching the edge is traced like any other. Contours come out in the
//! reverse of their discovery (raster) order, which is the order OpenCV's list
//! mode yields; the order only matters for the 3000-candidate cap and for the tile
//! merge order.

/// One contour: integer `(x, y)` vertices.
pub type Contour = Vec<[i32; 2]>;

/// 8-neighbourhood, counter-clockwise on screen starting east: `(dy, dx)`.
const DIRS: [(isize, isize); 8] = [
    (0, 1),
    (-1, 1),
    (-1, 0),
    (-1, -1),
    (0, -1),
    (1, -1),
    (1, 0),
    (1, 1),
];

fn dir_of(dy: isize, dx: isize) -> usize {
    DIRS.iter().position(|&d| d == (dy, dx)).unwrap_or(0)
}

/// Borders of the non-zero pixels of a `width × height` mask.
pub fn find_contours(mask: &[bool], width: usize, height: usize) -> Vec<Contour> {
    // Padded label image: 0 background, 1 unvisited foreground, ±nbd visited.
    let pw = width + 2;
    let ph = height + 2;
    let mut f = vec![0i32; pw * ph];
    for y in 0..height {
        for x in 0..width {
            if mask[y * width + x] {
                f[(y + 1) * pw + x + 1] = 1;
            }
        }
    }
    let at = |y: isize, x: isize| (y as usize) * pw + x as usize;
    let mut contours: Vec<Contour> = Vec::new();
    let mut nbd: i32 = 1;
    for i in 1..(ph as isize - 1) {
        for j in 1..(pw as isize - 1) {
            let v = f[at(i, j)];
            if v == 0 {
                continue;
            }
            let (i2, j2) = if v == 1 && f[at(i, j - 1)] == 0 {
                (i, j - 1) // outer border
            } else if v >= 1 && f[at(i, j + 1)] == 0 {
                (i, j + 1) // hole border
            } else {
                continue;
            };
            nbd += 1;
            contours.push(trace(&mut f, pw, (i, j), (i2, j2), nbd));
        }
    }
    contours.reverse();
    contours
}

/// Steps 3.1–3.5 of Suzuki–Abe from start `(i, j)` with the background neighbour
/// `(i2, j2)`; returns the simplified polygon in unpadded coordinates.
fn trace(
    f: &mut [i32],
    pw: usize,
    start: (isize, isize),
    from: (isize, isize),
    nbd: i32,
) -> Contour {
    let at = |y: isize, x: isize| (y as usize) * pw + x as usize;
    let (i, j) = start;
    // 3.1: clockwise from `from` for a non-zero neighbour.
    let d0 = dir_of(from.0 - i, from.1 - j);
    let mut found = None;
    for k in 0..8 {
        let d = (d0 + 8 - k) % 8;
        let (dy, dx) = DIRS[d];
        if f[at(i + dy, j + dx)] != 0 {
            found = Some((i + dy, j + dx));
            break;
        }
    }
    let Some(p1) = found else {
        f[at(i, j)] = -nbd;
        return vec![[(j - 1) as i32, (i - 1) as i32]];
    };
    let mut pts: Vec<(isize, isize)> = Vec::new();
    let mut p2 = p1;
    let mut p3 = (i, j);
    loop {
        pts.push(p3);
        // 3.3: counter-clockwise from the element after p2.
        let dprev = dir_of(p2.0 - p3.0, p2.1 - p3.1);
        let mut east_zero = false;
        let mut p4 = p3;
        for k in 1..=8 {
            let d = (dprev + k) % 8;
            let (dy, dx) = DIRS[d];
            let q = (p3.0 + dy, p3.1 + dx);
            if f[at(q.0, q.1)] != 0 {
                p4 = q;
                break;
            }
            if d == 0 {
                east_zero = true;
            }
        }
        // 3.4
        let idx = at(p3.0, p3.1);
        if east_zero {
            f[idx] = -nbd;
        } else if f[idx] == 1 {
            f[idx] = nbd;
        }
        // 3.5
        if p4 == (i, j) && p3 == p1 {
            break;
        }
        p2 = p3;
        p3 = p4;
    }
    simplify(&pts)
}

/// CHAIN_APPROX_SIMPLE: keep the vertices where the chain changes direction.
fn simplify(pts: &[(isize, isize)]) -> Contour {
    let n = pts.len();
    let to_xy = |p: (isize, isize)| [(p.1 - 1) as i32, (p.0 - 1) as i32];
    if n <= 2 {
        return pts.iter().map(|&p| to_xy(p)).collect();
    }
    let mut out = Vec::new();
    for k in 0..n {
        let prev = pts[(k + n - 1) % n];
        let cur = pts[k];
        let next = pts[(k + 1) % n];
        let d_in = (cur.0 - prev.0, cur.1 - prev.1);
        let d_out = (next.0 - cur.0, next.1 - cur.1);
        if d_in != d_out {
            out.push(to_xy(cur));
        }
    }
    if out.is_empty() {
        out.push(to_xy(pts[0]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(rows: &[&str]) -> (Vec<bool>, usize, usize) {
        let h = rows.len();
        let w = rows[0].len();
        let m = rows
            .iter()
            .flat_map(|r| r.chars().map(|c| c == '#'))
            .collect();
        (m, w, h)
    }

    #[test]
    fn block_at_origin() {
        let (m, w, h) = mask(&["#####.", "#####.", "#####.", "#####.", "......"]);
        let c = find_contours(&m, w, h);
        assert_eq!(c.len(), 1);
        let mut v = c[0].clone();
        v.sort();
        assert_eq!(v, vec![[0, 0], [0, 3], [4, 0], [4, 3]]);
    }

    #[test]
    fn ring_has_hole_border() {
        let (m, w, h) = mask(&["#####", "#...#", "#...#", "#####"]);
        let c = find_contours(&m, w, h);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn single_pixel() {
        let (m, w, h) = mask(&["...", ".#.", "..."]);
        assert_eq!(find_contours(&m, w, h), vec![vec![[1, 1]]]);
    }
}
