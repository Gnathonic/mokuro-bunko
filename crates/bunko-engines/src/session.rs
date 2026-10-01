//! One open session: the models of a generation row, a running stage pipeline, and
//! volumes fed through it (spec ocr-recognizers §1.5, §2).
//!
//! The pipeline lives as long as the session, so it never drains between volumes:
//! with two volumes accepted (`overlap() == 2`) the next one is detecting while the
//! previous one is still in the engine. Volumes are fed in arrival order (a ticket
//! turnstile), each volume's pages come back to its own sink, which reorders them by
//! page index and reports progress in page order. A page that fails in any stage
//! becomes a blank page sized from its image header (omitted when even the header is
//! unreadable); a volume with no page left fails with `every page failed`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bunko_layout::json::Value;
use bunko_layout::sidecar::{MOKURO_FORMAT_VERSION, OcrEngine, Page, VolumeHeader, build_volume};
use bunko_ocr::pages::ArchivePages;
use bunko_processor::{
    CancelToken, PageProgress, PipelineReport, ReadyInfo, RunError, VolumeMeta, VolumeOutcome,
    VolumeRunner,
};
use parking_lot::{Condvar, Mutex};
use tracing::{debug, error, info, warn};

use crate::page::{Engines, Finished, State, VolumeCtx, Work};
use crate::plan::{Road, STAGE_DETECT, STAGE_ENGINE, STAGE_LAYOUT, STAGE_POST, StagePlan};
use crate::stages::{PanicFn, Pipeline, StageDef, StageFn};

/// Everything a session needs to write sidecars besides its pages.
pub struct SessionInfo {
    pub engine: String,
    pub patch_budget: u32,
    pub generator: String,
    /// Insertion-ordered `ocr_engine.weights`.
    pub weights: Vec<(String, String)>,
    pub precision: Option<String>,
    pub ready: ReadyInfo,
}

/// Volumes take a ticket at arrival and feed in ticket order.
#[derive(Default)]
struct Turnstile {
    next_ticket: AtomicU64,
    /// (ticket being served, tickets that gave up before their turn)
    state: Mutex<(u64, BTreeSet<u64>)>,
    turn: Condvar,
}

impl Turnstile {
    fn ticket(&self) -> u64 {
        self.next_ticket.fetch_add(1, Ordering::SeqCst)
    }

    /// Wait for `ticket`'s turn; false when `cancel` fires first.
    fn wait(&self, ticket: u64, cancel: &CancelToken) -> bool {
        let mut state = self.state.lock();
        while state.0 != ticket {
            if cancel.is_cancelled() {
                return false;
            }
            self.turn.wait_for(&mut state, Duration::from_millis(100));
        }
        true
    }

    /// `ticket` is done feeding, or gave up (then its turn is skipped when it comes).
    fn done(&self, ticket: u64) {
        let mut state = self.state.lock();
        if state.0 == ticket {
            state.0 += 1;
            loop {
                let serving = state.0;
                if !state.1.remove(&serving) {
                    break;
                }
                state.0 += 1;
            }
        } else if ticket > state.0 {
            state.1.insert(ticket);
        }
        self.turn.notify_all();
    }
}

/// A loaded session (what `PagePipeline::open` returns).
pub struct EngineRunner {
    info: SessionInfo,
    pipeline: Arc<Pipeline<Work>>,
    router: Mutex<Option<std::thread::JoinHandle<()>>>,
    feed: Turnstile,
    broken: Mutex<Option<String>>,
    // Keeps the models alive as long as the session.
    _engines: Arc<Engines>,
}

impl EngineRunner {
    pub fn start(
        info: SessionInfo,
        engines: Arc<Engines>,
        road: Road,
        stages: &[StagePlan],
        source_capacity: u32,
    ) -> Result<EngineRunner, String> {
        let defs: Vec<StageDef<Work>> = stages
            .iter()
            .map(|s| {
                let e = engines.clone();
                let run: StageFn<Work> = match (road, s.key) {
                    (_, STAGE_DETECT) => Arc::new(move |w| e.detect(w)),
                    (Road::Line, STAGE_LAYOUT) => Arc::new(move |w| e.layout(w)),
                    (Road::Reconciled, STAGE_ENGINE) => Arc::new(move |w| e.engine(w)),
                    (Road::Reconciled, STAGE_POST) => Arc::new(move |w| e.post(w)),
                    (_, other) => {
                        let other = other.to_string();
                        Arc::new(move |mut w: Work| {
                            w.state = State::Failed(format!("no stage {other}"));
                            w
                        })
                    }
                };
                StageDef {
                    key: s.key.to_string(),
                    name: s.name.to_string(),
                    device: s.device.clone(),
                    device_bound: s.device_bound,
                    workers: s.workers,
                    capacity: s.capacity,
                    run,
                }
            })
            .collect();
        // A panicking stage loses its item; the volume's sink then never sees that
        // page, so a panic is turned into nothing here and the sink times the page
        // out (see `run_volume`). Stage functions do not panic in practice.
        let on_panic: Option<PanicFn<Work>> = None;
        let pipeline = Pipeline::start("ocr", defs, source_capacity, on_panic)
            .map_err(|e| format!("could not start the pipeline threads: {e}"))?;
        let router = {
            let p = pipeline.clone();
            std::thread::Builder::new()
                .name("ocr-sink".into())
                .spawn(move || {
                    while let Some(work) = p.take() {
                        work.finish();
                    }
                })
                .map_err(|e| format!("could not start the pipeline sink: {e}"))?
        };
        Ok(EngineRunner {
            info,
            pipeline,
            router: Mutex::new(Some(router)),
            feed: Turnstile::default(),
            broken: Mutex::new(None),
            _engines: engines,
        })
    }

    fn sidecar(&self, meta: &VolumeMeta, pages: &[(String, Page)]) -> Value {
        let pick = |v: &str| {
            if v.trim().is_empty() {
                meta.stem.clone()
            } else {
                v.to_string()
            }
        };
        let uuid = |v: &Option<String>| {
            v.clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
        };
        let mut engine = OcrEngine::new(
            &self.info.engine,
            Some(&self.info.generator),
            i64::from(self.info.patch_budget),
        );
        engine.weights = self.info.weights.clone();
        engine.precision = self.info.precision.clone();
        let header = VolumeHeader {
            version: MOKURO_FORMAT_VERSION.to_string(),
            title: pick(&meta.title),
            title_uuid: uuid(&meta.title_uuid),
            volume: pick(&meta.volume),
            volume_uuid: uuid(&meta.volume_uuid),
            ocr_engine: Some(engine.to_value()),
        };
        build_volume(&header, pages)
    }
}

impl Drop for EngineRunner {
    fn drop(&mut self) {
        self.pipeline.shutdown();
        if let Some(r) = self.router.lock().take() {
            let _ = r.join();
        }
    }
}

/// A page that has not come back for this long is given up as failed (only a stage
/// panic can lose a page; a slow CPU paddle-manga page takes tens of seconds).
const PAGE_LOST_AFTER: Duration = Duration::from_secs(1800);

impl VolumeRunner for EngineRunner {
    fn ready(&self) -> ReadyInfo {
        self.info.ready.clone()
    }

    fn overlap(&self) -> usize {
        2
    }

    fn stats(&self) -> Option<PipelineReport> {
        Some(self.pipeline.report())
    }

    fn run_volume(
        &self,
        archive: &Path,
        meta: &VolumeMeta,
        out: &Path,
        progress: &dyn Fn(PageProgress),
        cancel: &CancelToken,
    ) -> Result<VolumeOutcome, RunError> {
        if let Some(e) = self.broken.lock().clone() {
            return Err(RunError::Fatal(e));
        }
        let ticket = self.feed.ticket();
        let started = Instant::now();
        let result = self.run(archive, meta, out, progress, cancel, ticket);
        match &result {
            Ok(o) => info!(
                claim = %meta.claim,
                "wrote {} pages={} failed_pages={} elapsed={:.1}s",
                out.display(),
                o.pages,
                o.failed_pages,
                started.elapsed().as_secs_f64()
            ),
            Err(RunError::Cancelled) => info!(claim = %meta.claim, "volume cancelled"),
            Err(e) => warn!(claim = %meta.claim, "volume failed: {e}"),
        }
        result
    }
}

impl EngineRunner {
    fn run(
        &self,
        archive: &Path,
        meta: &VolumeMeta,
        out: &Path,
        progress: &dyn Fn(PageProgress),
        cancel: &CancelToken,
        ticket: u64,
    ) -> Result<VolumeOutcome, RunError> {
        let opened = ArchivePages::open(archive, Some(&meta.stem));
        let mut archive_pages = match opened {
            Ok(p) => p,
            Err(e) => {
                self.feed.done(ticket);
                return Err(RunError::Volume(e.to_string()));
            }
        };
        let pages: Vec<String> = archive_pages.pages().to_vec();
        if pages.is_empty() {
            self.feed.done(ticket);
            return Err(RunError::Volume(format!(
                "no page images found in {}.cbz",
                meta.stem
            )));
        }
        let total = pages.len();
        progress(PageProgress::Started {
            pages: total as u32,
        });
        let (tx, rx) = std::sync::mpsc::channel::<Finished>();
        let vol = Arc::new(VolumeCtx {
            claim: meta.claim.clone(),
            cancelled: AtomicBool::new(false),
            results: Mutex::new(tx),
        });
        let mark: Mutex<Option<PipelineReport>> = Mutex::new(None);
        let fed = AtomicU64::new(0);

        let outcome = std::thread::scope(|scope| {
            let feeder = scope.spawn(|| {
                if !self.feed.wait(ticket, cancel) {
                    self.feed.done(ticket);
                    return;
                }
                // The volume's window opens when its first page enters.
                *mark.lock() = Some(self.pipeline.report());
                for (index, rel) in pages.iter().enumerate() {
                    if cancel.is_cancelled() || vol.cancelled.load(Ordering::Relaxed) {
                        break;
                    }
                    let bytes = archive_pages.read(rel).map_err(|e| e.to_string());
                    let work = Work {
                        vol: vol.clone(),
                        index,
                        rel: rel.clone(),
                        size: None,
                        state: State::Bytes(bytes),
                    };
                    if self.pipeline.put(work).is_err() {
                        break;
                    }
                    fed.fetch_add(1, Ordering::SeqCst);
                }
                self.feed.done(ticket);
            });
            let result = self.sink(&vol, &rx, &pages, progress, cancel, &fed, &feeder);
            if result.is_err() {
                vol.cancelled.store(true, Ordering::SeqCst);
            }
            let _ = feeder.join();
            result
        });
        let (results, failed) = outcome?;
        if results.is_empty() {
            return Err(RunError::Volume("every page failed".into()));
        }
        let volume = self.sidecar(meta, &results);
        bunko_layout::sidecar::write_sidecar(out, &volume)
            .map_err(|e| RunError::Volume(e.to_string()))?;
        // The volume's buffers are gone: give the C heap's free pages back.
        crate::runtime::trim_heap();
        let stats = self.pipeline.report().since(mark.lock().as_ref());
        Ok(VolumeOutcome {
            pages: results.len() as u32,
            failed_pages: failed,
            stats: Some(stats),
        })
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn sink(
        &self,
        vol: &Arc<VolumeCtx>,
        rx: &std::sync::mpsc::Receiver<Finished>,
        pages: &[String],
        progress: &dyn Fn(PageProgress),
        cancel: &CancelToken,
        fed: &AtomicU64,
        feeder: &std::thread::ScopedJoinHandle<'_, ()>,
    ) -> Result<(Vec<(String, Page)>, u32), RunError> {
        let total = pages.len();
        let mut held: HashMap<usize, Finished> = HashMap::new();
        let mut results: Vec<(String, Page)> = Vec::with_capacity(total);
        let mut failed = 0u32;
        let mut next = 0usize;
        let mut last_seen = Instant::now();
        let mut page_started = Instant::now();
        while next < total {
            if cancel.is_cancelled() {
                return Err(RunError::Cancelled);
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(f) => {
                    last_seen = Instant::now();
                    held.insert(f.index, f);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // The feeder stopped early (pipeline closed): nothing more comes.
                    let all_fed = fed.load(Ordering::SeqCst) as usize;
                    if feeder.is_finished() && all_fed <= next && held.is_empty() {
                        return Err(RunError::Fatal(
                            "the OCR pipeline stopped before this volume did".into(),
                        ));
                    }
                    if last_seen.elapsed() <= PAGE_LOST_AFTER || !feeder.is_finished() {
                        continue;
                    }
                    // Only a panicking stage loses a page: give it up and go on.
                    error!(claim = %vol.claim, "page {} never came back", pages[next]);
                    last_seen = Instant::now();
                    held.entry(next).or_insert_with(|| Finished {
                        index: next,
                        rel: pages[next].clone(),
                        outcome: Err(crate::page::Failure {
                            error: "the page was lost in the pipeline".into(),
                            size: None,
                        }),
                    });
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(RunError::Fatal("the OCR pipeline went away".into()));
                }
            }
            while let Some(f) = held.remove(&next) {
                match f.outcome {
                    Ok(page) => {
                        debug!(
                            claim = %vol.claim,
                            "page {}/{} {} blocks={} ({:.2}s)",
                            next + 1,
                            total,
                            f.rel,
                            page.blocks.len(),
                            page_started.elapsed().as_secs_f64()
                        );
                        results.push((f.rel, page));
                    }
                    Err(failure) => {
                        failed += 1;
                        error!(claim = %vol.claim, "page {}: {}", f.rel, failure.error);
                        if let Some((w, h)) = failure.size {
                            results.push((f.rel, Page::blank(w, h)));
                        }
                    }
                }
                page_started = Instant::now();
                next += 1;
                progress(PageProgress::Page {
                    done: next as u32,
                    total: total as u32,
                });
            }
        }
        Ok((results, failed))
    }
}

/// `ready.stage_device`: the stages that hold a model.
pub fn stage_devices(stages: &[StagePlan]) -> BTreeMap<String, String> {
    stages
        .iter()
        .filter(|s| s.key == STAGE_DETECT || s.key == STAGE_ENGINE)
        .map(|s| (s.key.to_string(), s.device.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turnstile_serves_in_ticket_order_and_skips_quitters() {
        let t = Turnstile::default();
        let cancel = CancelToken::new();
        let (a, b, c) = (t.ticket(), t.ticket(), t.ticket());
        // b gives up before its turn; a finishes; c goes next.
        t.done(b);
        assert!(t.wait(a, &cancel));
        t.done(a);
        assert!(t.wait(c, &cancel));
        t.done(c);
        let d = t.ticket();
        assert!(t.wait(d, &cancel));
        let e = t.ticket();
        cancel.cancel();
        assert!(!t.wait(e, &cancel));
    }
}
