//! One throwaway Bunko server for a scenario: an isolated config and data dir, setup
//! completed over the API, catalog on, nothing reaching the network. A rig server can
//! also keep its library in a cloud account (cloudsim, or rclone's WebDAV), signed in
//! the way a connected server is: its credentials file and `storage.library`.

use super::http;
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use sha2::{Digest as _, Sha256};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = Duration::from_secs(300);

/// The password of every [`SeedUser`] the rig creates.
pub const SEED_PASSWORD: &str = "rig-password-1";

pub struct Server {
    bin: PathBuf,
    dir: PathBuf,
    env: Vec<(String, String)>,
    pub addr: SocketAddr,
    /// `Basic …` for the admin account (WebDAV writes).
    pub auth: String,
    child: Option<Child>,
}

/// Where a rig server keeps its library.
#[derive(Debug, Clone, Default)]
pub enum Backend {
    /// `<dir>/library` on this disk.
    #[default]
    Local,
    /// A cloud folder the server is already signed in to.
    Cloud(CloudLogin),
}

/// A cloud sign-in as a connected server keeps it: the `storage.library` backend and
/// one entry of `<base>/cloud/credentials.json`.
#[derive(Debug, Clone)]
pub struct CloudLogin {
    /// `storage.library.backend`: `gdrive`, `onedrive`, `mega` or `webdav`.
    pub backend: &'static str,
    /// The account id the store id is made from (WebDAV: `user@url`).
    pub account_id: String,
    /// The credentials entry, `provider` and `account_id` filled in on write.
    pub credential: serde_json::Value,
    /// WebDAV: the server's URL and user (`storage.library.webdav`).
    pub webdav: Option<(String, String)>,
}

/// An account the rig creates through the admin API after setup.
#[derive(Debug, Clone)]
pub struct SeedUser {
    pub username: String,
    /// `registered`, `uploader`, `editor`, `inviter`, `admin` or `processor`.
    pub role: String,
}

#[derive(Debug, Clone, Default)]
pub struct ServerOptions {
    pub backend: Backend,
    /// `server.serve_readers` (the reader's WebDAV and bridge).
    pub readers: bool,
    /// Created with [`SEED_PASSWORD`].
    pub users: Vec<SeedUser>,
    /// Remote OCR processors may connect (`ocr.sessions`), polled every second.
    pub ocr_remote: bool,
    /// Extra environment of the server process (cloudsim's addresses, the store tune).
    pub env: Vec<(String, String)>,
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn yaml_str(text: &str) -> String {
    serde_json::to_string(text).expect("a string")
}

/// The server's `storage::store_id`, which names a store's credentials entry and data
/// folder. Copied, not shared (xtask links no server crate): a mismatch shows as a
/// library that never connects, which [`Server::wait_storage`] reports.
pub fn store_id(provider: &str, pairing: &str, account_id: &str, folder: &str) -> String {
    let mut h = Sha256::new();
    for (i, part) in [provider, pairing, account_id, folder].iter().enumerate() {
        if i > 0 {
            h.update([0u8]);
        }
        h.update(part.as_bytes());
    }
    hex::encode(h.finalize())[..16].to_string()
}

/// The library folder in every cloud account the rig uses.
pub const CLOUD_FOLDER: &str = "mokuro-reader";

fn config_yaml(dir: &Path, port: u16, o: &ServerOptions) -> String {
    let mut y = format!(
        "server:\n  host: \"127.0.0.1\"\n  port: {port}\n\
         storage:\n  base_path: {base}\n",
        base = yaml_str(&dir.to_string_lossy()),
    );
    if let Backend::Cloud(login) = &o.backend {
        // Short quiet and poll periods: the rig waits on what the feed sees.
        y.push_str(&format!(
            "  library:\n    backend: {}\n    folder: {CLOUD_FOLDER}\n    quiet_s: 1\n\
             \x20   poll:\n      active_s: 2\n      idle_s: 2\n",
            login.backend
        ));
        if let Some((url, user)) = &login.webdav {
            y.push_str(&format!(
                "    webdav:\n      url: {}\n      username: {}\n",
                yaml_str(url),
                yaml_str(user)
            ));
        }
    }
    y.push_str(&format!(
        "catalog:\n  enabled: true\n  enrich_community: false\n\
         ocr:\n  local_processing: false\n  autobench: false\n  sessions: {}\n{}\
         update:\n  check: false\n\
         registration:\n  mode: disabled\n",
        o.ocr_remote,
        if o.ocr_remote {
            "  poll_interval: 1\n"
        } else {
            ""
        },
    ));
    y
}

/// Write the credentials file a connected server keeps (plain, unsealed: the rig sets
/// no secrets key).
fn write_credentials(dir: &Path, login: &CloudLogin) -> Result<()> {
    let cloud = dir.join("cloud");
    std::fs::create_dir_all(&cloud)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&cloud, std::fs::Permissions::from_mode(0o700))?;
    }
    // Only Drive has a pairing part; the rig's servers use the release pairing.
    let pairing = if login.backend == "gdrive" {
        "release"
    } else {
        ""
    };
    let id = store_id(login.backend, pairing, &login.account_id, CLOUD_FOLDER);
    let mut cred = login.credential.clone();
    cred["provider"] = login.backend.into();
    cred["account_id"] = login.account_id.clone().into();
    let doc = serde_json::json!({"version": 1, "stores": {id: cred}});
    let path = cloud.join("credentials.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&doc)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

impl Server {
    /// Start a server whose storage base is `dir` (the library is `dir/library`).
    pub fn start(bin: &Path, dir: &Path) -> Result<Server> {
        Server::start_with(
            bin,
            dir,
            ServerOptions {
                readers: true,
                ..ServerOptions::default()
            },
        )
    }

    /// Start a server as `o` describes, setup completed and its users created.
    pub fn start_with(bin: &Path, dir: &Path, o: ServerOptions) -> Result<Server> {
        std::fs::create_dir_all(dir)?;
        let port = free_port()?;
        std::fs::write(dir.join("config.yaml"), config_yaml(dir, port, &o))?;
        if let Backend::Cloud(login) = &o.backend {
            write_credentials(dir, login)?;
        }
        let mut server = Server {
            bin: bin.to_path_buf(),
            dir: dir.to_path_buf(),
            env: o.env.clone(),
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            auth: String::new(),
            child: None,
        };
        server.spawn()?;
        server.complete_setup(o.readers)?;
        for u in &o.users {
            server.create_user(u)?;
        }
        Ok(server)
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// `Basic …` for a [`SeedUser`].
    pub fn basic(user: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{SEED_PASSWORD}"))
        )
    }

    fn create_user(&self, u: &SeedUser) -> Result<()> {
        let body = serde_json::json!({
            "username": u.username, "password": SEED_PASSWORD, "role": u.role,
        });
        let reply = http::request(
            self.addr,
            "POST",
            "/_admin/api/users",
            &[
                ("Authorization", &self.auth),
                ("Content-Type", "application/json"),
            ],
            &serde_json::to_vec(&body)?,
        )?;
        if !(200..300).contains(&reply.status) {
            bail!(
                "creating {} answered {}: {}",
                u.username,
                reply.status,
                String::from_utf8_lossy(&reply.body)
            );
        }
        Ok(())
    }

    /// The admin Storage panel's document (`GET /_admin/api/storage`).
    pub fn storage(&self) -> Result<serde_json::Value> {
        let reply = http::request(
            self.addr,
            "GET",
            "/_admin/api/storage",
            &[("Authorization", &self.auth)],
            &[],
        )?;
        if reply.status != 200 {
            bail!("the storage panel answered {}", reply.status);
        }
        reply.json()
    }

    /// Wait until the cloud library is connected and its lease is in `state` (`held`,
    /// `standby`, …). Returns the panel's document.
    pub fn wait_storage(&mut self, state: &str, timeout: Duration) -> Result<serde_json::Value> {
        let start = Instant::now();
        let mut last = serde_json::Value::Null;
        while start.elapsed() < timeout {
            self.check_alive()?;
            if let Ok(v) = self.storage() {
                if v["connected"] == true && v["lease"]["state"] == state {
                    return Ok(v);
                }
                last = v;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        bail!(
            "the library did not reach lease state {state:?} in {} s; last: connected={} \
             problem={} lease={}\n{}",
            timeout.as_secs(),
            last["connected"],
            last["problem"],
            last["lease"],
            self.log_tail()
        )
    }

    fn spawn(&mut self) -> Result<()> {
        let xdg = self.dir.join("xdg");
        let tmp = self.dir.join("tmp");
        for sub in ["config", "data", "cache"] {
            std::fs::create_dir_all(xdg.join(sub))?;
        }
        std::fs::create_dir_all(&tmp)?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("serve.log"))?;
        let child = Command::new(&self.bin)
            .arg("-c")
            .arg(self.dir.join("config.yaml"))
            .arg("serve")
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .env("XDG_CONFIG_HOME", xdg.join("config"))
            .env("XDG_DATA_HOME", xdg.join("data"))
            .env("XDG_CACHE_HOME", xdg.join("cache"))
            .env("TMPDIR", &tmp)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .with_context(|| format!("starting {}", self.bin.display()))?;
        self.child = Some(child);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(60) {
            if http::get(self.addr, "/setup/").is_ok() {
                return Ok(());
            }
            self.check_alive()?;
            std::thread::sleep(Duration::from_millis(100));
        }
        bail!("the server did not answer in 60 s\n{}", self.log_tail())
    }

    pub fn check_alive(&mut self) -> Result<()> {
        if let Some(child) = &mut self.child
            && let Some(status) = child.try_wait()?
        {
            bail!("the server exited ({status})\n{}", self.log_tail());
        }
        Ok(())
    }

    fn complete_setup(&mut self, readers: bool) -> Result<()> {
        use rand::RngCore as _;
        let mut bytes = [0u8; 18];
        rand::rng().fill_bytes(&mut bytes);
        let password = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let secret = self.dir.join("admin-password.txt");
        std::fs::write(&secret, &password)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600))?;
        }
        let body = serde_json::json!({
            "admin": {"username": "admin", "password": password},
            "registration": {"mode": "disabled"},
            "library": {"serve_readers": readers, "link_progress": false},
            "ocr": {"on": false},
        });
        let reply = http::request(
            self.addr,
            "POST",
            "/setup/api/complete",
            &[("Content-Type", "application/json")],
            &serde_json::to_vec(&body)?,
        )?;
        if !(200..300).contains(&reply.status) {
            bail!(
                "setup answered {}: {}",
                reply.status,
                String::from_utf8_lossy(&reply.body)
            );
        }
        self.auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("admin:{password}"))
        );
        let config = http::get(self.addr, "/catalog/api/config")?;
        if readers && config.status != 200 {
            bail!("the catalog is off after setup ({})", config.status);
        }
        Ok(())
    }

    /// Wait until the startup pass has materialized every series (each listing row has
    /// its totals), so warm timings read the database and not the index fallback.
    pub fn wait_ready(&mut self, expected_series: usize) -> Result<Duration> {
        let start = Instant::now();
        let mut last = String::new();
        while start.elapsed() < READY_TIMEOUT {
            self.check_alive()?;
            if let Ok(reply) = http::get(self.addr, "/catalog/api/library")
                && reply.status == 200
                && let Ok(doc) = reply.json()
            {
                let rows = doc["series"].as_array().cloned().unwrap_or_default();
                let done = rows
                    .iter()
                    .filter(|row| row.get("total_pages").is_some())
                    .count();
                if rows.len() == expected_series && done == expected_series {
                    return Ok(start.elapsed());
                }
                last = format!("{done}/{} of {expected_series} series compiled", rows.len());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        bail!(
            "the library was not ready after {} s ({last})\n{}",
            READY_TIMEOUT.as_secs(),
            self.log_tail()
        )
    }

    /// Stop and start again on the same config and data (setup already done). Returns
    /// once the server answers.
    pub fn restart(&mut self) -> Result<()> {
        self.stop();
        self.spawn()
    }

    pub fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-INT", &child.id().to_string()])
                .status();
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(20) {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    pub fn log_tail(&self) -> String {
        let text = std::fs::read_to_string(self.dir.join("serve.log")).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(40)..].join("\n")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}
