//! Foreign `series.json` / `catalog.json` versions and first adoption (spec §9.3,
//! §9.4; D8, D18).
//!
//! **Submissions.** The feed queues a `json` notification in the same index
//! transaction that ingests a foreign `series.json` version (its `(pid, cver)` not in
//! the ledger), or any `series.json` that lands at a series key the server has no
//! facts row for ([`CloudHooks::has_series_facts`](crate::feed::CloudHooks::has_series_facts)); the notification is durable with
//! that transaction and delivered after it commits (again after a crash). Delivery
//! opens a `submission` row (`fetching`) in `state.sqlite`; the store runtime then
//! reads exactly that version (a pinned read: the provider's pin, or a re-stat that
//! must still show the cver), keeps its bytes under `<dir>/submissions/<id>.json`
//! (`queued`), and hands them to [`CloudHooks::series_submission`](crate::feed::CloudHooks::series_submission) on the events
//! thread: `applied`, or `refused` with a `lost-write` conflict row. A version
//! superseded before it could be read is dropped (the newer one has its own row).
//! Rows still `fetching` or `queued` are resumed at the next start; they wait for the
//! server's hooks when those are not installed yet.
//!
//! While a series has an unanswered submission its Compiler `series.json` ops stay
//! HELD. A submitted version (queued, applied or refused) is a valid Compiler
//! `Replace` base: the recompile that follows the merge publishes on top of it, which
//! counts as merged under D8; after a refusal it overwrites the file (the provider's
//! history keeps it). A Compiler PREFLIGHT that meets a foreign `series.json` captures
//! it the same way, inline, and indexes that version so the recompile sees it as its
//! base.
//!
//! A foreign `catalog.json` is only logged: it is derived, and the next compile
//! overwrites it — except at first adoption.
//!
//! **First adoption (INGEST_FACTS, D18).** After the first full sync and before the
//! lease is claimed or any Compiler op leaves HELD (state meta `adopted_at` unset):
//! every `series.json` in the index is applied as a submission (in order, each
//! answered before the next); then every `catalog.json` entry whose series folder has
//! no `series.json` is applied as a facts-only submission, with this synthesized body:
//!
//! ```json
//! {"version": 2, "updated_at": "<the entry's updated_at>",
//!  "external_ids": {..}, "titles": {..}, "synonyms": [..], "tag": "..", "unit": ".."}
//! ```
//!
//! The fact keys are copied verbatim from the entry when present; `volumes` and
//! `spine_offset` are absent, so the series' shelf alignment is untouched. The row's
//! path is the series' `series.json` path, its pid the `catalog.json`'s and its cver
//! that file's cver with `#catalog` appended (never mistaken for a `series.json`
//! version).
//! Then `adopted_at` is set and the lease may claim (§5.3 order: … → INGEST_FACTS →
//! CLAIM_LEASE → replay Deferred → STEADY).

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::backend::{BErr, Backend, RNode};
use crate::cache::{SMALL_MAX, now_ms};
use crate::feed::{EngineState, SubmissionResult};
use crate::index::{ROOT_EID, RelPath};
use crate::names::fold;
use crate::state::{ConflictRow, SubmissionRow};
use crate::store::{Core, Ver};

pub(crate) const SERIES_JSON: &str = "series.json";
pub(crate) const CATALOG_JSON: &str = "catalog.json";

/// How long a failed fetch waits before the next try (doubling, capped).
const FETCH_RETRY: Duration = Duration::from_secs(1);
const FETCH_RETRY_MAX: Duration = Duration::from_secs(60);

/// `<Series>/series.json`: a series key's facts file.
pub(crate) fn is_series_json(rel: &RelPath) -> bool {
    matches!(rel.segments(), [_, name] if fold(name) == SERIES_JSON)
}

pub(crate) fn is_catalog_json(rel: &RelPath) -> bool {
    matches!(rel.segments(), [name] if fold(name) == CATALOG_JSON)
}

/// The facts-only `series.json` body for one `catalog.json` entry (module docs).
pub fn synthesize_series_json(entry: &serde_json::Value) -> Option<Vec<u8>> {
    let e = entry.as_object()?;
    let mut out = serde_json::Map::new();
    out.insert("version".into(), 2.into());
    out.insert(
        "updated_at".into(),
        e.get("updated_at")
            .cloned()
            .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into()),
    );
    for k in ["external_ids", "titles", "synonyms", "tag", "unit"] {
        if let Some(v) = e.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    serde_json::to_vec(&serde_json::Value::Object(out)).ok()
}

/// What happened to one submission run.
enum Outcome {
    Answered,
    /// Not now (the hooks are not installed, the read failed): the row stays.
    Later,
    /// The server could not answer yet (busy): the row stays and is offered again.
    Retry(String),
}

impl<B: Backend> Core<B> {
    fn submission_file(&self, id: i64) -> PathBuf {
        self.dir.join("submissions").join(format!("{id}.json"))
    }

    /// First adoption is done (Compiler ops may leave HELD, the lease may claim).
    pub(crate) fn adopted(&self) -> bool {
        self.adopted.load(Ordering::Acquire)
    }

    /// The feed's foreign-JSON notification (on the feed task, after the ingest
    /// commit): open a submission and process it on the store runtime.
    pub(crate) fn foreign_json(self: &Arc<Self>, rel: &RelPath, pid: &str, cver: &str) {
        if is_catalog_json(rel) {
            tracing::info!(
                "a foreign catalog.json version arrived; the next compile overwrites it"
            );
            return;
        }
        if !is_series_json(rel) {
            return;
        }
        let Some((id, state, _)) = self.state.submission_open(&rel.to_string(), pid, cver) else {
            return;
        };
        if matches!(state.as_str(), "fetching" | "queued") {
            if self.fail_at("SUBMISSION_FETCHING").is_err() {
                return;
            }
            let core = self.clone();
            tokio::spawn(async move { core.process_submission(id).await });
        }
    }

    /// A Compiler PREFLIGHT (op `op_id`, which ends in CONFLICT) met the foreign
    /// version `n` at `dest`: capture and answer it now (the op path waits), and
    /// index that version (dropping the op's pending marker) so the recompile that
    /// follows the merge sees it as its base.
    pub(crate) async fn capture_submission(
        self: &Arc<Self>,
        dest: &RelPath,
        n: &RNode,
        op_id: &str,
    ) {
        {
            let idx = self.index.lock();
            let r = idx.clear_pending(op_id).and_then(|mut cleared| {
                let tx = idx.begin()?;
                cleared.extend(idx.upsert(
                    &tx,
                    n,
                    idx.generation(),
                    self.caps.mtime_granularity_ns,
                )?);
                tx.commit()?;
                Ok(cleared)
            });
            drop(idx);
            match r {
                Ok(ch) => self.emit_changes(&ch),
                Err(e) => tracing::warn!("series.json submission: index: {e}"),
            }
        }
        let Some((id, _, _)) = self
            .state
            .submission_open(&dest.to_string(), &n.pid, &n.cver)
        else {
            return;
        };
        self.clone().process_submission(id).await;
    }

    /// Fetch (if needed) and apply submission `id`; one at a time, in order. A
    /// failed fetch retries with backoff while the store runs.
    pub(crate) async fn process_submission(self: Arc<Self>, id: i64) {
        let mut wait = FETCH_RETRY;
        loop {
            let outcome = {
                let _one = self.subs_lock.lock().await;
                self.process_once(id).await
            };
            match outcome {
                Ok(Outcome::Answered) => return,
                Ok(Outcome::Later) => return,
                Ok(Outcome::Retry(why)) => {
                    if self.crashed.load(Ordering::Acquire) {
                        return;
                    }
                    tracing::debug!(id, "series.json submission not answered yet: {why}");
                    // The server already waited for its lock; ask again soon.
                    tokio::time::sleep(FETCH_RETRY).await;
                }
                Err(e) => {
                    if self.crashed.load(Ordering::Acquire) {
                        return;
                    }
                    tracing::info!(id, "series.json submission: {e}; retrying");
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(FETCH_RETRY_MAX);
                }
            }
        }
    }

    async fn process_once(&self, id: i64) -> Result<Outcome, BErr> {
        let Some(row) = self.state.submission_get(id) else {
            return Ok(Outcome::Answered);
        };
        let bytes = match row.state.as_str() {
            "fetching" => match self.fetch_submission(&row).await? {
                Some(b) => b,
                None => return Ok(Outcome::Answered),
            },
            "queued" => match std::fs::read(self.submission_file(id)) {
                Ok(b) => b,
                Err(_) => {
                    // The bytes went missing: read the version again.
                    self.state.submission_set(id, "fetching");
                    match self.fetch_submission(&row).await? {
                        Some(b) => b,
                        None => return Ok(Outcome::Answered),
                    }
                }
            },
            _ => return Ok(Outcome::Answered),
        };
        if !self.hooks_set.load(Ordering::Acquire) {
            // Answered once the server installs its hooks.
            return Ok(Outcome::Later);
        }
        Ok(self.apply_submission(&row, &bytes))
    }

    /// Read the row's exact version and pin its bytes (`queued`); `None` when the
    /// version is gone (superseded: the row is dropped).
    async fn fetch_submission(&self, row: &SubmissionRow) -> Result<Option<Vec<u8>>, BErr> {
        let v = match self.index.lock().node(&row.pid) {
            Some(n) if n.cver == row.cver => Some(Ver {
                pid: n.pid,
                cver: n.cver,
                pin: n.pin,
                size: n.size,
                hash: n.hash,
            }),
            _ => None,
        };
        let v = match v {
            Some(v) => v,
            None => match self.backend.stat(&row.pid).await? {
                Some(n) if !n.trashed && n.cver == row.cver => Ver {
                    pid: n.pid,
                    cver: n.cver,
                    pin: n.pin,
                    size: n.size,
                    hash: n.hash,
                },
                _ => {
                    tracing::info!(path = %row.path, "series.json submission: superseded before it was read");
                    self.state.submission_delete(row.id);
                    return Ok(None);
                }
            },
        };
        if v.size > SMALL_MAX {
            self.refuse(row, "the series.json is too large to merge");
            return Ok(None);
        }
        let data = match self.read_pinned(&v, None).await {
            Ok(d) => d,
            Err(BErr::PreconditionFailed | BErr::NotFound) => {
                tracing::info!(path = %row.path, "series.json submission: superseded while it was read");
                self.state.submission_delete(row.id);
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        self.pin_bytes(row.id, &data)
            .map_err(|e| BErr::Transient(format!("keeping the submission: {e}")))?;
        // The compile that follows reads this version too: from the cache, not Drive.
        let sha: [u8; 32] = Sha256::digest(&data).into();
        if let Err(e) = self.blobs.insert(&v.pid, &v.cver, &data, sha) {
            tracing::warn!("series.json submission: blob cache: {e}");
        }
        Ok(Some(data))
    }

    /// Keep a submission's bytes (fsynced) and mark it `queued`.
    fn pin_bytes(&self, id: i64, data: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let path = self.submission_file(id);
        std::fs::create_dir_all(self.dir.join("submissions"))?;
        let tmp = path.with_extension("part");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(data)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
        self.state
            .submission_fetched(id, &hex::encode(Sha256::digest(data)));
        Ok(())
    }

    fn refuse(&self, row: &SubmissionRow, why: &str) {
        self.state.submission_set(row.id, "refused");
        self.state
            .conflict_add(ConflictRow::LostWrite, &row.path, Some(&row.pid), why);
        self.wake.notify_one();
    }

    /// Hand the bytes to the server's merge (on the events thread) and record the
    /// answer; HELD Compiler ops of the series may go after this.
    fn apply_submission(&self, row: &SubmissionRow, bytes: &[u8]) -> Outcome {
        let Some(rel) = RelPath::parse(&row.path) else {
            self.refuse(row, "not a series.json path");
            return Outcome::Answered;
        };
        let parent =
            RelPath::from_segments(rel.segments()[..1].iter().cloned()).unwrap_or_default();
        let series = self.abs(&parent);
        // The slot runs the hook on the events thread (handing this worker back).
        match self.hooks.series_submission(&series, bytes) {
            SubmissionResult::Applied => {
                self.state.submission_set(row.id, "applied");
                self.wake.notify_one();
            }
            SubmissionResult::Refused(why) => self.refuse(row, &why),
            SubmissionResult::Later(why) => return Outcome::Retry(why),
        }
        Outcome::Answered
    }

    /// Resume every unanswered submission (startup, hooks installed).
    pub(crate) async fn resume_submissions(self: Arc<Self>) {
        for row in self.state.submissions_pending() {
            self.clone().process_submission(row.id).await;
        }
    }

    // ---- first adoption -----------------------------------------------------------

    /// INGEST_FACTS (§9.4), once per store: after the first full sync, before the
    /// lease and any Compiler write. Sets `adopted` (and wakes the lease starter).
    pub(crate) async fn adopt(self: Arc<Self>) {
        if self.state.meta_get("adopted_at").is_some() {
            self.mark_adopted();
            self.resume_submissions().await;
            return;
        }
        let mut rx = self.feed.subscribe();
        if rx.wait_for(|s| *s == EngineState::Steady).await.is_err() {
            return;
        }
        let (series, catalog) = self.adoption_inputs();
        if !series.is_empty() || catalog.is_some() {
            let mut told = false;
            loop {
                let installed = self.hooks_notify.notified();
                tokio::pin!(installed);
                installed.as_mut().enable();
                if self.hooks_set.load(Ordering::Acquire) {
                    break;
                }
                if !told {
                    tracing::info!("first adoption waits for the server's hooks");
                    told = true;
                }
                installed.await;
            }
        }
        self.adopting.store(true, Ordering::Release);
        tracing::info!(
            series = series.len(),
            catalog = catalog.is_some(),
            "first adoption: ingesting the library's facts"
        );
        // Whatever the feed already queued, then every series.json.
        self.clone().resume_submissions().await;
        let (series, catalog) = self.adoption_inputs();
        for (rel, n) in &series {
            if self.crashed.load(Ordering::Acquire) {
                return;
            }
            if let Some((id, _, _)) = self
                .state
                .submission_open(&rel.to_string(), &n.pid, &n.cver)
            {
                self.clone().process_submission(id).await;
            }
        }
        if let Some(cat) = catalog {
            self.clone().adopt_catalog(cat).await;
        }
        if self.crashed.load(Ordering::Acquire) {
            return;
        }
        let _ = self.state.meta_set("adopted_at", &now_ms().to_string());
        self.adopting.store(false, Ordering::Release);
        tracing::info!("first adoption done");
        self.mark_adopted();
    }

    fn mark_adopted(&self) {
        self.adopted.store(true, Ordering::Release);
        self.adopted_tx.send_replace(true);
        self.wake.notify_one();
    }

    /// Every `<Series>/series.json` node, and the root `catalog.json` node.
    #[allow(clippy::type_complexity)]
    fn adoption_inputs(&self) -> (Vec<(RelPath, RNode)>, Option<RNode>) {
        let idx = self.index.lock();
        let mut series = Vec::new();
        let mut catalog = None;
        for top in idx.children(ROOT_EID) {
            if !top.is_dir {
                if fold(&top.name) == CATALOG_JSON
                    && let Some(n) = top.winner_pid.as_deref().and_then(|p| idx.node(p))
                {
                    catalog = Some(n);
                }
                continue;
            }
            for c in idx.children(top.eid) {
                if !c.is_dir
                    && fold(&c.name) == SERIES_JSON
                    && let Some(n) = c.winner_pid.as_deref().and_then(|p| idx.node(p))
                    && let Some(rel) = RelPath::from_segments([top.name.clone(), c.name.clone()])
                {
                    series.push((rel, n));
                }
            }
        }
        (series, catalog)
    }

    /// Facts-only submissions for the `catalog.json` entries whose series folder has
    /// no `series.json`.
    async fn adopt_catalog(self: Arc<Self>, n: RNode) {
        let v = Ver {
            pid: n.pid.clone(),
            cver: n.cver.clone(),
            pin: n.pin.clone(),
            size: n.size,
            hash: n.hash.clone(),
        };
        let mut wait = FETCH_RETRY;
        let data = loop {
            if n.size > 16 * SMALL_MAX {
                tracing::warn!("first adoption: catalog.json is too large; skipped");
                return;
            }
            match self.read_pinned(&v, None).await {
                Ok(d) => break d,
                Err(BErr::PreconditionFailed | BErr::NotFound) => {
                    tracing::info!("first adoption: catalog.json changed while read; skipped");
                    return;
                }
                Err(e) => {
                    if self.crashed.load(Ordering::Acquire) {
                        return;
                    }
                    tracing::info!("first adoption: catalog.json: {e}; retrying");
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(FETCH_RETRY_MAX);
                }
            }
        };
        let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&data) else {
            tracing::warn!("first adoption: catalog.json does not parse; skipped");
            return;
        };
        let entries = doc
            .get("series")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default();
        for entry in entries {
            let Some(title) = entry.get("series_title").and_then(|t| t.as_str()) else {
                continue;
            };
            let Some(folder) = self.series_folder(title) else {
                continue;
            };
            let Some(rel) = RelPath::from_segments([folder.clone(), SERIES_JSON.to_string()])
            else {
                continue;
            };
            let has_own_file = {
                let idx = self.index.lock();
                idx.lookup(&RelPath::from_segments([folder.clone()]).unwrap_or_default())
                    .map(|e| {
                        idx.children(e.eid)
                            .iter()
                            .any(|c| !c.is_dir && fold(&c.name) == SERIES_JSON)
                    })
                    .unwrap_or(true)
            };
            if has_own_file {
                continue;
            }
            let Some(body) = synthesize_series_json(&entry) else {
                continue;
            };
            // The cver is marked so a resumed row is never fetched as a series.json.
            let cver = format!("{}#catalog", n.cver);
            let Some((id, state, _)) = self.state.submission_open(&rel.to_string(), &n.pid, &cver)
            else {
                continue;
            };
            if state == "fetching"
                && let Err(e) = self.pin_bytes(id, &body)
            {
                tracing::warn!("first adoption: keeping a catalog submission: {e}");
                continue;
            }
            self.clone().process_submission(id).await;
        }
    }

    /// The series folder a `catalog.json` title names: the exact name, else the
    /// one folder whose name folds the same.
    fn series_folder(&self, title: &str) -> Option<String> {
        let idx = self.index.lock();
        let tops = idx.children(ROOT_EID);
        if let Some(e) = tops.iter().find(|e| e.is_dir && e.name == title) {
            return Some(e.name.clone());
        }
        let f = fold(title);
        let mut hits = tops.iter().filter(|e| e.is_dir && fold(&e.name) == f);
        match (hits.next(), hits.next()) {
            (Some(e), None) => Some(e.name.clone()),
            _ => None,
        }
    }
}
