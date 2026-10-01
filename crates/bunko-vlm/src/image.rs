//! The two pixel buffers this crate deals in.
//!
//! Pages arrive as [`Bgr`] (what 0.5.2's pipeline worked on: Pillow decode → RGB →
//! `cv2.cvtColor(RGB2BGR)`), crops leave as [`Rgb`] (what the recognizers' processors
//! were fed). Both are tightly packed, row-major, 3 bytes per pixel, no stride padding.

/// A BGR u8 image: `data[(y * width + x) * 3 + c]`, c = 0 blue, 1 green, 2 red.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bgr {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

/// An RGB u8 image, same layout as [`Bgr`] with c = 0 red, 1 green, 2 blue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

macro_rules! impl_buf {
    ($t:ident) => {
        impl $t {
            /// A black image.
            pub fn new(width: usize, height: usize) -> Self {
                Self {
                    width,
                    height,
                    data: vec![0; width * height * 3],
                }
            }

            /// Wraps `data`; `None` when its length is not `width * height * 3`.
            pub fn from_raw(width: usize, height: usize, data: Vec<u8>) -> Option<Self> {
                (data.len() == width * height * 3).then_some(Self {
                    width,
                    height,
                    data,
                })
            }

            #[inline]
            pub fn px(&self, x: usize, y: usize) -> [u8; 3] {
                let i = (y * self.width + x) * 3;
                [self.data[i], self.data[i + 1], self.data[i + 2]]
            }

            /// Columns `a..b` of every row (`numpy img[:, a:b]`).
            pub fn columns(&self, a: usize, b: usize) -> Self {
                let w = b - a;
                let mut data = Vec::with_capacity(w * self.height * 3);
                for y in 0..self.height {
                    let row = &self.data[y * self.width * 3..(y + 1) * self.width * 3];
                    data.extend_from_slice(&row[a * 3..b * 3]);
                }
                Self {
                    width: w,
                    height: self.height,
                    data,
                }
            }

            /// `cv2.rotate(img, ROTATE_90_COUNTERCLOCKWISE)`: out(x, y) = in(w-1-y, x).
            pub fn rotate_ccw(&self) -> Self {
                let (w, h) = (self.width, self.height);
                let mut out = Self::new(h, w);
                for oy in 0..w {
                    for ox in 0..h {
                        let (sx, sy) = (w - 1 - oy, ox);
                        let s = (sy * w + sx) * 3;
                        let d = (oy * h + ox) * 3;
                        out.data[d..d + 3].copy_from_slice(&self.data[s..s + 3]);
                    }
                }
                out
            }

            /// `cv2.rotate(img, ROTATE_90_CLOCKWISE)`: out(x, y) = in(y, h-1-x).
            pub fn rotate_cw(&self) -> Self {
                let (w, h) = (self.width, self.height);
                let mut out = Self::new(h, w);
                for oy in 0..w {
                    for ox in 0..h {
                        let (sx, sy) = (oy, h - 1 - ox);
                        let s = (sy * w + sx) * 3;
                        let d = (oy * h + ox) * 3;
                        out.data[d..d + 3].copy_from_slice(&self.data[s..s + 3]);
                    }
                }
                out
            }
        }
    };
}

impl_buf!(Bgr);
impl_buf!(Rgb);

impl Bgr {
    /// Channel swap (`cv2.cvtColor(BGR2RGB)`), lossless.
    pub fn to_rgb(&self) -> Rgb {
        let mut data = self.data.clone();
        for p in data.chunks_exact_mut(3) {
            p.swap(0, 2);
        }
        Rgb {
            width: self.width,
            height: self.height,
            data,
        }
    }

    /// `cv2.cvtColor(BGR2GRAY)` as OpenCV 5.0 computes it: 15-bit fixed point
    /// `(B*3735 + G*19235 + R*9798 + 16384) >> 15` (spec §3.4, verified bit-exact).
    pub fn gray(&self) -> Vec<u8> {
        self.data
            .chunks_exact(3)
            .map(|p| {
                let v = u32::from(p[0]) * 3735
                    + u32::from(p[1]) * 19235
                    + u32::from(p[2]) * 9798
                    + 16384;
                (v >> 15) as u8
            })
            .collect()
    }
}

impl Rgb {
    /// Channel swap back to BGR.
    pub fn to_bgr(&self) -> Bgr {
        let mut data = self.data.clone();
        for p in data.chunks_exact_mut(3) {
            p.swap(0, 2);
        }
        Bgr {
            width: self.width,
            height: self.height,
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: usize, h: usize) -> Bgr {
        let data = (0..w * h * 3).map(|i| (i / 3) as u8).collect();
        Bgr {
            width: w,
            height: h,
            data,
        }
    }

    #[test]
    fn rotations_are_inverse() {
        let a = img(5, 3);
        let r = a.rotate_ccw();
        assert_eq!((r.width, r.height), (3, 5));
        // ccw: top-right pixel becomes top-left
        assert_eq!(r.px(0, 0), a.px(4, 0));
        assert_eq!(r.rotate_cw(), a);
        assert_eq!(a.rotate_cw().rotate_ccw(), a);
        // cw: bottom-left becomes top-left
        assert_eq!(a.rotate_cw().px(0, 0), a.px(0, 2));
    }

    #[test]
    fn gray_matches_opencv5_formula() {
        let b = Bgr {
            width: 2,
            height: 1,
            data: vec![255, 255, 255, 10, 200, 30],
        };
        let g = b.gray();
        assert_eq!(g[0], 255);
        assert_eq!(
            g[1],
            ((10 * 3735 + 200 * 19235 + 30 * 9798 + 16384) >> 15) as u8
        );
    }

    #[test]
    fn columns_slice() {
        let a = img(4, 2);
        let c = a.columns(1, 3);
        assert_eq!((c.width, c.height), (2, 2));
        assert_eq!(c.px(0, 1), a.px(1, 1));
    }
}
