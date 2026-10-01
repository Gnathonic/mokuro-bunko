//! `healthcheck [--url]`: GET the local server's `/api/health`; exit 0 on a 2xx, else 1.
//! For container `HEALTHCHECK`s, which have no curl/python in the image.
//!
//! The default URL is `http(s)://127.0.0.1:<server.port>/api/health` (`https` when
//! `ssl.enabled`; the certificate is not verified — it is our own, often self-signed).
//! A bound `server.host` other than a wildcard is probed directly.

use super::Ctx;
use crate::cfgfile;
use crate::out::{CmdResult, Fail, exit_with, runtime};
use std::time::Duration;

pub fn run(ctx: &Ctx, url: Option<String>) -> CmdResult {
    let url = match url {
        Some(u) => u,
        None => {
            let config = cfgfile::load_effective(&ctx.config_path)?;
            let scheme = if config.ssl.enabled { "https" } else { "http" };
            let host = match config.server.host.as_str() {
                "0.0.0.0" | "" | "::" => "127.0.0.1".to_string(),
                h if h.contains(':') => format!("[{h}]"),
                h => h.to_string(),
            };
            format!("{scheme}://{host}:{}/api/health", config.server.port)
        }
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .danger_accept_invalid_certs(true)
        .no_proxy()
        .build()
        .map_err(Fail::msg)?;
    let result = runtime()?.block_on(async { client.get(&url).send().await.map(|r| r.status()) });
    match result {
        Ok(status) if status.is_success() => {
            println!("healthy: {url} ({})", status.as_u16());
            Ok(())
        }
        Ok(status) => Err(exit_with(format!("unhealthy: {url} returned HTTP {}", status.as_u16()))),
        Err(e) => Err(exit_with(format!("unhealthy: {url}: {e}"))),
    }
}
