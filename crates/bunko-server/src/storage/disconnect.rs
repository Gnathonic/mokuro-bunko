//! Disconnect (0.8 design §7): leave a cloud library without losing anything.
//!
//! 1. Drain the outbox. When results are still owed (not yet uploaded, FAILED, or
//!    foreign `series.json` submissions not yet applied), nothing happens and the
//!    count comes back ("37 results not yet uploaded") with the choices **Wait**
//!    (drain longer), **Export** (copy their bytes to a folder) and **Discard**.
//! 2. Write `state: "released"` into this install's marker, so readers resume their
//!    own metadata writes at once.
//! 3. Delete the store's credentials and mark its directory (index, state, caches,
//!    spool) for removal at the next start, when nothing has it open. An
//!    `.upgrade-originals/` folder in it is kept.
//!
//! Disconnect never deletes anything in the cloud: the `pre-upgrade` layers D4 keeps
//! are files of the cloud library and stay there. Google is never revoked.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bunko_remote::auth::CredentialStore;
use serde::Serialize;
use serde_json::{Value, json};

use super::StoreHandle;
use super::connect::{DISCONNECTED_MARK, mark_disconnected};

/// How long releasing the lease may take.
pub const RELEASE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a plain Disconnect waits for the outbox to drain before it reports what
/// is still owed.
pub const DEFAULT_DRAIN: Duration = Duration::from_secs(20);

/// What a Disconnect does about results that are still owed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectMode {
    /// Wait up to this long for the outbox to drain; refuse when it does not.
    Drain(Duration),
    /// Copy the owed results to this folder, then go on.
    Export(PathBuf),
    /// Drop them.
    Discard,
}

impl DisconnectMode {
    /// `{"mode": "wait"|"export"|"discard", "wait_s": .., "folder": ..}` as the pages
    /// send it.
    pub fn from_json(v: &Value) -> Result<DisconnectMode, String> {
        match v.get("mode").and_then(Value::as_str).unwrap_or("wait") {
            "wait" | "drain" => Ok(DisconnectMode::Drain(Duration::from_secs(
                v.get("wait_s")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_DRAIN.as_secs())
                    .min(3600),
            ))),
            "export" => {
                let folder = v
                    .get("folder")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|f| !f.is_empty())
                    .ok_or("Export needs a folder on the server")?;
                let p = bunko_core::storage::expand_user(Path::new(folder));
                if !p.is_absolute() {
                    return Err("the export folder: give its full path".into());
                }
                Ok(DisconnectMode::Export(p))
            }
            "discard" => Ok(DisconnectMode::Discard),
            other => Err(format!("unknown mode {other:?}: wait, export or discard")),
        }
    }
}

/// What a store still owes the cloud.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Owed {
    /// Ops not yet committed (staged, held, uploading).
    pub pending: usize,
    /// Ops that gave up (FAILED).
    pub failed: usize,
    /// Foreign `series.json`/`catalog.json` versions not yet applied.
    pub submissions: usize,
}

impl Owed {
    pub fn total(&self) -> usize {
        self.pending + self.failed + self.submissions
    }

    /// "37 results not yet uploaded (2 failed), 1 submission not yet applied".
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        let results = self.pending + self.failed;
        if results > 0 {
            let mut s = format!(
                "{results} result{} not yet uploaded",
                if results == 1 { "" } else { "s" }
            );
            if self.failed > 0 {
                s.push_str(&format!(" ({} failed)", self.failed));
            }
            parts.push(s);
        }
        if self.submissions > 0 {
            parts.push(format!(
                "{} series edit{} not yet applied",
                self.submissions,
                if self.submissions == 1 { "" } else { "s" }
            ));
        }
        parts.join(", ")
    }
}

/// Why a Disconnect did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectError {
    /// Results are still owed: choose Wait, Export or Discard.
    Owed(Owed),
    Failed(String),
}

impl DisconnectError {
    pub fn as_json(&self) -> Value {
        match self {
            DisconnectError::Owed(o) => json!({
                "error": format!("{}: wait for them, export them to a folder, or discard them", o.describe()),
                "owed": o,
                "choices": ["wait", "export", "discard"],
            }),
            DisconnectError::Failed(e) => json!({"error": e}),
        }
    }
}

impl std::fmt::Display for DisconnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisconnectError::Owed(o) => write!(f, "{}", o.describe()),
            DisconnectError::Failed(e) => f.write_str(e),
        }
    }
}

/// What a Disconnect did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DisconnectReport {
    pub store_id: Option<String>,
    /// Files written by Export.
    pub exported: Vec<PathBuf>,
    /// Owed results dropped by Discard.
    pub discarded: usize,
    /// `state: "released"` was written into the marker.
    pub released: bool,
    pub credentials_removed: bool,
}

impl StoreHandle {
    /// What the store still owes the cloud (zero without a store).
    pub fn owed(&self) -> Owed {
        let Some(s) = &self.store else {
            return Owed::default();
        };
        let mut o = Owed::default();
        for op in s.outbox() {
            match op.state {
                "STAGED" | "HELD" | "PREFLIGHT" | "UPLOADING" | "PREPARED" => o.pending += 1,
                "FAILED" => o.failed += 1,
                _ => {}
            }
        }
        o.submissions = s
            .submissions()
            .iter()
            .filter(|(_, _, st)| !matches!(st.as_str(), "applied" | "refused"))
            .count();
        o
    }

    /// Disconnect this store (§7; see the module docs). Blocking: never call it from
    /// inside an async task. The server restarts afterwards to let the directory go.
    pub fn disconnect(
        &self,
        cloud_dir: &Path,
        mode: &DisconnectMode,
    ) -> Result<DisconnectReport, DisconnectError> {
        let mut report = DisconnectReport {
            store_id: self.store_id.clone(),
            ..DisconnectReport::default()
        };
        if let Some(store) = &self.store {
            let mut owed = self.owed();
            if owed.total() > 0 {
                match mode {
                    DisconnectMode::Drain(wait) => {
                        if owed.failed == 0 {
                            store.wait_outbox_idle(*wait);
                        }
                        owed = self.owed();
                        if owed.total() > 0 {
                            return Err(DisconnectError::Owed(owed));
                        }
                    }
                    DisconnectMode::Export(dir) => {
                        bunko_store::fs::create_dir_all(dir).map_err(|e| {
                            DisconnectError::Failed(format!("{}: {e}", dir.display()))
                        })?;
                        report.exported = store.export_outbox(dir).map_err(|e| {
                            DisconnectError::Failed(format!("exporting to {}: {e}", dir.display()))
                        })?;
                        tracing::info!(
                            "[STORAGE] Disconnect: exported {} owed result(s) to {}",
                            report.exported.len(),
                            dir.display()
                        );
                    }
                    DisconnectMode::Discard => {
                        report.discarded = owed.total();
                        tracing::warn!("[STORAGE] Disconnect: discarding {}", owed.describe());
                    }
                }
            }
            match store.release_lease(RELEASE_TIMEOUT) {
                Ok(released) => report.released = released,
                Err(e) => tracing::warn!(
                    "[STORAGE] Disconnect: could not write \"released\" into the marker ({e}); readers resume after the lease runs out"
                ),
            }
        }
        if let Some(id) = &self.store_id {
            report.credentials_removed =
                remove_store(cloud_dir, id).map_err(|e| DisconnectError::Failed(e.to_string()))?;
        }
        tracing::info!(
            "[STORAGE] Disconnected the {} library{}",
            self.backend.as_str(),
            self.store_id
                .as_deref()
                .map(|id| format!(" (store {id})"))
                .unwrap_or_default()
        );
        Ok(report)
    }
}

/// Disconnect step 3 for one store id: its credentials go, its directory is marked.
/// `true` when it had credentials.
pub fn remove_store(cloud_dir: &Path, id: &str) -> io::Result<bool> {
    let removed = match CredentialStore::open(cloud_dir) {
        Ok(c) => c.remove(id).map_err(io::Error::other)?,
        Err(e) => return Err(io::Error::other(e)),
    };
    mark_disconnected(cloud_dir, id)?;
    Ok(removed)
}

/// At startup, before any store opens: remove the directories Disconnect marked,
/// keeping an `.upgrade-originals/` folder (and so its directory) when one is there.
pub fn sweep_disconnected(cloud_dir: &Path) {
    let Ok(rd) = bunko_store::fs::read_dir(cloud_dir) else {
        return;
    };
    for e in rd.flatten() {
        let dir = e.path();
        if !bunko_store::fs::is_file(dir.join(DISCONNECTED_MARK)) {
            continue;
        }
        let keep = dir.join(".upgrade-originals");
        let result = if bunko_store::fs::is_dir(&keep) {
            remove_all_but(&dir, ".upgrade-originals")
        } else {
            bunko_store::fs::remove_dir_all(&dir)
        };
        match result {
            Ok(()) => tracing::info!(
                "[STORAGE] Removed the disconnected store directory {}",
                dir.display()
            ),
            Err(e) => tracing::warn!(
                "[STORAGE] Could not remove the disconnected store directory {}: {e}",
                dir.display()
            ),
        }
    }
}

fn remove_all_but(dir: &Path, keep: &str) -> io::Result<()> {
    for e in bunko_store::fs::read_dir(dir)?.flatten() {
        if e.file_name() == keep {
            continue;
        }
        let p = e.path();
        if bunko_store::fs::is_dir(&p) {
            bunko_store::fs::remove_dir_all(&p)?;
        } else {
            bunko_store::fs::remove_file(&p)?;
        }
    }
    Ok(())
}
