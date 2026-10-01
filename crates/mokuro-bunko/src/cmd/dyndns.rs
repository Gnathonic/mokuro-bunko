//! `dyndns *` (0.5.2 `dyndns_cli.py`). Exit codes as 0.5.2: `update` exits 0 even when
//! unconfigured or failing (spec Q14 left open; scripts may rely on it).

use super::Ctx;
use crate::cfgfile;
use crate::cli::DyndnsCmd;
use crate::out::{CmdResult, runtime};
use crate::prompt;
use bunko_core::config::{DYNDNS_PROVIDERS, DynDnsConfig};
use bunko_server::ops::dyndns::DynDnsService;

pub fn run(ctx: &Ctx, cmd: DyndnsCmd) -> CmdResult {
    let path = &ctx.config_path;
    match cmd {
        DyndnsCmd::Setup => {
            let mut config = cfgfile::load_for_write(path)?;
            println!("=== DynDNS Setup ===\n");
            let provider = prompt::choice("Provider", DYNDNS_PROVIDERS, Some("duckdns"))?;
            let domain = prompt::text("Domain", Some(&config.dyndns.domain))?;
            let token = prompt::hidden("API Token", false)?;
            let update_url = if provider == "generic" {
                prompt::text(
                    "Update URL (use {ip}, {domain}, {token} as placeholders)",
                    Some(&config.dyndns.update_url),
                )?
            } else {
                String::new()
            };
            // 0.5.2 crashed on an interval under 30; ask again instead.
            let interval = prompt::parsed("Update interval (seconds)", Some("300"), |s| {
                match s.trim().parse::<u32>() {
                    Ok(n) if n >= 30 => Ok(n),
                    Ok(_) => Err("DynDNS interval must be at least 30 seconds".into()),
                    Err(_) => Err(format!("'{s}' is not a valid integer.")),
                }
            })?;
            let enabled = prompt::confirm("Enable DynDNS?", Some(true))?;
            config.dyndns = DynDnsConfig {
                enabled,
                provider,
                token,
                domain,
                update_url,
                interval,
            };
            cfgfile::save(&config, path)?;
            println!("\nDynDNS configuration saved to {}", path.display());
            if enabled {
                println!("DynDNS will start automatically when the server runs.");
            }
        }
        DyndnsCmd::Status => {
            let d = cfgfile::load_effective(path)?.dyndns;
            let or_unset = |s: &str| {
                if s.is_empty() {
                    "(not set)".to_string()
                } else {
                    s.to_string()
                }
            };
            println!("Enabled:   {}", if d.enabled { "True" } else { "False" });
            println!("Provider:  {}", d.provider);
            println!("Domain:    {}", or_unset(&d.domain));
            println!(
                "Token:     {}",
                if d.token.is_empty() {
                    "(not set)"
                } else {
                    "****"
                }
            );
            println!("Interval:  {}s", d.interval);
            if d.provider == "generic" {
                println!("URL:       {}", or_unset(&d.update_url));
            }
        }
        DyndnsCmd::Update => {
            let config = cfgfile::load_effective(path)?;
            if config.dyndns.token.is_empty() || config.dyndns.domain.is_empty() {
                eprintln!("Error: DynDNS not configured. Run 'mokuro-bunko dyndns setup' first.");
                return Ok(());
            }
            println!("Updating DNS for {}...", config.dyndns.domain);
            let result = runtime()?.block_on(DynDnsService::new(config.dyndns).update_now());
            if result["success"].as_bool() == Some(true) {
                println!(
                    "Success! IP: {}",
                    result["ip"].as_str().unwrap_or("unknown")
                );
            } else {
                eprintln!(
                    "Failed: {}",
                    result["error"].as_str().unwrap_or("unknown error")
                );
            }
        }
        DyndnsCmd::Enable | DyndnsCmd::Disable => {
            let enable = matches!(cmd, DyndnsCmd::Enable);
            let mut config = cfgfile::load_for_write(path)?;
            config.dyndns.enabled = enable;
            cfgfile::save(&config, path)?;
            let word = if enable { "enabled" } else { "disabled" };
            println!("DynDNS {word}. Restart the server for changes to take effect.");
        }
    }
    Ok(())
}
