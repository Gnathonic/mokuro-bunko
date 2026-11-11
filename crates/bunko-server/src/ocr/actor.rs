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

/// Archives that arrived after the owed board was built: library-relative path → (how
/// many arrivals of it the scheduler has not taken in yet, when the last came). The
/// DAV hook adds to it before it tells the scheduler; the actor takes an arrival off
/// once it has published a board built after the scheduler took it in (judged it, on a
/// remote library). Until then the board's `[]` for that volume means "not seen yet",
/// not "nothing owed".
pub type FreshArrivals = Arc<parking_lot::Mutex<std::collections::HashMap<String, (u32, Instant)>>>;

/// An arrival the scheduler never took in (it stopped, or a helper never answered)
/// stops counting after this long.
pub const ARRIVAL_PATIENCE: Duration = Duration::from_secs(600);

/// An arrival of `rel` is on its way to the scheduler.
pub fn note_arrival(fresh: &FreshArrivals, rel: String) {
    let mut map = fresh.lock();
    map.retain(|_, (n, at)| *n > 0 && at.elapsed() < ARRIVAL_PATIENCE);
    let entry = map.entry(rel).or_insert((0, Instant::now()));
    *entry = (entry.0 + 1, Instant::now());
}

/// `rel` arrived and the published owed board may not know it yet.
pub fn arrival_unsettled(fresh: &FreshArrivals, rel: &str) -> bool {
    fresh
        .lock()
        .get(rel)
        .is_some_and(|(n, at)| *n > 0 && at.elapsed() < ARRIVAL_PATIENCE)
}

/// The board just published knows these arrivals.
fn settle_arrivals(fresh: &FreshArrivals, settled: &[String]) {
    if settled.is_empty() {
        return;
    }
    let mut map = fresh.lock();
    for rel in settled {
        if let Some((n, _)) = map.get_mut(rel) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(rel);
            }
        }
    }
}

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
    fresh: FreshArrivals,
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
                // Taken before the board is built, settled after it is published: a
                // manifest that finds an arrival settled finds it on the board.
                let settled = std::mem::take(&mut sched.settled_arrivals);
                *owed.write() = Some(Arc::new(sched.owed_board()));
                settle_arrivals(&fresh, &settled);
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
    fresh.lock().clear();
}

/// Start the thread.
pub fn spawn(
    sched: Scheduler,
    rx: Receiver<Msg>,
    board: RunningJobs,
    failing: FailingVolumes,
    owed: OwedVolumes,
    fresh: FreshArrivals,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("ocr-scheduler".into())
        .spawn(move || run(sched, rx, board, failing, owed, fresh))
}

/// A channel pair for the scheduler.
pub fn channel() -> (Sender<Msg>, Receiver<Msg>) {
    std::sync::mpsc::channel()
}
