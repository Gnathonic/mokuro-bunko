//! Connecting a cloud library (0.8 design §7, §11; M6): what one "Connect" click runs.
//!
//! [`Connector`] holds at most one sign-in at a time and answers its state
//! ([`ConnectStatus`]) to the setup wizard, the admin Storage panel, the tray and the
//! CLI. The state never carries a token, a password or a session blob: only what the
//! person needs to act (the device flow's user code and its verification URL, or the
//! loopback and paste flows' authorization URL to open), the account label and the
//! Check.
//!
//! * **Google Drive / OneDrive** (OAuth). Unless the request names a flow (`loopback`,
//!   `device` or `paste`), it is chosen automatically
//!   ([`bunko_remote::auth::choose_flow`]): loopback + PKCE when the request comes from
//!   loopback (a browser on this machine, not an embedded WebView) and the pairing has
//!   a loopback client. Otherwise (Docker, a NAS, a LAN browser) Drive gets the device
//!   flow and OneDrive the pasted address: Microsoft refuses device codes for personal
//!   accounts (AADSTS70002), so the person signs in on any device and pastes back the
//!   `http://localhost:<port>/bunko?code=…` address their browser ends on
//!   ([`Connector::paste`]); the PKCE verifier stays in this process's memory. The CLI
//!   names its flow (it knows whether it can open a browser). A device code is shown
//!   in the wizard, admin and the tray and printed to the log with its expiry; an
//!   expired code is replaced at once, and after [`MAX_UNUSED_CODES`] unused codes the
//!   sign-in stops and waits for a click. Drive signs in with the clients of the
//!   pairing the person chose ("Which Mokuro Reader do you use?", D25);
//!   `storage.library.oauth.google.*` overrides make it `custom`.
//! * **MEGA**: email, password and the optional two-factor code, once; the session
//!   blob is kept, the password never is.
//! * **WebDAV**: URL, user and app password; the driver's capability probe runs live.
//!
//! Then the **Check**: the account, the `mokuro-reader/` folder ("N volumes in M
//! series", or "Create it?"), duplicate roots, another Bunko's lease, and on Drive the
//! pairing with the switch offer of §7.
//!
//! The rules on what may be replaced (§7, D25):
//! * the same store (same provider, pairing, account and folder): a new sign-in
//!   replaces its tokens;
//! * a store that never ran here (no `state.sqlite`): replaced silently (the wizard's
//!   "try the other pairing");
//! * a store that ran, with **another account** of the same provider: refused until the
//!   admin chooses "use this account's library instead", which runs that store's
//!   Disconnect first (through the owner's hook);
//! * a store that ran with another pairing, folder or provider: refused ("disconnect
//!   the old library first"). A pairing change is a Disconnect plus a new store.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bunko_core::config::{GoogleProject, LibraryBackend, LibraryConfig};
use bunko_remote::auth::{
    AuthError, CredentialStore, DeviceFlow, Flow, LoopbackFlow, OAuthClient, PasteFlow, Provider,
    StoredCredential, Tokens, choose_flow, google_clients, microsoft_client,
};
use bunko_remote::mega::{LoginError, SessionBlob};
use bunko_remote::{AccountInfo, BErr, Backend, RNode};
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::store_id;

/// Device codes that may expire unused before a sign-in stops and waits for a click.
pub const MAX_UNUSED_CODES: u32 = 5;

/// A Drive folder with this many volumes or fewer gets the "try the other pairing"
/// offer: M0's wrong pairing listed at most 10 files (§7).
pub const PAIRING_OFFER_MAX_VOLUMES: u64 = 10;

/// Archive extensions the Check counts as volumes.
const VOLUME_EXTS: &[&str] = bunko_library::sidecar::VOLUME_ARCHIVE_EXTENSIONS;

/// The name a provider is shown by.
pub fn provider_name(b: LibraryBackend) -> &'static str {
    match b {
        LibraryBackend::Local => "this computer",
        LibraryBackend::GDrive => "Google Drive",
        LibraryBackend::OneDrive => "OneDrive",
        LibraryBackend::Mega => "MEGA",
        LibraryBackend::WebDav => "the WebDAV server",
    }
}

/// The pairing an embedded Google client belongs to (`None`: an override or unknown).
pub fn pairing_of_client(client_id: &str) -> Option<GoogleProject> {
    use bunko_remote::auth::clients::*;
    match client_id {
        RELEASE_DESKTOP_CLIENT_ID | RELEASE_DEVICE_CLIENT_ID => Some(GoogleProject::Release),
        BETA_DESKTOP_CLIENT_ID | BETA_DEVICE_CLIENT_ID => Some(GoogleProject::Beta),
        _ => None,
    }
}

/// How a pairing is shown: `release (reader.mokuro.app)`, `beta`, `custom (<id>)`.
pub fn pairing_label(p: &str) -> String {
    match p {
        "release" => "reader.mokuro.app (release)".into(),
        "beta" => "Mokuro Reader beta".into(),
        other => match other.strip_prefix("custom:") {
            Some(id) => format!("custom ({id})"),
            None => other.to_string(),
        },
    }
}

// ---------------------------------------------------------------- requests

/// The WebDAV form.
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct WebDavForm {
    pub url: String,
    pub username: String,
    /// An app password (kept in the credentials file, never in config.yaml).
    pub password: String,
    pub allow_http: bool,
}

/// The MEGA form. The password is used once and dropped.
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct MegaForm {
    pub email: String,
    pub password: String,
    /// The two-factor code, when the account has 2FA.
    pub totp: Option<String>,
}

/// What a Connect click asks for.
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct ConnectRequest {
    /// `gdrive`, `onedrive`, `mega` or `webdav`.
    pub provider: String,
    /// Drive only: `release` (reader.mokuro.app, the default) or `beta` (D25).
    pub reader: Option<String>,
    /// The cloud folder (default: the configured one, `mokuro-reader`).
    pub folder: Option<String>,
    /// Google Drive and OneDrive: `loopback` (or `browser`: a browser on the server's
    /// computer comes back to it), `device` (a code; OneDrive: work or school accounts
    /// only) or `paste` (sign in anywhere, paste the address back). None: chosen from
    /// where the request comes from.
    pub flow: Option<String>,
    pub webdav: Option<WebDavForm>,
    pub mega: Option<MegaForm>,
    /// The admin chose "use this account's library instead" (§7).
    pub use_this_account: bool,
}

impl fmt::Debug for ConnectRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectRequest")
            .field("provider", &self.provider)
            .field("reader", &self.reader)
            .field("folder", &self.folder)
            .field("flow", &self.flow)
            .field(
                "webdav",
                &self
                    .webdav
                    .as_ref()
                    .map(|w| (&w.url, &w.username, "<redacted>")),
            )
            .field(
                "mega",
                &self.mega.as_ref().map(|m| (&m.email, "<redacted>")),
            )
            .field("use_this_account", &self.use_this_account)
            .finish()
    }
}

/// Where a Connect click came from: decides the sign-in flow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestOrigin {
    /// The request reached the server from this machine (loopback peer and client).
    pub loopback: bool,
    /// An embedded browser (Android WebView): Google refuses OAuth there.
    pub webview: bool,
    /// `library connect` on this machine (in-process, or through the loopback-only
    /// control API). It names its flow; `loopback` is honoured since the terminal
    /// and the listener share the machine.
    pub cli: bool,
}

impl RequestOrigin {
    /// The CLI (`library connect`).
    pub fn cli() -> RequestOrigin {
        RequestOrigin {
            cli: true,
            ..RequestOrigin::default()
        }
    }

    /// A request's origin from its loopback-ness and its `User-Agent`.
    pub fn of(loopback: bool, user_agent: Option<&str>) -> RequestOrigin {
        RequestOrigin {
            loopback,
            webview: user_agent.is_some_and(is_webview),
            cli: false,
        }
    }

    /// A browser on this machine could come back to a loopback redirect.
    fn browser_here(self) -> bool {
        self.loopback && !self.webview
    }
}

/// An Android WebView's User-Agent (`; wv)`), or another embedded browser Google
/// refuses OAuth in.
pub fn is_webview(ua: &str) -> bool {
    ua.contains("; wv)") || ua.contains("Mokuro-Bunko-Android")
}

// ---------------------------------------------------------------- results

/// What the Check step shows (§11 step 4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CheckReport {
    pub folder: String,
    /// A folder of that exact name is visible.
    pub found: bool,
    /// How many folders of that name were found (more than one: merged, §5.9).
    pub roots: usize,
    pub volumes: u64,
    pub series: u64,
    /// Drive: the pairing signed in with (`release`, `beta`, `custom:<id>`).
    pub pairing: Option<String>,
    /// Drive: the other embedded pairing, when switching is offered.
    pub other_pairing: Option<String>,
    /// Drive: offer to try the other pairing (no folder, or 10 volumes or fewer).
    pub offer_switch: bool,
    /// The §7 warning that goes with the offer or with "Create it?".
    pub warning: Option<String>,
    /// Another Bunko manages this folder ("Another Bunko (nas) manages this library").
    pub lease: Option<String>,
    /// The folder was created by this Check.
    pub created: bool,
    /// The line the wizard shows.
    pub summary: String,
}

impl CheckReport {
    fn finish(&mut self) {
        self.summary = if self.found {
            let roots = if self.roots > 1 {
                format!(
                    " ({} folders of that name, merged into one library)",
                    self.roots
                )
            } else {
                String::new()
            };
            format!(
                "Found {}/ — {} in {}{roots}",
                self.folder,
                plural(self.volumes, "volume", "volumes"),
                plural(self.series, "series", "series"),
            )
        } else {
            format!("No {}/ folder yet. Create it?", self.folder)
        };
    }

    /// The Drive pairing offer and warnings (§7 Check step).
    fn pairing_advice(&mut self, pairing: &str) {
        let other = match pairing {
            "release" => "beta",
            "beta" => "release",
            _ => return,
        };
        self.pairing = Some(pairing.to_string());
        let (mine, theirs) = (pairing_label(pairing), pairing_label(other));
        if !self.found {
            self.offer_switch = true;
            self.other_pairing = Some(other.into());
            self.warning = Some(format!(
                "No Mokuro Reader folder is visible with the {mine} pairing. If you use {theirs}, switch the pairing."
            ));
        } else if self.volumes <= PAIRING_OFFER_MAX_VOLUMES {
            self.offer_switch = true;
            self.other_pairing = Some(other.into());
            self.warning = Some(format!(
                "Only {} visible with the {mine} pairing. If you read with {theirs}, try that pairing (one more consent); a small library can keep this one.",
                plural(self.volumes, "volume is", "volumes are")
            ));
        }
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn is_volume(name: &[u8]) -> bool {
    let name = String::from_utf8_lossy(name).to_lowercase();
    VOLUME_EXTS.iter().any(|ext| name.ends_with(ext))
}

/// The account and the Check of an opened backend: the folder's volumes and series,
/// duplicate roots, another Bunko's marker. With `create`, a missing folder is made.
/// `ours`: instance ids of this machine's stores (their markers are not "another
/// Bunko").
pub async fn check_backend<B: Backend>(
    b: &B,
    folder: &str,
    create: bool,
    ours: &HashSet<String>,
) -> Result<(AccountInfo, CheckReport), BErr> {
    use bunko_remote::status::{MARKER_NAME, Marker, MarkerNode};
    let account = b.account().await?;
    let mut roots: Vec<RNode> = b
        .locate_roots(folder)
        .await?
        .into_iter()
        .filter(|r| r.is_dir && !r.trashed)
        .collect();
    let mut report = CheckReport {
        folder: folder.to_string(),
        ..CheckReport::default()
    };
    if roots.is_empty() && create {
        roots.push(b.create_root(folder).await?);
        report.created = true;
    }
    report.found = !roots.is_empty();
    report.roots = roots.len();
    if !roots.is_empty() {
        let pids: Vec<String> = roots.iter().map(|r| r.pid.clone()).collect();
        let mut volumes = 0u64;
        let mut series: HashSet<String> = HashSet::new();
        let mut sink = |n: RNode| {
            if !n.is_dir && !n.trashed && is_volume(&n.name_raw) {
                volumes += 1;
                if let Some(p) = n.parent {
                    series.insert(p);
                }
            }
        };
        b.list_all(&pids, &mut sink).await?;
        report.volumes = volumes;
        report.series = series.len() as u64;
        for root in &roots {
            let Ok((nodes, now)) = b.find_named(&root.pid, MARKER_NAME).await else {
                continue;
            };
            for node in nodes.into_iter().filter(|n| !n.is_dir && !n.trashed) {
                if node.size > 64 * 1024 {
                    continue;
                }
                let Ok(body) = b.read(&node.pid, None, None).await else {
                    continue;
                };
                let Ok(bytes) = bunko_remote::backend::read_to_vec_max(body, 64 * 1024).await
                else {
                    continue;
                };
                let Some(marker) = Marker::parse(&bytes) else {
                    continue;
                };
                if ours.contains(&marker.instance_id) {
                    continue;
                }
                let m = MarkerNode { node, marker };
                if m.is_fresh(now) {
                    let name = if m.marker.instance_name.is_empty() {
                        "another machine".to_string()
                    } else {
                        m.marker.instance_name.clone()
                    };
                    report.lease = Some(format!(
                        "Another Bunko ({name}) manages this library. This Bunko waits as a standby until it stops, or until you take over in the admin panel."
                    ));
                }
            }
        }
    }
    report.finish();
    Ok((account, report))
}

// ---------------------------------------------------------------- provider seam

/// The secret a provider is opened with, held in memory only.
#[derive(Clone)]
pub enum Auth {
    OAuth {
        access_token: String,
    },
    Mega(SessionBlob),
    WebDav {
        url: String,
        username: String,
        password: String,
        allow_http: bool,
    },
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Auth::OAuth { .. } => "Auth::OAuth(<redacted>)",
            Auth::Mega(_) => "Auth::Mega(<redacted>)",
            Auth::WebDav { .. } => "Auth::WebDav(<redacted>)",
        })
    }
}

/// What the Connector asks of the providers: the live drivers, or a fake in tests.
pub trait ProviderOps: Send + Sync {
    /// Open `backend` with `auth`, then [`check_backend`].
    fn inspect<'a>(
        &'a self,
        backend: LibraryBackend,
        auth: &'a Auth,
        folder: &'a str,
        create: bool,
        scratch: &'a Path,
        ours: &'a HashSet<String>,
    ) -> BoxFuture<'a, Result<(AccountInfo, CheckReport), BErr>>;

    /// MEGA's one-time login.
    fn mega_login<'a>(
        &'a self,
        email: &'a str,
        password: &'a str,
        totp: Option<&'a str>,
    ) -> BoxFuture<'a, Result<SessionBlob, LoginError>>;
}

/// The real drivers.
pub struct LiveOps;

impl ProviderOps for LiveOps {
    fn inspect<'a>(
        &'a self,
        backend: LibraryBackend,
        auth: &'a Auth,
        folder: &'a str,
        create: bool,
        scratch: &'a Path,
        ours: &'a HashSet<String>,
    ) -> BoxFuture<'a, Result<(AccountInfo, CheckReport), BErr>> {
        Box::pin(async move {
            match (backend, auth) {
                (LibraryBackend::GDrive, Auth::OAuth { access_token }) => {
                    let b = bunko_remote::gdrive::GDriveBackend::new(
                        Arc::new(bunko_remote::token_dyn::StaticToken::new(
                            access_token.clone(),
                        )),
                        bunko_remote::gdrive::GDriveConfig::new(scratch.to_path_buf()),
                    )?;
                    check_backend(&b, folder, create, ours).await
                }
                (LibraryBackend::OneDrive, Auth::OAuth { access_token }) => {
                    let b = bunko_remote::onedrive::OneDriveBackend::new(
                        Arc::new(bunko_remote::token_dyn::StaticToken::new(
                            access_token.clone(),
                        )),
                        bunko_remote::onedrive::OneDriveConfig::default(),
                    )?;
                    check_backend(&b, folder, create, ours).await
                }
                (LibraryBackend::Mega, Auth::Mega(blob)) => {
                    let b = bunko_remote::mega::MegaBackend::resume(
                        blob,
                        bunko_remote::mega::MegaConfig::new(scratch.to_path_buf()),
                    )
                    .await?;
                    check_backend(&b, folder, create, ours).await
                }
                (
                    LibraryBackend::WebDav,
                    Auth::WebDav {
                        url,
                        username,
                        password,
                        allow_http,
                    },
                ) => {
                    let b = bunko_remote::webdav::WebDavBackend::connect(
                        bunko_remote::webdav::WebDavConfig {
                            url: url.clone(),
                            username: username.clone(),
                            password: password.clone(),
                            folder: folder.to_string(),
                            allow_http: *allow_http,
                            allow_bunko_backend_for_tests: false,
                            state_dir: scratch.to_path_buf(),
                            disable_cas: false,
                        },
                    )
                    .await?;
                    check_backend(&b, folder, create, ours).await
                }
                _ => Err(BErr::Fatal(
                    "this sign-in does not open that provider".into(),
                )),
            }
        })
    }

    fn mega_login<'a>(
        &'a self,
        email: &'a str,
        password: &'a str,
        totp: Option<&'a str>,
    ) -> BoxFuture<'a, Result<SessionBlob, LoginError>> {
        Box::pin(async move {
            bunko_remote::mega::MegaSession::login(email, password, totp.map(str::to_string)).await
        })
    }
}

// ---------------------------------------------------------------- state

/// One sign-in's state, as every page shows it. Never holds a secret.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConnectStatus {
    /// `idle`, `starting`, `browser` (loopback: open `auth_url`), `paste` (open
    /// `auth_url` on any device, then paste the address starting with `redirect_uri`
    /// that the browser ends on), `code` (device: enter `user_code` at
    /// `verification_url`), `checking`, `account-mismatch` (waiting for "use this
    /// account's library instead"), `connected`, `failed`, `stopped` (unused codes:
    /// waits for a click), `cancelled`.
    pub state: &'static str,
    pub provider: Option<&'static str>,
    /// `loopback`, `paste`, `device` or `form`.
    pub flow: Option<&'static str>,
    pub auth_url: Option<String>,
    /// `paste`: where the person's browser ends (`http://localhost:<port>/bunko`); the
    /// pasted address must start with it. Not a secret.
    pub redirect_uri: Option<String>,
    pub user_code: Option<String>,
    pub verification_url: Option<String>,
    /// RFC 3339, UTC.
    pub expires_at: Option<String>,
    /// Which code this is (1-based) of [`MAX_UNUSED_CODES`].
    pub code_number: Option<u32>,
    pub max_codes: u32,
    pub account: Option<String>,
    /// `account-mismatch`: the account the library on file belongs to.
    pub previous_account: Option<String>,
    pub store_id: Option<String>,
    pub pairing: Option<String>,
    pub check: Option<CheckReport>,
    pub error: Option<String>,
    pub message: Option<String>,
    /// `failed`: the flow that would work (`loopback` or `paste`), when the one that
    /// ran cannot (a personal Microsoft account with a device code).
    pub suggest_flow: Option<&'static str>,
    /// A new store (or new sign-in) waits for the server to restart onto it.
    pub needs_restart: bool,
    /// Bumped by every new sign-in.
    pub session: u64,
}

impl ConnectStatus {
    fn idle(max_codes: u32) -> ConnectStatus {
        ConnectStatus {
            state: "idle",
            provider: None,
            flow: None,
            auth_url: None,
            redirect_uri: None,
            user_code: None,
            verification_url: None,
            expires_at: None,
            code_number: None,
            max_codes,
            account: None,
            previous_account: None,
            store_id: None,
            pairing: None,
            check: None,
            error: None,
            message: None,
            suggest_flow: None,
            needs_restart: false,
            session: 0,
        }
    }

    /// Still waiting for the person or the provider.
    pub fn in_progress(&self) -> bool {
        matches!(
            self.state,
            "starting" | "browser" | "paste" | "code" | "checking"
        )
    }

    pub fn as_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// What a successful Connect writes into `storage.library` (never a secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryChoice {
    pub backend: LibraryBackend,
    pub folder: String,
    /// Drive: `release` or `beta` (`custom` stays as the overrides make it).
    pub google_project: Option<GoogleProject>,
    pub webdav_url: Option<String>,
    pub webdav_username: Option<String>,
    pub webdav_allow_http: bool,
}

impl LibraryChoice {
    /// Write the choice into a config's library block.
    pub fn apply_to(&self, lib: &mut LibraryConfig) {
        lib.backend = self.backend;
        lib.folder = self.folder.clone();
        if let Some(p) = self.google_project
            && !lib.oauth.google.any()
        {
            lib.google_project = p;
        }
        if self.backend == LibraryBackend::WebDav {
            lib.webdav.url = self.webdav_url.clone();
            lib.webdav.username = self.webdav_username.clone();
            lib.webdav.allow_http = self.webdav_allow_http;
        }
    }
}

/// Reads the configured library block.
pub type ConfigFn = Arc<dyn Fn() -> LibraryConfig + Send + Sync>;
/// Writes a successful Connect's choice into the configuration (and saves it).
pub type ApplyFn = Arc<dyn Fn(&LibraryChoice) -> Result<(), String> + Send + Sync>;
/// "Use this account's library instead": disconnect the store with this id (drained,
/// `released` written) or say why not.
pub type ReleaseOldFn = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Test knobs.
#[derive(Clone)]
pub struct Tuning {
    /// One provider second (the device flow's interval and expiry).
    pub time_unit: Duration,
    pub max_codes: u32,
    /// Point the OAuth clients elsewhere (a mock authorization server).
    pub rewrite_client: Option<Arc<dyn Fn(OAuthClient) -> OAuthClient + Send + Sync>>,
    pub loopback_timeout: Option<Duration>,
    pub paste_timeout: Option<Duration>,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            time_unit: Duration::from_secs(1),
            max_codes: MAX_UNUSED_CODES,
            rewrite_client: None,
            loopback_timeout: None,
            paste_timeout: None,
        }
    }
}

/// A finished sign-in's secrets, until they are written (and, for "Create it?",
/// after).
struct Held {
    backend: LibraryBackend,
    auth: Auth,
    cred: StoredCredential,
    store_id: String,
    account: AccountInfo,
    choice: LibraryChoice,
    /// A store this one replaces: its credentials go, its directory is marked.
    replaces: Vec<String>,
    /// The ran store of another account that "use this account's library instead"
    /// disconnects first.
    other_account: Option<String>,
    session: u64,
}

/// A pasted address and where to answer whether it was taken.
type PasteMsg = (String, tokio::sync::oneshot::Sender<Result<(), String>>);

/// The paste flow's mailbox while it waits for the address.
struct PasteSlot {
    session: u64,
    tx: tokio::sync::mpsc::Sender<PasteMsg>,
}

struct Inner {
    cloud_dir: PathBuf,
    config: ConfigFn,
    apply: ApplyFn,
    release_old: Mutex<Option<ReleaseOldFn>>,
    ops: Arc<dyn ProviderOps>,
    tuning: Tuning,
    status: Mutex<ConnectStatus>,
    changed: tokio::sync::watch::Sender<u64>,
    session: AtomicU64,
    task: Mutex<Option<tokio::task::AbortHandle>>,
    held: Mutex<Option<Held>>,
    paste: Mutex<Option<PasteSlot>>,
}

/// The one sign-in of this process (see the module docs).
#[derive(Clone)]
pub struct Connector(Arc<Inner>);

impl fmt::Debug for Connector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connector")
            .field("cloud_dir", &self.0.cloud_dir)
            .field("status", &*self.0.status.lock())
            .finish()
    }
}

/// The file marking a disconnected store directory for removal at the next start.
pub const DISCONNECTED_MARK: &str = ".disconnected";

/// A store directory that ran here (it has durable state and was not disconnected).
// unmounted: the store's own local directory.
pub fn store_ran(cloud_dir: &Path, id: &str) -> bool {
    let dir = cloud_dir.join(id);
    bunko_store::fs::is_file(dir.join("state.sqlite"))
        && !bunko_store::fs::exists(dir.join(DISCONNECTED_MARK))
}

/// Mark a store directory for removal at the next start (§7 Disconnect step 3; the
/// running store keeps it open until then). Nothing happens when it does not exist.
/// Refused for an id that is not a store id, or a path that is not a plain store
/// directory under `cloud_dir` (`disconnect::store_dir_checked`).
pub fn mark_disconnected(cloud_dir: &Path, id: &str) -> std::io::Result<()> {
    if !super::is_store_id(id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{id:?} is not a store id"),
        ));
    }
    if !bunko_store::fs::is_dir(cloud_dir.join(id)) {
        return Ok(());
    }
    let dir = super::disconnect::store_dir_checked(cloud_dir, id)?;
    bunko_store::fs::write(dir.join(DISCONNECTED_MARK), b"disconnected\n")
}

/// The instance ids of this machine's stores (their markers are "ours").
fn our_instances(cloud_dir: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(rd) = bunko_store::fs::read_dir(cloud_dir) else {
        return out;
    };
    for e in rd.flatten() {
        let state = e.path().join("state.sqlite");
        if !bunko_store::fs::is_file(&state) {
            continue;
        }
        if let Ok(db) = bunko_remote::state::StateDb::open(&state)
            && let Ok(id) = db.instance_id()
        {
            out.insert(id);
        }
    }
    out
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn hhmm(t: OffsetDateTime) -> String {
    format!("{:02}:{:02}", t.hour(), t.minute())
}

impl Connector {
    pub fn new(cloud_dir: PathBuf, config: ConfigFn, apply: ApplyFn) -> Connector {
        Self::with_ops(
            cloud_dir,
            config,
            apply,
            Arc::new(LiveOps),
            Tuning::default(),
        )
    }

    pub fn with_ops(
        cloud_dir: PathBuf,
        config: ConfigFn,
        apply: ApplyFn,
        ops: Arc<dyn ProviderOps>,
        tuning: Tuning,
    ) -> Connector {
        let max = tuning.max_codes;
        Connector(Arc::new(Inner {
            cloud_dir,
            config,
            apply,
            release_old: Mutex::new(None),
            ops,
            tuning,
            status: Mutex::new(ConnectStatus::idle(max)),
            changed: tokio::sync::watch::channel(0).0,
            session: AtomicU64::new(0),
            task: Mutex::new(None),
            held: Mutex::new(None),
            paste: Mutex::new(None),
        }))
    }

    /// The hook "use this account's library instead" disconnects the old store with.
    pub fn set_release_old(&self, f: ReleaseOldFn) {
        *self.0.release_old.lock() = Some(f);
    }

    pub fn cloud_dir(&self) -> &Path {
        &self.0.cloud_dir
    }

    /// The current sign-in's state.
    pub fn status(&self) -> ConnectStatus {
        self.0.status.lock().clone()
    }

    /// Wait until the state satisfies `pred` (or `timeout`); the state then.
    pub async fn wait_for(
        &self,
        mut pred: impl FnMut(&ConnectStatus) -> bool,
        timeout: Duration,
    ) -> ConnectStatus {
        let mut rx = self.0.changed.subscribe();
        let end = tokio::time::Instant::now() + timeout;
        loop {
            let s = self.status();
            if pred(&s) {
                return s;
            }
            match tokio::time::timeout_at(end, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => return self.status(),
            }
        }
    }

    fn set(&self, session: u64, f: impl FnOnce(&mut ConnectStatus)) {
        {
            let mut s = self.0.status.lock();
            if s.session != session {
                return;
            }
            f(&mut s);
        }
        self.0.changed.send_modify(|v| *v += 1);
    }

    /// Stop the sign-in in progress (its state becomes `cancelled`).
    pub fn cancel(&self) {
        if let Some(t) = self.0.task.lock().take() {
            t.abort();
        }
        let session = self.0.session.load(Ordering::SeqCst);
        self.0.held.lock().take();
        self.0.paste.lock().take();
        self.set(session, |s| {
            if s.in_progress() || s.state == "account-mismatch" {
                s.state = "cancelled";
                s.auth_url = None;
                s.redirect_uri = None;
                s.user_code = None;
                s.verification_url = None;
                s.expires_at = None;
                s.message = Some("The sign-in was cancelled.".into());
            }
        });
    }

    /// The `paste` state's answer: the address the person's browser ended on after
    /// signing in. `Ok`: taken (the sign-in goes on to `checking`) or the provider's
    /// refusal ended it (`failed`); `Err`: not this sign-in's redirect (another
    /// state, another address, no code), so the sign-in waits for another paste. The
    /// address is never logged or kept: only its code is redeemed.
    pub async fn paste(&self, pasted: String) -> Result<ConnectStatus, String> {
        const NOT_WAITING: &str =
            "No sign-in is waiting for a pasted address: click Connect to start one.";
        let session = self.0.session.load(Ordering::SeqCst);
        let tx = match &*self.0.paste.lock() {
            Some(slot) if slot.session == session => slot.tx.clone(),
            _ => return Err(NOT_WAITING.into()),
        };
        let (reply, answer) = tokio::sync::oneshot::channel();
        tx.send((pasted, reply))
            .await
            .map_err(|_| NOT_WAITING.to_string())?;
        match answer.await {
            Ok(Ok(())) => Ok(self.status()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(NOT_WAITING.into()),
        }
    }

    fn clear_paste(&self, session: u64) {
        let mut slot = self.0.paste.lock();
        if slot.as_ref().is_some_and(|s| s.session == session) {
            *slot = None;
        }
    }

    /// Start a sign-in (replacing one in progress). Must run inside a tokio runtime.
    /// A request that cannot start (an unknown provider, an empty form) is refused
    /// here, with nothing changed.
    pub fn start(
        &self,
        req: ConnectRequest,
        origin: RequestOrigin,
    ) -> Result<ConnectStatus, String> {
        let backend = LibraryBackend::parse(&req.provider)
            .filter(|b| b.is_cloud())
            .ok_or_else(|| {
                format!(
                    "unknown provider {:?}: gdrive, onedrive, mega or webdav",
                    req.provider
                )
            })?;
        let lib = (self.0.config)();
        let folder = req
            .folder
            .clone()
            .map(|f| f.trim().trim_matches('/').to_string())
            .filter(|f| !f.is_empty())
            .unwrap_or_else(|| lib.folder.clone());
        if folder.contains('/') || folder.contains('\\') {
            return Err("the cloud folder is one folder name, without slashes".into());
        }
        let flow = match backend {
            LibraryBackend::WebDav => {
                let w = req
                    .webdav
                    .as_ref()
                    .ok_or("the WebDAV server's URL, user and app password are needed")?;
                if w.url.trim().is_empty() || w.username.trim().is_empty() || w.password.is_empty()
                {
                    return Err("the WebDAV server's URL, user and app password are needed".into());
                }
                "form"
            }
            LibraryBackend::Mega => {
                let m = req
                    .mega
                    .as_ref()
                    .ok_or("the MEGA email and password are needed")?;
                if m.email.trim().is_empty() || m.password.is_empty() {
                    return Err("the MEGA email and password are needed".into());
                }
                "form"
            }
            LibraryBackend::GDrive | LibraryBackend::OneDrive => {
                if let Some(r) = req.reader.as_deref()
                    && backend == LibraryBackend::GDrive
                    && !matches!(r, "release" | "beta")
                {
                    return Err(format!("--reader {r:?}: release or beta"));
                }
                if let Some(f) = req.flow.as_deref()
                    && Flow::parse(f).is_none()
                {
                    return Err(format!("flow {f:?}: loopback, device or paste"));
                }
                ""
            }
            _ => "",
        };
        // The pairing a Drive sign-in uses: overrides make it custom (no question).
        if let Some(t) = self.0.task.lock().take() {
            t.abort();
        }
        self.0.held.lock().take();
        self.0.paste.lock().take();
        let session = self.0.session.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut s = self.0.status.lock();
            *s = ConnectStatus::idle(self.0.tuning.max_codes);
            s.session = session;
            s.state = "starting";
            s.provider = Some(backend.as_str());
            s.flow = (!flow.is_empty()).then_some(flow);
        }
        self.0.changed.send_modify(|v| *v += 1);
        let me = self.clone();
        let task = tokio::spawn(async move {
            me.run(session, backend, folder, lib, req, origin).await;
        });
        *self.0.task.lock() = Some(task.abort_handle());
        Ok(self.status())
    }

    async fn run(
        &self,
        session: u64,
        backend: LibraryBackend,
        folder: String,
        lib: LibraryConfig,
        req: ConnectRequest,
        origin: RequestOrigin,
    ) {
        let use_this_account = req.use_this_account;
        let outcome = match backend {
            LibraryBackend::GDrive | LibraryBackend::OneDrive => {
                self.oauth(session, backend, &folder, &lib, &req, origin)
                    .await
            }
            LibraryBackend::Mega => self.mega(session, &folder, req).await,
            _ => self.webdav(session, &folder, req).await,
        };
        let held = match outcome {
            Ok(Some(h)) => h,
            Ok(None) => return, // stopped: its state says why
            Err(e) => {
                tracing::warn!(
                    "[STORAGE] Connecting {} failed: {e}",
                    provider_name(backend)
                );
                self.set(session, |s| {
                    s.state = "failed";
                    s.auth_url = None;
                    s.redirect_uri = None;
                    s.user_code = None;
                    s.verification_url = None;
                    s.expires_at = None;
                    s.error = Some(e);
                });
                return;
            }
        };
        self.conclude(held, use_this_account).await;
    }

    /// Apply the replacement rules, then persist (or wait for the account decision).
    async fn conclude(&self, mut held: Held, use_this_account: bool) {
        let session = held.session;
        match self.plan_replacement(&mut held) {
            Err(e) => {
                self.set(session, |s| {
                    s.state = "failed";
                    s.error = Some(e);
                    s.account = Some(held.account.label.clone());
                });
            }
            Ok(Some(previous)) if !use_this_account => {
                self.set(session, |s| {
                    s.state = "account-mismatch";
                    s.account = Some(held.account.label.clone());
                    s.previous_account = Some(previous.clone());
                    s.message = Some(format!(
                        "This Bunko's {} library belongs to {previous}, not {}. Sign in with that account, or choose \"Use this account's library instead\".",
                        provider_name(held.backend),
                        held.account.label
                    ));
                });
                *self.0.held.lock() = Some(held);
            }
            Ok(_) => {
                let r = self.persist(held).await;
                if let Err(e) = r {
                    self.set(session, |s| {
                        s.state = "failed";
                        s.error = Some(e);
                    });
                }
            }
        }
    }

    /// The account decision of `account-mismatch`: `true` = "use this account's
    /// library instead" (the old store is disconnected first), `false` = cancel.
    pub async fn decide_account(&self, use_this_account: bool) -> Result<ConnectStatus, String> {
        if !use_this_account {
            self.cancel();
            return Ok(self.status());
        }
        let Some(held) = self.0.held.lock().take() else {
            return Err("no sign-in is waiting for that choice".into());
        };
        let session = held.session;
        if let Err(e) = self.persist(held).await {
            self.set(session, |s| {
                s.state = "failed";
                s.error = Some(e.clone());
            });
            return Err(e);
        }
        Ok(self.status())
    }

    /// Which stores the new one replaces. `Ok(Some(label))`: a store of another
    /// account ran here (the account decision is needed).
    fn plan_replacement(&self, held: &mut Held) -> Result<Option<String>, String> {
        let creds = CredentialStore::open(&self.0.cloud_dir).map_err(|e| e.to_string())?;
        let ids = creds.store_ids().map_err(|e| e.to_string())?;
        let mut previous = None;
        for id in ids {
            if id == held.store_id {
                continue;
            }
            let Ok(Some(c)) = creds.get(&id) else {
                continue;
            };
            if !store_ran(&self.0.cloud_dir, &id) {
                held.replaces.push(id);
                continue;
            }
            let same_provider = c.provider == held.backend.as_str();
            let other_account = c.account_id.as_deref() != Some(held.account.id.as_str());
            let old_name = LibraryBackend::parse(&c.provider)
                .map(provider_name)
                .unwrap_or("cloud");
            if same_provider && other_account {
                previous = Some(
                    c.account_label
                        .clone()
                        .or(c.account_id.clone())
                        .unwrap_or_else(|| "another account".into()),
                );
                held.other_account = Some(id);
                continue;
            }
            let why = if same_provider && held.backend == LibraryBackend::GDrive {
                let old = c
                    .client_id
                    .as_deref()
                    .and_then(pairing_of_client)
                    .map(|p| pairing_label(p.as_str()))
                    .unwrap_or_else(|| "another".into());
                format!(
                    "This Bunko's Google Drive library uses the {old} pairing: a pairing change is a different library. Disconnect it first (admin → Storage → Disconnect, or `mokuro-bunko library disconnect`), then connect with the new pairing."
                )
            } else {
                format!(
                    "This Bunko already manages a {old_name} library: this is a different library. Disconnect it first (admin → Storage → Disconnect, or `mokuro-bunko library disconnect`)."
                )
            };
            return Err(why);
        }
        Ok(previous)
    }

    /// Write the credentials and the configuration; the state becomes `connected`.
    async fn persist(&self, mut held: Held) -> Result<(), String> {
        let session = held.session;
        if let Some(old) = held.other_account.take() {
            let hook = self.0.release_old.lock().clone();
            match hook {
                Some(f) => {
                    let o = old.clone();
                    tokio::task::spawn_blocking(move || f(&o))
                        .await
                        .map_err(|e| e.to_string())??;
                }
                None => {
                    return Err("the library of the other account still has its local state here: disconnect it first (`mokuro-bunko library disconnect`)".into());
                }
            }
            held.replaces.push(old);
        }
        let creds = CredentialStore::open(&self.0.cloud_dir).map_err(|e| e.to_string())?;
        creds
            .put(&held.store_id, held.cred.clone())
            .map_err(|e| e.to_string())?;
        for old in &held.replaces {
            let _ = creds.remove(old);
            let _ = mark_disconnected(&self.0.cloud_dir, old);
        }
        let apply = self.0.apply.clone();
        let choice = held.choice.clone();
        tokio::task::spawn_blocking(move || apply(&choice))
            .await
            .map_err(|e| e.to_string())??;
        tracing::info!(
            "[STORAGE] Connected {} as {} (store {}); the library opens when the server restarts",
            provider_name(held.backend),
            held.account.label,
            held.store_id
        );
        let (label, id) = (held.account.label.clone(), held.store_id.clone());
        self.set(session, |s| {
            s.state = "connected";
            s.auth_url = None;
            s.redirect_uri = None;
            s.user_code = None;
            s.verification_url = None;
            s.expires_at = None;
            s.account = Some(label);
            s.store_id = Some(id);
            s.needs_restart = true;
            s.message = None;
        });
        *self.0.held.lock() = Some(held);
        Ok(())
    }

    /// "Create it?": make the cloud folder with the sign-in just made, and Check again.
    pub async fn create_folder(&self) -> Result<CheckReport, String> {
        let (backend, auth, folder, session, pairing) = {
            let h = self.0.held.lock();
            let h = h.as_ref().ok_or("connect first")?;
            (
                h.backend,
                h.auth.clone(),
                h.choice.folder.clone(),
                h.session,
                self.status().pairing,
            )
        };
        let ours = our_instances(&self.0.cloud_dir);
        let scratch = self.scratch_dir();
        let (_, mut check) = self
            .0
            .ops
            .inspect(backend, &auth, &folder, true, &scratch, &ours)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(p) = pairing.as_deref() {
            check.pairing_advice(p);
        }
        let c = check.clone();
        self.set(session, |s| s.check = Some(c));
        Ok(check)
    }

    // unmounted: scratch space for a driver's probe, in `<base>/cloud/`.
    fn scratch_dir(&self) -> PathBuf {
        let d = self.0.cloud_dir.join(".connect");
        let _ = bunko_store::fs::create_dir_all(&d);
        d
    }

    async fn inspect(
        &self,
        session: u64,
        backend: LibraryBackend,
        auth: &Auth,
        folder: &str,
    ) -> Result<(AccountInfo, CheckReport), String> {
        self.set(session, |s| {
            s.state = "checking";
            s.auth_url = None;
            s.redirect_uri = None;
            s.user_code = None;
            s.verification_url = None;
            s.expires_at = None;
            s.message = Some(format!("Checking {}…", provider_name(backend)));
        });
        let ours = our_instances(&self.0.cloud_dir);
        let scratch = self.scratch_dir();
        self.0
            .ops
            .inspect(backend, auth, folder, false, &scratch, &ours)
            .await
            .map_err(|e| match e {
                BErr::AuthExpired | BErr::AuthRevoked => format!(
                    "{} refused the sign-in: check the account and try again",
                    provider_name(backend)
                ),
                other => format!("{}: {other}", provider_name(backend)),
            })
    }

    fn checked(
        &self,
        session: u64,
        account: &AccountInfo,
        check: &CheckReport,
        pairing: Option<String>,
    ) {
        let (a, c) = (account.label.clone(), check.clone());
        self.set(session, |s| {
            s.account = Some(a);
            s.check = Some(c);
            s.pairing = pairing;
            s.message = None;
        });
    }

    // ---- OAuth -----------------------------------------------------------------

    async fn oauth(
        &self,
        session: u64,
        backend: LibraryBackend,
        folder: &str,
        lib: &LibraryConfig,
        req: &ConnectRequest,
        origin: RequestOrigin,
    ) -> Result<Option<Held>, String> {
        let rewrite = |c: OAuthClient| match &self.0.tuning.rewrite_client {
            Some(f) => f(c),
            None => c,
        };
        let (loopback, device, project) = if backend == LibraryBackend::GDrive {
            let project = if lib.oauth.google.any() || lib.google_project == GoogleProject::Custom {
                GoogleProject::Custom
            } else {
                req.reader
                    .as_deref()
                    .and_then(GoogleProject::parse)
                    .unwrap_or(lib.google_project)
            };
            let c = google_clients(project, &lib.oauth.google);
            (
                c.loopback.map(rewrite),
                c.device.map(rewrite),
                Some(project),
            )
        } else {
            let c = rewrite(microsoft_client(&lib.oauth.microsoft));
            (Some(c.clone()), Some(c), None)
        };
        let pairing = match (backend, project) {
            (LibraryBackend::GDrive, Some(p)) => {
                let mut l = lib.clone();
                l.backend = LibraryBackend::GDrive;
                l.google_project = p;
                super::pairing_of(&l)
            }
            _ => String::new(),
        };
        let provider = if backend == LibraryBackend::GDrive {
            Provider::Google
        } else {
            Provider::Microsoft
        };
        let asked = req.flow.as_deref().and_then(Flow::parse);
        let browser_here = origin.browser_here();
        let mut flow = match asked {
            // The CLI shares the machine with the listener (it opens the browser).
            Some(Flow::Loopback) if browser_here || origin.cli => Flow::Loopback,
            Some(Flow::Loopback) => {
                return Err("The browser sign-in needs a browser on the server's computer: sign in on any device and paste the address back instead.".into());
            }
            Some(f) => f,
            None => choose_flow(provider, browser_here, loopback.is_some()),
        };
        if flow == Flow::Device && device.is_none() {
            if loopback.is_some() && asked != Some(Flow::Device) {
                flow = if browser_here {
                    Flow::Loopback
                } else {
                    Flow::Paste
                };
            } else {
                return Err("the Google override has no device client (storage.library.oauth.google.device_client_id and device_client_secret): sign in through the browser, or add one".into());
            }
        }
        if flow != Flow::Device && loopback.is_none() {
            if asked.is_none() && device.is_some() {
                flow = Flow::Device;
            } else {
                return Err("the Google override has no Desktop client (storage.library.oauth.google.client_id and client_secret): use a sign-in code, or add one".into());
            }
        }
        // What a failed device sign-in of a personal Microsoft account should use.
        let instead = if browser_here || origin.cli {
            Flow::Loopback
        } else {
            Flow::Paste
        };
        let name = provider_name(backend);
        let (tokens, client) = match flow {
            Flow::Loopback => {
                let client = loopback.expect("checked above");
                let (url, mut pending) = LoopbackFlow::start(&client)
                    .await
                    .map_err(|e| e.to_string())?;
                let timeout = self
                    .0
                    .tuning
                    .loopback_timeout
                    .unwrap_or(bunko_remote::auth::oauth::LOOPBACK_TIMEOUT);
                pending.set_timeout(timeout);
                let until = OffsetDateTime::now_utc() + timeout;
                tracing::info!(
                    "[STORAGE] Sign in to {name} in the browser (waiting until {} UTC)",
                    hhmm(until)
                );
                self.set(session, |s| {
                    s.state = "browser";
                    s.flow = Some("loopback");
                    s.auth_url = Some(url);
                    s.expires_at = Some(rfc3339(until));
                    s.pairing = (!pairing.is_empty()).then(|| pairing.clone());
                    s.message = Some(format!(
                        "Sign in to {name} in the browser tab that opened, then come back here."
                    ));
                });
                let t = pending.wait().await.map_err(|e| sign_in_error(name, e))?;
                (t, client)
            }
            Flow::Paste => {
                let client = loopback.expect("checked above");
                let t = self.pasted(session, name, &client, &pairing).await?;
                (t, client)
            }
            Flow::Device => {
                let client = device.expect("checked above");
                match self
                    .device_codes(session, name, &client, &pairing, instead)
                    .await?
                {
                    Some(t) => (t, client),
                    None => return Ok(None),
                }
            }
        };
        let auth = Auth::OAuth {
            access_token: tokens.access_token.clone(),
        };
        let (account, mut check) = self.inspect(session, backend, &auth, folder).await?;
        if backend == LibraryBackend::GDrive {
            check.pairing_advice(&pairing);
        }
        self.checked(
            session,
            &account,
            &check,
            (!pairing.is_empty()).then(|| pairing.clone()),
        );
        let id = store_id(backend.as_str(), &pairing, &account.id, folder);
        let cred = StoredCredential {
            provider: backend.as_str().into(),
            refresh_token: tokens.refresh_token.clone(),
            access_token: Some(tokens.access_token.clone()),
            expires_at: Some(tokens.expires_at),
            account_id: Some(account.id.clone()),
            account_label: Some(account.label.clone()),
            client_id: Some(client.client_id.clone()),
            ..Default::default()
        };
        Ok(Some(Held {
            backend,
            auth,
            cred,
            store_id: id,
            account,
            choice: LibraryChoice {
                backend,
                folder: folder.to_string(),
                google_project: project.filter(|p| *p != GoogleProject::Custom),
                webdav_url: None,
                webdav_username: None,
                webdav_allow_http: false,
            },
            replaces: Vec::new(),
            other_account: None,
            session,
        }))
    }

    /// The paste flow: show the authorization URL, wait for the address the browser
    /// ends on ([`Connector::paste`]), redeem its code. Another sign-in's address, or
    /// not a redirect at all, is answered and waited past; the timeout ends it.
    async fn pasted(
        &self,
        session: u64,
        name: &str,
        client: &OAuthClient,
        pairing: &str,
    ) -> Result<Tokens, String> {
        let (url, mut pending) = PasteFlow::start(client).map_err(|e| e.to_string())?;
        let timeout = self
            .0
            .tuning
            .paste_timeout
            .unwrap_or(bunko_remote::auth::oauth::PASTE_TIMEOUT);
        pending.set_timeout(timeout);
        let until = OffsetDateTime::now_utc() + timeout;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<PasteMsg>(4);
        *self.0.paste.lock() = Some(PasteSlot { session, tx });
        // Neither the URL (its state) nor, later, the pasted address is logged.
        tracing::info!(
            "[STORAGE] Sign in to {name} on any device, then paste the address the browser ends on (waiting until {} UTC)",
            hhmm(until)
        );
        let redirect = pending.redirect_uri().to_string();
        self.set(session, |s| {
            s.state = "paste";
            s.flow = Some("paste");
            s.auth_url = Some(url);
            s.redirect_uri = Some(redirect.clone());
            s.expires_at = Some(rfc3339(until));
            s.pairing = (!pairing.is_empty()).then(|| pairing.to_string());
            s.message = Some(format!(
                "Open the sign-in page on any device and sign in to {name}. The browser then ends on a page that does not load ({redirect}?code=…): copy that page's whole address and paste it here."
            ));
        });
        let deadline = tokio::time::Instant::now() + timeout;
        let code = loop {
            let (pasted, reply) = match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(m)) => m,
                Err(_) | Ok(None) => {
                    self.clear_paste(session);
                    return Err(sign_in_error(name, AuthError::Timeout));
                }
            };
            match pending.code_from(&pasted) {
                Ok(code) => {
                    self.clear_paste(session);
                    self.set(session, |s| {
                        s.state = "checking";
                        s.auth_url = None;
                        s.redirect_uri = None;
                        s.expires_at = None;
                        s.message = Some(format!("Signing in to {name}…"));
                    });
                    let _ = reply.send(Ok(()));
                    break code;
                }
                Err(bunko_remote::auth::PasteError::Ended(e)) => {
                    self.clear_paste(session);
                    let msg = sign_in_error(name, e);
                    self.set(session, |s| {
                        s.state = "failed";
                        s.auth_url = None;
                        s.redirect_uri = None;
                        s.expires_at = None;
                        s.error = Some(msg.clone());
                    });
                    let _ = reply.send(Ok(()));
                    return Err(msg);
                }
                Err(e) => {
                    let _ = reply.send(Err(format!("Not taken: {e}.")));
                }
            }
        };
        pending
            .redeem(code)
            .await
            .map_err(|e| sign_in_error(name, e))
    }

    /// Device codes, one after another: an expired code is replaced at once; after
    /// `max_codes` unused ones the sign-in stops (`Ok(None)`). A personal Microsoft
    /// account (AADSTS70002) fails with `suggest_flow` = `instead`.
    async fn device_codes(
        &self,
        session: u64,
        name: &str,
        client: &OAuthClient,
        pairing: &str,
        instead: Flow,
    ) -> Result<Option<Tokens>, String> {
        let max = self.0.tuning.max_codes;
        let no_code = |e: AuthError| {
            if matches!(e, AuthError::PersonalAccountNoCode) {
                self.set(session, |s| s.suggest_flow = Some(instead.as_str()));
            }
            e
        };
        for n in 1..=max {
            let mut flow =
                DeviceFlow::start(client)
                    .await
                    .map_err(no_code)
                    .map_err(|e| match e {
                        AuthError::PersonalAccountNoCode => sign_in_error(name, e),
                        e => format!("{name} gave no sign-in code: {e}"),
                    })?;
            let unit = self.0.tuning.time_unit;
            if unit != Duration::from_secs(1) {
                flow.set_time_unit(unit);
            }
            let code = flow.code().clone();
            let lifetime =
                Duration::from_secs_f64(code.expires_in.as_secs_f64() * unit.as_secs_f64());
            let until = OffsetDateTime::now_utc() + lifetime;
            tracing::info!(
                "[STORAGE] Sign in to {name}: open {} and enter the code {} (code {n} of {max}; it expires at {} UTC)",
                code.verification_url,
                code.user_code,
                hhmm(until)
            );
            self.set(session, |s| {
                s.state = "code";
                s.flow = Some("device");
                s.user_code = Some(code.user_code.clone());
                s.verification_url = Some(code.verification_url.clone());
                s.expires_at = Some(rfc3339(until));
                s.code_number = Some(n);
                s.pairing = (!pairing.is_empty()).then(|| pairing.to_string());
                s.message = Some(format!(
                    "Open {} on any device and enter {}.",
                    code.verification_url, code.user_code
                ));
            });
            match flow.poll().await {
                Ok(t) => return Ok(Some(t)),
                Err(AuthError::Expired) => {
                    tracing::info!(
                        "[STORAGE] The {name} sign-in code {} expired unused",
                        code.user_code
                    );
                    continue;
                }
                Err(e) => return Err(sign_in_error(name, no_code(e))),
            }
        }
        tracing::warn!(
            "[STORAGE] {max} {name} sign-in codes went unused: stopped; click Connect for a new one"
        );
        self.set(session, |s| {
            s.state = "stopped";
            s.user_code = None;
            s.verification_url = None;
            s.expires_at = None;
            s.message = Some(format!(
                "{max} sign-in codes went unused, so Bunko stopped asking {name} for more. Click Connect for a new code."
            ));
        });
        Ok(None)
    }

    // ---- MEGA and WebDAV ----------------------------------------------------------

    async fn mega(
        &self,
        session: u64,
        folder: &str,
        req: ConnectRequest,
    ) -> Result<Option<Held>, String> {
        let form = req.mega.unwrap_or_default();
        self.set(session, |s| {
            s.state = "checking";
            s.message = Some("Signing in to MEGA…".into());
        });
        let totp = form
            .totp
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty());
        let blob = self
            .0
            .ops
            .mega_login(form.email.trim(), &form.password, totp)
            .await
            .map_err(|e| match e {
                LoginError::MfaRequired => {
                    "This MEGA account has two-factor authentication: enter its current code"
                        .to_string()
                }
                LoginError::BadCredentials => "MEGA refused the email or password".to_string(),
                other => format!("MEGA: {other}"),
            })?;
        drop(form);
        let auth = Auth::Mega(blob.clone());
        let (account, check) = self
            .inspect(session, LibraryBackend::Mega, &auth, folder)
            .await?;
        self.checked(session, &account, &check, None);
        let id = store_id("mega", "", &account.id, folder);
        let cred = StoredCredential {
            provider: "mega".into(),
            account_id: Some(account.id.clone()),
            account_label: Some(account.label.clone()),
            mega_session: Some(blob.expose().to_string()),
            ..Default::default()
        };
        Ok(Some(Held {
            backend: LibraryBackend::Mega,
            auth,
            cred,
            store_id: id,
            account,
            choice: LibraryChoice {
                backend: LibraryBackend::Mega,
                folder: folder.to_string(),
                google_project: None,
                webdav_url: None,
                webdav_username: None,
                webdav_allow_http: false,
            },
            replaces: Vec::new(),
            other_account: None,
            session,
        }))
    }

    async fn webdav(
        &self,
        session: u64,
        folder: &str,
        req: ConnectRequest,
    ) -> Result<Option<Held>, String> {
        let form = req.webdav.unwrap_or_default();
        let url = form.url.trim().to_string();
        let user = form.username.trim().to_string();
        // A server on this machine (localhost, 127.0.0.0/8, ::1) is exempt: its http
        // never leaves the host.
        let loopback = bunko_core::config::is_loopback_http_url(&url);
        if url.to_ascii_lowercase().starts_with("http://") && !form.allow_http && !loopback {
            return Err("http:// sends the password unencrypted: use https://, or tick \"Allow http\" for a server on your own network (a server on this computer, localhost or 127.0.0.1, needs neither)".into());
        }
        let auth = Auth::WebDav {
            url: url.clone(),
            username: user.clone(),
            password: form.password.clone(),
            allow_http: form.allow_http,
        };
        let (_, check) = self
            .inspect(session, LibraryBackend::WebDav, &auth, folder)
            .await?;
        let account = AccountInfo {
            id: format!("{user}@{url}"),
            label: format!("{user}@{url}"),
        };
        self.checked(session, &account, &check, None);
        let id = store_id("webdav", "", &account.id, folder);
        let cred = StoredCredential {
            provider: "webdav".into(),
            account_id: Some(account.id.clone()),
            account_label: Some(account.label.clone()),
            webdav_password: Some(form.password),
            ..Default::default()
        };
        Ok(Some(Held {
            backend: LibraryBackend::WebDav,
            auth,
            cred,
            store_id: id,
            account,
            choice: LibraryChoice {
                backend: LibraryBackend::WebDav,
                folder: folder.to_string(),
                google_project: None,
                webdav_url: Some(url),
                webdav_username: Some(user),
                webdav_allow_http: form.allow_http,
            },
            replaces: Vec::new(),
            other_account: None,
            session,
        }))
    }
}

fn sign_in_error(name: &str, e: AuthError) -> String {
    match e {
        AuthError::Denied => format!("The {name} sign-in was refused or cancelled."),
        AuthError::Timeout => format!("The {name} sign-in timed out. Start it again to sign in."),
        AuthError::Expired => {
            format!("The {name} sign-in code expired: click Connect for a new one.")
        }
        e @ AuthError::PersonalAccountNoCode => e.to_string(),
        other => format!("{name} sign-in failed: {other}"),
    }
}

#[cfg(test)]
mod tests;
