//! Benchmark arithmetic (0.5.2 `engine_runner.bench_fill` / `bench_window` /
//! `BenchRun._sized_repeat` and `bench.emission_window`; spec ocr-generations-bench
//! §6.7). The processor measures with it, the library keeps the same rules.
//!
//! A benchmark is timed by the instants its per-page results LEAVE the pipeline and by
//! nothing else: model load, warm-up and pipeline fill happen before or around those
//! instants and none of them can enter a rate (ADDENDUM 9).

use serde_json::{Map, Value};

use crate::congestion::MIN_PAGES;
use crate::py::{float_value, round_to};

/// A widening step must buy at least this much throughput to be kept.
pub const BENCH_GAIN: f64 = 0.03;
/// A narrowing step may cost at most this much (against the peak) to be kept.
pub const BENCH_HOLD: f64 = 0.01;
/// Width-search trials (the precision phase adds its own).
pub const BENCH_MAX_TRIALS: usize = 8;
/// The search's time budget; the library gives up `BENCH_SLACK_SECONDS` later.
pub const BENCH_BUDGET_SECONDS: f64 = 900.0;
/// Pages of the sample thrown away before the first trial.
pub const BENCH_WARMUP_PAGES: usize = 4;
/// `bench_progress` events are at least this far apart.
pub const BENCH_PROGRESS_INTERVAL: f64 = 1.0;
/// A trial re-feeds the sample until its measured window is this long...
pub const BENCH_MIN_WINDOW_SECONDS: f64 = 20.0;
/// ... or this many feeds have run.
pub const BENCH_MAX_PASSES: usize = 8;
/// How many times one feed may repeat the sample.
pub const BENCH_MAX_REPEAT: usize = 64;
/// A feed's own window is its steady-state rate only when it spans this much of it.
pub const BENCH_WINDOW_IS_THE_FEED: f64 = 0.25;
/// Under this the number is reported, flagged and never decided on.
pub const BENCH_SHORT_WINDOW_SECONDS: f64 = 10.0;

/// `bench_fill(pages)`: emissions discarded as pipeline fill, `min(8, N/4)`, taken
/// off the head of the first pass only.
pub fn bench_fill(pages: usize) -> usize {
    (MIN_PAGES.max(0) as usize).min(pages / 4)
}

/// A rate and the window it is a rate over (`BenchWindow`).
#[derive(Debug, Clone, PartialEq)]
pub struct BenchWindow {
    pub pages_per_second: f64,
    pub window_seconds: f64,
    pub pages_measured: usize,
    pub passes: usize,
    pub short_window: bool,
    pub first_emission_at: Option<f64>,
    pub last_emission_at: Option<f64>,
}

impl BenchWindow {
    /// `BenchWindow.as_dict()`: the window fields of a trial / baseline / best.
    pub fn to_map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert(
            "window_seconds".into(),
            float_value(round_to(self.window_seconds, 3)),
        );
        m.insert("pages_measured".into(), Value::from(self.pages_measured));
        m.insert("passes".into(), Value::from(self.passes));
        m.insert("short_window".into(), Value::Bool(self.short_window));
        for (key, at) in [
            ("first_emission_at", self.first_emission_at),
            ("last_emission_at", self.last_emission_at),
        ] {
            m.insert(
                key.into(),
                at.map_or(Value::Null, |t| float_value(round_to(t, 3))),
            );
        }
        m
    }
}

/// `bench_window(emissions, fill, passes)`: `(M - 1) / (t_last - t_first)` over the
/// emissions after the fill. A degenerate window yields 0.0, and either way
/// `short_window` (window < `short_seconds`) says it must not be decided on.
pub fn bench_window(
    emissions: &[f64],
    fill: usize,
    passes: usize,
    short_seconds: f64,
) -> BenchWindow {
    let measured = emissions.get(fill..).unwrap_or(&[]);
    let first = measured.first().copied();
    let last = measured.last().copied();
    let window = match (first, last) {
        (Some(a), Some(b)) => b - a,
        _ => 0.0,
    };
    let rate = if window > 0.0 && measured.len() > 1 {
        (measured.len() - 1) as f64 / window
    } else {
        0.0
    };
    BenchWindow {
        pages_per_second: rate,
        window_seconds: window.max(0.0),
        pages_measured: measured.len(),
        passes,
        short_window: window < short_seconds,
        first_emission_at: first,
        last_emission_at: last,
    }
}

/// The rules a measurement runs by (0.5.2's constants; tests shrink them).
#[derive(Debug, Clone, PartialEq)]
pub struct WindowRules {
    pub min_window_seconds: f64,
    pub short_window_seconds: f64,
    pub max_repeat: usize,
    pub window_is_the_feed: f64,
}

impl Default for WindowRules {
    fn default() -> Self {
        WindowRules {
            min_window_seconds: BENCH_MIN_WINDOW_SECONDS,
            short_window_seconds: BENCH_SHORT_WINDOW_SECONDS,
            max_repeat: BENCH_MAX_REPEAT,
            window_is_the_feed: BENCH_WINDOW_IS_THE_FEED,
        }
    }
}

/// `_sized_repeat(pages, feed, seconds, was)`: how many times to feed the sample so
/// ONE feed fills the window. Sized from the feed's steady-state rate where its window
/// spans the feed, else from its wall clock.
pub fn sized_repeat(
    pages: usize,
    feed: &[f64],
    seconds: f64,
    was: usize,
    rules: &WindowRules,
) -> usize {
    let done = feed.len();
    let wall_rate = if seconds > 0.0 && done > 0 {
        done as f64 / seconds
    } else {
        0.0
    };
    let window = bench_window(feed, bench_fill(pages), 1, rules.short_window_seconds);
    let mut rate = wall_rate;
    if window.window_seconds >= rules.window_is_the_feed * seconds
        && window.pages_per_second > wall_rate
    {
        rate = window.pages_per_second;
    }
    if rate <= 0.0 {
        return rules.max_repeat.min((was + 1).max(was * 2));
    }
    let wanted = bench_fill(pages) + (rules.min_window_seconds * 1.2 * rate) as usize + 1;
    let needed = wanted.div_ceil(pages.max(1));
    (was + 1).max(rules.max_repeat.min(needed))
}

/// `emission_window(emissions, passes)`: the library's copy of the window rule over
/// UNSORTED instants (`{pages_per_second, window_seconds, pages_measured, passes,
/// short_window, first_emission_at, last_emission_at}`; the instants are 0.0 when
/// nothing was measured).
pub fn emission_window(emissions: &[f64], passes: usize) -> Map<String, Value> {
    let mut ordered = emissions.to_vec();
    ordered.sort_by(f64::total_cmp);
    let fill = (MIN_PAGES.max(0) as usize).min(ordered.len() / 4);
    let w = bench_window(&ordered, fill, passes, BENCH_SHORT_WINDOW_SECONDS);
    let mut m = Map::new();
    m.insert("pages_per_second".into(), float_value(w.pages_per_second));
    m.insert("window_seconds".into(), float_value(w.window_seconds));
    m.insert("pages_measured".into(), Value::from(w.pages_measured));
    m.insert("passes".into(), Value::from(passes));
    m.insert("short_window".into(), Value::Bool(w.short_window));
    m.insert(
        "first_emission_at".into(),
        float_value(w.first_emission_at.unwrap_or(0.0)),
    );
    m.insert(
        "last_emission_at".into(),
        float_value(w.last_emission_at.unwrap_or(0.0)),
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_is_a_quarter_up_to_eight() {
        assert_eq!(bench_fill(0), 0);
        assert_eq!(bench_fill(7), 1);
        assert_eq!(bench_fill(32), 8);
        assert_eq!(bench_fill(512), 8);
    }

    #[test]
    fn window_counts_intervals_not_emissions() {
        // 11 emissions, one a second: 10 intervals over 10 s.
        let e: Vec<f64> = (0..11).map(f64::from).collect();
        let w = bench_window(&e, 0, 1, 10.0);
        assert_eq!(w.pages_per_second, 1.0);
        assert_eq!(w.window_seconds, 10.0);
        assert!(!w.short_window);
        // The fill comes off the head.
        let w = bench_window(&e, 2, 1, 10.0);
        assert_eq!(w.pages_measured, 9);
        assert_eq!(w.first_emission_at, Some(2.0));
        assert!(w.short_window);
        // A burst has no span: no rate rather than a six-figure one.
        let w = bench_window(&[5.0, 5.0, 5.0], 0, 1, 10.0);
        assert_eq!(w.pages_per_second, 0.0);
        assert!(w.short_window);
        let m = bench_window(&[], 0, 2, 10.0).to_map();
        assert_eq!(m["first_emission_at"], Value::Null);
        assert_eq!(m["passes"], 2);
    }

    #[test]
    fn repeat_is_sized_to_fill_the_window() {
        let rules = WindowRules::default();
        // 32 pages at 4 p/s over 8 s: wants 8 + 96 + 1 = 105 pages = 4 feeds.
        let feed: Vec<f64> = (0..32).map(|i| f64::from(i) * 0.25).collect();
        assert_eq!(sized_repeat(32, &feed, 8.0, 1, &rules), 4);
        // Nothing measured: double.
        assert_eq!(sized_repeat(32, &[], 0.0, 3, &rules), 6);
        // A burst's window says nothing: the wall clock sizes it (3.2 p/s -> 85 pages).
        let burst: Vec<f64> = (0..32).map(|i| f64::from(i) * 0.0001).collect();
        assert_eq!(sized_repeat(32, &burst, 10.0, 1, &rules), 3);
        // Never less than one more.
        let slow: Vec<f64> = (0..32).map(|i| f64::from(i) * 100.0).collect();
        assert_eq!(sized_repeat(32, &slow, 3100.0, 5, &rules), 6);
    }

    #[test]
    fn emission_window_sorts_and_reports_zeros() {
        let m = emission_window(&[3.0, 1.0, 2.0, 0.0], 1);
        assert_eq!(m["pages_per_second"], 1.0);
        assert_eq!(m["first_emission_at"], 1.0);
        let m = emission_window(&[], 1);
        assert_eq!(m["first_emission_at"], 0.0);
        assert_eq!(m["short_window"], true);
    }
}
