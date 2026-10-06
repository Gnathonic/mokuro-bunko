//! `serve`: validate, set up logging and the runtime, assemble the server and run it.
//!
//! The CLI has already loaded the config (file + `MOKURO_*` env) and applied the
//! explicitly passed flags; `config_path` is where the admin API saves.

use crate::cli::ServeArgs;
use bunko_core::Config;
use bunko_server::app::{self, ServeOptions, Services};
use std::path::PathBuf;
use tracing::{info, warn};

pub fn run(args: ServeArgs, config: Config, config_path: PathBuf) -> anyhow::Result<()> {
    if let Err(msg) = app::validate_startup(&config) {
        println!("Startup validation failed: {msg}");
        std::process::exit(2);
    }
    crate::logging::init_server(&config.storage.base_path, args.verbose);
    for w in &config.warnings {
        warn!("{w}");
    }
    let threads = match config.server.threads {
        0 => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .min(4),
        n => n as usize,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .max_blocking_threads(64)
        .thread_name("bunko")
        .enable_all()
        .build()?;
    let flavor = crate::update_flavor();
    // An automatic update the previous run made: check the new release's OCR backend
    // (a rollback restarts into the previous release and does not return).
    let who = crate::autoupdate::Who::Library {
        cli_config: Some(config_path.clone()),
    };
    let started =
        crate::autoupdate::after_restart(&config.storage.base_path, config.update.auto, || {
            if cfg!(feature = "ocr") {
                crate::autoupdate::probe_child(&who)
            } else {
                Ok(())
            }
        });
    if started.proven {
        crate::autoupdate::prune_models(&config.storage.base_path);
    }
    if config.update.auto {
        info!(
            "Automatic updates are on (update.auto): a newer {} release installs itself when nothing is running",
            config.update.channel
        );
    }
    let installer: std::sync::Arc<dyn bunko_update::auto::ReleaseInstaller> =
        std::sync::Arc::new(crate::autoupdate::Installer::new(
            &config.update.manifest_url,
            &config.update.channel,
            &config.update.public_key,
            who,
            &config.storage.base_path,
        ));
    runtime
        .block_on(async move {
            info!("mokuro-bunko {} ({flavor})", bunko_core::VERSION);
            info!("Storage path: {}", config.storage.base_path.display());
            info!(
                "Server log: {}",
                config
                    .storage
                    .base_path
                    .join("logs")
                    .join(crate::logging::SERVER_LOG_NAME)
                    .display()
            );
            // The local control API (GUI.md §2): the tray and the desktop app pages.
            let ocr_here = config.processes_locally(cfg!(feature = "ocr"));
            let control = bunko_control::enabled_from_env().then(|| {
                bunko_control::Control::new(crate::control::config(
                    bunko_control::Role::Server,
                    crate::control::hostname(),
                    &config.storage.base_path,
                    ocr_here,
                ))
            });
            let opts = ServeOptions {
                verbose: args.verbose,
                flavor,
                local: crate::local_ocr::factory(&config, control.clone()),
            };
            let control_setup = control
                .as_ref()
                .map(|_| ControlSetup::of(&config, ocr_here));
            let services = Services::new(config, Some(config_path.clone()), &opts)?;
            app::announce_setup(&services);
            services.updates.set_view(started.view, started.problems);
            if let Some(c) = &control {
                let c = c.clone();
                services
                    .updates
                    .set_reporter(std::sync::Arc::new(move |view, problems| {
                        c.set_update(view);
                        c.set_update_problems(problems);
                    }));
            }
            let control_server = match (&control, control_setup) {
                (Some(c), Some(setup)) => setup.start(c, &services, config_path).await,
                _ => None,
            };
            let router = app::assemble(&services, &opts);
            println!("Press Ctrl+C to stop");
            let served = app::serve_router_with(&services, router, Some(installer)).await;
            if let Some(server) = control_server {
                server.shutdown().await;
            }
            served?;
            Ok::<_, anyhow::Error>(
                services
                    .restart_requested
                    .load(std::sync::atomic::Ordering::SeqCst),
            )
        })
        .and_then(|restart| {
            if restart {
                info!("Restarting into the updated binary");
                bunko_update::restart()?;
            }
            Ok(())
        })
}

/// What the server's control API needs from the config before `Services` takes it.
struct ControlSetup {
    storage: PathBuf,
    library_url: String,
    #[cfg(feature = "ocr")]
    target: Option<crate::ocr_target::OcrTarget>,
}

impl ControlSetup {
    fn of(config: &Config, ocr_here: bool) -> ControlSetup {
        #[cfg(not(feature = "ocr"))]
        let _ = ocr_here;
        ControlSetup {
            storage: config.storage.base_path.clone(),
            library_url: crate::control::library_url(config),
            // The backend pack only matters when this server reads OCR itself.
            #[cfg(feature = "ocr")]
            target: ocr_here.then(|| crate::ocr_target::OcrTarget {
                role: crate::ocr_target::Role::Library,
                storage: config.storage.base_path.clone(),
                processor_config: None,
                library: None,
                reason: String::new(),
            }),
        }
    }

    /// Keep the control current (stop, queue, problems, load) and start its listener.
    async fn start(
        self,
        control: &bunko_control::Control,
        services: &Services,
        config_path: PathBuf,
    ) -> Option<bunko_control::ControlServer> {
        let stop = services.stop.clone();
        control.set_stop(stop.clone());
        control.set_library_url(&self.library_url);
        crate::control::watch_queue(
            control.clone(),
            self.library_url.clone(),
            services.ocr.clone(),
            stop.clone(),
        );
        #[cfg(feature = "ocr")]
        if let Some(target) = &self.target {
            control.set_backend_pack(crate::control::pack_name(&target.backends_dirs()));
            control.set_load_probe(std::sync::Arc::new(
                bunko_processor::utilization::LiveLoad::new(std::time::Duration::from_secs(5)),
            ));
        }
        let storage = self.storage;
        #[cfg(feature = "ocr")]
        let target = self.target;
        crate::control::watch_problems(control.clone(), stop, move || {
            #[allow(unused_mut)]
            let mut problems = Vec::new();
            #[cfg(feature = "ocr")]
            if let Some(t) = &target {
                problems.extend(crate::cmd::doctor::backend_problem(t));
            }
            problems.extend(crate::cmd::doctor::control_problems(&storage, true));
            problems
        });
        crate::control::start(control, config_path, None).await
    }
}
