//! AnimeText (YOLO12-x) on the pack: the host letterboxes the page
//! ([`bunko_ocr::animetext::canvas`]), the pack's optional `bt_detect_blocks` returns the
//! raw candidates scored [`MIN_SCORE`] or more, and the host turns them into blocks with
//! the CPU route's own [`blocks_from_output`]. Overlap suppression, containers and parts
//! therefore have one implementation for both routes: the device can only perturb the
//! raw floats.

use std::sync::Arc;

use bunko_ocr::animetext::{MIN_SCORE, SIDE, blocks_from_output, canvas};
use bunko_ocr::blocks::{BlockDetector, TextBlock};
use bunko_ocr::error::{Error, Result};
use bunko_ocr::image::BgrImage;
use bunko_ocr::lines::{DetectorInfo, DetectorPass};
use bunko_ocr::py;
use bunko_torch::abi::{DetectOut, LoadInfo};
use bunko_vlm::Rgb;

use super::loader::{Loaded, Pack};

/// What finds raw block candidates on letterboxed canvases: the pack's
/// `bt_detect_blocks`, or a stand-in in tests.
pub trait BlocksReader: Send + Sync {
    /// Per canvas, in order, its candidates scored `min_score` or more.
    fn detect_blocks(
        &self,
        canvases: &[&Rgb],
        min_score: f32,
    ) -> std::result::Result<Vec<DetectOut>, String>;
}

impl BlocksReader for Loaded {
    fn detect_blocks(
        &self,
        canvases: &[&Rgb],
        min_score: f32,
    ) -> std::result::Result<Vec<DetectOut>, String> {
        let raw: Vec<(&[u8], u32, u32)> = canvases
            .iter()
            .map(|c| (c.data.as_slice(), c.width as u32, c.height as u32))
            .collect();
        Loaded::detect_blocks(self, &raw, min_score)
    }
}

/// The block detector loaded on the pack (`bt_load("animetext-blocks", ..)`).
pub struct TorchDetector {
    /// What the pack reported at load (`bt_info`).
    pub info: LoadInfo,
    handle: Loaded,
    // The handle's functions live in the pack's library: keep it referenced.
    _pack: Arc<Pack>,
}

impl TorchDetector {
    pub(crate) fn new(pack: Arc<Pack>, handle: Loaded, info: LoadInfo) -> Self {
        Self {
            info,
            handle,
            _pack: pack,
        }
    }
}

impl BlocksReader for TorchDetector {
    fn detect_blocks(
        &self,
        canvases: &[&Rgb],
        min_score: f32,
    ) -> std::result::Result<Vec<DetectOut>, String> {
        BlocksReader::detect_blocks(&self.handle, canvases, min_score)
    }
}

/// Candidates `[cx, cy, w, h, score]` as the network's `[5, k]` output (row-major), the
/// shape [`blocks_from_output`] reads, and `k`.
pub fn candidates_as_output(c: &[[f32; 5]]) -> (Vec<f32>, usize) {
    let k = c.len();
    let mut out = vec![0.0f32; 5 * k];
    for (i, cand) in c.iter().enumerate() {
        for (row, v) in cand.iter().enumerate() {
            out[row * k + i] = *v;
        }
    }
    (out, k)
}

/// AnimeText behind the [`BlockDetector`] seam, one page a call on the pack's device.
pub struct TorchBlockDetector {
    reader: Arc<dyn BlocksReader>,
}

impl TorchBlockDetector {
    pub fn new(reader: Arc<dyn BlocksReader>) -> Self {
        Self { reader }
    }
}

impl std::fmt::Debug for TorchBlockDetector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TorchBlockDetector").finish_non_exhaustive()
    }
}

impl BlockDetector for TorchBlockDetector {
    fn detect(&self, bgr: &BgrImage) -> Result<(Vec<TextBlock>, DetectorInfo)> {
        let mut info = DetectorInfo {
            side: SIDE as u32,
            tile: "off".into(),
            ..Default::default()
        };
        // A page of zero width or height has no blocks: nothing goes to the pack.
        let Some(c) = canvas(bgr) else {
            return Ok((Vec::new(), info));
        };
        let found = self
            .reader
            .detect_blocks(&[&c.rgb], MIN_SCORE)
            .map_err(|e| Error::Invalid(format!("AnimeText on the GPU: {e}")))?;
        let [one] = found.as_slice() else {
            return Err(Error::ModelOutput(format!(
                "bt_detect_blocks returned {} results for 1 canvas",
                found.len()
            )));
        };
        let (out, k) = candidates_as_output(&one.cand);
        let blocks = blocks_from_output(&out, k, c.scale, c.page);
        info.passes.push(DetectorPass {
            scale: py::round_to(c.scale, 4),
            tiles: 1,
            lines: blocks.len(),
            why: "animetext".into(),
        });
        Ok((blocks, info))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bunko_ocr::animetext::blocks_from_output;
    use bunko_ocr::blocks::BlockDetector;
    use bunko_ocr::image::BgrImage;
    use bunko_torch::abi::DetectOut;
    use bunko_vlm::Rgb;

    use super::*;

    struct Fake(std::sync::atomic::AtomicUsize, Vec<[f32; 5]>);
    impl BlocksReader for Fake {
        fn detect_blocks(&self, c: &[&Rgb], _: f32) -> Result<Vec<DetectOut>, String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(c.iter()
                .map(|_| DetectOut {
                    n: 99,
                    cand: self.1.clone(),
                })
                .collect())
        }
    }

    #[test]
    fn torch_blocks_equal_the_onnx_post_processing() {
        let cand = vec![
            [100., 100., 50., 80., 0.9],
            [110., 105., 50., 80., 0.8],
            [300., 200., 60., 60., 0.35],
        ];
        let det = TorchBlockDetector::new(Arc::new(Fake(0.into(), cand.clone())));
        let page = BgrImage::from_raw(1280, 960, vec![200; 1280 * 960 * 3]).unwrap();
        let (blocks, info) = det.detect(&page).unwrap();
        let (out, k) = candidates_as_output(&cand);
        assert_eq!(blocks, blocks_from_output(&out, k, 0.5, (1280, 960)));
        assert_eq!(info.passes[0].why, "animetext");
    }

    #[test]
    fn a_zero_size_page_detects_nothing_without_a_call() {
        let fake = Arc::new(Fake(0.into(), vec![]));
        let det = TorchBlockDetector::new(fake.clone());
        assert!(det.detect(&BgrImage::zeros(0, 10)).unwrap().0.is_empty());
        assert_eq!(fake.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// The GPU route through this module against the CPU route on real pages: every
    /// page of `MOKURO_ANIMETEXT_PAGES` (a directory of images) detected by AnimeText on
    /// `gpu:0` through `bt_detect_blocks` and by [`bunko_ocr::animetext::AnimeText`]
    /// (ONNX Runtime, CPU) must give the same blocks in the same order, corners within
    /// 0.01 page px and no crop edge moved (`floor` x0/y0, `ceil` x1/y1). The variables
    /// are this test's own: `MOKURO_TORCH_PACK` (a pack with `bt_detect_blocks`),
    /// `MOKURO_ANIMETEXT_TORCH_PACKAGE` (the dir holding `detector.pt2`) and
    /// `MOKURO_ANIMETEXT_ONNX` (the mirrored `animetext_yolo12x.onnx`). Run it alone (one
    /// pack per process):
    /// `cargo test -p bunko-engines --features gpl-models --release --lib -- --ignored --exact torch::detector::tests::animetext_torch_matches_the_cpu_route`.
    #[test]
    #[ignore = "needs a backend pack with bt_detect_blocks, an AnimeText package, its ONNX file and pages (env vars)"]
    fn animetext_torch_matches_the_cpu_route() {
        use bunko_ocr::runtime::{ExecutionTarget, RuntimeOptions};

        let var = |k: &str| {
            std::path::PathBuf::from(
                std::env::var_os(k).unwrap_or_else(|| panic!("{k} is not set")),
            )
        };
        let pack = var(crate::torch::PACK_ENV);
        let package = var("MOKURO_ANIMETEXT_TORCH_PACKAGE");
        let onnx = var("MOKURO_ANIMETEXT_ONNX");
        let pages = var("MOKURO_ANIMETEXT_PAGES");
        let cache = tempfile::tempdir().unwrap();

        let backend = crate::torch::backend_in(&[], false)
            .unwrap_or_else(|e| panic!("{}: {e}", pack.display()));
        assert!(
            backend.pack.detects_blocks(),
            "{} has no bt_detect_blocks",
            pack.display()
        );
        let reader = backend
            .detector(&package, &onnx, "gpu:0", 4, Some(cache.path()))
            .unwrap();
        // Shared while held: a second load is the same handle.
        let again = backend
            .detector(&package, &onnx, "gpu:0", 4, Some(cache.path()))
            .unwrap();
        assert!(Arc::ptr_eq(&reader, &again));
        let gpu = TorchBlockDetector::new(reader);
        let opts = RuntimeOptions {
            intra_threads: 4,
            targets: vec![ExecutionTarget::Cpu],
            copies: 1,
            spinning: false,
        };
        let cpu = bunko_ocr::animetext::AnimeText::load(&onnx, &opts).unwrap();

        let mut files: Vec<_> = std::fs::read_dir(&pages)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        assert!(!files.is_empty(), "no pages in {}", pages.display());
        let (mut blocks, mut differ) = (0usize, Vec::new());
        for f in &files {
            let page = bunko_ocr::image::decode_bgr(&std::fs::read(f).unwrap()).unwrap();
            let (g, gi) = gpu.detect(&page).unwrap();
            let (c, ci) = cpu.detect(&page).unwrap();
            assert_eq!(gi, ci, "{}", f.display());
            blocks += c.len();
            let edges = |b: &[f32; 4]| [b[0].floor(), b[1].floor(), b[2].ceil(), b[3].ceil()];
            let same = g.len() == c.len()
                && g.iter().zip(&c).all(|(a, b)| {
                    a.bbox
                        .iter()
                        .zip(&b.bbox)
                        .all(|(x, y)| (x - y).abs() <= 0.01)
                        && edges(&a.bbox) == edges(&b.bbox)
                });
            if !same {
                differ.push(format!("{}: gpu {g:?} cpu {c:?}", f.display()));
            }
        }
        eprintln!(
            "AnimeText on gpu:0: {} of {} pages identical ({blocks} blocks on the CPU route)",
            files.len() - differ.len(),
            files.len()
        );
        assert!(differ.is_empty(), "{differ:#?}");
    }
}
