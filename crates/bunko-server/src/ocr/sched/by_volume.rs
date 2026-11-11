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

/// The machine (and lane) that started a volume.
#[derive(Debug)]
pub struct VolumeOwner {
    pub pid: String,
    pub lane: u64,
    /// This server's own OCR on a remote library: the spooled archive, pinned from the
    /// volume's first job until the owner's last job of it ends or the volume is given
    /// up, so the provider sends it once.
    pub spool: Option<SpoolHold>,
    /// The archive version `spool` holds.
    pub stamp: Option<Stamp>,
    /// Monotonic time the owner last held a claim of the volume or was waiting on a
    /// hold or a benchmark.
    pub since: f64,
    /// The owner can run jobs of the volume beyond the ones in flight (as of the last
    /// prune): its lane is not free for anyone else's volume.
    pub more: bool,
}

/// Whose turn a job of a volume is, for one lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turn {
    /// This lane started the volume.
    Mine,
    /// Any lane may take it.
    Open,
    /// Another lane's: its machine runs the volume and can run this job.
    Elsewhere,
}

/// What a machine can run of a row, judged once per pass (it reads the machine's
/// profile from disk).
#[derive(Default)]
pub struct RowChecks(HashMap<(String, Arc<str>), bool>);

/// The jobs a volume owes, as `candidates` builds them (its rows, then the primary's
/// upgrade when the primary itself is not owed).
pub fn owed_jobs(rel: &str, vol: &OwedVolume, primary: Option<&str>) -> Vec<Job> {
    let mut owed: Vec<Job> = vol
        .rows
        .iter()
        .map(|g| Job {
            rel: rel.into(),
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
        if let Some((pid, owner_lane)) = self
            .volume_owners
            .get(&*job.rel)
            .map(|o| (o.pid.clone(), o.lane))
        {
            let lane_gone = !self.lanes.iter().any(|l| l.id == owner_lane);
            if pid == lane.pid && (owner_lane == lane.id || lane_gone) {
                return Turn::Mine;
            }
            return if self.can_run(&pid, job, checks) {
                Turn::Elsewhere
            } else {
                Turn::Open
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

    /// Some machine with a lane can run `job` now.
    fn anyone_can_run(&mut self, job: &Job, checks: &mut RowChecks) -> bool {
        let mut pids: Vec<String> = self.lanes.iter().map(|l| l.pid.clone()).collect();
        pids.sort();
        pids.dedup();
        pids.iter().any(|pid| self.can_run(pid, job, checks))
    }

    /// A lane took a job of `job`'s volume: the volume is its machine's from now on,
    /// unless another machine runs it already (this one runs only what that one
    /// cannot).
    pub(super) fn own_volume(&mut self, lane: &Lane, job: &Job) {
        let now = self.mono();
        match self.volume_owners.get_mut(&*job.rel) {
            Some(owner) if owner.pid == lane.pid => {
                owner.lane = lane.id;
                owner.since = now;
            }
            Some(_) => {}
            None => {
                self.volume_owners.insert(
                    job.rel.clone(),
                    VolumeOwner {
                        pid: lane.pid.clone(),
                        lane: lane.id,
                        spool: None,
                        stamp: None,
                        since: now,
                        more: true,
                    },
                );
            }
        }
        self.volumes_started.insert(job.rel.clone());
    }

    /// Whether machine `pid` will be offered another job of `job`'s volume after this
    /// one: a remote processor is then asked to keep the archive.
    pub(super) fn more_of_volume_for(&mut self, pid: &str, job: &Job) -> bool {
        if !self.settings.by_volume {
            return false;
        }
        let mut checks = RowChecks::default();
        let owner = self.volume_owners.get(&*job.rel).map(|o| o.pid.clone());
        for j in self.volume_jobs(&job.rel) {
            if j == *job {
                continue;
            }
            let ours = match &owner {
                Some(o) if o != pid => {
                    !self.can_run(o, &j, &mut checks) && self.can_run(pid, &j, &mut checks)
                }
                _ => self.can_run(pid, &j, &mut checks),
            };
            if ours {
                return true;
            }
        }
        false
    }

    /// The archive the owner of `job`'s volume keeps spooled, when it is this claim's
    /// machine and the version the claim was taken at.
    pub(super) fn owner_spool(&self, job: &Job) -> Option<SpoolHold> {
        let claim = self.claims.get(job)?;
        let owner = self.volume_owners.get(&*job.rel)?;
        (owner.pid == claim.pid && owner.stamp == claim.stamp && claim.stamp.is_some())
            .then(|| owner.spool.clone())
            .flatten()
    }

    /// A claim's archive was spooled: its volume's owner keeps it for the next job.
    pub(super) fn keep_owner_spool(&mut self, job: &Job, hold: &SpoolHold) {
        if !self.settings.by_volume {
            return;
        }
        let Some((pid, stamp)) = self.claims.get(job).map(|c| (c.pid.clone(), c.stamp)) else {
            return;
        };
        if let Some(owner) = self.volume_owners.get_mut(&*job.rel)
            && owner.pid == pid
        {
            owner.spool = Some(hold.clone());
            owner.stamp = stamp;
        }
    }

    /// Let go of volumes whose owner is gone, has nothing left of them it can run, or
    /// stopped taking them (patience: [`OWNER_PATIENCE_SECONDS`] without a claim of the
    /// volume while able, never counted during a hold or a benchmark). Dropping an owner
    /// drops its spooled archive. Called at the start of every pump.
    pub(super) fn prune_volume_owners(&mut self) {
        if self.volume_owners.is_empty() {
            return;
        }
        let now = self.mono();
        let rels: Vec<Arc<str>> = self.volume_owners.keys().cloned().collect();
        let mut checks = RowChecks::default();
        let mut dropped = false;
        for rel in rels {
            let Some((pid, since)) = self
                .volume_owners
                .get(&rel)
                .map(|o| (o.pid.clone(), o.since))
            else {
                continue;
            };
            let Some((name, label)) = self
                .machines
                .get(&pid)
                .filter(|m| m.connected())
                .map(|m| (m.name.clone(), m.label()))
            else {
                self.volume_owners.remove(&rel);
                dropped = true;
                continue;
            };
            let in_flight = self
                .claims
                .iter()
                .any(|(j, c)| j.rel == rel && c.pid == pid);
            let key = profile_key(&name).to_string();
            let waiting =
                self.held(&name) || self.autobench.inflight.iter().any(|(m, _)| *m == key);
            let jobs = self.volume_jobs(&rel);
            let more = jobs.iter().any(|j| self.can_run(&pid, j, &mut checks));
            if !in_flight && !more {
                self.volume_owners.remove(&rel);
                dropped = true;
                continue;
            }
            if !in_flight && !waiting && now - since > OWNER_PATIENCE_SECONDS {
                self.log(format!(
                    "{label} did not continue {rel} for {} minutes; its other jobs go to any machine",
                    (OWNER_PATIENCE_SECONDS / 60.0).round()
                ));
                self.volume_owners.remove(&rel);
                dropped = true;
                continue;
            }
            if let Some(owner) = self.volume_owners.get_mut(&rel) {
                if in_flight || waiting {
                    owner.since = now;
                }
                owner.more = more;
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
                .any(|o| o.lane == lane_id && o.more)
    }

    /// Stop taking work by volume: every owner and its spooled archive goes.
    pub(super) fn forget_volume_owners(&mut self) {
        self.volume_owners.clear();
        self.volumes_started.clear();
    }
}
