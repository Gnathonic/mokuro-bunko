//! [`ServerCloudHooks`]: the DB hooks a change made directly in the cloud runs (D20,
//! spec §5.4) — the same ones a WebDAV MOVE, DELETE or PUT runs (`bunko-dav`
//! `methods/write.rs`, `methods/mod.rs`), through the server's [`ServerDavHooks`] so
//! the OCR queue and the library runtime hear about them too.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bunko_dav::DavHooks;
use bunko_db::{AuditDetails, Database, NewAuditEvent};
use bunko_library::compat::normalize_volume_title_key;
use bunko_library::store::MetadataStore;
use bunko_remote::{CloudHooks, SubmissionResult};

use crate::davhooks::ServerDavHooks;
use crate::library::LibraryRuntime;

/// The audit reason of an OCR result dropped because the reader wrote a sidecar first.
pub const DISCARDED_REASON: &str = "discarded: a sidecar appeared in the cloud";

/// The server's [`CloudHooks`] (installed on the store once the services exist).
pub struct ServerCloudHooks {
    root: PathBuf,
    dav: Arc<ServerDavHooks>,
    library: Arc<LibraryRuntime>,
    db: Arc<Database>,
}

fn lower(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn is_cbz(p: &Path) -> bool {
    lower(p).ends_with(".cbz")
}

fn is_sidecar(p: &Path) -> bool {
    let n = lower(p);
    n.ends_with(".mokuro") || n.ends_with(".mokuro.gz")
}

/// Every `.cbz` below `dir`, through the store (index reads on a remote store).
fn cbz_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = bunko_store::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(_) if is_cbz(&p) => out.push(p),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

impl ServerCloudHooks {
    pub fn new(
        root: PathBuf,
        dav: Arc<ServerDavHooks>,
        library: Arc<LibraryRuntime>,
        db: Arc<Database>,
    ) -> Self {
        ServerCloudHooks {
            root,
            dav,
            library,
            db,
        }
    }

    /// The `volume_uploads.volume_key` form of `p` (`Series/Vol 1.cbz`).
    fn rel(&self, p: &Path) -> Option<String> {
        crate::ocr::types::rel_of(&self.root, p)
    }

    /// 0.5.2 `_forget_ocr_records` (bunko-dav `forget_ocr_records`).
    fn forget_ocr_records(&self, p: &Path, rel: &str, archive_too: bool) {
        if is_cbz(p) {
            if archive_too {
                self.dav.forget_ocr_sidecars_of_volume(rel);
            }
        } else if is_sidecar(p) {
            self.dav.forget_ocr_sidecar(rel);
        }
    }
}

impl CloudHooks for ServerCloudHooks {
    fn moved(&self, from: &Path, to: &Path, is_dir: bool) {
        tracing::info!(
            "[STORAGE] Moved in the cloud: {} -> {}",
            from.display(),
            to.display()
        );
        let (old, new) = (self.rel(from), self.rel(to));
        if is_dir {
            // bunko-dav `move_folder`: ownership per volume, then the prefixes.
            let volumes = cbz_under(to);
            if let (Some(old), Some(new)) = (old.as_deref(), new.as_deref()) {
                for cbz in &volumes {
                    let Some(new_rel) = self.rel(cbz) else {
                        continue;
                    };
                    let suffix = new_rel[new.len().min(new_rel.len())..].trim_start_matches('/');
                    let old_rel = if suffix.is_empty() {
                        old.to_string()
                    } else {
                        format!("{old}/{suffix}")
                    };
                    self.dav.rename_volume_upload(&old_rel, &new_rel);
                }
                self.dav.rename_ocr_sidecars_under_prefix(old, new);
                self.dav.rename_volume_uuids_under_prefix(old, new);
            } else if let Some(old) = old.as_deref() {
                self.dav.forget_ocr_sidecars_under_prefix(old);
                self.dav.forget_volume_uuids_under_prefix(old);
            }
            self.dav.archives_removed(&[from.to_path_buf()]);
            for cbz in &volumes {
                self.dav.archive_arrived(cbz);
            }
        } else {
            // bunko-dav `move_file`.
            if let (Some(old), Some(new)) = (old.as_deref(), new.as_deref()) {
                self.dav.rename_volume_upload(old, new);
            }
            if let Some(old) = old.as_deref() {
                self.forget_ocr_records(from, old, true);
            }
            if let Some(new) = new.as_deref() {
                self.forget_ocr_records(to, new, false);
            }
            if is_cbz(from) {
                self.dav.archives_removed(&[from.to_path_buf()]);
            }
            if is_cbz(to) {
                self.dav.archive_arrived(to);
            }
        }
        self.dav
            .library_changed(&[from.to_path_buf(), to.to_path_buf()]);
    }

    fn removed(&self, path: &Path, is_dir: bool) {
        tracing::info!("[STORAGE] Deleted in the cloud: {}", path.display());
        let Some(rel) = self.rel(path) else {
            return;
        };
        if is_dir {
            // bunko-dav `delete_resource` (folder).
            self.dav.forget_volume_uploads_under_prefix(&rel);
            self.dav.forget_ocr_sidecars_under_prefix(&rel);
            self.dav.forget_volume_uuids_under_prefix(&rel);
            self.dav.archives_removed(&[path.to_path_buf()]);
        } else if is_cbz(path) {
            // bunko-dav `delete_file` (archive).
            self.dav.forget_volume_upload(&rel);
            self.dav.forget_volume_uuid(&rel);
            self.dav.forget_ocr_sidecars_of_volume(&rel);
            self.dav.archives_removed(&[path.to_path_buf()]);
        } else if is_sidecar(path) {
            // The file is gone from the index by now, so its volume id cannot be read
            // back (`primary_sidecar_leaving` needs the bytes); the reader's replacement
            // carries the same `volume_uuid`.
            self.dav.forget_ocr_sidecar(&rel);
        }
        self.dav.library_changed(&[path.to_path_buf()]);
    }

    fn sidecar_overwritten(&self, path: &Path) {
        // As a DAV PUT of a sidecar does: the provenance row no longer describes it.
        if let Some(rel) = self.rel(path) {
            self.dav.forget_ocr_sidecar(&rel);
        }
    }

    fn series_submission(&self, series_dir: &Path, bytes: &[u8]) -> SubmissionResult {
        let Some(title) = series_dir.file_name().map(|n| n.to_string_lossy()) else {
            return SubmissionResult::Refused("not a series folder".into());
        };
        match self
            .library
            .service()
            .apply_series_update(&title, bytes, Some("cloud"))
        {
            Ok(true) => SubmissionResult::Applied,
            Ok(false) => SubmissionResult::Refused("the series.json was not accepted".into()),
            Err(e) => SubmissionResult::Refused(e.to_string()),
        }
    }

    fn ocr_discarded(&self, path: &Path) {
        let target = self
            .rel(path)
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let event = NewAuditEvent::new("ocr_sidecar_discarded")
            .target_type("sidecar")
            .target_path(&target)
            .details(AuditDetails::new().with("reason", DISCARDED_REASON));
        if let Err(e) = self.db.log_audit_event(&event) {
            tracing::warn!("could not audit a discarded OCR result: {e}");
        }
    }

    fn has_series_facts(&self, series_dir: &Path) -> bool {
        let Some(title) = series_dir.file_name().map(|n| n.to_string_lossy()) else {
            return true;
        };
        let key = normalize_volume_title_key(&title);
        // An error reads as "has facts": nothing is re-ingested on a DB hiccup.
        self.library
            .store()
            .series_facts(&key)
            .map_or(true, |row| row.is_some())
    }
}
