//! Starting short-lived helpers (`mokuro-bunko gui`, the browser opener) without
//! failing silently: their stderr is kept, and one that exits with an error within a
//! few seconds is reported with its last stderr line, so a menu click that cannot
//! work says why instead of doing nothing.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A helper that exited with an error soon after it started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EarlyExit {
    /// `exit code 1`, `killed`.
    pub status: String,
    /// The last non-empty stderr line (`Error: ` prefix dropped), if it printed one.
    pub last_line: Option<String>,
}

impl EarlyExit {
    /// "Permission denied (os error 13)", else "exit code 1".
    pub fn reason(&self) -> String {
        self.last_line
            .clone()
            .unwrap_or_else(|| self.status.clone())
    }
}

/// How long after the start an error exit counts as "it did not come up".
pub const EARLY: Duration = Duration::from_secs(10);

/// Spawn `cmd` (stdin closed, stdout discarded, stderr kept), reap it in the
/// background, and call `on_early_exit` if it exits unsuccessfully within `window`.
/// Returns the child's pid.
pub fn spawn_watched(
    mut cmd: Command,
    window: Duration,
    on_early_exit: impl FnOnce(EarlyExit) + Send + 'static,
) -> std::io::Result<u32> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let started = Instant::now();
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let reader = child.stderr.take().map(|err| {
        let lines = lines.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                tracing::debug!("pid {pid}: {line}");
                let mut l = lines.lock().unwrap_or_else(|e| e.into_inner());
                l.push(line);
                if l.len() > 20 {
                    l.remove(0);
                }
            }
        })
    });
    std::thread::spawn(move || {
        let status = child.wait();
        if let Some(r) = reader {
            let _ = r.join();
        }
        let Ok(status) = status else { return };
        if status.success() || started.elapsed() > window {
            return;
        }
        let last_line = lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last()
            .map(|l| l.strip_prefix("Error: ").unwrap_or(l).to_string());
        on_early_exit(EarlyExit {
            status: match status.code() {
                Some(c) => format!("exit code {c}"),
                None => "killed".into(),
            },
            last_line,
        });
    });
    Ok(pid)
}

/// A desktop notification for a failure the user caused by clicking (macOS only:
/// `osascript`, no extra dependency); at most one every 30 s.
pub fn notify(title: &str, text: &str) {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(30)) {
            return;
        }
        *last = Some(Instant::now());
    }
    if cfg!(target_os = "macos") {
        let q = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            q(text),
            q(title)
        );
        let _ = Command::new("osascript")
            .args(["-e", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|mut c| std::thread::spawn(move || c.wait()));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn an_early_failure_is_reported_with_its_last_stderr_line() {
        let (tx, rx) = mpsc::channel();
        spawn_watched(
            sh("echo starting >&2; echo 'Error: Permission denied (os error 13)' >&2; exit 1"),
            EARLY,
            move |e| tx.send(e).unwrap(),
        )
        .unwrap();
        let e = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(e.status, "exit code 1");
        assert_eq!(e.reason(), "Permission denied (os error 13)");
    }

    #[test]
    fn success_and_late_failures_are_quiet() {
        let (tx, rx) = mpsc::channel::<EarlyExit>();
        let tx2 = tx.clone();
        spawn_watched(sh("echo fine >&2; exit 0"), EARLY, move |e| {
            tx.send(e).unwrap()
        })
        .unwrap();
        // A window already over: the failure is not "early".
        spawn_watched(
            sh("sleep 0.3; exit 3"),
            Duration::from_millis(50),
            move |e| tx2.send(e).unwrap(),
        )
        .unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_err());
    }

    #[test]
    fn no_stderr_names_the_exit_code() {
        let (tx, rx) = mpsc::channel();
        spawn_watched(sh("exit 2"), EARLY, move |e| tx.send(e).unwrap()).unwrap();
        let e = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(e.reason(), "exit code 2");
    }
}
