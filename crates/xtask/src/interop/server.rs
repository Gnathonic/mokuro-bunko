//! One throwaway Bunko server for a scenario: an isolated config and data dir, setup
//! completed over the API, catalog on, nothing reaching the network.

use super::http;
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = Duration::from_secs(300);

pub struct Server {
    bin: PathBuf,
    dir: PathBuf,
    pub addr: SocketAddr,
    /// `Basic …` for the admin account (WebDAV writes).
    pub auth: String,
    child: Option<Child>,
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn yaml_str(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).expect("a string")
}

impl Server {
    /// Start a server whose storage base is `dir` (the library is `dir/library`).
    pub fn start(bin: &Path, dir: &Path) -> Result<Server> {
        let port = free_port()?;
        let config = format!(
            "server:\n  host: \"127.0.0.1\"\n  port: {port}\n\
             storage:\n  base_path: {base}\n\
             catalog:\n  enabled: true\n  enrich_community: false\n\
             ocr:\n  local_processing: false\n  autobench: false\n\
             registration:\n  mode: disabled\n",
            base = yaml_str(dir),
        );
        std::fs::write(dir.join("config.yaml"), config)?;
        let mut server = Server {
            bin: bin.to_path_buf(),
            dir: dir.to_path_buf(),
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            auth: String::new(),
            child: None,
        };
        server.spawn()?;
        server.complete_setup()?;
        Ok(server)
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

    fn check_alive(&mut self) -> Result<()> {
        if let Some(child) = &mut self.child
            && let Some(status) = child.try_wait()?
        {
            bail!("the server exited ({status})\n{}", self.log_tail());
        }
        Ok(())
    }

    fn complete_setup(&mut self) -> Result<()> {
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
            "library": {"serve_readers": true, "link_progress": false},
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
        if config.status != 200 {
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
