//! The mokuro-bunko OCR processor runtime (full build only).
//!
//! One runtime serves two links (ARCHITECTURE §6, `docs/rust-port/PROTOCOL.md`):
//!
//! * **remote** — [`remote::serve`] logs in to a library server, registers, holds one
//!   WebSocket for every op and event, fetches archives over HTTP
//!   ([`fetch::ArchiveFetcher`]: `Range`/`If-Range` resume, CRC verification, the
//!   `volume_returned` classes) and uploads each sidecar with a `PUT` before its
//!   `volume_done`;
//! * **local** — [`local::LocalProcessor`] runs inside the library server over tokio
//!   channels, reading archives from disk.
//!
//! The OCR engines plug in through [`pipeline::PagePipeline`] /
//! [`pipeline::VolumeRunner`]; [`fake::FakePipeline`] stands in for them in tests.
//! [`bench`] answers the library's `bench` op on either link (the measurement runs
//! through the same [`pipeline::PagePipeline`] sessions use).
//! [`config`], [`setup`], [`service`] and [`status`] implement `processor.yaml` and
//! the `processor setup|service|status` commands (the binary wires clap).

pub mod bench;
pub mod client;
pub mod config;
pub mod fake;
pub mod fetch;
pub mod hostload;
pub mod local;
pub mod lock;
pub mod pipeline;
pub mod remote;
pub mod service;
mod session;
pub mod setup;
pub mod spool;
pub mod status;
pub mod tls;
pub mod utilization;
pub mod verify;

pub use bench::BenchConfig;
pub use config::{ProcessorConfig, TlsVerify, load_processor_config};
pub use fake::{FakeConfig, FakePipeline};
pub use fetch::{ArchiveFetcher, FetchTiming};
pub use local::{LocalConfig, LocalLink, LocalProcessor};
pub use pipeline::{
    CancelToken, MachineInfo, PagePipeline, PageProgress, PipelineReport, QueueReport, ReadyInfo,
    RunError, StageReport, VolumeMeta, VolumeOutcome, VolumeRunner,
};
pub use remote::{ServeError, ServeOptions, serve};
pub use session::SIDECAR_NOT_SENT;
