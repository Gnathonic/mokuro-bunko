//! The app's `config.yaml`: written on first start, and kept in line with the three
//! settings the native screen owns (storage directory, port, LAN access). Everything
//! else in the file is the admin UI's to change, as on any other install.

use bunko_core::Config;
use bunko_core::config::{load_config, save_config};
use std::path::{Path, PathBuf};

/// What the app passes to `start`.
#[derive(Debug, Clone)]
pub struct StartOptions {
    /// `storage.base_path`: the library, database, logs. Under the app's external files
    /// directory, so users can copy manga in over USB.
    pub storage_dir: PathBuf,
    pub config_path: PathBuf,
    pub port: u16,
    /// `0.0.0.0` (reachable from the Wi-Fi network) instead of `127.0.0.1`.
    pub lan: bool,
}

impl StartOptions {
    pub fn host(&self) -> &'static str {
        if self.lan { "0.0.0.0" } else { "127.0.0.1" }
    }
}

/// Load (or create) the config and apply the native settings. The file is rewritten only
/// when one of those settings changed, so admin edits are never clobbered otherwise.
pub fn prepare(opts: &StartOptions) -> Result<Config, String> {
    std::fs::create_dir_all(&opts.storage_dir).map_err(|e| format!("Could not create {}: {e}", opts.storage_dir.display()))?;
    let existed = opts.config_path.exists();
    let mut config = load_config(Some(&opts.config_path)).map_err(|e| format!("Invalid config {}: {e}", opts.config_path.display()))?;
    let before = existed.then(|| config.to_yaml());
    if !existed {
        first_run_defaults(&mut config);
    }
    config.storage.base_path = opts.storage_dir.clone();
    config.server.host = opts.host().to_string();
    config.server.port = opts.port;
    if before.as_deref() != Some(config.to_yaml().as_str()) {
        save_config(&config, Some(&opts.config_path)).map_err(|e| format!("Could not write {}: {e}", opts.config_path.display()))?;
    }
    Ok(config)
}

/// A phone is a small host: no local OCR (there is no runtime for it in this build) and a
/// smaller cache budget than the desktop default.
fn first_run_defaults(config: &mut Config) {
    config.ocr.local_processing = false;
    config.server.cache_mb = 16;
}

/// The address the app shows for other devices: `http://<ip>:<port>/`.
pub fn lan_url(ip: &str, port: u16) -> String {
    if ip.contains(':') { format!("http://[{ip}]:{port}/") } else { format!("http://{ip}:{port}/") }
}

/// `config.yaml` next to the storage directory by default (the app passes it explicitly).
pub fn default_config_path(storage_dir: &Path) -> PathBuf {
    storage_dir.join("config.yaml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(dir: &Path, port: u16, lan: bool) -> StartOptions {
        StartOptions { storage_dir: dir.join("storage"), config_path: dir.join("config.yaml"), port, lan }
    }

    #[test]
    fn first_start_writes_config() {
        let d = tempfile::tempdir().unwrap();
        let c = prepare(&opts(d.path(), 8123, false)).unwrap();
        assert_eq!(c.server.host, "127.0.0.1");
        assert_eq!(c.server.port, 8123);
        assert!(!c.ocr.local_processing);
        let text = std::fs::read_to_string(d.path().join("config.yaml")).unwrap();
        assert!(text.contains("8123"), "{text}");
        assert!(text.contains(&d.path().join("storage").display().to_string()));
    }

    #[test]
    fn keeps_admin_edits_and_applies_native_settings() {
        let d = tempfile::tempdir().unwrap();
        prepare(&opts(d.path(), 8123, false)).unwrap();
        let path = d.path().join("config.yaml");
        let mut c = load_config(Some(&path)).unwrap();
        c.registration.mode = "self".into();
        save_config(&c, Some(&path)).unwrap();
        let c = prepare(&opts(d.path(), 9000, true)).unwrap();
        assert_eq!(c.registration.mode, "self");
        assert_eq!(c.server.host, "0.0.0.0");
        let reread = load_config(Some(&path)).unwrap();
        assert_eq!(reread.server.port, 9000);
        assert_eq!(reread.registration.mode, "self");
    }

    #[test]
    fn unchanged_settings_do_not_rewrite() {
        let d = tempfile::tempdir().unwrap();
        prepare(&opts(d.path(), 8123, false)).unwrap();
        let path = d.path().join("config.yaml");
        let t0 = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        prepare(&opts(d.path(), 8123, false)).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), t0);
    }

    #[test]
    fn urls() {
        assert_eq!(lan_url("192.168.1.5", 8080), "http://192.168.1.5:8080/");
        assert_eq!(lan_url("fe80::1", 8080), "http://[fe80::1]:8080/");
    }
}
