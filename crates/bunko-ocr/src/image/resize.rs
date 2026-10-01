//! `cv2.resize(src_u8_bgr, (W, H), interpolation=INTER_LINEAR)`, bit-exact with
//! OpenCV 5.0 (spec B.1).
//!
//! OpenCV's 8-bit linear resize is fixed point: 11-bit coefficients, a horizontal
//! pass into `int` rows, then the SIMD vertical pass
//! `(((b0*(H0>>4))>>16) + ((b1*(H1>>4))>>16) + 2) >> 2`. A float bilinear differs by
//! ±1 on ~10 % of the pixels, which the models notice. Measured against cv2 5.0 on
//! random images (down- and upscaling): the horizontal coefficients are clamped at
//! the borders as `resize.cpp` does, the vertical ones are **not** (rows are clipped
//! instead) — with that, 0 mismatches. An exact 2× downscale on both axes is
//! OpenCV's `INTER_AREA` fast path (2×2 mean, rounded), as `cv::resize` switches.

use super::{BgrImage, ImageView};

const COEF_BITS: u32 = 11;
const COEF_SCALE: f32 = (1 << COEF_BITS) as f32;

/// Source index and fixed-point coefficients for each destination coordinate.
fn coefficients(src_n: usize, dst_n: usize, clamp: bool) -> Vec<(isize, i32, i32)> {
    // OpenCV: scale = 1/(dst/src) in double, then fx in float.
    let scale = 1.0 / (dst_n as f64 / src_n as f64);
    (0..dst_n)
        .map(|d| {
            let f = ((d as f64 + 0.5) * scale - 0.5) as f32;
            let mut s = f.floor() as isize;
            let mut f = f - s as f32;
            if clamp {
                if s < 0 {
                    f = 0.0;
                    s = 0;
                }
                if s >= src_n as isize - 1 {
                    f = 0.0;
                    s = src_n as isize - 1;
                }
            }
            let a0 = ((1.0f32 - f) * COEF_SCALE).round_ties_even() as i32;
            let a1 = (f * COEF_SCALE).round_ties_even() as i32;
            (s, a0, a1)
        })
        .collect()
}

#[inline]
fn clip(i: isize, n: usize) -> usize {
    i.clamp(0, n as isize - 1) as usize
}

/// Resize a BGR image (or a view of one) to `dst_w × dst_h` like `cv2.resize`
/// with `INTER_LINEAR`.
pub fn resize_linear(src: ImageView<'_>, dst_w: usize, dst_h: usize) -> BgrImage {
    let (sw, sh) = (src.width(), src.height());
    if dst_w == 0 || dst_h == 0 || sw == 0 || sh == 0 {
        return BgrImage::zeros(dst_w, dst_h);
    }
    if sw == dst_w && sh == dst_h {
        return src.to_owned();
    }
    if sw == 2 * dst_w && sh == 2 * dst_h {
        return area_half(src, dst_w, dst_h);
    }
    let xs = coefficients(sw, dst_w, true);
    let ys = coefficients(sh, dst_h, false);
    // Per destination column: the two source byte offsets and coefficients.
    let xmap: Vec<(usize, usize, i32, i32)> = xs
        .iter()
        .map(|&(s, a0, a1)| (clip(s, sw) * 3, clip(s + 1, sw) * 3, a0, a1))
        .collect();

    let row_len = dst_w * 3;
    let hpass = |y: usize, out: &mut Vec<i32>| {
        out.clear();
        let row = src.row(y);
        for &(o0, o1, a0, a1) in &xmap {
            for c in 0..3 {
                out.push(row[o0 + c] as i32 * a0 + row[o1 + c] as i32 * a1);
            }
        }
    };

    // Two cached horizontal rows, keyed by source row (OpenCV keeps a ring too).
    let mut cache: [(usize, Vec<i32>); 2] = [
        (usize::MAX, Vec::with_capacity(row_len)),
        (usize::MAX, Vec::with_capacity(row_len)),
    ];
    let mut data = vec![0u8; dst_w * dst_h * 3];
    for (dy, &(s, b0, b1)) in ys.iter().enumerate() {
        let r0 = clip(s, sh);
        let r1 = clip(s + 1, sh);
        for r in [r0, r1] {
            if cache[0].0 != r && cache[1].0 != r {
                // Evict the slot not holding the other row we need.
                let keep = if r == r0 { r1 } else { r0 };
                let slot = if cache[0].0 == keep { 1 } else { 0 };
                hpass(r, &mut cache[slot].1);
                cache[slot].0 = r;
            }
        }
        let h0 = if cache[0].0 == r0 {
            &cache[0].1
        } else {
            &cache[1].1
        };
        let h1 = if cache[0].0 == r1 {
            &cache[0].1
        } else {
            &cache[1].1
        };
        let out = &mut data[dy * row_len..(dy + 1) * row_len];
        for i in 0..row_len {
            let v = (((b0 * (h0[i] >> 4)) >> 16) + ((b1 * (h1[i] >> 4)) >> 16) + 2) >> 2;
            out[i] = v.clamp(0, 255) as u8;
        }
    }
    BgrImage {
        width: dst_w,
        height: dst_h,
        data,
    }
}

/// OpenCV's `resizeAreaFast` for an exact 2× reduction: `(a + b + c + d + 2) >> 2`.
fn area_half(src: ImageView<'_>, dst_w: usize, dst_h: usize) -> BgrImage {
    let mut data = Vec::with_capacity(dst_w * dst_h * 3);
    for y in 0..dst_h {
        let r0 = src.row(2 * y);
        let r1 = src.row(2 * y + 1);
        for x in 0..dst_w {
            for c in 0..3 {
                let i = 6 * x + c;
                let sum = r0[i] as u32 + r0[i + 3] as u32 + r1[i] as u32 + r1[i + 3] as u32;
                data.push(((sum + 2) >> 2) as u8);
            }
        }
    }
    BgrImage {
        width: dst_w,
        height: dst_h,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_constant() {
        let img = BgrImage::from_raw(3, 2, (0..18).collect()).unwrap();
        assert_eq!(resize_linear(img.view(), 3, 2), img);
        let flat = BgrImage::from_raw(5, 7, vec![77; 5 * 7 * 3]).unwrap();
        let out = resize_linear(flat.view(), 13, 3);
        assert!(out.as_raw().iter().all(|&v| v == 77));
    }
}
