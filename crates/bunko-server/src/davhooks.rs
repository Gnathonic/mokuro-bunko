//! `DavHooks` for the running server: database bookkeeping (audit, upload ownership,
//! OCR provenance, volume identities) plus fan-out of library events to the modules that
//! care (OCR queue, metadata/index runtime).

use bunko_dav::{AuditEvent, DavHooks, PutFollowUp};
use bunko_db::{AuditDetails, Database, NewAuditEvent};
use parking_lot::RwLock;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::warn;

pub struct ServerDavHooks {
    db: Arc<Database>,
    /// Non-DB listeners (OCR control, library runtime), added as modules start.
    listeners: RwLock<Vec<Arc<dyn DavHooks>>>,
}

impl ServerDavHooks {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db, listeners: RwLock::new(Vec::new()) }
    }

    pub fn add_listener(&self, l: Arc<dyn DavHooks>) {
        self.listeners.write().push(l);
    }

    fn log<T, E: std::fmt::Display>(what: &str, r: Result<T, E>) {
        if let Err(e) = r {
            warn!("{what} failed after a WebDAV write: {e}");
        }
    }

    fn each(&self, f: impl Fn(&dyn DavHooks)) {
        for l in self.listeners.read().iter() {
            f(l.as_ref());
        }
    }
}

/// `volume_uuid` of a primary sidecar (plain or gzip), if readable.
fn sidecar_volume_uuid(path: &Path) -> Option<String> {
    let raw = std::fs::read(path).ok()?;
    let bytes = if path.extension().is_some_and(|e| e == "gz") {
        let mut out = Vec::new();
        flate2::read::MultiGzDecoder::new(&raw[..]).read_to_end(&mut out).ok()?;
        out
    } else {
        raw
    };
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("volume_uuid").and_then(|u| u.as_str()).map(str::to_string).filter(|s| !s.trim().is_empty())
}

impl DavHooks for ServerDavHooks {
    fn audit(&self, e: AuditEvent) {
        let details = match e.details {
            Some(serde_json::Value::Object(m)) => Some(AuditDetails::from(m)),
            _ => None,
        };
        let mut ev = NewAuditEvent::new(e.action).actor(e.actor.as_deref()).target_type(e.target_type).target_path(&e.target_path);
        ev.details = details;
        Self::log("audit", self.db.log_audit_event(&ev));
    }

    fn record_volume_upload(&self, rel: &str, actor: &str, _existed_before: bool) {
        Self::log("record_volume_upload", self.db.record_volume_upload(rel, actor));
    }
    fn forget_volume_upload(&self, rel: &str) {
        Self::log("forget_volume_upload", self.db.forget_volume_upload(rel));
    }
    fn forget_volume_uuid(&self, rel: &str) {
        Self::log("forget_volume_uuid", self.db.forget_volume_uuid(rel));
    }
    fn forget_ocr_sidecars_of_volume(&self, rel: &str) {
        Self::log("forget_ocr_sidecars_of_volume", self.db.forget_ocr_sidecars_of_volume(rel));
    }
    fn forget_ocr_sidecar(&self, rel: &str) {
        Self::log("forget_ocr_sidecar", self.db.forget_ocr_sidecar(rel));
    }
    fn rename_volume_upload(&self, old: &str, new: &str) {
        Self::log("rename_volume_upload", self.db.rename_volume_upload(old, new));
    }
    fn forget_volume_uploads_under_prefix(&self, p: &str) {
        Self::log("forget_volume_uploads_under_prefix", self.db.forget_volume_uploads_under_prefix(p));
    }
    fn forget_ocr_sidecars_under_prefix(&self, p: &str) {
        Self::log("forget_ocr_sidecars_under_prefix", self.db.forget_ocr_sidecars_under_prefix(p));
    }
    fn forget_volume_uuids_under_prefix(&self, p: &str) {
        Self::log("forget_volume_uuids_under_prefix", self.db.forget_volume_uuids_under_prefix(p));
    }
    fn rename_ocr_sidecars_under_prefix(&self, old: &str, new: &str) {
        Self::log("rename_ocr_sidecars_under_prefix", self.db.rename_ocr_sidecars_under_prefix(old, new));
    }
    fn rename_volume_uuids_under_prefix(&self, old: &str, new: &str) {
        Self::log("rename_volume_uuids_under_prefix", self.db.rename_volume_uuids_under_prefix(old, new));
    }

    fn primary_sidecar_leaving(&self, rel: &str, sidecar: &Path) {
        // 0.5.2 `_remember_primary_uuid`: a re-uploaded volume keeps its id (and reading
        // progress) even though its sidecar went away first.
        if let Some(uuid) = sidecar_volume_uuid(sidecar) {
            Self::log("remember_volume_uuid", self.db.remember_volume_uuid(rel, &uuid));
        }
        self.each(|l| l.primary_sidecar_leaving(rel, sidecar));
    }

    fn archives_removed(&self, paths: &[PathBuf]) {
        self.each(|l| l.archives_removed(paths));
    }
    fn archive_arrived(&self, cbz: &Path) {
        self.each(|l| l.archive_arrived(cbz));
    }
    fn put_follow_up(&self, cbz: &Path, series: &str, volume: &str) -> Option<PutFollowUp> {
        self.listeners.read().iter().find_map(|l| l.put_follow_up(cbz, series, volume))
    }
    fn library_changed(&self, paths: &[PathBuf]) {
        self.each(|l| l.library_changed(paths));
    }
}
