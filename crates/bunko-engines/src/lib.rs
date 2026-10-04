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
//! recognizer pair (one ORT session each, run concurrently by the detect workers) and
//! one recognizer per (engine, package, device, precision) across sessions.
//!
//! Recognizer backends: **libtorch** (feature `torch`, the release backend): a backend
//! pack under `<storage>/backends/` loaded at run time ([`torch`]), CUDA / ROCm / CPU,
//! fp32 / bf16 / fp16 as 0.5.2. The ONNX Runtime recognizers (feature `onnx-vlm`) are
//! kept for reference and used only when no pack is loaded.
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
#[cfg(feature = "torch")]
pub mod torch;

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Instant;

use bunko_ocr::models::{Manifest, ModelStore, StoreOptions};
use bunko_ocr::ppocr::{PpOcr, PpocrPageReader};
use bunko_ocr::runtime::{ExecutionTarget, RuntimeOptions};
use bunko_processor::{MachineInfo, PagePipeline, ReadyInfo, VolumeRunner};
use bunko_proto::Device;
use bunko_proto::{Catalog, RowSpec};
#[cfg(feature = "onnx-vlm")]
use bunko_vlm::{EngineKey, OrtSessionFactory, RecognizerCache, SessionOptions};
use bunko_vlm::{Precision, Recognizer};
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

    /// `<storage>/backends`: where backend packs are installed (the sibling of
    /// `models_dir`; `MOKURO_BACKENDS_DIR` overrides it).
    pub fn backends_dir(&self) -> PathBuf {
        if let Some(d) = std::env::var_os(BACKENDS_DIR_ENV).filter(|v| !v.is_empty()) {
            return PathBuf::from(d);
        }
        self.models_dir
            .parent()
            .map(|p| p.join("backends"))
            .unwrap_or_else(|| PathBuf::from("backends"))
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

/// What a default row of an engine runs on here ([`EnginePipeline::default_need`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineNeed {
    pub engine: &'static str,
    /// `cpu` / `gpu:<n>` and its label.
    pub device: String,
    pub label: String,
    pub precision: Precision,
    /// Package targets for that device and precision, best first.
    pub targets: Vec<String>,
    /// Why the engine runs on the CPU although there is a GPU.
    pub fallback: Option<String>,
}

/// Whether an engine's rows can run here without a download
/// ([`EnginePipeline::package_status_for`]).
#[derive(Debug, Clone)]
pub struct PackageStatus {
    pub need: EngineNeed,
    /// The package directory, when the package is on disk with the weights it binds.
    pub package: Option<PathBuf>,
    /// Manifest ids of the recognizer's host files (tokenizer, tables, paddle-manga's
    /// embedding table) that are not on disk.
    pub missing_host_files: Vec<String>,
}

impl PackageStatus {
    /// Everything the recognizer loads is on disk.
    pub fn ready(&self) -> bool {
        self.package.is_some() && self.missing_host_files.is_empty()
    }
}

/// What [`EnginePipeline::prefetch`] put in place for an engine.
#[derive(Debug, Clone)]
pub struct Prefetched {
    pub need: EngineNeed,
    pub package: PathBuf,
    pub target: String,
    pub host_files: Vec<PathBuf>,
}

/// Overrides `<storage>/backends` (where backend packs are looked for).
pub const BACKENDS_DIR_ENV: &str = "MOKURO_BACKENDS_DIR";

/// The OCR engines of this machine (one per processor; shared by its sessions).
pub struct EnginePipeline {
    config: EngineConfig,
    store: ModelStore,
    #[cfg(feature = "onnx-vlm")]
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
            #[cfg(feature = "onnx-vlm")]
            recognizers: RecognizerCache::new(Arc::new(OrtSessionFactory)),
            ppocr: Mutex::new(None),
            loading: Mutex::new(()),
        }
    }

    /// The libtorch backend of this process (opened on first use; see [`torch`]).
    #[cfg(feature = "torch")]
    pub fn torch(&self) -> Result<Arc<torch::TorchBackend>, String> {
        torch::backend(
            &self.config.backends_dir(),
            self.config.backend == Backend::Cpu,
        )
    }

    /// The devices this machine offers under the configured backend: the libtorch
    /// pack's when one is loaded, else ONNX Runtime's.
    pub fn devices(&self) -> Vec<Device> {
        #[cfg(feature = "torch")]
        if let Ok(tb) = self.torch() {
            return tb
                .report
                .devices
                .iter()
                .filter(|d| self.config.backend.allows(&d.kind))
                .map(|d| Device {
                    id: d.id.clone(),
                    label: torch::label(d),
                    formats: d.formats.clone(),
                    provider: Some(d.kind.clone()),
                    arch: Some(d.arch.clone()).filter(|a| !a.is_empty()),
                })
                .collect();
        }
        runtime::devices(self.config.backend)
    }

    /// The devices for the catalog: with the libtorch backend each device lists only the
    /// formats a recognizer can run on it here (its compute formats with a package on
    /// disk or downloadable for hayai-nova or paddle-manga), not every format it computes
    /// in (a Turing card computes bf16 but no bf16 package exists for it).
    pub fn catalog_devices(&self) -> Vec<Device> {
        #[cfg_attr(not(feature = "torch"), allow(unused_mut))]
        let mut devices = self.devices();
        #[cfg(feature = "torch")]
        if let Ok(tb) = self.torch() {
            for d in &mut devices {
                d.formats = torch::runnable_formats(&tb.report, &d.id, |engine, p, targets| {
                    self.store.torch_package_obtainable(engine, p, targets)
                });
            }
        }
        devices
    }

    /// The compiled-package targets of `device` at `precision` (libtorch).
    #[cfg(feature = "torch")]
    fn torch_targets(tb: &torch::TorchBackend, device: &str, precision: Precision) -> Vec<String> {
        tb.device(device).map_or_else(Vec::new, |d| {
            bunko_ocr::models::torch_targets(&d.kind, &d.arch, &d.isa, precision.as_str())
        })
    }

    /// The formats `engine` can run in on `placement`: what the device computes in
    /// and what this machine has (or may download) the files for.
    fn formats(&self, engine: &str, placement: &runtime::Placement) -> Vec<&'static str> {
        #[cfg(feature = "torch")]
        if let Ok(tb) = self.torch() {
            return torch::device_formats(&tb.report, &placement.device, |p, targets| {
                self.store.torch_package_obtainable(engine, p, targets)
            });
        }
        let _ = engine;
        precision::supported(placement.is_gpu()).to_vec()
    }

    /// Whether the libtorch backend serves the recognizers in this process.
    fn torch_loaded(&self) -> bool {
        #[cfg(feature = "torch")]
        {
            self.torch().is_ok()
        }
        #[cfg(not(feature = "torch"))]
        {
            false
        }
    }

    /// Whether the placement's device computes bf16 natively ([`precision::bf16_native`]).
    fn bf16_native(&self, placement: &runtime::Placement) -> bool {
        #[cfg(feature = "torch")]
        if let Ok(tb) = self.torch()
            && let Some(d) = tb.device(&placement.device)
        {
            return precision::bf16_native(&d.kind, &d.arch);
        }
        let _ = placement;
        false
    }

    /// Whether a recognizer backend can run `engine` here (for `describe`).
    fn recognizer_ready(&self, engine: &str, devices: &[Device]) -> bool {
        #[cfg(feature = "torch")]
        if self.torch().is_ok() {
            if !models::obtainable(
                &self.store,
                &models::torch_host_ids(engine, Precision::Fp32),
            ) {
                return false;
            }
            return devices.iter().any(|d| {
                let (placement, _) = runtime::place(Some(&d.id), devices);
                !self.formats(engine, &placement).is_empty()
            });
        }
        let _ = devices;
        cfg!(feature = "onnx-vlm")
            && models::obtainable(&self.store, &models::engine_ids(engine, Precision::Fp32))
    }

    /// What a default row of `engine` (auto-accuracy, automatic device) runs on here:
    /// the device (the GPU, or the CPU when the GPU has no package), the precision and
    /// the package targets. `Err` when no device here can run it at all.
    #[cfg(feature = "torch")]
    pub fn default_need(&self, engine: &str) -> Result<EngineNeed, String> {
        self.need_for(engine, precision::MODE_ACCURACY)
    }

    /// What a row of `engine` with precision `mode` (`auto-*`, `fp32`, `bf16`, `fp16`)
    /// runs on here, as [`default_need`](Self::default_need).
    #[cfg(feature = "torch")]
    pub fn need_for(&self, engine: &str, mode: &str) -> Result<EngineNeed, String> {
        let tb = self.torch()?;
        let engine: &'static str = match engine {
            models::HAYAI => models::HAYAI,
            models::PADDLE => models::PADDLE,
            other => return Err(format!("{other} has no recognizer")),
        };
        let devices = self.devices();
        let (placement, fallback) =
            runtime::place_runnable(None, &devices, |pl| self.formats(engine, pl));
        let label = devices
            .iter()
            .find(|d| d.id == placement.device)
            .map_or_else(|| placement.device.clone(), |d| d.label.clone());
        let formats = precision::auto_formats(
            mode,
            self.formats(engine, &placement),
            self.bf16_native(&placement),
        );
        if formats.is_empty() {
            let wanted = Self::torch_targets(&tb, &placement.device, Precision::Fp32);
            return Err(format!(
                "no compiled {engine} package for {} ({label}) here or in the {} release (it needs one of: {}){}",
                placement.device,
                bunko_ocr::models::torch_release_name(),
                if wanted.is_empty() {
                    "none for this CPU".into()
                } else {
                    wanted.join(", ")
                },
                if self.store.can_download() {
                    ""
                } else {
                    "; downloads are off (MOKURO_MODELS_DOWNLOAD)"
                }
            ));
        }
        let precision = precision::resolve(engine, mode, &formats, None, "")?
            .map(|r| r.precision)
            .unwrap_or(Precision::Fp32);
        Ok(EngineNeed {
            engine,
            targets: Self::torch_targets(&tb, &placement.device, precision),
            device: placement.device,
            label,
            precision,
            fallback,
        })
    }

    /// Fetch what the default rows of `engine` (or every recognizer engine) need on
    /// this machine ([`default_need`](Self::default_need)): host files, the compiled
    /// package (unpacked) and its shared weights. Other precisions and devices are
    /// fetched when a session first needs them. One result per engine.
    #[cfg(feature = "torch")]
    pub fn prefetch(
        &self,
        engine: Option<&str>,
    ) -> Vec<(&'static str, Result<Prefetched, String>)> {
        let rows: Vec<(&str, &str)> = [models::HAYAI, models::PADDLE]
            .into_iter()
            .filter(|e| engine.is_none_or(|want| want == *e))
            .map(|e| (e, precision::MODE_ACCURACY))
            .collect();
        self.prefetch_rows(&rows)
    }

    /// [`prefetch`](Self::prefetch) for the given rows: `(engine, precision mode)`
    /// (the enabled generations). Rows of engines without a recognizer are skipped.
    #[cfg(feature = "torch")]
    pub fn prefetch_rows(
        &self,
        rows: &[(&str, &str)],
    ) -> Vec<(&'static str, Result<Prefetched, String>)> {
        let mut out: Vec<(&'static str, Result<Prefetched, String>)> = Vec::new();
        for &(engine, mode) in rows {
            let e: &'static str = match engine {
                models::HAYAI => models::HAYAI,
                models::PADDLE => models::PADDLE,
                _ => continue,
            };
            let r = self.need_for(e, mode).and_then(|need| {
                let pkg = self
                    .store
                    .ensure_torch_package(e, need.precision.as_str(), &need.targets)
                    .map_err(|err| err.to_string())?;
                let files =
                    models::torch_host_files(&self.store, e, need.precision, pkg.dir.clone())?;
                Ok(Prefetched {
                    need,
                    package: pkg.dir,
                    target: pkg.target,
                    host_files: [Some(files.tokenizer), files.pos_table, files.embeddings]
                        .into_iter()
                        .flatten()
                        .collect(),
                })
            });
            out.push((e, r));
        }
        out
    }

    /// Whether the default rows of `engine` can run here without a download (`doctor`);
    /// `Err` when nothing here can run it.
    #[cfg(feature = "torch")]
    pub fn package_status(&self, engine: &str) -> Result<PackageStatus, String> {
        self.package_status_for(engine, precision::MODE_ACCURACY)
    }

    /// [`package_status`](Self::package_status) for a row with precision `mode`: the
    /// package (graphs and the shared weights they bind) and the host files
    /// [`models::torch_host_files`] would load with it.
    #[cfg(feature = "torch")]
    pub fn package_status_for(&self, engine: &str, mode: &str) -> Result<PackageStatus, String> {
        let need = self.need_for(engine, mode)?;
        let package = self
            .store
            .torch_package_present(need.engine, need.precision.as_str(), &need.targets)
            .map(|p| p.dir);
        let mut host: Vec<&str> = models::torch_host_ids(need.engine, need.precision);
        if need.engine == models::PADDLE {
            // As `torch_host_files`: the embedding table is loaded unless the package's
            // shared decoder weights carry it (CPU packages never do).
            let blob = package.as_ref().is_some_and(|d| {
                d.parent()
                    .is_some_and(|p| p.join("weights-decoder.safetensors").is_file())
            });
            if !blob && (package.is_some() || need.device == "cpu") {
                host.push(models::paddle_embed_id(need.precision));
            }
        }
        let missing_host_files = host
            .into_iter()
            .filter(|id| self.store.locate(id).is_none())
            .map(str::to_string)
            .collect();
        Ok(PackageStatus {
            need,
            package,
            missing_host_files,
        })
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
        // One session pair serves every detect worker (concurrent `Run`). Its pool
        // gets two more intra-op threads per extra worker: with three workers, eight
        // threads match the speed of three 4-thread sessions at ~10% more CPU time
        // (twelve: +33% CPU for nothing; four: 10% slower).
        let opts = RuntimeOptions {
            intra_threads: plan::SESSION_THREADS + 2 * (copies.max(1) - 1),
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

    /// Load `engine`'s recognizer on `placement` at `precision`: libtorch when a pack
    /// is loaded, else the ONNX recognizers when built in.
    fn recognizer(
        &self,
        spec: &RowSpec,
        placement: &Placement,
        precision: Precision,
    ) -> Result<Arc<dyn Recognizer>, String> {
        #[cfg(feature = "torch")]
        if let Ok(tb) = self.torch() {
            return self.torch_recognizer(&tb, spec, placement, precision);
        }
        #[cfg(feature = "onnx-vlm")]
        {
            self.onnx_recognizer(spec, placement, precision)
        }
        #[cfg(not(feature = "onnx-vlm"))]
        {
            let _ = (placement, precision);
            let why = {
                #[cfg(feature = "torch")]
                {
                    self.torch().err().unwrap_or_default()
                }
                #[cfg(not(feature = "torch"))]
                {
                    "this build has no recognizer backend".to_string()
                }
            };
            Err(format!("{} needs the libtorch backend: {why}", spec.engine))
        }
    }

    #[cfg(feature = "torch")]
    fn torch_recognizer(
        &self,
        tb: &torch::TorchBackend,
        spec: &RowSpec,
        placement: &Placement,
        precision: Precision,
    ) -> Result<Arc<dyn Recognizer>, String> {
        let engine: &'static str = match spec.engine.as_str() {
            models::HAYAI => models::HAYAI,
            models::PADDLE => models::PADDLE,
            other => return Err(format!("{other} has no recognizer")),
        };
        let targets = Self::torch_targets(tb, &placement.device, precision);
        let pkg = self
            .store
            .ensure_torch_package(engine, precision.as_str(), &targets)
            .map_err(|e| e.to_string())?;
        let mut files = models::torch_host_files(&self.store, engine, precision, pkg.dir)?;
        files.unpack_cache = Some(
            self.store
                .options()
                .root
                .join(bunko_ocr::models::TORCH_DIR)
                .join(".unpacked"),
        );
        // A GPU recognizer needs one host thread (the device does the work); a CPU one
        // gets this job's share of the cores ([`plan::cpu_engine_threads`]; AOTInductor's CPU
        // kernels and oneDNN run on OpenMP).
        let threads = std::env::var(torch::THREADS_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|n| *n > 0)
            .unwrap_or_else(|| {
                if placement.is_gpu() {
                    1
                } else {
                    plan::cpu_engine_threads(
                        plan::physical_cpu_count(),
                        plan::performance_core_count(),
                        self.config.jobs,
                    ) as u32
                }
            });
        let r = tb
            .recognizer(
                engine,
                precision,
                &placement.device,
                &files,
                spec.patch_budget,
                threads,
            )
            .map_err(|e| format!("could not load {engine}: {e}"))?;
        Ok(r)
    }

    #[cfg(feature = "onnx-vlm")]
    fn onnx_recognizer(
        &self,
        spec: &RowSpec,
        placement: &Placement,
        precision: Precision,
    ) -> Result<Arc<dyn Recognizer>, String> {
        let cpus = plan::physical_cpu_count();
        let provider = placement
            .ort_provider()
            .ok_or_else(|| format!("{} has no ONNX Runtime provider", placement.device))?;
        let opts = SessionOptions {
            device: placement.vlm_device,
            provider,
            // A GPU session needs few host threads; a CPU one gets this job's cores.
            intra_threads: if placement.is_gpu() {
                4
            } else {
                plan::cpu_engine_threads(cpus, plan::performance_core_count(), self.config.jobs)
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

    /// A recognizer of `engine` at `precision` on `device` (`cpu`, `gpu:<n>`) outside
    /// any session: benchmarks and parity checks read crops through it.
    pub fn recognizer_for(
        &self,
        engine: &str,
        precision: Precision,
        device: &str,
        patch_budget: u32,
    ) -> Result<Arc<dyn Recognizer>, String> {
        let devices = self.devices();
        let (placement, why) = runtime::place(Some(device), &devices);
        if let Some(why) = why {
            return Err(why);
        }
        let spec = RowSpec {
            id: "adhoc".into(),
            name: engine.into(),
            engine: engine.into(),
            detector: None,
            patch_budget,
            precision: precision.as_str().into(),
            pools: Default::default(),
            precision_pick: None,
            precision_why: String::new(),
            primary: true,
        };
        self.recognizer(&spec, &placement, precision)
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
        let devices = self.devices();

        // Where the recognizer runs, and in which format.
        let mut placement = Placement::cpu();
        let mut resolved = None;
        if road == Road::Reconciled {
            let (p, why) = runtime::place_runnable(
                spec.pools
                    .stage_device
                    .get(plan::STAGE_ENGINE)
                    .map(String::as_str),
                &devices,
                |pl| self.formats(engine, pl),
            );
            if let Some(why) = why {
                warn!("{engine}: running on the CPU: {why}");
            }
            placement = p;
            let formats = precision::auto_formats(
                &spec.precision,
                self.formats(engine, &placement),
                self.bf16_native(&placement),
            );
            resolved = precision::resolve(
                engine,
                &spec.precision,
                &formats,
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
                    let formats = precision::auto_formats(
                        &spec.precision,
                        self.formats(engine, &placement),
                        false,
                    );
                    let again = precision::resolve(
                        engine,
                        &spec.precision,
                        &formats,
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

        let host = plan::Host {
            budget: plan::host_worker_budget(plan::logical_cpu_count(), self.config.jobs),
            physical_cores: plan::physical_cpu_count(),
            fast_gpu: placement.is_gpu() && self.bf16_native(&placement),
        };
        let stages = plan::plan(engine, road, &placement.device, &spec.pools, &host);
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
            // The export set the recognizer's files come from: the torch package release
            // for the libtorch recognizers, models-v1 for the ONNX ones.
            let release = if self.torch_loaded() {
                bunko_ocr::models::torch_release_name()
            } else {
                bunko_ocr::models_release::RELEASE.to_string()
            };
            weights.push((models::EXPORT_REPO.to_string(), release));
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
        let ceilings = plan::width_ceilings(&stages, &host);
        let runner = EngineRunner::start(info, engines, road, &stages, source, ceilings)?;
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
        let devices = self.catalog_devices();
        let engines =
            models::available_engines(&self.store, |e| self.recognizer_ready(e, &devices));
        let detectors = if engines.is_empty() {
            Vec::new()
        } else {
            vec![models::PPOCR.to_string()]
        };
        #[cfg(feature = "torch")]
        let (torch, cpu) = match self.torch() {
            Ok(t) => (
                Some(t.report.torch.clone()),
                t.device("cpu").map(|d| d.name.clone()),
            ),
            Err(_) => (None, None),
        };
        #[cfg(not(feature = "torch"))]
        let (torch, cpu): (Option<String>, Option<String>) = (None, None);
        MachineInfo {
            host: runtime::host_info(&devices, torch.as_deref(), cpu.as_deref()),
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
