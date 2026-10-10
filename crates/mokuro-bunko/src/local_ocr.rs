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
pub fn factory(
    _config: &bunko_core::Config,
    _control: Option<bunko_control::Control>,
) -> Option<Arc<dyn LocalProcessorFactory>> {
    None
}

/// The installer the full build's factory carries (none in the lite build).
#[cfg(feature = "ocr")]
pub type Installer = crate::ocr_install::Installer;

/// Full build: `bunko_processor::LocalProcessor` over `bunko_engines::EnginePipeline`.
/// Under the control API (`control`), the local OCR obeys its pause and reports into
/// its status (GUI.md §2, §3).
#[cfg(feature = "ocr")]
pub fn factory(
    config: &bunko_core::Config,
    control: Option<bunko_control::Control>,
    installer: Option<Installer>,
) -> Option<Arc<dyn LocalProcessorFactory>> {
    Some(Arc::new(full::Factory {
        control,
        installer,
        engines: bunko_engines::EngineConfig {
            models_dir: config.storage.layout().models(),
            backend: bunko_engines::Backend::parse(config.ocr.effective_backend()),
            jobs: config.ocr.concurrency.max(1) as usize,
            generator: format!("mokuro-bunko {}", bunko_core::VERSION),
            fallback_backends: crate::ocr_target::shipped_backends().into_iter().collect(),
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
        pub control: Option<bunko_control::Control>,
        pub installer: Option<super::Installer>,
    }

    impl LocalProcessorFactory for Factory {
        fn installer(&self) -> Option<Arc<dyn bunko_server::ocr::BackgroundInstall>> {
            self.installer
                .clone()
                .map(|i| Arc::new(i) as Arc<dyn bunko_server::ocr::BackgroundInstall>)
        }

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
            if let Some(c) = &self.control {
                c.set_devices(&machine.catalog.devices);
            }
            let mut link = LocalProcessor::spawn_controlled(
                pipeline,
                LocalConfig {
                    results_dir: results_dir.to_path_buf(),
                },
                bunko_processor::BenchConfig::default(),
                self.control.clone(),
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
