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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use bunko_torch::abi::{DeviceEntry, DevicesReport, LoadOptions, PACK_JSON};
use bunko_vlm::{Precision, RecognizerInfo};
use tracing::{info, warn};

pub use bunko_torch::abi;
pub use loader::{Pack, PackLoadError};
pub use recognizer::TorchRecognizer;

/// Use this pack directory instead of searching `<storage>/backends/`.
pub const PACK_ENV: &str = "MOKURO_TORCH_PACK";
/// CPU threads of a CPU recognizer (default: the session's share of the cores).
pub const THREADS_ENV: &str = "MOKURO_TORCH_THREADS";

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
            let variant = loader::read_manifest(&p)
                .map(|m| m.variant)
                .unwrap_or_else(|_| name.clone());
            Some((rank(&variant), name, p))
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
                    for (k, v) in &pack.env {
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
    let mut out: Vec<&str> = Vec::new();
    for engine in [crate::models::HAYAI, crate::models::PADDLE] {
        for f in device_formats(report, id, |p, t| has_package(engine, p, t)) {
            if !out.contains(&f) {
                out.push(f);
            }
        }
    }
    ["fp32", "bf16", "fp16"]
        .into_iter()
        .filter(|f| out.contains(f))
        .map(str::to_string)
        .collect()
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
