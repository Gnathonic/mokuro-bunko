//! The `tokio::fs` mirror. Unmounted and local paths call `tokio::fs`; a path in a
//! remote store runs the sync store call on the blocking pool.

use std::io;
use std::path::{Path, PathBuf};

use crate::meta::Meta;
use crate::mount::store_for;

fn local(p: &Path) -> bool {
    store_for(p).is_none_or(|s| !s.caps().remote)
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
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
}

impl File {
    pub async fn open(p: impl AsRef<Path>) -> io::Result<File> {
        let p = p.as_ref();
        if local(p) {
            return tokio::fs::File::open(p).await.map(File::Tokio);
        }
        let p = p.to_path_buf();
        let f = blocking(move || crate::fs::File::open(&p)).await?;
        match f.into_std() {
            Some(std) => Ok(File::Tokio(tokio::fs::File::from_std(std))),
            None => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this store has no local file to open",
            )),
        }
    }

    /// The tokio file, when the file is local.
    pub fn into_std(self) -> Option<tokio::fs::File> {
        match self {
            File::Tokio(f) => Some(f),
        }
    }
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
        let (p, o) = (p.to_path_buf(), self.0.clone());
        let f = blocking(move || o.open(&p)).await?;
        f.into_std().map(tokio::fs::File::from_std).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "this store has no local file to open",
            )
        })
    }
}
