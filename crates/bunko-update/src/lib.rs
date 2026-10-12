//! Release checks and one-click self-update.
//!
//! Each GitHub release publishes `release.json` (a [`Manifest`]) and `release.json.sig`
//! (base64 ed25519 signature over the manifest's exact bytes). The public key is
//! compiled in ([`RELEASE_PUBLIC_KEY`]), so a compromised download host cannot push a
//! binary: the archive's sha256 comes from the signed manifest.
//!
//! How an update is applied depends on how bunko was installed ([`InstallKind`]):
//! * self-managed binary (tarball/zip/portable): download, verify, swap the executable
//!   in place (`self-replace`) and restart;
//! * Docker: only report the image tag to pull;
//! * a system package or app store: only report.

pub mod auto;
pub mod backend;
pub mod layout;

use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

/// ed25519 public key (base64, 32 bytes) of the release signing key. Forks override it
/// at build time with `BUNKO_RELEASE_PUBLIC_KEY`.
pub const RELEASE_PUBLIC_KEY: &str = match option_env!("BUNKO_RELEASE_PUBLIC_KEY") {
    Some(k) => k,
    None => "bvEsRQhVlCH124jKyBWh5TAl7tl5SXHXhdFGJCs3HEk=",
};

/// The Rust target triple this binary was built for (set by build.rs).
pub const TARGET: &str = env!("BUNKO_TARGET");

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("could not reach the release server: {0}")]
    Http(#[from] reqwest::Error),
    /// The release server answered 404: nothing is published there (yet).
    #[error("no release has been published yet ({0} does not exist)")]
    NotPublished(String),
    #[error("the release manifest is not valid: {0}")]
    Manifest(String),
    #[error("the release manifest signature does not verify")]
    BadSignature,
    #[error("no {flavor} build for {target} in release {version}")]
    NoArtifact {
        flavor: String,
        target: String,
        version: String,
    },
    #[error("the download's sha256 is {got}, the signed manifest says {want}")]
    Checksum { got: String, want: String },
    #[error("this installation is managed by {0}; update it there")]
    Managed(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("could not unpack the update: {0}")]
    Unpack(String),
    #[error("{0}")]
    NoSpace(String),
}

impl UpdateError {
    /// Only the owner can fix it: retrying will not help (a bad signature, a managed
    /// install, no room on the disk, no build for this platform).
    pub fn needs_owner(&self) -> bool {
        matches!(
            self,
            UpdateError::BadSignature
                | UpdateError::Managed(_)
                | UpdateError::NoSpace(_)
                | UpdateError::NoArtifact { .. }
        )
    }
}

/// A verified new release unpacked next to the running executable ([`Updater::stage`]).
#[derive(Debug)]
pub struct Staged {
    pub version: String,
    /// The new executable (runnable: prefetch steps run it before it is installed).
    pub binary: PathBuf,
    unpacked: layout::Unpacked,
    /// The downloaded archive (removed once unpacked or discarded).
    archive: PathBuf,
}

impl Staged {
    /// Put the new release in place of the running one: the executable, the other
    /// copy of it in this install (Windows: the `mokuro-bunko.exe` /
    /// `mokuro-bunko-cli.exe` pair) and the macOS app bundle ([`layout::Plan`]), which
    /// is sealed again once the staging folder is gone.
    pub fn commit(self) -> Result<String, UpdateError> {
        let plan = current_exe().map(|exe| layout::Plan::for_exe(&exe));
        let r = match &plan {
            Ok(p) => p.commit(&self.unpacked),
            Err(e) => Err(UpdateError::Io(std::io::Error::new(
                e.kind(),
                e.to_string(),
            ))),
        };
        self.unpacked.remove();
        let _ = std::fs::remove_file(&self.archive);
        if let Ok(p) = &plan {
            p.seal();
        }
        r.map(|()| self.version)
    }

    pub fn discard(self) {
        self.unpacked.remove();
        let _ = std::fs::remove_file(&self.archive);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub url: String,
    pub sha256: String,
    pub size: u64,
    /// File name of the executable inside the archive.
    #[serde(default = "default_binary")]
    pub binary: String,
}

fn default_binary() -> String {
    if cfg!(windows) {
        "mokuro-bunko.exe".into()
    } else {
        "mokuro-bunko".into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub published_at: String,
    #[serde(default)]
    pub notes_url: String,
    /// target triple → flavor (`full`, `lite`) → artifact.
    #[serde(default)]
    pub artifacts: BTreeMap<String, BTreeMap<String, Artifact>>,
    /// flavor → image reference, for Docker installs.
    #[serde(default)]
    pub docker: BTreeMap<String, String>,
    /// target triple → backend variant (`cpu`, `cu130`, `rocm7.1`) → OCR backend pack
    /// (`install-ocr`; [`backend`]). Absent in manifests before 0.7.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub backends: BTreeMap<String, BTreeMap<String, backend::BackendArtifact>>,
}

/// Read `loc`: an http(s) URL, a `file://` URL or a plain path (mirrors on disk, tests).
pub async fn read_location(client: &reqwest::Client, loc: &str) -> Result<Vec<u8>, UpdateError> {
    if loc.contains("://") && !loc.starts_with("file://") {
        let resp = client.get(loc).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(UpdateError::NotPublished(loc.to_string()));
        }
        Ok(resp.error_for_status()?.bytes().await?.to_vec())
    } else {
        let p = loc.strip_prefix("file://").unwrap_or(loc);
        std::fs::read(p)
            .map_err(|e| UpdateError::Io(std::io::Error::new(e.kind(), format!("{p}: {e}"))))
    }
}

/// Fetch and verify the signed manifest at `loc` (and `<loc>.sig`).
pub async fn fetch_signed(
    client: &reqwest::Client,
    loc: &str,
    public_key: &str,
) -> Result<Manifest, UpdateError> {
    let body = read_location(client, loc).await?;
    let sig = read_location(client, &format!("{loc}.sig")).await?;
    parse_manifest(&body, &String::from_utf8_lossy(&sig), public_key)
}

impl Manifest {
    pub fn semver(&self) -> Result<semver::Version, UpdateError> {
        semver::Version::parse(self.version.trim_start_matches('v'))
            .map_err(|e| UpdateError::Manifest(e.to_string()))
    }

    pub fn artifact(&self, target: &str, flavor: &str) -> Result<&Artifact, UpdateError> {
        self.artifacts
            .get(target)
            .and_then(|f| f.get(flavor))
            .ok_or_else(|| UpdateError::NoArtifact {
                flavor: flavor.into(),
                target: target.into(),
                version: self.version.clone(),
            })
    }
}

/// Verify `sig_b64` over `bytes` with the base64 public key.
pub fn verify_signature(
    bytes: &[u8],
    sig_b64: &str,
    public_key_b64: &str,
) -> Result<(), UpdateError> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let key: [u8; 32] = b64
        .decode(public_key_b64.trim())
        .ok()
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| UpdateError::Manifest("bad public key".into()))?;
    let key = VerifyingKey::from_bytes(&key)
        .map_err(|_| UpdateError::Manifest("bad public key".into()))?;
    let sig: [u8; 64] = b64
        .decode(sig_b64.trim())
        .ok()
        .and_then(|s| s.try_into().ok())
        .ok_or(UpdateError::BadSignature)?;
    key.verify_strict(bytes, &Signature::from_bytes(&sig))
        .map_err(|_| UpdateError::BadSignature)
}

/// Parse and verify a manifest.
pub fn parse_manifest(
    bytes: &[u8],
    sig_b64: &str,
    public_key_b64: &str,
) -> Result<Manifest, UpdateError> {
    verify_signature(bytes, sig_b64, public_key_b64)?;
    let m: Manifest =
        serde_json::from_slice(bytes).map_err(|e| UpdateError::Manifest(e.to_string()))?;
    m.semver()?;
    Ok(m)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InstallKind {
    /// We own our executable and may replace it.
    SelfManaged {
        exe: PathBuf,
    },
    Docker,
    /// Installed by a package manager / system location we must not touch.
    Managed {
        by: String,
    },
    /// Mobile app bundles update through their store.
    Mobile,
}

impl InstallKind {
    /// `MOKURO_INSTALL_KIND` (`self`, `docker`, or a manager name) wins; then Docker and
    /// mobile detection; then the executable's location.
    pub fn detect() -> InstallKind {
        match std::env::var("MOKURO_INSTALL_KIND").ok().as_deref() {
            Some("docker") => return InstallKind::Docker,
            Some("self") => {}
            Some(other) if !other.is_empty() => {
                return InstallKind::Managed {
                    by: other.to_string(),
                };
            }
            _ => {
                if Path::new("/.dockerenv").exists() {
                    return InstallKind::Docker;
                }
            }
        }
        if cfg!(any(target_os = "android", target_os = "ios")) {
            return InstallKind::Mobile;
        }
        match current_exe() {
            Ok(exe) => {
                let s = exe.to_string_lossy();
                if s.starts_with("/usr/bin")
                    || s.starts_with("/usr/sbin")
                    || s.contains("/Cellar/")
                    || s.starts_with("/nix/store")
                {
                    InstallKind::Managed {
                        by: "the system package manager".into(),
                    }
                } else {
                    InstallKind::SelfManaged { exe }
                }
            }
            Err(_) => InstallKind::Managed {
                by: "an unknown installer".into(),
            },
        }
    }

    pub fn can_apply(&self) -> bool {
        matches!(self, InstallKind::SelfManaged { .. })
    }
}

/// The result of a check, as shown in the admin panel.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub current: String,
    pub latest: Option<String>,
    pub available: bool,
    pub notes_url: Option<String>,
    pub install: InstallKind,
    pub can_apply: bool,
    /// For Docker installs: the image to pull.
    pub docker_image: Option<String>,
    pub checked_at: Option<String>,
    pub error: Option<String>,
    /// Not a failure, but worth saying: e.g. no release has been published yet.
    pub note: Option<String>,
}

pub struct Updater {
    client: reqwest::Client,
    manifest_url: String,
    public_key: String,
    channel: String,
    flavor: String,
}

/// The channel an `update.channel` setting means for a build of `version`: `auto` (or
/// empty) follows the build, so a pre-release build is on `prerelease` and a stable build
/// on `stable`; an explicit `stable` or `prerelease` is taken as written.
pub fn resolve_channel(setting: &str, version: &str) -> &'static str {
    match setting.trim() {
        "stable" => "stable",
        "prerelease" => "prerelease",
        _ => match semver::Version::parse(version.trim_start_matches('v')) {
            Ok(v) if !v.pre.is_empty() => "prerelease",
            _ => "stable",
        },
    }
}

impl Updater {
    /// `flavor` is `full` or `lite`; `channel` is `stable`, `prerelease` or `auto`
    /// (see [`resolve_channel`]: this build's own channel).
    pub fn new(
        manifest_url: impl Into<String>,
        channel: impl Into<String>,
        flavor: impl Into<String>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(format!("mokuro-bunko/{}", bunko_core::VERSION))
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self {
            client,
            manifest_url: manifest_url.into(),
            public_key: RELEASE_PUBLIC_KEY.into(),
            channel: resolve_channel(&channel.into(), bunko_core::VERSION).into(),
            flavor: flavor.into(),
        }
    }

    /// The channel this updater follows (`stable` or `prerelease`), `auto` resolved.
    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn with_public_key(mut self, key: impl Into<String>) -> Self {
        self.public_key = key.into();
        self
    }

    pub async fn fetch_manifest(&self) -> Result<Manifest, UpdateError> {
        let url = self.manifest_location().await?;
        fetch_signed(&self.client, &url, &self.public_key).await
    }

    /// The signed manifest of exactly `version` ([`auto::release_manifest_url`]); an
    /// error when it names another version.
    pub async fn fetch_version(&self, version: &str) -> Result<Manifest, UpdateError> {
        let url = auto::release_manifest_url(&self.manifest_url, version);
        let m = fetch_signed(&self.client, &url, &self.public_key).await?;
        if m.version.trim_start_matches('v') != version.trim_start_matches('v') {
            return Err(UpdateError::Manifest(format!(
                "{url} is release {}, not {version}",
                m.version
            )));
        }
        Ok(m)
    }

    pub fn manifest_url(&self) -> &str {
        &self.manifest_url
    }

    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    pub fn flavor(&self) -> &str {
        &self.flavor
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Download and verify the new executable of `manifest` next to the running one,
    /// without installing it: [`Staged::commit`] swaps it in, [`Staged::discard`]
    /// throws it away. Refuses anything not newer than this binary (never a downgrade)
    /// and anything but a self-managed install.
    pub async fn stage(&self, manifest: &Manifest) -> Result<Staged, UpdateError> {
        let exe = match InstallKind::detect() {
            InstallKind::SelfManaged { exe } => exe,
            InstallKind::Docker => {
                return Err(UpdateError::Managed("Docker (pull the new image)".into()));
            }
            InstallKind::Managed { by } => return Err(UpdateError::Managed(by)),
            InstallKind::Mobile => return Err(UpdateError::Managed("the app store".into())),
        };
        if !auto::is_newer(&manifest.version, bunko_core::VERSION) {
            return Err(UpdateError::Manifest(format!(
                "{} is not newer than {}: never a downgrade",
                manifest.version,
                bunko_core::VERSION
            )));
        }
        let artifact = manifest.artifact(TARGET, &self.flavor)?.clone();
        let dir = exe
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        if let Err(e) = auto::check_space(&dir, artifact.size.saturating_mul(4)) {
            return Err(UpdateError::NoSpace(e));
        }
        let archive = dir.join(format!(".mokuro-bunko-update-{}.part", manifest.version));
        let staging = dir.join(format!(".mokuro-bunko-update-{}.d", manifest.version));
        let result = async {
            self.download_verified(&artifact, &archive).await?;
            let (a, kind, b, out) = (
                archive.clone(),
                layout::Kind::of(&artifact.url),
                artifact.binary.clone(),
                staging.clone(),
            );
            tokio::task::spawn_blocking(move || layout::unpack(&a, kind, &b, &out))
                .await
                .map_err(|e| UpdateError::Unpack(e.to_string()))?
        }
        .await;
        let _ = std::fs::remove_file(&archive);
        match result {
            Ok(unpacked) => Ok(Staged {
                version: manifest.version.clone(),
                binary: unpacked.cli.clone(),
                unpacked,
                archive,
            }),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                Err(e)
            }
        }
    }

    /// Where `release.json` is. GitHub's `releases/latest/download/` never points at a
    /// pre-release, so on the `prerelease` channel with a GitHub `latest` URL the newest
    /// published release (pre-release or not) is looked up through the releases API.
    async fn manifest_location(&self) -> Result<String, UpdateError> {
        let Some((repo, file)) = prerelease_lookup(&self.manifest_url, &self.channel) else {
            return Ok(self.manifest_url.clone());
        };
        let releases: Vec<GithubRelease> = self
            .client
            .get(format!(
                "https://api.github.com/repos/{repo}/releases?per_page=30"
            ))
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(match newest_release_with(&releases, &file) {
            Some(tag) => format!("https://github.com/{repo}/releases/download/{tag}/{file}"),
            None => self.manifest_url.clone(),
        })
    }

    /// Whether `latest` should be offered over `current` on this channel.
    pub fn newer(&self, current: &str, latest: &semver::Version) -> bool {
        let Ok(current) = semver::Version::parse(current) else {
            return false;
        };
        (latest.pre.is_empty() || self.channel == "prerelease") && *latest > current
    }

    /// Compare the latest release with this binary.
    pub async fn check(&self) -> UpdateStatus {
        let install = InstallKind::detect();
        let mut status = UpdateStatus {
            current: bunko_core::VERSION.into(),
            latest: None,
            available: false,
            notes_url: None,
            can_apply: false,
            install: install.clone(),
            docker_image: None,
            checked_at: Some(now_iso()),
            error: None,
            note: None,
        };
        match self.fetch_manifest().await {
            Ok(m) => {
                status.available = m
                    .semver()
                    .map(|l| self.newer(bunko_core::VERSION, &l))
                    .unwrap_or(false);
                status.latest = Some(m.version.clone());
                status.notes_url = (!m.notes_url.is_empty()).then(|| m.notes_url.clone());
                status.docker_image = m.docker.get(&self.flavor).cloned();
                status.can_apply = status.available
                    && install.can_apply()
                    && m.artifact(TARGET, &self.flavor).is_ok();
            }
            // Nothing published (yet): the check worked, there is just nothing to
            // update to. Not an error, so automatic updates do not count it as one.
            Err(UpdateError::NotPublished(_)) => {
                status.note = Some("No release has been published yet.".into())
            }
            Err(e) => status.error = Some(e.to_string()),
        }
        status
    }

    /// Download, verify and install the latest release over the running executable.
    /// Returns the installed version; the caller then calls [`restart`].
    pub async fn apply(&self) -> Result<String, UpdateError> {
        let exe = match InstallKind::detect() {
            InstallKind::SelfManaged { exe } => exe,
            InstallKind::Docker => {
                return Err(UpdateError::Managed("Docker (pull the new image)".into()));
            }
            InstallKind::Managed { by } => return Err(UpdateError::Managed(by)),
            InstallKind::Mobile => return Err(UpdateError::Managed("the app store".into())),
        };
        let manifest = self.fetch_manifest().await?;
        let latest = manifest.semver()?;
        if !self.newer(bunko_core::VERSION, &latest) {
            return Err(UpdateError::Manifest(format!(
                "{} is not newer than {}",
                manifest.version,
                bunko_core::VERSION
            )));
        }
        drop(exe);
        let staged = self.stage(&manifest).await?;
        tokio::task::spawn_blocking(move || staged.commit())
            .await
            .map_err(|e| UpdateError::Unpack(e.to_string()))?
    }

    /// Download `artifact` to `download` and check its sha256 against the manifest.
    async fn download_verified(
        &self,
        artifact: &Artifact,
        download: &Path,
    ) -> Result<(), UpdateError> {
        if !artifact.url.contains("://") || artifact.url.starts_with("file://") {
            let src = artifact
                .url
                .strip_prefix("file://")
                .unwrap_or(&artifact.url);
            tokio::fs::copy(src, download).await?;
        } else {
            let mut resp = self
                .client
                .get(&artifact.url)
                .send()
                .await?
                .error_for_status()?;
            let mut file = tokio::fs::File::create(download).await?;
            let name = artifact.url.rsplit('/').next().unwrap_or(&artifact.url);
            let mut progress = Progress::new(name, artifact.size);
            while let Some(chunk) = resp.chunk().await? {
                file.write_all(&chunk).await?;
                progress.add(chunk.len() as u64);
            }
            file.flush().await?;
        }
        let path = download.to_path_buf();
        let got = tokio::task::spawn_blocking(move || backend::sha256_file(&path))
            .await
            .map_err(|e| UpdateError::Unpack(e.to_string()))?
            .map_err(|e| UpdateError::Unpack(e.to_string()))?
            .0;
        if !got.eq_ignore_ascii_case(&artifact.sha256) {
            return Err(UpdateError::Checksum {
                got,
                want: artifact.sha256.clone(),
            });
        }
        Ok(())
    }
}

/// A release as GitHub's releases API lists it (the fields the pre-release lookup reads).
#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
}

/// The tag of the highest-versioned published release that carries `file`. Drafts are
/// not downloadable, and a release without the manifest (a model release such as
/// `models-v1`) is not an app release.
fn newest_release_with(releases: &[GithubRelease], file: &str) -> Option<String> {
    releases
        .iter()
        .filter(|r| !r.draft && r.assets.iter().any(|a| a.name == file))
        .filter_map(|r| {
            semver::Version::parse(r.tag_name.trim_start_matches('v'))
                .ok()
                .map(|v| (v, &r.tag_name))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, tag)| tag.clone())
}

/// `(owner/repo, file)` when `url` is a GitHub `releases/latest/download/<file>` URL and
/// the channel needs the releases API to see pre-releases.
pub fn prerelease_lookup(url: &str, channel: &str) -> Option<(String, String)> {
    if channel != "prerelease" {
        return None;
    }
    let rest = url.strip_prefix("https://github.com/")?;
    let (repo, file) = rest.split_once("/releases/latest/download/")?;
    Some((repo.to_string(), file.to_string()))
}

/// Download progress in the log: a line every 10 %, as the bytes arrive.
struct Progress {
    name: String,
    total: u64,
    done: u64,
    next: u64,
}

impl Progress {
    fn new(name: &str, total: u64) -> Progress {
        Progress {
            name: name.to_string(),
            total,
            done: 0,
            next: 10,
        }
    }

    fn add(&mut self, n: u64) {
        self.done += n;
        if self.total == 0 {
            return;
        }
        let pct = self.done.saturating_mul(100) / self.total;
        if pct >= self.next {
            tracing::info!(
                "update: {} {pct}% of {:.0} MB",
                self.name,
                self.total as f64 / 1e6
            );
            self.next = (pct / 10 + 1) * 10;
        }
    }
}

/// Pull `binary` out of a `.tar.gz`, `.zip` or bare executable download into `out`.
pub fn extract_binary(
    archive: &Path,
    url: &str,
    binary: &str,
    out: &Path,
) -> Result<(), UpdateError> {
    let stage = out.with_extension("unpack");
    let un = layout::unpack(archive, layout::Kind::of(url), binary, &stage)?;
    let r = std::fs::rename(&un.cli, out).map_err(UpdateError::Io);
    un.remove();
    r
}

/// This process's executable path. On Linux, once an update has replaced the file,
/// `/proc/self/exe` reads `<path> (deleted)`: the suffix is dropped, so a restart starts
/// the new file at the same path.
pub fn current_exe() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    #[cfg(target_os = "linux")]
    if let Some(s) = exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        return Ok(PathBuf::from(s));
    }
    Ok(exe)
}

/// Where [`backup_running`] keeps the executable an automatic update replaces, until the
/// new release has proven itself (its backend pack loads).
pub fn previous_exe_path() -> std::io::Result<PathBuf> {
    let exe = current_exe()?;
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    Ok(dir.join(format!(
        ".mokuro-bunko-previous{}",
        std::env::consts::EXE_SUFFIX
    )))
}

/// The other copies of the program in this install ([`layout::Plan`]) and where their
/// backups go.
fn companion_backups() -> Vec<(PathBuf, PathBuf)> {
    let Ok(exe) = current_exe() else {
        return Vec::new();
    };
    layout::Plan::for_exe(&exe)
        .programs()
        .into_iter()
        .skip(1)
        .map(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let backup = p.with_file_name(format!(".{name}.previous"));
            (p, backup)
        })
        .collect()
}

/// Copy the running executable aside ([`previous_exe_path`]) before an update replaces
/// it, and the other copies in this install beside theirs.
pub fn backup_running() -> std::io::Result<PathBuf> {
    let exe = current_exe()?;
    let prev = previous_exe_path()?;
    let _ = std::fs::remove_file(&prev);
    std::fs::copy(&exe, &prev)?;
    for (installed, backup) in companion_backups() {
        let _ = std::fs::remove_file(&backup);
        if let Err(e) = std::fs::copy(&installed, &backup) {
            tracing::warn!("update: no copy of {} kept: {e}", installed.display());
        }
    }
    Ok(prev)
}

/// Put the backed-up executable (and copies) back in place of the running one (a
/// rollback). A macOS app bundle is sealed again.
pub fn restore_previous() -> std::io::Result<()> {
    let prev = previous_exe_path()?;
    if !prev.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no previous executable at {}", prev.display()),
        ));
    }
    self_replace::self_replace(&prev)?;
    let _ = std::fs::remove_file(&prev);
    for (installed, backup) in companion_backups() {
        if backup.is_file()
            && let Err(e) = layout::replace_file(&backup, &installed)
        {
            tracing::warn!("rollback: {} not restored: {e}", installed.display());
        }
    }
    if let Ok(exe) = current_exe() {
        layout::Plan::for_exe(&exe).seal();
    }
    Ok(())
}

/// The new release proved itself: the backups are not needed any more.
pub fn drop_previous() {
    if let Ok(p) = previous_exe_path() {
        let _ = std::fs::remove_file(p);
    }
    for (_, backup) in companion_backups() {
        let _ = std::fs::remove_file(backup);
    }
    // A macOS bundle sealed with the backups in it: seal it without them.
    if let Ok(exe) = current_exe() {
        let plan = layout::Plan::for_exe(&exe);
        if !plan.bundles.is_empty() {
            plan.seal();
        }
    }
}

/// Exit code the portable launcher (`run.bat`) treats as "start me again".
pub const RESTART_EXIT_CODE: i32 = 75;

/// Replace this process with a fresh copy of the (updated) executable, same arguments.
/// On Unix this is `exec` (same pid, so systemd and Docker keep supervising it); on
/// Windows a new process is started and this one exits.
pub fn restart() -> std::io::Result<std::convert::Infallible> {
    let exe = current_exe()?;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(std::process::Command::new(&exe).args(&args).exec())
    }
    #[cfg(not(unix))]
    {
        // Under the portable launcher (run.bat loops on 75) let the launcher restart us,
        // so the console window and environment stay the launcher's.
        if std::env::var_os("MOKURO_LAUNCHER").is_some() {
            std::process::exit(RESTART_EXIT_CODE)
        }
        std::process::Command::new(&exe).args(&args).spawn()?;
        std::process::exit(0)
    }
}

pub(crate) fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil-from-days (Howard Hinnant), UTC.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn keypair() -> (SigningKey, String) {
        let sk = SigningKey::generate(&mut rand_core::OsRng);
        let pk = base64::engine::general_purpose::STANDARD.encode(sk.verifying_key().to_bytes());
        (sk, pk)
    }

    fn manifest_bytes() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": "9.9.9",
            "artifacts": {TARGET: {"lite": {"url": "https://x/a.tar.gz", "sha256": "00", "size": 1}}},
            "docker": {"lite": "ghcr.io/gnathonic/mokuro-bunko:9.9.9-lite"}
        }))
        .unwrap()
    }

    #[test]
    fn signature_roundtrip() {
        let (sk, pk) = keypair();
        let bytes = manifest_bytes();
        let sig = base64::engine::general_purpose::STANDARD.encode(sk.sign(&bytes).to_bytes());
        let m = parse_manifest(&bytes, &sig, &pk).unwrap();
        assert_eq!(m.version, "9.9.9");
        assert!(m.artifact(TARGET, "lite").is_ok());
        assert!(m.artifact(TARGET, "full").is_err());
        let mut tampered = bytes.clone();
        tampered[5] ^= 1;
        assert!(matches!(
            parse_manifest(&tampered, &sig, &pk),
            Err(UpdateError::BadSignature)
        ));
        let (_, other) = keypair();
        assert!(matches!(
            parse_manifest(&bytes, &sig, &other),
            Err(UpdateError::BadSignature)
        ));
    }

    #[test]
    fn channel_rules() {
        let u = Updater::new("http://x", "stable", "lite");
        assert!(u.newer("0.7.0", &semver::Version::parse("0.7.1").unwrap()));
        assert!(!u.newer("0.7.1", &semver::Version::parse("0.7.1").unwrap()));
        assert!(!u.newer("0.7.0", &semver::Version::parse("0.8.0-beta.1").unwrap()));
        let p = Updater::new("http://x", "prerelease", "lite");
        assert!(p.newer("0.7.0", &semver::Version::parse("0.8.0-beta.1").unwrap()));
    }

    #[test]
    fn extract_from_tarball() {
        let dir = tempfile::tempdir().unwrap();
        let tgz = dir.path().join("a.tar.gz");
        {
            let f = std::fs::File::create(&tgz).unwrap();
            let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut b = tar::Builder::new(gz);
            let data = b"#!/bin/sh\necho hi\n";
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, "mokuro-bunko-0.7/mokuro-bunko", &data[..])
                .unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let out = dir.path().join("out");
        extract_binary(&tgz, "https://x/a.tar.gz", "mokuro-bunko", &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"#!/bin/sh\necho hi\n");
    }

    #[test]
    fn auto_channel_follows_the_build() {
        assert_eq!(resolve_channel("auto", "0.7.0-beta.1"), "prerelease");
        assert_eq!(resolve_channel("", "0.7.0-rc.2"), "prerelease");
        assert_eq!(resolve_channel("auto", "0.7.0"), "stable");
        assert_eq!(resolve_channel("auto", "v0.7.1"), "stable");
        // An explicit setting wins over the build.
        assert_eq!(resolve_channel("stable", "0.7.0-beta.1"), "stable");
        assert_eq!(resolve_channel("prerelease", "0.7.0"), "prerelease");
        let u = Updater::new("http://x", "auto", "lite");
        let expect = if semver::Version::parse(bunko_core::VERSION)
            .unwrap()
            .pre
            .is_empty()
        {
            "stable"
        } else {
            "prerelease"
        };
        assert_eq!(u.channel(), expect);
        assert_eq!(
            Updater::new("http://x", "stable", "lite").channel(),
            "stable"
        );
    }

    #[test]
    fn prerelease_lookup_skips_drafts_and_model_releases() {
        let json = r#"[
            {"tag_name": "v0.7.0-beta.3", "draft": true, "assets": [{"name": "release.json"}]},
            {"tag_name": "torch-models-v1", "draft": false, "assets": [{"name": "torch-models.json"}]},
            {"tag_name": "models-v1", "draft": false, "assets": [{"name": "comic-text-detector.onnx"}]},
            {"tag_name": "v0.7.0-beta.1", "draft": false, "assets": [{"name": "release.json"}]},
            {"tag_name": "v0.7.0-beta.2", "draft": false, "assets": [{"name": "release.json"}, {"name": "release.json.sig"}]}
        ]"#;
        let releases: Vec<GithubRelease> = serde_json::from_str(json).unwrap();
        assert_eq!(
            newest_release_with(&releases, "release.json").as_deref(),
            Some("v0.7.0-beta.2")
        );
        assert_eq!(newest_release_with(&releases[..3], "release.json"), None);
    }

    #[test]
    fn prerelease_lookup_rules() {
        let url = "https://github.com/Gnathonic/mokuro-bunko/releases/latest/download/release.json";
        assert_eq!(prerelease_lookup(url, "stable"), None);
        assert_eq!(
            prerelease_lookup(url, "prerelease"),
            Some(("Gnathonic/mokuro-bunko".into(), "release.json".into()))
        );
        assert_eq!(
            prerelease_lookup("https://mirror.example/release.json", "prerelease"),
            None
        );
    }

    #[test]
    fn iso_format() {
        let s = now_iso();
        assert_eq!(s.len(), 20, "{s}");
        assert!(s.starts_with("20"));
        assert!(s.ends_with('Z'));
    }
}
