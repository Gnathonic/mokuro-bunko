//! `processor.yaml`: the whole configuration of a processor (0.5.2
//! `processor/config.py`, minus the Python environment keys).
//!
//! A processor owns no library, no users and no settings the library owns: where to
//! log in, what to call itself, how many pipelines its hardware holds, where its
//! storage is and how much RAM queued archives may use. Everything about a job
//! arrives in an op.

use std::path::{Path, PathBuf};

use serde_yaml_ng::{Mapping, Value};

/// The spool's default RAM budget (`processor.archive_memory_mb`).
pub const DEFAULT_ARCHIVE_MEMORY_MB: u64 = 2048;
/// The library stores at most this much of a processor's (public) name.
pub const MAX_PUBLIC_NAME: usize = 64;
/// Overrides `library.password` / `library.password_file` (for containers and
/// service managers that inject secrets through the environment).
pub const PASSWORD_ENV: &str = "MOKURO_PROCESSOR_PASSWORD";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ConfigError(pub String);

fn err(message: impl Into<String>) -> ConfigError {
    ConfigError(message.into())
}

/// `library.tls_verify`: verify normally, skip verification, or trust one
/// certificate file (a CA, or the library's own self-signed certificate).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TlsVerify {
    #[default]
    Yes,
    No,
    Cert(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySettings {
    /// The library's URL, trailing `/` stripped; a path prefix is kept and prepended
    /// to every request path.
    pub url: String,
    pub username: String,
    pub password: String,
    pub tls_verify: TlsVerify,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessorSettings {
    /// How the library shows this machine to admins; the profile key. Default: hostname.
    pub name: String,
    /// What the public queue page calls it (None → a numbered alias).
    pub public_name: Option<String>,
    pub max_sessions: u32,
    /// Logs, volume workspaces, archives too big for RAM, the status file.
    pub storage: PathBuf,
    /// RAM queued archives may use across all sessions; 0 = always disk.
    pub archive_memory_mb: u64,
    /// Follow the library: when it reports a newer version, finish the running volume,
    /// install exactly that release (binary, backend pack, models) and restart. Opt-in;
    /// never a downgrade.
    pub auto_update: bool,
}

impl Default for ProcessorSettings {
    fn default() -> Self {
        ProcessorSettings {
            name: default_name(),
            public_name: None,
            max_sessions: 1,
            storage: default_storage_path(),
            archive_memory_mb: DEFAULT_ARCHIVE_MEMORY_MB,
            auto_update: false,
        }
    }
}

/// `update:`: where releases come from (only `processor.auto_update` uses it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateSettings {
    /// The latest release's `release.json` (a mirror or fork); the exact version's is
    /// derived from it (`bunko_update::auto::release_manifest_url`).
    pub manifest_url: String,
    /// A fork's or test release's ed25519 key (base64); empty: the compiled-in key.
    /// Only ever read from this file.
    pub public_key: String,
}

pub const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/Gnathonic/mokuro-bunko/releases/latest/download/release.json";

impl Default for UpdateSettings {
    fn default() -> Self {
        UpdateSettings {
            manifest_url: DEFAULT_MANIFEST_URL.into(),
            public_key: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessorConfig {
    pub library: LibrarySettings,
    pub processor: ProcessorSettings,
    pub update: UpdateSettings,
}

/// The machine's hostname (`processor.name`'s default).
pub fn default_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "processor".to_string())
}

pub(crate) fn home_dir() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `%LOCALAPPDATA%\mokuro-bunko-processor` on Windows (`~/AppData/Local` when unset),
/// `$XDG_DATA_HOME/mokuro-bunko-processor` (`~/.local/share`) elsewhere.
pub fn default_storage_path() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join("AppData").join("Local"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".local").join("share"))
    };
    base.join("mokuro-bunko-processor")
}

/// `~` and `~/…` expanded against the home directory.
pub fn expand_user(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return home_dir().join(rest);
    }
    PathBuf::from(path)
}

fn section<'a>(
    data: &'a Mapping,
    name: &str,
    allowed: &[&str],
) -> Result<Option<&'a Mapping>, ConfigError> {
    let raw = match data.get(name) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Mapping(m)) => m,
        Some(_) => return Err(err(format!("{name}: must be a block of settings"))),
    };
    let mut unknown: Vec<String> = raw
        .keys()
        .map(key_text)
        .filter(|k| !allowed.contains(&k.as_str()))
        .collect();
    unknown.sort();
    if let Some(first) = unknown.first() {
        return Err(err(format!(
            "{name}.{first}: no such setting (expected one of {})",
            allowed.join(", ")
        )));
    }
    Ok(Some(raw))
}

fn key_text(key: &Value) -> String {
    match key {
        Value::String(s) => s.clone(),
        other => serde_yaml_ng::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

fn get<'a>(map: Option<&'a Mapping>, key: &str) -> Option<&'a Value> {
    map.and_then(|m| m.get(key)).filter(|v| !v.is_null())
}

fn text(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => serde_yaml_ng::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

/// Read and validate one `processor.yaml`.
pub fn load_processor_config(path: &Path) -> Result<ProcessorConfig, ConfigError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| err(format!("could not read {}: {e}", path.display())))?;
    parse_processor_config(&raw, path)
}

/// [`load_processor_config`] over text already read (`origin` names it in errors).
pub fn parse_processor_config(raw: &str, origin: &Path) -> Result<ProcessorConfig, ConfigError> {
    let value: Value = if raw.trim().is_empty() {
        Value::Mapping(Mapping::new())
    } else {
        serde_yaml_ng::from_str(raw)
            .map_err(|e| err(format!("{} is not valid YAML: {e}", origin.display())))?
    };
    let data = match value {
        Value::Null => Mapping::new(),
        Value::Mapping(m) => m,
        _ => {
            return Err(err(format!(
                "{} must be a block of settings",
                origin.display()
            )));
        }
    };
    let mut unknown: Vec<String> = data
        .keys()
        .map(key_text)
        .filter(|k| !matches!(k.as_str(), "library" | "processor" | "ocr" | "update"))
        .collect();
    unknown.sort();
    if let Some(first) = unknown.first() {
        return Err(err(format!("{first}: no such section")));
    }

    let library = section(
        &data,
        "library",
        &["url", "username", "password", "password_file", "tls_verify"],
    )?;
    let url = text(get(library, "url"));
    let username = text(get(library, "username"));
    for (key, value) in [("url", &url), ("username", &username)] {
        if value.trim().is_empty() {
            return Err(err(format!(
                "library.{key} is required: a processor logs in like any client"
            )));
        }
    }
    let password = password(library, origin)?;
    let tls_verify = match get(library, "tls_verify") {
        None | Some(Value::Bool(true)) => TlsVerify::Yes,
        Some(Value::Bool(false)) => TlsVerify::No,
        Some(Value::String(s)) => TlsVerify::Cert(expand_user(s)),
        Some(_) => {
            return Err(err(
                "library.tls_verify: true, false, or the path to the library's certificate",
            ));
        }
    };

    let processor = section(
        &data,
        "processor",
        &[
            "name",
            "public_name",
            "max_sessions",
            "storage",
            "archive_memory_mb",
            "auto_update",
        ],
    )?;
    let auto_update = match get(processor, "auto_update") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(err("processor.auto_update: true or false")),
    };
    let update = section(&data, "update", &["manifest_url", "public_key"])?;
    let update = UpdateSettings {
        manifest_url: {
            let u = text(get(update, "manifest_url"));
            if u.trim().is_empty() {
                DEFAULT_MANIFEST_URL.to_string()
            } else {
                u.trim().to_string()
            }
        },
        public_key: text(get(update, "public_key")).trim().to_string(),
    };
    let public_name = match get(processor, "public_name") {
        None => None,
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.chars().count() > MAX_PUBLIC_NAME {
                return Err(err(format!(
                    "processor.public_name: at most {MAX_PUBLIC_NAME} characters"
                )));
            }
            (!s.is_empty()).then(|| s.to_string())
        }
        Some(_) => return Err(err("processor.public_name: text")),
    };
    let max_sessions = match get(processor, "max_sessions") {
        None => 1,
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64))
            .ok_or_else(|| err("processor.max_sessions: a whole number"))?,
        Some(Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| err("processor.max_sessions: a whole number"))?,
        Some(_) => return Err(err("processor.max_sessions: a whole number")),
    };
    if max_sessions < 1 {
        return Err(err("processor.max_sessions: at least 1"));
    }
    let archive_memory_mb = match get(processor, "archive_memory_mb") {
        None => DEFAULT_ARCHIVE_MEMORY_MB,
        Some(Value::Number(n)) if n.as_u64().is_some() => n.as_u64().unwrap_or(0),
        Some(_) => {
            return Err(err(
                "processor.archive_memory_mb: a whole number of megabytes, 0 or more (0 keeps every archive on disk)",
            ));
        }
    };
    let name = {
        let n = text(get(processor, "name"));
        if n.trim().is_empty() {
            default_name()
        } else {
            n
        }
    };
    let storage = match get(processor, "storage") {
        Some(v) if !text(Some(v)).is_empty() => expand_user(&text(Some(v))),
        _ => default_storage_path(),
    };

    // `ocr.backend` chose a torch build in 0.5; the Rust processor ships its engines,
    // so the key is accepted (old files keep loading) and ignored.
    if let Some(ocr) = section(&data, "ocr", &["backend"])?
        && ocr.contains_key("backend")
    {
        tracing::warn!(
            "ocr.backend in {} is ignored: this processor needs no OCR environments",
            origin.display()
        );
    }

    Ok(ProcessorConfig {
        library: LibrarySettings {
            url: url.trim_end_matches('/').to_string(),
            username,
            password,
            tls_verify,
        },
        processor: ProcessorSettings {
            name,
            public_name,
            max_sessions: u32::try_from(max_sessions).unwrap_or(u32::MAX),
            storage,
            archive_memory_mb,
            auto_update,
        },
        update,
    })
}

/// The password: `$MOKURO_PROCESSOR_PASSWORD`, else `library.password`, else the first
/// line of `library.password_file` (relative to the config's directory).
fn password(library: Option<&Mapping>, origin: &Path) -> Result<String, ConfigError> {
    if let Ok(value) = std::env::var(PASSWORD_ENV)
        && !value.trim().is_empty()
    {
        return Ok(value);
    }
    let inline = text(get(library, "password"));
    if !inline.trim().is_empty() {
        return Ok(inline);
    }
    let file = text(get(library, "password_file"));
    if !file.trim().is_empty() {
        let mut path = expand_user(file.trim());
        if path.is_relative()
            && let Some(dir) = origin.parent()
        {
            path = dir.join(path);
        }
        let content = std::fs::read_to_string(&path).map_err(|e| {
            err(format!(
                "library.password_file: could not read {}: {e}",
                path.display()
            ))
        })?;
        let line = content
            .lines()
            .next()
            .unwrap_or("")
            .trim_end_matches('\r')
            .to_string();
        if line.trim().is_empty() {
            return Err(err(format!(
                "library.password_file: {} is empty",
                path.display()
            )));
        }
        return Ok(line);
    }
    Err(err(
        "library.password is required: a processor logs in like any client",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<ProcessorConfig, ConfigError> {
        parse_processor_config(text, Path::new("processor.yaml"))
    }

    const MINIMAL: &str =
        "library:\n  url: https://lib.example:8080/\n  username: gpu\n  password: pw\n";

    #[test]
    fn minimal_config_takes_defaults() {
        let c = parse(MINIMAL).unwrap();
        assert_eq!(c.library.url, "https://lib.example:8080");
        assert_eq!(c.library.tls_verify, TlsVerify::Yes);
        assert_eq!(c.processor.max_sessions, 1);
        assert_eq!(c.processor.archive_memory_mb, 2048);
        assert_eq!(c.processor.name, default_name());
        assert!(c.processor.storage.ends_with("mokuro-bunko-processor"));
    }

    #[test]
    fn unknown_keys_are_named() {
        assert_eq!(parse("extra: 1\n").unwrap_err().0, "extra: no such section");
        let e = parse(&format!("{MINIMAL}processor:\n  nmae: x\n")).unwrap_err();
        assert_eq!(
            e.0,
            "processor.nmae: no such setting (expected one of name, public_name, max_sessions, storage, archive_memory_mb, auto_update)"
        );
        assert_eq!(
            parse("library:\n  url: x\n  username: u\n").unwrap_err().0,
            "library.password is required: a processor logs in like any client"
        );
    }

    #[test]
    fn values_are_validated() {
        let bad = |extra: &str| parse(&format!("{MINIMAL}{extra}")).unwrap_err().0;
        assert_eq!(
            bad("processor:\n  max_sessions: 0\n"),
            "processor.max_sessions: at least 1"
        );
        assert_eq!(
            bad("processor:\n  max_sessions: many\n"),
            "processor.max_sessions: a whole number"
        );
        assert!(
            bad("processor:\n  archive_memory_mb: -1\n").starts_with("processor.archive_memory_mb")
        );
        assert!(
            bad("processor:\n  archive_memory_mb: true\n")
                .starts_with("processor.archive_memory_mb")
        );
        assert_eq!(
            bad("processor:\n  public_name: 3\n"),
            "processor.public_name: text"
        );
        let long = "x".repeat(65);
        assert!(bad(&format!("processor:\n  public_name: {long}\n")).contains("at most 64"));
        let ok = parse(&format!(
            "{MINIMAL}  tls_verify: false\nprocessor:\n  name: tower\n  public_name: '  '\n  max_sessions: 2\n  archive_memory_mb: 0\n  storage: /data/p\nocr:\n  backend: cuda\n"
        ))
        .unwrap();
        assert_eq!(ok.library.tls_verify, TlsVerify::No);
        assert_eq!(ok.processor.public_name, None);
        assert_eq!(ok.processor.max_sessions, 2);
        assert_eq!(ok.processor.archive_memory_mb, 0);
        assert_eq!(ok.processor.storage, PathBuf::from("/data/p"));
        assert_eq!(ok.processor.name, "tower");
        assert!(!ok.processor.auto_update, "opt-in");
        assert_eq!(ok.update, UpdateSettings::default());
    }

    #[test]
    fn auto_update_and_update_section() {
        let c = parse(&format!(
            "{MINIMAL}processor:\n  auto_update: true\nupdate:\n  manifest_url: http://127.0.0.1:8000/release.json\n  public_key: abc=\n"
        ))
        .unwrap();
        assert!(c.processor.auto_update);
        assert_eq!(c.update.manifest_url, "http://127.0.0.1:8000/release.json");
        assert_eq!(c.update.public_key, "abc=");
        assert_eq!(
            parse(&format!("{MINIMAL}processor:\n  auto_update: yes please\n"))
                .unwrap_err()
                .0,
            "processor.auto_update: true or false"
        );
        assert!(
            parse(&format!("{MINIMAL}update:\n  channel: x\n"))
                .unwrap_err()
                .0
                .starts_with("update.channel: no such setting")
        );
    }

    #[test]
    fn password_file_is_read_relative_to_the_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("secret"), "s3cret\nignored\n").unwrap();
        let cfg = dir.path().join("processor.yaml");
        std::fs::write(
            &cfg,
            "library:\n  url: http://h\n  username: u\n  password_file: secret\n",
        )
        .unwrap();
        // The env override would win; make sure it is not set by the environment.
        if std::env::var(PASSWORD_ENV).is_err() {
            assert_eq!(
                load_processor_config(&cfg).unwrap().library.password,
                "s3cret"
            );
        }
    }
}
