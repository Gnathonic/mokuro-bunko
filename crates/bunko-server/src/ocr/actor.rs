//! The scheduler's thread: one OS thread owns the [`Scheduler`] and handles its inbox,
//! plus a 1 s tick. Requests from async code send a message (never blocking) and await
//! a oneshot reply.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde_json::{Map, Value};

use super::sched::{Msg, Scheduler};

/// The tick period.
pub const TICK: Duration = Duration::from_secs(1);

/// How often, at most, the running-jobs board is republished between ticks.
const BOARD_EVERY: Duration = Duration::from_millis(250);

/// The scheduler's running jobs ([`Scheduler::running_jobs`]) as it last published them.
/// The public OCR status reads this instead of asking the scheduler, so a busy scheduler
/// never makes a status request wait.
pub type RunningJobs = Arc<RwLock<Arc<Vec<Map<String, Value>>>>>;

/// The failing volumes ([`Scheduler::failing_volumes`]) as last published (each tick),
/// for the catalog's "OCR failed" badge: casefolded relative path → public text.
pub type FailingVolumes = Arc<RwLock<Arc<std::collections::HashMap<String, String>>>>;

/// What every owed volume is still owed ([`Scheduler::owed_board`]): the rows, without
/// finishing times. The manifest answers from it when the scheduler does not answer
/// within its second, so a busy queue still reads as "these jobs are pending" rather
/// than as nothing pending (which a reader takes as finished) or as no manifest at all
/// (which a deep link takes as "guess the sidecars").
#[derive(Debug, Default)]
pub struct OwedBoard {
    /// Enabled rows, the primary first, then list order.
    pub rows: Vec<bunko_sched::outlook::OwedRow>,
    /// Library-relative archive path (as the owed index keys it) → indexes into `rows`,
    /// ascending.
    pub volumes: std::collections::HashMap<String, Vec<u16>>,
}

impl OwedBoard {
    /// The manifest's `pending` for `rel`: `[{kind, id, eta: null}]`, `[]` when nothing
    /// is owed.
    pub fn pending(&self, rel: &str) -> Vec<Value> {
        let owed: Vec<bunko_sched::outlook::OwedRow> = self
            .volumes
            .get(rel)
            .into_iter()
            .flatten()
            .filter_map(|i| self.rows.get(usize::from(*i)).cloned())
            .collect();
        bunko_sched::outlook::pending_entries(&owed, "", "", &[])
    }
}

/// The [`OwedBoard`] as last published; None until the scheduler first publishes it.
pub type OwedVolumes = Arc<RwLock<Option<Arc<OwedBoard>>>>;

/// The owed board is rebuilt at most this often while the queue keeps changing …
const OWED_EVERY: Duration = Duration::from_secs(5);
/// … and at least this often (a renamed row changes no queue generation).
const OWED_AT_LEAST: Duration = Duration::from_secs(60);

/// Run the scheduler until `Msg::Stop` (or every sender is gone).
pub fn run(
    mut sched: Scheduler,
    rx: Receiver<Msg>,
    board: RunningJobs,
    failing: FailingVolumes,
    owed: OwedVolumes,
) {
    let mut next_tick = Instant::now() + TICK;
    let mut published: Option<Instant> = None;
    let mut owed_at: Option<(u64, Instant)> = None;
    loop {
        let wait = next_tick.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(Msg::Stop) => {
                sched.handle(Msg::Stop);
                break;
            }
            Ok(msg) => {
                let started = Instant::now();
                let name = format!("{msg:?}");
                sched.handle(msg);
                let took = started.elapsed();
                if took > Duration::from_millis(500) {
                    tracing::debug!("the OCR scheduler took {took:?} over {name}");
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let ticked = Instant::now() >= next_tick;
        if ticked {
            sched.handle(Msg::Tick);
            next_tick = Instant::now() + TICK;
            let now = sched.failing_volumes();
            if **failing.read() != now {
                *failing.write() = Arc::new(now);
            }
            let generation = sched.queue_generation;
            let due = owed_at.is_none_or(|(g, at)| {
                (g != generation && at.elapsed() >= OWED_EVERY) || at.elapsed() >= OWED_AT_LEAST
            });
            if due {
                *owed.write() = Some(Arc::new(sched.owed_board()));
                owed_at = Some((generation, Instant::now()));
            }
        }
        if ticked || published.is_none_or(|at| at.elapsed() >= BOARD_EVERY) {
            *board.write() = Arc::new(sched.running_jobs());
            published = Some(Instant::now());
        }
    }
    *board.write() = Arc::new(Vec::new());
    *failing.write() = Arc::default();
    *owed.write() = None;
}

/// Start the thread.
pub fn spawn(
    sched: Scheduler,
    rx: Receiver<Msg>,
    board: RunningJobs,
    failing: FailingVolumes,
    owed: OwedVolumes,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("ocr-scheduler".into())
        .spawn(move || run(sched, rx, board, failing, owed))
}

/// A channel pair for the scheduler.
pub fn channel() -> (Sender<Msg>, Receiver<Msg>) {
    std::sync::mpsc::channel()
}
