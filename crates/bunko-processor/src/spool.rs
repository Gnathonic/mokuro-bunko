//! Where a downloaded archive's bytes live while its claim is alive (spec
//! remote-processors §8.3, 0.5.2 `ArchiveSpool`).
//!
//! RAM when it fits the processor's budget (`processor.archive_memory_mb`, shared by
//! every session) and the machine — or its memory cgroup — keeps a gigabyte beside it;
//! otherwise a file under `<storage>/.processing/archives/`, when the disk keeps
//! 256 MiB beside it; otherwise the claim goes back as `no_room`. A placement reserves
//! its whole size up front, so two downloads placing at once can never overshoot the
//! budget, and releases it exactly once (on [`Placement::release`] or drop).
//!
//! RAM placements are Linux `memfd`s, read back by the in-process pipeline through
//! `/proc/self/fd/<n>` (the kernel frees them with the last handle, crash included).
//! Other platforms always use the disk (0.5.2: Windows has no `/dev/shm`).

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;
/// What must stay available beside an archive placed in RAM.
pub const MEMORY_MARGIN: u64 = GIB;
/// What must stay free on storage beside an archive placed on disk.
pub const DISK_MARGIN: u64 = 256 * MIB;

/// The disk fallback, under the processor's storage. Swept at start, which only the
/// storage lock's holder may do.
pub fn archives_dir(storage: &Path) -> PathBuf {
    storage.join(".processing").join("archives")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementKind {
    Memory,
    Disk,
}

impl PlacementKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PlacementKind::Memory => "memory",
            PlacementKind::Disk => "disk",
        }
    }
}

/// Why nothing could be placed (a `no_room` return), or a write failed.
#[derive(Debug, thiserror::Error)]
pub enum SpoolError {
    #[error("{0}")]
    NoRoom(String),
    /// A write hit ENOSPC/EDQUOT (the RAM copy re-places on disk; a disk copy is
    /// `no_room`).
    #[error("{0}")]
    Full(io::Error),
    #[error("{0}")]
    Io(#[from] io::Error),
}

type Headroom = dyn Fn() -> Option<u64> + Send + Sync;
type FreeBytes = dyn Fn(&Path) -> io::Result<u64> + Send + Sync;

struct Shared {
    in_memory: Mutex<u64>,
}

/// One processor's archive spool.
pub struct ArchiveSpool {
    storage: PathBuf,
    budget: u64,
    shared: Arc<Shared>,
    headroom: Box<Headroom>,
    free: Box<FreeBytes>,
    memory_enabled: bool,
}

impl std::fmt::Debug for ArchiveSpool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveSpool")
            .field("storage", &self.storage)
            .field("budget", &self.budget)
            .finish()
    }
}

impl ArchiveSpool {
    pub fn new(storage: &Path, memory_mb: u64) -> ArchiveSpool {
        ArchiveSpool {
            storage: storage.to_path_buf(),
            budget: memory_mb.saturating_mul(MIB),
            shared: Arc::new(Shared {
                in_memory: Mutex::new(0),
            }),
            headroom: Box::new(memory_headroom),
            free: Box::new(|p| fs4::available_space(p)),
            memory_enabled: memfd_supported(),
        }
    }

    /// Replace the probes (tests).
    pub fn with_probes(
        mut self,
        headroom: impl Fn() -> Option<u64> + Send + Sync + 'static,
        free: impl Fn(&Path) -> io::Result<u64> + Send + Sync + 'static,
    ) -> ArchiveSpool {
        self.headroom = Box::new(headroom);
        self.free = Box::new(free);
        self
    }

    pub fn disk_dir(&self) -> PathBuf {
        archives_dir(&self.storage)
    }

    /// RAM reserved right now by live memory placements.
    pub fn in_memory_bytes(&self) -> u64 {
        *self.shared.in_memory.lock()
    }

    /// A new, empty placement for an archive of `size` bytes: RAM if it fits (and
    /// `memory`), else disk, else [`SpoolError::NoRoom`].
    pub fn place(&self, size: u64, memory: bool) -> Result<Placement, SpoolError> {
        if memory && self.memory_enabled && self.budget > 0 {
            let mut in_memory = self.shared.in_memory.lock();
            let fits_budget = *in_memory + size <= self.budget;
            let fits_machine =
                fits_budget && (self.headroom)().is_none_or(|h| h >= size + MEMORY_MARGIN);
            if fits_machine && let Some((file, path)) = open_memfd() {
                *in_memory += size;
                return Ok(Placement {
                    shared: self.shared.clone(),
                    kind: PlacementKind::Memory,
                    size,
                    file: Some(file),
                    read_path: path,
                    named: None,
                    written: 0,
                    released: false,
                });
            }
        }
        self.place_on_disk(size)
    }

    fn place_on_disk(&self, size: u64) -> Result<Placement, SpoolError> {
        let dir = self.disk_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| SpoolError::NoRoom(format!("the processor's storage is unusable: {e}")))?;
        let free = (self.free)(&dir)
            .map_err(|e| SpoolError::NoRoom(format!("the processor's storage is unusable: {e}")))?;
        if free < size + DISK_MARGIN {
            return Err(SpoolError::NoRoom(format!(
                "no room for a {:.1} MB archive: not in memory, and {} has {:.0} MB free",
                size as f64 / MIB as f64,
                dir.display(),
                free as f64 / MIB as f64
            )));
        }
        let path = dir.join(format!("{}.cbz", uuid::Uuid::new_v4().simple()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Placement {
            shared: self.shared.clone(),
            kind: PlacementKind::Disk,
            size,
            file: Some(file),
            read_path: path.clone(),
            named: Some(path),
            written: 0,
            released: false,
        })
    }

    /// Empty the disk fallback directory: leftovers of an earlier run. Only at start,
    /// under the storage lock.
    pub fn sweep(&self) {
        let Ok(entries) = std::fs::read_dir(self.disk_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let _ = if is_dir {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }
}

/// One archive's bytes: the file, its reservation, nothing else. Released once.
pub struct Placement {
    shared: Arc<Shared>,
    kind: PlacementKind,
    size: u64,
    file: Option<File>,
    read_path: PathBuf,
    named: Option<PathBuf>,
    written: u64,
    released: bool,
}

impl std::fmt::Debug for Placement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Placement")
            .field("kind", &self.kind)
            .field("size", &self.size)
            .field("path", &self.read_path)
            .field("written", &self.written)
            .finish()
    }
}

impl Placement {
    pub fn kind(&self) -> PlacementKind {
        self.kind
    }

    /// The size reserved for it.
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// A path this process can open read-only (with its own offset).
    pub fn path(&self) -> &Path {
        &self.read_path
    }

    /// Append `data`. [`SpoolError::Full`] on ENOSPC/EDQUOT.
    pub fn write(&mut self, data: &[u8]) -> Result<(), SpoolError> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| SpoolError::Io(io::Error::other("placement released")))?;
        match file.write_all(data) {
            Ok(()) => {
                self.written += data.len() as u64;
                Ok(())
            }
            Err(e) if is_full(&e) => Err(SpoolError::Full(e)),
            Err(e) => Err(SpoolError::Io(e)),
        }
    }

    /// Empty the file for a download that starts over from byte 0.
    pub fn reset(&mut self) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
        }
        self.written = 0;
        Ok(())
    }

    /// Give back this placement's reservation and file. Idempotent.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        if self.kind == PlacementKind::Memory {
            let mut in_memory = self.shared.in_memory.lock();
            *in_memory = in_memory.saturating_sub(self.size);
        }
        self.file = None;
        if let Some(path) = self.named.take() {
            // On Windows a file still open elsewhere cannot be deleted; the next
            // start's sweep takes it.
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Drop for Placement {
    fn drop(&mut self) {
        self.release();
    }
}

fn is_full(e: &io::Error) -> bool {
    #[cfg(target_os = "linux")]
    {
        matches!(e.raw_os_error(), Some(libc::ENOSPC) | Some(libc::EDQUOT))
    }
    #[cfg(not(target_os = "linux"))]
    {
        e.kind() == io::ErrorKind::StorageFull
    }
}

#[cfg(target_os = "linux")]
fn memfd_supported() -> bool {
    Path::new("/proc/self/fd").is_dir()
}

#[cfg(not(target_os = "linux"))]
fn memfd_supported() -> bool {
    false
}

#[cfg(target_os = "linux")]
fn open_memfd() -> Option<(File, PathBuf)> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: the name is a valid NUL-terminated C string and the flags are a valid
    // combination; memfd_create returns a new descriptor or -1 and touches no memory
    // of ours.
    let fd = unsafe { libc::memfd_create(c"mokuro-archive".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` was just created by memfd_create and is owned by nobody else.
    let file = unsafe { File::from_raw_fd(fd) };
    let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    Some((file, path))
}

#[cfg(not(target_os = "linux"))]
fn open_memfd() -> Option<(File, PathBuf)> {
    None
}

// --- memory headroom --------------------------------------------------------------------

fn read_int(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    if text == "max" {
        return None;
    }
    text.parse().ok()
}

/// `MemAvailable` in bytes.
pub fn mem_available() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with("MemAvailable:"))?;
    line.split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()
        .map(|kib| kib * 1024)
}

/// What this process's memory cgroup still allows (the smallest `max - current` over
/// its cgroup v2 ancestors, or the v1 controller's `limit - usage`); None for no limit.
pub fn cgroup_headroom() -> Option<u64> {
    cgroup_headroom_at(Path::new("/proc/self/cgroup"), Path::new("/sys/fs/cgroup"))
}

fn cgroup_headroom_at(proc_cgroup: &Path, root: &Path) -> Option<u64> {
    const UNLIMITED: u64 = 1 << 60;
    let text = std::fs::read_to_string(proc_cgroup).ok()?;
    let mut best: Option<u64> = None;
    let mut consider = |v: Option<u64>| {
        if let Some(v) = v {
            best = Some(best.map_or(v, |b| b.min(v)));
        }
    };
    for line in text.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(hierarchy), Some(controllers), Some(path)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let relative = path.trim().trim_start_matches('/');
        if hierarchy == "0" && controllers.is_empty() {
            let mut node = if relative.is_empty() {
                root.to_path_buf()
            } else {
                root.join(relative)
            };
            loop {
                if let (Some(limit), Some(current)) = (
                    read_int(&node.join("memory.max")),
                    read_int(&node.join("memory.current")),
                ) {
                    consider(Some(limit.saturating_sub(current)));
                }
                if node == root || !node.starts_with(root) {
                    break;
                }
                match node.parent() {
                    Some(parent) => node = parent.to_path_buf(),
                    None => break,
                }
            }
        } else if controllers.split(',').any(|c| c == "memory") {
            let node = root.join("memory").join(relative);
            if let (Some(limit), Some(usage)) = (
                read_int(&node.join("memory.limit_in_bytes")),
                read_int(&node.join("memory.usage_in_bytes")),
            ) && limit < UNLIMITED
            {
                consider(Some(limit.saturating_sub(usage)));
            }
        }
    }
    best
}

/// `min(MemAvailable, cgroup headroom)` over whichever can be read.
pub fn memory_headroom() -> Option<u64> {
    match (mem_available(), cgroup_headroom()) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spool(dir: &Path, mb: u64, headroom: Option<u64>, free: u64) -> ArchiveSpool {
        ArchiveSpool::new(dir, mb).with_probes(move || headroom, move |_| Ok(free))
    }

    #[test]
    fn memory_within_budget_then_disk_then_no_room() {
        let dir = tempfile::tempdir().unwrap();
        let s = spool(dir.path(), 1, Some(100 * GIB), 10 * GIB);
        let mut a = s.place(MIB / 2, true).unwrap();
        if memfd_supported() {
            assert_eq!(a.kind(), PlacementKind::Memory);
            assert_eq!(s.in_memory_bytes(), MIB / 2);
        }
        // Over budget: disk.
        let b = s.place(MIB, true).unwrap();
        assert_eq!(b.kind(), PlacementKind::Disk);
        assert!(b.path().starts_with(s.disk_dir()));
        a.write(b"hello").unwrap();
        assert_eq!(std::fs::read(a.path()).unwrap(), b"hello");
        a.reset().unwrap();
        assert_eq!(std::fs::read(a.path()).unwrap(), b"");
        a.release();
        a.release();
        assert_eq!(s.in_memory_bytes(), 0);
        let named = b.path().to_path_buf();
        drop(b);
        assert!(!named.exists());

        let tight = spool(dir.path(), 0, None, DISK_MARGIN);
        match tight.place(10, true) {
            Err(SpoolError::NoRoom(msg)) => assert!(msg.contains("no room"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn memory_needs_machine_headroom() {
        let dir = tempfile::tempdir().unwrap();
        let s = spool(dir.path(), 2048, Some(GIB), 10 * GIB);
        assert_eq!(s.place(MIB, true).unwrap().kind(), PlacementKind::Disk);
    }

    #[test]
    fn sweep_empties_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let s = spool(dir.path(), 0, None, 10 * GIB);
        std::fs::create_dir_all(s.disk_dir().join("sub")).unwrap();
        std::fs::write(s.disk_dir().join("old.cbz"), b"x").unwrap();
        s.sweep();
        assert_eq!(std::fs::read_dir(s.disk_dir()).unwrap().count(), 0);
    }

    #[test]
    fn cgroup_v2_walks_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cg");
        let leaf = root.join("a/b");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(leaf.join("memory.max"), "max\n").unwrap();
        std::fs::write(leaf.join("memory.current"), "10\n").unwrap();
        std::fs::write(root.join("a/memory.max"), "1000\n").unwrap();
        std::fs::write(root.join("a/memory.current"), "400\n").unwrap();
        let proc = dir.path().join("cgroup");
        std::fs::write(&proc, "0::/a/b\n").unwrap();
        assert_eq!(cgroup_headroom_at(&proc, &root), Some(600));
        let _ = memory_headroom();
    }
}
