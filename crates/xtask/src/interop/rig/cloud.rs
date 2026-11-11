//! The `cloud` suite: Bunko and one cloud (cloudsim, rclone or a library folder on this
//! disk), no reader, every party its own process. One library, one server holding its
//! lease, then in order: cold start, adoption, the outbox under faults, OCR results
//! leaving through the outbox, a reader writing and renaming in a folder library, the
//! lease handed to a second server, a provider that is full, and a folder library
//! taken away and brought back.

use super::super::genlib;
use super::super::http;
use super::super::server::{Backend, CloudLogin, SeedUser, Server, ServerOptions};
use super::cloudsim::Cloudsim;
use super::fakeproc::{FakeOptions, FakeProcessor};
use super::folder::Folder;
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
    Folder(Folder),
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
            CloudSide::Rclone(_) | CloudSide::Folder(_) => None,
        }
    }

    fn folder(&self) -> Option<&Folder> {
        match &self.cloud {
            CloudSide::Folder(f) => Some(f),
            CloudSide::Sim(_) | CloudSide::Rclone(_) => None,
        }
    }

    /// Why a scenario needs knobs this cloud has none of.
    fn no_knobs(&self, what: &str) -> String {
        match &self.cloud {
            CloudSide::Folder(_) => format!("a folder has no {what}"),
            _ => format!("{} has no {what}", self.p.name()),
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
            CloudSide::Folder(f) => Ok(f.bytes(rel).map(|b| sha256_hex(&b))),
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
            CloudSide::Folder(f) => Ok(f.bytes(rel)),
        }
    }

    fn login(&self) -> Result<CloudLogin> {
        match &self.cloud {
            CloudSide::Sim(s) => s.login(self.p, ACCOUNT),
            CloudSide::Rclone(r) => Ok(rclone_login(r)),
            CloudSide::Folder(f) => Ok(f.login()),
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
        path: None,
    }
}

type Step = fn(&mut Rig) -> Result<Option<String>>;

/// `(name, step)`: a step answers `Ok(None)` (passed), `Ok(Some(why))` (skipped) or
/// `Err` (failed). Order matters: the handover replaces server A with B, the quota
/// hold stops uploads for minutes, and `unplug` takes the whole library away.
const SCENARIOS: [(&str, Step); 9] = [
    ("cold-start", cold_start),
    ("adoption", adoption),
    ("outbox-faults", outbox_faults),
    ("ocr-results", ocr_results),
    ("reader-writes", reader_writes),
    ("reader-rename", reader_rename),
    ("lease-handover", lease_handover),
    ("quota-pause", quota_pause),
    ("unplug", unplug),
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
                    notes: Vec::new(),
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
            match sim.take_violations() {
                Ok(v) if v.is_empty() => {}
                Ok(v) => {
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
            notes: Vec::new(),
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
    let cloud = if p == Provider::Folder {
        let f = Folder::create(work)?;
        f.seed(&files)?;
        // The reader-rename scenario's own series: a volume with its OCR.
        f.seed(&[
            (
                format!("{RENAME_FROM}/Vol 1.cbz"),
                genlib::cbz_bytes("rig rename Vol 1")?,
            ),
            (
                format!("{RENAME_FROM}/Vol 1.mokuro"),
                genlib::mokuro_doc(RENAME_FROM, "Vol 1", RENAME_DOC),
            ),
        ])?;
        CloudSide::Folder(f)
    } else if p == Provider::Rclone {
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
        return Ok(Some(rig.no_knobs("fault knobs")));
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
        return Ok(Some(rig.no_knobs("quota knob")));
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

/// The reader-rename scenario's series, seeded in a folder library with its OCR, and
/// the name the reader renames it to.
const RENAME_FROM: &str = "Rig Rename";
const RENAME_TO: &str = "Rig Renamed";
/// Its sidecar's volume id number: no other rig volume's.
const RENAME_DOC: usize = 71;
/// The reader-writes scenario's series, written by the "reader" in a folder library.
const WRITES_SERIES: &str = "Reader Writes";
/// How long a write in progress is watched: past a few of Bunko's polls (2 s each)
/// and its watcher's quiet wait (2 s).
const MID_WRITE: Duration = Duration::from_secs(6);
/// How soon a finished reader write must show (plan: within 10 s).
const PICKUP: Duration = Duration::from_secs(10);

/// The catalog's volume count for `series` (0 when it lists no such series).
fn volume_count(server: &Server, series: &str) -> Result<u64> {
    let v = http::get(server.addr, "/catalog/api/library")?.json()?;
    Ok(v["series"]
        .as_array()
        .and_then(|rows| rows.iter().find(|s| s["name"] == series))
        .and_then(|s| s["volume_count"].as_u64())
        .unwrap_or(0))
}

/// Every series the catalog lists, `(name, volume count)`, sorted.
fn catalog_rows(server: &Server) -> Result<Vec<(String, u64)>> {
    let v = http::get(server.addr, "/catalog/api/library")?.json()?;
    let mut rows: Vec<(String, u64)> = v["series"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|s| {
                    (
                        s["name"].as_str().unwrap_or_default().to_string(),
                        s["volume_count"].as_u64().unwrap_or(0),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    rows.sort();
    Ok(rows)
}

/// What the bridge serves the reader account at `rel`: the bytes, or `None` when it
/// answers anything but 200.
fn served(server: &Server, rel: &str) -> Result<Option<Vec<u8>>> {
    let r = http::request(
        server.addr,
        "GET",
        &dav_path(rel),
        &[("Authorization", &Server::basic(READER))],
        &[],
    )?;
    Ok((r.status == 200).then_some(r.body))
}

/// Bunko's OCR queue as its admin sees it (`/queue/api/status`).
fn queue_status(server: &Server) -> Result<Value> {
    let r = http::request(
        server.addr,
        "GET",
        "/queue/api/status",
        &[("Authorization", &server.auth)],
        &[],
    )?;
    if r.status != 200 {
        bail!("the queue status answered {}", r.status);
    }
    r.json()
}

/// The volumes of `series` the queue runs, waits on or has failed.
fn jobs_of(status: &Value, series: &str) -> Vec<String> {
    let mut jobs: Vec<&Value> = Vec::new();
    for list in [&status["pending"], &status["failed"]] {
        jobs.extend(list.as_array().into_iter().flatten());
    }
    for m in status["machines"].as_array().into_iter().flatten() {
        for list in [&m["jobs"], &m["next"]] {
            jobs.extend(list.as_array().into_iter().flatten());
        }
    }
    jobs.into_iter()
        .filter(|j| j["series"] == series)
        .map(|j| j["volume"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Wait until Bunko has finished `n` library walks that started after this call (a
/// testing build counts them in `/queue/api/status`'s `walks`).
fn walks_after(server: &Server, n: u64) -> Result<()> {
    let start = queue_status(server)?["walks"]
        .as_u64()
        .context("/queue/api/status names no walks: the rig needs a testing build")?;
    wait_for(
        Duration::from_secs(60),
        &format!("Bunko never walked the library {n} more times"),
        || {
            let walks = queue_status(server)?["walks"].as_u64().unwrap_or(0);
            Ok((walks > start + n).then_some(()))
        },
    )
}

/// A reader writes into a folder library the way Chromium's File System Access does
/// (an empty or old file, the new bytes in `<name>.crswap` until the write closes).
/// A new volume shows only after the swap, and within 10 s of it; a replaced sidecar
/// is served with its old bytes until the swap, and with the new ones within 10 s.
fn reader_writes(rig: &mut Rig) -> Result<Option<String>> {
    let Some(folder) = rig.folder() else {
        return Ok(Some(
            "a reader writes to the library's disk only on a folder".into(),
        ));
    };
    let a = rig.server()?;
    let cbz_rel = format!("{WRITES_SERIES}/Vol 1.cbz");
    let cbz = genlib::cbz_bytes("rig reader-writes Vol 1")?;
    folder.swap_write(&cbz_rel, &cbz, || {
        let until = Instant::now() + MID_WRITE;
        while Instant::now() < until {
            let n = volume_count(a, WRITES_SERIES)?;
            if n != 0 {
                bail!("{cbz_rel} was listed ({n} volumes) while the reader was still writing it");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Ok(())
    })?;
    wait_for(
        PICKUP,
        &format!("{cbz_rel} never showed after the reader's write closed"),
        || Ok((volume_count(a, WRITES_SERIES)? == 1).then_some(())),
    )?;
    let got = served(a, &cbz_rel)?;
    if got.as_deref() != Some(&cbz[..]) {
        bail!(
            "the bridge serves {cbz_rel} as {:?} bytes, want the reader's {}",
            got.map(|b| b.len()),
            cbz.len()
        );
    }

    // A sidecar the reader writes, then replaces.
    let doc_rel = format!("{WRITES_SERIES}/Vol 1.mokuro");
    let first = genlib::mokuro_doc(WRITES_SERIES, "Vol 1", 72);
    folder.swap_write(&doc_rel, &first, || Ok(()))?;
    wait_for(
        PICKUP,
        &format!("{doc_rel} was never served after the reader wrote it"),
        || Ok((served(a, &doc_rel)?.as_deref() == Some(&first[..])).then_some(())),
    )?;
    let mut edited: Value = serde_json::from_slice(&first)?;
    edited["rig_revision"] = json!(2);
    let second = serde_json::to_vec(&edited)?;
    folder.swap_write(&doc_rel, &second, || {
        let until = Instant::now() + MID_WRITE;
        while Instant::now() < until {
            let now = served(a, &doc_rel)?;
            if now.as_deref() != Some(&first[..]) {
                bail!(
                    "while the reader replaced {doc_rel}, the bridge served {:?} bytes, \
                     not the old {}",
                    now.map(|b| b.len()),
                    first.len()
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Ok(())
    })?;
    wait_for(
        PICKUP,
        &format!("{doc_rel} was not served with the reader's new bytes"),
        || Ok((served(a, &doc_rel)?.as_deref() == Some(&second[..])).then_some(())),
    )?;
    Ok(None)
}

/// The reader renames a series in a folder library as its Local Folder does: per file,
/// a copy (a swap write) then a delete of the source, the archive before its sidecar.
/// The catalog moves the volume, and two walks later nothing is queued for it (its
/// OCR came with it), and no numbered sidecar was made.
fn reader_rename(rig: &mut Rig) -> Result<Option<String>> {
    let Some(folder) = rig.folder() else {
        return Ok(Some(
            "a reader renames on the library's disk only on a folder".into(),
        ));
    };
    let a = rig.server()?;
    wait_for(
        Duration::from_secs(60),
        &format!("the catalog never listed {RENAME_FROM}"),
        || Ok((volume_count(a, RENAME_FROM)? == 1).then_some(())),
    )?;
    let sidecar = folder
        .bytes(&format!("{RENAME_FROM}/Vol 1.mokuro"))
        .context("the seeded sidecar is gone")?;
    for name in ["Vol 1.cbz", "Vol 1.mokuro"] {
        let from = format!("{RENAME_FROM}/{name}");
        let bytes = folder
            .bytes(&from)
            .with_context(|| format!("{from} is gone"))?;
        folder.swap_write(&format!("{RENAME_TO}/{name}"), &bytes, || Ok(()))?;
        std::fs::remove_file(folder.path(&from))?;
    }
    // The reader prunes the emptied folder.
    std::fs::remove_dir(folder.path(RENAME_FROM))?;
    wait_for(
        Duration::from_secs(30),
        &format!("the catalog never moved the volume to {RENAME_TO}"),
        || {
            Ok(
                (volume_count(a, RENAME_TO)? == 1 && volume_count(a, RENAME_FROM)? == 0)
                    .then_some(()),
            )
        },
    )?;
    walks_after(a, 2)?;
    let status = queue_status(a)?;
    for series in [RENAME_TO, RENAME_FROM] {
        let jobs = jobs_of(&status, series);
        if !jobs.is_empty() {
            bail!("after the rename the queue holds {jobs:?} of {series}: its OCR came along");
        }
    }
    let moved = folder.volume_files()?;
    let want = [
        format!("{RENAME_TO}/Vol 1.cbz"),
        format!("{RENAME_TO}/Vol 1.mokuro"),
    ];
    let of_series: Vec<&String> = moved
        .iter()
        .filter(|r| {
            r.starts_with(&format!("{RENAME_TO}/")) || r.starts_with(&format!("{RENAME_FROM}/"))
        })
        .collect();
    if of_series != want.iter().collect::<Vec<_>>() {
        bail!("after the rename the folder holds {of_series:?}, want {want:?}");
    }
    if folder.bytes(&want[1]).as_deref() != Some(&sidecar[..]) {
        bail!("{} is not the sidecar the reader moved", want[1]);
    }
    Ok(None)
}

/// The library folder goes away (its parent renamed, as an unplugged drive or a share
/// gone): the store goes offline and removes nothing. Back again: the lease is held,
/// the catalog and the library's volumes are as they were.
fn unplug(rig: &mut Rig) -> Result<Option<String>> {
    let Some(folder) = rig.folder() else {
        return Ok(Some("only a folder library can be unplugged".into()));
    };
    let a = rig.server()?;
    let parent = folder
        .root
        .parent()
        .context("the folder's parent")?
        .to_path_buf();
    let away = parent.with_file_name("folder-away");
    let rows = catalog_rows(a)?;
    let files = folder.volume_files()?;
    std::fs::rename(&parent, &away)?;
    let offline = wait_for(
        Duration::from_secs(60),
        "the store never went offline without its folder",
        || {
            let st = a.storage()?;
            Ok((st["health"]["state"] == "offline" || st["engine"] == "offline").then_some(()))
        },
    );
    let mid = offline.and_then(|()| {
        // A few more polls of an absent folder remove nothing.
        std::thread::sleep(Duration::from_secs(5));
        let now = catalog_rows(a)?;
        if now != rows {
            bail!("with the folder away the catalog changed: {now:?}, was {rows:?}");
        }
        Ok(())
    });
    // Always put it back, so a failure leaves the work folder whole for a look.
    std::fs::rename(&away, &parent)?;
    mid?;
    wait_for(
        Duration::from_secs(120),
        "the store never came back online holding the lease",
        || {
            let st = a.storage()?;
            Ok((st["health"]["state"] == "online" && st["lease"]["state"] == "held").then_some(()))
        },
    )?;
    walks_after(a, 1)?;
    let now = catalog_rows(a)?;
    if now != rows {
        bail!("after the folder came back the catalog is {now:?}, was {rows:?}");
    }
    let back = folder.volume_files()?;
    if back != files {
        bail!("after the folder came back it holds {back:?}, was {files:?}");
    }
    Ok(None)
}
