//! cloudsim as its own process (`bunko-cloudsim`), and a client for its control API
//! (`/_sim/…`, crates/bunko-cloudsim/README.md).

use super::super::http;
use super::super::server::CloudLogin;
use super::Provider;
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde_json::{Value, json};
use std::io::BufRead as _;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub struct Cloudsim {
    child: Child,
    addr: SocketAddr,
    /// `http://127.0.0.1:PORT`, what clients reach it at.
    pub base: String,
}

/// The rig's one MEGA login (any account; cloudsim keys accounts by name).
const MEGA_PASSWORD: &str = "cloudsim";

/// The address in cloudsim's first stdout line, `listening on http://HOST:PORT`.
pub fn parse_listening(line: &str) -> Option<SocketAddr> {
    line.trim()
        .strip_prefix("listening on http://")?
        .parse()
        .ok()
}

/// The email and password [`Cloudsim::login`] gives `account` (its MEGA sign-in, and
/// the label of its OAuth sign-ins).
pub fn mega_credentials(account: &str) -> (String, &'static str) {
    (format!("{account}@cloudsim.test"), MEGA_PASSWORD)
}

impl Cloudsim {
    /// Start `bin` on a free port, its log at `dir/cloudsim.log`. OneDrive's
    /// pre-authenticated URLs name `localhost`: another origin than the API's
    /// `127.0.0.1`, as Microsoft's storage hosts are, for the browser as for jsdom.
    pub fn start(bin: &Path, dir: &Path) -> Result<Cloudsim> {
        std::fs::create_dir_all(dir)?;
        let log = dir.join("cloudsim.log");
        let mut child = Command::new(bin)
            .args(["--port", "0", "--preauth-host", "localhost"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&log)?)
            .spawn()
            .with_context(|| format!("starting {}", bin.display()))?;
        let stdout = child.stdout.take().context("cloudsim's stdout")?;
        let mut lines = std::io::BufReader::new(stdout).lines();
        let first = lines.next().transpose()?.unwrap_or_default();
        let Some(addr) = parse_listening(&first) else {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "cloudsim printed {first:?}, not its address\n{}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
        };
        // Nothing else comes on stdout; drain it anyway so a write never blocks.
        std::thread::spawn(move || for _ in lines {});
        let sim = Cloudsim {
            child,
            addr,
            base: format!("http://{addr}"),
        };
        sim.get("health").context("cloudsim's health check")?;
        Ok(sim)
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let bytes = body
            .map(serde_json::to_vec)
            .transpose()?
            .unwrap_or_default();
        let headers: &[(&str, &str)] = if body.is_some() {
            &[("Content-Type", "application/json")]
        } else {
            &[]
        };
        let reply = http::request(self.addr, method, &format!("/_sim/{path}"), headers, &bytes)?;
        if !(200..300).contains(&reply.status) {
            bail!(
                "cloudsim {method} /_sim/{path} answered {}: {}",
                reply.status,
                String::from_utf8_lossy(&reply.body)
            );
        }
        if reply.body.is_empty() {
            return Ok(Value::Null);
        }
        reply.json()
    }

    pub fn get(&self, path: &str) -> Result<Value> {
        self.call("GET", path, None)
    }

    pub fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.call("POST", path, Some(&body))
    }

    /// Put each `(path, bytes)` into the account, folders created along the way.
    pub fn seed(&self, p: Provider, account: &str, files: &[(String, Vec<u8>)]) -> Result<()> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let files: Vec<Value> = files
            .iter()
            .map(|(path, data)| json!({"path": path, "content_base64": b64.encode(data)}))
            .collect();
        self.post(
            "seed",
            json!({"provider": p.sim_name(), "account": account, "files": files}),
        )
        .map(|_| ())
    }

    /// Every live file of the account: `[{path, size, sha256}]`.
    pub fn files(&self, p: Provider, account: &str) -> Result<Vec<Value>> {
        let v = self.get(&format!(
            "files?provider={}&account={account}",
            p.sim_name()
        ))?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    /// The sha256 of the live file at `path`, if there is one.
    pub fn file_sha256(&self, p: Provider, account: &str, path: &str) -> Result<Option<String>> {
        Ok(self
            .files(p, account)?
            .into_iter()
            .find(|f| f["path"] == path)
            .and_then(|f| f["sha256"].as_str().map(str::to_string)))
    }

    /// The next `times` matching Drive or Graph requests answer `status`.
    pub fn fault(&self, p: Provider, account: &str, fault: Value) -> Result<()> {
        let mut body = fault;
        body["provider"] = p.sim_name().into();
        body["account"] = account.into();
        self.post("fault", body).map(|_| ())
    }

    pub fn knobs(&self, p: Provider, account: &str, knobs: Value) -> Result<()> {
        let mut body = knobs;
        body["provider"] = p.sim_name().into();
        body["account"] = account.into();
        self.post("knobs", body).map(|_| ())
    }

    /// The request log: `[{seq, provider, account, method, path, class, units,
    /// bytes_in, bytes_out, status}]`.
    pub fn log(&self) -> Result<Vec<Value>> {
        Ok(self.get("log")?.as_array().cloned().unwrap_or_default())
    }

    pub fn clear_log(&self) -> Result<()> {
        self.call("DELETE", "log", None).map(|_| ())
    }

    /// Requests cloudsim could not accept: every one is a defect of the client that
    /// sent it (or a gap in the simulator).
    pub fn violations(&self) -> Result<Vec<Value>> {
        Ok(self
            .get("violations")?
            .as_array()
            .cloned()
            .unwrap_or_default())
    }

    /// The violations so far, cleared in the same call: a GET then a DELETE would wipe,
    /// unseen, one recorded between them (the server polls cloudsim all along).
    pub fn take_violations(&self) -> Result<Vec<Value>> {
        match self.call("DELETE", "violations", None)? {
            Value::Array(v) => Ok(v),
            // A cloudsim older than this rig clears them and answers nothing.
            other => bail!(
                "cloudsim's DELETE /_sim/violations answered {other}, not the list it cleared"
            ),
        }
    }

    /// The process environment that points a testing server's stores at cloudsim
    /// (`storage/test_cloud.rs` in bunko-server).
    pub fn server_env(&self) -> Vec<(String, String)> {
        let b = &self.base;
        [
            ("MOKURO_TEST_CLOUD_API_BASE_GDRIVE", format!("{b}/google")),
            (
                "MOKURO_TEST_CLOUD_API_BASE_ONEDRIVE",
                format!("{b}/graph/v1.0"),
            ),
            ("MOKURO_TEST_CLOUD_API_BASE_MEGA", format!("{b}/mega/")),
            (
                "MOKURO_TEST_CLOUD_TOKEN_URL_GOOGLE",
                format!("{b}/oauth/google/token"),
            ),
            (
                "MOKURO_TEST_CLOUD_TOKEN_URL_MICROSOFT",
                format!("{b}/oauth/microsoft/token"),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    /// A server's sign-in to `account`: OAuth tokens from cloudsim (the access token
    /// already expired, so the server's first call refreshes it there), or MEGA's
    /// saved session.
    pub fn login(&self, p: Provider, account: &str) -> Result<CloudLogin> {
        let (label, _) = mega_credentials(account);
        let (backend, credential) = match p {
            Provider::Google | Provider::Microsoft => {
                let t = self.post(
                    "token",
                    json!({"provider": p.sim_name(), "account": account}),
                )?;
                let expired = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs()
                    .saturating_sub(60);
                (
                    if p == Provider::Google {
                        "gdrive"
                    } else {
                        "onedrive"
                    },
                    json!({
                        "refresh_token": t["refresh_token"],
                        "access_token": t["access_token"],
                        "expires_at": expired,
                        "account_label": label,
                        "client_id": "cloudsim-client",
                    }),
                )
            }
            Provider::Mega => {
                let v = self.post(
                    "mega/account",
                    json!({"account": account, "email": label, "password": MEGA_PASSWORD}),
                )?;
                let session = v["session"]
                    .as_str()
                    .context("cloudsim gave no MEGA session (an older cloudsim?)")?;
                (
                    "mega",
                    json!({"account_label": label, "mega_session": session}),
                )
            }
            Provider::Local | Provider::Rclone | Provider::Folder => {
                bail!("{} is not a cloudsim provider", p.name())
            }
        };
        Ok(CloudLogin {
            backend,
            account_id: format!("sim-{account}"),
            credential,
            webdav: None,
            path: None,
        })
    }
}

impl Drop for Cloudsim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_comes_from_the_listening_line() {
        assert_eq!(
            parse_listening("listening on http://127.0.0.1:4321\n"),
            Some("127.0.0.1:4321".parse().unwrap())
        );
        assert_eq!(parse_listening("listening on https://127.0.0.1:1"), None);
        assert_eq!(parse_listening("seeded 3 file(s)"), None);
    }
}
