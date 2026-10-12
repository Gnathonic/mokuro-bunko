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

/// Run the scheduler until `Msg::Stop` (or every sender is gone).
pub fn run(mut sched: Scheduler, rx: Receiver<Msg>, board: RunningJobs) {
    let mut next_tick = Instant::now() + TICK;
    let mut published: Option<Instant> = None;
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
        }
        if ticked || published.is_none_or(|at| at.elapsed() >= BOARD_EVERY) {
            *board.write() = Arc::new(sched.running_jobs());
            published = Some(Instant::now());
        }
    }
    *board.write() = Arc::new(Vec::new());
}

/// Start the thread.
pub fn spawn(
    sched: Scheduler,
    rx: Receiver<Msg>,
    board: RunningJobs,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("ocr-scheduler".into())
        .spawn(move || run(sched, rx, board))
}

/// A channel pair for the scheduler.
pub fn channel() -> (Sender<Msg>, Receiver<Msg>) {
    std::sync::mpsc::channel()
}
