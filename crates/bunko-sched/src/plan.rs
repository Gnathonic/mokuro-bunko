//! The queue plan: a lane simulation that turns rates, the running cards and
//! the queue (in the scheduler's order) into a finishing time for every item
//! and for the whole queue (`eta.lanes_from_running`, `_price_running`,
//! `plan_queue`, `_earliest_finish_lane`).
//!
//! Cards and items are JSON objects (they are the queue page's and the
//! progress file's dicts); priced copies are returned with the 0.5.2 fields
//! added in the 0.5.2 order.
//!
//! **Removed:** 0.5.2's `startup_every_volume` callback (rows that ran the
//! one-volume mokuro command line paid their startup for every volume).
//! Every row now runs in a session, so a lane pays a row's startup only when
//! it switches to it.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::py::{
    Object, as_float, as_int, float_value, iso_utc, median_int, opt_float_value, py_str, round_int,
    round_to, truthy,
};
use crate::rate::{RateEstimate, RateModel, StartupEstimate, emission_rate, rate_key};

/// How fast a row reads, and starts, on a machine (`machine` None: a plan
/// that names no machines prices every lane alike, by row alone).
pub trait Pricing {
    fn rate_for(
        &self,
        generation_id: &str,
        machine: Option<&str>,
        observed_pages: i64,
        observed_seconds: f64,
    ) -> Option<RateEstimate>;
    fn startup_for(&self, generation_id: &str, machine: Option<&str>) -> StartupEstimate;
}

/// A machine's own saved benchmark of a row (its profile's `bench`), the
/// prior `_lane_pricing` hands the rate model.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MachineBench {
    pub pages_per_second: Option<f64>,
    pub startup_seconds: Option<f64>,
}

/// `_lane_pricing`: [`RateModel::rate_on`] / [`RateModel::startup_on`] keyed
/// by [`rate_key`], with each machine's benchmark as its prior. `bench` is
/// asked once per (row, machine) for the life of this value.
pub struct LanePricing<'a, B: Fn(&str, &str) -> Option<MachineBench>> {
    pub rates: &'a RateModel,
    bench: B,
    memo: std::cell::RefCell<HashMap<(String, String), Option<MachineBench>>>,
}

impl<'a, B: Fn(&str, &str) -> Option<MachineBench>> LanePricing<'a, B> {
    pub fn new(rates: &'a RateModel, bench: B) -> Self {
        LanePricing {
            rates,
            bench,
            memo: std::cell::RefCell::new(HashMap::new()),
        }
    }

    fn machine_bench(&self, generation_id: &str, machine: Option<&str>) -> Option<MachineBench> {
        let machine = machine?;
        let key = (generation_id.to_owned(), machine.to_owned());
        if let Some(found) = self.memo.borrow().get(&key) {
            return *found;
        }
        let found = (self.bench)(generation_id, machine);
        self.memo.borrow_mut().insert(key, found);
        found
    }
}

impl<B: Fn(&str, &str) -> Option<MachineBench>> Pricing for LanePricing<'_, B> {
    fn rate_for(
        &self,
        generation_id: &str,
        machine: Option<&str>,
        observed_pages: i64,
        observed_seconds: f64,
    ) -> Option<RateEstimate> {
        let prior = self
            .machine_bench(generation_id, machine)
            .and_then(|b| b.pages_per_second)
            .filter(|p| *p > 0.0)
            .map(RateEstimate::bench);
        let key = machine.map(|m| rate_key(generation_id, m));
        self.rates.rate_on(
            generation_id,
            key.as_deref(),
            prior.as_ref(),
            observed_pages,
            observed_seconds,
        )
    }

    fn startup_for(&self, generation_id: &str, machine: Option<&str>) -> StartupEstimate {
        let prior = self
            .machine_bench(generation_id, machine)
            .and_then(|b| b.startup_seconds);
        let key = machine.map(|m| rate_key(generation_id, m));
        self.rates.startup_on(generation_id, key.as_deref(), prior)
    }
}

/// One session slot: what it is serving, and when it frees.
#[derive(Clone, Debug, PartialEq)]
pub struct Lane {
    /// The row the lane is on right now (a change costs a startup).
    pub generation_id: Option<String>,
    pub free_in: f64,
    pub used: bool,
    /// Whose hardware this lane is (None when the plan names no machines).
    pub machine: Option<String>,
}

impl Lane {
    fn new(machine: Option<String>) -> Self {
        Lane {
            generation_id: None,
            free_in: 0.0,
            used: false,
            machine,
        }
    }
}

/// Everything one status poll needs to say when things will be done.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueuePlan {
    pub running: Vec<Object>,
    pub pending: Vec<Object>,
    pub done_in: Option<f64>,
    pub done_at: Option<String>,
}

/// A running card's lane-group key: its int `slot`, else `job-<index>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum SlotKey {
    Int(i64),
    Job(String),
}

impl SlotKey {
    fn sort_key(&self) -> (u8, String) {
        match self {
            SlotKey::Int(i) => (0, format!("{i:09}")),
            SlotKey::Job(s) => (1, s.clone()),
        }
    }
}

/// Lane row of a running card: `entry.get("generation_id") or previous`.
/// A truthy non-string id (never written by bunko) can match no row.
fn card_row(value: Option<&Value>) -> Option<Option<String>> {
    if !truthy(value) {
        return None;
    }
    Some(match value {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    })
}

/// `lanes_from_running`: the running cards as occupied lanes, each card
/// priced in place (cards of one slot share a lane and are chained).
pub fn lanes_from_running(
    running: &[Object],
    lane_count: usize,
    lane_machines: Option<&[String]>,
    pricing: &dyn Pricing,
    now: f64,
) -> (Vec<Lane>, Vec<Object>) {
    let lane_machines = lane_machines.filter(|m| !m.is_empty());
    let mut lanes: Vec<Lane> = match lane_machines {
        Some(machines) => machines
            .iter()
            .map(|m| Lane::new(Some(m.clone())))
            .collect(),
        None => (0..lane_count.max(1)).map(|_| Lane::new(None)).collect(),
    };
    let mut priced: Vec<Object> = running.to_vec();
    let mut order: Vec<SlotKey> = Vec::new();
    let mut by_slot: HashMap<SlotKey, Vec<usize>> = HashMap::new();
    for (index, entry) in priced.iter().enumerate() {
        let key = match entry.get("slot") {
            Some(Value::Number(n)) if n.is_i64() || n.is_u64() => {
                SlotKey::Int(n.as_i64().unwrap_or(i64::MAX))
            }
            Some(Value::Bool(b)) => SlotKey::Int(i64::from(*b)),
            _ => SlotKey::Job(format!("job-{index}")),
        };
        if !by_slot.contains_key(&key) {
            order.push(key.clone());
        }
        by_slot.entry(key).or_default().push(index);
    }
    order.sort_by_key(SlotKey::sort_key);

    let mut taken: HashSet<usize> = HashSet::new();
    for key in order {
        let jobs = &by_slot[&key];
        let wanted: Option<String> = jobs.iter().find_map(|i| {
            priced[*i]
                .get("machine")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        let position = match free_lane(&lanes, &taken, wanted.as_deref()) {
            Some(p) => p,
            None => {
                lanes.push(Lane::new(if lane_machines.is_some() {
                    wanted.clone()
                } else {
                    None
                }));
                lanes.len() - 1
            }
        };
        taken.insert(position);
        let machine: Option<String> = if lane_machines.is_some() {
            match &wanted {
                Some(w) if !w.is_empty() => Some(w.clone()),
                _ => lanes[position].machine.clone(),
            }
        } else {
            None
        };
        let mut cursor = 0.0;
        let mut known = true;
        let mut generation_id: Option<String> = None;
        for (index, &i) in jobs.iter().enumerate() {
            let entry = &mut priced[i];
            if let Some(row) = card_row(entry.get("generation_id")) {
                generation_id = row.or_else(|| Some("\u{0}non-string".to_owned()));
            }
            if !known {
                no_prediction(entry, "waiting behind a volume that cannot be timed");
                continue;
            }
            match price_running(entry, pricing, now, machine.as_deref(), cursor, index == 0) {
                Some(finish) => cursor = finish,
                None => known = false,
            }
        }
        let lane = &mut lanes[position];
        lane.generation_id = generation_id;
        lane.used = true;
        lane.free_in = if known { cursor } else { f64::INFINITY };
    }
    (lanes, priced)
}

fn free_lane(lanes: &[Lane], taken: &HashSet<usize>, machine: Option<&str>) -> Option<usize> {
    let free: Vec<usize> = (0..lanes.len()).filter(|i| !taken.contains(i)).collect();
    if let Some(m) = machine
        && let Some(i) = free
            .iter()
            .find(|i| lanes[**i].machine.as_deref() == Some(m))
    {
        return Some(*i);
    }
    free.first().copied()
}

/// `_price_running`: fill one running card's ETA fields in place; return
/// when its lane frees (seconds from now), None when it cannot be priced.
/// `offset` is how long the volumes ahead of it on the lane still need.
pub fn price_running(
    entry: &mut Object,
    pricing: &dyn Pricing,
    now: f64,
    machine: Option<&str>,
    offset: f64,
    charge_startup: bool,
) -> Option<f64> {
    let generation_id = if truthy(entry.get("generation_id")) {
        py_str(entry.get("generation_id"))
    } else {
        String::new()
    };
    let done = as_int(entry.get("done_pages")).unwrap_or(0);
    let total = as_int(entry.get("total_pages"));
    let first_page_at = as_float(entry.get("first_page_at")).filter(|f| *f != 0.0);
    let observed_seconds = first_page_at.map_or(0.0, |f| (now - f).max(0.0));
    let estimate = pricing.rate_for(&generation_id, machine, done, observed_seconds);
    entry.insert(
        "rate_pages_per_second".into(),
        opt_float_value(estimate.as_ref().map(|e| round_to(e.pages_per_second, 4))),
    );
    entry.insert(
        "latency_seconds".into(),
        opt_float_value(estimate.as_ref().map(|e| round_to(e.latency_seconds, 2))),
    );
    entry.insert(
        "rate_source".into(),
        estimate
            .as_ref()
            .map_or(Value::Null, |e| Value::String(e.source.clone())),
    );

    let status = entry.get("status").and_then(Value::as_str);
    if matches!(status, Some("error") | Some("done")) {
        entry.insert("eta_at".into(), Value::Null);
        return Some(offset);
    }

    if done <= 0 {
        let startup = pricing.startup_for(&generation_id, machine);
        let started_at = as_float(entry.get("session_started_at"))
            .filter(|f| *f != 0.0)
            .or_else(|| as_float(entry.get("started_at")))
            .filter(|f| *f != 0.0);
        let spent = started_at.map_or(0.0, |s| (now - s).max(0.0));
        let charge = charge_startup && entry.get("session_ready") != Some(&Value::Bool(true));
        let left = if charge {
            (startup.seconds - spent).max(0.0)
        } else {
            0.0
        };
        entry.insert("status".into(), Value::from("starting"));
        entry.insert(
            "startup_seconds".into(),
            if left >= 1.0 {
                Value::from(round_int(left))
            } else {
                Value::Null
            },
        );
        entry.insert("startup_rough".into(), Value::Bool(startup.rough));
        let (Some(estimate), Some(total)) = (estimate, total) else {
            entry.insert("eta_seconds".into(), Value::Null);
            entry.insert("eta_at".into(), Value::Null);
            return None;
        };
        let finish = offset + left + estimate.volume_seconds(total as f64);
        entry.insert("eta_seconds".into(), Value::from(round_int(finish)));
        entry.insert("eta_at".into(), Value::String(iso_utc(now + finish)));
        return Some(finish);
    }

    if let Some(total) = total
        && done >= total
    {
        entry.insert("status".into(), Value::from("finalizing"));
        entry.insert("percent".into(), Value::from(100));
        entry.insert("eta_seconds".into(), Value::from(0));
        entry.insert("eta_at".into(), Value::String(iso_utc(now + offset)));
        return Some(offset);
    }

    entry.insert("status".into(), Value::from("running"));
    let (Some(total), Some(estimate)) = (total, estimate) else {
        entry.insert("eta_seconds".into(), Value::Null);
        entry.insert("eta_at".into(), Value::Null);
        return None;
    };
    let finish = offset + estimate.seconds_for((total - done) as f64);
    entry.insert("eta_seconds".into(), Value::from(round_int(finish)));
    entry.insert("eta_at".into(), Value::String(iso_utc(now + finish)));
    Some(finish)
}

/// `_no_prediction`: no ETA, and why.
pub fn no_prediction(entry: &mut Object, reason: &str) {
    entry.insert("eta_seconds".into(), Value::Null);
    entry.insert("eta_at".into(), Value::Null);
    entry.insert("rate_source".into(), Value::Null);
    if !entry.contains_key("latency_seconds") {
        entry.insert("latency_seconds".into(), Value::Null);
    }
    entry.insert("reason".into(), Value::String(reason.to_owned()));
}

/// `_median_pages`: the middle page count of the items that have one.
pub fn median_pages(pending: &[Object]) -> Option<i64> {
    let known: Vec<i64> = pending
        .iter()
        .filter_map(|i| as_int(i.get("pages")))
        .filter(|p| *p > 0)
        .collect();
    median_int(&known)
}

/// `refusal_for(generation_id, machine)`: why that machine may not run the row.
pub type RefusalFn<'a> = &'a dyn Fn(&str, &str) -> Option<String>;
/// `hold_for(generation_id)`: why a row no lane may take is held.
pub type HoldFn<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The inputs of [`plan_queue`] besides the cards and items.
pub struct PlanInputs<'a> {
    /// Anonymous lanes when `lane_machines` is None or empty (at least 1).
    pub lane_count: usize,
    /// Whose hardware each lane is, in lane order.
    pub lane_machines: Option<&'a [String]>,
    pub pricing: &'a dyn Pricing,
    pub now: f64,
    /// Stop once the item at this index is priced (`done_at` is then None).
    pub through: Option<i64>,
    /// Why a machine may NOT run a row (its lanes never take that row).
    pub refusal_for: Option<RefusalFn<'a>>,
    /// Why a row no lane may take is held (else `"no connected machine can run <name>"`).
    pub hold_for: Option<HoldFn<'a>>,
}

/// `plan_queue`: price the running cards, then walk the queue across the
/// lanes, each item to the lane that finishes it first (else the lane that
/// frees first). An item that cannot be priced blocks everything behind it;
/// a held item (no lane may take its row) blocks nothing.
pub fn plan_queue(running: &[Object], pending: &[Object], inputs: &PlanInputs<'_>) -> QueuePlan {
    let pricing = inputs.pricing;
    let now = inputs.now;
    let (mut lanes, priced) = lanes_from_running(
        running,
        inputs.lane_count,
        inputs.lane_machines,
        pricing,
        now,
    );
    let median = median_pages(pending);
    let mut planned: Vec<Object> = Vec::new();
    let mut blocked_by: Option<String> = None;

    for (index, item) in pending.iter().enumerate() {
        if let Some(through) = inputs.through
            && index as i64 > through
        {
            break;
        }
        let mut entry = item.clone();
        let generation_id = if truthy(entry.get("generation_id")) {
            py_str(entry.get("generation_id"))
        } else {
            String::new()
        };
        let name = if truthy(entry.get("generation")) {
            py_str(entry.get("generation"))
        } else if !generation_id.is_empty() {
            generation_id.clone()
        } else {
            "that generation".to_owned()
        };
        let mut pages = as_int(entry.get("pages"));
        let mut rough = false;
        if pages.is_none_or(|p| p <= 0) {
            pages = median;
            rough = true;
        }
        entry.insert("pages".into(), pages.map_or(Value::Null, Value::from));
        entry.insert("rough".into(), Value::Bool(rough));

        if let Some(blocked) = &blocked_by {
            no_prediction(
                &mut entry,
                &format!("waiting behind a volume that cannot be timed: {blocked}"),
            );
            planned.push(entry);
            continue;
        }
        let open_lanes: Vec<usize> = (0..lanes.len())
            .filter(|i| match (inputs.refusal_for, &lanes[*i].machine) {
                (Some(refusal), Some(machine)) => refusal(&generation_id, machine).is_none(),
                _ => true,
            })
            .collect();
        if open_lanes.is_empty() {
            let reason = inputs
                .hold_for
                .and_then(|h| h(&generation_id))
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| format!("no connected machine can run {name}"));
            no_prediction(&mut entry, &reason);
            entry.insert("held".into(), Value::Bool(true));
            planned.push(entry);
            continue;
        }
        let lane_index = earliest_finish_lane(&lanes, &open_lanes, &generation_id, pages, pricing)
            .unwrap_or_else(|| {
                let mut best = open_lanes[0];
                for &i in &open_lanes[1..] {
                    if lanes[i].free_in < lanes[best].free_in {
                        best = i;
                    }
                }
                best
            });
        let lane_machine = lanes[lane_index].machine.clone();
        let Some(estimate) = pricing.rate_for(&generation_id, lane_machine.as_deref(), 0, 0.0)
        else {
            let why = format!("nothing has measured how fast {name} reads a page yet");
            no_prediction(&mut entry, &why);
            blocked_by = Some(why);
            planned.push(entry);
            continue;
        };
        let Some(pages) = pages else {
            let why = "no volume in the queue has a known page count".to_owned();
            no_prediction(&mut entry, &why);
            blocked_by = Some(why);
            planned.push(entry);
            continue;
        };
        let lane = &mut lanes[lane_index];
        if !lane.free_in.is_finite() {
            let why = "a volume already running has not said how long it is yet".to_owned();
            no_prediction(&mut entry, &why);
            blocked_by = Some(why);
            planned.push(entry);
            continue;
        }
        let mut start = lane.free_in;
        if lane.generation_id.as_deref() != Some(generation_id.as_str()) {
            start += pricing
                .startup_for(&generation_id, lane_machine.as_deref())
                .seconds;
        }
        let seconds = start + estimate.volume_seconds(pages as f64);
        lane.generation_id = Some(generation_id.clone());
        lane.free_in = seconds;
        lane.used = true;
        entry.insert("eta_seconds".into(), Value::from(round_int(seconds)));
        entry.insert("eta_at".into(), Value::String(iso_utc(now + seconds)));
        entry.insert("rate_source".into(), Value::String(estimate.source.clone()));
        entry.insert(
            "latency_seconds".into(),
            float_value(round_to(estimate.latency_seconds, 2)),
        );
        entry.insert("reason".into(), Value::Null);
        planned.push(entry);
    }

    let finished: Vec<f64> = lanes.iter().filter(|l| l.used).map(|l| l.free_in).collect();
    let truncated = inputs.through.is_some_and(|t| t + 1 < pending.len() as i64);
    if truncated
        || blocked_by.is_some()
        || finished.is_empty()
        || !finished.iter().all(|f| f.is_finite())
    {
        return QueuePlan {
            running: priced,
            pending: planned,
            done_in: None,
            done_at: None,
        };
    }
    let mut done_in = finished[0];
    for f in &finished[1..] {
        if *f > done_in {
            done_in = *f;
        }
    }
    QueuePlan {
        running: priced,
        pending: planned,
        done_in: Some(done_in),
        done_at: Some(iso_utc(now + done_in)),
    }
}

/// `_earliest_finish_lane`: among `open` (indices into `lanes`), the lane
/// that finishes `pages` of this row first (first minimum). None when that
/// cannot be said: no page count, a lane of unknown length, or a lane whose
/// machine has no rate for the row.
pub fn earliest_finish_lane(
    lanes: &[Lane],
    open: &[usize],
    generation_id: &str,
    pages: Option<i64>,
    pricing: &dyn Pricing,
) -> Option<usize> {
    let pages = pages?;
    if open.is_empty() {
        return None;
    }
    let mut best = None;
    let mut best_at = f64::INFINITY;
    for &i in open {
        let lane = &lanes[i];
        if !lane.free_in.is_finite() {
            return None;
        }
        let estimate = pricing.rate_for(generation_id, lane.machine.as_deref(), 0, 0.0)?;
        let mut start = lane.free_in;
        if lane.generation_id.as_deref() != Some(generation_id) {
            start += pricing
                .startup_for(generation_id, lane.machine.as_deref())
                .seconds;
        }
        let at = start + estimate.volume_seconds(pages as f64);
        if at < best_at {
            best = Some(i);
            best_at = at;
        }
    }
    best
}

/// `plan_items`' identity of a job: `(series, volume, generation_id)`.
pub fn job_identity(job: &Object) -> (Value, Value, Value) {
    let get = |k: &str| job.get(k).cloned().unwrap_or(Value::Null);
    (get("series"), get("volume"), get("generation_id"))
}

/// `OCRProcessor._progress_metrics(done, total, elapsed, rate)`:
/// `(percent, eta_seconds, status)` for a volume being read. `elapsed` is
/// the time since this volume's FIRST page emission.
pub fn progress_metrics(
    done: i64,
    total: i64,
    elapsed: f64,
    rate: Option<f64>,
) -> (Option<i64>, Option<i64>, &'static str) {
    if total <= 0 {
        return (None, None, if done <= 0 { "starting" } else { "running" });
    }
    if done >= total {
        return (Some(100), Some(0), "finalizing");
    }
    if done <= 0 {
        return (Some(0), None, "starting");
    }
    let percent = 99.min(((done as f64 / total as f64) * 100.0).trunc() as i64);
    let pages_per_second = match rate {
        Some(r) if r > 0.0 => Some(r),
        _ => emission_rate(done, elapsed),
    };
    let eta = pages_per_second
        .filter(|p| *p != 0.0)
        .map(|p| ((total - done) as f64 / p).trunc() as i64);
    (Some(percent), eta, "running")
}

/// What `_session_progress` writes into a running card on a page event.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionProgress<'a> {
    pub generation_id: &'a str,
    pub slot: i64,
    pub done_pages: i64,
    /// The volume's page count, 0 when not announced yet.
    pub total_pages: i64,
    pub first_page_at: Option<f64>,
    pub session_started_at: f64,
    pub session_ready: bool,
    pub delivered: bool,
    pub now: f64,
}

/// `_session_progress`'s card update: the estimate is `rate_on` that
/// machine's key with the volume in flight (`observed_*`) blended in.
pub fn session_progress_update(p: &SessionProgress<'_>, estimate: Option<&RateEstimate>) -> Object {
    let since_first = p
        .first_page_at
        .filter(|f| *f != 0.0)
        .map_or(0.0, |f| p.now - f);
    let (percent, eta, status) = progress_metrics(
        p.done_pages,
        p.total_pages,
        since_first,
        estimate.map(|e| e.pages_per_second),
    );
    let mut m = Map::new();
    m.insert("percent".into(), percent.map_or(Value::Null, Value::from));
    m.insert("eta_seconds".into(), eta.map_or(Value::Null, Value::from));
    m.insert("done_pages".into(), Value::from(p.done_pages));
    m.insert(
        "total_pages".into(),
        if p.total_pages != 0 {
            Value::from(p.total_pages)
        } else {
            Value::Null
        },
    );
    m.insert("status".into(), Value::from(status));
    m.insert("generation_id".into(), Value::from(p.generation_id));
    m.insert("slot".into(), Value::from(p.slot));
    m.insert("first_page_at".into(), opt_float_value(p.first_page_at));
    m.insert(
        "session_started_at".into(),
        float_value(p.session_started_at),
    );
    m.insert("session_ready".into(), Value::Bool(p.session_ready));
    m.insert("delivered".into(), Value::Bool(p.delivered));
    m.insert(
        "rate_pages_per_second".into(),
        opt_float_value(estimate.map(|e| round_to(e.pages_per_second, 4))),
    );
    m.insert(
        "latency_seconds".into(),
        opt_float_value(estimate.map(|e| round_to(e.latency_seconds, 2))),
    );
    m.insert(
        "rate_source".into(),
        estimate.map_or(Value::Null, |e| Value::String(e.source.clone())),
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_metrics_cases() {
        assert_eq!(progress_metrics(0, 0, 0.0, None), (None, None, "starting"));
        assert_eq!(progress_metrics(3, 0, 0.0, None), (None, None, "running"));
        assert_eq!(
            progress_metrics(10, 10, 0.0, None),
            (Some(100), Some(0), "finalizing")
        );
        assert_eq!(
            progress_metrics(0, 10, 0.0, None),
            (Some(0), None, "starting")
        );
        assert_eq!(
            progress_metrics(5, 10, 4.0, None),
            (Some(50), Some(5), "running")
        );
        assert_eq!(
            progress_metrics(1, 10, 4.0, None),
            (Some(10), None, "running")
        );
        assert_eq!(
            progress_metrics(5, 10, 4.0, Some(2.0)),
            (Some(50), Some(2), "running")
        );
    }

    struct Fixed;

    impl Pricing for Fixed {
        fn rate_for(&self, _: &str, machine: Option<&str>, _: i64, _: f64) -> Option<RateEstimate> {
            Some(RateEstimate::new(
                if machine == Some("fast") { 10.0 } else { 1.0 },
                "bench",
                0,
                0.0,
            ))
        }

        fn startup_for(&self, _: &str, _: Option<&str>) -> StartupEstimate {
            StartupEstimate::new(5.0, "bench")
        }
    }

    fn item(gen_id: &str, pages: i64) -> Object {
        serde_json::json!({"series": "S", "volume": "V", "generation": "Row", "generation_id": gen_id, "pages": pages})
            .as_object()
            .cloned()
            .unwrap()
    }

    #[test]
    fn held_rows_block_nothing_and_say_why() {
        let machines = vec!["slow".to_owned(), "fast".to_owned()];
        let refusal = |g: &str, _: &str| (g == "g-2").then(|| "no".to_owned());
        let hold = |g: &str| (g == "g-2").then(|| "No connected machine can run fp16".to_owned());
        let inputs = PlanInputs {
            lane_count: 1,
            lane_machines: Some(&machines),
            pricing: &Fixed,
            now: 0.0,
            through: None,
            refusal_for: Some(&refusal),
            hold_for: Some(&hold),
        };
        let plan = plan_queue(&[], &[item("g-2", 10), item("g-1", 100)], &inputs);
        assert_eq!(
            plan.pending[0]["reason"],
            "No connected machine can run fp16"
        );
        assert_eq!(plan.pending[0]["held"], true);
        // The 100-page volume goes to the fast lane: 5 s startup + 10 s.
        assert_eq!(plan.pending[1]["eta_seconds"], 15);
        assert_eq!(plan.pending[1]["eta_at"], "1970-01-01T00:00:15Z");
        assert_eq!(plan.done_in, Some(15.0));
    }
}
