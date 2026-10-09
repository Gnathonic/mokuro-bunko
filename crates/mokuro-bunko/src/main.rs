//! `mokuro-bunko` — the command-line entry point: one binary for the server, its admin
//! tools and (full build) the OCR processor. See `cli.rs` for the command tree.

mod autoupdate;
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
#[cfg(feature = "tray")]
mod migrate;
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
/// `MOKURO_UPDATE_FLAVOR` overrides it (a container that should be told to pull another
/// tag, e.g. `full-cuda` for `:<ver>-cuda`; the release images no longer set it: `-cuda`
/// is a tag of the full image).
pub fn update_flavor() -> &'static str {
    static F: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    F.get_or_init(|| match std::env::var("MOKURO_UPDATE_FLAVOR") {
        Ok(v) if !v.trim().is_empty() => Box::leak(v.trim().to_string().into_boxed_str()),
        _ => FLAVOR,
    })
}

fn main() {
    // Old macOS versions pass -psn_… to apps the Finder starts.
    let args: Vec<std::ffi::OsString> = std::env::args_os()
        .enumerate()
        .filter(|(i, a)| *i == 0 || !a.to_string_lossy().starts_with("-psn_"))
        .map(|(_, a)| a)
        .collect();
    let cli = if args.len() == 1 {
        match no_argument_start() {
            Start::Help => Cli::parse_from(args),
            Start::Run(cmd) => Cli::parse_from(["mokuro-bunko", cmd]),
            Start::Done => return,
        }
    } else {
        Cli::parse_from(args)
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
        #[cfg(feature = "tray")]
        Command::Tray(args) => cmd::tray::run(&ctx, args),
    }
}

enum Start {
    /// The help (a terminal, a script).
    Help,
    /// This command.
    Run(&'static str),
    /// Nothing more to do.
    #[cfg_attr(not(windows), allow(dead_code))]
    Done,
}

/// What a start without arguments runs. From a terminal: the help. Started by the
/// desktop: the Windows GUI build (`Mokuro Bunko.exe`) and the macOS app run the tray,
/// which opens the setup wizard on a machine with nothing set up; the Windows command
/// line double-clicked in Explorer starts `Mokuro Bunko.exe` above its `bin` folder and
/// exits, or else opens the desktop app pages (`gui`).
fn no_argument_start() -> Start {
    if windows_gui_subsystem() {
        return Start::Run(if cfg!(feature = "tray") {
            "tray"
        } else {
            "gui"
        });
    }
    if !double_clicked() {
        return Start::Help;
    }
    #[cfg(windows)]
    if let Some(gui) = bunko_update::current_exe().ok().and_then(|exe| {
        let root = exe.parent()?.parent()?;
        let gui = root.join(bunko_update::layout::WINDOWS_GUI_EXE);
        gui.is_file().then_some(gui)
    }) && std::process::Command::new(&gui).spawn().is_ok()
    {
        return Start::Done;
    }
    if cfg!(all(feature = "tray", target_os = "macos")) {
        Start::Run("tray")
    } else {
        Start::Run("gui")
    }
}

/// This is the GUI-subsystem build (`Mokuro Bunko.exe`: Windows gives it no console).
fn windows_gui_subsystem() -> bool {
    #[cfg(windows)]
    {
        // SAFETY: the module handle of this executable stays valid while it runs; the
        // reads stay inside its mapped PE headers (DOS header e_lfanew, then the
        // optional header's Subsystem field at offset 68, the same in PE32 and PE32+).
        unsafe {
            let base = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null())
                as *const u8;
            if base.is_null() {
                return false;
            }
            let e_lfanew = std::ptr::read_unaligned(base.add(0x3c) as *const u32) as usize;
            let subsystem =
                std::ptr::read_unaligned(base.add(e_lfanew + 4 + 20 + 68) as *const u16);
            subsystem == bunko_update::layout::PE_SUBSYSTEM_GUI
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Started without a terminal to talk to: on Windows the console is this process's
/// alone (Explorer or a shortcut made it), on macOS LaunchServices started it (Finder,
/// the Dock, `open`: a child of launchd, stdin not a terminal). A script, `ssh host
/// mokuro-bunko` or a test harness without a terminal is neither, and gets the help.
fn double_clicked() -> bool {
    #[cfg(windows)]
    {
        let mut ids = [0u32; 4];
        // SAFETY: the buffer and its length match; the call only writes into it.
        let n = unsafe {
            windows_sys::Win32::System::Console::GetConsoleProcessList(ids.as_mut_ptr(), 4)
        };
        n == 1
    }
    #[cfg(target_os = "macos")]
    {
        use std::io::IsTerminal;
        // SAFETY: getppid has no preconditions.
        !std::io::stdin().is_terminal() && unsafe { libc::getppid() } == 1
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}
