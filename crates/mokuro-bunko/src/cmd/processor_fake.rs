//! `processor-fake` (testing build, hidden): a remote OCR processor with no models, for
//! the interop rig. It logs in, registers and holds its socket as `processor serve`
//! does, over `bunko-processor`'s `FakePipeline` (blank pages, one per image), so a
//! lite build can OCR. `--hold-file` holds each volume mid-OCR until the file exists
//! ("rename during OCR"); `--engine-revision` stamps its sidecars for upgrade
//! scenarios. No control API, no automatic update, no OCR install, console logging.

use super::Ctx;
use crate::cli::ProcessorFakeArgs;
use crate::out::{CmdResult, Fail};
use bunko_processor::config::{LibrarySettings, ProcessorSettings, TlsVerify, UpdateSettings};
use bunko_processor::{FakeConfig, FakePipeline, ProcessorConfig, ServeError, ServeOptions};
use std::sync::Arc;
use std::time::Duration;

pub fn run(ctx: &Ctx, args: ProcessorFakeArgs) -> CmdResult {
    crate::logging::init_console(args.verbose || ctx.verbose);
    let engines = if args.engines.is_empty() {
        bunko_core::engines::ENGINES
            .iter()
            .map(|e| e.id.to_string())
            .collect()
    } else {
        args.engines
    };
    let fake = FakePipeline::new(FakeConfig {
        load_delay: Duration::from_millis(args.load_delay_ms),
        page_delay: Duration::from_millis(args.page_delay_ms),
        engines,
        hold_file: args.hold_file,
        engine_revision: args.engine_revision,
        ..FakeConfig::default()
    });
    let config = ProcessorConfig {
        library: LibrarySettings {
            url: args.url.trim_end_matches('/').to_string(),
            username: args.username,
            password: args.password,
            tls_verify: TlsVerify::Yes,
        },
        processor: ProcessorSettings {
            name: args.name,
            max_sessions: args.max_sessions.max(1),
            storage: args.storage,
            ..ProcessorSettings::default()
        },
        update: UpdateSettings::default(),
    };
    tracing::info!(
        "mokuro-bunko processor-fake {} for {}",
        bunko_core::VERSION,
        config.library.url
    );
    let mut options = ServeOptions::new(config, Arc::new(fake));
    options.verbose = args.verbose || ctx.verbose;
    let shutdown = options.shutdown.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("processor-fake")
        .build()?;
    let result = runtime.block_on(async move {
        tokio::spawn(async move {
            bunko_server::serve::shutdown_signal().await;
            shutdown.cancel();
        });
        bunko_processor::serve(options).await
    });
    match result {
        // No installer: an update is never installed, only reported.
        Ok(_) => Ok(()),
        Err(e @ ServeError::LoginRefused(_)) => {
            eprintln!("{e}");
            Err(Fail::Exit(1))
        }
        Err(e) => Err(Fail::msg(e)),
    }
}
