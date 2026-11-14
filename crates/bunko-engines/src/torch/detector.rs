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

    /// The detector gate's largest corner difference, in page px (`verify.WORST_CORNER`).
    const CORNER: f32 = 0.01;

    /// How a GPU block set compares with the CPU route's.
    #[derive(Debug, PartialEq)]
    struct Parity {
        /// The largest corner difference over the paired blocks.
        worst: f32,
        /// Crop edges (`floor` x0/y0, `ceil` x1/y1) that differ over the paired blocks.
        edges_moved: usize,
        /// The pairs are not in the CPU route's order.
        reordered: bool,
    }

    impl Parity {
        fn passes(&self) -> bool {
            self.worst <= CORNER && self.edges_moved == 0
        }
    }

    /// Pair each CPU block with the unused GPU block nearest it (the largest corner
    /// difference), in the CPU route's order; `None` when the counts differ.
    fn match_blocks(cpu: &[TextBlock], gpu: &[TextBlock]) -> Option<Parity> {
        if cpu.len() != gpu.len() {
            return None;
        }
        let edges = |b: &[f32; 4]| [b[0].floor(), b[1].floor(), b[2].ceil(), b[3].ceil()];
        let off = |a: &[f32; 4], b: &[f32; 4]| {
            a.iter()
                .zip(b)
                .map(|(x, y)| (x - y).abs())
                .fold(0.0f32, f32::max)
        };
        let mut used = vec![false; gpu.len()];
        let mut p = Parity {
            worst: 0.0,
            edges_moved: 0,
            reordered: false,
        };
        for (i, c) in cpu.iter().enumerate() {
            let (j, d) = gpu
                .iter()
                .enumerate()
                .filter(|(j, _)| !used[*j])
                .map(|(j, g)| (j, off(&c.bbox, &g.bbox)))
                .min_by(|a, b| a.1.total_cmp(&b.1))?;
            used[j] = true;
            p.worst = p.worst.max(d);
            p.reordered |= i != j;
            p.edges_moved += edges(&c.bbox)
                .iter()
                .zip(edges(&gpu[j].bbox))
                .filter(|(a, b)| **a != *b)
                .count();
        }
        Some(p)
    }

    /// The gate's pages: a directory's images (recursively, sorted by path), or a list
    /// file's (one path a line, relative to the list's directory; blank lines and `#`
    /// comments skipped), as `verify.page_files` reads them.
    fn gate_pages(pages: &std::path::Path) -> Vec<std::path::PathBuf> {
        const SUFFIXES: [&str; 6] = ["png", "jpg", "jpeg", "webp", "avif", "bmp"];
        fn walk(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(d).unwrap_or_else(|e| panic!("{}: {e}", d.display())) {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| SUFFIXES.contains(&x.to_ascii_lowercase().as_str()))
                {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        if pages.is_dir() {
            walk(pages, &mut files);
            files.sort();
        } else {
            let text = std::fs::read_to_string(pages).unwrap_or_else(|e| {
                panic!("{}: no such directory or page list ({e})", pages.display())
            });
            let base = pages.parent().unwrap_or(std::path::Path::new("."));
            for line in text.lines().map(str::trim) {
                if !line.is_empty() && !line.starts_with('#') {
                    files.push(base.join(line));
                }
            }
            let absent: Vec<_> = files.iter().filter(|p| !p.is_file()).collect();
            assert!(
                absent.is_empty(),
                "{}: {} listed pages do not exist, first {:?}",
                pages.display(),
                absent.len(),
                &absent[..absent.len().min(3)]
            );
        }
        assert!(!files.is_empty(), "{}: no pages", pages.display());
        files
    }

    fn block(b: [f32; 4]) -> TextBlock {
        TextBlock {
            bbox: b,
            score: 0.9,
        }
    }

    #[test]
    fn parity_matches_blocks_as_a_set() {
        let a = block([10.2, 20.5, 30.7, 40.0]);
        let b = block([100.0, 5.0, 150.5, 60.25]);
        // the same blocks: nothing worse, nothing moved, the order kept
        assert_eq!(
            match_blocks(&[a, b], &[a, b]),
            Some(Parity {
                worst: 0.0,
                edges_moved: 0,
                reordered: false
            })
        );
        // the same set in another order still pairs up
        assert_eq!(
            match_blocks(&[a, b], &[b, a]),
            Some(Parity {
                worst: 0.0,
                edges_moved: 0,
                reordered: true
            })
        );
        // a corner 0.005 px away whose crop edge stays put, then one that crosses a pixel
        let near = block([10.205, 20.5, 30.7, 40.0]);
        let p = match_blocks(&[a], &[near]).unwrap();
        assert!(
            (p.worst - 0.005).abs() < 1e-4 && p.edges_moved == 0,
            "{p:?}"
        );
        let crossed = block([10.2, 20.5, 30.7, 40.004]);
        let p = match_blocks(&[a], &[crossed]).unwrap();
        assert_eq!(p.edges_moved, 1, "{p:?}");
        assert!(!p.passes());
        // another count is another set
        assert_eq!(match_blocks(&[a, b], &[a]), None);
        assert!(match_blocks(&[], &[]).unwrap().passes());
        // a block more than 0.01 px off fails the gate (it still pairs, to report how far)
        let far = block([10.2, 20.5, 30.75, 40.0]);
        assert!(!match_blocks(&[a], &[far]).unwrap().passes());
    }

    #[test]
    fn gate_pages_come_from_a_directory_or_a_list() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("vol")).unwrap();
        for f in ["b.png", "vol/a.jpg", "vol/c.WEBP", "notes.txt"] {
            std::fs::write(d.path().join(f), b"x").unwrap();
        }
        let got = gate_pages(d.path());
        let names: Vec<_> = got
            .iter()
            .map(|p| {
                p.strip_prefix(d.path())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(names, ["b.png", "vol/a.jpg", "vol/c.WEBP"]);
        // a list: one path a line, relative to the list's directory, `#` comments skipped
        let list = d.path().join("pages.txt");
        let abs = d.path().join("b.png");
        std::fs::write(
            &list,
            format!("# gate pages\nvol/a.jpg\n\n{}\n", abs.display()),
        )
        .unwrap();
        assert_eq!(gate_pages(&list), [d.path().join("vol/a.jpg"), abs]);
    }

    #[test]
    #[should_panic(expected = "listed pages do not exist")]
    fn a_gate_list_naming_a_missing_page_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let list = d.path().join("pages.txt");
        std::fs::write(&list, "gone.png\n").unwrap();
        gate_pages(&list);
    }

    /// The GPU route through this module against the CPU route on real pages (plan 04
    /// Task 10's gate, the Rust side of `torch_export verify ... animetext-blocks`): every
    /// page of `MOKURO_ANIMETEXT_PAGES` (a directory of images, searched recursively, or a
    /// list file of them, one path a line relative to the list) detected by AnimeText on
    /// `gpu:0` through `bt_detect_blocks` and by [`bunko_ocr::animetext::AnimeText`] (ONNX
    /// Runtime, CPU) must give the same block set: the same count, every corner within
    /// [`CORNER`] page px and no crop edge moved (`floor` x0/y0, `ceil` x1/y1, as the page
    /// is cropped). A page neither route can decode (AVIF) is skipped and listed. The
    /// variables are this test's own (the server reads none of them): `MOKURO_TORCH_PACK`
    /// (a pack with `bt_detect_blocks`), `MOKURO_ANIMETEXT_TORCH_PACKAGE` (the dir holding
    /// `detector.pt2`) and `MOKURO_ANIMETEXT_ONNX` (the mirrored `animetext_yolo12x.onnx`).
    /// Run it alone (one pack per process), under the GPU lock:
    /// `cargo test -p bunko-engines --features gpl-models --release --lib -- --ignored --exact torch::detector::tests::animetext_torch_parity`.
    #[test]
    #[ignore = "needs a backend pack with bt_detect_blocks, an AnimeText package, its ONNX file and pages (env vars)"]
    fn animetext_torch_parity() {
        use bunko_ocr::runtime::{ExecutionTarget, RuntimeOptions};
        use std::time::{Duration, Instant};

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

        let files = gate_pages(&pages);
        let (mut blocks, mut worst, mut reordered) = (0usize, 0.0f32, 0usize);
        let (mut differ, mut skipped) = (Vec::new(), Vec::new());
        let (mut gpu_time, mut cpu_time) = (Duration::ZERO, Duration::ZERO);
        for f in &files {
            let page = match bunko_ocr::image::decode_bgr(&std::fs::read(f).unwrap()) {
                Ok(p) => p,
                Err(e) => {
                    skipped.push(format!("{}: {e}", f.display()));
                    continue;
                }
            };
            let t = Instant::now();
            let (g, gi) = gpu
                .detect(&page)
                .unwrap_or_else(|e| panic!("{}: {e}", f.display()));
            gpu_time += t.elapsed();
            let t = Instant::now();
            let (c, ci) = cpu.detect(&page).unwrap();
            cpu_time += t.elapsed();
            // what ran is the same (a pass's line count is the block count, compared below)
            let ran = |i: &DetectorInfo| {
                let p: Vec<_> = i
                    .passes
                    .iter()
                    .map(|p| (p.scale, p.tiles, p.why.clone()))
                    .collect();
                (i.side, i.tile.clone(), p)
            };
            assert_eq!(ran(&gi), ran(&ci), "{}", f.display());
            blocks += c.len();
            match match_blocks(&c, &g) {
                Some(p) if p.passes() => {
                    worst = worst.max(p.worst);
                    reordered += usize::from(p.reordered);
                }
                p => differ.push(format!("{} ({p:?}): cpu {c:?} gpu {g:?}", f.display())),
            }
        }
        let n = files.len() - skipped.len();
        assert!(n > 0, "no decodable pages in {}", pages.display());
        eprintln!(
            "AnimeText on gpu:0: {} of {n} pages identical, worst corner {worst:.4} px, \
             {reordered} in another order; {blocks} blocks on the CPU route; \
             {:.1} ms/page on the GPU route, {:.1} on the CPU (one page a call, the host's \
             letterbox included); {} pages not decodable{}",
            n - differ.len(),
            gpu_time.as_secs_f64() * 1e3 / n as f64,
            cpu_time.as_secs_f64() * 1e3 / n as f64,
            skipped.len(),
            if skipped.is_empty() {
                String::new()
            } else {
                format!(": {skipped:#?}")
            },
        );
        assert!(
            differ.is_empty(),
            "{} pages differ: {differ:#?}",
            differ.len()
        );
    }
}
