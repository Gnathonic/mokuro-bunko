//! Cloudflare quick tunnel via the external `cloudflared` binary (0.5.2 `tunnel/`).

use parking_lot::Mutex;
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

pub const INSTALL_URL: &str =
    "https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/";

/// The first `https://<x>.trycloudflare.com` in a line of cloudflared output.
pub fn find_tunnel_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start + 8..];
    let end = rest
        .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.'))
        .unwrap_or(rest.len());
    let host = &rest[..end];
    let sub = host.strip_suffix(".trycloudflare.com")?;
    (!sub.is_empty()
        && sub
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
    .then(|| format!("https://{host}"))
}

pub fn cloudflared_path() -> Option<std::path::PathBuf> {
    which::which("cloudflared").ok()
}

#[derive(Clone, Default)]
pub struct TunnelService {
    inner: Arc<Mutex<Option<Running>>>,
}

struct Running {
    child: Child,
    url: Arc<Mutex<Option<String>>>,
}

impl TunnelService {
    pub fn status(&self) -> Value {
        let mut guard = self.inner.lock();
        if let Some(r) = guard.as_mut()
            && matches!(r.child.try_wait(), Ok(Some(_)))
        {
            *guard = None;
        }
        let running = guard.is_some();
        let url = guard.as_ref().and_then(|r| r.url.lock().clone());
        json!({"running": running, "url": if running { url } else { None }, "available": cloudflared_path().is_some()})
    }

    /// Start `cloudflared tunnel --url <scheme>://localhost:<port>`; no-op if running.
    pub fn start(&self, port: u16, https: bool) -> Result<(), String> {
        let mut guard = self.inner.lock();
        if guard.is_some() {
            return Ok(());
        }
        let bin = cloudflared_path().ok_or_else(|| "cloudflared is not installed".to_string())?;
        let scheme = if https { "https" } else { "http" };
        let mut child = Command::new(bin)
            .args(["tunnel", "--url", &format!("{scheme}://localhost:{port}")])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| e.to_string())?;
        let url = Arc::new(Mutex::new(None));
        if let Some(stderr) = child.stderr.take() {
            let url = url.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut u = url.lock();
                    if u.is_none() {
                        *u = find_tunnel_url(&line);
                    }
                }
            });
        }
        *guard = Some(Running { child, url });
        Ok(())
    }

    pub async fn stop(&self) {
        let running = self.inner.lock().take();
        if let Some(mut r) = running {
            let _ = r.child.start_kill();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), r.child.wait()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_regex() {
        assert_eq!(
            find_tunnel_url("2024 INF |  https://quiet-river-12.trycloudflare.com  |").as_deref(),
            Some("https://quiet-river-12.trycloudflare.com")
        );
        assert_eq!(find_tunnel_url("https://example.com"), None);
    }
}
