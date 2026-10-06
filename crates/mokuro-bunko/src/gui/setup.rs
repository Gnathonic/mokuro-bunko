//! The wizard's two "write the configuration" steps, over the same code the CLI uses:
//!
//! * library server: `setup`'s questions as one form (bunko-core `Config`, the admin
//!   account in the database, a self-signed certificate when asked for);
//! * processor: `processor setup` (bunko-processor checks the account against the
//!   library, renders and writes processor.yaml mode 600), plus the processor settings
//!   the CLI wizard leaves at their defaults.

use bunko_core::config::{Config, DYNDNS_PROVIDERS, DynDnsConfig, REGISTRATION_MODES, SslConfig};
use bunko_core::{Role, storage};
use bunko_db::{Database, UserStatus, validate_password, validate_username};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AdminAccount {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct DynDnsForm {
    pub provider: String,
    pub domain: String,
    pub token: String,
    pub update_url: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SslForm {
    /// `off`, `self-signed` or `files`.
    pub mode: String,
    pub cert_file: String,
    pub key_file: String,
}

/// The library server wizard's answers.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ServerSetup {
    pub storage: String,
    pub host: String,
    pub port: u16,
    pub admin: Option<AdminAccount>,
    pub registration_mode: String,
    /// `lan`, `cloudflare`, `dyndns` or `reverse-proxy` (as `setup` asks).
    pub access: String,
    pub dyndns: DynDnsForm,
    pub ssl: SslForm,
    pub cors_origins: Vec<String>,
    pub local_ocr: bool,
    pub overwrite: bool,
}

/// Build the config the answers describe (every other setting at its default, as
/// `setup` does). Errors name the field.
pub fn server_config(form: &ServerSetup) -> Result<Config, String> {
    let mut config = Config::default();
    let storage_path = if form.storage.trim().is_empty() {
        storage::default_storage_path()
    } else {
        storage::expand_user(Path::new(form.storage.trim()))
    };
    if !storage_path.is_absolute() {
        return Err("storage: give the full path of the library folder".into());
    }
    if form.port == 0 {
        return Err("port: 1-65535".into());
    }
    let host = if form.host.trim().is_empty() {
        "0.0.0.0"
    } else {
        form.host.trim()
    };
    if host.parse::<std::net::IpAddr>().is_err() && host != "localhost" {
        return Err(format!("host: {host} is not an IP address"));
    }
    let mode = if form.registration_mode.is_empty() {
        "self"
    } else {
        form.registration_mode.as_str()
    };
    if !REGISTRATION_MODES.contains(&mode) {
        return Err(format!("registration mode: {mode}?"));
    }
    if let Some(a) = &form.admin {
        if let Some(e) = validate_username(&a.username) {
            return Err(format!("admin username: {e}"));
        }
        if let Some(e) = validate_password(&a.password) {
            return Err(format!("admin password: {e}"));
        }
    }
    config.ssl = match form.ssl.mode.as_str() {
        "" | "off" => SslConfig::default(),
        "self-signed" => SslConfig {
            enabled: true,
            auto_cert: true,
            ..SslConfig::default()
        },
        "files" => {
            for (label, p) in [
                ("certificate", &form.ssl.cert_file),
                ("private key", &form.ssl.key_file),
            ] {
                if !Path::new(p.trim()).is_file() {
                    return Err(format!("{label} file: {p} does not exist"));
                }
            }
            SslConfig {
                enabled: true,
                auto_cert: false,
                cert_file: form.ssl.cert_file.trim().into(),
                key_file: form.ssl.key_file.trim().into(),
            }
        }
        other => return Err(format!("HTTPS: {other}?")),
    };
    if form.access == "dyndns" {
        let d = &form.dyndns;
        let provider = if d.provider.is_empty() {
            "duckdns"
        } else {
            d.provider.as_str()
        };
        if !DYNDNS_PROVIDERS.contains(&provider) {
            return Err(format!("DynDNS provider: {provider}?"));
        }
        if d.domain.trim().is_empty() || d.token.trim().is_empty() {
            return Err("DynDNS: the domain and the token are needed".into());
        }
        if provider == "generic" && d.update_url.trim().is_empty() {
            return Err("DynDNS: the generic provider needs an update URL".into());
        }
        config.dyndns = DynDnsConfig {
            enabled: true,
            provider: provider.into(),
            token: d.token.trim().into(),
            domain: d.domain.trim().into(),
            update_url: if provider == "generic" {
                d.update_url.trim().into()
            } else {
                String::new()
            },
            ..DynDnsConfig::default()
        };
    }
    for o in &form.cors_origins {
        let o = o.trim();
        if !o.is_empty() && !config.cors.allowed_origins.iter().any(|x| x == o) {
            config.cors.allowed_origins.push(o.to_string());
        }
    }
    config.server.host = host.into();
    config.server.port = form.port;
    config.storage.base_path = storage_path;
    config.registration.mode = mode.into();
    config.ocr.local_processing = form.local_ocr && cfg!(feature = "ocr");
    Ok(config)
}

/// Write config.yaml, the admin account and the certificate. Returns notes.
pub fn write_server(form: &ServerSetup, config_path: &Path) -> Result<Value, String> {
    if config_path.exists() && !form.overwrite {
        return Err(format!(
            "{} already exists: tick \"Replace it\" to write a new one",
            config_path.display()
        ));
    }
    let config = server_config(form)?;
    let layout = config.storage.layout();
    layout
        .ensure_directories()
        .map_err(|e| format!("{}: {e}", config.storage.base_path.display()))?;
    crate::cfgfile::save(&config, config_path).map_err(|e| e.to_string())?;
    let mut notes = vec![format!("Saved {}", config_path.display())];
    if let Some(a) = &form.admin {
        let created = Database::open(layout.database())
            .map_err(|e| e.to_string())
            .and_then(|db| {
                db.create_user(
                    &a.username,
                    &a.password,
                    Role::Admin,
                    UserStatus::Active,
                    "",
                )
                .map_err(|e| e.to_string())
            });
        match created {
            Ok(_) => notes.push(format!("Created the admin account '{}'", a.username)),
            Err(e) => notes.push(format!(
                "Could not create the admin account '{}': {e} (sign in with the existing one, or use the admin panel)",
                a.username
            )),
        }
    }
    if config.ssl.enabled && config.ssl.auto_cert {
        let (cert, key) = bunko_server::tls::default_cert_paths();
        if !cert.exists() {
            bunko_server::tls::generate_self_signed(&cert, &key, "localhost")
                .map_err(|e| e.to_string())?;
            notes.push(format!(
                "Generated a self-signed certificate: {}",
                cert.display()
            ));
        }
    }
    if form.access == "cloudflare" {
        notes.push(
            "Start the Cloudflare tunnel from the admin panel (Connectivity) once the server runs."
                .into(),
        );
    }
    if form.access == "reverse-proxy" {
        notes.push(format!(
            "Point your reverse proxy at port {} of this machine.",
            config.server.port
        ));
    }
    Ok(json!({"notes": notes, "config": config_path, "storage": config.storage.base_path}))
}

/// The processor wizard's answers (and the processor settings page's).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ProcessorForm {
    pub url: String,
    pub username: String,
    /// Empty on the settings page: keep the one in the file.
    pub password: String,
    /// `true`, `false` or a certificate path.
    pub tls_verify: String,
    pub name: String,
    pub public_name: String,
    pub max_sessions: Option<u32>,
    pub storage: String,
    pub archive_memory_mb: Option<u64>,
    /// `processor.auto_update`: install the library's updates (opt-in).
    pub auto_update: bool,
    pub overwrite: bool,
}

#[cfg(feature = "ocr")]
pub use full::*;

#[cfg(feature = "ocr")]
mod full {
    use super::*;
    use bunko_processor::config::{DEFAULT_ARCHIVE_MEMORY_MB, TlsVerify, default_name};
    use bunko_processor::setup as ps;
    use serde_yaml_ng::{Mapping, Value as Yaml};

    fn tls(raw: &str) -> Result<TlsVerify, String> {
        ps::parse_tls_verify(if raw.is_empty() { "true" } else { raw }).map_err(|e| e.0)
    }

    /// Log in with the answers (registers nothing). The library's answer, or why not.
    pub async fn test_connection(form: &ProcessorForm) -> Result<Value, String> {
        let (url, note) = ps::normalize_url(&form.url).map_err(|e| e.0)?;
        if form.username.trim().is_empty() || form.password.is_empty() {
            return Err("the processor account's username and password are needed".into());
        }
        let v = ps::verify_account(
            &url,
            form.username.trim(),
            &form.password,
            &tls(&form.tls_verify)?,
        )
        .await
        .map_err(|e| e.0)?;
        let mut notes: Vec<String> = note.into_iter().collect();
        notes.extend(v.notes);
        Ok(json!({
            "url": url,
            "username": v.username,
            "role": v.role,
            "protocol": v.protocol,
            "library_version": v.library_version,
            "notes": notes,
        }))
    }

    /// The processor keys that differ from their defaults, added to `data`.
    fn put_processor_keys(data: &mut Mapping, form: &ProcessorForm) -> Result<(), String> {
        let mut p = match data.remove("processor") {
            Some(Yaml::Mapping(m)) => m,
            _ => Mapping::new(),
        };
        let hostname = default_name();
        let name = form.name.trim();
        if name.is_empty() || name == hostname {
            p.remove("name");
        } else {
            p.insert("name".into(), name.into());
        }
        let public = form.public_name.trim();
        if public.is_empty() {
            p.remove("public_name");
        } else {
            if public.chars().count() > bunko_processor::config::MAX_PUBLIC_NAME {
                return Err("public name: at most 64 characters".into());
            }
            p.insert("public_name".into(), public.into());
        }
        match form.max_sessions {
            Some(0) => return Err("max sessions: at least 1".into()),
            Some(1) | None => {
                p.remove("max_sessions");
            }
            Some(n) => {
                p.insert("max_sessions".into(), Yaml::Number(n.into()));
            }
        }
        let storage = form.storage.trim();
        let default_storage = bunko_processor::config::default_storage_path();
        if storage.is_empty() || Path::new(storage) == default_storage {
            p.remove("storage");
        } else {
            p.insert("storage".into(), storage.into());
        }
        match form.archive_memory_mb {
            Some(n) if n != DEFAULT_ARCHIVE_MEMORY_MB => {
                p.insert("archive_memory_mb".into(), Yaml::Number(n.into()));
            }
            _ => {
                p.remove("archive_memory_mb");
            }
        }
        if form.auto_update {
            p.insert("auto_update".into(), Yaml::Bool(true));
        } else {
            p.remove("auto_update");
        }
        if !p.is_empty() {
            data.insert("processor".into(), Yaml::Mapping(p));
        }
        Ok(())
    }

    fn render(data: &Mapping) -> String {
        format!(
            "{}{}",
            ps::HEADER,
            serde_yaml_ng::to_string(&Yaml::Mapping(data.clone())).unwrap_or_default()
        )
    }

    /// Check the account, then write processor.yaml. Nothing is written when the
    /// library refuses.
    pub async fn write_processor(form: &ProcessorForm, path: &Path) -> Result<Value, String> {
        if path.exists() && !form.overwrite {
            return Err(format!(
                "{} already exists: tick \"Replace it\" to write a new one",
                path.display()
            ));
        }
        let checked = test_connection(form).await?;
        let url = checked["url"].as_str().unwrap_or_default().to_string();
        let username = checked["username"].as_str().unwrap_or_default().to_string();
        let text = ps::render_config(
            &url,
            &username,
            &form.password,
            None,
            &tls(&form.tls_verify)?,
            &default_name(),
            false,
        );
        let mut data: Mapping = serde_yaml_ng::from_str(&text).map_err(|e| e.to_string())?;
        put_processor_keys(&mut data, form)?;
        let warning = ps::write_config(path, &render(&data), true).map_err(|e| e.0)?;
        Ok(json!({"connection": checked, "config": path, "warning": warning}))
    }

    /// processor.yaml as the settings page shows it (no password).
    pub fn read_processor(path: &Path) -> Result<Value, String> {
        if !path.is_file() {
            return Ok(json!({"exists": false, "config": path,
                "defaults": {"name": default_name(), "max_sessions": 1,
                    "storage": bunko_processor::config::default_storage_path(),
                    "archive_memory_mb": DEFAULT_ARCHIVE_MEMORY_MB}}));
        }
        let c = bunko_processor::load_processor_config(path).map_err(|e| e.0)?;
        let tls = match &c.library.tls_verify {
            TlsVerify::Yes => "true".to_string(),
            TlsVerify::No => "false".to_string(),
            TlsVerify::Cert(p) => p.display().to_string(),
        };
        Ok(json!({
            "exists": true,
            "config": path,
            "url": c.library.url,
            "username": c.library.username,
            "password_set": !c.library.password.is_empty(),
            "tls_verify": tls,
            "name": c.processor.name,
            "public_name": c.processor.public_name,
            "max_sessions": c.processor.max_sessions,
            "storage": c.processor.storage,
            "archive_memory_mb": c.processor.archive_memory_mb,
            "auto_update": c.processor.auto_update,
            "status": bunko_processor::status::read_status(&c.processor.storage),
        }))
    }

    /// Change processor.yaml in place (other keys, comments aside, kept). A changed
    /// login is checked against the library first.
    pub async fn update_processor(form: &ProcessorForm, path: &Path) -> Result<Value, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut data: Mapping = serde_yaml_ng::from_str(&raw).map_err(|e| e.to_string())?;
        let current = bunko_processor::load_processor_config(path).map_err(|e| e.0)?;
        let mut lib = match data.remove("library") {
            Some(Yaml::Mapping(m)) => m,
            _ => Mapping::new(),
        };
        let password = if form.password.is_empty() {
            current.library.password.clone()
        } else {
            form.password.clone()
        };
        let (url, _) = ps::normalize_url(&form.url).map_err(|e| e.0)?;
        let tls_verify = tls(&form.tls_verify)?;
        let login_changed = url != current.library.url
            || form.username.trim() != current.library.username
            || password != current.library.password
            || tls_verify != current.library.tls_verify;
        let mut checked = Value::Null;
        if login_changed {
            let mut f = ProcessorForm {
                url: url.clone(),
                username: form.username.clone(),
                password: password.clone(),
                tls_verify: form.tls_verify.clone(),
                ..ProcessorForm::default()
            };
            checked = test_connection(&f).await?;
            f.password.clear();
        }
        lib.insert("url".into(), url.into());
        lib.insert("username".into(), form.username.trim().into());
        if !form.password.is_empty() {
            lib.remove("password_file");
            lib.insert("password".into(), form.password.clone().into());
        }
        match &tls_verify {
            TlsVerify::Yes => {
                lib.remove("tls_verify");
            }
            TlsVerify::No => {
                lib.insert("tls_verify".into(), Yaml::Bool(false));
            }
            TlsVerify::Cert(p) => {
                lib.insert("tls_verify".into(), p.to_string_lossy().into_owned().into());
            }
        }
        data.insert("library".into(), Yaml::Mapping(lib));
        put_processor_keys(&mut data, form)?;
        let warning = ps::write_config(path, &render(&data), true).map_err(|e| e.0)?;
        Ok(
            json!({"connection": checked, "config": path, "warning": warning,
                  "restart_needed": true}),
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn processor_keys_only_when_not_default() {
            let mut data = Mapping::new();
            let form = ProcessorForm {
                name: default_name(),
                max_sessions: Some(1),
                archive_memory_mb: Some(DEFAULT_ARCHIVE_MEMORY_MB),
                ..ProcessorForm::default()
            };
            put_processor_keys(&mut data, &form).unwrap();
            assert!(data.get("processor").is_none());
            let form = ProcessorForm {
                name: "tower-x".into(),
                public_name: "Tower".into(),
                max_sessions: Some(3),
                storage: "/srv/p".into(),
                archive_memory_mb: Some(512),
                auto_update: true,
                ..ProcessorForm::default()
            };
            put_processor_keys(&mut data, &form).unwrap();
            let text = serde_yaml_ng::to_string(&Yaml::Mapping(data.clone())).unwrap();
            for k in [
                "name: tower-x",
                "public_name: Tower",
                "max_sessions: 3",
                "storage: /srv/p",
                "archive_memory_mb: 512",
                "auto_update: true",
            ] {
                assert!(text.contains(k), "{text}");
            }
            let bad = ProcessorForm {
                max_sessions: Some(0),
                ..ProcessorForm::default()
            };
            assert!(put_processor_keys(&mut data, &bad).is_err());
        }
    }
}

/// The directories and files a picker may show under `dir`: subdirectories, and
/// with `files`, the files whose extension is in `exts` (empty: none).
pub fn list_dir(dir: &Path, exts: &[String]) -> Result<Value, String> {
    let dir = std::fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !dir.is_dir() {
        return Err(format!("{} is not a folder", dir.display()));
    }
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let rd = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for e in rd.flatten().take(5000) {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        let is_dir = ft.is_dir() || (ft.is_symlink() && e.path().is_dir());
        if is_dir {
            dirs.push(name);
        } else if !exts.is_empty() {
            let ext = Path::new(&name)
                .extension()
                .map(|x| x.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if exts.iter().any(|x| x.eq_ignore_ascii_case(&ext)) {
                files.push(name);
            }
        }
    }
    dirs.sort_by_key(|a| a.to_lowercase());
    files.sort_by_key(|a| a.to_lowercase());
    let parent: Option<PathBuf> = dir.parent().map(Path::to_path_buf);
    Ok(
        json!({"path": dir, "parent": parent, "dirs": dirs, "files": files,
              "sep": std::path::MAIN_SEPARATOR.to_string()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_answers_make_a_config() {
        let dir = tempfile::tempdir().unwrap();
        let form = ServerSetup {
            storage: dir.path().join("lib").display().to_string(),
            host: "127.0.0.1".into(),
            port: 8099,
            admin: Some(AdminAccount {
                username: "admin".into(),
                password: "correct horse".into(),
            }),
            registration_mode: "invite".into(),
            access: "dyndns".into(),
            dyndns: DynDnsForm {
                provider: "duckdns".into(),
                domain: "x.duckdns.org".into(),
                token: "t".into(),
                update_url: String::new(),
            },
            cors_origins: vec!["https://a.example".into(), "".into()],
            ..ServerSetup::default()
        };
        let c = server_config(&form).unwrap();
        assert_eq!(c.server.port, 8099);
        assert_eq!(c.registration.mode, "invite");
        assert!(c.dyndns.enabled);
        // Added to the defaults (Mokuro Reader keeps working), blanks dropped.
        let origins = &c.cors.allowed_origins;
        assert_eq!(
            origins.last().map(String::as_str),
            Some("https://a.example")
        );
        assert!(origins.iter().any(|o| o == "https://reader.mokuro.app"));
        assert!(!origins.iter().any(String::is_empty));
        let path = dir.path().join("config.yaml");
        let out = write_server(&form, &path).unwrap();
        assert!(path.is_file());
        assert!(
            out["notes"]
                .to_string()
                .contains("Created the admin account")
        );
        // Not over an existing one unless asked.
        assert!(
            write_server(&form, &path)
                .unwrap_err()
                .contains("already exists")
        );
        let loaded = bunko_core::config::load_config(Some(&path)).unwrap();
        assert_eq!(loaded.server.port, 8099);

        let bad = ServerSetup {
            port: 8080,
            admin: Some(AdminAccount {
                username: "a".into(),
                password: "x".into(),
            }),
            ..ServerSetup::default()
        };
        assert!(server_config(&bad).unwrap_err().contains("admin username"));
        let relative = ServerSetup {
            storage: "lib".into(),
            port: 8080,
            ..ServerSetup::default()
        };
        assert!(server_config(&relative).is_err());
    }

    #[test]
    fn picker_lists_folders_and_chosen_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Manga")).unwrap();
        std::fs::create_dir(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join("cert.PEM"), "x").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        let v = list_dir(dir.path(), &[]).unwrap();
        assert_eq!(v["dirs"], json!(["Manga"]));
        assert_eq!(v["files"], json!([]));
        let v = list_dir(dir.path(), &["pem".into()]).unwrap();
        assert_eq!(v["files"], json!(["cert.PEM"]));
        assert!(list_dir(&dir.path().join("nope"), &[]).is_err());
    }
}
