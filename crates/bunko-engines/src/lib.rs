//! mokuro-bunko's OCR engines as a processor pipeline (full build only).
//!
//! [`EnginePipeline`] implements `bunko_processor::PagePipeline` for the three
//! engines of 0.7:
//!
//! * **ppocr-manga** (the `line` road): `detect` (decode, PP-OCR detection + CTC read,
//!   column joins, end probes, kanji votes — `bunko-ocr`'s `PpocrPageReader` with the
//!   layout hooks from `bunko-layout`) → `layout` (the line layout into a mokuro page);
//! * **hayai-nova** / **paddle-manga** (the `reconciled` road): `detect` as above →
//!   `engine` (the recognizer's batched first read with per-line token caps,
//!   reconcile with the CTC read, paddle-manga's second read of disputed lines,
//!   engine-only verdicts, seam trim — `bunko-vlm` + `bunko-layout`'s `EngineRoad`) →
//!   `post` (layout into a mokuro page).
//!
//! Stages are pools of OS threads joined by bounded queues ([`stages`]), sized from
//! the row's `pools` and the derived widths ([`plan`]); their counters feed the
//! `stats` events and each volume's window. Models are shared: one PP-OCR detector /
//! recognizer pair (with one ORT session per detect worker) and one recognizer per
//! (engine, assets, device, precision) across sessions ([`bunko_vlm::RecognizerCache`]).
//! Model files come from [`bunko_ocr::models::ModelStore`]; missing ones are downloaded
//! when a session opens ([`models`]). Precision modes resolve per device
//! ([`precision`]); the recognizer runs on the device `pools.stage_device.engine`
//! names (or the first GPU), falling back to the CPU with a logged reason.
//!
//! Sidecars are written as 0.5.2's runner wrote them (`bunko-layout`'s writer:
//! `title`/`volume`/uuids from the op, the `ocr_engine` block, version 0.2.5, default
//! separators, `<out>.tmp` + rename). Not written any more: the per-page progress
//! files, the raw dumps and `review.json` (0.5.2 artefacts read only by the runner's
//! own tooling; the protocol carries progress).

pub mod models;
pub mod page;
pub mod plan;
pub mod precision;
pub mod runtime;
pub mod session;
pub mod stages;

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Instant;

use bunko_ocr::models::{Manifest, ModelStore, StoreOptions};
use bunko_ocr::ppocr::{PpOcr, PpocrPageReader};
use bunko_ocr::runtime::{ExecutionTarget, RuntimeOptions};
use bunko_processor::{MachineInfo, PagePipeline, ReadyInfo, VolumeRunner};
use bunko_proto::{Catalog, RowSpec};
use bunko_vlm::{
    EngineKey, OrtSessionFactory, Precision, Recognizer, RecognizerCache, SessionOptions,
};
use parking_lot::Mutex;
use tracing::{info, warn};

pub use runtime::Backend;

use crate::page::Engines;
use crate::plan::{Road, SOURCE_EXTRA, STAGE_DETECT};
use crate::runtime::Placement;
use crate::session::{EngineRunner, SessionInfo};

/// How an [`EnginePipeline`] is set up.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// `<storage>/models`: where model files are kept (and downloaded to).
    pub models_dir: PathBuf,
    /// Which execution providers may be used.
    pub backend: Backend,
    /// OCR jobs that may run at once on this host (`ocr.concurrency` / processor
    /// sessions): the host's cores are shared between them.
    pub jobs: usize,
    /// `ocr_engine.generator`: `"mokuro-bunko <version>"`.
    pub generator: String,
}

impl EngineConfig {
    pub fn new(models_dir: PathBuf, backend: Backend) -> EngineConfig {
        EngineConfig {
            models_dir,
            backend,
            jobs: 1,
            generator: format!("mokuro-bunko {}", env!("CARGO_PKG_VERSION")),
        }
    }

    /// The model store: `models_dir`, with the `MOKURO_MODELS_DIR` override and the
    /// `MOKURO_MODELS_DOWNLOAD` switch from the environment.
    pub fn store(&self) -> ModelStore {
        let env = StoreOptions::from_env(&self.models_dir);
        ModelStore::new(
            StoreOptions {
                root: self.models_dir.clone(),
                override_dir: env.override_dir,
                download: env.download,
            },
            Manifest::builtin(),
        )
    }
}

/// The OCR engines of this machine (one per processor; shared by its sessions).
pub struct EnginePipeline {
    config: EngineConfig,
    store: ModelStore,
    recognizers: RecognizerCache,
    /// The PP-OCR pair, shared by every session that needs no more detect workers
    /// than it has sessions.
    ppocr: Mutex<Option<(usize, Weak<PpocrPageReader>)>>,
    /// One load at a time: model loads are memory-heavy.
    loading: Mutex<()>,
}

impl EnginePipeline {
    pub fn new(config: EngineConfig) -> EnginePipeline {
        runtime::init();
        let store = config.store();
        EnginePipeline {
            config,
            store,
            recognizers: RecognizerCache::new(Arc::new(OrtSessionFactory)),
            ppocr: Mutex::new(None),
            loading: Mutex::new(()),
        }
    }

    pub fn store(&self) -> &ModelStore {
        &self.store
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    fn reader(&self, copies: usize, spinning: bool) -> Result<Arc<PpocrPageReader>, String> {
        let mut slot = self.ppocr.lock();
        if let Some((have, weak)) = slot.as_ref()
            && *have >= copies
            && let Some(r) = weak.upgrade()
        {
            return Ok(r);
        }
        let files = models::ppocr_files(&self.store)?;
        let opts = RuntimeOptions {
            intra_threads: plan::SESSION_THREADS,
            targets: vec![ExecutionTarget::Cpu],
            copies,
            spinning,
        };
        let engine = PpOcr::load(&files.detector, &files.recognizer, &files.dictionary, &opts)
            .map_err(|e| format!("could not load the PP-OCR models: {e}"))?
            .with_prep_threads(2);
        let reader = Arc::new(PpocrPageReader::new(engine, files.pinned));
        *slot = Some((copies, Arc::downgrade(&reader)));
        Ok(reader)
    }

    /// Load `engine`'s recognizer on `placement` at `precision`.
    fn recognizer(
        &self,
        spec: &RowSpec,
        placement: &Placement,
        precision: Precision,
    ) -> Result<Arc<dyn Recognizer>, String> {
        let cpus = plan::physical_cpu_count();
        let opts = SessionOptions {
            device: placement.vlm_device,
            provider: placement.provider,
            // A GPU session needs few host threads; a CPU one gets this job's cores.
            intra_threads: if placement.is_gpu() {
                4
            } else {
                (cpus.saturating_sub(1) / self.config.jobs.max(1)).max(1)
            },
            spinning: placement.is_gpu(),
            sharing: Default::default(),
        };
        let key = match spec.engine.as_str() {
            models::HAYAI => EngineKey::Hayai {
                assets: models::hayai_assets(&self.store, precision)?,
                opts,
                budget: spec.patch_budget as usize,
            },
            models::PADDLE => EngineKey::Paddle {
                assets: models::paddle_assets(&self.store, precision)?,
                opts,
            },
            other => return Err(format!("{other} has no recognizer")),
        };
        self.recognizers
            .get(&key)
            .map_err(|e| format!("could not load {}: {e}", spec.engine))
    }

    fn open_session(&self, spec: &RowSpec) -> Result<EngineRunner, String> {
        let started = Instant::now();
        let engine = spec.engine.as_str();
        let road = Road::of(engine).ok_or_else(|| {
            format!(
                "unknown engine {engine} (this processor runs {})",
                models::ENGINES.join(", ")
            )
        })?;
        if let Some(d) = spec
            .detector
            .as_deref()
            .filter(|d| !d.is_empty() && *d != models::PPOCR)
        {
            return Err(format!("unknown detector {d} (only ppocr-manga is left)"));
        }
        let _one_load = self.loading.lock();
        let devices = runtime::devices(self.config.backend);

        // Where the recognizer runs, and in which format.
        let mut placement = Placement::cpu();
        let mut resolved = None;
        if road == Road::Reconciled {
            let (p, why) = runtime::place(
                spec.pools
                    .stage_device
                    .get(plan::STAGE_ENGINE)
                    .map(String::as_str),
                &devices,
            );
            if let Some(why) = why {
                warn!("{engine}: running on the CPU: {why}");
            }
            placement = p;
            resolved = precision::resolve(
                engine,
                &spec.precision,
                precision::supported(placement.is_gpu()),
                spec.precision_pick.as_deref(),
                &spec.precision_why,
            )?;
        }

        let mut recognizer = None;
        if let Some(r) = &resolved {
            match self.recognizer(spec, &placement, r.precision) {
                Ok(rec) => recognizer = Some(rec),
                Err(e) if placement.is_gpu() => {
                    warn!(
                        "{engine} could not start on {} ({e}); falling back to the CPU",
                        placement.device
                    );
                    placement = Placement::cpu();
                    let again = precision::resolve(
                        engine,
                        &spec.precision,
                        precision::supported(false),
                        spec.precision_pick.as_deref(),
                        &spec.precision_why,
                    )?;
                    resolved = again;
                    let p = resolved.as_ref().map_or(Precision::Fp32, |r| r.precision);
                    recognizer = Some(self.recognizer(spec, &placement, p)?);
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(r) = &resolved {
            info!("{engine} precision: {} ({})", r.precision, r.why);
        }

        let budget = plan::host_worker_budget(plan::physical_cpu_count(), self.config.jobs);
        let stages = plan::plan(engine, road, &placement.device, &spec.pools, budget);
        let detect_workers = stages
            .iter()
            .find(|s| s.key == STAGE_DETECT)
            .map_or(1, |s| s.workers) as usize;
        let reader = self.reader(detect_workers, placement.is_gpu() || road == Road::Line)?;

        // Provenance: PP-OCR's pin when its files are the pinned ones, then the
        // recognizer's source repos and the export set (spec Q9).
        let mut weights: Vec<(String, String)> = reader.repos();
        if let Some(rec) = &recognizer {
            for (repo, rev) in &rec.info().repos {
                weights.push((repo.to_string(), rev.to_string()));
            }
            weights.push((
                models::EXPORT_REPO.to_string(),
                bunko_ocr::models_release::RELEASE.to_string(),
            ));
        }
        let (stage_workers, queue_capacity) = plan::tables(&stages);
        let pipeline = plan::graph_line(&stages);
        let precision = resolved.as_ref().map(|r| r.precision.to_string());
        let ready = ReadyInfo {
            weights: weights.iter().cloned().collect(),
            stage_workers,
            queue_capacity,
            stage_device: session::stage_devices(&stages),
            pipeline: pipeline.clone(),
            precision: precision.clone(),
        };
        let info = SessionInfo {
            engine: engine.to_string(),
            patch_budget: spec.patch_budget,
            generator: self.config.generator.clone(),
            weights,
            precision,
            ready,
        };
        let engines = Arc::new(Engines { reader, recognizer });
        let source = detect_workers as u32 + SOURCE_EXTRA;
        let runner = EngineRunner::start(info, engines, road, &stages, source)?;
        info!(
            "{} ({engine}) ready in {:.1}s: {pipeline}",
            spec.name,
            started.elapsed().as_secs_f64()
        );
        Ok(runner)
    }
}

impl PagePipeline for EnginePipeline {
    fn describe(&self) -> MachineInfo {
        let devices = runtime::devices(self.config.backend);
        let engines = models::available_engines(&self.store);
        let detectors = if engines.is_empty() {
            Vec::new()
        } else {
            vec![models::PPOCR.to_string()]
        };
        MachineInfo {
            host: runtime::host_info(&devices),
            catalog: Catalog {
                engines,
                detectors,
                devices,
            },
        }
    }

    fn open(&self, spec: &RowSpec) -> Result<Box<dyn VolumeRunner>, String> {
        self.open_session(spec)
            .map(|r| Box::new(r) as Box<dyn VolumeRunner>)
    }
}
