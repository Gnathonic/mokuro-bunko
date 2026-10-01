//! Which machine takes a volume: earliest-finish list scheduling
//! (`eta.earliest_finish_claim`, plus the pure parts of the worker's
//! `_eft_lanes`, `_eft_left` and `_rows_left_to_warm`).
//!
//! Every slot of the running scan is a lane. When a lane asks for work the
//! head of the queue is walked and each volume booked on the lane that would
//! FINISH it first; the asking lane takes the first volume booked on itself,
//! and keeps any volume it would finish within 10 % (≤ 5 s) + 2 s of the best
//! other lane.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use crate::py::median_int;
use crate::rate::RateEstimate;

/// The asker keeps a volume within this share of the best other lane…
pub const EFT_MARGIN: f64 = 0.10;
/// …capped at this many seconds…
pub const EFT_MARGIN_CAP_SECONDS: f64 = 5.0;
/// …plus this slack.
pub const EFT_SLACK_SECONDS: f64 = 2.0;
/// Jobs walked per claim.
pub const EFT_LOOKAHEAD: usize = 256;
/// Grace after the predicted start before anyone may take a left volume.
pub const EFT_CLAIM_GRACE_SECONDS: f64 = 15.0;

/// One slot that could take a volume, as the walk sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct EftLane<K> {
    pub key: K,
    pub machine: String,
    /// When its current work ends (seconds from now; 0 = idle).
    pub free_in: f64,
    /// The row its open session serves (no startup to read more of it).
    pub warm: Option<String>,
    /// The rows it may be given at all.
    pub rows: HashSet<String>,
}

/// A volume the asking lane leaves to a lane that finishes it sooner.
#[derive(Clone, Debug, PartialEq)]
pub struct EftLeft<J, K> {
    pub job: J,
    pub to: K,
    /// Seconds from now until it would be done on `to`…
    pub there: f64,
    /// …and on the asking lane.
    pub here: f64,
    /// Seconds from now until `to` would start it (startup included).
    pub starts: f64,
    /// Whether `to` has work to finish first.
    pub busy_first: bool,
}

/// `mine`: the first volume the walk gives the asking lane; `left`: the
/// volumes before it the asking lane could run, booked elsewhere.
#[derive(Clone, Debug, PartialEq)]
pub struct EftDecision<J, K> {
    pub mine: Option<J>,
    pub left: Vec<EftLeft<J, K>>,
}

/// The walk's tunables (the 0.5.2 defaults via [`Default`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EftParams {
    pub margin: f64,
    pub margin_cap: f64,
    pub slack: f64,
    pub limit: usize,
}

impl Default for EftParams {
    fn default() -> Self {
        EftParams {
            margin: EFT_MARGIN,
            margin_cap: EFT_MARGIN_CAP_SECONDS,
            slack: EFT_SLACK_SECONDS,
            limit: EFT_LOOKAHEAD,
        }
    }
}

/// `earliest_finish_claim(jobs, lanes, asking, rate_for, startup_for)`.
///
/// `jobs` are `(job, row id, pages)` in the scheduler's order. None when the
/// walk cannot be priced (a lane that may take a row has no rate for it, or
/// an unknown page count with no known one to take the median of) — the
/// caller then keeps plain first-come.
pub fn earliest_finish_claim<J, K, R, S>(
    jobs: &[(J, String, Option<i64>)],
    lanes: &[EftLane<K>],
    asking: &K,
    rate_for: R,
    startup_for: S,
    params: EftParams,
) -> Option<EftDecision<J, K>>
where
    J: Clone,
    K: Clone + Eq + Hash,
    R: Fn(&str, &str) -> Option<RateEstimate>,
    S: Fn(&str, &str) -> f64,
{
    let known: Vec<i64> = jobs
        .iter()
        .filter_map(|(_, _, p)| *p)
        .filter(|p| *p > 0)
        .collect();
    let median = median_int(&known);
    lanes.iter().find(|l| l.key == *asking)?;
    let mut state: HashMap<K, (f64, Option<String>)> = lanes
        .iter()
        .map(|l| (l.key.clone(), (l.free_in, l.warm.clone())))
        .collect();
    let mut decision = EftDecision {
        mine: None,
        left: Vec::new(),
    };
    let mut rates: HashMap<(String, String), Option<RateEstimate>> = HashMap::new();

    for (job, generation_id, pages) in jobs.iter().take(params.limit) {
        let pages = match pages {
            Some(p) if *p > 0 => *p,
            _ => median?,
        };
        let mut best: Option<&EftLane<K>> = None;
        let mut best_at = f64::INFINITY;
        let mut best_starts = 0.0;
        let mut best_busy = false;
        let mut mine_at: Option<f64> = None;
        for item in lanes {
            if !item.rows.contains(generation_id) {
                continue;
            }
            let key = (generation_id.clone(), item.machine.clone());
            let rate = rates
                .entry(key)
                .or_insert_with(|| rate_for(generation_id, &item.machine))
                .clone()?;
            let (free_in, warm) = &state[&item.key];
            let start = if warm.as_deref() == Some(generation_id.as_str()) {
                *free_in
            } else {
                free_in + startup_for(generation_id, &item.machine)
            };
            let at = start + rate.volume_seconds(pages as f64);
            if item.key == *asking {
                mine_at = Some(at);
            } else if at < best_at {
                best = Some(item);
                best_at = at;
                best_starts = start;
                best_busy = state[&item.key].0 > 0.0;
            }
        }
        if let Some(mine) = mine_at
            && (best.is_none()
                || mine
                    <= best_at + (best_at * params.margin).min(params.margin_cap) + params.slack)
        {
            decision.mine = Some(job.clone());
            return Some(decision);
        }
        let Some(best) = best else { continue };
        if let Some(here) = mine_at {
            decision.left.push(EftLeft {
                job: job.clone(),
                to: best.key.clone(),
                there: best_at,
                here,
                starts: best_starts,
                busy_first: best_busy,
            });
        }
        state.insert(best.key.clone(), (best_at, Some(generation_id.clone())));
    }
    Some(decision)
}

/// One claimed in-flight volume of a lane, for pricing its `free_in`.
#[derive(Clone, Debug, PartialEq)]
pub struct InflightCard {
    pub generation_id: String,
    /// The card's `eta_seconds` when numeric.
    pub eta_seconds: Option<f64>,
    /// The card's `total_pages`, else the archive's known page count.
    pub total_pages: Option<i64>,
    pub done_pages: Option<i64>,
}

/// `_eft_lanes`' `free_in` of one lane: Σ over its in-flight volumes of the
/// card's ETA, else the machine's rate × remaining pages (fill included).
/// None when a volume can be priced neither way (EFT then stands down).
pub fn lane_free_in(
    owned: &[InflightCard],
    rate_for: impl Fn(&str) -> Option<RateEstimate>,
) -> Option<f64> {
    let mut free_in = 0.0;
    for card in owned {
        let eta = match card.eta_seconds {
            Some(eta) => eta,
            None => {
                let rate = rate_for(&card.generation_id);
                let total = card.total_pages.filter(|t| *t != 0);
                let (Some(rate), Some(total)) = (rate, total) else {
                    return None;
                };
                let done = card.done_pages.unwrap_or(0);
                rate.volume_seconds((total - done).max(0) as f64)
            }
        };
        free_in += eta.max(0.0);
    }
    Some(free_in)
}

/// `_eft_left`'s deadline rule for one left volume: a lane with work first
/// follows the moving prediction (never shrinks); an idle one gets a fixed
/// clock (never pushed back). `predicted = now + max(0, starts) + 15`.
pub fn eft_deadline(now: f64, starts: f64, busy_first: bool, known: Option<f64>) -> f64 {
    let predicted = now + starts.max(0.0) + EFT_CLAIM_GRACE_SECONDS;
    match known {
        None => predicted,
        Some(k) if busy_first => k.max(predicted),
        Some(k) => k.min(predicted),
    }
}

/// What applying a decision's left volumes yields.
#[derive(Clone, Debug)]
pub struct EftLeftOutcome<J, K> {
    /// Volumes to pass over now (their deadline has not run out).
    pub skip: HashSet<J>,
    /// The left volumes still within their deadline (for the log).
    pub kept: Vec<EftLeft<J, K>>,
    /// Some volume was left for the first time (wake the machines).
    pub fresh: bool,
}

/// The worker's `_eft_deadlines` map: how long a volume left to a faster
/// lane waits for it.
#[derive(Clone, Debug, Default)]
pub struct EftDeadlines<J: Eq + Hash> {
    deadlines: HashMap<J, f64>,
}

impl<J: Clone + Eq + Hash> EftDeadlines<J> {
    pub fn new() -> Self {
        EftDeadlines {
            deadlines: HashMap::new(),
        }
    }

    pub fn get(&self, job: &J) -> Option<f64> {
        self.deadlines.get(job).copied()
    }

    /// A claimed volume's deadline is dropped.
    pub fn forget(&mut self, job: &J) {
        self.deadlines.remove(job);
    }

    /// Drop deadlines of jobs no longer proposed, then set/refresh one per
    /// left volume and report which to skip.
    pub fn apply<K: Clone>(
        &mut self,
        proposed: &HashSet<J>,
        left: &[EftLeft<J, K>],
        now: f64,
    ) -> EftLeftOutcome<J, K> {
        self.deadlines.retain(|job, _| proposed.contains(job));
        let mut skip = HashSet::new();
        let mut kept = Vec::new();
        let mut fresh = false;
        for item in left {
            let known = self.deadlines.get(&item.job).copied();
            fresh = fresh || known.is_none();
            let deadline = eft_deadline(now, item.starts, item.busy_first, known);
            self.deadlines.insert(item.job.clone(), deadline);
            if now < deadline {
                skip.insert(item.job.clone());
                kept.push(item.clone());
            }
        }
        EftLeftOutcome { skip, kept, fresh }
    }
}

/// `_rows_left_to_warm`, step 1: the rows of `proposed` whose sessions are
/// warm elsewhere (`warm_rows`) and that have exactly ONE job this machine
/// could take (not in `unavailable`), with that job.
pub fn single_volume_rows<J: Clone + Eq + Hash>(
    proposed: &[(J, String)],
    warm_rows: &HashSet<String>,
    unavailable: &HashSet<J>,
) -> Vec<(String, J)> {
    let mut order: Vec<String> = Vec::new();
    let mut open: HashMap<String, Vec<J>> = HashMap::new();
    for (job, gen_id) in proposed {
        if warm_rows.contains(gen_id) && !unavailable.contains(job) {
            if !open.contains_key(gen_id) {
                order.push(gen_id.clone());
            }
            open.entry(gen_id.clone()).or_default().push(job.clone());
        }
    }
    order
        .into_iter()
        .filter_map(|g| {
            let jobs = open.remove(&g)?;
            if jobs.len() == 1 {
                jobs.into_iter().next().map(|j| (g, j))
            } else {
                None
            }
        })
        .collect()
}

/// A machine with a warm session of the row, as `_rows_left_to_warm` prices it.
#[derive(Clone, Debug, PartialEq)]
pub struct WarmMachine {
    pub machine: String,
    pub rate: Option<RateEstimate>,
    /// Its in-flight volumes of that row (card ETA, else rate × remaining).
    pub ahead: Vec<InflightCard>,
}

/// The verdict of [`warm_session_wins`]: leave the row to `machine`.
#[derive(Clone, Debug, PartialEq)]
pub struct WarmLeave {
    pub machine: String,
    /// Seconds until the warm session would have read it.
    pub theirs: f64,
    /// Seconds opening a session here would take.
    pub mine: f64,
}

/// `_rows_left_to_warm`, step 2, for one single-volume row: `mine = startup
/// here + my volume_seconds(pages)`; each warm machine `theirs = Σ its
/// in-flight remaining + its volume_seconds(pages)`; the first warm machine
/// with `theirs < mine` wins. Only on real evidence: `pages` (metadata
/// cache count) and both rates.
pub fn warm_session_wins(
    pages: Option<i64>,
    mine_rate: Option<&RateEstimate>,
    mine_startup_seconds: f64,
    warm: &[WarmMachine],
) -> Option<WarmLeave> {
    let pages = pages.filter(|p| *p != 0)?;
    let mine_rate = mine_rate?;
    let mine = mine_startup_seconds + mine_rate.volume_seconds(pages as f64);
    for candidate in warm {
        let Some(rate) = &candidate.rate else {
            continue;
        };
        let mut ahead = 0.0;
        for card in &candidate.ahead {
            let eta = card.eta_seconds.unwrap_or_else(|| {
                let total = card.total_pages.unwrap_or(0);
                let done = card.done_pages.unwrap_or(0);
                rate.volume_seconds((total - done).max(0) as f64)
            });
            ahead += eta.max(0.0);
        }
        let theirs = ahead + rate.volume_seconds(pages as f64);
        if theirs < mine {
            return Some(WarmLeave {
                machine: candidate.machine.clone(),
                theirs,
                mine,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(key: u32, machine: &str, free_in: f64, rows: &[&str]) -> EftLane<u32> {
        EftLane {
            key,
            machine: machine.into(),
            free_in,
            warm: None,
            rows: rows.iter().map(|r| r.to_string()).collect(),
        }
    }

    #[test]
    fn lone_volume_goes_to_the_fast_machine() {
        let lanes = vec![lane(1, "slow", 0.0, &["g"]), lane(2, "fast", 0.0, &["g"])];
        let jobs = vec![("v1", "g".to_owned(), Some(100))];
        let rate = |_: &str, m: &str| {
            Some(RateEstimate::new(
                if m == "fast" { 10.0 } else { 1.0 },
                "x",
                0,
                0.0,
            ))
        };
        let d = earliest_finish_claim(&jobs, &lanes, &1, rate, |_, _| 5.0, EftParams::default())
            .unwrap();
        assert_eq!(d.mine, None);
        assert_eq!(d.left.len(), 1);
        assert_eq!(d.left[0].to, 2);
        let d = earliest_finish_claim(&jobs, &lanes, &2, rate, |_, _| 5.0, EftParams::default())
            .unwrap();
        assert_eq!(d.mine, Some("v1"));
    }

    #[test]
    fn deadlines_follow_or_fix() {
        assert_eq!(eft_deadline(100.0, 10.0, true, None), 125.0);
        assert_eq!(eft_deadline(100.0, 10.0, true, Some(130.0)), 130.0);
        assert_eq!(eft_deadline(100.0, 10.0, false, Some(110.0)), 110.0);
        assert_eq!(eft_deadline(100.0, -5.0, false, None), 115.0);
    }
}
