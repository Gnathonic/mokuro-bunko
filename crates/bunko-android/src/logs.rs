//! Logging for the embedded server: logcat (tag `mokuro-bunko`), the usual
//! `<storage>/logs/server.log` (rotated at 2 MiB, 3 backups), and an in-memory tail the
//! settings screen shows (`logTail()`), since a phone user cannot easily read either.
//!
//! The global subscriber is installed once per process; the file follows the storage
//! directory of the latest `start`.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub const SERVER_LOG_NAME: &str = "server.log";
const MAX_BYTES: u64 = 2 * 1024 * 1024;
const BACKUPS: usize = 3;
const TAIL_LINES: usize = 300;

static FILE: Mutex<Option<Rotating>> = Mutex::new(None);
static TAIL: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
static INIT: OnceLock<()> = OnceLock::new();

/// Install the subscriber (first call only) and point the file log at `storage/logs`.
pub fn init(storage: &Path) {
    set_log_dir(&storage.join("logs"));
    INIT.get_or_init(|| {
        let filter = || EnvFilter::new("info,hyper=warn,h2=warn,rustls=warn");
        let file = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(FileWriter)
            .with_filter(filter());
        let tail = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(false)
            .with_writer(TailWriter)
            .with_filter(filter());
        let registry = tracing_subscriber::registry().with(file).with(tail);
        #[cfg(target_os = "android")]
        let registry = registry.with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .without_time() // logcat stamps lines itself
                .with_writer(logcat::Logcat)
                .with_filter(filter()),
        );
        let _ = registry.try_init();
    });
}

fn set_log_dir(dir: &Path) {
    let path = dir.join(SERVER_LOG_NAME);
    let mut slot = FILE.lock();
    if slot.as_ref().is_some_and(|r| r.path == path) {
        return;
    }
    *slot = match std::fs::create_dir_all(dir) {
        Ok(()) => Some(Rotating {
            size: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            path,
            file: None,
        }),
        Err(_) => None, // logcat and the tail still work
    };
}

/// The last log lines, oldest first.
pub fn tail() -> String {
    let t = TAIL.lock();
    let mut out = String::with_capacity(t.iter().map(|l| l.len() + 1).sum());
    for line in t.iter() {
        out.push_str(line);
        if !line.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Add a line to the tail without going through tracing (start errors before init).
pub fn note(line: impl Into<String>) {
    push_tail(line.into());
}

fn push_tail(line: String) {
    let mut t = TAIL.lock();
    if t.len() == TAIL_LINES {
        t.pop_front();
    }
    t.push_back(line);
}

struct Rotating {
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl Rotating {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size > 0 && self.size + buf.len() as u64 > MAX_BYTES {
            self.file = None;
            for i in (1..BACKUPS).rev() {
                let _ = std::fs::rename(backup(&self.path, i), backup(&self.path, i + 1));
            }
            let _ = std::fs::rename(&self.path, backup(&self.path, 1));
            self.size = 0;
        }
        if self.file.is_none() {
            self.file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?,
            );
        }
        let n = match self.file.as_mut() {
            Some(f) => f.write(buf)?,
            None => 0,
        };
        self.size += n as u64;
        Ok(n)
    }
}

fn backup(path: &Path, i: usize) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{i}"));
    PathBuf::from(s)
}

struct FileWriter;

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match FILE.lock().as_mut() {
            Some(r) => r.write(buf),
            None => Ok(buf.len()),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        if let Some(f) = FILE.lock().as_mut().and_then(|r| r.file.as_mut()) {
            f.flush()?;
        }
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for FileWriter {
    type Writer = FileWriter;
    fn make_writer(&'a self) -> FileWriter {
        FileWriter
    }
}

/// Buffers one formatted event and appends it to the tail when dropped.
#[derive(Default)]
struct TailEvent(Vec<u8>);

impl Write for TailEvent {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for TailEvent {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            push_tail(String::from_utf8_lossy(&self.0).trim_end().to_string());
        }
    }
}

struct TailWriter;

impl<'a> MakeWriter<'a> for TailWriter {
    type Writer = TailEvent;
    fn make_writer(&'a self) -> TailEvent {
        TailEvent::default()
    }
}

#[cfg(target_os = "android")]
mod logcat {
    //! A writer that sends each formatted event to `__android_log_write` (liblog, part
    //! of every Android system; no crate needed).
    use std::ffi::CString;
    use std::io::{self, Write};
    use std::os::raw::{c_char, c_int};
    use tracing::Level;
    use tracing_subscriber::fmt::MakeWriter;

    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    const TAG: &[u8] = b"mokuro-bunko\0";

    pub struct Logcat;

    pub struct Event {
        prio: c_int,
        buf: Vec<u8>,
    }

    impl Write for Event {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buf.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for Event {
        fn drop(&mut self) {
            let mut text = std::mem::take(&mut self.buf);
            text.retain(|b| *b != 0);
            while text.last() == Some(&b'\n') {
                text.pop();
            }
            if let Ok(text) = CString::new(text) {
                // SAFETY: both pointers are NUL-terminated strings that outlive the call.
                unsafe {
                    __android_log_write(self.prio, TAG.as_ptr().cast(), text.as_ptr());
                }
            }
        }
    }

    impl<'a> MakeWriter<'a> for Logcat {
        type Writer = Event;
        fn make_writer(&'a self) -> Event {
            Event {
                prio: 4,
                buf: Vec::new(),
            }
        }
        fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Event {
            // android/log.h: VERBOSE 2, DEBUG 3, INFO 4, WARN 5, ERROR 6.
            let prio = match *meta.level() {
                Level::TRACE => 2,
                Level::DEBUG => 3,
                Level::INFO => 4,
                Level::WARN => 5,
                Level::ERROR => 6,
            };
            Event {
                prio,
                buf: Vec::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_and_tails() {
        let d = tempfile::tempdir().unwrap();
        let mut r = Rotating {
            path: d.path().join("server.log"),
            file: None,
            size: 0,
        };
        let chunk = vec![b'x'; 512 * 1024];
        for _ in 0..20 {
            r.write(&chunk).unwrap();
        }
        assert!(backup(&r.path, 1).exists());
        assert!(backup(&r.path, 3).exists());
        assert!(!backup(&r.path, 4).exists());
        assert!(std::fs::metadata(&r.path).unwrap().len() <= MAX_BYTES);

        for i in 0..(TAIL_LINES + 5) {
            note(format!("line {i}"));
        }
        let t = tail();
        assert_eq!(t.lines().count(), TAIL_LINES);
        // Other tests log concurrently into the same tail: check eviction, not order.
        assert!(t.contains(&format!("line {}\n", TAIL_LINES + 4)));
        assert!(!t.contains("line 0\n"));
    }
}
