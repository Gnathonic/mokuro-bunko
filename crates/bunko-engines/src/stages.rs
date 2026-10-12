//! The staged pipeline: bounded queues between pools of OS threads, with the counters
//! the congestion readout needs (0.5.2 `StageQueue` / `Pipeline._run_pooled`, spec
//! ocr-scheduling §21.1).
//!
//! Queues are `in-><first>`, `<stage>-><next>`, ..., `<last>->out`. A stage worker
//! loops: take an item (time spent waiting = *starved*), run the stage (= *busy*),
//! hand it on (time spent waiting for room = *blocked*). Each queue integrates its
//! depth over time, so `mean_depth = depth_seconds / elapsed`. Counters are
//! cumulative for the life of the pipeline (one session); a volume's window is
//! `report().since(mark)`.
//!
//! Capacity is memory: an item can hold a decoded page (~14 MB), so queues are a
//! handful of slots, not buffers.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use bunko_processor::{PipelineReport, QueueReport, StageReport};
use parking_lot::{Condvar, Mutex};

struct QState<T> {
    items: VecDeque<T>,
    finished: bool,
    closed: bool,
    max_depth: u32,
    puts: u64,
    gets: u64,
    blocked_seconds: f64,
    blocked_events: u64,
    starved_seconds: f64,
    starved_events: u64,
    depth_seconds: f64,
    last_change: Instant,
}

impl<T> QState<T> {
    fn accrue(&mut self) {
        let now = Instant::now();
        self.depth_seconds +=
            self.items.len() as f64 * now.duration_since(self.last_change).as_secs_f64();
        self.last_change = now;
    }
}

/// A bounded FIFO with blocking put/get and congestion counters.
pub struct Queue<T> {
    name: String,
    capacity: usize,
    state: Mutex<QState<T>>,
    /// Producers wait here for room, consumers for work: two conditions on one lock.
    room: Condvar,
    work: Condvar,
}

impl<T> Queue<T> {
    pub fn new(name: impl Into<String>, capacity: u32) -> Self {
        Self {
            name: name.into(),
            capacity: capacity.max(1) as usize,
            state: Mutex::new(QState {
                items: VecDeque::new(),
                finished: false,
                closed: false,
                max_depth: 0,
                puts: 0,
                gets: 0,
                blocked_seconds: 0.0,
                blocked_events: 0,
                starved_seconds: 0.0,
                starved_events: 0,
                depth_seconds: 0.0,
                last_change: Instant::now(),
            }),
            room: Condvar::new(),
            work: Condvar::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Hand an item on, waiting for room. Gives it back once the queue is closed.
    pub fn put(&self, item: T) -> Result<(), T> {
        let mut s = self.state.lock();
        if s.items.len() >= self.capacity && !s.closed {
            let started = Instant::now();
            while s.items.len() >= self.capacity && !s.closed {
                self.room.wait(&mut s);
            }
            s.blocked_seconds += started.elapsed().as_secs_f64();
            s.blocked_events += 1;
        }
        if s.closed {
            return Err(item);
        }
        s.accrue();
        s.items.push_back(item);
        s.puts += 1;
        s.max_depth = s.max_depth.max(s.items.len() as u32);
        self.work.notify_one();
        Ok(())
    }

    /// The next item, waiting for one; `None` once finished and empty, or closed.
    pub fn get(&self) -> Option<T> {
        let mut s = self.state.lock();
        if s.items.is_empty() && !s.finished && !s.closed {
            let started = Instant::now();
            while s.items.is_empty() && !s.finished && !s.closed {
                self.work.wait(&mut s);
            }
            s.starved_seconds += started.elapsed().as_secs_f64();
            s.starved_events += 1;
        }
        if s.closed {
            return None;
        }
        s.accrue();
        let item = s.items.pop_front()?;
        s.gets += 1;
        self.room.notify_one();
        Some(item)
    }

    /// No more items will be put; consumers leave once it is empty.
    pub fn finish(&self) {
        self.state.lock().finished = true;
        self.work.notify_all();
    }

    /// Tear down: drop what is held and wake every waiter.
    pub fn close(&self) {
        let mut s = self.state.lock();
        s.closed = true;
        s.accrue();
        s.items.clear();
        self.room.notify_all();
        self.work.notify_all();
    }

    pub fn report(&self, elapsed: f64) -> QueueReport {
        let mut s = self.state.lock();
        s.accrue();
        QueueReport {
            name: self.name.clone(),
            capacity: self.capacity as u32,
            depth: s.items.len() as u32,
            max_depth: s.max_depth,
            mean_depth: if elapsed > 0.0 {
                s.depth_seconds / elapsed
            } else {
                0.0
            },
            depth_seconds: s.depth_seconds,
            puts: s.puts,
            gets: s.gets,
            blocked_seconds: s.blocked_seconds,
            blocked_events: s.blocked_events,
            starved_seconds: s.starved_seconds,
            starved_events: s.starved_events,
        }
    }
}

/// A stage function: one item in, one item out. Failures travel inside the item.
pub type StageFn<T> = Arc<dyn Fn(T) -> T + Send + Sync>;

/// What a stage function does when it panics: turn the item into a failed one.
pub type PanicFn<T> = Arc<dyn Fn(String) -> Option<T> + Send + Sync>;

/// A stage to run.
pub struct StageDef<T> {
    pub key: String,
    pub name: String,
    pub device: String,
    pub device_bound: bool,
    pub workers: u32,
    /// Capacity of the queue this stage fills.
    pub capacity: u32,
    pub run: StageFn<T>,
}

#[derive(Default)]
struct StageCounters {
    items: u64,
    busy: f64,
    blocked: f64,
    starved: f64,
}

struct StageInfo {
    key: String,
    name: String,
    device: String,
    device_bound: bool,
    workers: u32,
    counters: Mutex<StageCounters>,
}

/// A running pipeline. Items go in with [`Pipeline::put`] and come out of the last
/// queue, which the owner drains with [`Pipeline::take`].
pub struct Pipeline<T: Send + 'static> {
    queues: Vec<Arc<Queue<T>>>,
    stages: Vec<Arc<StageInfo>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    started: Instant,
    items_out: Mutex<u64>,
}

impl<T: Send + 'static> Pipeline<T> {
    /// Start the worker threads. `source_capacity` sizes `in-><first>`; `on_panic`
    /// rebuilds an item when a stage panics (the item itself is lost with the panic,
    /// so stage functions should not panic; this only keeps a worker alive).
    pub fn start(
        thread_prefix: &str,
        defs: Vec<StageDef<T>>,
        source_capacity: u32,
        on_panic: Option<PanicFn<T>>,
    ) -> std::io::Result<Arc<Self>> {
        let first = defs.first().map(|d| d.key.clone()).unwrap_or_default();
        let mut queues = vec![Arc::new(Queue::new(
            format!("in->{first}"),
            source_capacity,
        ))];
        for (i, d) in defs.iter().enumerate() {
            let next = defs
                .get(i + 1)
                .map(|n| n.key.clone())
                .unwrap_or_else(|| "out".into());
            queues.push(Arc::new(Queue::new(
                format!("{}->{next}", d.key),
                d.capacity,
            )));
        }
        let stages: Vec<Arc<StageInfo>> = defs
            .iter()
            .map(|d| {
                Arc::new(StageInfo {
                    key: d.key.clone(),
                    name: d.name.clone(),
                    device: d.device.clone(),
                    device_bound: d.device_bound,
                    workers: d.workers.max(1),
                    counters: Mutex::new(StageCounters::default()),
                })
            })
            .collect();
        let pipeline = Arc::new(Self {
            queues,
            stages,
            threads: Mutex::new(Vec::new()),
            started: Instant::now(),
            items_out: Mutex::new(0),
        });
        for (index, def) in defs.into_iter().enumerate() {
            for w in 0..def.workers.max(1) {
                let inq = pipeline.queues[index].clone();
                let outq = pipeline.queues[index + 1].clone();
                let info = pipeline.stages[index].clone();
                let run = def.run.clone();
                let on_panic = on_panic.clone();
                let handle = std::thread::Builder::new()
                    .name(format!("{thread_prefix}-{}-{w}", def.key))
                    .spawn(move || worker(inq, outq, info, run, on_panic))?;
                pipeline.threads.lock().push(handle);
            }
        }
        Ok(pipeline)
    }

    /// Feed one item (waits for room). Gives it back once the pipeline is closed.
    pub fn put(&self, item: T) -> Result<(), T> {
        self.queues[0].put(item)
    }

    /// The next finished item (waits); `None` once closed.
    pub fn take(&self) -> Option<T> {
        let out = self.queues.last()?.get();
        if out.is_some() {
            *self.items_out.lock() += 1;
        }
        out
    }

    /// Stop: every queue is closed, held items dropped, threads joined.
    pub fn shutdown(&self) {
        for q in &self.queues {
            q.close();
        }
        let threads = std::mem::take(&mut *self.threads.lock());
        for t in threads {
            let _ = t.join();
        }
    }

    /// Cumulative counters since the pipeline started.
    pub fn report(&self) -> PipelineReport {
        let elapsed = self.started.elapsed().as_secs_f64();
        let stages = self
            .stages
            .iter()
            .map(|s| {
                let c = s.counters.lock();
                let pool = f64::from(s.workers.max(1)) * elapsed;
                StageReport {
                    key: s.key.clone(),
                    name: s.name.clone(),
                    device: s.device.clone(),
                    workers: s.workers,
                    items: c.items,
                    busy_seconds: c.busy,
                    blocked_seconds: c.blocked,
                    starved_seconds: c.starved,
                    utilisation: if pool > 0.0 { c.busy / pool } else { 0.0 },
                    device_bound: s.device_bound,
                }
            })
            .collect();
        PipelineReport {
            elapsed_seconds: elapsed,
            items: *self.items_out.lock(),
            stages,
            queues: self.queues.iter().map(|q| q.report(elapsed)).collect(),
        }
    }
}

impl<T: Send + 'static> Drop for Pipeline<T> {
    fn drop(&mut self) {
        for q in &self.queues {
            q.close();
        }
    }
}

fn worker<T: Send + 'static>(
    inq: Arc<Queue<T>>,
    outq: Arc<Queue<T>>,
    info: Arc<StageInfo>,
    run: StageFn<T>,
    on_panic: Option<PanicFn<T>>,
) {
    loop {
        let waited = Instant::now();
        let Some(item) = inq.get() else {
            return;
        };
        let starved = waited.elapsed().as_secs_f64();
        let busy_from = Instant::now();
        let out = match catch_unwind(AssertUnwindSafe(|| run(item))) {
            Ok(out) => Some(out),
            Err(panic) => {
                let msg = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic".into());
                tracing::error!(stage = %info.key, "stage panicked: {msg}");
                on_panic.as_ref().and_then(|f| f(msg))
            }
        };
        let busy = busy_from.elapsed().as_secs_f64();
        // Counted before the hand-off: once the item is downstream, a report must
        // already include it.
        {
            let mut c = info.counters.lock();
            c.items += 1;
            c.busy += busy;
            c.starved += starved;
        }
        let blocked_from = Instant::now();
        let closed = match out {
            Some(out) => outq.put(out).is_err(),
            None => false,
        };
        info.counters.lock().blocked += blocked_from.elapsed().as_secs_f64();
        if closed {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn items_flow_in_order_through_one_worker_per_stage() {
        let double: StageFn<u64> = Arc::new(|x| x * 2);
        let slow: StageFn<u64> = Arc::new(|x| {
            std::thread::sleep(Duration::from_millis(5));
            x + 1
        });
        let p = Pipeline::start(
            "t",
            vec![
                StageDef {
                    key: "detect".into(),
                    name: "d".into(),
                    device: "cpu".into(),
                    device_bound: false,
                    workers: 1,
                    capacity: 4,
                    run: double,
                },
                StageDef {
                    key: "engine".into(),
                    name: "e".into(),
                    device: "gpu:0".into(),
                    device_bound: true,
                    workers: 1,
                    capacity: 1,
                    run: slow,
                },
            ],
            3,
            None,
        )
        .unwrap();
        let feeder = {
            let p = p.clone();
            std::thread::spawn(move || {
                for i in 0..20u64 {
                    p.put(i).unwrap();
                }
            })
        };
        let got: Vec<u64> = (0..20).map(|_| p.take().unwrap()).collect();
        feeder.join().unwrap();
        assert_eq!(got, (0..20).map(|i| i * 2 + 1).collect::<Vec<_>>());
        let r = p.report();
        assert_eq!(r.items, 20);
        assert_eq!(
            r.queues.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(),
            ["in->detect", "detect->engine", "engine->out"]
        );
        assert_eq!(r.stages[1].items, 20);
        assert!(r.stages[1].busy_seconds >= 0.09, "{:?}", r.stages[1]);
        // The fast stage waits on the slow one.
        assert!(r.stages[0].blocked_seconds > 0.0);
        assert!(r.queues[1].max_depth >= 3);
        assert_eq!(r.queues[1].puts, 20);
        assert_eq!(r.bottleneck().unwrap().key, "engine");
        p.shutdown();
        assert!(p.put(1).is_err());
    }

    #[test]
    fn a_panicking_stage_keeps_its_worker() {
        let boom: StageFn<i32> = Arc::new(|x| {
            if x == 3 {
                panic!("three");
            }
            x
        });
        let on_panic: PanicFn<i32> = Arc::new(|_| Some(-1));
        let p = Pipeline::start(
            "t",
            vec![StageDef {
                key: "detect".into(),
                name: "d".into(),
                device: "cpu".into(),
                device_bound: false,
                workers: 1,
                capacity: 2,
                run: boom,
            }],
            2,
            Some(on_panic),
        )
        .unwrap();
        let feeder = {
            let p = p.clone();
            std::thread::spawn(move || {
                for i in 0..6 {
                    p.put(i).unwrap();
                }
            })
        };
        let got: Vec<i32> = (0..6).map(|_| p.take().unwrap()).collect();
        feeder.join().unwrap();
        assert_eq!(got, vec![0, 1, 2, -1, 4, 5]);
        p.shutdown();
    }
}
