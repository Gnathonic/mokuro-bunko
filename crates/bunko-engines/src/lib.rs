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
#[cfg(feature = "nc-models")]
pub mod textbox;
#[cfg(feature = "torch")]
pub mod torch;
#[cfg(feature = "torch")]
pub mod vulkan;

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
    /// More directories backend packs may be installed in, searched after
    /// [`EngineConfig::backends_dir`] (a processor also uses a pack `install-ocr` put in
    /// the library's default storage).
    pub fallback_backends: Vec<PathBuf>,
    /// `ocr.vulkan_backends`: the Vulkan pack's runtimes in order of preference
    /// (default `ncnn`, `webgpu`; docs/rust-port/VULKAN.md).
    pub vulkan_backends: Vec<String>,
}

impl EngineConfig {
    pub fn new(models_dir: PathBuf, backend: Backend) -> EngineConfig {
        EngineConfig {
            models_dir,
            backend,
            jobs: 1,
            generator: format!("mokuro-bunko {}", env!("CARGO_PKG_VERSION")),
            fallback_backends: Vec::new(),
            vulkan_backends: default_vulkan_backends(),
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

    /// Every directory packs are looked for in, in order: `MOKURO_BACKENDS_DIR`, the
    /// sibling `backends/` of `models_dir`, then [`EngineConfig::fallback_backends`]
    /// (each once).
    pub fn backends_dirs(&self) -> Vec<PathBuf> {
        let sibling = self
            .models_dir
            .parent()
            .map(|p| p.join("backends"))
            .unwrap_or_else(|| PathBuf::from("backends"));
        let mut dirs = vec![self.backends_dir(), sibling];
        dirs.extend(self.fallback_backends.iter().cloned());
        let mut out: Vec<PathBuf> = Vec::new();
        for d in dirs {
            if !out.contains(&d) {
                out.push(d);
            }
        }
        out
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

/// `ocr.vulkan_backends` when unset: ncnn, then WebGPU.
pub fn default_vulkan_backends() -> Vec<String> {
    ["ncnn", "webgpu"].into_iter().map(String::from).collect()
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

/// The files a row of an engine needs on this machine ([`EnginePipeline::package_plan`]):
/// what `models list`, `models download`, `doctor` and the admin panel all count.
#[derive(Debug, Clone)]
pub struct PackagePlan {
    pub need: EngineNeed,
    /// The package target counted: the one on disk, else the first of `need.targets`
    /// the release has (what a download fetches). None when neither exists.
    pub target: Option<String>,
    /// Manifest ids of that package: its graphs and the shared weights they bind.
    /// Empty for a package on disk that the release does not list (the override
    /// directory, a hand-made one): see `package`.
    pub package_ids: Vec<String>,
    /// The package directory, when the package is on disk with the weights it binds.
    pub package: Option<PathBuf>,
    /// Manifest ids of the recognizer's host files read with that package (tokenizer,
    /// tables, paddle-manga's embedding table for a package without it).
    pub host_ids: Vec<&'static str>,
}

/// What [`EnginePipeline::prefetch_rows`] put in place for an engine.
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
    /// The PP-OCR pair of a line detector (its id first), shared by every session on
    /// that detector that needs no more detect workers than it has sessions.
    ppocr: Mutex<Option<(String, usize, Weak<PpocrPageReader>)>>,
    /// The loaded block detector (its id first), shared by the sessions on it.
    #[cfg(feature = "gpl-models")]
    block_detector: Mutex<Option<(String, Weak<bunko_ocr::animetext::AnimeText>)>>,
    /// The loaded text-box recognizers by engine and session threads, shared by their
    /// sessions.
    #[cfg(feature = "nc-models")]
    textbox: Mutex<Vec<(&'static str, usize, Weak<textbox::TextboxRecognizer>)>>,
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
            #[cfg(feature = "gpl-models")]
            block_detector: Mutex::new(None),
            #[cfg(feature = "nc-models")]
            textbox: Mutex::new(Vec::new()),
            loading: Mutex::new(()),
        }
    }

    /// The libtorch backend of this process (opened on first use; see [`torch`]).
    #[cfg(feature = "torch")]
    pub fn torch(&self) -> Result<Arc<torch::TorchBackend>, String> {
        torch::backend_in(
            &self.config.backends_dirs(),
            self.config.backend == Backend::Cpu,
        )
    }

    /// The devices this machine offers under the configured backend: the libtorch
    /// pack's when one is loaded, else ONNX Runtime's.
    pub fn devices(&self) -> Vec<Device> {
        #[cfg_attr(not(feature = "torch"), allow(unused_mut))]
        let mut devices: Vec<Device> = Vec::new();
        #[cfg(feature = "torch")]
        if let Ok(tb) = self.torch() {
            devices = tb
                .report
                .devices
                .iter()
                .filter(|d| self.config.backend.allows(&d.kind))
                .map(|d| Device {
                    format_engines: Default::default(),
                    id: d.id.clone(),
                    label: torch::label(d),
                    formats: d.formats.clone(),
                    provider: Some(d.kind.clone()),
                    arch: Some(d.arch.clone()).filter(|a| !a.is_empty()),
                })
                .collect();
        }
        if devices.is_empty() {
            devices = runtime::devices(self.config.backend);
        }
        #[cfg(feature = "torch")]
        for (id, p) in self.vulkan_pairings() {
            if p.torch_id.is_none() {
                devices.push(Device {
                    format_engines: Default::default(),
                    id,
                    label: format!("{} (Vulkan)", torch::label(&p.entry)),
                    formats: vec!["fp32".into()],
                    provider: Some("vulkan".into()),
                    arch: Some(p.entry.arch.clone()),
                });
            }
        }
        devices
    }

    /// The Vulkan pack of this process, when the configured backend may use it.
    #[cfg(feature = "torch")]
    pub fn vulkan(&self) -> Result<Arc<vulkan::VulkanBackend>, String> {
        if !matches!(self.config.backend, Backend::Auto | Backend::Vulkan) {
            return Err(format!(
                "ocr.backend {:?} does not use the Vulkan pack",
                self.config.backend
            ));
        }
        vulkan::backend_in(&self.config.backends_dirs())
    }

    /// The Vulkan pack's GPUs with their catalog ids: a GPU libtorch also drives keeps
    /// libtorch's id (its runtimes are more candidates of that device); the others are
    /// `gpu:<n>` numbered after the libtorch GPUs, in the pack's order.
    #[cfg(feature = "torch")]
    pub fn vulkan_pairings(&self) -> Vec<(String, vulkan::Pairing)> {
        let Ok(vb) = self.vulkan() else {
            return Vec::new();
        };
        let torch: Vec<bunko_torch::abi::DeviceEntry> = self
            .torch()
            .map(|tb| {
                tb.report
                    .devices
                    .iter()
                    .filter(|d| self.config.backend.allows(&d.kind))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let mut next = torch.iter().filter(|d| d.kind != "cpu").count();
        vb.pairings(self.config.backend, &torch)
            .into_iter()
            .map(|p| {
                let id = match &p.torch_id {
                    Some(t) => t.clone(),
                    None => {
                        next += 1;
                        format!("gpu:{}", next - 1)
                    }
                };
                (id, p)
            })
            .collect()
    }

    /// The Vulkan pairing of a placement's device.
    #[cfg(feature = "torch")]
    fn vulkan_pairing(&self, placement: &Placement) -> Option<vulkan::Pairing> {
        self.vulkan_pairings()
            .into_iter()
            .find(|(id, _)| *id == placement.device)
            .map(|(_, p)| p)
    }

    /// The Vulkan runtimes that may run `engine` on a pairing, in `ocr.vulkan_backends`
    /// order: served by the pack there, not excluded ([`vulkan::EXCEPTIONS`]), with no
    /// failed self-test, their files here or downloadable. A forced runtime is alone.
    /// Each with its verdict key and graph directory.
    #[cfg(feature = "torch")]
    fn vulkan_runtimes(&self, engine: &str, p: &vulkan::Pairing) -> Vec<vulkan::Candidate> {
        self.vulkan_ranked(engine, p).0
    }

    /// The Vulkan runtimes of `engine` on a pairing kept as fallbacks only
    /// ([`vulkan::Action::FallbackOnly`]): not benchmarked, not fetched ahead.
    #[cfg(feature = "torch")]
    fn vulkan_fallbacks(&self, engine: &str, p: &vulkan::Pairing) -> Vec<vulkan::Candidate> {
        self.vulkan_ranked(engine, p).1
    }

    /// `(candidates, fallbacks)` of `engine` on a pairing, each in `ocr.vulkan_backends`
    /// order, by [`vulkan::standing`]; excluded runtimes and ones with a failed verdict or
    /// no files obtainable are in neither.
    #[cfg(feature = "torch")]
    fn vulkan_ranked(
        &self,
        engine: &str,
        p: &vulkan::Pairing,
    ) -> (Vec<vulkan::Candidate>, Vec<vulkan::Candidate>) {
        let Ok(vb) = self.vulkan() else {
            return (Vec::new(), Vec::new());
        };
        let all = vb.candidates(
            &self.config.vulkan_backends,
            engine,
            &p.entry,
            self.store.locate_vulkan_golden(engine).as_deref(),
            |rt| self.store.locate_vulkan(engine, rt),
        );
        let usable = |c: &vulkan::Candidate| {
            self.store.vulkan_obtainable(engine, &c.runtime)
                && vb.store.verdict(&c.key).is_none_or(|v| v.pass)
        };
        let (mut ranked, mut fallback) = (Vec::new(), Vec::new());
        for c in all {
            match self.vulkan_standing(&c.runtime, engine, p) {
                vulkan::Standing::Forced(_) if usable(&c) => return (vec![c], Vec::new()),
                vulkan::Standing::Candidate if usable(&c) => ranked.push(c),
                vulkan::Standing::FallbackOnly(_) if usable(&c) => fallback.push(c),
                _ => {}
            }
        }
        (ranked, fallback)
    }

    /// A runtime's standing on a pairing ([`vulkan::EXCEPTIONS`]).
    #[cfg(feature = "torch")]
    fn vulkan_standing(
        &self,
        runtime: &str,
        engine: &str,
        p: &vulkan::Pairing,
    ) -> vulkan::Standing {
        vulkan::standing(
            vulkan::EXCEPTIONS,
            runtime,
            engine,
            std::env::consts::OS,
            vulkan::vendor_of(&p.entry),
            p.gfx.as_deref(),
            p.torch_id.is_some(),
        )
    }

    /// The formats the Vulkan runtimes add on a placement: on a GPU of their own, `fp32`
    /// (the first runtime) and, unless `ocr.backend: vulkan` pins the order, `fp32@<rt>`
    /// for each other one; beside libtorch, `fp32@<rt>` for each (benchmarked against
    /// libtorch's own formats).
    #[cfg(feature = "torch")]
    fn vulkan_formats(&self, engine: &str, placement: &Placement) -> Vec<&'static str> {
        let Some(p) = self.vulkan_pairing(placement) else {
            return Vec::new();
        };
        let rts = self.vulkan_runtimes(engine, &p);
        let pinned = self.config.backend == Backend::Vulkan;
        if p.torch_id.is_some() {
            return rts.iter().map(|c| vulkan::qualified(&c.runtime)).collect();
        }
        // Fallbacks alone still make the GPU usable (they are tried in order).
        if rts.is_empty() && !self.vulkan_fallbacks(engine, &p).is_empty() {
            return vec!["fp32"];
        }
        let mut out = Vec::new();
        for (i, c) in rts.iter().enumerate() {
            if i == 0 {
                out.push("fp32");
            } else if !pinned {
                out.push(vulkan::qualified(&c.runtime));
            }
        }
        out
    }

    /// What the Vulkan route does on this machine, for `doctor` and the admin panel:
    /// per GPU of the pack, each runtime × engine as a candidate (with its self-test
    /// state) or skipped (with why). Opens the pack (not the runtimes' models).
    #[cfg(feature = "torch")]
    pub fn vulkan_report(&self) -> vulkan::Report {
        let mode = match self.config.backend {
            Backend::Auto => "benchmark",
            Backend::Vulkan => "pinned",
            _ => "off",
        };
        let vb = match self.vulkan() {
            Ok(vb) => vb,
            Err(e) => {
                return vulkan::Report {
                    mode,
                    error: Some(e),
                    ..Default::default()
                };
            }
        };
        let mut devices = Vec::new();
        for (id, p) in self.vulkan_pairings() {
            let mut runtimes = Vec::new();
            for engine in [models::HAYAI, models::PADDLE] {
                let cands = vb.candidates(
                    &self.config.vulkan_backends,
                    engine,
                    &p.entry,
                    self.store.locate_vulkan_golden(engine).as_deref(),
                    |rt| self.store.locate_vulkan(engine, rt),
                );
                let usable: Vec<String> = self
                    .vulkan_runtimes(engine, &p)
                    .into_iter()
                    .map(|c| c.runtime)
                    .collect();
                for rt in &self.config.vulkan_backends {
                    let c = cands.iter().find(|c| &c.runtime == rt);
                    let v = c.and_then(|c| vb.store.verdict(&c.key));
                    let standing = self.vulkan_standing(rt, engine, &p);
                    let (status, reason) = if c.is_none() {
                        (
                            "skipped",
                            format!("the pack's {rt} does not run on this GPU"),
                        )
                    } else if let vulkan::Standing::Excluded(why) = standing {
                        ("skipped", why.to_string())
                    } else if !self.store.vulkan_obtainable(engine, rt) {
                        (
                            "skipped",
                            format!("no {engine} graphs for {rt} here or in the release"),
                        )
                    } else if let Some(v) = v.as_ref().filter(|v| !v.pass) {
                        ("skipped", v.reason.clone())
                    } else if let vulkan::Standing::FallbackOnly(why) = standing {
                        ("fallback-only", why.to_string())
                    } else if !usable.contains(rt) {
                        ("skipped", "another runtime is forced here".to_string())
                    } else if let Some(v) = v {
                        ("candidate", v.reason)
                    } else {
                        (
                            "candidate",
                            "not self-tested yet (tested when first used)".to_string(),
                        )
                    };
                    runtimes.push(vulkan::RuntimeStatus {
                        runtime: rt.clone(),
                        engine,
                        status,
                        reason,
                    });
                }
            }
            devices.push(vulkan::DeviceReport {
                id,
                name: p.entry.name.clone(),
                driver: p.entry.driver.clone().unwrap_or_default(),
                radv: p
                    .entry
                    .driver
                    .as_deref()
                    .is_some_and(|d| d.to_ascii_lowercase().starts_with("radv")),
                beside: p.torch_id.as_ref().map(|_| "libtorch".to_string()),
                gfx: p.gfx.clone(),
                runtimes,
            });
        }
        vulkan::Report {
            mode,
            pack: Some(format!("{} ({})", vb.pack.dir.display(), vb.report.torch)),
            order: self.config.vulkan_backends.clone(),
            warnings: vb.report.warnings.clone(),
            crashes: vb.crashes.clone(),
            devices,
            error: None,
        }
    }

    /// The devices for the catalog: with the libtorch backend each device lists only the
    /// formats a recognizer can run on it here (its compute formats with a package on
    /// disk or downloadable for one of the recognizer engines), not every format it
    /// computes in (a Turing card computes bf16 but no bf16 package exists for it). A
    /// GPU's format that only some engines run there (hayai-nova's bf16, a gfx with
    /// hayai-nova packages and no nar-small one, a Vulkan runtime) lists those engines
    /// in `format_engines`. A text-box engine counts on a GPU only by the whole test of
    /// its placement ([`textbox_gpu_formats`]): an old pack without `bt_read_chars`
    /// offers it no GPU format.
    pub fn catalog_devices(&self) -> Vec<Device> {
        #[cfg_attr(not(feature = "torch"), allow(unused_mut))]
        let mut devices = self.devices();
        #[cfg(feature = "torch")]
        let recognizers: Vec<&'static str> = models::ENGINES
            .iter()
            .copied()
            .filter(|e| *e != models::PPOCR)
            .collect();
        #[cfg(feature = "torch")]
        if let Ok(tb) = self.torch() {
            let reads_chars = tb.pack.reads_chars();
            let host_files = |e: &str| {
                models::obtainable(&self.store, &models::torch_host_ids(e, Precision::Fp32))
            };
            for d in devices
                .iter_mut()
                .filter(|d| d.provider.as_deref() != Some("vulkan"))
            {
                let cpu = d.id == "cpu";
                let (formats, only) =
                    torch::engine_formats(&tb.report, &d.id, &recognizers, |engine, p, targets| {
                        if !models::textbox_engine(engine) {
                            return self.store.torch_package_obtainable(engine, p, targets);
                        }
                        // The text-box engines read on the CPU with ONNX Runtime.
                        if cpu {
                            return p == "fp32"
                                && models::obtainable(&self.store, &models::textbox_ids(engine));
                        }
                        let packaged: Vec<&'static str> = ["fp32", "bf16", "fp16"]
                            .into_iter()
                            .filter(|f| {
                                *f == p && self.store.torch_package_obtainable(engine, p, targets)
                            })
                            .collect();
                        !textbox_gpu_formats(engine, reads_chars, &packaged, host_files(engine))
                            .is_empty()
                    });
                d.formats = formats;
                d.format_engines.extend(only);
            }
        }
        // The Vulkan runtimes' formats (hayai-nova and paddle-manga), after the device's
        // own.
        #[cfg(feature = "torch")]
        for d in &mut devices {
            if d.id == "cpu" {
                continue;
            }
            let pl = runtime::place(Some(&d.id), std::slice::from_ref(&*d)).0;
            let mut by: std::collections::BTreeMap<&'static str, Vec<String>> = Default::default();
            for engine in [models::HAYAI, models::PADDLE] {
                for f in self.vulkan_formats(engine, &pl) {
                    by.entry(f).or_default().push(engine.to_string());
                }
            }
            add_vulkan_formats(d, by, &recognizers);
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
        if models::textbox_engine(engine) {
            return self.textbox_formats(engine, placement);
        }
        if placement.provider == "vulkan" {
            #[cfg(feature = "torch")]
            return self.vulkan_formats(engine, placement);
            #[cfg(not(feature = "torch"))]
            return Vec::new();
        }
        #[cfg(feature = "torch")]
        if let Ok(tb) = self.torch() {
            let mut f = torch::device_formats(&tb.report, &placement.device, |p, targets| {
                self.store.torch_package_obtainable(engine, p, targets)
            });
            if placement.is_gpu() {
                f.extend(self.vulkan_formats(engine, placement));
            }
            return f;
        }
        let _ = engine;
        precision::supported(placement.is_gpu()).to_vec()
    }

    /// The formats text-box `engine` runs in on `placement`: on the CPU, fp32 when its
    /// ONNX files are here or downloadable; on a libtorch GPU, [`textbox_gpu_formats`]
    /// over the device's formats with a package here or in the release; on a Vulkan GPU
    /// (or with no libtorch pack) none.
    fn textbox_formats(&self, engine: &str, placement: &Placement) -> Vec<&'static str> {
        if !placement.is_gpu() {
            return if models::obtainable(&self.store, &models::textbox_ids(engine)) {
                vec!["fp32"]
            } else {
                Vec::new()
            };
        }
        if placement.provider == "vulkan" {
            return Vec::new();
        }
        #[cfg(feature = "torch")]
        if let Ok(tb) = self.torch() {
            let packaged = torch::device_formats(&tb.report, &placement.device, |p, targets| {
                self.store.torch_package_obtainable(engine, p, targets)
            });
            let host_files = models::obtainable(
                &self.store,
                &models::torch_host_ids(engine, Precision::Fp32),
            );
            return textbox_gpu_formats(engine, tb.pack.reads_chars(), &packaged, host_files);
        }
        Vec::new()
    }

    /// Whether the loaded libtorch pack reads text boxes (`bt_read_chars`); `None` with
    /// no libtorch pack.
    #[cfg(feature = "nc-models")]
    fn pack_reads_chars(&self) -> Option<bool> {
        #[cfg(feature = "torch")]
        if let Ok(tb) = self.torch() {
            return Some(tb.pack.reads_chars());
        }
        None
    }

    /// Log why a text-box row reads on the CPU although a GPU was asked for or is here
    /// (`why`, from [`runtime::place_runnable`]; [`textbox_cpu_reason`] picks the pack's
    /// reason where it is the cause). A row that pinned its engine stage to a GPU is
    /// warned about on every session; an unpinned row runs on the CPU as planned, noted
    /// once per process per reason ([`textbox_cpu_note`]).
    #[cfg(feature = "nc-models")]
    fn note_textbox_on_cpu(
        &self,
        engine: &str,
        asked: Option<&str>,
        devices: &[Device],
        why: &str,
    ) {
        let (reason, pack) = textbox_cpu_reason(asked, devices, self.pack_reads_chars(), why);
        match textbox_cpu_note(engine, asked, &reason, pack) {
            None => warn!("{engine}: running on the CPU: {reason}"),
            Some(key) => {
                if TEXTBOX_CPU_NOTED.lock().insert(key) {
                    info!("{engine}: reading on the CPU: {reason}");
                }
            }
        }
    }

    /// nar-small or charspot-small on a libtorch GPU at `precision` (fp32, or nar-small's
    /// fp16): the package for the device's targets, the authors' weights and vocabulary
    /// (the same fp32 file at either precision: an fp16 package casts it at load), one
    /// `bt_read_chars` per page. Also the export set its package comes from (the sidecar's
    /// provenance).
    #[cfg(all(feature = "torch", feature = "nc-models"))]
    fn torch_textbox(
        &self,
        engine: &str,
        placement: &Placement,
        precision: Precision,
    ) -> Result<(Arc<dyn Recognizer>, String), String> {
        let engine: &'static str = match engine {
            models::NAR => models::NAR,
            models::CHARSPOT => models::CHARSPOT,
            other => return Err(format!("{other} is no text-box recognizer")),
        };
        let (repo, _) = models::textbox_source(engine)
            .ok_or_else(|| format!("{engine} has no pinned source"))?;
        let tb = self.torch()?;
        let targets = Self::torch_targets(&tb, &placement.device, precision);
        let pkg = self
            .store
            .ensure_torch_package(engine, precision.as_str(), &targets)
            .map_err(|e| e.to_string())?;
        // The sidecar names the authors' pin only when their files are the pinned ones.
        let pinned = models::ensure_all(&self.store, &models::torch_host_ids(engine, precision))?
            .iter()
            .all(|f| f.verified);
        let mut files = models::torch_host_files(&self.store, engine, precision, pkg.dir)?;
        files.unpack_cache = Some(
            self.store
                .options()
                .root
                .join(bunko_ocr::models::TORCH_DIR)
                .join(".unpacked"),
        );
        // The pack's preprocessing pool (the crops are cut and resized on the host).
        let threads = plan::cpu_engine_threads(
            plan::physical_cpu_count(),
            plan::performance_core_count(),
            self.config.jobs,
        ) as u32;
        let max_batch =
            torch::textbox_max_batch(tb.device(&placement.device).and_then(|d| d.vram_mb));
        let reader = tb.textbox(
            engine,
            precision,
            &placement.device,
            &files,
            threads,
            max_batch,
        )?;
        let device: bunko_vlm::Device = placement
            .device
            .parse()
            .map_err(|e: bunko_vlm::VlmError| e.to_string())?;
        let info = bunko_vlm::RecognizerInfo {
            engine,
            recognizer: repo,
            repos: models::textbox_source(engine)
                .filter(|_| pinned)
                .into_iter()
                .collect(),
            precision,
            device,
            token_caps: false,
            second_read: false,
            default_max_tokens: 0,
            batch: reader.info.batch as usize,
            patch_budget: None,
        };
        let rec: Arc<dyn Recognizer> = Arc::new(textbox::TextboxRecognizer::with_reader(
            engine, reader, info,
        ));
        Ok((rec, bunko_ocr::models::torch_release_name_for(engine)))
    }

    /// The error of a row whose engine has no compiled package for `placement`'s device
    /// here or in the release. Only the torch rows' needs ask (`need_for`).
    #[cfg_attr(not(feature = "torch"), allow(dead_code))]
    fn no_package_error(&self, engine: &str, placement: &Placement, label: &str) -> String {
        #[cfg(feature = "torch")]
        let wanted = self
            .torch()
            .map(|tb| Self::torch_targets(&tb, &placement.device, Precision::Fp32))
            .unwrap_or_default();
        #[cfg(not(feature = "torch"))]
        let wanted: Vec<String> = Vec::new();
        format!(
            "no compiled {engine} package for {} ({label}) here or in the {} release (it needs one of: {}){}",
            placement.device,
            bunko_ocr::models::torch_release_name(),
            if wanted.is_empty() {
                "none for this device".into()
            } else {
                wanted.join(", ")
            },
            if self.store.can_download() {
                ""
            } else {
                "; downloads are off (MOKURO_MODELS_DOWNLOAD)"
            }
        )
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

    /// Why no recognizer of `engine` can load in this process: no libtorch backend pack
    /// loaded and no ONNX recognizers built in. None when one can, and always for a
    /// text-box engine (it reads on the CPU with ONNX Runtime in every build).
    fn no_recognizer_backend(&self, engine: &str) -> Option<String> {
        if models::textbox_engine(engine) || self.torch_loaded() || cfg!(feature = "onnx-vlm") {
            return None;
        }
        #[cfg(feature = "torch")]
        if !self.vulkan_pairings().is_empty() {
            return None;
        }
        #[cfg(feature = "torch")]
        let why = self.torch().err().unwrap_or_default();
        #[cfg(not(feature = "torch"))]
        let why = "this build has no recognizer backend".to_string();
        Some(format!("{engine} needs the libtorch backend: {why}"))
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

    /// The text-box recognizer of `engine` (ONNX Runtime on the CPU), loaded once and
    /// shared by the sessions that read with it. Its threads are sized for an engine
    /// stage of `workers` ([`plan::textbox_session_threads`]): a session of that many
    /// threads is shared, one of another count is loaded beside it, so a benchmark that
    /// tries 1, 2 and 3 workers measures each at the threads a session of that width
    /// will run with (from 3 workers on they all share one 1-thread session).
    #[cfg(feature = "nc-models")]
    fn textbox_recognizer(
        &self,
        engine: &str,
        workers: u32,
    ) -> Result<Arc<dyn Recognizer>, String> {
        let single = plan::cpu_engine_threads(
            plan::physical_cpu_count(),
            plan::performance_core_count(),
            self.config.jobs,
        );
        let threads = plan::textbox_session_threads(single, workers);
        let mut loaded = self.textbox.lock();
        loaded.retain(|(_, _, r)| r.strong_count() > 0);
        if let Some(r) = loaded
            .iter()
            .find(|(e, t, _)| *e == engine && *t == threads)
            .and_then(|(_, _, r)| r.upgrade())
        {
            return Ok(r);
        }
        let files = models::ensure_all(&self.store, &models::textbox_ids(engine))?;
        info!("{engine}: one shared session of {threads} thread(s) for {workers} worker(s)");
        let opts = RuntimeOptions {
            intra_threads: threads,
            targets: vec![ExecutionTarget::Cpu],
            copies: 1,
            spinning: false,
        };
        // The sidecar names the pinned source only when these are the pinned files.
        let source = files
            .iter()
            .all(|f| f.verified)
            .then(|| models::textbox_source(engine))
            .flatten();
        // The graph, the vocabulary, then the weights of a graph that holds none.
        let rec = textbox::TextboxRecognizer::load(
            engine,
            &files[0].path,
            files.get(2).map(|f| f.path.as_path()),
            &files[1].path,
            &opts,
            source,
        )
        .map_err(|e| format!("could not load {engine}: {e}"))?;
        let rec = Arc::new(rec);
        loaded.push((rec.info().engine, threads, Arc::downgrade(&rec)));
        Ok(rec)
    }

    /// Whether a recognizer backend can run `engine` here (for `describe`).
    fn recognizer_ready(&self, engine: &str, devices: &[Device]) -> bool {
        // The text-box recognizers: their ONNX files for the CPU, or a GPU that runs
        // them through the libtorch pack.
        if models::textbox_engine(engine) {
            return models::obtainable(&self.store, &models::textbox_ids(engine))
                || devices.iter().any(|d| {
                    let (placement, _) = runtime::place(Some(&d.id), devices);
                    placement.is_gpu() && !self.textbox_formats(engine, &placement).is_empty()
                });
        }
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
        // No libtorch pack: the Vulkan pack's GPUs alone (ocr.backend vulkan beside no
        // CPU pack) still run the recognizers.
        #[cfg(feature = "torch")]
        if !self.vulkan_pairings().is_empty() {
            return models::obtainable(&self.store, &models::vulkan_host_ids(engine, &[]))
                && devices.iter().any(|d| {
                    let (placement, _) = runtime::place(Some(&d.id), devices);
                    placement.provider == "vulkan" && !self.formats(engine, &placement).is_empty()
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
        if models::textbox_engine(engine) {
            return self.textbox_need(engine, mode);
        }
        // Without a libtorch pack the Vulkan pack's GPUs still have a plan.
        let tb = match self.torch() {
            Ok(tb) => Some(tb),
            Err(_) if !self.vulkan_pairings().is_empty() => None,
            Err(e) => return Err(e),
        };
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
            return Err(self.no_package_error(engine, &placement, &label));
        }
        let precision = precision::resolve(engine, mode, &formats, None, "")?
            .map(|r| r.precision)
            .unwrap_or(Precision::Fp32);
        Ok(EngineNeed {
            engine,
            targets: tb
                .as_ref()
                .map(|tb| Self::torch_targets(tb, &placement.device, precision))
                .unwrap_or_default(),
            device: placement.device,
            label,
            precision,
            fallback,
        })
    }

    /// [`need_for`](Self::need_for) of a text-box engine: the GPU its placement picks
    /// over [`textbox_formats`](Self::textbox_formats) at the precision `mode` resolves
    /// to there (nar-small: fp16 in every auto mode on a GPU with an fp16 package, O2)
    /// with that device's package targets, else the CPU (the ONNX route, fp32, no
    /// targets). With no libtorch pack the CPU is the need and `fallback` says why; a
    /// pack without `bt_read_chars` says [`torch::TEXTBOX_PACK_REASON`] when that is the
    /// cause ([`textbox_cpu_reason`]). A forced format the device cannot run is the
    /// precision refusal.
    #[cfg(feature = "torch")]
    fn textbox_need(&self, engine: &str, mode: &str) -> Result<EngineNeed, String> {
        let engine: &'static str = models::ENGINES
            .iter()
            .copied()
            .find(|e| *e == engine)
            .ok_or_else(|| format!("{engine} has no recognizer"))?;
        let devices = self.devices();
        let (placement, mut fallback) =
            runtime::place_runnable(None, &devices, |pl| self.textbox_formats(engine, pl));
        let tb = self.torch();
        match &tb {
            Err(e) => {
                fallback.get_or_insert_with(|| e.clone());
            }
            Ok(tb) => {
                fallback = fallback.map(|why| {
                    textbox_cpu_reason(None, &devices, Some(tb.pack.reads_chars()), &why).0
                });
            }
        }
        let label = devices
            .iter()
            .find(|d| d.id == placement.device)
            .map_or_else(|| placement.device.clone(), |d| d.label.clone());
        let formats = self.textbox_formats(engine, &placement);
        if formats.is_empty() {
            return Err(format!(
                "{engine}'s files are not here and downloads are off (MOKURO_MODELS_DOWNLOAD)"
            ));
        }
        let formats = precision::auto_formats(mode, formats, false);
        let precision = precision::resolve(engine, mode, &formats, None, "")?
            .map_or(Precision::Fp32, |r| r.precision);
        let targets = match &tb {
            Ok(tb) if placement.is_gpu() => Self::torch_targets(tb, &placement.device, precision),
            _ => Vec::new(),
        };
        Ok(EngineNeed {
            engine,
            device: placement.device,
            label,
            precision,
            targets,
            fallback,
        })
    }

    /// Fetch what the given rows need on this machine: `(engine, precision mode)`, the
    /// enabled generations (so an opt-in engine, such as the non-commercial text-box
    /// ones, is fetched only for a row that uses it). Per row: the host files, the
    /// compiled package (unpacked) and its shared weights, for the device and precision
    /// [`need_for`](Self::need_for) picks, and a text-box row's other formats its mode
    /// can resolve to there (a benchmarked mode's fp16 and fp32 packages); other
    /// precisions and devices are fetched when a session first needs them. Rows of
    /// engines without a recognizer are skipped.
    #[cfg(feature = "torch")]
    pub fn prefetch_rows(
        &self,
        rows: &[(&str, &str)],
    ) -> Vec<(&'static str, Result<Prefetched, String>)> {
        let mut out: Vec<(&'static str, Result<Prefetched, String>)> = Vec::new();
        for &(engine, mode) in rows {
            if models::textbox_engine(engine)
                && let Some(e) = models::ENGINES.iter().copied().find(|e| *e == engine)
            {
                let r = self
                    .need_for(e, mode)
                    .and_then(|need| self.prefetch_textbox(e, mode, need));
                out.push((e, r));
                continue;
            }
            let e: &'static str = match engine {
                models::HAYAI => models::HAYAI,
                models::PADDLE => models::PADDLE,
                _ => continue,
            };
            let r = self.need_for(e, mode).and_then(|need| {
                #[cfg(feature = "torch")]
                if self.is_vulkan_device(&need.device) {
                    return self.prefetch_vulkan(e, need);
                }
                let pkg = self
                    .store
                    .ensure_torch_package(e, need.precision.as_str(), &need.targets)
                    .map_err(|err| err.to_string())?;
                let files =
                    models::torch_host_files(&self.store, e, need.precision, pkg.dir.clone())?;
                // The Vulkan runtimes beside libtorch on this GPU are benchmark candidates
                // of every fp32 row (and of the balanced/speed modes): fetched now too,
                // smallest first, so a benchmark never waits on them.
                #[cfg(feature = "torch")]
                if need.precision == Precision::Fp32
                    || bunko_sched::precision::is_benched(&precision::normalize_mode(mode))
                {
                    self.fetch_vulkan_beside(e, &need.device);
                }
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

    /// [`prefetch_rows`](Self::prefetch_rows) of a text-box row in precision `mode`. On a
    /// GPU: the package for its targets at the precision it resolves to, the GPU route's
    /// host files and the ONNX files (the CPU route, the fallback when the GPU cannot
    /// start, is then free), and the packages of the other formats the mode can resolve
    /// to there ([`textbox_prefetch_formats`]: a benchmarked mode tries fp16 and fp32),
    /// whose failure is only logged; on the CPU: the ONNX files, the graph reported as the
    /// package.
    #[cfg(feature = "torch")]
    fn prefetch_textbox(
        &self,
        engine: &'static str,
        mode: &str,
        need: EngineNeed,
    ) -> Result<Prefetched, String> {
        if need.device != Placement::cpu().device {
            let pkg = self
                .store
                .ensure_torch_package(engine, need.precision.as_str(), &need.targets)
                .map_err(|err| err.to_string())?;
            let placement = runtime::place(Some(&need.device), &self.devices()).0;
            let formats = self.textbox_formats(engine, &placement);
            for f in textbox_prefetch_formats(engine, mode, &formats) {
                if f == need.precision.as_str() {
                    continue;
                }
                let targets = self.torch().ok().and_then(|tb| {
                    tb.device(&need.device)
                        .map(|d| bunko_ocr::models::torch_targets(&d.kind, &d.arch, &d.isa, &f))
                });
                if let Err(e) = self.store.ensure_torch_package(
                    engine,
                    &f,
                    targets.as_deref().unwrap_or_default(),
                ) {
                    warn!("{engine}: the {f} package for {}: {e}", need.device);
                }
            }
            let files =
                models::torch_host_files(&self.store, engine, need.precision, pkg.dir.clone())?;
            // The CPU route's files are fetched by the download plan too: a failure
            // here is that route's, not the GPU's.
            if let Err(e) = models::ensure_all(&self.store, &models::textbox_ids(engine)) {
                warn!("{engine}: the CPU route's files: {e}");
            }
            return Ok(Prefetched {
                need,
                package: pkg.dir,
                target: pkg.target,
                host_files: std::iter::once(files.tokenizer)
                    .chain(files.weights)
                    .collect(),
            });
        }
        let mut files = models::ensure_all(&self.store, &models::textbox_ids(engine))?.into_iter();
        let graph = files
            .next()
            .ok_or_else(|| format!("{engine} has no ONNX graph"))?;
        Ok(Prefetched {
            need,
            package: graph.path,
            target: TEXTBOX_CPU_TARGET.to_string(),
            host_files: files.map(|f| f.path).collect(),
        })
    }

    /// Fetch the graphs of the Vulkan runtimes paired with libtorch GPU `device` for
    /// `engine` (smallest first) and the goldens; failures are logged (the runtime then
    /// is no candidate until its files are here).
    #[cfg(feature = "torch")]
    fn fetch_vulkan_beside(&self, engine: &str, device: &str) {
        let pl = runtime::place(Some(device), &self.devices()).0;
        let Some(pairing) = self.vulkan_pairing(&pl) else {
            return;
        };
        let size = |rt: &str| -> u64 {
            self.store
                .vulkan_ids(engine, rt)
                .iter()
                .filter_map(|id| self.store.manifest().get(id))
                .map(|f| f.size)
                .sum()
        };
        let mut rts = self.vulkan_runtimes(engine, &pairing);
        rts.sort_by_key(|c| size(&c.runtime));
        for c in rts.iter().filter(|c| c.model_dir.is_none()) {
            info!(
                "{engine}: fetching the {} graphs ({:.0} MB), a benchmark candidate on {device}",
                c.runtime,
                size(&c.runtime) as f64 / 1e6
            );
            if let Err(e) = self.store.ensure_vulkan(engine, &c.runtime) {
                warn!("{engine} graphs for {}: {e}", c.runtime);
            }
        }
        if !rts.is_empty() {
            let names: Vec<&str> = rts.iter().map(|c| c.runtime.as_str()).collect();
            if let Err(e) = self
                .store
                .ensure_vulkan_golden(engine)
                .map_err(|e| e.to_string())
                .and_then(|_| models::vulkan_host_files(&self.store, engine, &names))
            {
                warn!("{engine}: Vulkan host files: {e}");
            }
        }
    }

    /// Whether catalog device `id` is a GPU only the Vulkan pack drives.
    #[cfg(feature = "torch")]
    fn is_vulkan_device(&self, id: &str) -> bool {
        self.devices()
            .iter()
            .any(|d| d.id == id && d.provider.as_deref() == Some("vulkan"))
    }

    /// [`prefetch_rows`](Self::prefetch_rows) on a Vulkan-only GPU: every candidate
    /// runtime's graphs (smallest download first), the self-test goldens and the host
    /// files. The package reported is the first runtime's.
    #[cfg(feature = "torch")]
    fn prefetch_vulkan(
        &self,
        engine: &'static str,
        need: EngineNeed,
    ) -> Result<Prefetched, String> {
        let pl = runtime::place(Some(&need.device), &self.devices()).0;
        let pairing = self
            .vulkan_pairing(&pl)
            .ok_or_else(|| format!("{} has no Vulkan runtime here", need.device))?;
        let mut rts = self.vulkan_runtimes(engine, &pairing);
        let size = |rt: &str| -> u64 {
            self.store
                .vulkan_ids(engine, rt)
                .iter()
                .filter_map(|id| self.store.manifest().get(id))
                .map(|f| f.size)
                .sum()
        };
        rts.sort_by_key(|c| size(&c.runtime));
        let mut first = None;
        let mut errors = Vec::new();
        for c in &rts {
            info!(
                "{engine}: fetching the {} graphs ({:.0} MB)",
                c.runtime,
                size(&c.runtime) as f64 / 1e6
            );
            match self.store.ensure_vulkan(engine, &c.runtime) {
                Ok(d) => {
                    first.get_or_insert((d, c.runtime.clone()));
                }
                Err(e) => errors.push(format!("{}: {e}", c.runtime)),
            }
        }
        let (package, rt) = first.ok_or_else(|| {
            format!(
                "no Vulkan runtime's {engine} graphs could be fetched: {}",
                errors.join("; ")
            )
        })?;
        if let Err(e) = self.store.ensure_vulkan_golden(engine) {
            warn!("{engine}: no Vulkan self-test goldens ({e})");
        }
        let names: Vec<&str> = rts.iter().map(|c| c.runtime.as_str()).collect();
        let (tokenizer, pos_table, embeddings) =
            models::vulkan_host_files(&self.store, engine, &names)?;
        Ok(Prefetched {
            need,
            package,
            target: format!("vulkan/{rt}"),
            host_files: [Some(tokenizer), pos_table, embeddings]
                .into_iter()
                .flatten()
                .collect(),
        })
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
        let plan = self.package_plan(engine, mode)?;
        let missing_host_files = plan
            .host_ids
            .iter()
            .filter(|id| self.store.locate(id).is_none())
            .map(|id| id.to_string())
            .collect();
        Ok(PackageStatus {
            need: plan.need,
            package: plan.package,
            missing_host_files,
        })
    }

    /// Every file a row of `engine` with precision `mode` needs on this machine, on
    /// disk or not: the device and precision it runs at ([`need_for`](Self::need_for)),
    /// the package for that device (graphs and shared weights) and the host files.
    #[cfg(feature = "torch")]
    pub fn package_plan(&self, engine: &str, mode: &str) -> Result<PackagePlan, String> {
        let need = self.need_for(engine, mode)?;
        if models::textbox_engine(need.engine) && need.device == Placement::cpu().device {
            // The ONNX route: the graph and the weights it binds are the package, the
            // vocabulary the host file (`textbox_ids`: graph, vocabulary, weights).
            let ids = models::textbox_ids(need.engine);
            let host_ids: Vec<&'static str> = ids.iter().copied().skip(1).take(1).collect();
            let package_ids: Vec<String> = ids
                .iter()
                .filter(|id| !host_ids.contains(id))
                .map(|id| id.to_string())
                .collect();
            let package = package_ids
                .iter()
                .map(|id| self.store.locate(id))
                .collect::<Option<Vec<PathBuf>>>()
                .and_then(|paths| {
                    paths
                        .first()
                        .and_then(|p| p.parent())
                        .map(std::path::Path::to_path_buf)
                });
            return Ok(PackagePlan {
                need,
                target: Some(TEXTBOX_CPU_TARGET.to_string()),
                package_ids,
                package,
                host_ids,
            });
        }
        if self.is_vulkan_device(&need.device) {
            let pl = runtime::place(Some(&need.device), &self.devices()).0;
            let rt = self
                .vulkan_pairing(&pl)
                .and_then(|p| self.vulkan_runtimes(need.engine, &p).into_iter().next())
                .map(|c| c.runtime);
            let rts: Vec<String> = self
                .vulkan_pairing(&pl)
                .map(|p| {
                    self.vulkan_runtimes(need.engine, &p)
                        .into_iter()
                        .map(|c| c.runtime)
                        .collect()
                })
                .unwrap_or_default();
            let names: Vec<&str> = rts.iter().map(String::as_str).collect();
            let host_ids = models::vulkan_host_ids(need.engine, &names);
            return Ok(PackagePlan {
                target: rt.as_ref().map(|r| format!("vulkan/{r}")),
                package_ids: rt
                    .as_deref()
                    .map(|r| self.store.vulkan_ids(need.engine, r))
                    .unwrap_or_default(),
                package: rt
                    .as_deref()
                    .and_then(|r| self.store.locate_vulkan(need.engine, r)),
                host_ids,
                need,
            });
        }
        let precision = need.precision.as_str();
        let present = self
            .store
            .torch_package_present(need.engine, precision, &need.targets);
        let target = present.as_ref().map(|p| p.target.clone()).or_else(|| {
            need.targets
                .iter()
                .find(|t| {
                    self.store
                        .torch_package_ids(need.engine, precision, t)
                        .is_some()
                })
                .cloned()
        });
        let package_ids = target
            .as_deref()
            .and_then(|t| self.store.torch_package_ids(need.engine, precision, t))
            .unwrap_or_default();
        let package = present.map(|p| p.dir);
        let mut host_ids: Vec<&'static str> = models::torch_host_ids(need.engine, need.precision);
        if need.engine == models::PADDLE {
            // As `torch_host_files`: the embedding table is loaded only for a package
            // whose shared decoder weights do not carry it (the release's packages all
            // do; an older or hand-made package may not).
            let blob = match &package {
                Some(d) => d
                    .parent()
                    .is_some_and(|p| p.join("weights-decoder.safetensors").is_file()),
                None => {
                    target.is_none()
                        || package_ids
                            .iter()
                            .any(|id| id.ends_with("/weights-decoder.safetensors"))
                }
            };
            if !blob {
                host_ids.push(models::paddle_embed_id(need.precision));
            }
        }
        Ok(PackagePlan {
            need,
            target,
            package_ids,
            package,
            host_ids,
        })
    }

    pub fn store(&self) -> &ModelStore {
        &self.store
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// The block detector `detector` (`models::block_detector`), loaded once, and the
    /// pinned `(repo, revision)` of its file when it is the pinned one.
    #[cfg(feature = "gpl-models")]
    #[allow(clippy::type_complexity)]
    fn block_detector(
        &self,
        detector: &str,
        copies: usize,
    ) -> Result<
        (
            Arc<dyn bunko_ocr::blocks::BlockDetector>,
            Option<(String, String)>,
        ),
        String,
    > {
        let file = models::ensure_all(&self.store, &models::detector_ids(detector))?.remove(0);
        let (repo, revision) = bunko_ocr::models::ANIMETEXT_SOURCE;
        let source = file
            .verified
            .then(|| (repo.to_string(), revision.to_string()));
        let mut slot = self.block_detector.lock();
        if let Some((of, weak)) = slot.as_ref()
            && of == detector
            && let Some(d) = weak.upgrade()
        {
            return Ok((d, source));
        }
        let opts = RuntimeOptions {
            intra_threads: plan::SESSION_THREADS + 2 * (copies.max(1) - 1),
            targets: vec![ExecutionTarget::Cpu],
            copies,
            spinning: false,
        };
        let loaded = Arc::new(
            bunko_ocr::animetext::AnimeText::load(&file.path, &opts)
                .map_err(|e| format!("could not load AnimeText: {e}"))?,
        );
        *slot = Some((detector.to_string(), Arc::downgrade(&loaded)));
        Ok((loaded, source))
    }

    /// The page reader on `detector`'s lines (`models::DETECTORS`).
    fn reader(
        &self,
        detector: &str,
        copies: usize,
        spinning: bool,
    ) -> Result<Arc<PpocrPageReader>, String> {
        let mut slot = self.ppocr.lock();
        if let Some((of, have, weak)) = slot.as_ref()
            && of == detector
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
        let reader = match detector {
            #[cfg(feature = "gpl-models")]
            models::CTD => {
                let file =
                    models::ensure_all(&self.store, &models::detector_ids(detector))?.remove(0);
                let ctd = bunko_ocr::ppocr::ctd::Ctd::load(&file.path, &opts)
                    .map_err(|e| format!("could not load comic-text-detector: {e}"))?;
                let reader =
                    PpocrPageReader::new(engine.with_line_detector(Arc::new(ctd)), files.pinned);
                if file.verified {
                    reader.with_detector_source(
                        bunko_ocr::models::CTD_REPO,
                        bunko_ocr::models::CTD_REVISION,
                    )
                } else {
                    reader
                }
            }
            _ => PpocrPageReader::new(engine, files.pinned),
        };
        let reader = Arc::new(reader);
        *slot = Some((detector.to_string(), copies, Arc::downgrade(&reader)));
        Ok(reader)
    }

    /// Load `engine`'s recognizer on `placement` at `precision`: libtorch when a pack
    /// is loaded, else the ONNX recognizers when built in.
    fn recognizer(
        &self,
        spec: &RowSpec,
        placement: &Placement,
        precision: Precision,
        backend: Option<&str>,
    ) -> Result<Arc<dyn Recognizer>, String> {
        #[cfg(feature = "torch")]
        if placement.provider == "vulkan" {
            return self.vulkan_recognizer(spec, placement, precision, backend);
        }
        // A libtorch GPU's format on one of the Vulkan runtimes (`fp32@ncnn`); when none
        // of them starts there, libtorch's own fp32 serves.
        #[cfg(feature = "torch")]
        if let Some(rt) = backend {
            match self.vulkan_recognizer(spec, placement, precision, Some(rt)) {
                Ok(r) => return Ok(r),
                Err(e) => warn!(
                    "{} on {} with {rt}: {e}; libtorch serves",
                    spec.engine, placement.device
                ),
            }
        }
        #[cfg(not(feature = "torch"))]
        let _ = backend;
        #[cfg(feature = "torch")]
        if let Ok(tb) = self.torch() {
            let r = self.torch_recognizer(&tb, spec, placement, precision);
            // libtorch cannot start on this GPU: its Vulkan runtimes (the candidates, then
            // the fallbacks, fetched now) before the session falls back to the CPU.
            if let Err(e) = &r
                && placement.is_gpu()
                && precision == Precision::Fp32
                && self.vulkan_pairing(placement).is_some()
            {
                warn!(
                    "{} on {}: libtorch failed ({e}); trying its Vulkan runtimes",
                    spec.engine, placement.device
                );
                if let Ok(v) = self.vulkan_recognizer(spec, placement, precision, None) {
                    return Ok(v);
                }
            }
            return r;
        }
        #[cfg(feature = "onnx-vlm")]
        {
            self.onnx_recognizer(spec, placement, precision)
        }
        #[cfg(not(feature = "onnx-vlm"))]
        {
            let _ = (placement, precision);
            Err(self
                .no_recognizer_backend(&spec.engine)
                .unwrap_or_else(|| format!("{} has no recognizer here", spec.engine)))
        }
    }

    /// A recognizer on a Vulkan GPU: the first runtime of `ocr.vulkan_backends` that
    /// loads and passes the self-test there ([`vulkan`]); fp32 only.
    #[cfg(feature = "torch")]
    fn vulkan_recognizer(
        &self,
        spec: &RowSpec,
        placement: &Placement,
        precision: Precision,
        backend: Option<&str>,
    ) -> Result<Arc<dyn Recognizer>, String> {
        let engine: &'static str = match spec.engine.as_str() {
            models::HAYAI => models::HAYAI,
            models::PADDLE => models::PADDLE,
            other => return Err(format!("{other} has no recognizer")),
        };
        if precision != Precision::Fp32 {
            return Err(format!(
                "the Vulkan runtimes compute in fp32 only (asked for {precision})"
            ));
        }
        let vb = self.vulkan()?;
        let pairing = self
            .vulkan_pairing(placement)
            .ok_or_else(|| format!("{} has no Vulkan runtime here", placement.device))?;
        let dev = pairing.entry.clone();
        // The goldens first: the verdicts are keyed on them.
        let golden = match self.store.ensure_vulkan_golden(engine) {
            Ok(p) => Some(vulkan::Golden::load(&p)?),
            Err(e) => {
                warn!("{engine}: no Vulkan self-test goldens ({e})");
                None
            }
        };
        // Fetch the graphs of every runtime that may be tried (a fallback must not wait
        // for a download in the middle of a volume), then key them on the files.
        for c in self.vulkan_runtimes(engine, &pairing) {
            if c.model_dir.is_none()
                && let Err(e) = self.store.ensure_vulkan(engine, &c.runtime)
            {
                warn!("{engine} graphs for {}: {e}", c.runtime);
            }
        }
        // The runtime the format names first (a benchmarked `fp32@<rt>`), then the other
        // candidates, then the fallbacks (fetched only when reached).
        let (mut candidates, fallbacks) = self.vulkan_ranked(engine, &pairing);
        let ranked = candidates.len();
        candidates.extend(fallbacks);
        if let Some(rt) = backend {
            match candidates.iter().position(|c| c.runtime == rt) {
                Some(i) => {
                    let c = candidates.remove(i);
                    candidates.insert(0, c);
                }
                None => return Err(format!("{rt} cannot run {engine} on {} here", dev.name)),
            }
        }
        // Only what the runtimes that may read here load (not libtorch's host files): the
        // ranked ones' fetched now, a fallback's when it is reached.
        let all: Vec<&str> = candidates.iter().map(|c| c.runtime.as_str()).collect();
        let now: Vec<&str> =
            all[..ranked.max(usize::from(backend.is_some())).min(all.len())].to_vec();
        if !now.is_empty() {
            models::vulkan_host_files(&self.store, engine, &now)?;
        }
        let (tokenizer, pos_table, embeddings) =
            models::vulkan_host_paths(&self.store, engine, &all);
        let tokenizer = tokenizer.ok_or_else(|| format!("{engine} has no tokenizer"))?;
        let store = self.store.clone();
        let fetch: Arc<vulkan::Fetch> = Arc::new(move |rt: &str| {
            info!("{engine}: fetching the {rt} files (a fallback is needed now)");
            models::vulkan_host_files(&store, engine, &[rt])?;
            store.ensure_vulkan(engine, rt).map_err(|e| e.to_string())
        });
        let opts = bunko_torch::abi::LoadOptions {
            tokenizer: tokenizer.to_string_lossy().into_owned(),
            backend: None,
            pos_table: pos_table.map(|p| p.to_string_lossy().into_owned()),
            embeddings: embeddings.map(|p| p.to_string_lossy().into_owned()),
            patch_budget: (engine == models::HAYAI).then_some(spec.patch_budget),
            threads: 1,
            ..Default::default()
        };
        let info = torch::recognizer_info(
            engine,
            precision,
            placement.vlm_device,
            if engine == models::HAYAI {
                bunko_vlm::hayai::BATCH
            } else {
                1
            },
            spec.patch_budget,
        );
        let r = vb
            .recognizer(engine, &dev, candidates, opts, golden, info, Some(fetch))
            .map_err(|e| match torch::pack_budget_error(&e, spec.patch_budget) {
                Some(update) => format!("{e} ({update})"),
                None => e,
            })?;
        Ok(r)
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
            .map_err(|e| {
                torch::pack_budget_error(&e, spec.patch_budget)
                    .unwrap_or_else(|| format!("could not load {engine}: {e}"))
            })?;
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
        self.recognizer(&spec, &placement, precision, None)
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
        let detector = spec
            .detector
            .as_deref()
            .filter(|d| !d.is_empty())
            .unwrap_or(models::PPOCR);
        if !models::DETECTORS.contains(&detector) {
            return Err(models::detector_left_out(detector).unwrap_or_else(|| {
                format!(
                    "unknown detector {detector} (this processor runs {})",
                    models::DETECTORS.join(", ")
                )
            }));
        }
        // ppocr-manga alone is its own detector and recognizer.
        if road == Road::Line && detector != models::PPOCR {
            return Err(format!(
                "{engine} reads with its own detector, not with {detector}"
            ));
        }
        // A block detector finds no lines: only an engine that reads blocks goes with it.
        let by_blocks = models::block_detector(detector);
        if by_blocks && !models::textbox_engine(engine) {
            return Err(format!(
                "the {detector} detector finds text blocks, and {engine} reads lines"
            ));
        }
        let _one_load = self.loading.lock();
        let devices = self.devices();

        // Where the recognizer runs, and in which format.
        let mut placement = Placement::cpu();
        let mut resolved = None;
        // The text-box recognizers place, load and fall back in their own arm below (a
        // GPU through the pack, else the ONNX pool on the CPU), in one format each.
        let textbox = models::textbox_engine(engine);
        if road == Road::Reconciled && !textbox {
            // No recognizer backend: nothing here knows the GPUs (unless `ocr.backend:
            // cpu` rules them out anyway), so that is the error to fix first.
            if self.config.backend != Backend::Cpu
                && let Some(e) = self.no_recognizer_backend(engine)
            {
                return Err(e);
            }
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
            let runnable = self.formats(engine, &placement);
            let formats =
                precision::auto_formats(&spec.precision, runnable, self.bf16_native(&placement));
            resolved = precision::resolve(
                engine,
                &spec.precision,
                &formats,
                spec.precision_pick.as_deref(),
                &spec.precision_why,
            )?;
        }

        let host_of = |placement: &Placement| plan::Host {
            budget: plan::host_worker_budget(plan::logical_cpu_count(), self.config.jobs),
            physical_cores: plan::core_count(),
            jobs: self.config.jobs,
            fast_gpu: placement.is_gpu() && self.bf16_native(placement),
        };
        let mut recognizer = None;
        // The export set of a text-box recognizer's files (provenance), when they are ours.
        #[allow(unused_mut)]
        let mut textbox_release: Option<String> = None;
        #[cfg(feature = "nc-models")]
        if textbox {
            let (p, why) = runtime::place_runnable(
                spec.pools
                    .stage_device
                    .get(plan::STAGE_ENGINE)
                    .map(String::as_str),
                &devices,
                |pl| self.textbox_formats(engine, pl),
            );
            if let Some(why) = why {
                let asked = spec.pools.stage_device.get(plan::STAGE_ENGINE);
                self.note_textbox_on_cpu(engine, asked.map(String::as_str), &devices, &why);
            }
            placement = p;
            // The row's precision there (nar-small; charspot-small fixes its own): on a
            // GPU every auto mode takes fp16 where it has an fp16 package (O2), on the CPU
            // (an fp32-only route, whose missing files are its loader's error) fp32. A
            // forced format the device cannot run is the precision refusal. Never through
            // `resolved`: the text-box engines stay off the generic recognizer path.
            let resolve_on = |pl: &Placement| {
                let formats = if pl.is_gpu() {
                    self.textbox_formats(engine, pl)
                } else {
                    vec!["fp32"]
                };
                precision::resolve(
                    engine,
                    &spec.precision,
                    &precision::auto_formats(&spec.precision, formats, false),
                    spec.precision_pick.as_deref(),
                    &spec.precision_why,
                )
            };
            #[allow(unused_mut)]
            let mut textbox_precision = resolve_on(&placement)?;
            #[cfg(feature = "torch")]
            if placement.is_gpu() {
                let at = textbox_precision
                    .as_ref()
                    .map_or(Precision::Fp32, |r| r.precision);
                match self.torch_textbox(engine, &placement, at) {
                    Ok((rec, release)) => {
                        recognizer = Some(rec);
                        textbox_release = Some(release);
                    }
                    Err(e) => {
                        let gpu = placement.device.clone();
                        warn!("{engine} could not start on {gpu} ({e}); reading on the CPU");
                        placement = Placement::cpu();
                        // An auto mode reads at fp32 there; a forced fp16 cannot, and the
                        // GPU's error is the one to fix: say both, the GPU's first.
                        textbox_precision = resolve_on(&placement)
                            .map_err(|cpu| gpu_then_cpu_error(engine, &gpu, &e, &cpu))?;
                    }
                }
            }
            if let Some(r) = &textbox_precision {
                info!("{engine} precision: {} ({})", r.precision, r.why);
            }
            if recognizer.is_none() {
                // The ONNX pool on the CPU: the session's threads follow the engine
                // stage's planned width.
                let host = host_of(&placement);
                let workers = plan::plan(engine, road, &placement.device, &spec.pools, &host)
                    .iter()
                    .find(|s| s.key == plan::STAGE_ENGINE)
                    .map_or(1, |s| s.workers);
                recognizer = Some(self.textbox_recognizer(engine, workers)?);
                // nar-small's CPU graph is ours; charspot-small runs its authors' own.
                textbox_release =
                    (engine == models::NAR).then(|| bunko_ocr::models::TEXTBOX_RELEASE.to_string());
            }
        }
        if let Some(r) = &resolved {
            match self.recognizer(spec, &placement, r.precision, r.backend.as_deref()) {
                Ok(rec) => recognizer = Some(rec),
                // A pack that cannot read this patch budget: updating it is the fix, and
                // the CPU reads with the same pack (no slow run that ends in the same
                // refusal).
                #[cfg(feature = "torch")]
                Err(e) if e.contains(torch::PACK_UPDATE) => return Err(e),
                Err(e) if placement.is_gpu() => {
                    let gpu = placement.device.clone();
                    warn!("{engine} could not start on {gpu} ({e}); falling back to the CPU");
                    placement = Placement::cpu();
                    let formats = precision::auto_formats(
                        &spec.precision,
                        self.formats(engine, &placement),
                        false,
                    );
                    // When the CPU cannot take it either, the GPU's error is the one to
                    // fix (a model download, a driver): say both, the GPU's first.
                    let both = |cpu: String| gpu_then_cpu_error(engine, &gpu, &e, &cpu);
                    let again = precision::resolve(
                        engine,
                        &spec.precision,
                        &formats,
                        spec.precision_pick.as_deref(),
                        &spec.precision_why,
                    )
                    .map_err(both)?;
                    resolved = again;
                    let p = resolved.as_ref().map_or(Precision::Fp32, |r| r.precision);
                    recognizer = Some(self.recognizer(spec, &placement, p, None).map_err(both)?);
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(r) = &resolved {
            info!("{engine} precision: {} ({})", r.precision, r.why);
        }

        let host = host_of(&placement);
        let stages = plan::plan(engine, road, &placement.device, &spec.pools, &host);
        let detect_workers = stages
            .iter()
            .find(|s| s.key == STAGE_DETECT)
            .map_or(1, |s| s.workers) as usize;
        #[allow(unused_mut)]
        let mut blocks: Option<Arc<dyn bunko_ocr::blocks::BlockDetector>> = None;
        #[allow(unused_mut)]
        let mut block_source: Option<(String, String)> = None;
        #[cfg(feature = "gpl-models")]
        if by_blocks {
            let (loaded, source) = self.block_detector(detector, detect_workers)?;
            (blocks, block_source) = (Some(loaded), source);
        }
        let reader = if by_blocks {
            None
        } else {
            Some(self.reader(
                detector,
                detect_workers,
                placement.is_gpu() || road == Road::Line,
            )?)
        };

        // Provenance: PP-OCR's pin when its files are the pinned ones (or the block
        // detector's), then the recognizer's source repos and the export set (spec Q9).
        let mut weights: Vec<(String, String)> =
            reader.as_ref().map(|r| r.repos()).unwrap_or_default();
        weights.extend(block_source);
        if let Some(rec) = &recognizer {
            for (repo, rev) in &rec.info().repos {
                weights.push((repo.to_string(), rev.to_string()));
            }
            // The export set the recognizer's files come from: the torch package release
            // for the libtorch recognizers, models-v1 for the ONNX ones; for a text-box
            // recognizer its arm's (`textbox-torch-v1` on a GPU, `textbox-graphs-v1` for
            // nar-small on the CPU, none for charspot-small's own files). One entry for
            // our sets (spec §14).
            let recognizer_release = if textbox {
                textbox_release.clone()
            } else {
                let vulkan = placement.provider == "vulkan"
                    || resolved.as_ref().is_some_and(|r| r.backend.is_some());
                Some(if vulkan {
                    bunko_ocr::models::vulkan_release_name()
                } else if self.torch_loaded() {
                    bunko_ocr::models::torch_release_name()
                } else {
                    bunko_ocr::models_release::RELEASE.to_string()
                })
            };
            weights.extend(models::export_entry(None, recognizer_release.as_deref()));
        }
        let (stage_workers, queue_capacity) = plan::tables(&stages);
        let pipeline = plan::graph_line(&stages);
        // A recognizer with one format (the text-box ones) says so itself.
        let fixed = recognizer
            .as_ref()
            .filter(|_| resolved.is_none())
            .map(|r| r.info().precision.to_string());
        let precision = resolved
            .as_ref()
            .map(|r| r.precision.to_string())
            .or_else(|| fixed.clone());
        // What ran, backend included (`fp32@ncnn`): the benchmark tells its candidates
        // apart by it. The sidecar keeps the plain precision.
        let ran = resolved
            .as_ref()
            .map(|r| match &r.backend {
                Some(b) => format!("{}@{b}", r.precision),
                None => r.precision.to_string(),
            })
            .or(fixed);
        let ready = ReadyInfo {
            weights: weights.iter().cloned().collect(),
            stage_workers,
            queue_capacity,
            stage_device: session::stage_devices(&stages),
            pipeline: pipeline.clone(),
            precision: ran,
        };
        let info = SessionInfo {
            engine: engine.to_string(),
            detector: detector.to_string(),
            patch_budget: spec.patch_budget,
            generator: self.config.generator.clone(),
            weights,
            precision,
            ready,
        };
        let engines = Arc::new(Engines {
            reader,
            blocks,
            recognizer,
        });
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
        // A detector is offered like an engine: when its files are here or may be
        // downloaded (the first session on it fetches them).
        let detectors = if engines.is_empty() {
            Vec::new()
        } else {
            models::DETECTORS
                .iter()
                .filter(|d| models::obtainable(&self.store, &models::detector_ids(d)))
                .map(|d| d.to_string())
                .collect()
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
                engine_revisions: engines
                    .iter()
                    .map(|e| (e.clone(), bunko_layout::sidecar::engine_revision(e)))
                    .collect(),
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

/// A recognizer that failed on the GPU and then on the CPU fallback: the GPU's error
/// first (it is the one to fix: a model download, the driver), then the CPU's, whose
/// text keeps the scheduler's classification (e.g. a precision refusal).
fn gpu_then_cpu_error(engine: &str, gpu: &str, on_gpu: &str, on_cpu: &str) -> String {
    format!("{engine} could not start on {gpu}: {on_gpu}; nor on the CPU: {on_cpu}")
}

/// Add the Vulkan runtimes' formats of a GPU (`by`: format → the engines a runtime
/// runs in it there) to its catalog entry, after its own. A format only some of
/// `recognizers` run there (WebGPU has no paddle-manga; the text-box engines have no
/// Vulkan runtime) lists those engines in `format_engines`, so the library and the
/// benchmark offer it to them only. A format libtorch runs too on that GPU also serves
/// libtorch's engines there; a GPU only the Vulkan pack drives (provider `vulkan`) has
/// no libtorch format: its listed `fp32` is the runtimes'.
#[cfg_attr(not(feature = "torch"), allow(dead_code))]
fn add_vulkan_formats(
    d: &mut Device,
    by: std::collections::BTreeMap<&'static str, Vec<String>>,
    recognizers: &[&str],
) {
    let libtorch = d.provider.as_deref() != Some("vulkan");
    for (f, engines) in by {
        let mut serving: Vec<String> = match d.format_engines.get(f) {
            Some(only) => only.clone(),
            None if libtorch && d.formats.iter().any(|x| x == f) => {
                recognizers.iter().map(|e| e.to_string()).collect()
            }
            None => Vec::new(),
        };
        for e in engines {
            if !serving.contains(&e) {
                serving.push(e);
            }
        }
        if !d.formats.iter().any(|x| x == f) {
            d.formats.push(f.to_string());
        }
        if serving.len() < recognizers.len() {
            serving.sort_by_key(|e| recognizers.iter().position(|r| r == e));
            d.format_engines.insert(f.to_string(), serving);
        } else {
            d.format_engines.remove(f);
        }
    }
}

/// The formats a text-box engine's libtorch packages are built in (never bf16):
/// nar-small fp32 and fp16 (the authors' fp16 mode, bound to their fp32 weights cast at
/// load), charspot-small fp32. Empty for any other engine.
#[cfg_attr(not(feature = "torch"), allow(dead_code))]
pub(crate) fn textbox_precisions(engine: &str) -> &'static [&'static str] {
    match engine {
        "nar-small" => &["fp32", "fp16"],
        "charspot-small" => &["fp32"],
        _ => &[],
    }
}

/// The formats a text-box row of `engine` in precision `mode` can resolve to on a device
/// that runs it in `device_formats` ([`EnginePipeline::textbox_formats`]): the packages
/// its prefetch fetches (Q21). What `bunko_sched::precision::resolve_mode` finds usable
/// there (an `auto-accuracy` row one format, a benchmarked mode each candidate), empty
/// when the row cannot run there; fp32 for an engine with a fixed precision
/// (charspot-small).
#[cfg_attr(not(feature = "torch"), allow(dead_code))]
pub(crate) fn textbox_prefetch_formats(
    engine: &str,
    mode: &str,
    device_formats: &[&str],
) -> Vec<String> {
    if !precision::is_precision_engine(engine) {
        return vec!["fp32".to_string()];
    }
    let set: std::collections::BTreeSet<String> =
        device_formats.iter().map(|f| f.to_string()).collect();
    let r = bunko_sched::precision::resolve_mode(engine, mode, Some(&set), None, "");
    if r.eligible { r.usable } else { Vec::new() }
}

/// The formats a text-box engine runs in on a libtorch GPU: none unless the pack reads
/// text boxes (`bt_read_chars`, `reads_chars`) and the host files are here or
/// downloadable (`host_files`); else the formats with a package for the device
/// (`packaged`, as [`torch::device_formats`] lists them) that the engine is built in
/// ([`textbox_precisions`]), in `packaged` order.
#[cfg_attr(not(feature = "torch"), allow(dead_code))]
pub(crate) fn textbox_gpu_formats(
    engine: &str,
    reads_chars: bool,
    packaged: &[&'static str],
    host_files: bool,
) -> Vec<&'static str> {
    if !(reads_chars && host_files) {
        return Vec::new();
    }
    let built = textbox_precisions(engine);
    packaged
        .iter()
        .copied()
        .filter(|p| built.contains(p))
        .collect()
}

/// The target a text-box row on the CPU names (its ONNX route) in
/// [`PackagePlan::target`] and [`Prefetched::target`].
#[cfg(feature = "torch")]
const TEXTBOX_CPU_TARGET: &str = "onnx";

/// Why a text-box row reads on the CPU, as [`runtime::place_runnable`] reported it
/// (`why`): the pack's own reason ([`torch::TEXTBOX_PACK_REASON`]) only when the device
/// the row was placed on is a libtorch GPU and the loaded pack has no `bt_read_chars`
/// (`pack_reads_chars` is `None` with no libtorch pack); any other reason (a pinned GPU
/// this machine lacks, a GPU only the Vulkan pack drives, no package for the device)
/// as given. `true` with the pack's reason.
#[cfg_attr(not(feature = "torch"), allow(dead_code))]
pub(crate) fn textbox_cpu_reason(
    asked: Option<&str>,
    devices: &[Device],
    pack_reads_chars: Option<bool>,
    why: &str,
) -> (String, bool) {
    let (p, _) = runtime::place(asked, devices);
    if pack_reads_chars == Some(false) && p.is_gpu() && p.provider != "vulkan" {
        #[cfg(feature = "torch")]
        return (torch::TEXTBOX_PACK_REASON.to_string(), true);
    }
    (why.to_string(), false)
}

/// How a text-box row's CPU placement is logged: `None`, a warning on every session,
/// when the row pinned its engine stage to a GPU; otherwise the key of a note logged
/// once per process (the pack's reason once for every engine, any other reason once per
/// engine and reason, which names the device).
#[cfg_attr(not(feature = "nc-models"), allow(dead_code))]
pub(crate) fn textbox_cpu_note(
    engine: &str,
    asked: Option<&str>,
    reason: &str,
    pack_reason: bool,
) -> Option<String> {
    let pinned = asked
        .map(|a| a.trim().to_ascii_lowercase())
        .is_some_and(|a| !matches!(a.as_str(), "" | "auto" | "cpu"));
    if pinned {
        None
    } else if pack_reason {
        Some("pack".to_string())
    } else {
        Some(format!("{engine}\n{reason}"))
    }
}

/// The text-box CPU notes already logged in this process ([`textbox_cpu_note`]'s keys).
#[cfg(feature = "nc-models")]
static TEXTBOX_CPU_NOTED: std::sync::LazyLock<Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

#[cfg(test)]
mod backends_dirs_tests {
    use super::*;

    /// The GPU's error leads; the CPU's refusal text survives for the scheduler.
    #[test]
    fn a_failed_fallback_names_the_gpu_error_first() {
        let cpu = precision::PRECISION_REFUSAL.to_string() + ": paddle-manga is asked for bf16";
        let msg = gpu_then_cpu_error(
            "paddle-manga",
            "gpu:0",
            "download failed: sha256 x != y",
            &cpu,
        );
        assert!(
            msg.starts_with("paddle-manga could not start on gpu:0: download failed"),
            "{msg}"
        );
        assert!(msg.contains(precision::PRECISION_REFUSAL), "{msg}");
    }

    /// A text-box engine runs on a GPU only through a pack that has `bt_read_chars`,
    /// with its host files obtainable, and only in the formats it has packages for and
    /// computes in (nar-small fp32 and fp16, charspot-small fp32; never bf16).
    #[test]
    fn textbox_gpu_formats_need_read_chars() {
        assert!(textbox_gpu_formats("nar-small", false, &["fp32", "fp16"], true).is_empty());
        assert!(textbox_gpu_formats("nar-small", true, &["fp32"], false).is_empty());
        assert_eq!(
            textbox_gpu_formats("nar-small", true, &["fp32"], true),
            ["fp32"]
        );
        assert_eq!(
            textbox_gpu_formats("charspot-small", true, &["fp32"], true),
            ["fp32"]
        );
        assert!(textbox_gpu_formats("hayai-nova", true, &["fp32"], true).is_empty());
        assert_eq!(textbox_precisions("nar-small"), ["fp32", "fp16"]);
        assert!(textbox_precisions("paddle-manga").is_empty());
    }

    /// The pack's reason only where it is the cause: a libtorch GPU the row was placed
    /// on, under a pack without `bt_read_chars`. A pinned GPU this machine lacks and a
    /// GPU only the Vulkan pack drives keep their own reason.
    #[test]
    fn a_text_box_cpu_reason_names_its_cause() {
        let dev = |id: &str, provider: &str| Device {
            format_engines: Default::default(),
            id: id.into(),
            label: id.into(),
            formats: vec!["fp32".into()],
            provider: Some(provider.into()),
            arch: None,
        };
        let rocm = [dev("cpu", "cpu"), dev("gpu:0", "rocm")];
        let vulkan = [dev("cpu", "cpu"), dev("gpu:0", "vulkan")];
        let why = "gpu:0 (gpu:0) has no compiled package for this engine here";
        #[cfg(feature = "torch")]
        {
            let pack = (torch::TEXTBOX_PACK_REASON.to_string(), true);
            assert_eq!(textbox_cpu_reason(None, &rocm, Some(false), why), pack);
            assert_eq!(
                textbox_cpu_reason(Some("auto"), &rocm, Some(false), why),
                pack
            );
            assert_eq!(
                textbox_cpu_reason(Some("gpu:0"), &rocm, Some(false), why),
                pack
            );
        }
        let own = |w: &str| (w.to_string(), false);
        // The pack reads text boxes: the GPU has no package for the engine.
        assert_eq!(textbox_cpu_reason(None, &rocm, Some(true), why), own(why));
        // No libtorch pack at all.
        assert_eq!(textbox_cpu_reason(None, &rocm, None, why), own(why));
        // A pinned GPU this machine lacks.
        let gone = "gpu:5 is not a device of this machine";
        assert_eq!(
            textbox_cpu_reason(Some("gpu:5"), &rocm, Some(false), gone),
            own(gone)
        );
        // A GPU only the Vulkan pack drives.
        assert_eq!(
            textbox_cpu_reason(None, &vulkan, Some(false), why),
            own(why)
        );
    }

    /// A row pinned to a GPU is warned about every time; an unpinned row is noted once
    /// per process per reason, and the pack's reason once for every engine.
    #[test]
    fn a_text_box_cpu_note_warns_only_for_a_pin() {
        assert_eq!(
            textbox_cpu_note("nar-small", Some("gpu:0"), "r", false),
            None
        );
        assert_eq!(
            textbox_cpu_note("nar-small", Some(" GPU:1 "), "r", true),
            None
        );
        let note = |asked| textbox_cpu_note("nar-small", asked, "r", false);
        assert_eq!(note(None), Some("nar-small\nr".to_string()));
        assert_eq!(note(Some("auto")), note(None));
        assert_eq!(note(Some("")), note(None));
        assert_ne!(
            textbox_cpu_note("charspot-small", None, "r", false),
            note(None)
        );
        assert_eq!(
            textbox_cpu_note("nar-small", None, "r", true),
            textbox_cpu_note("charspot-small", None, "other", true)
        );
    }

    /// nar-small runs at fp16 on a GPU with an fp16 package; charspot-small has fp32
    /// only.
    #[test]
    fn nar_takes_fp16_on_a_gpu_and_charspot_does_not() {
        assert_eq!(
            textbox_gpu_formats("nar-small", true, &["fp32", "bf16", "fp16"], true),
            ["fp32", "fp16"]
        );
        assert_eq!(
            textbox_gpu_formats("charspot-small", true, &["fp32", "bf16", "fp16"], true),
            ["fp32"]
        );
    }

    /// Q21, O2: a text-box row prefetches the packages its mode can resolve to on the
    /// device: fp16 for auto-accuracy on a GPU with an fp16 package, both for the
    /// benchmarked modes, fp32 where there is no fp16 package, nothing for a forced
    /// format the device cannot run.
    #[test]
    fn textbox_prefetch_takes_what_the_mode_can_resolve_to() {
        let both = ["fp32", "fp16"];
        assert_eq!(
            textbox_prefetch_formats("nar-small", "auto-accuracy", &both),
            ["fp16"]
        );
        assert_eq!(
            textbox_prefetch_formats("nar-small", "auto-balanced", &both),
            ["fp16", "fp32"]
        );
        assert_eq!(
            textbox_prefetch_formats("nar-small", "auto-speed", &both),
            ["fp16", "fp32"]
        );
        assert_eq!(
            textbox_prefetch_formats("nar-small", "fp32", &both),
            ["fp32"]
        );
        // no fp16 package there
        assert_eq!(
            textbox_prefetch_formats("nar-small", "auto-accuracy", &["fp32"]),
            ["fp32"]
        );
        assert!(textbox_prefetch_formats("nar-small", "fp16", &["fp32"]).is_empty());
        assert_eq!(
            textbox_prefetch_formats("charspot-small", "auto-accuracy", &["fp32"]),
            ["fp32"]
        );
    }

    /// The Vulkan formats in the catalog: a GPU only the Vulkan pack drives runs no
    /// text-box engine, a qualified format beside libtorch serves its runtimes' engines
    /// only, and a libtorch format the runtimes also run keeps libtorch's engines.
    #[test]
    fn vulkan_formats_name_their_engines() {
        let all = ["hayai-nova", "paddle-manga", "charspot-small", "nar-small"];
        let two = |a: &str, b: &str| vec![a.to_string(), b.to_string()];
        let by = |pairs: &[(&'static str, &[&str])]| -> std::collections::BTreeMap<&'static str, Vec<String>> {
            pairs
                .iter()
                .map(|(f, e)| (*f, e.iter().map(|x| x.to_string()).collect()))
                .collect()
        };
        // A GPU only the Vulkan pack drives: its fp32 is hayai's and paddle's.
        let mut d = Device {
            id: "gpu:1".into(),
            formats: vec!["fp32".into()],
            provider: Some("vulkan".into()),
            ..Default::default()
        };
        add_vulkan_formats(
            &mut d,
            by(&[
                ("fp32", &["hayai-nova", "paddle-manga"]),
                ("fp32@webgpu", &["hayai-nova"]),
            ]),
            &all,
        );
        assert_eq!(d.formats, ["fp32", "fp32@webgpu"]);
        assert_eq!(
            d.format_engines.get("fp32"),
            Some(&two("hayai-nova", "paddle-manga"))
        );
        assert_eq!(
            d.format_engines.get("fp32@webgpu"),
            Some(&vec!["hayai-nova".to_string()])
        );
        // Without the text-box engines in the build, both engines are every engine.
        let mut d = Device {
            id: "gpu:1".into(),
            provider: Some("vulkan".into()),
            ..Default::default()
        };
        add_vulkan_formats(
            &mut d,
            by(&[("fp32", &["hayai-nova", "paddle-manga"])]),
            &["hayai-nova", "paddle-manga"],
        );
        assert_eq!(d.formats, ["fp32"]);
        assert!(d.format_engines.is_empty());
        // Beside libtorch: fp32 (every engine there) stays unlisted; nar-only fp16
        // gains nothing; the qualified format is the runtime's engines'.
        let mut d = Device {
            id: "gpu:0".into(),
            formats: vec!["fp32".into(), "fp16".into()],
            provider: Some("rocm".into()),
            format_engines: [("fp16".to_string(), vec!["nar-small".to_string()])]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        add_vulkan_formats(
            &mut d,
            by(&[("fp32@ncnn", &["hayai-nova", "paddle-manga"])]),
            &all,
        );
        assert_eq!(d.formats, ["fp32", "fp16", "fp32@ncnn"]);
        assert!(!d.format_engines.contains_key("fp32"));
        assert_eq!(
            d.format_engines.get("fp32@ncnn"),
            Some(&two("hayai-nova", "paddle-manga"))
        );
        assert_eq!(
            d.format_engines.get("fp16"),
            Some(&vec!["nar-small".to_string()])
        );
    }

    #[test]
    fn sibling_then_fallbacks_each_once() {
        if std::env::var_os(BACKENDS_DIR_ENV).is_some() {
            return; // the override would come first; covered by its doc
        }
        let mut c = EngineConfig::new(PathBuf::from("/p/models"), Backend::Auto);
        c.fallback_backends = vec![PathBuf::from("/lib/backends"), PathBuf::from("/p/backends")];
        assert_eq!(c.backends_dir(), PathBuf::from("/p/backends"));
        assert_eq!(
            c.backends_dirs(),
            vec![PathBuf::from("/p/backends"), PathBuf::from("/lib/backends")]
        );
    }
}
