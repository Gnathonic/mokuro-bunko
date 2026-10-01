//! Page images: decoding, and the OpenCV primitives the OCR needs, reimplemented to
//! match OpenCV 5.0 where it changes what the models see (spec Appendix B).
//!
//! Images are 8-bit, 3-channel, interleaved **BGR** (OpenCV order), row-major.

mod decode;
mod resize;
mod thumbnail;
mod warp;

pub use decode::{decode_bgr, decode_size};
pub use resize::resize_linear;
pub use thumbnail::{
    THUMBNAIL_BOX, THUMBNAIL_QUALITY, contain_size, make_thumbnail, resize_lanczos_rgb,
};
pub use warp::{
    Matrix3, get_perspective_transform, rotate90_ccw, rotate90_cw, warp_perspective_cubic,
};

/// An owned 8-bit BGR image.
#[derive(Clone, PartialEq, Eq)]
pub struct BgrImage {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl std::fmt::Debug for BgrImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BgrImage({}x{})", self.width, self.height)
    }
}

impl BgrImage {
    /// An image from interleaved BGR bytes (`data.len() == width * height * 3`).
    pub fn from_raw(width: usize, height: usize, data: Vec<u8>) -> Option<Self> {
        (data.len() == width * height * 3).then_some(Self {
            width,
            height,
            data,
        })
    }

    /// A black image.
    pub fn zeros(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            data: vec![0; width * height * 3],
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Interleaved BGR bytes, row-major, no padding.
    pub fn as_raw(&self) -> &[u8] {
        &self.data
    }

    pub fn into_raw(self) -> Vec<u8> {
        self.data
    }

    /// The whole image as a view.
    pub fn view(&self) -> ImageView<'_> {
        ImageView {
            data: &self.data,
            width: self.width,
            height: self.height,
            stride: self.width * 3,
        }
    }

    /// `bgr[y0:y1, x0:x1]` as a view (no copy). Bounds are clamped like numpy slicing.
    pub fn region(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> ImageView<'_> {
        self.view().region(x0, y0, x1, y1)
    }

    /// `bgr[y0:y1, x0:x1].copy()`.
    pub fn crop(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> BgrImage {
        self.region(x0, y0, x1, y1).to_owned()
    }

    /// Stack images like `np.concatenate(..., axis=1)` (same height) — test helper
    /// for composite pages.
    pub fn hconcat(parts: &[BgrImage]) -> Option<BgrImage> {
        let height = parts.first()?.height;
        if parts.iter().any(|p| p.height != height) {
            return None;
        }
        let width: usize = parts.iter().map(|p| p.width).sum();
        let mut data = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            for p in parts {
                data.extend_from_slice(p.view().row(y));
            }
        }
        Some(BgrImage {
            width,
            height,
            data,
        })
    }

    /// Stack images like `np.concatenate(..., axis=0)` (same width).
    pub fn vconcat(parts: &[BgrImage]) -> Option<BgrImage> {
        let width = parts.first()?.width;
        if parts.iter().any(|p| p.width != width) {
            return None;
        }
        let height = parts.iter().map(|p| p.height).sum();
        let mut data = Vec::with_capacity(width * height * 3);
        for p in parts {
            data.extend_from_slice(&p.data);
        }
        Some(BgrImage {
            width,
            height,
            data,
        })
    }
}

/// A borrowed rectangle of a BGR image.
#[derive(Clone, Copy)]
pub struct ImageView<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
    /// Bytes from one row to the next.
    stride: usize,
}

impl<'a> ImageView<'a> {
    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Row `y`: `width * 3` bytes.
    pub fn row(&self, y: usize) -> &'a [u8] {
        let start = y * self.stride;
        &self.data[start..start + self.width * 3]
    }

    /// Channel `c` of pixel `(x, y)`.
    #[inline]
    pub fn at(&self, x: usize, y: usize, c: usize) -> u8 {
        self.data[y * self.stride + x * 3 + c]
    }

    /// A sub-rectangle, clamped like numpy slicing.
    pub fn region(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> ImageView<'a> {
        let x1 = x1.min(self.width);
        let y1 = y1.min(self.height);
        let x0 = x0.min(x1);
        let y0 = y0.min(y1);
        let width = x1 - x0;
        let height = y1 - y0;
        if width == 0 || height == 0 {
            return ImageView {
                data: &[],
                width,
                height,
                stride: 0,
            };
        }
        let start = y0 * self.stride + x0 * 3;
        let end = (y1 - 1) * self.stride + x1 * 3;
        ImageView {
            data: &self.data[start..end],
            width,
            height,
            stride: self.stride,
        }
    }

    pub fn to_owned(&self) -> BgrImage {
        let mut data = Vec::with_capacity(self.width * self.height * 3);
        for y in 0..self.height {
            data.extend_from_slice(self.row(y));
        }
        BgrImage {
            width: self.width,
            height: self.height,
            data,
        }
    }
}
