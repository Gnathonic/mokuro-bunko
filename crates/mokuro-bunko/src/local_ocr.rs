//! Local OCR: the in-process processor over the real engines (full build only).
//!
//! `ocr.backend` picks the devices: `cpu` keeps everything on the CPU, `cuda` / `rocm`
//! limit the recognizer to the libtorch backend pack's NVIDIA / AMD GPUs
//! (`webgpu`/`directml`/`coreml` are the deferred ONNX Runtime providers), `auto` takes
//! the first GPU the installed pack can drive. `skip` turns local processing off (the
//! server never starts the processor then).

use bunko_server::ocr::LocalProcessorFactory;
use std::sync::Arc;

/// The lite build has no local OCR: the server waits for remote processors.
#[cfg(not(feature = "ocr"))]
pub fn factory(_config: &bunko_core::Config) -> Option<Arc<dyn LocalProcessorFactory>> {
    None
}

/// Full build: `bunko_processor::LocalProcessor` over `bunko_engines::EnginePipeline`.
#[cfg(feature = "ocr")]
pub fn factory(config: &bunko_core::Config) -> Option<Arc<dyn LocalProcessorFactory>> {
    Some(Arc::new(full::Factory {
        engines: bunko_engines::EngineConfig {
            models_dir: config.storage.layout().models(),
            backend: bunko_engines::Backend::parse(config.ocr.effective_backend()),
            jobs: config.ocr.concurrency.max(1) as usize,
            generator: format!("mokuro-bunko {}", bunko_core::VERSION),
        },
    }))
}

#[cfg(feature = "ocr")]
mod full {
    use std::path::Path;
    use std::sync::Arc;

    use bunko_engines::{EngineConfig, EnginePipeline};
    use bunko_processor::{LocalConfig, LocalProcessor, PagePipeline};
    use bunko_server::ocr::{LocalChannels, LocalProcessorFactory};

    pub struct Factory {
        pub engines: EngineConfig,
    }

    impl LocalProcessorFactory for Factory {
        fn start(&self, results_dir: &Path) -> Result<LocalChannels, String> {
            let pipeline = Arc::new(EnginePipeline::new(self.engines.clone()));
            let machine = pipeline.describe();
            tracing::info!(
                "Local OCR: {} on {} ({}); engines: {}",
                machine.host.runner_build,
                machine.host.cpu,
                machine.host.backend,
                if machine.catalog.engines.is_empty() {
                    "none yet (models missing and downloads off)".to_string()
                } else {
                    machine.catalog.engines.join(", ")
                }
            );
            let mut link = LocalProcessor::spawn(
                pipeline,
                LocalConfig {
                    results_dir: results_dir.to_path_buf(),
                },
            );
            // Dropping the op sender (when the server stops) is the processor leaving;
            // the server's stop then awaits `finished`, so the sessions' threads are
            // joined and their models freed before `main` returns.
            let finished = link.take_finished();
            Ok(LocalChannels {
                ops: link.ops,
                events: link.events,
                catalog: machine.catalog,
                host: machine.host,
                finished,
            })
        }
    }
}
