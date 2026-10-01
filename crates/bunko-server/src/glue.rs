//! Adapters between the OCR scheduler and the library/catalog runtime.

use crate::library::{ArchiveEvents, OcrStatusSource, OutlookSource, VolumeOutlook};
use crate::ocr::OcrControl;
use serde_json::{Map, Value};
use std::path::Path;
use std::time::Duration;

/// Run a blocking scheduler query from wherever the caller is (async handler or blocking
/// thread) without starving a tokio worker.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
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
        // 0.5.2 waited at most 1 s for the pending list when serving a manifest.
        let answer = blocking(move || ocr.ask_blocking(Duration::from_secs(1), move |s| (s.volume_pending(&rel, None), s.now())));
        match answer {
            Some((pending, now)) => {
                let recheck = bunko_sched::outlook::recheck_after(&pending, now);
                VolumeOutlook { pending, recheck_after: recheck.map(Value::from) }
            }
            None => VolumeOutlook::default(),
        }
    }
}

impl OcrStatusSource for OcrGlue {
    /// The 0.5.2 `.ocr-progress.json` document: the first running card at the top level
    /// plus `jobs` with every card.
    fn progress(&self) -> Option<Map<String, Value>> {
        let ocr = self.0.clone();
        let jobs = blocking(move || ocr.ask_blocking(Duration::from_millis(500), |s| s.running_jobs()))?;
        let first = jobs.first()?.clone();
        let mut doc = first;
        doc.insert("active".into(), Value::Bool(true));
        doc.insert("jobs".into(), Value::Array(jobs.into_iter().map(Value::Object).collect()));
        Some(doc)
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
