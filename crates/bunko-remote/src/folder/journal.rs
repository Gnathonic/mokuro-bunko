//! The case-rename journal (`<state_dir>/folder-journal.json`). A case-only rename
//! (`Series` → `series`) goes through a temporary name, on every host: a filesystem
//! that folds case cannot rename a name onto its own other spelling in one step
//! everywhere. Between the two renames the entry exists only under the temporary name,
//! so the step is written down first and finished at the next connect after a crash.

use std::path::Path;

use serde::{Deserialize, Serialize};

pub(crate) const JOURNAL_FILE: &str = "folder-journal.json";

/// One case-only rename in flight: pids relative to the library folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub temp: String,
    pub from: String,
    pub to: String,
}

pub(crate) fn read(state_dir: &Path) -> Vec<Entry> {
    std::fs::read(state_dir.join(JOURNAL_FILE))
        .ok()
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default()
}

/// Write `entries` durably (an empty list clears the journal).
pub(crate) fn write(state_dir: &Path, entries: &[Entry]) -> std::io::Result<()> {
    use std::io::Write;
    let path = state_dir.join(JOURNAL_FILE);
    if entries.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(state_dir)?;
    let data = serde_json::to_vec(entries).map_err(std::io::Error::other)?;
    let tmp = state_dir.join(format!("{JOURNAL_FILE}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    super::fsops::fsync_dir(state_dir);
    Ok(())
}
