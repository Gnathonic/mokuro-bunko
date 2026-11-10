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
use bunko_ocr::blocks::BlockDetector;
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
    let edge = |v: f64, limit: usize| v.clamp(0.0, limit as f64) as usize;
    let (cx0, cy0) = (
        edge((x0 - pad).floor(), page.width),
        edge((y0 - pad).floor(), page.height),
    );
    let (cx1, cy1) = (
        edge((x1 + pad).ceil(), page.width).max(cx0),
        edge((y1 + pad).ceil(), page.height).max(cy0),
    );
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
        let (w, h, page) = (image.width(), image.height(), image.as_raw());
        let (mut crops, mut places) = (Vec::new(), Vec::new());
        for block in &blocks {
            let edge = |v: f32, limit: usize| v.clamp(0.0, limit as f32) as usize;
            let (x0, y0) = (
                edge(block.bbox[0].floor(), w),
                edge(block.bbox[1].floor(), h),
            );
            let (x1, y1) = (edge(block.bbox[2].ceil(), w), edge(block.bbox[3].ceil(), h));
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
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
        work.state = State::Blocked(Box::new(Blocked { crops, places }));
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
