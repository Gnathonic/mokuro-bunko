//! The filesystem watcher: only a wake-up hint for the change feed, never a source of
//! changes (the snapshot diff is). It counts relevant events; `wait_change` returns
//! once the count passed the cursor's walk and the folder has been quiet for
//! [`QUIET`] (at most [`CAP`] after it noticed), or at the caller's `max`.
//!
//! Reads (open, access) are not events, nor is anything under Bunko's own internal
//! names. A watch that cannot be set up (no inotify instances, too many folders,
//! a network share) leaves the feed polling.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use notify::{EventKind, RecursiveMode, Watcher};
use parking_lot::Mutex;

/// How long the folder must be quiet before the feed looks.
pub(crate) const QUIET: Duration = Duration::from_secs(2);
/// The longest a stream of events delays the feed.
pub(crate) const CAP: Duration = Duration::from_secs(10);

pub(crate) struct WatchState {
    events: AtomicU64,
    /// Nanoseconds after `base` of the last relevant event.
    last_ns: AtomicU64,
    base: Instant,
    wake: tokio::sync::Notify,
    /// The watch lost its folder (removed or replaced): set it up again.
    dead: AtomicBool,
    /// The system refused (watch limits): poll only.
    broken: AtomicBool,
    warned: AtomicBool,
}

impl WatchState {
    fn hit(&self) {
        self.last_ns
            .store(self.base.elapsed().as_nanos() as u64, Ordering::Release);
        self.events.fetch_add(1, Ordering::AcqRel);
        self.wake.notify_waiters();
    }

    fn quiet_for(&self) -> Duration {
        let last = Duration::from_nanos(self.last_ns.load(Ordering::Acquire));
        self.base.elapsed().saturating_sub(last)
    }
}

pub(crate) struct Watch {
    root: PathBuf,
    state: Arc<WatchState>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

/// An event path inside the folder that is Bunko's own business or a reader's
/// half-written file: it wakes nothing. `roots`: the folder as configured and as
/// resolved (macOS reports `/private/var/...` for `/var/...`).
fn internal(roots: &[PathBuf], p: &Path) -> bool {
    let Some(rel) = roots.iter().find_map(|r| p.strip_prefix(r).ok()) else {
        return false;
    };
    rel.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| super::hidden_name(s, false))
    })
}

impl Watch {
    pub(crate) fn new(root: PathBuf) -> Watch {
        Watch {
            root,
            state: Arc::new(WatchState {
                events: AtomicU64::new(0),
                last_ns: AtomicU64::new(0),
                base: Instant::now(),
                wake: tokio::sync::Notify::new(),
                dead: AtomicBool::new(false),
                broken: AtomicBool::new(false),
                warned: AtomicBool::new(false),
            }),
            watcher: Mutex::new(None),
        }
    }

    /// Relevant events seen so far.
    pub(crate) fn events(&self) -> u64 {
        self.state.events.load(Ordering::Acquire)
    }

    pub(crate) fn running(&self) -> bool {
        self.watcher.lock().is_some() && !self.state.dead.load(Ordering::Acquire)
    }

    /// The folder went away: the next [`Watch::ensure`] sets the watch up again.
    pub(crate) fn lost(&self) {
        self.state.dead.store(true, Ordering::Release);
    }

    /// Start (or restart) the watch when there is none or it lost its folder.
    /// Blocking (a recursive watch walks the tree). Returns whether one runs.
    pub(crate) fn ensure(&self) -> bool {
        if self.state.broken.load(Ordering::Acquire) {
            return false;
        }
        let mut slot = self.watcher.lock();
        if slot.is_some() && !self.state.dead.load(Ordering::Acquire) {
            return true;
        }
        *slot = None;
        self.state.dead.store(false, Ordering::Release);
        let state = self.state.clone();
        let mut roots = vec![self.root.clone()];
        if let Ok(c) = std::fs::canonicalize(&self.root)
            && c != self.root
        {
            roots.push(c);
        }
        let made = notify::recommended_watcher(
            move |res: notify::Result<notify::Event>| match res {
                Ok(ev) => {
                    if matches!(ev.kind, EventKind::Access(_)) {
                        return;
                    }
                    if ev.paths.iter().any(|p| roots.contains(p))
                        && matches!(
                            ev.kind,
                            EventKind::Remove(_)
                                | EventKind::Modify(notify::event::ModifyKind::Name(_))
                        )
                    {
                        state.dead.store(true, Ordering::Release);
                    }
                    if !ev.need_rescan()
                        && !ev.paths.is_empty()
                        && ev.paths.iter().all(|p| internal(&roots, p))
                    {
                        return;
                    }
                    state.hit();
                }
                Err(e) => {
                    if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) {
                        state.broken.store(true, Ordering::Release);
                        if !state.warned.swap(true, Ordering::AcqRel) {
                            tracing::warn!(
                                "folder: the system's watch limit is reached; the library folder is polled instead"
                            );
                        }
                    } else {
                        tracing::debug!("folder: watch error: {e}");
                    }
                    state.hit();
                }
            },
        );
        let mut w = match made {
            Ok(w) => w,
            Err(e) => {
                self.refused(&e);
                return false;
            }
        };
        if let Err(e) = w.watch(&self.root, RecursiveMode::Recursive) {
            self.refused(&e);
            return false;
        }
        *slot = Some(w);
        true
    }

    fn refused(&self, e: &notify::Error) {
        if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) {
            self.state.broken.store(true, Ordering::Release);
        }
        if !self.state.warned.swap(true, Ordering::AcqRel) {
            tracing::warn!(
                "folder: cannot watch {} ({e}); it is polled instead",
                self.root.display()
            );
        }
    }

    /// Return once there were events after `since` and the folder has been quiet,
    /// or at `max`.
    pub(crate) async fn wait(&self, since: u64, max: Duration) {
        let deadline = tokio::time::Instant::now() + max;
        loop {
            let notified = self.state.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.events() > since {
                break;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return;
            }
        }
        let cap = deadline.min(tokio::time::Instant::now() + CAP);
        loop {
            let quiet = self.state.quiet_for();
            let now = tokio::time::Instant::now();
            if quiet >= QUIET || now >= cap {
                return;
            }
            tokio::time::sleep_until(cap.min(now + (QUIET - quiet))).await;
        }
    }
}
