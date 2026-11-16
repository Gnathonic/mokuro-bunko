//! `cargo xtask-live interop live --provider <p>`: one live run (I5 Tasks 2, 3 and 6).
//!
//! 1. The credential from the environment (`BUNKO_LIVE_*`), refreshed once; a rotated
//!    OneDrive token is stored back at once.
//! 2. The run's sandbox `bunko-interop-<run id>` made at the account's top level; a
//!    crashed run's sandbox older than 48 hours removed; then the snapshot (the top
//!    level and the direct children of `mokuro-reader/`, metadata only).
//! 3. The fixture library uploaded into the sandbox and the `cloud` suite run on it,
//!    Bunko's library folder being the sandbox ([`super::super::rig::run_live`]).
//! 4. Whatever happened: the newest OneDrive token stored, every credential file of
//!    the run's servers removed, the sandbox deleted, the account snapshotted again
//!    and compared. A difference fails the run.
//!
//! Every path that writes to the account goes through [`Sandbox`], which takes paths
//! inside the sandbox only. With `--sim-base`, every provider address and token
//! endpoint is cloudsim's, and the testing server gets the same addresses: the same
//! code path, end to end, with no real account.

use super::auth::{GOOGLE_SECRET_ENV, google_secret, mega_verify, refresh_and_verify};
use super::snapshot::{
    Change, Entry, READER_FOLDER, Redactor, RunOutcome, SANDBOX_PREFIX, STALE_AFTER_MS, Snapshot,
    check_sandbox_name, excerpt, mask_lines, run_id, sandbox_name, sandbox_segments, with_previous,
};
use super::{LiveArgs, LiveProvider, Secret, SecretWriter};
use crate::interop::rig::{self, LiveCloud, Outcome, Paths, Report};
use crate::interop::server::CloudLogin;
use crate::util;
use anyhow::{Context, Result, anyhow, bail};
use bunko_remote::auth::clients::{BETA_DESKTOP_CLIENT_ID, MICROSOFT_CLIENT_ID};
use bunko_remote::auth::{OAuthClient, refresh};
use bunko_remote::backend::{BErr, Backend, OpCtx, RNode, UploadSource, UploadTarget, read_to_vec};
use bunko_remote::gdrive::{GDriveBackend, GDriveConfig};
use bunko_remote::mega::{MegaBackend, MegaConfig, SessionBlob};
use bunko_remote::onedrive::{OneDriveBackend, OneDriveConfig};
use bunko_remote::token_dyn::TokenSourceDyn;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// Where every provider lives: the real services, or one cloudsim.
#[derive(Debug, Clone)]
pub struct Bases {
    pub drive_api: String,
    pub graph_api: String,
    pub mega_origin: String,
    pub google_token: String,
    pub microsoft_token: String,
    /// cloudsim's base, when the bases are its.
    pub sim: Option<String>,
}

impl Bases {
    pub fn real() -> Bases {
        Bases {
            drive_api: bunko_remote::gdrive::DEFAULT_API_BASE.into(),
            graph_api: bunko_remote::onedrive::DEFAULT_API_BASE.into(),
            mega_origin: bunko_mega::DEFAULT_API_ORIGIN.into(),
            google_token: bunko_remote::auth::oauth::GOOGLE_TOKEN_URL.into(),
            microsoft_token: bunko_remote::auth::oauth::MICROSOFT_TOKEN_URL.into(),
            sim: None,
        }
    }

    /// cloudsim at `base` (its prefixes, as `rig::cloudsim::Cloudsim::server_env`).
    pub fn sim(base: &str) -> Bases {
        let b = base.trim_end_matches('/');
        Bases {
            drive_api: format!("{b}/google"),
            graph_api: format!("{b}/graph/v1.0"),
            mega_origin: format!("{b}/mega/"),
            google_token: format!("{b}/oauth/google/token"),
            microsoft_token: format!("{b}/oauth/microsoft/token"),
            sim: Some(b.to_string()),
        }
    }

    /// The testing server's addresses for cloudsim; nothing for the real services.
    pub fn server_env(&self) -> Vec<(String, String)> {
        if self.sim.is_none() {
            return Vec::new();
        }
        [
            ("MOKURO_TEST_CLOUD_API_BASE_GDRIVE", &self.drive_api),
            ("MOKURO_TEST_CLOUD_API_BASE_ONEDRIVE", &self.graph_api),
            ("MOKURO_TEST_CLOUD_API_BASE_MEGA", &self.mega_origin),
            ("MOKURO_TEST_CLOUD_TOKEN_URL_GOOGLE", &self.google_token),
            (
                "MOKURO_TEST_CLOUD_TOKEN_URL_MICROSOFT",
                &self.microsoft_token,
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
    }
}

fn in_actions() -> bool {
    std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true")
}

/// Every secret this run has seen (the redactor of everything it writes).
static REDACTOR: OnceLock<Redactor> = OnceLock::new();

fn redactor() -> &'static Redactor {
    REDACTOR.get_or_init(Redactor::default)
}

/// Print `::add-mask::` for `forms` in Actions (nothing elsewhere).
fn print_masks(forms: &[String]) {
    let v: Vec<&str> = forms.iter().map(String::as_str).collect();
    let lines = mask_lines(&v, in_actions());
    if !lines.is_empty() {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(lines.as_bytes());
        let _ = out.flush();
    }
}

/// Remember `values` for redaction and mask them in the rest of an Actions log, before
/// anything could show one.
fn mask(values: &[&Secret]) {
    let mut new = Vec::new();
    for v in values {
        new.extend(redactor().add(v.expose()));
    }
    print_masks(&new);
}

/// A MEGA session and its parts (the session id travels on its own).
fn mask_mega(blob: &Secret) {
    print_masks(&redactor().add_mega_session(blob.expose()));
}

/// One line on stderr, redacted.
fn say(text: impl AsRef<str>) {
    eprintln!("{}", redactor().redact(text.as_ref()));
}

/// The bound on a reason in the result file and on stderr.
const REASON_MAX: usize = 600;
/// The bound on a scenario's detail in the result file.
const DETAIL_MAX: usize = 400;

/// The result file's document: every string redacted, the reason and each scenario's
/// detail cut to one bounded line (no log tail, no response body).
fn summary_doc(red: &Redactor, summary: &Summary) -> Result<Value> {
    let mut v = serde_json::to_value(summary)?;
    if let Some(r) = summary.reason.as_deref() {
        v["reason"] = excerpt(&red.redact(r), REASON_MAX).into();
    }
    if let Some(list) = v["scenarios"].as_array_mut() {
        for s in list {
            if let Some(d) = s["detail"].as_str() {
                s["detail"] = excerpt(&red.redact(d), DETAIL_MAX).into();
            }
            if let Some(notes) = s.get_mut("notes").and_then(Value::as_array_mut) {
                for n in notes {
                    if let Some(t) = n.as_str() {
                        *n = excerpt(&red.redact(t), DETAIL_MAX).into();
                    }
                }
            }
        }
    }
    Ok(red.redact_json(&v))
}

/// The scenario lines on stderr: one bounded, redacted line each.
fn scenario_lines(red: &Redactor, scenarios: &[rig::Scenario]) -> Vec<String> {
    scenarios
        .iter()
        .map(|s| {
            red.redact(&format!(
                "  {:<16} {:<8} {:>6} ms{}",
                s.name,
                format!("{:?}", s.outcome).to_lowercase(),
                s.ms,
                s.detail
                    .as_deref()
                    .map(|d| format!("  {}", excerpt(&red.redact(d), DETAIL_MAX)))
                    .unwrap_or_default()
            ))
        })
        .collect()
}

/// An error as the run reports it: redacted, bounded.
fn redacted_error(red: &Redactor, e: &anyhow::Error) -> anyhow::Error {
    let text = red.redact(&format!("{e:#}"));
    anyhow!("{}", text.chars().take(2000).collect::<String>())
}

fn env_secret(name: &str, p: LiveProvider) -> Result<Secret> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Ok(Secret::new(v.trim())),
        _ => bail!(
            "{name} is not set: the owner stores it with `cargo xtask-live interop live-auth {}`",
            p.target().name()
        ),
    }
}

/// The newest refresh token, and when it was obtained.
#[derive(Clone)]
struct Newest {
    token: Secret,
    at: SystemTime,
}

/// An access-token source for the runner's own calls: refreshes when the token is old
/// or was refused, and keeps the newest refresh token (Microsoft rotates it).
struct LiveToken {
    client: OAuthClient,
    access: Mutex<(Secret, Instant)>,
    newest: Arc<Mutex<Newest>>,
}

impl LiveToken {
    const MAX_AGE: Duration = Duration::from_secs(45 * 60);
}

impl TokenSourceDyn for LiveToken {
    fn access_token(&self) -> Pin<Box<dyn Future<Output = Result<String, BErr>> + Send + '_>> {
        Box::pin(async move {
            {
                let a = self.access.lock().expect("token lock");
                if a.1.elapsed() < Self::MAX_AGE {
                    return Ok(a.0.expose().to_string());
                }
            }
            let rt = self.newest.lock().expect("token lock").token.clone();
            let t = refresh(&self.client, rt.expose())
                .await
                .map_err(BErr::from)?;
            let access = Secret::new(t.access_token.clone());
            mask(&[&access]);
            if let Some(new) = t.refresh_token.filter(|n| n != rt.expose()) {
                let new = Secret::new(new);
                mask(&[&new]);
                *self.newest.lock().expect("token lock") = Newest {
                    token: new,
                    at: SystemTime::now(),
                };
            }
            *self.access.lock().expect("token lock") = (access.clone(), Instant::now());
            Ok(access.expose().to_string())
        })
    }

    fn invalidate(&self) {
        let mut a = self.access.lock().expect("token lock");
        a.1 = Instant::now() - Self::MAX_AGE * 2;
    }
}

/// One provider driver, Bunko's own.
enum Cloud {
    Drive(GDriveBackend),
    OneDrive(OneDriveBackend),
    Mega(MegaBackend),
}

macro_rules! on {
    ($cloud:expr, $b:ident => $e:expr) => {
        match $cloud {
            Cloud::Drive($b) => $e,
            Cloud::OneDrive($b) => $e,
            Cloud::Mega($b) => $e,
        }
    };
}

impl Cloud {
    async fn list_dir(&self, dir: &str) -> Result<Vec<RNode>, BErr> {
        let mut v = Vec::new();
        on!(self, b => b.list_dir(dir, &mut |n| v.push(n)).await)?;
        Ok(v)
    }

    async fn child(&self, parent: &str, name: &str) -> Result<Option<RNode>, BErr> {
        let (found, _) = on!(self, b => b.find_named(parent, name).await)?;
        // The newest of same-named nodes: a MEGA replacement is a new node beside
        // the old one until the old is removed.
        Ok(found
            .into_iter()
            .filter(|n| !n.trashed)
            .max_by_key(|n| (n.created_ms, n.base_mtime_ms)))
    }
}

fn name_of(n: &RNode) -> String {
    String::from_utf8_lossy(&n.name_raw).into_owned()
}

fn entry(path: String, n: &RNode) -> Entry {
    Entry {
        path,
        is_dir: n.is_dir,
        size: n.size,
        version: n.version.clone(),
        cver: n.cver.clone(),
        hash: n.hash.as_ref().map(|(_, h)| h.clone()),
    }
}

fn sha256_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

/// `rel → (pid, version, sha, checked at)`.
type Seen = HashMap<String, (String, String, Option<String>, Instant)>;

/// The run's sandbox in the account: the only place the runner writes.
pub struct Sandbox {
    rt: tokio::runtime::Runtime,
    cloud: Cloud,
    p: LiveProvider,
    name: String,
    root: String,
    /// The account's top level (the sandbox's parent).
    top: String,
    bases: Bases,
    /// Bunko's credential, without the refresh token (filled from `newest`).
    login: CloudLogin,
    newest: Option<Arc<Mutex<Newest>>>,
    /// `rel → (pid, version, sha)`: a file's hash is downloaded once per version.
    seen: Mutex<Seen>,
    downloaded: AtomicU64,
    state_dir: PathBuf,
}

impl Sandbox {
    /// Make the sandbox folder at the account's top level.
    fn open(
        rt: tokio::runtime::Runtime,
        c: Connected,
        p: LiveProvider,
        name: &str,
        bases: &Bases,
        state_dir: &Path,
    ) -> Result<Sandbox> {
        check_sandbox_name(name)?;
        let root = rt
            .block_on(async { on!(&c.cloud, b => b.create_root(name).await) })
            .map_err(|e| anyhow!("making the sandbox {name}: {e}"))?;
        let top = root
            .parent
            .clone()
            .context("the sandbox has no parent folder")?;
        Ok(Sandbox {
            rt,
            cloud: c.cloud,
            p,
            name: name.to_string(),
            root: root.pid,
            top,
            bases: bases.clone(),
            login: c.login,
            newest: c.newest,
            seen: Mutex::new(HashMap::new()),
            downloaded: AtomicU64::new(0),
            state_dir: state_dir.to_path_buf(),
        })
    }

    fn block<T>(&self, f: impl Future<Output = T>) -> T {
        self.rt.block_on(f)
    }

    /// The node at `rel` below the sandbox.
    async fn resolve(&self, rel: &str) -> Result<Option<RNode>> {
        let mut at = self.root.clone();
        let segs = sandbox_segments(rel)?;
        let mut node = None;
        for seg in segs {
            match self
                .cloud
                .child(&at, seg)
                .await
                .map_err(|e| anyhow!("{rel}: {e}"))?
            {
                Some(n) => {
                    at = n.pid.clone();
                    node = Some(n);
                }
                None => return Ok(None),
            }
        }
        Ok(node)
    }

    async fn ensure_dirs(&self, segs: &[&str]) -> Result<String> {
        let mut at = self.root.clone();
        for seg in segs {
            at = match self.cloud.child(&at, seg).await? {
                Some(n) if n.is_dir => n.pid,
                Some(_) => bail!("{seg} is a file, not a folder"),
                None => {
                    on!(&self.cloud, b => b.mkdir(&at, seg, &OpCtx::default()).await)
                        .map_err(|e| anyhow!("making {seg}: {e}"))?
                        .pid
                }
            };
        }
        Ok(at)
    }

    async fn put(&self, rel: &str, bytes: &[u8]) -> Result<()> {
        let segs = sandbox_segments(rel)?;
        let (name, dirs) = segs.split_last().context("an empty path")?;
        let parent = self.ensure_dirs(dirs).await?;
        let existing = self.cloud.child(&parent, name).await?;
        let tmp = self
            .state_dir
            .join(format!("upload-{}", sha256_hex(rel.as_bytes())));
        std::fs::write(&tmp, bytes)?;
        let sha: [u8; 32] = Sha256::digest(bytes).into();
        let src = UploadSource::File {
            path: &tmp,
            size: bytes.len() as u64,
            sha256: sha,
        };
        let to = match &existing {
            Some(n) => UploadTarget::Replace {
                pid: &n.pid,
                if_cver: None,
            },
            None => UploadTarget::New {
                parent: &parent,
                name,
            },
        };
        let mut op = OpCtx::default();
        let r = on!(&self.cloud, b => b.upload(to, src, &mut op).await);
        let _ = std::fs::remove_file(&tmp);
        r.map_err(|e| anyhow!("uploading {rel}: {e}"))?;
        Ok(())
    }

    /// The top level and the direct children of every top-level `mokuro-reader`
    /// folder, the sandbox left out. Names and versions only: no content is read.
    async fn snapshot(&self) -> Result<Snapshot> {
        let mut s = Snapshot::default();
        for n in self.cloud.list_dir(&self.top).await? {
            if n.pid == self.root {
                continue;
            }
            let name = name_of(&n);
            if n.is_dir && name == READER_FOLDER {
                for c in self.cloud.list_dir(&n.pid).await? {
                    s.insert(c.pid.clone(), entry(format!("{name}/{}", name_of(&c)), &c));
                }
            }
            s.insert(n.pid.clone(), entry(name, &n));
        }
        Ok(s)
    }

    /// A crashed run's sandbox (older than 48 hours) is removed; a younger one means
    /// another run may be going on: fail.
    async fn sweep_stale(&self, now_ms: i64) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        for n in self.cloud.list_dir(&self.top).await? {
            let name = name_of(&n);
            if n.pid == self.root
                || !n.is_dir
                || !name.to_ascii_lowercase().starts_with(SANDBOX_PREFIX)
                || check_sandbox_name(&name).is_err()
            {
                continue;
            }
            let born = if n.created_ms > 0 {
                n.created_ms
            } else {
                n.base_mtime_ms
            };
            if now_ms - born < STALE_AFTER_MS {
                bail!(
                    "another live run's sandbox {name} is {} minutes old: one run per account at a time",
                    (now_ms - born) / 60_000
                );
            }
            on!(&self.cloud, b => b.delete_permanent(&n.pid).await)
                .map_err(|e| anyhow!("removing the stale sandbox {name}: {e}"))?;
            removed.push(name);
        }
        Ok(removed)
    }

    async fn delete_sandbox(&self) -> Result<()> {
        on!(&self.cloud, b => b.delete_permanent(&self.root).await)
            .map_err(|e| anyhow!("deleting the sandbox {}: {e}", self.name))?;
        if let Some(n) = self.cloud.child(&self.top, &self.name).await? {
            bail!("the sandbox {} is still there ({})", self.name, n.pid);
        }
        Ok(())
    }

    /// One byte of a seeded volume: `Ok(false)` when MEGA's transfer quota is out.
    async fn read_one_byte(&self, rel: &str) -> Result<bool> {
        let Some(n) = self.resolve(rel).await? else {
            bail!("{rel} is not in the sandbox");
        };
        let r = on!(&self.cloud, b => b.read(&n.pid, n.pin.as_deref(), Some((0, Some(0)))).await);
        match r {
            Ok(body) => {
                let got = read_to_vec(body).await?;
                self.downloaded
                    .fetch_add(got.len() as u64, Ordering::Relaxed);
                Ok(true)
            }
            Err(BErr::TransferQuota(_)) => Ok(false),
            Err(e) => Err(anyhow!("reading {rel}: {e}")),
        }
    }

    /// The test knob of `--inject-leak` (cloudsim only): a file outside the sandbox,
    /// written as a stray client would, so the diff has something to find.
    async fn inject_leak(&self) -> Result<()> {
        if self.bases.sim.is_none() {
            bail!("--inject-leak runs on cloudsim only");
        }
        let tmp = self.state_dir.join("leak");
        let body = b"written outside the sandbox";
        std::fs::write(&tmp, body)?;
        let src = UploadSource::File {
            path: &tmp,
            size: body.len() as u64,
            sha256: Sha256::digest(body).into(),
        };
        on!(&self.cloud, b => b.upload(UploadTarget::New { parent: &self.top, name: "live-run-leak.txt" }, src, &mut OpCtx::default()).await)
            .map_err(|e| anyhow!("the leak: {e}"))?;
        Ok(())
    }
}

impl LiveCloud for Sandbox {
    fn sha(&self, rel: &str) -> Result<Option<String>> {
        if let Some((_, _, sha, at)) = self.seen.lock().expect("lock").get(rel)
            && at.elapsed() < Duration::from_secs(1)
        {
            return Ok(sha.clone());
        }
        self.block(async {
            let Some(n) = self.resolve(rel).await? else {
                self.seen.lock().expect("lock").remove(rel);
                return Ok(None);
            };
            let cached = self.seen.lock().expect("lock").get(rel).cloned();
            if let Some((pid, version, sha, _)) = cached
                && pid == n.pid
                && version == n.version
            {
                self.seen
                    .lock()
                    .expect("lock")
                    .insert(rel.into(), (pid, version, sha.clone(), Instant::now()));
                return Ok(sha);
            }
            let body = on!(&self.cloud, b => b.read(&n.pid, n.pin.as_deref(), None).await)
                .map_err(|e| anyhow!("reading {rel}: {e}"))?;
            let bytes = read_to_vec(body).await?;
            self.downloaded
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            let sha = Some(sha256_hex(&bytes));
            self.seen.lock().expect("lock").insert(
                rel.into(),
                (
                    n.pid.clone(),
                    n.version.clone(),
                    sha.clone(),
                    Instant::now(),
                ),
            );
            Ok(sha)
        })
    }

    fn seed(&self, files: &[(String, Vec<u8>)]) -> Result<()> {
        self.block(async {
            for (rel, bytes) in files {
                self.put(rel, bytes).await?;
            }
            Ok(())
        })
    }

    fn login(&self, servers: &Path) -> Result<CloudLogin> {
        let mut login = self.login.clone();
        if let Some(cell) = &self.newest {
            update_newest(cell, servers, self.p);
            let t = cell.lock().expect("lock").token.clone();
            login.credential["refresh_token"] = t.expose().into();
        }
        Ok(login)
    }

    fn server_env(&self) -> Vec<(String, String)> {
        self.bases.server_env()
    }

    fn transfer_ok(&self, rel: &str) -> Result<bool> {
        if self.p != LiveProvider::Mega {
            return Ok(true);
        }
        self.block(self.read_one_byte(rel))
    }
}

/// The volume the quota probe reads one byte of (the cloud suite's first).
const QUOTA_PROBE: &str = "Series/Vol 1.cbz";

/// Every `<servers>/<name>/cloud/credentials.json` of the run.
fn credential_files(servers: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(servers) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|d| d.path().join("cloud").join("credentials.json"))
        .filter(|p| p.is_file())
        .collect()
}

/// Take a refresh token from the servers' credential files when one is newer than the
/// cell's (Bunko stores its rotated token before it uses it).
fn update_newest(cell: &Mutex<Newest>, servers: &Path, p: LiveProvider) {
    let backend = p.backend();
    for f in credential_files(servers) {
        let Ok(meta) = std::fs::metadata(&f) else {
            continue;
        };
        let Ok(at) = meta.modified() else { continue };
        let Ok(doc) = std::fs::read(&f)
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(serde_json::from_slice::<Value>(&b)?))
        else {
            continue;
        };
        let Some(stores) = doc["stores"].as_object() else {
            continue;
        };
        for c in stores.values() {
            if c["provider"] != backend {
                continue;
            }
            let Some(t) = c["refresh_token"].as_str().filter(|t| !t.is_empty()) else {
                continue;
            };
            let mut n = cell.lock().expect("lock");
            if t != n.token.expose() && at > n.at {
                let s = Secret::new(t);
                mask(&[&s]);
                *n = Newest { token: s, at };
            }
        }
    }
}

/// Remove the credential files and configs (the Google client's secret) of the run's
/// servers: the work folder may be uploaded or kept.
fn scrub(servers: &Path) {
    for f in credential_files(servers) {
        let _ = std::fs::remove_file(f);
    }
    if let Ok(rd) = std::fs::read_dir(servers) {
        for d in rd.flatten() {
            let _ = std::fs::remove_file(d.path().join("config.yaml"));
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// What a run writes as `live-<provider>.json`.
#[derive(serde::Serialize)]
struct Summary {
    provider: &'static str,
    run_id: String,
    sandbox: String,
    outcome: RunOutcome,
    reason: Option<String>,
    on_cloudsim: bool,
    stale_sandboxes_removed: Vec<String>,
    diff: Vec<Change>,
    /// Bytes the runner itself downloaded (hash checks), every provider.
    runner_bytes_downloaded: u64,
    token_stored: bool,
    scenarios: Value,
    started_at_ms: i64,
    duration_ms: u64,
}

fn yaml_str(s: &str) -> String {
    serde_json::to_string(s).expect("a string")
}

/// A live run's credential, read from the environment.
pub struct Creds {
    /// The refresh token (Drive, OneDrive) or the session (MEGA).
    pub secret: Secret,
    /// Drive: the beta Desktop client's secret.
    pub google_secret: Option<Secret>,
}

impl Creds {
    fn from_env(p: LiveProvider) -> Result<Creds> {
        let secret = env_secret(p.target().secret_name(), p)?;
        if p == LiveProvider::Mega {
            mask_mega(&secret);
        } else {
            mask(&[&secret]);
        }
        let google_secret = if p == LiveProvider::Gdrive {
            Some(google_secret(
                None,
                std::env::var(GOOGLE_SECRET_ENV).ok(),
                bunko_remote::auth::clients::BETA_DESKTOP_CLIENT_SECRET,
            )?)
        } else {
            None
        };
        if let Some(g) = &google_secret {
            mask(&[g]);
        }
        Ok(Creds {
            secret,
            google_secret,
        })
    }
}

/// The provider driver signed in, and Bunko's sign-in to the sandbox.
struct Connected {
    cloud: Cloud,
    login: CloudLogin,
    newest: Option<Arc<Mutex<Newest>>>,
}

/// Refresh once (checking the credential with one read), then sign the runner's
/// driver in. The account is not printed: it would land in a public log.
async fn connect(
    p: LiveProvider,
    bases: &Bases,
    creds: &Creds,
    state_dir: &Path,
    name: &str,
) -> Result<Connected> {
    let mut sink = std::io::sink();
    match p {
        LiveProvider::Gdrive | LiveProvider::Onedrive => {
            let refresh_token = creds.secret.clone();
            mask(&[&refresh_token]);
            let mut client = if p == LiveProvider::Gdrive {
                let s = creds
                    .google_secret
                    .clone()
                    .context("no Google client secret")?;
                mask(&[&s]);
                OAuthClient::google(BETA_DESKTOP_CLIENT_ID, s.expose())
            } else {
                OAuthClient::microsoft(MICROSOFT_CLIENT_ID)
            };
            client.token_url = if p == LiveProvider::Gdrive {
                bases.google_token.clone()
            } else {
                bases.microsoft_token.clone()
            };
            let api = if p == LiveProvider::Gdrive {
                &bases.drive_api
            } else {
                &bases.graph_api
            };
            // The account is not printed: it would land in a public log.
            let got =
                refresh_and_verify(p.target(), &client, &refresh_token, api, &mut sink).await?;
            mask(&[&got.secret]);
            // A refresh minted an access token; the next one comes from LiveToken.
            let first = refresh(&client, got.secret.expose())
                .await
                .map_err(|e| anyhow!("refreshing the {} sign-in: {e}", p.name()))?;
            let access = Secret::new(first.access_token);
            mask(&[&access]);
            let newest_token = match first.refresh_token {
                Some(t) if t != got.secret.expose() => Secret::new(t),
                _ => got.secret.clone(),
            };
            mask(&[&newest_token]);
            let cell = Arc::new(Mutex::new(Newest {
                token: newest_token,
                at: SystemTime::now(),
            }));
            let token = Arc::new(LiveToken {
                client: client.clone(),
                access: Mutex::new((access, Instant::now())),
                newest: cell.clone(),
            });
            let cloud = if p == LiveProvider::Gdrive {
                let mut cfg = GDriveConfig::new(state_dir);
                cfg.api_base = bases.drive_api.clone();
                Cloud::Drive(GDriveBackend::new(token, cfg).map_err(|e| anyhow!("{e}"))?)
            } else {
                let cfg = OneDriveConfig {
                    api_base: bases.graph_api.clone(),
                    ..OneDriveConfig::default()
                };
                Cloud::OneDrive(OneDriveBackend::new(token, cfg).map_err(|e| anyhow!("{e}"))?)
            };
            let account = on!(&cloud, b => b.account().await).map_err(|e| anyhow!("{e}"))?;
            let (credential, pairing, library_yaml) = if p == LiveProvider::Gdrive {
                let secret = client.client_secret.clone().unwrap_or_default();
                (
                    json!({"client_id": BETA_DESKTOP_CLIENT_ID, "account_label": "live test account"}),
                    Some(format!("custom:{BETA_DESKTOP_CLIENT_ID}")),
                    Some(format!(
                        "    oauth:\n      google:\n        client_id: {}\n        client_secret: {}\n",
                        yaml_str(BETA_DESKTOP_CLIENT_ID),
                        yaml_str(&secret)
                    )),
                )
            } else {
                (
                    json!({"client_id": MICROSOFT_CLIENT_ID, "account_label": "live test account"}),
                    None,
                    None,
                )
            };
            let login = CloudLogin {
                backend: p.backend(),
                account_id: account.id,
                credential,
                folder: Some(name.to_string()),
                pairing,
                library_yaml,
                ..CloudLogin::default()
            };
            Ok(Connected {
                cloud,
                login,
                newest: Some(cell),
            })
        }
        LiveProvider::Mega => {
            let blob = creds.secret.clone();
            mask_mega(&blob);
            let mut cfg = MegaConfig::new(state_dir);
            cfg.client.api_origin = bases.mega_origin.clone();
            mega_verify(cfg.client.clone(), &blob, &mut sink).await?;
            let backend = MegaBackend::resume(&SessionBlob::new(blob.expose()), cfg)
                .await
                .map_err(|e| anyhow!("resuming the MEGA session: {e}"))?;
            let cloud = Cloud::Mega(backend);
            let account = on!(&cloud, b => b.account().await).map_err(|e| anyhow!("{e}"))?;
            let login = CloudLogin {
                backend: "mega",
                account_id: account.id,
                credential: json!({"mega_session": blob.expose(), "account_label": "live test account"}),
                folder: Some(name.to_string()),
                ..CloudLogin::default()
            };
            Ok(Connected {
                cloud,
                login,
                newest: None,
            })
        }
    }
}

pub fn run(a: &LiveArgs) -> Result<()> {
    // The GitHub token the write-back uses is a secret of the run too.
    for k in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(v) = std::env::var(k) {
            mask(&[&Secret::new(v)]);
        }
    }
    run_inner(a).map_err(|e| redacted_error(redactor(), &e))
}

fn run_inner(a: &LiveArgs) -> Result<()> {
    let p = a.provider;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let root = util::workspace_root();
    let target = util::target_dir(&root);
    let out_dir = a
        .out
        .clone()
        .unwrap_or_else(|| target.join("interop").join("live"));
    let suffix = std::env::var("GITHUB_RUN_ID").unwrap_or_else(|_| "local".into());
    let id = match &a.run_id {
        Some(r) => r.clone(),
        None => run_id(now_ms() / 1000, &suffix),
    };
    let name = sandbox_name(&id)?;
    let bin = a.bin.clone().unwrap_or_else(|| {
        target
            .join("release")
            .join(format!("mokuro-bunko{}", std::env::consts::EXE_SUFFIX))
    });
    if !bin.exists() {
        bail!(
            "{} not found: pass --bin <a testing build of mokuro-bunko>",
            bin.display()
        );
    }
    rig::check_testing_build(&bin)?;
    let bases = match &a.sim_base {
        Some(b) => Bases::sim(b),
        None => Bases::real(),
    };
    let writer = SecretWriter {
        gh: a.gh.clone(),
        repo: a.repo.clone(),
        dry_run: a.no_store,
    };
    let work = out_dir.join("work").join(p.name());
    let _ = std::fs::remove_dir_all(&work);
    let state_dir = work.join("runner");
    std::fs::create_dir_all(&state_dir)?;
    let started = now_ms();
    let t0 = Instant::now();
    say(format!(
        "== live {} in {name}{}",
        p.name(),
        if bases.sim.is_some() {
            " (cloudsim)"
        } else {
            ""
        }
    ));

    // 1. The credential.
    let creds = Creds::from_env(p)?;
    let mut stored_token = None::<Secret>;
    let mut token_stored = false;
    let connected = rt.block_on(connect(p, &bases, &creds, &state_dir, &name))?;
    let newest = connected.newest.clone();
    // Store a rotated token before anything else can fail.
    let store = |t: &Secret, stored: &mut Option<Secret>, flag: &mut bool| -> Result<()> {
        if stored.as_ref() == Some(t) {
            return Ok(());
        }
        let mut said = Vec::new();
        let r = writer.store(p.target().secret_name(), t, &mut said);
        say(String::from_utf8_lossy(&said).trim_end());
        r?;
        *stored = Some(t.clone());
        *flag = !writer.dry_run;
        Ok(())
    };
    if let Some(cell) = &newest {
        let t = cell.lock().expect("lock").token.clone();
        if t != creds.secret {
            store(&t, &mut stored_token, &mut token_stored)?;
        } else {
            stored_token = Some(creds.secret.clone());
        }
    }

    // 2. The sandbox, then the snapshot.
    let sandbox = Sandbox::open(rt, connected, p, &name, &bases, &state_dir)?;

    let mut reason = None;
    let mut outcome = RunOutcome::Passed;
    let mut scenarios = Value::Null;
    let mut stale = Vec::new();
    let servers = work.join("suite");
    std::fs::create_dir_all(&servers)?;

    let body = (|| -> Result<Snapshot> {
        stale = sandbox.block(sandbox.sweep_stale(now_ms()))?;
        for s in &stale {
            say(format!("removed a crashed run's sandbox {s}"));
        }
        let before = sandbox.block(sandbox.snapshot())?;
        let paths = Paths {
            out: out_dir.clone(),
            tools: target.join("interop").join("tools"),
            bin: bin.clone(),
            cloudsim_bin: PathBuf::new(),
            rclone_bin: None,
        };
        let mut report = Report::new("cloud", p.rig_provider().name(), &bin);
        report.bunko_sha = None;
        rig::run_live(
            &paths,
            p.rig_provider(),
            &servers,
            &sandbox,
            a.grep.as_deref(),
            &mut report,
        );
        report.finish();
        scenarios = serde_json::to_value(&report.scenarios)?;
        for line in scenario_lines(redactor(), &report.scenarios) {
            eprintln!("{line}");
        }
        if report.outcome == Outcome::Failed {
            // A MEGA run that failed with the transfer quota out was skipped (§7.3).
            if p == LiveProvider::Mega && !sandbox.transfer_ok(QUOTA_PROBE).unwrap_or(true) {
                outcome = RunOutcome::Skipped;
                reason = Some("skipped: transfer quota".into());
            } else {
                outcome = RunOutcome::Failed;
                reason = Some(format!("{} scenario(s) failed", report.failed));
            }
        }
        Ok(before)
    })();

    // 4. Whatever happened.
    if let Some(cell) = &newest {
        update_newest(cell, &servers, p);
        let t = cell.lock().expect("lock").token.clone();
        if let Err(e) = store(&t, &mut stored_token, &mut token_stored) {
            say(format!(
                "storing the newest {} token failed: {e:#}",
                p.name()
            ));
            outcome = RunOutcome::Failed;
            reason.get_or_insert_with(|| "the rotated token was not stored".into());
        }
    }
    scrub(&servers);
    if a.inject_leak
        && let Err(e) = sandbox.block(sandbox.inject_leak())
    {
        say(format!("{e:#}"));
    }
    let cleanup = sandbox.block(sandbox.delete_sandbox());
    let mut diff = Vec::new();
    match (&body, cleanup) {
        (Err(e), _) => {
            outcome = RunOutcome::Failed;
            reason = Some(format!("{e:#}"));
        }
        (Ok(_), Err(e)) => {
            outcome = RunOutcome::Failed;
            reason = Some(format!("cleanup: {e:#}"));
        }
        (Ok(before), Ok(())) => match sandbox.block(sandbox.snapshot()) {
            Ok(after) => {
                diff = before.diff(&after);
                if !diff.is_empty() {
                    outcome = RunOutcome::Failed;
                    reason = Some(format!(
                        "the account is not as the run found it: {}",
                        diff.iter()
                            .map(|c| c.to_string())
                            .collect::<Vec<_>>()
                            .join("; ")
                    ));
                }
            }
            Err(e) => {
                outcome = RunOutcome::Failed;
                reason = Some(format!("the snapshot after the run: {e:#}"));
            }
        },
    }
    let previous = a
        .previous
        .as_deref()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| serde_json::from_value::<RunOutcome>(v["outcome"].clone()).ok());
    let (final_outcome, why) = with_previous(outcome, previous);
    if let Some(why) = why {
        reason = Some(why);
    }
    let summary = Summary {
        provider: p.name(),
        run_id: id,
        sandbox: name,
        outcome: final_outcome,
        reason: reason.clone(),
        on_cloudsim: bases.sim.is_some(),
        stale_sandboxes_removed: stale,
        diff,
        runner_bytes_downloaded: sandbox.downloaded.load(Ordering::Relaxed),
        token_stored,
        scenarios,
        started_at_ms: started,
        duration_ms: t0.elapsed().as_millis() as u64,
    };
    std::fs::create_dir_all(&out_dir)?;
    let file = out_dir.join(format!("live-{}.json", p.name()));
    let doc = summary_doc(redactor(), &summary)?;
    std::fs::write(&file, serde_json::to_vec_pretty(&doc)?)?;
    let reason = reason.map(|r| excerpt(&redactor().redact(&r), REASON_MAX));
    say(format!(
        "live {}: {:?}{} (runner downloaded {} bytes) -> {}",
        p.name(),
        final_outcome,
        reason
            .as_deref()
            .map(|r| format!(": {r}"))
            .unwrap_or_default(),
        summary.runner_bytes_downloaded,
        file.display()
    ));
    if !a.keep {
        let _ = std::fs::remove_dir_all(&work);
    }
    match final_outcome {
        RunOutcome::Passed => Ok(()),
        RunOutcome::Skipped => {
            if in_actions() {
                println!(
                    "{}",
                    redactor().redact(&format!(
                        "::warning::live {}: {}",
                        p.name(),
                        reason.unwrap_or_default()
                    ))
                );
            }
            Ok(())
        }
        RunOutcome::Failed => bail!("live {} failed: {}", p.name(), reason.unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bunko_cloudsim::Provider as Sim;

    const ACCOUNT: &str = "live";

    struct Rigged {
        // Dropped first: the simulator checks its violations while the runtime runs.
        sim: bunko_cloudsim::CloudSim,
        sb: Sandbox,
        _dir: tempfile::TempDir,
    }

    fn sim_sandbox(p: LiveProvider, run: &str) -> Rigged {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let sim = rt.block_on(bunko_cloudsim::start());
        // The reader's library, already in the account.
        let sp = match p {
            LiveProvider::Gdrive => Some(Sim::Google),
            LiveProvider::Onedrive => Some(Sim::Microsoft),
            LiveProvider::Mega => None,
        };
        if let Some(sp) = sp {
            sim.seed(
                sp,
                ACCOUNT,
                &[
                    ("mokuro-reader/volume-data.json".into(), b"{}".to_vec()),
                    ("mokuro-reader/Old Series/Vol 1.cbz".into(), vec![7u8; 64]),
                    ("notes.txt".into(), b"mine".to_vec()),
                ],
            );
        }
        let creds = match p {
            LiveProvider::Gdrive => Creds {
                secret: Secret::new(sim.token(Sim::Google, ACCOUNT).refresh_token),
                google_secret: Some(Secret::new("test-client-secret")),
            },
            LiveProvider::Onedrive => Creds {
                secret: Secret::new(sim.token(Sim::Microsoft, ACCOUNT).refresh_token),
                google_secret: None,
            },
            LiveProvider::Mega => {
                let m = sim.mega(ACCOUNT);
                let lib = m.mkdir(bunko_cloudsim::mega::ROOT, "mokuro-reader");
                m.put(&lib, "volume-data.json", b"{}");
                Creds {
                    secret: Secret::new(m.session_blob()),
                    google_secret: None,
                }
            }
        };
        let dir = tempfile::tempdir().unwrap();
        let bases = Bases::sim(&sim.base);
        let name = sandbox_name(run).unwrap();
        let c = rt
            .block_on(connect(p, &bases, &creds, dir.path(), &name))
            .unwrap();
        let sb = Sandbox::open(rt, c, p, &name, &bases, dir.path()).unwrap();
        Rigged { sim, sb, _dir: dir }
    }

    fn round_trip(p: LiveProvider) {
        let r = sim_sandbox(p, "20261115t000000-test");
        let sb = &r.sb;
        let before = sb.block(sb.snapshot()).unwrap();
        let paths: Vec<&str> = before.0.values().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"mokuro-reader"), "{paths:?}");
        assert!(
            paths.contains(&"mokuro-reader/volume-data.json"),
            "{paths:?}"
        );
        // Direct children only, and never the sandbox.
        assert!(!paths.iter().any(|p| p.contains("Vol 1.cbz")), "{paths:?}");
        assert!(
            !paths.iter().any(|p| p.starts_with(SANDBOX_PREFIX)),
            "{paths:?}"
        );

        let files = vec![
            ("Series/Vol 1.cbz".to_string(), vec![1u8; 300]),
            ("Series/series.json".to_string(), b"{\"v\":1}".to_vec()),
        ];
        sb.seed(&files).unwrap();
        assert_eq!(
            sb.sha("Series/series.json").unwrap(),
            Some(sha256_hex(b"{\"v\":1}"))
        );
        assert_eq!(sb.sha("Series/none.json").unwrap(), None);
        // A replacement is seen (a new version is downloaded once).
        sb.seed(&[("Series/series.json".into(), b"{\"v\":2}".to_vec())])
            .unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(
            sb.sha("Series/series.json").unwrap(),
            Some(sha256_hex(b"{\"v\":2}"))
        );
        assert!(sb.transfer_ok("Series/Vol 1.cbz").unwrap());
        // Writes outside the sandbox are refused before any request.
        assert!(sb.seed(&[("../escape.txt".into(), b"x".to_vec())]).is_err());
        assert!(sb.seed(&[("/escape.txt".into(), b"x".to_vec())]).is_err());

        sb.block(sb.delete_sandbox()).unwrap();
        let after = sb.block(sb.snapshot()).unwrap();
        assert_eq!(before.diff(&after), Vec::new(), "the account changed");

        // A stray write outside the sandbox: the diff names it.
        sb.block(sb.inject_leak()).unwrap();
        let leaked = sb.block(sb.snapshot()).unwrap();
        let d: Vec<String> = before.diff(&leaked).iter().map(|c| c.to_string()).collect();
        assert!(d.contains(&"added: live-run-leak.txt".to_string()), "{d:?}");
        let _ = r.sim.take_violations();
    }

    #[test]
    fn drive_sandbox_round_trip() {
        round_trip(LiveProvider::Gdrive);
    }

    #[test]
    fn onedrive_sandbox_round_trip() {
        round_trip(LiveProvider::Onedrive);
    }

    #[test]
    fn mega_sandbox_round_trip() {
        round_trip(LiveProvider::Mega);
    }

    #[test]
    fn a_crashed_runs_sandbox_is_swept_and_a_live_one_stops_the_run() {
        let r = sim_sandbox(LiveProvider::Gdrive, "20261115t000000-mine");
        let sb = &r.sb;
        sb.block(async {
            on!(&sb.cloud, b => b.create_root("bunko-interop-20261101t000000-other").await)
        })
        .unwrap();
        // Something else with the prefix but not a sandbox name is left alone.
        sb.block(async {
            on!(&sb.cloud, b => b.create_root("bunko-interop-notes and more").await)
        })
        .unwrap();
        let err = sb.block(sb.sweep_stale(now_ms())).unwrap_err();
        assert!(err.to_string().contains("one run per account"), "{err}");
        let removed = sb
            .block(sb.sweep_stale(now_ms() + STALE_AFTER_MS + 60_000))
            .unwrap();
        assert_eq!(removed, ["bunko-interop-20261101t000000-other"]);
        let left: Vec<String> = sb
            .block(sb.cloud.list_dir(&sb.top))
            .unwrap()
            .iter()
            .map(name_of)
            .collect();
        assert!(
            left.contains(&"bunko-interop-notes and more".to_string()),
            "{left:?}"
        );
        assert!(left.contains(&sb.name), "{left:?}");
        let _ = r.sim.take_violations();
    }

    #[test]
    fn onedrive_keeps_the_newest_token_and_the_servers_newer_one() {
        let r = sim_sandbox(LiveProvider::Onedrive, "20261115t000000-tok");
        let cell = r.sb.newest.clone().unwrap();
        let servers = tempfile::tempdir().unwrap();
        // Bunko's login carries the runner's newest token.
        let login = r.sb.login(servers.path()).unwrap();
        let first = cell.lock().unwrap().token.clone();
        assert_eq!(login.credential["refresh_token"], first.expose());
        assert_eq!(login.folder(), r.sb.name);
        assert_eq!(login.backend, "onedrive");
        // A server rotated it since: its file is newer.
        std::thread::sleep(Duration::from_millis(20));
        let cloud = servers.path().join("server-a").join("cloud");
        std::fs::create_dir_all(&cloud).unwrap();
        std::fs::write(
            cloud.join("credentials.json"),
            json!({"version": 1, "stores": {"x": {"provider": "onedrive", "refresh_token": "rotated-by-bunko"}}}).to_string(),
        )
        .unwrap();
        std::fs::write(servers.path().join("server-a").join("config.yaml"), "x").unwrap();
        update_newest(&cell, servers.path(), LiveProvider::Onedrive);
        assert_eq!(cell.lock().unwrap().token.expose(), "rotated-by-bunko");
        scrub(servers.path());
        assert!(credential_files(servers.path()).is_empty());
        assert!(!servers.path().join("server-a").join("config.yaml").exists());
        let _ = r.sim.take_violations();
    }

    #[test]
    fn drive_logins_name_the_beta_client_and_the_sandbox() {
        let r = sim_sandbox(LiveProvider::Gdrive, "20261115t000000-login");
        let login = r.sb.login(Path::new("/nonexistent")).unwrap();
        assert_eq!(login.backend, "gdrive");
        assert_eq!(login.credential["client_id"], BETA_DESKTOP_CLIENT_ID);
        assert_eq!(
            login.pairing.as_deref(),
            Some(format!("custom:{BETA_DESKTOP_CLIENT_ID}").as_str())
        );
        let yaml = login.library_yaml.clone().unwrap();
        assert!(
            yaml.contains("client_secret: \"test-client-secret\""),
            "{yaml}"
        );
        // Debug never shows the credential or the client.
        let dbg = format!("{login:?}");
        assert!(
            !dbg.contains("test-client-secret") && !dbg.contains("refresh"),
            "{dbg}"
        );
        let _ = r.sim.take_violations();
    }

    /// Every kind of secret a run collects, planted in an error chain, a scenario's
    /// detail (with a log tail after it) and a diff path: none reaches the result file,
    /// the scenario lines, the final error or the masks' complement.
    #[test]
    fn no_secret_reaches_the_result_file_or_the_output() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut raw = vec![1u8];
        raw.extend([5u8; 16]);
        raw.extend(b"planted-mega-session-id-bytes-xyz");
        let blob = b64.encode(&raw);
        let sid = b64.encode(&raw[17..]);
        let secrets = [
            "1//planted-refresh-token",
            "M.C5-planted-rotated-token",
            "ya29.planted-access-token",
            "planted-client-secret-GOCSPX",
            "github_pat_planted_token_0123",
        ];
        let red = Redactor::default();
        for s in secrets {
            red.add(s);
        }
        red.add_mega_session(&blob);
        let all: Vec<String> = secrets
            .iter()
            .map(|s| s.to_string())
            .chain([
                blob.clone(),
                sid.clone(),
                "1%2F%2Fplanted-refresh-token".into(),
            ])
            .collect();

        let chain = anyhow!("HTTP 400: {{\"refresh_token\": \"{}\"}}", secrets[0])
            .context(format!("GET /cs?id=1&sid={sid} with {}", secrets[2]))
            .context(format!("token endpoint said {}", secrets[1]));
        let reason = format!("{chain:#}");
        let detail = format!(
            "setup: a refresh with {} failed\nserve.log: client_secret={} pat={} session={blob} {}",
            secrets[3], secrets[3], secrets[4], "1%2F%2Fplanted-refresh-token"
        );
        let scen = rig::Scenario {
            name: "cold-start".into(),
            outcome: Outcome::Failed,
            ms: 1,
            detail: Some(detail.clone()),
            capability: None,
            notes: vec![format!("note {}", secrets[1])],
        };
        let summary = Summary {
            provider: "onedrive",
            run_id: "r".into(),
            sandbox: "bunko-interop-r".into(),
            outcome: RunOutcome::Failed,
            reason: Some(reason.clone()),
            on_cloudsim: true,
            stale_sandboxes_removed: vec![],
            diff: vec![Change::Added {
                path: format!("leak-{}.txt", secrets[4]),
            }],
            runner_bytes_downloaded: 0,
            token_stored: false,
            scenarios: serde_json::to_value(vec![scen.clone()]).unwrap(),
            started_at_ms: 0,
            duration_ms: 0,
        };
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("live-onedrive.json");
        let doc = summary_doc(&red, &summary).unwrap();
        std::fs::write(&file, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
        let written = std::fs::read_to_string(&file).unwrap();
        let lines = scenario_lines(&red, &[scen]).join("\n");
        let err = format!("{:#}", redacted_error(&red, &chain));
        for (what, text) in [
            ("result file", &written),
            ("stderr lines", &lines),
            ("error", &err),
        ] {
            for s in &all {
                assert!(!text.contains(s.as_str()), "{what} carries {s}: {text}");
            }
            assert!(text.contains("<redacted>"), "{what}: {text}");
        }
        // Bounded, one line: the log tail after the detail's first line is gone.
        assert!(!written.contains("serve.log"), "{written}");
        assert!(
            doc["scenarios"][0]["detail"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= DETAIL_MAX + 1
        );
        assert!(doc["reason"].as_str().unwrap().chars().count() <= REASON_MAX + 1);
    }

    #[test]
    fn cloudsim_bases_reach_the_server_and_real_ones_add_nothing() {
        assert!(Bases::real().server_env().is_empty());
        let env = Bases::sim("http://127.0.0.1:1/").server_env();
        assert!(env.contains(&(
            "MOKURO_TEST_CLOUD_API_BASE_GDRIVE".into(),
            "http://127.0.0.1:1/google".into()
        )));
        assert_eq!(env.len(), 5);
    }
}
