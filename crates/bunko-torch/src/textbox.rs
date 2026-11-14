//! The text-box recognizers on libtorch, compiled without weights and bound at load to
//! the authors' `model.safetensors` (`bunko.io = "textbox-1"`).
//!
//! - manga-ocr-nar-small is one AOTInductor graph (role `model`): `P [b,324,588] f32,
//!   V [b,324] f32, R, C [b,324] i32, H, W [b] i32 -> ids [b,161] i64`.
//! - manga-ocr-charspot-small is two (roles `dense` and `tail`): `dense(img [B,3,H,W] f32
//!   0..255, hw [B,2] i64) -> heat [B,1,H/8,W/8], box [B,4,..], ruby [B,1,..], emb
//!   [B,144,..]` and `tail(emb [R,144,H8,W8], boxes [R,N,4] f32, geo [R,N,4] f32, mask
//!   [R,N] bool) -> ids [R,N] i64, pair [R,N,N], start [R,N], end [R,N]`. Between them the
//!   host runs the authors' decode, and after the tail their reading-order chain.
//!
//! The host preprocessing, the decode and the ids-to-text rules are bunko-vlm's
//! (`bunko_vlm::textbox`), the ONNX route's, so both routes build bit-identical inputs.

use std::path::Path;
use std::sync::Mutex;

use bunko_vlm::Rgb;
use bunko_vlm::textbox::{
    CHARSPOT_STRIDE, CharspotChars, Detected, NAR_BUDGET, NAR_PATCH, NAR_SLOTS, NarInput, batches,
    canvas_batches, charspot_batch_cap, charspot_batch_maps, charspot_canvas, charspot_canvas_hw,
    charspot_classes, charspot_dense_input, charspot_detect, charspot_part_chars,
    charspot_tail_input, charspot_tail_plan, empty, load_characters, nar_batch, nar_input,
    nar_text,
};
use tch::{Kind, Tensor};

use crate::TorchError;
use crate::abi::{CharEntry, CharsRead};
use crate::aoti::{Aoti, Placement, dev_f32, dev_i32, dev_i64, host_f32, host_i64};
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
        let package_max = package_batch("nar-small", set.metadata(MAX_BATCH_KEY).as_deref())
            .map_err(TorchError::Load)?;
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

/// charspot-small's graph roles: the detection heads and embeddings, then the classes
/// and reading-order scores.
pub const CHARSPOT_DENSE: &str = "dense";
pub const CHARSPOT_TAIL: &str = "tail";

/// manga-ocr-charspot-small on libtorch: `dense` per canvas batch, the authors' decode
/// on the host, `tail` per part of the batch's crops, the chain on the host.
pub struct TorchCharspot {
    set: GraphSet,
    /// The class table (`charspot_classes`): the vocabulary padded, then the `!?` glyphs.
    classes: Vec<String>,
    dev: tch::Device,
    /// Crops per dense call: `charspot_batch_cap(LoadOptions.max_batch)`, within the
    /// package's own `bunko.max_batch`.
    batch: usize,
    /// Host threads for the crops' canvases.
    helpers: usize,
    gpu_lock: Mutex<()>,
}

impl TorchCharspot {
    /// Wraps a loaded and bound graph set (`dense`, `tail`), with the vocabulary's class
    /// table. `max_batch` caps the crops per dense call (a small GPU's cap).
    pub fn load(
        set: GraphSet,
        vocabulary: &Path,
        at: Placement,
        max_batch: Option<u32>,
    ) -> Result<Self, TorchError> {
        for role in [CHARSPOT_DENSE, CHARSPOT_TAIL] {
            if set.graph(role).is_none() {
                return Err(TorchError::Load(format!(
                    "charspot-small: the package set has no `{role}` graph"
                )));
            }
        }
        let package_max = package_batch("charspot-small", set.metadata(MAX_BATCH_KEY).as_deref())
            .map_err(TorchError::Load)?;
        let classes = charspot_classes(load_characters(vocabulary)?)
            .map_err(|e| TorchError::Load(format!("vocabulary {}: {e}", vocabulary.display())))?;
        Ok(Self {
            set,
            classes,
            dev: at.dev,
            batch: charspot_batch_cap(max_batch).min(package_max),
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

    /// Crops per dense call.
    pub fn batch(&self) -> usize {
        self.batch
    }

    /// Weight tensors bound from the weights file.
    pub fn bound(&self) -> usize {
        self.set.weights.as_ref().map_or(0, |w| w.len())
    }

    fn graph(&self, role: &str) -> Result<&Aoti, TorchError> {
        self.set
            .graph(role)
            .ok_or_else(|| TorchError::Run(format!("charspot-small has no `{role}` graph")))
    }

    /// Each crop's characters in reading order, boxes in crop pixels, and the text they
    /// spell, in crop order. An empty crop, or one where nothing is detected, reads
    /// `""` with no characters. A crop with more characters than the tail graph takes
    /// ([`bunko_vlm::textbox::CHARSPOT_MAX_CHARS`]; a prepared crop never has) answers
    /// `chars: None`: this pack did not read it, and the caller reads it on its CPU
    /// route. The crops are batched on canvases by the authors' rule; each canvas is
    /// drawn on host threads while the device runs the batches before it.
    pub fn read_chars(&self, crops: &[&Rgb]) -> Result<Vec<CharsRead>, TorchError> {
        let mut out: Vec<CharsRead> = crops.iter().map(|_| nothing_read()).collect();
        let kept: Vec<usize> = (0..crops.len()).filter(|&i| !empty(crops[i])).collect();
        let shapes: Vec<(usize, usize)> = kept
            .iter()
            .map(|&i| charspot_canvas_hw(crops[i].height, crops[i].width))
            .collect();
        let plan = canvas_batches(&shapes, self.batch);
        // The crops in batch order, and where each batch ends in that order.
        let order: Vec<usize> = plan
            .iter()
            .flat_map(|(idx, _)| idx.iter().map(|&k| kept[k]))
            .collect();
        let mut bounds = plan.iter().scan(0usize, |end, (idx, hw)| {
            *end += idx.len();
            Some((*end, *hw))
        });
        let mut current = bounds.next();
        let mut pending: Vec<(Rgb, (f32, f32))> = Vec::with_capacity(self.batch);
        let mut done = 0usize;
        crate::par::map_ordered(
            &order,
            self.helpers,
            |&i| charspot_canvas(crops[i]),
            |k, canvas| {
                pending.push(canvas);
                let Some((end, hw)) = current.filter(|(e, _)| *e == k + 1) else {
                    return Ok(());
                };
                if crate::runtime::shutting_down() {
                    return Err(TorchError::Run("the process is exiting".into()));
                }
                let reads = self.run(&pending, hw)?;
                for (slot, read) in order[done..end].iter().zip(reads) {
                    out[*slot] = read;
                }
                pending.clear();
                done = end;
                current = bounds.next();
                Ok::<(), TorchError>(())
            },
        )?;
        Ok(out)
    }

    /// One canvas batch: `dense`, the decode per crop, `tail` per part, the chain per
    /// crop. One [`CharsRead`] per canvas, in order.
    fn run(
        &self,
        items: &[(Rgb, (f32, f32))],
        hw: (usize, usize),
    ) -> Result<Vec<CharsRead>, TorchError> {
        let canvases: Vec<&Rgb> = items.iter().map(|(c, _)| c).collect();
        let d = charspot_dense_input(&canvases, hw);
        let (b, h, w) = (d.len as i64, d.height as i64, d.width as i64);
        let (h8, w8) = (d.height / CHARSPOT_STRIDE, d.width / CHARSPOT_STRIDE);
        let (dense, tail) = (self.graph(CHARSPOT_DENSE)?, self.graph(CHARSPOT_TAIL)?);
        let dev = self.dev;
        let _g = self.gpu_lock.lock().unwrap_or_else(|p| p.into_inner());
        let _ng = tch::no_grad_guard();
        let out = dense.run(&[
            &dev_f32(&d.img, &[b, 3, h, w], dev),
            &dev_i64(&d.hw, &[b, 2], dev),
        ])?;
        let [heat, boxes, ruby, emb] = &out[..] else {
            return Err(revision("dense", format!("{} outputs, not 4", out.len())));
        };
        let (hh, ww) = (h8 as i64, w8 as i64);
        shape("dense heat", heat, &[b, 1, hh, ww])?;
        shape("dense box", boxes, &[b, 4, hh, ww])?;
        shape("dense ruby", ruby, &[b, 1, hh, ww])?;
        let es = emb.size();
        if es.len() != 4 || es[0] != b || es[2..] != [hh, ww] {
            return Err(revision("dense", format!("emb of shape {es:?}")));
        }
        let maps = host_f32(&Tensor::cat(&[heat, boxes, ruby], 1))?;
        let dets: Vec<Option<Detected>> = canvases
            .iter()
            .enumerate()
            .map(|(r, c)| {
                charspot_detect(
                    &charspot_batch_maps(&maps, r, h8, w8),
                    c.height / CHARSPOT_STRIDE,
                    c.width / CHARSPOT_STRIDE,
                )
            })
            .collect();
        let counts: Vec<usize> = dets
            .iter()
            .map(|d| d.as_ref().map_or(0, |d| d.boxes.len()))
            .collect();
        let mut reads: Vec<CharsRead> = items.iter().map(|_| nothing_read()).collect();
        let (parts, over) = charspot_tail_plan(&counts);
        for r in over {
            reads[r].chars = None;
        }
        for part in parts {
            let rows: Vec<&Detected> = part.iter().filter_map(|&r| dets[r].as_ref()).collect();
            if rows.len() != part.len() {
                return Err(TorchError::Run(
                    "charspot-small: a tail part holds a crop with no characters".into(),
                ));
            }
            let t = charspot_tail_input(&rows);
            let (r_, n) = (t.rows as i64, t.width as i64);
            let idx: Vec<i64> = part.iter().map(|&r| r as i64).collect();
            let o = tail.run(&[
                &emb.index_select(0, &dev_i64(&idx, &[r_], dev)),
                &dev_f32(&t.boxes, &[r_, n, 4], dev),
                &dev_f32(&t.geo, &[r_, n, 4], dev),
                &Tensor::from_slice(&t.mask).view([r_, n]).to_device(dev),
            ])?;
            let [ids, pair, start, end] = &o[..] else {
                return Err(revision("tail", format!("{} outputs, not 4", o.len())));
            };
            shape("tail ids", ids, &[r_, n])?;
            shape("tail pair", pair, &[r_, n, n])?;
            shape("tail start", start, &[r_, n])?;
            shape("tail end", end, &[r_, n])?;
            let ids = host_i64(&ids.to_kind(Kind::Int64).contiguous().reshape([-1]))?;
            let (pair, start, end) = (host_f32(pair)?, host_f32(start)?, host_f32(end)?);
            let backs: Vec<(f32, f32)> = part.iter().map(|&r| items[r].1).collect();
            let read = charspot_part_chars(
                &rows,
                t.width,
                &ids,
                &pair,
                &start,
                &end,
                &self.classes,
                &backs,
            );
            for (&r, chars) in part.iter().zip(read) {
                reads[r] = chars_read(chars);
            }
        }
        Ok(reads)
    }
}

/// A crop that reads nothing: `""`, no characters.
fn nothing_read() -> CharsRead {
    CharsRead {
        text: String::new(),
        chars: Some(Vec::new()),
    }
}

/// A crop's characters as the ABI answers them.
fn chars_read((text, chars): CharspotChars) -> CharsRead {
    CharsRead {
        text,
        chars: Some(
            chars
                .into_iter()
                .map(|(c, b, s)| CharEntry(c, b[0], b[1], b[2], b[3], s))
                .collect(),
        ),
    }
}

/// An error for a graph whose outputs are not this revision's.
fn revision(graph: &str, what: String) -> TorchError {
    TorchError::Run(format!(
        "charspot-small's {graph} graph returned {what}: a graph of another revision"
    ))
}

fn shape(what: &str, t: &Tensor, want: &[i64]) -> Result<(), TorchError> {
    let size = t.size();
    if size != want {
        return Err(TorchError::Run(format!(
            "charspot-small: {what} of shape {size:?}, not {want:?}: a graph of another revision"
        )));
    }
    Ok(())
}

/// The package's largest batch from its `bunko.max_batch`.
fn package_batch(engine: &str, meta: Option<&str>) -> Result<usize, String> {
    let v = meta.ok_or_else(|| format!("{engine}: the package does not say {MAX_BATCH_KEY}"))?;
    match v.trim().parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("{engine}: bad {MAX_BATCH_KEY} {v:?}")),
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
        assert_eq!(package_batch("nar-small", Some("64")), Ok(64));
        assert_eq!(package_batch("nar-small", Some(" 8\n")), Ok(8));
        let e = package_batch("charspot-small", None).unwrap_err();
        assert!(
            e.starts_with("charspot-small:") && e.contains(MAX_BATCH_KEY),
            "{e}"
        );
        assert!(package_batch("nar-small", Some("0")).is_err());
        assert!(package_batch("nar-small", Some("x")).is_err());
        assert_eq!(capped_batch(64, None), 64);
        assert_eq!(capped_batch(64, Some(8)), 8);
        assert_eq!(capped_batch(64, Some(100)), 64);
        assert_eq!(capped_batch(64, Some(0)), 1);
    }
}
