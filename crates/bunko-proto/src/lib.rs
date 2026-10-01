//! The processor protocol, version 3 (mokuro-bunko 0.7).
//!
//! One vocabulary serves both the in-process local processor (over channels) and remote
//! processors (over HTTP). See `docs/rust-port/PROTOCOL.md` for the transport.
//!
//! Remote transport summary:
//! * `POST /_processor/register` — [`RegisterRequest`] → [`RegisterReply`]; a protocol
//!   mismatch answers 400 with [`ProtocolMismatch`] (the setup wizard probes with
//!   `protocol: 0`).
//! * `GET /_processor/{pid}/socket` — a WebSocket. Library → processor text frames are
//!   [`Op`]s, processor → library text frames are [`Event`]s, one JSON object per frame.
//! * `PUT /_processor/{pid}/results/{sid}/{claim}` — the finished sidecar's bytes, sent
//!   before the claim's `volume_done` (which carries the same sha256).
//! * Archives are fetched with ordinary authenticated `GET`s of the library's file URLs
//!   (`Range` / `If-Range` against a strong `ETag`).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const PROTOCOL_VERSION: u32 = 3;
pub const PROCESSOR_ROOT: &str = "/_processor";
pub const ARCHIVES_ROOT: &str = "/mokuro-reader/";

/// The library pings the socket this often; the processor must answer (WebSocket pong
/// or any event).
pub const HEARTBEAT_SECONDS: u64 = 15;
/// No frame at all from a processor for this long means it is gone (not busy).
pub const SILENCE_SECONDS: u64 = 30;
/// Re-check the processor's account (disabled / re-roled / new password) this often.
pub const ACCOUNT_RECHECK_SECONDS: u64 = 15;
/// Claims a session may hold at once (scheduler lookahead).
pub const MAX_OUTSTANDING_VOLUMES: usize = 2;
pub const MAX_SESSIONS_PER_PROCESSOR: u32 = 16;
pub const MAX_ENTRIES_PER_ACCOUNT: usize = 4;
pub const MAX_PROCESSOR_NAME: usize = 64;
pub const MAX_REGISTER_BODY_BYTES: usize = 256 * 1024;
pub const MAX_IDENTITY_BYTES: usize = 16 * 1024;
/// Largest sidecar a processor may upload.
pub const MAX_RESULT_BYTES: u64 = 256 * 1024 * 1024;
/// A runner silent this long is wedged.
pub const SESSION_WEDGE_SECONDS: u64 = 600;

/// Header carrying the lowercase hex sha256 of an uploaded result body.
pub const HEADER_RESULT_SHA256: &str = "x-mokuro-sha256";
/// Header carrying the sidecar file name of an uploaded result.
pub const HEADER_RESULT_NAME: &str = "x-mokuro-sidecar-name";

/// Ids that become file names: `[A-Za-z0-9_-]{1,64}`.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

// --- registration ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
    #[serde(default)]
    pub cpu: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<String>,
    /// Execution provider in use: `cpu`, `cuda`, `webgpu`, `directml`, `coreml`, ...
    #[serde(default)]
    pub backend: String,
    /// mokuro-bunko version of the processor.
    #[serde(default)]
    pub version: String,
    /// Build identity (version + git sha + onnxruntime version), recorded in provenance.
    #[serde(default)]
    pub runner_build: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cores: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// `cpu` or `gpu:<n>`.
    pub id: String,
    pub label: String,
    /// Precisions this device runs: subset of `fp32`, `fp16`, `bf16`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<String>,
    /// The execution provider behind a GPU id (`cuda`, `webgpu`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// What a processor can run. Empty `engines` means "still installing" (models
/// downloading) and matches no row.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub engines: Vec<String>,
    #[serde(default)]
    pub detectors: Vec<String>,
    #[serde(default)]
    pub devices: Vec<Device>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub protocol: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_name: Option<String>,
    #[serde(default)]
    pub host: HostInfo,
    #[serde(default)]
    pub catalog: Catalog,
    #[serde(default = "one")]
    pub max_sessions: u32,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisterReply {
    pub protocol: u32,
    pub processor_id: String,
    /// WebSocket path, e.g. `/_processor/<pid>/socket`.
    pub socket: String,
    /// Upload path template with literal `{sid}` and `{claim}` placeholders.
    pub results: String,
    pub archives: String,
    /// The library's version, for the processor's log and update hints.
    pub version: String,
}

/// 400 body when `protocol` does not match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtocolMismatch {
    pub error: String,
    pub protocols: Vec<u32>,
    pub version: String,
}

// --- the recipe a session runs -----------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolsSpec {
    #[serde(default)]
    pub stage_workers: BTreeMap<String, u32>,
    #[serde(default)]
    pub queue_capacity: BTreeMap<String, u32>,
    #[serde(default)]
    pub stage_device: BTreeMap<String, String>,
}

/// A generation row as one machine should run it (pools and precision pick resolved
/// for that machine). Mirrors `bunko_core::generations::Generation::to_value`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowSpec {
    pub id: String,
    pub name: String,
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detector: Option<String>,
    #[serde(default = "default_patch_budget")]
    pub patch_budget: u32,
    #[serde(default = "default_precision")]
    pub precision: String,
    #[serde(default)]
    pub pools: PoolsSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precision_pick: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub precision_why: String,
    #[serde(default)]
    pub primary: bool,
}

fn default_patch_budget() -> u32 {
    512
}
fn default_precision() -> String {
    "auto-accuracy".to_string()
}

// --- ops: library → processor ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeOp {
    pub sid: String,
    pub claim: String,
    /// URL path of the archive (archives root + library-relative posix path, not
    /// percent-encoded), or a local filesystem path for the in-process processor.
    pub archive: String,
    /// The file name the library expects the result under.
    pub sidecar_name: String,
    pub title: String,
    pub volume_title: String,
    #[serde(default)]
    pub title_uuid: Option<String>,
    #[serde(default)]
    pub volume_uuid: Option<String>,
    /// Library's `st_size` at claim time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Library's strong ETag at claim time (for `If-Range`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    OpenSession {
        sid: String,
        generation: RowSpec,
    },
    Volume(VolumeOp),
    /// Cancel a claim (nothing is recorded for it) or, with no claim, the whole session.
    Cancel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claim: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bid: Option<String>,
    },
    /// Finish what was accepted, abandon downloads, then end the session (`exit`).
    CloseSession {
        sid: String,
    },
    Bench(BenchOp),
    Heartbeat,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchOp {
    pub bid: String,
    pub spec: RowSpec,
    /// URL path of the packed sample archive.
    pub sample: String,
    pub pages: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub precision_only: bool,
}

// --- events: processor → library ---------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Models loaded; the session accepts volumes.
    Ready {
        sid: String,
        startup_seconds: f64,
        #[serde(default)]
        weights: BTreeMap<String, String>,
        #[serde(default)]
        stage_workers: BTreeMap<String, u32>,
        #[serde(default)]
        queue_capacity: BTreeMap<String, u32>,
        #[serde(default)]
        stage_device: BTreeMap<String, String>,
        #[serde(default)]
        pipeline: String,
        /// The precision actually used.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        precision: Option<String>,
    },
    Fatal {
        sid: String,
        error: String,
    },
    SpawnFailed {
        sid: String,
        error: String,
    },
    /// Always the session's last event, exactly once.
    Exit {
        sid: String,
        #[serde(default)]
        returncode: Option<i32>,
    },
    VolumeStarted {
        sid: String,
        id: String,
        pages: u32,
    },
    Page {
        sid: String,
        id: String,
        done: u32,
        total: u32,
    },
    Stats {
        sid: String,
        pipeline: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cpu_pressure: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        other_cpu: Option<f64>,
    },
    VolumeDone {
        sid: String,
        id: String,
        pages: u32,
        #[serde(default)]
        failed_pages: u32,
        seconds: f64,
        #[serde(default)]
        stats: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cpu_pressure: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        other_cpu: Option<f64>,
        /// sha256 of the uploaded sidecar (remote) — must match the result upload.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sidecar_sha256: Option<String>,
    },
    VolumeFailed {
        sid: String,
        id: String,
        error: String,
    },
    /// Download progress (`downloading` / `retrying` / `restarting`) or `ready` once the
    /// verified archive is handed to the pipeline.
    Fetch {
        sid: String,
        id: String,
        state: String,
        #[serde(flatten)]
        detail: BTreeMap<String, Value>,
    },
    /// A claim that never reached the pipeline; never a failure of the volume.
    VolumeReturned {
        sid: String,
        id: String,
        class: String,
        error: String,
        #[serde(flatten)]
        detail: BTreeMap<String, Value>,
    },
    BenchReady {
        bid: String,
        #[serde(flatten)]
        detail: BTreeMap<String, Value>,
    },
    BenchProgress {
        bid: String,
        #[serde(flatten)]
        detail: BTreeMap<String, Value>,
    },
    BenchTrial {
        bid: String,
        #[serde(flatten)]
        detail: BTreeMap<String, Value>,
    },
    BenchDone {
        bid: String,
        #[serde(flatten)]
        detail: BTreeMap<String, Value>,
    },
    /// Catalog changed (models finished downloading, device lost).
    Catalog {
        catalog: Catalog,
    },
    Ping,
}

impl Event {
    pub fn sid(&self) -> Option<&str> {
        match self {
            Event::Ready { sid, .. }
            | Event::Fatal { sid, .. }
            | Event::SpawnFailed { sid, .. }
            | Event::Exit { sid, .. }
            | Event::VolumeStarted { sid, .. }
            | Event::Page { sid, .. }
            | Event::Stats { sid, .. }
            | Event::VolumeDone { sid, .. }
            | Event::VolumeFailed { sid, .. }
            | Event::Fetch { sid, .. }
            | Event::VolumeReturned { sid, .. } => Some(sid),
            _ => None,
        }
    }

    /// The claim a per-volume event is about.
    pub fn claim(&self) -> Option<&str> {
        match self {
            Event::VolumeStarted { id, .. }
            | Event::Page { id, .. }
            | Event::VolumeDone { id, .. }
            | Event::VolumeFailed { id, .. }
            | Event::Fetch { id, .. }
            | Event::VolumeReturned { id, .. } => Some(id),
            _ => None,
        }
    }
}

/// Classes of a returned claim (`volume_returned.class`).
pub mod return_class {
    pub const STALLED: &str = "stalled";
    pub const DIFFERS: &str = "differs";
    pub const CHANGED: &str = "changed";
    pub const NO_RANGE: &str = "no_range";
    pub const MISMATCH: &str = "mismatch";
    pub const MISSING: &str = "missing";
    pub const REJECTED: &str = "rejected";
    pub const NO_ROOM: &str = "no_room";
    pub const LOCAL: &str = "local";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_shapes() {
        let op = Op::Volume(VolumeOp {
            sid: "s1".into(),
            claim: "v1".into(),
            archive: "/mokuro-reader/A/B.cbz".into(),
            sidecar_name: "B.mokuro".into(),
            title: "A".into(),
            volume_title: "B".into(),
            title_uuid: None,
            volume_uuid: Some("u".into()),
            size: Some(3),
            etag: None,
        });
        let text = serde_json::to_string(&op).unwrap();
        assert!(text.starts_with(r#"{"op":"volume","sid":"s1""#), "{text}");
        let back: Op = serde_json::from_str(&text).unwrap();
        assert_eq!(back, op);
        let hb: Op = serde_json::from_str(r#"{"op":"heartbeat"}"#).unwrap();
        assert_eq!(hb, Op::Heartbeat);
    }

    #[test]
    fn event_shapes() {
        let e: Event = serde_json::from_str(
            r#"{"event":"fetch","sid":"s","id":"v1","state":"ready","bytes":10,"crc32":"abcd1234"}"#,
        )
        .unwrap();
        assert_eq!(e.claim(), Some("v1"));
        match &e {
            Event::Fetch { detail, state, .. } => {
                assert_eq!(state, "ready");
                assert_eq!(detail["bytes"], 10);
            }
            _ => panic!(),
        }
        let p: Event = serde_json::from_str(r#"{"event":"ping"}"#).unwrap();
        assert_eq!(p, Event::Ping);
        assert!(valid_id("v-12_a"));
        assert!(!valid_id("../x"));
    }
}
