//! The scheduler's thread: one OS thread owns the [`Scheduler`] and handles its inbox,
//! plus a 1 s tick. Requests from async code send a message (never blocking) and await
//! a oneshot reply.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use super::sched::{Msg, Scheduler};

/// The tick period.
pub const TICK: Duration = Duration::from_secs(1);

/// Run the scheduler until `Msg::Stop` (or every sender is gone).
pub fn run(mut sched: Scheduler, rx: Receiver<Msg>) {
    let mut next_tick = Instant::now() + TICK;
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
        if Instant::now() >= next_tick {
            sched.handle(Msg::Tick);
            next_tick = Instant::now() + TICK;
        }
    }
}

/// Start the thread.
pub fn spawn(sched: Scheduler, rx: Receiver<Msg>) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("ocr-scheduler".into()).spawn(move || run(sched, rx))
}

/// A channel pair for the scheduler.
pub fn channel() -> (Sender<Msg>, Receiver<Msg>) {
    std::sync::mpsc::channel()
}
