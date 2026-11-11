//! The connect probe: what the folder's filesystem does with names, renames and
//! times. It works inside `<root>/.bunko-probe-<token>/` and removes it after; a
//! folder Bunko cannot write is probed read-only. It never fails.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::FolderProbe;
use super::fsops;

/// What the probe found that the public [`FolderProbe`] does not show.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Hidden {
    /// The probe ran (the folder was reachable).
    pub ran: bool,
    /// An NFC name and its NFD spelling name one entry (APFS, HFS+).
    pub folds_unicode: bool,
    /// The folder's device, when the probe ran (Unix).
    pub device: Option<u64>,
}

const NFC_NAME: &str = "\u{e9}t\u{e9}";
const NFD_NAME: &str = "e\u{301}te\u{301}";

/// Probe the folder at `root` (which the caller found to be a directory).
pub(crate) fn run(root: &Path, token: &str) -> (FolderProbe, Hidden) {
    let mut p = FolderProbe {
        reachable: true,
        mtime_granularity_ns: 0,
        ..FolderProbe::default()
    };
    let mut h = Hidden {
        ran: true,
        ..Hidden::default()
    };
    if let Ok(m) = std::fs::metadata(root) {
        h.device = fsops::device_of(&m);
        if let (Some(dev), Some(parent)) = (h.device, root.parent())
            && let Ok(pm) = std::fs::metadata(parent)
        {
            p.mount_point = fsops::device_of(&pm).is_some_and(|d| d != dev);
        }
    }
    let dir = root.join(format!(".bunko-probe-{token}"));
    if std::fs::create_dir(&dir).is_err() {
        read_only_guess(root, &mut p, &mut h);
        return (p, h);
    }
    p.writable = true;
    write_tests(&dir, &mut p, &mut h);
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        tracing::warn!(
            "folder: cannot remove the probe folder {}: {e}",
            dir.display()
        );
    }
    (p, h)
}

fn touch(p: &Path) -> bool {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(p)
        .is_ok()
}

fn write_tests(dir: &Path, p: &mut FolderProbe, h: &mut Hidden) {
    // Case: create `Aa`, look up `aA`.
    if touch(&dir.join("Aa")) {
        p.folds_case = std::fs::symlink_metadata(dir.join("aA")).is_ok();
    }
    // Unicode: create the NFC name, read the bytes back, look up the NFD spelling.
    if touch(&dir.join(NFC_NAME)) {
        let listed: Vec<std::ffi::OsString> = std::fs::read_dir(dir)
            .map(|rd| rd.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        p.rewrites_names = !listed.iter().any(|n| n == NFC_NAME);
        h.folds_unicode = p.rewrites_names || std::fs::symlink_metadata(dir.join(NFD_NAME)).is_ok();
    }
    // A rename that refuses to replace.
    let (a, b, c) = (dir.join("a"), dir.join("b"), dir.join("c"));
    if touch(&a) && touch(&b) {
        p.no_replace_rename = match fsops::rename_noreplace(&a, &b) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                matches!(fsops::rename_noreplace(&a, &c), Ok(true))
            }
            _ => false,
        };
    }
    // Hard links.
    let src = if c.exists() { c } else { a };
    p.hard_links = std::fs::hard_link(&src, dir.join("d")).is_ok();
    // The mtime resolution: set a time with 1_234_567 ns past a whole two seconds and
    // see what is kept.
    p.mtime_granularity_ns = granularity(&dir.join("t"));
}

/// The modification-time resolution of the filesystem `probe` (a new file) is on.
fn granularity(probe: &Path) -> i64 {
    const FRACTION: u64 = 1_234_567;
    // Coarse when unknown: a safe guess for the coarse-mtime bump.
    const UNKNOWN: i64 = 2_000_000_000;
    let Ok(f) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(probe)
    else {
        return UNKNOWN;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let base = Duration::from_secs(now - now % 2);
    let set = UNIX_EPOCH + base + Duration::from_nanos(FRACTION);
    if f.set_modified(set).is_err() {
        return UNKNOWN;
    }
    drop(f);
    let Ok(got) = std::fs::metadata(probe).and_then(|m| m.modified()) else {
        return UNKNOWN;
    };
    let set_ns = base.as_nanos() as i128 + i128::from(FRACTION as i64);
    let got_ns = match got.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(_) => return UNKNOWN,
    };
    [
        1i128,
        100,
        1_000,
        1_000_000,
        10_000_000,
        1_000_000_000,
        2_000_000_000,
    ]
    .into_iter()
    .find(|g| set_ns - set_ns.rem_euclid(*g) == got_ns)
    .map_or(UNKNOWN, |g| g as i64)
}

/// A folder Bunko cannot write: guess whether names fold from one already there.
fn read_only_guess(root: &Path, p: &mut FolderProbe, h: &mut Hidden) {
    p.mtime_granularity_ns = 1_000_000;
    let folds = std::fs::read_dir(root).ok().and_then(|rd| {
        rd.flatten().find_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let swapped: String = name
                .chars()
                .map(|c| {
                    if c.is_ascii_lowercase() {
                        c.to_ascii_uppercase()
                    } else {
                        c.to_ascii_lowercase()
                    }
                })
                .collect();
            if swapped == name {
                return None;
            }
            let other = root.join(&swapped);
            Some(fsops::same_file(&e.path(), &other).unwrap_or(false))
        })
    });
    p.folds_case = folds.unwrap_or(cfg!(any(windows, target_os = "macos")));
    h.folds_unicode = cfg!(target_os = "macos");
}
