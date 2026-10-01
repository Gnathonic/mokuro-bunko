//! The claim (spec ocr-scheduling §7): which job a lane takes, decided in one place.
//! Phase A refuses cheaply, phase B proposes the queue in order and prices the walk
//! (earliest-finish, warm sessions, backoffs, autobench gating), phase C takes.

use std::collections::{BTreeMap, HashMap, HashSet};

use bunko_core::generations::Generation;
use bunko_proto::{PoolsSpec, RowSpec};
use bunko_sched::breaker::{BackoffState, backoff_state};
use bunko_sched::eft::{EftLane, EftParams, InflightCard, WarmMachine, earliest_finish_claim, lane_free_in, single_volume_rows, warm_session_wins};
use bunko_sched::failures::{Eligibility, eligibility, failure_key};
use bunko_sched::job_order::{JobKey, order_jobs};
use bunko_sched::plan::{LanePricing, MachineBench, Pricing};
use serde_json::{Map, Value};

use super::{Claimed, Lane, Scheduler};
use crate::ocr::profiles::{machine_pools, profile_key, runner_pools};
use crate::ocr::types::{Job, LOCAL, stamp_of};

/// The pseudo row id upgrade jobs are ordered under: after every real row.
pub const UPGRADE_ORDER_KEY: &str = "\u{10FFFF}upgrade";

/// What phase B decided about the earliest-finish walk.
#[derive(Default, Debug)]
pub struct EftOutcome {
    pub mine: Option<Job>,
    pub skip: HashSet<Job>,
}

/// `supported_for(device)` on a processor's reported devices: what the model device
/// computes in. `None`: the card is not reported.
pub fn supported_for(catalog: &bunko_proto::Catalog, device: &str) -> Option<HashSet<String>> {
    let fp32 = || HashSet::from(["fp32".to_string()]);
    let with = |formats: &[String]| {
        let mut s = fp32();
        s.extend(formats.iter().cloned());
        s
    };
    match device {
        "cpu" => Some(fp32()),
        "" | "auto" => match catalog.devices.iter().find(|d| d.id.starts_with("gpu:")) {
            Some(gpu) => Some(with(&gpu.formats)),
            None => Some(fp32()),
        },
        id => catalog.devices.iter().find(|d| d.id == id).map(|d| with(&d.formats)),
    }
}

/// `row_refusal`: a forced precision the machine's model device cannot run.
pub fn precision_refusal(row: &Generation, catalog: &bunko_proto::Catalog, stage_device: &Map<String, Value>) -> Option<String> {
    if !row.precision_applies() || !bunko_core::engines::is_forced_precision(&row.precision) {
        return None;
    }
    let device = stage_device.get("engine").and_then(Value::as_str).unwrap_or("auto");
    let mode = &row.precision;
    match supported_for(catalog, device) {
        Some(s) if s.contains(mode.as_str()) => None,
        Some(_) => Some(format!("it cannot run {mode} ({mode} not supported)")),
        None if mode == "fp32" => None,
        None => Some(format!("it cannot run {mode} (card not reported)")),
    }
}

/// `catalog_can_run`: None when the machine can run the row as it would run it.
pub fn catalog_can_run(catalog: &bunko_proto::Catalog, row: &Generation, stage_device: &Map<String, Value>) -> Option<String> {
    if !catalog.engines.contains(&row.engine) {
        return Some(format!("{} is not installed on this processor", row.engine));
    }
    let detector = row.effective_detector();
    if !detector.is_empty() && !catalog.detectors.iter().any(|d| d == detector) {
        return Some(format!("the {detector} detector is not installed on this processor"));
    }
    let known: HashSet<&str> = catalog.devices.iter().map(|d| d.id.as_str()).collect();
    let mut pins: Vec<(&String, &Value)> = stage_device.iter().collect();
    pins.sort_by(|a, b| a.0.cmp(b.0));
    for (stage, device) in pins {
        let device = device.as_str().unwrap_or("");
        if !["auto", "cpu", ""].contains(&device) && !known.contains(device) {
            return Some(format!("{stage} is pinned to {device}, which this processor does not report"));
        }
    }
    precision_refusal(row, catalog, stage_device)
}

/// `resolve_device(asked, gpu)`: what `auto` means on a machine.
fn resolve_device(asked: &str, has_gpu: bool) -> String {
    match asked {
        "" | "auto" => {
            if has_gpu {
                "gpu:0".into()
            } else {
                "cpu".into()
            }
        }
        other => other.to_string(),
    }
}

impl Scheduler {
    // --- candidates -----------------------------------------------------------------------

    /// `(series, volume, generation id)` as `order_jobs` reads it.
    pub fn job_key(job: &Job) -> JobKey {
        JobKey {
            series: job.series().to_string(),
            volume: job.volume().to_string(),
            generation_id: if job.upgrade { UPGRADE_ORDER_KEY.to_string() } else { job.gid.to_string() },
        }
    }

    pub fn failure_key_of(&self, job: &Job, row: &Generation) -> String {
        if job.upgrade { format!("{}@upgrade", job.rel) } else { failure_key(&job.rel, row.primary, &row.name) }
    }

    /// `_ocr_candidates`: runnable jobs (row live, not in failure backoff, not excluded),
    /// in processing order. `reset_stale` drops records a replaced archive made void.
    pub fn candidates(&mut self, exclude: &HashSet<Job>, reset_stale: bool) -> Vec<Job> {
        let now = self.now();
        let poll = self.settings.poll_interval;
        let primary = self.primary().map(|p| p.id.clone());
        let mut jobs = Vec::new();
        let mut stale: Vec<String> = Vec::new();
        for (rel, vol) in &self.owed.volumes {
            let mut owed: Vec<Job> = vol.rows.iter().map(|g| Job { rel: rel.as_str().into(), gid: g.clone(), upgrade: false }).collect();
            if vol.upgrade
                && let Some(p) = &primary
                && !vol.rows.iter().any(|g| &**g == p)
            {
                owed.push(Job::upgrade(rel, p));
            }
            for job in owed {
                let Some(row) = self.row(&job.gid) else { continue };
                let key = self.failure_key_of(&job, row);
                let record = self.failures.get(&key).and_then(Value::as_object);
                match eligibility(record, Some(vol.mtime), now, poll) {
                    Eligibility::BackedOff => continue,
                    Eligibility::Replaced => stale.push(key),
                    _ => {}
                }
                if !exclude.contains(&job) {
                    jobs.push(job);
                }
            }
        }
        if reset_stale && !stale.is_empty() {
            for key in &stale {
                self.failures.shift_remove(key);
            }
            self.save_failures();
        }
        let mut rank = self.rank();
        rank.insert(UPGRADE_ORDER_KEY.to_string(), rank.len());
        order_jobs(jobs, &rank, Self::job_key, &self.last_served)
    }

    /// A job is no longer owed (taken, finished, or found done at claim).
    pub fn forget_candidate(&mut self, job: &Job) {
        if let Some(vol) = self.owed.volumes.get_mut(&*job.rel) {
            if job.upgrade {
                vol.upgrade = false;
            } else {
                vol.rows.retain(|g| *g != job.gid);
            }
            if vol.rows.is_empty() && !vol.upgrade && vol.skipped.is_empty() {
                self.owed.volumes.remove(&*job.rel);
            }
        }
    }

    /// The pages the queue knows for a volume (cache count, else the zip's own).
    pub fn known_pages(&self, rel: &str) -> Option<i64> {
        if let Some(p) = self.owed.volumes.get(rel).and_then(|v| v.pages) {
            return Some(p);
        }
        self.deps.facts.page_count(&self.library().join(rel))
    }

    // --- what a machine can run --------------------------------------------------------------

    /// The machine's effective pools for a row: its stored pools over the row's own.
    pub fn machine_row_pools(&self, machine_name: &str, row: &Generation) -> Map<String, Value> {
        let stored = self.profiles.row(profile_key(machine_name), &row.id, Some(&row.output_affecting())).map(|p| p.pools).unwrap_or_default();
        runner_pools(&machine_pools(&stored, &row.pools.to_value()))
    }

    /// `_slot_refusal`: why this machine may not be offered the row.
    pub fn refusal(&self, pid: &str, row: &Generation) -> Option<String> {
        let machine = self.machines.get(pid)?;
        if machine.installing() {
            return Some("it is still installing".into());
        }
        let pools = self.machine_row_pools(&machine.name, row);
        let stage_device = pools.get("stage_device").and_then(Value::as_object).cloned().unwrap_or_default();
        catalog_can_run(&machine.catalog, row, &stage_device)
    }

    /// The row as that machine runs it (its pools, the row's precision).
    pub fn row_spec(&self, machine_name: &str, row: &Generation) -> RowSpec {
        let pools = self.machine_row_pools(machine_name, row);
        let table_u32 = |key: &str| -> BTreeMap<String, u32> {
            pools.get(key).and_then(Value::as_object).map(|t| t.iter().filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n as u32))).collect()).unwrap_or_default()
        };
        let stage_device: BTreeMap<String, String> = pools
            .get("stage_device")
            .and_then(Value::as_object)
            .map(|t| t.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
            .unwrap_or_default();
        RowSpec {
            id: row.id.clone(),
            name: row.name.clone(),
            engine: row.engine.clone(),
            detector: row.detector.clone(),
            patch_budget: row.patch_budget,
            precision: row.precision.clone(),
            pools: PoolsSpec { stage_workers: table_u32("stage_workers"), queue_capacity: table_u32("queue_capacity"), stage_device },
            precision_pick: None,
            precision_why: String::new(),
            primary: row.primary,
        }
    }

    /// The device the row's model runs on, on that machine.
    pub fn engine_device(&self, pid: &str, row: &Generation) -> String {
        let Some(machine) = self.machines.get(pid) else { return "cpu".into() };
        let pools = self.machine_row_pools(&machine.name, row);
        let asked = pools.get("stage_device").and_then(|t| t.get("engine")).and_then(Value::as_str).unwrap_or("auto").to_string();
        resolve_device(&asked, machine.has_gpu())
    }

    pub fn breaker_open(&self, pid: &str) -> bool {
        let now = self.now();
        self.breakers.get(pid).is_some_and(|b| b.is_open(now))
    }

    pub fn held(&self, machine_name: &str) -> bool {
        self.holds.get(machine_name).copied().unwrap_or(0) > 0
    }

    /// `_backed_off_rows`: rows whose runner would not start here lately.
    pub fn backed_off_rows(&mut self, pid: &str, rows: &[String]) -> HashSet<String> {
        let Some(name) = self.machines.get(pid).map(|m| m.name.clone()) else { return HashSet::new() };
        let now = self.now();
        let mut out = HashSet::new();
        for gid in rows {
            let key = (gid.clone(), name.clone());
            let Some(backoff) = self.start_backoff.get(&key).cloned() else { continue };
            let Some(row) = self.row(gid).cloned() else { continue };
            let signature = self.start_signature(pid, &row);
            match backoff_state(&backoff, &signature, now) {
                BackoffState::Void => {
                    self.start_backoff.remove(&key);
                }
                BackoffState::Waiting => {
                    out.insert(gid.clone());
                }
                BackoffState::Expired => {}
            }
        }
        out
    }

    /// The row as that machine would run it, hashed with the machine: a change of
    /// either retries a backed-off start at once.
    pub fn start_signature(&self, pid: &str, row: &Generation) -> String {
        use sha2::Digest;
        let name = self.machines.get(pid).map(|m| m.name.clone()).unwrap_or_else(|| LOCAL.into());
        let spec = serde_json::to_string(&self.row_spec(&name, row)).unwrap_or_default();
        let digest = hex::encode(sha2::Sha256::digest(format!("{spec}\0{pid}").as_bytes()));
        digest[..16].to_string()
    }

    /// `autobench_kind`: `"full"` when the pair was never measured on this machine.
    pub fn autobench_kind(&self, pid: &str, row: &Generation) -> Option<&'static str> {
        if !self.settings.autobench {
            return None;
        }
        let machine = self.machines.get(pid)?;
        if machine.local && !self.settings.local_processing {
            return None;
        }
        let key = (profile_key(&machine.name).to_string(), row.id.clone());
        if self.autobench.failed.contains(&key) {
            return None;
        }
        let hand_set = machine.local && !row.pools.is_empty();
        if hand_set {
            return None;
        }
        match self.profiles.row(profile_key(&machine.name), &row.id, Some(&row.output_affecting())) {
            None => Some("full"),
            Some(_) => None,
        }
    }

    pub fn want_autobench(&mut self, pid: &str, row: &Generation) {
        let Some(name) = self.machines.get(pid).map(|m| m.name.clone()) else { return };
        let key = (profile_key(&name).to_string(), row.id.clone());
        if self.autobench.asked.contains(&key) || self.autobench.inflight.contains(&key) {
            return;
        }
        self.autobench.asked.insert(key.clone());
        self.autobench.inflight.insert(key.clone());
        self.autobench.wanted.push(key);
    }

    /// The machine's saved benchmark of a row (its rate/startup prior).
    pub fn machine_bench(&self, gid: &str, machine: &str) -> Option<MachineBench> {
        let row = self.settings.rows.iter().find(|r| r.id == gid)?;
        let profile = self.profiles.row(profile_key(machine), gid, Some(&row.output_affecting()))?;
        Some(MachineBench { pages_per_second: profile.bench_pages_per_second(), startup_seconds: profile.bench_startup_seconds() })
    }

    pub fn pricing(&self) -> LanePricing<'_, impl Fn(&str, &str) -> Option<MachineBench> + '_> {
        LanePricing::new(&self.rates, move |g: &str, m: &str| self.machine_bench(g, m))
    }

    // --- the claim ----------------------------------------------------------------------

    /// `claim_for_session(slot, generation_id)` / `claim_next(slot)`: `(job, preempt)`.
    pub fn claim(&mut self, lane_id: u64, gid: Option<&str>) -> (Option<Job>, bool) {
        let Some(lane) = self.lanes.iter().find(|l| l.id == lane_id).cloned() else { return (None, false) };
        let Some(mname) = self.machines.get(&lane.pid).map(|m| m.name.clone()) else { return (None, false) };
        let pid = lane.pid.clone();
        // Phase A.
        if self.stopping || self.held(&mname) || self.breaker_open(&pid) {
            return (None, false);
        }
        let unavailable: HashSet<Job> = self.claims.keys().cloned().chain(self.attempted.iter().cloned()).collect();
        // Phase B.
        let proposed = self.candidates(&unavailable, true);
        let mut offered: HashMap<String, bool> = HashMap::new();
        let mut walked_rows: Vec<String> = Vec::new();
        for job in &proposed {
            if !offered.contains_key(&*job.gid) {
                walked_rows.push(job.gid.to_string());
                let ok = self.row(&job.gid).is_some_and(|r| self.refusal(&pid, r).is_none());
                offered.insert(job.gid.to_string(), ok);
            }
        }
        let mut unmeasured: HashSet<String> = HashSet::new();
        for g in &walked_rows {
            if offered.get(g) == Some(&true)
                && let Some(row) = self.row(g)
                && self.autobench_kind(&pid, row).is_some()
            {
                unmeasured.insert(g.clone());
            }
        }
        let backed_off = self.backed_off_rows(&pid, &walked_rows);
        let left_to_warm = if gid.is_none() { self.rows_left_to_warm(&lane, &proposed) } else { HashSet::new() };
        let eft = self.eft_left(&lane, &proposed);
        // Phase C.
        let rank = self.rank();
        let rank_of = |j: &Job| if j.upgrade { rank.len() } else { rank.get(&*j.gid).copied().unwrap_or(rank.len()) };
        let mut preempt = false;
        if let Some(g) = gid {
            let mine = rank.get(g).copied().unwrap_or(rank.len());
            preempt = proposed.iter().any(|j| {
                rank_of(j) < mine
                    && !self.stopped.contains(&(j.gid.to_string(), mname.clone()))
                    && !backed_off.contains(&*j.gid)
                    && !unmeasured.contains(&*j.gid)
                    && offered.get(&*j.gid) == Some(&true)
            });
        }
        let mut ordered = proposed.clone();
        if gid.is_none() {
            let busy: HashSet<String> = self
                .sessions
                .values()
                .filter(|s| s.pid == pid && s.lane != lane_id && !s.closing)
                .map(|s| self.engine_device(&pid, &s.row))
                .collect();
            if !busy.is_empty() {
                let (free, taken): (Vec<Job>, Vec<Job>) = ordered.into_iter().partition(|j| {
                    self.row(&j.gid).is_none_or(|r| !busy.contains(&self.engine_device(&pid, r)))
                });
                ordered = free.into_iter().chain(taken).collect();
            }
        }
        if let Some(mine) = &eft.mine {
            if gid.is_some_and(|g| *mine.gid != *g) {
                ordered.clear();
            } else if let Some(pos) = ordered.iter().position(|j| j == mine) {
                let j = ordered.remove(pos);
                ordered.insert(0, j);
            }
        }
        let library = self.library();
        let mut taken: Option<(Job, Generation)> = None;
        for job in ordered {
            if gid.is_some_and(|g| *job.gid != *g) {
                continue;
            }
            if self.stopped.contains(&(job.gid.to_string(), mname.clone())) || backed_off.contains(&*job.gid) || left_to_warm.contains(&*job.gid) {
                continue;
            }
            if eft.skip.contains(&job) || self.claims.contains_key(&job) || self.attempted.contains(&job) {
                continue;
            }
            if self.returned_by.get(&job).is_some_and(|by| by.contains(&pid)) {
                continue;
            }
            let Some(row) = self.row(&job.gid).cloned() else { continue };
            if offered.get(&*job.gid) != Some(&true) {
                continue;
            }
            if !crate::ocr::owed::still_owed(&library, &job.rel, &row, job.upgrade) {
                self.forget_candidate(&job);
                continue;
            }
            if unmeasured.contains(&*job.gid) {
                self.want_autobench(&pid, &row);
                continue;
            }
            taken = Some((job, row));
            break;
        }
        match taken {
            Some((job, row)) => {
                self.take(&lane, &mname, &job, row);
                (Some(job), preempt)
            }
            None => {
                let waiting = proposed.iter().any(|j| eft.skip.contains(j));
                if let Some(l) = self.lanes.iter_mut().find(|l| l.id == lane_id) {
                    l.waiting_for_faster = waiting;
                }
                (None, preempt)
            }
        }
    }

    fn take(&mut self, lane: &Lane, machine: &str, job: &Job, row: Generation) {
        let stamp = stamp_of(&job.path(&self.library()));
        self.attempted.insert(job.clone());
        self.eft_deadlines.forget(job);
        if let Some(l) = self.lanes.iter_mut().find(|l| l.id == lane.id) {
            l.waiting_for_faster = false;
        }
        let key = Self::job_key(job);
        self.last_served.insert(key.generation_id, key.series);
        self.claims.insert(
            job.clone(),
            Claimed { lane: lane.id, pid: lane.pid.clone(), machine: machine.to_string(), sid: None, claim: None, stamp, settling: false, row },
        );
        self.bump();
    }

    // --- earliest finish ---------------------------------------------------------------------

    /// The in-flight volumes of a lane, priced as `_eft_lanes` prices them.
    fn lane_cards(&self, lane_id: u64) -> Vec<InflightCard> {
        self.claims
            .iter()
            .filter(|(_, c)| c.lane == lane_id)
            .map(|(job, _)| {
                let card = self.cards.get(job);
                let get_i = |k: &str| card.and_then(|c| c.get(k)).and_then(Value::as_i64);
                InflightCard {
                    generation_id: job.gid.to_string(),
                    eta_seconds: card.and_then(|c| c.get("eta_seconds")).and_then(Value::as_f64),
                    total_pages: get_i("total_pages").filter(|t| *t > 0).or_else(|| self.known_pages(&job.rel)),
                    done_pages: get_i("done_pages"),
                }
            })
            .collect()
    }

    /// `_eft_lanes`: every active lane, or None (plain first-come).
    fn eft_lanes(&self, asker: &Lane, walked: &[String]) -> Option<Vec<EftLane<u64>>> {
        if self.lanes.len() < 2 || !self.lanes.iter().any(|l| l.id == asker.id) {
            return None;
        }
        let pricing = self.pricing();
        let mut out = Vec::new();
        for lane in &self.lanes {
            let Some(machine) = self.machines.get(&lane.pid) else { continue };
            if self.held(&machine.name) || (!machine.local && self.breaker_open(&lane.pid)) {
                continue;
            }
            let cards = self.lane_cards(lane.id);
            let name = machine.name.clone();
            let free_in = lane_free_in(&cards, |g| pricing.rate_for(g, Some(&name), 0, 0.0))?;
            let warm = lane.session.as_ref().and_then(|sid| self.sessions.get(sid)).filter(|s| !s.closing).map(|s| s.row.id.clone());
            let now = self.now();
            let rows: HashSet<String> = walked
                .iter()
                .filter(|g| {
                    let Some(row) = self.row(g) else { return false };
                    !self.stopped.contains(&((*g).clone(), name.clone()))
                        && !self.start_backoff.get(&((*g).clone(), name.clone())).is_some_and(|b| now < b.until)
                        && self.refusal(&lane.pid, row).is_none()
                        && self.autobench_kind(&lane.pid, row).is_none()
                })
                .cloned()
                .collect();
            out.push(EftLane { key: lane.id, machine: name, free_in, warm, rows });
        }
        Some(out)
    }

    /// `_eft_left`: the walk's decision for the asking lane, with deadlines applied.
    pub fn eft_left(&mut self, lane: &Lane, proposed: &[Job]) -> EftOutcome {
        let mut walked: Vec<String> = Vec::new();
        for j in proposed {
            if !walked.iter().any(|g| **g == *j.gid) {
                walked.push(j.gid.to_string());
            }
        }
        let Some(lanes) = self.eft_lanes(lane, &walked) else { return EftOutcome::default() };
        let jobs: Vec<(Job, String, Option<i64>)> =
            proposed.iter().take(bunko_sched::eft::EFT_LOOKAHEAD).map(|j| (j.clone(), j.gid.to_string(), self.known_pages(&j.rel))).collect();
        let decision = {
            let pricing = self.pricing();
            earliest_finish_claim(
                &jobs,
                &lanes,
                &lane.id,
                |g, m| pricing.rate_for(g, Some(m), 0, 0.0),
                |g, m| pricing.startup_for(g, Some(m)).seconds,
                EftParams::default(),
            )
        };
        let Some(decision) = decision else { return EftOutcome::default() };
        if decision.left.is_empty() {
            return EftOutcome { mine: decision.mine, ..EftOutcome::default() };
        }
        let proposed_set: HashSet<Job> = proposed.iter().cloned().collect();
        let now = self.now();
        let outcome = self.eft_deadlines.apply(&proposed_set, &decision.left, now);
        if outcome.fresh {
            // A volume was just left to another lane: wake the idle ones.
            for l in &mut self.lanes {
                l.idle_at = None;
            }
        }
        let my_machine = self.machines.get(&lane.pid).map(|m| m.label()).unwrap_or_default();
        for left in &outcome.kept {
            let to = self.lanes.iter().find(|l| l.id == left.to).and_then(|l| self.machines.get(&l.pid)).map(|m| m.label()).unwrap_or_default();
            if self.left_logged.insert((left.job.clone(), to.clone())) {
                let row = self.row(&left.job.gid).map(|r| r.name.clone()).unwrap_or_default();
                self.log(format!(
                    "Leaving {} ({row}) to {to}: done there in ~{}s; {my_machine} would take ~{}s",
                    left.job.file_name(),
                    left.there.round(),
                    left.here.round()
                ));
            }
        }
        EftOutcome { mine: decision.mine, skip: outcome.skip }
    }

    /// `_rows_left_to_warm`: a row with ONE volume left, read sooner by a warm session
    /// elsewhere than by opening one here.
    fn rows_left_to_warm(&mut self, lane: &Lane, proposed: &[Job]) -> HashSet<String> {
        let Some(my_name) = self.machines.get(&lane.pid).map(|m| m.name.clone()) else { return HashSet::new() };
        // Rows with an open, live session on ANOTHER machine that may work.
        let mut warm_on: HashMap<String, Vec<String>> = HashMap::new();
        for s in self.sessions.values() {
            if s.closing || s.pid == lane.pid {
                continue;
            }
            let Some(m) = self.machines.get(&s.pid) else { continue };
            if self.held(&m.name) || self.breaker_open(&s.pid) {
                continue;
            }
            let list = warm_on.entry(s.row.id.clone()).or_default();
            if !list.contains(&s.pid) {
                list.push(s.pid.clone());
            }
        }
        if warm_on.is_empty() {
            return HashSet::new();
        }
        let warm_rows: HashSet<String> = warm_on.keys().cloned().collect();
        let unavailable: HashSet<Job> = self.claims.keys().cloned().chain(self.attempted.iter().cloned()).collect();
        let pairs: Vec<(Job, String)> = proposed.iter().map(|j| (j.clone(), j.gid.to_string())).collect();
        let singles = single_volume_rows(&pairs, &warm_rows, &unavailable);
        let mut out = HashSet::new();
        for (gid, job) in singles {
            let pages = self.deps.facts.page_count(&job.path(&self.library()));
            let leave = {
                let pricing = self.pricing();
                let mine_rate = pricing.rate_for(&gid, Some(&my_name), 0, 0.0);
                let mine_startup = pricing.startup_for(&gid, Some(&my_name)).seconds;
                let warm: Vec<WarmMachine> = warm_on[&gid]
                    .iter()
                    .filter_map(|pid| self.machines.get(pid))
                    .map(|m| {
                        let ahead: Vec<InflightCard> = self
                            .claims
                            .iter()
                            .filter(|(j, c)| c.pid == m.pid && *j.gid == *gid)
                            .map(|(j, _)| {
                                let card = self.cards.get(j);
                                InflightCard {
                                    generation_id: gid.clone(),
                                    eta_seconds: card.and_then(|c| c.get("eta_seconds")).and_then(Value::as_f64),
                                    total_pages: card.and_then(|c| c.get("total_pages")).and_then(Value::as_i64),
                                    done_pages: card.and_then(|c| c.get("done_pages")).and_then(Value::as_i64),
                                }
                            })
                            .collect();
                        WarmMachine { machine: m.name.clone(), rate: pricing.rate_for(&gid, Some(&m.name), 0, 0.0), ahead }
                    })
                    .collect();
                warm_session_wins(pages, mine_rate.as_ref(), mine_startup, &warm)
            };
            if let Some(leave) = leave {
                if self.left_logged.insert((job.clone(), leave.machine.clone())) {
                    let row = self.row(&gid).map(|r| r.name.clone()).unwrap_or_default();
                    self.log(format!(
                        "Leaving {} ({row}) to {}: its warm session reads it in ~{}s; opening one on {my_name} would take ~{}s",
                        job.file_name(),
                        leave.machine,
                        leave.theirs.round(),
                        leave.mine.round()
                    ));
                }
                out.insert(gid);
            }
        }
        out
    }
}
