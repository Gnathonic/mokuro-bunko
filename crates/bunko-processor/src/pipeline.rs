//! What the processor runtime needs from an OCR engine: the [`PagePipeline`] seam.
//!
//! The runtime (sessions, fetching, the remote client, the local link) knows nothing
//! about models. An engine crate plugs in by implementing [`PagePipeline`] (load the
//! models for one generation row) and [`VolumeRunner`] (run one volume on the loaded
//! models). Everything the library hears — `ready`, `volume_started`, `page`, `stats`,
//! `volume_done`, `volume_failed`, `fatal`, `exit` — is produced by the runtime from
//! these calls, with the invariants of the 0.5.2 runner protocol
//! (`spec/ocr-recognizers.md` §1, `spec/ocr-scheduling.md` §17).

use std::collections::BTreeMap;
use std::path::Path;

use bunko_proto::{Catalog, HostInfo, RowSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Cooperative cancellation shared by the runtime and an engine. Engines poll
/// [`CancelToken::is_cancelled`] between pages (it is cheap) and return
/// [`RunError::Cancelled`] once it is set.
pub type CancelToken = tokio_util::sync::CancellationToken;

/// What a machine is and can run: sent at every registration (`host`, `catalog`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MachineInfo {
    pub host: HostInfo,
    pub catalog: Catalog,
}

/// The OCR engine side of a processor. One instance serves every session of a
/// processor; [`PagePipeline::open`] is called once per `open_session`, on a dedicated
/// OS thread (never a tokio worker), and may block for as long as model loading takes.
pub trait PagePipeline: Send + Sync {
    /// The host and the catalog (engines, detectors, devices) this machine offers.
    /// Re-read at every registration, so models that finish downloading while the
    /// processor runs become visible on the next reconnect. May block briefly.
    fn describe(&self) -> MachineInfo;

    /// Load the models for one generation row and return the loaded session.
    /// An `Err` becomes `fatal{error}` then `exit{returncode: 1}`; nothing is blamed
    /// for it on the library (a session that never became ready is an environment
    /// failure).
    fn open(&self, spec: &RowSpec) -> Result<Box<dyn VolumeRunner>, String>;
}

/// One loaded session: the models of one row on this machine.
///
/// `run_volume` is called from dedicated OS threads. With [`VolumeRunner::overlap`]
/// returning 2, two calls may run at once (the next volume is detecting while the
/// previous one is still in the recognizer), so implementations that overlap must be
/// internally synchronised; the runtime keeps terminal events in arrival order.
pub trait VolumeRunner: Send + Sync {
    /// What `ready` reports. `startup_seconds` is measured by the runtime.
    fn ready(&self) -> ReadyInfo;

    /// How many volumes may run at once (1 or 2; anything else is clamped).
    fn overlap(&self) -> usize {
        1
    }

    /// Run one volume: read the archive at `archive` (a verified `.cbz`/zip; for a RAM
    /// placement on Linux a `/proc/self/fd/<n>` path), write the finished sidecar to
    /// `out` (via `<out>.tmp` + rename), and report progress.
    ///
    /// `progress(PageProgress::Started{pages})` should come first, once the page list
    /// is known; then one `Page{done, total}` per page leaving the pipeline, in page
    /// order. If the runner never reports `Started`, the runtime sends
    /// `volume_started` itself before the terminal event (pages 0 on failure).
    ///
    /// Errors: [`RunError::Volume`] fails this volume only (`volume_failed`, e.g.
    /// "every page failed", "no page images found in <stem>.cbz");
    /// [`RunError::Fatal`] ends the whole session (a model that failed mid-run — never
    /// report that as blank pages); [`RunError::Cancelled`] after `cancel` was set.
    fn run_volume(
        &self,
        archive: &Path,
        meta: &VolumeMeta,
        out: &Path,
        progress: &dyn Fn(PageProgress),
        cancel: &CancelToken,
    ) -> Result<VolumeOutcome, RunError>;

    /// The cumulative pipeline counters right now (spec ocr-scheduling §21.1), for the
    /// `stats` event the runtime sends at most every 2 s while pages flow.
    fn stats(&self) -> Option<PipelineReport> {
        None
    }

    /// The widest each stage may run in this session on this host, by stage key: how
    /// far a benchmark's width search may widen it (0.5.2's structural ceiling and host
    /// budget). Empty: the engine does not say, and the benchmark estimates it
    /// ([`crate::bench::estimated_ceiling`]).
    fn width_ceilings(&self) -> BTreeMap<String, u32> {
        BTreeMap::new()
    }
}

/// Everything about a volume the sidecar needs besides its pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeMeta {
    /// The library's claim id (`v<n>`); also the `id` of every event.
    pub claim: String,
    /// Series title (`title`).
    pub title: String,
    /// Volume title (`volume`).
    pub volume: String,
    pub title_uuid: Option<String>,
    pub volume_uuid: Option<String>,
    /// The archive's own stem (basename, no extension): messages name the archive by
    /// it (`no page images found in <stem>.cbz`), never by a spool path.
    pub stem: String,
    /// The sidecar file name the library expects (`out`'s file name).
    pub sidecar_name: String,
}

/// Progress of one volume, reported from inside [`VolumeRunner::run_volume`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageProgress {
    /// The page list is known.
    Started { pages: u32 },
    /// A page left the pipeline (`done` counts failed pages too).
    Page { done: u32, total: u32 },
}

/// A finished volume. The sidecar is at `out`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VolumeOutcome {
    /// Pages written to the sidecar.
    pub pages: u32,
    /// Pages that failed and were written blank.
    pub failed_pages: u32,
    /// This volume's window of the pipeline counters (`report.since(mark)`).
    pub stats: Option<PipelineReport>,
}

/// Why a volume did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// This volume failed; the session goes on.
    Volume(String),
    /// The session is broken (a model failed to load or died): every accepted volume
    /// is failed with this error, then `fatal`, then `exit{1}`.
    Fatal(String),
    /// `cancel` was set.
    Cancelled,
}

impl From<String> for RunError {
    fn from(error: String) -> Self {
        RunError::Volume(error)
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Volume(e) | RunError::Fatal(e) => f.write_str(e),
            RunError::Cancelled => f.write_str("cancelled"),
        }
    }
}

/// What `ready` reports about a loaded session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadyInfo {
    /// `{repo or export id: revision}` of every weight file in use.
    pub weights: BTreeMap<String, String>,
    pub stage_workers: BTreeMap<String, u32>,
    pub queue_capacity: BTreeMap<String, u32>,
    /// `{model stage: "cpu" | "gpu:<n>"}`.
    pub stage_device: BTreeMap<String, String>,
    /// The graph line, e.g. `detect (cpu x3, queue 4) -> engine (gpu:0 x1, queue 1)`.
    pub pipeline: String,
    /// The precision actually used.
    pub precision: Option<String>,
}

// --- pipeline counters (spec ocr-scheduling §21.1) ---------------------------------------

/// One stage's cumulative counters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StageReport {
    pub key: String,
    pub name: String,
    pub device: String,
    pub workers: u32,
    pub items: u64,
    pub busy_seconds: f64,
    pub blocked_seconds: f64,
    pub starved_seconds: f64,
    /// busy / (max(workers,1) × elapsed).
    pub utilisation: f64,
    #[serde(default)]
    pub device_bound: bool,
}

/// One queue's cumulative counters (`<producer>-><consumer>`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueueReport {
    pub name: String,
    pub capacity: u32,
    pub depth: u32,
    pub max_depth: u32,
    pub mean_depth: f64,
    pub depth_seconds: f64,
    pub puts: u64,
    pub gets: u64,
    pub blocked_seconds: f64,
    pub blocked_events: u64,
    pub starved_seconds: f64,
    pub starved_events: u64,
}

impl QueueReport {
    pub fn fill(&self) -> f64 {
        if self.capacity > 0 {
            self.mean_depth / f64::from(self.capacity)
        } else {
            0.0
        }
    }
}

/// Everything a pipeline's pools and queues measured since it opened.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PipelineReport {
    pub elapsed_seconds: f64,
    pub items: u64,
    pub stages: Vec<StageReport>,
    pub queues: Vec<QueueReport>,
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

impl PipelineReport {
    /// The busiest stage (per worker) among stages that saw items.
    pub fn bottleneck(&self) -> Option<&StageReport> {
        self.stages
            .iter()
            .filter(|s| s.items > 0)
            .max_by(|a, b| a.utilisation.total_cmp(&b.utilisation))
    }

    /// This report minus an earlier one: one volume's window of a session. A report
    /// whose stages/queues do not line up with `earlier` is returned whole (0.5.2).
    pub fn since(&self, earlier: Option<&PipelineReport>) -> PipelineReport {
        let Some(earlier) = earlier else {
            return self.clone();
        };
        let same_stages = self
            .stages
            .iter()
            .map(|s| &s.key)
            .eq(earlier.stages.iter().map(|s| &s.key));
        let same_queues = self
            .queues
            .iter()
            .map(|q| &q.name)
            .eq(earlier.queues.iter().map(|q| &q.name));
        if !same_stages || !same_queues {
            return self.clone();
        }
        let elapsed = (self.elapsed_seconds - earlier.elapsed_seconds).max(0.0);
        let stages = self
            .stages
            .iter()
            .zip(&earlier.stages)
            .map(|(now, was)| {
                let busy = now.busy_seconds - was.busy_seconds;
                let workers = f64::from(now.workers.max(1));
                StageReport {
                    items: now.items.saturating_sub(was.items),
                    busy_seconds: busy,
                    blocked_seconds: now.blocked_seconds - was.blocked_seconds,
                    starved_seconds: now.starved_seconds - was.starved_seconds,
                    utilisation: if elapsed > 0.0 {
                        busy / (workers * elapsed)
                    } else {
                        0.0
                    },
                    ..now.clone()
                }
            })
            .collect();
        let queues = self
            .queues
            .iter()
            .zip(&earlier.queues)
            .map(|(now, was)| {
                let depth_seconds = now.depth_seconds - was.depth_seconds;
                QueueReport {
                    mean_depth: if elapsed > 0.0 {
                        depth_seconds / elapsed
                    } else {
                        0.0
                    },
                    depth_seconds,
                    puts: now.puts.saturating_sub(was.puts),
                    gets: now.gets.saturating_sub(was.gets),
                    blocked_seconds: now.blocked_seconds - was.blocked_seconds,
                    blocked_events: now.blocked_events.saturating_sub(was.blocked_events),
                    starved_seconds: now.starved_seconds - was.starved_seconds,
                    starved_events: now.starved_events.saturating_sub(was.starved_events),
                    ..now.clone()
                }
            })
            .collect();
        PipelineReport {
            elapsed_seconds: elapsed,
            items: self.items.saturating_sub(earlier.items),
            stages,
            queues,
        }
    }

    /// The wire shape (`PipelineReport.as_dict` in 0.5.2): rounded ratios, `fill`
    /// per queue and the `bottleneck` stage key.
    pub fn to_value(&self) -> Value {
        let stages: Vec<Value> = self
            .stages
            .iter()
            .map(|s| {
                serde_json::json!({
                    "key": s.key, "name": s.name, "device": s.device, "workers": s.workers,
                    "items": s.items, "busy_seconds": s.busy_seconds,
                    "blocked_seconds": s.blocked_seconds, "starved_seconds": s.starved_seconds,
                    "utilisation": round3(s.utilisation), "device_bound": s.device_bound,
                })
            })
            .collect();
        let queues: Vec<Value> = self
            .queues
            .iter()
            .map(|q| {
                serde_json::json!({
                    "name": q.name, "capacity": q.capacity, "depth": q.depth,
                    "max_depth": q.max_depth, "mean_depth": q.mean_depth,
                    "depth_seconds": q.depth_seconds, "puts": q.puts, "gets": q.gets,
                    "blocked_seconds": q.blocked_seconds, "blocked_events": q.blocked_events,
                    "starved_seconds": q.starved_seconds, "starved_events": q.starved_events,
                    "fill": round3(q.fill()),
                })
            })
            .collect();
        serde_json::json!({
            "elapsed_seconds": round3(self.elapsed_seconds),
            "items": self.items,
            "stages": stages,
            "queues": queues,
            "bottleneck": self.bottleneck().map(|s| s.key.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(elapsed: f64, items: u64, busy: f64, depth_seconds: f64) -> PipelineReport {
        PipelineReport {
            elapsed_seconds: elapsed,
            items,
            stages: vec![StageReport {
                key: "detect".into(),
                name: "ppocr".into(),
                device: "cpu".into(),
                workers: 2,
                items,
                busy_seconds: busy,
                utilisation: busy / (2.0 * elapsed),
                ..Default::default()
            }],
            queues: vec![QueueReport {
                name: "detect->engine".into(),
                capacity: 4,
                depth_seconds,
                mean_depth: depth_seconds / elapsed,
                puts: items,
                ..Default::default()
            }],
        }
    }

    #[test]
    fn since_differences_counters_and_rederives_ratios() {
        let a = report(10.0, 10, 8.0, 20.0);
        let b = report(20.0, 30, 18.0, 30.0);
        let w = b.since(Some(&a));
        assert_eq!(w.items, 20);
        assert!((w.elapsed_seconds - 10.0).abs() < 1e-9);
        assert!((w.stages[0].utilisation - 0.5).abs() < 1e-9);
        assert!((w.queues[0].mean_depth - 1.0).abs() < 1e-9);
        let v = w.to_value();
        assert_eq!(v["bottleneck"], "detect");
        assert_eq!(v["queues"][0]["fill"], 0.25);
    }

    #[test]
    fn since_refuses_a_different_pipeline() {
        let a = report(10.0, 10, 8.0, 20.0);
        let mut b = report(20.0, 30, 18.0, 30.0);
        b.stages[0].key = "engine".into();
        assert_eq!(b.since(Some(&a)), b);
    }
}
