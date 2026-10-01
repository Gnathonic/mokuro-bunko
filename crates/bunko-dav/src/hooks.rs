//! What the DAV layer tells the rest of the server, after each write has committed.
//!
//! The server implements [`DavHooks`] on top of the database (ownership, OCR provenance,
//! audit) and the OCR queue. Every method has a no-op default so a server (or a test)
//! implements only what it has. The DB-shaped methods are named after the 0.5.2
//! `Database` calls they replace (spec §8.6) so the mapping stays one-to-one.
//!
//! **Timing:** every hook runs after the filesystem change it reports has happened
//! (0.5.2 ran the DB calls in the same order, inside the request). Hooks are called from a
//! blocking thread (`spawn_blocking`), so synchronous SQLite work is fine; they must not
//! panic, and they cannot fail the request: a database error after the file is in place is
//! the server's to log (spec 14.14: never answer 500 after commit).

use std::path::{Path, PathBuf};

/// One audit row (0.5.2 `Database.log_audit_event`).
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEvent {
    /// `upload`, `edit`, `delete`, `move`, `copy`, `mkdir`, `lock_conflict`.
    pub action: &'static str,
    pub actor: Option<String>,
    /// `library`, `progress`, `webdav`, `library_folder`, `webdav_folder`.
    pub target_type: &'static str,
    pub target_path: String,
    /// The `details` object (0.5.2 serialises it compact, ASCII-escaped).
    pub details: Option<serde_json::Value>,
}

/// Follow-up headers of a successful `.cbz` PUT (spec §9): the volume's manifest URL and
/// when to look again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutFollowUp {
    /// `X-Mokuro-Manifest`, e.g. `/catalog/api/manifest?series=S&volume=V1`.
    pub manifest: String,
    /// `X-Mokuro-Recheck-After`, whole seconds.
    pub recheck_after: u64,
}

/// Library-relative paths (`rel`) use `/` separators and no leading slash, exactly the
/// `volume_uploads.volume_key` form (`Series/Vol 1.cbz`).
pub trait DavHooks: Send + Sync + 'static {
    fn audit(&self, _event: AuditEvent) {}

    /// A PUT committed a library file. 0.5.2 calls this for every library file; the DB
    /// only creates a row for a `.cbz` (sidecars update `last_modified_*` of an existing
    /// row).
    fn record_volume_upload(&self, _rel: &str, _actor: &str, _existed_before: bool) {}
    /// A library `.cbz` was deleted.
    fn forget_volume_upload(&self, _rel: &str) {}
    /// A library `.cbz` was deleted: a new upload under its name is a new volume.
    fn forget_volume_uuid(&self, _rel: &str) {}
    /// A library `.cbz` left (deleted or moved away): its OCR provenance rows go.
    fn forget_ocr_sidecars_of_volume(&self, _rel: &str) {}
    /// A `.mokuro`/`.mokuro.gz` was written, deleted or moved over WebDAV.
    fn forget_ocr_sidecar(&self, _rel: &str) {}
    /// A library file was moved: ownership follows it (upsert).
    fn rename_volume_upload(&self, _old_rel: &str, _new_rel: &str) {}
    /// A library folder was deleted (or overwritten by a folder MOVE).
    fn forget_volume_uploads_under_prefix(&self, _prefix: &str) {}
    fn forget_ocr_sidecars_under_prefix(&self, _prefix: &str) {}
    fn forget_volume_uuids_under_prefix(&self, _prefix: &str) {}
    /// A library folder was moved inside the library (its sidecars moved with it).
    fn rename_ocr_sidecars_under_prefix(&self, _old_prefix: &str, _new_prefix: &str) {}
    fn rename_volume_uuids_under_prefix(&self, _old_prefix: &str, _new_prefix: &str) {}
    /// A volume's primary sidecar (`<stem>.mokuro[.gz]` with `<stem>.cbz` beside it) is
    /// about to be deleted or moved away (0.5.2 `_remember_primary_uuid`). Called BEFORE
    /// the file goes, so the server can read the volume id out of it.
    fn primary_sidecar_leaving(&self, _rel: &str, _sidecar: &Path) {}

    /// Library paths (a `.cbz`, or a whole folder) a successful DELETE or MOVE took away:
    /// cancel their OCR.
    fn archives_removed(&self, _paths: &[PathBuf]) {}
    /// A library `.cbz` is now in place (PUT, or MOVE/COPY into place): queue its OCR.
    fn archive_arrived(&self, _cbz: &Path) {}
    /// After `archive_arrived` for a PUT only: the follow-up headers, if any OCR is owed.
    /// `series` is the archive's parent relative to the library (`.` for a loose root
    /// file), `volume` its stem.
    fn put_follow_up(&self, _cbz: &Path, _series: &str, _volume: &str) -> Option<PutFollowUp> {
        None
    }
    /// Something under the library changed through DAV (physical paths). The PROPFIND
    /// cache is already invalidated; this is for the library index / metadata scheduler.
    fn library_changed(&self, _paths: &[PathBuf]) {}
}

/// Hooks that do nothing (tests, or a server with no database attached).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoHooks;

impl DavHooks for NoHooks {}
