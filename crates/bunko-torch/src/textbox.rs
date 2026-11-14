//! The text-box recognizers on libtorch. manga-ocr-nar-small is one AOTInductor graph
//! (role `model`, `bunko.io = "textbox-1"`) compiled without weights and bound at load to
//! the authors' `model.safetensors`: `P [b,324,588] f32, V [b,324] f32, R, C [b,324] i32,
//! H, W [b] i32 -> ids [b,161] i64`. The host preprocessing and the ids-to-text rule are
//! bunko-vlm's (`bunko_vlm::textbox`), the ONNX route's, so both routes build
//! bit-identical inputs and read the same text.

use std::path::Path;
use std::sync::Mutex;

use bunko_vlm::Rgb;
use bunko_vlm::textbox::{
    NAR_BUDGET, NAR_PATCH, NAR_SLOTS, NarInput, batches, empty, load_characters, nar_batch,
    nar_input, nar_text,
};
use tch::Kind;

use crate::TorchError;
use crate::abi::CharsRead;
use crate::aoti::{Aoti, Placement, dev_f32, dev_i32, host_i64};
use crate::package::GraphSet;

/// nar-small's one graph role.
pub const NAR_ROLE: &str = "model";

/// The package metadata key of the largest batch a graph was compiled for.
const MAX_BATCH_KEY: &str = "bunko.max_batch";

/// Host threads at most for the crops' preprocessing when the handle names none.
const DEFAULT_HELPERS: usize = 8;

/// manga-ocr-nar-small on libtorch.
pub struct TorchNar {
    set: GraphSet,
    characters: Vec<String>,
    dev: tch::Device,
    /// Crops per graph run: `min(bunko.max_batch, LoadOptions.max_batch)`.
    batch: usize,
    /// Host threads for the crops' preprocessing.
    helpers: usize,
    gpu_lock: Mutex<()>,
}

impl TorchNar {
    /// Wraps a loaded and bound graph set (`model`), with the vocabulary's characters
    /// (the end id is their count). `max_batch` caps the package's own largest batch.
    pub fn load(
        set: GraphSet,
        vocabulary: &Path,
        at: Placement,
        max_batch: Option<u32>,
    ) -> Result<Self, TorchError> {
        if set.graph(NAR_ROLE).is_none() {
            return Err(TorchError::Load(format!(
                "nar-small: the package set has no `{NAR_ROLE}` graph"
            )));
        }
        let package_max =
            package_batch(set.metadata(MAX_BATCH_KEY).as_deref()).map_err(TorchError::Load)?;
        let characters = load_characters(vocabulary)?;
        Ok(Self {
            set,
            characters,
            dev: at.dev,
            batch: capped_batch(package_max, max_batch),
            helpers: DEFAULT_HELPERS,
            gpu_lock: Mutex::new(()),
        })
    }

    /// The handle's preprocessing threads (`LoadOptions.threads`; 0 keeps the default).
    pub fn with_threads(mut self, threads: u32) -> Self {
        if threads > 0 {
            self.helpers = threads as usize;
        }
        self
    }

    /// Crops per graph run.
    pub fn batch(&self) -> usize {
        self.batch
    }

    /// Weight tensors bound from the weights file.
    pub fn bound(&self) -> usize {
        self.set.weights.as_ref().map_or(0, |w| w.len())
    }

    fn model(&self) -> Result<&Aoti, TorchError> {
        self.set
            .graph(NAR_ROLE)
            .ok_or_else(|| TorchError::Run(format!("nar-small has no `{NAR_ROLE}` graph")))
    }

    /// One text per crop, in crop order (`chars` is `None`: nar-small gives no boxes).
    /// An empty crop reads `""` without a model call. The crops are prepared on host
    /// threads while the device runs the batches before them, and cut into batches of
    /// at most [`TorchNar::batch`].
    pub fn read_chars(&self, crops: &[&Rgb]) -> Result<Vec<CharsRead>, TorchError> {
        let mut out: Vec<CharsRead> = crops
            .iter()
            .map(|_| CharsRead {
                text: String::new(),
                chars: None,
            })
            .collect();
        let kept: Vec<usize> = (0..crops.len()).filter(|&i| !empty(crops[i])).collect();
        let chunks = batches(kept.len(), self.batch);
        let mut chunk = chunks.iter();
        let mut current = chunk.next();
        let mut pending: Vec<NarInput> = Vec::with_capacity(self.batch.min(kept.len()));
        crate::par::map_ordered(
            &kept,
            self.helpers,
            |&i| nar_input(crops[i]),
            |k, input| {
                pending.push(input);
                let Some(range) = current.filter(|r| r.end == k + 1) else {
                    return Ok(());
                };
                if crate::runtime::shutting_down() {
                    return Err(TorchError::Run("the process is exiting".into()));
                }
                let refs: Vec<&NarInput> = pending.iter().collect();
                let ids = self.run(&refs)?;
                for (slot, row) in kept[range.clone()].iter().zip(ids.chunks(NAR_SLOTS)) {
                    out[*slot].text = nar_text(row, &self.characters);
                }
                pending.clear();
                current = chunk.next();
                Ok::<(), TorchError>(())
            },
        )?;
        Ok(out)
    }

    /// One graph run: the ids of `inputs`, row after row (`NAR_SLOTS` each).
    fn run(&self, inputs: &[&NarInput]) -> Result<Vec<i64>, TorchError> {
        let nb = nar_batch(inputs);
        let (b, p) = (nb.len as i64, NAR_BUDGET as i64);
        let feat = (3 * NAR_PATCH * NAR_PATCH) as i64;
        let model = self.model()?;
        let dev = self.dev;
        let _g = self.gpu_lock.lock().unwrap_or_else(|p| p.into_inner());
        let _ng = tch::no_grad_guard();
        let out = model.run(&[
            &dev_f32(&nb.patches, &[b, p, feat], dev),
            &dev_f32(&nb.valid, &[b, p], dev),
            &dev_i32(&nb.rows, &[b, p], dev),
            &dev_i32(&nb.cols, &[b, p], dev),
            &dev_i32(&nb.heights, &[b], dev),
            &dev_i32(&nb.widths, &[b], dev),
        ])?;
        let ids = out
            .first()
            .ok_or_else(|| TorchError::Run("nar-small returned nothing".into()))?;
        let size = ids.size();
        if size != [b, NAR_SLOTS as i64] {
            return Err(TorchError::Run(format!(
                "nar-small returned ids of shape {size:?}, not [{b}, {NAR_SLOTS}]: a graph of another revision"
            )));
        }
        host_i64(&ids.to_kind(Kind::Int64).reshape([-1]))
    }
}

/// The package's largest batch from its `bunko.max_batch`.
fn package_batch(meta: Option<&str>) -> Result<usize, String> {
    let v = meta.ok_or_else(|| format!("nar-small: the package does not say {MAX_BATCH_KEY}"))?;
    match v.trim().parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("nar-small: bad {MAX_BATCH_KEY} {v:?}")),
    }
}

/// The batch a handle runs: the package's, capped by the host's `max_batch` (at least 1).
fn capped_batch(package_max: usize, max_batch: Option<u32>) -> usize {
    max_batch.map_or(package_max, |m| (m as usize).clamp(1, package_max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_batch_is_the_package_max_capped_by_the_host() {
        assert_eq!(package_batch(Some("64")), Ok(64));
        assert_eq!(package_batch(Some(" 8\n")), Ok(8));
        assert!(package_batch(None).unwrap_err().contains(MAX_BATCH_KEY));
        assert!(package_batch(Some("0")).is_err());
        assert!(package_batch(Some("x")).is_err());
        assert_eq!(capped_batch(64, None), 64);
        assert_eq!(capped_batch(64, Some(8)), 8);
        assert_eq!(capped_batch(64, Some(100)), 64);
        assert_eq!(capped_batch(64, Some(0)), 1);
    }
}
