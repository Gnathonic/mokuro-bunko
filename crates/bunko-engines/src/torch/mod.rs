//! The libtorch recognizer backend (feature `torch`, docs/rust-port/TORCH-BACKEND.md).
//!
//! The binary never links libtorch: a **backend pack** under `<storage>/backends/`
//! (`torch-<variant>-<torch version>/` with `pack.json`, `libbunko_torch` and `lib/`) is
//! opened at run time ([`loader`]). One pack per process, opened on first use and kept
//! for the life of the process ([`backend`]). With no pack, or one that fails to load,
//! the recognizer engines are not offered and the reason is logged; ppocr-manga (ONNX
//! Runtime, CPU) still works — as 0.5.2 ran before its torch venv was installed.
//!
//! Which pack: `MOKURO_TORCH_PACK=<dir>` if set; else among the installed packs, a GPU
//! variant whose vendor's driver is present (`/dev/kfd` → ROCm, the NVIDIA driver →
//! CUDA), then `cpu`, then any other. A GPU pack also runs on the CPU. When packs may
//! be installed in more than one directory (a processor also looks in the library's
//! default `backends/`, see `EngineConfig::backends_dirs`), the directories are tried
//! in order, each in that order ([`discover_all`]).

pub mod loader;
pub mod recognizer;
pub mod textbox;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use bunko_torch::abi::{DeviceEntry, DevicesReport, LoadOptions, PACK_JSON};
use bunko_vlm::{Precision, RecognizerInfo};
use tracing::{info, warn};

pub use bunko_torch::abi;
pub use loader::{Pack, PackLoadError};
pub use recognizer::TorchRecognizer;
pub use textbox::TorchTextbox;

/// Use this pack directory instead of searching `<storage>/backends/`.
pub const PACK_ENV: &str = "MOKURO_TORCH_PACK";
/// CPU threads of a CPU recognizer (default: the session's share of the cores).
pub const THREADS_ENV: &str = "MOKURO_TORCH_THREADS";

/// What the hayai-nova of a pack built before 0.8.0 answers for a patch budget other than
/// 512 (libtorch, then ncnn): those packs read 512 only. The texts are fixed in the
/// released packs.
const OLD_PACK_BUDGET_REFUSALS: &[&str] = &[
    "hayai-nova packages are compiled for patch budget",
    "hayai-nova's ncnn graphs read patch budget",
];

/// What the error for such a pack asks for ([`pack_budget_error`]).
pub const PACK_UPDATE: &str = "update the OCR backend pack";

/// Why a text-box engine does not run on a pack without `bt_read_chars` (one built
/// before 0.8.0).
pub const TEXTBOX_PACK_REASON: &str = "the OCR backend pack predates GPU text-box reading; update the OCR backend pack (`mokuro-bunko install-ocr --force`)";

/// A GPU that reports less memory than this reads text boxes in batches of
/// [`TEXTBOX_SMALL_BATCH`] instead of its package's own `bunko.max_batch`.
pub const TEXTBOX_SMALL_VRAM_MB: u64 = 4096;
/// The text-box batch of a GPU under [`TEXTBOX_SMALL_VRAM_MB`].
pub const TEXTBOX_SMALL_BATCH: u32 = 8;

/// The batch cap of a text-box engine on a device that reports `vram_mb`
/// (`DeviceEntry::vram_mb`): [`TEXTBOX_SMALL_BATCH`] on a GPU under
/// [`TEXTBOX_SMALL_VRAM_MB`], else None (the package's own `bunko.max_batch`). Unknown
/// memory is None.
pub fn textbox_max_batch(vram_mb: Option<u64>) -> Option<u32> {
    vram_mb
        .filter(|mb| *mb < TEXTBOX_SMALL_VRAM_MB)
        .map(|_| TEXTBOX_SMALL_BATCH)
}

/// The error to show when `err` is a pack's refusal of patch budget `budget` (a pack from
/// before 0.8.0, which reads 512 only): the pack is to update, not the device. None for
/// any other error.
pub fn pack_budget_error(err: &str, budget: u32) -> Option<String> {
    is_pack_budget_refusal(err).then(|| {
        format!(
            "the installed OCR backend pack reads hayai-nova at patch budget 512 only, not \
             {budget}: {PACK_UPDATE} (`mokuro-bunko install-ocr --force`) or set the \
             generation's patch budget to 512"
        )
    })
}

/// Whether `err` is the refusal of a pack from before 0.8.0 to read hayai-nova at a patch
/// budget other than 512.
pub fn is_pack_budget_refusal(err: &str) -> bool {
    OLD_PACK_BUDGET_REFUSALS.iter().any(|r| err.contains(r))
}

/// Installed packs in `backends_dir`, in the order they are tried.
pub fn discover(backends_dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(backends_dir) else {
        return Vec::new();
    };
    let mut packs: Vec<(u8, String, PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        // `.staging-*`, `.prev-*`: an install or an automatic update in progress or kept
        // for a rollback, never a pack to open.
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .filter(|p| p.join(PACK_JSON).is_file())
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().into_owned();
            // The variant only says what the pack is (libtorch or Vulkan, which GPU), so
            // it comes from the manifest whatever the pack's release: a new release
            // lists the old one's packs during its automatic update. Opening a pack
            // checks its release.
            let variant = std::fs::read_to_string(p.join(PACK_JSON))
                .ok()
                .and_then(|text| bunko_torch::abi::PackManifest::parse(&text).ok())
                .map(|m| m.variant)
                .unwrap_or_else(|| name.clone());
            // The Vulkan pack speaks the same ABI but is not a libtorch backend: it is
            // opened beside one (`crate::vulkan`).
            (variant != crate::vulkan::VARIANT).then(|| (rank(&variant), name, p))
        })
        .collect();
    // Best rank first; within a rank the newest name (torch version) first.
    packs.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    packs.into_iter().map(|p| p.2).collect()
}

fn vendor_present(variant: &str) -> bool {
    if variant.starts_with("rocm") {
        return Path::new("/dev/kfd").exists();
    }
    if variant.starts_with("cu") {
        return Path::new("/proc/driver/nvidia").exists()
            || Path::new("/dev/nvidiactl").exists()
            || std::env::var_os("SystemRoot")
                .is_some_and(|r| Path::new(&r).join("System32/nvcuda.dll").exists());
    }
    false
}

/// 0: a GPU pack whose driver is here; 1: cpu; 2: anything else.
fn rank(variant: &str) -> u8 {
    if variant == "cpu" {
        1
    } else if vendor_present(variant) {
        0
    } else {
        2
    }
}

/// The opened pack of this process and what it reported.
pub struct TorchBackend {
    pub pack: Arc<Pack>,
    pub report: DevicesReport,
    loaded: Mutex<HashMap<Key, Weak<TorchRecognizer>>>,
    textboxes: Mutex<HashMap<Key, Weak<TorchTextbox>>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    engine: String,
    precision: Precision,
    device: String,
    dir: PathBuf,
    budget: u32,
    threads: u32,
}

/// [`discover`] over several directories: the first directory's packs first; a pack
/// reached twice (the same directory listed twice) once.
pub fn discover_all(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        for pack in discover(dir) {
            let key = std::fs::canonicalize(&pack).unwrap_or_else(|_| pack.clone());
            if !seen
                .iter()
                .any(|s| std::fs::canonicalize(s).unwrap_or_else(|_| s.clone()) == key)
            {
                seen.push(pack);
            }
        }
    }
    seen
}

/// The process's backend, once decided. A loaded library is never unloaded, so a
/// success stays for the life of the process; a failure stays too (every caller sees the
/// same answer) until [`forget_failure`] lets the next caller try again.
static BACKEND: Mutex<Option<Result<Arc<TorchBackend>, String>>> = Mutex::new(None);

/// The process's backend: the first pack of [`discover`] (or `MOKURO_TORCH_PACK`)
/// that opens. Decided once; later calls return the same result whatever they pass.
pub fn backend(backends_dir: &Path, cpu_only: bool) -> Result<Arc<TorchBackend>, String> {
    backend_in(&[backends_dir.to_path_buf()], cpu_only)
}

/// [`backend`] searching several directories in order ([`discover_all`]).
pub fn backend_in(backends_dirs: &[PathBuf], cpu_only: bool) -> Result<Arc<TorchBackend>, String> {
    let mut slot = BACKEND.lock().unwrap_or_else(|e| e.into_inner());
    slot.get_or_insert_with(|| open_backend(backends_dirs, cpu_only))
        .clone()
}

/// The backend if [`backend`] was already called (no loading).
pub fn backend_if_open() -> Option<Arc<TorchBackend>> {
    BACKEND
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|r| r.as_ref().ok().cloned())
}

/// Let the next [`backend`] call look for a pack again after a failure (a backend pack
/// was installed while this process runs: its OCR picks it up without a restart).
/// Returns false when a backend is loaded already: that one stays.
pub fn forget_failure() -> bool {
    let mut slot = BACKEND.lock().unwrap_or_else(|e| e.into_inner());
    match slot.as_ref() {
        Some(Ok(_)) => false,
        _ => {
            *slot = None;
            true
        }
    }
}

fn open_backend(backends_dirs: &[PathBuf], cpu_only: bool) -> Result<Arc<TorchBackend>, String> {
    let candidates = match std::env::var_os(PACK_ENV).filter(|v| !v.is_empty()) {
        Some(p) => vec![PathBuf::from(p)],
        None => discover_all(backends_dirs),
    };
    if candidates.is_empty() {
        let msg = format!(
            "no libtorch backend pack in {} (install one with `mokuro-bunko install-ocr`)",
            backends_dirs
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(" or ")
        );
        info!("{msg}");
        return Err(msg);
    }
    let mut errors = Vec::new();
    for dir in candidates {
        let t0 = std::time::Instant::now();
        match Pack::open(&dir, cpu_only) {
            Ok(pack) => match pack.devices() {
                Ok(report) => {
                    for w in &report.warnings {
                        warn!("libtorch backend: {w}");
                    }
                    // (A moved TMPDIR is in the warnings above, with why.)
                    for (k, v) in pack.env.iter().filter(|(k, _)| k != abi::TMPDIR_VAR) {
                        info!("libtorch backend: set {k}={v}");
                    }
                    info!(
                        "libtorch backend {} for {} ({}, torch {}) loaded in {:.1}s: {}",
                        pack.manifest.variant,
                        if pack.manifest.bunko_version.is_empty() {
                            "a development build"
                        } else {
                            pack.manifest.bunko_version.as_str()
                        },
                        dir.display(),
                        report.torch,
                        t0.elapsed().as_secs_f64(),
                        report
                            .devices
                            .iter()
                            .map(describe_device)
                            .collect::<Vec<_>>()
                            .join("; ")
                    );
                    return Ok(Arc::new(TorchBackend {
                        pack: Arc::new(pack),
                        report,
                        loaded: Mutex::new(HashMap::new()),
                        textboxes: Mutex::new(HashMap::new()),
                    }));
                }
                Err(e) => errors.push(format!("{}: {e}", dir.display())),
            },
            Err(e) => {
                warn!("libtorch backend pack not usable: {e}");
                errors.push(e.to_string());
            }
        }
    }
    Err(errors.join("; "))
}

fn describe_device(d: &DeviceEntry) -> String {
    let mut s = format!("{} {}", d.id, d.name);
    if !d.arch.is_empty() {
        s.push_str(&format!(" ({})", d.arch));
    }
    if let Some(mb) = d.vram_mb {
        s.push_str(&format!(" {:.1} GB", mb as f64 / 1024.0));
    }
    s.push_str(&format!(" [{}]", d.formats.join(",")));
    s
}

/// The catalog label of a device.
pub fn label(d: &DeviceEntry) -> String {
    match d.kind.as_str() {
        "cpu" => {
            let threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            format!("CPU ({threads} threads)")
        }
        _ => {
            let mut s = d.name.clone();
            let mut extra = Vec::new();
            if !d.arch.is_empty() {
                extra.push(d.arch.clone());
            }
            if let Some(mb) = d.vram_mb {
                extra.push(format!("{:.0} GB", mb as f64 / 1024.0));
            }
            if !extra.is_empty() {
                s.push_str(&format!(" ({})", extra.join(", ")));
            }
            s
        }
    }
}

/// Everything a torch recognizer needs from the model store.
#[derive(Debug, Clone)]
pub struct TorchFiles {
    pub package: PathBuf,
    pub tokenizer: PathBuf,
    pub pos_table: Option<PathBuf>,
    /// hayai-nova's token embeddings; paddle-manga's input embeddings when the
    /// packages' weights do not carry them.
    pub embeddings: Option<PathBuf>,
    /// Where `.pt2` zips are unpacked to be loaded in place (under `<storage>`).
    pub unpack_cache: Option<PathBuf>,
    /// Weights a weightless package binds at load (nar-small: the authors'
    /// `model.safetensors`). Empty for hayai-nova and paddle-manga, whose packages
    /// find their shared weights beside them.
    pub weights: Vec<PathBuf>,
}

/// The load options of a text-box engine: its vocabulary, the weights its package
/// binds, the unpack cache, the preprocessing threads and the batch cap (None: the
/// package's own `bunko.max_batch`).
pub fn textbox_load_options(
    files: &TorchFiles,
    threads: u32,
    max_batch: Option<u32>,
) -> LoadOptions {
    LoadOptions {
        tokenizer: files.tokenizer.to_string_lossy().into_owned(),
        threads,
        weights: Some(
            files
                .weights
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        ),
        cache_dir: files
            .unpack_cache
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        max_batch,
        ..LoadOptions::default()
    }
}

impl TorchBackend {
    /// The catalog entry of `id` (`cpu`, `gpu:<n>`).
    pub fn device(&self, id: &str) -> Option<&DeviceEntry> {
        self.report.devices.iter().find(|d| d.id == id)
    }

    /// The recognizer for these files on `device` (`cpu` / `gpu:<n>`), shared while
    /// anyone holds it.
    #[allow(clippy::too_many_arguments)]
    pub fn recognizer(
        &self,
        engine: &'static str,
        precision: Precision,
        device: &str,
        files: &TorchFiles,
        budget: u32,
        threads: u32,
    ) -> Result<Arc<TorchRecognizer>, String> {
        let key = Key {
            engine: engine.into(),
            precision,
            device: device.into(),
            dir: files.package.clone(),
            budget,
            threads,
        };
        let mut map = self.loaded.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(r) = map.get(&key).and_then(Weak::upgrade) {
            return Ok(r);
        }
        let opts = LoadOptions {
            tokenizer: files.tokenizer.to_string_lossy().into_owned(),
            backend: None,
            pos_table: files
                .pos_table
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            embeddings: files
                .embeddings
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            patch_budget: (engine == crate::models::HAYAI).then_some(budget),
            threads,
            vision: None,
            prefill: None,
            step: None,
            weights: None,
            cache_dir: files
                .unpack_cache
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            max_batch: None,
        };
        let handle = self
            .pack
            .load(engine, &files.package, precision.as_str(), device, &opts)?;
        let li = handle.info()?;
        let vlm_device = if device == "cpu" {
            bunko_vlm::Device::Cpu
        } else {
            device
                .parse()
                .map_err(|e: bunko_vlm::VlmError| e.to_string())?
        };
        let info = recognizer_info(engine, precision, vlm_device, li.batch as usize, budget);
        info!(
            "{engine} {} on {device} (libtorch {}, package I/O v{}, {} shared weights) loaded in {:.1}s from {}",
            li.precision,
            li.torch,
            li.io,
            li.weights,
            li.load_seconds,
            files.package.display()
        );
        // The compiled models are loaded now: they must have bound to the pack's libtorch.
        let foreign = self.pack.foreign_libraries();
        if !foreign.is_empty() {
            return Err(format!(
                "libtorch libraries from outside the pack were loaded ({}); remove them from LD_LIBRARY_PATH / LD_PRELOAD",
                foreign.join(", ")
            ));
        }
        let r = Arc::new(TorchRecognizer::new(self.pack.clone(), handle, info));
        map.retain(|_, w| w.strong_count() > 0);
        map.insert(key, Arc::downgrade(&r));
        Ok(r)
    }

    /// The text-box reader of `engine` for these files on `device`, shared while anyone
    /// holds it. `max_batch` caps the crops per graph call ([`textbox_max_batch`]; None:
    /// the package's own). A pack without `bt_read_chars` answers
    /// [`TEXTBOX_PACK_REASON`].
    #[allow(clippy::too_many_arguments)]
    pub fn textbox(
        &self,
        engine: &'static str,
        precision: Precision,
        device: &str,
        files: &TorchFiles,
        threads: u32,
        max_batch: Option<u32>,
    ) -> Result<Arc<TorchTextbox>, String> {
        if !self.pack.reads_chars() {
            return Err(TEXTBOX_PACK_REASON.into());
        }
        let key = Key {
            engine: engine.into(),
            precision,
            device: device.into(),
            dir: files.package.clone(),
            budget: max_batch.unwrap_or(0),
            threads,
        };
        let mut map = self.textboxes.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(r) = map.get(&key).and_then(Weak::upgrade) {
            return Ok(r);
        }
        let opts = textbox_load_options(files, threads, max_batch);
        let handle = self
            .pack
            .load(engine, &files.package, precision.as_str(), device, &opts)?;
        let li = handle.info()?;
        info!(
            "{engine} {} on {device} (libtorch {}, batch {}, {} bound weights) loaded in {:.1}s from {}",
            li.precision,
            li.torch,
            li.batch,
            li.weights,
            li.load_seconds,
            files.package.display()
        );
        // As for the recognizers: the package must have bound to the pack's libtorch.
        let foreign = self.pack.foreign_libraries();
        if !foreign.is_empty() {
            return Err(format!(
                "libtorch libraries from outside the pack were loaded ({}); remove them from LD_LIBRARY_PATH / LD_PRELOAD",
                foreign.join(", ")
            ));
        }
        let r = Arc::new(TorchTextbox::new(self.pack.clone(), handle, li));
        map.retain(|_, w| w.strong_count() > 0);
        map.insert(key, Arc::downgrade(&r));
        Ok(r)
    }
}

/// The formats a recognizer runs in on catalog device `id`: the device's formats for
/// which `has_package(precision, targets)` holds (a compiled package for one of the
/// device's targets is on disk or downloadable). A GPU pack without a usable GPU still
/// lists the CPU, so the CPU packages keep the engines offered (0.5.2's CUDA image fell
/// back to CPU torch the same way).
pub fn device_formats(
    report: &DevicesReport,
    id: &str,
    has_package: impl Fn(&str, &[String]) -> bool,
) -> Vec<&'static str> {
    let Some(dev) = report.devices.iter().find(|d| d.id == id) else {
        return Vec::new();
    };
    ["fp32", "bf16", "fp16"]
        .into_iter()
        .filter(|p| dev.formats.iter().any(|f| f == p))
        .filter(|p| {
            let targets = bunko_ocr::models::torch_targets(&dev.kind, &dev.arch, &dev.isa, p);
            has_package(p, &targets)
        })
        .collect()
}

/// The formats device `id` runs for any recognizer engine here: the union over
/// hayai-nova and paddle-manga of [`device_formats`], in `fp32, bf16, fp16` order.
pub fn runnable_formats(
    report: &DevicesReport,
    id: &str,
    has_package: impl Fn(&str, &str, &[String]) -> bool,
) -> Vec<String> {
    engine_formats(
        report,
        id,
        &[crate::models::HAYAI, crate::models::PADDLE],
        has_package,
    )
    .0
}

/// The formats device `id` runs for any of `engines` (`has_package(engine, precision,
/// targets)`, as [`device_formats`]): their union in `fp32, bf16, fp16` order, and, for
/// each format that not every one of `engines` runs there, the engines that do (the
/// catalog's `format_engines`: a nar-small package without hayai-nova's, hayai-nova's
/// bf16). The map is built for GPUs only: the CPU's would only mislead, since the
/// server counts fp32 as the CPU's for every engine.
pub fn engine_formats(
    report: &DevicesReport,
    id: &str,
    engines: &[&str],
    has_package: impl Fn(&str, &str, &[String]) -> bool,
) -> (Vec<String>, std::collections::BTreeMap<String, Vec<String>>) {
    let per: Vec<(&str, Vec<&'static str>)> = engines
        .iter()
        .map(|e| (*e, device_formats(report, id, |p, t| has_package(e, p, t))))
        .collect();
    let all: Vec<String> = ["fp32", "bf16", "fp16"]
        .into_iter()
        .filter(|f| per.iter().any(|(_, fs)| fs.contains(f)))
        .map(str::to_string)
        .collect();
    let mut only = std::collections::BTreeMap::new();
    let gpu = report.devices.iter().any(|d| d.id == id && d.kind != "cpu");
    if gpu {
        for f in &all {
            let serving: Vec<String> = per
                .iter()
                .filter(|(_, fs)| fs.contains(&f.as_str()))
                .map(|(e, _)| e.to_string())
                .collect();
            if serving.len() < engines.len() {
                only.insert(f.clone(), serving);
            }
        }
    }
    (all, only)
}

/// What the session and the sidecar know of a torch recognizer.
pub fn recognizer_info(
    engine: &'static str,
    precision: Precision,
    device: bunko_vlm::Device,
    batch: usize,
    budget: u32,
) -> RecognizerInfo {
    use bunko_vlm::{hayai, paddle};
    if engine == crate::models::PADDLE {
        RecognizerInfo {
            engine: "paddle-manga",
            recognizer: paddle::LORA_REPO,
            repos: vec![
                (paddle::BASE_REPO, paddle::BASE_REVISION),
                (paddle::LORA_REPO, paddle::LORA_REVISION),
            ],
            precision,
            device,
            second_read: true,
            token_caps: true,
            default_max_tokens: paddle::DEFAULT_MAX_NEW_TOKENS,
            batch,
            patch_budget: None,
        }
    } else {
        RecognizerInfo {
            engine: "hayai-nova",
            recognizer: hayai::REPO,
            repos: vec![
                (hayai::REPO, hayai::REVISION),
                (hayai::VISION_REPO, hayai::VISION_REVISION),
            ],
            precision,
            device,
            second_read: false,
            token_caps: false,
            default_max_tokens: hayai::MAX_NEW_TOKENS as u32,
            batch,
            patch_budget: Some(budget),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusals of the packs before 0.8.0 (their exact texts) ask for a pack update;
    /// anything else, the new packs' own budget check included, is left alone.
    #[test]
    fn an_old_packs_budget_refusal_asks_for_an_update() {
        for old in [
            "hayai-nova packages are compiled for patch budget 512, not 256",
            "hayai-nova's ncnn graphs read patch budget 512, not 384",
            "could not load hayai-nova: hayai-nova packages are compiled for patch budget 512, not 256",
        ] {
            let e = pack_budget_error(old, 256).unwrap_or_else(|| panic!("{old}"));
            assert!(e.contains(PACK_UPDATE), "{e}");
            assert!(e.contains("not 256"), "{e}");
        }
        for other in [
            "hayai-nova's graphs read patch budgets 1 to 512, not 1024",
            "download failed: sha256 x != y",
            "",
        ] {
            assert_eq!(pack_budget_error(other, 256), None, "{other}");
        }
    }

    /// A pack installed while the process runs: after [`forget_failure`] the next call
    /// looks again (a different answer: this "pack" is found, then fails to open).
    #[test]
    fn a_failure_is_forgotten_on_request() {
        if std::env::var_os(PACK_ENV).is_some() || backend_if_open().is_some() {
            return; // a real backend in this process: nothing to forget
        }
        let dir = tempfile::tempdir().unwrap();
        let dirs = vec![dir.path().to_path_buf()];
        let first = backend_in(&dirs, true).err().unwrap_or_default();
        if !first.contains("no libtorch backend pack") {
            return; // another test decided this process's backend first
        }
        assert_eq!(backend_in(&dirs, true).err(), Some(first.clone()), "kept");
        std::fs::create_dir_all(dir.path().join("torch-cpu-2.13.0")).unwrap();
        std::fs::write(dir.path().join("torch-cpu-2.13.0").join(PACK_JSON), "{}").unwrap();
        assert_eq!(
            backend_in(&dirs, true).err(),
            Some(first.clone()),
            "still kept"
        );
        assert!(forget_failure());
        let second = backend_in(&dirs, true).err().unwrap_or_default();
        assert_ne!(second, first, "looked again");
        assert!(forget_failure());
    }

    #[test]
    fn text_box_load_options_carry_the_weights_and_the_vocabulary() {
        let f = TorchFiles {
            package: "/p/model".into(),
            tokenizer: "/v.json".into(),
            pos_table: None,
            embeddings: None,
            unpack_cache: Some("/u".into()),
            weights: vec!["/w.safetensors".into()],
        };
        let o = textbox_load_options(&f, 4, Some(16));
        assert_eq!(o.tokenizer, "/v.json");
        assert_eq!(o.weights, Some(vec!["/w.safetensors".to_string()]));
        assert_eq!(
            (o.threads, o.max_batch, o.patch_budget),
            (4, Some(16), None)
        );
        assert_eq!(o.cache_dir.as_deref(), Some("/u"));
        assert_eq!(textbox_load_options(&f, 4, None).max_batch, None);
        // Nothing of the recognizers' options.
        assert_eq!(
            (o.pos_table, o.embeddings, o.backend, o.vision),
            (None, None, None, None)
        );
    }

    #[test]
    fn a_small_gpu_caps_the_text_box_batch() {
        assert_eq!(textbox_max_batch(Some(3072)), Some(8));
        assert_eq!(textbox_max_batch(Some(4095)), Some(8));
        assert_eq!(textbox_max_batch(Some(4096)), None);
        assert_eq!(textbox_max_batch(Some(24564)), None);
        assert_eq!(textbox_max_batch(None), None);
    }

    fn entry(id: &str, kind: &str, arch: &str, isa: &[&str], formats: &[&str]) -> DeviceEntry {
        DeviceEntry {
            id: id.into(),
            kind: kind.into(),
            name: id.into(),
            arch: arch.into(),
            isa: isa.iter().map(|s| s.to_string()).collect(),
            vram_mb: None,
            pci: None,
            formats: formats.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn report(devices: Vec<DeviceEntry>) -> DevicesReport {
        DevicesReport {
            abi: 1,
            torch: "2.13.0+cu130".into(),
            gpu_backend: Some("cuda".into()),
            devices,
            warnings: vec!["cuda libtorch is loaded but sees no GPU".into()],
        }
    }

    fn os(t: &str) -> String {
        format!("{}-{t}", std::env::consts::OS)
    }

    #[test]
    fn gpu_pack_without_a_gpu_offers_the_cpu_packages() {
        let cpu = entry("cpu", "cpu", "x86_64", &["avx2", "fma"], &["fp32"]);
        let r = report(vec![cpu]);
        // only the CPU fp32 package is here
        let has = |p: &str, t: &[String]| p == "fp32" && t.contains(&os("cpu-x86_64-v3"));
        assert_eq!(device_formats(&r, "cpu", has), vec!["fp32"]);
        assert!(device_formats(&r, "gpu:0", has).is_empty());
        // nothing compiled for this CPU: no format, the engine is not offered
        assert!(device_formats(&r, "cpu", |_, _| false).is_empty());
    }

    #[test]
    fn the_catalog_lists_runnable_formats_only() {
        // Turing: computes bf16, but packages exist for fp32/fp16 only (sm_75).
        let r = report(vec![
            entry("cpu", "cpu", "x86_64", &["avx2", "fma"], &["fp32"]),
            entry("gpu:0", "cuda", "sm_75", &[], &["fp32", "fp16", "bf16"]),
        ]);
        let has = |engine: &str, p: &str, t: &[String]| {
            (engine == "hayai-nova" && p != "bf16" && t.contains(&os("cuda-sm_75")))
                || (engine == "paddle-manga" && p == "fp32" && t.contains(&os("cuda-sm_75")))
                || (p == "fp32" && t.contains(&os("cpu-x86_64-v3")))
        };
        assert_eq!(runnable_formats(&r, "gpu:0", has), vec!["fp32", "fp16"]);
        assert_eq!(runnable_formats(&r, "cpu", has), vec!["fp32"]);
        assert!(runnable_formats(&r, "gpu:0", |_, _, _| false).is_empty());
    }

    #[test]
    fn a_gpu_without_its_package_falls_back_to_the_cpu() {
        let r = report(vec![
            entry("cpu", "cpu", "x86_64", &["avx2", "fma"], &["fp32"]),
            entry("gpu:0", "cuda", "sm_89", &[], &["fp32", "fp16", "bf16"]),
        ]);
        let cpu_only = |p: &str, t: &[String]| p == "fp32" && t.contains(&os("cpu-x86_64-v3"));
        let devices: Vec<bunko_proto::Device> = r
            .devices
            .iter()
            .map(|d| bunko_proto::Device {
                format_engines: Default::default(),
                id: d.id.clone(),
                label: d.name.clone(),
                formats: d.formats.clone(),
                provider: Some(d.kind.clone()),
                arch: Some(d.arch.clone()),
            })
            .collect();
        let fmts = |pl: &crate::runtime::Placement| device_formats(&r, &pl.device, cpu_only);
        let (p, why) = crate::runtime::place_runnable(None, &devices, fmts);
        assert_eq!(p, crate::runtime::Placement::cpu());
        assert!(why.unwrap().contains("gpu:0"));
        // with an sm_86 package the sm_89 card runs it (PTX), in the formats it has
        let older = |p: &str, t: &[String]| p != "bf16" && t.contains(&os("cuda-sm_86"));
        let fmts = |pl: &crate::runtime::Placement| device_formats(&r, &pl.device, older);
        let (p, why) = crate::runtime::place_runnable(Some("auto"), &devices, fmts);
        assert_eq!((p.device.as_str(), why), ("gpu:0", None));
        assert_eq!(device_formats(&r, "gpu:0", older), vec!["fp32", "fp16"]);
    }

    #[test]
    fn format_engines_name_the_engines_a_format_serves() {
        let r = report(vec![entry(
            "gpu:0",
            "rocm",
            "gfx1201",
            &[],
            &["fp32", "bf16", "fp16"],
        )]);
        let (f, only) = engine_formats(&r, "gpu:0", &["hayai-nova", "nar-small"], |e, p, _| {
            (e == "hayai-nova" && (p == "fp32" || p == "bf16")) || (e == "nar-small" && p == "fp32")
        });
        assert_eq!(f, ["fp32", "bf16"]);
        assert_eq!(only.get("bf16"), Some(&vec!["hayai-nova".to_string()]));
        assert!(!only.contains_key("fp32"));
        let (f, only) = engine_formats(&r, "gpu:0", &["hayai-nova", "nar-small"], |e, p, _| {
            e == "nar-small" && p == "fp32"
        });
        assert_eq!(f, ["fp32"]);
        assert_eq!(only.get("fp32"), Some(&vec!["nar-small".to_string()]));
        // runnable_formats is the union over hayai-nova and paddle-manga, as before
        assert_eq!(
            runnable_formats(&r, "gpu:0", |e, p, _| e == "paddle-manga" && p == "fp16"),
            ["fp16"]
        );
    }

    #[test]
    fn format_engines_are_for_gpus_only() {
        let r = report(vec![
            entry("cpu", "cpu", "x86_64", &["avx2"], &["fp32"]),
            entry("gpu:0", "rocm", "gfx1201", &[], &["fp32", "bf16", "fp16"]),
        ]);
        // hayai has a CPU package, nar-small none: the CPU still lists fp32, with no map
        let (f, only) = engine_formats(&r, "cpu", &["hayai-nova", "nar-small"], |e, p, _| {
            e == "hayai-nova" && p == "fp32"
        });
        assert_eq!(f, ["fp32"]);
        assert!(only.is_empty());
        // the closure of a pack without bt_read_chars says no for nar: hayai alone serves fp32
        let reads_chars = false;
        let (f, only) = engine_formats(&r, "gpu:0", &["hayai-nova", "nar-small"], |e, p, _| {
            if e == "nar-small" {
                crate::textbox_gpu_formats("nar-small", reads_chars, &["fp32"], true).contains(&p)
            } else {
                p == "fp32"
            }
        });
        assert_eq!(f, ["fp32"]);
        assert_eq!(only.get("fp32"), Some(&vec!["hayai-nova".to_string()]));
        // a device the report does not have: nothing
        let (f, only) = engine_formats(&r, "gpu:7", &["hayai-nova"], |_, _, _| true);
        assert!(f.is_empty() && only.is_empty());
    }

    /// Regression: during an automatic update the NEW release lists the packs of the old
    /// one. Their release-checked manifest read failed, so they were classified by
    /// directory name: `vulkan-0.7.4` slipped past the Vulkan filter, and with every
    /// pack ranked "anything else" it sorted first by name. The update then staged the
    /// Vulkan pack instead of the libtorch one and failed on hayai-nova. A pack's
    /// variant comes from its manifest whatever its release; opening it checks that.
    #[test]
    fn another_releases_packs_are_classified_by_their_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |name: &str, variant: &str| {
            let d = tmp.path().join(name);
            std::fs::create_dir_all(&d).unwrap();
            let m = serde_json::json!({
                "format": 1, "variant": variant, "torch": "2.13.0", "abi": 1,
                "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
                "library": bunko_torch::abi::library_file_name(), "files": [],
                "bunko_version": "0.0.1"
            });
            std::fs::write(d.join(PACK_JSON), m.to_string()).unwrap();
        };
        mk("torch-cu130-2.13.0", "cu130");
        mk("torch-cpu-2.13.0", "cpu");
        mk("vulkan-0.0.1", crate::vulkan::VARIANT);
        let names: Vec<String> = discover(tmp.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // The Vulkan pack is never a libtorch candidate, and cu130 ranks as a GPU pack:
        // first where its driver is, after cpu where it is not.
        let want = if vendor_present("cu130") {
            ["torch-cu130-2.13.0", "torch-cpu-2.13.0"]
        } else {
            ["torch-cpu-2.13.0", "torch-cu130-2.13.0"]
        };
        assert_eq!(names, want);
    }

    #[test]
    fn discovery_order() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |name: &str, variant: &str| {
            let d = tmp.path().join(name);
            std::fs::create_dir_all(&d).unwrap();
            let m = serde_json::json!({
                "format": 1, "variant": variant, "torch": "2.13.0", "abi": 1,
                "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
                "library": bunko_torch::abi::library_file_name(), "files": []
            });
            std::fs::write(d.join(PACK_JSON), m.to_string()).unwrap();
        };
        mk("torch-cpu-2.13.0", "cpu");
        mk("torch-zz-2.13.0", "zz");
        std::fs::create_dir_all(tmp.path().join("not-a-pack")).unwrap();
        let found = discover(tmp.path());
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["torch-cpu-2.13.0", "torch-zz-2.13.0"]);
        assert!(discover(&tmp.path().join("missing")).is_empty());

        // Several directories: the first one's packs first, a repeated dir once.
        let other = tempfile::tempdir().unwrap();
        let d = other.path().join("torch-cu130-2.13.0");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join(PACK_JSON),
            serde_json::json!({"variant": "cu130"}).to_string(),
        )
        .unwrap();
        let all = discover_all(&[
            other.path().to_path_buf(),
            tmp.path().to_path_buf(),
            other.path().to_path_buf(),
            tmp.path().join("missing"),
        ]);
        let names: Vec<String> = all
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["torch-cu130-2.13.0", "torch-cpu-2.13.0", "torch-zz-2.13.0"]
        );
    }
}
