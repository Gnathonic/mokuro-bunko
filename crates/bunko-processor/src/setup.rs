//! `processor setup`: from a fresh install to a running processor (0.5.2
//! `processor/setup.py`, minus the Python environment install).
//!
//! Only three settings have no default — the library's URL and the processor
//! account's username and password — so the wizard asks for those, CHECKS them against
//! the library before it writes a byte, writes only what differs from the defaults
//! (mode 600, atomically), and offers to start the processor as a service.
//!
//! The check never registers (a registration would put a phantom machine on the
//! library's admin panel): who the account is comes from `/login/api/me`, and the
//! protocol from a registration with protocol 0, which every library refuses with
//! the protocols it speaks and its version.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bunko_proto::PROTOCOL_VERSION;
use serde_json::{Value, json};
use serde_yaml_ng::{Mapping, Value as Yaml};

use crate::client::LibraryClient;
use crate::config::{LibrarySettings, TlsVerify, default_name, expand_user, load_processor_config};
use crate::pipeline::MachineInfo;
use crate::tls::tls_failure;

pub const DEFAULT_CONFIG_NAME: &str = "processor.yaml";
pub const IDENTITY_PATH: &str = "/login/api/me";
pub const REGISTER_PATH: &str = "/_processor/register";
/// A registration naming this protocol is refused before anything is registered.
pub const PROBE_PROTOCOL: u32 = 0;
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(15);
pub const PROCESSOR_ROLE: &str = "processor";
const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const HEADER: &str = "# mokuro-bunko processor, written by `mokuro-bunko processor setup`.
#
# Only the settings that differ from the defaults are here. Every other
# setting, and what each one does, is in docs/processor.example.yaml.
#
# This file holds the library password: keep it private.

";

/// Setup cannot go on; the message says why and what to do about it.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct SetupError(pub String);

fn fail(message: impl Into<String>) -> SetupError {
    SetupError(message.into())
}

/// `(url, note)`: the library URL as the config stores it. A bare host gets
/// `http://` (and a note saying so); a trailing slash is dropped.
pub fn normalize_url(raw: &str) -> Result<(String, Option<String>), SetupError> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(fail("the library URL is empty"));
    }
    let defaulted = !text.contains("://");
    let full = if defaulted {
        format!("http://{text}")
    } else {
        text.to_string()
    };
    let scheme = full.split("://").next().unwrap_or("").to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(fail(format!(
            "{text}: the library URL must start with http:// or https://"
        )));
    }
    let parsed = reqwest::Url::parse(&full).map_err(|e| {
        if e.to_string().contains("port") {
            fail(format!("{text}: {e}"))
        } else {
            fail(format!("{text}: no host name in the library URL"))
        }
    })?;
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(fail(format!("{text}: no host name in the library URL")));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(fail(format!(
            "{text}: the library URL is its address only, without ?... or #..."
        )));
    }
    let url = parsed.as_str().trim_end_matches('/').to_string();
    let note = defaulted.then(|| {
        format!("No scheme given, so using {url} (type https://... if the library uses TLS).")
    });
    Ok((url, note))
}

/// `--tls-verify`: true, false, or the path of the certificate to trust.
pub fn parse_tls_verify(raw: &str) -> Result<TlsVerify, SetupError> {
    let value = raw.trim();
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" | "" => return Ok(TlsVerify::Yes),
        "false" | "no" | "0" => return Ok(TlsVerify::No),
        _ => {}
    }
    let path = expand_user(value);
    if !path.is_file() {
        return Err(fail(format!(
            "--tls-verify {value}: no such certificate file"
        )));
    }
    Ok(TlsVerify::Cert(
        std::fs::canonicalize(&path).unwrap_or(path),
    ))
}

/// What the library said about the account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAccount {
    pub username: String,
    pub role: String,
    pub protocol: u32,
    pub library_version: Option<String>,
    pub notes: Vec<String>,
}

fn article(word: &str) -> &'static str {
    if word
        .chars()
        .next()
        .is_some_and(|c| "aeiouAEIOU".contains(c))
    {
        "an"
    } else {
        "a"
    }
}

fn unreachable(url: &str, e: &reqwest::Error) -> SetupError {
    if let Some(tls) = tls_failure(e) {
        if let rustls::Error::InvalidCertificate(why) = tls {
            return fail(format!(
                "The library's TLS certificate at {url} was not accepted: {why:?}.\n\
                 If the library uses a self-signed certificate, run setup again with\n\
                 --tls-verify /path/to/its-certificate.pem (or --tls-verify false on a network\n\
                 you trust). That is the `tls_verify` setting in processor.yaml."
            ));
        }
        return fail(format!(
            "Could not open a TLS connection to {url}: {tls}.\n\
             If the library serves plain HTTP, use an http:// URL. For certificate trouble, \
             see --tls-verify (the `tls_verify` setting in processor.yaml)."
        ));
    }
    fail(format!(
        "Could not reach the library at {url}: {}.\n\
         Check the URL, and that the library is running and reachable from this machine.",
        crate::client::describe_reqwest(e)
    ))
}

/// Log in, and check the account is a processor on a library that speaks this
/// machine's protocol. Registers nothing.
pub async fn verify_account(
    url: &str,
    username: &str,
    password: &str,
    tls_verify: &TlsVerify,
) -> Result<VerifiedAccount, SetupError> {
    let settings = LibrarySettings {
        url: url.to_string(),
        username: username.to_string(),
        password: password.to_string(),
        tls_verify: tls_verify.clone(),
    };
    let client = LibraryClient::new(&settings, "setup").map_err(|e| fail(e.to_string()))?;
    let (status, body, location) = client
        .exchange_basic(http::Method::GET, IDENTITY_PATH, None, VERIFY_TIMEOUT)
        .await
        .map_err(|e| unreachable(url, &e))?;
    if (300..400).contains(&status) {
        let target = location.unwrap_or_default();
        let target = target
            .split(IDENTITY_PATH)
            .next()
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string();
        let target = if target.is_empty() {
            "somewhere else".to_string()
        } else {
            target
        };
        return Err(fail(format!(
            "{url} redirects to {target}: run setup again with that URL."
        )));
    }
    if status == 401 {
        return Err(fail(format!(
            "The library refused the username '{username}' or its password. Check both; \
             an admin can set a new password with: mokuro-bunko admin set-password {username}"
        )));
    }
    if status == 429 {
        let detail = body
            .as_ref()
            .and_then(|b| b.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("too many failed attempts")
            .to_string();
        return Err(fail(format!(
            "The library is refusing logins for '{username}' for now: {detail}"
        )));
    }
    let Some(body) = body.filter(|b| status == 200 && b.contains_key("authenticated")) else {
        return Err(fail(format!(
            "{url} does not look like a mokuro-bunko library: it answered {status} to {IDENTITY_PATH}. \
             Check the URL (the address the library's web page is at)."
        )));
    };
    if !body
        .get("authenticated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(fail(format!(
            "{url} did not see the login (it answered as if anonymous). A proxy in front \
             of the library may be dropping the Authorization header."
        )));
    }
    let role = body
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let account = body
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or(username)
        .to_string();
    if role != PROCESSOR_ROLE {
        return Err(fail(format!(
            "'{account}' is {} {role} account, not a processor account. \
             Ask an admin to run: mokuro-bunko admin change-role {account} processor",
            article(&role)
        )));
    }

    let mut notes = Vec::new();
    let (status, body, _) = client
        .exchange_basic(
            http::Method::POST,
            REGISTER_PATH,
            Some(&json!({"protocol": PROBE_PROTOCOL})),
            VERIFY_TIMEOUT,
        )
        .await
        .map_err(|e| unreachable(url, &e))?;
    if status == 404 {
        return Err(fail(format!(
            "The library at {url} has no remote processors (it answered 404 to {REGISTER_PATH}). \
             Update the library to the same release as this machine (mokuro-bunko {VERSION})."
        )));
    }
    let library_version = body
        .as_ref()
        .and_then(|b| b.get("version"))
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| (!v.is_null()).then(|| v.to_string()))
        })
        .filter(|v| !v.is_empty());
    let protocols = body
        .as_ref()
        .and_then(|b| b.get("protocols"))
        .and_then(Value::as_array)
        .cloned();
    let Some(protocols) = protocols.filter(|_| status == 400) else {
        notes.push(format!(
            "Could not check the library's processor protocol (it answered {status}); \
             `processor serve` will say if they differ."
        ));
        return Ok(VerifiedAccount {
            username: account,
            role,
            protocol: PROTOCOL_VERSION,
            library_version,
            notes,
        });
    };
    let speaks: Vec<i64> = protocols.iter().filter_map(Value::as_i64).collect();
    if !speaks.contains(&i64::from(PROTOCOL_VERSION)) {
        let theirs = if protocols.is_empty() {
            "none".to_string()
        } else {
            protocols
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let newer_library = protocols
            .iter()
            .all(|p| p.as_i64().is_some_and(|p| p > i64::from(PROTOCOL_VERSION)));
        let side = if newer_library {
            "Update this machine to the library's release"
        } else {
            "Update the library to this machine's release"
        };
        let release = library_version
            .as_ref()
            .map(|v| format!(" (mokuro-bunko {v})"))
            .unwrap_or_default();
        return Err(fail(format!(
            "This machine speaks processor protocol {PROTOCOL_VERSION} (mokuro-bunko {VERSION}); \
             the library{release} speaks {theirs}. {side} -- both must run the same release."
        )));
    }
    if let Some(theirs) = &library_version
        && theirs != VERSION
    {
        notes.push(format!(
            "This machine runs mokuro-bunko {VERSION} and the library {theirs}. Both speak \
             processor protocol {PROTOCOL_VERSION}, so this works; update them to the same release when you can."
        ));
    }
    Ok(VerifiedAccount {
        username: account,
        role,
        protocol: PROTOCOL_VERSION,
        library_version,
        notes,
    })
}

/// processor.yaml with only what differs from the defaults.
pub fn render_config(
    url: &str,
    username: &str,
    password: &str,
    name: Option<&str>,
    tls_verify: &TlsVerify,
    hostname: &str,
    auto_update: bool,
) -> String {
    let mut library = Mapping::new();
    library.insert("url".into(), url.into());
    library.insert("username".into(), username.into());
    library.insert("password".into(), password.into());
    match tls_verify {
        TlsVerify::Yes => {}
        TlsVerify::No => {
            library.insert("tls_verify".into(), Yaml::Bool(false));
        }
        TlsVerify::Cert(path) => {
            library.insert(
                "tls_verify".into(),
                path.to_string_lossy().into_owned().into(),
            );
        }
    }
    let mut data = Mapping::new();
    data.insert("library".into(), Yaml::Mapping(library));
    let mut processor = Mapping::new();
    if let Some(name) = name.filter(|n| !n.is_empty() && *n != hostname) {
        processor.insert("name".into(), name.into());
    }
    if auto_update {
        processor.insert("auto_update".into(), Yaml::Bool(true));
    }
    if !processor.is_empty() {
        data.insert("processor".into(), Yaml::Mapping(processor));
    }
    let body = serde_yaml_ng::to_string(&Yaml::Mapping(data)).unwrap_or_default();
    // The default (off) is written out as a comment, so the choice can be found and
    // flipped in the file.
    let note = if auto_update {
        "\n# processor.auto_update: true updates this processor when its library updates\n\
         # (the running volume finishes first; never a downgrade).\n"
    } else {
        "\n# To update this processor automatically when its library updates (the running\n\
         # volume finishes first; never a downgrade), add under `processor:`:\n\
         #   auto_update: true\n"
    };
    format!("{HEADER}{body}{note}")
}

/// Mode 600, or the Windows equivalent. A warning when it cannot be done.
fn restrict_to_owner(path: &Path, shown: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            Ok(()) => None,
            Err(_) => Some(format!(
                "{} holds the library password; keep it where only you can read it.",
                shown.display()
            )),
        }
    }
    #[cfg(windows)]
    {
        let principal = std::process::Command::new("whoami")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("USERNAME").ok());
        let warning = Some(format!(
            "{} holds the library password; keep it where only you can read it.",
            shown.display()
        ));
        let Some(principal) = principal else {
            return warning;
        };
        let done = std::process::Command::new("icacls")
            .arg(path)
            .args(["/inheritance:r", "/grant:r", &format!("{principal}:F")])
            .output();
        match done {
            Ok(o) if o.status.success() => None,
            _ => warning,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Some(format!(
            "{} holds the library password; keep it where only you can read it.",
            shown.display()
        ))
    }
}

/// Write `text` to `path` all at once, readable by its owner only: a temporary file
/// beside it is written, restricted, loaded back the way `serve` loads it, and only
/// then moved over `path`. Returns a warning when the file could not be restricted.
pub fn write_config(
    path: &Path,
    text: &str,
    overwrite: bool,
) -> Result<Option<String>, SetupError> {
    if path.exists() && !overwrite {
        return Err(fail(format!(
            "{} already exists; pass --force to overwrite it",
            path.display()
        )));
    }
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)
        .map_err(|e| fail(format!("could not create {}: {e}", dir.display())))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(DEFAULT_CONFIG_NAME);
    let staged = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
            .map_err(|e| fail(format!("could not write {}: {e}", staged.display())))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| fail(format!("could not write {}: {e}", staged.display())))?;
        drop(file);
        let warning = restrict_to_owner(&staged, path);
        load_processor_config(&staged)
            .map_err(|e| fail(format!("the configuration setup wrote does not load: {e}")))?;
        std::fs::rename(&staged, path)
            .map_err(|e| fail(format!("could not write {}: {e}", path.display())))?;
        Ok(warning)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

/// How the wizard talks to a person (the binary wires a terminal; tests script it).
pub trait Prompter {
    fn say(&mut self, line: &str);
    /// One answer; `hidden` for the password.
    fn ask(&mut self, prompt: &str, hidden: bool) -> std::io::Result<String>;
    fn confirm(&mut self, question: &str, default: bool) -> std::io::Result<bool>;
}

/// The wizard's inputs (the CLI flags).
#[derive(Debug, Clone, Default)]
pub struct SetupOptions {
    pub config: PathBuf,
    pub url: Option<String>,
    pub username: Option<String>,
    /// From `--password-stdin` (there is deliberately no `--password` flag).
    pub password: Option<String>,
    pub name: Option<String>,
    pub tls_verify: TlsVerify,
    pub yes: bool,
    /// Offer the service step.
    pub service: bool,
    pub force: bool,
    /// `--auto-update`: write `processor.auto_update: true` without asking.
    pub auto_update: bool,
    /// What this machine offers (printed; from the pipeline's `describe`).
    pub machine: Option<MachineInfo>,
    /// The command prefix that runs this binary, for the printed hints.
    pub command: String,
}

/// Installs and starts the service for a config path (see [`crate::service::install`]).
pub type ServiceStep<'a> = &'a (dyn Fn(&Path) -> Result<ServiceStarted, String> + Sync);

/// What the service step reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStarted {
    /// The summary's `Running:` text.
    pub running: String,
    /// The summary's `Logs:` text.
    pub logs: String,
}

fn required(
    value: Option<&str>,
    flag: &str,
    prompt: &str,
    options: &SetupOptions,
    ui: &mut dyn Prompter,
    hidden: bool,
) -> Result<String, SetupError> {
    if let Some(v) = value.filter(|v| !v.trim().is_empty()) {
        return Ok(if hidden {
            v.to_string()
        } else {
            v.trim().to_string()
        });
    }
    if options.yes {
        return Err(fail(format!("{flag} is required with --yes")));
    }
    let answer = ui
        .ask(prompt, hidden)
        .map_err(|e| fail(format!("could not read the answer: {e}")))?;
    if answer.trim().is_empty() {
        return Err(fail(format!("{} is empty", prompt.to_lowercase())));
    }
    Ok(if hidden {
        answer
    } else {
        answer.trim().to_string()
    })
}

/// The whole wizard. Nothing is written before the account checks out.
/// `service_step(config)` installs and starts the service (None where unsupported).
pub async fn run(
    options: &SetupOptions,
    ui: &mut (dyn Prompter + Send),
    service_step: Option<ServiceStep<'_>>,
) -> Result<(), SetupError> {
    let path = expand_user(&options.config.to_string_lossy());
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut overwrite = options.force;
    if path.exists() && !overwrite {
        if options.yes {
            return Err(fail(format!(
                "{} already exists; pass --force to overwrite it",
                path.display()
            )));
        }
        let yes = ui
            .confirm(
                &format!("{} already exists. Overwrite it?", path.display()),
                false,
            )
            .map_err(|e| fail(format!("could not read the answer: {e}")))?;
        if !yes {
            ui.say("Nothing written.");
            return Ok(());
        }
        overwrite = true;
    }

    let raw_url = required(
        options.url.as_deref(),
        "--url",
        "Library URL",
        options,
        ui,
        false,
    )?;
    let (url, note) = normalize_url(&raw_url)?;
    if let Some(note) = note {
        ui.say(&note);
    }
    let username = required(
        options.username.as_deref(),
        "--username",
        "Processor username",
        options,
        ui,
        false,
    )?;
    let password = required(
        options.password.as_deref(),
        "--password-stdin",
        "Password",
        options,
        ui,
        true,
    )?;

    ui.say(&format!("Checking {username} on {url} ..."));
    let verified = verify_account(&url, &username, &password, &options.tls_verify).await?;
    let release = verified
        .library_version
        .as_ref()
        .map(|v| format!(", mokuro-bunko {v}"))
        .unwrap_or_default();
    ui.say(&format!(
        "Logged in: {} is a processor account (protocol {}{release}).",
        verified.username, verified.protocol
    ));
    for line in &verified.notes {
        ui.say(line);
    }
    if let Some(machine) = &options.machine {
        let engines = if machine.catalog.engines.is_empty() {
            "none yet (models still to download)".to_string()
        } else {
            machine.catalog.engines.join(", ")
        };
        let gpu = machine
            .host
            .gpu
            .as_deref()
            .map(|g| format!(", {g}"))
            .unwrap_or_default();
        ui.say(&format!(
            "This machine: {}{gpu}; engines: {engines}",
            machine.host.cpu
        ));
    }

    let auto_update = options.auto_update
        || (!options.yes
            && ui
                .confirm(
                    "Update this processor automatically when its library updates? \
                     (the running volume finishes first; never a downgrade)",
                    false,
                )
                .map_err(|e| fail(format!("could not read the answer: {e}")))?);

    let hostname = default_name();
    let name = options
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(&hostname)
        .to_string();
    let text = render_config(
        &url,
        &verified.username,
        &password,
        Some(&name),
        &options.tls_verify,
        &hostname,
        auto_update,
    );
    let warning = write_config(&path, &text, overwrite)?;
    ui.say(&format!(
        "Wrote {}{}",
        path.display(),
        if warning.is_some() {
            ""
        } else {
            " (readable by you only)"
        }
    ));
    if let Some(w) = &warning {
        ui.say(&format!("Note: {w}"));
    }

    let config = path.display().to_string();
    let command = if options.command.is_empty() {
        "mokuro-bunko".to_string()
    } else {
        options.command.clone()
    };
    let mut running = "no".to_string();
    let mut logs = String::new();
    let mut failed = Vec::new();
    if options.service
        && let Some(step) = service_step
    {
        let question = if cfg!(windows) {
            "Start the processor now, and at every logon (a Startup entry)?"
        } else if cfg!(target_os = "macos") {
            "Run the processor as a launchd agent now, and at every login?"
        } else {
            "Run the processor as a systemd user service now?"
        };
        let yes = options.yes
            || ui
                .confirm(question, true)
                .map_err(|e| fail(format!("could not read the answer: {e}")))?;
        if yes {
            match step(&path) {
                Ok(started) => {
                    running = started.running;
                    logs = started.logs;
                }
                Err(e) => {
                    running = format!("no, the service failed: {e}");
                    failed.push(format!(
                        "the service did not start; fix the cause above and run: {command} processor service --install --config {config}"
                    ));
                }
            }
        }
    }
    if logs.is_empty() {
        ui.say("");
        ui.say("To run the processor:");
        ui.say(&format!("  {command} processor serve --config {config}"));
        if cfg!(windows) {
            ui.say(&format!("To start it at every logon: {command} processor service --install --config {config}"));
        }
        logs = format!(
            "printed by the command above; its last state: {command} processor status --config {config}"
        );
    }
    ui.say("");
    ui.say("Summary");
    ui.say(&format!("  Config:    {config}"));
    ui.say(&format!("  Running:   {running}"));
    ui.say(&format!("  Logs:      {logs}"));
    if !failed.is_empty() {
        return Err(fail(failed.join("; ")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_normalised() {
        assert_eq!(
            normalize_url("lib.example:8080/").unwrap(),
            (
                "http://lib.example:8080".to_string(),
                Some("No scheme given, so using http://lib.example:8080 (type https://... if the library uses TLS).".into())
            )
        );
        assert_eq!(
            normalize_url("https://Lib.Example/bunko/").unwrap().0,
            "https://lib.example/bunko"
        );
        assert!(
            normalize_url("ftp://x")
                .unwrap_err()
                .0
                .contains("must start with http")
        );
        assert!(
            normalize_url("http://x/?a=1")
                .unwrap_err()
                .0
                .contains("without ?")
        );
        assert!(normalize_url("").is_err());
        assert!(normalize_url("http://host:notaport").is_err());
    }

    #[test]
    fn tls_verify_flag() {
        assert_eq!(parse_tls_verify("true").unwrap(), TlsVerify::Yes);
        assert_eq!(parse_tls_verify("no").unwrap(), TlsVerify::No);
        assert!(
            parse_tls_verify("/nonexistent.pem")
                .unwrap_err()
                .0
                .contains("no such certificate file")
        );
    }

    #[test]
    fn config_holds_only_non_defaults_and_loads_back() {
        let text = render_config(
            "https://lib",
            "gpu",
            "p#ss: word",
            Some("tower"),
            &TlsVerify::No,
            "tower",
            false,
        );
        assert!(text.starts_with(HEADER));
        assert!(!text.contains("\nprocessor:"), "{text}");
        assert!(text.contains("tls_verify: false"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("processor.yaml");
        write_config(&path, &text, false).unwrap();
        if std::env::var(crate::config::PASSWORD_ENV).is_err() {
            let loaded = load_processor_config(&path).unwrap();
            assert_eq!(loaded.library.password, "p#ss: word");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(
            write_config(&path, &text, false)
                .unwrap_err()
                .0
                .contains("already exists")
        );
        let named = render_config(
            "https://lib",
            "gpu",
            "pw",
            Some("other"),
            &TlsVerify::Yes,
            "tower",
            true,
        );
        assert!(
            named.contains("processor:\n  name: other\n  auto_update: true"),
            "{named}"
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temporary file left"
        );
    }
}
