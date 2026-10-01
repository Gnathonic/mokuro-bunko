//! What each stage does to a page (spec ocr-recognizers §2.3-§2.4, ocr-ppocr-layout
//! §5, §7): detect + CTC read with the page-level passes; the engine's first read,
//! reconcile, second read and verdicts; the layout into a mokuro page.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bunko_layout::json::Value;
use bunko_layout::layout::{PageLayout, column_pieces, layout_page};
use bunko_layout::reconcile::Reconciled;
use bunko_layout::records::{RawLine, RawPage};
use bunko_layout::road::EngineRoad;
use bunko_layout::sidecar::{MOKURO_FORMAT_VERSION, Page, finish_page};
use bunko_ocr::image::{BgrImage, decode_bgr, decode_size};
use bunko_ocr::lines::{DetectorInfo, RawPage as OcrRawPage};
use bunko_ocr::ppocr::{LayoutHooks, Line, PpocrPageReader, ReadLines};
use bunko_vlm::{Bgr, CropSet, Recognizer};
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
    Engined(Box<Engined>),
    Done(Page),
    Failed(String),
}

pub struct Detected {
    image: BgrImage,
    read: ReadLines<PageLayout>,
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

/// The models one session reads with.
pub struct Engines {
    pub reader: Arc<PpocrPageReader>,
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
        match self.reader.read_lines(&image, &LayoutBridge) {
            Ok(read) => {
                work.state = State::Detected(Box::new(Detected { image, read }));
                work
            }
            Err(e) => work.fail(e.to_string()),
        }
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
        let (image, read) = match std::mem::replace(&mut work.state, State::Failed(String::new())) {
            State::Detected(d) => (d.image, d.read),
            _ => return work.fail("engine got a page that was not detected"),
        };
        let mut lines: Vec<RawLine> = read.lines.iter().map(to_layout_line).collect();
        let road = EngineRoad::plan(&lines, &read.first);
        let (iw, ih) = (image.width(), image.height());
        let page = Bgr {
            width: iw,
            height: ih,
            data: image.into_raw(),
        };
        let caps: Option<Vec<u32>> = rec.info().token_caps.then(|| {
            road.token_caps()
                .into_iter()
                .map(|c| c.clamp(0, i64::from(u32::MAX)) as u32)
                .collect()
        });
        let crops: Vec<CropSet> = road
            .targets
            .iter()
            .map(|&i| rec.crop(&page, &quad_of(&lines[i]), lines[i].vertical))
            .collect();
        let texts = match rec.read(&crops, caps.as_deref()) {
            Ok(t) => t,
            Err(e) => return work.fail(format!("recognizer: {e}")),
        };
        drop(crops);
        let mut settled = road.reconcile_first(&lines, &texts);
        let doubted = road.doubted(&settled);
        if !doubted.is_empty() {
            let second: Option<Vec<CropSet>> = doubted
                .iter()
                .map(|&k| {
                    let i = road.targets[k];
                    rec.second_crop(&page, &quad_of(&lines[i]), lines[i].vertical)
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
            _ => work.fail("post got a page that was not read by the engine"),
        }
    }
}
