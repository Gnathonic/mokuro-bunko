//! The in-process local processor: the same session runtime the remote processor
//! runs, driven over tokio channels by the library server itself (ARCHITECTURE §6).
//!
//! Differences from a remote processor, all at the link:
//!
//! * `VolumeOp.archive` is a filesystem path. It is read in place: no download, no
//!   verification, no spool, no `fetch` events. A path that is not a file is
//!   returned as `volume_returned{class: "missing"}`.
//! * The sidecar is written to `<results_dir>/<sid>/<claim>/result.mokuro`
//!   ([`bunko_proto::RESULT_FILE`], never the volume's own name) — the same
//!   `{sid}/{claim}` keying and file name as the remote upload path, so the server
//!   collects both the same way. `volume_done`
//!   carries its `sidecar_sha256`. After a `volume_done` the server owns
//!   `<results_dir>/<sid>/<claim>/` and removes it once it has collected the file;
//!   for every other ending the processor removes it itself.
//! * Dropping the op sender (or [`LocalLink::shutdown`]) is the processor leaving:
//!   every session is abandoned and nothing more is said.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use bunko_proto::{Event, Op};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bench::BenchConfig;
use crate::pipeline::{MachineInfo, PagePipeline};
use crate::session::{Hub, Link, LocalLink as LocalPaths};

/// Where the local processor writes sidecars for the server to collect.
#[derive(Debug, Clone)]
pub struct LocalConfig {
    pub results_dir: PathBuf,
}

impl LocalConfig {
    /// `<results_dir>/<sid>/<claim>/result.mokuro`: where a claim's sidecar lands.
    pub fn result_path(&self, sid: &str, claim: &str) -> PathBuf {
        self.results_dir
            .join(sid)
            .join(claim)
            .join(bunko_proto::RESULT_FILE)
    }
}

/// The server's end of a running local processor.
pub struct LocalLink {
    /// Ops in (the same [`Op`]s a remote processor receives).
    pub ops: mpsc::Sender<Op>,
    /// Events out (the same [`Event`]s a remote processor sends).
    pub events: mpsc::UnboundedReceiver<Event>,
    shutdown: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl LocalLink {
    /// The processor's task, to await once the op sender is dropped: it ends after
    /// every session wound down (up to 10 s), their models freed. Taken by an owner that
    /// hands `ops` / `events` on; [`LocalLink::shutdown`] no longer waits after this.
    pub fn take_finished(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        self.task.take()
    }

    /// Abandon every session silently and wait (up to 10 s) for them to wind down.
    /// Nothing more arrives on `events` afterwards.
    pub async fn shutdown(&mut self) {
        self.shutdown.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

/// The in-process processor.
pub struct LocalProcessor;

impl LocalProcessor {
    /// What this machine can run (the local entry's catalog).
    pub fn describe(pipeline: &dyn PagePipeline) -> MachineInfo {
        pipeline.describe()
    }

    /// Start it on the current tokio runtime.
    pub fn spawn(pipeline: Arc<dyn PagePipeline>, config: LocalConfig) -> LocalLink {
        Self::spawn_with(pipeline, config, BenchConfig::default())
    }

    /// [`LocalProcessor::spawn`] with the benchmark's numbers (tests shrink them).
    pub fn spawn_with(
        pipeline: Arc<dyn PagePipeline>,
        config: LocalConfig,
        bench: BenchConfig,
    ) -> LocalLink {
        Self::spawn_controlled(pipeline, config, bench, None)
    }

    /// [`LocalProcessor::spawn_with`] under a control API (GUI.md §3): the server's own
    /// OCR obeys the same pause as a remote processor (announced on `events` as
    /// `availability` / `released`) and reports into the control's activity (the
    /// caller labels its devices: `Control::set_devices` with the described catalog).
    pub fn spawn_controlled(
        pipeline: Arc<dyn PagePipeline>,
        config: LocalConfig,
        bench: BenchConfig,
        control: Option<bunko_control::Control>,
    ) -> LocalLink {
        let (ops_tx, mut ops_rx) = mpsc::channel::<Op>(64);
        let (events_tx, events_rx) = mpsc::unbounded_channel::<Event>();
        let shutdown = CancellationToken::new();
        let leaving = Arc::new(AtomicBool::new(false));
        let hub = Hub::new(
            pipeline,
            Link::Local(LocalPaths {
                results_dir: config.results_dir,
            }),
            events_tx,
            leaving,
            bench,
            control,
            None,
        );
        let stop = shutdown.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    op = ops_rx.recv() => match op {
                        Some(op) => hub.handle(op),
                        None => break,
                    },
                }
            }
            hub.leave(Duration::from_secs(10)).await;
        });
        LocalLink {
            ops: ops_tx,
            events: events_rx,
            shutdown,
            task: Some(task),
        }
    }
}
