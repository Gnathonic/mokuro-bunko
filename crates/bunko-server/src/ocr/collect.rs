//! Installing a finished sidecar (spec ocr-scheduling §11.2, OCR-WORKER "Collection").
//!
//! Runs on a helper thread, never on the scheduler: archive-still-current check, the
//! result's sha256, JSON validity, normalisation (title, volume, uuids, `ocr_engine`
//! stamp), the move beside the archive under the DAV write lock (or the generation
//! upgrade's swap), provenance, audit, metadata recompile. Nothing here may fail an
//! OCR result for a database error: those are logged and swallowed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bunko_core::generations::Generation;
use bunko_db::{AuditDetails, Database, NewAuditEvent, OcrSidecar};
use serde_json::{Map, Value};
use sha2::Digest;

use super::owed::sidecar_paths;
use super::types::{Job, LibraryFacts, PathLocks, Stamp, stamp_of};

/// Everything one installation needs, owned (it crosses to a helper thread).
pub struct CollectRequest {
    pub job: Job,
    pub row: Generation,
    pub rows: Vec<Generation>,
    pub library: PathBuf,
    pub inbox: PathBuf,
    /// The finished sidecar on disk (`<storage>/.processing/<sid>/<claim>/<name>`).
    pub result: PathBuf,
    /// What the processor announced (`volume_done.sidecar_sha256`, or the upload's own).
    pub expected_sha256: Option<String>,
    /// The archive as the claim / the processor's verified download pinned it.
    pub stamp: Option<Stamp>,
    pub machine: String,
    pub account: Option<String>,
    pub runner_build: Option<String>,
    pub pages: Option<i64>,
    pub failed_pages: Option<i64>,
    pub generator: String,
    pub db: Option<Arc<Database>>,
    pub facts: Arc<dyn LibraryFacts>,
    pub locks: PathLocks,
    pub sid: String,
    pub claim: String,
    pub congestion: Option<Map<String, Value>>,
    pub upgrade: Option<Arc<super::upgrade::Upgrade>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Installed at this path.
    Installed(PathBuf),
    /// A real failure of the volume (bad JSON, no sidecar, move failed).
    Failed(String),
    /// The archive changed under the claim: give the job back unrecorded.
    Discarded,
    /// The destination is locked by a WebDAV write: retry the job this scan.
    Busy(String),
}

#[derive(Debug)]
pub struct CollectDone {
    pub job: Job,
    pub sid: String,
    pub claim: String,
    pub outcome: Outcome,
    pub congestion: Option<Map<String, Value>>,
}

/// `/mokuro-reader/<rel>` of a library path (the audit target).
fn target_of(library: &Path, path: &Path) -> String {
    match crate::ocr::types::rel_of(library, path) {
        Some(rel) => format!("{}{rel}", bunko_proto::ARCHIVES_ROOT),
        None => path.to_string_lossy().into_owned(),
    }
}

pub fn audit_rejected(req: &CollectRequest, reason: &str) {
    let Some(db) = &req.db else { return };
    let cbz = req.job.path(&req.library);
    let (plain, _) = sidecar_paths(&cbz, &req.row.sidecar_suffix());
    let target = target_of(&req.library, &plain);
    let details = AuditDetails::new()
        .with("generation", req.row.name.clone())
        .with("generation_id", req.row.id.clone())
        .with("machine", req.machine.clone())
        .with("engine", req.row.engine.clone())
        .with("reason", reason.chars().take(500).collect::<String>());
    let event = NewAuditEvent::new("ocr_sidecar_rejected")
        .actor(req.account.as_deref())
        .target_type("sidecar")
        .target_path(&target)
        .details(details);
    if let Err(e) = db.log_audit_event(&event) {
        tracing::warn!("could not audit a rejected sidecar: {e}");
    }
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn sidecar_volume_uuid(path: &Path) -> Option<String> {
    let loaded = bunko_library::sidecar::load_sidecar(path);
    loaded
        .data?
        .get("volume_uuid")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `volume_uuid_for(cbz, row)`: the primary's, the remembered one, the oldest layer's,
/// else the reader's deterministic id of `<Series>/<Volume>`.
pub fn volume_uuid_for(
    cbz: &Path,
    row: &Generation,
    rows: &[Generation],
    library: &Path,
    inbox: &Path,
    db: Option<&Database>,
) -> String {
    if !row.primary {
        for suffix in [".mokuro", ".mokuro.gz"] {
            if let Some(found) =
                sidecar_volume_uuid(&bunko_library::sidecar::with_suffix(cbz, suffix))
            {
                return found;
            }
        }
    }
    if let (Some(db), Some(rel)) = (db, crate::ocr::types::rel_of(library, cbz))
        && let Ok(Some(found)) = db.remembered_volume_uuid(&rel)
    {
        return found;
    }
    let mut found: Vec<(f64, String)> = Vec::new();
    for other in rows {
        if other.primary || other.id == row.id {
            continue;
        }
        let (plain, gz) = sidecar_paths(cbz, &other.sidecar_suffix());
        for candidate in [plain, gz] {
            let Some(uuid) = sidecar_volume_uuid(&candidate) else {
                continue;
            };
            let Some(mtime) = std::fs::metadata(&candidate)
                .ok()
                .and_then(|m| m.modified().ok())
            else {
                continue;
            };
            let secs = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |d| d.as_secs_f64());
            found.push((secs, uuid));
        }
    }
    found.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    if let Some((_, uuid)) = found.into_iter().next() {
        return uuid;
    }
    let series = bunko_layout::sidecar::derive_series_name(cbz, library, inbox);
    let stem = cbz
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    bunko_layout::sidecar::deterministic_uuid(&format!("{series}/{stem}"))
}

/// `_get_unique_path`: `<stem>_<n><suffix>` (Python `Path.stem` / `Path.suffix`).
pub fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (stem, suffix) = match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.clone(), String::new()),
    };
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut n = 1;
    loop {
        let candidate = parent.join(format!("{stem}_{n}{suffix}"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Move `from` to `to`: a rename, or a copy into a temp file beside `to` then a rename
/// when they are on different filesystems.
pub fn move_into_place(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            let tmp = to.with_file_name(format!(
                ".{}.part",
                to.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ));
            std::fs::copy(from, &tmp)?;
            std::fs::rename(&tmp, to)?;
            let _ = std::fs::remove_file(from);
            Ok(())
        }
    }
}

/// Try the DAV write lock for a while: a write in flight is short.
pub fn lock_with_patience(locks: &PathLocks, path: &Path) -> Option<Box<dyn Send>> {
    for _ in 0..50 {
        if let Some(g) = locks.try_lock(path) {
            return Some(g);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// Install one result.
pub fn run(req: CollectRequest) -> CollectDone {
    let outcome = install(&req);
    match &outcome {
        Outcome::Installed(_) => {}
        Outcome::Discarded => audit_rejected(&req, super::sched::DISCARDED),
        Outcome::Failed(reason) | Outcome::Busy(reason) => audit_rejected(&req, reason),
    }
    if !matches!(outcome, Outcome::Installed(_)) {
        let _ = std::fs::remove_file(&req.result);
    }
    if let Some(dir) = req.result.parent() {
        let _ = std::fs::remove_dir_all(dir);
        if let Some(session_dir) = dir.parent() {
            let _ = std::fs::remove_dir(session_dir);
        }
    }
    CollectDone {
        job: req.job.clone(),
        sid: req.sid.clone(),
        claim: req.claim.clone(),
        outcome,
        congestion: req.congestion.clone(),
    }
}

fn install(req: &CollectRequest) -> Outcome {
    let cbz = req.job.path(&req.library);
    // 1. The archive is still the one the claim read.
    let current = match req.stamp {
        Some(stamp) => stamp_of(&cbz) == Some(stamp),
        None => cbz.is_file(),
    };
    if !current {
        return Outcome::Discarded;
    }
    let row = &req.row;
    // 2. The bytes are the ones announced.
    if !req.result.is_file() {
        return Outcome::Failed(format!("no valid {} sidecar generated", row.name));
    }
    if let Some(expected) = &req.expected_sha256 {
        match sha256_file(&req.result) {
            Ok(actual) if actual.eq_ignore_ascii_case(expected) => {}
            Ok(_) => {
                return Outcome::Failed(format!(
                    "the {} sidecar that arrived does not match the sha256 its processor announced",
                    row.name
                ));
            }
            Err(e) => {
                return Outcome::Failed(format!("the {} sidecar could not be read: {e}", row.name));
            }
        }
    }
    // 3. Facts before normalisation (the runner's own `ocr_engine` block).
    let loaded = bunko_library::sidecar::load_sidecar(&req.result);
    let Some(data) = loaded.data else {
        return Outcome::Failed(format!(
            "the {} sidecar it wrote is not readable JSON",
            row.name
        ));
    };
    let facts_pages = data
        .get("pages")
        .and_then(|p| p.as_array())
        .map(|a| a.len() as i64);
    let engine_block = data.get("ocr_engine").and_then(|b| b.as_object());
    let block_str = |k: &str| {
        engine_block
            .and_then(|b| b.get(k))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let detector = block_str("detector").or_else(|| Some(row.effective_detector().to_string()));
    let precision =
        block_str("precision").or_else(|| row.precision_applies().then(|| row.precision.clone()));
    drop(data);
    // 4. Normalise.
    let series_name = bunko_layout::sidecar::derive_series_name(&cbz, &req.library, &req.inbox);
    let volume_uuid = volume_uuid_for(
        &cbz,
        row,
        &req.rows,
        &req.library,
        &req.inbox,
        req.db.as_deref(),
    );
    let stamp = (!row.primary).then(|| bunko_layout::sidecar::LayerStamp {
        engine: row.engine.clone(),
        generator: req.generator.clone(),
        generation: row.name.clone(),
    });
    let norm = bunko_layout::sidecar::Normalization {
        series_name,
        volume: req.job.volume().to_string(),
        volume_uuid,
        stamp,
    };
    if let Err(e) = bunko_layout::sidecar::normalize_sidecar_file(&req.result, &norm) {
        return Outcome::Failed(format!(
            "the {} sidecar it wrote is not readable JSON ({e})",
            row.name
        ));
    }
    // 5. Into place, under the DAV write lock.
    let destination = if req.job.upgrade {
        match super::upgrade::swap_in_generated(req, &cbz) {
            Ok(path) => path,
            Err(outcome) => return outcome,
        }
    } else {
        let (plain, gz) = sidecar_paths(&cbz, &row.sidecar_suffix());
        let Some(_guard) = lock_with_patience(&req.locks, &plain) else {
            return Outcome::Busy(format!("{} is locked by a WebDAV write", plain.display()));
        };
        let dest = if plain.exists() || gz.exists() {
            unique_path(&plain)
        } else {
            plain
        };
        if let Err(e) = move_into_place(&req.result, &dest) {
            return Outcome::Failed(format!(
                "could not move the {} sidecar to {}: {e}",
                row.name,
                dest.display()
            ));
        }
        dest
    };
    // 6. Provenance + audit; 7. metadata.
    record_written(req, &cbz, &destination, detector, precision, facts_pages);
    req.facts.sidecar_installed(&cbz);
    Outcome::Installed(destination)
}

fn record_written(
    req: &CollectRequest,
    cbz: &Path,
    sidecar: &Path,
    detector: Option<String>,
    precision: Option<String>,
    facts_pages: Option<i64>,
) {
    let Some(db) = &req.db else { return };
    let (Some(sidecar_rel), Some(volume_key)) = (
        crate::ocr::types::rel_of(&req.library, sidecar),
        crate::ocr::types::rel_of(&req.library, cbz),
    ) else {
        return;
    };
    let archive = stamp_of(cbz);
    let pages = req.pages.or(facts_pages);
    let row = OcrSidecar {
        sidecar_path: sidecar_rel,
        volume_key,
        generation_id: req.row.id.clone(),
        generation_name: req.row.name.clone(),
        machine: req.machine.clone(),
        account: req.account.clone(),
        engine: Some(req.row.engine.clone()),
        detector: detector.clone(),
        precision: precision.clone(),
        runner_build: req.runner_build.clone(),
        pages,
        failed_pages: req.failed_pages,
        archive_size: archive.map(|a| a.0 as i64),
        archive_mtime_ns: archive.map(|a| a.1 as i64),
        written_at: String::new(),
    };
    if let Err(e) = db.record_ocr_sidecar(&row) {
        tracing::warn!(
            "could not record the provenance of {}: {e}",
            sidecar.display()
        );
    }
    let target = target_of(&req.library, sidecar);
    let details = AuditDetails::new()
        .with("generation", req.row.name.clone())
        .with("generation_id", req.row.id.clone())
        .with("machine", req.machine.clone())
        .with("engine", req.row.engine.clone())
        .with("detector", detector.map_or(Value::Null, Value::String))
        .with("precision", precision.map_or(Value::Null, Value::String))
        .with("pages", pages.map_or(Value::Null, Value::from))
        .with(
            "failed_pages",
            req.failed_pages.map_or(Value::Null, Value::from),
        )
        .with(
            "runner_build",
            req.runner_build.clone().map_or(Value::Null, Value::String),
        );
    let event = NewAuditEvent::new("ocr_sidecar_written")
        .actor(req.account.as_deref())
        .target_type("sidecar")
        .target_path(&target)
        .details(details);
    if let Err(e) = db.log_audit_event(&event) {
        tracing::warn!("could not audit {}: {e}", sidecar.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_names() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("Vol.mokuro");
        assert_eq!(unique_path(&p), p);
        std::fs::write(&p, b"x").unwrap();
        assert_eq!(unique_path(&p), dir.path().join("Vol_1.mokuro"));
        let layer = dir.path().join("Vol.fast.mokuro");
        std::fs::write(&layer, b"x").unwrap();
        assert_eq!(unique_path(&layer), dir.path().join("Vol.fast_1.mokuro"));
    }
}
