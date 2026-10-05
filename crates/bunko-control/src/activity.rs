//! What this machine is reading now, and how much it has read (status `current` and
//! `stats`), learned from the very ops and events its processor exchanges with the
//! library — the same for a remote processor and a server's local OCR.
//!
//! Today/total counts are kept in `<storage>/.stats.json` so they survive restarts;
//! "today" is the local calendar day.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bunko_proto::{Device, Event, Op};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::types::{CurrentVolume, DayStats, STATS_FILE, TotalStats};

/// The window `rate_pages_per_minute` counts pages over.
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// "Something in the status changed": a counter the SSE stream and the tray wait on.
#[derive(Clone, Debug)]
pub struct Changes(Arc<watch::Sender<u64>>);

impl Default for Changes {
    fn default() -> Self {
        Changes(Arc::new(watch::channel(0).0))
    }
}

impl Changes {
    pub fn bump(&self) {
        self.0.send_modify(|n| *n = n.wrapping_add(1));
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.subscribe()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct StatsFile {
    /// The local day `today` counts (`YYYY-MM-DD`).
    #[serde(default)]
    day: String,
    #[serde(default)]
    today: DayStats,
    #[serde(default)]
    total: TotalStats,
    /// Fractional busy seconds not yet counted in `today.busy_seconds`.
    #[serde(default)]
    busy_carry: f64,
}

fn local_day() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

struct SessionInfo {
    engine: Option<String>,
    precision: Option<String>,
    device: Option<String>,
}

struct Claim {
    sid: String,
    volume: String,
    started: Option<Instant>,
    done: u32,
    total: u32,
}

struct Act {
    sessions: HashMap<String, SessionInfo>,
    /// Claims held, in arrival order (claim ids are unique per library).
    claims: Vec<(String, Claim)>,
    /// `(when, pages)` increments within [`RATE_WINDOW`].
    pages: VecDeque<(Instant, u32)>,
    devices: BTreeMap<String, String>,
    stats: StatsFile,
}

/// The activity of one machine.
pub struct Activity {
    path: PathBuf,
    inner: Mutex<Act>,
    changes: Changes,
}

impl std::fmt::Debug for Activity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Activity")
            .field("path", &self.path)
            .finish()
    }
}

impl Activity {
    /// Counts kept under `storage`; `changes` is bumped on every visible change.
    pub fn new(storage: &Path, changes: Changes) -> Arc<Activity> {
        let path = storage.join(STATS_FILE);
        let stats = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<StatsFile>(&t).ok())
            .unwrap_or_default();
        Arc::new(Activity {
            path,
            inner: Mutex::new(Act {
                sessions: HashMap::new(),
                claims: Vec::new(),
                pages: VecDeque::new(),
                devices: BTreeMap::new(),
                stats,
            }),
            changes,
        })
    }

    /// The machine's devices (`gpu:0` → `RTX 4090`), for `current[].device`.
    pub fn set_devices(&self, devices: &[Device]) {
        let mut a = self.inner.lock();
        a.devices = devices
            .iter()
            .map(|d| (d.id.clone(), d.label.clone()))
            .collect();
    }

    /// An op the processor received (and accepted).
    pub fn op(&self, op: &Op) {
        let mut a = self.inner.lock();
        match op {
            Op::OpenSession { sid, generation } => {
                a.sessions.insert(
                    sid.clone(),
                    SessionInfo {
                        engine: Some(generation.engine.clone()),
                        precision: None,
                        device: None,
                    },
                );
            }
            Op::Volume(v) => {
                let volume = if v.title.is_empty() || v.title == "." {
                    v.volume_title.clone()
                } else {
                    format!("{}/{}", v.title, v.volume_title)
                };
                a.claims.push((
                    v.claim.clone(),
                    Claim {
                        sid: v.sid.clone(),
                        volume,
                        started: None,
                        done: 0,
                        total: 0,
                    },
                ));
            }
            _ => return,
        }
        drop(a);
        self.changes.bump();
    }

    /// An event the processor sent.
    pub fn event(&self, event: &Event) {
        let now = Instant::now();
        let mut a = self.inner.lock();
        let changed = match event {
            Event::Ready {
                sid,
                precision,
                stage_device,
                ..
            } => {
                let device = pick_device(stage_device, &a.devices);
                if let Some(s) = a.sessions.get_mut(sid) {
                    s.precision = precision.clone();
                    s.device = device;
                }
                true
            }
            Event::VolumeStarted { id, pages, .. } => {
                if let Some(c) = claim_mut(&mut a.claims, id) {
                    c.started.get_or_insert(now);
                    if *pages > 0 {
                        c.total = *pages;
                    }
                }
                true
            }
            Event::Page {
                id, done, total, ..
            } => {
                let mut delta = 0;
                if let Some(c) = claim_mut(&mut a.claims, id) {
                    c.started.get_or_insert(now);
                    delta = done.saturating_sub(c.done);
                    c.done = (*done).max(c.done);
                    if *total > 0 {
                        c.total = *total;
                    }
                }
                if delta > 0 {
                    a.pages.push_back((now, delta));
                }
                true
            }
            Event::VolumeDone {
                id, pages, seconds, ..
            } => {
                remove_claim(&mut a.claims, id);
                let day = local_day();
                let s = &mut a.stats;
                if s.day != day {
                    s.day = day;
                    s.today = DayStats::default();
                    s.busy_carry = 0.0;
                }
                s.today.volumes += 1;
                s.today.pages += u64::from(*pages);
                s.busy_carry += seconds.max(0.0);
                let whole = s.busy_carry.floor();
                s.today.busy_seconds += whole as u64;
                s.busy_carry -= whole;
                s.total.volumes += 1;
                s.total.pages += u64::from(*pages);
                let text = serde_json::to_string_pretty(&a.stats).unwrap_or_default();
                if let Err(e) = crate::file::write_atomic(&self.path, text.as_bytes(), false) {
                    tracing::debug!("could not save {}: {e}", self.path.display());
                }
                true
            }
            Event::VolumeFailed { id, .. } | Event::VolumeReturned { id, .. } => {
                remove_claim(&mut a.claims, id);
                true
            }
            Event::Released { claims } => {
                for id in claims {
                    remove_claim(&mut a.claims, id);
                }
                true
            }
            Event::Exit { sid, .. } => {
                a.sessions.remove(sid);
                a.claims.retain(|(_, c)| &c.sid != sid);
                true
            }
            _ => false,
        };
        drop(a);
        if changed {
            self.changes.bump();
        }
    }

    /// The link to the library ended: whatever was in flight is gone with it.
    pub fn clear(&self) {
        let mut a = self.inner.lock();
        a.sessions.clear();
        a.claims.clear();
        drop(a);
        self.changes.bump();
    }

    /// Volumes being read now (started, not finished), oldest first.
    pub fn current(&self) -> Vec<CurrentVolume> {
        let a = self.inner.lock();
        let now = Instant::now();
        a.claims
            .iter()
            .filter_map(|(_, c)| {
                let started = c.started?;
                let session = a.sessions.get(&c.sid);
                let elapsed = now.duration_since(started).as_secs_f64();
                let rate = (c.done > 0 && elapsed >= 1.0).then(|| c.done as f64 / elapsed);
                let eta = rate
                    .filter(|r| *r > 0.0)
                    .map(|r| (f64::from(c.total.saturating_sub(c.done)) / r).round() as u64);
                Some(CurrentVolume {
                    volume: c.volume.clone(),
                    engine: session.and_then(|s| s.engine.clone()),
                    precision: session.and_then(|s| s.precision.clone()),
                    device: session.and_then(|s| s.device.clone()),
                    pages_done: c.done,
                    pages_total: c.total,
                    pages_per_second: rate.map(|r| (r * 100.0).round() / 100.0),
                    eta_seconds: eta,
                })
            })
            .collect()
    }

    /// Claims held (started or not).
    pub fn held(&self) -> usize {
        self.inner.lock().claims.len()
    }

    /// Sessions open.
    pub fn sessions(&self) -> usize {
        self.inner.lock().sessions.len()
    }

    /// `(today, total)`; today reads as zeros once the day has turned.
    pub fn counts(&self) -> (DayStats, TotalStats) {
        let a = self.inner.lock();
        let today = if a.stats.day == local_day() {
            a.stats.today.clone()
        } else {
            DayStats::default()
        };
        (today, a.stats.total.clone())
    }

    /// Pages read over the last minute.
    pub fn pages_last_minute(&self) -> f64 {
        let mut a = self.inner.lock();
        let now = Instant::now();
        while a
            .pages
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > RATE_WINDOW)
        {
            a.pages.pop_front();
        }
        // fold from +0.0: an empty `sum` is -0.0, which serializes as `-0.0`.
        a.pages.iter().fold(0.0, |t, (_, n)| t + f64::from(*n))
    }
}

fn claim_mut<'a>(claims: &'a mut [(String, Claim)], id: &str) -> Option<&'a mut Claim> {
    claims.iter_mut().find(|(c, _)| c == id).map(|(_, c)| c)
}

fn remove_claim(claims: &mut Vec<(String, Claim)>, id: &str) {
    claims.retain(|(c, _)| c != id);
}

/// The device a session's heavy stage runs on: the first GPU among its stages, else
/// the CPU; labelled from the catalog (`gpu:0 RTX 4090`).
fn pick_device(
    stage_device: &BTreeMap<String, String>,
    labels: &BTreeMap<String, String>,
) -> Option<String> {
    let id = stage_device
        .values()
        .find(|d| d.starts_with("gpu"))
        .or_else(|| stage_device.values().next())?;
    Some(match labels.get(id) {
        Some(label) if !label.is_empty() && label != id => format!("{id} {label}"),
        _ => id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bunko_proto::{RowSpec, VolumeOp};

    fn row() -> RowSpec {
        serde_json::from_value(serde_json::json!({
            "id": "g", "name": "G", "engine": "hayai-nova"
        }))
        .unwrap()
    }

    fn vol(claim: &str) -> Op {
        Op::Volume(VolumeOp {
            sid: "s".into(),
            claim: claim.into(),
            archive: "/x.cbz".into(),
            sidecar_name: "x.mokuro".into(),
            title: "Dr Stone".into(),
            volume_title: format!("Vol {claim}"),
            title_uuid: None,
            volume_uuid: None,
            size: None,
            etag: None,
        })
    }

    #[test]
    fn follows_a_session_and_counts() {
        let dir = tempfile::tempdir().unwrap();
        let changes = Changes::default();
        let rx = changes.subscribe();
        let a = Activity::new(dir.path(), changes);
        a.set_devices(&[Device {
            id: "gpu:0".into(),
            label: "RX 9070 XT".into(),
            ..Default::default()
        }]);
        a.op(&Op::OpenSession {
            sid: "s".into(),
            generation: row(),
        });
        a.op(&vol("v1"));
        a.op(&vol("v2"));
        assert!(rx.has_changed().unwrap());
        a.event(&Event::Ready {
            sid: "s".into(),
            startup_seconds: 1.0,
            weights: BTreeMap::new(),
            stage_workers: BTreeMap::new(),
            queue_capacity: BTreeMap::new(),
            stage_device: [
                ("det".to_string(), "cpu".to_string()),
                ("engine".to_string(), "gpu:0".to_string()),
            ]
            .into_iter()
            .collect(),
            pipeline: String::new(),
            precision: Some("bf16".into()),
        });
        assert!(a.current().is_empty(), "nothing started yet");
        assert_eq!(a.held(), 2);
        a.event(&Event::VolumeStarted {
            sid: "s".into(),
            id: "v1".into(),
            pages: 10,
        });
        for done in 1..=4 {
            a.event(&Event::Page {
                sid: "s".into(),
                id: "v1".into(),
                done,
                total: 10,
            });
        }
        let cur = a.current();
        assert_eq!(cur.len(), 1);
        assert_eq!(cur[0].volume, "Dr Stone/Vol v1");
        assert_eq!(cur[0].engine.as_deref(), Some("hayai-nova"));
        assert_eq!(cur[0].precision.as_deref(), Some("bf16"));
        assert_eq!(cur[0].device.as_deref(), Some("gpu:0 RX 9070 XT"));
        assert_eq!((cur[0].pages_done, cur[0].pages_total), (4, 10));
        assert_eq!(a.pages_last_minute(), 4.0);
        a.event(&Event::VolumeDone {
            sid: "s".into(),
            id: "v1".into(),
            pages: 10,
            failed_pages: 0,
            seconds: 12.5,
            stats: serde_json::Value::Null,
            cpu_pressure: None,
            other_cpu: None,
            sidecar_sha256: None,
        });
        a.event(&Event::Released {
            claims: vec!["v2".into()],
        });
        assert_eq!(a.held(), 0);
        let (today, total) = a.counts();
        assert_eq!(
            (today.volumes, today.pages, today.busy_seconds),
            (1, 10, 12)
        );
        assert_eq!((total.volumes, total.pages), (1, 10));
        // Kept across restarts.
        let b = Activity::new(dir.path(), Changes::default());
        assert_eq!(b.counts().1.pages, 10);
        a.event(&Event::Exit {
            sid: "s".into(),
            returncode: Some(0),
        });
        assert_eq!(a.sessions(), 0);
    }
}
