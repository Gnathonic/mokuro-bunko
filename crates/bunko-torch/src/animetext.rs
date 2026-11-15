//! The AnimeText YOLO12-x block detector on libtorch: one AOTInductor graph (role
//! `detector`, `bunko.io = "detect-1"`) compiled without weights and bound at load to the
//! initializers of the ONNX file the CPU route runs (`bunko.weights_format = "onnx"`):
//! `images [b,3,h,w] f32 -> candidates [b,5,n] f32` (centre x, centre y, width, height,
//! score in canvas pixels; no NMS in the graph). The host letterboxes the page onto an
//! RGB8 canvas and builds the input with bunko-vlm's [`canvas_chw`], the ONNX route's
//! function, so both routes feed the network the same floats; the blocks are made from
//! the candidates on the host, by the ONNX route's post-processing.

use std::sync::Mutex;

use bunko_vlm::Rgb;
use bunko_vlm::animetext::{MAX_SIDE, STRIDE, canvas_chw, canvas_ok};
use tch::Kind;

use crate::TorchError;
use crate::abi::DetectOut;
use crate::aoti::{Aoti, Placement, dev_f32};
use crate::package::GraphSet;

/// The detector's one graph role.
pub const DETECTOR_ROLE: &str = "detector";

/// The package metadata key of the largest batch a graph was compiled for.
const MAX_BATCH_KEY: &str = "bunko.max_batch";

/// Host threads at most for the canvases' conversion when the handle names none.
const DEFAULT_HELPERS: usize = 8;

/// AnimeText on libtorch.
pub struct TorchAnimeText {
    set: GraphSet,
    dev: tch::Device,
    /// Canvases per graph run: the package's `bunko.max_batch`.
    batch: usize,
    /// Host threads for the canvases' conversion.
    helpers: usize,
    gpu_lock: Mutex<()>,
}

impl TorchAnimeText {
    /// Wraps a loaded and bound graph set (`detector`).
    pub fn load(set: GraphSet, at: Placement) -> Result<Self, TorchError> {
        if set.graph(DETECTOR_ROLE).is_none() {
            return Err(TorchError::Load(format!(
                "animetext-blocks: the package set has no `{DETECTOR_ROLE}` graph"
            )));
        }
        let batch =
            package_batch(set.metadata(MAX_BATCH_KEY).as_deref()).map_err(TorchError::Load)?;
        Ok(Self {
            set,
            dev: at.dev,
            batch,
            helpers: DEFAULT_HELPERS,
            gpu_lock: Mutex::new(()),
        })
    }

    /// The handle's conversion threads (`LoadOptions.threads`; 0 keeps the default).
    pub fn with_threads(mut self, threads: u32) -> Self {
        if threads > 0 {
            self.helpers = threads as usize;
        }
        self
    }

    /// Weight tensors bound from the ONNX file.
    pub fn bound(&self) -> usize {
        self.set.weights.as_ref().map_or(0, |w| w.len())
    }

    fn model(&self) -> Result<&Aoti, TorchError> {
        self.set.graph(DETECTOR_ROLE).ok_or_else(|| {
            TorchError::Run(format!("animetext-blocks has no `{DETECTOR_ROLE}` graph"))
        })
    }

    /// One [`DetectOut`] per canvas, in canvas order: the candidates scored `min_score` or
    /// more, in the output's index order. Canvases of one size run together, at most
    /// the package's batch at a time; each is converted on host threads while the device
    /// runs the batches before it.
    pub fn detect(&self, canvases: &[&Rgb], min_score: f32) -> Result<Vec<DetectOut>, TorchError> {
        if min_score.is_nan() {
            return Err(TorchError::Arg("min_score is NaN".into()));
        }
        for c in canvases {
            check_canvas(c.width, c.height)?;
        }
        let sizes: Vec<(usize, usize)> = canvases.iter().map(|c| (c.width, c.height)).collect();
        let chunks = size_batches(&sizes, self.batch);
        let order: Vec<usize> = chunks.iter().flatten().copied().collect();
        let mut out: Vec<Option<DetectOut>> = vec![None; canvases.len()];
        let mut ends = chunks.iter().scan(0, |at, c| {
            *at += c.len();
            Some((*at, c))
        });
        let mut current = ends.next();
        let mut pending: Vec<f32> = Vec::new();
        crate::par::map_ordered(
            &order,
            self.helpers,
            |&i| canvas_chw(canvases[i]),
            |k, x| {
                pending.extend_from_slice(&x);
                let Some((_, chunk)) = current.filter(|(end, _)| *end == k + 1) else {
                    return Ok(());
                };
                if crate::runtime::shutting_down() {
                    return Err(TorchError::Run("the process is exiting".into()));
                }
                let (w, h) = sizes[chunk[0]];
                let (n, data) = self.run(&pending, chunk.len(), w, h)?;
                for (j, &slot) in chunk.iter().enumerate() {
                    let one = &data[j * 5 * n..(j + 1) * 5 * n];
                    out[slot] = Some(candidates(one, n, min_score).map_err(TorchError::Run)?);
                }
                pending.clear();
                current = ends.next();
                Ok::<(), TorchError>(())
            },
        )?;
        out.into_iter()
            .enumerate()
            .map(|(i, o)| o.ok_or_else(|| TorchError::Run(format!("canvas {i} was not run"))))
            .collect()
    }

    /// One graph run over `b` canvases of `w` x `h` (`x` holds their `[3,h,w]` planes one
    /// after the other): `(n, [b,5,n] row-major)`.
    fn run(
        &self,
        x: &[f32],
        b: usize,
        w: usize,
        h: usize,
    ) -> Result<(usize, Vec<f32>), TorchError> {
        let model = self.model()?;
        let dev = self.dev;
        let shape = [b as i64, 3, h as i64, w as i64];
        let _g = self.gpu_lock.lock().unwrap_or_else(|p| p.into_inner());
        let _ng = tch::no_grad_guard();
        let outs = model.run(&[&dev_f32(x, &shape, dev)])?;
        let t = outs
            .first()
            .ok_or_else(|| TorchError::Run("animetext-blocks returned nothing".into()))?;
        let size = t.size();
        let n = match size[..] {
            [ob, 5, n] if ob == b as i64 && n >= 0 => n as usize,
            _ => {
                return Err(TorchError::Run(format!(
                    "animetext-blocks returned candidates of shape {size:?}, not [{b}, 5, n]: a graph of another revision"
                )));
            }
        };
        let host = t
            .to_device(tch::Device::Cpu)
            .to_kind(Kind::Float)
            .reshape([-1]);
        let data = Vec::<f32>::try_from(&host).map_err(|e| TorchError::Run(e.to_string()))?;
        Ok((n, data))
    }
}

/// A canvas the graph takes: both sides positive multiples of 32, at most 640.
pub fn check_canvas(width: usize, height: usize) -> Result<(), TorchError> {
    if canvas_ok(width, height) {
        Ok(())
    } else {
        Err(TorchError::Arg(format!(
            "canvas {width}x{height}: sides must be multiples of {STRIDE}, at most {MAX_SIDE}"
        )))
    }
}

/// The package's largest batch from its `bunko.max_batch`.
fn package_batch(meta: Option<&str>) -> Result<usize, String> {
    let v =
        meta.ok_or_else(|| format!("animetext-blocks: the package does not say {MAX_BATCH_KEY}"))?;
    match v.trim().parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("animetext-blocks: bad {MAX_BATCH_KEY} {v:?}")),
    }
}

/// The canvases (by index) as graph runs: canvases of one size together, in the order
/// each size first appears and in canvas order within it, at most `batch` a run.
pub fn size_batches(sizes: &[(usize, usize)], batch: usize) -> Vec<Vec<usize>> {
    let batch = batch.max(1);
    let mut groups: Vec<((usize, usize), Vec<usize>)> = Vec::new();
    for (i, s) in sizes.iter().enumerate() {
        match groups.iter_mut().find(|(g, _)| g == s) {
            Some((_, v)) => v.push(i),
            None => groups.push((*s, vec![i])),
        }
    }
    groups
        .into_iter()
        .flat_map(|(_, v)| v.chunks(batch).map(<[usize]>::to_vec).collect::<Vec<_>>())
        .collect()
}

/// One canvas's `[5, n]` output (row-major) as its [`DetectOut`]: the candidates scored
/// `min_score` or more, in index order. A NaN score is no score below `min_score`, so the
/// ONNX route's post-processing would keep it; JSON cannot carry it, so such a candidate
/// (or a kept one with an infinite or NaN coordinate) is an error, not a silent drop.
pub fn candidates(out: &[f32], n: usize, min_score: f32) -> Result<DetectOut, String> {
    if out.len() != 5 * n {
        return Err(format!("{} values for 5 x {n} candidates", out.len()));
    }
    let mut cand = Vec::new();
    for i in 0..n {
        let score = out[4 * n + i];
        if score >= min_score || score.is_nan() {
            let c = [out[i], out[n + i], out[2 * n + i], out[3 * n + i], score];
            if c.iter().any(|v| !v.is_finite()) {
                return Err(format!(
                    "animetext-blocks returned candidate {i} as {c:?}: not a finite box and score"
                ));
            }
            cand.push(c);
        }
    }
    Ok(DetectOut { n, cand })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvases_of_one_size_run_together_in_order() {
        let s = [(640, 480), (320, 640), (640, 480), (640, 480), (320, 640)];
        assert_eq!(size_batches(&s, 8), vec![vec![0, 2, 3], vec![1, 4]]);
        assert_eq!(size_batches(&s, 2), vec![vec![0, 2], vec![3], vec![1, 4]]);
        assert_eq!(
            size_batches(&s, 0),
            vec![vec![0], vec![2], vec![3], vec![1], vec![4]]
        );
        assert!(size_batches(&[], 8).is_empty());
    }

    #[test]
    fn candidates_keep_the_score_threshold_and_the_order() {
        // n = 4: cx, cy, w, h, score rows
        let n = 4;
        let rows = [
            [1., 2., 3., 4., 0.9],
            [5., 6., 7., 8., 0.29],
            [9., 10., 11., 12., 0.3],
            [13., 14., 15., 16., 0.31],
        ];
        let mut out = vec![0.0f32; 5 * n];
        for (i, r) in rows.iter().enumerate() {
            for c in 0..5 {
                out[c * n + i] = r[c];
            }
        }
        let d = candidates(&out, n, 0.3).unwrap();
        assert_eq!(d.n, 4);
        assert_eq!(d.cand, vec![rows[0], rows[2], rows[3]]);
        assert_eq!(
            candidates(&[], 0, 0.3).unwrap(),
            DetectOut { n: 0, cand: vec![] }
        );
        assert!(candidates(&out[1..], n, 0.3).is_err());
        // A NaN score is kept by the CPU route's post-processing: not dropped silently.
        out[4 * n + 1] = f32::NAN;
        assert!(
            candidates(&out, n, 0.3)
                .unwrap_err()
                .contains("candidate 1")
        );
        // A candidate below the score is never looked at.
        out[4 * n + 1] = 0.1;
        out[1] = f32::INFINITY;
        assert_eq!(candidates(&out, n, 0.3).unwrap().cand.len(), 3);
    }

    #[test]
    fn canvas_sides_and_batches_are_checked() {
        assert!(check_canvas(640, 32).is_ok());
        let e = check_canvas(33, 64).unwrap_err();
        assert!(matches!(e, TorchError::Arg(_)));
        assert_eq!(
            e.to_string(),
            "canvas 33x64: sides must be multiples of 32, at most 640"
        );
        assert!(check_canvas(0, 0).is_err());
        assert!(check_canvas(672, 32).is_err());
        assert_eq!(package_batch(Some("8")), Ok(8));
        assert!(package_batch(None).unwrap_err().contains(MAX_BATCH_KEY));
        assert!(package_batch(Some("0")).is_err());
    }
}
