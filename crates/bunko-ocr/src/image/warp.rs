//! `cv2.getPerspectiveTransform`, `cv2.warpPerspective(..., INTER_CUBIC,
//! BORDER_REPLICATE)` and `cv2.rotate`, following OpenCV's own arithmetic
//! (spec B.6–B.8):
//!
//! * the 8×8 system is built with float products and solved by OpenCV's LU with
//!   partial pivoting (f64); the 3×3 inverse uses `cv::invert`'s closed form;
//! * the warp itself is OpenCV 5's `genericWarp` + `bicubicVec` (float coordinates,
//!   float weights and accumulation with FMA), not the 4.x fixed-point tables.

use super::{BgrImage, ImageView};

/// A 3×3 homography, row-major.
pub type Matrix3 = [[f64; 3]; 3];

/// Index loops on purpose: this mirrors the C++ statement by statement.
#[allow(clippy::needless_range_loop)]
/// OpenCV `LUImpl<double>` solving `a x = b` in place (b becomes x). Returns false
/// for a singular matrix.
fn lu_solve(a: &mut [[f64; 8]; 8], b: &mut [f64; 8]) -> bool {
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
            return false;
        }
        if k != i {
            for j in i..m {
                let t = a[i][j];
                a[i][j] = a[k][j];
                a[k][j] = t;
            }
            b.swap(i, k);
        }
        let d = -1.0 / a[i][i];
        for j in i + 1..m {
            let alpha = a[j][i] * d;
            for kk in i + 1..m {
                a[j][kk] += alpha * a[i][kk];
            }
            b[j] += alpha * b[i];
        }
    }
    for i in (0..m).rev() {
        let mut s = b[i];
        for kk in i + 1..m {
            s -= a[i][kk] * b[kk];
        }
        b[i] = s / a[i][i];
    }
    true
}

/// `cv2.getPerspectiveTransform(src, dst)` for float32 points. `None` if singular.
pub fn get_perspective_transform(src: &[[f32; 2]; 4], dst: &[[f32; 2]; 4]) -> Option<Matrix3> {
    let mut a = [[0.0f64; 8]; 8];
    let mut b = [0.0f64; 8];
    for i in 0..4 {
        let (sx, sy) = (src[i][0], src[i][1]);
        let (dx, dy) = (dst[i][0], dst[i][1]);
        a[i][0] = sx as f64;
        a[i + 4][3] = sx as f64;
        a[i][1] = sy as f64;
        a[i + 4][4] = sy as f64;
        a[i][2] = 1.0;
        a[i + 4][5] = 1.0;
        // Products of two floats, in float, as the C++ source computes them.
        a[i][6] = (-sx * dx) as f64;
        a[i][7] = (-sy * dx) as f64;
        a[i + 4][6] = (-sx * dy) as f64;
        a[i + 4][7] = (-sy * dy) as f64;
        b[i] = dx as f64;
        b[i + 4] = dy as f64;
    }
    if !lu_solve(&mut a, &mut b) {
        return None;
    }
    Some([[b[0], b[1], b[2]], [b[3], b[4], b[5]], [b[6], b[7], 1.0]])
}

/// `cv::invert(M, DECOMP_LU)` for 3×3 doubles (closed form); `None` if singular.
fn invert3(m: &Matrix3) -> Option<Matrix3> {
    let s = |r: usize, c: usize| m[r][c];
    let det = s(0, 0) * (s(1, 1) * s(2, 2) - s(1, 2) * s(2, 1))
        - s(0, 1) * (s(1, 0) * s(2, 2) - s(1, 2) * s(2, 0))
        + s(0, 2) * (s(1, 0) * s(2, 1) - s(1, 1) * s(2, 0));
    if det == 0.0 {
        return None;
    }
    let d = 1.0 / det;
    Some([
        [
            (s(1, 1) * s(2, 2) - s(1, 2) * s(2, 1)) * d,
            (s(0, 2) * s(2, 1) - s(0, 1) * s(2, 2)) * d,
            (s(0, 1) * s(1, 2) - s(0, 2) * s(1, 1)) * d,
        ],
        [
            (s(1, 2) * s(2, 0) - s(1, 0) * s(2, 2)) * d,
            (s(0, 0) * s(2, 2) - s(0, 2) * s(2, 0)) * d,
            (s(0, 2) * s(1, 0) - s(0, 0) * s(1, 2)) * d,
        ],
        [
            (s(1, 0) * s(2, 1) - s(1, 1) * s(2, 0)) * d,
            (s(0, 1) * s(2, 0) - s(0, 0) * s(2, 1)) * d,
            (s(0, 0) * s(1, 1) - s(0, 1) * s(1, 0)) * d,
        ],
    ])
}

/// OpenCV 5 `bicubicWeights` (vector form, A = −0.75), with its fused multiply-adds.
#[inline(always)]
fn bicubic_weights(alpha: f32) -> [f32; 4] {
    const A: f32 = -0.75;
    let a2 = alpha * alpha;
    let b = 1.0 - alpha;
    let b2 = b * b;
    let w0 = A * (alpha * b2);
    let w3 = A * (a2 * b);
    let w1 = a2.mul_add((A + 2.0).mul_add(alpha, -(A + 3.0)), 1.0);
    let w2 = ((1.0 - w0) - w1) - w3;
    [w0, w1, w2, w3]
}

/// `cv2.warpPerspective(src, m, (dst_w, dst_h), flags=INTER_CUBIC,
/// borderMode=BORDER_REPLICATE)`; `m` maps source to destination as given to cv2.
///
/// OpenCV 5 no longer uses the 1/32-px tables here: `genericWarp` inverts `m` in
/// double, converts it to float, maps every destination pixel in float/double
/// mixed arithmetic, and the SIMD `bicubicVec` kernel interpolates in float with
/// fused multiply-adds (x86 AVX2/AVX-512 dispatch), rounding half to even.
/// Measured bit-exact against cv2 5.0 (`tests/golden/cv_golden.json`).
pub fn warp_perspective_cubic(
    src: ImageView<'_>,
    m: &Matrix3,
    dst_w: usize,
    dst_h: usize,
) -> BgrImage {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("fma") {
        // SAFETY: the CPU supports FMA (checked just above), which is all the
        // `target_feature` function requires.
        return unsafe { warp_fma(src, m, dst_w, dst_h) };
    }
    warp_impl(src, m, dst_w, dst_h)
}

/// The same kernel compiled with hardware FMA, so `mul_add` is one instruction
/// instead of a libm call (≈20× faster; results are identical by definition).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "fma")]
unsafe fn warp_fma(src: ImageView<'_>, m: &Matrix3, dst_w: usize, dst_h: usize) -> BgrImage {
    warp_impl(src, m, dst_w, dst_h)
}

#[inline(always)]
fn warp_impl(src: ImageView<'_>, m: &Matrix3, dst_w: usize, dst_h: usize) -> BgrImage {
    let mut out = BgrImage::zeros(dst_w, dst_h);
    let (sw, sh) = (src.width(), src.height());
    if dst_w == 0 || dst_h == 0 || sw == 0 || sh == 0 {
        return out;
    }
    let Some(inv) = invert3(m) else {
        return out;
    };
    let mf: [[f32; 3]; 3] = inv.map(|row| row.map(|v| v as f32));
    let (m_xx, m_yx, m_zx) = (mf[0][0] as f64, mf[1][0] as f64, mf[2][0] as f64);
    let big_w = sw.max(16) as f32;
    let big_h = sh.max(16) as f32;
    let data = &mut out.data;
    for y in 0..dst_h {
        let yf = y as f32;
        let m_x = yf * mf[0][1] + mf[0][2];
        let m_y = yf * mf[1][1] + mf[1][2];
        let m_z = yf * mf[2][1] + mf[2][2];
        for x in 0..dst_w {
            let xf = x as f64;
            let invz = 1.0 / (m_z as f64 + m_zx * xf);
            let fx = ((m_x as f64 + m_xx * xf) * invz) as f32;
            let fy = ((m_y as f64 + m_yx * xf) * invz) as f32;
            // NaN (invz infinite) clamps like the SIMD min/max would to the low bound.
            let vx = if fx.is_nan() {
                -big_w
            } else {
                fx.clamp(-big_w, big_w * 2.0)
            };
            let vy = if fy.is_nan() {
                -big_h
            } else {
                fy.clamp(-big_h, big_h * 2.0)
            };
            let ix = vx.floor();
            let iy = vy.floor();
            let wx = bicubic_weights(vx - ix);
            let wy = bicubic_weights(vy - iy);
            let (ix, iy) = (ix as i64 - 1, iy as i64 - 1);
            let mut xo = [0usize; 4];
            let mut yo = [0usize; 4];
            for i in 0..4 {
                xo[i] = (ix + i as i64).clamp(0, sw as i64 - 1) as usize * 3;
                yo[i] = (iy + i as i64).clamp(0, sh as i64 - 1) as usize;
            }
            let mut acc = [0.0f32; 3];
            for (r, &yy) in yo.iter().enumerate() {
                let row = src.row(yy);
                for (c, a) in acc.iter_mut().enumerate() {
                    let v = |j: usize| row[xo[j] + c] as f32;
                    let mut sum = v(1).mul_add(wx[1], v(0) * wx[0]);
                    sum = v(2).mul_add(wx[2], sum);
                    sum = v(3).mul_add(wx[3], sum);
                    *a = sum.mul_add(wy[r], *a);
                }
            }
            let dst = (y * dst_w + x) * 3;
            for c in 0..3 {
                data[dst + c] = (acc[c].round_ties_even() as i32).clamp(0, 255) as u8;
            }
        }
    }
    out
}

/// `cv2.rotate(img, ROTATE_90_COUNTERCLOCKWISE)`: `out[r][c] = in[c][W-1-r]`.
pub fn rotate90_ccw(img: &BgrImage) -> BgrImage {
    let (w, h) = (img.width, img.height);
    let mut data = Vec::with_capacity(w * h * 3);
    for r in 0..w {
        for c in 0..h {
            let i = (c * w + (w - 1 - r)) * 3;
            data.extend_from_slice(&img.data[i..i + 3]);
        }
    }
    BgrImage {
        width: h,
        height: w,
        data,
    }
}

/// `cv2.rotate(img, ROTATE_90_CLOCKWISE)`: `out[r][c] = in[H-1-c][r]`.
pub fn rotate90_cw(img: &BgrImage) -> BgrImage {
    let (w, h) = (img.width, img.height);
    let mut data = Vec::with_capacity(w * h * 3);
    for r in 0..w {
        for c in 0..h {
            let i = ((h - 1 - c) * w + r) * 3;
            data.extend_from_slice(&img.data[i..i + 3]);
        }
    }
    BgrImage {
        width: h,
        height: w,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_transform_keeps_pixels() {
        let img = BgrImage::from_raw(4, 3, (0..36).collect()).unwrap();
        let q = [[0.0f32, 0.0], [4.0, 0.0], [4.0, 3.0], [0.0, 3.0]];
        let m = get_perspective_transform(&q, &q).unwrap();
        assert_eq!(warp_perspective_cubic(img.view(), &m, 4, 3), img);
    }

    #[test]
    fn rotations_invert() {
        let img = BgrImage::from_raw(3, 2, (0..18).collect()).unwrap();
        let r = rotate90_ccw(&img);
        assert_eq!((r.width(), r.height()), (2, 3));
        // Top-right pixel goes to top-left.
        assert_eq!(&r.as_raw()[0..3], &img.as_raw()[6..9]);
        assert_eq!(rotate90_cw(&r), img);
    }
}
