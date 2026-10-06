//! The command tree (clap derive). Mirrors 0.5.2's click CLI; global flags go before
//! the subcommand, as there (`processor *` has its own `--config`).

use bunko_core::config::OCR_BACKENDS;
use clap::builder::PossibleValuesParser;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// Roles the admin CLI accepts for accounts (no `anonymous`, no legacy `writer`).
pub const ACCOUNT_ROLES: [&str; 6] = [
    "registered",
    "uploader",
    "inviter",
    "editor",
    "admin",
    "processor",
];
/// `sorted(INVITABLE_ROLES)`.
pub const INVITE_ROLES: [&str; 4] = ["editor", "inviter", "registered", "uploader"];
pub const USER_STATUSES: [&str; 4] = ["active", "pending", "disabled", "deleted"];

#[derive(Parser, Debug)]
#[command(
    name = "mokuro-bunko",
    about = "Mokuro Bunko Server - Manga library with OCR support.",
    disable_version_flag = true
)]
pub struct Cli {
    /// Path to configuration file
    #[arg(
        short = 'c',
        long = "config",
        env = "MOKURO_CONFIG",
        value_name = "PATH"
    )]
    pub config: Option<PathBuf>,
    /// Enable verbose output
    #[arg(short = 'v', long)]
    pub verbose: bool,
    /// Show the version, build flavor and target, then exit
    #[arg(long)]
    pub version: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start the server.
    Serve(ServeArgs),
    /// Interactive first-time setup wizard.
    Setup {
        /// Skip setup if config file already exists
        #[arg(long)]
        skip_if_exists: bool,
    },
    /// Diagnose common installation and OCR problems.
    Doctor {
        /// Check this machine's processor (its processor.yaml, backend pack and
        /// models) instead of the library server; automatic on a machine with a
        /// processor.yaml and no library configuration (full build)
        #[arg(long)]
        processor: bool,
    },
    /// Admin commands for user management.
    #[command(subcommand)]
    Admin(AdminCmd),
    /// Manage configuration.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Manage SSL certificates.
    #[command(subcommand)]
    Ssl(SslCmd),
    /// Manage tunnels for remote access.
    #[command(subcommand)]
    Tunnel(TunnelCmd),
    /// Manage Dynamic DNS settings.
    #[command(subcommand)]
    Dyndns(DyndnsCmd),
    /// Check for and install new releases.
    #[command(subcommand)]
    Update(UpdateCmd),
    /// Manage the OCR models and the compiled recognizer packages (full build).
    #[cfg(feature = "ocr")]
    #[command(subcommand)]
    Models(ModelsCmd),
    /// Install the OCR backend (libtorch pack for this GPU, or the CPU) and the models.
    #[command(name = "install-ocr")]
    InstallOcr(InstallOcrArgs),
    /// Run this machine as an OCR processor for a library (full build).
    #[cfg(feature = "ocr")]
    #[command(subcommand)]
    Processor(ProcessorCmd),
    /// Probe the running server's /api/health (for container HEALTHCHECKs).
    Healthcheck {
        /// URL to probe instead of the configured local server
        #[arg(long)]
        url: Option<String>,
    },
    /// Open the desktop app in the browser: setup wizard, settings and dashboard.
    /// (Double-clicking the program on Windows or macOS does the same.)
    Gui(GuiArgs),
}

/// `gui` options.
#[derive(Args, Debug, Clone, Default)]
pub struct GuiArgs {
    /// Print the address instead of opening the browser
    #[arg(long)]
    pub no_browser: bool,
    /// The page to open first, e.g. /app/settings or /app/setup/processor
    #[arg(long, value_name = "PAGE", default_value = "/app/")]
    pub open: String,
}

/// `serve` options. Every flag is optional: a flag that is passed always wins over the
/// config file and `MOKURO_*` variables, even when it equals the built-in default
/// (0.5.2 could not tell `--port 8080` from "not passed").
#[derive(Args, Debug, Clone, Default)]
pub struct ServeArgs {
    /// Host to bind to  [default: 0.0.0.0]
    #[arg(long)]
    pub host: Option<String>,
    /// Port to listen on  [default: 8080]
    #[arg(long)]
    pub port: Option<u16>,
    /// OCR backend to use  [default: auto]
    #[arg(long, value_parser = PossibleValuesParser::new(OCR_BACKENDS))]
    pub ocr: Option<String>,
    /// OCR generations as JSON, in run order, overriding ocr.generations:
    /// '[{"name": "hayai-nova", "engine": "hayai-nova", "primary": true}]'
    #[arg(long, value_name = "JSON")]
    pub generations: Option<String>,
    /// The global `-v` flag.
    #[arg(skip)]
    pub verbose: bool,
}

#[derive(Subcommand, Debug)]
pub enum AdminCmd {
    /// Add a new user.
    #[command(name = "add-user")]
    AddUser {
        username: String,
        /// User role
        #[arg(long, default_value = "registered", value_parser = PossibleValuesParser::new(ACCOUNT_ROLES))]
        role: String,
        /// User password (asked for when left out)
        #[arg(long)]
        password: Option<String>,
    },
    /// Delete a user.
    #[command(name = "delete-user")]
    DeleteUser {
        username: String,
        /// Skip confirmation
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// List all users.
    #[command(name = "list-users")]
    ListUsers {
        /// Filter by status
        #[arg(long, value_parser = PossibleValuesParser::new(USER_STATUSES))]
        status: Option<String>,
    },
    /// Change a user's role.
    #[command(name = "change-role")]
    ChangeRole {
        username: String,
        #[arg(value_parser = PossibleValuesParser::new(ACCOUNT_ROLES))]
        role: String,
    },
    /// Generate an invite code.
    #[command(name = "generate-invite")]
    GenerateInvite {
        /// Role for invited user
        #[arg(long, default_value = "registered", value_parser = PossibleValuesParser::new(INVITE_ROLES))]
        role: String,
        /// Expiration time (e.g., 1h, 7d, 30d)
        #[arg(long, default_value = "7d")]
        expires: String,
    },
    /// List invite codes.
    #[command(name = "list-invites")]
    ListInvites {
        /// Include used/expired invites
        #[arg(long = "all")]
        include_all: bool,
    },
    /// Delete an invite code.
    #[command(name = "delete-invite")]
    DeleteInvite {
        /// The code (may start with '-')
        #[arg(allow_hyphen_values = true)]
        code: String,
    },
    /// Bring a deleted account back, with a new password.
    #[command(name = "restore-user")]
    RestoreUser {
        username: String,
        /// Give the restored account this role (default: the role it had)
        #[arg(long, value_parser = PossibleValuesParser::new(ACCOUNT_ROLES))]
        role: Option<String>,
        /// New password (asked for when left out)
        #[arg(long)]
        password: Option<String>,
    },
    /// Approve a pending user.
    #[command(name = "approve-user")]
    ApproveUser { username: String },
    /// Disable a user account.
    #[command(name = "disable-user")]
    DisableUser { username: String },
    /// Set a user's password.
    #[command(name = "set-password")]
    SetPassword {
        username: String,
        /// New password (asked for when left out)
        #[arg(long)]
        password: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Show current configuration as YAML.
    Show,
    /// Set a configuration value by dotted key path.
    ///
    /// Examples: config set server.port 8443, config set registration.mode invite
    Set { key: String, value: String },
    /// Show config file and storage locations.
    Path,
    /// Create a default configuration file.
    Init {
        /// Overwrite existing config file
        #[arg(long)]
        force: bool,
    },
    /// Add a CORS allowed origin.
    #[command(name = "cors-add")]
    CorsAdd { origin: String },
    /// Remove a CORS allowed origin.
    #[command(name = "cors-remove")]
    CorsRemove { origin: String },
}

#[derive(Subcommand, Debug)]
pub enum SslCmd {
    /// Enable SSL.
    Enable {
        /// Generate a self-signed certificate
        #[arg(long)]
        auto_cert: bool,
        /// Path to certificate file
        #[arg(long, value_parser = existing_path)]
        cert: Option<String>,
        /// Path to private key file
        #[arg(long, value_parser = existing_path)]
        key: Option<String>,
    },
    /// Disable SSL.
    Disable,
    /// Show SSL status and certificate details.
    Status,
    /// Generate a self-signed certificate.
    Generate {
        /// Hostname for the certificate
        #[arg(long, default_value = "localhost")]
        hostname: String,
        /// Validity in days
        #[arg(long, default_value_t = 365)]
        days: u32,
    },
}

/// click `Path(exists=True)`.
fn existing_path(s: &str) -> Result<String, String> {
    if std::path::Path::new(s).exists() {
        Ok(s.to_string())
    } else {
        Err(format!("Path '{s}' does not exist."))
    }
}

#[derive(Subcommand, Debug)]
pub enum TunnelCmd {
    /// Check if cloudflared is installed.
    Status,
    /// Start a Cloudflare quick tunnel.
    Cloudflare {
        /// Local port to tunnel (auto-detects from config)
        #[arg(long)]
        port: Option<u16>,
    },
}

#[derive(Subcommand, Debug)]
pub enum DyndnsCmd {
    /// Interactive DynDNS setup.
    Setup,
    /// Show current DynDNS configuration.
    Status,
    /// Force an immediate DNS update.
    Update,
    /// Enable DynDNS updates.
    Enable,
    /// Disable DynDNS updates.
    Disable,
}

#[derive(Subcommand, Debug)]
pub enum UpdateCmd {
    /// Compare this binary with the latest release.
    Check,
    /// Download, verify and install the latest release over this binary.
    Apply {
        /// Do not ask for confirmation
        #[arg(short = 'y', long)]
        yes: bool,
        /// After installing, start the new binary's server in this process
        /// (exec `mokuro-bunko serve` with the same global options)
        #[arg(long)]
        restart: bool,
    },
    /// Internal (automatic updates): run by a downloaded release BEFORE it is
    /// installed, to fetch, verify and load-check its own backend pack (for the variant
    /// installed here) and its models, so that switching to it leaves nothing to fetch.
    /// Prints one JSON line; exit 0 ok, 3 the owner must act, 1 try again later.
    #[command(hide = true)]
    Prefetch {
        /// For this machine's processor (its processor.yaml) instead of the library
        #[arg(long, value_name = "PATH")]
        processor_config: Option<PathBuf>,
        /// The latest-release manifest URL to derive this release's from
        #[arg(long)]
        manifest_url: String,
    },
}

#[cfg(feature = "ocr")]
#[derive(Subcommand, Debug)]
pub enum ModelsCmd {
    /// List the models this build knows and which are on disk.
    List {
        #[command(flatten)]
        target: OcrTargetArgs,
    },
    /// Download (and verify) models into <storage>/models.
    Download {
        /// Only the models this engine needs (hayai-nova, paddle-manga, ppocr-manga)
        #[arg(long)]
        engine: Option<String>,
        #[command(flatten)]
        target: OcrTargetArgs,
    },
    /// Check the sha256 of every downloaded model.
    Verify {
        #[command(flatten)]
        target: OcrTargetArgs,
    },
}

/// Whose storage the OCR tools work on.
#[cfg(feature = "ocr")]
#[derive(Args, Debug, Clone, Copy, Default)]
pub struct OcrTargetArgs {
    /// Use this machine's processor storage (processor.storage of its processor.yaml,
    /// found through MOKURO_PROCESSOR_CONFIG) instead of the library's; automatic on a
    /// machine with a processor.yaml and no library configuration
    #[arg(long)]
    pub processor: bool,
}

/// `install-ocr` options (the 0.5.2 ones are accepted: `--backend` maps to `--variant`).
#[derive(Args, Debug, Default)]
pub struct InstallOcrArgs {
    /// Backend pack: auto (detect the GPU), cpu, cu130 (NVIDIA), rocm7.1 (AMD, Linux)
    #[arg(long, value_parser = PossibleValuesParser::new(["auto", "cpu", "cu130", "rocm7.1"]))]
    pub variant: Option<String>,
    /// Install from a directory holding the pack archive (and optionally the signed
    /// release.json and the NVIDIA wheels) instead of downloading
    #[arg(long, value_name = "DIR")]
    pub from: Option<PathBuf>,
    /// Install packs here  [default: <storage>/backends]
    #[arg(long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// Only the backend pack, not the models
    #[arg(long)]
    pub no_models: bool,
    /// Reinstall even when the pack is already there
    #[arg(long)]
    pub force: bool,
    /// Show the detected hardware, the pack it would install and the installed packs
    #[arg(long, alias = "list-backends")]
    pub list: bool,
    /// Install for this machine's processor (into its storage, from its processor.yaml
    /// found through MOKURO_PROCESSOR_CONFIG) instead of the library server; automatic
    /// on a machine with a processor.yaml and no library configuration
    #[arg(long)]
    pub processor: bool,
    /// Internal: load the backend pack this role would use (or the one in --dir) in
    /// this process and list its devices, without installing anything; exit 1 when it
    /// does not load (automatic updates check a new pack this way)
    #[arg(long, hide = true)]
    pub probe: bool,
    /// 0.5.2: auto, cuda, rocm, cpu (same as --variant)
    #[arg(long, hide = true)]
    pub backend: Option<String>,
    #[arg(long, hide = true)]
    pub engines: Option<String>,
    #[arg(long, hide = true)]
    pub detector: Option<String>,
}

#[cfg(feature = "ocr")]
#[derive(Subcommand, Debug)]
pub enum ProcessorCmd {
    /// Log in to the library and process whatever it sends.
    Serve {
        /// Path to processor.yaml
        #[arg(long = "config", env = "MOKURO_PROCESSOR_CONFIG", value_name = "PATH")]
        config: PathBuf,
        /// Log every op and event
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// Set this machine up as a processor: check the account, write the config, start it.
    Setup(ProcessorSetupArgs),
    /// What this processor last did.
    Status {
        /// Path to processor.yaml
        #[arg(long = "config", env = "MOKURO_PROCESSOR_CONFIG", value_name = "PATH")]
        config: PathBuf,
    },
    /// Run this processor as you: a systemd user unit, or on Windows a Startup entry.
    Service {
        /// Path to processor.yaml
        #[arg(long = "config", env = "MOKURO_PROCESSOR_CONFIG", value_name = "PATH")]
        config: PathBuf,
        /// Write it to ~/.config/systemd/user and enable + start it
        #[arg(long = "install")]
        install: bool,
    },
}

#[cfg(feature = "ocr")]
#[derive(Args, Debug)]
pub struct ProcessorSetupArgs {
    /// Where to write processor.yaml
    #[arg(long = "config", default_value = "processor.yaml", value_name = "PATH")]
    pub config: PathBuf,
    /// The library's address, as a browser reaches it.
    #[arg(long)]
    pub url: Option<String>,
    /// A processor account on that library.
    #[arg(long)]
    pub username: Option<String>,
    /// Read the password from the first line of stdin.
    #[arg(long)]
    pub password_stdin: bool,
    /// How the library shows this machine.  [default: the hostname]
    #[arg(long)]
    pub name: Option<String>,
    /// Accepted for 0.5 scripts: the processor uses the devices of the installed OCR
    /// backend pack (install-ocr picks the pack); anything but auto prints how to
    /// narrow it with MOKURO_OCR_BACKEND.
    #[arg(long, default_value = "auto", value_parser = PossibleValuesParser::new(OCR_BACKENDS))]
    pub backend: String,
    /// true, false, or the path of the library's certificate (a self-signed one).
    #[arg(long, default_value = "true")]
    pub tls_verify: String,
    /// Ask nothing: accept every default.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Accepted for 0.5 scripts; there is nothing to install.
    #[arg(long, hide = true)]
    pub no_install: bool,
    /// Do not set up starting it (a systemd user service; on Windows a Startup entry).
    #[arg(long)]
    pub no_service: bool,
    /// Overwrite an existing config file.
    #[arg(long)]
    pub force: bool,
    /// Update automatically to the library's version when it reports a newer one
    /// (processor.auto_update: true): the running volume finishes first. Never a
    /// downgrade.
    #[arg(long)]
    pub auto_update: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_tree_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn delete_invite_takes_dash_codes() {
        let cli = Cli::try_parse_from(["mokuro-bunko", "admin", "delete-invite", "-abc"]).unwrap();
        match cli.command {
            Some(Command::Admin(AdminCmd::DeleteInvite { code })) => assert_eq!(code, "-abc"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn serve_flags_are_optional() {
        let cli = Cli::try_parse_from(["mokuro-bunko", "serve", "--port", "8080"]).unwrap();
        match cli.command {
            Some(Command::Serve(a)) => {
                assert_eq!(a.port, Some(8080));
                assert_eq!(a.host, None);
            }
            other => panic!("{other:?}"),
        }
    }
}
