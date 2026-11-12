//! The OCR orchestrator (docs/rust-port/OCR-WORKER.md): one scheduler for every machine
//! that reads OCR — this server's in-process processor (full build) and remote
//! processors (protocol v3) — plus the queue page, the reader's queue file, per-volume
//! outlooks, generation upgrades and benchmarks.
//!
//! * [`control`] — [`OcrControl`], the public handle, its [`OcrDeps`] and the wiring.
//! * [`sched`] — the scheduler: all state, one message at a time ([`actor`] runs it).
//! * [`owed`] — what the library is owed; [`collect`] — installing a result;
//!   [`upgrade`] — generation upgrades; [`profiles`] — `processors/*.json`;
//!   [`busy`] — the volumes OCR work touches now (the case merge waits for them);
//!   [`sidecar_facts`] — what each sidecar says, read once per version of the file;
//!   [`orphans`] — sweeping the `<stem>_<n>.mokuro` files older collectors wrote;
//!   [`provenance`] — keeping a sealed block through an older reader's rewrite.
//! * [`api`] — `/_processor/*` (protocol v3); [`queue_api`] + [`shape`] — `/queue`;
//!   [`queue_file`] — `/mokuro-reader/.mokuro-queue.json` (and, through it, each
//!   series' queue file v2, [`series_queue`]); [`admin`] — `OcrAdmin`.

pub mod actor;
pub mod admin;
pub mod api;
pub mod busy;
pub mod collect;
pub mod control;
pub mod orphans;
pub mod owed;
pub mod profiles;
pub mod provenance;
pub mod pyjson;
pub mod queue_api;
pub mod queue_file;
pub mod sched;
pub mod series_queue;
pub mod shape;
pub mod sidecar_facts;
pub mod types;
pub mod upgrade;

pub use control::*;
