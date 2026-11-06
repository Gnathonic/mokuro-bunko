//! Filesystem helpers with Python's exact numeric semantics.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use bunko_store::fs::{self, File, OpenOptions};
use bunko_store::{Durability, Meta, Mode, Origin, Version, WriteIntent};

/// `os.stat(...).st_mtime`: CPython computes `sec + nsec * 1e-9` in doubles;
/// the entry cache compares it EXACTLY and embeds its `repr` in keys.
pub fn py_mtime(meta: &Meta) -> f64 {
    match meta.unix_mtime() {
        Some((secs, nanos)) => secs as f64 + nanos as f64 * 1e-9,
        None => match meta
            .modified()
            .map(|time| time.duration_since(std::time::UNIX_EPOCH))
        {
            Ok(Ok(duration)) => {
                duration.as_secs() as f64 + f64::from(duration.subsec_nanos()) * 1e-9
            }
            Ok(Err(before)) => {
                let duration = before.duration();
                let secs = -(duration.as_secs() as f64) - 1.0;
                let nanos = 1e9 - f64::from(duration.subsec_nanos());
                secs + nanos * 1e-9
            }
            Err(_) => 0.0,
        },
    }
}

/// `int(st_mtime)` (truncation toward zero).
pub fn py_mtime_int(meta: &Meta) -> i64 {
    py_mtime(meta).trunc() as i64
}

/// `st_size` as Python's int.
pub fn size_of(meta: &Meta) -> i64 {
    i64::try_from(meta.len()).unwrap_or(i64::MAX)
}

/// `Path.stat()` (follows symlinks).
pub fn stat(path: &Path) -> Option<Meta> {
    fs::metadata(path).ok()
}

/// `Path.is_file()` / `is_dir()` (follow symlinks, errors are `false`).
pub fn is_file(path: &Path) -> bool {
    stat(path).is_some_and(|meta| meta.is_file())
}

pub fn is_dir(path: &Path) -> bool {
    stat(path).is_some_and(|meta| meta.is_dir())
}

/// A directory listing as `(name, path)`, names that are not valid UTF-8
/// skipped with a warning (0.5.2 carried them as lone surrogates; a Rust
/// `String` cannot, and such a name cannot be served over the UTF-8 WebDAV
/// paths either).
pub fn list_dir(dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        match entry.file_name().into_string() {
            Ok(name) => out.push((name, entry.path())),
            Err(raw) => {
                tracing::warn!(dir = %dir.display(), name = ?raw, "skipping a non-UTF-8 file name")
            }
        }
    }
    Ok(out)
}

/// The names listed in `dir` (among `names`) that another listed spelling shadows: in
/// each group that folds alike (only a case-sensitive disk or a remote store holds
/// one), all but the store's case winner, else the byte-smallest
/// ([`bunko_core::sidecar_names::case_losers`], the rule PathCase resolves requests
/// by). No request reaches them: the catalog and the library walks leave them out, and
/// nothing is ever written, moved or deleted for them. Doctor reports them.
pub fn case_losers<'a>(
    dir: &Path,
    names: impl IntoIterator<Item = &'a str>,
) -> std::collections::HashSet<String> {
    let store = bunko_store::store_for(dir);
    bunko_core::sidecar_names::case_losers(names, |fold| store.as_ref()?.case_winner(dir, fold))
}

/// [`list_dir`] without the [`case_losers`]: the entries a request can reach.
pub fn list_dir_reachable(dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let entries = list_dir(dir)?;
    let losers = case_losers(dir, entries.iter().map(|(n, _)| n.as_str()));
    Ok(if losers.is_empty() {
        entries
    } else {
        entries
            .into_iter()
            .filter(|(n, _)| !losers.contains(n))
            .collect()
    })
}

/// The winner among `variants`, listed spellings in `dir` of the fold `folded`: the
/// store's case winner when it is one of them, else the byte-smallest (PathCase's rule,
/// so what Bunko reads and writes is what a request reaches). `None` when empty.
pub fn pick_winner(dir: &Path, folded: &str, variants: Vec<String>) -> Option<String> {
    if variants.len() > 1
        && let Some(w) = bunko_store::store_for(dir).and_then(|s| s.case_winner(dir, folded))
        && variants.contains(&w)
    {
        return Some(w);
    }
    variants.into_iter().min()
}

/// The spelling `dir` lists of the name `name` in any case (the [`pick_winner`] of
/// several), or `None` when no entry folds like it (or `dir` cannot be listed).
pub fn listed_name(dir: &Path, name: &str) -> Option<String> {
    let folded = bunko_core::sidecar_names::fold(name);
    let variants: Vec<String> = list_dir(dir)
        .ok()?
        .into_iter()
        .map(|(n, _)| n)
        .filter(|n| n == name || bunko_core::sidecar_names::fold(n) == folded)
        .collect();
    pick_winner(dir, &folded, variants)
}

/// `dir/name` in the spelling `dir` lists ([`listed_name`]); the requested spelling when
/// nothing there folds like it. What Bunko reads and writes for a name it builds itself
/// (`series.json`, `catalog.json`): never a second spelling of an existing entry.
pub fn spelled(dir: &Path, name: &str) -> PathBuf {
    dir.join(listed_name(dir, name).unwrap_or_else(|| name.to_string()))
}

fn temp_suffix() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    let mut value = hasher.finish();
    (0..8)
        .map(|_| {
            let c = ALPHABET[(value % ALPHABET.len() as u64) as usize];
            value /= ALPHABET.len() as u64;
            char::from(c)
        })
        .collect()
}

fn create_temp(dir: &Path, name: &str) -> io::Result<(File, PathBuf)> {
    for _ in 0..100 {
        let temp = dir.join(format!(".{name}.compile-{}.tmp", temp_suffix()));
        let mut options = OpenOptions::new();
        // 0o666 & ~umask, applied by the kernel on unix: what 0.5.2's chmod
        // after `mkstemp` arrived at (mkstemp's 0600 would break nginx-served
        // downloads). Ignored elsewhere.
        options.write(true).create_new(true).mode(0o666);
        match options.open(&temp) {
            Ok(file) => return Ok((file, temp)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free temporary file name",
    ))
}

/// `atomic_write_bytes`: temp file in the same directory, write, fsync,
/// rename over the target (a reader mid-GET never sees half a document).
pub fn atomic_write_bytes(path: &Path, data: &[u8]) -> io::Result<()> {
    atomic_write_bytes_with(path, data, None)
}

/// [`atomic_write_bytes`], publishing only over `base` when it is set (the
/// version of the file the caller compared against; spec §3.8). The final step
/// is a store `publish`, which for a local or unmounted path is the rename.
pub fn atomic_write_bytes_with(path: &Path, data: &[u8], base: Option<Version>) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (mut file, temp) = create_temp(dir, &name)?;
    let result = (|| {
        file.write_all(data)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::publish(
            &temp,
            path,
            &WriteIntent::new(
                Mode::Replace { base },
                Origin::Compiler,
                Durability::Deferred,
            ),
        )
        .map(|_| ())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
