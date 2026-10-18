//! File metadata that a local file and a remote entry both answer.

use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// An opaque content version (a provider's content version). Local files have none.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version(pub Arc<str>);

impl Version {
    pub fn new(v: impl Into<Arc<str>>) -> Self {
        Version(v.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `std::fs::FileType`, which has no public constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Dir,
    Symlink,
    Other,
}

impl FileType {
    pub fn is_file(&self) -> bool {
        matches!(self, FileType::File)
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, FileType::Dir)
    }

    pub fn is_symlink(&self) -> bool {
        matches!(self, FileType::Symlink)
    }

    pub fn from_std(t: std::fs::FileType) -> Self {
        if t.is_symlink() {
            FileType::Symlink
        } else if t.is_dir() {
            FileType::Dir
        } else if t.is_file() {
            FileType::File
        } else {
            FileType::Other
        }
    }
}

/// Metadata of a remote entry. `mtime_ns` is the effective mtime of spec §5.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMeta {
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_s: i64,
    pub is_dir: bool,
    /// The mount id: two entries are the same file when `(dev, ino)` match.
    pub dev: u64,
    /// The index entry id.
    pub ino: u64,
    pub cver: Option<Version>,
    /// The bytes are a version Bunko wrote.
    pub own: bool,
    /// Lowercase hex SHA-256 of the bytes, when the provider's listing gave one.
    pub sha256: Option<Arc<str>>,
}

/// `std::fs::Metadata`, or a remote entry's equivalent.
#[derive(Debug, Clone)]
pub enum Meta {
    Std(std::fs::Metadata),
    Remote(RemoteMeta),
}

impl Meta {
    pub fn len(&self) -> u64 {
        match self {
            Meta::Std(m) => m.len(),
            Meta::Remote(r) => r.size,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lowercase hex SHA-256 of the bytes, known without reading them (a remote
    /// entry whose provider listing has one); `None` for a local file.
    pub fn content_sha256(&self) -> Option<&str> {
        match self {
            Meta::Std(_) => None,
            Meta::Remote(r) => r.sha256.as_deref(),
        }
    }

    pub fn is_file(&self) -> bool {
        match self {
            Meta::Std(m) => m.is_file(),
            Meta::Remote(r) => !r.is_dir,
        }
    }

    pub fn is_dir(&self) -> bool {
        match self {
            Meta::Std(m) => m.is_dir(),
            Meta::Remote(r) => r.is_dir,
        }
    }

    pub fn is_symlink(&self) -> bool {
        match self {
            Meta::Std(m) => m.is_symlink(),
            Meta::Remote(_) => false,
        }
    }

    pub fn file_type(&self) -> FileType {
        match self {
            Meta::Std(m) => FileType::from_std(m.file_type()),
            Meta::Remote(r) if r.is_dir => FileType::Dir,
            Meta::Remote(_) => FileType::File,
        }
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        match self {
            Meta::Std(m) => m.modified(),
            Meta::Remote(r) => Ok(system_time_from_ns(r.mtime_ns)),
        }
    }

    /// `(st_mtime, st_mtime_nsec)` where the platform has them (unix `MetadataExt`,
    /// and every remote entry); `None` on other platforms, whose callers fall back to
    /// [`Meta::modified`].
    pub fn unix_mtime(&self) -> Option<(i64, i64)> {
        match self {
            #[cfg(unix)]
            Meta::Std(m) => {
                use std::os::unix::fs::MetadataExt;
                Some((m.mtime(), m.mtime_nsec()))
            }
            #[cfg(not(unix))]
            Meta::Std(_) => None,
            Meta::Remote(r) => Some((
                r.mtime_ns.div_euclid(1_000_000_000) as i64,
                r.mtime_ns.rem_euclid(1_000_000_000) as i64,
            )),
        }
    }

    /// `st_ctime` seconds where the platform has it.
    pub fn unix_ctime(&self) -> Option<i64> {
        match self {
            #[cfg(unix)]
            Meta::Std(m) => {
                use std::os::unix::fs::MetadataExt;
                Some(m.ctime())
            }
            #[cfg(not(unix))]
            Meta::Std(_) => None,
            Meta::Remote(r) => Some(r.ctime_s),
        }
    }

    pub fn dev(&self) -> Option<u64> {
        match self {
            #[cfg(unix)]
            Meta::Std(m) => {
                use std::os::unix::fs::MetadataExt;
                Some(m.dev())
            }
            #[cfg(not(unix))]
            Meta::Std(_) => None,
            Meta::Remote(r) => Some(r.dev),
        }
    }

    pub fn ino(&self) -> Option<u64> {
        match self {
            #[cfg(unix)]
            Meta::Std(m) => {
                use std::os::unix::fs::MetadataExt;
                Some(m.ino())
            }
            #[cfg(not(unix))]
            Meta::Std(_) => None,
            Meta::Remote(r) => Some(r.ino),
        }
    }

    /// The content version: `None` for local files.
    pub fn version(&self) -> Option<&Version> {
        match self {
            Meta::Std(_) => None,
            Meta::Remote(r) => r.cver.as_ref(),
        }
    }

    /// The bytes are a version Bunko wrote (always `false` for local files).
    pub fn own(&self) -> bool {
        match self {
            Meta::Std(_) => false,
            Meta::Remote(r) => r.own,
        }
    }

    pub fn as_std(&self) -> Option<&std::fs::Metadata> {
        match self {
            Meta::Std(m) => Some(m),
            Meta::Remote(_) => None,
        }
    }
}

impl From<std::fs::Metadata> for Meta {
    fn from(m: std::fs::Metadata) -> Self {
        Meta::Std(m)
    }
}

fn system_time_from_ns(ns: i128) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos(ns.unsigned_abs() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(mtime_ns: i128) -> Meta {
        Meta::Remote(RemoteMeta {
            size: 3,
            mtime_ns,
            ctime_s: 0,
            is_dir: false,
            dev: 1,
            ino: 2,
            cver: Some(Version::new("v1")),
            own: true,
            sha256: None,
        })
    }

    #[test]
    fn remote_mtime_splits_like_stat() {
        assert_eq!(remote(1_500_000_000_250).unix_mtime(), Some((1500, 250)));
        // pre-1970: floor seconds, non-negative nanoseconds, as stat reports
        assert_eq!(remote(-1).unix_mtime(), Some((-1, 999_999_999)));
        assert_eq!(
            remote(-1).modified().unwrap(),
            UNIX_EPOCH - Duration::from_nanos(1)
        );
    }

    #[test]
    fn remote_answers_identity_and_version() {
        let m = remote(0);
        assert_eq!((m.dev(), m.ino()), (Some(1), Some(2)));
        assert_eq!(m.version().map(Version::as_str), Some("v1"));
        assert!(m.own() && m.is_file() && !m.is_dir() && !m.is_symlink());
        assert_eq!(m.len(), 3);
    }
}
