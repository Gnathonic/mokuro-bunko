//! `tunnel *` (0.5.2 `tunnel_cli.py`): shells out to an installed `cloudflared`.

use super::Ctx;
use crate::cfgfile;
use crate::cli::TunnelCmd;
use crate::out::{CmdResult, Fail, exit_with, runtime};
use crate::prompt;
use bunko_server::ops::tunnel::{INSTALL_URL, cloudflared_path, find_tunnel_url};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

pub fn run(ctx: &Ctx, cmd: TunnelCmd) -> CmdResult {
    match cmd {
        TunnelCmd::Status => status(),
        TunnelCmd::Cloudflare { port } => cloudflare(ctx, port),
    }
}

fn status() -> CmdResult {
    let Some(path) = cloudflared_path() else {
        println!("cloudflared: not installed");
        println!("Install from: {INSTALL_URL}");
        return Ok(());
    };
    println!("cloudflared: {}", path.display());
    let rt = runtime()?;
    let out = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), tokio::process::Command::new(&path).arg("version").output()).await
    });
    match out {
        Ok(Ok(o)) => {
            let stdout = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let text = if stdout.is_empty() { String::from_utf8_lossy(&o.stderr).trim().to_string() } else { stdout };
            if !text.is_empty() {
                println!("Version: {text}");
            }
        }
        Ok(Err(e)) => eprintln!("Could not get version: {e}"),
        Err(_) => eprintln!("Could not get version: timed out after 10 seconds"),
    }
    Ok(())
}

fn cloudflare(ctx: &Ctx, port: Option<u16>) -> CmdResult {
    let Some(bin) = cloudflared_path() else {
        eprintln!("Error: cloudflared is not installed");
        return Err(exit_with(format!("Install from: {INSTALL_URL}")));
    };
    let config = cfgfile::load_effective(&ctx.config_path)?;
    let port = port.unwrap_or(config.server.port);
    let scheme = if config.ssl.enabled { "https" } else { "http" };
    let local_url = format!("{scheme}://localhost:{port}");
    println!("Starting Cloudflare tunnel for {local_url}...");
    println!("Press Ctrl+C to stop\n");

    let rt = runtime()?;
    rt.block_on(async {
        let mut child = tokio::process::Command::new(bin)
            .args(["tunnel", "--url", &local_url])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(Fail::msg)?;
        let stderr = child.stderr.take().ok_or_else(|| Fail::msg("cloudflared has no stderr"))?;
        let mut lines = BufReader::new(stderr).lines();
        let mut tunnel_url: Option<String> = None;
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(line)) => {
                        eprintln!("{line}");
                        if let Some(url) = tunnel_url.is_none().then(|| find_tunnel_url(&line)).flatten() {
                            println!("\nTunnel URL: {url}\n");
                            offer_cors(ctx, &url)?;
                            tunnel_url = Some(url);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => return Err(Fail::msg(e)),
                },
                _ = tokio::signal::ctrl_c() => {
                    println!("\nStopping tunnel...");
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                    return Ok(());
                }
            }
        }
        let _ = child.wait().await;
        Ok(())
    })
}

/// `Add tunnel URL to CORS allowed origins?` — saved from the file alone (no env baking).
fn offer_cors(ctx: &Ctx, url: &str) -> CmdResult {
    if !prompt::confirm("Add tunnel URL to CORS allowed origins?", Some(true))? {
        return Ok(());
    }
    let mut config = cfgfile::load_for_write(&ctx.config_path)?;
    if !config.cors.allowed_origins.iter().any(|o| o == url) {
        config.cors.allowed_origins.push(url.to_string());
        cfgfile::save(&config, &ctx.config_path)?;
        println!("Added {url} to CORS origins");
    }
    Ok(())
}
