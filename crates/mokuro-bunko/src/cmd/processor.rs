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
        ProcessorCmd::Serve { config, verbose } => serve(ctx, &config, verbose || ctx.verbose),
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
/// build has (`MOKURO_OCR_BACKEND` narrows it), cores shared by its sessions. Backend
/// packs: `MOKURO_BACKENDS_DIR`, `<storage>/backends`, then the library storage's
/// (`install-ocr` without `--processor` on this machine put it there).
fn pipeline(storage: &Path, sessions: u32) -> EnginePipeline {
    let backend = std::env::var("MOKURO_OCR_BACKEND")
        .map(|b| Backend::parse(&b))
        .unwrap_or(Backend::Auto);
    let mut config = EngineConfig::new(storage.join("models"), backend);
    config.jobs = sessions.max(1) as usize;
    config.fallback_backends = crate::ocr_target::library_fallback_backends();
    config
        .fallback_backends
        .extend(crate::ocr_target::shipped_backends());
    EnginePipeline::new(config)
}

fn serve(ctx: &Ctx, config: &Path, verbose: bool) -> CmdResult {
    let cfg = load_processor_config(config).map_err(Fail::msg)?;
    crate::logging::init_server(&cfg.processor.storage, verbose);
    tracing::info!(
        "mokuro-bunko processor {} ({}) for {}",
        bunko_core::VERSION,
        crate::FLAVOR,
        cfg.library.url
    );
    // An automatic update the previous run made: check the new release's OCR backend
    // (a rollback restarts into the previous release and does not return).
    let processor_yaml = std::path::absolute(config).unwrap_or_else(|_| config.to_path_buf());
    let who = crate::autoupdate::Who::Processor {
        config: processor_yaml,
    };
    let started =
        crate::autoupdate::after_restart(&cfg.processor.storage, cfg.processor.auto_update, || {
            crate::autoupdate::probe_child(&who)
        });
    if started.proven {
        crate::autoupdate::prune_models(&cfg.processor.storage);
    }
    if cfg.processor.auto_update {
        tracing::info!(
            "Automatic updates are on (processor.auto_update): this processor follows its library's version"
        );
    }
    let installer: Arc<dyn bunko_update::auto::ReleaseInstaller> =
        Arc::new(crate::autoupdate::Installer::new(
            &cfg.update.manifest_url,
            "stable",
            &cfg.update.public_key,
            who,
            &cfg.processor.storage,
        ));
    let engines = Arc::new(pipeline(&cfg.processor.storage, cfg.processor.max_sessions));
    let backends = engines.config().backends_dirs();
    let (name, storage, library_url) = (
        cfg.processor.name.clone(),
        cfg.processor.storage.clone(),
        cfg.library.url.clone(),
    );
    let mut options = ServeOptions::new(cfg, engines);
    options.verbose = verbose;
    options.installer = Some(installer);
    // The OCR backend installs in the background when it is missing: the processor
    // registers at once, as not available until it is done (crate::ocr_install).
    let ocr_installer = crate::ocr_install::Installer::new(crate::ocr_install::Who::Processor {
        config: processor_config_path(config),
    });
    options.install = Some(ocr_installer.subscribe());
    let shutdown = options.shutdown.clone();
    let config_path = ctx.config_path.clone();
    let processor_config = std::path::absolute(config).unwrap_or_else(|_| config.to_path_buf());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("processor")
        .build()?;
    let result = runtime.block_on(async move {
        let signal = shutdown.clone();
        tokio::spawn(async move {
            bunko_server::serve::shutdown_signal().await;
            tracing::info!("Stopping the processor");
            signal.cancel();
        });
        // Only the instance that will hold the storage installs (a second one fails
        // below).
        let installs = storage_free(&storage);
        // The local control API (GUI.md §2): pause/resume, status, the app pages. Only
        // the instance holding the storage lock serves it (a second one fails below).
        let control_server = if bunko_control::enabled_from_env() && storage_free(&storage) {
            let control = bunko_control::Control::new(crate::control::config(
                bunko_control::Role::Processor,
                name,
                &storage,
                true,
            ));
            control.set_stop(shutdown.clone());
            control.set_library_url(&library_url);
            control.set_backend_pack(crate::control::pack_name(&backends));
            control.set_load_probe(Arc::new(bunko_processor::utilization::LiveLoad::new(
                std::time::Duration::from_secs(5),
            )));
            let target = crate::ocr_target::OcrTarget {
                role: crate::ocr_target::Role::Processor,
                storage: storage.clone(),
                processor_config: Some(processor_config.clone()),
                library: None,
                reason: String::new(),
            };
            ocr_installer.attach_control(&control);
            let watched = ocr_installer.clone();
            crate::control::watch_problems(
                control.clone(),
                shutdown.clone(),
                Some(bunko_server::ocr::BackgroundInstall::finished(
                    &ocr_installer,
                )),
                move || {
                    let mut problems: Vec<_> =
                        if crate::ocr_install::speaks_for_backend(Some(&watched)) {
                            Vec::new()
                        } else {
                            super::doctor::backend_problem(&target)
                                .into_iter()
                                .collect()
                        };
                    problems.extend(super::doctor::control_problems(&target.storage, false));
                    problems
                },
            );
            control.set_update(started.view);
            control.set_update_problems(started.problems);
            options.control = Some(control.clone());
            crate::control::start(&control, config_path, Some(processor_config)).await
        } else {
            None
        };
        if installs {
            ocr_installer.start_if_needed();
        }
        let result = bunko_processor::serve(options).await;
        if let Some(server) = control_server {
            server.shutdown().await;
        }
        result
    });
    match result {
        Ok(bunko_processor::ServeExit::Stopped) => Ok(()),
        Ok(bunko_processor::ServeExit::Updated(version)) => {
            // The storage lock and the control listener are released: start the new
            // release in this process's place (exec on Unix, so a service manager or
            // the tray keeps supervising the same pid; exit 75 under the Windows tray).
            drop(runtime);
            tracing::info!("Restarting into mokuro-bunko {version}");
            let Err(e) = bunko_update::restart();
            Err(Fail::msg(format!(
                "installed {version} but could not restart: {e}; start the processor again"
            )))
        }
        Err(e @ ServeError::LoginRefused(_)) => {
            tracing::error!("{e}");
            eprintln!("{e}");
            Err(Fail::Exit(1))
        }
        Err(e) => Err(Fail::msg(e)),
    }
}

/// processor.yaml as an absolute path (the install child runs elsewhere).
fn processor_config_path(config: &Path) -> PathBuf {
    std::path::absolute(config).unwrap_or_else(|_| config.to_path_buf())
}

/// Nobody else serves this storage (`bunko_processor::lock`): probed and released, the
/// processor's loop takes it for real.
fn storage_free(storage: &Path) -> bool {
    matches!(bunko_processor::lock::lock_storage(storage), Ok(Some(_)))
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
        auto_update: args.auto_update,
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
