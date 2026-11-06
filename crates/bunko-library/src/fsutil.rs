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

/// The fold groups of `dir`'s listing `entries` (`(name, is_dir)`): which names no
/// request reaches and which the freeze holds
/// ([`bunko_core::fold_groups::FoldGroups`]), the store's case winner preferred.
pub fn fold_groups<'a>(
    dir: &Path,
    entries: impl IntoIterator<Item = (&'a str, bool)>,
) -> bunko_core::fold_groups::FoldGroups {
    let store = bunko_store::store_for(dir);
    bunko_core::fold_groups::FoldGroups::new(entries, |fold| store.as_ref()?.case_winner(dir, fold))
}

/// `dir`'s UTF-8 entries as `(name, is_dir)`, and their [`fold_groups`]. At a library
/// root (`top`) the reader's per-user names are left out: they are never library
/// entries (a series spelled like one is [`bunko_core::fold_groups::Frozen::Reserved`]).
/// `None` when `dir` cannot be listed.
pub fn listed_groups(
    dir: &Path,
    top: bool,
) -> Option<(Vec<(String, bool)>, bunko_core::fold_groups::FoldGroups)> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if top && bunko_core::reader_tree::is_per_user_root_name(&name) {
            continue;
        }
        entries.push((name, entry.file_type().is_ok_and(|t| t.is_dir())));
    }
    let groups = fold_groups(dir, entries.iter().map(|(n, d)| (n.as_str(), *d)));
    Some((entries, groups))
}

/// Where the library path `rel` (below the library root `library`) meets the freeze:
/// a per-user name at the root, or a segment a case-variant group freezes
/// ([`bunko_core::fold_groups::frozen_along`]). Nothing Bunko does on its own (OCR,
/// covers, sweeps, adoption, record keeping) writes, moves, deletes or re-keys such a
/// path. From the listings alone: the same on every host.
pub fn frozen(library: &Path, rel: &str) -> Option<bunko_core::fold_groups::Frozen> {
    bunko_core::fold_groups::frozen_along(rel, true, |folder| {
        let dir = if folder.is_empty() {
            library.to_path_buf()
        } else {
            library.join(folder)
        };
        listed_groups(&dir, folder.is_empty()).map(|(_, groups)| groups)
    })
}

/// [`frozen`] for a path below `library` (`None` for one outside it).
pub fn frozen_path(library: &Path, path: &Path) -> Option<bunko_core::fold_groups::Frozen> {
    let rel = path.strip_prefix(library).ok()?;
    let rel = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    frozen(library, &rel)
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

/// Where a library-relative path leads through the folder listings alone
/// ([`ListedWalk::walk`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListedPath {
    /// Every segment names a listed entry, each through the winner of its spelling group
    /// (the entry a request reaches): the path in listed spelling.
    Reaches(String),
    /// Some segment is spelled exactly like a listed entry another spelling shadows: the
    /// path is that hidden entry's (or below it), never the winner's.
    Shadowed,
    /// Some segment names nothing listed (or a per-user name at the root).
    Missing,
}

/// Resolves library-relative paths segment by segment from listings, never by opening a
/// path by name, so the answer is the same on every host: a case-sensitive disk, a
/// case-insensitive one and a remote store alike. Each folder is listed once per walker.
pub struct ListedWalk {
    root: PathBuf,
    listings: std::cell::RefCell<std::collections::HashMap<PathBuf, Option<Vec<String>>>>,
}

impl ListedWalk {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            listings: Default::default(),
        }
    }

    /// The UTF-8 names `dir` lists; `None` when it is gone or no folder; `Err` when it
    /// cannot be read.
    fn names(&self, dir: &Path) -> io::Result<Option<Vec<String>>> {
        if let Some(cached) = self.listings.borrow().get(dir) {
            return Ok(cached.clone());
        }
        let names = match list_dir(dir) {
            Ok(entries) => Some(entries.into_iter().map(|(n, _)| n).collect()),
            Err(e)
                if dir != self.root
                    && matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
            {
                None
            }
            Err(e) => return Err(e),
        };
        self.listings
            .borrow_mut()
            .insert(dir.to_path_buf(), names.clone());
        Ok(names)
    }

    /// Where `rel` leads: each segment takes the winner of the listed names that fold
    /// like it (PathCase's rule), unless the segment is spelled exactly like a listed
    /// loser ([`ListedPath::Shadowed`]). `Err` when a folder on the way cannot be read.
    pub fn walk(&self, rel: &str) -> io::Result<ListedPath> {
        let mut dir = self.root.clone();
        let mut resolved: Vec<String> = Vec::new();
        for (position, segment) in rel.split('/').filter(|s| !s.is_empty()).enumerate() {
            let top = position == 0;
            if segment == "." || segment == ".." {
                return Ok(ListedPath::Missing);
            }
            if top && bunko_core::reader_tree::is_per_user_root_name(segment) {
                return Ok(ListedPath::Missing);
            }
            let Some(names) = self.names(&dir)? else {
                return Ok(ListedPath::Missing);
            };
            let folded = bunko_core::sidecar_names::fold(segment);
            let variants: Vec<String> = names
                .iter()
                .filter(|n| !(top && bunko_core::reader_tree::is_per_user_root_name(n)))
                .filter(|n| *n == segment || bunko_core::sidecar_names::fold(n) == folded)
                .cloned()
                .collect();
            let exact = variants.iter().any(|n| n == segment);
            let Some(winner) = pick_winner(&dir, &folded, variants) else {
                return Ok(ListedPath::Missing);
            };
            if exact && winner != segment {
                return Ok(ListedPath::Shadowed);
            }
            dir.push(&winner);
            resolved.push(winner);
        }
        Ok(ListedPath::Reaches(resolved.join("/")))
    }

    /// Does the stored path `stored` (a database key, in the spelling Bunko wrote) name
    /// the entry `target` reaches, and not a separate one? True when `stored` leads, by
    /// listings, to the very path `target` does: it is that entry spelled otherwise (a
    /// respelling outside Bunko), never a case variant listed beside it. Unreadable
    /// folders decide nothing (false).
    pub fn same_entry(&self, stored: &str, target: &str) -> bool {
        match (self.walk(stored), self.walk(target)) {
            (Ok(ListedPath::Reaches(a)), Ok(ListedPath::Reaches(b))) => a == b,
            _ => false,
        }
    }

    /// Does the stored path `stored` lead to no listed entry at all (neither one a
    /// request reaches nor a shadowed one): a row left behind? Unreadable folders decide
    /// nothing (false).
    pub fn leads_nowhere(&self, stored: &str) -> bool {
        matches!(self.walk(stored), Ok(ListedPath::Missing))
    }
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
