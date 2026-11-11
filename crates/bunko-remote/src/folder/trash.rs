//! The in-folder trash (D16): `<root>/.mokuro-bunko-trash/<UTC stamp>-<op8>/<path>`.
//!
//! Always on the library's own device, so every step is a rename. Every trashed file
//! is renamed to `<name>.bunko-trashed`, and the trash folder's name is reserved: no
//! reader lists anything in it (readers filter by basename suffix). A file goes in one
//! rename; a folder first has every file below it renamed in place (deepest first),
//! then moves whole. Re-running after a crash finishes the job. Batches older than
//! [`KEEP_DAYS`] are purged at connect and daily.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::backend::BErr;
use crate::names::{FOLDER_TRASH_DIR, TRASHED_SUFFIX};

use super::fsops::{self, Act, io_err};

/// How long a trashed batch stays recoverable.
pub(crate) const KEEP_DAYS: u64 = 30;

/// `yyyymmddThhmmssZ` (UTC) of `t`.
pub(crate) fn stamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

/// The unix seconds a batch name `yyyymmddThhmmssZ-…` starts with.
pub(crate) fn parse_stamp(name: &str) -> Option<i64> {
    let s = name.get(..16)?;
    let b = s.as_bytes();
    if b[8] != b'T' || b[15] != b'Z' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(r)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let (y, mo, d) = (num(0..4)?, num(4..6)?, num(6..8)?);
    let (h, mi, se) = (num(9..11)?, num(11..13)?, num(13..15)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se)
}

// Howard Hinnant's civil-calendar algorithms (proleptic Gregorian, UTC).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The trash folder of the library at `root`, created (and hidden on Windows).
fn trash_dir(root: &Path) -> Result<PathBuf, BErr> {
    let dir = root.join(FOLDER_TRASH_DIR);
    match std::fs::create_dir(&dir) {
        Ok(()) => fsops::set_hidden(&dir),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io_err(&e, Act::Create, FOLDER_TRASH_DIR)),
    }
    Ok(dir)
}

/// Rename the file `abs` in place to `<name>.bunko-trashed` (unless it already ends
/// so); a name already taken there gets a token before the suffix. Returns the new
/// path when it renamed.
fn suffix_in_place(abs: &Path, token: &str, rel: &str) -> Result<Option<PathBuf>, BErr> {
    let Some(name) = abs.file_name().and_then(|n| n.to_str()) else {
        return Ok(None);
    };
    if name.to_ascii_lowercase().ends_with(TRASHED_SUFFIX) {
        return Ok(None);
    }
    let first = abs.with_file_name(format!("{name}{TRASHED_SUFFIX}"));
    match super::rename_new(abs, &first, false) {
        Ok(()) => Ok(Some(first)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let other = abs.with_file_name(format!("{name}~{token}{TRASHED_SUFFIX}"));
            super::rename_new(abs, &other, false).map_err(|e| io_err(&e, Act::Rename, rel))?;
            Ok(Some(other))
        }
        Err(e) => Err(io_err(&e, Act::Rename, rel)),
    }
}

/// A folder's trash stopped before the folder moved: give the files renamed in place
/// their names back, so the folder is as it was and not emptied in readers' eyes (an
/// outbox retry renames them again). Best effort; what cannot go back stays suffixed.
fn undo_suffixes(done: &[(PathBuf, PathBuf)]) {
    for (orig, now) in done.iter().rev() {
        if let Err(e) = super::rename_new(now, orig, false) {
            tracing::warn!(
                "folder: {} keeps its trash name after a failed trash: {e}",
                now.display()
            );
        }
    }
}

/// Every non-folder entry below `dir` (symbolic links are not followed), deepest
/// first.
fn files_below(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let t = e.file_type()?;
            if t.is_dir() {
                stack.push((e.path(), depth + 1));
            } else {
                out.push((depth + 1, e.path()));
            }
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.0));
    Ok(out.into_iter().map(|(_, p)| p).collect())
}

/// Move `pid` (at `abs`) into a new trash batch. The caller checked that it exists.
pub(crate) fn trash(
    root: &Path,
    pid: &str,
    abs: &Path,
    is_dir: bool,
    now: SystemTime,
    token: &str,
) -> Result<(), BErr> {
    let batch = trash_dir(root)?.join(format!("{}-{token}", stamp(now)));
    let mut dest = batch.clone();
    for seg in pid.split('/') {
        dest.push(seg);
    }
    let mut done = Vec::new();
    if is_dir {
        let files = files_below(abs).map_err(|e| io_err(&e, Act::Read, pid))?;
        for f in files {
            match suffix_in_place(&f, token, pid) {
                Ok(Some(now)) => done.push((f, now)),
                Ok(None) => {}
                Err(e) => {
                    undo_suffixes(&done);
                    return Err(e);
                }
            }
        }
    } else {
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        dest.set_file_name(format!("{name}{TRASHED_SUFFIX}"));
    }
    let moved = dest
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .map_err(|e| io_err(&e, Act::Create, FOLDER_TRASH_DIR))
        .and_then(|()| std::fs::rename(abs, &dest).map_err(|e| io_err(&e, Act::Rename, pid)));
    if let Err(e) = moved {
        undo_suffixes(&done);
        return Err(e);
    }
    if let Some(parent) = abs.parent() {
        fsops::fsync_dir(parent);
    }
    Ok(())
}

/// Remove the trash batches older than [`KEEP_DAYS`]; a batch whose name is not a
/// stamp is left alone. Returns how many went.
pub(crate) fn purge(root: &Path, now: SystemTime) -> usize {
    let dir = root.join(FOLDER_TRASH_DIR);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let now_s = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let keep = Duration::from_secs(KEEP_DAYS * 86_400).as_secs() as i64;
    let mut gone = 0;
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(at) = name.to_str().and_then(parse_stamp) else {
            continue;
        };
        if now_s - at <= keep {
            continue;
        }
        let r = if e.file_type().is_ok_and(|t| t.is_dir()) {
            std::fs::remove_dir_all(e.path())
        } else {
            std::fs::remove_file(e.path())
        };
        match r {
            Ok(()) => gone += 1,
            Err(err) => tracing::warn!(batch = ?name, "folder: cannot purge a trash batch: {err}"),
        }
    }
    gone
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_round_trip() {
        for secs in [0u64, 951_782_400, 1_760_000_000, 4_102_444_800] {
            let t = UNIX_EPOCH + Duration::from_secs(secs);
            let s = stamp(t);
            assert_eq!(
                parse_stamp(&format!("{s}-deadbeef")),
                Some(secs as i64),
                "{s}"
            );
        }
        assert_eq!(
            stamp(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "20000229T000000Z"
        );
        for bad in [
            "",
            "2020",
            "20201301T000000Z-x",
            "2020010lT000000Z",
            "20200101 000000Z",
        ] {
            assert_eq!(parse_stamp(bad), None, "{bad}");
        }
    }
}
