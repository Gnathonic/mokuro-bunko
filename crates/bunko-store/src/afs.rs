//! The `tokio::fs` mirror. Unmounted and local paths call `tokio::fs`; a path in a
//! remote store runs the sync store call on the blocking pool.

use std::io;
use std::path::{Path, PathBuf};

use crate::meta::Meta;
use crate::mount::store_for;

fn local(p: &Path) -> bool {
    store_for(p).is_none_or(|s| !s.caps().remote)
}

/// The tokio shim serves requests (bridge reads and writes, catalog covers), so its
/// store calls run in the interactive latency class: a provider pause answers at once
/// with a retry-after error instead of holding the request (spec §3.1).
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(move || crate::interactive(f))
        .await
        .map_err(io::Error::other)?
}

pub async fn metadata(p: impl AsRef<Path>) -> io::Result<Meta> {
    let p = p.as_ref();
    if local(p) {
        return tokio::fs::metadata(p).await.map(Meta::Std);
    }
    let p = p.to_path_buf();
    blocking(move || crate::fs::metadata(&p)).await
}

pub async fn create_dir_all(p: impl AsRef<Path>) -> io::Result<()> {
    let p = p.as_ref();
    if local(p) {
        return tokio::fs::create_dir_all(p).await;
    }
    let p = p.to_path_buf();
    blocking(move || crate::fs::create_dir_all(&p)).await
}

pub async fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    if local(from) && local(to) {
        return tokio::fs::rename(from, to).await;
    }
    let (from, to): (PathBuf, PathBuf) = (from.into(), to.into());
    blocking(move || crate::fs::rename(&from, &to)).await
}

pub async fn remove_file(p: impl AsRef<Path>) -> io::Result<()> {
    let p = p.as_ref();
    if local(p) {
        return tokio::fs::remove_file(p).await;
    }
    let p = p.to_path_buf();
    blocking(move || crate::fs::remove_file(&p)).await
}

/// An opened file. Local files are tokio's; remote reads arrive with M2's stream.
#[derive(Debug)]
pub enum File {
    Tokio(tokio::fs::File),
    /// A remote store's file (spec §10): its bytes come through
    /// [`File::into_stream`], read on the blocking pool in chunks, so memory stays
    /// flat whatever the size.
    Stream(RemoteStream),
}

/// A remote store's open file and the metadata of the version it reads. The file sits
/// behind a mutex only so that `&File` is `Sync` (futures that hold it stay `Send`);
/// it is never contended.
#[derive(Debug)]
pub struct RemoteStream {
    file: parking_lot::Mutex<crate::file::File>,
    meta: Meta,
}

/// A file body: chunks of at most [`STREAM_CHUNK`] bytes.
pub type BodyStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = io::Result<bytes::Bytes>> + Send>>;

/// The largest chunk [`File::into_stream`] yields.
pub const STREAM_CHUNK: usize = 256 * 1024;

impl File {
    pub async fn open(p: impl AsRef<Path>) -> io::Result<File> {
        let p = p.as_ref();
        if local(p) {
            return tokio::fs::File::open(p).await.map(File::Tokio);
        }
        let p = p.to_path_buf();
        blocking(move || {
            let f = crate::fs::File::open(&p)?;
            if let Some(std) = f.as_std() {
                let std = std.try_clone()?;
                return Ok(File::Tokio(tokio::fs::File::from_std(std)));
            }
            let meta = f.metadata()?;
            Ok(File::Stream(RemoteStream {
                file: parking_lot::Mutex::new(f),
                meta,
            }))
        })
        .await
    }

    /// The metadata of what was opened (a remote file's is bound to its version).
    pub async fn metadata(&self) -> io::Result<Meta> {
        match self {
            File::Tokio(f) => f.metadata().await.map(Meta::Std),
            File::Stream(s) => Ok(s.meta.clone()),
        }
    }

    /// The tokio file, when the file is local.
    pub fn into_std(self) -> Option<tokio::fs::File> {
        match self {
            File::Tokio(f) => Some(f),
            File::Stream(_) => None,
        }
    }

    /// `len` bytes from offset `start`, as a stream. A remote file reads on the
    /// blocking pool, one chunk at a time; a file that ends before `len` bytes (a new
    /// version replaced the one opened) ends the stream with `UnexpectedEof`.
    pub fn into_stream(self, start: u64, len: u64) -> BodyStream {
        use futures_util::StreamExt;
        enum St {
            Tokio(tokio::fs::File),
            Remote(crate::file::File),
        }
        let st = match self {
            File::Tokio(f) => St::Tokio(f),
            File::Stream(s) => St::Remote(s.file.into_inner()),
        };
        let init = (Some(st), start, len, true);
        futures_util::stream::unfold(init, |(st, start, left, first)| async move {
            let st = st?;
            if left == 0 {
                return None;
            }
            let want = left.min(STREAM_CHUNK as u64) as usize;
            let res: io::Result<(St, Vec<u8>)> = match st {
                St::Tokio(mut f) => {
                    use tokio::io::AsyncSeekExt;
                    async {
                        if first && start > 0 {
                            f.seek(io::SeekFrom::Start(start)).await?;
                        }
                        let mut buf = vec![0u8; want];
                        let n = read_full_async(&mut f, &mut buf).await?;
                        buf.truncate(n);
                        Ok((St::Tokio(f), buf))
                    }
                    .await
                }
                St::Remote(mut f) => {
                    blocking(move || {
                        use std::io::{Read, Seek};
                        if first && start > 0 {
                            f.seek(io::SeekFrom::Start(start))?;
                        }
                        let mut buf = vec![0u8; want];
                        let mut n = 0;
                        while n < want {
                            match f.read(&mut buf[n..]) {
                                Ok(0) => break,
                                Ok(k) => n += k,
                                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                                Err(e) => return Err(e),
                            }
                        }
                        buf.truncate(n);
                        Ok((St::Remote(f), buf))
                    })
                    .await
                }
            };
            match res {
                Ok((_, buf)) if buf.is_empty() => Some((
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the file ended before its announced length",
                    )),
                    (None, start, 0, false),
                )),
                Ok((st, buf)) => {
                    let n = buf.len() as u64;
                    Some((
                        Ok(bytes::Bytes::from(buf)),
                        (Some(st), start, left - n, false),
                    ))
                }
                Err(e) => Some((Err(e), (None, start, 0, false))),
            }
        })
        .boxed()
    }
}

/// Fill `buf` from `f` (short only at the end of the file).
async fn read_full_async(f: &mut tokio::fs::File, buf: &mut [u8]) -> io::Result<usize> {
    use tokio::io::AsyncReadExt;
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]).await {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// `tokio::fs::OpenOptions`, store-routed.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions(crate::file::OpenOptions);

impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read(&mut self, v: bool) -> &mut Self {
        self.0.read(v);
        self
    }

    pub fn write(&mut self, v: bool) -> &mut Self {
        self.0.write(v);
        self
    }

    pub fn append(&mut self, v: bool) -> &mut Self {
        self.0.append(v);
        self
    }

    pub fn truncate(&mut self, v: bool) -> &mut Self {
        self.0.truncate(v);
        self
    }

    pub fn create(&mut self, v: bool) -> &mut Self {
        self.0.create(v);
        self
    }

    pub fn create_new(&mut self, v: bool) -> &mut Self {
        self.0.create_new(v);
        self
    }

    pub fn mode(&mut self, m: u32) -> &mut Self {
        self.0.mode(m);
        self
    }

    pub async fn open(&self, p: impl AsRef<Path>) -> io::Result<tokio::fs::File> {
        let p = p.as_ref();
        if local(p) {
            return tokio::fs::OpenOptions::from(self.0.to_std()).open(p).await;
        }
        // A remote store's write-open is a local staging file (spec §3.3), written
        // here at local speed and published later; a read-open streams from the
        // provider and has no local file (`File::open` serves those).
        let (p, o) = (p.to_path_buf(), self.0.clone());
        let f = blocking(move || o.open(&p)).await?;
        f.into_local().map(tokio::fs::File::from_std).map_err(|_| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "this store has no local file to open (open it with afs::File::open)",
            )
        })
    }
}
