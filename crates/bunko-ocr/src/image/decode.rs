//! Page decoding as the 0.5.2 runner does it: `PIL.Image.open(...).convert("RGB")`,
//! then a channel swap to BGR (spec §2.4).
//!
//! * first frame only; no EXIF orientation; alpha dropped without compositing;
//!   grey replicated; palettes expanded;
//! * the format is sniffed from the bytes, not the file name.
//!
//! Decoders are the `image` crate's pure-Rust ones. WebP (image-webp) decodes
//! bit-identically to libwebp/Pillow. JPEG (zune-jpeg) differs from Pillow's
//! libjpeg-turbo by ±1..5 on 0.5–5 % of the pixels (measured on the bench pages),
//! which flipped 3 of 721 golden lines; with the `libjpeg` feature, YCbCr and
//! greyscale JPEGs go through libjpeg-turbo's decoder (mozjpeg, ISLOW IDCT, fancy
//! upsampling — Pillow's settings) and decode bit-identically. JPEG XL (bare
//! codestream or container) goes through jxl-rs, the decoder Chromium and Firefox
//! ship, hooked into the `image` crate by [`bunko_thumb::jxl`], which also keeps its
//! planes within the decode budget. AVIF is not decoded
//! (no permissively licensed pure-Rust decoder); such a page fails decode and takes
//! the blank-page path, as a corrupt page does.

use super::BgrImage;
use crate::error::{Error, Result};

/// Decode an image file's bytes into BGR, within the `image` crate's default budget
/// (512 MiB of buffers).
pub fn decode_bgr(bytes: &[u8]) -> Result<BgrImage> {
    let budget = ::image::Limits::default().max_alloc.unwrap_or(u64::MAX);
    decode_bgr_limited(bytes, budget)
}

/// [`decode_bgr`] refusing images whose decode needs more than `max_alloc` bytes of
/// buffers. A decoder that panics on a broken page fails this page only.
pub fn decode_bgr_limited(bytes: &[u8], max_alloc: u64) -> Result<BgrImage> {
    bunko_thumb::jxl::register();
    bunko_thumb::jxl::check_budget(bytes, max_alloc).map_err(Error::Decode)?;
    #[cfg(feature = "libjpeg")]
    if bytes.starts_with(&[0xFF, 0xD8])
        && let Some(img) = decode_libjpeg(bytes)
    {
        return Ok(img);
    }
    let mut limits = ::image::Limits::default();
    limits.max_alloc = Some(max_alloc);
    let mut reader = ::image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::Decode(e.to_string()))?;
    reader.limits(limits);
    let dynamic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reader.decode()))
        .map_err(|_| Error::Decode("the image decoder panicked".into()))?
        .map_err(|e| Error::Decode(e.to_string()))?;
    let rgb = dynamic.into_rgb8();
    let (width, height) = (rgb.width() as usize, rgb.height() as usize);
    let mut data = rgb.into_raw();
    for px in data.as_chunks_mut::<3>().0 {
        px.swap(0, 2);
    }
    Ok(BgrImage {
        width,
        height,
        data,
    })
}

/// libjpeg-turbo (via mozjpeg) decode of a YCbCr or greyscale JPEG straight to
/// BGR; `None` for anything else (CMYK/YCCK keep the `image` crate's path) or on
/// error. mozjpeg reports fatal errors by unwinding, hence `catch_unwind`.
#[cfg(feature = "libjpeg")]
fn decode_libjpeg(bytes: &[u8]) -> Option<BgrImage> {
    use mozjpeg::{ColorSpace, Decompress};
    std::panic::catch_unwind(|| {
        let d = Decompress::new_mem(bytes).ok()?;
        if !matches!(
            d.color_space(),
            ColorSpace::JCS_YCbCr | ColorSpace::JCS_GRAYSCALE
        ) {
            return None;
        }
        let mut started = d.to_colorspace(ColorSpace::JCS_EXT_BGR).ok()?;
        let (width, height) = (started.width(), started.height());
        let data = started.read_scanlines::<u8>().ok()?;
        started.finish().ok()?;
        BgrImage::from_raw(width, height, data)
    })
    .ok()
    .flatten()
}

/// `(width, height)` from the image header only (what a blank page record needs).
pub fn decode_size(bytes: &[u8]) -> Result<(u32, u32)> {
    bunko_thumb::jxl::register();
    let reader = ::image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::Decode(e.to_string()))?;
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reader.into_dimensions()))
        .map_err(|_| Error::Decode("the image decoder panicked".into()))?
        .map_err(|e| Error::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/jxl")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// The pixels of `page-rgb.jxl` and `page-container.jxl`, in BGR (see the fixtures'
    /// README).
    fn rgb_fixture_bgr() -> Vec<u8> {
        let mut out = Vec::new();
        for y in 0..16u8 {
            for x in 0..24u8 {
                let blue = if (x + y) % 2 == 0 { 200 } else { 40 };
                out.extend([blue, 15 * y, 10 * x]);
            }
        }
        out
    }

    #[test]
    fn decodes_a_lossless_jxl_codestream() {
        let img = decode_bgr(&fixture("page-rgb.jxl")).unwrap();
        assert_eq!((img.width, img.height), (24, 16));
        assert_eq!(img.data, rgb_fixture_bgr());
    }

    #[test]
    fn decodes_a_jxl_container() {
        let img = decode_bgr(&fixture("page-container.jxl")).unwrap();
        assert_eq!((img.width, img.height), (24, 16));
        assert_eq!(img.data, rgb_fixture_bgr());
    }

    #[test]
    fn decodes_a_greyscale_jxl_to_three_equal_channels() {
        let img = decode_bgr(&fixture("page-grey.jxl")).unwrap();
        assert_eq!((img.width, img.height), (24, 16));
        let mut want = Vec::new();
        for y in 0..16u8 {
            for x in 0..24u8 {
                want.extend([10 * x + y; 3]);
            }
        }
        assert_eq!(img.data, want);
    }

    /// A JPEG transcoded to JPEG XL decodes to the JPEG's own pixels, give or take the
    /// rounding of a different IDCT.
    #[test]
    fn decodes_a_jpeg_transcoded_jxl_like_its_jpeg() {
        let jxl = decode_bgr(&fixture("page-from-jpeg.jxl")).unwrap();
        let jpeg = decode_bgr(&fixture("page.jpg")).unwrap();
        assert_eq!((jxl.width, jxl.height), (64, 48));
        assert_eq!((jxl.width, jxl.height), (jpeg.width, jpeg.height));
        let worst = jxl
            .data
            .iter()
            .zip(&jpeg.data)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(worst <= 3, "pixels differ by up to {worst}");
    }

    /// Regression (security review): a JPEG XL page whose decoder planes (24×16×3
    /// 32-bit samples, 4608 bytes) exceed the budget is refused before jxl-rs
    /// allocates them, even though its output (twice 1152 bytes) would fit.
    #[test]
    fn a_jxl_page_over_the_decode_budget_is_refused() {
        let page = fixture("page-rgb.jxl");
        assert!(matches!(
            decode_bgr_limited(&page, 3000),
            Err(Error::Decode(_))
        ));
        let img = decode_bgr_limited(&page, 24 * 16 * 3 * 4 + 4).unwrap();
        assert_eq!(img.data, rgb_fixture_bgr());
    }

    #[test]
    fn reads_a_jxl_size_from_its_header() {
        assert_eq!(decode_size(&fixture("page-rgb.jxl")).unwrap(), (24, 16));
        assert_eq!(
            decode_size(&fixture("page-container.jxl")).unwrap(),
            (24, 16)
        );
    }
}
