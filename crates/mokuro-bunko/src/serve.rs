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
        0 => std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).min(4),
        n => n as usize,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .max_blocking_threads(64)
        .thread_name("bunko")
        .enable_all()
        .build()?;
    let flavor = crate::FLAVOR;
    runtime.block_on(async move {
        info!("mokuro-bunko {} ({flavor})", bunko_core::VERSION);
        info!("Storage path: {}", config.storage.base_path.display());
        info!("Server log: {}", config.storage.base_path.join("logs").join(crate::logging::SERVER_LOG_NAME).display());
        let services = Services::new(config, Some(config_path), flavor)?;
        let opts = ServeOptions { verbose: args.verbose, flavor };
        app::announce_setup(&services);
        let router = app::assemble(&services, &opts);
        println!("Press Ctrl+C to stop");
        app::serve_router(&services, router).await?;
        Ok::<_, anyhow::Error>(services.restart_requested.load(std::sync::atomic::Ordering::SeqCst))
    })
    .and_then(|restart| {
        if restart {
            info!("Restarting into the updated binary");
            bunko_update::restart()?;
        }
        Ok(())
    })
}
