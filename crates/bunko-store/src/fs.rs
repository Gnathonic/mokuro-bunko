//! A `std::fs` mirror routed through the mount table: a path under a mounted store
//! goes to that store, any other path straight to `std::fs`.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub use crate::file::{DirEntry, File, OpenOptions, ReadDir};
use crate::meta::Meta;
use crate::mount::{route, same_store, store_for};
use crate::store::{Published, WriteIntent, crosses_devices};

pub fn metadata(p: impl AsRef<Path>) -> io::Result<Meta> {
    let p = p.as_ref();
    route(p, |s| s.metadata(p), || std::fs::metadata(p).map(Meta::Std))
}

pub fn symlink_metadata(p: impl AsRef<Path>) -> io::Result<Meta> {
    let p = p.as_ref();
    route(
        p,
        |s| s.symlink_metadata(p),
        || std::fs::symlink_metadata(p).map(Meta::Std),
    )
}

pub fn read_dir(p: impl AsRef<Path>) -> io::Result<ReadDir> {
    let p = p.as_ref();
    route(
        p,
        |s| s.read_dir(p),
        || std::fs::read_dir(p).map(ReadDir::Std),
    )
}

pub fn canonicalize(p: impl AsRef<Path>) -> io::Result<PathBuf> {
    let p = p.as_ref();
    route(p, |s| s.canonicalize(p), || std::fs::canonicalize(p))
}

pub fn read_link(p: impl AsRef<Path>) -> io::Result<PathBuf> {
    let p = p.as_ref();
    route(p, |s| s.read_link(p), || std::fs::read_link(p))
}

/// `Path::exists`: follows symlinks; any error is `false`.
pub fn exists(p: impl AsRef<Path>) -> bool {
    metadata(p).is_ok()
}

/// `Path::try_exists`.
pub fn try_exists(p: impl AsRef<Path>) -> io::Result<bool> {
    match metadata(p) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// `Path::is_file`.
pub fn is_file(p: impl AsRef<Path>) -> bool {
    metadata(p).is_ok_and(|m| m.is_file())
}

/// `Path::is_dir`.
pub fn is_dir(p: impl AsRef<Path>) -> bool {
    metadata(p).is_ok_and(|m| m.is_dir())
}

pub(crate) fn open(p: &Path) -> io::Result<File> {
    route(p, |s| s.open(p), || std::fs::File::open(p).map(File::Std))
}

pub(crate) fn open_with(p: &Path, o: &OpenOptions) -> io::Result<File> {
    route(
        p,
        |s| s.open_with(p, o),
        || o.to_std().open(p).map(File::Std),
    )
}

pub fn read(p: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let p = p.as_ref();
    match store_for(p) {
        None => std::fs::read(p),
        Some(s) if !s.caps().remote => std::fs::read(p),
        Some(s) => {
            let mut v = Vec::new();
            s.open(p)?.read_to_end(&mut v)?;
            Ok(v)
        }
    }
}

pub fn read_to_string(p: impl AsRef<Path>) -> io::Result<String> {
    let p = p.as_ref();
    match store_for(p) {
        None => std::fs::read_to_string(p),
        Some(s) if !s.caps().remote => std::fs::read_to_string(p),
        Some(s) => {
            let mut v = String::new();
            s.open(p)?.read_to_string(&mut v)?;
            Ok(v)
        }
    }
}

/// `std::fs::write`. On a remote store: one Sync publish through a staging file
/// (spec §3.4); nothing is ever published on drop.
pub fn write(p: impl AsRef<Path>, data: impl AsRef<[u8]>) -> io::Result<()> {
    let p = p.as_ref();
    match store_for(p) {
        None => std::fs::write(p, data),
        Some(s) if !s.caps().remote => std::fs::write(p, data),
        Some(s) => {
            let mut o = OpenOptions::new();
            o.write(true).create(true).truncate(true);
            let mut f = s.open_with(p, &o)?;
            f.write_all(data.as_ref())?;
            f.flush()?;
            drop(f);
            s.publish(p, p, &WriteIntent::default_publish()).map(|_| ())
        }
    }
}

pub fn create_dir(p: impl AsRef<Path>) -> io::Result<()> {
    let p = p.as_ref();
    route(p, |s| s.create_dir(p), || std::fs::create_dir(p))
}

pub fn create_dir_all(p: impl AsRef<Path>) -> io::Result<()> {
    let p = p.as_ref();
    route(p, |s| s.create_dir_all(p), || std::fs::create_dir_all(p))
}

pub fn remove_file(p: impl AsRef<Path>) -> io::Result<()> {
    let p = p.as_ref();
    route(p, |s| s.remove_file(p), || std::fs::remove_file(p))
}

pub fn remove_dir(p: impl AsRef<Path>) -> io::Result<()> {
    let p = p.as_ref();
    route(p, |s| s.remove_dir(p), || std::fs::remove_dir(p))
}

pub fn remove_dir_all(p: impl AsRef<Path>) -> io::Result<()> {
    let p = p.as_ref();
    route(p, |s| s.remove_dir_all(p), || std::fs::remove_dir_all(p))
}

/// `std::fs::rename` (spec §3.4): within one store, the store's `rename`; from an
/// unmounted path into a store, `publish` with the default intent; between two
/// different stores, `CrossesDevices`.
///
/// A local rename between two names of one file (a case-only rename on a
/// case-insensitive disk) goes through a temporary name, see [`local_rename`].
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    match (store_for(from), store_for(to)) {
        (None, None) => local_rename(from, to),
        (Some(a), Some(b)) if same_store(&a, &b) => a.rename(from, to),
        (None, Some(b)) => b
            .publish(from, to, &WriteIntent::default_publish())
            .map(|_| ()),
        (Some(a), None) if !a.caps().remote => local_rename(from, to),
        _ => Err(crosses_devices()),
    }
}

/// Do `a` and `b` name one existing file or folder (Python `os.path.samefile`)? `false`
/// when either is missing.
pub fn same_file(a: &Path, b: &Path) -> bool {
    match (metadata(a), metadata(b)) {
        (Ok(x), Ok(y)) => match (x.dev().zip(x.ino()), y.dev().zip(y.ino())) {
            (Some(p), Some(q)) => p == q,
            // Windows: the final path of an open handle carries the on-disk spelling.
            _ => matches!(
                (canonicalize(a), canonicalize(b)),
                (Ok(x), Ok(y)) if x == y
            ),
        },
        _ => false,
    }
}

/// The prefix of the temporary name a case-only rename passes through.
pub const CASE_RENAME_PREFIX: &str = ".bunko-case-";

/// `std::fs::rename`, except between two names of one file. POSIX makes that a no-op
/// (Linux casefold, hard links), while APFS and NTFS rename. Bunko behaves the same on
/// every host: when `from` and `to` differ but [`same_file`] holds, a plain rename is
/// tried first and its result read back from the listing (never a probe of the host);
/// when `to` is not listed yet, or `from` still is, the entry goes through a hidden
/// `.bunko-case-<random>-<to's name>` (`.bunko-case-<random>` when that would be too
/// long a name) in `from`'s folder, then to `to`. When the second step fails the first
/// is undone and its error returned. A crash between the two leaves the temporary name
/// behind (doctor reports it). Two hard links of one file end as the one name `to`.
pub(crate) fn local_rename(from: &Path, to: &Path) -> io::Result<()> {
    if from == to || !same_file(from, to) {
        return std::fs::rename(from, to);
    }
    let (Some(parent), Some(from_name), Some(name)) =
        (from.parent(), from.file_name(), to.file_name())
    else {
        return std::fs::rename(from, to);
    };
    let to_parent = to.parent().unwrap_or(parent);
    std::fs::rename(from, to)?;
    if listed(to_parent, name) && !listed(parent, from_name) {
        return Ok(());
    }
    let mut temp_name = format!("{CASE_RENAME_PREFIX}{}", temp_token());
    let full = format!("{temp_name}-{}", name.to_string_lossy());
    // 255 bytes is the longest name APFS, ext4 and NTFS (in UTF-16 units) take.
    if full.len() <= 255 {
        temp_name = full;
    }
    let temp = parent.join(temp_name);
    std::fs::rename(from, &temp)?;
    if let Err(e) = std::fs::rename(&temp, to) {
        if let Err(back) = std::fs::rename(&temp, from) {
            tracing::error!(
                temp = %temp.display(),
                from = %from.display(),
                error = %back,
                "a case-only rename failed and could not be undone: rename the temporary name back by hand"
            );
        }
        return Err(e);
    }
    // Hard links: the second step was itself a rename between two names of one file.
    if listed(parent, temp.file_name().unwrap_or_default()) && same_file(&temp, to) {
        std::fs::remove_file(&temp)?;
    }
    Ok(())
}

/// `dir` lists exactly `name`.
fn listed(dir: &Path, name: &std::ffi::OsStr) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| entries.flatten().any(|e| e.file_name() == name))
}

/// A short token unique enough for a temporary name in one folder.
fn temp_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}{:x}{:x}", std::process::id(), nanos, n)
}

/// `std::fs::copy`, routed by the destination's store.
pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    let (from, to) = (from.as_ref(), to.as_ref());
    match (store_for(from), store_for(to)) {
        (_, Some(s)) => s.copy(from, to),
        (None, None) => std::fs::copy(from, to),
        (Some(s), None) if !s.caps().remote => std::fs::copy(from, to),
        (Some(s), None) => {
            let mut src = s.open(from)?;
            let mut dst = std::fs::File::create(to)?;
            io::copy(&mut src, &mut dst)
        }
    }
}

/// Commit a staged file as `dest` (spec D1). For an unmounted destination, a rename.
/// [`LibraryStore::writable`] of the store holding `p` (unmounted: `Ok`).
///
/// [`LibraryStore::writable`]: crate::LibraryStore::writable
pub fn writable(p: impl AsRef<Path>) -> io::Result<()> {
    let p = p.as_ref();
    match store_for(p) {
        Some(s) => s.writable(p),
        None => Ok(()),
    }
}

pub fn publish(
    staged: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    w: &WriteIntent,
) -> io::Result<Published> {
    let (staged, dest) = (staged.as_ref(), dest.as_ref());
    match store_for(dest) {
        Some(s) => s.publish(staged, dest, w),
        None => std::fs::rename(staged, dest).map(|_| Published::default()),
    }
}
