//! `rclone serve webdav`, the rig's plain WebDAV cloud: a pinned release, downloaded
//! once into `target/interop/tools` and checked against its sha256 before use.

use crate::torch_pack;
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use std::io::Read as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const RCLONE_VERSION: &str = "v1.75.2";

/// One release archive: its sha256 (rclone's SHA256SUMS) and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin {
    pub os_arch: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

const PINS: &[Pin] = &[
    Pin {
        os_arch: "linux-amd64",
        sha256: "349ac8fba6ff65d6247043f1750cdcb518ec5d500ef91463a10d37c0ccdf3702",
        size: 31_615_572,
    },
    Pin {
        os_arch: "linux-arm64",
        sha256: "7e1e8d69654941b7b7df84ee74c5f7fb09cce7d5496947fb7bf16b55ff427d10",
        size: 28_773_641,
    },
    Pin {
        os_arch: "osx-amd64",
        sha256: "574e1f31cf1c464e5fb0325cb66fcbdeaf1efa1802bd9b17f13c256fd74b6561",
        size: 35_069_545,
    },
    Pin {
        os_arch: "osx-arm64",
        sha256: "610ab7d48615135dfc60b763858b11a426a0d3d1160c1bb76fb0ae6011b02edd",
        size: 32_648_708,
    },
    Pin {
        os_arch: "windows-amd64",
        sha256: "8a9db2bad1dc79a65b3348fd857069598e2aab7cf4d118b77b1df23055aff818",
        size: 31_634_900,
    },
    Pin {
        os_arch: "windows-arm64",
        sha256: "7eb9844c9e5b723b9ef28821891caa17f6d9d1a375c49365cbe04114cec8dcd9",
        size: 28_556_640,
    },
];

/// The pin for `os` and `arch` (`std::env::consts` names).
pub fn pin_for(os: &str, arch: &str) -> Option<Pin> {
    let os = match os {
        "linux" => "linux",
        "macos" => "osx",
        "windows" => "windows",
        _ => return None,
    };
    let arch = match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => return None,
    };
    let want = format!("{os}-{arch}");
    PINS.iter().copied().find(|p| p.os_arch == want)
}

fn url(pin: &Pin) -> String {
    format!(
        "https://downloads.rclone.org/{RCLONE_VERSION}/rclone-{RCLONE_VERSION}-{}.zip",
        pin.os_arch
    )
}

/// Download `url` into `cache` unless a copy matching `pin` is there; a download that
/// does not match the pin is deleted and refused.
pub fn fetch_archive(url: &str, pin: &Pin, cache: &Path) -> Result<PathBuf> {
    torch_pack::fetch_url(url, pin.sha256, pin.size, cache)
}

/// The rclone binary for this machine, under `tools` (`target/interop/tools`).
pub fn install(tools: &Path) -> Result<PathBuf> {
    let pin = pin_for(std::env::consts::OS, std::env::consts::ARCH).with_context(|| {
        format!(
            "no pinned rclone for {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let exe = format!("rclone{}", std::env::consts::EXE_SUFFIX);
    let dir = tools.join(format!("rclone-{RCLONE_VERSION}-{}", pin.os_arch));
    let bin = dir.join(&exe);
    // The archive in the cache is re-checked against the pin on every run; the
    // binary is extracted from it only once.
    let zip_path = fetch_archive(&url(&pin), &pin, &tools.join("downloads"))?;
    if bin.is_file() {
        return Ok(bin);
    }
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&zip_path)?)?;
    let name = (0..zip.len())
        .filter_map(|i| zip.name_for_index(i).map(str::to_string))
        .find(|n| n.rsplit('/').next() == Some(exe.as_str()))
        .with_context(|| format!("{} holds no {exe}", zip_path.display()))?;
    let mut bytes = Vec::new();
    zip.by_name(&name)?.read_to_end(&mut bytes)?;
    std::fs::create_dir_all(&dir)?;
    let part = dir.join(format!("{exe}.part"));
    std::fs::write(&part, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&part, &bin)?;
    Ok(bin)
}

/// `rclone serve webdav` over a local folder.
pub struct Rclone {
    child: Child,
    pub addr: SocketAddr,
    pub user: String,
    pub password: String,
    /// The folder it serves (the cloud account's root).
    pub root: PathBuf,
    pub log: PathBuf,
}

impl Rclone {
    /// Serve `root` on a free port. `origin`: the reader's origin, allowed by CORS.
    pub fn start(bin: &Path, root: &Path, dir: &Path, origin: Option<&str>) -> Result<Rclone> {
        use rand::RngCore as _;
        std::fs::create_dir_all(root)?;
        std::fs::create_dir_all(dir)?;
        let port = std::net::TcpListener::bind("127.0.0.1:0")?
            .local_addr()?
            .port();
        let mut bytes = [0u8; 18];
        rand::rng().fill_bytes(&mut bytes);
        let password = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let log = dir.join("rclone.log");
        // An empty config of its own: never the user's remotes.
        let conf = dir.join("rclone.conf");
        std::fs::write(&conf, "")?;
        let mut cmd = Command::new(bin);
        cmd.arg("serve")
            .arg("webdav")
            .arg(root)
            .args(["--addr", &format!("127.0.0.1:{port}")])
            .args(["--user", "rig", "--pass", &password])
            .arg("--config")
            .arg(&conf);
        if let Some(o) = origin {
            cmd.args(["--allow-origin", o]);
        }
        let file = std::fs::File::create(&log)?;
        let child = cmd
            .stdin(Stdio::null())
            .stdout(file.try_clone()?)
            .stderr(file)
            .spawn()
            .with_context(|| format!("starting {}", bin.display()))?;
        let mut r = Rclone {
            child,
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            user: "rig".into(),
            password,
            root: root.to_path_buf(),
            log,
        };
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(30) {
            if std::net::TcpStream::connect(r.addr).is_ok() {
                return Ok(r);
            }
            if let Some(status) = r.child.try_wait()? {
                bail!(
                    "rclone exited ({status})\n{}",
                    std::fs::read_to_string(&r.log).unwrap_or_default()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        bail!("rclone did not listen in 30 s")
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Write `(path, bytes)` straight into the served folder (rclone serves the disk).
    pub fn seed(&self, files: &[(String, Vec<u8>)]) -> Result<()> {
        for (path, data) in files {
            let dest = self.root.join(path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(dest, data)?;
        }
        Ok(())
    }
}

impl Drop for Rclone {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ci_platform_has_a_pin() {
        for (os, arch) in [
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "aarch64"),
            ("macos", "x86_64"),
            ("windows", "x86_64"),
        ] {
            let pin = pin_for(os, arch).unwrap_or_else(|| panic!("{os}-{arch}"));
            assert_eq!(pin.sha256.len(), 64);
            assert!(pin.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
        }
        assert_eq!(pin_for("freebsd", "x86_64"), None);
        assert_eq!(
            url(&pin_for("linux", "x86_64").unwrap()),
            "https://downloads.rclone.org/v1.75.2/rclone-v1.75.2-linux-amd64.zip"
        );
    }

    // A file:// URL for curl; unix paths only.
    #[cfg(unix)]
    #[test]
    fn a_download_that_does_not_match_its_pin_is_refused_and_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("rclone-v1.75.2-linux-amd64.zip");
        std::fs::write(&bad, b"not the release archive").unwrap();
        let cache = dir.path().join("cache");
        // Right size, wrong bytes: the sha256 check is what refuses it.
        let pin = Pin {
            os_arch: "linux-amd64",
            sha256: "349ac8fba6ff65d6247043f1750cdcb518ec5d500ef91463a10d37c0ccdf3702",
            size: 23,
        };
        let url = format!("file://{}", bad.display());
        let err = fetch_archive(&url, &pin, &cache).unwrap_err().to_string();
        assert!(err.contains("pinned"), "{err}");
        assert!(
            std::fs::read_dir(&cache).unwrap().next().is_none(),
            "nothing is left in the cache"
        );
        // A cached copy that does not match is not used either: it is fetched again
        // (and refused again).
        std::fs::write(cache.join("rclone-v1.75.2-linux-amd64.zip"), b"tampered").unwrap();
        assert!(fetch_archive(&url, &pin, &cache).is_err());
        assert!(!cache.join("rclone-v1.75.2-linux-amd64.zip").exists());
    }
}
