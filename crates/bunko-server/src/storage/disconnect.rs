//! Disconnect (0.8 design §7): leave a cloud library without losing anything.
//!
//! 1. Drain the outbox. When results are still owed (not yet uploaded, FAILED, or
//!    foreign `series.json` submissions not yet applied), nothing happens and the
//!    count comes back ("37 results not yet uploaded") with the choices **Wait**
//!    (drain longer), **Export** (copy their bytes to a folder) and **Discard**.
//!    Over every HTTP API the export folder is Bunko's own, a new
//!    `<base>/exports/<store-id>-<UTC time>/` whose path the report returns; only the
//!    local CLI, run in-process by the machine's user, names a folder of its own
//!    ([`DisconnectMode::ExportTo`]: refused when it exists and is not empty, or is a
//!    symlink). Every file is created new (never through an existing file or symlink)
//!    under a flat, sanitised name `<op id>-<destination>`, and `index.json` maps
//!    each one back to its destination in the library.
//! 2. Write `state: "released"` into this install's marker, so readers resume their
//!    own metadata writes at once.
//! 3. Delete the store's credentials and mark its directory (index, state, caches,
//!    spool) for removal at the next start, when nothing has it open. An
//!    `.upgrade-originals/` folder in it is kept. Only a directory named by a store id
//!    (16 lowercase hex digits), directly under `<base>/cloud` and not a symlink (nor
//!    resolving anywhere else), is ever marked or removed.
//!
//! Disconnect never deletes anything in the cloud: the `pre-upgrade` layers D4 keeps
//! are files of the cloud library and stay there. Google is never revoked.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bunko_remote::auth::CredentialStore;
use serde::Serialize;
use serde_json::{Value, json};

use super::connect::{DISCONNECTED_MARK, mark_disconnected};
use super::{CloudStore, StoreHandle, is_store_id};

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
    /// Copy the owed results into a new folder Bunko owns,
    /// `<base>/exports/<store-id>-<UTC time>/` (the report names it), then go on. The
    /// only export an HTTP API can ask for.
    Export,
    /// Copy them into this folder, then go on: the local CLI only (in-process, run by
    /// the machine's user; never built from a request). Refused when it exists and is
    /// not empty, or is a symlink.
    ExportTo(PathBuf),
    /// Drop them.
    Discard,
}

impl DisconnectMode {
    /// `{"mode": "wait"|"export"|"discard", "wait_s": ..}` as the pages (and every other
    /// HTTP caller) send it. A request never chooses where an export is written: one
    /// that names a `folder` is refused.
    pub fn from_json(v: &Value) -> Result<DisconnectMode, String> {
        match v.get("mode").and_then(Value::as_str).unwrap_or("wait") {
            "wait" | "drain" => Ok(DisconnectMode::Drain(Duration::from_secs(
                v.get("wait_s")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_DRAIN.as_secs())
                    .min(3600),
            ))),
            "export" => {
                if v.get("folder").is_some_and(|f| !f.is_null()) {
                    return Err("Export writes to a new folder Bunko makes under its storage folder (exports/), and the reply names it; a request cannot choose the folder".into());
                }
                Ok(DisconnectMode::Export)
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
    /// The folder Export wrote to.
    pub export_dir: Option<PathBuf>,
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
                    DisconnectMode::Export | DisconnectMode::ExportTo(_) => {
                        let target = match mode {
                            DisconnectMode::ExportTo(dir) => ExportTarget::Chosen(dir),
                            _ => ExportTarget::Owned(self.store_id.as_deref()),
                        };
                        let (dir, files) = export_owed(store.as_ref(), cloud_dir, target)
                            .map_err(DisconnectError::Failed)?;
                        tracing::info!(
                            "[STORAGE] Disconnect: exported {} owed result(s) to {}",
                            files.len(),
                            dir.display()
                        );
                        report.export_dir = Some(dir);
                        report.exported = files;
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
/// `true` when it had credentials. An id that is not a store id has no directory to
/// mark (nothing outside `<base>/cloud/<id>` is ever touched).
pub fn remove_store(cloud_dir: &Path, id: &str) -> io::Result<bool> {
    let removed = match CredentialStore::open(cloud_dir) {
        Ok(c) => c.remove(id).map_err(io::Error::other)?,
        Err(e) => return Err(io::Error::other(e)),
    };
    if is_store_id(id) {
        mark_disconnected(cloud_dir, id)?;
    } else {
        tracing::warn!("[STORAGE] Disconnect: {id:?} is not a store id; no directory marked");
    }
    Ok(removed)
}

/// `<cloud_dir>/<id>` when `id` is a store id and that path is a real directory
/// directly under `cloud_dir`: not a symlink, and resolving to a child of the resolved
/// `cloud_dir`. The only directories Disconnect ever removes.
pub fn store_dir_checked(cloud_dir: &Path, id: &str) -> io::Result<PathBuf> {
    let refuse = |why: &str| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "not a store directory: {}: {why}",
                cloud_dir.join(id).display()
            ),
        )
    };
    if !is_store_id(id) {
        return Err(refuse("not a store id"));
    }
    let dir = cloud_dir.join(id);
    let meta = bunko_store::fs::symlink_metadata(&dir)?;
    if meta.is_symlink() || !meta.is_dir() {
        return Err(refuse("not a directory"));
    }
    let real_cloud = bunko_store::fs::canonicalize(cloud_dir)?;
    let real_dir = bunko_store::fs::canonicalize(&dir)?;
    if real_dir.parent() != Some(real_cloud.as_path()) {
        return Err(refuse("it resolves outside the cloud folder"));
    }
    Ok(dir)
}

/// At startup, before any store opens: remove the directories Disconnect marked,
/// keeping an `.upgrade-originals/` folder (and so its directory) when one is there.
/// Only store directories ([`store_dir_checked`]) are considered.
pub fn sweep_disconnected(cloud_dir: &Path) {
    let Ok(rd) = bunko_store::fs::read_dir(cloud_dir) else {
        return;
    };
    for e in rd.flatten() {
        let Some(id) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(dir) = store_dir_checked(cloud_dir, &id) else {
            continue;
        };
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

/// Remove every entry of `dir` but `keep`. A symlink is removed itself, never followed.
fn remove_all_but(dir: &Path, keep: &str) -> io::Result<()> {
    for e in bunko_store::fs::read_dir(dir)?.flatten() {
        if e.file_name() == keep {
            continue;
        }
        let p = e.path();
        let meta = bunko_store::fs::symlink_metadata(&p)?;
        if meta.is_dir() && !meta.is_symlink() {
            bunko_store::fs::remove_dir_all(&p)?;
        } else {
            bunko_store::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

/// Where Export writes.
enum ExportTarget<'a> {
    /// A new folder under `<base>/exports/`, named after the store.
    Owned(Option<&'a str>),
    /// The CLI's own folder.
    Chosen(&'a Path),
}

/// `<base>/exports`: the folders Bunko's own exports go to (`<base>/cloud`'s sibling).
pub fn exports_dir(cloud_dir: &Path) -> PathBuf {
    cloud_dir.parent().unwrap_or(cloud_dir).join("exports")
}

/// A name safe as one file name on every OS: no path separators, no `..`, no control
/// or reserved characters, no leading dot or space, at most 150 bytes. Never empty.
pub fn safe_file_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    out = out
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' '])
        .to_string();
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    if out.len() > 150 {
        let mut cut = 150;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    if out.is_empty() {
        out.push_str("file");
    }
    out
}

/// Make `dir` the empty folder an export writes into: a new one when it is missing
/// (its parents may be created); refused when it is a symlink, not a folder, or a
/// folder that holds anything.
pub fn prepare_export_dir(dir: &Path) -> Result<(), String> {
    let shown = dir.display();
    match bunko_store::fs::symlink_metadata(dir) {
        Ok(m) if m.is_symlink() => {
            return Err(format!("{shown} is a symlink: give a plain folder"));
        }
        Ok(m) if !m.is_dir() => return Err(format!("{shown} exists and is not a folder")),
        Ok(_) => {
            let full = bunko_store::fs::read_dir(dir)
                .map_err(|e| format!("{shown}: {e}"))?
                .next()
                .is_some();
            if full {
                return Err(format!(
                    "{shown} is not empty: give a new or empty folder (nothing in it is overwritten)"
                ));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                bunko_store::fs::create_dir_all(parent).map_err(|e| format!("{shown}: {e}"))?;
            }
            bunko_store::fs::create_dir(dir).map_err(|e| format!("{shown}: {e}"))?;
        }
        Err(e) => return Err(format!("{shown}: {e}")),
    }
    match bunko_store::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() && !m.is_symlink() => Ok(()),
        _ => Err(format!("{shown} changed while it was prepared")),
    }
}

/// A new, empty folder `<parent>/<name>` (`<name>-2`, `-3`… when taken), created with
/// `create_dir` so nothing already there is ever reused.
fn new_dir_under(parent: &Path, name: &str) -> Result<PathBuf, String> {
    for n in 1u32..1000 {
        let leaf = if n == 1 {
            name.to_string()
        } else {
            format!("{name}-{n}")
        };
        let dir = parent.join(&leaf);
        match bunko_store::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {e}", dir.display())),
        }
    }
    Err(format!("{}: no free name for {name}", parent.display()))
}

/// Copy `from` (a regular file, never followed through a symlink) to `dir/<name>`,
/// created new: `<stem>-2<ext>`… when the name is taken, never overwriting anything.
fn copy_new(from: &Path, dir: &Path, name: &str) -> io::Result<PathBuf> {
    let meta = bunko_store::fs::symlink_metadata(from)?;
    if meta.is_symlink() || !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a plain file", from.display()),
        ));
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 1u32..1000 {
        let candidate = if n == 1 {
            name.to_string()
        } else {
            format!("{stem}-{n}{ext}")
        };
        let to = dir.join(&candidate);
        let mut out = match bunko_store::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&to)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let mut src = bunko_store::fs::File::open(from)?;
        io::copy(&mut src, &mut out)?;
        out.sync_all()?;
        return Ok(to);
    }
    Err(io::Error::other(format!(
        "{}: no free name for {name}",
        dir.display()
    )))
}

/// The flat export name of a result bound for `dest` (`Series/Vol 1.mokuro`):
/// `<op id>-Series__Vol 1.mokuro`, sanitised.
fn export_name(op: Option<u64>, dest: &str) -> String {
    let flat = dest
        .split('/')
        .filter(|s| !s.is_empty())
        .map(safe_file_name)
        .collect::<Vec<_>>()
        .join("__");
    safe_file_name(&match op {
        Some(id) => format!("{id}-{flat}"),
        None => flat,
    })
}

/// Which op wrote each staged file `rel` (the path `export_outbox` gave it under the
/// staging folder: the op's destination, or `<stem> (<op id>)<ext>` beside it when two
/// ops share one).
fn ops_of(rels: &[String], owed: &[(u64, String)]) -> Vec<Option<u64>> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut by_dest: BTreeMap<&str, BTreeSet<u64>> = BTreeMap::new();
    for (id, dest) in owed {
        by_dest.entry(dest.as_str()).or_default().insert(*id);
    }
    let renamed = |rel: &str| -> Option<(String, u64)> {
        let (dir, name) = rel.rsplit_once('/').map_or(("", rel), |(d, n)| (d, n));
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], &name[i..]),
            _ => (name, ""),
        };
        let (base, id) = stem.strip_suffix(')')?.rsplit_once(" (")?;
        let id: u64 = id.parse().ok()?;
        let dest = if dir.is_empty() {
            format!("{base}{ext}")
        } else {
            format!("{dir}/{base}{ext}")
        };
        by_dest
            .get(dest.as_str())
            .is_some_and(|ids| ids.contains(&id))
            .then_some((dest, id))
    };
    let mut claimed: BTreeSet<u64> = BTreeSet::new();
    let mut out: Vec<Option<u64>> = rels
        .iter()
        .map(|rel| {
            let found = renamed(rel).map(|(_, id)| id);
            if let Some(id) = found {
                claimed.insert(id);
            }
            found
        })
        .collect();
    for (i, rel) in rels.iter().enumerate() {
        if out[i].is_none()
            && let Some(ids) = by_dest.get(rel.as_str())
            && let Some(id) = ids.iter().find(|id| !claimed.contains(id)).copied()
        {
            claimed.insert(id);
            out[i] = Some(id);
        }
    }
    out
}

/// Export (§7): the owed results' bytes, copied flat into the target folder under
/// [`export_name`]s, plus `index.json` (`[{file, dest, op}]`). The store first copies
/// them into a fresh staging folder of Bunko's own (its destinations are checked
/// relative paths, so they stay inside it); each is then copied out with
/// [`copy_new`]. Returns the folder and the files written.
fn export_owed(
    store: &dyn CloudStore,
    cloud_dir: &Path,
    target: ExportTarget<'_>,
) -> Result<(PathBuf, Vec<PathBuf>), String> {
    let exports = exports_dir(cloud_dir);
    bunko_store::fs::create_dir_all(&exports).map_err(|e| format!("{}: {e}", exports.display()))?;
    match bunko_store::fs::symlink_metadata(&exports) {
        Ok(m) if m.is_dir() && !m.is_symlink() => {}
        _ => return Err(format!("{} is not a plain folder", exports.display())),
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let stamp = bunko_sched::py::iso_utc(now).replace(':', "");
    let id = match &target {
        ExportTarget::Owned(Some(id)) if is_store_id(id) => (*id).to_string(),
        _ => "library".to_string(),
    };
    let leaf = format!("{id}-{stamp}");
    let owned = matches!(target, ExportTarget::Owned(_));
    let dir = match target {
        ExportTarget::Owned(_) => new_dir_under(&exports, &leaf)?,
        ExportTarget::Chosen(dir) => {
            prepare_export_dir(dir)?;
            dir.to_path_buf()
        }
    };
    let staging = new_dir_under(&exports, &format!(".staging-{leaf}"))?;
    let result = (|| {
        let owed: Vec<(u64, String)> = store.outbox().into_iter().map(|o| (o.id, o.dest)).collect();
        let staged = store
            .export_outbox(&staging)
            .map_err(|e| format!("exporting to {}: {e}", dir.display()))?;
        let rels: Vec<String> = staged
            .iter()
            .map(|p| {
                p.strip_prefix(&staging)
                    .map(|r| {
                        r.components()
                            .map(|c| c.as_os_str().to_string_lossy().into_owned())
                            .collect::<Vec<_>>()
                            .join("/")
                    })
                    .map_err(|_| format!("{} is outside the export staging folder", p.display()))
            })
            .collect::<Result<_, _>>()?;
        let ops = ops_of(&rels, &owed);
        let mut written = Vec::new();
        let mut index = Vec::new();
        for ((from, rel), op) in staged.iter().zip(&rels).zip(ops) {
            let name = export_name(op, rel);
            let to = copy_new(from, &dir, &name).map_err(|e| format!("{}: {e}", dir.display()))?;
            index.push(json!({
                "file": to.file_name().map(|n| n.to_string_lossy().into_owned()),
                "dest": rel,
                "op": op,
            }));
            written.push(to);
        }
        let listing = serde_json::to_vec_pretty(&Value::Array(index)).unwrap_or_default();
        let index_path = dir.join("index.json");
        let mut f = bunko_store::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&index_path)
            .map_err(|e| format!("{}: {e}", index_path.display()))?;
        io::Write::write_all(&mut f, &listing)
            .map_err(|e| format!("{}: {e}", index_path.display()))?;
        Ok(written)
    })();
    let _ = bunko_store::fs::remove_dir_all(&staging);
    if result.is_err() && owned {
        // Bunko's own folder, left empty, goes (one with files stays).
        let _ = bunko_store::fs::remove_dir(&dir);
    }
    result.map(|written| (dir, written))
}

/// The CLI side of an export a running server made: copy the files of Bunko's export
/// folder `from` (plain files only, never through a symlink) into the CLI's own `to`
/// ([`prepare_export_dir`]: new or empty), each created new under a sanitised name.
pub fn copy_export(from: &Path, to: &Path) -> Result<Vec<PathBuf>, String> {
    prepare_export_dir(to)?;
    let mut names: Vec<String> = bunko_store::fs::read_dir(from)
        .map_err(|e| format!("{}: {e}", from.display()))?
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect();
    names.sort();
    let mut written = Vec::new();
    for name in names {
        let src = from.join(&name);
        let plain =
            bunko_store::fs::symlink_metadata(&src).is_ok_and(|m| m.is_file() && !m.is_symlink());
        if !plain {
            continue;
        }
        let to_path = copy_new(&src, to, &safe_file_name(&name))
            .map_err(|e| format!("{}: {e}", to.display()))?;
        written.push(to_path);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names_have_no_separators_or_dot_dot() {
        for (raw, want) in [
            ("Vol 1.mokuro", "Vol 1.mokuro"),
            ("../../etc/passwd", "_._etc_passwd"),
            ("..", "file"),
            (".", "file"),
            ("", "file"),
            ("a\\b:c*d?e\"f<g>h|i", "a_b_c_d_e_f_g_h_i"),
            (".hidden", "hidden"),
            ("x\u{0}y\ny", "x_y_y"),
            ("a..b", "a.b"),
        ] {
            let got = safe_file_name(raw);
            assert_eq!(got, want, "{raw:?}");
            assert!(!got.contains('/') && !got.contains('\\') && !got.contains(".."));
        }
        let long = "あ".repeat(100);
        assert!(safe_file_name(&long).len() <= 150);
    }

    #[test]
    fn export_names_are_flat_and_carry_the_op() {
        assert_eq!(
            export_name(Some(12), "Series/Vol 1.mokuro"),
            "12-Series__Vol 1.mokuro"
        );
        assert_eq!(export_name(None, "a/../b"), "a__file__b");
        assert_eq!(export_name(Some(3), ".."), "3-file");
    }

    #[test]
    fn staged_files_map_back_to_their_ops() {
        let owed = vec![
            (4, "S/V.mokuro".to_string()),
            (9, "S/V.mokuro".to_string()),
            (5, "T/W.mokuro".to_string()),
        ];
        let rels = vec![
            "S/V.mokuro".to_string(),
            "S/V (9).mokuro".to_string(),
            "T/W.mokuro".to_string(),
            "U/X.mokuro".to_string(),
        ];
        assert_eq!(ops_of(&rels, &owed), [Some(4), Some(9), Some(5), None]);
    }

    #[test]
    fn store_ids_are_checked_before_any_removal() {
        let dir = tempfile::tempdir().unwrap();
        let cloud = dir.path().join("cloud");
        bunko_store::fs::create_dir_all(&cloud).unwrap();
        let id = "0123456789abcdef";
        bunko_store::fs::create_dir(cloud.join(id)).unwrap();
        assert_eq!(store_dir_checked(&cloud, id).unwrap(), cloud.join(id));
        for bad in [
            "..",
            "../x",
            "0123456789ABCDEF",
            "0123456789abcde",
            "/etc",
            "",
        ] {
            assert!(store_dir_checked(&cloud, bad).is_err(), "{bad:?}");
        }
        // A symlink named like a store, pointing outside: never a store directory.
        let outside = dir.path().join("precious");
        bunko_store::fs::create_dir(&outside).unwrap();
        bunko_store::fs::write(outside.join("keep"), b"x").unwrap();
        bunko_store::fs::write(outside.join(DISCONNECTED_MARK), b"x").unwrap();
        let link = "fedcba9876543210";
        std::os::unix::fs::symlink(&outside, cloud.join(link)).unwrap();
        assert!(store_dir_checked(&cloud, link).is_err());
        // The sweep removes the marked real store directory and nothing else.
        bunko_store::fs::write(cloud.join(id).join(DISCONNECTED_MARK), b"x").unwrap();
        bunko_store::fs::write(cloud.join(id).join("state.sqlite"), b"x").unwrap();
        sweep_disconnected(&cloud);
        assert!(!bunko_store::fs::exists(cloud.join(id)));
        assert!(
            bunko_store::fs::is_file(outside.join("keep")),
            "never followed"
        );
        assert!(bunko_store::fs::is_dir(cloud.join(link)));
        // An id that is not a store id marks nothing.
        assert!(mark_disconnected(&cloud, "../precious").is_err());
    }

    #[test]
    fn a_chosen_export_folder_must_be_new_or_empty_and_not_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("a/b/new");
        prepare_export_dir(&fresh).unwrap();
        assert!(bunko_store::fs::is_dir(&fresh));
        prepare_export_dir(&fresh).unwrap();
        bunko_store::fs::write(fresh.join("x"), b"x").unwrap();
        assert!(
            prepare_export_dir(&fresh)
                .unwrap_err()
                .contains("not empty")
        );
        let target = dir.path().join("target");
        bunko_store::fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(prepare_export_dir(&link).unwrap_err().contains("symlink"));
        let file = dir.path().join("file");
        bunko_store::fs::write(&file, b"x").unwrap();
        assert!(prepare_export_dir(&file).is_err());
    }

    #[test]
    fn copies_are_created_new_and_never_follow_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from");
        bunko_store::fs::create_dir(&from).unwrap();
        bunko_store::fs::write(from.join("1-S__V.mokuro"), b"one").unwrap();
        let secret = dir.path().join("secret");
        bunko_store::fs::write(&secret, b"secret").unwrap();
        std::os::unix::fs::symlink(&secret, from.join("2-link.mokuro")).unwrap();
        let to = dir.path().join("to");
        let written = copy_export(&from, &to).unwrap();
        assert_eq!(written, [to.join("1-S__V.mokuro")]);
        assert_eq!(bunko_store::fs::read(&written[0]).unwrap(), b"one");
        assert!(!bunko_store::fs::exists(to.join("2-link.mokuro")));
        // A name already taken in the folder is never written through.
        let victim = dir.path().join("victim");
        bunko_store::fs::write(&victim, b"keep").unwrap();
        let out = dir.path().join("out");
        bunko_store::fs::create_dir(&out).unwrap();
        std::os::unix::fs::symlink(&victim, out.join("1-S__V.mokuro")).unwrap();
        let to2 = copy_new(&from.join("1-S__V.mokuro"), &out, "1-S__V.mokuro").unwrap();
        assert_eq!(to2, out.join("1-S__V-2.mokuro"));
        assert_eq!(bunko_store::fs::read(&victim).unwrap(), b"keep");
    }
}
