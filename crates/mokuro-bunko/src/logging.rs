//! Logging: console (INFO, DEBUG with `-v`) plus `<storage>/logs/server.log` rotated at
//! 2 MiB with 5 backups (0.5.2 `logging_setup.py`).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub const SERVER_LOG_NAME: &str = "server.log";
const MAX_BYTES: u64 = 2 * 1024 * 1024;
const BACKUPS: usize = 5;

/// A size-rotating file writer (`server.log`, `server.log.1` … `.5`).
#[derive(Clone)]
pub struct RotatingFile {
    inner: Arc<Mutex<Rotating>>,
}

struct Rotating {
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl RotatingFile {
    pub fn new(path: PathBuf) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Ok(Self { inner: Arc::new(Mutex::new(Rotating { path, file: None, size })) })
    }
}

impl Rotating {
    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        for i in (1..BACKUPS).rev() {
            let from = backup(&self.path, i);
            if from.exists() {
                let _ = std::fs::rename(&from, backup(&self.path, i + 1));
            }
        }
        let _ = std::fs::rename(&self.path, backup(&self.path, 1));
        self.size = 0;
        Ok(())
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size + buf.len() as u64 > MAX_BYTES && self.size > 0 {
            self.rotate()?;
        }
        if self.file.is_none() {
            self.file = Some(OpenOptions::new().create(true).append(true).open(&self.path)?);
        }
        let n = self.file.as_mut().map(|f| f.write(buf)).transpose()?.unwrap_or(0);
        self.size += n as u64;
        Ok(n)
    }
}

fn backup(path: &Path, i: usize) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{i}"));
    PathBuf::from(s)
}

pub struct Handle(Arc<Mutex<Rotating>>);

impl Write for Handle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().map_err(|_| io::Error::other("log lock poisoned"))?.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        if let Ok(mut g) = self.0.lock()
            && let Some(f) = g.file.as_mut()
        {
            f.flush()?;
        }
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for RotatingFile {
    type Writer = Handle;
    fn make_writer(&'a self) -> Handle {
        Handle(self.inner.clone())
    }
}

/// Console-only logging for CLI commands.
pub fn init_console(verbose: bool) {
    let filter = EnvFilter::try_from_env("MOKURO_LOG").unwrap_or_else(|_| EnvFilter::new(if verbose { "debug" } else { "info" }));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_target(false).try_init();
}

/// Console + rotating file for `serve` / `processor serve`.
pub fn init_server(storage: &Path, verbose: bool) {
    let console_level = if verbose { "debug" } else { "info" };
    let console_filter = EnvFilter::try_from_env("MOKURO_LOG").unwrap_or_else(|_| EnvFilter::new(format!("{console_level},hyper=warn,h2=warn,ort=warn")));
    let console = tracing_subscriber::fmt::layer().with_target(true).with_filter_reload_free(console_filter);
    let path = storage.join("logs").join(SERVER_LOG_NAME);
    match RotatingFile::new(path) {
        Ok(file) => {
            let file_layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file)
                .with_filter_reload_free(EnvFilter::new("info,hyper=warn,ort=warn"));
            let _ = tracing_subscriber::registry().with(console).with(file_layer).try_init();
        }
        Err(e) => {
            let _ = tracing_subscriber::registry().with(console).try_init();
            tracing::warn!("Could not create log file under {}: {e} (console logging only)", storage.display());
        }
    }
}

trait FilterExt<S>: Sized {
    fn with_filter_reload_free(self, f: EnvFilter) -> tracing_subscriber::filter::Filtered<Self, EnvFilter, S>;
}

impl<S, L> FilterExt<S> for L
where
    L: tracing_subscriber::Layer<S>,
    S: tracing::Subscriber,
{
    fn with_filter_reload_free(self, f: EnvFilter) -> tracing_subscriber::filter::Filtered<Self, EnvFilter, S> {
        tracing_subscriber::Layer::with_filter(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.log");
        let rf = RotatingFile::new(path.clone()).unwrap();
        let mut w = rf.make_writer();
        let chunk = vec![b'x'; 512 * 1024];
        for _ in 0..12 {
            w.write_all(&chunk).unwrap();
        }
        assert!(backup(&path, 1).exists());
        assert!(backup(&path, 2).exists());
        assert!(std::fs::metadata(&path).unwrap().len() <= MAX_BYTES);
        assert!(!backup(&path, 6).exists());
    }
}
