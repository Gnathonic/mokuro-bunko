//! Reading (design §6): what people read, from the per-user files the reader uploads
//! (`positions.json`, `history/**`, `volume-data.json`), read back from disk after they
//! are written, off the request path.
//!
//! The collector calls [`Reading::on_file_written`] for every successful (2xx) PUT,
//! DELETE or MOVE of a per-user file; it queues the event on a bounded channel (drops
//! counted), never blocking the request.
//!
//! FOUNDATION STUB (area B1 fills it in): the worker [`Reading::start`] runs only
//! drains the queue; the API says "not collected yet".

pub mod api;

use crate::analytics::Analytics;
use bunko_dav::DavTarget;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How many events wait for the worker before new ones are dropped.
pub const QUEUE: usize = 1024;

/// A per-user file that was just written (or removed, or moved into place).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileWritten {
    pub username: String,
    /// [`DavTarget::Progress`] (the root files and `history/**`).
    pub target: DavTarget,
    /// The per-user-relative path: `positions.json`, `history/<device>/<file>`, ...
    pub rel: String,
    /// `PUT`, `DELETE` or `MOVE` (a MOVE reports its destination).
    pub method: &'static str,
    /// Epoch milliseconds the request started.
    pub at_ms: i64,
}

pub struct Reading {
    tx: mpsc::Sender<FileWritten>,
    /// Until the worker takes it.
    rx: Mutex<Option<mpsc::Receiver<FileWritten>>>,
    queued: AtomicU64,
    dropped: AtomicU64,
}

impl Reading {
    pub fn new() -> Reading {
        let (tx, rx) = mpsc::channel(QUEUE);
        Reading {
            tx,
            rx: Mutex::new(Some(rx)),
            queued: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    /// Queue a written per-user file for the worker (never blocks; a full queue drops
    /// the event and counts it).
    pub fn on_file_written(&self, ev: FileWritten) {
        match self.tx.try_send(ev) {
            Ok(()) => {
                self.queued.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Events queued since start.
    pub fn queued(&self) -> u64 {
        self.queued.load(Ordering::Relaxed)
    }

    /// Events dropped (queue full) since start.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Take the queue's receiving end (the worker, once).
    pub fn take_receiver(&self) -> Option<mpsc::Receiver<FileWritten>> {
        self.rx.lock().take()
    }

    /// Start the worker: debounce per (user, file) for 10 s, read the file back, update
    /// the reading tables.
    pub fn start(&self, _a: &Analytics, stop: CancellationToken) {
        let Some(mut rx) = self.take_receiver() else {
            return;
        };
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    ev = rx.recv() => if ev.is_none() { return },
                }
            }
        });
    }
}

impl Default for Reading {
    fn default() -> Self {
        Self::new()
    }
}
