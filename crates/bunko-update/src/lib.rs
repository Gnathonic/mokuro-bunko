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

pub mod backend;

use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
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
        match std::env::current_exe() {
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
}

pub struct Updater {
    client: reqwest::Client,
    manifest_url: String,
    public_key: String,
    channel: String,
    flavor: String,
}

impl Updater {
    /// `flavor` is `full` or `lite`; `channel` is `stable` or `prerelease`.
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
            channel: channel.into(),
            flavor: flavor.into(),
        }
    }

    pub fn with_public_key(mut self, key: impl Into<String>) -> Self {
        self.public_key = key.into();
        self
    }

    pub async fn fetch_manifest(&self) -> Result<Manifest, UpdateError> {
        let url = self.manifest_location().await?;
        let body = self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let sig = self
            .client
            .get(format!("{url}.sig"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        parse_manifest(&body, &sig, &self.public_key)
    }

    /// Where `release.json` is. GitHub's `releases/latest/download/` never points at a
    /// pre-release, so on the `prerelease` channel with a GitHub `latest` URL the newest
    /// published release (pre-release or not) is looked up through the releases API.
    async fn manifest_location(&self) -> Result<String, UpdateError> {
        let Some((repo, file)) = prerelease_lookup(&self.manifest_url, &self.channel) else {
            return Ok(self.manifest_url.clone());
        };
        #[derive(Deserialize)]
        struct Release {
            tag_name: String,
            draft: bool,
        }
        let releases: Vec<Release> = self
            .client
            .get(format!(
                "https://api.github.com/repos/{repo}/releases?per_page=10"
            ))
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        // Newest first; drafts are not downloadable.
        Ok(match releases.into_iter().find(|r| !r.draft) {
            Some(r) => format!(
                "https://github.com/{repo}/releases/download/{}/{file}",
                r.tag_name
            ),
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
        let artifact = manifest.artifact(TARGET, &self.flavor)?.clone();
        // Download next to the executable so the final swap is a same-filesystem rename.
        let dir = exe
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let download = dir.join(format!(".mokuro-bunko-update-{}.part", manifest.version));
        let result = self.download_and_install(&artifact, &download).await;
        let _ = tokio::fs::remove_file(&download).await;
        result.map(|_| manifest.version)
    }

    async fn download_and_install(
        &self,
        artifact: &Artifact,
        download: &Path,
    ) -> Result<(), UpdateError> {
        let mut resp = self
            .client
            .get(&artifact.url)
            .send()
            .await?
            .error_for_status()?;
        let mut file = tokio::fs::File::create(download).await?;
        let mut hasher = Sha256::new();
        while let Some(chunk) = resp.chunk().await? {
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);
        let got = hex::encode(hasher.finalize());
        if !got.eq_ignore_ascii_case(&artifact.sha256) {
            return Err(UpdateError::Checksum {
                got,
                want: artifact.sha256.clone(),
            });
        }
        let download = download.to_path_buf();
        let binary = artifact.binary.clone();
        let url = artifact.url.clone();
        tokio::task::spawn_blocking(move || -> Result<(), UpdateError> {
            let unpacked = download.with_extension("bin");
            extract_binary(&download, &url, &binary, &unpacked)?;
            let r = self_replace::self_replace(&unpacked).map_err(UpdateError::Io);
            let _ = std::fs::remove_file(&unpacked);
            r?;
            if let Some(dir) = download.parent() {
                update_companions(&download, &url, dir);
            }
            Ok(())
        })
        .await
        .map_err(|e| UpdateError::Unpack(e.to_string()))?
    }
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

/// Pull `binary` out of a `.tar.gz`, `.zip` or bare executable download into `out`.
pub fn extract_binary(
    archive: &Path,
    url: &str,
    binary: &str,
    out: &Path,
) -> Result<(), UpdateError> {
    let lower = url.to_ascii_lowercase();
    let mut bytes = Vec::new();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        let mut tar =
            tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(archive)?));
        let mut found = false;
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.to_path_buf();
            if path.file_name().is_some_and(|n| n == binary) {
                entry.read_to_end(&mut bytes)?;
                found = true;
                break;
            }
        }
        if !found {
            return Err(UpdateError::Unpack(format!(
                "{binary} is not in the archive"
            )));
        }
    } else if lower.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?)
            .map_err(|e| UpdateError::Unpack(e.to_string()))?;
        let name = zip
            .file_names()
            .find(|n| *n == binary || n.rsplit('/').next() == Some(binary))
            .map(str::to_string)
            .ok_or_else(|| UpdateError::Unpack(format!("{binary} is not in the archive")))?;
        zip.by_name(&name)
            .map_err(|e| UpdateError::Unpack(e.to_string()))?
            .read_to_end(&mut bytes)?;
    } else {
        std::fs::copy(archive, out)?;
        set_executable(out)?;
        return Ok(());
    }
    std::fs::write(out, bytes)?;
    set_executable(out)?;
    Ok(())
}

fn set_executable(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Executables installed next to `mokuro-bunko` from the same archive, which an update
/// replaces too when they are there: the desktop tray (GUI.md §6). In the macOS archive
/// the tray is inside `mokuro-bunko.app`.
pub const COMPANIONS: &[&str] = &[if cfg!(windows) {
    "mokuro-bunko-tray.exe"
} else {
    "mokuro-bunko-tray"
}];

/// Where the installed copies of companion `name` are, under the executable's `dir`.
fn companion_paths(dir: &Path, name: &str) -> Vec<PathBuf> {
    [
        dir.join(name),
        dir.join("mokuro-bunko.app")
            .join("Contents")
            .join("MacOS")
            .join(name),
    ]
    .into_iter()
    .filter(|p| p.is_file())
    .collect()
}

/// Replace the installed [`COMPANIONS`] under `dir` with the ones in the downloaded
/// archive. Best effort, after the main executable was replaced: a failure is logged and
/// the old companion keeps working. A running tray goes on running its old copy until it
/// is started again. Returns the paths replaced.
pub fn update_companions(archive: &Path, url: &str, dir: &Path) -> Vec<PathBuf> {
    let mut done = Vec::new();
    for name in COMPANIONS {
        for target in companion_paths(dir, name) {
            let new = target.with_file_name(format!(".{name}.new"));
            let result = extract_binary(archive, url, name, &new)
                .and_then(|()| replace_file(&new, &target).map_err(UpdateError::Io));
            match result {
                Ok(()) => done.push(target),
                Err(e) => {
                    let _ = std::fs::remove_file(&new);
                    tracing::warn!("update: {} not replaced: {e}", target.display());
                }
            }
        }
    }
    done
}

/// Put `new` in place of `target`. Windows cannot overwrite a running executable but can
/// rename it, so the old one moves aside (`.old`, removed now or by the next update).
fn replace_file(new: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let old = target.with_extension("exe.old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(target, &old)?;
        if let Err(e) = std::fs::rename(new, target) {
            let _ = std::fs::rename(&old, target);
            return Err(e);
        }
        let _ = std::fs::remove_file(&old);
        Ok(())
    }
    #[cfg(not(windows))]
    std::fs::rename(new, target)
}

/// Exit code the portable launcher (`run.bat`) treats as "start me again".
pub const RESTART_EXIT_CODE: i32 = 75;

/// Replace this process with a fresh copy of the (updated) executable, same arguments.
/// On Unix this is `exec` (same pid, so systemd and Docker keep supervising it); on
/// Windows a new process is started and this one exits.
pub fn restart() -> std::io::Result<std::convert::Infallible> {
    let exe = std::env::current_exe()?;
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

fn now_iso() -> String {
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
    #[test]
    fn update_replaces_installed_companions_only() {
        let dir = tempfile::tempdir().unwrap();
        let tray = COMPANIONS[0];
        // The archive: top/mokuro-bunko + top/<tray> + the macOS bundle's copy.
        let archive = dir.path().join("a.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(
                std::fs::File::create(&archive).unwrap(),
                flate2::Compression::fast(),
            );
            let mut tar = tar::Builder::new(gz);
            for (path, body) in [
                ("top/mokuro-bunko", &b"cli-new"[..]),
                (&*format!("top/{tray}"), &b"tray-new"[..]),
            ] {
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(0o755);
                h.set_cksum();
                tar.append_data(&mut h, path, body).unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
        }
        let install = dir.path().join("install");
        std::fs::create_dir_all(&install).unwrap();
        // No tray installed: nothing to do.
        assert!(update_companions(&archive, "x.tar.gz", &install).is_empty());
        std::fs::write(install.join(tray), b"tray-old").unwrap();
        let bundle = install.join("mokuro-bunko.app/Contents/MacOS");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join(tray), b"tray-old").unwrap();
        let done = update_companions(&archive, "x.tar.gz", &install);
        assert_eq!(done.len(), 2);
        assert_eq!(std::fs::read(install.join(tray)).unwrap(), b"tray-new");
        assert_eq!(std::fs::read(bundle.join(tray)).unwrap(), b"tray-new");
        assert!(!install.join(format!(".{tray}.new")).exists());
        // An archive without the tray leaves the installed one alone.
        let bare = dir.path().join("b.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(
                std::fs::File::create(&bare).unwrap(),
                flate2::Compression::fast(),
            );
            let mut tar = tar::Builder::new(gz);
            let mut h = tar::Header::new_gnu();
            h.set_size(3);
            h.set_cksum();
            tar.append_data(&mut h, "top/mokuro-bunko", &b"cli"[..])
                .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        assert!(update_companions(&bare, "x.tar.gz", &install).is_empty());
        assert_eq!(std::fs::read(install.join(tray)).unwrap(), b"tray-new");
    }

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
