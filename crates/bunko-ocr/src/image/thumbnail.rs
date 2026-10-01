//! The volume cover thumbnail `<Volume>.webp` (spec §9): the cover page decoded
//! like Pillow's `convert("RGB")`, `ImageOps.contain(img, (250, 350), LANCZOS)`
//! (which also enlarges small covers), saved as lossy WebP, quality 85, method 6,
//! no metadata.
//!
//! The resampler is a port of Pillow's `ImagingResample` (two separable passes,
//! horizontal first, Lanczos-3 with support scaled for reduction, 22-bit fixed
//! point coefficients), so thumbnails look like the ones 0.5.2 made. The encoder is
//! libwebp (BSD-3-Clause) through the `webp` crate (MIT/Apache-2.0); the `image`
//! crate only writes lossless WebP. Byte-equality with Pillow's file is not a goal
//! (Pillow wraps the same libwebp encoder, but container details differ by version).

use crate::error::{Error, Result};

/// The box the cover is fitted into: `(width, height)`.
pub const THUMBNAIL_BOX: (u32, u32) = (250, 350);
/// WebP quality (0–100).
pub const THUMBNAIL_QUALITY: f32 = 85.0;
/// libwebp effort (0–6).
const THUMBNAIL_METHOD: i32 = 6;

/// `ImageOps.contain`'s output size for a `width × height` image in `bx`.
pub fn contain_size(width: u32, height: u32, bx: (u32, u32)) -> (u32, u32) {
    let im_ratio = width as f64 / height as f64;
    let dest_ratio = bx.0 as f64 / bx.1 as f64;
    let mut size = bx;
    if im_ratio != dest_ratio {
        if im_ratio > dest_ratio {
            let new_h = (height as f64 / width as f64 * bx.0 as f64).round_ties_even() as u32;
            if new_h != bx.1 {
                size = (bx.0, new_h);
            }
        } else {
            let new_w = (width as f64 / height as f64 * bx.1 as f64).round_ties_even() as u32;
            if new_w != bx.0 {
                size = (new_w, bx.1);
            }
        }
    }
    (size.0.max(1), size.1.max(1))
}

const PRECISION_BITS: u32 = 32 - 8 - 2;

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    let x = x * std::f64::consts::PI;
    x.sin() / x
}

fn lanczos(x: f64) -> f64 {
    if (-3.0..3.0).contains(&x) {
        sinc(x) * sinc(x / 3.0)
    } else {
        0.0
    }
}

/// Pillow `precompute_coeffs` + `normalize_coeffs_8bpc`: per output index the first
/// input index, the tap count and the fixed-point weights.
fn coeffs(in_size: usize, out_size: usize) -> (usize, Vec<(usize, usize)>, Vec<i32>) {
    let scale = in_size as f64 / out_size as f64;
    let filterscale = scale.max(1.0);
    let support = 3.0 * filterscale;
    let ksize = support.ceil() as usize * 2 + 1;
    let mut bounds = Vec::with_capacity(out_size);
    let mut kk = vec![0i32; out_size * ksize];
    for xx in 0..out_size {
        let center = (xx as f64 + 0.5) * scale;
        let ss = 1.0 / filterscale;
        let xmin = ((center - support + 0.5) as i64).max(0) as usize;
        let xmax = ((center + support + 0.5) as i64).min(in_size as i64) as usize;
        let n = xmax.saturating_sub(xmin);
        let mut w = vec![0.0f64; n];
        let mut ww = 0.0;
        for (x, slot) in w.iter_mut().enumerate() {
            *slot = lanczos((x as f64 + xmin as f64 - center + 0.5) * ss);
            ww += *slot;
        }
        for (x, v) in w.iter().enumerate() {
            let v = if ww != 0.0 { v / ww } else { *v };
            let scaled = v * (1u64 << PRECISION_BITS) as f64;
            kk[xx * ksize + x] = if v < 0.0 {
                (-0.5 + scaled) as i32
            } else {
                (0.5 + scaled) as i32
            };
        }
        bounds.push((xmin, n));
    }
    (ksize, bounds, kk)
}

#[inline]
fn clip8(v: i64) -> u8 {
    if v >= (1i64 << PRECISION_BITS << 8) {
        255
    } else if v <= 0 {
        0
    } else {
        (v >> PRECISION_BITS) as u8
    }
}

/// Pillow's `Image.resize(size, LANCZOS)` for an RGB (or BGR — channels are
/// independent) 8-bit image.
pub fn resize_lanczos_rgb(
    src: &[u8],
    width: usize,
    height: usize,
    out_w: usize,
    out_h: usize,
) -> Vec<u8> {
    if out_w == width && out_h == height {
        return src.to_vec();
    }
    let (kh, bh, wh) = coeffs(width, out_w);
    let (kv, bv, wv) = coeffs(height, out_h);
    // Rows the vertical pass reads.
    let y_first = bv.first().map_or(0, |b| b.0);
    let y_last = bv.last().map_or(0, |b| b.0 + b.1);
    let need_h = out_w != width;
    let need_v = out_h != height;

    let (tmp, tmp_w, row0) = if need_h {
        let rows = y_last - y_first;
        let mut tmp = vec![0u8; out_w * rows * 3];
        for y in 0..rows {
            let src_row = &src[(y + y_first) * width * 3..(y + y_first + 1) * width * 3];
            for (xx, &(xmin, n)) in bh.iter().enumerate() {
                let k = &wh[xx * kh..xx * kh + n];
                for c in 0..3 {
                    let mut ss: i64 = 1 << (PRECISION_BITS - 1);
                    for (x, &w) in k.iter().enumerate() {
                        ss += src_row[(xmin + x) * 3 + c] as i64 * w as i64;
                    }
                    tmp[(y * out_w + xx) * 3 + c] = clip8(ss);
                }
            }
        }
        (tmp, out_w, y_first)
    } else {
        (src.to_vec(), width, 0)
    };
    if !need_v {
        return tmp;
    }
    let mut out = vec![0u8; out_w * out_h * 3];
    for (yy, &(ymin, n)) in bv.iter().enumerate() {
        let k = &wv[yy * kv..yy * kv + n];
        for xx in 0..out_w {
            for c in 0..3 {
                let mut ss: i64 = 1 << (PRECISION_BITS - 1);
                for (y, &w) in k.iter().enumerate() {
                    ss += tmp[((ymin - row0 + y) * tmp_w + xx) * 3 + c] as i64 * w as i64;
                }
                out[(yy * out_w + xx) * 3 + c] = clip8(ss);
            }
        }
    }
    out
}

/// Encode the cover thumbnail from a page image's file bytes: returns the WebP bytes.
pub fn make_thumbnail(image_bytes: &[u8]) -> Result<Vec<u8>> {
    let rgb = ::image::load_from_memory(image_bytes)
        .map_err(|e| Error::Decode(e.to_string()))?
        .into_rgb8();
    let (w, h) = (rgb.width(), rgb.height());
    let (tw, th) = contain_size(w, h, THUMBNAIL_BOX);
    let pixels = resize_lanczos_rgb(
        rgb.as_raw(),
        w as usize,
        h as usize,
        tw as usize,
        th as usize,
    );
    let mut config =
        webp::WebPConfig::new().map_err(|()| Error::Encode("libwebp config".into()))?;
    config.lossless = 0;
    config.quality = THUMBNAIL_QUALITY;
    config.method = THUMBNAIL_METHOD;
    let encoded = webp::Encoder::from_rgb(&pixels, tw, th)
        .encode_advanced(&config)
        .map_err(|e| Error::Encode(format!("libwebp: {e:?}")))?;
    Ok(encoded.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contain_like_pillow() {
        assert_eq!(contain_size(1777, 2800, THUMBNAIL_BOX), (222, 350));
        assert_eq!(contain_size(3850, 2800, THUMBNAIL_BOX), (250, 182));
        assert_eq!(contain_size(100, 140, THUMBNAIL_BOX), (250, 350));
        assert_eq!(contain_size(50, 100, THUMBNAIL_BOX), (175, 350));
    }

    #[test]
    fn constant_image_stays_constant() {
        let src = vec![200u8; 40 * 30 * 3];
        let out = resize_lanczos_rgb(&src, 40, 30, 17, 23);
        assert!(out.iter().all(|&v| v == 200));
        let up = resize_lanczos_rgb(&src, 40, 30, 90, 70);
        assert!(up.iter().all(|&v| v == 200));
    }
}

#[cfg(test)]
mod pillow_golden {
    use base64::Engine;

    use super::*;

    #[test]
    fn lanczos_and_contain_match_pillow() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/pillow_golden.json"
        );
        let f: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json");
        let b64 = |v: &serde_json::Value| {
            base64::engine::general_purpose::STANDARD
                .decode(v.as_str().expect("b64"))
                .expect("b64")
        };
        let n = |v: &serde_json::Value| v.as_u64().expect("int") as usize;
        for c in f["lanczos"].as_array().expect("cases") {
            let out = resize_lanczos_rgb(
                &b64(&c["src"]),
                n(&c["w"]),
                n(&c["h"]),
                n(&c["W"]),
                n(&c["H"]),
            );
            assert_eq!(
                out,
                b64(&c["dst"]),
                "lanczos {}x{} -> {}x{}",
                c["w"],
                c["h"],
                c["W"],
                c["H"]
            );
        }
        for c in f["contain"].as_array().expect("contain") {
            let v: Vec<u32> = c
                .as_array()
                .expect("row")
                .iter()
                .map(|x| x.as_u64().unwrap_or(0) as u32)
                .collect();
            assert_eq!(
                contain_size(v[0], v[1], THUMBNAIL_BOX),
                (v[2], v[3]),
                "contain {}x{}",
                v[0],
                v[1]
            );
        }
    }

    #[test]
    fn thumbnail_is_lossy_webp_of_the_right_size() {
        let mut png = Vec::new();
        let img = ::image::RgbImage::from_fn(500, 700, |x, y| {
            ::image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        img.write_to(
            &mut std::io::Cursor::new(&mut png),
            ::image::ImageFormat::Png,
        )
        .expect("png");
        let webp = make_thumbnail(&png).expect("thumbnail");
        assert_eq!(&webp[0..4], b"RIFF");
        assert_eq!(&webp[12..16], b"VP8 ");
        let back = ::image::load_from_memory(&webp).expect("decode");
        assert_eq!((back.width(), back.height()), (250, 350));
    }
}
