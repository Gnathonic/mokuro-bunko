//! Counting state machines of the remote-processor path (the worker's
//! `_DownloadBreaker`, `_note_download_return`, `_download_delivered`,
//! `_Returns` / `_note_job_return`, `_StartBackoff` / `_note_start_failure`
//! / `_backed_off_rows`), as plain structs the server keeps under its lock.

use crate::failures::retry_delay_seconds;
use crate::py::truncate_chars;

/// Returned claims in a row that open a processor's breaker.
pub const DOWNLOAD_BREAKER_RETURNS: u32 = 3;
/// The first hold, doubling per re-open…
pub const DOWNLOAD_BREAKER_HOLD: f64 = 600.0;
/// …up to this.
pub const DOWNLOAD_BREAKER_MAX_HOLD: f64 = 3600.0;
/// Counted returns of one job that record it as "download failed".
pub const DOWNLOAD_RETURN_LIMIT: u32 = 3;
/// The class of a return that is about the file, never the processor.
pub const RETURN_CLASS_CHANGED: &str = "changed";
/// The class of a return that never counts against the job.
pub const RETURN_CLASS_NO_ROOM: &str = "no_room";

/// One processor registration's run of archive downloads it gave back.
#[derive(Clone, Debug, PartialEq)]
pub struct DownloadBreaker {
    pub label: String,
    pub consecutive: u32,
    /// Whether this registration has delivered an archive at all.
    pub proven: bool,
    pub hold: f64,
    pub open_until: f64,
    pub last_error: String,
}

/// The breaker just opened (log `"Holding <label> for N min: …"`).
#[derive(Clone, Debug, PartialEq)]
pub struct BreakerOpened {
    pub hold: f64,
    pub until: f64,
    pub last_error: String,
    pub consecutive: u32,
}

impl BreakerOpened {
    /// `"Holding <label> for N min: k archive downloads in a row failed (last: …)"`.
    pub fn log_line(&self, label: &str) -> String {
        format!(
            "Holding {label} for {} min: {} archive downloads in a row failed (last: {})",
            crate::py::fmt_fixed(self.hold / 60.0, 0),
            self.consecutive,
            self.last_error
        )
    }
}

/// What one return did to a breaker.
#[derive(Clone, Debug, PartialEq)]
pub struct ReturnNoted {
    /// Whether the JOB counts too (proven path, nothing pending before).
    pub job_counted: bool,
    pub opened: Option<BreakerOpened>,
}

impl DownloadBreaker {
    pub fn new(label: impl Into<String>) -> Self {
        DownloadBreaker {
            label: label.into(),
            consecutive: 0,
            proven: false,
            hold: DOWNLOAD_BREAKER_HOLD,
            open_until: 0.0,
            last_error: String::new(),
        }
    }

    pub fn is_open(&self, now: f64) -> bool {
        now < self.open_until
    }

    /// `_note_download_return`: count one return against the processor
    /// (`changed` never does); the third in a row opens the breaker for
    /// `hold`, and the next hold doubles (≤ 1 h).
    pub fn note_return(&mut self, klass: &str, error: &str, now: f64) -> ReturnNoted {
        let job_counted = self.proven && self.consecutive == 0;
        if klass == RETURN_CLASS_CHANGED {
            return ReturnNoted {
                job_counted,
                opened: None,
            };
        }
        self.consecutive += 1;
        self.last_error = truncate_chars(&format!("{klass}: {error}"), 300);
        let mut opened = None;
        if self.consecutive >= DOWNLOAD_BREAKER_RETURNS && !self.is_open(now) {
            let hold = self.hold;
            self.open_until = now + hold;
            self.hold = (hold * 2.0).min(DOWNLOAD_BREAKER_MAX_HOLD);
            opened = Some(BreakerOpened {
                hold,
                until: self.open_until,
                last_error: self.last_error.clone(),
                consecutive: self.consecutive,
            });
        }
        ReturnNoted {
            job_counted,
            opened,
        }
    }

    /// `_download_delivered`: the path works — proven, reset, closed.
    /// Returns whether it had been opened (log "… no longer held").
    pub fn note_delivered(&mut self) -> bool {
        let reopened = self.open_until > 0.0;
        self.proven = true;
        self.consecutive = 0;
        self.open_until = 0.0;
        self.hold = DOWNLOAD_BREAKER_HOLD;
        self.last_error.clear();
        reopened
    }
}

/// A job's counted download returns, across scans.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobReturns {
    pub count: u32,
    pub machines: Vec<String>,
    pub klass: String,
    pub error: String,
    pub machine: String,
    pub at: f64,
    /// `(size, mtime_ns)` of the archive when counted.
    pub stamp: Option<(u64, i128)>,
}

impl JobReturns {
    /// `_note_job_return`: a different file under the same name (stamp
    /// moved) starts the count again; `counted` appends the machine.
    pub fn note(
        existing: Option<JobReturns>,
        stamp: Option<(u64, i128)>,
        klass: &str,
        error: &str,
        machine: &str,
        counted: bool,
        now: f64,
    ) -> JobReturns {
        let mut returns = match existing {
            Some(r) if stamp.is_none() || r.stamp == stamp => r,
            _ => JobReturns {
                stamp,
                ..JobReturns::default()
            },
        };
        if counted {
            returns.count += 1;
            returns.machines.push(machine.to_owned());
        }
        returns.klass = klass.to_owned();
        returns.error = error.to_owned();
        returns.machine = machine.to_owned();
        returns.at = now;
        returns
    }

    /// The pending entry's `returned` object.
    pub fn as_entry(&self) -> serde_json::Value {
        serde_json::json!({
            "count": self.count,
            "class": self.klass,
            "error": self.error,
            "machine": self.machine,
            "at": self.at,
        })
    }

    /// Recorded as a failure: `"download failed on N tries (m1, m2): class: error"`.
    pub fn failure_summary(&self, klass: &str, error: &str) -> String {
        format!(
            "download failed on {} tries ({}): {klass}: {error}",
            self.count,
            self.machines.join(", ")
        )
    }
}

/// `_judge_returned`'s inputs from a `volume_returned` event:
/// `(str(class or "local")[:40], str(error or "")[:300])`.
pub fn return_class_and_error(
    event: &serde_json::Map<String, serde_json::Value>,
) -> (String, String) {
    use crate::py::{py_str, truthy};
    let klass = if truthy(event.get("class")) {
        py_str(event.get("class"))
    } else {
        "local".to_owned()
    };
    let error = if truthy(event.get("error")) {
        py_str(event.get("error"))
    } else {
        String::new()
    };
    (truncate_chars(&klass, 40), truncate_chars(&error, 300))
}

/// Whether a counted return also counts against the job (`no_room` never).
pub fn return_counts_for_job(job_counted: bool, klass: &str) -> bool {
    job_counted && klass != RETURN_CLASS_NO_ROOM
}

/// A row whose runner keeps failing to START on one machine.
#[derive(Clone, Debug, PartialEq)]
pub struct StartBackoff {
    pub failures: i64,
    pub until: f64,
    /// The error, first 300 characters.
    pub error: String,
    /// The row as that machine would run it (change it: retry at once).
    pub signature: String,
    pub name: String,
}

impl StartBackoff {
    /// `_note_start_failure`: count on from `previous` when the signature is
    /// the same, else start at 1; wait `retry_delay(failures)`.
    pub fn note_failure(
        previous: Option<&StartBackoff>,
        signature: &str,
        error: &str,
        name: &str,
        poll_interval: f64,
        now: f64,
    ) -> StartBackoff {
        let failures = match previous {
            Some(p) if p.signature == signature => p.failures + 1,
            _ => 1,
        };
        let delay = retry_delay_seconds(poll_interval, failures);
        StartBackoff {
            failures,
            until: now + delay,
            error: truncate_chars(error, 300),
            signature: signature.to_owned(),
            name: name.to_owned(),
        }
    }

    /// The log line of a fresh start failure (`where_`: `""` or `" on <machine>"`).
    pub fn log_line(
        &self,
        row_name: &str,
        where_: &str,
        poll_interval: f64,
        full_error: &str,
    ) -> String {
        let delay = retry_delay_seconds(poll_interval, self.failures);
        format!(
            "Not starting {row_name}{where_} again for {}s: its runner could not start ({} time{} in a row): {full_error}",
            crate::py::fmt_fixed(delay, 0),
            self.failures,
            if self.failures != 1 { "s" } else { "" }
        )
    }
}

/// `_backed_off_rows`' rule for one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackoffState {
    /// The row as that machine runs it changed: drop the entry, try now.
    Void,
    /// Still backing off.
    Waiting,
    /// The wait is over (the entry stays until a session is ready).
    Expired,
}

/// Judge one start backoff against the current signature.
pub fn backoff_state(backoff: &StartBackoff, current_signature: &str, now: f64) -> BackoffState {
    if current_signature != backoff.signature {
        BackoffState::Void
    } else if now < backoff.until {
        BackoffState::Waiting
    } else {
        BackoffState::Expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaker_opens_on_third_and_doubles() {
        let mut b = DownloadBreaker::new("box");
        assert!(!b.note_return("stalled", "x", 0.0).job_counted);
        b.note_delivered();
        assert!(b.note_return("stalled", "x", 1.0).job_counted);
        assert!(!b.note_return("stalled", "x", 2.0).job_counted);
        let third = b.note_return("stalled", "x", 3.0);
        let opened = third.opened.unwrap();
        assert_eq!(opened.hold, 600.0);
        assert_eq!(opened.until, 603.0);
        assert!(b.is_open(10.0));
        assert_eq!(b.hold, 1200.0);
        assert_eq!(
            opened.log_line("box"),
            "Holding box for 10 min: 3 archive downloads in a row failed (last: stalled: x)"
        );
        // While open, further returns count but do not re-open.
        assert!(b.note_return("stalled", "x", 4.0).opened.is_none());
        assert!(b.note_return("changed", "x", 700.0).opened.is_none());
        assert!(b.note_delivered());
        assert_eq!(b.hold, 600.0);
    }

    #[test]
    fn job_returns_reset_on_new_stamp() {
        let r = JobReturns::note(None, Some((1, 2)), "stalled", "e", "a", true, 1.0);
        let r = JobReturns::note(Some(r), Some((1, 2)), "stalled", "e", "b", true, 2.0);
        assert_eq!(r.count, 2);
        assert_eq!(
            r.failure_summary("stalled", "e"),
            "download failed on 2 tries (a, b): stalled: e"
        );
        let r = JobReturns::note(Some(r), Some((9, 9)), "stalled", "e", "c", false, 3.0);
        assert_eq!(r.count, 0);
        assert_eq!(r.machine, "c");
    }

    #[test]
    fn start_backoff_counts_per_signature() {
        let a = StartBackoff::note_failure(None, "sig", "no gpu", "main", 30.0, 0.0);
        assert_eq!((a.failures, a.until), (1, 30.0));
        let b = StartBackoff::note_failure(Some(&a), "sig", "no gpu", "main", 30.0, 100.0);
        assert_eq!((b.failures, b.until), (2, 220.0));
        let c = StartBackoff::note_failure(Some(&b), "other", "no gpu", "main", 30.0, 100.0);
        assert_eq!(c.failures, 1);
        assert_eq!(backoff_state(&b, "sig", 150.0), BackoffState::Waiting);
        assert_eq!(backoff_state(&b, "x", 150.0), BackoffState::Void);
        assert_eq!(
            b.log_line("main", " on box", 30.0, "no gpu"),
            "Not starting main on box again for 120s: its runner could not start (2 times in a row): no gpu"
        );
    }
}
