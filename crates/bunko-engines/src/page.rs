//! What each stage does to a page (spec ocr-recognizers §2.3-§2.4, ocr-ppocr-layout
//! §5, §7): detect + CTC read with the page-level passes; the engine's first read,
//! reconcile, second read and verdicts; the layout into a mokuro page. On the
//! reconciled road the detect stage also plans the engine road and cuts the
//! first-read crops, so decoded pages do not wait in the engine's queue.
//!
//! A recognizer that reads whole text blocks (`Recognizer::block_reader`) gets one
//! crop per block of the first layout instead of one per line; it answers line by
//! line, so everything after its read (reconcile, verdicts, layout) is the same.
//!
//! With a block detector there are no detected lines at all: `detect` boxes the
//! page's text blocks and cuts their crops, `engine` has the block reader find and
//! read the lines of each, and `post` lays those lines out like any others.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bunko_layout::json::Value;
use bunko_layout::layout::{PageLayout, column_pieces, layout_page};
use bunko_layout::reconcile::Reconciled;
use bunko_layout::records::{RawLine, RawPage};
use bunko_layout::road::{EngineRoad, quad_extents};
use bunko_layout::script::normalize_text;
use bunko_layout::sidecar::{MOKURO_FORMAT_VERSION, Page, blocks_page, finish_page};
use bunko_ocr::blocks::{BlockDetector, TextBlock};
use bunko_ocr::image::{BgrImage, decode_bgr, decode_size};
use bunko_ocr::lines::{DetectorInfo, RawPage as OcrRawPage};
use bunko_ocr::ppocr::{LayoutHooks, Line, PpocrPageReader, ReadLines};
use bunko_vlm::{Bgr, BlockCrop, CropSet, Recognizer, Rgb};
use parking_lot::Mutex;

/// One volume in flight: where its finished pages go, and whether to stop.
pub struct VolumeCtx {
    pub claim: String,
    pub cancelled: AtomicBool,
    pub results: Mutex<std::sync::mpsc::Sender<Finished>>,
}

/// A page as it leaves the pipeline.
pub struct Finished {
    pub index: usize,
    pub rel: String,
    pub outcome: Result<Page, Failure>,
}

/// Why a page has no blocks, and what is known about its image.
#[derive(Debug, Clone)]
pub struct Failure {
    pub error: String,
    /// `(width, height)` from the image header, for the blank page; `None` omits the
    /// page from the sidecar (0.5.2 `_blank_from`).
    pub size: Option<(i64, i64)>,
}

/// A page on its way through the stages.
pub struct Work {
    pub vol: Arc<VolumeCtx>,
    pub index: usize,
    pub rel: String,
    pub size: Option<(i64, i64)>,
    pub state: State,
}

pub enum State {
    /// The member's bytes (or why they could not be read).
    Bytes(Result<Vec<u8>, String>),
    Detected(Box<Detected>),
    /// A block detector's page: the crops of its text blocks.
    Blocked(Box<Blocked>),
    Engined(Box<Engined>),
    /// A block detector's page, read: the lines the block reader found.
    BlockRead(Box<BlockRead>),
    Done(Page),
    Failed(String),
}

/// A detected page. The page image is not carried on (a decoded page is ~15 MB and
/// the queue in front of the engine holds several): the engine's first-read crops
/// are cut here, and the image is kept only for a recognizer that may take a second
/// read of doubted lines (paddle-manga).
pub struct Detected {
    read: ReadLines<PageLayout>,
    first: Option<FirstRead>,
}

/// A page whose text blocks a block detector found.
pub struct Blocked {
    crops: Vec<Rgb>,
    /// Per crop: where its top-left corner is on the page, and the block's score.
    places: Vec<(f64, f64, f64)>,
}

/// The lines a block reader found in each of a block detector's blocks, in page pixels.
pub struct BlockRead {
    blocks: Vec<Vec<RawLine>>,
}

/// One text block of a page for a recognizer that reads blocks.
struct BlockPlan {
    /// The block's lines, as positions in the road's targets, in the block's order.
    targets: Vec<usize>,
    crop: BlockCrop,
}

/// How far a block's crop reaches past the block's box, in line thicknesses: the
/// block readers were trained on tight boxes.
const BLOCK_MARGIN_EM: f64 = 0.1;

/// The engine road's plan and first-read crops of a page, cut in the detect stage.
pub struct FirstRead {
    lines: Vec<RawLine>,
    road: EngineRoad,
    crops: Vec<CropSet>,
    /// For a recognizer that reads blocks: the page's blocks (`crops` is empty then).
    blocks: Vec<BlockPlan>,
    /// The page, for second-read crops.
    page: Option<Bgr>,
}

pub struct Engined {
    lines: Vec<RawLine>,
    settled: Vec<Reconciled>,
    road: EngineRoad,
    detector: Option<Value>,
}

impl Work {
    fn fail(mut self, error: impl Into<String>) -> Work {
        self.state = State::Failed(error.into());
        self
    }

    fn skip(&self) -> bool {
        matches!(self.state, State::Failed(_)) || self.vol.cancelled.load(Ordering::Relaxed)
    }

    /// Hand the finished page to its volume.
    pub fn finish(self) {
        let outcome = match self.state {
            State::Done(page) => Ok(page),
            State::Failed(error) => Err(Failure {
                error,
                size: self.size,
            }),
            _ => Err(Failure {
                error: "the page left the pipeline unfinished".into(),
                size: self.size,
            }),
        };
        let _ = self.vol.results.lock().send(Finished {
            index: self.index,
            rel: self.rel,
            outcome,
        });
    }
}

/// The line layout behind `bunko-ocr`'s page-level passes.
pub struct LayoutBridge;

fn layout_lines(raw: &OcrRawPage) -> Vec<RawLine> {
    raw.lines
        .iter()
        .map(|l| RawLine {
            quad: l.quad.to_vec(),
            score: l.score,
            text: l.text.clone(),
            conf: l.conf,
            vertical: l.vertical,
            angle: l.angle,
            char_confs: l.char_confs.clone(),
        })
        .collect()
}

impl LayoutHooks for LayoutBridge {
    type Layout = PageLayout;

    fn column_pieces(&self, raw: &OcrRawPage) -> Vec<Vec<usize>> {
        column_pieces(&layout_lines(raw))
    }

    fn layout(&self, raw: &OcrRawPage) -> PageLayout {
        // `raw` is already rounded the way `page_to_json` rounds.
        layout_page(&RawPage::new(
            i64::from(raw.width),
            i64::from(raw.height),
            layout_lines(raw),
        ))
    }

    fn ruby_lines(&self, layout: &PageLayout) -> BTreeSet<usize> {
        layout.ruby_lines()
    }

    fn body_lines(&self, layout: &PageLayout) -> BTreeSet<usize> {
        layout.body_members()
    }
}

/// A reader line as the layout and the road see it: unrounded, float32 widened.
pub fn to_layout_line(l: &Line) -> RawLine {
    RawLine {
        quad: l.quad.iter().map(|p| [p[0] as f64, p[1] as f64]).collect(),
        score: l.score,
        text: l.text.clone(),
        conf: l.conf,
        vertical: l.vertical(),
        angle: l.angle(),
        char_confs: l.char_confs.clone(),
    }
}

fn detector_value(info: &DetectorInfo) -> Option<Value> {
    serde_json::to_string(info)
        .ok()
        .and_then(|s| Value::parse(&s).ok())
}

fn quad_of(line: &RawLine) -> bunko_vlm::Quad {
    let mut q = [[0.0f64; 2]; 4];
    for (dst, src) in q.iter_mut().zip(&line.quad) {
        *dst = *src;
    }
    q
}

/// The blocks a block reader reads on a page: every block of the first layout with
/// the engine's lines in it (ruby is not among them), then each line the layout put in
/// no block, alone.
fn block_plans(
    page: &Bgr,
    lines: &[RawLine],
    road: &EngineRoad,
    layout: &PageLayout,
) -> Vec<BlockPlan> {
    let position: HashMap<usize, usize> = road
        .targets
        .iter()
        .enumerate()
        .map(|(k, &i)| (i, k))
        .collect();
    let mut taken = vec![false; road.targets.len()];
    let mut plans = Vec::new();
    for (group, block) in layout.groups.iter().zip(&layout.blocks) {
        let members: Vec<usize> = group
            .iter()
            .filter_map(|i| position.get(i).copied())
            .filter(|&k| !std::mem::replace(&mut taken[k], true))
            .collect();
        if !members.is_empty() {
            plans.push(cut_block(page, lines, road, members, Some(block.bbox)));
        }
    }
    for (k, _) in taken.iter().enumerate().filter(|(_, t)| !**t) {
        plans.push(cut_block(page, lines, road, vec![k], None));
    }
    plans
}

/// A crop edge in whole pixels: `v` rounded to the nearest pixel (half away from zero),
/// clamped to `0..=limit`. Never forced up or down: an edge a hair either side of a
/// whole pixel (53.0 on one route, 53.000015 on another) lands on that pixel either way.
fn crop_edge(v: f64, limit: usize) -> usize {
    v.round().clamp(0.0, limit as f64) as usize
}

/// The pixels `lo..hi` crops along one axis of `limit` pixels ([`crop_edge`] at both
/// ends). A span with any extent on the page keeps at least one pixel, which rounding
/// alone could take away (`10.6..10.9` rounds to `11..11`): the crop is then the pixel
/// its rounded edge starts, or the page's last one. `None` for a span with no extent on
/// the page, or a NaN edge.
fn crop_span(lo: f64, hi: f64, limit: usize) -> Option<(usize, usize)> {
    if lo.is_nan() || hi.is_nan() || hi.min(limit as f64) <= lo.max(0.0) {
        return None;
    }
    let (a, b) = (crop_edge(lo, limit), crop_edge(hi, limit));
    Some(if b > a {
        (a, b)
    } else if a < limit {
        (a, a + 1)
    } else {
        (limit - 1, limit)
    })
}

/// Where a block detector's box (`[x0, y0, x1, y1]`, page px) is cropped on a `w` x `h`
/// page: `[x0, y0, x1, y1]` in whole pixels ([`crop_span`] on each axis), `None` for a
/// box with no extent on the page. The detector gate compares crops by this rule.
pub(crate) fn block_crop(bbox: &[f32; 4], w: usize, h: usize) -> Option<[usize; 4]> {
    let (x0, x1) = crop_span(f64::from(bbox[0]), f64::from(bbox[2]), w)?;
    let (y0, y1) = crop_span(f64::from(bbox[1]), f64::from(bbox[3]), h)?;
    Some([x0, y0, x1, y1])
}

/// A block detector's page cut into its blocks' crops (RGB, by [`block_crop`]); a block
/// with no extent on the page has none.
fn cut_blocks(image: &BgrImage, blocks: &[TextBlock]) -> Blocked {
    let (w, h, page) = (image.width(), image.height(), image.as_raw());
    let (mut crops, mut places) = (Vec::new(), Vec::new());
    for block in blocks {
        let Some([x0, y0, x1, y1]) = block_crop(&block.bbox, w, h) else {
            continue;
        };
        let mut data = Vec::with_capacity((x1 - x0) * (y1 - y0) * 3);
        for y in y0..y1 {
            for px in page[(y * w + x0) * 3..(y * w + x1) * 3].as_chunks::<3>().0 {
                data.extend_from_slice(&[px[2], px[1], px[0]]);
            }
        }
        crops.push(Rgb {
            width: x1 - x0,
            height: y1 - y0,
            data,
        });
        places.push((x0 as f64, y0 as f64, f64::from(block.score)));
    }
    Blocked { crops, places }
}

/// The upright crop of one block: the box of its lines and of the layout's block box
/// (which reaches over the block's ruby), a little margin around it, cut at the page.
fn cut_block(
    page: &Bgr,
    lines: &[RawLine],
    road: &EngineRoad,
    members: Vec<usize>,
    block: Option<[i64; 4]>,
) -> BlockPlan {
    let of = |k: usize| &lines[road.targets[k]];
    let quads: Vec<bunko_vlm::Quad> = members.iter().map(|&k| quad_of(of(k))).collect();
    let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, 0.0f64, 0.0f64);
    for p in quads.iter().flatten() {
        (x0, y0, x1, y1) = (x0.min(p[0]), y0.min(p[1]), x1.max(p[0]), y1.max(p[1]));
    }
    if let Some(b) = block {
        (x0, y0) = (x0.min(b[0] as f64), y0.min(b[1] as f64));
        (x1, y1) = (x1.max(b[2] as f64), y1.max(b[3] as f64));
    }
    let mut thick: Vec<f64> = members
        .iter()
        .map(|&k| quad_extents(&of(k).quad, of(k).vertical).1)
        .collect();
    thick.sort_by(f64::total_cmp);
    let pad = BLOCK_MARGIN_EM * thick.get(thick.len() / 2).copied().unwrap_or(0.0);
    // A box with no extent on the page (no lines) gives an empty crop, as before.
    let span = |lo: f64, hi: f64, limit: usize| {
        crop_span(lo, hi, limit).unwrap_or_else(|| {
            let at = crop_edge(lo, limit);
            (at, at)
        })
    };
    let (cx0, cx1) = span(x0 - pad, x1 + pad, page.width);
    let (cy0, cy1) = span(y0 - pad, y1 + pad, page.height);
    let (w, h) = (cx1 - cx0, cy1 - cy0);
    let mut data = Vec::with_capacity(w * h * 3);
    for y in cy0..cy1 {
        let row = &page.data[(y * page.width + cx0) * 3..(y * page.width + cx1) * 3];
        for px in row.as_chunks::<3>().0 {
            data.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    let shifted = quads
        .iter()
        .map(|q| q.map(|p| [p[0] - cx0 as f64, p[1] - cy0 as f64]))
        .collect();
    BlockPlan {
        crop: BlockCrop {
            image: Rgb {
                width: w,
                height: h,
                data,
            },
            lines: shifted,
            // The CTC read as the reconcile sees it.
            hints: members
                .iter()
                .map(|&k| normalize_text(bunko_layout::py::strip(&of(k).text)))
                .collect(),
        },
        targets: members,
    }
}

/// The models one session reads with: a line reader (the PP-OCR pair, on the row's
/// line detector) or a block detector, and the row's recognizer.
pub struct Engines {
    pub reader: Option<Arc<PpocrPageReader>>,
    pub blocks: Option<Arc<dyn BlockDetector>>,
    pub recognizer: Option<Arc<dyn Recognizer>>,
}

impl Engines {
    /// `detect + CTC read`: decode, read lines, join pieces, probe ends, vote.
    pub fn detect(&self, mut work: Work) -> Work {
        if work.skip() {
            return work.fail("cancelled");
        }
        let bytes = match std::mem::replace(&mut work.state, State::Failed(String::new())) {
            State::Bytes(Ok(b)) => b,
            State::Bytes(Err(e)) => return work.fail(e),
            other => {
                work.state = other;
                return work.fail("detect got a page that was not read");
            }
        };
        let image = match decode_bgr(&bytes) {
            Ok(img) => img,
            Err(e) => {
                work.size = decode_size(&bytes)
                    .ok()
                    .map(|(w, h)| (i64::from(w), i64::from(h)));
                return work.fail(e.to_string());
            }
        };
        drop(bytes);
        work.size = Some((image.width() as i64, image.height() as i64));
        if let Some(detector) = &self.blocks {
            return Self::detect_blocks(work, &image, detector.as_ref());
        }
        let Some(reader) = &self.reader else {
            return work.fail("this session has no detector");
        };
        let read = match reader.read_lines(&image, &LayoutBridge) {
            Ok(read) => read,
            Err(e) => return work.fail(e.to_string()),
        };
        let first = self.recognizer.as_ref().map(|rec| {
            let lines: Vec<RawLine> = read.lines.iter().map(to_layout_line).collect();
            let road = EngineRoad::plan(&lines, &read.first);
            let (iw, ih) = (image.width(), image.height());
            let page = Bgr {
                width: iw,
                height: ih,
                data: image.into_raw(),
            };
            let (crops, blocks) = if rec.block_reader().is_some() {
                (Vec::new(), block_plans(&page, &lines, &road, &read.first))
            } else {
                let crops: Vec<CropSet> = road
                    .targets
                    .iter()
                    .map(|&i| rec.crop(&page, &quad_of(&lines[i]), lines[i].vertical))
                    .collect();
                (crops, Vec::new())
            };
            FirstRead {
                lines,
                road,
                crops,
                blocks,
                page: rec.info().second_read.then_some(page),
            }
        });
        work.state = State::Detected(Box::new(Detected { read, first }));
        work
    }

    /// `detect` with a block detector: the page's text blocks and their crops, as the
    /// detector boxed them (the block readers were trained on such boxes).
    fn detect_blocks(mut work: Work, image: &BgrImage, detector: &dyn BlockDetector) -> Work {
        let blocks = match detector.detect(image) {
            Ok((found, _)) => found,
            Err(e) => return work.fail(e.to_string()),
        };
        work.state = State::Blocked(Box::new(cut_blocks(image, &blocks)));
        work
    }

    /// `engine` on a block detector's page: the block reader finds and reads the lines
    /// of every block; they come back in page pixels.
    fn read_blocks(mut work: Work, page: Blocked, rec: &dyn Recognizer) -> Work {
        let Some(reader) = rec.block_reader() else {
            return work.fail(format!(
                "{} reads lines, and this page has blocks",
                rec.info().engine
            ));
        };
        let found = match reader.read_lines(&page.crops) {
            Ok(found) => found,
            Err(e) => return work.fail(format!("recognizer: {e}")),
        };
        let mut blocks = Vec::with_capacity(found.len());
        for (&(ox, oy, score), block) in page.places.iter().zip(found) {
            let mut lines = Vec::with_capacity(block.len());
            for line in block {
                if line.text.is_empty() {
                    continue;
                }
                let (x0, y0) = (ox + f64::from(line.bbox[0]), oy + f64::from(line.bbox[1]));
                let (x1, y1) = (ox + f64::from(line.bbox[2]), oy + f64::from(line.bbox[3]));
                // One confidence per character, or none: then the block's score
                // stands for the line.
                let confs: Vec<f64> = if line.confs.len() == line.text.chars().count() {
                    line.confs.iter().map(|c| f64::from(*c)).collect()
                } else {
                    Vec::new()
                };
                let conf = if confs.is_empty() {
                    score
                } else {
                    confs.iter().sum::<f64>() / confs.len() as f64
                };
                lines.push(RawLine {
                    quad: vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]],
                    score,
                    text: line.text,
                    conf,
                    vertical: line.vertical,
                    angle: 0.0,
                    char_confs: confs,
                });
            }
            blocks.push(lines);
        }
        if rec.info().device == bunko_vlm::Device::Cpu {
            crate::runtime::trim_heap();
        }
        work.state = State::BlockRead(Box::new(BlockRead { blocks }));
        work
    }

    /// The line road's second stage: `layout + dump`.
    pub fn layout(&self, mut work: Work) -> Work {
        if work.skip() {
            return work.fail("cancelled");
        }
        let (w, h) = work.size.unwrap_or((0, 0));
        match std::mem::replace(&mut work.state, State::Failed(String::new())) {
            State::Detected(d) => {
                let read = d.read;
                let lines: Vec<RawLine> = read.lines.iter().map(to_layout_line).collect();
                let done = finish_page(
                    &lines,
                    w,
                    h,
                    detector_value(&read.info),
                    MOKURO_FORMAT_VERSION,
                );
                work.state = State::Done(done.page);
                work
            }
            _ => work.fail("layout got a page that was not detected"),
        }
    }

    /// `engine read + reconcile` (`ReconciledPageReader.engine_read`).
    pub fn engine(&self, mut work: Work) -> Work {
        if work.skip() {
            return work.fail("cancelled");
        }
        let Some(rec) = self.recognizer.as_ref() else {
            return work.fail("this session has no recognizer");
        };
        let (w, h) = work.size.unwrap_or((0, 0));
        let (read, first) = match std::mem::replace(&mut work.state, State::Failed(String::new())) {
            State::Detected(d) => (d.read, d.first),
            State::Blocked(b) => return Self::read_blocks(work, *b, rec.as_ref()),
            _ => return work.fail("engine got a page that was not detected"),
        };
        let Some(FirstRead {
            mut lines,
            road,
            crops,
            blocks,
            page,
        }) = first
        else {
            return work.fail("engine got a page without first-read crops");
        };
        let caps: Option<Vec<u32>> = rec.info().token_caps.then(|| {
            road.token_caps()
                .into_iter()
                .map(|c| c.clamp(0, i64::from(u32::MAX)) as u32)
                .collect()
        });
        let answer = match rec.block_reader() {
            // Block by block; each block answers for its lines.
            Some(reader) => {
                let (owners, crops): (Vec<Vec<usize>>, Vec<BlockCrop>) =
                    blocks.into_iter().map(|b| (b.targets, b.crop)).unzip();
                reader.read_blocks(&crops).map(|per_block| {
                    let mut texts = vec![String::new(); road.targets.len()];
                    for (targets, block) in owners.iter().zip(per_block) {
                        for (&k, text) in targets.iter().zip(block) {
                            texts[k] = text;
                        }
                    }
                    texts
                })
            }
            None => rec.read(&crops, caps.as_deref()),
        };
        let texts = match answer {
            Ok(t) => t,
            Err(e) => return work.fail(format!("recognizer: {e}")),
        };
        drop(crops);
        let mut settled = if rec.verbatim() {
            road.take_first(&lines, &texts)
        } else {
            road.reconcile_first(&lines, &texts)
        };
        let doubted = road.doubted(&settled);
        if let Some(page) = page.as_ref()
            && !doubted.is_empty()
        {
            let second: Option<Vec<CropSet>> = doubted
                .iter()
                .map(|&k| {
                    let i = road.targets[k];
                    rec.second_crop(page, &quad_of(&lines[i]), lines[i].vertical)
                })
                .collect();
            // Only recognizers with a wider second crop (paddle-manga) read twice.
            if let Some(second) = second {
                let caps2: Option<Vec<u32>> = caps
                    .as_ref()
                    .map(|c| doubted.iter().map(|&k| c[k]).collect());
                match rec.read(&second, caps2.as_deref()) {
                    Ok(texts) => road.settle_second(&mut settled, &doubted, &texts),
                    Err(e) => return work.fail(format!("recognizer (second read): {e}")),
                }
            }
        }
        drop(page);
        if rec.info().device == bunko_vlm::Device::Cpu {
            // A CPU page takes seconds; a trim takes milliseconds and keeps what the
            // recognizer and detector freed from piling up (−150 MB peak, hayai-nova).
            crate::runtime::trim_heap();
        }
        road.apply(&mut lines, &mut settled, w, h);
        work.state = State::Engined(Box::new(Engined {
            lines,
            settled,
            road,
            detector: detector_value(&read.info),
        }));
        work
    }

    /// `layout + dump` on the reconciled road (`finish_read`).
    pub fn post(&self, mut work: Work) -> Work {
        if work.skip() {
            return work.fail("cancelled");
        }
        let (w, h) = work.size.unwrap_or((0, 0));
        match std::mem::replace(&mut work.state, State::Failed(String::new())) {
            State::Engined(e) => {
                let done = e.road.finish(
                    &e.lines,
                    &e.settled,
                    w,
                    h,
                    e.detector,
                    MOKURO_FORMAT_VERSION,
                );
                work.state = State::Done(done.page);
                work
            }
            // A block detector's page: its blocks stand, the layout orders them.
            State::BlockRead(r) => {
                work.state = State::Done(blocks_page(&r.blocks, w, h, MOKURO_FORMAT_VERSION));
                work
            }
            _ => work.fail("post got a page that was not read by the engine"),
        }
    }
}

#[cfg(test)]
mod crop_tests {
    use super::*;

    #[test]
    fn an_edge_rounds_to_the_nearest_pixel() {
        // a hair either side of a whole pixel lands on it (one GPU route read 53.0
        // where the CPU read 53.000015; `ceil` made those 53 and 54)
        assert_eq!(crop_edge(f64::from(53.000_015f32), 100), 53);
        assert_eq!(crop_edge(53.0, 100), 53);
        assert_eq!(crop_edge(f64::from(52.999_985f32), 100), 53);
        assert_eq!(crop_edge(52.6, 100), 53);
        assert_eq!(crop_edge(52.4, 100), 52);
        // half away from zero, neither up nor down by rule
        assert_eq!(crop_edge(52.5, 100), 53);
        assert_eq!(
            block_crop(&[10.6, 52.4, 30.4, 52.6 + 10.0], 100, 100),
            Some([11, 52, 30, 63])
        );
        assert_eq!(
            block_crop(&[0.0, 53.000_015, 20.0, 60.0], 100, 100),
            block_crop(&[0.0, 53.0, 20.0, 60.0], 100, 100)
        );
    }

    #[test]
    fn a_box_on_the_page_keeps_a_pixel_where_rounding_would_collapse_it() {
        // 10.6..10.9 rounds to 11..11: one pixel stays
        assert_eq!(crop_span(10.6, 10.9, 100), Some((11, 12)));
        assert_eq!(crop_span(10.4, 10.45, 100), Some((10, 11)));
        // collapsed at the page's far edge: the last pixel
        assert_eq!(crop_span(99.6, 99.9, 100), Some((99, 100)));
        assert_eq!(crop_span(99.7, 140.0, 100), Some((99, 100)));
        // collapsed at the near edge
        assert_eq!(crop_span(-3.0, 0.2, 100), Some((0, 1)));
        assert_eq!(
            block_crop(&[10.6, 20.0, 10.9, 20.3], 50, 50),
            Some([11, 20, 12, 21])
        );
        // no extent on the page, or none at all: no crop
        assert_eq!(crop_span(10.0, 10.0, 100), None);
        assert_eq!(crop_span(12.0, 11.0, 100), None);
        assert_eq!(crop_span(-5.0, -2.0, 100), None);
        assert_eq!(crop_span(-0.3, 0.0, 100), None);
        assert_eq!(crop_span(100.0, 120.0, 100), None);
        assert_eq!(crop_span(0.0, 5.0, 0), None);
        assert_eq!(crop_span(f64::NAN, 5.0, 100), None);
        assert_eq!(crop_span(0.0, f64::NAN, 100), None);
        assert_eq!(block_crop(&[0.0, 0.0, 10.0, f32::NAN], 50, 50), None);
    }

    #[test]
    fn a_crop_is_clamped_to_the_page() {
        assert_eq!(
            block_crop(&[-4.6, -0.4, 640.7, 960.2], 640, 960),
            Some([0, 0, 640, 960])
        );
        assert_eq!(crop_span(-1.5, 3.5, 3), Some((0, 3)));
        assert_eq!(crop_span(2.6, 9.0, 3), Some((2, 3)));
    }

    #[test]
    fn a_detected_block_is_cropped_by_the_rounding_rule() {
        // every pixel says where it is: B = x, G = y
        let (w, h) = (8usize, 6usize);
        let mut raw = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                raw.extend_from_slice(&[x as u8, y as u8, 7]);
            }
        }
        let page = BgrImage::from_raw(w, h, raw).unwrap();
        let blocks = [
            [1.4, 2.000_015, 4.6, 4.0],
            [7.6, 0.0, 7.9, 1.2],
            [3.0, 3.0, 3.0, 5.0],
        ]
        .map(|bbox| TextBlock { bbox, score: 0.5 });
        let b = cut_blocks(&page, &blocks);
        // the zero-width block has no crop; the sliver at the far edge keeps the last column
        assert_eq!(b.places, [(1.0, 2.0, 0.5), (7.0, 0.0, 0.5)]);
        let (first, second) = (&b.crops[0], &b.crops[1]);
        assert_eq!((first.width, first.height), (4, 2));
        // RGB out: R = 7, G = y, B = x; the first pixel is (1, 2), the last (4, 3)
        assert_eq!(&first.data[..3], &[7, 2, 1]);
        assert_eq!(&first.data[first.data.len() - 3..], &[7, 3, 4]);
        assert_eq!((second.width, second.height), (1, 1));
        assert_eq!(second.data, [7, 0, 7]);
    }
}
