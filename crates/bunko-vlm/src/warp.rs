//! `cv2.getPerspectiveTransform` + `cv2.warpPerspective`, as far as they matter here.
//!
//! The transform solve is OpenCV's own (8×8 system, f64, LU with partial pivoting, the
//! `-src·dst` products taken in f32 as `Point2f` arithmetic does). The warp samples per
//! pixel in f32 (spec §3.4): OpenCV 5.0's kernels are not reproduced bit-for-bit, the
//! measured difference of this model is ±1 on < 0.1 % of channel values (open question Q2).

use crate::image::Bgr;

/// A 3×3 row-major homography, f64.
pub type Mat3 = [f64; 9];

/// `cv2.getPerspectiveTransform(src, dst)` (both `float32` point arrays).
pub fn perspective_transform(src: &[[f32; 2]; 4], dst: &[[f32; 2]; 4]) -> Option<Mat3> {
    let mut a = [[0f64; 8]; 8];
    let mut b = [0f64; 8];
    for i in 0..4 {
        let (sx, sy) = (src[i][0], src[i][1]);
        let (dx, dy) = (dst[i][0], dst[i][1]);
        a[i][0] = f64::from(sx);
        a[i + 4][3] = f64::from(sx);
        a[i][1] = f64::from(sy);
        a[i + 4][4] = f64::from(sy);
        a[i][2] = 1.0;
        a[i + 4][5] = 1.0;
        // Point2f products: computed in f32, then widened.
        a[i][6] = f64::from(-sx * dx);
        a[i][7] = f64::from(-sy * dx);
        a[i + 4][6] = f64::from(-sx * dy);
        a[i + 4][7] = f64::from(-sy * dy);
        b[i] = f64::from(dx);
        b[i + 4] = f64::from(dy);
    }
    lu_solve(&mut a, &mut b)?;
    Some([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], 1.0])
}

/// OpenCV's `LUImpl` (Gaussian elimination, partial pivoting), solving in place into `b`.
fn lu_solve(a: &mut [[f64; 8]; 8], b: &mut [f64; 8]) -> Option<()> {
    let m = 8;
    let eps = f64::EPSILON * 100.0;
    for i in 0..m {
        let mut k = i;
        for j in i + 1..m {
            if a[j][i].abs() > a[k][i].abs() {
                k = j;
            }
        }
        if a[k][i].abs() < eps {
            return None;
        }
        if k != i {
            a.swap(i, k);
            b.swap(i, k);
        }
        let d = -1.0 / a[i][i];
        let pivot = a[i];
        for j in i + 1..m {
            let alpha = a[j][i] * d;
            for (x, p) in a[j][i + 1..].iter_mut().zip(&pivot[i + 1..]) {
                *x += alpha * p;
            }
            b[j] += alpha * b[i];
        }
    }
    for i in (0..m).rev() {
        let mut s = b[i];
        for k in i + 1..m {
            s -= a[i][k] * b[k];
        }
        b[i] = s / a[i][i];
    }
    Some(())
}

/// `cv::invert(M, DECOMP_LU)` for 3×3 f64: OpenCV's closed-form cofactor inverse.
pub fn invert3(s: &Mat3) -> Option<Mat3> {
    let m = |r: usize, c: usize| s[r * 3 + c];
    let det = m(0, 0) * (m(1, 1) * m(2, 2) - m(1, 2) * m(2, 1))
        - m(0, 1) * (m(1, 0) * m(2, 2) - m(1, 2) * m(2, 0))
        + m(0, 2) * (m(1, 0) * m(2, 1) - m(1, 1) * m(2, 0));
    if det == 0.0 {
        return None;
    }
    let d = 1.0 / det;
    Some([
        (m(1, 1) * m(2, 2) - m(1, 2) * m(2, 1)) * d,
        (m(0, 2) * m(2, 1) - m(0, 1) * m(2, 2)) * d,
        (m(0, 1) * m(1, 2) - m(0, 2) * m(1, 1)) * d,
        (m(1, 2) * m(2, 0) - m(1, 0) * m(2, 2)) * d,
        (m(0, 0) * m(2, 2) - m(0, 2) * m(2, 0)) * d,
        (m(0, 2) * m(1, 0) - m(0, 0) * m(1, 2)) * d,
        (m(1, 0) * m(2, 1) - m(1, 1) * m(2, 0)) * d,
        (m(0, 1) * m(2, 0) - m(0, 0) * m(2, 1)) * d,
        (m(0, 0) * m(1, 1) - m(0, 1) * m(1, 0)) * d,
    ])
}

/// Sampling filter of [`warp_perspective`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interp {
    /// `INTER_LINEAR`
    Linear,
    /// `INTER_CUBIC` (Keys, a = −0.75)
    Cubic,
}

/// What a tap outside the source image reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Border {
    /// `BORDER_CONSTANT` with value 0.
    Black,
    /// `BORDER_REPLICATE` (coordinates clamped).
    Replicate,
}

/// `cv2.warpPerspective(img, M, (w, h), interp, border)` without `WARP_INVERSE_MAP`.
/// Returns `None` when `M` is singular.
pub fn warp_perspective(
    img: &Bgr,
    m: &Mat3,
    w: usize,
    h: usize,
    interp: Interp,
    border: Border,
) -> Option<Bgr> {
    let inv = invert3(m)?;
    let mf: [f32; 9] = inv.map(|v| v as f32);
    let mut out = Bgr::new(w, h);
    if img.width == 0 || img.height == 0 {
        return Some(out);
    }
    for y in 0..h {
        let yf = y as f32;
        let bx = mf[1] * yf + mf[2];
        let by = mf[4] * yf + mf[5];
        let bw = mf[7] * yf + mf[8];
        for x in 0..w {
            let xf = x as f32;
            let wv = mf[6] * xf + bw;
            let iw = if wv != 0.0 { 1.0 / wv } else { 0.0 };
            let sx = (mf[0] * xf + bx) * iw;
            let sy = (mf[3] * xf + by) * iw;
            let px = match interp {
                Interp::Linear => sample_linear(img, sx, sy, border),
                Interp::Cubic => sample_cubic(img, sx, sy, border),
            };
            let d = (y * w + x) * 3;
            out.data[d..d + 3].copy_from_slice(&px);
        }
    }
    Some(out)
}

#[inline]
fn tap(img: &Bgr, x: i64, y: i64, border: Border) -> [f32; 3] {
    let (w, h) = (img.width as i64, img.height as i64);
    let (x, y) = match border {
        Border::Black => {
            if x < 0 || y < 0 || x >= w || y >= h {
                return [0.0; 3];
            }
            (x, y)
        }
        Border::Replicate => (x.clamp(0, w - 1), y.clamp(0, h - 1)),
    };
    let i = ((y * w + x) * 3) as usize;
    [
        f32::from(img.data[i]),
        f32::from(img.data[i + 1]),
        f32::from(img.data[i + 2]),
    ]
}

#[inline]
fn sat(v: f32) -> u8 {
    // cvRound (half to even) + saturate_cast<uchar>
    let r = v.round_ties_even();
    r.clamp(0.0, 255.0) as u8
}

/// Coordinates far outside any image are pinned so the integer maths cannot overflow.
#[inline]
fn floor_coord(v: f32) -> Option<(i64, f32)> {
    if !v.is_finite() {
        return None;
    }
    let v = v.clamp(-1.0e7, 1.0e7);
    let f = v.floor();
    Some((f as i64, v - f))
}

fn sample_linear(img: &Bgr, sx: f32, sy: f32, border: Border) -> [u8; 3] {
    let (Some((x0, a)), Some((y0, b))) = (floor_coord(sx), floor_coord(sy)) else {
        return [0; 3];
    };
    let p00 = tap(img, x0, y0, border);
    let p01 = tap(img, x0 + 1, y0, border);
    let p10 = tap(img, x0, y0 + 1, border);
    let p11 = tap(img, x0 + 1, y0 + 1, border);
    let mut out = [0u8; 3];
    for c in 0..3 {
        let t = p00[c] + a * (p01[c] - p00[c]);
        let u = p10[c] + a * (p11[c] - p10[c]);
        out[c] = sat(t + b * (u - t));
    }
    out
}

/// OpenCV's `interpolateCubic` weights for fraction `x`.
#[inline]
fn cubic_weights(x: f32) -> [f32; 4] {
    const A: f32 = -0.75;
    let c0 = ((A * (x + 1.0) - 5.0 * A) * (x + 1.0) + 8.0 * A) * (x + 1.0) - 4.0 * A;
    let c1 = ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0;
    let c2 = ((A + 2.0) * (1.0 - x) - (A + 3.0)) * (1.0 - x) * (1.0 - x) + 1.0;
    [c0, c1, c2, 1.0 - c0 - c1 - c2]
}

fn sample_cubic(img: &Bgr, sx: f32, sy: f32, border: Border) -> [u8; 3] {
    let (Some((x0, a)), Some((y0, b))) = (floor_coord(sx), floor_coord(sy)) else {
        return [0; 3];
    };
    let wx = cubic_weights(a);
    let wy = cubic_weights(b);
    let mut acc = [0f32; 3];
    for (r, wyr) in wy.iter().enumerate() {
        let mut row = [0f32; 3];
        for (c, wxc) in wx.iter().enumerate() {
            let p = tap(img, x0 - 1 + c as i64, y0 - 1 + r as i64, border);
            for k in 0..3 {
                row[k] += wxc * p[k];
            }
        }
        for k in 0..3 {
            acc[k] += wyr * row[k];
        }
    }
    acc.map(sat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_transform() {
        let s = [[0.0, 0.0], [9.0, 0.0], [9.0, 4.0], [0.0, 4.0]];
        let m = perspective_transform(&s, &s).unwrap();
        for (i, v) in m.iter().enumerate() {
            let want = if i % 4 == 0 { 1.0 } else { 0.0 };
            assert!((v - want).abs() < 1e-12, "{m:?}");
        }
    }

    #[test]
    fn known_homography() {
        // a scale by 2 and a shift by (3, 5)
        let src = [[3.0, 5.0], [8.0, 5.0], [8.0, 9.0], [3.0, 9.0]];
        let dst = [[0.0, 0.0], [10.0, 0.0], [10.0, 8.0], [0.0, 8.0]];
        let m = perspective_transform(&src, &dst).unwrap();
        let want = [2.0, 0.0, -6.0, 0.0, 2.0, -10.0, 0.0, 0.0, 1.0];
        for (a, b) in m.iter().zip(want) {
            assert!((a - b).abs() < 1e-9, "{m:?}");
        }
        let inv = invert3(&m).unwrap();
        assert!((inv[0] - 0.5).abs() < 1e-12 && (inv[2] - 3.0).abs() < 1e-12);
    }

    #[test]
    fn identity_warp_is_lossless() {
        let data: Vec<u8> = (0..6 * 4 * 3).map(|i| (i * 7 % 256) as u8).collect();
        let img = Bgr::from_raw(6, 4, data).unwrap();
        let id = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        for interp in [Interp::Linear, Interp::Cubic] {
            let out = warp_perspective(&img, &id, 6, 4, interp, Border::Replicate).unwrap();
            assert_eq!(out, img);
        }
        // outside the image, constant border is black
        let shift = [1.0, 0.0, 10.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let out = warp_perspective(&img, &shift, 6, 4, Interp::Linear, Border::Black).unwrap();
        assert!(out.data.iter().all(|&v| v == 0));
    }
}
