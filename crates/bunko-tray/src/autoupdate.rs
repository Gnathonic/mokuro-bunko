//! The tray's side of opt-in automatic updates: telling a deliberate restart from a
//! crash, and notifying once per distinct "needs you" problem. Pure (no clock, no
//! I/O) so it is unit-tested; `app.rs` feeds it and `notify.rs` shows the result.

use crate::status::Status;
use std::collections::HashSet;
use std::time::Duration;

/// How long after its last answer an instance that said it was installing or
/// restarting is still assumed to be doing exactly that.
pub const RESTART_GRACE: Duration = Duration::from_secs(180);

/// "Updating to 0.7.1…" for an instance that stopped answering (or left discovery, or
/// whose supervised slot is restarting) while its last good status said `installing`
/// or `restarting`, and that status is at most `RESTART_GRACE` old. `None`: behave as
/// usual (a crash is a crash). Returns the version being installed (None when the
/// instance did not say).
pub fn restart_grace(last_good: Option<&Status>, age: Duration) -> Option<Option<String>> {
    let u = last_good?.update.as_ref()?;
    if age > RESTART_GRACE || !is_restarting(&u.state) {
        return None;
    }
    Some(u.version.clone().filter(|v| !v.is_empty()))
}

/// States in which the instance is about to exec/exit by itself.
pub fn is_restarting(state: &str) -> bool {
    matches!(state, "installing" | "restarting")
}

/// States of an update attempt in progress (its outcome, and any problem, still to come).
pub fn is_attempt(state: &str) -> bool {
    matches!(
        state,
        "waiting" | "downloading" | "installing" | "restarting"
    )
}

/// A problem only the owner can fix, to be shown as a desktop notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alarm {
    pub role: String,
    pub text: String,
    pub hint: Option<String>,
}

impl Alarm {
    pub const TITLE: &'static str = "Mokuro Bunko needs you";

    /// The notification body: the problem, then its fix.
    pub fn body(&self) -> String {
        match self.hint.as_deref().filter(|h| !h.is_empty()) {
            Some(h) => format!("{}\n{h}", self.text),
            None => self.text.clone(),
        }
    }
}

/// Remembers which (role, text) alarms were already shown. A problem that goes away
/// is forgotten, so the same one coming back is announced again — but not while the
/// instance is only trying again: a retry clears its problem for the attempt and the
/// same failure returns a few seconds later (seen on macOS: the update to a release
/// whose pack does not load, retried on its backoff, notified on every attempt).
#[derive(Debug, Default)]
pub struct Notified {
    seen: HashSet<(String, String)>,
}

impl Notified {
    /// Feed the latest status of `role`; returns the alarms not yet announced.
    pub fn observe(&mut self, role: &str, status: &Status) -> Vec<Alarm> {
        let current: Vec<_> = status
            .problems
            .iter()
            .filter(|p| p.kind.as_deref() == Some("update") && p.severity == "fail")
            .collect();
        let attempting = status.update.as_ref().is_some_and(|u| is_attempt(&u.state));
        if !attempting {
            self.seen
                .retain(|(r, t)| r != role || current.iter().any(|p| p.text == *t));
        }
        let mut out = Vec::new();
        for p in current {
            if self.seen.insert((role.to_string(), p.text.clone())) {
                out.push(Alarm {
                    role: role.to_string(),
                    text: p.text.clone(),
                    hint: p.hint.clone(),
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(json: &str) -> Status {
        serde_json::from_str(json).unwrap()
    }

    fn restarting(state: &str) -> Status {
        st(&format!(
            r#"{{"role":"server","update":{{"state":"{state}","version":"0.7.1"}}}}"#
        ))
    }

    #[test]
    fn grace_covers_installing_and_restarting_for_three_minutes() {
        let s = restarting("restarting");
        assert_eq!(
            restart_grace(Some(&s), Duration::from_secs(10)),
            Some(Some("0.7.1".into()))
        );
        assert!(restart_grace(Some(&restarting("installing")), Duration::from_secs(179)).is_some());
        assert_eq!(restart_grace(Some(&s), Duration::from_secs(181)), None);
    }

    #[test]
    fn no_grace_for_other_states_or_no_history() {
        for state in [
            "idle",
            "waiting",
            "downloading",
            "updated",
            "failed",
            "blocked",
        ] {
            assert_eq!(
                restart_grace(Some(&restarting(state)), Duration::ZERO),
                None,
                "{state}"
            );
        }
        assert_eq!(restart_grace(None, Duration::ZERO), None);
        let plain = st(r#"{"role":"server","state":"idle"}"#);
        assert_eq!(restart_grace(Some(&plain), Duration::ZERO), None);
        // Version unknown: still a grace, without a version.
        let nov = st(r#"{"update":{"state":"restarting"}}"#);
        assert_eq!(restart_grace(Some(&nov), Duration::ZERO), Some(None));
    }

    fn with_problems(problems: &str) -> Status {
        st(&format!(r#"{{"problems":{problems}}}"#))
    }

    #[test]
    fn one_alarm_per_distinct_update_problem() {
        let mut n = Notified::default();
        let a = with_problems(
            r#"[{"severity":"fail","text":"A","hint":"fix A","kind":"update"},
                {"severity":"warn","text":"W","kind":"update"},
                {"severity":"fail","text":"Other"}]"#,
        );
        let first = n.observe("server", &a);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].body(), "A\nfix A");
        assert_eq!(Alarm::TITLE, "Mokuro Bunko needs you");
        // Every later poll: nothing new.
        assert!(n.observe("server", &a).is_empty());
        // The other role has its own copy of the same text.
        assert_eq!(n.observe("processor", &a).len(), 1);
        // A second distinct problem only announces itself.
        let ab = with_problems(
            r#"[{"severity":"fail","text":"A","kind":"update"},{"severity":"fail","text":"B","kind":"update"}]"#,
        );
        let new = n.observe("server", &ab);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].text, "B");
        assert_eq!(new[0].body(), "B");
    }

    #[test]
    fn a_retry_of_the_same_failure_is_not_announced_again() {
        let mut n = Notified::default();
        let blocked = st(
            r#"{"update":{"state":"blocked","version":"0.7.1"},"problems":[{"severity":"fail","text":"A","kind":"update"}]}"#,
        );
        assert_eq!(n.observe("server", &blocked).len(), 1);
        // The retry: the attempt clears the problem while it runs...
        for state in ["waiting", "downloading", "installing"] {
            let trying = st(&format!(
                r#"{{"update":{{"state":"{state}","version":"0.7.1"}},"problems":[]}}"#
            ));
            assert!(n.observe("server", &trying).is_empty(), "{state}");
        }
        // ...and fails the same way: no second notification.
        assert!(n.observe("server", &blocked).is_empty());
        // Fixed for real (the update went through), then a new failure later: announced.
        let updated = st(r#"{"update":{"state":"updated","version":"0.7.1"},"problems":[]}"#);
        assert!(n.observe("server", &updated).is_empty());
        assert_eq!(n.observe("server", &blocked).len(), 1);
    }

    #[test]
    fn a_problem_that_goes_away_and_returns_is_announced_again() {
        let mut n = Notified::default();
        let a = with_problems(r#"[{"severity":"fail","text":"A","kind":"update"}]"#);
        assert_eq!(n.observe("server", &a).len(), 1);
        assert!(n.observe("server", &with_problems("[]")).is_empty());
        assert_eq!(n.observe("server", &a).len(), 1);
    }
}
