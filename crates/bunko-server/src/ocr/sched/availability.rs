//! A processor's own pause (GUI.md §3, protocol v3 `availability` / `released`).
//!
//! A paused machine contributes no lanes, its open sessions drain (finish what they
//! hold, then close), it is offered no session, volume or benchmark, and nothing that
//! ends on it while paused is anybody's failure. `released` claims go back to the
//! queue at once, unrecorded, and are offered again this scan. An admin sees the pause
//! (`/api/processors` → `pause`) but cannot lift it.

use bunko_proto::Availability;
use serde_json::{Value, json};

use super::Scheduler;
use crate::ocr::types::Job;

impl Scheduler {
    /// The machine `pid` paused itself.
    pub fn machine_paused(&self, pid: &str) -> bool {
        self.machines.get(pid).is_some_and(|m| m.pause.is_some())
    }

    /// The machine named `name` (its hardware key) paused itself.
    pub fn name_paused(&self, name: &str) -> bool {
        self.machines
            .values()
            .any(|m| m.name == name && m.pause.is_some())
    }

    /// An `availability` event.
    pub fn set_availability(&mut self, pid: &str, availability: Availability) {
        let Some(m) = self.machines.get_mut(pid) else {
            return;
        };
        let was = m.pause.is_some();
        let before_installing = m.pause.as_ref().is_some_and(|a| a.is_installing());
        let label = m.label();
        let now_paused = availability.paused;
        // Only the install's progress moved: nothing to rebuild, nothing to log.
        let same = |a: &Availability, b: &Availability| {
            a.paused == b.paused
                && a.reason == b.reason
                && a.until.as_ref().filter(|u| !u.is_empty())
                    == b.until.as_ref().filter(|u| !u.is_empty())
        };
        if now_paused
            && let Some(before) = m.pause.as_mut()
            && same(before, &availability)
        {
            if before.install != availability.install {
                before.install = availability.install;
                self.bump();
            }
            return;
        }
        m.pause = now_paused.then(|| {
            let mut a = availability.clone();
            a.until = a.until.filter(|u| !u.is_empty());
            a
        });
        if now_paused {
            let until = m
                .pause
                .as_ref()
                .and_then(|a| a.until.clone())
                .map(|u| format!(" until {u}"))
                .unwrap_or_default();
            let installing = m.pause.as_ref().is_some_and(|a| a.is_installing());
            if installing {
                if !was {
                    self.log(format!(
                        "{label} is installing its OCR backend; it takes no work until that is done"
                    ));
                }
            } else if !was || before_installing {
                self.log(format!(
                    "{label} paused itself{until}; it takes no new work (what it runs finishes)"
                ));
            }
        } else if was {
            self.log(if before_installing {
                format!("{label} is available (its OCR backend install ended)")
            } else {
                format!("{label} resumed")
            });
        }
        self.rebuild_lanes();
        for lane in &mut self.lanes {
            lane.idle_at = None;
        }
        self.bump();
        self.bump_page();
        if !now_paused {
            self.maybe_start_scan();
        }
    }

    /// A `released` event: claims a pausing processor gives back. Each goes back to
    /// the queue now, nothing recorded; a session left with nothing drains and closes.
    pub fn released(&mut self, pid: &str, claims: &[String]) {
        let label = self
            .machines
            .get(pid)
            .map(|m| m.label())
            .unwrap_or_default();
        let mut back = 0;
        for claim in claims {
            let sid = self
                .sessions
                .iter()
                .find(|(_, s)| s.pid == pid && s.jobs.contains_key(claim))
                .map(|(sid, _)| sid.clone());
            let Some(sid) = sid else { continue };
            let Some(entry) = self.pop_job(&sid, claim) else {
                continue;
            };
            self.drop_result(&sid, claim);
            self.release_job(&entry.job, &format!("{label} paused"));
            back += 1;
        }
        if back > 0 {
            self.bump();
            self.bump_page();
        }
    }

    fn release_job(&mut self, job: &Job, reason: &str) {
        self.release(job, reason, true);
    }

    /// `pause` for the admin's processor list: `{until, reason}` or null.
    pub fn pause_value(&self, pid: &str) -> Value {
        match self.machines.get(pid).and_then(|m| m.pause.as_ref()) {
            Some(a) => pause_json(a),
            None => Value::Null,
        }
    }
}

/// A pause for the admin's processor list: `{paused, until, reason}`, plus `install`
/// (how far its OCR backend install is) while it installs.
pub fn pause_json(a: &Availability) -> Value {
    let mut v = json!({"paused": true, "until": a.until, "reason": a.reason});
    if let Some(i) = &a.install {
        v["install"] = serde_json::to_value(i).unwrap_or(Value::Null);
    }
    v
}

/// The `availability` a registration body carries (lenient: anything malformed reads
/// as not paused).
pub fn availability_of(body: &serde_json::Map<String, Value>) -> Option<Availability> {
    let value = body.get("availability")?;
    serde_json::from_value::<Availability>(value.clone())
        .ok()
        .filter(|a| a.paused)
}
