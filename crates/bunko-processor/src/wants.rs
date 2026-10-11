//! The engines the library asks for (`RegisterReply.engines`), kept in the processor's
//! storage: `install-ocr --processor` and `models download` fetch only their models.
//! Missing (before the first registration, or an older library): only PP-OCR's.

use std::path::{Path, PathBuf};

use bunko_proto::EngineWant;

pub const FILE: &str = "library-engines.json";

pub fn path(storage: &Path) -> PathBuf {
    storage.join(FILE)
}

/// What the library asked for at the last registration; None when it has not said.
pub fn read(storage: &Path) -> Option<Vec<EngineWant>> {
    serde_json::from_slice(&std::fs::read(path(storage)).ok()?).ok()
}

/// Keep `wants` (written atomically). Returns whether it differs from what was kept.
pub fn write(storage: &Path, wants: &[EngineWant]) -> std::io::Result<bool> {
    if read(storage).as_deref() == Some(wants) {
        return Ok(false);
    }
    std::fs::create_dir_all(storage)?;
    let tmp = storage.join(format!(".{FILE}.tmp"));
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(wants).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, path(storage))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_reports_changes() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), None);
        let wants = vec![EngineWant {
            engine: "hayai-nova".into(),
            precision: "auto-accuracy".into(),
        }];
        assert!(write(dir.path(), &wants).unwrap());
        assert!(!write(dir.path(), &wants).unwrap());
        assert_eq!(read(dir.path()), Some(wants));
        assert!(write(dir.path(), &[]).unwrap());
        assert_eq!(read(dir.path()), Some(vec![]));
    }
}
