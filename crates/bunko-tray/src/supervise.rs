//! Tray-managed running (GUI.md §5): start `serve` / `processor serve` as children when
//! `tray.json` asks for it and no instance of that role is up; restart a crashed child
//! with exponential backoff; never start one next to a service-managed instance (a
//! systemd unit, a launchd agent, or any instance whose `.control.json` answers).

use crate::model::SupervisedView;
use crate::trayconf::Managed;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `mokuro-bunko` asks its launcher to start it again (after an in-place update).
pub const RESTART_EXIT_CODE: i32 = 75;
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A child that ran this long counts as healthy: the next crash restarts after 1 s.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotState {
    /// Not decided yet (waiting for the first discovery pass).
    Waiting,
    /// Another instance of this role is up (or its service is active): not ours to run.
    External(String),
    Running {
        pid: u32,
    },
    Backoff {
        exit: String,
        failures: u32,
        until: Instant,
    },
    NoExecutable,
    Stopped,
}

/// The wait before restart number `failures` (1-based) of a child that ran `uptime`.
pub fn backoff(failures: u32, uptime: Duration) -> Duration {
    if uptime >= HEALTHY_AFTER || failures <= 1 {
        return Duration::from_secs(1);
    }
    let secs = 1u64 << (failures - 1).min(6);
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

/// What the supervisor needs from the rest of the tray.
pub trait World: Send + Sync {
    /// The pid of a live, answering instance of `role`, if there is one.
    fn live_instance(&self, role: &str) -> Option<u32>;
    /// Whether discovery has completed at least one pass.
    fn discovered(&self) -> bool;
    /// A system service that runs (or is starting) `role`, by name.
    fn service_active(&self, role: &str) -> Option<String>;
}

pub struct Slot {
    pub managed: Managed,
    args: Vec<String>,
    state: Mutex<SlotState>,
    child: Mutex<Option<Child>>,
}

impl Slot {
    pub fn state(&self) -> SlotState {
        lock(&self.state).clone()
    }

    fn set(&self, s: SlotState) {
        *lock(&self.state) = s;
    }

    pub fn child_pid(&self) -> Option<u32> {
        lock(&self.child).as_ref().map(Child::id)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A poisoned lock only means a supervisor thread panicked; the data is still usable.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Supervisor {
    pub slots: Vec<Arc<Slot>>,
    quitting: Arc<AtomicBool>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

pub struct Launch {
    pub cli: Option<PathBuf>,
    pub env: Vec<(String, OsString)>,
    pub log_dir: PathBuf,
}

impl Supervisor {
    pub fn start(
        managed: &[Managed],
        args_for: impl Fn(&Managed) -> Vec<String>,
        launch: Launch,
        world: Arc<dyn World>,
        on_change: Arc<dyn Fn() + Send + Sync>,
    ) -> Supervisor {
        let quitting = Arc::new(AtomicBool::new(false));
        let launch = Arc::new(launch);
        let mut slots = Vec::new();
        let mut threads = Vec::new();
        for m in managed {
            let slot = Arc::new(Slot {
                managed: m.clone(),
                args: args_for(m),
                state: Mutex::new(SlotState::Waiting),
                child: Mutex::new(None),
            });
            slots.push(slot.clone());
            let (q, w, l, c) = (
                quitting.clone(),
                world.clone(),
                launch.clone(),
                on_change.clone(),
            );
            threads.push(std::thread::spawn(move || {
                run_slot(&slot, &q, &*w, &l, &*c)
            }));
        }
        Supervisor {
            slots,
            quitting,
            threads: Mutex::new(threads),
        }
    }

    pub fn is_child(&self, pid: u32) -> bool {
        self.slots.iter().any(|s| s.child_pid() == Some(pid))
    }

    /// Menu lines for slots whose instance is not (yet) visible.
    pub fn views(&self, visible_roles: &[&str]) -> Vec<SupervisedView> {
        let now = Instant::now();
        self.slots
            .iter()
            .filter_map(|s| {
                let role = s.managed.role.clone();
                let (text, failing) = match s.state() {
                    SlotState::Waiting => ("starting…".to_string(), false),
                    SlotState::Running { .. } if !visible_roles.contains(&role.as_str()) => {
                        ("starting…".to_string(), false)
                    }
                    SlotState::Backoff {
                        exit,
                        failures,
                        until,
                    } => {
                        let wait = until.saturating_duration_since(now).as_secs();
                        (
                            format!("stopped ({exit}), restarting in {wait} s"),
                            failures >= 2,
                        )
                    }
                    SlotState::NoExecutable => (
                        "cannot start: the mokuro-bunko command line was not found".to_string(),
                        true,
                    ),
                    _ => return None,
                };
                Some(SupervisedView {
                    role,
                    text,
                    failing,
                })
            })
            .collect()
    }

    /// Stop every child this tray started: ask it through the control API (`stop`
    /// returns true when that request was accepted), else SIGTERM, then kill after
    /// `grace`.
    pub fn stop_all(&self, stop: impl Fn(u32) -> bool, grace: Duration) {
        self.quitting.store(true, Ordering::SeqCst);
        for slot in &self.slots {
            let mut guard = lock(&slot.child);
            let Some(child) = guard.as_mut() else {
                continue;
            };
            let pid = child.id();
            if !stop(pid) {
                terminate(child);
            }
            let deadline = Instant::now() + grace;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        tracing::info!("{} (pid {pid}) stopped: {status}", slot.managed.role);
                        break;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    _ => {
                        tracing::warn!(
                            "{} (pid {pid}) did not stop in time; killing it",
                            slot.managed.role
                        );
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
            *guard = None;
            slot.set(SlotState::Stopped);
        }
        for t in lock(&self.threads).drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(unix)]
fn terminate(child: &mut Child) {
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: plain kill(2) on our own child's pid.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
}

#[cfg(not(unix))]
fn terminate(child: &mut Child) {
    // No SIGTERM on Windows: the control API's stop is the clean way; this is the fallback.
    let _ = child.kill();
}

/// Sleep in small steps so Quit is not held up.
fn nap(quitting: &AtomicBool, d: Duration) {
    let end = Instant::now() + d;
    while !quitting.load(Ordering::SeqCst) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn run_slot(
    slot: &Slot,
    quitting: &AtomicBool,
    world: &dyn World,
    launch: &Launch,
    changed: &dyn Fn(),
) {
    let role = slot.managed.role.as_str();
    while !world.discovered() && !quitting.load(Ordering::SeqCst) {
        nap(quitting, Duration::from_millis(300));
    }
    let mut failures = 0u32;
    while !quitting.load(Ordering::SeqCst) {
        if let Some(desc) = world.service_active(role) {
            if slot.state() != SlotState::External(desc.clone()) {
                tracing::info!("{role}: {desc} is active; not starting a second one");
                slot.set(SlotState::External(desc));
                changed();
            }
            nap(quitting, Duration::from_secs(5));
            continue;
        }
        if let Some(pid) = world.live_instance(role) {
            let desc = format!("pid {pid}");
            if slot.state() != SlotState::External(desc.clone()) {
                tracing::info!("{role}: already running (pid {pid}); not starting another");
                slot.set(SlotState::External(desc));
                changed();
            }
            nap(quitting, Duration::from_secs(3));
            continue;
        }
        let Some(cli) = launch.cli.as_deref() else {
            if slot.state() != SlotState::NoExecutable {
                tracing::error!("{role}: no mokuro-bunko executable to start");
                slot.set(SlotState::NoExecutable);
                changed();
            }
            nap(quitting, Duration::from_secs(10));
            continue;
        };
        let started = Instant::now();
        let child = match spawn(cli, &slot.args, &launch.env, &launch.log_dir, role) {
            Ok(c) => c,
            Err(e) => {
                failures += 1;
                let wait = backoff(failures, Duration::ZERO);
                tracing::error!("{role}: could not start {}: {e}", cli.display());
                slot.set(SlotState::Backoff {
                    exit: format!("could not start: {e}"),
                    failures,
                    until: Instant::now() + wait,
                });
                changed();
                nap(quitting, wait);
                continue;
            }
        };
        let pid = child.id();
        tracing::info!(
            "{role}: started pid {pid}: {} {}",
            cli.display(),
            slot.args.join(" ")
        );
        *lock(&slot.child) = Some(child);
        slot.set(SlotState::Running { pid });
        changed();

        let exit = loop {
            if quitting.load(Ordering::SeqCst) {
                return; // stop_all owns the child now
            }
            let mut guard = lock(&slot.child);
            let Some(c) = guard.as_mut() else { return };
            match c.try_wait() {
                Ok(Some(status)) => {
                    *guard = None;
                    break status;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("{role}: waiting for pid {pid}: {e}");
                }
            }
            drop(guard);
            std::thread::sleep(Duration::from_millis(500));
        };
        if quitting.load(Ordering::SeqCst) {
            return;
        }
        let uptime = started.elapsed();
        if exit.code() == Some(RESTART_EXIT_CODE) {
            tracing::info!("{role}: pid {pid} asked to be restarted (update installed)");
            failures = 0;
            continue;
        }
        failures = if uptime >= HEALTHY_AFTER {
            1
        } else {
            failures + 1
        };
        let wait = backoff(failures, uptime);
        let text = match exit.code() {
            Some(c) => format!("exit code {c}"),
            None => "killed".to_string(),
        };
        tracing::warn!(
            "{role}: pid {pid} exited ({text}) after {:.0} s; restarting in {} s",
            uptime.as_secs_f64(),
            wait.as_secs()
        );
        slot.set(SlotState::Backoff {
            exit: text,
            failures,
            until: Instant::now() + wait,
        });
        changed();
        nap(quitting, wait);
    }
}

fn spawn(
    cli: &Path,
    args: &[String],
    env: &[(String, OsString)],
    log_dir: &Path,
    role: &str,
) -> std::io::Result<Child> {
    // stdout/stderr: the instance logs to its own files; this catches what happens
    // before its logging starts (a bad config, a panic).
    std::fs::create_dir_all(log_dir)?;
    let console = log_dir.join(format!("tray-{role}-console.log"));
    if std::fs::metadata(&console).is_ok_and(|m| m.len() > 4 << 20) {
        let _ = std::fs::rename(&console, log_dir.join(format!("tray-{role}-console.log.1")));
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&console)?;
    let err = out.try_clone()?;
    let mut cmd = Command::new(cli);
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        // bunko-control: an instance the tray started accepts `POST /control/stop`.
        .env("MOKURO_CONTROL_MANAGED", "1")
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    if let Some(dir) = cli.parent() {
        cmd.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // If the tray dies without stopping it (a crash, a session ending without
        // KillUserProcesses), the instance gets a SIGTERM and shuts down cleanly rather
        // than running on unowned. This fires when the spawning thread ends: the slot's
        // thread, which lives until `stop_all`.
        // SAFETY: prctl is async-signal-safe; nothing else runs between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    cmd.spawn()
}

/// Whether a system service runs `role` right now (or is starting it).
pub fn service_active(role: &str) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let unit = match role {
            "processor" => "mokuro-bunko-processor.service",
            _ => "mokuro-bunko.service",
        };
        for (scope, args) in [
            ("systemd user unit", vec!["--user", "is-active", unit]),
            ("systemd unit", vec!["is-active", unit]),
        ] {
            let out = Command::new("systemctl")
                .args(&args)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if matches!(state.as_str(), "active" | "activating" | "reloading") {
                return Some(format!("{scope} {unit}"));
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: getuid has no failure mode.
        let uid = unsafe { libc::getuid() };
        let label = match role {
            "processor" => crate::discover::PROCESSOR_LAUNCHD_LABEL,
            _ => crate::discover::SERVER_LAUNCHD_LABEL,
        };
        let out = Command::new("launchctl")
            .args(["print", &format!("gui/{uid}/{label}")])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        (out.status.success() && text.contains("state = running"))
            .then(|| format!("launchd agent {label}"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = role;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_resets() {
        let quick = Duration::from_secs(2);
        let seq: Vec<u64> = (1..=9).map(|f| backoff(f, quick).as_secs()).collect();
        assert_eq!(seq, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(backoff(7, Duration::from_secs(120)), Duration::from_secs(1));
    }

    struct FakeWorld {
        live: Mutex<Option<u32>>,
        service: Option<String>,
    }

    impl World for FakeWorld {
        fn live_instance(&self, _: &str) -> Option<u32> {
            *lock(&self.live)
        }
        fn discovered(&self) -> bool {
            true
        }
        fn service_active(&self, _: &str) -> Option<String> {
            self.service.clone()
        }
    }

    fn wait_for(cond: impl Fn() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// A stand-in for mokuro-bunko: a shell script (Unix only).
    #[cfg(unix)]
    fn fake_cli(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("mokuro-bunko");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn managed(role: &str) -> Managed {
        Managed {
            role: role.into(),
            args: vec![],
        }
    }

    #[cfg(unix)]
    #[test]
    fn restarts_a_crashing_child_with_backoff_and_stops_on_quit() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        // Crash twice, then stay up (until SIGTERM).
        let cli = fake_cli(
            dir.path(),
            &format!(
                "echo run >> '{c}'\nn=$(wc -l < '{c}')\n[ \"$n\" -le 2 ] && exit 3\nexec sleep 30",
                c = count.display()
            ),
        );
        let world = Arc::new(FakeWorld {
            live: Mutex::new(None),
            service: None,
        });
        let sup = Supervisor::start(
            &[managed("processor")],
            |_| vec!["processor".into(), "serve".into()],
            Launch {
                cli: Some(cli),
                env: vec![("MOKURO_LAUNCHER".into(), "tray".into())],
                log_dir: dir.path().join("logs"),
            },
            world,
            Arc::new(|| {}),
        );
        let runs = || {
            std::fs::read_to_string(&count)
                .map(|t| t.lines().count())
                .unwrap_or(0)
        };
        assert!(wait_for(|| runs() >= 3), "runs: {}", runs());
        let slot = sup.slots[0].clone();
        assert!(wait_for(|| matches!(
            slot.state(),
            SlotState::Running { .. }
        )));
        let pid = slot.child_pid().unwrap();
        assert!(sup.is_child(pid));
        // Third start came after backoffs of 1 s and 2 s.
        let t = Instant::now();
        sup.stop_all(|_| false, Duration::from_secs(5));
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "SIGTERM should end it"
        );
        assert_eq!(slot.state(), SlotState::Stopped);
        assert_eq!(runs(), 3);
        assert!(dir.path().join("logs/tray-processor-console.log").exists());
    }

    #[cfg(unix)]
    #[test]
    fn exit_75_restarts_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let cli = fake_cli(
            dir.path(),
            &format!(
                "echo run >> '{c}'\nn=$(wc -l < '{c}')\n[ \"$n\" -le 3 ] && exit 75\nexec sleep 30",
                c = count.display()
            ),
        );
        let world = Arc::new(FakeWorld {
            live: Mutex::new(None),
            service: None,
        });
        let t = Instant::now();
        let sup = Supervisor::start(
            &[managed("server")],
            |_| vec!["serve".into()],
            Launch {
                cli: Some(cli),
                env: vec![],
                log_dir: dir.path().join("logs"),
            },
            world,
            Arc::new(|| {}),
        );
        let slot = sup.slots[0].clone();
        assert!(wait_for(|| matches!(
            slot.state(),
            SlotState::Running { .. }
        ) && std::fs::read_to_string(&count)
            .map(|t| t.lines().count())
            .unwrap_or(0)
            == 4));
        assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
        sup.stop_all(|_| false, Duration::from_secs(5));
    }

    #[test]
    fn never_starts_next_to_a_live_or_service_instance() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        // Any start would create the marker; the cli path does not even need to exist
        // for the test to fail loudly if a start is attempted.
        for (live, service) in [
            (Some(4242), None),
            (None, Some("systemd user unit x".to_string())),
        ] {
            let world = Arc::new(FakeWorld {
                live: Mutex::new(live),
                service: service.clone(),
            });
            let sup = Supervisor::start(
                &[managed("processor")],
                |_| vec![],
                Launch {
                    cli: Some(marker.clone()),
                    env: vec![],
                    log_dir: dir.path().join("logs"),
                },
                world,
                Arc::new(|| {}),
            );
            let slot = sup.slots[0].clone();
            assert!(wait_for(|| matches!(slot.state(), SlotState::External(_))));
            let views = sup.views(&[]);
            assert!(views.is_empty(), "{views:?}");
            sup.stop_all(|_| false, Duration::from_secs(1));
            assert!(!marker.exists());
        }
    }

    #[test]
    fn missing_executable_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let world = Arc::new(FakeWorld {
            live: Mutex::new(None),
            service: None,
        });
        let sup = Supervisor::start(
            &[managed("processor")],
            |_| vec![],
            Launch {
                cli: None,
                env: vec![],
                log_dir: dir.path().join("logs"),
            },
            world,
            Arc::new(|| {}),
        );
        let slot = sup.slots[0].clone();
        assert!(wait_for(|| slot.state() == SlotState::NoExecutable));
        let v = sup.views(&[]);
        assert!(v[0].failing && v[0].text.contains("not found"));
        sup.stop_all(|_| false, Duration::from_secs(1));
    }
}
