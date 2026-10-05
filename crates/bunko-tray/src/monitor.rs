//! Keeps the tray's picture of the running instances current: rescans the candidate
//! storages for `.control.json` every few seconds, follows each instance's
//! `/control/events` stream, and polls `/control/status` every 2 s when the stream is
//! not available (GUI.md §2's fallback).

use crate::client::Client;
use crate::discover;
use crate::status::{ControlFile, Status};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RESCAN: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_secs(2);
/// In polling mode, try the event stream again this often.
const RETRY_STREAM: Duration = Duration::from_secs(30);
/// Consecutive failed status requests after which an instance counts as gone.
const GONE_AFTER: u32 = 3;

#[derive(Debug, Clone)]
pub struct Live {
    pub storage: PathBuf,
    pub control: ControlFile,
    pub status: Option<Status>,
    pub error: Option<String>,
}

struct Entry {
    live: Live,
    stop: Arc<AtomicBool>,
    gone: bool,
}

type Candidates = dyn Fn() -> Vec<PathBuf> + Send + Sync;

pub struct Monitor {
    entries: Mutex<Vec<Entry>>,
    discovered: AtomicBool,
    quit: AtomicBool,
    candidates: Box<Candidates>,
    on_change: Arc<dyn Fn() + Send + Sync>,
    notice: Mutex<Option<(String, Instant)>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Monitor {
    pub fn start(
        candidates: Box<Candidates>,
        on_change: Arc<dyn Fn() + Send + Sync>,
    ) -> Arc<Monitor> {
        let m = Arc::new(Monitor {
            entries: Mutex::new(Vec::new()),
            discovered: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            candidates,
            on_change,
            notice: Mutex::new(None),
        });
        let me = m.clone();
        std::thread::spawn(move || {
            while !me.quit.load(Ordering::SeqCst) {
                me.rescan();
                if !me.discovered.swap(true, Ordering::SeqCst) {
                    (me.on_change)();
                }
                let end = Instant::now() + RESCAN;
                while Instant::now() < end && !me.quit.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        });
        m
    }

    pub fn discovered(&self) -> bool {
        self.discovered.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        self.quit.store(true, Ordering::SeqCst);
        for e in lock(&self.entries).iter() {
            e.stop.store(true, Ordering::SeqCst);
        }
    }

    pub fn instances(&self) -> Vec<Live> {
        lock(&self.entries)
            .iter()
            .filter(|e| !e.gone)
            .map(|e| e.live.clone())
            .collect()
    }

    /// The pid of an answering instance of `role`.
    pub fn live_pid(&self, role: &str) -> Option<u32> {
        lock(&self.entries)
            .iter()
            .find(|e| !e.gone && e.live.control.role == role && e.live.status.is_some())
            .map(|e| e.live.control.pid)
            // An instance that just died still has its last status here for a few seconds.
            .filter(|pid| crate::discover::pid_alive(*pid) != Some(false))
    }

    pub fn client_for_pid(&self, pid: u32) -> Option<Client> {
        lock(&self.entries)
            .iter()
            .find(|e| !e.gone && e.live.control.pid == pid)
            .map(|e| Client::new(&e.live.control))
    }

    /// Record a status that an action (pause/resume) returned.
    pub fn record(&self, pid: u32, status: Status) {
        if let Some(e) = lock(&self.entries)
            .iter_mut()
            .find(|e| e.live.control.pid == pid)
        {
            e.live.status = Some(status);
            e.live.error = None;
        }
        (self.on_change)();
    }

    /// A short message for the menu (an action that failed), shown for a minute.
    pub fn set_notice(&self, text: String) {
        *lock(&self.notice) = Some((text, Instant::now()));
        (self.on_change)();
    }

    pub fn notice(&self) -> Option<String> {
        let mut n = lock(&self.notice);
        match &*n {
            Some((t, at)) if at.elapsed() < Duration::from_secs(60) => Some(t.clone()),
            Some(_) => {
                *n = None;
                None
            }
            None => None,
        }
    }

    fn rescan(self: &Arc<Self>) {
        let found = discover::found(&(self.candidates)());
        let mut changed = false;
        {
            let mut entries = lock(&self.entries);
            entries.retain(|e| {
                let keep = !e.gone
                    && found
                        .iter()
                        .any(|(s, c)| *s == e.live.storage && *c == e.live.control);
                if !keep {
                    e.stop.store(true, Ordering::SeqCst);
                    changed = true;
                }
                keep
            });
        }
        for (storage, control) in found {
            let known = lock(&self.entries)
                .iter()
                .any(|e| e.live.storage == storage && e.live.control == control);
            if known {
                continue;
            }
            // Probe before showing it: a file left behind by a crash (Windows has no
            // pid check here) answers nothing.
            let client = Client::new(&control);
            let status = match client.status() {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!("{}: {e}", storage.display());
                    continue;
                }
            };
            tracing::info!(
                "found {} {} (pid {}, port {}) in {}",
                control.role,
                control.version,
                control.pid,
                control.port,
                storage.display()
            );
            let stop = Arc::new(AtomicBool::new(false));
            lock(&self.entries).push(Entry {
                live: Live {
                    storage: storage.clone(),
                    control: control.clone(),
                    status: Some(status),
                    error: None,
                },
                stop: stop.clone(),
                gone: false,
            });
            changed = true;
            let me = self.clone();
            std::thread::spawn(move || me.watch(storage, control, client, stop));
        }
        if changed {
            (self.on_change)();
        }
    }

    fn update(&self, storage: &PathBuf, control: &ControlFile, f: impl FnOnce(&mut Entry)) {
        if let Some(e) = lock(&self.entries)
            .iter_mut()
            .find(|e| e.live.storage == *storage && e.live.control == *control)
        {
            f(e);
        }
        (self.on_change)();
    }

    fn watch(&self, storage: PathBuf, control: ControlFile, client: Client, stop: Arc<AtomicBool>) {
        let mut failures = 0u32;
        while !stop.load(Ordering::SeqCst) {
            // Event stream first.
            let started = Instant::now();
            let res = client.follow_events(|s| {
                if stop.load(Ordering::SeqCst) {
                    return false;
                }
                self.update(&storage, &control, |e| {
                    e.live.status = Some(s);
                    e.live.error = None;
                });
                true
            });
            if stop.load(Ordering::SeqCst) {
                return;
            }
            if let Err(e) = &res {
                tracing::debug!("{}: events: {e}; polling", control.role);
            }
            if started.elapsed() > Duration::from_secs(5) {
                failures = 0; // the stream worked for a while: retry it at once
            }
            // Poll until it is time to try the stream again.
            let until = Instant::now() + RETRY_STREAM;
            while Instant::now() < until && !stop.load(Ordering::SeqCst) {
                match client.status() {
                    Ok(s) => {
                        failures = 0;
                        self.update(&storage, &control, |e| {
                            e.live.status = Some(s);
                            e.live.error = None;
                        });
                        if res.is_ok() {
                            break; // the stream ended cleanly: reconnect
                        }
                    }
                    Err(err) => {
                        failures += 1;
                        let gone = failures >= GONE_AFTER;
                        tracing::warn!("{} (pid {}): {err}", control.role, control.pid);
                        self.update(&storage, &control, |e| {
                            e.live.error = Some(err.to_string());
                            e.gone = gone;
                        });
                        if gone {
                            return;
                        }
                    }
                }
                std::thread::sleep(POLL);
            }
        }
    }
}
