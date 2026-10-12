//! Pillow's `Image.resize` (libImaging/Resample.c) for 8-bit RGB, bit-exact (spec §4.1),
//! and the float antialiased weights torch uses for hayai's position-table resize.

use crate::image::Rgb;

const PRECISION_BITS: u32 = 22;

/// Resampling filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    /// `Image.BILINEAR` (triangle, support 1).
    Bilinear,
    /// `Image.BICUBIC` (a = −0.5, support 2).
    Bicubic,
}

impl Filter {
    fn support(self) -> f64 {
        match self {
            Filter::Bilinear => 1.0,
            Filter::Bicubic => 2.0,
        }
    }

    fn kernel(self, x: f64) -> f64 {
        let x = x.abs();
        match self {
            Filter::Bilinear => {
                if x < 1.0 {
                    1.0 - x
                } else {
                    0.0
                }
            }
            Filter::Bicubic => {
                const A: f64 = -0.5;
                if x < 1.0 {
                    ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0
                } else if x < 2.0 {
                    (((x - 5.0) * x + 8.0) * x - 4.0) * A
                } else {
                    0.0
                }
            }
        }
    }
}

/// Per output index: `(xmin, count, normalised f64 weights)` — Pillow's `precompute_coeffs`
/// for the whole-image box.
pub(crate) fn coeffs(in_size: usize, out_size: usize, filter: Filter) -> Vec<(usize, Vec<f64>)> {
    let scale = in_size as f64 / out_size as f64;
    let fs = scale.max(1.0);
    let support = filter.support() * fs;
    let ss = 1.0 / fs;
    (0..out_size)
        .map(|xx| {
            let center = (xx as f64 + 0.5) * scale;
            // C `(int)` truncates toward zero, then clamps
            let xmin = ((center - support + 0.5) as i64).max(0) as usize;
            let xmax = ((center + support + 0.5) as i64).min(in_size as i64) as usize;
            let n = xmax.saturating_sub(xmin);
            let mut k: Vec<f64> = (0..n)
                .map(|x| filter.kernel((x as f64 + xmin as f64 - center + 0.5) * ss))
                .collect();
            let ww: f64 = k.iter().sum();
            if ww != 0.0 {
                for v in &mut k {
                    *v /= ww;
                }
            }
            (xmin, k)
        })
        .collect()
}

fn fixed(k: &[f64]) -> Vec<i32> {
    let one = f64::from(1u32 << PRECISION_BITS);
    k.iter()
        .map(|&v| {
            if v < 0.0 {
                (-0.5 + v * one) as i32
            } else {
                (0.5 + v * one) as i32
            }
        })
        .collect()
}

#[inline]
fn clip8(v: i64) -> u8 {
    let shifted = v >> PRECISION_BITS;
    shifted.clamp(0, 255) as u8
}

/// `img.resize((out_w, out_h), filter)` for an RGB image, as Pillow 12 computes it.
pub fn resize(img: &Rgb, out_w: usize, out_h: usize, filter: Filter) -> Rgb {
    let (in_w, in_h) = (img.width, img.height);
    if out_w == in_w && out_h == in_h {
        return img.clone();
    }
    let hz: Vec<(usize, Vec<i32>)> = coeffs(in_w, out_w, filter)
        .into_iter()
        .map(|(m, k)| (m, fixed(&k)))
        .collect();
    let mut vt: Vec<(usize, Vec<i32>)> = coeffs(in_h, out_h, filter)
        .into_iter()
        .map(|(m, k)| (m, fixed(&k)))
        .collect();
    let half = 1i64 << (PRECISION_BITS - 1);

    let mut cur: Rgb;
    let src: &Rgb = if out_w != in_w {
        // rows used by the vertical pass only
        let first = vt[0].0;
        let last = vt[out_h - 1].0 + vt[out_h - 1].1.len();
        for b in &mut vt {
            b.0 -= first;
        }
        let rows = last - first;
        cur = Rgb::new(out_w, rows);
        for y in 0..rows {
            let srow = &img.data[(y + first) * in_w * 3..(y + first + 1) * in_w * 3];
            let drow = &mut cur.data[y * out_w * 3..(y + 1) * out_w * 3];
            for (xx, (xmin, k)) in hz.iter().enumerate() {
                let mut acc = [half; 3];
                for (x, &w) in k.iter().enumerate() {
                    let p = (xmin + x) * 3;
                    for (a, &v) in acc.iter_mut().zip(&srow[p..p + 3]) {
                        *a += i64::from(v) * i64::from(w);
                    }
                }
                for (d, &a) in drow[xx * 3..xx * 3 + 3].iter_mut().zip(&acc) {
                    *d = clip8(a);
                }
            }
        }
        &cur
    } else {
        img
    };
    if out_h == in_h {
        return src.clone();
    }
    let w = src.width;
    let mut out = Rgb::new(w, out_h);
    for (yy, (ymin, k)) in vt.iter().enumerate() {
        let drow = &mut out.data[yy * w * 3..(yy + 1) * w * 3];
        for xx in 0..w {
            let mut acc = [half; 3];
            for (y, &wt) in k.iter().enumerate() {
                let p = ((ymin + y) * w + xx) * 3;
                for (a, &v) in acc.iter_mut().zip(&src.data[p..p + 3]) {
                    *a += i64::from(v) * i64::from(wt);
                }
            }
            for (d, &a) in drow[xx * 3..xx * 3 + 3].iter_mut().zip(&acc) {
                *d = clip8(a);
            }
        }
    }
    out
}

/// Dense `(out, in)` weights of torch's antialiased bilinear resize (`align_corners=False`):
/// the Pillow coefficients in f64, normalised, cast to f32, no integer step (spec §5.4).
pub fn aa_weights(in_size: usize, out_size: usize) -> Vec<f32> {
    let mut w = vec![0f32; in_size * out_size];
    for (o, (xmin, k)) in coeffs(in_size, out_size, Filter::Bilinear)
        .into_iter()
        .enumerate()
    {
        for (j, v) in k.into_iter().enumerate() {
            w[o * in_size + xmin + j] = v as f32;
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_size_is_copy_and_uniform_stays_uniform() {
        let img = Rgb {
            width: 7,
            height: 5,
            data: vec![77; 7 * 5 * 3],
        };
        assert_eq!(resize(&img, 7, 5, Filter::Bicubic), img);
        for (w, h) in [(3, 2), (20, 11), (7, 9), (2, 5)] {
            for f in [Filter::Bilinear, Filter::Bicubic] {
                let r = resize(&img, w, h, f);
                assert_eq!((r.width, r.height), (w, h));
                assert!(r.data.iter().all(|&v| v == 77), "{w}x{h} {f:?}");
            }
        }
    }

    #[test]
    fn aa_weights_rows_sum_to_one() {
        for (i, o) in [(16, 5), (16, 16), (16, 40)] {
            let w = aa_weights(i, o);
            for r in 0..o {
                let s: f32 = w[r * i..(r + 1) * i].iter().sum();
                assert!((s - 1.0).abs() < 1e-5);
            }
        }
    }
}
