//! Line crops for the two recognizers (spec §3), from a BGR page and a detector quad.
//!
//! Quads are four points, TL, TR, BR, BL in the line's upright frame (the PP-OCR
//! detector's `order_quad`), in page pixels. Neither recognizer ever sees rotated
//! glyphs: a vertical column stays a column.

use crate::image::{Bgr, Rgb};
use crate::pyfmt::round_half_even;
use crate::warp::{Border, Interp, perspective_transform, warp_perspective};

/// A line quad: TL, TR, BR, BL, page pixels.
pub type Quad = [[f64; 2]; 4];

/// Glyph size of a hayai line strip (`TEXT_HEIGHT`).
pub const TEXT_HEIGHT: usize = 64;
/// Longest vertical strip read in one piece, in strip heights (`MAX_RATIO_VERTICAL`).
pub const MAX_RATIO_VERTICAL: usize = 16;
/// Longest horizontal strip read in one piece (`MAX_RATIO_HORIZONTAL`).
pub const MAX_RATIO_HORIZONTAL: usize = 8;
/// Search window around each chunk anchor, in strip heights (`ANCHOR_WINDOW`).
pub const ANCHOR_WINDOW: usize = 2;
/// The paddle LoRA's training margin, 12 % of the longer side (`UPRIGHT_MARGIN`).
pub const UPRIGHT_MARGIN: f64 = 0.12;
/// First-read margin of a paddle line crop, in ems (`LINE_MARGIN_EM`).
pub const LINE_MARGIN_EM: f64 = 0.25;
/// Second-read margin of a paddle line crop, in ems (`SECOND_MARGIN_EM`).
pub const SECOND_MARGIN_EM: f64 = 0.5;
/// Smallest crop side the LoRA was trained on (`MIN_CROP_SIDE`).
pub const MIN_CROP_SIDE: f64 = 16.0;

/// `cv2.getGaussianKernel(128, 8.0)` from OpenCV 5.0, first half (spec §3.4); `K[127-i] = K[i]`.
const GAUSS_HALF: [f64; 64] = [
    1.0389895019074014e-15,
    2.7804800927626877e-15,
    7.325589342867247e-15,
    1.900113298220203e-14,
    4.852109287460303e-14,
    1.2198201404822987e-13,
    3.019083842773331e-13,
    7.356456934342762e-13,
    1.7647222790927795e-12,
    4.167716688152101e-12,
    9.690231624253081e-12,
    2.2181161089654484e-11,
    4.998601813631643e-11,
    1.1089882831357237e-10,
    2.4222531072100706e-10,
    5.208662698467078e-10,
    1.1026739019184627e-09,
    2.298169761000774e-09,
    4.715538172432299e-09,
    9.525648951343214e-09,
    1.8944014991623525e-08,
    3.709058078072457e-08,
    7.149396568086143e-08,
    1.3567170722359226e-07,
    2.534681185584036e-07,
    4.661992183999863e-07,
    8.441777275272705e-07,
    1.504909511410206e-06,
    2.64119844630433e-06,
    4.563581678754164e-06,
    7.762913937226354e-06,
    1.300043438435446e-05,
    2.143409269782652e-05,
    3.479096654644236e-05,
    5.5595806240599604e-05,
    8.746448026196381e-05,
    0.00013546763708401668,
    0.00020656347932306637,
    0.0003100885094505717,
    0.00045828110906597245,
    0.0006667950791726975,
    0.0009551398514655293,
    0.0013469630855317647,
    0.0018700730459375219,
    0.0025560867919538536,
    0.0034395907356138287,
    0.004556717382806207,
    0.0059430798490069815,
    0.007631065579529709,
    0.009646570775100003,
    0.012005351300600717,
    0.01470926389308055,
    0.017742759054549573,
    0.021070047708563668,
    0.024633381437753288,
    0.028352848238740617,
    0.03212798612176742,
    0.03584135850494638,
    0.03936403145442814,
    0.04256266634277,
    0.045307722478648345,
    0.047482085479490184,
    0.04898932866777286,
    0.04985808237175914,
];

fn gauss(i: usize) -> f64 {
    if i < 64 {
        GAUSS_HALF[i]
    } else {
        GAUSS_HALF[127 - i]
    }
}

fn norm_f32(dx: f32, dy: f32) -> f32 {
    (dx * dx + dy * dy).sqrt()
}

/// `warp_line` (ER:1202): the quad deskewed to a strip 64 px thick, orientation kept
/// (a vertical line becomes a 64-px-wide column). Black outside the page.
pub fn warp_line(page: &Bgr, quad: &Quad, vertical: bool) -> Bgr {
    let src: [[f32; 2]; 4] = quad.map(|p| [p[0] as f32, p[1] as f32]);
    let mid: [[f32; 2]; 4] = std::array::from_fn(|i| {
        [
            (src[(i + 1) % 4][0] + src[i][0]) / 2.0,
            (src[(i + 1) % 4][1] + src[i][1]) / 2.0,
        ]
    });
    let len_v = norm_f32(mid[2][0] - mid[0][0], mid[2][1] - mid[0][1]);
    let len_h = norm_f32(mid[1][0] - mid[3][0], mid[1][1] - mid[3][1]);
    let ratio = f64::from(len_v / len_h.max(1e-6));
    let th = TEXT_HEIGHT as f64;
    let (w, h) = if vertical {
        (TEXT_HEIGHT, round_half_even(th * ratio).max(1) as usize)
    } else {
        (
            round_half_even(th / ratio.max(1e-6)).max(1) as usize,
            TEXT_HEIGHT,
        )
    };
    let (wf, hf) = ((w - 1) as f32, (h - 1) as f32);
    let dst = [[0.0, 0.0], [wf, 0.0], [wf, hf], [0.0, hf]];
    perspective_transform(&src, &dst)
        .and_then(|m| warp_perspective(page, &m, w, h, Interp::Linear, Border::Black))
        .unwrap_or_else(|| Bgr::new(w, h))
}

/// `chunk_cut_points` (ER:1140): cut columns at the density minimum nearest each of
/// `n - 1` equally spaced anchors.
pub fn chunk_cut_points(density: &[f64], width: usize, n: usize, window: usize) -> Vec<usize> {
    if n <= 1 || width == 0 {
        return Vec::new();
    }
    let mut cuts = Vec::with_capacity(n - 1);
    for k in 1..n {
        let anchor = round_half_even(width as f64 * k as f64 / n as f64).max(0) as usize;
        let lo = anchor.saturating_sub(window / 2);
        let hi = width.min(anchor + window / 2);
        if hi <= lo {
            cuts.push(anchor);
            continue;
        }
        let mut best = lo;
        for i in lo + 1..hi {
            if density[i] < density[best] {
                best = i;
            }
        }
        cuts.push(best);
    }
    cuts
}

/// `split_long_line` (ER:1230) on a horizontal strip: pieces no longer than
/// `max_ratio` strip heights, cut between glyphs (smoothed column-ink minima).
pub fn split_long_line(strip: &Bgr, max_ratio: usize) -> Vec<Bgr> {
    let (w, h) = (strip.width, strip.height);
    let ratio = w as f64 / h.max(1) as f64;
    if ratio <= max_ratio as f64 {
        return vec![strip.clone()];
    }
    let n = (ratio / max_ratio as f64).ceil() as usize;
    let gray = strip.gray();
    let mut density = vec![0f64; w];
    for y in 0..h {
        for (x, d) in density.iter_mut().enumerate() {
            *d += f64::from(255 - gray[y * w + x]);
        }
    }
    // np.convolve(density, K, "same") for the even 128-tap kernel: full[i + 63].
    let klen = 2 * TEXT_HEIGHT;
    let off = (klen - 1) / 2;
    let smooth: Vec<f64> = (0..w)
        .map(|i| {
            let nfull = i + off; // index into the full convolution
            let kmin = nfull.saturating_sub(klen - 1);
            let kmax = nfull.min(w - 1);
            let mut s = 0.0;
            for (k, d) in density.iter().enumerate().take(kmax + 1).skip(kmin) {
                s += d * gauss(nfull - k);
            }
            s
        })
        .collect();
    let cuts = chunk_cut_points(&smooth, w, n, ANCHOR_WINDOW * TEXT_HEIGHT);
    let mut bounds = Vec::with_capacity(cuts.len() + 2);
    bounds.push(0);
    bounds.extend(cuts);
    bounds.push(w);
    bounds
        .windows(2)
        .filter(|ab| ab[1] > ab[0])
        .map(|ab| strip.columns(ab[0], ab[1]))
        .collect()
}

/// hayai-nova's crops of one line (`make_line_crop_fn`, ER:1253): the deskewed strip,
/// chunked when long, as RGB, glyphs upright. A vertical line is rotated only while
/// it is chunked.
pub fn hayai_line_crops(page: &Bgr, quad: &Quad, vertical: bool) -> Vec<Rgb> {
    let region = warp_line(page, quad, vertical);
    let strip = if vertical {
        region.rotate_ccw()
    } else {
        region
    };
    let max_ratio = if vertical {
        MAX_RATIO_VERTICAL
    } else {
        MAX_RATIO_HORIZONTAL
    };
    split_long_line(&strip, max_ratio)
        .into_iter()
        .map(|c| if vertical { c.rotate_cw() } else { c })
        .map(|c| c.to_rgb())
        .collect()
}

/// `quad_extents(quad, vertical)` (ER:882): `(main, cross)` lengths between edge midpoints.
pub fn quad_extents(quad: &Quad, vertical: bool) -> (f64, f64) {
    let mid: [[f64; 2]; 4] = std::array::from_fn(|i| {
        [
            (quad[i][0] + quad[(i + 1) % 4][0]) / 2.0,
            (quad[i][1] + quad[(i + 1) % 4][1]) / 2.0,
        ]
    });
    let (vx, vy) = (mid[2][0] - mid[0][0], mid[2][1] - mid[0][1]);
    let (hx, hy) = (mid[1][0] - mid[3][0], mid[1][1] - mid[3][1]);
    let len_v = (vx * vx + vy * vy).sqrt();
    let len_h = (hx * hx + hy * hy).sqrt();
    if vertical {
        (len_v, len_h)
    } else {
        (len_h, len_v)
    }
}

/// `line_margin_px` (ER:1284): the smaller of 12 % of the long side and `margin_em` ems.
pub fn line_margin_px(quad: &Quad, margin_em: f64) -> f64 {
    let (main, cross) = quad_extents(quad, true);
    (UPRIGHT_MARGIN * main.max(cross)).min(margin_em * main.min(cross))
}

/// `padded_quad` (ER:1290): grown by `pad` px on every side along the quad's own axes.
pub fn padded_quad(quad: &Quad, pad: f64) -> Quad {
    fn unit(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let n = (dx * dx + dy * dy).sqrt();
        let n = if n == 0.0 { 1.0 } else { n };
        [dx / n, dy / n]
    }
    let u = unit(quad[0], quad[1]);
    let v = unit(quad[0], quad[3]);
    const SIGNS: [(f64, f64); 4] = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)];
    std::array::from_fn(|i| {
        let (su, sv) = SIGNS[i];
        [
            quad[i][0] + pad * (su * u[0] + sv * v[0]),
            quad[i][1] + pad * (su * u[1] + sv * v[1]),
        ]
    })
}

/// paddle-manga's crop of one line (`make_quad_crop_fn`, ER:1313): the quad padded by
/// [`line_margin_px`], deskewed with bicubic sampling and replicated edges, orientation
/// kept, the short side scaled up to at least 16 px. `margin_em` is
/// [`LINE_MARGIN_EM`] for the first read and [`SECOND_MARGIN_EM`] for the second.
pub fn paddle_quad_crop(page: &Bgr, quad: &Quad, margin_em: f64) -> Rgb {
    let padded = padded_quad(quad, line_margin_px(quad, margin_em));
    let src: [[f32; 2]; 4] = padded.map(|p| [p[0] as f32, p[1] as f32]);
    let width = f64::from(norm_f32(src[1][0] - src[0][0], src[1][1] - src[0][1]));
    let height = f64::from(norm_f32(src[3][0] - src[0][0], src[3][1] - src[0][1]));
    let scale = (MIN_CROP_SIDE / width.min(height).max(1.0)).max(1.0);
    let w = round_half_even(width * scale).max(2) as usize;
    let h = round_half_even(height * scale).max(2) as usize;
    let (wf, hf) = (w as f32, h as f32);
    let dst = [[0.0, 0.0], [wf, 0.0], [wf, hf], [0.0, hf]];
    perspective_transform(&src, &dst)
        .and_then(|m| warp_perspective(page, &m, w, h, Interp::Cubic, Border::Replicate))
        .unwrap_or_else(|| Bgr::new(w, h))
        .to_rgb()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_sums_to_one() {
        let s: f64 = (0..128).map(gauss).sum();
        assert!((s - 1.0).abs() < 1e-12, "{s}");
    }

    #[test]
    fn cut_points_pick_first_minimum() {
        let mut d = vec![5.0; 300];
        d[140] = 1.0;
        d[160] = 1.0;
        assert_eq!(chunk_cut_points(&d, 300, 2, 128), vec![140]);
        assert!(chunk_cut_points(&d, 300, 1, 128).is_empty());
    }

    #[test]
    fn short_strip_is_not_split() {
        let s = Bgr::new(64 * 8, 64);
        assert_eq!(split_long_line(&s, 8).len(), 1);
        let s = Bgr::new(64 * 8 + 1, 64);
        let parts = split_long_line(&s, 8);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts.iter().map(|p| p.width).sum::<usize>(), 64 * 8 + 1);
    }

    #[test]
    fn split_cuts_at_white_gap() {
        // 64 x 1200 strip, ink everywhere except a white gap at 590..610
        let (w, h) = (1200, 64);
        let mut s = Bgr::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let v = if (590..610).contains(&x) { 255 } else { 0 };
                let i = (y * w + x) * 3;
                s.data[i..i + 3].fill(v);
            }
        }
        let parts = split_long_line(&s, 16);
        assert_eq!(parts.len(), 2);
        assert!((590..610).contains(&parts[0].width), "{}", parts[0].width);
    }

    #[test]
    fn line_crop_shapes() {
        let page = Bgr::new(200, 400);
        let quad = [[10.0, 10.0], [42.0, 10.0], [42.0, 330.0], [10.0, 330.0]];
        let crops = hayai_line_crops(&page, &quad, true);
        assert_eq!(crops.len(), 1);
        assert_eq!((crops[0].width, crops[0].height), (64, 640));
        // 32 x 320 column at ratio 10 → 64 x 640 → one crop (≤ 16)
        let quad_h = [[10.0, 10.0], [330.0, 10.0], [330.0, 42.0], [10.0, 42.0]];
        let crops = hayai_line_crops(&page, &quad_h, false);
        assert_eq!(crops.len(), 2); // 640 / 64 = 10 > 8
        assert_eq!(crops.iter().map(|c| c.width).sum::<usize>(), 640);
        assert!(crops.iter().all(|c| c.height == 64));
    }

    #[test]
    fn quad_crop_padding() {
        let quad = [[10.0, 10.0], [42.0, 10.0], [42.0, 330.0], [10.0, 330.0]];
        // main 320, cross 32: pad = min(0.12*320, 0.25*32) = 8
        assert!((line_margin_px(&quad, LINE_MARGIN_EM) - 8.0).abs() < 1e-12);
        let p = padded_quad(&quad, 8.0);
        assert_eq!(p[0], [2.0, 2.0]);
        assert_eq!(p[2], [50.0, 338.0]);
        let page = Bgr::new(200, 400);
        let c = paddle_quad_crop(&page, &quad, LINE_MARGIN_EM);
        assert_eq!((c.width, c.height), (48, 336));
        // a tiny quad is scaled so its short side reaches 16 px
        let tiny = [[0.0, 0.0], [4.0, 0.0], [4.0, 8.0], [0.0, 8.0]];
        let c = paddle_quad_crop(&page, &tiny, LINE_MARGIN_EM);
        assert_eq!(c.width.min(c.height), 16);
    }
}
