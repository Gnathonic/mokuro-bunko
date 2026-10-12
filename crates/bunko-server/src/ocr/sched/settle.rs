//! Settling claims (spec ocr-scheduling §11.3, §15, §16): finish, release, collection
//! hand-off, returned claims and the download breaker, WebDAV arrivals and removals.

use std::path::Path;

use bunko_sched::breaker::{
    DOWNLOAD_RETURN_LIMIT, DownloadBreaker, JobReturns, return_counts_for_job,
};
use bunko_sched::failures::{NewFailure, failure_log_line, record_failure};
use serde_json::{Map, Value};

use super::{Msg, Scheduler};
use crate::ocr::collect::{CollectRequest, Outcome};
use crate::ocr::types::{Job, LOCAL, Stamp, rel_of, stamp_of};

/// The time `read_own_copy` may take.
pub const OWN_COPY_READ_SECONDS: u64 = 60;

/// What a returned claim's judgement needs after the library read its own copy.
#[derive(Debug, Clone)]
pub struct ReturnContext {
    pub pid: String,
    pub machine: String,
    pub label: String,
    pub local: bool,
    pub klass: String,
    pub error: String,
    pub stamp: Option<Stamp>,
}

/// `read_own_copy`: sequential 1 MiB reads of the whole file; the OS error and where.
pub fn read_own_copy(path: &Path) -> Option<String> {
    use std::io::Read;
    let started = std::time::Instant::now();
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return Some(format!("{e} at byte 0")),
    };
    let mut buf = vec![0u8; 1 << 20];
    let mut at: u64 = 0;
    loop {
        if started.elapsed().as_secs() > OWN_COPY_READ_SECONDS {
            return Some(format!(
                "reading it took longer than {OWN_COPY_READ_SECONDS}s at byte {at}"
            ));
        }
        match f.read(&mut buf) {
            Ok(0) => return None,
            Ok(n) => at += n as u64,
            Err(e) => return Some(format!("{e} at byte {at}")),
        }
    }
}

impl Scheduler {
    fn label_of(&self, pid: &str) -> String {
        self.machines
            .get(pid)
            .map(|m| m.label())
            .unwrap_or_else(|| pid.to_string())
    }

    /// `finish_ocr_job`: the claim's outcome, recorded (a failure gets a record and a
    /// backoff unless the claim was cancelled or its processor left).
    pub fn finish(
        &mut self,
        job: &Job,
        ok: bool,
        failure: Option<String>,
        congestion: Option<Map<String, Value>>,
    ) {
        let Some(claim) = self.claims.remove(job) else {
            self.log(format!(
                "Ignored a late outcome for {}: its claim was returned already",
                job.rel
            ));
            return;
        };
        let row = claim.row.clone();
        let returned = !ok && !self.machines.contains_key(&claim.pid);
        let cancelled = self.cancelled.remove(job);
        if !returned && !cancelled {
            self.download_returns.remove(job);
        }
        if ok {
            self.forget_candidate(job);
            let key = self.failure_key_of(job, &row);
            if self.failures.shift_remove(&key).is_some() {
                self.save_failures();
            }
            if let Some(record) = congestion {
                let ids: Vec<String> = self.settings.rows.iter().map(|r| r.id.clone()).collect();
                if let Err(e) = self.congestion.record(&row.id, record, Some(&ids)) {
                    tracing::warn!("could not record congestion: {e}");
                }
            }
        } else if returned {
            self.log(format!(
                "Returned {} for {}: {} disconnected",
                row.name,
                job.file_name(),
                claim.machine
            ));
        } else if cancelled {
            self.log(format!(
                "Skipped {} for {}: cancelled, not a failure of the volume",
                row.name,
                job.file_name()
            ));
        } else if self.settings.rows.iter().any(|r| r.id == row.id) {
            self.record_failure(job, &row, failure.as_deref());
        } else {
            self.log(format!(
                "Skipped {} for {}: the generation is no longer configured",
                row.name,
                job.file_name()
            ));
        }
        if returned {
            self.attempted.remove(job);
        }
        self.cards.shift_remove(job);
        self.bump();
        self.bump_page();
    }

    fn record_failure(
        &mut self,
        job: &Job,
        row: &bunko_core::generations::Generation,
        error: Option<&str>,
    ) {
        let key = self.failure_key_of(job, row);
        let failure = NewFailure {
            series: job.series(),
            volume: job.volume(),
            generation: &row.name,
            engine: &row.engine,
            detector: Some(row.effective_detector()),
            error,
            log_file: None,
        };
        let now = self.now();
        let record = record_failure(&mut self.failures, &key, &failure, now);
        self.log(failure_log_line(
            &row.name,
            &job.rel,
            record.attempts,
            self.settings.poll_interval,
            &record.error,
        ));
        self.save_failures();
    }

    /// `release_ocr_job`: give the claim back, recording nothing.
    pub fn release(&mut self, job: &Job, reason: &str, retry_this_scan: bool) {
        let Some(claim) = self.claims.remove(job) else {
            return;
        };
        self.log(format!(
            "Returned {} for {} to the queue: {reason}",
            claim.row.name,
            job.file_name()
        ));
        self.cancelled.remove(job);
        if retry_this_scan {
            self.attempted.remove(job);
        }
        self.cards.shift_remove(job);
        self.bump();
        self.bump_page();
    }

    /// `volume_done`: hand the result to a helper thread for installation.
    pub fn collect(
        &mut self,
        sid: &str,
        entry: super::session::SessionJob,
        pages: u32,
        failed_pages: u32,
        sidecar_sha256: Option<String>,
        congestion: Option<Map<String, Value>>,
    ) {
        let job = entry.job.clone();
        let owned = self.claims.get(&job).is_some_and(|c| {
            c.sid.as_deref() == Some(sid) && c.claim.as_deref() == Some(entry.claim.as_str())
        });
        let uploaded = self.results.remove(&(sid.to_string(), entry.claim.clone()));
        if !owned {
            self.log(format!("Ignored a late {} outcome for {}: a file written now would land beside the one the next owner writes", entry.row.name, job.rel));
            if let Some((path, _)) = uploaded
                && let Some(dir) = path.parent()
            {
                let _ = std::fs::remove_dir_all(dir);
            }
            return;
        }
        let Some(claim) = self.claims.get_mut(&job) else {
            return;
        };
        claim.settling = true;
        let stamp = claim.stamp;
        let pid = claim.pid.clone();
        let machine = self.machines.get(&pid);
        let local = machine.is_none_or(|m| m.local);
        let (result, expected) = match uploaded {
            Some((path, sha)) => {
                if let Some(announced) = &sidecar_sha256
                    && !announced.eq_ignore_ascii_case(&sha)
                {
                    // The volume_done names other bytes than the ones that arrived.
                    (path, Some(announced.clone()))
                } else {
                    (path, Some(sha))
                }
            }
            None => (
                self.storage()
                    .join(".processing")
                    .join(sid)
                    .join(&entry.claim)
                    .join(bunko_proto::RESULT_FILE),
                sidecar_sha256.clone(),
            ),
        };
        let account = machine.and_then(|m| m.username.clone());
        let runner_build = machine
            .map(|m| {
                if !m.host.runner_build.is_empty() {
                    m.host.runner_build.clone()
                } else if !m.host.version.is_empty() {
                    format!("mokuro-bunko {}", m.host.version)
                } else {
                    self.deps.generator.clone()
                }
            })
            .filter(|s| !s.is_empty());
        let machine_name = if local {
            LOCAL.to_string()
        } else {
            machine.map(|m| m.name.clone()).unwrap_or_default()
        };
        let req = CollectRequest {
            job,
            row: entry.row.clone(),
            rows: self.settings.rows.clone(),
            library: self.library(),
            inbox: self.deps.layout.inbox(),
            result,
            expected_sha256: expected,
            stamp,
            machine: machine_name,
            account,
            runner_build,
            pages: Some(i64::from(pages)),
            failed_pages: Some(i64::from(failed_pages)),
            generator: self.deps.generator.clone(),
            db: self.deps.db.clone(),
            facts: self.deps.facts.clone(),
            locks: self.deps.locks.clone(),
            sid: sid.to_string(),
            claim: entry.claim.clone(),
            congestion,
            upgrade: self.deps.upgrade.clone(),
        };
        self.run_background(Box::new(move || {
            Msg::Collected(Box::new(crate::ocr::collect::run(req)))
        }));
    }

    /// A helper thread finished installing (or refused) a result.
    pub fn collected(&mut self, done: crate::ocr::collect::CollectDone) {
        match done.outcome {
            Outcome::Installed(path) => {
                let mut u = Map::new();
                u.insert("percent".into(), Value::from(100));
                u.insert("status".into(), Value::from("done"));
                self.update_card(&done.job, u);
                self.log(format!("Wrote {}", path.display()));
                self.finish(&done.job, true, None, done.congestion);
            }
            Outcome::Failed(error) => self.finish(&done.job, false, Some(error), None),
            Outcome::Discarded => self.release(&done.job, super::DISCARDED, true),
            Outcome::Busy(reason) => self.release(&done.job, &reason, true),
        }
    }

    /// A result upload landed for an outstanding claim.
    ///
    /// `name` comes off the wire: it is compared, never joined. The result's path is
    /// built from the server's own ids and [`bunko_proto::RESULT_FILE`] only (a Windows
    /// prefix such as `C:x.mokuro` would otherwise REPLACE the joined path, and the cleanup
    /// below would `remove_dir_all` outside `.processing`; and the volume's own name may
    /// hold characters this OS refuses in a file name).
    pub fn result_stored(&mut self, pid: &str, sid: &str, claim: &str, name: &str, sha256: String) {
        if !bunko_proto::valid_id(sid) || !bunko_proto::valid_id(claim) {
            return;
        }
        let dir = self.storage().join(".processing").join(sid).join(claim);
        let wanted = self
            .sessions
            .get(sid)
            .filter(|s| s.pid == pid)
            .and_then(|s| s.jobs.get(claim))
            .map(|j| j.sidecar_name.clone());
        match wanted {
            Some(expected) if expected == name => {
                let path = dir.join(bunko_proto::RESULT_FILE);
                self.results
                    .insert((sid.to_string(), claim.to_string()), (path, sha256));
            }
            Some(expected) => {
                let _ = std::fs::remove_dir_all(&dir);
                let reason = format!(
                    "its sidecar arrived as {}, not {}",
                    bunko_sched::py::py_repr(&Value::String(name.chars().take(80).collect())),
                    bunko_sched::py::py_repr(&Value::String(expected))
                );
                if let Some(job) = self
                    .sessions
                    .get(sid)
                    .and_then(|s| s.jobs.get(claim))
                    .map(|j| (j.job.clone(), j.row.clone()))
                {
                    let req_like = (job.0, job.1);
                    self.audit_rejection(&req_like.0, &req_like.1, pid, &reason);
                }
                self.kill_session_blaming_pub(sid, format!("a sidecar arrived as {name}"));
            }
            None => {
                // A cancelled or ended claim may still be answered: nothing to do.
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }

    fn audit_rejection(
        &self,
        job: &Job,
        row: &bunko_core::generations::Generation,
        pid: &str,
        reason: &str,
    ) {
        let Some(db) = &self.deps.db else { return };
        let cbz = job.path(&self.library());
        let (plain, _) = crate::ocr::owed::sidecar_paths(&cbz, &row.sidecar_suffix());
        let target = rel_of(&self.library(), &plain)
            .map(|r| format!("{}{r}", bunko_proto::ARCHIVES_ROOT))
            .unwrap_or_default();
        let machine = self.machines.get(pid);
        let details = bunko_db::AuditDetails::new()
            .with("generation", row.name.clone())
            .with("generation_id", row.id.clone())
            .with(
                "machine",
                machine.map(|m| m.name.clone()).unwrap_or_default(),
            )
            .with("engine", row.engine.clone())
            .with("reason", reason.chars().take(500).collect::<String>());
        let account = machine.and_then(|m| m.username.clone());
        let event = bunko_db::NewAuditEvent::new("ocr_sidecar_rejected")
            .actor(account.as_deref())
            .target_type("sidecar")
            .target_path(&target)
            .details(details);
        if let Err(e) = db.log_audit_event(&event) {
            tracing::warn!("could not audit a rejected sidecar: {e}");
        }
    }

    // --- returned claims and the download breaker -------------------------------------------

    /// `_download_delivered`: the processor has the verified archive.
    pub fn download_delivered(
        &mut self,
        pid: &str,
        job: &Job,
        detail: &std::collections::BTreeMap<String, Value>,
    ) {
        let stamp = stamp_of(&job.path(&self.library()));
        if let Some(c) = self.claims.get_mut(job) {
            c.stamp = stamp;
        }
        self.download_returns.remove(job);
        let label = self.label_of(pid);
        let reopened = self
            .breakers
            .entry(pid.to_string())
            .or_insert_with(|| DownloadBreaker::new(label.clone()))
            .note_delivered();
        if let Some(m) = self.machines.get_mut(pid) {
            m.transfer.note_ready(detail);
            m.transfer.held_until = None;
            m.transfer.held_error.clear();
        }
        if reopened {
            self.log(format!("{label} fetched an archive again; no longer held"));
            self.bump();
        }
        let requests = detail
            .get("requests")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let anomalous = requests > 1.0
            || detail
                .get("restarts")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                > 0.0
            || detail.get("repairs").and_then(Value::as_f64).unwrap_or(0.0) > 0.0
            || detail.get("verdict").is_some_and(|v| !v.is_null());
        let line = format!(
            "{label} fetched {}: {}",
            job.rel,
            serde_json::to_string(detail).unwrap_or_default()
        );
        if anomalous {
            self.log(line);
        } else {
            tracing::debug!("{line}");
        }
    }

    /// `_judge_returned` (spec §16.2).
    pub fn judge_returned(
        &mut self,
        pid: &str,
        _sid: &str,
        entry: super::session::SessionJob,
        class: &str,
        error: &str,
    ) {
        let klass: String = if class.is_empty() {
            "local".into()
        } else {
            class.chars().take(40).collect()
        };
        let error: String = error.chars().take(300).collect();
        let now = self.now();
        let (label, machine, local) = match self.machines.get_mut(pid) {
            Some(m) => {
                m.transfer.note_returned(&klass, &error, now);
                (m.label(), m.name.clone(), m.local)
            }
            None => (pid.to_string(), pid.to_string(), false),
        };
        let job = entry.job.clone();
        if self.stopping {
            self.release(&job, "the worker is stopping", false);
            return;
        }
        let path = job.path(&self.library());
        let meta = match std::fs::metadata(&path) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.release(&job, &format!("{label}: the archive is gone"), false);
                return;
            }
            Err(_) => None,
        };
        if let (Some(m), Some(size)) = (&meta, entry.archive_size)
            && m.len() != size
        {
            self.release(
                &job,
                &format!("{label}: the archive changed after it was sent"),
                true,
            );
            return;
        }
        let ctx = ReturnContext {
            pid: pid.to_string(),
            machine,
            label,
            local,
            klass: klass.clone(),
            error,
            stamp: stamp_of(&path),
        };
        if klass == "stalled" || klass == "differs" {
            if let Some(c) = self.claims.get_mut(&job) {
                c.settling = true;
            }
            let job2 = job.clone();
            self.run_background(Box::new(move || {
                let error = read_own_copy(&path);
                Msg::OwnCopyRead {
                    job: job2,
                    error,
                    then: Box::new(ctx),
                }
            }));
            return;
        }
        self.count_return(job, ctx);
    }

    pub fn own_copy_read(&mut self, job: Job, error: Option<String>, ctx: ReturnContext) {
        if let Some(c) = self.claims.get_mut(&job) {
            c.settling = false;
        }
        if let Some(e) = error {
            self.finish(
                &job,
                false,
                Some(format!(
                    "the library cannot read its own copy of this archive: {e}"
                )),
                None,
            );
            return;
        }
        self.count_return(job, ctx);
    }

    fn count_return(&mut self, job: Job, ctx: ReturnContext) {
        let now = self.now();
        let breaker = self
            .breakers
            .entry(ctx.pid.clone())
            .or_insert_with(|| DownloadBreaker::new(ctx.label.clone()));
        let noted = breaker.note_return(&ctx.klass, &ctx.error, now);
        if let Some(opened) = &noted.opened {
            self.log(opened.log_line(&ctx.label));
            if let Some(m) = self.machines.get_mut(&ctx.pid) {
                m.transfer.held_until = Some(opened.until);
                m.transfer.held_error = opened.last_error.clone();
            }
            self.bump();
            self.bump_page();
        }
        let counted = return_counts_for_job(noted.job_counted, &ctx.klass);
        let returns = JobReturns::note(
            self.download_returns.remove(&job),
            ctx.stamp,
            &ctx.klass,
            &ctx.error,
            &ctx.machine,
            counted,
            now,
        );
        if counted && returns.count >= DOWNLOAD_RETURN_LIMIT {
            let summary = returns.failure_summary(&ctx.klass, &ctx.error);
            self.finish(&job, false, Some(summary), None);
            return;
        }
        self.download_returns.insert(job.clone(), returns);
        if !ctx.local {
            self.returned_by
                .entry(job.clone())
                .or_default()
                .insert(ctx.pid.clone());
        }
        self.release(
            &job,
            &format!(
                "{} could not fetch the archive ({}): {}",
                ctx.label, ctx.klass, ctx.error
            ),
            true,
        );
    }

    // --- WebDAV hooks ---------------------------------------------------------------------

    /// Running jobs whose archive `gone` names and whose file is no longer the claimed
    /// one are cancelled: a session only when ALL its claims are stale (otherwise the
    /// stale result is discarded at collection).
    fn cancel_stale_jobs(&mut self, gone: impl Fn(&str) -> bool) {
        let library = self.library();
        let stale: Vec<Job> = self
            .claims
            .iter()
            .filter(|(j, c)| gone(&j.rel) && !c.settling && c.stamp != stamp_of(&j.path(&library)))
            .map(|(j, _)| j.clone())
            .collect();
        if stale.is_empty() {
            return;
        }
        let sids: Vec<String> = self.sessions.keys().cloned().collect();
        for sid in sids {
            let Some(s) = self.sessions.get(&sid) else {
                continue;
            };
            let held: Vec<Job> = s.jobs.values().map(|j| j.job.clone()).collect();
            if !held.is_empty() && held.iter().all(|j| stale.contains(j)) {
                for j in &held {
                    self.cancelled.insert(j.clone());
                }
                self.log(format!(
                    "Stopping the {} session: its archive was replaced or removed",
                    s.row.name
                ));
                self.kill_session(&sid, None);
            }
        }
    }

    /// `archive_arrived(cbz)`: a `.cbz` is now in place (PUT / MOVE / COPY).
    pub fn archive_arrived(&mut self, path: &Path) {
        let library = self.library();
        let Some(rel) = rel_of(&library, path) else {
            return;
        };
        if !bunko_library::sidecar::is_cbz_name(&rel) {
            return;
        }
        self.cancel_stale_jobs(|r| r == rel);
        let probe = self.upgrade_probe();
        let entry = crate::ocr::owed::compute(
            &library,
            path,
            &self.settings.rows,
            self.deps.facts.as_ref(),
            probe
                .as_deref()
                .map(|p| p as &dyn crate::ocr::owed::UpgradeProbe),
        );
        match entry {
            Some(mut v) => {
                v.pages = crate::ocr::owed::pages_of(path, self.deps.facts.as_ref());
                self.owed.volumes.insert(rel, v);
            }
            None => {
                self.owed.volumes.remove(&rel);
            }
        }
        self.bump();
        self.bump_page();
        if self.processing_hold().is_none() {
            self.maybe_start_scan();
            for lane in &mut self.lanes {
                lane.idle_at = None;
            }
        }
    }

    /// `archive_removed(path)`: an archive or a whole folder left the library.
    pub fn archive_removed(&mut self, path: &Path) {
        let library = self.library();
        let Some(rel) = rel_of(&library, path) else {
            if path == library {
                self.cancel_stale_jobs(|_| true);
                self.owed.volumes.clear();
                self.bump();
                self.bump_page();
            }
            return;
        };
        let is_archive = rel.to_lowercase().ends_with(".cbz");
        let prefix = format!("{rel}/");
        let gone = |r: &str| {
            if is_archive {
                r == rel
            } else {
                r.starts_with(&prefix)
            }
        };
        self.cancel_stale_jobs(gone);
        let before = self.owed.volumes.len();
        self.owed.volumes.retain(|r, _| {
            !(if is_archive {
                r == &rel
            } else {
                r.starts_with(&prefix)
            })
        });
        if self.owed.volumes.len() != before {
            self.bump_page();
        }
        self.bump();
    }
}

impl Scheduler {
    pub fn kill_session_blaming_pub(&mut self, sid: &str, error: String) {
        if let Some(s) = self.sessions.get_mut(sid) {
            s.fatal_error = Some(error);
        }
        let Some(s) = self.sessions.get(sid) else {
            return;
        };
        let pid = s.pid.clone();
        let claims = s.order.clone();
        if let Some(m) = self.machines.get(&pid) {
            for claim in claims {
                m.send(bunko_proto::Op::Cancel {
                    sid: Some(sid.to_string()),
                    claim: Some(claim),
                    bid: None,
                });
            }
            m.send(bunko_proto::Op::CloseSession {
                sid: sid.to_string(),
            });
        }
        self.end_session(sid, None, false);
    }
}
