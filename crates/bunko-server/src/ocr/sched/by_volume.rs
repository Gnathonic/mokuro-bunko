//! Taking work by volume (`ocr.queue_order: volume`, spec ocr-scheduling §4.2).
//!
//! The queue goes volume by volume, in the order each volume's first job would come by
//! generation, and the lane that starts a volume runs every job of it its machine can
//! run, one after another, from one copy of the archive. While that machine is
//! connected and able to run a job of the volume, the job is offered to that lane
//! alone. A job it cannot run (an engine it does not have, a row stopped or backed off
//! there) is open to any machine, which fetches the archive itself.
//!
//! A volume nobody has started begins at its first job: its later jobs wait while some
//! machine can run that first one, so a machine without the first generation's engine
//! never starts a volume halfway and splits it.
//!
//! The machine that takes a job its volume's owner cannot run becomes the volume's
//! next holder (`volume_owners` keeps them in order, the owner first). A job goes to
//! the first holder whose machine can run it, on the lane that holder runs the volume
//! on; it is open to every lane only when no holder can run it. So a second machine
//! runs all of what the first cannot, one job after another from one download, and a
//! third gets only what neither can. When the owner lets the volume go, the next
//! holder becomes its owner.

use std::collections::HashMap;
use std::sync::Arc;

use bunko_sched::failures::{Eligibility, Fleet, eligibility_in};
use serde_json::Value;

use super::{Lane, Scheduler, SpoolHold};
use crate::ocr::owed::OwedVolume;
use crate::ocr::profiles::profile_key;
use crate::ocr::types::{Job, Stamp};

/// An owner that holds no claim of its volume, could run one of its jobs and is not
/// waiting on a hold or a benchmark, gives the volume up after this long.
pub const OWNER_PATIENCE_SECONDS: f64 = 600.0;

/// A machine (and lane) running a volume: the one that started it (its owner, first in
/// `Scheduler::volume_owners`), or one that took a job no earlier holder can run.
#[derive(Debug)]
pub struct VolumeOwner {
    pub pid: String,
    pub lane: u64,
    /// This server's own OCR on a remote library: the spooled archive, pinned from the
    /// holder's first job of the volume until its last one ends or it lets the volume
    /// go, so the provider sends it once.
    pub spool: Option<SpoolHold>,
    /// The archive version `spool` holds.
    pub stamp: Option<Stamp>,
    /// Monotonic time the owner last held a claim of the volume or was waiting on a
    /// hold or a benchmark.
    pub since: f64,
    /// The holder has jobs of the volume beyond the ones in flight that no earlier
    /// holder takes (as of the last prune): its lane is not free for anyone else's
    /// volume.
    pub more: bool,
}

/// Whose turn a job of a volume is, for one lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turn {
    /// This lane holds the volume and is the first holder able to run the job.
    Mine,
    /// Any lane may take it.
    Open,
    /// Another lane's: it holds the volume and is the first holder able to run it.
    Elsewhere,
}

/// What a machine can run of a row, judged once per pass (it reads the machine's
/// profile from disk).
#[derive(Default)]
pub struct RowChecks(HashMap<(String, Arc<str>), bool>);

/// The jobs a volume owes, as `candidates` builds them (its rows, then the primary's
/// upgrade when the primary itself is not owed).
pub fn owed_jobs(rel: &str, vol: &OwedVolume, primary: Option<&str>) -> Vec<Job> {
    // One path shared by the volume's jobs: a large queue is thousands of them.
    let shared: Arc<str> = rel.into();
    let mut owed: Vec<Job> = vol
        .rows
        .iter()
        .map(|g| Job {
            rel: shared.clone(),
            gid: g.clone(),
            upgrade: false,
        })
        .collect();
    if vol.upgrade
        && let Some(p) = primary
        && !vol.rows.iter().any(|g| &**g == p)
    {
        owed.push(Job::upgrade(rel, p));
    }
    owed
}

impl Scheduler {
    /// Volume mode: the volumes being run first (by the lanes that started them), then
    /// volumes started earlier whose machine let them go, then the rest; each volume's
    /// jobs together in generation order, volumes in the order of their first job.
    pub(super) fn volume_order(&self, jobs: Vec<Job>) -> Vec<Job> {
        let mut first: HashMap<Arc<str>, usize> = HashMap::new();
        for (i, j) in jobs.iter().enumerate() {
            first.entry(j.rel.clone()).or_insert(i);
        }
        let class = |rel: &str| {
            if self.volume_owners.contains_key(rel) {
                0u8
            } else if self.volumes_started.contains(rel) {
                1
            } else {
                2
            }
        };
        let mut keyed: Vec<((u8, usize, usize), Job)> = jobs
            .into_iter()
            .enumerate()
            .map(|(i, j)| ((class(&j.rel), first[&j.rel], i), j))
            .collect();
        keyed.sort_by_key(|(k, _)| *k);
        keyed.into_iter().map(|(_, j)| j).collect()
    }

    /// The jobs of `rel`'s volume that could run now (row live, not in failure backoff,
    /// not claimed, not tried this scan), in generation order.
    pub fn volume_jobs(&self, rel: &str) -> Vec<Job> {
        let Some(vol) = self.owed.volumes.get(rel) else {
            return Vec::new();
        };
        let now = self.now();
        let poll = self.settings.poll_interval;
        let library_version = self.deps.version.clone();
        let fleet = Fleet {
            library_version: &library_version,
            machine_versions: self
                .machines
                .keys()
                .map(|pid| self.release_of(pid))
                .collect(),
        };
        let primary = self.primary().map(|p| p.id.clone());
        let rank = self.order_rank();
        let mut jobs: Vec<(usize, Job)> = owed_jobs(rel, vol, primary.as_deref())
            .into_iter()
            .filter(|job| {
                if self.claims.contains_key(job) || self.attempted.contains(job) {
                    return false;
                }
                let Some(row) = self.row(&job.gid) else {
                    return false;
                };
                let key = self.failure_key_of(job, row);
                let record = self.failures.get(&key).and_then(Value::as_object);
                !matches!(
                    eligibility_in(record, Some(vol.mtime), now, poll, &fleet),
                    Eligibility::BackedOff | Eligibility::Parked
                )
            })
            .map(|j| {
                let r = rank
                    .get(&self.job_key(&j).generation_id)
                    .copied()
                    .unwrap_or(usize::MAX);
                (r, j)
            })
            .collect();
        jobs.sort_by_key(|(r, _)| *r);
        jobs.into_iter().map(|(_, j)| j).collect()
    }

    /// Whether machine `pid` can run `row` now: connected with a lane, not paused, its
    /// downloads not held, the row not refused, stopped or backed off there. A row still
    /// waiting for its benchmark there counts as runnable: the machine benchmarks it,
    /// then runs it.
    fn machine_runs_row(&mut self, pid: &str, gid: &str) -> bool {
        let Some(name) = self
            .machines
            .get(pid)
            .filter(|m| m.connected())
            .map(|m| m.name.clone())
        else {
            return false;
        };
        if !self.lanes.iter().any(|l| l.pid == pid)
            || self.machine_paused(pid)
            || self.breaker_open(pid)
        {
            return false;
        }
        let Some(row) = self.row(gid) else {
            return false;
        };
        if self.refusal(pid, row).is_some() || self.stopped.contains(&(gid.to_string(), name)) {
            return false;
        }
        self.backed_off_rows(pid, &[gid.to_string()]).is_empty()
    }

    /// Whether machine `pid` can run `job` now ([`Self::machine_runs_row`], and the job
    /// itself: not claimed, not tried this scan, not given back by it, a failure it may
    /// retry, pages its release reads). Pages still being read count as readable.
    pub(super) fn can_run(&mut self, pid: &str, job: &Job, checks: &mut RowChecks) -> bool {
        if self.claims.contains_key(job) || self.attempted.contains(job) {
            return false;
        }
        let key = (pid.to_string(), job.gid.clone());
        let row_ok = match checks.0.get(&key) {
            Some(ok) => *ok,
            None => {
                let ok = self.machine_runs_row(pid, &job.gid);
                checks.0.insert(key, ok);
                ok
            }
        };
        row_ok
            && !self.returned_by.get(job).is_some_and(|by| by.contains(pid))
            && self.may_retry_on(pid, job)
            && self.machine_reading(pid, job) != Some(false)
    }

    /// Split the queue for `lane`: the jobs of volumes it started, and the jobs open to
    /// it (in queue order). The rest are another lane's.
    pub(super) fn volume_turns(&mut self, lane: &Lane, proposed: Vec<Job>) -> (Vec<Job>, Vec<Job>) {
        let mut checks = RowChecks::default();
        let mut first_of: HashMap<Arc<str>, Job> = HashMap::new();
        let mut later_open: HashMap<Arc<str>, bool> = HashMap::new();
        let (mut mine, mut open) = (Vec::new(), Vec::new());
        for job in proposed {
            match self.volume_turn(lane, &job, &mut checks, &mut first_of, &mut later_open) {
                Turn::Mine => mine.push(job),
                Turn::Open => open.push(job),
                Turn::Elsewhere => {}
            }
        }
        (mine, open)
    }

    /// One job's [`Turn`]. `proposed` is in volume order, so the first job of a volume
    /// seen is its first remaining one (`first_of`); `later_open` remembers, per volume
    /// nobody started, whether its later jobs are open (nobody can run the first).
    fn volume_turn(
        &mut self,
        lane: &Lane,
        job: &Job,
        checks: &mut RowChecks,
        first_of: &mut HashMap<Arc<str>, Job>,
        later_open: &mut HashMap<Arc<str>, bool>,
    ) -> Turn {
        if self.volume_owners.contains_key(&*job.rel) {
            return match self.holder_for(job, checks) {
                Some((pid, holder_lane)) => {
                    let lane_gone = !self.lanes.iter().any(|l| l.id == holder_lane);
                    if pid == lane.pid && (holder_lane == lane.id || lane_gone) {
                        Turn::Mine
                    } else {
                        Turn::Elsewhere
                    }
                }
                None => Turn::Open,
            };
        }
        let Some(first) = first_of.get(&job.rel).cloned() else {
            first_of.insert(job.rel.clone(), job.clone());
            return Turn::Open;
        };
        let open = match later_open.get(&job.rel) {
            Some(open) => *open,
            None => {
                let open = !self.anyone_can_run(&first, checks);
                later_open.insert(job.rel.clone(), open);
                open
            }
        };
        if open { Turn::Open } else { Turn::Elsewhere }
    }

    /// The holder of `job`'s volume whose turn the job is: the first, owner first, whose
    /// machine can run it now (its pid and lane). None: no holder can.
    fn holder_for(&mut self, job: &Job, checks: &mut RowChecks) -> Option<(String, u64)> {
        let holders: Vec<(String, u64)> = self
            .volume_owners
            .get(&*job.rel)?
            .iter()
            .map(|o| (o.pid.clone(), o.lane))
            .collect();
        holders
            .into_iter()
            .find(|(pid, _)| self.can_run(pid, job, checks))
    }

    /// Some machine with a lane can run `job` now.
    fn anyone_can_run(&mut self, job: &Job, checks: &mut RowChecks) -> bool {
        let mut pids: Vec<String> = self.lanes.iter().map(|l| l.pid.clone()).collect();
        pids.sort();
        pids.dedup();
        pids.iter().any(|pid| self.can_run(pid, job, checks))
    }

    /// A lane took a job of `job`'s volume: its machine holds the volume from now on.
    /// The first to take one owns it; a later one (it took a job no earlier holder can
    /// run) is the next holder.
    pub(super) fn own_volume(&mut self, lane: &Lane, job: &Job) {
        self.hold_volume(&lane.pid, lane.id, &job.rel);
    }

    fn hold_volume(&mut self, pid: &str, lane: u64, rel: &Arc<str>) {
        let now = self.mono();
        let holders = self.volume_owners.entry(rel.clone()).or_default();
        match holders.iter_mut().find(|o| o.pid == pid) {
            Some(holder) => {
                holder.lane = lane;
                holder.since = now;
            }
            None => holders.push(VolumeOwner {
                pid: pid.to_string(),
                lane,
                spool: None,
                stamp: None,
                since: now,
                more: true,
            }),
        }
        self.volumes_started.insert(rel.clone());
    }

    /// Taking work by volume was just switched on: each claim in flight holds its
    /// volume (with its spooled archive), so the volume's other jobs go to the machine
    /// already fetching it rather than to one that would fetch it again.
    pub(super) fn hold_volumes_in_flight(&mut self) {
        let mut jobs: Vec<Job> = self.claims.keys().cloned().collect();
        jobs.sort();
        for job in jobs {
            let Some((pid, lane, spool, stamp)) = self
                .claims
                .get(&job)
                .map(|c| (c.pid.clone(), c.lane, c.spool.clone(), c.stamp))
            else {
                continue;
            };
            self.hold_volume(&pid, lane, &job.rel);
            if let Some(hold) = spool
                && let Some(holder) = self
                    .volume_owners
                    .get_mut(&*job.rel)
                    .and_then(|h| h.iter_mut().find(|o| o.pid == pid))
                && holder.spool.is_none()
            {
                holder.spool = Some(hold);
                holder.stamp = stamp;
            }
        }
    }

    /// Whether machine `pid` will be offered another job of `job`'s volume after this
    /// one: a remote processor is then asked to keep the archive.
    pub(super) fn more_of_volume_for(&mut self, pid: &str, job: &Job) -> bool {
        if !self.settings.by_volume {
            return false;
        }
        let mut checks = RowChecks::default();
        let held = self.volume_owners.contains_key(&*job.rel);
        for j in self.volume_jobs(&job.rel) {
            if j == *job {
                continue;
            }
            let ours = if held {
                self.holder_for(&j, &mut checks)
                    .is_some_and(|(holder, _)| holder == pid)
            } else {
                self.can_run(pid, &j, &mut checks)
            };
            if ours {
                return true;
            }
        }
        false
    }

    /// The archives (as a remote processor's volume op names them) of every volume
    /// machine `pid` holds, sorted.
    pub(super) fn volumes_held_by(&self, pid: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .volume_owners
            .iter()
            .filter(|(_, holders)| holders.iter().any(|o| o.pid == pid))
            .map(|(rel, _)| format!("{}{rel}", bunko_proto::ARCHIVES_ROOT))
            .collect();
        out.sort();
        out
    }

    /// The archive the holder of `job`'s volume on this claim's machine keeps spooled,
    /// when it is the version the claim was taken at.
    pub(super) fn owner_spool(&self, job: &Job) -> Option<SpoolHold> {
        let claim = self.claims.get(job)?;
        let holder = self
            .volume_owners
            .get(&*job.rel)?
            .iter()
            .find(|o| o.pid == claim.pid)?;
        (holder.stamp == claim.stamp && claim.stamp.is_some())
            .then(|| holder.spool.clone())
            .flatten()
    }

    /// A claim's archive was spooled: its machine's hold on the volume keeps it for the
    /// machine's next job of the volume.
    pub(super) fn keep_owner_spool(&mut self, job: &Job, hold: &SpoolHold) {
        if !self.settings.by_volume {
            return;
        }
        let Some((pid, stamp)) = self.claims.get(job).map(|c| (c.pid.clone(), c.stamp)) else {
            return;
        };
        if let Some(holder) = self
            .volume_owners
            .get_mut(&*job.rel)
            .and_then(|h| h.iter_mut().find(|o| o.pid == pid))
        {
            holder.spool = Some(hold.clone());
            holder.stamp = stamp;
        }
    }

    /// Let go of holders that are gone, have nothing left of the volume they would
    /// get (a job they can run that no holder kept before them can), or stopped taking
    /// its jobs (patience: [`OWNER_PATIENCE_SECONDS`] without a claim of the volume
    /// while able, never counted during a hold or a benchmark). Dropping a holder drops
    /// its spooled archive; the next holder becomes the owner. Called at the start of
    /// every pump.
    pub(super) fn prune_volume_owners(&mut self) {
        if self.volume_owners.is_empty() {
            return;
        }
        let now = self.mono();
        let rels: Vec<Arc<str>> = self.volume_owners.keys().cloned().collect();
        let mut checks = RowChecks::default();
        let mut dropped = false;
        for rel in rels {
            let holders: Vec<(String, f64)> = self
                .volume_owners
                .get(&rel)
                .into_iter()
                .flatten()
                .map(|o| (o.pid.clone(), o.since))
                .collect();
            let jobs = self.volume_jobs(&rel);
            // Holders kept, in order, with (refresh `since`, `more`).
            let mut kept: Vec<(String, bool, bool)> = Vec::new();
            for (pid, since) in holders {
                let Some((name, label)) = self
                    .machines
                    .get(&pid)
                    .filter(|m| m.connected())
                    .map(|m| (m.name.clone(), m.label()))
                else {
                    continue;
                };
                let in_flight = self
                    .claims
                    .iter()
                    .any(|(j, c)| j.rel == rel && c.pid == pid);
                let key = profile_key(&name).to_string();
                let waiting =
                    self.held(&name) || self.autobench.inflight.iter().any(|(m, _)| *m == key);
                let mut more = false;
                for j in &jobs {
                    if !self.can_run(&pid, j, &mut checks) {
                        continue;
                    }
                    let earlier = kept.iter().any(|(k, _, _)| self.can_run(k, j, &mut checks));
                    if !earlier {
                        more = true;
                        break;
                    }
                }
                if !in_flight && !more {
                    continue;
                }
                if !in_flight && !waiting && now - since > OWNER_PATIENCE_SECONDS {
                    self.log(format!(
                        "{label} did not continue {rel} for {} minutes; its other jobs go to any machine",
                        (OWNER_PATIENCE_SECONDS / 60.0).round()
                    ));
                    continue;
                }
                kept.push((pid, in_flight || waiting, more));
            }
            let Some(holders) = self.volume_owners.get_mut(&rel) else {
                continue;
            };
            let before = holders.len();
            holders.retain(|o| kept.iter().any(|(k, _, _)| *k == o.pid));
            dropped |= holders.len() != before;
            for holder in holders.iter_mut() {
                if let Some((_, busy, more)) = kept.iter().find(|(k, _, _)| *k == holder.pid) {
                    if *busy {
                        holder.since = now;
                    }
                    holder.more = *more;
                }
            }
            if holders.is_empty() {
                self.volume_owners.remove(&rel);
            }
        }
        if dropped {
            self.bump();
            for lane in &mut self.lanes {
                lane.idle_at = None;
            }
        }
    }

    /// A lane busy with a volume it started that has jobs left for it: the
    /// earliest-finish walk does not count on it for other work.
    pub(super) fn lane_runs_a_volume(&self, lane_id: u64) -> bool {
        self.settings.by_volume
            && self
                .volume_owners
                .values()
                .flatten()
                .any(|o| o.lane == lane_id && o.more)
    }

    /// Stop taking work by volume: every owner and its spooled archive goes.
    pub(super) fn forget_volume_owners(&mut self) {
        self.volume_owners.clear();
        self.volumes_started.clear();
    }
}
