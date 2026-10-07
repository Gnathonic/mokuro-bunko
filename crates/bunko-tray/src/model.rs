//! What the menu and the icon show, computed from what the tray knows (GUI.md §5).
//! Pure: the UI layer only copies these strings and flags into native menu items.

use crate::status::Status;
use crate::trayconf::role_label;
use chrono::{DateTime, Local, NaiveDate};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IconState {
    Idle,
    Working,
    Paused,
    Attention,
}

impl IconState {
    pub fn name(self) -> &'static str {
        match self {
            IconState::Idle => "idle",
            IconState::Working => "working",
            IconState::Paused => "paused",
            IconState::Attention => "attention",
        }
    }
}

/// One instance as the tray sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceView {
    pub role: String,
    /// The last status received (None: not answering yet).
    pub status: Option<Status>,
    /// Why the last request failed, when it did.
    pub error: Option<String>,
    /// Started by this tray (Quit stops it) rather than by a service or a terminal.
    pub tray_started: bool,
}

/// A tray-managed role that is not up (starting, crashed, waiting to restart).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisedView {
    pub role: String,
    pub text: String,
    pub failing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateView {
    pub checking: bool,
    pub latest: Option<String>,
    pub available: bool,
    pub error: Option<String>,
    /// Not a failure (e.g. no release published yet).
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuModel {
    pub status_lines: Vec<String>,
    pub stats_lines: Vec<String>,
    pub icon: IconState,
    pub tooltip: String,
    pub can_pause_after: bool,
    pub can_pause_now: bool,
    pub can_resume: bool,
    pub can_open_dashboard: bool,
    pub library_url: Option<String>,
    pub update_text: String,
    /// The update item leads to the Updates settings (a newer release is known) rather
    /// than running a check.
    pub update_available: bool,
    pub quit_text: String,
}

fn n(v: Option<f64>) -> String {
    v.map(|x| group(x.round() as i64))
        .unwrap_or_else(|| "?".into())
}

/// 7310 → "7,310".
fn group(v: i64) -> String {
    let digits = v.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if v < 0 { format!("-{out}") } else { out }
}

fn plural(v: Option<f64>, one: &str, many: &str) -> String {
    let word = if v.map(|x| x.round() as i64) == Some(1) {
        one
    } else {
        many
    };
    format!("{} {word}", n(v))
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// "Series/Dr Stone 01" → "Dr Stone 01".
fn volume_name(v: &str) -> &str {
    v.trim_end_matches('/')
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(v)
}

/// `until` as local time: "18:00" today, "Mon 08:00" within a week, else the date.
pub fn until_text(until: &str, now: DateTime<Local>) -> String {
    let Ok(t) = DateTime::parse_from_rfc3339(until) else {
        return until.to_string();
    };
    let t = t.with_timezone(&Local);
    let days = t
        .date_naive()
        .signed_duration_since(now.date_naive())
        .num_days();
    match days {
        0 => t.format("%H:%M").to_string(),
        1 => format!("tomorrow {}", t.format("%H:%M")),
        2..=6 => t.format("%a %H:%M").to_string(),
        _ => t.format("%Y-%m-%d %H:%M").to_string(),
    }
}

fn state_text(s: &Status, now: DateTime<Local>) -> String {
    let queue = s
        .library
        .as_ref()
        .and_then(|l| l.queue_pending)
        .map(|q| format!(" — {} in queue", n(Some(q))));
    match s.state.as_str() {
        "working" => match s.current.first() {
            Some(c) => {
                let mut parts = vec![format!("Working: {}", volume_name(&c.volume))];
                if c.pages_done.is_some() || c.pages_total.is_some() {
                    parts.push(format!("{}/{}", n(c.pages_done), n(c.pages_total)));
                }
                if let Some(r) = c.pages_per_second {
                    parts.push(format!("{r:.1} p/s"));
                }
                let mut text = parts.join(" · ");
                if s.current.len() > 1 {
                    text.push_str(&format!(" (+{} more)", s.current.len() - 1));
                }
                text
            }
            None => "Working".into(),
        },
        "idle" => format!("Idle{}", queue.unwrap_or_default()),
        "paused" | "pausing" => {
            let base = if s.state == "pausing" {
                "Pausing after this volume"
            } else {
                "Paused"
            };
            match s.pause.until.as_deref().filter(|u| !u.is_empty()) {
                Some(u) => format!("{base} until {}", until_text(u, now)),
                None => base.to_string(),
            }
        }
        "connecting" => "Connecting to the library…".into(),
        "disconnected" => match s.library.as_ref().and_then(|l| l.error.as_deref()) {
            Some(e) if !e.is_empty() => format!("Can't reach library ({})", shorten(e, 60)),
            _ => "Can't reach library".into(),
        },
        "error" => match s.problems.first() {
            Some(p) => format!("Error: {}", p.text),
            None => "Error".into(),
        },
        "setup" => "Not set up yet — open the setup wizard".into(),
        "" => "Running".into(),
        other => {
            let mut c = other.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        }
    }
}

pub struct Inputs<'a> {
    pub instances: &'a [InstanceView],
    pub supervised: &'a [SupervisedView],
    pub update: &'a UpdateView,
    /// Roles inside the restart grace (an automatic update is restarting them), with
    /// the version being installed: shown as "Updating to X…", never as a crash.
    pub updating: &'a [(String, Option<String>)],
    /// A short-lived message (an action that failed).
    pub notice: Option<&'a str>,
    pub now: DateTime<Local>,
}

/// How long "Updated to X" stays in the menu.
const UPDATED_SHOWN_FOR_MINUTES: i64 = 15;

fn updating_text(version: Option<&str>) -> String {
    match version.filter(|v| !v.is_empty()) {
        Some(v) => format!("Updating to {v}…"),
        None => "Updating…".into(),
    }
}

/// The status line that replaces the state text while an automatic update runs.
fn updating_line(s: &Status) -> Option<String> {
    let u = s.update.as_ref()?;
    let base = updating_text(u.version.as_deref());
    match u.state.as_str() {
        "waiting" => Some(format!("{base} (after the running volume)")),
        "downloading" | "installing" | "restarting" => Some(base),
        _ => None,
    }
}

/// An automatic update of this instance is installing or restarting it, or finished
/// less than a minute ago (its processors may not have reconnected yet).
fn update_restarting(s: &Status, now: DateTime<Local>) -> bool {
    let Some(u) = s.update.as_ref() else {
        return false;
    };
    match u.state.as_str() {
        "installing" | "restarting" => true,
        "updated" => u
            .since
            .as_deref()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .is_some_and(|t| {
                now.signed_duration_since(t.with_timezone(&Local)) < chrono::Duration::seconds(60)
            }),
        _ => false,
    }
}

/// Extra lines under the state: the result of the last automatic update.
fn update_result_line(s: &Status, now: DateTime<Local>) -> Option<String> {
    let u = s.update.as_ref()?;
    match u.state.as_str() {
        "updated" => {
            let since = DateTime::parse_from_rfc3339(u.since.as_deref()?).ok()?;
            let age = now.signed_duration_since(since.with_timezone(&Local));
            if age > chrono::Duration::minutes(UPDATED_SHOWN_FOR_MINUTES) {
                return None;
            }
            Some(match u.version.as_deref().filter(|v| !v.is_empty()) {
                Some(v) => format!("Updated to {v}"),
                None => "Updated".into(),
            })
        }
        "failed" => Some(match u.version.as_deref().filter(|v| !v.is_empty()) {
            Some(v) => format!("⚠ Update to {v} failed — will retry"),
            None => "⚠ Update failed — will retry".into(),
        }),
        _ => None,
    }
}

pub fn build(inp: &Inputs) -> MenuModel {
    let mut status_lines = Vec::new();
    let mut stats_lines = Vec::new();
    let mut attention = false;
    let mut working = false;
    let mut paused_any = false;
    let mut pausable_running = false; // can still be paused (not paused/pausing)
    let mut pausable_not_fully_paused = false; // pausing counts: "now" still helps
    let mut library_url = None;
    let mut dashboard = false;
    let multi = inp.instances.len() + inp.supervised.len() > 1;

    let grace = |role: &str| inp.updating.iter().find(|(r, _)| r == role);
    // The library on this machine is restarting for its update (or came back from it
    // within the last minute): a processor here that lost it for those seconds is
    // expected, not something to flag (seen on macOS: one "Can't reach library" sample
    // while the server exec'd into the new release).
    let library_restarting = grace("server").is_some()
        || inp.instances.iter().any(|i| {
            i.role == "server"
                && i.status
                    .as_ref()
                    .is_some_and(|s| update_restarting(s, inp.now))
        });
    for inst in inp.instances {
        let label = role_label(&inst.role);
        if inst.status.is_none()
            && let Some((_, v)) = grace(&inst.role)
        {
            status_lines.push(format!("{label}: {}", updating_text(v.as_deref())));
            continue;
        }
        match &inst.status {
            Some(s) => {
                dashboard = true;
                let text = updating_line(s).unwrap_or_else(|| {
                    if s.state == "disconnected" && inst.role == "processor" && library_restarting {
                        "Reconnecting (the library is restarting for its update)".into()
                    } else {
                        state_text(s, inp.now)
                    }
                });
                status_lines.push(format!("{label}: {text}"));
                if let Some(l) = update_result_line(s, inp.now) {
                    status_lines.push(if l.starts_with('⚠') || !multi {
                        l
                    } else {
                        format!("{label}: {l}")
                    });
                }
                let expected_gap =
                    s.state == "disconnected" && inst.role == "processor" && library_restarting;
                if (matches!(s.state.as_str(), "error" | "disconnected") && !expected_gap)
                    || s.problems.iter().any(|p| p.severity == "fail")
                {
                    attention = true;
                }
                // Say why the icon asks for attention ("error" already names its problem).
                let fails: Vec<_> = s.problems.iter().filter(|p| p.severity == "fail").collect();
                if s.state != "error"
                    && let Some(p) = fails.first()
                {
                    let more = match fails.len() {
                        1 => String::new(),
                        n => format!(" (+{} more under Statistics)", n - 1),
                    };
                    status_lines.push(format!("✖ {}{more}", shorten(&p.text, 70)));
                }
                if s.state == "working" {
                    working = true;
                }
                if s.is_paused() || s.is_pausing() {
                    paused_any = true;
                }
                if s.can_pause() {
                    if !s.is_paused() && !s.is_pausing() {
                        pausable_running = true;
                    }
                    if !s.is_paused() {
                        pausable_not_fully_paused = true;
                    }
                }
                if library_url.is_none() {
                    library_url = s.library_url().map(str::to_string);
                }
                stats_lines.extend(stats_for(s, multi.then_some(label)));
            }
            None => {
                attention = attention || inst.error.is_some();
                status_lines.push(format!(
                    "{label}: {}",
                    if inst.error.is_some() {
                        "not responding"
                    } else {
                        "starting…"
                    }
                ));
            }
        }
    }
    // A role that is restarting for its update but is not (yet) in discovery at all.
    for (role, v) in inp.updating {
        if !inp.instances.iter().any(|i| i.role == *role) {
            status_lines.push(format!(
                "{}: {}",
                role_label(role),
                updating_text(v.as_deref())
            ));
        }
    }
    for sup in inp.supervised {
        if grace(&sup.role).is_some() {
            continue; // the deliberate restart, not "stopped (exit code 75)"
        }
        status_lines.push(format!("{}: {}", role_label(&sup.role), sup.text));
        attention = attention || sup.failing;
    }
    if status_lines.is_empty() {
        status_lines.push("Not running".into());
    }
    if let Some(n) = inp.notice {
        status_lines.push(format!("⚠ {n}"));
    }
    if stats_lines.is_empty() {
        stats_lines.push("No statistics yet".into());
    }

    let icon = if attention {
        IconState::Attention
    } else if working {
        IconState::Working
    } else if paused_any {
        IconState::Paused
    } else {
        IconState::Idle
    };

    // An instance whose own check found a newer release it will not install by itself
    // (`update.auto` off): say so here too, until a manual check here says otherwise.
    let reported = inp.instances.iter().find_map(|i| {
        let u = i.status.as_ref()?.update.as_ref()?;
        (u.state == "available").then(|| u.version.clone().unwrap_or_default())
    });
    let unchecked =
        inp.update.latest.is_none() && inp.update.error.is_none() && inp.update.note.is_none();
    let update_available = inp.update.available || (unchecked && reported.is_some());
    let update_text = if inp.update.checking {
        "Checking for updates…".to_string()
    } else if update_available {
        let v = inp
            .update
            .latest
            .clone()
            .or(reported)
            .filter(|v| !v.is_empty());
        format!(
            "Update available: {} …",
            v.as_deref().unwrap_or("new version")
        )
    } else if let Some(latest) = &inp.update.latest {
        format!("Up to date ({latest}) — check again")
    } else if inp.update.note.is_some() {
        "No release published yet — check again".to_string()
    } else if inp.update.error.is_some() {
        "Update check failed — try again".to_string()
    } else {
        "Check for updates".to_string()
    };

    let external: Vec<&str> = inp
        .instances
        .iter()
        .filter(|i| !i.tray_started && i.role != "gui")
        .map(|i| role_label(&i.role))
        .collect();
    let quit_text = if external.is_empty() {
        "Quit".to_string()
    } else {
        format!(
            "Quit (the {} {} running)",
            external.join(" and ").to_lowercase(),
            if external.len() > 1 { "keep" } else { "keeps" }
        )
    };

    let tooltip = format!("mokuro-bunko — {}", status_lines.join("; "));
    MenuModel {
        status_lines,
        stats_lines,
        icon,
        tooltip,
        can_pause_after: pausable_running,
        can_pause_now: pausable_not_fully_paused,
        can_resume: paused_any,
        can_open_dashboard: dashboard,
        library_url,
        update_text,
        update_available,
        quit_text,
    }
}

fn stats_for(s: &Status, prefix: Option<&str>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(st) = &s.stats {
        if let Some(t) = &st.today {
            lines.push(format!(
                "Today: {}, {}",
                plural(t.volumes, "volume", "volumes"),
                plural(t.pages, "page", "pages")
            ));
        }
        if let Some(t) = &st.total {
            lines.push(format!(
                "Total: {}, {}",
                plural(t.volumes, "volume", "volumes"),
                plural(t.pages, "page", "pages")
            ));
        }
        if let Some(r) = st.rate_pages_per_minute {
            lines.push(format!("Rate: {} pages/min", n(Some(r))));
        }
        if let Some(g) = st.gpu_busy_percent {
            lines.push(format!("GPU busy: {}%", n(Some(g))));
        }
        if let Some(c) = st.cpu_cores_busy {
            lines.push(format!("CPU: {c:.1} cores busy"));
        }
    }
    if let Some(b) = &s.backend {
        if let Some(p) = &b.pack {
            lines.push(format!("Backend: {p}"));
        }
        for d in &b.devices {
            lines.push(format!("Device: {d}"));
        }
    }
    if let Some(lib) = &s.library
        && let Some(q) = lib.queue_pending
    {
        lines.push(format!("Queue: {} waiting", n(Some(q))));
    }
    for p in &s.problems {
        let mark = if p.severity == "fail" { "✖" } else { "⚠" };
        lines.push(format!("{mark} {}", p.text));
    }
    match prefix {
        Some(label) => lines
            .into_iter()
            .map(|l| format!("{label} · {l}"))
            .collect(),
        None => lines,
    }
}

/// "Pause until tomorrow 08:00": the next day's 08:00 local time, ISO-8601 with offset.
pub fn tomorrow_at_8(now: DateTime<Local>) -> String {
    let date: NaiveDate = now.date_naive().succ_opt().unwrap_or(now.date_naive());
    let naive = date.and_hms_opt(8, 0, 0).unwrap_or_default();
    // A DST gap at 08:00 is not a thing anywhere in practice; fall back to +1 day.
    naive
        .and_local_timezone(Local)
        .earliest()
        .unwrap_or(now + chrono::Duration::days(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// "Pause for 1 hour".
pub fn in_one_hour(now: DateTime<Local>) -> String {
    (now + chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::tests::CONTRACT_EXAMPLE;
    use chrono::TimeZone;

    fn now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 4, 12, 30, 0).unwrap()
    }

    fn status(json: &str) -> Status {
        serde_json::from_str(json).unwrap()
    }

    fn inst(s: Status, tray_started: bool) -> InstanceView {
        InstanceView {
            role: s.role.clone(),
            status: Some(s),
            error: None,
            tray_started,
        }
    }

    fn model(instances: &[InstanceView]) -> MenuModel {
        build(&Inputs {
            instances,
            supervised: &[],
            update: &UpdateView::default(),
            updating: &[],
            notice: None,
            now: now(),
        })
    }

    fn with_update(state: &str, since: &str) -> Status {
        status(&format!(
            r#"{{"role":"server","state":"idle","update":{{"state":"{state}","version":"0.7.1","from":"0.7.0","since":"{since}"}}}}"#
        ))
    }

    #[test]
    fn updating_replaces_the_state_text() {
        for state in ["downloading", "installing", "restarting"] {
            let m = model(&[inst(with_update(state, ""), false)]);
            assert_eq!(m.status_lines, ["Library: Updating to 0.7.1…"], "{state}");
        }
        let m = model(&[inst(with_update("waiting", ""), false)]);
        assert_eq!(
            m.status_lines,
            ["Library: Updating to 0.7.1… (after the running volume)"]
        );
        assert_eq!(m.icon, IconState::Idle);
    }

    #[test]
    fn updated_is_shown_for_fifteen_minutes() {
        let at = |mins: i64| (now() - chrono::Duration::minutes(mins)).to_rfc3339();
        let m = model(&[inst(with_update("updated", &at(5)), false)]);
        assert_eq!(m.status_lines, ["Library: Idle", "Updated to 0.7.1"]);
        let m = model(&[inst(with_update("updated", &at(16)), false)]);
        assert_eq!(m.status_lines, ["Library: Idle"]);
        // No usable time: do not claim it is recent.
        let m = model(&[inst(with_update("updated", ""), false)]);
        assert_eq!(m.status_lines, ["Library: Idle"]);
    }

    #[test]
    fn an_update_the_library_only_reports_shows_in_the_update_item() {
        let m = model(&[inst(with_update("available", ""), true)]);
        assert_eq!(m.update_text, "Update available: 0.7.1 …");
        assert!(m.update_available);
        assert_eq!(m.icon, IconState::Idle);
        // A manual check here wins.
        let m = build(&Inputs {
            instances: &[inst(with_update("available", ""), true)],
            supervised: &[],
            update: &UpdateView {
                latest: Some("0.7.1".into()),
                ..Default::default()
            },
            updating: &[],
            notice: None,
            now: now(),
        });
        assert!(!m.update_available);
        let m = model(&[inst(with_update("updated", ""), true)]);
        assert_eq!(m.update_text, "Check for updates");
    }

    #[test]
    fn a_processor_losing_the_restarting_library_is_not_flagged() {
        // What the Mac run showed: the library exec'd into its update, the processor
        // here briefly could not reach it.
        let gap = || {
            status(
                r#"{"role":"processor","state":"disconnected","library":{"connected":false,"error":"the library closed the socket"}}"#,
            )
        };
        let just = (now() - chrono::Duration::seconds(5)).to_rfc3339();
        for server in [with_update("restarting", ""), with_update("updated", &just)] {
            let m = model(&[inst(server, true), inst(gap(), true)]);
            assert_eq!(m.icon, IconState::Idle, "{:?}", m.status_lines);
            assert!(
                m.status_lines.contains(
                    &"Processor: Reconnecting (the library is restarting for its update)"
                        .to_string()
                ),
                "{:?}",
                m.status_lines
            );
        }
        // In the restart grace the server may not answer at all.
        let gone = InstanceView {
            role: "server".into(),
            status: None,
            error: Some("connection refused".into()),
            tray_started: true,
        };
        let m = build(&Inputs {
            instances: &[gone, inst(gap(), true)],
            supervised: &[],
            update: &UpdateView::default(),
            updating: &[("server".to_string(), Some("0.7.1".to_string()))],
            notice: None,
            now: now(),
        });
        assert_eq!(m.icon, IconState::Idle, "{:?}", m.status_lines);
        // Long after the update, or with no update at all: a lost library is flagged.
        let old = (now() - chrono::Duration::minutes(5)).to_rfc3339();
        let m = model(&[inst(with_update("updated", &old), true), inst(gap(), true)]);
        assert_eq!(m.icon, IconState::Attention);
        let m = model(&[inst(gap(), true)]);
        assert_eq!(m.icon, IconState::Attention);
    }

    #[test]
    fn a_failed_update_says_it_will_retry_without_raising_attention() {
        let m = model(&[inst(with_update("failed", ""), false)]);
        assert_eq!(
            m.status_lines,
            ["Library: Idle", "⚠ Update to 0.7.1 failed — will retry"]
        );
        assert_eq!(m.icon, IconState::Idle);
    }

    #[test]
    fn blocked_updates_drive_attention_through_their_problem() {
        let mut s = with_update("blocked", "");
        s.problems = vec![crate::status::Problem {
            severity: "fail".into(),
            text: "The automatic update to 0.7.1 needs you: not enough disk space".into(),
            hint: None,
            kind: Some("update".into()),
        }];
        let m = model(&[inst(s, false)]);
        assert_eq!(m.icon, IconState::Attention);
        assert!(m.status_lines[1].starts_with("✖ The automatic update"));
    }

    #[test]
    fn the_restart_grace_hides_a_missing_instance_and_a_restarting_slot() {
        let updating = [("server".to_string(), Some("0.7.1".to_string()))];
        let gone = InstanceView {
            role: "server".into(),
            status: None,
            error: Some("connection refused".into()),
            tray_started: true,
        };
        let sup = SupervisedView {
            role: "server".into(),
            text: "stopped (exit code 75), restarting in 0 s".into(),
            failing: true,
        };
        let run = |instances: &[InstanceView], supervised: &[SupervisedView]| {
            build(&Inputs {
                instances,
                supervised,
                update: &UpdateView::default(),
                updating: &updating,
                notice: None,
                now: now(),
            })
        };
        let m = run(std::slice::from_ref(&gone), std::slice::from_ref(&sup));
        assert_eq!(m.status_lines, ["Library: Updating to 0.7.1…"]);
        assert_eq!(m.icon, IconState::Idle);
        // Not in discovery at all (the control file is being rewritten).
        let m = run(&[], &[sup]);
        assert_eq!(m.status_lines, ["Library: Updating to 0.7.1…"]);
        assert_eq!(m.icon, IconState::Idle);
        // Without the grace the same view is "not responding" + attention.
        let m = model(&[gone]);
        assert_eq!(m.status_lines, ["Library: not responding"]);
        assert_eq!(m.icon, IconState::Attention);
    }

    #[test]
    fn working_processor() {
        let mut s = status(CONTRACT_EXAMPLE);
        s.problems.clear();
        s.current[0].volume = "Dr Stone/Dr Stone 01".into();
        let m = model(&[inst(s, true)]);
        assert_eq!(
            m.status_lines,
            ["Processor: Working: Dr Stone 01 · 37/196 · 4.2 p/s"]
        );
        assert_eq!(m.icon, IconState::Working);
        assert!(m.can_pause_after && m.can_pause_now && !m.can_resume);
        assert_eq!(
            m.stats_lines,
            [
                "Today: 3 volumes, 512 pages",
                "Total: 41 volumes, 7,310 pages",
                "Rate: 252 pages/min",
                "GPU busy: 71%",
                "CPU: 6.5 cores busy",
                "Backend: torch-cu130-2.13.0",
                "Device: gpu:0 RTX 4090 sm_89",
                "Queue: 12 waiting",
            ]
        );
        assert_eq!(m.library_url.as_deref(), Some("https://lib.example"));
        assert_eq!(m.quit_text, "Quit");
    }

    #[test]
    fn paused_until_and_pausing() {
        let until = Local
            .with_ymd_and_hms(2026, 10, 4, 18, 0, 0)
            .unwrap()
            .to_rfc3339();
        let s = status(&format!(
            r#"{{"role":"processor","state":"paused","pause":{{"mode":"now","until":"{until}","reason":"user"}}}}"#
        ));
        let m = model(&[inst(s, false)]);
        assert_eq!(m.status_lines, ["Processor: Paused until 18:00"]);
        assert_eq!(m.icon, IconState::Paused);
        assert!(!m.can_pause_after && !m.can_pause_now && m.can_resume);
        assert_eq!(m.quit_text, "Quit (the processor keeps running)");
        let lib = status(r#"{"role":"server","state":"idle"}"#);
        let m = model(&[
            inst(lib, false),
            inst(status(r#"{"role":"processor","state":"idle"}"#), false),
        ]);
        assert_eq!(m.quit_text, "Quit (the library and processor keep running)");

        let s = status(
            r#"{"role":"server","state":"pausing","pause":{"mode":"after_volume"},"current":[{"volume":"a/b"}]}"#,
        );
        let m = model(&[inst(s, true)]);
        assert_eq!(m.status_lines, ["Library: Pausing after this volume"]);
        assert!(!m.can_pause_after && m.can_pause_now && m.can_resume);

        let tomorrow = tomorrow_at_8(now());
        assert!(tomorrow.starts_with("2026-10-05T08:00:00"), "{tomorrow}");
        let s = status(&format!(
            r#"{{"role":"processor","state":"paused","pause":{{"until":"{tomorrow}"}}}}"#
        ));
        assert_eq!(
            model(&[inst(s, true)]).status_lines,
            ["Processor: Paused until tomorrow 08:00"]
        );
    }

    #[test]
    fn idle_disconnected_and_problems() {
        let s = status(r#"{"role":"processor","state":"idle","library":{"queue_pending":0}}"#);
        let m = model(&[inst(s, true)]);
        assert_eq!(m.status_lines, ["Processor: Idle — 0 in queue"]);
        assert_eq!(m.icon, IconState::Idle);
        let s = status(r#"{"role":"processor","state":"disconnected"}"#);
        let m = model(&[inst(s, true)]);
        assert_eq!(m.status_lines, ["Processor: Can't reach library"]);
        assert_eq!(m.icon, IconState::Attention);
        let s = status(
            r#"{"role":"server","state":"idle","problems":[{"severity":"fail","text":"OCR backend failed to load"}]}"#,
        );
        let m = model(&[inst(s, true)]);
        assert_eq!(m.icon, IconState::Attention);
        assert_eq!(
            m.status_lines,
            ["Library: Idle", "✖ OCR backend failed to load"]
        );
        assert!(
            m.stats_lines
                .contains(&"✖ OCR backend failed to load".to_string())
        );
    }

    #[test]
    fn nothing_running_and_supervision() {
        let m = model(&[]);
        assert_eq!(m.status_lines, ["Not running"]);
        assert!(!m.can_pause_now && !m.can_resume && !m.can_open_dashboard);
        let m = build(&Inputs {
            instances: &[],
            supervised: &[SupervisedView {
                role: "processor".into(),
                text: "stopped (exit code 1), restarting in 8 s".into(),
                failing: true,
            }],
            update: &UpdateView {
                available: true,
                latest: Some("0.7.1".into()),
                ..Default::default()
            },
            updating: &[],
            notice: Some("pause failed"),
            now: now(),
        });
        assert_eq!(
            m.status_lines,
            [
                "Processor: stopped (exit code 1), restarting in 8 s",
                "⚠ pause failed"
            ]
        );
        assert_eq!(m.icon, IconState::Attention);
        assert_eq!(m.update_text, "Update available: 0.7.1 …");
    }

    #[test]
    fn two_instances_prefix_their_statistics() {
        let a =
            status(r#"{"role":"server","state":"idle","stats":{"today":{"volumes":1,"pages":1}}}"#);
        let b = status(r#"{"role":"processor","state":"working","current":[]}"#);
        let m = model(&[inst(a, true), inst(b, true)]);
        assert_eq!(m.status_lines, ["Library: Idle", "Processor: Working"]);
        assert_eq!(m.stats_lines, ["Library · Today: 1 volume, 1 page"]);
        assert_eq!(m.icon, IconState::Working);
    }

    #[test]
    fn number_grouping() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1,000");
        assert_eq!(group(1234567), "1,234,567");
    }
}
