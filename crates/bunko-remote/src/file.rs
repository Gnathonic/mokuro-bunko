//! The open files of a RemoteStore ([`bunko_store::RemoteFile`] impls).
//!
//! - [`CachedFile`]: a small file's bytes from the blob cache.
//! - [`ArchiveFile`]: Read + Seek over a large remote file. The tail (central
//!   directory, end record) comes from the zipdir cache, filled by one pinned read of
//!   the last 256 KiB the first time a read lands there; other ranges come from the
//!   spool when one exists (waiting for a progressive fill when the read is within
//!   4 MiB of it), else from pinned ranged reads through the 256 KiB block cache. A
//!   handle whose first operation is a read at offset 0 (a whole-file consumer such
//!   as a copy or an HTTP GET) joins or starts the spool instead, falling back to
//!   ranged passthrough when the spool budget is full. A handle that seeks first (the
//!   zip reader) never starts a download.
//! - [`PendingFile`]: the staged bytes of a pending Deferred write.
//! - [`StagingFile`]: the local staging file `open_with` hands out.
//!
//! A version-pin mismatch fails the read with `io::Error(Other, BErr::PreconditionFailed)`
//! and caches nothing (D24).

use std::fmt;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;

use bunko_store::{Meta, RemoteFile};
use bytes::Bytes;

use crate::backend::{BErr, Backend};
use crate::cache::{
    BLOCK, MAX_CD, SPOOL_AHEAD, SpoolPinGuard, TAIL, ZipTail, locate_central_directory,
};
use crate::store::{RemoteStore, Ver, berr_to_io};

/// The largest read-ahead of a sequential ranged reader.
const MAX_READAHEAD: u64 = 4 << 20;

fn read_only() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "a remote library file opened for reading cannot be written",
    )
}

/// A small remote file, served from the blob cache.
pub struct CachedFile {
    data: Cursor<Vec<u8>>,
    meta: Meta,
}

impl CachedFile {
    pub(crate) fn new(data: Vec<u8>, meta: Meta) -> CachedFile {
        CachedFile {
            data: Cursor::new(data),
            meta,
        }
    }
}

impl fmt::Debug for CachedFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedFile")
            .field("len", &self.data.get_ref().len())
            .field("version", &self.meta.version())
            .finish()
    }
}

impl Read for CachedFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.data.read(buf)
    }
}

impl Seek for CachedFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.data.seek(pos)
    }
}

impl Write for CachedFile {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(read_only())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl RemoteFile for CachedFile {
    fn metadata(&self) -> io::Result<Meta> {
        Ok(self.meta.clone())
    }

    fn sync_all(&self) -> io::Result<()> {
        Ok(())
    }

    fn set_len(&self, _len: u64) -> io::Result<()> {
        Err(read_only())
    }
}

/// A large remote file, read with version-pinned ranged reads.
pub struct ArchiveFile<B: Backend> {
    store: Arc<RemoteStore<B>>,
    v: Ver,
    meta: Meta,
    pos: u64,
    /// No read or seek happened yet.
    fresh: bool,
    /// The first operation was a read at 0: a whole-file consumer.
    sequential: bool,
    tail: Option<Arc<ZipTail>>,
    tail_fetched: bool,
    spool: Option<(SpoolPinGuard, Option<std::fs::File>)>,
    /// A spool was looked for (or refused) already.
    spool_tried: bool,
    last_end: u64,
    readahead: u64,
}

impl<B: Backend> ArchiveFile<B> {
    pub(crate) fn new(store: Arc<RemoteStore<B>>, v: Ver, meta: Meta) -> ArchiveFile<B> {
        let tail = store.core.zipdir.get(&v.pid, &v.cver, v.size);
        ArchiveFile {
            store,
            v,
            meta,
            pos: 0,
            fresh: true,
            sequential: false,
            tail,
            tail_fetched: false,
            spool: None,
            spool_tried: false,
            last_end: u64::MAX,
            readahead: BLOCK,
        }
    }

    fn pinned(&self, range: (u64, u64)) -> io::Result<Vec<u8>> {
        self.store
            .block_on("read", self.store.core.read_pinned(&self.v, Some(range)))
            .map_err(berr_to_io)
    }

    /// Fetch and cache the archive's tail (and a central directory that starts
    /// before it, up to [`MAX_CD`]).
    fn fetch_tail(&mut self) -> io::Result<()> {
        self.tail_fetched = true;
        let size = self.v.size;
        let start = size - TAIL.min(size);
        let mut bytes = self.pinned((start, size - 1))?;
        let (eocd_off, cd_start) = locate_central_directory(&bytes, start);
        let mut start = start;
        if let Some(cd) = cd_start
            && cd < start
            && start - cd <= MAX_CD
        {
            let mut head = self.pinned((cd, start - 1))?;
            head.extend_from_slice(&bytes);
            bytes = head;
            start = cd;
        }
        let t = Arc::new(ZipTail {
            start,
            eocd_off,
            bytes,
        });
        self.store
            .core
            .zipdir
            .put(&self.v.pid, &self.v.cver, t.clone());
        self.tail = Some(t);
        Ok(())
    }

    /// Serve `buf` from the spool when it holds (or will soon hold) `pos`.
    fn read_spool(&mut self, pos: u64, buf: &mut [u8]) -> io::Result<Option<usize>> {
        if self.spool.is_none() && !self.spool_tried {
            self.spool_tried = true;
            let key = self.v.key();
            let pin = match self.store.core.spool.attach(&key) {
                Some(p) => Some(p),
                None if self.sequential => self.store.spool_for(&self.v, false)?,
                None => None,
            };
            self.spool = pin.map(|p| (p, None));
        }
        loop {
            let Some((pin, _)) = &self.spool else {
                return Ok(None);
            };
            let entry = pin.entry.clone();
            let fill = entry.fill();
            if pos < fill.filled {
                let n = (buf.len() as u64).min(fill.filled - pos) as usize;
                let (_, file) = self.spool.as_mut().expect("checked above");
                if file.is_none() {
                    *file = Some(std::fs::File::open(&entry.path)?);
                }
                let f = file.as_mut().expect("opened above");
                f.seek(SeekFrom::Start(pos))?;
                return f.read(&mut buf[..n]).map(Some);
            }
            match fill.done {
                Some(Err(BErr::PreconditionFailed)) => {
                    self.spool = None;
                    return Err(berr_to_io(BErr::PreconditionFailed));
                }
                Some(_) => {
                    // Failed otherwise (or complete, so `pos` is past the end): ranged.
                    self.spool = None;
                    return Ok(None);
                }
                None if pos < fill.filled.saturating_add(SPOOL_AHEAD) => {
                    self.store.block_on("read", entry.wait(pos + 1));
                }
                None => return Ok(None),
            }
        }
    }

    /// Serve `buf` from the block containing `pos`, fetching it (with read-ahead for
    /// sequential access) by one pinned ranged read.
    fn read_blocks(&mut self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        let core = self.store.core.clone();
        let (pid, cver, size) = (&self.v.pid, &self.v.cver, self.v.size);
        let b0 = pos / BLOCK;
        let block = match core.blocks.get(pid, cver, b0) {
            Some(b) => b,
            None => {
                self.readahead = if pos == self.last_end {
                    (self.readahead * 2).min(MAX_READAHEAD)
                } else {
                    BLOCK
                };
                let end = size.min(pos.saturating_add((buf.len() as u64).max(self.readahead)));
                let b_last = (end - 1) / BLOCK;
                let mut b1 = b0;
                while b1 < b_last && !core.blocks.contains(pid, cver, b1 + 1) {
                    b1 += 1;
                }
                let first = b0 * BLOCK;
                let last = size.min((b1 + 1).saturating_mul(BLOCK)) - 1;
                let data = Bytes::from(self.pinned((first, last))?);
                let mut b = b0;
                let mut off = 0usize;
                while off < data.len() {
                    let n = (BLOCK as usize).min(data.len() - off);
                    core.blocks.put(pid, cver, b, data.slice(off..off + n));
                    off += n;
                    b += 1;
                }
                data.slice(0..(BLOCK as usize).min(data.len()))
            }
        };
        let at = (pos - b0 * BLOCK) as usize;
        if at >= block.len() {
            return Ok(0);
        }
        let n = buf.len().min(block.len() - at);
        buf[..n].copy_from_slice(&block[at..at + n]);
        Ok(n)
    }
}

impl<B: Backend> fmt::Debug for ArchiveFile<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArchiveFile")
            .field("pid", &self.v.pid)
            .field("cver", &self.v.cver)
            .field("size", &self.v.size)
            .field("pos", &self.pos)
            .finish()
    }
}

impl<B: Backend> Read for ArchiveFile<B> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let size = self.v.size;
        if self.fresh {
            self.fresh = false;
            self.sequential = self.pos == 0;
        }
        if buf.is_empty() || self.pos >= size {
            return Ok(0);
        }
        let pos = self.pos;
        let mut want = (buf.len() as u64).min(size - pos) as usize;
        // 1. The cached tail.
        if let Some(t) = &self.tail {
            if pos >= t.start {
                let at = (pos - t.start) as usize;
                let n = want.min(t.bytes.len() - at);
                buf[..n].copy_from_slice(&t.bytes[at..at + n]);
                self.pos += n as u64;
                return Ok(n);
            }
            want = want.min((t.start - pos) as usize);
        }
        // 2. The spool.
        if let Some(n) = self.read_spool(pos, &mut buf[..want])? {
            self.pos += n as u64;
            self.last_end = self.pos;
            return Ok(n);
        }
        // 3. The tail, fetched once, when the read lands in it.
        if self.tail.is_none() && !self.tail_fetched && pos >= size - TAIL.min(size) {
            self.fetch_tail()?;
            return self.read(buf);
        }
        // 4. Blocks.
        let n = self.read_blocks(pos, &mut buf[..want])?;
        self.pos += n as u64;
        self.last_end = self.pos;
        Ok(n)
    }
}

impl<B: Backend> Seek for ArchiveFile<B> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.fresh = false;
        let size = self.v.size as i128;
        let target = match to {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::End(d) => size + i128::from(d),
            SeekFrom::Current(d) => i128::from(self.pos) + i128::from(d),
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a negative or overflowing position",
            ));
        }
        self.pos =
            u64::try_from(target).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        Ok(self.pos)
    }
}

impl<B: Backend> Write for ArchiveFile<B> {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(read_only())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<B: Backend> RemoteFile for ArchiveFile<B> {
    fn metadata(&self) -> io::Result<Meta> {
        Ok(self.meta.clone())
    }

    fn sync_all(&self) -> io::Result<()> {
        Ok(())
    }

    fn set_len(&self, _len: u64) -> io::Result<()> {
        Err(read_only())
    }
}

/// The staged bytes of a pending Deferred write (read-after-write), read-only.
pub struct PendingFile {
    file: std::fs::File,
    meta: Meta,
}

impl PendingFile {
    pub(crate) fn new(file: std::fs::File, meta: Meta) -> PendingFile {
        PendingFile { file, meta }
    }
}

impl fmt::Debug for PendingFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingFile")
            .field("version", &self.meta.version())
            .finish()
    }
}

impl Read for PendingFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Seek for PendingFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl Write for PendingFile {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(read_only())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl RemoteFile for PendingFile {
    fn metadata(&self) -> io::Result<Meta> {
        Ok(self.meta.clone())
    }

    fn sync_all(&self) -> io::Result<()> {
        Ok(())
    }

    fn set_len(&self, _len: u64) -> io::Result<()> {
        Err(read_only())
    }
}

/// A local staging file under `<dir>/staging/` for the library path `dest`: what
/// `open_with` hands out. It reaches the cloud only through an explicit `publish`
/// (or a `rename` of `dest`), never on drop; until then it is a local-only file.
pub struct StagingFile {
    pub(crate) file: std::fs::File,
    pub(crate) path: PathBuf,
    pub(crate) dest: PathBuf,
}

impl fmt::Debug for StagingFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StagingFile")
            .field("path", &self.path)
            .field("dest", &self.dest)
            .finish()
    }
}

impl Read for StagingFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for StagingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for StagingFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl RemoteFile for StagingFile {
    fn metadata(&self) -> io::Result<Meta> {
        self.file.metadata().map(Meta::Std)
    }

    fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }
}
