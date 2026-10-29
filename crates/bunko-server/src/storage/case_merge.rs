//! [`ServerCaseMergePolicy`]: what the automatic case merge (spec §5.9) asks the
//! server. Which of two differing main sidecars matches the primary row's recipe
//! (the upgrade's `classify` + `Recipe::matches`, never `Verdict::Current`), whether
//! OCR work touches a variant (`ocr::busy`), which sets are the reader's own (the
//! per-user tree, never merged), and the audit record of a finished merge.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bunko_db::{AuditDetails, Database, NewAuditEvent};
use bunko_remote::{CaseMergePolicy, MergeChoice, MergeRecord};

use crate::core::Core;

/// The server's [`CaseMergePolicy`] (installed with the cloud hooks).
pub struct ServerCaseMergePolicy {
    library: PathBuf,
    core: Core,
    db: Arc<Database>,
}

impl ServerCaseMergePolicy {
    pub fn new(library: PathBuf, core: Core, db: Arc<Database>) -> Self {
        ServerCaseMergePolicy { library, core, db }
    }

    fn row_of(&self, sidecar: &Path) -> Option<bunko_db::OcrSidecar> {
        let rel = crate::ocr::types::rel_of(&self.library, sidecar)?;
        self.db.get_ocr_sidecar(&rel).ok().flatten()
    }
}

impl CaseMergePolicy for ServerCaseMergePolicy {
    fn prefer_main(&self, a: &Path, b: &Path) -> MergeChoice {
        let (enabled, rows) = {
            let c = self.core.config.read();
            (c.ocr.upgrade.enabled, c.ocr.generations.clone())
        };
        if !enabled {
            return MergeChoice::Block;
        }
        let Some(primary) = bunko_core::generations::enabled_generations(&rows).find(|r| r.primary)
        else {
            return MergeChoice::Block;
        };
        let target = primary.output_affecting();
        let matches =
            |p: &Path| crate::ocr::upgrade::classify(p, self.row_of(p).as_ref()).matches(&target);
        match (matches(a), matches(b)) {
            (true, false) => MergeChoice::A,
            (false, true) => MergeChoice::B,
            _ => MergeChoice::Block,
        }
    }

    /// The cloud root's per-user names (its owner's `history/`, root files) and
    /// everything inside `history/` are the reader's, not library spellings: a
    /// `History/` series is never merged with, or blocked against, them.
    fn excluded(&self, dir: &[String], key: &str) -> bool {
        match dir.first() {
            None => bunko_core::reader_tree::folds_to_per_user_root_name(key),
            Some(top) => top == bunko_core::reader_tree::HISTORY_DIR,
        }
    }

    fn busy(&self, path: &Path) -> bool {
        crate::ocr::busy::busy_under(path)
    }

    fn audit(&self, m: &MergeRecord) {
        let event = NewAuditEvent::new("library_case_merged")
            .target_type("folder")
            .target_path(&m.winner)
            .details(
                AuditDetails::new()
                    .with("merged", m.losers.join(", "))
                    .with("origin", m.origin.clone())
                    .with("moved", m.moved)
                    .with("trashed", m.trashed)
                    .with("blocked", m.blocked),
            );
        if let Err(e) = self.db.log_audit_event(&event) {
            tracing::warn!("could not audit a case merge: {e}");
        }
    }
}
