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
    /// disk or downloadable for hayai-nova or paddle-manga), not every format it computes
    /// in (a Turing card computes bf16 but no bf16 package exists for it).
    pub fn catalog_devices(&self) -> Vec<Device> {
        #[cfg_attr(not(feature = "torch"), allow(unused_mut))]
        let mut devices = self.devices();
        #[cfg(feature = "torch")]
        if let Ok(tb) = self.torch() {
            for d in devices
                .iter_mut()
                .filter(|d| d.provider.as_deref() != Some("vulkan"))
            {
                d.formats = torch::runnable_formats(&tb.report, &d.id, |engine, p, targets| {
                    self.store.torch_package_obtainable(engine, p, targets)
                });
            }
        }
        // The Vulkan runtimes' formats (either engine), after the device's own.
        #[cfg(feature = "torch")]
        for d in &mut devices {
            if d.id == "cpu" {
                continue;
            }
            let pl = runtime::place(Some(&d.id), std::slice::from_ref(&*d)).0;
            let mut by: std::collections::BTreeMap<&'static str, Vec<String>> = Default::default();
            for engine in [models::HAYAI, models::PADDLE] {
                for f in self.vulkan_formats(engine, &pl) {
                    if !d.formats.iter().any(|x| x == f) {
                        d.formats.push(f.to_string());
                    }
                    by.entry(f).or_default().push(engine.to_string());
                }
            }
            // A Vulkan format only some engines run here (WebGPU has no paddle-manga):
            // the library and the benchmark offer it to those engines only.
            for (f, engines) in by {
                if f.contains('@') && engines.len() < 2 {
                    d.format_engines.insert(f.to_string(), engines);
                }
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

    /// The error of a row whose engine has no compiled package for `placement`'s device
    /// here or in the release.
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
    /// loaded and no ONNX recognizers built in. None when one can.
    fn no_recognizer_backend(&self, engine: &str) -> Option<String> {
        if self.torch_loaded() || cfg!(feature = "onnx-vlm") {
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
        let r = vb.recognizer(engine, &dev, candidates, opts, golden, info, Some(fetch))?;
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

        let mut recognizer = None;
        if let Some(r) = &resolved {
            match self.recognizer(spec, &placement, r.precision, r.backend.as_deref()) {
                Ok(rec) => recognizer = Some(rec),
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
            let vulkan = placement.provider == "vulkan"
                || resolved.as_ref().is_some_and(|r| r.backend.is_some());
            let release = if vulkan {
                bunko_ocr::models::vulkan_release_name()
            } else if self.torch_loaded() {
                bunko_ocr::models::torch_release_name()
            } else {
                bunko_ocr::models_release::RELEASE.to_string()
            };
            weights.push((models::EXPORT_REPO.to_string(), release));
        }
        let (stage_workers, queue_capacity) = plan::tables(&stages);
        let pipeline = plan::graph_line(&stages);
        let precision = resolved.as_ref().map(|r| r.precision.to_string());
        // What ran, backend included (`fp32@ncnn`): the benchmark tells its candidates
        // apart by it. The sidecar keeps the plain precision.
        let ran = resolved.as_ref().map(|r| match &r.backend {
            Some(b) => format!("{}@{b}", r.precision),
            None => r.precision.to_string(),
        });
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

/// A recognizer that failed on the GPU and then on the CPU fallback: the GPU's error
/// first (it is the one to fix: a model download, the driver), then the CPU's, whose
/// text keeps the scheduler's classification (e.g. a precision refusal).
fn gpu_then_cpu_error(engine: &str, gpu: &str, on_gpu: &str, on_cpu: &str) -> String {
    format!("{engine} could not start on {gpu}: {on_gpu}; nor on the CPU: {on_cpu}")
}

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
