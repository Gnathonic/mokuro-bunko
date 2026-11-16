//! Platform file operations for the folder driver: a rename that never replaces, the
//! same-file test, directory fsync, the hidden attribute, and how an I/O error reads as
//! a [`BErr`].
//!
//! Only `std`, `libc` (Unix) and `windows-sys` (Windows) are used here.

use std::fs::Metadata;
use std::io;
use std::path::Path;

use crate::backend::BErr;

/// What a failing call was doing, for [`io_err`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Act {
    Read,
    Create,
    Rename,
    Delete,
}

/// Windows: `ERROR_ACCESS_DENIED`, `ERROR_SHARING_VIOLATION`, `ERROR_LOCK_VIOLATION`.
#[cfg(windows)]
const WIN_ACCESS_DENIED: i32 = 5;
#[cfg(windows)]
const WIN_SHARING: [i32; 2] = [32, 33];

/// The error an I/O failure on `rel` (a path relative to the library folder, for the
/// message) means. Never `AuthExpired` or `AuthRevoked`.
pub(crate) fn io_err(e: &io::Error, act: Act, rel: &str) -> BErr {
    use io::ErrorKind as K;
    let raw = e.raw_os_error();
    #[cfg(windows)]
    {
        // Another program holds the file (Chrome, Explorer's thumbnails, an antivirus):
        // it lets go soon.
        if raw.is_some_and(|r| WIN_SHARING.contains(&r))
            || (raw == Some(WIN_ACCESS_DENIED) && matches!(act, Act::Rename | Act::Delete))
        {
            return BErr::Transient(format!("{rel} is in use by another program: {e}"));
        }
    }
    let shown = if rel.is_empty() {
        "the library folder"
    } else {
        rel
    };
    match e.kind() {
        K::NotFound | K::NotADirectory => BErr::NotFound,
        K::AlreadyExists => BErr::NameConflict,
        K::StorageFull | K::QuotaExceeded => BErr::StorageFull,
        K::FileTooLarge => BErr::TooLarge,
        K::InvalidFilename => BErr::InvalidName,
        K::CrossesDevices => {
            BErr::Fatal(format!("{shown} crosses a mount point inside the library"))
        }
        K::PermissionDenied => BErr::Fatal(match act {
            Act::Read => format!("Bunko may not read {shown}"),
            _ => format!("Bunko may not write {shown}"),
        }),
        K::InvalidInput if matches!(act, Act::Create | Act::Rename) => BErr::InvalidName,
        _ if is_bad_name_errno(raw) && matches!(act, Act::Create | Act::Rename) => {
            BErr::InvalidName
        }
        _ => BErr::Transient(format!("{shown}: {e}")),
    }
}

#[cfg(unix)]
fn is_bad_name_errno(raw: Option<i32>) -> bool {
    matches!(
        raw,
        Some(libc::EINVAL) | Some(libc::EILSEQ) | Some(libc::ENAMETOOLONG)
    )
}

#[cfg(not(unix))]
fn is_bad_name_errno(_raw: Option<i32>) -> bool {
    false
}

/// Rename `from` to `to` unless `to` exists. `Ok(true)`: renamed. `Ok(false)`: this
/// platform or filesystem has no such rename (the caller falls back). An existing
/// `to` fails with `AlreadyExists`.
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<bool> {
    sys::rename_noreplace(from, to)
}

/// `a` and `b` name one file (or folder): same device and inode, or on Windows the
/// same volume serial and file index. Symbolic links are not followed.
pub(crate) fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
    sys::same_file(a, b)
}

/// Make a rename or create in `dir` durable (Unix; best effort: some filesystems
/// refuse to sync a folder). A no-op on Windows.
pub(crate) fn fsync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        if let Ok(f) = std::fs::File::open(dir) {
            let _ = f.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

/// Windows: give `path` the hidden attribute (best effort). A no-op elsewhere, where
/// the leading dot already hides it.
pub(crate) fn set_hidden(path: &Path) {
    sys::set_hidden(path);
}

/// The device a node lives on (Unix); `None` on Windows.
pub(crate) fn device_of(m: &Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(m.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = m;
        None
    }
}

/// Open a file for reading without following a symbolic link at its last segment
/// (`O_NOFOLLOW` on Unix).
pub(crate) fn open_nofollow(path: &Path) -> io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW);
    }
    o.open(path)
}

#[cfg(target_os = "linux")]
mod sys {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    fn cstr(p: &Path) -> io::Result<CString> {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in a path"))
    }

    pub(super) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<bool> {
        let (a, b) = (cstr(from)?, cstr(to)?);
        // SAFETY: both pointers are NUL-terminated strings that outlive the call; the
        // raw syscall is used so a static musl build needs no glibc wrapper.
        let r = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                a.as_ptr(),
                libc::AT_FDCWD,
                b.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if r == 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            // The kernel or the filesystem (NFS, CIFS, FUSE) has no RENAME_NOREPLACE.
            Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP) => Ok(false),
            _ => Err(e),
        }
    }

    pub(super) fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
        super::unix_same_file(a, b)
    }

    pub(super) fn set_hidden(_path: &Path) {}
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod sys {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    fn cstr(p: &Path) -> io::Result<CString> {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in a path"))
    }

    pub(super) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<bool> {
        let (a, b) = (cstr(from)?, cstr(to)?);
        // SAFETY: both pointers are NUL-terminated strings that outlive the call.
        let r = unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_EXCL) };
        if r == 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ENOTSUP) | Some(libc::EINVAL) | Some(libc::ENOSYS) => Ok(false),
            _ => Err(e),
        }
    }

    pub(super) fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
        super::unix_same_file(a, b)
    }

    pub(super) fn set_hidden(_path: &Path) {}
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "macos", target_os = "ios"))
))]
mod sys {
    use std::io;
    use std::path::Path;

    pub(super) fn rename_noreplace(_from: &Path, _to: &Path) -> io::Result<bool> {
        Ok(false)
    }

    pub(super) fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
        super::unix_same_file(a, b)
    }

    pub(super) fn set_hidden(_path: &Path) {}
}

#[cfg(unix)]
fn unix_same_file(a: &Path, b: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let (ma, mb) = (std::fs::symlink_metadata(a)?, std::fs::symlink_metadata(b)?);
    Ok(ma.dev() == mb.dev() && ma.ino() == mb.ino())
}

#[cfg(windows)]
mod sys {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Component, Path, Prefix};

    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_HIDDEN, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileAttributesW, GetFileInformationByHandle, INVALID_FILE_ATTRIBUTES, MoveFileExW,
        SetFileAttributesW,
    };

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// `p` as a verbatim (`\\?\`, `\\?\UNC\`) wide string, so a long path works.
    fn verbatim(p: &Path) -> Vec<u16> {
        let mut comps = p.components();
        let Some(Component::Prefix(pre)) = comps.next() else {
            return wide(p.as_os_str());
        };
        let mut out = std::ffi::OsString::new();
        match pre.kind() {
            Prefix::Verbatim(_) | Prefix::VerbatimUNC(..) | Prefix::VerbatimDisk(_) => {
                return wide(p.as_os_str());
            }
            Prefix::Disk(d) => out.push(format!(r"\\?\{}:", d as char)),
            Prefix::UNC(server, share) => {
                out.push(r"\\?\UNC\");
                out.push(server);
                out.push(r"\");
                out.push(share);
            }
            Prefix::DeviceNS(_) => return wide(p.as_os_str()),
        }
        for c in comps {
            match c {
                Component::RootDir => {}
                Component::CurDir => {}
                Component::ParentDir => return wide(p.as_os_str()),
                Component::Normal(s) => {
                    out.push(r"\");
                    out.push(s);
                }
                Component::Prefix(_) => return wide(p.as_os_str()),
            }
        }
        wide(&out)
    }

    pub(super) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<bool> {
        let (a, b) = (verbatim(from), verbatim(to));
        // SAFETY: both are NUL-terminated wide strings that outlive the call; flags 0
        // means "fail when the target exists".
        let ok = unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), 0) };
        if ok != 0 {
            Ok(true)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn file_id(p: &Path) -> io::Result<(u32, u32, u32)> {
        let f = std::fs::OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(p)?;
        // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid value to overwrite.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the handle is open for the duration of the call; `info` is writable.
        let ok = unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ))
    }

    pub(super) fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
        Ok(file_id(a)? == file_id(b)?)
    }

    pub(super) fn set_hidden(path: &Path) {
        let w = verbatim(path);
        // SAFETY: `w` is a NUL-terminated wide string that outlives both calls.
        unsafe {
            let attrs = GetFileAttributesW(w.as_ptr());
            if attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_HIDDEN == 0 {
                SetFileAttributesW(w.as_ptr(), attrs | FILE_ATTRIBUTE_HIDDEN);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_noreplace_refuses_an_existing_target() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, c) = (d.path().join("a"), d.path().join("b"), d.path().join("c"));
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        match rename_noreplace(&a, &b) {
            Ok(false) => return, // no such rename here: nothing to check
            Ok(true) => panic!("replaced an existing file"),
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e}"),
        }
        assert!(rename_noreplace(&a, &c).unwrap());
        assert_eq!(std::fs::read(&c).unwrap(), b"a");
        assert_eq!(std::fs::read(&b).unwrap(), b"b");
        assert!(same_file(&c, &c).unwrap());
        assert!(!same_file(&b, &c).unwrap());
    }

    #[test]
    fn errors_map_to_backend_errors() {
        let nf = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(io_err(&nf, Act::Read, "x"), BErr::NotFound);
        let full = io::Error::from(io::ErrorKind::StorageFull);
        assert_eq!(io_err(&full, Act::Create, "x"), BErr::StorageFull);
        let big = io::Error::from(io::ErrorKind::FileTooLarge);
        assert_eq!(io_err(&big, Act::Create, "x"), BErr::TooLarge);
        let bad = io::Error::from(io::ErrorKind::InvalidFilename);
        assert_eq!(io_err(&bad, Act::Rename, "x"), BErr::InvalidName);
        let xdev = io::Error::from(io::ErrorKind::CrossesDevices);
        assert!(matches!(io_err(&xdev, Act::Rename, "x"), BErr::Fatal(_)));
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(matches!(io_err(&denied, Act::Create, "x"), BErr::Fatal(_)));
        let ro = io::Error::from(io::ErrorKind::ReadOnlyFilesystem);
        assert!(matches!(io_err(&ro, Act::Create, "x"), BErr::Transient(_)));
        #[cfg(unix)]
        {
            let eilseq = io::Error::from_raw_os_error(libc::EILSEQ);
            assert_eq!(io_err(&eilseq, Act::Create, "x"), BErr::InvalidName);
            let eio = io::Error::from_raw_os_error(libc::EIO);
            assert!(matches!(io_err(&eio, Act::Read, "x"), BErr::Transient(_)));
            let edquot = io::Error::from_raw_os_error(libc::EDQUOT);
            assert_eq!(io_err(&edquot, Act::Create, "x"), BErr::StorageFull);
        }
    }
}
