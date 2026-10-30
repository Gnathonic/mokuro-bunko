//! The `cloud` suite: Bunko and one cloud (cloudsim or rclone), no reader, every party
//! its own process. One library, one server holding its lease, then in order: cold
//! start, adoption, the outbox under faults, OCR results leaving through the outbox,
//! the lease handed to a second server, and a provider that is full.

use super::super::genlib;
use super::super::http;
use super::super::server::{Backend, CloudLogin, SeedUser, Server, ServerOptions};
use super::cloudsim::Cloudsim;
use super::fakeproc::{FakeOptions, FakeProcessor};
use super::rclone::{self, Rclone};
use super::results::{Outcome, Report, Scenario};
use super::{Paths, Provider};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The cloud account every party uses.
pub const ACCOUNT: &str = "bunko";
const SERIES: &str = "Series";
/// The origin rclone allows for the CORS preflight check: any one serves, no browser
/// runs in this suite.
const PREFLIGHT_ORIGIN: &str = "http://127.0.0.1:4173";

/// Short lease timing (`MOKURO_TEST_CLOUD_TUNE`): a handover takes about ttl + ttl/3
/// instead of 15 minutes (an hour on MEGA). A heartbeat keeps writes open for ttl/2,
/// so MEGA, at one API command a second, gets a longer ttl than a 4 s heartbeat.
pub fn tune(p: Provider) -> String {
    let ttl = if p == Provider::Mega { 30 } else { 12 };
    format!("lease_ttl_s={ttl},lease_settle_ms=1000,retry_base_ms=200,queue_floor_ms=500")
}
const READER: &str = "reader";
const PROCESSOR: &str = "ocr";
/// The tag the seeded `series.json` carries, for the adoption check.
const ADOPTED_TAG: &str = "Adopted";

/// The four volumes: 1 has a sidecar; 2 gets one by PUT (faults); 3 and 4 are left
/// for the fake processor (and 3 for the refused PUT on a full provider).
const VOLUMES: [&str; 4] = ["Vol 1", "Vol 2", "Vol 3", "Vol 4"];

fn sha256_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

fn cloud_path(rel: &str) -> String {
    format!("{}/{rel}", super::super::server::CLOUD_FOLDER)
}

/// `/mokuro-reader/Series/Vol%201.cbz`: a library path as the bridge serves it.
fn dav_path(rel: &str) -> String {
    format!(
        "/{}/{}",
        super::super::server::CLOUD_FOLDER,
        rel.replace(' ', "%20")
    )
}

/// Wait for `check` to give `Some`, polling; `Err` with `what` after `timeout`.
fn wait_for<T>(
    timeout: Duration,
    what: &str,
    mut check: impl FnMut() -> Result<Option<T>>,
) -> Result<T> {
    let start = Instant::now();
    let mut last_err = None;
    while start.elapsed() < timeout {
        match check() {
            Ok(Some(v)) => return Ok(v),
            Ok(None) => {}
            Err(e) => last_err = Some(e),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    match last_err {
        Some(e) => Err(e.context(format!("{what} (after {} s)", timeout.as_secs()))),
        None => bail!("{what} (after {} s)", timeout.as_secs()),
    }
}

/// The cloud the server keeps its library in.
enum CloudSide {
    Sim(Cloudsim),
    Rclone(Rclone),
}

struct Rig<'a> {
    p: Provider,
    paths: &'a Paths,
    work: PathBuf,
    cloud: CloudSide,
    /// The server holding the lease.
    a: Option<Server>,
    vol1: Vec<u8>,
}

impl Rig<'_> {
    fn sim(&self) -> Option<&Cloudsim> {
        match &self.cloud {
            CloudSide::Sim(s) => Some(s),
            CloudSide::Rclone(_) => None,
        }
    }

    /// The sha256 of the cloud file at `rel` (below the library folder).
    fn cloud_sha(&self, rel: &str) -> Result<Option<String>> {
        match &self.cloud {
            CloudSide::Sim(s) => s.file_sha256(self.p, ACCOUNT, &cloud_path(rel)),
            CloudSide::Rclone(r) => {
                let path = r.root.join(cloud_path(rel));
                Ok(std::fs::read(path).ok().map(|b| sha256_hex(&b)))
            }
        }
    }

    fn cloud_bytes(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        match &self.cloud {
            // cloudsim lists hashes, not bytes: read it back through the server.
            CloudSide::Sim(_) => {
                let a = self.server()?;
                let r = http::request(
                    a.addr,
                    "GET",
                    &dav_path(rel),
                    &[("Authorization", &Server::basic(READER))],
                    &[],
                )?;
                Ok((r.status == 200).then_some(r.body))
            }
            CloudSide::Rclone(r) => Ok(std::fs::read(r.root.join(cloud_path(rel))).ok()),
        }
    }

    fn login(&self) -> Result<CloudLogin> {
        match &self.cloud {
            CloudSide::Sim(s) => s.login(self.p, ACCOUNT),
            CloudSide::Rclone(r) => Ok(rclone_login(r)),
        }
    }

    fn start_server(&self, name: &str) -> Result<Server> {
        let mut env = vec![("MOKURO_TEST_CLOUD_TUNE".to_string(), tune(self.p))];
        if let Some(s) = self.sim() {
            env.extend(s.server_env());
        }
        Server::start_with(
            &self.paths.bin,
            &self.work.join(name),
            ServerOptions {
                backend: Backend::Cloud(self.login()?),
                readers: true,
                users: vec![
                    SeedUser {
                        username: READER.into(),
                        role: "registered".into(),
                    },
                    SeedUser {
                        username: PROCESSOR.into(),
                        role: "processor".into(),
                    },
                ],
                ocr_remote: true,
                env,
            },
        )
    }

    /// The server holding the library: A, then B after the handover.
    fn server(&self) -> Result<&Server> {
        self.a.as_ref().context("no server holds the library")
    }

    /// PUT `body` at `rel` through `server` as its admin: `(status, body text)`.
    fn put(server: &Server, rel: &str, body: &[u8]) -> Result<(u16, String)> {
        let r = http::request(
            server.addr,
            "PUT",
            &dav_path(rel),
            &[
                ("Authorization", &server.auth),
                ("Content-Type", "application/json"),
            ],
            body,
        )?;
        Ok((r.status, String::from_utf8_lossy(&r.body).into_owned()))
    }

    /// Wait until the cloud holds exactly `body` at `rel`.
    fn wait_cloud(&self, rel: &str, body: &[u8], timeout: Duration) -> Result<()> {
        let want = sha256_hex(body);
        wait_for(timeout, &format!("{rel} never reached the cloud"), || {
            Ok((self.cloud_sha(rel)?.as_deref() == Some(want.as_str())).then_some(()))
        })
    }

    /// A fault on the provider's upload requests (Drive, Graph); MEGA has no HTTP
    /// faults, its knob answers the next `cs` commands with an error code instead.
    /// Graph creates a small file in one PUT to `…:/content` but replaces one through
    /// an upload session: `replace` also faults the session's creation.
    fn upload_fault(&self, status: u16, reason: &str, times: u64, replace: bool) -> Result<()> {
        let sim = self.sim().context("no fault knobs on rclone")?;
        match self.p {
            // A create is a POST; a lease heartbeat replaces the marker with a PATCH,
            // so a create-only fault cannot be spent on one.
            Provider::Google => sim.fault(
                self.p,
                ACCOUNT,
                if replace {
                    json!({"path_has": "/upload/", "status": status, "reason": reason,
                           "times": times})
                } else {
                    json!({"method": "POST", "path_has": "/upload/", "status": status,
                           "reason": reason, "times": times})
                },
            ),
            Provider::Microsoft => {
                sim.fault(
                    self.p,
                    ACCOUNT,
                    json!({"method": "PUT", "path_has": ":/content", "status": status,
                           "reason": reason, "times": times}),
                )?;
                if replace {
                    sim.fault(
                        self.p,
                        ACCOUNT,
                        json!({"method": "POST", "path_has": "/createUploadSession",
                               "status": status, "reason": reason, "times": times}),
                    )?;
                }
                Ok(())
            }
            _ => bail!("no upload fault on {}", self.p.name()),
        }
    }
}

/// The upload faults ([`Rig::upload_fault`] at `status`, or MEGA's `eagain`) that fired
/// since the log was last cleared: a 503 on Drive and Graph, a `-3` MEGA answered in its
/// 200 body.
fn faults_fired(sim: &Cloudsim, p: Provider, status: u16) -> Result<usize> {
    Ok(sim
        .log()?
        .into_iter()
        .filter(|e| match p {
            Provider::Mega => e["error"] == -3,
            _ => e["status"] == status,
        })
        .count())
}

/// A server's sign-in to rclone's WebDAV, as a connected server keeps one.
pub fn rclone_login(r: &Rclone) -> CloudLogin {
    let url = r.url();
    CloudLogin {
        backend: "webdav",
        account_id: format!("{}@{url}", r.user),
        credential: json!({"webdav_password": r.password}),
        webdav: Some((url, r.user.clone())),
    }
}

type Step = fn(&mut Rig) -> Result<Option<String>>;

/// `(name, step)`: a step answers `Ok(None)` (passed), `Ok(Some(why))` (skipped) or
/// `Err` (failed). Order matters: the handover replaces server A with B, and the
/// quota hold stops uploads for minutes.
const SCENARIOS: [(&str, Step); 6] = [
    ("cold-start", cold_start),
    ("adoption", adoption),
    ("outbox-faults", outbox_faults),
    ("ocr-results", ocr_results),
    ("lease-handover", lease_handover),
    ("quota-pause", quota_pause),
];

pub fn run(paths: &Paths, p: Provider, work: &Path, grep: Option<&str>, report: &mut Report) {
    let selected: Vec<(&str, Step)> = SCENARIOS
        .into_iter()
        .filter(|(n, _)| grep.is_none_or(|g| n.contains(g)))
        .collect();
    let start = Instant::now();
    let mut rig = match setup(paths, p, work) {
        Ok(r) => r,
        Err(e) => {
            let detail = format!("setup: {e:#}");
            for (name, _) in selected {
                report.add(Scenario {
                    name: name.into(),
                    outcome: Outcome::Failed,
                    ms: start.elapsed().as_millis() as u64,
                    detail: Some(detail.clone()),
                    capability: None,
                });
            }
            return;
        }
    };
    for (name, step) in selected {
        let t = Instant::now();
        let mut r = step(&mut rig);
        // Whatever the step saw, a request cloudsim could not accept fails it.
        if let Some(sim) = rig.sim() {
            match sim.violations() {
                Ok(v) if v.is_empty() => {}
                Ok(v) => {
                    let _ = sim.clear_violations();
                    let msg = format!("cloudsim violations: {}", Value::Array(v));
                    r = match r {
                        Err(e) => Err(e.context(msg)),
                        _ => Err(anyhow::anyhow!(msg)),
                    };
                }
                Err(e) => r = Err(e.context("reading cloudsim's violations")),
            }
        }
        let (outcome, detail) = match r {
            Ok(None) => (Outcome::Passed, None),
            Ok(Some(why)) => (Outcome::Skipped, Some(why)),
            Err(e) => (Outcome::Failed, Some(format!("{e:#}"))),
        };
        report.add(Scenario {
            name: name.into(),
            outcome,
            ms: t.elapsed().as_millis() as u64,
            detail,
            capability: None,
        });
    }
}

/// The library in the cloud, then server A started on it and holding the lease.
fn setup<'a>(paths: &'a Paths, p: Provider, work: &Path) -> Result<Rig<'a>> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut vol1 = Vec::new();
    for (i, v) in VOLUMES.iter().enumerate() {
        let bytes = genlib::cbz_bytes(&format!("rig {v}"))?;
        if i == 0 {
            vol1 = bytes.clone();
            files.push((
                cloud_path(&format!("{SERIES}/{v}.mokuro")),
                genlib::mokuro_doc(SERIES, v, i),
            ));
        }
        files.push((cloud_path(&format!("{SERIES}/{v}.cbz")), bytes));
    }
    // A reader-written series.json: the first adoption applies its facts.
    files.push((
        cloud_path(&format!("{SERIES}/series.json")),
        serde_json::to_vec(&json!({
            "version": 2, "series_title": SERIES, "external_ids": {}, "titles": {},
            "synonyms": [], "tag": ADOPTED_TAG, "updated_at": "2026-01-02T03:04:05.000Z",
            "volumes": [],
        }))?,
    ));
    let cloud = if p == Provider::Rclone {
        let bin = rclone::binary(paths.rclone_bin.as_deref(), &paths.tools)?;
        // Bunko sends no Origin; the allowed one is for the preflight check, which the
        // browser suites rely on (the reader's own WebDAV client sends Authorization).
        let r = Rclone::start(
            &bin,
            &work.join("rclone-root"),
            &work.join("rclone"),
            Some(PREFLIGHT_ORIGIN),
        )?;
        r.preflight(PREFLIGHT_ORIGIN)?;
        r.seed(&files)?;
        CloudSide::Rclone(r)
    } else {
        let sim = Cloudsim::start(&paths.cloudsim_bin, &work.join("cloudsim"))?;
        sim.seed(p, ACCOUNT, &files)?;
        if p != Provider::Mega {
            // Only Bunko talks to this account: hold it to what Bunko's driver must send.
            sim.knobs(p, ACCOUNT, json!({"strict_bunko": true}))?;
        }
        CloudSide::Sim(sim)
    };
    let mut rig = Rig {
        p,
        paths,
        work: work.to_path_buf(),
        cloud,
        a: None,
        vol1,
    };
    let mut a = rig.start_server("server-a")?;
    a.wait_storage("held", Duration::from_secs(120))
        .context("server A never held the lease")?;
    rig.a = Some(a);
    Ok(rig)
}

/// The catalog lists the series from the cloud, and the bridge serves an archive,
/// whole and ranged, byte for byte.
fn cold_start(rig: &mut Rig) -> Result<Option<String>> {
    let a = rig.server()?;
    wait_for(
        Duration::from_secs(60),
        "the catalog never listed the series",
        || {
            let r = http::get(a.addr, "/catalog/api/library")?;
            let v = r.json()?;
            let row = v["series"]
                .as_array()
                .and_then(|rows| rows.iter().find(|s| s["name"] == SERIES).cloned());
            Ok(row.filter(|s| s["volume_count"] == VOLUMES.len()))
        },
    )?;
    let path = dav_path(&format!("{SERIES}/Vol 1.cbz"));
    let auth = Server::basic(READER);
    let whole = http::request(a.addr, "GET", &path, &[("Authorization", &auth)], &[])?;
    if whole.status != 200 || whole.body != rig.vol1 {
        bail!(
            "GET {path}: {} with {} bytes, want 200 with the archive's {}",
            whole.status,
            whole.body.len(),
            rig.vol1.len()
        );
    }
    let ranged = http::request(
        a.addr,
        "GET",
        &path,
        &[("Authorization", &auth), ("Range", "bytes=10-19")],
        &[],
    )?;
    if ranged.status != 206 || ranged.body != rig.vol1[10..20] {
        bail!("ranged GET {path}: {} {:?}", ranged.status, ranged.body);
    }
    Ok(None)
}

/// The first sync applied the reader's `series.json` facts, and the lease marker is
/// in the cloud folder.
fn adoption(rig: &mut Rig) -> Result<Option<String>> {
    let a = rig.server()?;
    wait_for(
        Duration::from_secs(60),
        "the adopted tag never showed in the catalog",
        || {
            let v = http::get(a.addr, "/catalog/api/library")?.json()?;
            Ok(v["series"]
                .as_array()
                .and_then(|rows| rows.iter().find(|s| s["name"] == SERIES))
                .filter(|s| s["tag"] == ADOPTED_TAG)
                .map(|_| ()))
        },
    )?;
    if rig.cloud_sha(".mokuro-bunko.json")?.is_none() {
        bail!("no lease marker (.mokuro-bunko.json) in the cloud folder");
    }
    Ok(None)
}

/// Upload requests fail twice with 503 (Drive: creates only; MEGA: the next two `cs`
/// batches answer `-3`); a PUT through the bridge still lands, and the simulator's log
/// shows both faults fired.
fn outbox_faults(rig: &mut Rig) -> Result<Option<String>> {
    let Some(sim) = rig.sim() else {
        return Ok(Some("rclone has no fault knobs".into()));
    };
    sim.clear_log()?;
    match rig.p {
        Provider::Mega => sim.knobs(rig.p, ACCOUNT, json!({"eagain": 2}))?,
        _ => rig.upload_fault(503, "backendError", 2, false)?,
    }
    let rel = format!("{SERIES}/Vol 2.mokuro");
    let body = genlib::mokuro_doc(SERIES, "Vol 2", 1);
    let (status, text) = Rig::put(rig.server()?, &rel, &body)?;
    if !(200..300).contains(&status) {
        bail!("PUT {rel} answered {status}: {text}");
    }
    rig.wait_cloud(&rel, &body, Duration::from_secs(60))?;
    let fired = faults_fired(sim, rig.p, 503)?;
    if fired < 2 {
        bail!("the faults never fired ({fired} of 2 in cloudsim's log)");
    }
    Ok(None)
}

/// The provider says it is full: a bridge PUT answers 507 at once, and the store
/// holds every upload after it (507 again, without asking the provider) until a probe
/// write finds room, the first one five minutes later. Last, so the hold delays none
/// of the other scenarios.
fn quota_pause(rig: &mut Rig) -> Result<Option<String>> {
    let Some(sim) = rig.sim() else {
        return Ok(Some("rclone has no quota knob".into()));
    };
    match rig.p {
        // Vol 3 may hold the fake processor's result by now: a create or a replace.
        Provider::Google => rig.upload_fault(403, "storageQuotaExceeded", 1, true)?,
        Provider::Microsoft => rig.upload_fault(507, "quotaLimitReached", 1, true)?,
        // EGOINGOVERQUOTA on the node put that ends an upload.
        _ => sim.knobs(
            rig.p,
            ACCOUNT,
            json!({"fail_command": {"command": "p", "code": -24}}),
        )?,
    }
    sim.clear_log()?;
    let rel = format!("{SERIES}/Vol 3.mokuro");
    let body = genlib::mokuro_doc(SERIES, "Vol 3", 2);
    let server = rig.server()?;
    for attempt in ["on a full provider", "while held"] {
        let (status, text) = Rig::put(server, &rel, &body)?;
        if status != 507 {
            bail!("PUT {rel} {attempt} answered {status}, want 507: {text}");
        }
    }
    // The held PUT never reached the provider: one upload request, the refused one.
    let uploads = sim
        .log()?
        .into_iter()
        .filter(|e| {
            let path = e["path"].as_str().unwrap_or_default();
            match rig.p {
                Provider::Google => path.contains("/upload/"),
                Provider::Microsoft => {
                    (e["method"] == "PUT" && path.contains(":/content"))
                        || path.contains("/createUploadSession")
                }
                _ => false,
            }
        })
        .count();
    if rig.p != Provider::Mega && uploads != 1 {
        bail!("{uploads} upload requests reached the provider, want 1 (the refused one)");
    }
    let health = &server.storage()?["health"];
    if health["state"] != "blocked" || health["reason"] != "StorageFull" {
        bail!("the store's health is {health}, want blocked by StorageFull");
    }
    // (The fake processor's result for Vol 3 may be there already.)
    if rig.cloud_sha(&rel)? == Some(sha256_hex(&body)) {
        bail!("the PUT of {rel} reached the cloud although the provider was full");
    }
    Ok(None)
}

/// The fake processor OCRs the volume without a sidecar; its result leaves through the
/// outbox past two faults (as [`outbox_faults`]; the log shows both fired) and lands in
/// the cloud. The bytes are read back through the server, checked against the cloud's
/// own hash.
fn ocr_results(rig: &mut Rig) -> Result<Option<String>> {
    if let Some(sim) = rig.sim() {
        sim.clear_log()?;
    }
    match rig.p {
        Provider::Google | Provider::Microsoft => {
            rig.upload_fault(503, "backendError", 2, false)?
        }
        Provider::Mega => {
            rig.sim()
                .context("cloudsim")?
                .knobs(rig.p, ACCOUNT, json!({"eagain": 2}))?
        }
        _ => {}
    }
    let a = rig.server()?;
    let mut fake = FakeProcessor::start(
        &rig.paths.bin,
        &rig.work.join("processor"),
        &a.base_url(),
        PROCESSOR,
        &FakeOptions::default(),
    )?;
    let rel = format!("{SERIES}/Vol 4.mokuro");
    let bytes = wait_for(
        Duration::from_secs(180),
        &format!("{rel} never reached the cloud"),
        || {
            fake.check_alive()?;
            let Some(sha) = rig.cloud_sha(&rel)? else {
                return Ok(None);
            };
            // cloudsim lists hashes, not bytes: the server's copy stands for the
            // cloud's only when they hash the same.
            Ok(rig.cloud_bytes(&rel)?.filter(|b| sha256_hex(b) == sha))
        },
    )?;
    if let Some(sim) = rig.sim() {
        let fired = faults_fired(sim, rig.p, 503)?;
        if fired < 2 {
            bail!("the faults never fired ({fired} of 2 in cloudsim's log)");
        }
    }
    let v: Value = serde_json::from_slice(&bytes).context("the OCR result is not JSON")?;
    let pages = v["pages"].as_array().map_or(0, Vec::len);
    if pages != genlib::pages_per_volume() || v["ocr_engine"].is_null() {
        bail!(
            "the OCR result has {pages} pages and ocr_engine {}",
            v["ocr_engine"]
        );
    }
    Ok(None)
}

/// Server B on the same cloud folder stands by (its writes answer 503 naming the
/// lease); once A stops, its marker goes stale and B takes the library over.
fn lease_handover(rig: &mut Rig) -> Result<Option<String>> {
    let mut b = rig.start_server("server-b")?;
    let st = b
        .wait_storage("standby", Duration::from_secs(90))
        .context("server B never stood by")?;
    if st["lease"]["holder"]["ours"] != false {
        bail!(
            "server B in standby names itself the holder: {}",
            st["lease"]
        );
    }
    let rel = format!("{SERIES}/Vol 1.mokuro");
    let body = genlib::mokuro_doc(SERIES, "Vol 1", 7);
    let (status, text) = Rig::put(&b, &rel, &body)?;
    if status != 503 || !text.contains("storage-lease") {
        bail!("PUT through the standby server answered {status}, want 503 storage-lease: {text}");
    }
    let mut a = rig.a.take().context("server A is not running")?;
    // A clean stop writes nothing: B waits out the marker's ttl.
    a.stop();
    b.wait_storage("held", Duration::from_secs(120))
        .context("server B never took the library over")?;
    let (status, text) = Rig::put(&b, &rel, &body)?;
    if !(200..300).contains(&status) {
        bail!("PUT through the new holder answered {status}: {text}");
    }
    rig.wait_cloud(&rel, &body, Duration::from_secs(60))?;
    // B is the library's server from here on.
    rig.a = Some(b);
    Ok(None)
}
