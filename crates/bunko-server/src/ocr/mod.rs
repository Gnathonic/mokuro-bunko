//! The OCR orchestrator (docs/rust-port/OCR-WORKER.md): one scheduler for every machine
//! that reads OCR — this server's in-process processor (full build) and remote
//! processors (protocol v3) — plus the queue page, the reader's queue file, per-volume
//! outlooks, generation upgrades and benchmarks.
//!
//! * [`control`] — [`OcrControl`], the public handle, its [`OcrDeps`] and the wiring.
//! * [`sched`] — the scheduler: all state, one message at a time ([`actor`] runs it).
//! * [`owed`] — what the library is owed; [`collect`] — installing a result;
//!   [`upgrade`] — generation upgrades; [`profiles`] — `processors/*.json`.
//! * [`api`] — `/_processor/*` (protocol v3); [`queue_api`] + [`shape`] — `/queue`;
//!   [`queue_file`] — `/mokuro-reader/.mokuro-queue.json`; [`admin`] — `OcrAdmin`.

pub mod actor;
pub mod admin;
pub mod api;
pub mod collect;
pub mod control;
pub mod owed;
pub mod profiles;
pub mod pyjson;
pub mod queue_api;
pub mod queue_file;
pub mod sched;
pub mod shape;
pub mod types;
pub mod upgrade;

pub use control::*;
