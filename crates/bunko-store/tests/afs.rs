#![cfg(feature = "tokio")]

use bunko_store::afs;

#[tokio::test]
async fn afs_passthrough() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("a/b");
    afs::create_dir_all(&dir).await.unwrap();
    let p = dir.join("f");
    let mut f = afs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&p)
        .await
        .unwrap();
    use tokio::io::AsyncWriteExt;
    f.write_all(b"hello").await.unwrap();
    f.sync_all().await.unwrap();
    drop(f);
    assert_eq!(afs::metadata(&p).await.unwrap().len(), 5);
    let q = dir.join("g");
    afs::rename(&p, &q).await.unwrap();
    let file = afs::File::open(&q).await.unwrap().into_std().unwrap();
    assert_eq!(file.metadata().await.unwrap().len(), 5);
    afs::remove_file(&q).await.unwrap();
    assert!(afs::metadata(&q).await.is_err());
}

/// A remote store holding one in-memory file, to drive `afs::File::Stream`.
mod mem {
    use bunko_store::*;
    use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};

    #[derive(Debug)]
    pub struct MemFile(pub Cursor<Vec<u8>>, pub Meta);

    impl Read for MemFile {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            // Short reads on purpose: the stream must loop.
            let n = buf.len().min(1000);
            self.0.read(&mut buf[..n])
        }
    }
    impl Write for MemFile {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Seek for MemFile {
        fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
            self.0.seek(p)
        }
    }
    impl RemoteFile for MemFile {
        fn metadata(&self) -> io::Result<Meta> {
            Ok(self.1.clone())
        }
        fn sync_all(&self) -> io::Result<()> {
            Ok(())
        }
        fn set_len(&self, _: u64) -> io::Result<()> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn into_local(self: Box<Self>) -> Result<std::fs::File, Box<dyn RemoteFile>> {
            Err(self)
        }
    }

    pub struct MemStore {
        pub root: PathBuf,
        pub data: Vec<u8>,
    }

    impl MemStore {
        fn meta(&self) -> Meta {
            Meta::Remote(RemoteMeta {
                size: self.data.len() as u64,
                mtime_ns: 1_700_000_000_000_000_000,
                ctime_s: 0,
                is_dir: false,
                dev: 9,
                ino: 1,
                cver: Some(Version::new("c1")),
                own: false,
                sha256: None,
            })
        }
    }

    fn no() -> io::Error {
        io::ErrorKind::Unsupported.into()
    }

    impl LibraryStore for MemStore {
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
        fn metadata(&self, _: &Path) -> io::Result<Meta> {
            Ok(self.meta())
        }
        fn symlink_metadata(&self, p: &Path) -> io::Result<Meta> {
            self.metadata(p)
        }
        fn read_dir(&self, _: &Path) -> io::Result<ReadDir> {
            Err(no())
        }
        fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
            Ok(p.into())
        }
        fn read_link(&self, _: &Path) -> io::Result<PathBuf> {
            Err(no())
        }
        fn open(&self, _: &Path) -> io::Result<File> {
            Ok(File::Remote(Box::new(MemFile(
                Cursor::new(self.data.clone()),
                self.meta(),
            ))))
        }
        fn local_path(&self, _: &Path, _: Option<&Version>) -> io::Result<LocalPath> {
            Err(no())
        }
        fn open_with(&self, _: &Path, _: &OpenOptions) -> io::Result<File> {
            Err(no())
        }
        fn publish(&self, _: &Path, _: &Path, _: &WriteIntent) -> io::Result<Published> {
            Err(no())
        }
        fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
            Err(no())
        }
        fn copy(&self, _: &Path, _: &Path) -> io::Result<u64> {
            Err(no())
        }
        fn remove_file(&self, _: &Path) -> io::Result<()> {
            Err(no())
        }
        fn remove_dir(&self, _: &Path) -> io::Result<()> {
            Err(no())
        }
        fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
            Err(no())
        }
        fn create_dir(&self, _: &Path) -> io::Result<()> {
            Err(no())
        }
        fn create_dir_all(&self, _: &Path) -> io::Result<()> {
            Err(no())
        }
        fn watch(&self, _: WatchSink) -> io::Result<WatchGuard> {
            Err(no())
        }
    }
}

async fn collect(s: afs::BodyStream) -> std::io::Result<Vec<u8>> {
    use futures_util::StreamExt;
    let mut out = Vec::new();
    let mut s = s;
    while let Some(chunk) = s.next().await {
        let chunk = chunk?;
        assert!(chunk.len() <= afs::STREAM_CHUNK);
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// A remote file opens as a stream (no local file), with version-bound metadata, and
/// any range of it streams in bounded chunks.
#[tokio::test(flavor = "multi_thread")]
async fn remote_files_open_as_streams() {
    let data: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
    let root = std::path::PathBuf::from("/afs-stream-test-store");
    let _m = bunko_store::mount(std::sync::Arc::new(mem::MemStore {
        root: root.clone(),
        data: data.clone(),
    }));
    let p = root.join("a.cbz");
    let f = afs::File::open(&p).await.unwrap();
    assert!(matches!(f, afs::File::Stream(_)));
    assert_eq!(f.metadata().await.unwrap().len(), data.len() as u64);
    assert!(afs::File::open(&p).await.unwrap().into_std().is_none());
    let all = collect(f.into_stream(0, data.len() as u64)).await.unwrap();
    assert_eq!(all, data);
    let f = afs::File::open(&p).await.unwrap();
    let part = collect(f.into_stream(300_001, 5000)).await.unwrap();
    assert_eq!(part, &data[300_001..305_001]);
    // Asking past the end: the stream fails instead of ending short.
    let f = afs::File::open(&p).await.unwrap();
    let e = collect(f.into_stream(699_990, 20)).await.unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
}

/// A local file streams the same way.
#[tokio::test]
async fn local_files_stream_too() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("f");
    std::fs::write(&p, b"0123456789").unwrap();
    let f = afs::File::open(&p).await.unwrap();
    assert_eq!(collect(f.into_stream(3, 4)).await.unwrap(), b"3456");
}
