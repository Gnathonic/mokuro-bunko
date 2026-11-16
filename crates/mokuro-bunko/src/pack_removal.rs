//! Removing the installed OCR backend packs (This server's Remove) without ever leaving
//! one half deleted.
//!
//! * A pack goes in two steps: one rename takes it out of the packs' names
//!   (`<backends>/.removing-<name>-<n>`, which no pack lookup reads), then the directory
//!   is deleted. A pack is therefore installed whole or not at all: a deletion that
//!   fails partway leaves only that trash, which the next start removes
//!   ([`at_start`]). A rename that fails leaves the pack as it was.
//! * A pack loaded in this process cannot be deleted where the system locks the files
//!   of loaded libraries (Windows: its DLLs). It is recorded in
//!   `<backends>/.remove-at-start.json` instead and removed at the next start, before
//!   any pack is loaded ([`at_start`]). Turned on again meanwhile, OCR on this machine
//!   keeps the pack: its OCR starting again calls the removal off ([`call_off`]), and so
//!   does a start that finds OCR on this machine on.
//! * Elsewhere (Linux, macOS) a loaded pack is deleted at once: the libraries mapped in
//!   memory stay readable. The process must not use that pack again, though (it reads
//!   files of it later, such as GPU kernel libraries): its next start of OCR restarts
//!   the server instead ([`loaded_pack_removed`]).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tracing::{info, warn};

/// The packs to remove at the next start (a pack loaded when it was removed).
pub const PENDING_FILE: &str = ".remove-at-start.json";
/// What a pack is renamed to before it is deleted.
const TRASH_PREFIX: &str = ".removing-";

/// The record of [`PENDING_FILE`].
#[derive(Debug, Default, Serialize, Deserialize)]
struct Pending {
    packs: Vec<String>,
}

/// What a removal did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Removal {
    /// Packs no longer installed.
    pub removed: Vec<String>,
    /// Packs loaded in this process, removed when the server next starts.
    pub deferred: Vec<String>,
    /// Packs left in place (name, why): they are as they were.
    pub failed: Vec<(String, String)>,
    /// Bytes the removed packs took.
    pub freed: u64,
    /// A pack this process had loaded was deleted in place.
    pub removed_loaded: bool,
}

/// Where the system keeps loaded libraries' files from being deleted (Windows).
pub const LOADED_LOCKED: bool = cfg!(windows);

/// A pack directory name this module may act on (`torch-*`, `vulkan-*`, nothing else).
fn pack_name(name: &str) -> bool {
    (name.starts_with("torch-") || name.starts_with("vulkan-"))
        && !name.contains(['/', '\\'])
        && name != "."
        && name != ".."
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let c = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    c(a) == c(b)
}

/// The packs recorded for removal at the next start.
pub fn pending(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join(PENDING_FILE))
        .ok()
        .and_then(|t| serde_json::from_str::<Pending>(&t).ok())
        .map(|p| p.packs.into_iter().filter(|n| pack_name(n)).collect())
        .unwrap_or_default()
}

fn write_pending(root: &Path, packs: &[String]) -> std::io::Result<()> {
    let path = root.join(PENDING_FILE);
    if packs.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let tmp = root.join(format!("{PENDING_FILE}.tmp"));
    let body = serde_json::to_string_pretty(&Pending {
        packs: packs.to_vec(),
    })
    .unwrap_or_default();
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)
}

/// A fresh trash name beside the packs for `name`.
fn trash_for(root: &Path, name: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    loop {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let p = root.join(format!("{TRASH_PREFIX}{name}-{}-{n}", std::process::id()));
        if !p.exists() {
            return p;
        }
    }
}

/// Take `dir` out of the installed packs with one rename, then delete it. Err: the
/// rename failed and the pack is as it was. A deletion that fails after the rename is
/// only logged: the trash goes at the next start.
fn remove_dir(root: &Path, dir: &Path, name: &str) -> std::io::Result<()> {
    let trash = trash_for(root, name);
    std::fs::rename(dir, &trash)?;
    if let Err(e) = std::fs::remove_dir_all(&trash) {
        warn!(
            "OCR backend pack {name}: removed, but not all of its files could be deleted yet ({e}); {} goes at the next start",
            trash.display()
        );
    }
    Ok(())
}

/// Remove every pack installed under `root` (the packs [`bunko_update::backend::installed`]
/// lists). `loaded`: the pack directories this process has loaded; with
/// `defer_loaded`, those are recorded for removal at the next start instead (see the
/// module docs). `size` measures a pack before it goes.
pub fn remove_installed(
    root: &Path,
    loaded: &[PathBuf],
    defer_loaded: bool,
    size: impl Fn(&Path) -> u64,
) -> Removal {
    let mut out = Removal::default();
    let mut deferred = pending(root);
    for (dir, man) in bunko_update::backend::installed(root) {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| man.name.clone());
        if !pack_name(&name) {
            continue;
        }
        let is_loaded = loaded.iter().any(|l| same_dir(l, &dir));
        if is_loaded && defer_loaded {
            if !deferred.contains(&name) {
                deferred.push(name.clone());
            }
            out.deferred.push(name);
            continue;
        }
        let bytes = size(&dir);
        match remove_dir(root, &dir, &name) {
            Ok(()) => {
                info!("Removed the OCR backend pack {}", dir.display());
                out.freed += bytes;
                out.removed_loaded |= is_loaded;
                deferred.retain(|n| *n != name);
                out.removed.push(name);
            }
            Err(e) => {
                warn!(
                    "Could not remove the OCR backend pack {} ({e}); it is left as it was",
                    dir.display()
                );
                out.failed.push((name, e.to_string()));
            }
        }
    }
    if let Err(e) = write_pending(root, &deferred) {
        // Without the record the pack would stay: say it is not removed.
        warn!(
            "could not record the OCR backend packs to remove at the next start ({}: {e})",
            root.join(PENDING_FILE).display()
        );
        for name in std::mem::take(&mut out.deferred) {
            out.failed.push((
                name,
                format!("it is in use and could not be recorded for removal: {e}"),
            ));
        }
    } else if !out.deferred.is_empty() {
        info!(
            "OCR backend pack(s) {} in use by this server: removed when it next starts",
            out.deferred.join(", ")
        );
    }
    out
}

/// At the start of the server, before any pack is loaded: delete what an earlier
/// removal left (trash), and the packs recorded for removal at this start, unless OCR
/// on this machine was turned on again since (`local_on`: the removal is called off
/// and the packs stay). Returns the packs removed.
pub fn at_start(root: &Path, local_on: bool) -> Vec<String> {
    let mut removed = Vec::new();
    let wanted = pending(root);
    if !wanted.is_empty() {
        if local_on {
            call_off(root);
        } else {
            let mut left = Vec::new();
            for name in wanted {
                let dir = root.join(&name);
                if !dir.exists() {
                    continue;
                }
                match remove_dir(root, &dir, &name) {
                    Ok(()) => {
                        info!(
                            "Removed the OCR backend pack {} (asked for before this start)",
                            dir.display()
                        );
                        removed.push(name);
                    }
                    Err(e) => {
                        warn!(
                            "Could not remove the OCR backend pack {} ({e}); trying again at the next start",
                            dir.display()
                        );
                        left.push(name);
                    }
                }
            }
            if let Err(e) = write_pending(root, &left) {
                warn!(
                    "could not update {}: {e}",
                    root.join(PENDING_FILE).display()
                );
            }
        }
    }
    for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(TRASH_PREFIX) && e.file_type().is_ok_and(|t| t.is_dir()) {
            match std::fs::remove_dir_all(e.path()) {
                Ok(()) => info!("Deleted the rest of a removed OCR backend pack ({name})"),
                Err(err) => warn!("could not delete {} yet: {err}", e.path().display()),
            }
        }
    }
    removed
}

/// OCR on this machine is on again (its OCR starts in this process, or the server
/// starts with it on): the packs recorded for removal at the next start stay, and the
/// record goes. Turned off again before that start, OCR on this machine keeps them
/// still: only a new Remove removes them. Returns the packs the removal was called off
/// for.
pub fn call_off(root: &Path) -> Vec<String> {
    let wanted = pending(root);
    if wanted.is_empty() {
        return wanted;
    }
    match write_pending(root, &[]) {
        Ok(()) => info!(
            "OCR on this machine is on again: the OCR backend pack(s) {} recorded for removal stay",
            wanted.join(", ")
        ),
        Err(e) => warn!(
            "OCR on this machine is on again, but {} could not be updated ({e}): the OCR backend pack(s) {} go at the next start with OCR on this machine off",
            root.join(PENDING_FILE).display(),
            wanted.join(", ")
        ),
    }
    wanted
}

static LOADED_REMOVED: AtomicBool = AtomicBool::new(false);
type Restart = Arc<dyn Fn() + Send + Sync>;
static RESTART: OnceLock<Restart> = OnceLock::new();

/// A pack this process had loaded was deleted (not on Windows: see the module docs).
pub fn note_loaded_removed() {
    LOADED_REMOVED.store(true, Ordering::SeqCst);
}

/// This process must not start OCR on the pack it has loaded: it was removed.
pub fn loaded_pack_removed() -> bool {
    LOADED_REMOVED.load(Ordering::SeqCst)
}

/// How the server restarts (the machine's hooks), for [`restart_for_removed_pack`].
pub fn set_restart(restart: Restart) {
    let _ = RESTART.set(restart);
}

/// OCR is to start again after its loaded pack was removed: restart the server in a
/// moment, so that it loads the pack installed now. False when no restart is wired.
pub fn restart_for_removed_pack() -> bool {
    let Some(restart) = RESTART.get().cloned() else {
        return false;
    };
    info!("Restarting the server: the OCR backend it had loaded was removed");
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        restart();
    });
    true
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A pack as `install-ocr` leaves it (a pack.json the lookup reads, a library).
    pub(crate) fn pack(root: &Path, name: &str, variant: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/libtorch.so"), vec![0u8; 1000]).unwrap();
        let man = serde_json::json!({
            "format": bunko_update::backend::PACK_FORMAT,
            "name": name,
            "variant": variant,
            "torch": "2.13.0",
            "target": bunko_update::TARGET,
            "os": "linux",
            "arch": "x86_64",
            "abi": 1,
            "library": "lib/libtorch.so",
            "bunko_version": bunko_core::VERSION,
            "lib_dir": "lib",
            "files": [],
        });
        std::fs::write(
            dir.join(bunko_update::backend::PACK_JSON),
            serde_json::to_vec(&man).unwrap(),
        )
        .unwrap();
        assert!(
            bunko_update::backend::installed(root)
                .iter()
                .any(|(p, _)| p == &dir),
            "the fixture reads as installed: {:?}",
            std::fs::read_to_string(dir.join(bunko_update::backend::PACK_JSON))
        );
        dir
    }

    fn names(root: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn installed(root: &Path) -> Vec<String> {
        bunko_update::backend::installed(root)
            .into_iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// Packs not loaded go at once and leave nothing behind.
    #[test]
    fn packs_not_loaded_go_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        pack(root, "torch-rocm7.1-2.13.0", "rocm7.1");
        pack(root, "vulkan-0.8.0", "vulkan");
        let r = remove_installed(root, &[], true, |_| 7);
        assert_eq!(r.removed.len(), 2, "{r:?}");
        assert!(r.deferred.is_empty() && r.failed.is_empty(), "{r:?}");
        assert_eq!(r.freed, 14);
        assert!(installed(root).is_empty());
        assert!(names(root).is_empty(), "{:?}", names(root));
    }

    /// Regression (Windows): Remove deleted every pack with remove_dir_all, and a pack
    /// whose DLLs this server had loaded failed partway, leaving it half deleted. A
    /// loaded pack (where loaded files cannot be deleted) is recorded instead, stays
    /// whole, and goes at the next start before anything loads it; the others go now.
    #[test]
    fn a_loaded_pack_is_removed_at_the_next_start_where_it_is_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let torch = pack(root, "torch-rocm7.1-2.13.0", "rocm7.1");
        pack(root, "vulkan-0.8.0", "vulkan");
        let r = remove_installed(root, std::slice::from_ref(&torch), true, |_| 1);
        assert_eq!(r.removed, ["vulkan-0.8.0"]);
        assert_eq!(r.deferred, ["torch-rocm7.1-2.13.0"]);
        assert!(!r.removed_loaded);
        // Whole and still installed until the next start.
        assert_eq!(installed(root), ["torch-rocm7.1-2.13.0"]);
        assert!(torch.join("lib/libtorch.so").is_file());
        assert_eq!(pending(root), ["torch-rocm7.1-2.13.0"]);
        // Removed again before the restart: recorded once.
        let again = remove_installed(root, std::slice::from_ref(&torch), true, |_| 1);
        assert_eq!(again.deferred, ["torch-rocm7.1-2.13.0"]);
        assert_eq!(pending(root), ["torch-rocm7.1-2.13.0"]);

        // The next start, OCR on this machine still off: it goes, record included.
        assert_eq!(at_start(root, false), ["torch-rocm7.1-2.13.0"]);
        assert!(names(root).is_empty(), "{:?}", names(root));
    }

    /// Turned on again before the restart, OCR on this machine keeps the pack.
    #[test]
    fn a_removal_at_start_is_called_off_when_ocr_is_on_again() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let torch = pack(root, "torch-cu130-2.13.0", "cu130");
        remove_installed(root, std::slice::from_ref(&torch), true, |_| 1);
        assert_eq!(at_start(root, true), Vec::<String>::new());
        assert_eq!(installed(root), ["torch-cu130-2.13.0"]);
        assert!(pending(root).is_empty());
    }

    /// Regression (Windows): a removal recorded for the next start was called off only
    /// when that start found OCR on this machine on. Turned on again (its OCR running on
    /// the pack once more) and then off before the restart, OCR on this machine lost the
    /// pack at that start, though the owner had taken the Remove back, and the panel
    /// meanwhile said "removed when the server next starts" of a pack in use. OCR
    /// starting again calls the removal off for good.
    #[test]
    fn ocr_started_again_calls_a_removal_at_start_off() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let torch = pack(root, "torch-cu130-2.13.0", "cu130");
        let r = remove_installed(root, std::slice::from_ref(&torch), true, |_| 1);
        assert_eq!(r.deferred, ["torch-cu130-2.13.0"]);
        // Turned on again: this server's OCR starts on the pack.
        assert_eq!(call_off(root), ["torch-cu130-2.13.0"]);
        assert!(pending(root).is_empty(), "the panel no longer says it goes");
        assert!(call_off(root).is_empty());
        // Turned off again, then the server restarts: the pack stays.
        assert!(at_start(root, false).is_empty());
        assert_eq!(installed(root), ["torch-cu130-2.13.0"]);
        assert!(torch.join("lib/libtorch.so").is_file());
    }

    /// Where loaded files can be deleted (Linux, macOS), a loaded pack goes at once and
    /// the process knows it may not use it again.
    #[test]
    fn a_loaded_pack_goes_at_once_where_it_can() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let torch = pack(root, "torch-cpu-2.13.0", "cpu");
        let r = remove_installed(root, std::slice::from_ref(&torch), false, |_| 1);
        assert_eq!(r.removed, ["torch-cpu-2.13.0"]);
        assert!(r.removed_loaded);
        assert!(names(root).is_empty());
    }

    /// A pack that cannot be taken out of place (here: a folder that refuses changes)
    /// is left whole and still installed, never half deleted; a deletion that failed
    /// after the rename leaves only trash, which no lookup reads and the next start
    /// removes.
    #[cfg(unix)]
    #[test]
    fn a_failed_removal_never_leaves_a_broken_pack() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let torch = pack(root, "torch-cpu-2.13.0", "cpu");
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o555)).unwrap();
        let writable = std::fs::write(root.join("probe"), b"").is_ok();
        let r = remove_installed(root, &[], true, |_| 1);
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
        if writable {
            return; // Running as root: permissions refuse nothing.
        }
        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert!(r.removed.is_empty());
        assert_eq!(installed(root), ["torch-cpu-2.13.0"]);
        assert!(torch.join("lib/libtorch.so").is_file(), "whole");

        // Trash a deletion could not finish: not a pack, and gone at the next start.
        let trash = root.join(format!("{TRASH_PREFIX}torch-cuda-x-1-0"));
        std::fs::create_dir_all(trash.join("lib")).unwrap();
        std::fs::write(trash.join("lib/libtorch.so"), b"x").unwrap();
        assert_eq!(installed(root), ["torch-cpu-2.13.0"]);
        at_start(root, false);
        assert!(!trash.exists());
        assert_eq!(installed(root), ["torch-cpu-2.13.0"]);
    }

    /// A record naming anything but a pack directory is ignored.
    #[test]
    fn the_record_names_only_packs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::fs::write(
            root.join(PENDING_FILE),
            r#"{"packs": ["../models", "models", "torch-x/../../y"]}"#,
        )
        .unwrap();
        assert!(pending(root).is_empty());
        at_start(root, false);
        assert!(root.join("models").is_dir());
    }
}
