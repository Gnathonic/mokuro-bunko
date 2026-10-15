//! [`LocalStore`]: today's on-disk library, every call forwarded to `std::fs`.

use std::io;
use std::path::{Path, PathBuf};

use crate::file::{File, OpenOptions, ReadDir};
use crate::meta::{Meta, Version};
use crate::store::{Caps, LibraryStore, LocalPath, Published, WatchGuard, WatchSink, WriteIntent};
use crate::watch::Translate;

/// The local library (spec §4). No behaviour of its own: each method is the std call
/// the code made before the seam existed.
pub struct LocalStore {
    root: PathBuf,
    translate: Option<Translate>,
}

impl LocalStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        LocalStore {
            root: root.into(),
            translate: None,
        }
    }

    /// A store whose [`LibraryStore::watch`] turns backend events into
    /// [`crate::WatchEvent`]s with `translate`.
    pub fn with_translator(root: impl Into<PathBuf>, translate: Translate) -> Self {
        LocalStore {
            root: root.into(),
            translate: Some(translate),
        }
    }
}

impl LibraryStore for LocalStore {
    fn root(&self) -> &Path {
        &self.root
    }

    fn caps(&self) -> Caps {
        Caps {
            remote: false,
            case_sensitive: false,
            symlinks: true,
            local_paths: true,
            atomic_replace: true,
            server_copy: true,
            trash_on_delete: false,
        }
    }

    fn metadata(&self, p: &Path) -> io::Result<Meta> {
        std::fs::metadata(p).map(Meta::Std)
    }

    fn symlink_metadata(&self, p: &Path) -> io::Result<Meta> {
        std::fs::symlink_metadata(p).map(Meta::Std)
    }

    fn read_dir(&self, p: &Path) -> io::Result<ReadDir> {
        std::fs::read_dir(p).map(ReadDir::Std)
    }

    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(p)
    }

    fn read_link(&self, p: &Path) -> io::Result<PathBuf> {
        std::fs::read_link(p)
    }

    fn open(&self, p: &Path) -> io::Result<File> {
        std::fs::File::open(p).map(File::Std)
    }

    fn local_path(&self, p: &Path, _expect: Option<&Version>) -> io::Result<LocalPath> {
        Ok(LocalPath::InPlace(p.to_path_buf()))
    }

    fn open_with(&self, p: &Path, o: &OpenOptions) -> io::Result<File> {
        o.to_std().open(p).map(File::Std)
    }

    fn publish(&self, staged: &Path, dest: &Path, _w: &WriteIntent) -> io::Result<Published> {
        std::fs::rename(staged, dest).map(|_| Published::default())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        std::fs::copy(from, to)
    }

    fn remove_file(&self, p: &Path) -> io::Result<()> {
        std::fs::remove_file(p)
    }

    fn remove_dir(&self, p: &Path) -> io::Result<()> {
        std::fs::remove_dir(p)
    }

    fn remove_dir_all(&self, p: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(p)
    }

    fn create_dir(&self, p: &Path) -> io::Result<()> {
        std::fs::create_dir(p)
    }

    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        std::fs::create_dir_all(p)
    }

    fn watch(&self, sink: WatchSink) -> io::Result<WatchGuard> {
        crate::watch::watch_local(&self.root, self.translate.as_ref(), sink)
    }
}
