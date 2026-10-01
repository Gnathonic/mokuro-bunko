//! `processor serve|setup|status|service` (full build): run this machine as a remote OCR
//! processor for a library (spec remote-processors.md; 0.5.2 `processor/cli.py`), over
//! `bunko-processor`'s runtime and `bunko-engines`.
//!
//! * `serve`: log to `<processor storage>/logs`, then the processor's loop (log in,
//!   register, hold the socket, reconnect with backoff). SIGINT/SIGTERM stop it
//!   cleanly; a refused login exits 1.
//! * `setup`: the wizard (account check, `processor.yaml`, service); `--no-install` and
//!   `--backend` are accepted for 0.5 scripts (nothing to install; the backend is picked
//!   from what this build and machine can run).
//! * `status`: the last status file under the processor storage.
//! * `service`: print (or with `--install`, install and start) this platform's service.

use super::Ctx;
use crate::cli::{ProcessorCmd, ProcessorSetupArgs};
use crate::out::{self, CmdResult, Fail};
use bunko_engines::{Backend, EngineConfig, EnginePipeline};
use bunko_processor::setup::{Prompter, ServiceStarted, SetupOptions};
use bunko_processor::{PagePipeline, ServeError, ServeOptions, load_processor_config};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn run(ctx: &Ctx, cmd: ProcessorCmd) -> CmdResult {
    match cmd {
        ProcessorCmd::Serve { config, verbose } => serve(&config, verbose || ctx.verbose),
        ProcessorCmd::Setup(args) => setup(ctx, args),
        ProcessorCmd::Status { config } => {
            let cfg = load_processor_config(&config).map_err(Fail::msg)?;
            println!(
                "{}",
                bunko_processor::status::status_line(&cfg.processor.storage)
            );
            Ok(())
        }
        ProcessorCmd::Service { config, install } => service(&config, install),
    }
}

/// The engines of a processor: models under `<storage>/models`, every provider this
/// build has (`MOKURO_OCR_BACKEND` narrows it), cores shared by its sessions.
fn pipeline(storage: &Path, sessions: u32) -> EnginePipeline {
    let backend = std::env::var("MOKURO_OCR_BACKEND")
        .map(|b| Backend::parse(&b))
        .unwrap_or(Backend::Auto);
    let mut config = EngineConfig::new(storage.join("models"), backend);
    config.jobs = sessions.max(1) as usize;
    EnginePipeline::new(config)
}

fn serve(config: &Path, verbose: bool) -> CmdResult {
    let cfg = load_processor_config(config).map_err(Fail::msg)?;
    crate::logging::init_server(&cfg.processor.storage, verbose);
    tracing::info!(
        "mokuro-bunko processor {} ({}) for {}",
        bunko_core::VERSION,
        crate::FLAVOR,
        cfg.library.url
    );
    let engines = Arc::new(pipeline(&cfg.processor.storage, cfg.processor.max_sessions));
    let mut options = ServeOptions::new(cfg, engines);
    options.verbose = verbose;
    let shutdown = options.shutdown.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("processor")
        .build()?;
    let result = runtime.block_on(async move {
        tokio::spawn(async move {
            bunko_server::serve::shutdown_signal().await;
            tracing::info!("Stopping the processor");
            shutdown.cancel();
        });
        bunko_processor::serve(options).await
    });
    match result {
        Ok(()) => Ok(()),
        Err(e @ ServeError::LoginRefused(_)) => {
            tracing::error!("{e}");
            eprintln!("{e}");
            Err(Fail::Exit(1))
        }
        Err(e) => Err(Fail::msg(e)),
    }
}

/// The wizard's terminal: click-style prompts, scriptable through stdin.
struct Terminal;

impl Prompter for Terminal {
    fn say(&mut self, line: &str) {
        println!("{line}");
    }

    fn ask(&mut self, prompt: &str, hidden: bool) -> std::io::Result<String> {
        let answer = if hidden {
            crate::prompt::hidden(prompt, false)
        } else {
            crate::prompt::text(prompt, None)
        };
        answer.map_err(|e| std::io::Error::other(fail_text(e)))
    }

    fn confirm(&mut self, question: &str, default: bool) -> std::io::Result<bool> {
        crate::prompt::confirm(question, Some(default))
            .map_err(|e| std::io::Error::other(fail_text(e)))
    }
}

fn fail_text(e: Fail) -> String {
    match e {
        Fail::Error(m) => m,
        Fail::Exit(code) => format!("aborted (exit {code})"),
    }
}

fn setup(ctx: &Ctx, args: ProcessorSetupArgs) -> CmdResult {
    crate::logging::init_console(ctx.verbose);
    if args.no_install {
        println!("--no-install: there is nothing to install any more (OCR is built in).");
    }
    if args.backend != "auto" {
        println!(
            "--backend {}: the processor uses what this build and machine can run; set MOKURO_OCR_BACKEND={} in its environment to narrow it.",
            args.backend, args.backend
        );
    }
    let password = if args.password_stdin {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Some(line.trim_end_matches(['\r', '\n']).to_string())
    } else {
        None
    };
    let tls_verify =
        bunko_processor::setup::parse_tls_verify(&args.tls_verify).map_err(Fail::msg)?;
    let machine = {
        let probe = pipeline(&bunko_processor::config::default_storage_path(), 1);
        Some(probe.describe())
    };
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "mokuro-bunko".into());
    let options = SetupOptions {
        config: args.config,
        url: args.url,
        username: args.username,
        password,
        name: args.name,
        tls_verify,
        yes: args.yes,
        service: !args.no_service,
        force: args.force,
        machine,
        command: exe,
    };
    let step = |config: &Path| -> Result<ServiceStarted, String> {
        let installed = bunko_processor::service::install(config).map_err(|e| e.to_string())?;
        for line in &installed.messages {
            println!("{line}");
        }
        Ok(ServiceStarted {
            running: installed.running,
            logs: installed.logs,
        })
    };
    let runtime = out::runtime()?;
    let mut terminal = Terminal;
    runtime
        .block_on(bunko_processor::setup::run(
            &options,
            &mut terminal,
            Some(&step),
        ))
        .map_err(Fail::msg)
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

fn service(config: &Path, install: bool) -> CmdResult {
    let config = absolute(config);
    // Refuse to install a service over a config that would not start.
    load_processor_config(&config).map_err(Fail::msg)?;
    if install {
        let installed = bunko_processor::service::install(&config).map_err(Fail::msg)?;
        for line in installed.messages {
            println!("{line}");
        }
        return Ok(());
    }
    let rendered = bunko_processor::service::render(&config).map_err(Fail::msg)?;
    println!("# {}", rendered.path.display());
    println!("{}", rendered.text);
    Ok(())
}
