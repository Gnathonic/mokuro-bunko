//! `config *` (0.5.2 `config_cli.py`). Writers load the file without env overrides; see
//! `cfgfile`.

use super::Ctx;
use crate::cfgfile;
use crate::cli::ConfigCmd;
use crate::out::{CmdResult, exit_with};
use bunko_core::{Config, ConfigError};

pub fn run(ctx: &Ctx, cmd: ConfigCmd) -> CmdResult {
    let path = &ctx.config_path;
    match cmd {
        ConfigCmd::Show => {
            // The effective config (env included, token NOT masked), as 0.5.2.
            let config = cfgfile::load_effective(path)?;
            print!("{}", config.to_yaml());
        }
        ConfigCmd::Set { key, value } => {
            let mut config = cfgfile::load_for_write(path)?;
            if let Err(e) = config.set_by_dotted_key(&key, &value) {
                // `UnknownKey` already carries the whole sentence (`Unknown config
                // section: x`, ...); its Display would prefix "Unknown config key: ".
                let msg = match e {
                    ConfigError::UnknownKey(m) => m,
                    other => other.to_string(),
                };
                return Err(exit_with(format!("Error: {msg}")));
            }
            cfgfile::save(&config, path)?;
            println!("Set {key} = {value}");
        }
        ConfigCmd::Path => {
            println!("Config file: {}", path.display());
            match cfgfile::load_effective(path) {
                Ok(c) => println!("Storage dir: {}", c.storage.base_path.display()),
                Err(e) => println!("Storage dir: unknown -- the config file cannot be read ({e})"),
            }
        }
        ConfigCmd::Init { force } => {
            if path.exists() && !force {
                eprintln!("Error: Config file already exists at {}", path.display());
                return Err(exit_with("Use --force to overwrite"));
            }
            cfgfile::save(&Config::default(), path)?;
            println!("Created config file at {}", path.display());
        }
        ConfigCmd::CorsAdd { origin } => {
            let mut config = cfgfile::load_for_write(path)?;
            if config.cors.allowed_origins.contains(&origin) {
                println!("Origin already allowed: {origin}");
                return Ok(());
            }
            config.cors.allowed_origins.push(origin.clone());
            cfgfile::save(&config, path)?;
            println!("Added CORS origin: {origin}");
        }
        ConfigCmd::CorsRemove { origin } => {
            let mut config = cfgfile::load_for_write(path)?;
            let Some(pos) = config.cors.allowed_origins.iter().position(|o| *o == origin) else {
                return Err(exit_with(format!("Error: Origin not found: {origin}")));
            };
            config.cors.allowed_origins.remove(pos);
            cfgfile::save(&config, path)?;
            println!("Removed CORS origin: {origin}");
        }
    }
    Ok(())
}
