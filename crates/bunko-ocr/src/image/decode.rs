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
//! upsampling — Pillow's settings) and decode bit-identically. AVIF is not decoded
//! (no permissively licensed pure-Rust decoder); such a page fails decode and takes
//! the blank-page path, as a corrupt page does.

use super::BgrImage;
use crate::error::{Error, Result};

/// Decode an image file's bytes into BGR.
pub fn decode_bgr(bytes: &[u8]) -> Result<BgrImage> {
    #[cfg(feature = "libjpeg")]
    if bytes.starts_with(&[0xFF, 0xD8])
        && let Some(img) = decode_libjpeg(bytes)
    {
        return Ok(img);
    }
    let dynamic = ::image::load_from_memory(bytes).map_err(|e| Error::Decode(e.to_string()))?;
    let rgb = dynamic.into_rgb8();
    let (width, height) = (rgb.width() as usize, rgb.height() as usize);
    let mut data = rgb.into_raw();
    for px in data.chunks_exact_mut(3) {
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
    let reader = ::image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::Decode(e.to_string()))?;
    reader
        .into_dimensions()
        .map_err(|e| Error::Decode(e.to_string()))
}
