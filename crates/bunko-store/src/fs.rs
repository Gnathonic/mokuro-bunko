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
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    match (store_for(from), store_for(to)) {
        (None, None) => std::fs::rename(from, to),
        (Some(a), Some(b)) if same_store(&a, &b) => a.rename(from, to),
        (None, Some(b)) => b
            .publish(from, to, &WriteIntent::default_publish())
            .map(|_| ()),
        (Some(a), None) if !a.caps().remote => std::fs::rename(from, to),
        _ => Err(crosses_devices()),
    }
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
