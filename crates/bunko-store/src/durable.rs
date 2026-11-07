//! Moves that never replace anything, and the durability pieces a journaled move needs
//! (the local case merge, Track A).
//!
//! `std::fs::rename` silently replaces a file, and on POSIX an empty folder, at the
//! destination. [`move_noreplace`] never does: it climbs a ladder of ways to move, each
//! refusing a taken destination, so every host merges with the same safety and only the
//! speed differs.
//!
//! | rung | files | folders |
//! |---|---|---|
//! | [`MoveMode::Native`] | the kernel's no-replace rename: `renameat2(RENAME_NOREPLACE)` (Linux, Android), `renamex_np(RENAME_EXCL)` (macOS), `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` (Windows) | the same |
//! | [`MoveMode::Link`] | `link(to)` then `unlink(from)` | - |
//! | [`MoveMode::Claim`] | - | `mkdir(to)` claims the name, then `rename(from, to)` onto that empty folder (POSIX replaces an empty folder atomically; anything put inside it meanwhile refuses the rename) |
//! | [`MoveMode::Copy`] | an exclusive-create copy, fsynced, its mtime set, verified by sha256, then `unlink(from)` | - |
//!
//! A rung the host or filesystem does not have (`EINVAL`, `ENOSYS`, `ENOTSUP`: an old
//! kernel, some FUSE and network filesystems) passes to the next; a link the filesystem
//! refuses (`EPERM`, Unraid's shfs, SMB) passes to the copy; two filesystems (`EXDEV`)
//! skip the link. Folders stop at [`MoveMode::Claim`] (Windows has no claim: its native
//! move takes folders on one volume) and never cross filesystems.
//!
//! The other pieces: [`fsync_dir`] (a folder's entries reach the disk), [`identity`]
//! (`dev:ino`, or the volume serial and file index on Windows: how a journal knows a file
//! is the one it planned for), [`copy_exclusive_verified`] (the copy rung on its own, for a
//! journal that keeps the copy and the unlink as separate states), and
//! [`last_move_mode`] (the rung a filesystem last needed, for doctor).
//!
//! These are local-disk calls: a path under a remote store is refused (`Unsupported`).
//! Feature `testing` adds [`testing`]: faults (the native rename unsupported, links
//! refused, two filesystems) and crash points for a folder tree.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::OnceLock;

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::mount::store_for;

/// The rung of [`move_noreplace`] that moved an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MoveMode {
    /// The kernel's own no-replace rename.
    Native,
    /// A hard link at the destination, then the source unlinked.
    Link,
    /// A folder: the destination claimed with `mkdir`, then renamed onto.
    Claim,
    /// An exclusive-create copy verified by sha256, then the source unlinked.
    Copy,
}

impl MoveMode {
    pub fn as_str(self) -> &'static str {
        match self {
            MoveMode::Native => "native",
            MoveMode::Link => "link",
            MoveMode::Claim => "claim",
            MoveMode::Copy => "copy",
        }
    }

    pub fn parse(s: &str) -> Option<MoveMode> {
        Some(match s {
            "native" => MoveMode::Native,
            "link" => MoveMode::Link,
            "claim" => MoveMode::Claim,
            "copy" => MoveMode::Copy,
            _ => return None,
        })
    }
}

impl std::fmt::Display for MoveMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which file a name points at: `st_dev`/`st_ino` on Unix, the volume serial number and
/// the file index on Windows. Two names with one identity are one file (hard links, or
/// two spellings on a case-insensitive disk); a rename keeps it, a copy does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

impl std::fmt::Display for FileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.dev, self.ino)
    }
}

/// The identity of the entry `p` names, a final symbolic link itself (not its target).
pub fn identity(p: impl AsRef<Path>) -> io::Result<FileId> {
    let p = p.as_ref();
    refuse_remote(p)?;
    sys::identity(p)
}

/// The identity of an open file.
pub fn file_identity(f: &std::fs::File) -> io::Result<FileId> {
    sys::file_identity(f)
}

/// Make `dir`'s entries durable: the names created, renamed or removed in it survive a
/// power cut once this returns (`fsync` of the folder; `F_FULLFSYNC` on macOS;
/// `FlushFileBuffers` on a backup-semantics handle on Windows). A filesystem that cannot
/// sync a folder (it answers `EINVAL`/`ENOTSUP`, or Windows refuses the handle) counts as
/// done: its entries are as durable as it makes them. A missing folder is an error.
pub fn fsync_dir(dir: impl AsRef<Path>) -> io::Result<()> {
    let dir = dir.as_ref();
    refuse_remote(dir)?;
    sys::fsync_dir(dir)
}

/// The sha256 of a file's bytes.
pub fn sha256_file(p: impl AsRef<Path>) -> io::Result<[u8; 32]> {
    let mut f = std::fs::File::open(p.as_ref())?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().into())
}

const COPY_BUF: usize = 1 << 20;

/// The error [`move_noreplace`] and [`copy_exclusive_verified`] give when anything is at
/// the destination (kind `AlreadyExists` on every host, whatever the OS said: `EEXIST`,
/// `ENOTEMPTY`, `ERROR_ALREADY_EXISTS`).
fn taken(to: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{} is taken: nothing is moved over it", to.display()),
    )
}

/// Is `e` the destination being taken?
pub fn is_taken(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty
    ) || sys::is_taken_code(e.raw_os_error())
}

fn is_cross_device(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::CrossesDevices || sys::is_cross_device_code(e.raw_os_error())
}

/// The host or filesystem has no such call (the next rung may work).
fn is_unsupported(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Unsupported || sys::is_unsupported_code(e.raw_os_error())
}

/// The filesystem refuses hard links (the copy rung may work).
fn link_refused(e: &io::Error) -> bool {
    is_unsupported(e)
        || e.kind() == io::ErrorKind::PermissionDenied
        || sys::is_link_refused_code(e.raw_os_error())
}

fn refuse_remote(p: &Path) -> io::Result<()> {
    match store_for(p) {
        Some(s) if s.caps().remote => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{} is in a remote store: a local-disk call cannot reach it",
                p.display()
            ),
        )),
        _ => Ok(()),
    }
}

/// Move the file or folder `from` to `to`, never replacing anything at `to`: the ladder
/// in the module docs. Returns the rung that moved it.
///
/// - Anything at `to` (a file, a folder empty or not, a symbolic link, or `from` itself
///   under another spelling on a case-insensitive disk) refuses the move with kind
///   `AlreadyExists`, and both names are left as they were. A case-only rename of one
///   file goes through [`crate::fs::rename`] instead.
/// - [`MoveMode::Link`] and [`MoveMode::Copy`] make the new name durable
///   ([`fsync_dir`] of `to`'s folder) before the source is unlinked, so a crash leaves
///   the source, or both names; never neither. The caller still syncs both folders
///   after the move ([`fsync_dir`]) for the rename itself.
/// - [`MoveMode::Copy`] unlinks the source only when the copy's sha256 matches and the
///   source is still the file (identity, size and mtime) it copied; otherwise the copy
///   is removed and the move fails. The copy keeps the source's mtime and permissions,
///   not its identity.
/// - A folder moves by [`MoveMode::Native`] or [`MoveMode::Claim`] only: across two
///   filesystems it fails with kind `CrossesDevices`, and on Windows without the native
///   move with kind `Unsupported`.
/// - A symbolic link (or another entry that is neither a file nor a folder) moves by
///   [`MoveMode::Native`] only, else kind `Unsupported`: a link would be copied as its
///   target's bytes.
/// - A path under a remote store is refused (`Unsupported`).
pub fn move_noreplace(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<MoveMode> {
    let (from, to) = (from.as_ref(), to.as_ref());
    refuse_remote(from)?;
    refuse_remote(to)?;
    let meta = std::fs::symlink_metadata(from)?;
    if from == to || std::fs::symlink_metadata(to).is_ok() {
        return Err(taken(to));
    }
    let faults = testing::faults_for(from);
    let mut cross = false;
    match native(from, to, &faults) {
        Ok(()) => return Ok(note_mode(to, MoveMode::Native)),
        Err(e) if is_taken(&e) => return Err(taken(to)),
        Err(e) if is_cross_device(&e) => cross = true,
        Err(e) if is_unsupported(&e) => {}
        Err(e) => return Err(e),
    }
    let kind = meta.file_type();
    if kind.is_dir() {
        if cross {
            return Err(io::Error::new(
                io::ErrorKind::CrossesDevices,
                format!(
                    "{} and {} are on different filesystems: a folder is not copied across",
                    from.display(),
                    to.display()
                ),
            ));
        }
        return claim(from, to, &faults).map(|()| note_mode(to, MoveMode::Claim));
    }
    if !kind.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{} is neither a file nor a folder: it moves only by the native rename, which this host refused",
                from.display()
            ),
        ));
    }
    if !cross {
        match link(from, to, &faults) {
            // Both names exist from here: what fails now is not the link being refused.
            Ok(()) => {
                return unlink_linked(from, to, &faults).map(|()| note_mode(to, MoveMode::Link));
            }
            Err(e) if is_taken(&e) => return Err(taken(to)),
            Err(e) if is_cross_device(&e) || link_refused(&e) => {}
            Err(e) => return Err(e),
        }
    }
    copy_then_unlink(from, to, &faults).map(|()| note_mode(to, MoveMode::Copy))
}

fn native(from: &Path, to: &Path, faults: &testing::Faults) -> io::Result<()> {
    if faults.cross_device {
        return Err(sys::cross_device_error());
    }
    if faults.native_unsupported {
        return Err(sys::unsupported_error());
    }
    sys::rename_noreplace(from, to)
}

fn parent_of(p: &Path) -> &Path {
    match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    }
}

fn link(from: &Path, to: &Path, faults: &testing::Faults) -> io::Result<()> {
    if faults.link_refused {
        return Err(sys::link_refused_error());
    }
    std::fs::hard_link(from, to)
}

/// After [`link`]: check both names are one file, make the new one durable, then drop
/// the old. A failure leaves both names (one file), never neither.
fn unlink_linked(from: &Path, to: &Path, faults: &testing::Faults) -> io::Result<()> {
    let (a, b) = (sys::identity(from)?, sys::identity(to)?);
    if a != b {
        return Err(io::Error::other(format!(
            "{} changed while it was linked as {}: both names are left",
            from.display(),
            to.display()
        )));
    }
    fsync_dir(parent_of(to))?;
    testing::crash_at(faults, testing::CrashPoint::Linked)?;
    std::fs::remove_file(from)
}

fn copy_then_unlink(from: &Path, to: &Path, faults: &testing::Faults) -> io::Result<()> {
    let (_, id) = copy_verified(from, to, faults)?;
    fsync_dir(parent_of(to))?;
    testing::crash_at(faults, testing::CrashPoint::Copied)?;
    if sys::identity(from)? != id {
        return Err(io::Error::other(format!(
            "{} was replaced after it was copied to {}: both are left",
            from.display(),
            to.display()
        )));
    }
    std::fs::remove_file(from)
}

/// Copy the file `from` to a new file `to` (never replacing one: kind `AlreadyExists`),
/// with its mtime and permissions, fsynced, then read back and checked against the
/// sha256 of what was read; also checks that `from` is still the file it was (identity,
/// size, mtime). On any failure the copy is removed. Returns the sha256. `from` stays.
pub fn copy_exclusive_verified(
    from: impl AsRef<Path>,
    to: impl AsRef<Path>,
) -> io::Result<[u8; 32]> {
    let (from, to) = (from.as_ref(), to.as_ref());
    refuse_remote(from)?;
    refuse_remote(to)?;
    let faults = testing::faults_for(from);
    copy_verified(from, to, &faults).map(|(sum, _)| sum)
}

/// [`copy_exclusive_verified`]; also returns the identity of the source it copied.
fn copy_verified(
    from: &Path,
    to: &Path,
    faults: &testing::Faults,
) -> io::Result<([u8; 32], FileId)> {
    let mut src = std::fs::File::open(from)?;
    let before = src.metadata()?;
    if !before.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a file", from.display()),
        ));
    }
    let src_id = sys::file_identity(&src)?;
    let dst = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
    {
        Ok(f) => f,
        Err(e) if is_taken(&e) => return Err(taken(to)),
        Err(e) => return Err(e),
    };
    let dst_id = sys::file_identity(&dst)?;
    let outcome = (|| {
        let mut dst = dst;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; COPY_BUF];
        let half = before.len() / 2;
        let mut written = 0u64;
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            if faults.crash == Some(testing::CrashPoint::CopyHalfway) && written + n as u64 > half {
                let k = (half - written) as usize;
                dst.write_all(&buf[..k])?;
                dst.flush()?;
                return Err(testing::crash_error(testing::CrashPoint::CopyHalfway));
            }
            dst.write_all(&buf[..n])?;
            written += n as u64;
        }
        let sum: [u8; 32] = h.finalize().into();
        if let Ok(t) = before.modified() {
            dst.set_modified(t)?;
        }
        dst.sync_all()?;
        drop(dst);
        drop(src);
        if sha256_file(to)? != sum {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "the copy {} differs from {}: it is removed",
                    to.display(),
                    from.display()
                ),
            ));
        }
        let after = std::fs::symlink_metadata(from)?;
        if sys::identity(from)? != src_id
            || after.len() != before.len()
            || after.modified().ok() != before.modified().ok()
        {
            return Err(io::Error::other(format!(
                "{} changed while it was copied to {}: the copy is removed",
                from.display(),
                to.display()
            )));
        }
        // Last: a read-only source makes a read-only copy, after its mtime was set.
        std::fs::set_permissions(to, before.permissions())?;
        Ok(sum)
    })();
    match outcome {
        Ok(sum) => Ok((sum, src_id)),
        Err(e) => {
            // A crash leaves its partial copy, as a real one would.
            if !testing::is_crash(&e) && sys::identity(to).ok() == Some(dst_id) {
                let _ = std::fs::remove_file(to);
            }
            Err(e)
        }
    }
}

fn claim(from: &Path, to: &Path, faults: &testing::Faults) -> io::Result<()> {
    if !sys::CAN_CLAIM {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{} cannot move to {}: this host has no no-replace folder rename",
                from.display(),
                to.display()
            ),
        ));
    }
    match std::fs::create_dir(to) {
        Ok(()) => {}
        Err(e) if is_taken(&e) => return Err(taken(to)),
        Err(e) => return Err(e),
    }
    let claimed = sys::identity(to)?;
    testing::crash_at(faults, testing::CrashPoint::Claimed)?;
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Undo the claim only while it is still the empty folder made here.
            if sys::identity(to).ok() == Some(claimed) {
                let _ = std::fs::remove_dir(to);
            }
            if is_taken(&e) { Err(taken(to)) } else { Err(e) }
        }
    }
}

fn modes() -> &'static Mutex<HashMap<u64, MoveMode>> {
    static MODES: OnceLock<Mutex<HashMap<u64, MoveMode>>> = OnceLock::new();
    MODES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn note_mode(to: &Path, mode: MoveMode) -> MoveMode {
    if let Ok(id) = sys::identity(to) {
        modes().lock().insert(id.dev, mode);
    }
    mode
}

/// The rung [`move_noreplace`] last needed on the filesystem `dev` ([`FileId::dev`]) in
/// this process, `None` before its first move there. Doctor shows it; `Copy` is slow.
pub fn last_move_mode(dev: u64) -> Option<MoveMode> {
    modes().lock().get(&dev).copied()
}

#[cfg(unix)]
mod sys {
    use super::FileId;
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    pub const CAN_CLAIM: bool = true;

    pub fn identity(p: &Path) -> io::Result<FileId> {
        let m = std::fs::symlink_metadata(p)?;
        Ok(FileId {
            dev: m.dev(),
            ino: m.ino(),
        })
    }

    pub fn file_identity(f: &std::fs::File) -> io::Result<FileId> {
        let m = f.metadata()?;
        Ok(FileId {
            dev: m.dev(),
            ino: m.ino(),
        })
    }

    fn cstr(p: &Path) -> io::Result<std::ffi::CString> {
        use std::os::unix::ffi::OsStrExt;
        Ok(std::ffi::CString::new(p.as_os_str().as_bytes())?)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        let (f, t) = (cstr(from)?, cstr(to)?);
        // The raw syscall: musl and older glibc have no renameat2 wrapper.
        let r = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                f.as_ptr(),
                libc::AT_FDCWD,
                t.as_ptr(),
                libc::RENAME_NOREPLACE as libc::c_uint,
            )
        };
        if r == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(target_vendor = "apple")]
    pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        let (f, t) = (cstr(from)?, cstr(to)?);
        let r = unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) };
        if r == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    pub fn rename_noreplace(_from: &Path, _to: &Path) -> io::Result<()> {
        Err(unsupported_error())
    }

    pub fn fsync_dir(dir: &Path) -> io::Result<()> {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::File::open(dir)?;
        if !f.metadata()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a folder", dir.display()),
            ));
        }
        let fd = f.as_raw_fd();
        #[cfg(target_vendor = "apple")]
        if unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) } == 0 {
            return Ok(());
        }
        if unsafe { libc::fsync(fd) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if one_of(
            e.raw_os_error(),
            &[libc::EINVAL, libc::ENOTSUP, libc::EOPNOTSUPP],
        ) {
            Ok(())
        } else {
            Err(e)
        }
    }

    /// `code` is one of `codes` (a guard, not a pattern: `ENOTSUP` is `EOPNOTSUPP` on
    /// Linux and not on macOS).
    fn one_of(code: Option<i32>, codes: &[i32]) -> bool {
        code.is_some_and(|c| codes.contains(&c))
    }

    pub fn is_taken_code(code: Option<i32>) -> bool {
        one_of(code, &[libc::EEXIST, libc::ENOTEMPTY])
    }

    pub fn is_cross_device_code(code: Option<i32>) -> bool {
        code == Some(libc::EXDEV)
    }

    pub fn is_unsupported_code(code: Option<i32>) -> bool {
        one_of(
            code,
            &[libc::EINVAL, libc::ENOSYS, libc::ENOTSUP, libc::EOPNOTSUPP],
        )
    }

    pub fn is_link_refused_code(code: Option<i32>) -> bool {
        one_of(code, &[libc::EPERM, libc::EACCES, libc::EMLINK])
    }

    pub fn cross_device_error() -> io::Error {
        io::Error::from_raw_os_error(libc::EXDEV)
    }

    pub fn unsupported_error() -> io::Error {
        io::Error::from_raw_os_error(libc::EINVAL)
    }

    pub fn link_refused_error() -> io::Error {
        io::Error::from_raw_os_error(libc::EPERM)
    }
}

#[cfg(windows)]
mod sys {
    use super::FileId;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        GetFileInformationByHandle, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    const ERROR_INVALID_FUNCTION: i32 = 1;
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_NOT_SAME_DEVICE: i32 = 17;
    const ERROR_NOT_SUPPORTED: i32 = 50;
    const ERROR_FILE_EXISTS: i32 = 80;
    const ERROR_INVALID_PARAMETER: i32 = 87;
    const ERROR_ALREADY_EXISTS: i32 = 183;
    const ERROR_DIR_NOT_EMPTY: i32 = 145;
    const ERROR_TOO_MANY_LINKS: i32 = 1142;

    /// `rename` onto an existing folder is refused on Windows even when it is empty, and
    /// the native move takes folders on one volume.
    pub const CAN_CLAIM: bool = false;

    pub fn identity(p: &Path) -> io::Result<FileId> {
        let f = std::fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(p)?;
        file_identity(&f)
    }

    pub fn file_identity(f: &std::fs::File) -> io::Result<FileId> {
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(FileId {
            dev: u64::from(info.dwVolumeSerialNumber),
            ino: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        })
    }

    /// `p` as a NUL-terminated wide string, absolute, with the `\\?\` prefix when it is
    /// too long for the legacy path limit.
    fn wide(p: &Path) -> io::Result<Vec<u16>> {
        let abs = std::path::absolute(p)?;
        let mut v: Vec<u16> = abs.as_os_str().encode_wide().collect();
        let starts = |v: &[u16], s: &str| {
            let w: Vec<u16> = s.encode_utf16().collect();
            v.starts_with(&w)
        };
        if v.len() > 247 && !starts(&v, r"\\?\") {
            v = if starts(&v, r"\\") {
                r"\\?\UNC\"
                    .encode_utf16()
                    .chain(v[2..].iter().copied())
                    .collect()
            } else {
                r"\\?\".encode_utf16().chain(v).collect()
            };
        }
        v.push(0);
        Ok(v)
    }

    pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        let (f, t) = (wide(from)?, wide(to)?);
        if unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), MOVEFILE_WRITE_THROUGH) } != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn fsync_dir(dir: &Path) -> io::Result<()> {
        if !std::fs::metadata(dir)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a folder", dir.display()),
            ));
        }
        let tolerated = |e: &io::Error| {
            matches!(
                e.raw_os_error(),
                Some(ERROR_ACCESS_DENIED)
                    | Some(ERROR_INVALID_FUNCTION)
                    | Some(ERROR_NOT_SUPPORTED)
                    | Some(ERROR_INVALID_PARAMETER)
            )
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)
        {
            Ok(f) => match f.sync_all() {
                Err(e) if !tolerated(&e) => Err(e),
                _ => Ok(()),
            },
            Err(e) if tolerated(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn is_taken_code(code: Option<i32>) -> bool {
        matches!(
            code,
            Some(ERROR_ALREADY_EXISTS) | Some(ERROR_FILE_EXISTS) | Some(ERROR_DIR_NOT_EMPTY)
        )
    }

    pub fn is_cross_device_code(code: Option<i32>) -> bool {
        code == Some(ERROR_NOT_SAME_DEVICE)
    }

    pub fn is_unsupported_code(code: Option<i32>) -> bool {
        matches!(
            code,
            Some(ERROR_INVALID_FUNCTION)
                | Some(ERROR_NOT_SUPPORTED)
                | Some(ERROR_INVALID_PARAMETER)
        )
    }

    pub fn is_link_refused_code(code: Option<i32>) -> bool {
        matches!(code, Some(ERROR_ACCESS_DENIED) | Some(ERROR_TOO_MANY_LINKS))
    }

    pub fn cross_device_error() -> io::Error {
        io::Error::from_raw_os_error(ERROR_NOT_SAME_DEVICE)
    }

    pub fn unsupported_error() -> io::Error {
        io::Error::from_raw_os_error(ERROR_INVALID_FUNCTION)
    }

    pub fn link_refused_error() -> io::Error {
        io::Error::from_raw_os_error(ERROR_ACCESS_DENIED)
    }
}

/// Fault and crash injection for the moves of one folder tree (feature `testing`).
/// Without the feature, the same names compile to no-ops.
pub mod testing {
    #[cfg(feature = "testing")]
    use std::path::{Path, PathBuf};

    use std::io;

    /// What the moves under a root meet. Each fault is the real error a host gives, so
    /// the ladder's own error handling runs.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct Faults {
        /// The native no-replace rename is missing (`EINVAL`; Windows
        /// `ERROR_INVALID_FUNCTION`).
        pub native_unsupported: bool,
        /// Hard links are refused (`EPERM`; Windows `ERROR_ACCESS_DENIED`).
        pub link_refused: bool,
        /// The two names are on different filesystems (`EXDEV`; Windows
        /// `ERROR_NOT_SAME_DEVICE`).
        pub cross_device: bool,
        /// Stop as a crash would, at this point, leaving what is on disk.
        pub crash: Option<CrashPoint>,
    }

    /// A point inside [`super::move_noreplace`] a crash can hit.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CrashPoint {
        /// Link: both names exist (and are durable), the source is not unlinked yet.
        Linked,
        /// Copy: half the bytes are written to the new file.
        CopyHalfway,
        /// Copy: the copy is complete, verified and durable; the source is not
        /// unlinked yet.
        Copied,
        /// Claim: the empty destination folder exists; nothing is renamed yet.
        Claimed,
    }

    const CRASH: &str = "the process stopped here (failpoint)";

    pub(crate) fn crash_error(at: CrashPoint) -> io::Error {
        io::Error::other(format!("{CRASH}: {at:?}"))
    }

    /// Is `e` an injected crash ([`Faults::crash`])?
    pub fn is_crash(e: &io::Error) -> bool {
        e.kind() == io::ErrorKind::Other && e.to_string().starts_with(CRASH)
    }

    pub(crate) fn crash_at(f: &Faults, at: CrashPoint) -> io::Result<()> {
        if f.crash == Some(at) {
            Err(crash_error(at))
        } else {
            Ok(())
        }
    }

    #[cfg(feature = "testing")]
    fn table() -> &'static parking_lot::Mutex<Vec<(u64, PathBuf, Faults)>> {
        static T: std::sync::OnceLock<parking_lot::Mutex<Vec<(u64, PathBuf, Faults)>>> =
            std::sync::OnceLock::new();
        T.get_or_init(Default::default)
    }

    /// Every move whose source is under `root` meets `faults` until the guard drops
    /// (the newest injection under the longest root wins).
    #[cfg(feature = "testing")]
    pub fn inject(root: impl Into<PathBuf>, faults: Faults) -> FaultGuard {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        table().lock().push((id, root.into(), faults));
        FaultGuard { id }
    }

    /// The faults that make [`super::move_noreplace`] under `root` use `mode`: none for
    /// `Native`, the native rename missing for `Link` and `Claim`, and links refused too
    /// for `Copy`.
    #[cfg(feature = "testing")]
    pub fn force(root: impl Into<PathBuf>, mode: super::MoveMode) -> FaultGuard {
        use super::MoveMode;
        let faults = Faults {
            native_unsupported: mode != MoveMode::Native,
            link_refused: mode == MoveMode::Copy,
            ..Faults::default()
        };
        inject(root, faults)
    }

    /// Removes its injection on drop.
    #[cfg(feature = "testing")]
    #[must_use = "the faults end when the guard drops"]
    pub struct FaultGuard {
        id: u64,
    }

    #[cfg(feature = "testing")]
    impl Drop for FaultGuard {
        fn drop(&mut self) {
            table().lock().retain(|(id, _, _)| *id != self.id);
        }
    }

    #[cfg(feature = "testing")]
    pub(crate) fn faults_for(p: &Path) -> Faults {
        table()
            .lock()
            .iter()
            .filter(|(_, root, _)| p.starts_with(root))
            .max_by_key(|(id, root, _)| (root.components().count(), *id))
            .map(|(_, _, f)| *f)
            .unwrap_or_default()
    }

    #[cfg(not(feature = "testing"))]
    pub(crate) fn faults_for(_p: &std::path::Path) -> Faults {
        Faults::default()
    }
}
