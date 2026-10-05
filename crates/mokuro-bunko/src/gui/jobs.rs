//! Long-running CLI work started from the pages: this executable in a child process,
//! its output kept (bounded) and streamed to the page over Server-Sent Events, with
//! a progress figure read from the `NN% of ...` lines the CLI prints.

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::{Notify, watch};

/// Output lines kept per job (the oldest are dropped first).
const MAX_LINES: usize = 4000;
/// Finished jobs kept for the pages to read back.
const MAX_JOBS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Ok,
    Failed,
    Cancelled,
}

impl JobState {
    fn as_str(self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Ok => "ok",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }
}

struct Inner {
    lines: VecDeque<String>,
    /// Number of the first line in `lines` (earlier ones were dropped).
    first: u64,
    state: JobState,
    exit_code: Option<i32>,
    progress: Option<f64>,
    stage: String,
}

pub struct Job {
    pub id: u64,
    pub kind: String,
    pub title: String,
    /// The command line, for the page ("mokuro-bunko install-ocr --variant cpu").
    pub command: String,
    inner: Mutex<Inner>,
    /// Bumped on every change; the SSE streams wait on it.
    tick: watch::Sender<u64>,
    cancel: Notify,
}

impl Job {
    fn bump(&self) {
        self.tick.send_modify(|n| *n += 1);
    }

    fn push_line(&self, line: String) {
        {
            let mut g = self.inner.lock();
            let percent = parse_percent(&line);
            if let Some(p) = percent {
                g.progress = Some(p);
            }
            // A new top-level line is a new step: its own progress (indented
            // "  name: NN% of X MB" lines) starts over, unknown until it prints one.
            if !line.starts_with(char::is_whitespace) && !line.trim().is_empty() {
                g.stage = line.trim().to_string();
                if percent.is_none() {
                    g.progress = None;
                }
            }
            g.lines.push_back(line);
            if g.lines.len() > MAX_LINES {
                g.lines.pop_front();
                g.first += 1;
            }
        }
        self.bump();
    }

    fn finish(&self, state: JobState, code: Option<i32>) {
        {
            let mut g = self.inner.lock();
            g.state = state;
            g.exit_code = code;
            if state == JobState::Ok {
                g.progress = Some(100.0);
            }
        }
        self.bump();
    }

    pub fn state(&self) -> JobState {
        self.inner.lock().state
    }

    /// Everything about the job but its lines.
    pub fn summary(&self) -> Value {
        let g = self.inner.lock();
        json!({
            "id": self.id,
            "kind": self.kind,
            "title": self.title,
            "command": self.command,
            "state": g.state.as_str(),
            "exit_code": g.exit_code,
            "progress": g.progress,
            "stage": g.stage,
            "lines": g.first + g.lines.len() as u64,
        })
    }

    /// Lines from number `from` on, and the number after the last.
    pub fn lines_from(&self, from: u64) -> (Vec<(u64, String)>, u64) {
        let g = self.inner.lock();
        let start = from.max(g.first);
        let out: Vec<(u64, String)> = g
            .lines
            .iter()
            .enumerate()
            .map(|(i, l)| (g.first + i as u64, l.clone()))
            .filter(|(n, _)| *n >= start)
            .collect();
        (out, g.first + g.lines.len() as u64)
    }

    pub fn output(&self) -> String {
        let g = self.inner.lock();
        g.lines.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    pub fn cancel(&self) {
        self.cancel.notify_one();
    }

    /// Wait until the job ends.
    #[cfg(test)]
    pub async fn wait(&self) -> JobState {
        let mut rx = self.tick.subscribe();
        loop {
            let s = self.state();
            if s != JobState::Running {
                return s;
            }
            if rx.changed().await.is_err() {
                return self.state();
            }
        }
    }
}

/// `"  name: 25% of 900 MB"` → 25.
pub fn parse_percent(line: &str) -> Option<f64> {
    let at = line.find("% of ")?;
    let digits: String = line[..at]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    digits
        .parse::<f64>()
        .ok()
        .filter(|p| (0.0..=100.0).contains(p))
}

#[derive(Default, Clone)]
pub struct Jobs {
    list: Arc<Mutex<VecDeque<Arc<Job>>>>,
    next: Arc<AtomicU64>,
}

impl Jobs {
    pub fn get(&self, id: u64) -> Option<Arc<Job>> {
        self.list.lock().iter().find(|j| j.id == id).cloned()
    }

    pub fn all(&self) -> Vec<Arc<Job>> {
        self.list.lock().iter().cloned().collect()
    }

    /// A running job of this kind, if any (one install at a time).
    pub fn running(&self, kind: &str) -> Option<Arc<Job>> {
        self.list
            .lock()
            .iter()
            .find(|j| j.kind == kind && j.state() == JobState::Running)
            .cloned()
    }

    /// Run `exe args...` (env added) as a job.
    pub fn start(
        &self,
        kind: &str,
        title: &str,
        exe: &Path,
        args: Vec<String>,
        envs: Vec<(String, String)>,
    ) -> Arc<Job> {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let shown = std::iter::once("mokuro-bunko".to_string())
            .chain(args.iter().map(|a| {
                if a.contains(' ') {
                    format!("\"{a}\"")
                } else {
                    a.clone()
                }
            }))
            .collect::<Vec<_>>()
            .join(" ");
        let job = Arc::new(Job {
            id,
            kind: kind.to_string(),
            title: title.to_string(),
            command: shown,
            inner: Mutex::new(Inner {
                lines: VecDeque::new(),
                first: 0,
                state: JobState::Running,
                exit_code: None,
                progress: None,
                stage: String::new(),
            }),
            tick: watch::channel(0).0,
            cancel: Notify::new(),
        });
        {
            let mut list = self.list.lock();
            list.push_back(job.clone());
            while list.len() > MAX_JOBS {
                let Some(pos) = list.iter().position(|j| j.state() != JobState::Running) else {
                    break;
                };
                list.remove(pos);
            }
        }
        let mut cmd = tokio::process::Command::new(exe);
        cmd.args(&args)
            .envs(envs)
            // Plain output (no colours); fine-grained download progress for the bar.
            .env("NO_COLOR", "1")
            .env("MOKURO_PROGRESS_STEP", "2")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let j = job.clone();
        tokio::spawn(async move {
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    j.push_line(format!("Could not start {}: {e}", j.command));
                    j.finish(JobState::Failed, None);
                    return;
                }
            };
            let out = child.stdout.take().map(|s| pump(j.clone(), s));
            let err = child.stderr.take().map(|s| pump(j.clone(), s));
            let status = tokio::select! {
                s = child.wait() => s.ok(),
                _ = j.cancel.notified() => {
                    let _ = child.kill().await;
                    None
                }
            };
            for h in [out, err].into_iter().flatten() {
                let _ = h.await;
            }
            match status {
                Some(s) if s.success() => j.finish(JobState::Ok, s.code()),
                Some(s) => j.finish(JobState::Failed, s.code()),
                None => {
                    j.push_line("Cancelled.".into());
                    j.finish(JobState::Cancelled, None);
                }
            }
        });
        job
    }
}

fn pump<R: AsyncRead + Unpin + Send + 'static>(
    job: Arc<Job>,
    stream: R,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            job.push_line(line);
        }
    })
}

/// The job's events from line `from` on: `line` {n, text}, `progress` {progress,
/// stage} after each batch, and `done` (the summary) at the end.
pub fn events(
    job: Arc<Job>,
    from: u64,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    struct S {
        job: Arc<Job>,
        rx: watch::Receiver<u64>,
        cursor: u64,
        pending: VecDeque<Event>,
        done: bool,
    }
    let rx = job.tick.subscribe();
    let init = S {
        job,
        rx,
        cursor: from,
        pending: VecDeque::new(),
        done: false,
    };
    let stream =
        futures_util::stream::unfold(init, |mut s| async move {
            loop {
                if let Some(ev) = s.pending.pop_front() {
                    return Some((Ok(ev), s));
                }
                if s.done {
                    return None;
                }
                let running = s.job.state() == JobState::Running;
                let (lines, next) = s.job.lines_from(s.cursor);
                let fresh = !lines.is_empty();
                for (n, text) in lines {
                    s.pending.push_back(
                        Event::default()
                            .event("line")
                            .data(json!({"n": n, "text": text}).to_string()),
                    );
                }
                s.cursor = next;
                if fresh {
                    let sum = s.job.summary();
                    s.pending.push_back(Event::default().event("progress").data(
                        json!({"progress": sum["progress"], "stage": sum["stage"]}).to_string(),
                    ));
                }
                if !running {
                    s.pending.push_back(
                        Event::default()
                            .event("done")
                            .data(s.job.summary().to_string()),
                    );
                    s.done = true;
                    continue;
                }
                if !fresh && s.rx.changed().await.is_err() {
                    // The job is gone: report what there is.
                    s.done = true;
                }
            }
        });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_lines() {
        assert_eq!(
            parse_percent("  mokuro-bunko-x.tar.zst [1/1]: 25% of 900 MB"),
            Some(25.0)
        );
        assert_eq!(
            parse_percent("  nvidia-cublas 13 (NVIDIA): 100% of 1 MB"),
            Some(100.0)
        );
        assert_eq!(parse_percent("Unpacking and verifying..."), None);
        assert_eq!(parse_percent("200% of x"), None);
    }

    #[tokio::test]
    async fn runs_streams_and_finishes() {
        let jobs = Jobs::default();
        let sh = Path::new("/bin/sh");
        if !sh.exists() {
            return;
        }
        let job = jobs.start(
            "t",
            "test",
            sh,
            vec![
                "-c".into(),
                "echo Stage one; echo '  x: 50% of 1 MB'; echo '  detail'; exit 3".into(),
            ],
            vec![],
        );
        assert_eq!(job.wait().await, JobState::Failed);
        let sum = job.summary();
        assert_eq!(sum["exit_code"], 3);
        assert_eq!(sum["progress"], 50.0);
        assert_eq!(sum["stage"], "Stage one");
        assert!(job.output().contains("Stage one"));
        let (lines, next) = job.lines_from(1);
        assert_eq!(next, 3);
        assert_eq!(lines.len(), 2);
        // A new step starts its progress over; stderr is captured too.
        let two = jobs.start(
            "t",
            "two",
            sh,
            vec![
                "-c".into(),
                "echo '  x: 100% of 1 MB'; echo Step two; echo oops >&2; exit 2".into(),
            ],
            vec![],
        );
        assert_eq!(two.wait().await, JobState::Failed);
        assert!(two.output().contains("oops"));
        let sum = two.summary();
        assert!(sum["progress"].is_null(), "{sum}");
        let ok = jobs.start("t", "ok", sh, vec!["-c".into(), "true".into()], vec![]);
        assert_eq!(ok.wait().await, JobState::Ok);
        let slow = jobs.start(
            "t",
            "slow",
            sh,
            vec!["-c".into(), "sleep 30".into()],
            vec![],
        );
        assert!(jobs.running("t").is_some());
        slow.cancel();
        assert_eq!(slow.wait().await, JobState::Cancelled);
    }
}
