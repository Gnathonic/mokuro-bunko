//! `setup`: the interactive first-run wizard (0.5.2 `setup_cli.py`), same prompts in the
//! same order, so answers piped on stdin keep working.
//!
//! It builds a fresh config (no env overrides, every other section default — OCR
//! generations default to hayai-nova). Deviation: the admin username and password are
//! checked at the prompt and asked again when refused (0.5.2 saved the config and then
//! failed to create the account; spec Q12).

use super::Ctx;
use crate::cfgfile;
use crate::out::{CmdResult, Fail};
use crate::prompt;
use bunko_core::config::{Config, DYNDNS_PROVIDERS, DynDnsConfig, REGISTRATION_MODES, SslConfig};
use bunko_core::{Role, storage};
use bunko_db::{Database, UserStatus, validate_password, validate_username};
use bunko_server::tls;
use std::path::Path;

pub fn run(ctx: &Ctx, skip_if_exists: bool) -> CmdResult {
    let path = &ctx.config_path;
    if path.exists() {
        if skip_if_exists {
            println!("Config file already exists at {}, skipping setup.", path.display());
            return Ok(());
        }
        if !prompt::confirm(&format!("Config file exists at {}. Overwrite?", path.display()), Some(false))? {
            return Ok(());
        }
    }

    println!("=== mokuro-bunko setup ===\n");

    // 1. Storage path, 2. port
    let default_storage = storage::default_storage_path().display().to_string();
    let storage_path = prompt::text("Storage path", Some(&default_storage))?;
    let port = prompt::parsed("Server port", Some("8080"), |s| match s.trim().parse::<i64>() {
        Ok(p) if (0..65536).contains(&p) => Ok(p as u16),
        Ok(p) => Err(format!("Invalid port: {p}")),
        Err(_) => Err(format!("'{s}' is not a valid integer.")),
    })?;

    // 3. SSL
    let mut ssl = SslConfig::default();
    if prompt::confirm("Enable SSL?", Some(false))? {
        if prompt::confirm("  Generate a self-signed certificate?", Some(true))? {
            ssl = SslConfig { enabled: true, auto_cert: true, ..SslConfig::default() };
        } else {
            let cert_file = prompt::text("  Path to certificate file", None)?;
            let key_file = prompt::text("  Path to private key file", None)?;
            ssl = SslConfig { enabled: true, auto_cert: false, cert_file, key_file };
        }
    }

    // 4. Admin user
    let create_admin = prompt::confirm("Create an admin user?", Some(true))?;
    let mut admin = None;
    if create_admin {
        let username = prompt::parsed("  Admin username", Some("admin"), |s| match validate_username(s) {
            None => Ok(s.to_string()),
            Some(e) => Err(e.to_string()),
        })?;
        let password = loop {
            let pw = prompt::hidden("  Admin password", true)?;
            match validate_password(&pw) {
                None => break pw,
                Some(e) => println!("Error: {e}"),
            }
        };
        admin = Some((username, password));
    }

    // 5. Registration mode
    let reg_mode = prompt::choice("Registration mode", REGISTRATION_MODES, Some("self"))?;

    // 6. Connectivity
    let mut dyndns = DynDnsConfig::default();
    println!("\nConnectivity options:");
    let access = prompt::choice("Access method", &["lan", "cloudflare", "dyndns", "reverse-proxy"], Some("lan"))?;
    match access.as_str() {
        "dyndns" => {
            let provider = prompt::choice("  DynDNS provider", DYNDNS_PROVIDERS, Some("duckdns"))?;
            let domain = prompt::text("  Domain", None)?;
            let token = prompt::hidden("  API token", false)?;
            let update_url = if provider == "generic" { prompt::text("  Update URL", None)? } else { String::new() };
            dyndns = DynDnsConfig { enabled: true, provider, token, domain, update_url, ..DynDnsConfig::default() };
        }
        "cloudflare" => {
            println!("  Cloudflare tunnel will be available via the admin panel or 'mokuro-bunko tunnel cloudflare'")
        }
        "reverse-proxy" => println!("  Configure your reverse proxy to forward to the server port"),
        _ => {}
    }

    // 7. CORS origins
    let mut config = Config::default();
    if prompt::confirm("Add custom CORS origins?", Some(false))? {
        loop {
            let origin = prompt::text_opts("  Origin (empty to finish)", Some(""), false)?;
            if origin.is_empty() {
                break;
            }
            config.cors.allowed_origins.push(origin);
        }
    }

    config.server.host = "0.0.0.0".into();
    config.server.port = port;
    config.storage.base_path = storage::expand_user(Path::new(&storage_path));
    config.registration.mode = reg_mode;
    config.ssl = ssl.clone();
    config.dyndns = dyndns;

    println!("\n=== Configuration Summary ===");
    print!("{}", config.to_yaml());

    if !prompt::confirm("\nSave this configuration?", Some(true))? {
        println!("Setup cancelled.");
        return Ok(());
    }

    cfgfile::save(&config, path)?;
    println!("\nConfig saved to {}", path.display());

    if let Some((username, password)) = admin {
        match create_admin_user(&config, &username, &password) {
            Ok(()) => println!("Admin user '{username}' created"),
            Err(e) => eprintln!("Warning: Could not create admin user: {e}"),
        }
    }

    if ssl.enabled && ssl.auto_cert {
        let (cert_path, key_path) = tls::default_cert_paths();
        if !cert_path.exists() {
            tls::generate_self_signed(&cert_path, &key_path, "localhost").map_err(Fail::msg)?;
            println!("SSL certificate generated at {}", cert_path.display());
        }
    }

    println!("\nSetup complete! Run 'mokuro-bunko serve' to start the server.");
    Ok(())
}

fn create_admin_user(config: &Config, username: &str, password: &str) -> anyhow::Result<()> {
    let layout = config.storage.layout();
    layout.ensure_directories()?;
    let db = Database::open(layout.database())?;
    db.create_user(username, password, Role::Admin, UserStatus::Active, "")?;
    Ok(())
}
