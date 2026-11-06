//! [`UnconnectedStore`]: the library of a cloud store that is not connected yet.

use std::io;
use std::path::{Path, PathBuf};

use bunko_store::{
    Caps, File, Health, LibraryStore, LocalPath, Meta, OpenOptions, Published, ReadDir, RemoteMeta,
    Version, WatchGuard, WatchSink, WriteIntent,
};

/// An empty, read-only library mounted at a cloud library's synthetic root while its
/// store cannot be opened (not signed in, no password). The root is an empty folder,
/// everything else is `NotFound`, every write fails `NotConnected` with the reason, and
/// health is `NeedsReauth`. Nothing ever touches the disk.
pub struct UnconnectedStore {
    root: PathBuf,
    why: String,
}

impl UnconnectedStore {
    pub fn new(root: impl Into<PathBuf>, why: &str) -> UnconnectedStore {
        UnconnectedStore {
            root: root.into(),
            why: why.to_string(),
        }
    }

    fn is_root(&self, p: &Path) -> bool {
        p.strip_prefix(&self.root)
            .is_ok_and(|rest| rest.components().all(|c| c == std::path::Component::CurDir))
    }

    fn not_found(&self, p: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{}: the cloud library is not connected", p.display()),
        )
    }

    fn refused(&self) -> io::Error {
        io::Error::new(
            io::ErrorKind::NotConnected,
            format!("the cloud library is not connected: {}", self.why),
        )
    }
}

impl LibraryStore for UnconnectedStore {
    fn root(&self) -> &Path {
        &self.root
    }

    fn caps(&self) -> Caps {
        Caps {
            remote: true,
            case_sensitive: true,
            ..Caps::default()
        }
    }

    fn health(&self) -> Health {
        Health::NeedsReauth
    }

    fn metadata(&self, p: &Path) -> io::Result<Meta> {
        if !self.is_root(p) {
            return Err(self.not_found(p));
        }
        Ok(Meta::Remote(RemoteMeta {
            size: 0,
            mtime_ns: 0,
            ctime_s: 0,
            is_dir: true,
            dev: 0,
            ino: 1,
            cver: None,
            own: false,
            sha256: None,
        }))
    }

    fn symlink_metadata(&self, p: &Path) -> io::Result<Meta> {
        self.metadata(p)
    }

    fn read_dir(&self, p: &Path) -> io::Result<ReadDir> {
        self.metadata(p)?;
        Ok(ReadDir::Remote(Vec::new().into_iter()))
    }

    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        self.metadata(p)?;
        Ok(self.root.clone())
    }

    fn read_link(&self, p: &Path) -> io::Result<PathBuf> {
        self.metadata(p)?;
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a remote library has no symlinks",
        ))
    }

    fn open(&self, p: &Path) -> io::Result<File> {
        Err(self.not_found(p))
    }

    fn local_path(&self, p: &Path, _expect: Option<&Version>) -> io::Result<LocalPath> {
        Err(self.not_found(p))
    }

    fn open_with(&self, _p: &Path, _o: &OpenOptions) -> io::Result<File> {
        Err(self.refused())
    }

    fn publish(&self, _staged: &Path, _dest: &Path, _w: &WriteIntent) -> io::Result<Published> {
        Err(self.refused())
    }

    fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Err(self.refused())
    }

    fn copy(&self, _from: &Path, _to: &Path) -> io::Result<u64> {
        Err(self.refused())
    }

    fn remove_file(&self, _p: &Path) -> io::Result<()> {
        Err(self.refused())
    }

    fn remove_dir(&self, _p: &Path) -> io::Result<()> {
        Err(self.refused())
    }

    fn remove_dir_all(&self, _p: &Path) -> io::Result<()> {
        Err(self.refused())
    }

    fn create_dir(&self, _p: &Path) -> io::Result<()> {
        Err(self.refused())
    }

    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        if self.is_root(p) {
            return Ok(());
        }
        Err(self.refused())
    }

    fn watch(&self, _sink: WatchSink) -> io::Result<WatchGuard> {
        Ok(WatchGuard(Box::new(())))
    }
}
