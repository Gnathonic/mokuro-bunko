//! The tray following its own program's update. An automatic update replaces the
//! executable on disk and restarts the instances it runs in (they exec into the new
//! file), but the tray is a separate process that would keep running the old one. It
//! notices the replaced file, asks it for its version, and once every instance it runs
//! is idle it stops them cleanly and restarts itself into the new release (which starts
//! them again, as at login).
//!
//! The decisions are pure (no clock, no I/O) so they are unit-tested; `app` feeds them.

use crate::status::Status;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// How often the tray looks at its executable.
pub const CHECK_EVERY: Duration = Duration::from_secs(30);

/// An instance that does not answer blocks the restart for at most this long (it may be
/// in the middle of something the tray cannot see); after it, it counts as idle.
pub const UNKNOWN_BOUND: Duration = Duration::from_secs(6 * 3600);

/// What identifies the file at a path: a replaced executable differs in at least one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileId {
    len: u64,
    modified: Option<SystemTime>,
    /// (device, inode) on Unix: a rename-over is a new inode even with the same size
    /// and time.
    node: Option<(u64, u64)>,
}

pub fn file_id(path: &Path) -> Option<FileId> {
    let m = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let node = {
        use std::os::unix::fs::MetadataExt;
        Some((m.dev(), m.ino()))
    };
    #[cfg(not(unix))]
    let node = None;
    Some(FileId {
        len: m.len(),
        modified: m.modified().ok(),
        node,
    })
}

/// `mokuro-bunko, version 0.7.2` (the first line of `mokuro-bunko --version`).
pub fn parse_version_output(out: &str) -> Option<String> {
    let line = out.lines().next()?.trim();
    let v = line.strip_prefix("mokuro-bunko, version ")?.trim();
    semver::Version::parse(v.trim_start_matches('v'))
        .ok()
        .map(|_| v.to_string())
}

/// Whether `candidate` is a newer release than `current` (semver precedence; anything
/// unparsable is not newer). Equal or older never restarts the tray: a rollback is the
/// updater's business, and restarting into the same release changes nothing.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| semver::Version::parse(v.trim().trim_start_matches('v')).ok();
    match (parse(candidate), parse(current)) {
        (Some(c), Some(cur)) => c.cmp_precedence(&cur).is_gt(),
        _ => false,
    }
}

/// The version of the program at `exe` (`--version`, at most `timeout`).
pub fn query_version(exe: &Path, timeout: Duration) -> Option<String> {
    let mut cmd = Command::new(exe);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    parse_version_output(&out)
}

/// What an instance the tray runs is doing, as far as the restart is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    Idle,
    /// Reading a volume, installing, downloading, mid-update: why.
    Busy(String),
    /// It does not answer.
    Unknown,
}

/// `status` of an instance (None: it does not answer).
pub fn activity(status: Option<&Status>) -> Activity {
    let Some(s) = status else {
        return Activity::Unknown;
    };
    if let Some(c) = s.current.first() {
        let what = if c.volume.is_empty() {
            "a volume".to_string()
        } else {
            c.volume.clone()
        };
        return Activity::Busy(format!("reading {what}"));
    }
    if matches!(s.state.as_str(), "working" | "installing" | "pausing") {
        return Activity::Busy(s.state.clone());
    }
    if s.install.as_ref().is_some_and(|i| i.state == "running") {
        return Activity::Busy("installing the OCR backend".into());
    }
    if let Some(u) = &s.update
        && crate::autoupdate::is_attempt(&u.state)
    {
        return Activity::Busy(format!("updating ({})", u.state));
    }
    if let Some(a) = s
        .library
        .as_ref()
        .and_then(|l| l.activity.as_deref())
        .filter(|a| !a.is_empty())
    {
        return Activity::Busy(a.to_string());
    }
    Activity::Idle
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Stop the instances and restart now.
    Restart,
    /// Not yet: why.
    Wait(String),
}

/// Whether to restart now, given what each instance the tray runs is doing and how long
/// the new release has been waiting. A busy instance always waits (the tray never stops
/// an instance mid-job); one that does not answer waits up to [`UNKNOWN_BOUND`].
pub fn decide(activities: &[(String, Activity)], waited: Duration) -> Decision {
    for (role, a) in activities {
        match a {
            Activity::Busy(why) => return Decision::Wait(format!("{role}: {why}")),
            Activity::Unknown if waited < UNKNOWN_BOUND => {
                return Decision::Wait(format!("{role}: not answering"));
            }
            _ => {}
        }
    }
    Decision::Restart
}

/// The tray's own pause (after the current volume: an instance that reads volume after
/// volume is never idle by itself) lifts itself after this, even when no tray resumes it.
pub const PAUSE_FOR: Duration = Duration::from_secs(2 * 3600);
/// A pause of the tray's with less than this left is renewed while the volume still runs.
pub const PAUSE_RENEW: Duration = Duration::from_secs(30 * 60);
/// The new tray resumes the pauses of the old one within this (else they lift by
/// themselves at their `until`).
pub const RESUME_WITHIN: Duration = Duration::from_secs(10 * 60);
/// `<tray data>/.tray-resume.json`: the pauses the tray made for its restart, by role
/// (their `until`), for the restarted tray to lift.
pub const RESUME_FILE: &str = ".tray-resume.json";

fn parse_time(t: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(t)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

/// Whether `status`'s pause is the one the tray asked for (`ours`: its `until`).
pub fn is_our_pause(status: &Status, ours: &str) -> bool {
    (status.is_paused() || status.is_pausing())
        && status.pause.until.as_deref().and_then(parse_time) == parse_time(ours)
        && parse_time(ours).is_some()
}

/// Whether to ask an instance to pause after its current volume so the tray can
/// restart (`ours`: the `until` of the tray's pause already asked for). Only an instance
/// reading volumes is asked; a pause someone else made is never touched; the tray's own
/// is renewed while the volume outlasts it.
pub fn should_pause(
    status: &Status,
    ours: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if !status.can_pause() || status.is_paused() {
        return false;
    }
    if status.is_pausing() {
        return match ours {
            Some(o) if is_our_pause(status, o) => {
                parse_time(o).is_some_and(|u| (u - now).to_std().unwrap_or_default() < PAUSE_RENEW)
            }
            _ => false,
        };
    }
    !status.current.is_empty() || status.state == "working"
}

/// The `until` of a new pause of the tray's.
pub fn pause_until(now: chrono::DateTime<chrono::Utc>) -> String {
    let d = chrono::Duration::from_std(PAUSE_FOR).unwrap_or_else(|_| chrono::Duration::hours(2));
    (now + d).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The menu line while a new release waits for the instances to be idle.
pub fn waiting_line(version: &str) -> String {
    format!("Restarting the tray for {version} after the current job")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{Current, InstallInfo, Library, UpdateInfo};

    fn st(state: &str) -> Status {
        Status {
            state: state.into(),
            ..Status::default()
        }
    }

    #[test]
    fn versions() {
        assert_eq!(
            parse_version_output("mokuro-bunko, version 0.7.2\nflavor: full\n").as_deref(),
            Some("0.7.2")
        );
        assert_eq!(
            parse_version_output("mokuro-bunko, version 0.7.0-beta.5").as_deref(),
            Some("0.7.0-beta.5")
        );
        assert_eq!(parse_version_output("something else 0.7.2"), None);
        assert_eq!(parse_version_output("mokuro-bunko, version garbage"), None);
        assert_eq!(parse_version_output(""), None);
        assert!(is_newer("0.7.2", "0.7.1"));
        assert!(is_newer("0.7.1", "0.7.0-beta.5"));
        assert!(!is_newer("0.7.1", "0.7.1"), "equal: nothing to do");
        assert!(
            !is_newer("0.7.0", "0.7.1"),
            "older: a rollback is the updater's"
        );
        assert!(!is_newer("garbage", "0.7.1"));
    }

    #[test]
    fn what_counts_as_busy() {
        assert_eq!(activity(None), Activity::Unknown);
        assert_eq!(activity(Some(&st("idle"))), Activity::Idle);
        assert_eq!(activity(Some(&st("paused"))), Activity::Idle);
        assert_eq!(activity(Some(&st("disconnected"))), Activity::Idle);
        assert!(matches!(activity(Some(&st("working"))), Activity::Busy(_)));
        assert!(matches!(activity(Some(&st("pausing"))), Activity::Busy(_)));
        assert!(matches!(
            activity(Some(&st("installing"))),
            Activity::Busy(_)
        ));
        let mut s = st("idle");
        s.current = vec![Current {
            volume: "Vol 01".into(),
            ..Current::default()
        }];
        assert_eq!(activity(Some(&s)), Activity::Busy("reading Vol 01".into()));
        let mut s = st("idle");
        s.install = Some(InstallInfo {
            state: "running".into(),
            ..InstallInfo::default()
        });
        assert!(matches!(activity(Some(&s)), Activity::Busy(_)));
        s.install.as_mut().unwrap().state = "done".into();
        assert_eq!(activity(Some(&s)), Activity::Idle);
        for (state, busy) in [
            ("downloading", true),
            ("waiting", true),
            ("restarting", true),
            ("updated", false),
            ("available", false),
            ("failed", false),
        ] {
            let mut s = st("idle");
            s.update = Some(UpdateInfo {
                state: state.into(),
                ..UpdateInfo::default()
            });
            assert_eq!(
                matches!(activity(Some(&s)), Activity::Busy(_)),
                busy,
                "{state}"
            );
        }
        let mut s = st("idle");
        s.library = Some(Library {
            activity: Some("measuring hayai-nova".into()),
            ..Library::default()
        });
        assert!(matches!(activity(Some(&s)), Activity::Busy(_)));
    }

    #[test]
    fn restart_only_when_everything_it_runs_is_idle() {
        let idle = ("server".to_string(), Activity::Idle);
        let busy = (
            "processor".to_string(),
            Activity::Busy("reading a volume".into()),
        );
        let silent = ("processor".to_string(), Activity::Unknown);
        assert_eq!(decide(&[], Duration::ZERO), Decision::Restart);
        assert_eq!(
            decide(std::slice::from_ref(&idle), Duration::ZERO),
            Decision::Restart
        );
        assert_eq!(
            decide(&[idle.clone(), busy.clone()], Duration::ZERO),
            Decision::Wait("processor: reading a volume".into())
        );
        // Busy never gives way, however long the new release has waited.
        assert!(matches!(
            decide(&[busy], UNKNOWN_BOUND * 4),
            Decision::Wait(_)
        ));
        // Not answering: waits up to the bound, then counts as idle.
        assert!(matches!(
            decide(&[idle.clone(), silent.clone()], Duration::from_secs(60)),
            Decision::Wait(_)
        ));
        assert_eq!(decide(&[idle, silent], UNKNOWN_BOUND), Decision::Restart);
    }

    #[test]
    fn the_tray_pauses_only_volume_work_and_never_someone_elses_pause() {
        use crate::status::Pause;
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-12T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let mut working = st("working");
        working.role = "server".into();
        working.current = vec![Current::default()];
        assert!(should_pause(&working, None, now));
        // Not reading (idle, installing, updating): pausing would not help.
        let mut idle = st("idle");
        idle.role = "server".into();
        assert!(!should_pause(&idle, None, now));
        let mut installing = st("installing");
        installing.role = "server".into();
        assert!(!should_pause(&installing, None, now));
        // A server that cannot pause (lite).
        let mut lite = working.clone();
        lite.can_pause = Some(false);
        assert!(!should_pause(&lite, None, now));
        // Already paused: idle for the restart, nothing to ask.
        let mut paused = st("paused");
        paused.role = "server".into();
        assert!(!should_pause(&paused, None, now));
        // Pausing on someone else's request: leave it.
        let mut owners = working.clone();
        owners.state = "pausing".into();
        owners.pause = Pause {
            mode: Some("after_volume".into()),
            until: None,
            reason: Some("user".into()),
        };
        assert!(!should_pause(&owners, None, now));
        assert!(!should_pause(&owners, Some("2026-10-12T12:00:00Z"), now));
        // Pausing on ours: renewed only when it is about to lift.
        let ours = pause_until(now);
        assert_eq!(ours, "2026-10-12T12:00:00Z");
        let mut mine = owners.clone();
        mine.pause.until = Some("2026-10-12T12:00:00+00:00".into());
        mine.pause.reason = Some("schedule".into());
        assert!(is_our_pause(&mine, &ours));
        assert!(!should_pause(&mine, Some(&ours), now));
        let later = now + chrono::Duration::minutes(100);
        assert!(should_pause(&mine, Some(&ours), later));
        // Resume only our own pause.
        assert!(!is_our_pause(&owners, &ours));
        assert!(!is_our_pause(&idle, &ours));
        let mut mine_paused = mine.clone();
        mine_paused.state = "paused".into();
        assert!(is_our_pause(&mine_paused, &ours));
        assert!(!is_our_pause(&mine_paused, "not a time"));
    }

    #[test]
    fn a_replaced_file_has_another_id() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("prog");
        std::fs::write(&p, b"old").unwrap();
        let a = file_id(&p).unwrap();
        assert_eq!(file_id(&p), Some(a.clone()));
        // Replaced the way the updater does: a new file renamed over the old one.
        let n = tmp.path().join("prog.new");
        std::fs::write(&n, b"newer").unwrap();
        std::fs::rename(&n, &p).unwrap();
        assert_ne!(file_id(&p), Some(a));
        assert_eq!(file_id(&tmp.path().join("missing")), None);
    }

    #[cfg(unix)]
    #[test]
    fn query_version_runs_the_program() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("mokuro-bunko");
        std::fs::write(
            &p,
            "#!/bin/sh\necho 'mokuro-bunko, version 9.9.9'\necho 'flavor: full'\n",
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            query_version(&p, Duration::from_secs(10)).as_deref(),
            Some("9.9.9")
        );
        assert_eq!(
            query_version(&tmp.path().join("missing"), Duration::from_secs(1)),
            None
        );
    }
}
