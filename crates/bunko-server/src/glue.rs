//! Adapters between the OCR scheduler and the library/catalog runtime.

use crate::library::{ArchiveEvents, OcrStatusSource, OutlookSource, VolumeOutlook};
use crate::ocr::{Asked, OcrControl};
use serde_json::{Map, Value};
use std::path::Path;
use std::time::Duration;

/// Run a blocking scheduler query from wherever the caller is (async handler or blocking
/// thread) without starving a tokio worker.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

pub struct OcrGlue(pub OcrControl);

impl OutlookSource for OcrGlue {
    fn volume_outlook(&self, cbz: &Path, _series: &str, _volume: &str) -> VolumeOutlook {
        let library = self.0.core().layout.library();
        let Some(rel) = crate::ocr::types::rel_of(&library, cbz) else {
            return VolumeOutlook::default();
        };
        let ocr = self.0.clone();
        let asked = rel.clone();
        // 0.5.2 waited at most 1 s for the pending list when serving a manifest.
        let answer = blocking(move || {
            ocr.ask_within(Duration::from_secs(1), move |s| {
                let judging = s.arrival_judging(&asked);
                (s.volume_pending(&asked, None), s.now(), judging)
            })
        });
        let busy = VolumeOutlook {
            busy: true,
            ..VolumeOutlook::default()
        };
        match answer {
            // A cloud arrival not judged yet: unknown, not "nothing owed" (`[]` makes a
            // reader take the volume as finished and stop watching it).
            Asked::Answer((pending, _, true)) if pending.is_empty() => busy,
            Asked::Answer((pending, now, _)) => {
                let recheck = bunko_sched::outlook::recheck_after(&pending, now);
                VolumeOutlook {
                    pending,
                    recheck_after: recheck.map(Value::from),
                    busy: false,
                }
            }
            // Too busy to price the volume in time: the rows it owes as the scheduler
            // last published them, without finishing times. Not `[]` for "unknown": a
            // reader told `[]` takes the volume as finished and stops watching it. Not a
            // 503 either while there is an answer: a deep link would import the volume
            // without its layers. Busy only before anything was published, or for a
            // volume that arrived after the board was built and that it lists nothing
            // for (read before the board: see `arrival_unsettled`).
            Asked::TimedOut => match (self.0.arrival_unsettled(&rel), self.0.owed_pending(&rel)) {
                (fresh, Some(pending)) if !(fresh && pending.is_empty()) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0.0, |d| d.as_secs_f64());
                    let recheck = bunko_sched::outlook::recheck_after(&pending, now);
                    VolumeOutlook {
                        pending,
                        recheck_after: recheck.map(Value::from),
                        busy: false,
                    }
                }
                _ => busy,
            },
            Asked::Gone => VolumeOutlook::default(),
        }
    }
}

impl OcrStatusSource for OcrGlue {
    /// The 0.5.2 `.ocr-progress.json` document: the first running card at the top level
    /// plus `jobs` with every card.
    /// Read from the board the scheduler publishes: never waits on the scheduler.
    fn progress(&self) -> Option<Map<String, Value>> {
        let jobs = self.0.running_jobs();
        let first = jobs.first()?.clone();
        let mut doc = first;
        doc.insert("active".into(), Value::Bool(true));
        doc.insert(
            "jobs".into(),
            Value::Array(jobs.iter().cloned().map(Value::Object).collect()),
        );
        Some(doc)
    }

    /// From the failing volumes the scheduler publishes: never waits on the scheduler.
    fn volume_failure(&self, series: &str, volume: &str) -> Option<String> {
        self.0.failing_volume(&format!("{series}/{volume}.cbz"))
    }
}

impl ArchiveEvents for OcrGlue {
    fn archive_added(&self, cbz: &Path) {
        self.0.archive_arrived(cbz);
    }
    fn archive_removed(&self, cbz: &Path) {
        self.0.archives_removed(&[cbz.to_path_buf()]);
    }
}
