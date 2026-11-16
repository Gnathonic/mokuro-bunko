//! `tunnel *` (0.5.2 `tunnel_cli.py`): shells out to an installed `cloudflared`.

use super::Ctx;
use crate::cfgfile;
use crate::cli::{CorsAnswer, TunnelCmd};
use crate::out::{CmdResult, Fail, exit_with, runtime};
use crate::prompt;
use bunko_server::ops::tunnel::{INSTALL_URL, cloudflared_path, find_tunnel_url};
use bunko_update::tools::Windowless;
use std::io::IsTerminal;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

pub fn run(ctx: &Ctx, cmd: TunnelCmd) -> CmdResult {
    match cmd {
        TunnelCmd::Status => status(),
        TunnelCmd::Cloudflare { port, cors } => cloudflare(ctx, port, cors),
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
        tokio::time::timeout(
            Duration::from_secs(10),
            // The app runs this command from a process without a console.
            tokio::process::Command::new(&path)
                .arg("version")
                .windowless()
                .output(),
        )
        .await
    });
    match out {
        Ok(Ok(o)) => {
            let stdout = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let text = if stdout.is_empty() {
                String::from_utf8_lossy(&o.stderr).trim().to_string()
            } else {
                stdout
            };
            if !text.is_empty() {
                println!("Version: {text}");
            }
        }
        Ok(Err(e)) => eprintln!("Could not get version: {e}"),
        Err(_) => eprintln!("Could not get version: timed out after 10 seconds"),
    }
    Ok(())
}

fn cloudflare(ctx: &Ctx, port: Option<u16>, cors: CorsAnswer) -> CmdResult {
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
        let mut stderr = BufReader::new(stderr);
        let mut buf = Vec::new();
        let mut tunnel_url: Option<String> = None;
        loop {
            buf.clear();
            tokio::select! {
                read = stderr.read_until(b'\n', &mut buf) => match read {
                    Ok(0) => break,
                    Ok(_) => {
                        // Read as bytes: a line that is not UTF-8 failed the read, and the
                        // error ended the command, which took cloudflared down with it.
                        let line = String::from_utf8_lossy(&buf);
                        let line = line.trim_end_matches(['\n', '\r']);
                        eprintln!("{line}");
                        if let Some(url) = tunnel_url.is_none().then(|| find_tunnel_url(line)).flatten() {
                            println!("\nTunnel URL: {url}\n");
                            offer_cors(ctx, &url, cors);
                            tunnel_url = Some(url);
                        }
                    }
                    // The tunnel does not depend on its log: wait for cloudflared instead.
                    Err(e) => {
                        eprintln!("Warning: cannot read cloudflared's output any more ({e}); the tunnel keeps running");
                        break;
                    }
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

/// What `offer_cors` does with the tunnel URL.
#[derive(Debug, PartialEq, Eq)]
enum CorsStep {
    Add,
    Skip,
    Ask,
    /// `--cors ask` with no terminal to ask on.
    SkipNoTerminal,
}

/// The CORS step for `--cors` and whether stdin is a terminal. Without a terminal there
/// is nobody to answer, and an unanswered question would end the tunnel, so it is not asked.
fn cors_step(answer: CorsAnswer, stdin_is_terminal: bool) -> CorsStep {
    match answer {
        CorsAnswer::Yes => CorsStep::Add,
        CorsAnswer::No => CorsStep::Skip,
        CorsAnswer::Ask if stdin_is_terminal => CorsStep::Ask,
        CorsAnswer::Ask => CorsStep::SkipNoTerminal,
    }
}

/// `Add tunnel URL to CORS allowed origins?`, saved from the file alone (no env baking).
/// Nothing here ends the command: cloudflared is the long-running process, so a "no", an
/// end of input or a config that cannot be saved leaves the tunnel up.
fn offer_cors(ctx: &Ctx, url: &str, answer: CorsAnswer) {
    let add = match cors_step(answer, std::io::stdin().is_terminal()) {
        CorsStep::Add => true,
        CorsStep::Skip => false,
        CorsStep::SkipNoTerminal => {
            println!(
                "Not adding the tunnel URL to CORS origins (stdin is not a terminal); \
                 pass --cors yes to add it."
            );
            return;
        }
        CorsStep::Ask => {
            match prompt::confirm_or_eof("Add tunnel URL to CORS allowed origins?", Some(true)) {
                Ok(Some(yes)) => yes,
                Ok(None) => {
                    println!();
                    false
                }
                Err(e) => {
                    eprintln!("Warning: could not read an answer: {e}");
                    false
                }
            }
        }
    };
    if !add {
        println!("Not adding {url} to CORS origins");
        return;
    }
    match add_cors_origin(ctx, url) {
        Ok(true) => println!("Added {url} to CORS origins"),
        Ok(false) => println!("{url} is already a CORS origin"),
        Err(e) => eprintln!("Warning: could not add {url} to CORS origins: {e}"),
    }
}

/// Adds `url` to `cors.allowed_origins` in the config file; `false` when it was there.
fn add_cors_origin(ctx: &Ctx, url: &str) -> Result<bool, Fail> {
    let mut config = cfgfile::load_for_write(&ctx.config_path)?;
    if config.cors.allowed_origins.iter().any(|o| o == url) {
        return Ok(false);
    }
    config.cors.allowed_origins.push(url.to_string());
    cfgfile::save(&config, &ctx.config_path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cors_step_asks_only_on_a_terminal() {
        assert_eq!(cors_step(CorsAnswer::Yes, false), CorsStep::Add);
        assert_eq!(cors_step(CorsAnswer::Yes, true), CorsStep::Add);
        assert_eq!(cors_step(CorsAnswer::No, false), CorsStep::Skip);
        assert_eq!(cors_step(CorsAnswer::No, true), CorsStep::Skip);
        assert_eq!(cors_step(CorsAnswer::Ask, true), CorsStep::Ask);
        assert_eq!(cors_step(CorsAnswer::Ask, false), CorsStep::SkipNoTerminal);
    }
}
