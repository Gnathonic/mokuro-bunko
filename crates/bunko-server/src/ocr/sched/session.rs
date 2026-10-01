//! Sessions (spec ocr-scheduling §9): open, feed with a 2-deep lookahead, react to the
//! processor's events, drain, close, and settle what a dying session held.

use std::collections::HashMap;

use bunko_core::generations::Generation;
use bunko_proto::{Event, Op, VolumeOp};
use bunko_sched::breaker::StartBackoff;
use bunko_sched::congestion::{VolumeTiming, build_record, summarize_event_stats};
use bunko_sched::plan::{Pricing, SessionProgress, session_progress_update};
use bunko_sched::rate::rate_key;
use bunko_sched::speed::busy_reason;
use serde_json::{Map, Value, json};

use super::{
    CLOSE_GRACE_SECONDS, PRECISION_REFUSAL, SESSION_CRASH_LIMIT, SESSION_LOOKAHEAD,
    SESSION_WEDGE_SECONDS, SILENCE_SECONDS, Scheduler,
};
use crate::ocr::profiles::{LOCAL_PROFILE, profile_key};
use crate::ocr::types::{Job, LOCAL, token_hex};

/// One open session: one machine running one row snapshot.
#[derive(Clone, Debug)]
pub struct Session {
    pub pid: String,
    pub machine: String,
    pub local: bool,
    pub lane: u64,
    pub row: Generation,
    /// When the decision to open it was taken (startup is measured from here).
    pub started_at: f64,
    pub ready_at: Option<f64>,
    /// Monotonic time of its last event (wedge judgement).
    pub last_event: f64,
    pub completed: u32,
    /// Claims in submit order.
    pub order: Vec<String>,
    pub jobs: HashMap<String, SessionJob>,
    pub closing: bool,
    pub closing_since: f64,
    pub draining: Option<String>,
    pub fatal_error: Option<String>,
    pub next_claim_at: f64,
}

/// One volume a session holds.
#[derive(Clone, Debug)]
pub struct SessionJob {
    pub job: Job,
    pub row: Generation,
    pub claim: String,
    /// The runner has it (local: from submit; remote: `fetch ready` / `volume_started`).
    pub delivered: bool,
    pub archive_size: Option<u64>,
    pub sidecar_name: String,
    pub first_page_at: Option<f64>,
    pub done: i64,
    pub total: i64,
}

fn precision_refused(error: Option<&str>) -> bool {
    error.is_some_and(|e| e.contains(PRECISION_REFUSAL))
}

impl Scheduler {
    fn where_(&self, machine: &str) -> String {
        if machine == LOCAL {
            String::new()
        } else {
            format!(" on {machine}")
        }
    }

    /// Open a session on `lane` for the job it just claimed.
    pub fn open_session(&mut self, lane_id: u64, job: Job) {
        let Some(claimed) = self.claims.get(&job).cloned() else {
            return;
        };
        let Some((name, local)) = self
            .machines
            .get(&claimed.pid)
            .map(|m| (m.name.clone(), m.local))
        else {
            return;
        };
        let row = claimed.row.clone();
        let sid = token_hex(6);
        let spec = self.row_spec(&name, &row);
        let now = self.now();
        let session = Session {
            pid: claimed.pid.clone(),
            machine: name.clone(),
            local,
            lane: lane_id,
            row: row.clone(),
            started_at: now,
            ready_at: None,
            last_event: self.mono(),
            completed: 0,
            order: Vec::new(),
            jobs: HashMap::new(),
            closing: false,
            closing_since: 0.0,
            draining: None,
            fatal_error: None,
            next_claim_at: 0.0,
        };
        let sent = self.machines.get(&claimed.pid).is_some_and(|m| {
            m.send(Op::OpenSession {
                sid: sid.clone(),
                generation: spec,
            })
        });
        if !sent {
            self.fail_session_start(&claimed.pid, &row, &job, &format!("{name} disconnected"));
            return;
        }
        self.sessions.insert(sid.clone(), session);
        if let Some(l) = self.lanes.iter_mut().find(|l| l.id == lane_id) {
            l.session = Some(sid.clone());
        }
        if !self.submit(&sid, &job)
            && let Some(s) = self.sessions.get_mut(&sid)
        {
            s.draining = Some("the runner stopped accepting volumes".into());
        }
    }

    /// `_fail_session_start`: the session could not even be asked to open.
    fn fail_session_start(&mut self, pid: &str, row: &Generation, job: &Job, error: &str) {
        self.log(format!("Could not start {}: {error}", row.name));
        let machine = self.machines.get(pid).map(|m| (m.name.clone(), m.local));
        let gone = machine.is_none();
        let name = machine.as_ref().map(|m| m.0.clone()).unwrap_or_default();
        if !gone {
            self.note_start_failure(pid, row, &name, error);
        }
        let local = machine.as_ref().is_some_and(|m| m.1);
        let precision = precision_refused(Some(error));
        if !local || precision {
            self.release(job, error, true);
            if !gone && !precision {
                self.strike(row, &name, error);
            }
        } else {
            self.finish(job, false, Some(error.to_string()), None);
        }
    }

    /// `_submit_session_volume`: send one claim. False when the runner refused it.
    pub fn submit(&mut self, sid: &str, job: &Job) -> bool {
        let Some(session) = self.sessions.get(sid) else {
            return false;
        };
        let (pid, local, machine) = (session.pid.clone(), session.local, session.machine.clone());
        let row = self
            .row(&job.gid)
            .cloned()
            .unwrap_or_else(|| session.row.clone());
        self.claim_seq += 1;
        let claim = format!("v{}", self.claim_seq);
        let library = self.library();
        let cbz = job.path(&library);
        let stamp = crate::ocr::types::stamp_of(&cbz);
        let series_name =
            bunko_layout::sidecar::derive_series_name(&cbz, &library, &self.deps.layout.inbox());
        let volume_uuid = self
            .deps
            .db
            .as_ref()
            .and_then(|db| db.remembered_volume_uuid(&job.rel).ok().flatten());
        let sidecar_name = format!("{}{}", job.volume(), row.sidecar_suffix());
        let op = VolumeOp {
            sid: sid.to_string(),
            claim: claim.clone(),
            archive: if local {
                cbz.to_string_lossy().into_owned()
            } else {
                format!("{}{}", bunko_proto::ARCHIVES_ROOT, job.rel)
            },
            sidecar_name: sidecar_name.clone(),
            title: series_name.clone(),
            volume_title: job.volume().to_string(),
            title_uuid: Some(bunko_layout::sidecar::title_uuid(&series_name)),
            volume_uuid,
            size: stamp.map(|s| s.0),
            etag: None,
        };
        let sent = self
            .machines
            .get(&pid)
            .is_some_and(|m| m.send(Op::Volume(op)));
        if !sent {
            self.release(job, "the runner closed before it took the volume", true);
            return false;
        }
        if let Some(c) = self.claims.get_mut(job) {
            c.sid = Some(sid.to_string());
            c.claim = Some(claim.clone());
            if local {
                // The file the runner reads now is the one the result must match.
                c.stamp = stamp;
            }
        }
        let ready = self.sessions.get(sid).is_some_and(|s| s.ready_at.is_some());
        let started_at = self
            .sessions
            .get(sid)
            .map(|s| s.started_at)
            .unwrap_or_default();
        if let Some(s) = self.sessions.get_mut(sid) {
            s.order.push(claim.clone());
            s.jobs.insert(
                claim.clone(),
                SessionJob {
                    job: job.clone(),
                    row: row.clone(),
                    claim,
                    delivered: local,
                    archive_size: stamp.map(|s| s.0),
                    sidecar_name,
                    first_page_at: None,
                    done: 0,
                    total: 0,
                },
            );
        }
        let lane = self.claims.get(job).map(|c| c.lane).unwrap_or_default();
        self.begin_card(job, &row, &machine, lane, ready, local, started_at);
        true
    }

    /// `begin_ocr_job`: the running card.
    #[allow(clippy::too_many_arguments)]
    fn begin_card(
        &mut self,
        job: &Job,
        row: &Generation,
        machine: &str,
        slot: u64,
        ready: bool,
        delivered: bool,
        session_started_at: f64,
    ) {
        let now = self.now();
        let label = self
            .machine_by_name(machine)
            .filter(|m| !m.local)
            .map(|m| m.label());
        let mut card = Map::new();
        card.insert("started_at".into(), json!(now));
        card.insert("active".into(), json!(true));
        card.insert("updated_at".into(), json!(now));
        card.insert("generation".into(), json!(row.name));
        card.insert("generation_id".into(), json!(row.id));
        card.insert("processor".into(), json!(label));
        card.insert("engine".into(), json!(row.engine));
        card.insert("detector".into(), json!(row.effective_detector()));
        card.insert("series".into(), json!(job.series()));
        card.insert("volume".into(), json!(job.volume()));
        card.insert("relative_cbz".into(), json!(&*job.rel));
        card.insert("percent".into(), json!(0));
        card.insert("eta_seconds".into(), Value::Null);
        card.insert("status".into(), json!("starting"));
        card.insert("session_ready".into(), json!(ready));
        card.insert("delivered".into(), json!(delivered));
        card.insert("slot".into(), json!(slot));
        card.insert("machine".into(), json!(machine));
        card.insert("session_started_at".into(), json!(session_started_at));
        if let Some(pages) = self.deps.facts.page_count(&job.path(&self.library())) {
            card.insert("total_pages".into(), json!(pages));
        }
        self.cards.insert(job.clone(), card);
        self.bump_page();
    }

    pub fn update_card(&mut self, job: &Job, update: Map<String, Value>) {
        let now = self.now();
        if let Some(card) = self.cards.get_mut(job) {
            for (k, v) in update {
                card.insert(k, v);
            }
            card.insert("updated_at".into(), json!(now));
            self.page_version += 1;
        }
    }

    /// `_session_drain_reason`.
    fn drain_reason(&self, sid: &str) -> Option<String> {
        let s = self.sessions.get(sid)?;
        if self.stopping {
            return Some("the worker is stopping".into());
        }
        if self.held(&s.machine) {
            return Some("the queue is held".into());
        }
        if self
            .stopped
            .contains(&(s.row.id.clone(), s.machine.clone()))
        {
            return Some("the generation was stopped for this scan".into());
        }
        if !s.local && self.breaker_open(&s.pid) {
            return Some("its archive downloads keep failing".into());
        }
        if self.row(&s.row.id).is_none() {
            return Some("the generation was removed or disabled".into());
        }
        if !self.machines.contains_key(&s.pid) {
            return Some("the runner exited".into());
        }
        None
    }

    /// The session loop's claim half: keep two claims in flight, drain, close.
    pub fn top_up(&mut self, sid: String) {
        let Some(s) = self.sessions.get(&sid) else {
            return;
        };
        if s.closing {
            return;
        }
        let (lane, row_id) = (s.lane, s.row.id.clone());
        if s.draining.is_none()
            && let Some(reason) = self.drain_reason(&sid)
        {
            let s = self.sessions.get_mut(&sid).expect("session checked above");
            s.draining = Some(reason.clone());
            let msg = format!(
                "Draining the {} session{}: {reason}",
                s.row.name,
                if s.local {
                    String::new()
                } else {
                    format!(" on {}", s.machine)
                }
            );
            self.log(msg);
        }
        loop {
            let Some(s) = self.sessions.get(&sid) else {
                return;
            };
            let now = self.mono();
            if s.draining.is_some()
                || s.order.len() >= SESSION_LOOKAHEAD
                || (!s.order.is_empty() && now < s.next_claim_at)
            {
                break;
            }
            let (job, preempt) = self.claim(lane, Some(&row_id));
            let Some(job) = job else {
                let poll = self.settings.poll_interval.max(1.0);
                let s = self.sessions.get_mut(&sid).expect("session exists");
                if preempt {
                    s.draining = Some("an earlier generation has work".into());
                } else {
                    s.next_claim_at = now + poll;
                }
                break;
            };
            if let Some(s) = self.sessions.get_mut(&sid) {
                s.next_claim_at = 0.0;
            }
            if !self.submit(&sid, &job) {
                if let Some(s) = self.sessions.get_mut(&sid) {
                    s.draining = Some("the runner stopped accepting volumes".into());
                }
                break;
            }
            if preempt {
                if let Some(s) = self.sessions.get_mut(&sid) {
                    s.draining = Some("an earlier generation has work".into());
                }
                break;
            }
        }
        if self.sessions.get(&sid).is_some_and(|s| s.order.is_empty()) {
            self.close_session(&sid);
        }
    }

    /// Finish what was accepted (nothing), then `exit`.
    fn close_session(&mut self, sid: &str) {
        let now = self.mono();
        let Some(s) = self.sessions.get_mut(sid) else {
            return;
        };
        if s.closing {
            return;
        }
        s.closing = true;
        s.closing_since = now;
        let pid = s.pid.clone();
        let sent = self.machines.get(&pid).is_some_and(|m| {
            m.send(Op::CloseSession {
                sid: sid.to_string(),
            })
        });
        if !sent {
            self.end_session(sid, None, false);
        }
    }

    /// Cancel every claim of a session and close it; it ends now (`kill()`).
    pub fn kill_session(&mut self, sid: &str, fatal_error: Option<String>) {
        let Some(s) = self.sessions.get(sid) else {
            return;
        };
        let pid = s.pid.clone();
        let claims: Vec<String> = s.order.clone();
        if let Some(m) = self.machines.get(&pid) {
            for claim in claims {
                m.send(Op::Cancel {
                    sid: Some(sid.to_string()),
                    claim: Some(claim),
                    bid: None,
                });
            }
            m.send(Op::CloseSession {
                sid: sid.to_string(),
            });
        }
        if let Some(s) = self.sessions.get_mut(sid) {
            s.fatal_error = fatal_error.or(s.fatal_error.take());
        }
        self.end_session(sid, None, true);
    }

    /// Timers: wedged sessions, closing sessions that never said `exit`.
    pub fn check_sessions(&mut self) {
        let now = self.mono();
        let mut wedged: Vec<String> = Vec::new();
        let mut overdue: Vec<String> = Vec::new();
        for (sid, s) in &self.sessions {
            if s.closing {
                if now - s.closing_since > CLOSE_GRACE_SECONDS {
                    overdue.push(sid.clone());
                }
            } else if now - s.last_event > SESSION_WEDGE_SECONDS {
                wedged.push(sid.clone());
            }
        }
        for sid in overdue {
            self.kill_session(&sid, None);
        }
        for sid in wedged {
            let Some(s) = self.sessions.get(&sid) else {
                continue;
            };
            let (pid, local, name) = (s.pid.clone(), s.local, s.row.name.clone());
            let silent = self
                .machines
                .get(&pid)
                .map(|m| now - m.last_frame)
                .unwrap_or(f64::INFINITY);
            if !local && silent > SILENCE_SECONDS {
                self.drop_processor(
                    &pid,
                    &format!("it has sent nothing for {}s", silent.round()),
                );
                continue;
            }
            let wedge = format!(
                "the {name} runner stopped responding (no event for {}s)",
                SESSION_WEDGE_SECONDS as i64
            );
            self.kill_session_blaming(&sid, wedge);
        }
    }

    /// A wedged runner: killed, and judged like a crash (the oldest delivered volume).
    fn kill_session_blaming(&mut self, sid: &str, error: String) {
        let Some(s) = self.sessions.get(sid) else {
            return;
        };
        let pid = s.pid.clone();
        let claims = s.order.clone();
        if let Some(m) = self.machines.get(&pid) {
            for claim in claims {
                m.send(Op::Cancel {
                    sid: Some(sid.to_string()),
                    claim: Some(claim),
                    bid: None,
                });
            }
            m.send(Op::CloseSession {
                sid: sid.to_string(),
            });
        }
        if let Some(s) = self.sessions.get_mut(sid) {
            s.fatal_error = Some(error);
        }
        self.end_session(sid, None, false);
    }

    // --- events ------------------------------------------------------------------------------

    pub fn on_event(&mut self, pid: &str, event: Event) {
        self.seen(pid);
        if !self.machines.contains_key(pid) {
            return;
        }
        match &event {
            Event::Ping => return,
            Event::Catalog { catalog } => {
                self.catalog_changed(pid, catalog.clone());
                return;
            }
            Event::BenchReady { .. }
            | Event::BenchProgress { .. }
            | Event::BenchTrial { .. }
            | Event::BenchDone { .. } => {
                self.bench_event(pid, event);
                return;
            }
            _ => {}
        }
        let Some(sid) = event.sid().map(str::to_string) else {
            return;
        };
        if sid.starts_with("bench-") {
            self.bench_event(pid, event);
            return;
        }
        let Some(session) = self.sessions.get_mut(&sid) else {
            return;
        };
        if session.pid != pid {
            return;
        }
        session.last_event = self.deps.clock.monotonic();
        match event {
            Event::Exit { returncode, .. } => {
                let error = self.session_exit_error(&sid, returncode);
                if let Some(s) = self.sessions.get_mut(&sid) {
                    s.fatal_error = error;
                }
                self.end_session(&sid, returncode, false);
            }
            Event::Fatal { error, .. } | Event::SpawnFailed { error, .. } => {
                if let Some(s) = self.sessions.get_mut(&sid) {
                    s.fatal_error = Some(error);
                }
            }
            Event::Ready {
                startup_seconds,
                pipeline,
                ..
            } => self.on_ready(&sid, startup_seconds, &pipeline),
            Event::Stats {
                pipeline,
                cpu_pressure,
                other_cpu,
                ..
            } => {
                let mut update = Map::new();
                if let Some(summary) = summarize_event_stats(&pipeline) {
                    update.insert("pipeline".into(), Value::Object(summary));
                }
                if cpu_pressure.is_some() || other_cpu.is_some() {
                    let mut ev = Map::new();
                    ev.insert("cpu_pressure".into(), json!(cpu_pressure));
                    ev.insert("other_cpu".into(), json!(other_cpu));
                    update.insert("host_busy".into(), json!(busy_reason(&ev).is_some()));
                }
                if !update.is_empty() {
                    let jobs: Vec<Job> = self
                        .sessions
                        .get(&sid)
                        .map(|s| s.jobs.values().map(|j| j.job.clone()).collect())
                        .unwrap_or_default();
                    for j in jobs {
                        self.update_card(&j, update.clone());
                    }
                }
            }
            Event::Fetch {
                id, state, detail, ..
            } => {
                if state == "ready" {
                    let Some(job) = self
                        .sessions
                        .get_mut(&sid)
                        .and_then(|s| s.jobs.get_mut(&id))
                        .map(|j| {
                            j.delivered = true;
                            j.job.clone()
                        })
                    else {
                        return;
                    };
                    let mut u = Map::new();
                    u.insert("delivered".into(), json!(true));
                    self.update_card(&job, u);
                    self.download_delivered(pid, &job, &detail);
                }
            }
            Event::VolumeReturned {
                id, class, error, ..
            } => {
                let Some(entry) = self.pop_job(&sid, &id) else {
                    return;
                };
                self.judge_returned(pid, &sid, entry, &class, &error);
            }
            Event::VolumeStarted { id, pages, .. } => {
                let Some(j) = self
                    .sessions
                    .get_mut(&sid)
                    .and_then(|s| s.jobs.get_mut(&id))
                else {
                    return;
                };
                j.delivered = true;
                j.total = i64::from(pages);
                self.session_progress(&sid, &id);
            }
            Event::Page {
                id, done, total, ..
            } => {
                let now = self.now();
                let Some(j) = self
                    .sessions
                    .get_mut(&sid)
                    .and_then(|s| s.jobs.get_mut(&id))
                else {
                    return;
                };
                j.done = i64::from(done);
                if total > 0 {
                    j.total = i64::from(total);
                }
                if j.first_page_at.is_none() && j.done > 0 {
                    j.first_page_at = Some(now);
                }
                self.session_progress(&sid, &id);
            }
            Event::VolumeDone {
                id,
                pages,
                failed_pages,
                seconds,
                stats,
                cpu_pressure,
                other_cpu,
                sidecar_sha256,
                ..
            } => {
                let Some(entry) = self.pop_job(&sid, &id) else {
                    return;
                };
                self.volume_done(
                    &sid,
                    entry,
                    pages,
                    failed_pages,
                    seconds,
                    stats,
                    cpu_pressure,
                    other_cpu,
                    sidecar_sha256,
                );
            }
            Event::VolumeFailed { id, error, .. } => {
                let Some(entry) = self.pop_job(&sid, &id) else {
                    return;
                };
                let row_name = entry.row.name.clone();
                let error = if error.is_empty() {
                    format!("{row_name} could not read this volume")
                } else {
                    error
                };
                self.drop_result(&sid, &id);
                if self.stopping {
                    self.release(&entry.job, "the worker is stopping", false);
                } else {
                    self.finish(&entry.job, false, Some(error), None);
                }
            }
            _ => {}
        }
    }

    fn pop_job(&mut self, sid: &str, claim: &str) -> Option<SessionJob> {
        let s = self.sessions.get_mut(sid)?;
        let entry = s.jobs.remove(claim)?;
        s.order.retain(|c| c != claim);
        Some(entry)
    }

    fn drop_result(&mut self, sid: &str, claim: &str) {
        if let Some((path, _)) = self.results.remove(&(sid.to_string(), claim.to_string()))
            && let Some(dir) = path.parent()
        {
            let _ = std::fs::remove_dir_all(dir);
        }
        let dir = self.storage().join(".processing").join(sid).join(claim);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn on_ready(&mut self, sid: &str, startup_seconds: f64, pipeline: &str) {
        let now = self.now();
        let Some(s) = self.sessions.get_mut(sid) else {
            return;
        };
        s.ready_at = Some(now);
        let (name, machine, row_id) = (s.row.name.clone(), s.machine.clone(), s.row.id.clone());
        let jobs: Vec<Job> = s.jobs.values().map(|j| j.job.clone()).collect();
        let where_ = self.where_(&machine);
        let pipe = if pipeline.is_empty() {
            String::new()
        } else {
            format!(": {pipeline}")
        };
        self.log(format!(
            "{name} session ready in {startup_seconds:.1}s{where_}{pipe}"
        ));
        self.rates
            .record_startup(&rate_key(&row_id, &machine), startup_seconds);
        for j in jobs {
            let mut u = Map::new();
            u.insert("session_ready".into(), json!(true));
            self.update_card(&j, u);
        }
        self.start_backoff.remove(&(row_id, machine));
    }

    /// `_session_progress`: a page event's card update.
    fn session_progress(&mut self, sid: &str, claim: &str) {
        let Some(s) = self.sessions.get(sid) else {
            return;
        };
        let Some(j) = s.jobs.get(claim) else { return };
        let now = self.now();
        let since_first = j.first_page_at.map_or(0.0, |f| now - f);
        let estimate = self
            .pricing()
            .rate_for(&j.row.id, Some(&s.machine), j.done, since_first);
        let p = SessionProgress {
            generation_id: &j.row.id,
            slot: s.lane as i64,
            done_pages: j.done,
            total_pages: j.total,
            first_page_at: j.first_page_at,
            session_started_at: s.started_at,
            session_ready: s.ready_at.is_some(),
            delivered: j.delivered,
            now,
        };
        let update = session_progress_update(&p, estimate.as_ref());
        let job = j.job.clone();
        self.update_card(&job, update);
    }

    #[allow(clippy::too_many_arguments)]
    fn volume_done(
        &mut self,
        sid: &str,
        entry: SessionJob,
        pages: u32,
        failed_pages: u32,
        seconds: f64,
        stats: Value,
        cpu_pressure: Option<f64>,
        other_cpu: Option<f64>,
        sidecar_sha256: Option<String>,
    ) {
        let Some(s) = self.sessions.get_mut(sid) else {
            return;
        };
        let first_of_session = s.completed == 0;
        s.completed += 1;
        let (machine, local) = (s.machine.clone(), s.local);
        let mut ev = Map::new();
        ev.insert("cpu_pressure".into(), json!(cpu_pressure));
        ev.insert("other_cpu".into(), json!(other_cpu));
        let busy = busy_reason(&ev);
        let contended = busy.is_some();
        if let Some(busy) = &busy {
            self.log(format!(
                "{} ran on a busy host ({busy}{}); its speed is not learned as this machine's",
                entry.job.rel,
                self.where_(&machine)
            ));
        } else {
            self.rates.record_volume(
                &rate_key(&entry.row.id, &machine),
                f64::from(pages),
                seconds,
                first_of_session,
            );
        }
        let now = self.now();
        let summary = summarize_event_stats(&stats);
        let recipe = entry.row.output_affecting();
        if local {
            self.profiles.record_run(
                LOCAL_PROFILE,
                &entry.row.id,
                i64::from(pages),
                seconds,
                None,
                Some(&recipe),
                contended,
                now,
            );
        } else {
            let record = summary.as_ref().map(|s| {
                build_record(
                    s,
                    &entry.job.rel,
                    now,
                    VolumeTiming {
                        volume_pages: Some(i64::from(pages)),
                        volume_seconds: Some(seconds),
                        volume_first: first_of_session,
                    },
                )
            });
            self.profiles.record_run(
                profile_key(&machine),
                &entry.row.id,
                i64::from(pages),
                seconds,
                record.as_ref(),
                Some(&recipe),
                contended,
                now,
            );
        }
        // The local, uncontended record goes into this server's congestion history at
        // collection (it only counts if the volume is installed).
        let congestion = if local && !contended {
            summary.map(|s| {
                build_record(
                    &s,
                    &entry.job.rel,
                    now,
                    VolumeTiming {
                        volume_pages: Some(i64::from(pages)),
                        volume_seconds: Some(seconds),
                        volume_first: first_of_session,
                    },
                )
            })
        } else {
            None
        };
        self.collect(sid, entry, pages, failed_pages, sidecar_sha256, congestion);
    }

    /// `_session_exit_error`.
    fn session_exit_error(&self, sid: &str, code: Option<i32>) -> Option<String> {
        let s = self.sessions.get(sid)?;
        let clean = matches!(code, None | Some(0));
        if s.closing && s.order.is_empty() && clean {
            return None;
        }
        if s.order.is_empty() && clean {
            return None;
        }
        let detail = s.fatal_error.clone();
        let mut text = format!("the {} runner exited", s.row.name);
        if let Some(c) = code.filter(|c| *c != 0) {
            text.push_str(&format!(" with status {c}"));
        }
        match detail {
            Some(d) if !d.is_empty() => text.push_str(&format!(": {d}")),
            _ => text.push_str(" before its volumes were finished"),
        }
        Some(text)
    }

    /// `_end_session`: settle whatever was still in flight (§9.8).
    pub fn end_session(&mut self, sid: &str, _code: Option<i32>, killed: bool) {
        let Some(s) = self.sessions.remove(sid) else {
            return;
        };
        let now = self.now();
        self.ended_sessions.insert(sid.to_string(), now);
        if let Some(l) = self.lanes.iter_mut().find(|l| l.id == s.lane) {
            l.session = None;
            l.idle_at = None;
        }
        let fatal_error = s.fatal_error.clone();
        let processor_left = !self.machines.contains_key(&s.pid);
        let cancelled = s.jobs.values().any(|j| self.cancelled.contains(&j.job));
        let ready = s.ready_at.is_some();
        if !ready
            && !killed
            && fatal_error.is_some()
            && !processor_left
            && !cancelled
            && !self.stopping
        {
            self.note_start_failure(
                &s.pid,
                &s.row,
                &s.machine,
                fatal_error.as_deref().unwrap_or_default(),
            );
        }
        self.rebuild_lanes();
        self.bump();
        if s.order.is_empty() {
            if let Some(error) = &fatal_error
                && s.completed == 0
                && !processor_left
                && !killed
            {
                self.strike(&s.row, &s.machine, error);
            }
            return;
        }
        let error = fatal_error.clone().unwrap_or_else(|| {
            format!(
                "the {} runner ended before it finished this volume",
                s.row.name
            )
        });
        let blame_oldest = !self.stopping && !processor_left;
        let environment = (!s.local && !ready) || precision_refused(fatal_error.as_deref());
        let oldest = s
            .order
            .first()
            .filter(|c| blame_oldest && !environment && s.jobs.get(*c).is_some_and(|j| j.delivered))
            .cloned();
        for claim in &s.order {
            let Some(entry) = s.jobs.get(claim) else {
                continue;
            };
            self.drop_result(sid, claim);
            if Some(claim) == oldest.as_ref() {
                self.finish(&entry.job, false, Some(error.clone()), None);
            } else {
                self.release(&entry.job, &error, true);
            }
        }
        if !blame_oldest || cancelled || killed {
            return;
        }
        if s.completed == 0 {
            self.strike(&s.row, &s.machine, &error);
        } else {
            self.strikes.remove(&(s.row.id.clone(), s.machine.clone()));
        }
    }

    /// `_strike_session`: two sessions in a row with nothing finished stop the row on
    /// that machine for this scan.
    pub fn strike(&mut self, row: &Generation, machine: &str, error: &str) {
        let key = (row.id.clone(), machine.to_string());
        let n = self.strikes.entry(key.clone()).or_insert(0);
        *n += 1;
        let strikes = *n;
        if strikes >= SESSION_CRASH_LIMIT {
            self.stopped.insert(key);
            self.bump();
            self.log(format!(
                "Stopping {}{} for this scan: {strikes} sessions in a row ended without finishing a volume ({error})",
                row.name,
                self.where_(machine)
            ));
        }
    }

    /// `_note_start_failure`: space retries of a row whose runner will not start.
    pub fn note_start_failure(&mut self, pid: &str, row: &Generation, machine: &str, error: &str) {
        let key = (row.id.clone(), machine.to_string());
        let signature = self.start_signature(pid, row);
        let now = self.now();
        let backoff = StartBackoff::note_failure(
            self.start_backoff.get(&key),
            &signature,
            error,
            &row.name,
            self.settings.poll_interval,
            now,
        );
        self.log(backoff.log_line(
            &row.name,
            &self.where_(machine),
            self.settings.poll_interval,
            error,
        ));
        self.start_backoff.insert(key, backoff);
    }

    /// `start_backoffs(machine)`: `[{generation, until, failures, error}]`.
    pub fn start_backoffs(&self, machine: &str) -> Vec<Value> {
        let mut out: Vec<(&String, &StartBackoff)> = self
            .start_backoff
            .iter()
            .filter(|((_, m), _)| m == machine)
            .map(|((g, _), b)| (g, b))
            .collect();
        out.sort_by(|a, b| a.0.cmp(b.0));
        out.into_iter()
            .filter(|(_, b)| self.now() < b.until)
            .map(|(_, b)| json!({"generation": b.name, "until": b.until, "failures": b.failures, "error": b.error}))
            .collect()
    }
}
