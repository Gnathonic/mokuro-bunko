//! The AnimeText block detector's input tensor, shared by its two routes: the ONNX
//! Runtime detector on the CPU (`bunko-ocr`) and the libtorch detector on a GPU
//! (`bunko-torch`). Both build the network's `images` tensor from the same letterboxed
//! RGB8 canvas with [`canvas_chw`], so the two routes feed the network the same floats.

use crate::image::Rgb;

/// The longest side a canvas may have (the network's input side).
pub const MAX_SIDE: usize = 640;
/// Canvas sides are multiples of the network's largest stride.
pub const STRIDE: usize = 32;

/// The canvas as the network's `images` tensor without its batch axis: `[3, h, w]`,
/// red plane first, each byte as `f32::from(v) / 255.0`.
pub fn canvas_chw(canvas: &Rgb) -> Vec<f32> {
    let plane = canvas.width * canvas.height;
    let mut x = vec![0.0f32; 3 * plane];
    for (at, px) in canvas
        .data
        .as_chunks::<3>()
        .0
        .iter()
        .enumerate()
        .take(plane)
    {
        x[at] = f32::from(px[0]) / 255.0;
        x[plane + at] = f32::from(px[1]) / 255.0;
        x[2 * plane + at] = f32::from(px[2]) / 255.0;
    }
    x
}

/// Whether a canvas of `width` x `height` is one the network takes: both sides
/// positive multiples of [`STRIDE`], at most [`MAX_SIDE`].
pub fn canvas_ok(width: usize, height: usize) -> bool {
    let side = |v: usize| v > 0 && v.is_multiple_of(STRIDE) && v <= MAX_SIDE;
    side(width) && side(height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chw_puts_red_first() {
        let c = Rgb::from_raw(2, 1, vec![255, 0, 51, 0, 255, 102]).unwrap();
        assert_eq!(
            canvas_chw(&c),
            [1.0, 0.0, 0.0, 1.0, 51.0 / 255.0, 102.0 / 255.0]
        );
        assert!(canvas_chw(&Rgb::new(0, 0)).is_empty());
    }

    #[test]
    fn canvases_are_sides_of_32_up_to_640() {
        assert!(canvas_ok(640, 32));
        assert!(canvas_ok(416, 640));
        assert!(!canvas_ok(0, 32));
        assert!(!canvas_ok(32, 0));
        assert!(!canvas_ok(33, 32));
        assert!(!canvas_ok(672, 32));
    }
}
