//! JPEG XL through jxl-rs (BSD-3-Clause), the decoder Chromium and Firefox ship, hooked
//! into the `image` crate, plus the memory budget that hook does not enforce.
//!
//! The hook (`jxl-image-rs-integration`) checks the `image` crate's limits against the
//! output buffer only. jxl-rs itself decodes each frame into 32-bit planes sized from
//! that frame's header, and a frame may be larger than the image. [`check_budget`]
//! reads those headers first, and refuses a file whose planes would not fit.

use jxl::api::{Event, JxlDecoder, JxlDecoderOptions};

/// Bytes of decoder planes per sample: jxl-rs keeps channels as f32 or i32 planes.
const BYTES_PER_SAMPLE: u64 = 4;

/// The two JPEG XL signatures: a bare codestream and the ISO BMFF container.
const CODESTREAM_SIGNATURE: [u8; 2] = [0xFF, 0x0A];
const CONTAINER_SIGNATURE: [u8; 12] = [
    0x00, 0x00, 0x00, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A,
];

/// Teach the `image` crate JPEG XL, once per process. The hook is process-wide;
/// registering it again (another crate's decoder got there first) is a no-op.
pub fn register() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        jxl_image_rs_integration::register_image_decoding_hook();
    });
}

/// Whether `bytes` start like a JPEG XL file.
pub fn is_jxl(bytes: &[u8]) -> bool {
    bytes.starts_with(&CODESTREAM_SIGNATURE) || bytes.starts_with(&CONTAINER_SIGNATURE)
}

/// Refuse a JPEG XL file whose image, preview or any frame holds more samples (pixels
/// × channels) than `max_alloc` bytes of decoder planes can take. Only headers and
/// tables of contents are read; nothing is decoded. Anything that is not JPEG XL
/// passes: the `image` crate's limits cover the other decoders.
pub fn check_budget(bytes: &[u8], max_alloc: u64) -> Result<(), String> {
    if !is_jxl(bytes) {
        return Ok(());
    }
    let mut options = JxlDecoderOptions::default();
    options.scan_frames_only = true;
    options.sample_limit =
        Some(usize::try_from(max_alloc / BYTES_PER_SAMPLE).unwrap_or(usize::MAX));
    let mut decoder = JxlDecoder::new(options);
    let mut input = bytes;
    loop {
        let event = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            decoder.process(&mut input, None, None)
        }))
        .map_err(|_| "JPEG XL: the decoder panicked".to_string())?
        .map_err(|e| format!("JPEG XL: {e}"))?;
        // A truncated file stops here too; decoding it then fails on its own.
        if matches!(event, Event::Complete | Event::NeedMoreInput { .. }) {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../bunko-ocr/tests/fixtures/jxl")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    #[test]
    fn knows_both_signatures() {
        assert!(is_jxl(&fixture("page-rgb.jxl")));
        assert!(is_jxl(&fixture("page-container.jxl")));
        assert!(!is_jxl(&fixture("page.jpg")));
        assert!(!is_jxl(b""));
    }

    /// 24×16 RGB is 1152 samples, 4608 bytes of planes.
    #[test]
    fn refuses_planes_over_the_budget() {
        for name in ["page-rgb.jxl", "page-container.jxl"] {
            let page = fixture(name);
            assert!(check_budget(&page, 4608).is_err(), "{name}");
            assert!(check_budget(&page, 4612).is_ok(), "{name}");
        }
    }

    #[test]
    fn other_formats_pass() {
        assert!(check_budget(&fixture("page.jpg"), 0).is_ok());
        assert!(check_budget(b"not an image", 0).is_ok());
    }

    #[test]
    fn a_damaged_file_is_an_error_or_truncated_not_a_panic() {
        let page = fixture("page-rgb.jxl");
        for cut in 2..page.len() {
            let _ = check_budget(&page[..cut], 1 << 20);
        }
        let mut garbage = CODESTREAM_SIGNATURE.to_vec();
        garbage.extend([0xFF; 64]);
        let _ = check_budget(&garbage, 1 << 20);
    }
}
