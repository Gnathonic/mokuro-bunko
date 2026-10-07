//! `config.yaml` loading, validation, env overrides and saving — a drop-in for 0.5.2's
//! `config.py`. Unknown sections and keys from newer or older versions are preserved on
//! save rather than silently dropped.

use crate::generations::{self, Generation, ParsedGenerations};
use crate::storage::{self, StorageLayout};
use serde_json::{Map, Value, json};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    UnknownKey(String),
    #[error("could not read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid YAML in {path}: {message}")]
    Yaml { path: PathBuf, message: String },
}

impl From<generations::GenerationError> for ConfigError {
    fn from(e: generations::GenerationError) -> Self {
        ConfigError::Invalid(e.message)
    }
}

fn invalid(msg: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(msg.into())
}

pub const REGISTRATION_MODES: &[&str] = &["disabled", "self", "invite", "approval"];
pub const DEFAULT_ROLES: &[&str] = &["registered", "uploader", "inviter", "editor"];
pub const QUEUE_DISPLAY_LEVELS: &[&str] = &["minimal", "normal", "detailed"];
/// `ocr.backend`: which devices local OCR may use and so which backend pack
/// `install-ocr` and the Docker images fetch (`cuda`: NVIDIA, `rocm`: AMD, `cpu`, `auto`:
/// the hardware decides; `skip`: no local OCR). `webgpu`, `directml`, `coreml` are the
/// unreleased ONNX Runtime providers.
pub const OCR_BACKENDS: &[&str] = &[
    "auto", "cuda", "rocm", "webgpu", "directml", "coreml", "cpu", "skip",
];
pub const DYNDNS_PROVIDERS: &[&str] = &["duckdns", "generic"];
pub const MAX_OCR_CONCURRENCY: u32 = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub trusted_proxies: Vec<String>,
    /// New in 0.7: tokio worker threads (0 = min(cores, 4)).
    pub threads: u32,
    /// New in 0.7: byte budget for in-memory caches, in MiB.
    pub cache_mb: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8080,
            trusted_proxies: vec![],
            threads: 0,
            cache_mb: 32,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StorageConfig {
    pub base_path: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            base_path: storage::default_storage_path(),
        }
    }
}

impl StorageConfig {
    pub fn layout(&self) -> StorageLayout {
        StorageLayout::new(self.base_path.clone())
    }
    pub fn library_path(&self) -> PathBuf {
        self.layout().library()
    }
    pub fn inbox_path(&self) -> PathBuf {
        self.layout().inbox()
    }
    pub fn users_path(&self) -> PathBuf {
        self.layout().users()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegistrationConfig {
    pub mode: String,
    pub default_role: String,
    pub allow_anonymous_browse: bool,
    pub allow_anonymous_download: bool,
    pub require_login: bool,
}

impl Default for RegistrationConfig {
    fn default() -> Self {
        Self {
            mode: "self".into(),
            default_role: "registered".into(),
            allow_anonymous_browse: true,
            allow_anonymous_download: true,
            require_login: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CorsConfig {
    pub enabled: bool,
    pub allowed_origins: Vec<String>,
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_origins: vec![
                "https://reader.mokuro.app".into(),
                "http://localhost:5173".into(),
                "http://localhost:*".into(),
                "http://127.0.0.1:*".into(),
            ],
            allow_credentials: true,
        }
    }
}

impl CorsConfig {
    pub fn is_origin_allowed(&self, origin: &str) -> bool {
        self.enabled
            && self
                .allowed_origins
                .iter()
                .any(|p| Self::matches_pattern(origin, p))
    }

    /// Exact match, or `scheme://host:*` matching any numeric port (0.5.2 semantics).
    pub fn matches_pattern(origin: &str, pattern: &str) -> bool {
        if !pattern.contains('*') {
            return origin == pattern;
        }
        if let Some(prefix) = pattern.strip_suffix(":*")
            && let Some(rest) = origin.strip_prefix(prefix)
            && let Some(port) = rest.strip_prefix(':')
        {
            return !port.is_empty() && port.chars().all(|c| c.is_ascii_digit());
        }
        false
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SslConfig {
    pub enabled: bool,
    pub auto_cert: bool,
    pub cert_file: String,
    pub key_file: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AdminConfig {
    pub enabled: bool,
    pub path: String,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "/_admin".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatalogConfig {
    pub enabled: bool,
    pub reader_url: String,
    pub use_as_homepage: bool,
    pub enrich_community: bool,
}

impl Default for CatalogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            reader_url: "https://reader.mokuro.app".into(),
            use_as_homepage: false,
            enrich_community: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueueConfig {
    pub show_in_nav: bool,
    pub public_access: bool,
    pub display: String,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            show_in_nav: false,
            public_access: true,
            display: "normal".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DatabaseConfig {
    pub busy_timeout_ms: u32,
    pub lock_retries: u32,
    pub retry_initial_delay_seconds: f64,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            busy_timeout_ms: 5000,
            lock_retries: 5,
            retry_initial_delay_seconds: 0.05,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpgradeConfig {
    /// Replace older primary OCR with the current primary recipe (0.6 design, §3).
    pub enabled: bool,
    /// Recipe families to replace: engine ids or `mokuro-legacy`.
    pub replace: Vec<String>,
}

impl Default for UpgradeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            replace: vec!["mokuro-legacy".into(), "mokuro".into()],
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OcrConfig {
    pub backend: String,
    pub poll_interval: u32,
    pub concurrency: u32,
    pub sessions: bool,
    pub local_processing: bool,
    pub autobench: bool,
    pub generations: Vec<Generation>,
    pub upgrade: UpgradeConfig,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            backend: "auto".into(),
            poll_interval: 30,
            concurrency: 1,
            sessions: true,
            local_processing: true,
            autobench: true,
            generations: generations::default_generations(),
            upgrade: UpgradeConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DynDnsConfig {
    pub enabled: bool,
    pub provider: String,
    pub token: String,
    pub domain: String,
    pub update_url: String,
    pub interval: u32,
}

impl Default for DynDnsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "duckdns".into(),
            token: String::new(),
            domain: String::new(),
            update_url: String::new(),
            interval: 300,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateConfig {
    /// Check for new releases in the background.
    pub check: bool,
    /// `stable` or `prerelease`.
    pub channel: String,
    /// Release manifest URL (override for mirrors/testing).
    pub manifest_url: String,
    /// Install a newer release on the channel by itself, at a quiet moment, and
    /// restart (self-managed installs only; opt-in).
    pub auto: bool,
    /// The ed25519 key (base64) releases are checked against, for a fork or a test
    /// release; empty: the key compiled into this build. Read from the config FILE only:
    /// no environment variable, `config set`, admin API or app page sets it.
    pub public_key: String,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check: true,
            channel: "stable".into(),
            manifest_url:
                "https://github.com/Gnathonic/mokuro-bunko/releases/latest/download/release.json"
                    .into(),
            auto: false,
            public_key: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub registration: RegistrationConfig,
    pub cors: CorsConfig,
    pub ssl: SslConfig,
    pub admin: AdminConfig,
    pub catalog: CatalogConfig,
    pub queue: QueueConfig,
    pub database: DatabaseConfig,
    pub ocr: OcrConfig,
    pub dyndns: DynDnsConfig,
    pub update: UpdateConfig,
    /// Notes about migrated settings (removed engines/detectors), for logs and admin UI.
    pub warnings: Vec<String>,
    /// Sections/keys this version does not know, preserved verbatim on save.
    pub extra: Map<String, Value>,
}

// --- value helpers ----------------------------------------------------------------------

fn section(data: &Map<String, Value>, name: &str) -> Result<Map<String, Value>, ConfigError> {
    match data.get(name) {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => Ok(m.clone()),
        Some(_) => Err(invalid(format!(
            "config section '{name}' must be a mapping"
        ))),
    }
}

fn get_bool(
    m: &Map<String, Value>,
    sec: &str,
    key: &str,
    default: bool,
) -> Result<bool, ConfigError> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::Number(n)) => Ok(n.as_f64().unwrap_or(0.0) != 0.0),
        Some(Value::String(s)) => {
            parse_bool(s).ok_or_else(|| invalid(format!("{sec}.{key}: Invalid boolean value: {s}")))
        }
        Some(other) => Err(invalid(format!(
            "{sec}.{key}: expected a boolean, got {other}"
        ))),
    }
}

fn get_int(m: &Map<String, Value>, sec: &str, key: &str, default: i64) -> Result<i64, ConfigError> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
            .ok_or_else(|| invalid(format!("{sec}.{key}: expected a whole number, got {n}"))),
        Some(Value::String(s)) => s
            .trim()
            .parse()
            .map_err(|_| invalid(format!("{sec}.{key}: expected a whole number, got '{s}'"))),
        Some(Value::Bool(b)) => Ok(*b as i64),
        Some(other) => Err(invalid(format!(
            "{sec}.{key}: expected a whole number, got {other}"
        ))),
    }
}

fn get_float(
    m: &Map<String, Value>,
    sec: &str,
    key: &str,
    default: f64,
) -> Result<f64, ConfigError> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) => Ok(n.as_f64().unwrap_or(default)),
        Some(Value::String(s)) => s
            .trim()
            .parse()
            .map_err(|_| invalid(format!("{sec}.{key}: expected a number, got '{s}'"))),
        Some(other) => Err(invalid(format!(
            "{sec}.{key}: expected a number, got {other}"
        ))),
    }
}

fn get_str(m: &Map<String, Value>, key: &str, default: &str) -> String {
    match m.get(key) {
        None | Some(Value::Null) => default.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn get_list(
    m: &Map<String, Value>,
    sec: &str,
    key: &str,
    default: &[String],
) -> Result<Vec<String>, ConfigError> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(default.to_vec()),
        Some(Value::Array(a)) => Ok(a
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect()),
        Some(Value::String(s)) => Ok(split_list(s)),
        Some(other) => Err(invalid(format!(
            "{sec}.{key}: expected a list, got {other}"
        ))),
    }
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
}

pub fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Some(true),
        "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

/// Take the known keys out of a section map; whatever is left is preserved as extra.
fn leftovers(mut m: Map<String, Value>, known: &[&str]) -> Map<String, Value> {
    for k in known {
        m.remove(*k);
    }
    m
}

const RETIRED_OCR_KEYS: &[(&str, &str)] = &[
    (
        "engines",
        "each engine becomes a generation row with its own name",
    ),
    ("detector", "the detector is now per generation"),
    ("patch_budget", "the patch budget is now per generation"),
];
const REMOVED_OCR_KEYS: &[(&str, &str)] = &[(
    "char_map",
    "the character-map system was removed (no per-character placement mode produced output worth using; \
     readers lay characters on a uniform grid) — delete the key",
)];

fn reject_retired_ocr_keys(ocr: &Map<String, Value>) -> Result<(), ConfigError> {
    let gone: Vec<String> = REMOVED_OCR_KEYS
        .iter()
        .filter(|(k, _)| ocr.contains_key(*k))
        .map(|(k, why)| format!("ocr.{k}: {why}"))
        .collect();
    if !gone.is_empty() {
        return Err(invalid(gone.join("; ")));
    }
    let present: Vec<&(&str, &str)> = RETIRED_OCR_KEYS
        .iter()
        .filter(|(k, _)| ocr.contains_key(*k))
        .collect();
    if present.is_empty() {
        return Ok(());
    }
    let named: Vec<String> = present.iter().map(|(k, _)| format!("ocr.{k}")).collect();
    let reasons: Vec<String> = present
        .iter()
        .map(|(k, why)| format!("ocr.{k}: {why}"))
        .collect();
    Err(invalid(format!(
        "{} was replaced by ocr.generations, a list of named OCR recipes ({}) — rewrite the ocr section as \
         generations, e.g. generations: [{{name: hayai-nova, engine: hayai-nova, primary: true}}]",
        named.join(", "),
        reasons.join("; ")
    )))
}

// --- Config ------------------------------------------------------------------------------

impl Config {
    /// Build from the parsed YAML document (a JSON-model value).
    pub fn from_value(data: &Value) -> Result<Config, ConfigError> {
        let empty = Map::new();
        let data = match data {
            Value::Object(m) => m,
            Value::Null => &empty,
            _ => return Err(invalid("config file must be a mapping of sections")),
        };
        let mut c = Config::default();

        let s = section(data, "server")?;
        c.server.host = get_str(&s, "host", &c.server.host);
        let port = get_int(&s, "server", "port", 8080)?;
        if !(0..65536).contains(&port) {
            return Err(invalid(format!("Invalid port: {port}")));
        }
        c.server.port = port as u16;
        c.server.trusted_proxies = get_list(&s, "server", "trusted_proxies", &[])?;
        validate_trusted_proxies(&c.server.trusted_proxies)?;
        c.server.threads = get_int(&s, "server", "threads", 0)?.clamp(0, 256) as u32;
        c.server.cache_mb = get_int(&s, "server", "cache_mb", 32)?.clamp(1, 65536) as u32;
        let server_extra = leftovers(
            s,
            &["host", "port", "trusted_proxies", "threads", "cache_mb"],
        );

        let s = section(data, "storage")?;
        if let Some(p) = s.get("base_path").filter(|v| !v.is_null()) {
            let raw = match p {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            c.storage.base_path = storage::expand_user(Path::new(&raw));
        }
        let storage_extra = leftovers(s, &["base_path"]);

        let mut s = section(data, "registration")?;
        if !s.contains_key("allow_anonymous_browse") && !s.contains_key("allow_anonymous_download")
        {
            let require_login = get_bool(&s, "registration", "require_login", false)?;
            s.insert("allow_anonymous_browse".into(), Value::Bool(!require_login));
            s.insert(
                "allow_anonymous_download".into(),
                Value::Bool(!require_login),
            );
        }
        c.registration.mode = get_str(&s, "mode", "self");
        if !REGISTRATION_MODES.contains(&c.registration.mode.as_str()) {
            return Err(invalid(format!(
                "Invalid registration mode: {}",
                c.registration.mode
            )));
        }
        c.registration.default_role = get_str(&s, "default_role", "registered");
        if c.registration.default_role == "writer" {
            c.registration.default_role = "uploader".into();
        }
        if !DEFAULT_ROLES.contains(&c.registration.default_role.as_str()) {
            return Err(invalid(format!(
                "Invalid default role: {}. Must be one of: ('registered', 'uploader', 'inviter', 'editor')",
                c.registration.default_role
            )));
        }
        c.registration.allow_anonymous_browse =
            get_bool(&s, "registration", "allow_anonymous_browse", true)?;
        c.registration.allow_anonymous_download =
            get_bool(&s, "registration", "allow_anonymous_download", true)?;
        c.registration.require_login = get_bool(&s, "registration", "require_login", false)?;
        let registration_extra = leftovers(
            s,
            &[
                "mode",
                "default_role",
                "allow_anonymous_browse",
                "allow_anonymous_download",
                "require_login",
            ],
        );

        let s = section(data, "cors")?;
        c.cors.enabled = get_bool(&s, "cors", "enabled", true)?;
        c.cors.allowed_origins = get_list(
            &s,
            "cors",
            "allowed_origins",
            &CorsConfig::default().allowed_origins,
        )?;
        c.cors.allow_credentials = get_bool(&s, "cors", "allow_credentials", true)?;
        let cors_extra = leftovers(s, &["enabled", "allowed_origins", "allow_credentials"]);

        let s = section(data, "ssl")?;
        c.ssl.enabled = get_bool(&s, "ssl", "enabled", false)?;
        c.ssl.auto_cert = get_bool(&s, "ssl", "auto_cert", false)?;
        c.ssl.cert_file = get_str(&s, "cert_file", "");
        c.ssl.key_file = get_str(&s, "key_file", "");
        if c.ssl.enabled
            && !c.ssl.auto_cert
            && (c.ssl.cert_file.is_empty() || c.ssl.key_file.is_empty())
        {
            return Err(invalid(
                "SSL enabled but cert_file and key_file not provided. Either provide cert paths or set auto_cert: true",
            ));
        }
        let ssl_extra = leftovers(s, &["enabled", "auto_cert", "cert_file", "key_file"]);

        let s = section(data, "admin")?;
        c.admin.enabled = get_bool(&s, "admin", "enabled", true)?;
        c.admin.path = get_str(&s, "path", "/_admin");
        let admin_extra = leftovers(s, &["enabled", "path"]);

        let s = section(data, "catalog")?;
        c.catalog.enabled = get_bool(&s, "catalog", "enabled", false)?;
        c.catalog.reader_url = get_str(&s, "reader_url", "https://reader.mokuro.app");
        c.catalog.use_as_homepage = get_bool(&s, "catalog", "use_as_homepage", false)?;
        c.catalog.enrich_community = get_bool(&s, "catalog", "enrich_community", true)?;
        let catalog_extra = leftovers(
            s,
            &[
                "enabled",
                "reader_url",
                "use_as_homepage",
                "enrich_community",
            ],
        );

        let s = section(data, "queue")?;
        c.queue.show_in_nav = get_bool(&s, "queue", "show_in_nav", false)?;
        c.queue.public_access = get_bool(&s, "queue", "public_access", true)?;
        c.queue.display = get_str(&s, "display", "normal");
        if !QUEUE_DISPLAY_LEVELS.contains(&c.queue.display.as_str()) {
            c.warnings.push(format!(
                "queue.display '{}' is not one of {}; using 'normal'",
                c.queue.display,
                QUEUE_DISPLAY_LEVELS.join(", ")
            ));
            c.queue.display = "normal".into();
        }
        let queue_extra = leftovers(s, &["show_in_nav", "public_access", "display"]);

        let s = section(data, "database")?;
        c.database.busy_timeout_ms =
            get_int(&s, "database", "busy_timeout_ms", 5000)?.max(0) as u32;
        c.database.lock_retries = get_int(&s, "database", "lock_retries", 5)?.max(0) as u32;
        c.database.retry_initial_delay_seconds =
            get_float(&s, "database", "retry_initial_delay_seconds", 0.05)?;
        c.database.validate()?;
        let database_extra = leftovers(
            s,
            &[
                "busy_timeout_ms",
                "lock_retries",
                "retry_initial_delay_seconds",
            ],
        );

        let s = section(data, "ocr")?;
        reject_retired_ocr_keys(&s)?;
        c.ocr.backend = get_str(&s, "backend", "auto");
        c.ocr.poll_interval = get_int(&s, "ocr", "poll_interval", 30)?.max(0) as u32;
        c.ocr.concurrency = validate_concurrency(get_int(&s, "ocr", "concurrency", 1)?)?;
        c.ocr.sessions = get_bool(&s, "ocr", "sessions", true)?;
        c.ocr.local_processing = get_bool(&s, "ocr", "local_processing", true)?;
        c.ocr.autobench = get_bool(&s, "ocr", "autobench", true)?;
        let parsed =
            generations::parse_generation_list(s.get("generations").unwrap_or(&Value::Null))?;
        c.apply_parsed_generations(parsed);
        if let Some(up) = s.get("upgrade").and_then(Value::as_object) {
            c.ocr.upgrade.enabled = get_bool(up, "ocr.upgrade", "enabled", false)?;
            c.ocr.upgrade.replace = get_list(
                up,
                "ocr.upgrade",
                "replace",
                &UpgradeConfig::default().replace,
            )?;
        }
        c.ocr.validate()?;
        let ocr_extra = leftovers(
            s,
            &[
                "backend",
                "poll_interval",
                "concurrency",
                "sessions",
                "local_processing",
                "autobench",
                "generations",
                "upgrade",
            ],
        );

        let s = section(data, "dyndns")?;
        c.dyndns.enabled = get_bool(&s, "dyndns", "enabled", false)?;
        c.dyndns.provider = get_str(&s, "provider", "duckdns");
        c.dyndns.token = get_str(&s, "token", "");
        c.dyndns.domain = get_str(&s, "domain", "");
        c.dyndns.update_url = get_str(&s, "update_url", "");
        c.dyndns.interval = get_int(&s, "dyndns", "interval", 300)?.max(0) as u32;
        c.dyndns.validate()?;
        let dyndns_extra = leftovers(
            s,
            &[
                "enabled",
                "provider",
                "token",
                "domain",
                "update_url",
                "interval",
            ],
        );

        let s = section(data, "update")?;
        c.update.check = get_bool(&s, "update", "check", true)?;
        c.update.channel = get_str(&s, "channel", "stable");
        c.update.manifest_url = get_str(&s, "manifest_url", &UpdateConfig::default().manifest_url);
        c.update.auto = get_bool(&s, "update", "auto", false)?;
        c.update.public_key = get_str(&s, "public_key", "");
        let update_extra = leftovers(
            s,
            &["check", "channel", "manifest_url", "auto", "public_key"],
        );

        // Preserve whatever we did not understand.
        let known_sections = [
            ("server", server_extra),
            ("storage", storage_extra),
            ("registration", registration_extra),
            ("cors", cors_extra),
            ("ssl", ssl_extra),
            ("admin", admin_extra),
            ("catalog", catalog_extra),
            ("queue", queue_extra),
            ("database", database_extra),
            ("ocr", ocr_extra),
            ("dyndns", dyndns_extra),
            ("update", update_extra),
        ];
        for (name, extra) in known_sections {
            if !extra.is_empty() {
                c.extra.insert(name.to_string(), Value::Object(extra));
            }
        }
        for (k, v) in data {
            if !c.extra.contains_key(k)
                && !matches!(
                    k.as_str(),
                    "server"
                        | "storage"
                        | "registration"
                        | "cors"
                        | "ssl"
                        | "admin"
                        | "catalog"
                        | "queue"
                        | "database"
                        | "ocr"
                        | "dyndns"
                        | "update"
                )
            {
                c.extra.insert(k.clone(), v.clone());
            }
        }
        Ok(c)
    }

    fn apply_parsed_generations(&mut self, parsed: ParsedGenerations) {
        self.warnings.extend(parsed.warnings);
        self.ocr.generations = parsed.rows;
    }

    /// The document saved to `config.yaml` (sorted keys, like PyYAML's `safe_dump`).
    pub fn to_value(&self) -> Value {
        let merge = |name: &str, base: Value| -> Value {
            let mut base = base;
            if let (Value::Object(b), Some(Value::Object(extra))) =
                (&mut base, self.extra.get(name))
            {
                for (k, v) in extra {
                    b.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
            base
        };
        let mut out = Map::new();
        out.insert(
            "server".into(),
            merge(
                "server",
                json!({
                    "host": self.server.host,
                    "port": self.server.port,
                    "trusted_proxies": self.server.trusted_proxies,
                }),
            ),
        );
        if self.server.threads != 0 {
            out["server"]["threads"] = json!(self.server.threads);
        }
        if self.server.cache_mb != 32 {
            out["server"]["cache_mb"] = json!(self.server.cache_mb);
        }
        out.insert(
            "storage".into(),
            merge(
                "storage",
                json!({"base_path": self.storage.base_path.to_string_lossy()}),
            ),
        );
        out.insert(
            "registration".into(),
            merge(
                "registration",
                json!({
                    "mode": self.registration.mode,
                    "default_role": self.registration.default_role,
                    "allow_anonymous_browse": self.registration.allow_anonymous_browse,
                    "allow_anonymous_download": self.registration.allow_anonymous_download,
                    "require_login": !self.registration.allow_anonymous_browse && !self.registration.allow_anonymous_download,
                }),
            ),
        );
        out.insert(
            "cors".into(),
            merge(
                "cors",
                json!({
                    "enabled": self.cors.enabled,
                    "allowed_origins": self.cors.allowed_origins,
                    "allow_credentials": self.cors.allow_credentials,
                }),
            ),
        );
        out.insert(
            "ssl".into(),
            merge(
                "ssl",
                json!({
                    "enabled": self.ssl.enabled,
                    "auto_cert": self.ssl.auto_cert,
                    "cert_file": self.ssl.cert_file,
                    "key_file": self.ssl.key_file,
                }),
            ),
        );
        out.insert(
            "admin".into(),
            merge(
                "admin",
                json!({"enabled": self.admin.enabled, "path": self.admin.path}),
            ),
        );
        out.insert(
            "catalog".into(),
            merge(
                "catalog",
                json!({
                    "enabled": self.catalog.enabled,
                    "reader_url": self.catalog.reader_url,
                    "use_as_homepage": self.catalog.use_as_homepage,
                    "enrich_community": self.catalog.enrich_community,
                }),
            ),
        );
        out.insert(
            "queue".into(),
            merge(
                "queue",
                json!({
                    "show_in_nav": self.queue.show_in_nav,
                    "public_access": self.queue.public_access,
                    "display": self.queue.display,
                }),
            ),
        );
        out.insert(
            "database".into(),
            merge(
                "database",
                json!({
                    "busy_timeout_ms": self.database.busy_timeout_ms,
                    "lock_retries": self.database.lock_retries,
                    "retry_initial_delay_seconds": self.database.retry_initial_delay_seconds,
                }),
            ),
        );
        let mut ocr = json!({
            "backend": self.ocr.backend,
            "poll_interval": self.ocr.poll_interval,
            "concurrency": self.ocr.concurrency,
            "sessions": self.ocr.sessions,
            "local_processing": self.ocr.local_processing,
            "autobench": self.ocr.autobench,
            "generations": self.ocr.generations.iter().map(Generation::to_value).collect::<Vec<_>>(),
        });
        if self.ocr.upgrade != UpgradeConfig::default() {
            ocr["upgrade"] =
                json!({"enabled": self.ocr.upgrade.enabled, "replace": self.ocr.upgrade.replace});
        }
        out.insert("ocr".into(), merge("ocr", ocr));
        out.insert(
            "dyndns".into(),
            merge(
                "dyndns",
                json!({
                    "enabled": self.dyndns.enabled,
                    "provider": self.dyndns.provider,
                    "token": self.dyndns.token,
                    "domain": self.dyndns.domain,
                    "update_url": self.dyndns.update_url,
                    "interval": self.dyndns.interval,
                }),
            ),
        );
        if self.update != UpdateConfig::default() || self.extra.contains_key("update") {
            out.insert(
                "update".into(),
                merge(
                    "update",
                    {
                        let mut u = json!({"check": self.update.check, "channel": self.update.channel, "manifest_url": self.update.manifest_url, "auto": self.update.auto});
                        if !self.update.public_key.is_empty() {
                            u["public_key"] = json!(self.update.public_key);
                        }
                        u
                    },
                ),
            );
        }
        for (k, v) in &self.extra {
            out.entry(k.clone()).or_insert_with(|| v.clone());
        }
        Value::Object(out)
    }

    pub fn to_yaml(&self) -> String {
        serde_yaml_ng::to_string(&sort_value(&self.to_value())).unwrap_or_default()
    }

    /// Set a value by dotted key (`server.port`) from its string form, as `config set`
    /// and `MOKURO_*` environment variables do.
    pub fn set_by_dotted_key(&mut self, key: &str, value: &str) -> Result<(), ConfigError> {
        let parts: Vec<&str> = key.split('.').collect();
        if parts.len() != 2 {
            return Err(ConfigError::UnknownKey(format!(
                "Invalid key: {key}. Expected format: section.field"
            )));
        }
        let b =
            || parse_bool(value).ok_or_else(|| invalid(format!("Invalid boolean value: {value}")));
        let i = || {
            value
                .trim()
                .parse::<i64>()
                .map_err(|_| invalid(format!("invalid literal for int() with base 10: '{value}'")))
        };
        match key {
            "server.port" => {
                let p = i()?;
                if !(0..65536).contains(&p) {
                    return Err(invalid(format!("Invalid port: {p}")));
                }
                self.server.port = p as u16;
            }
            "server.host" => self.server.host = value.to_string(),
            "server.trusted_proxies" => {
                let list = split_list(value);
                validate_trusted_proxies(&list)?;
                self.server.trusted_proxies = list;
            }
            "server.threads" => self.server.threads = i()?.clamp(0, 256) as u32,
            "server.cache_mb" => self.server.cache_mb = i()?.clamp(1, 65536) as u32,
            "storage.base_path" => self.storage.base_path = storage::expand_user(Path::new(value)),
            "registration.mode" => {
                if !REGISTRATION_MODES.contains(&value) {
                    return Err(invalid(format!("Invalid registration mode: {value}")));
                }
                self.registration.mode = value.to_string();
            }
            "registration.default_role" => {
                let v = if value == "writer" { "uploader" } else { value };
                if !DEFAULT_ROLES.contains(&v) {
                    return Err(invalid(format!("Invalid default role: {v}")));
                }
                self.registration.default_role = v.to_string();
            }
            "registration.allow_anonymous_browse" => {
                self.registration.allow_anonymous_browse = b()?
            }
            "registration.allow_anonymous_download" => {
                self.registration.allow_anonymous_download = b()?
            }
            "registration.require_login" => self.registration.require_login = b()?,
            "cors.enabled" => self.cors.enabled = b()?,
            "cors.allow_credentials" => self.cors.allow_credentials = b()?,
            "cors.allowed_origins" => self.cors.allowed_origins = split_list(value),
            "ssl.enabled" => self.ssl.enabled = b()?,
            "ssl.auto_cert" => self.ssl.auto_cert = b()?,
            "ssl.cert_file" => self.ssl.cert_file = value.to_string(),
            "ssl.key_file" => self.ssl.key_file = value.to_string(),
            "admin.enabled" => self.admin.enabled = b()?,
            "admin.path" => self.admin.path = value.to_string(),
            "catalog.enabled" => self.catalog.enabled = b()?,
            "catalog.reader_url" => self.catalog.reader_url = value.to_string(),
            "catalog.use_as_homepage" => self.catalog.use_as_homepage = b()?,
            "catalog.enrich_community" => self.catalog.enrich_community = b()?,
            "queue.show_in_nav" => self.queue.show_in_nav = b()?,
            "queue.public_access" => self.queue.public_access = b()?,
            "queue.display" => {
                if !QUEUE_DISPLAY_LEVELS.contains(&value) {
                    return Err(invalid(format!(
                        "Invalid queue display level: '{value}' (expected one of: {})",
                        QUEUE_DISPLAY_LEVELS.join(", ")
                    )));
                }
                self.queue.display = value.to_string();
            }
            "database.busy_timeout_ms" => {
                self.database.busy_timeout_ms = i()?.max(0) as u32;
                self.database.validate()?;
            }
            "database.lock_retries" => {
                self.database.lock_retries = i()?.max(0) as u32;
                self.database.validate()?;
            }
            "database.retry_initial_delay_seconds" => {
                self.database.retry_initial_delay_seconds = value.trim().parse().map_err(|_| {
                    invalid(format!("could not convert string to float: '{value}'"))
                })?;
                self.database.validate()?;
            }
            "ocr.backend" => {
                if !OCR_BACKENDS.contains(&value) {
                    return Err(invalid(format!("Invalid OCR backend: {value}")));
                }
                self.ocr.backend = value.to_string();
            }
            "ocr.poll_interval" => {
                let v = i()?;
                if v < 1 {
                    return Err(invalid(format!("Invalid poll interval: {v}")));
                }
                self.ocr.poll_interval = v as u32;
            }
            "ocr.concurrency" => self.ocr.concurrency = validate_concurrency(i()?)?,
            "ocr.sessions" => self.ocr.sessions = b()?,
            "ocr.local_processing" => self.ocr.local_processing = b()?,
            "ocr.autobench" => self.ocr.autobench = b()?,
            "ocr.generations" => {
                let parsed = generations::parse_generation_list(&Value::String(value.to_string()))?;
                self.apply_parsed_generations(parsed);
            }
            "dyndns.enabled" => self.dyndns.enabled = b()?,
            "dyndns.provider" => {
                self.dyndns.provider = value.to_string();
                self.dyndns.validate()?;
            }
            "dyndns.token" => self.dyndns.token = value.to_string(),
            "dyndns.domain" => self.dyndns.domain = value.to_string(),
            "dyndns.update_url" => self.dyndns.update_url = value.to_string(),
            "dyndns.interval" => {
                self.dyndns.interval = i()?.max(0) as u32;
                self.dyndns.validate()?;
            }
            "update.check" => self.update.check = b()?,
            "update.channel" => self.update.channel = value.to_string(),
            "update.manifest_url" => self.update.manifest_url = value.to_string(),
            "update.auto" => self.update.auto = b()?,
            _ => {
                let section_ok = [
                    "server",
                    "storage",
                    "registration",
                    "cors",
                    "ssl",
                    "admin",
                    "catalog",
                    "queue",
                    "database",
                    "ocr",
                    "dyndns",
                    "update",
                ]
                .contains(&parts[0]);
                return Err(ConfigError::UnknownKey(if section_ok {
                    format!("Unknown field '{}' in section '{}'", parts[1], parts[0])
                } else {
                    format!("Unknown config section: {}", parts[0])
                }));
            }
        }
        Ok(())
    }

    /// Every dotted key `set_by_dotted_key` accepts (also the `MOKURO_*` env names).
    pub const KEYS: &'static [&'static str] = &[
        "server.port",
        "server.host",
        "server.trusted_proxies",
        "server.threads",
        "server.cache_mb",
        "storage.base_path",
        "registration.mode",
        "registration.default_role",
        "registration.allow_anonymous_browse",
        "registration.allow_anonymous_download",
        "registration.require_login",
        "cors.enabled",
        "cors.allow_credentials",
        "ssl.enabled",
        "ssl.auto_cert",
        "ssl.cert_file",
        "ssl.key_file",
        "admin.enabled",
        "admin.path",
        "catalog.enabled",
        "catalog.reader_url",
        "catalog.use_as_homepage",
        "catalog.enrich_community",
        "queue.show_in_nav",
        "queue.public_access",
        "queue.display",
        "ocr.backend",
        "ocr.poll_interval",
        "ocr.concurrency",
        "ocr.sessions",
        "ocr.local_processing",
        "ocr.autobench",
        "ocr.generations",
        "dyndns.enabled",
        "dyndns.provider",
        "dyndns.token",
        "dyndns.domain",
        "dyndns.update_url",
        "dyndns.interval",
        "update.check",
        "update.channel",
        "update.manifest_url",
        "update.auto",
        // Not `update.public_key`: a key is trusted only from the config file itself.
    ];

    /// Apply `MOKURO_<SECTION>_<KEY>` variables plus the `MOKURO_HOST`/`PORT`/`STORAGE` aliases.
    pub fn apply_env_overrides(
        &mut self,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<(), ConfigError> {
        let removed: Vec<String> = REMOVED_OCR_KEYS
            .iter()
            .filter(|(k, _)| env(&format!("MOKURO_OCR_{}", k.to_uppercase())).is_some())
            .map(|(k, why)| format!("MOKURO_OCR_{}: {why}", k.to_uppercase()))
            .collect();
        if !removed.is_empty() {
            return Err(invalid(removed.join("; ")));
        }
        let retired: Vec<String> = RETIRED_OCR_KEYS
            .iter()
            .map(|(k, _)| format!("MOKURO_OCR_{}", k.to_uppercase()))
            .filter(|name| env(name).is_some())
            .collect();
        if !retired.is_empty() {
            return Err(invalid(format!(
                "{} was replaced by MOKURO_OCR_GENERATIONS, the JSON text of a list of named OCR recipes, e.g. \
                 '[{{\"name\": \"hayai-nova\", \"engine\": \"hayai-nova\", \"primary\": true}}]'",
                retired.join(", ")
            )));
        }
        for key in Self::KEYS {
            let name = format!("MOKURO_{}", key.replace('.', "_").to_uppercase());
            if let Some(v) = env(&name) {
                self.set_by_dotted_key(key, &v)
                    .map_err(|e| invalid(format!("{name}: {e}")))?;
            }
        }
        for (name, key) in [
            ("MOKURO_HOST", "server.host"),
            ("MOKURO_PORT", "server.port"),
            ("MOKURO_STORAGE", "storage.base_path"),
        ] {
            if let Some(v) = env(name) {
                self.set_by_dotted_key(key, &v)
                    .map_err(|e| invalid(format!("{name}: {e}")))?;
            }
        }
        Ok(())
    }

    /// The server only reads the library over WebDAV when this holds; mirrors
    /// `processes_locally` in 0.5.2 plus the lite build's lack of an OCR runtime.
    pub fn processes_locally(&self, ocr_available: bool) -> bool {
        ocr_available && self.ocr.backend != "skip" && self.ocr.local_processing
    }
}

impl DatabaseConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.busy_timeout_ms < 100 {
            return Err(invalid("Database busy timeout must be at least 100 ms"));
        }
        if self.lock_retries < 1 {
            return Err(invalid("Database lock retries must be at least 1"));
        }
        if self.retry_initial_delay_seconds <= 0.0 {
            return Err(invalid("Database retry initial delay must be positive"));
        }
        Ok(())
    }
}

impl OcrConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if !OCR_BACKENDS.contains(&self.backend.as_str()) {
            return Err(invalid(format!("Invalid OCR backend: {}", self.backend)));
        }
        if self.poll_interval < 1 {
            return Err(invalid(format!(
                "Invalid poll interval: {}",
                self.poll_interval
            )));
        }
        Ok(())
    }

    /// The backend the engines get: `rocm` is the AMD path again in 0.7 (libtorch's
    /// ROCm build in the `rocm7.1` pack), not an alias of WebGPU as in the ONNX-only
    /// design, which made `ocr.backend: rocm` hide the AMD GPU.
    pub fn effective_backend(&self) -> &str {
        &self.backend
    }
}

impl DynDnsConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if !DYNDNS_PROVIDERS.contains(&self.provider.as_str()) {
            return Err(invalid(format!(
                "Invalid DynDNS provider: {}",
                self.provider
            )));
        }
        if self.interval < 30 {
            return Err(invalid("DynDNS interval must be at least 30 seconds"));
        }
        Ok(())
    }
}

fn validate_concurrency(slots: i64) -> Result<u32, ConfigError> {
    if slots < 1 {
        return Err(invalid(format!(
            "Invalid OCR concurrency: {slots} (must be at least 1)"
        )));
    }
    if slots > MAX_OCR_CONCURRENCY as i64 {
        return Err(invalid(format!(
            "Invalid OCR concurrency: {slots} (at most {MAX_OCR_CONCURRENCY}; one job already needs 2-4 cores)"
        )));
    }
    Ok(slots as u32)
}

pub fn validate_trusted_proxies(list: &[String]) -> Result<(), ConfigError> {
    for network in list {
        let ok = network.parse::<ipnet::IpNet>().is_ok() || network.parse::<IpAddr>().is_ok();
        if !ok {
            return Err(invalid(format!(
                "server.trusted_proxies: '{network}' is not a network or address"
            )));
        }
    }
    Ok(())
}

fn sort_value(v: &Value) -> Value {
    // PyYAML's safe_dump sorts keys; serde_json keeps insertion order (preserve_order).
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sort_value(&m[k])))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(sort_value).collect()),
        other => other.clone(),
    }
}

/// Load `path` (or the default path). A missing file means defaults. Environment
/// overrides are applied afterwards.
pub fn load_config(path: Option<&Path>) -> Result<Config, ConfigError> {
    let path = path
        .map(Path::to_path_buf)
        .unwrap_or_else(storage::default_config_path);
    let mut config = if path.exists() {
        let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Io {
            path: path.clone(),
            source,
        })?;
        let value: Value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_yaml_ng::from_str(&text).map_err(|e| ConfigError::Yaml {
                path: path.clone(),
                message: e.to_string(),
            })?
        };
        Config::from_value(&value)?
    } else {
        Config::default()
    };
    config.apply_env_overrides(|k| std::env::var(k).ok())?;
    Ok(config)
}

/// Write `config` as YAML, atomically (temp file + rename).
pub fn save_config(config: &Config, path: Option<&Path>) -> Result<(), ConfigError> {
    let path = path
        .map(Path::to_path_buf)
        .unwrap_or_else(storage::default_config_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, config.to_yaml()).map_err(|source| ConfigError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, &path).map_err(|source| ConfigError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(s: &str) -> Value {
        serde_yaml_ng::from_str(s).unwrap()
    }

    #[test]
    fn defaults() {
        let c = Config::from_value(&Value::Null).unwrap();
        assert_eq!(c.server.port, 8080);
        assert_eq!(c.registration.mode, "self");
        assert!(c.cors.is_origin_allowed("http://localhost:3000"));
        assert!(!c.cors.is_origin_allowed("http://localhost:abc"));
        assert!(c.cors.is_origin_allowed("https://reader.mokuro.app"));
        assert_eq!(c.ocr.generations[0].engine, "hayai-nova");
    }

    #[test]
    fn legacy_require_login() {
        let c = Config::from_value(&yaml("registration: {require_login: true}")).unwrap();
        assert!(!c.registration.allow_anonymous_browse);
        assert!(!c.registration.allow_anonymous_download);
    }

    #[test]
    fn writer_role_migrates() {
        let c = Config::from_value(&yaml("registration: {default_role: writer}")).unwrap();
        assert_eq!(c.registration.default_role, "uploader");
    }

    #[test]
    fn retired_keys_refused() {
        assert!(Config::from_value(&yaml("ocr: {engines: [mokuro]}")).is_err());
        assert!(Config::from_value(&yaml("ocr: {char_map: true}")).is_err());
    }

    #[test]
    fn env_overrides() {
        let mut c = Config::default();
        c.apply_env_overrides(|k| match k {
            "MOKURO_PORT" => Some("9000".into()),
            "MOKURO_SSL_ENABLED" => Some("yes".into()),
            "MOKURO_SERVER_TRUSTED_PROXIES" => Some("10.0.0.0/8, 192.168.1.1".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(c.server.port, 9000);
        assert!(c.ssl.enabled);
        assert_eq!(c.server.trusted_proxies, vec!["10.0.0.0/8", "192.168.1.1"]);
        assert!(c.apply_env_overrides(|k| (k == "MOKURO_SERVER_TRUSTED_PROXIES").then(|| "nope".into())).is_err());
    }

    #[test]
    fn roundtrip_preserves_unknown() {
        let c = Config::from_value(&yaml(
            "server: {port: 1234, future_knob: 7}\nfuture_section: {a: 1}\n",
        ))
        .unwrap();
        let v = c.to_value();
        assert_eq!(v["server"]["port"], 1234);
        assert_eq!(v["server"]["future_knob"], 7);
        assert_eq!(v["future_section"]["a"], 1);
        let again = Config::from_value(&v).unwrap();
        assert_eq!(again.server.port, 1234);
    }

    #[test]
    fn mokuro_config_from_052_loads() {
        let c = Config::from_value(&yaml(
            "ocr:\n  generations:\n    - {id: g-1, name: mokuro, engine: mokuro, primary: true, enabled: true}\n    - {id: g-2, name: paddle-manga, engine: paddle-manga, detector: ctd, patch_budget: 512}\n",
        ))
        .unwrap();
        assert!(!c.warnings.is_empty());
        let primary = generations::primary_generation(&c.ocr.generations).unwrap();
        assert_eq!(primary.engine, "hayai-nova");
        let paddle = c
            .ocr
            .generations
            .iter()
            .find(|g| g.engine == "paddle-manga")
            .unwrap();
        assert_eq!(paddle.detector.as_deref(), Some("ppocr-manga"));
        assert!(c.to_yaml().contains("engine: mokuro"));
    }
}
