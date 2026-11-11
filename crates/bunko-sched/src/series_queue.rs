//! The OCR queue file v2 (`docs/design/queue-file-v2.md`): one file per series,
//! listing every job the series owes, with waiting jobs placed on a work clock.
//!
//! This module is the pure half, shared by every sink: the format, the work clock and
//! its fit, the dirty test and the renderer. It reads a [`Snapshot`] of the scheduler
//! and keeps, per series, what a sink last published ([`Published`]). It does no I/O.
//!
//! - **Work clock** ([`Clock`], §4.4): a waiting job carries a work position `w`, and
//!   its finishing time is `clock(w)`. A refit ([`Clock::fit`], §6.2) absorbs a change
//!   of fleet speed or a pause without rewriting any series file.
//! - **Dirty test** ([`classify`], §6.3): a file is rewritten only for a real change
//!   (a job added or settled, a job starting, an ETA that left its tolerance after a
//!   clock switch), never for time passing.
//! - **Settle rule** (§5.3): a job that is no longer owed stays in its file, shown as
//!   running, until the series is settled (the caller says when).
//! - **Renderer** ([`render`], [`Published::body`]): the stored JSON body.
//!
//! [`Sink`] ties them together for a sink without a budget (Bunko's HTTP copy): every
//! dirty series is rendered at once.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

/// The format version (`v`).
pub const FORMAT: u64 = 2;
/// The file's name in each series folder.
pub const FILE_NAME: &str = ".mokuro-series-queue.json";
/// Work positions and predicted finishes are rounded to this many seconds.
pub const ROUND_SECONDS: i64 = 60;
/// A `done` entry is kept this long …
pub const DONE_KEEP_SECONDS: i64 = 86_400;
/// … and at most this many of them (the newest).
pub const DONE_MAX: usize = 64;
/// The least tolerance of the dirty test (`tol`).
pub const TOL_MIN_SECONDS: f64 = 60.0;
/// The share of the time left the dirty test tolerates.
pub const TOL_SHARE: f64 = 0.25;
/// The refit's slope stays within these.
pub const SLOPE_MIN: f64 = 0.1;
pub const SLOPE_MAX: f64 = 10.0;
/// A clock has 2 to this many knots.
pub const MAX_KNOTS: usize = 16;
/// A clock is refitted at most this often without a store (§6.2).
pub const REFIT_SECONDS: f64 = 300.0;
/// A running job overruns its published finish by more than this before it counts
/// as drift (or half its published run time, whichever is more).
pub const OVERRUN_MIN_SECONDS: f64 = 300.0;

/// `tol(x) = max(60 s, 25 % of x)`, where `x` is the published time left (never below 0).
pub fn tol(left: f64) -> f64 {
    TOL_MIN_SECONDS.max(TOL_SHARE * left.max(0.0))
}

/// `t` rounded to [`ROUND_SECONDS`].
pub fn round_minute(t: f64) -> i64 {
    ((t / ROUND_SECONDS as f64).round() as i64) * ROUND_SECONDS
}

// --- the work clock --------------------------------------------------------------------------

/// The work clock: knots `[w, t]`, strictly increasing in both, linear between them and
/// extrapolated with the end segments' slopes beyond them. `at` is when it was fitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clock {
    pub knots: Vec<(i64, i64)>,
    pub at: i64,
}

impl Clock {
    /// The first clock: the identity, `[[t0, t0], [t0 + 86400, t0 + 86400]]`.
    pub fn identity(t0: i64) -> Clock {
        Clock {
            knots: vec![(t0, t0), (t0 + 86_400, t0 + 86_400)],
            at: t0,
        }
    }

    /// The segment `[i, i + 1]` that `x` falls in (by `pick`), the end ones beyond.
    fn segment(&self, x: f64, pick: impl Fn(&(i64, i64)) -> i64) -> usize {
        let n = self.knots.len();
        let mut i = 0;
        while i + 2 < n && x > pick(&self.knots[i + 1]) as f64 {
            i += 1;
        }
        i
    }

    /// `clock(w)`: when work position `w` is reached.
    pub fn eta(&self, w: f64) -> f64 {
        let i = self.segment(w, |k| k.0);
        let ((w0, t0), (w1, t1)) = (self.knots[i], self.knots[i + 1]);
        t0 as f64 + (w - w0 as f64) * (t1 - t0) as f64 / (w1 - w0) as f64
    }

    /// `clock⁻¹(t)`: the work position reached at `t`.
    pub fn position(&self, t: f64) -> f64 {
        let i = self.segment(t, |k| k.1);
        let ((w0, t0), (w1, t1)) = (self.knots[i], self.knots[i + 1]);
        w0 as f64 + (t - t0 as f64) * (w1 - w0) as f64 / (t1 - t0) as f64
    }

    /// `[[w, t], …]`.
    pub fn knots_json(&self) -> Value {
        Value::Array(self.knots.iter().map(|(w, t)| json!([w, t])).collect())
    }

    /// A clock from `[[w, t], …]` and its fit time: None unless it has 2 to
    /// [`MAX_KNOTS`] integer knots, strictly increasing in both.
    pub fn from_json(knots: &Value, at: i64) -> Option<Clock> {
        let list = knots.as_array()?;
        if !(2..=MAX_KNOTS).contains(&list.len()) {
            return None;
        }
        let mut out: Vec<(i64, i64)> = Vec::with_capacity(list.len());
        for k in list {
            let pair = k.as_array().filter(|p| p.len() == 2)?;
            let (w, t) = (pair[0].as_i64()?, pair[1].as_i64()?);
            if out.last().is_some_and(|(pw, pt)| w <= *pw || t <= *pt) {
                return None;
            }
            out.push((w, t));
        }
        Some(Clock { knots: out, at })
    }

    /// The refit (§6.2): a weighted least-squares line of finishing time `e` on
    /// published work position `w`, each point weighted `1 / tol(e − now)²`, the slope
    /// clamped to [[`SLOPE_MIN`], [`SLOPE_MAX`]], through the weighted means. Its two
    /// knots sit at the least and the greatest of `span` (every published `w`). None
    /// with fewer than 2 points.
    pub fn fit(points: &[(f64, f64)], span: (f64, f64), now: f64) -> Option<Clock> {
        if points.len() < 2 {
            return None;
        }
        let weight = |e: f64| 1.0 / tol(e - now).powi(2);
        let total: f64 = points.iter().map(|(_, e)| weight(*e)).sum();
        let w_mean = points.iter().map(|(w, e)| weight(*e) * w).sum::<f64>() / total;
        let e_mean = points.iter().map(|(_, e)| weight(*e) * e).sum::<f64>() / total;
        let spread: f64 = points
            .iter()
            .map(|(w, e)| weight(*e) * (w - w_mean).powi(2))
            .sum();
        let along: f64 = points
            .iter()
            .map(|(w, e)| weight(*e) * (w - w_mean) * (e - e_mean))
            .sum();
        let slope = if spread > 0.0 { along / spread } else { 1.0 };
        let slope = slope.clamp(SLOPE_MIN, SLOPE_MAX);
        let f = |w: f64| e_mean + slope * (w - w_mean);
        let w_min = span.0.min(span.1).round() as i64;
        let mut w_max = span.0.max(span.1).round() as i64;
        if w_max <= w_min {
            w_max = w_min + 86_400;
        }
        let t_min = f(w_min as f64).round() as i64;
        let t_max = (f(w_max as f64).round() as i64).max(t_min + 1);
        Some(Clock {
            knots: vec![(w_min, t_min), (w_max, t_max)],
            at: now.floor() as i64,
        })
    }
}

// --- the live queue ----------------------------------------------------------------------------

/// A row as files name it: `{kind, id, parts}` (v1's job `kind`, `id` and `parts`).
#[derive(Clone, Debug, PartialEq)]
pub struct RowDesc {
    /// `"ocr"` for the primary row, else `"layer"`.
    pub kind: String,
    /// The row's name (what v1 put in a job's `id`).
    pub id: String,
    /// `[{role, name, version?}]`.
    pub parts: Value,
}

impl RowDesc {
    fn to_json(&self) -> Value {
        json!({"kind": self.kind, "id": self.id, "parts": self.parts})
    }
}

/// What the scheduler says of one owed job now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LiveState {
    /// Waiting; `e` is the plan's predicted finish, None when the plan cannot time it
    /// (or it is outside the plan: failure backoff).
    Queued { e: Option<f64> },
    /// Claimed by a machine that has not started it.
    Claimed,
    /// Running since `t0`, predicted to finish at `e`.
    Running { t0: f64, e: Option<f64> },
}

/// One owed (volume, row).
#[derive(Clone, Debug, PartialEq)]
pub struct LiveJob {
    /// The row's id ([`RowDesc::id`]).
    pub row: String,
    pub state: LiveState,
}

/// One series' owed jobs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SeriesLive {
    /// The folder as the library spells it.
    pub folder: String,
    /// Archive stem → its owed jobs, in row order.
    pub vols: BTreeMap<String, Vec<LiveJob>>,
}

/// The scheduler's whole queue at one moment, by series fold key.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// The scheduler's clock (epoch seconds).
    pub now: f64,
    /// The enabled rows, the primary first, then list order.
    pub rows: Vec<RowDesc>,
    /// Why the whole queue is held (`queue_hold`), if it is.
    pub held: Option<String>,
    /// Rows no open lane can take now, by id.
    pub held_rows: BTreeSet<String>,
    /// Series fold key → its owed jobs. Series owing nothing are absent.
    pub series: BTreeMap<String, SeriesLive>,
}

// --- what a sink published ----------------------------------------------------------------------

/// A job as a file shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tuple {
    /// `[r, "q", w]`: waiting at work position `w` (None: not timed).
    Queued(Option<i64>),
    /// `[r, "c"]`: claimed, not started.
    Claimed,
    /// `[r, "r", t0, e]`: running since `t0`, predicted to finish at `e`.
    Running { t0: i64, e: Option<i64> },
}

impl Tuple {
    fn to_json(self, row: usize) -> Value {
        match self {
            Tuple::Queued(w) => json!([row, "q", w]),
            Tuple::Claimed => json!([row, "c"]),
            Tuple::Running { t0, e } => json!([row, "r", t0, e]),
        }
    }
}

/// What a sink last published for one series.
#[derive(Clone, Debug, PartialEq)]
pub struct Published {
    /// The folder spelling.
    pub folder: String,
    pub rev: u64,
    /// When it was rendered (the sink's clock).
    pub at: i64,
    /// The fit time of the clock its `w` values were computed under.
    pub clock_at: i64,
    /// The rows its jobs refer to, in row order.
    pub rows: Vec<RowDesc>,
    /// Stem → `(row id, tuple)`, in row order.
    pub vols: BTreeMap<String, Vec<(String, Tuple)>>,
    /// Stem → when its last job settled.
    pub done: BTreeMap<String, i64>,
}

impl Published {
    /// A file with nothing in it (a series that owes nothing and never did here).
    pub fn empty(folder: &str, clock: &Clock, now: f64) -> Published {
        Published {
            folder: folder.to_string(),
            rev: 0,
            at: now.floor() as i64,
            clock_at: clock.at,
            rows: Vec::new(),
            vols: BTreeMap::new(),
            done: BTreeMap::new(),
        }
    }

    fn tuple(&self, stem: &str, row: &str) -> Option<Tuple> {
        self.vols
            .get(stem)?
            .iter()
            .find(|(r, _)| r == row)
            .map(|(_, t)| *t)
    }

    /// The stored body (§4.1): `{v, instance_id, rev, at, clock_at, series, rows, vols,
    /// done}`, keys in that order, `vols` and `done` in byte order of their keys.
    pub fn body(&self, instance_id: &str) -> Map<String, Value> {
        let index: BTreeMap<&str, usize> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.id.as_str(), i))
            .collect();
        let mut vols = Map::new();
        for (stem, jobs) in &self.vols {
            let list: Vec<Value> = jobs
                .iter()
                .filter_map(|(row, t)| index.get(row.as_str()).map(|i| t.to_json(*i)))
                .collect();
            vols.insert(stem.clone(), Value::Array(list));
        }
        let mut done = Map::new();
        for (stem, at) in &self.done {
            done.insert(stem.clone(), json!(at));
        }
        let mut m = Map::new();
        m.insert("v".into(), json!(FORMAT));
        m.insert("instance_id".into(), json!(instance_id));
        m.insert("rev".into(), json!(self.rev));
        m.insert("at".into(), json!(self.at));
        m.insert("clock_at".into(), json!(self.clock_at));
        m.insert("series".into(), json!(self.folder));
        m.insert(
            "rows".into(),
            Value::Array(self.rows.iter().map(RowDesc::to_json).collect()),
        );
        m.insert("vols".into(), Value::Object(vols));
        m.insert("done".into(), Value::Object(done));
        m
    }

    /// Nothing owed, and every `done` entry older than [`DONE_KEEP_SECONDS`]: the file
    /// can go (P3).
    pub fn expired(&self, now: f64) -> bool {
        self.vols.is_empty()
            && self
                .done
                .values()
                .all(|at| (*at as f64) + DONE_KEEP_SECONDS as f64 <= now)
    }
}

/// The library-wide part of the HTTP copy (§4.3) and of the marker's block (§4.2):
/// `{held, held_rows, clock, clock_at, batch_s}`.
pub fn live_block(snap: &Snapshot, clock: &Clock, batch_s: u64) -> Value {
    json!({
        "held": snap.held.as_ref().map_or(Value::Null, |r| json!({"reason": r})),
        "held_rows": snap.held_rows.iter().collect::<Vec<_>>(),
        "clock": clock.knots_json(),
        "clock_at": clock.at,
        "batch_s": batch_s,
    })
}

// --- the dirty test ------------------------------------------------------------------------------

/// Why a series file must be rewritten (§6.3), most urgent first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// A job added or settled, a new `done` entry, the rows table or folder changed, a
    /// first publish.
    Hard,
    /// A job started, was released, or became timed or untimed.
    State,
    /// An ETA left its tolerance (judged only right after a clock switch).
    Drift,
}

/// What a job should show, before it is turned into a tuple.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Want {
    /// Owed: as the scheduler says. `held`: its row is held now.
    Live { state: LiveState, held: bool },
    /// No longer owed, its series not settled yet: shown running until it is.
    Finishing { t0: i64, e: i64 },
}

/// Every job the file should show: what is owed, plus what was published and is no
/// longer owed while the series is not settled (§5.3).
fn wanted(
    published: Option<&Published>,
    live: Option<&SeriesLive>,
    held_rows: &BTreeSet<String>,
    settled: bool,
    now: f64,
) -> BTreeMap<String, Vec<(String, Want)>> {
    let mut out: BTreeMap<String, Vec<(String, Want)>> = BTreeMap::new();
    if let Some(live) = live {
        for (stem, jobs) in &live.vols {
            let list = out.entry(stem.clone()).or_default();
            for job in jobs {
                if list.iter().any(|(r, _)| *r == job.row) {
                    continue;
                }
                list.push((
                    job.row.clone(),
                    Want::Live {
                        state: job.state,
                        held: held_rows.contains(&job.row),
                    },
                ));
            }
        }
    }
    if settled {
        return out;
    }
    let now_s = now.floor() as i64;
    for (stem, jobs) in published.map(|p| &p.vols).into_iter().flatten() {
        for (row, tuple) in jobs {
            let owed = out
                .get(stem)
                .is_some_and(|l| l.iter().any(|(r, _)| r == row));
            if owed {
                continue;
            }
            let finishing = match tuple {
                Tuple::Running { t0, e } => Want::Finishing {
                    t0: *t0,
                    e: e.unwrap_or(now_s).max(now_s),
                },
                _ => Want::Finishing {
                    t0: now_s,
                    e: now_s,
                },
            };
            out.entry(stem.clone())
                .or_default()
                .push((row.clone(), finishing));
        }
    }
    out
}

/// The rows a file showing `want` refers to: in the live row order, then any the
/// published file had that are no longer enabled.
fn rows_for(
    want: &BTreeMap<String, Vec<(String, Want)>>,
    snap: &Snapshot,
    published: Option<&Published>,
) -> Vec<RowDesc> {
    let used: BTreeSet<&str> = want.values().flatten().map(|(r, _)| r.as_str()).collect();
    let mut rows: Vec<RowDesc> = snap
        .rows
        .iter()
        .filter(|r| used.contains(r.id.as_str()))
        .cloned()
        .collect();
    for r in published.map(|p| p.rows.as_slice()).unwrap_or_default() {
        if used.contains(r.id.as_str()) && !rows.iter().any(|x| x.id == r.id) {
            rows.push(r.clone());
        }
    }
    rows
}

/// The dirty test (§6.3): why the series' file must be rewritten, or None. `drift`:
/// judge drift too (only right after a confirmed clock switch). Time passing, the
/// global hold and the ETAs of held rows are never a reason.
pub fn classify(
    published: Option<&Published>,
    live: Option<&SeriesLive>,
    snap: &Snapshot,
    settled: bool,
    clock: &Clock,
    drift: bool,
) -> Option<Class> {
    let now = snap.now;
    let want = wanted(published, live, &snap.held_rows, settled, now);
    let Some(p) = published else {
        return (!want.is_empty()).then_some(Class::Hard);
    };
    // Membership: a job added, a job settled (and so a new `done` entry), a volume
    // renamed (a stem gone, another come).
    let want_keys: BTreeMap<&String, BTreeSet<&String>> = want
        .iter()
        .map(|(s, l)| (s, l.iter().map(|(r, _)| r).collect()))
        .collect();
    let pub_keys: BTreeMap<&String, BTreeSet<&String>> = p
        .vols
        .iter()
        .map(|(s, l)| (s, l.iter().map(|(r, _)| r).collect()))
        .collect();
    if want_keys != pub_keys {
        return Some(Class::Hard);
    }
    if live.is_some_and(|l| l.folder != p.folder) || rows_for(&want, snap, Some(p)) != p.rows {
        return Some(Class::Hard);
    }
    let mut class: Option<Class> = None;
    for (stem, jobs) in &want {
        for (row, w) in jobs {
            let Some(old) = p.tuple(stem, row) else {
                return Some(Class::Hard);
            };
            let seen = match (*w, old) {
                (Want::Finishing { .. }, Tuple::Running { .. }) => None,
                (Want::Finishing { .. }, _) => Some(Class::State),
                (
                    Want::Live {
                        state: LiveState::Running { e, .. },
                        ..
                    },
                    Tuple::Running { t0, e: was },
                ) => match (e, was) {
                    (Some(_), None) => Some(Class::State),
                    (Some(live_e), Some(was_e)) if drift => {
                        let allowed = OVERRUN_MIN_SECONDS.max(0.5 * (was_e - t0) as f64);
                        (live_e - was_e as f64 > allowed).then_some(Class::Drift)
                    }
                    _ => None,
                },
                (
                    Want::Live {
                        state: LiveState::Running { .. },
                        ..
                    },
                    _,
                ) => Some(Class::State),
                (
                    Want::Live {
                        state: LiveState::Claimed,
                        ..
                    },
                    // `q` → `c` is no change worth a write; `r` → `c` cannot happen.
                    _,
                ) => None,
                (
                    Want::Live {
                        state: LiveState::Queued { .. },
                        ..
                    },
                    Tuple::Running { .. } | Tuple::Claimed,
                ) => Some(Class::State),
                (
                    Want::Live {
                        state: LiveState::Queued { .. },
                        held: true,
                    },
                    Tuple::Queued(_),
                ) => None,
                (
                    Want::Live {
                        state: LiveState::Queued { e },
                        held: false,
                    },
                    Tuple::Queued(w),
                ) => match (e, w) {
                    (Some(_), None) | (None, Some(_)) => Some(Class::State),
                    (Some(e), Some(w)) if drift => {
                        let shown = clock.eta(w as f64);
                        ((shown - e).abs() > tol(shown - now)).then_some(Class::Drift)
                    }
                    _ => None,
                },
            };
            class = match (class, seen) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
    }
    class
}

/// Render the series' file (§5.2) under `clock` as revision `rev`: every owed job as
/// the scheduler says (a held row's waiting job keeps its published `w`), the jobs
/// finishing until the series is settled, and the `done` tail.
pub fn render(
    published: Option<&Published>,
    live: Option<&SeriesLive>,
    snap: &Snapshot,
    settled: bool,
    clock: &Clock,
    rev: u64,
) -> Published {
    let now = snap.now;
    let now_s = now.floor() as i64;
    let want = wanted(published, live, &snap.held_rows, settled, now);
    let rows = rows_for(&want, snap, published);
    let mut vols: BTreeMap<String, Vec<(String, Tuple)>> = BTreeMap::new();
    for (stem, jobs) in &want {
        let mut list: Vec<(String, Tuple)> = jobs
            .iter()
            .map(|(row, w)| {
                let tuple = match *w {
                    Want::Finishing { t0, e } => Tuple::Running { t0, e: Some(e) },
                    Want::Live { state, held } => match state {
                        LiveState::Running { t0, e } => Tuple::Running {
                            t0: t0.floor() as i64,
                            e: e.map(round_minute),
                        },
                        LiveState::Claimed => Tuple::Claimed,
                        LiveState::Queued { .. } if held => {
                            match published.and_then(|p| p.tuple(stem, row)) {
                                Some(Tuple::Queued(w)) => Tuple::Queued(w),
                                _ => Tuple::Queued(None),
                            }
                        }
                        LiveState::Queued { e } => {
                            Tuple::Queued(e.map(|e| round_minute(clock.position(e))))
                        }
                    },
                };
                (row.clone(), tuple)
            })
            .collect();
        let order = |row: &str| rows.iter().position(|r| r.id == row).unwrap_or(usize::MAX);
        list.sort_by_key(|(row, _)| order(row));
        vols.insert(stem.clone(), list);
    }
    let mut done = published.map(|p| p.done.clone()).unwrap_or_default();
    for stem in published.map(|p| p.vols.keys()).into_iter().flatten() {
        if !vols.contains_key(stem) {
            done.insert(stem.clone(), now_s);
        }
    }
    done.retain(|stem, at| !vols.contains_key(stem) && *at + DONE_KEEP_SECONDS > now_s);
    if done.len() > DONE_MAX {
        let mut by_age: Vec<(i64, String)> = done.iter().map(|(s, at)| (*at, s.clone())).collect();
        by_age.sort();
        for (_, stem) in by_age.iter().take(done.len() - DONE_MAX) {
            done.remove(stem);
        }
    }
    let folder = live
        .map(|l| l.folder.clone())
        .or_else(|| published.map(|p| p.folder.clone()))
        .unwrap_or_default();
    Published {
        folder,
        rev,
        at: now_s,
        clock_at: clock.at,
        rows,
        vols,
        done,
    }
}

/// `(points (w, e), span of every published w)` ([`fit_points`]).
pub type FitPoints = (Vec<(f64, f64)>, Option<(f64, f64)>);

/// The points and span a refit fits to: every waiting, timed job of a row not held
/// that has a published `w`, against its current predicted finish; the span covers
/// every published `w`.
pub fn fit_points<'a>(
    published: impl IntoIterator<Item = (&'a String, &'a Published)>,
    snap: &Snapshot,
) -> FitPoints {
    let mut points = Vec::new();
    let mut span: Option<(f64, f64)> = None;
    for (key, p) in published {
        let live = snap.series.get(key);
        for (stem, jobs) in &p.vols {
            for (row, tuple) in jobs {
                let Tuple::Queued(Some(w)) = tuple else {
                    continue;
                };
                let w = *w as f64;
                span = Some(span.map_or((w, w), |(a, b)| (a.min(w), b.max(w))));
                if snap.held_rows.contains(row) {
                    continue;
                }
                let e = live
                    .and_then(|l| l.vols.get(stem))
                    .and_then(|jobs| jobs.iter().find(|j| j.row == *row))
                    .and_then(|j| match j.state {
                        LiveState::Queued { e } => e,
                        _ => None,
                    });
                if let Some(e) = e {
                    points.push((w, e));
                }
            }
        }
    }
    (points, span)
}

// --- a sink without a budget -------------------------------------------------------------------------

/// What one [`Sink::refresh`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Refreshed {
    /// The clock was refitted and switched.
    pub refit: bool,
    /// Series rewritten, by fold key, with why.
    pub written: Vec<(String, Class)>,
    /// Series dropped (nothing owed, `done` tail expired).
    pub dropped: Vec<String>,
}

/// A sink's published state: its clock, its revision counter and every series it
/// published. [`Sink::refresh`] rewrites every dirty series at once (no budget).
#[derive(Clone, Debug)]
pub struct Sink {
    pub clock: Option<Clock>,
    /// The last revision handed out.
    pub rev: u64,
    pub series: BTreeMap<String, Published>,
    /// Seconds between refits (`None`: the caller refits, [`Sink::switch_clock`]).
    pub refit_every: Option<f64>,
}

impl Sink {
    /// `rev` continues from `rev_base` (a sink that keeps nothing across restarts
    /// starts from the time, so its revisions keep growing).
    pub fn new(rev_base: u64, refit_every: Option<f64>) -> Sink {
        Sink {
            clock: None,
            rev: rev_base,
            series: BTreeMap::new(),
            refit_every,
        }
    }

    /// The clock in use (the identity from `now` before the first).
    pub fn clock_at(&mut self, now: f64) -> &Clock {
        self.clock
            .get_or_insert_with(|| Clock::identity(now.floor() as i64))
    }

    /// Fit a new clock to what is published now, without switching to it: None while
    /// the whole queue is held or with fewer than 2 points.
    pub fn fit(&self, snap: &Snapshot) -> Option<Clock> {
        if snap.held.is_some() {
            return None;
        }
        let (points, span) = fit_points(&self.series, snap);
        Clock::fit(&points, span?, snap.now)
    }

    /// Bring every series up to `snap`: refit when due, then rewrite each series the
    /// dirty test names (judging drift right after a switch), and drop each that owes
    /// nothing and whose `done` tail expired. `settled(folder)`: the series has no
    /// write of its own left to land (§5.3).
    pub fn refresh(&mut self, snap: &Snapshot, settled: &dyn Fn(&str) -> bool) -> Refreshed {
        let mut out = Refreshed::default();
        let now = snap.now;
        let due = match (&self.clock, self.refit_every) {
            (Some(c), Some(every)) => now - c.at as f64 >= every,
            _ => false,
        };
        self.clock_at(now);
        if due && let Some(fitted) = self.fit(snap) {
            self.clock = Some(fitted);
            out.refit = true;
        }
        self.apply(snap, settled, out.refit, &mut out);
        out
    }

    /// Switch to `clock` (fitted elsewhere, confirmed), and judge drift under it.
    pub fn switch_clock(
        &mut self,
        clock: Clock,
        snap: &Snapshot,
        settled: &dyn Fn(&str) -> bool,
    ) -> Refreshed {
        self.clock = Some(clock);
        let mut out = Refreshed {
            refit: true,
            ..Default::default()
        };
        self.apply(snap, settled, true, &mut out);
        out
    }

    fn apply(
        &mut self,
        snap: &Snapshot,
        settled: &dyn Fn(&str) -> bool,
        drift: bool,
        out: &mut Refreshed,
    ) {
        let clock = self.clock_at(snap.now).clone();
        let keys: BTreeSet<String> = snap
            .series
            .keys()
            .chain(self.series.keys())
            .cloned()
            .collect();
        for key in keys {
            let live = snap.series.get(&key);
            let published = self.series.get(&key);
            let folder = live
                .map(|l| l.folder.as_str())
                .or(published.map(|p| p.folder.as_str()))
                .unwrap_or_default();
            // Asked only when a published job is no longer owed.
            let gone = published.is_some_and(|p| {
                p.vols.iter().any(|(stem, jobs)| {
                    jobs.iter().any(|(row, _)| {
                        !live
                            .and_then(|l| l.vols.get(stem))
                            .is_some_and(|l| l.iter().any(|j| j.row == *row))
                    })
                })
            });
            let is_settled = !gone || settled(folder);
            if let Some(class) = classify(published, live, snap, is_settled, &clock, drift) {
                self.rev += 1;
                let next = render(published, live, snap, is_settled, &clock, self.rev);
                self.series.insert(key.clone(), next);
                out.written.push((key, class));
            } else if live.is_none() && published.is_some_and(|p| p.expired(snap.now)) {
                self.series.remove(&key);
                out.dropped.push(key);
            }
        }
    }
}

#[cfg(test)]
mod tests;
