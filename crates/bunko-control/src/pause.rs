//! The pause state machine (GUI.md §3): `after_volume` / `now`, an optional `until`
//! that lifts the pause by itself, kept in `<storage>/.pause.json` across restarts.
//!
//! [`PauseCtl`] only holds and persists the state; whoever does the work (the
//! processor's session hub) subscribes and acts on each change. A pause whose `until`
//! has passed is dropped when read, so a machine switched off over the deadline comes
//! back unpaused.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, SubsecRound, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::types::{PAUSE_FILE, PauseMode, PauseView};

/// The wall clock is re-read at least this often while waiting for `until` (a laptop's
/// suspend stops tokio's monotonic clock, not the deadline).
const UNTIL_RECHECK: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseState {
    pub mode: PauseMode,
    #[serde(default, with = "opt_time")]
    pub until: Option<DateTime<Utc>>,
    /// `user` or `schedule`.
    pub reason: String,
    #[serde(with = "time_rfc3339")]
    pub since: DateTime<Utc>,
}

impl PauseState {
    pub fn view(&self) -> PauseView {
        PauseView {
            mode: Some(self.mode),
            until: self.until.map(rfc3339),
            reason: Some(self.reason.clone()),
            since: Some(rfc3339(self.since)),
        }
    }

    /// The protocol's `Availability` for this pause.
    pub fn availability(&self) -> bunko_proto::Availability {
        bunko_proto::Availability {
            paused: true,
            until: self.until.map(rfc3339),
            reason: Some(self.reason.clone()),
            install: None,
        }
    }

    fn expired(&self, now: DateTime<Utc>) -> bool {
        self.until.is_some_and(|u| u <= now)
    }
}

pub fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// RFC 3339 with any offset (or a trailing `Z`) to UTC.
pub fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

mod time_rfc3339 {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::rfc3339(*t))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let text = String::deserialize(d)?;
        super::parse_time(&text).ok_or_else(|| serde::de::Error::custom("not an RFC 3339 time"))
    }
}

mod opt_time {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => s.serialize_str(&super::rfc3339(*t)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        let text = Option::<String>::deserialize(d)?;
        match text {
            None => Ok(None),
            Some(t) => super::parse_time(&t)
                .map(Some)
                .ok_or_else(|| serde::de::Error::custom("not an RFC 3339 time")),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PauseError {
    #[error("until is not an RFC 3339 time: {0:?}")]
    BadUntil(String),
    #[error("until is in the past")]
    UntilPassed,
    #[error("reason must be user or schedule")]
    BadReason,
}

/// The live pause of one instance. Cheap to clone.
#[derive(Clone)]
pub struct PauseCtl(Arc<Inner>);

struct Inner {
    path: PathBuf,
    tx: watch::Sender<Option<PauseState>>,
    /// Serialises changes (state + file stay in step).
    write: Mutex<()>,
}

impl std::fmt::Debug for PauseCtl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PauseCtl")
            .field("path", &self.0.path)
            .field("state", &*self.0.tx.borrow())
            .finish()
    }
}

impl PauseCtl {
    /// The pause kept under `storage` (none, or an expired one, reads as not paused).
    /// Inside a tokio runtime the `until` timer starts at once; elsewhere call
    /// [`PauseCtl::spawn_timer`] from one.
    pub fn open(storage: &Path) -> PauseCtl {
        let path = storage.join(PAUSE_FILE);
        let state = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<PauseState>(&t).ok());
        let state = match state {
            Some(s) if s.expired(Utc::now()) => {
                let _ = std::fs::remove_file(&path);
                tracing::info!("The pause ended while this was not running");
                None
            }
            other => other,
        };
        let (tx, _) = watch::channel(state);
        let ctl = PauseCtl(Arc::new(Inner {
            path,
            tx,
            write: Mutex::new(()),
        }));
        if tokio::runtime::Handle::try_current().is_ok() {
            ctl.spawn_timer();
        }
        ctl
    }

    pub fn current(&self) -> Option<PauseState> {
        self.0.tx.borrow().clone()
    }

    pub fn is_paused(&self) -> bool {
        self.0.tx.borrow().is_some()
    }

    /// Every change, starting from the current state.
    pub fn subscribe(&self) -> watch::Receiver<Option<PauseState>> {
        self.0.tx.subscribe()
    }

    pub fn view(&self) -> PauseView {
        self.current().map(|s| s.view()).unwrap_or_default()
    }

    /// Pause (or change the running pause: `now` over `after_volume` takes effect, a
    /// new `until` replaces the old). `since` is kept from a pause already running.
    pub fn pause(
        &self,
        mode: PauseMode,
        until: Option<&str>,
        reason: Option<&str>,
    ) -> Result<PauseState, PauseError> {
        // Whole seconds: what `.pause.json` keeps.
        let now = Utc::now().trunc_subsecs(0);
        let until = match until.map(str::trim).filter(|u| !u.is_empty()) {
            None => None,
            Some(text) => {
                let t = parse_time(text)
                    .ok_or_else(|| PauseError::BadUntil(text.to_string()))?
                    .trunc_subsecs(0);
                if t <= now {
                    return Err(PauseError::UntilPassed);
                }
                Some(t)
            }
        };
        let reason = match reason.unwrap_or("user") {
            r @ ("user" | "schedule") => r.to_string(),
            _ => return Err(PauseError::BadReason),
        };
        let _w = self.0.write.lock();
        let since = self.current().map(|s| s.since).unwrap_or(now);
        // `now` stays `now` once taken: the volumes are already back in the queue.
        let mode = match (self.current().map(|s| s.mode), mode) {
            (Some(PauseMode::Now), _) => PauseMode::Now,
            (_, m) => m,
        };
        let state = PauseState {
            mode,
            until,
            reason,
            since,
        };
        self.persist(Some(&state));
        self.0.tx.send_replace(Some(state.clone()));
        tracing::info!(
            "Paused ({}){}",
            mode.as_str(),
            until
                .map(|u| format!(" until {}", rfc3339(u)))
                .unwrap_or_default()
        );
        Ok(state)
    }

    /// Lift the pause (nothing happens when not paused).
    pub fn resume(&self) {
        let _w = self.0.write.lock();
        if self.current().is_none() {
            return;
        }
        self.persist(None);
        self.0.tx.send_replace(None);
        tracing::info!("Resumed");
    }

    fn persist(&self, state: Option<&PauseState>) {
        let path = &self.0.path;
        let result = match state {
            None => match std::fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
            Some(state) => {
                let text = serde_json::to_string_pretty(state).unwrap_or_default();
                crate::file::write_atomic(path, text.as_bytes(), false)
            }
        };
        if let Err(e) = result {
            tracing::warn!(
                "could not save the pause in {}: {e} (it lasts until this stops)",
                path.display()
            );
        }
    }

    /// Lift the pause at its `until` (one task per [`PauseCtl`]; it ends with the
    /// last handle).
    pub fn spawn_timer(&self) {
        let weak = Arc::downgrade(&self.0);
        let mut rx = self.0.tx.subscribe();
        tokio::spawn(async move {
            loop {
                let until = rx.borrow_and_update().as_ref().and_then(|s| s.until);
                match until {
                    None => {
                        if rx.changed().await.is_err() {
                            return;
                        }
                    }
                    Some(until) => {
                        let left = (until - Utc::now()).to_std().unwrap_or(Duration::ZERO);
                        if left.is_zero() {
                            let Some(inner) = weak.upgrade() else { return };
                            let ctl = PauseCtl(inner);
                            if ctl.current().and_then(|s| s.until) == Some(until) {
                                tracing::info!("The pause reached its end ({})", rfc3339(until));
                                ctl.resume();
                            }
                            continue;
                        }
                        tokio::select! {
                            changed = rx.changed() => if changed.is_err() { return },
                            _ = tokio::time::sleep(left.min(UNTIL_RECHECK)) => {}
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pause_persists_and_resume_clears() {
        let dir = tempfile::tempdir().unwrap();
        let ctl = PauseCtl::open(dir.path());
        assert!(!ctl.is_paused());
        let s = ctl.pause(PauseMode::AfterVolume, None, None).unwrap();
        assert_eq!(s.reason, "user");
        assert!(dir.path().join(PAUSE_FILE).is_file());
        // A restart reads it back.
        let again = PauseCtl::open(dir.path());
        assert_eq!(again.current(), Some(s.clone()));
        // `now` over `after_volume` wins and keeps `since`; `after_volume` over `now`
        // stays `now`.
        let n = ctl.pause(PauseMode::Now, None, None).unwrap();
        assert_eq!((n.mode, n.since), (PauseMode::Now, s.since));
        let n2 = ctl.pause(PauseMode::AfterVolume, None, None).unwrap();
        assert_eq!(n2.mode, PauseMode::Now);
        ctl.resume();
        assert!(!dir.path().join(PAUSE_FILE).exists());
        assert!(!PauseCtl::open(dir.path()).is_paused());
    }

    #[tokio::test]
    async fn bad_input_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctl = PauseCtl::open(dir.path());
        assert!(matches!(
            ctl.pause(PauseMode::Now, Some("tomorrow"), None),
            Err(PauseError::BadUntil(_))
        ));
        assert_eq!(
            ctl.pause(PauseMode::Now, Some("2001-01-01T00:00:00Z"), None),
            Err(PauseError::UntilPassed)
        );
        assert_eq!(
            ctl.pause(PauseMode::Now, None, Some("admin")),
            Err(PauseError::BadReason)
        );
        assert!(!ctl.is_paused());
    }

    #[tokio::test]
    async fn until_lifts_the_pause() {
        let dir = tempfile::tempdir().unwrap();
        let ctl = PauseCtl::open(dir.path());
        let until = rfc3339(Utc::now() + chrono::Duration::seconds(2));
        let s = ctl
            .pause(PauseMode::AfterVolume, Some(&until), Some("schedule"))
            .unwrap();
        assert_eq!(s.view().until.as_deref(), Some(until.as_str()));
        let mut rx = ctl.subscribe();
        tokio::time::timeout(Duration::from_secs(5), async {
            while rx.borrow_and_update().is_some() {
                rx.changed().await.unwrap();
            }
        })
        .await
        .expect("the pause lifted itself");
        assert!(!dir.path().join(PAUSE_FILE).exists());
    }

    #[tokio::test]
    async fn an_expired_pause_is_dropped_at_start() {
        let dir = tempfile::tempdir().unwrap();
        let state = PauseState {
            mode: PauseMode::Now,
            until: Some(Utc::now() - chrono::Duration::seconds(5)),
            reason: "user".into(),
            since: Utc::now() - chrono::Duration::seconds(60),
        };
        std::fs::write(
            dir.path().join(PAUSE_FILE),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();
        assert!(!PauseCtl::open(dir.path()).is_paused());
        assert!(!dir.path().join(PAUSE_FILE).exists());
    }
}
