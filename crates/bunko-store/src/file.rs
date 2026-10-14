//! Open files, open options and directory listings, store-routed.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::meta::{FileType, Meta};

/// A remote store's open file.
pub trait RemoteFile: Read + Write + Seek + Send + fmt::Debug {
    fn metadata(&self) -> io::Result<Meta>;
    fn sync_all(&self) -> io::Result<()>;
    fn set_len(&self, len: u64) -> io::Result<()>;
    /// The local file this handle writes, when it is one (a remote store's staging
    /// file, spec §3.3 `open_with`): the bytes written to it are the staged bytes a
    /// later `publish` takes. Anything else (a stream from the provider) gives itself
    /// back.
    fn into_local(self: Box<Self>) -> Result<std::fs::File, Box<dyn RemoteFile>>;
}

/// `std::fs::File`, or a remote store's file.
#[derive(Debug)]
pub enum File {
    Std(std::fs::File),
    Remote(Box<dyn RemoteFile>),
}

impl File {
    pub fn open(p: impl AsRef<Path>) -> io::Result<File> {
        crate::fs::open(p.as_ref())
    }

    /// `std::fs::File::create`.
    pub fn create(p: impl AsRef<Path>) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(p)
    }

    /// `std::fs::File::create_new`.
    pub fn create_new(p: impl AsRef<Path>) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(p)
    }

    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    pub fn metadata(&self) -> io::Result<Meta> {
        match self {
            File::Std(f) => f.metadata().map(Meta::Std),
            File::Remote(f) => f.metadata(),
        }
    }

    pub fn sync_all(&self) -> io::Result<()> {
        match self {
            File::Std(f) => f.sync_all(),
            File::Remote(f) => f.sync_all(),
        }
    }

    pub fn sync_data(&self) -> io::Result<()> {
        match self {
            File::Std(f) => f.sync_data(),
            File::Remote(f) => f.sync_all(),
        }
    }

    pub fn set_len(&self, len: u64) -> io::Result<()> {
        match self {
            File::Std(f) => f.set_len(len),
            File::Remote(f) => f.set_len(len),
        }
    }

    /// Set file times; a remote store ignores it (provider mtimes are never set).
    pub fn set_times(&self, times: std::fs::FileTimes) -> io::Result<()> {
        match self {
            File::Std(f) => f.set_times(times),
            File::Remote(_) => Ok(()),
        }
    }

    pub fn set_modified(&self, t: SystemTime) -> io::Result<()> {
        match self {
            File::Std(f) => f.set_modified(t),
            File::Remote(_) => Ok(()),
        }
    }

    pub fn into_std(self) -> Option<std::fs::File> {
        match self {
            File::Std(f) => Some(f),
            File::Remote(_) => None,
        }
    }

    /// A local file to write through: `Std` as it is, a remote store's staging file
    /// ([`RemoteFile::into_local`]); anything else is given back.
    pub fn into_local(self) -> Result<std::fs::File, File> {
        match self {
            File::Std(f) => Ok(f),
            File::Remote(f) => f.into_local().map_err(File::Remote),
        }
    }

    pub fn as_std(&self) -> Option<&std::fs::File> {
        match self {
            File::Std(f) => Some(f),
            File::Remote(_) => None,
        }
    }
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            File::Std(f) => f.read(buf),
            File::Remote(f) => f.read(buf),
        }
    }
}

impl Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            File::Std(f) => f.write(buf),
            File::Remote(f) => f.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            File::Std(f) => f.flush(),
            File::Remote(f) => f.flush(),
        }
    }
}

impl Seek for File {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            File::Std(f) => f.seek(pos),
            File::Remote(f) => f.seek(pos),
        }
    }
}

/// `std::fs::OpenOptions`, store-routed.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub truncate: bool,
    pub create: bool,
    pub create_new: bool,
    /// unix permission bits for a created file; ignored elsewhere.
    pub mode: Option<u32>,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read(&mut self, v: bool) -> &mut Self {
        self.read = v;
        self
    }

    pub fn write(&mut self, v: bool) -> &mut Self {
        self.write = v;
        self
    }

    pub fn append(&mut self, v: bool) -> &mut Self {
        self.append = v;
        self
    }

    pub fn truncate(&mut self, v: bool) -> &mut Self {
        self.truncate = v;
        self
    }

    pub fn create(&mut self, v: bool) -> &mut Self {
        self.create = v;
        self
    }

    pub fn create_new(&mut self, v: bool) -> &mut Self {
        self.create_new = v;
        self
    }

    /// `OpenOptionsExt::mode` on unix; ignored elsewhere.
    pub fn mode(&mut self, m: u32) -> &mut Self {
        self.mode = Some(m);
        self
    }

    pub fn open(&self, p: impl AsRef<Path>) -> io::Result<File> {
        crate::fs::open_with(p.as_ref(), self)
    }

    /// The equivalent `std::fs::OpenOptions`.
    pub fn to_std(&self) -> std::fs::OpenOptions {
        let mut o = std::fs::OpenOptions::new();
        o.read(self.read)
            .write(self.write)
            .append(self.append)
            .truncate(self.truncate)
            .create(self.create)
            .create_new(self.create_new);
        #[cfg(unix)]
        if let Some(m) = self.mode {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(m);
        }
        o
    }
}

/// A remote store's directory entry.
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub path: PathBuf,
    pub name: OsString,
    pub meta: Meta,
}

/// `std::fs::DirEntry`, or a remote store's entry. Not boxed: listings are hot paths
/// and the std variant is the one every local entry takes.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum DirEntry {
    Std(std::fs::DirEntry),
    Remote(RemoteEntry),
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        match self {
            DirEntry::Std(e) => e.path(),
            DirEntry::Remote(e) => e.path.clone(),
        }
    }

    pub fn file_name(&self) -> OsString {
        match self {
            DirEntry::Std(e) => e.file_name(),
            DirEntry::Remote(e) => e.name.clone(),
        }
    }

    /// Without a stat on most platforms, as `std::fs::DirEntry::file_type`.
    pub fn file_type(&self) -> io::Result<FileType> {
        match self {
            DirEntry::Std(e) => e.file_type().map(FileType::from_std),
            DirEntry::Remote(e) => Ok(e.meta.file_type()),
        }
    }

    /// Lazy for std entries: exactly `std::fs::DirEntry::metadata`.
    pub fn metadata(&self) -> io::Result<Meta> {
        match self {
            DirEntry::Std(e) => e.metadata().map(Meta::Std),
            DirEntry::Remote(e) => Ok(e.meta.clone()),
        }
    }
}

/// `std::fs::ReadDir`, or a remote listing.
#[allow(clippy::large_enum_variant)]
pub enum ReadDir {
    Std(std::fs::ReadDir),
    Remote(std::vec::IntoIter<io::Result<DirEntry>>),
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            ReadDir::Std(r) => r.next().map(|e| e.map(DirEntry::Std)),
            ReadDir::Remote(r) => r.next(),
        }
    }
}
