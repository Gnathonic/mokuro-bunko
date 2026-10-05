//! `mokuro-bunko` — the command-line entry point: one binary for the server, its admin
//! tools and (full build) the OCR processor. See `cli.rs` for the command tree.

mod cfgfile;
mod cli;
mod cmd;
mod control;
#[cfg(feature = "ocr")]
mod hwdetect;
// `init_server` is for `serve.rs` (orchestrator) and `processor serve`. logging.rs is
// not this CLI's file; its one collapsible `if` is left to its owner.
mod gui;
#[allow(dead_code)]
mod local_ocr;
mod logging;
mod machine;
#[cfg(feature = "ocr")]
mod ocr_probe;
#[cfg(feature = "ocr")]
mod ocr_target;
mod out;
mod prompt;
mod serve;

use clap::{CommandFactory, Parser};
use cli::{Cli, Command};
use mimalloc::MiMalloc;
use out::Fail;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// `full` (local OCR linked in) or `lite` (server only).
/// The release flavor this binary updates to (the `artifacts[target][flavor]` key).
pub const FLAVOR: &str = if cfg!(feature = "cuda") {
    "full-cuda"
} else if cfg!(feature = "webgpu") && cfg!(target_os = "linux") {
    "full-webgpu"
} else if cfg!(feature = "ocr") {
    "full"
} else {
    "lite"
};

/// The flavor the updater looks up (`artifacts[target][flavor]`, `docker[flavor]`).
/// `MOKURO_UPDATE_FLAVOR` overrides it: the CUDA Docker image runs the plain `full`
/// binary but must be told to pull the `-cuda` image (it sets `full-cuda`).
pub fn update_flavor() -> &'static str {
    static F: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    F.get_or_init(|| match std::env::var("MOKURO_UPDATE_FLAVOR") {
        Ok(v) if !v.trim().is_empty() => Box::leak(v.trim().to_string().into_boxed_str()),
        _ => FLAVOR,
    })
}

fn main() {
    // A double-click (Windows Explorer, a macOS app bundle) has no arguments and no
    // terminal of its own: open the desktop app. From a terminal, no arguments still
    // print the help.
    let cli = if std::env::args_os().len() == 1 && double_clicked() {
        Cli::parse_from(["mokuro-bunko", "gui"])
    } else {
        Cli::parse()
    };
    if cli.version {
        println!("mokuro-bunko, version {}", bunko_core::VERSION);
        println!("flavor: {FLAVOR}, target: {}", bunko_update::TARGET);
        return;
    }
    let code = match run(cli) {
        Ok(()) => 0,
        Err(Fail::Exit(code)) => code,
        Err(Fail::Error(msg)) => {
            eprintln!("Error: {msg}");
            1
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> out::CmdResult {
    let Some(command) = cli.command else {
        // click's `invoke_without_command`: show the help, exit 0.
        let _ = Cli::command().print_help();
        println!();
        return Ok(());
    };
    let ctx = cmd::Ctx {
        config_path: cfgfile::resolve(cli.config.as_deref()),
        verbose: cli.verbose,
        cli_config: cli.config,
    };
    match command {
        Command::Serve(args) => cmd::serve::run(&ctx, args),
        Command::Setup { skip_if_exists } => cmd::setup::run(&ctx, skip_if_exists),
        Command::Doctor { processor } => cmd::doctor::run(&ctx, processor),
        Command::Admin(c) => cmd::admin::run(&ctx, c),
        Command::Config(c) => cmd::config::run(&ctx, c),
        Command::Ssl(c) => cmd::ssl::run(&ctx, c),
        Command::Tunnel(c) => cmd::tunnel::run(&ctx, c),
        Command::Dyndns(c) => cmd::dyndns::run(&ctx, c),
        Command::Update(c) => cmd::update::run(&ctx, c),
        #[cfg(feature = "ocr")]
        Command::Models(c) => cmd::models::run(&ctx, c),
        Command::InstallOcr(args) => cmd::install_ocr::run(&ctx, args),
        #[cfg(feature = "ocr")]
        Command::Processor(c) => cmd::processor::run(&ctx, c),
        Command::Healthcheck { url } => cmd::healthcheck::run(&ctx, url),
        Command::Gui(args) => cmd::gui::run(&ctx, args),
    }
}

/// Started without a terminal to talk to: on Windows the console is this process's
/// alone (Explorer made it), on macOS stdin is not a terminal (Finder, an app bundle).
fn double_clicked() -> bool {
    #[cfg(windows)]
    {
        use std::io::IsTerminal;
        let mut ids = [0u32; 4];
        // SAFETY: the buffer and its length match; the call only writes into it.
        let n = unsafe {
            windows_sys::Win32::System::Console::GetConsoleProcessList(ids.as_mut_ptr(), 4)
        };
        n == 1 || !std::io::stdin().is_terminal()
    }
    #[cfg(target_os = "macos")]
    {
        use std::io::IsTerminal;
        !std::io::stdin().is_terminal()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}
