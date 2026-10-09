//! ONNX Runtime set-up and what this machine offers: version and providers (for
//! `doctor`), the device catalog filtered by the configured backend, the host block
//! of a registration, and where a session's recognizer runs.
//!
//! The device catalog comes from the libtorch backend pack when one is loaded (feature
//! `torch`: CUDA / ROCm GPUs as libtorch numbers them), else from ONNX Runtime's
//! compiled-in execution providers (the CPU only in a release build).

use std::ffi::CStr;

use bunko_proto::{Device, HostInfo};

/// `ocr.backend` (local) or the processor's choice: which devices may be used. `Auto`
/// takes the first GPU there is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Auto,
    Cpu,
    Cuda,
    /// AMD GPUs through libtorch's ROCm build (0.5.2's `rocm`).
    Rocm,
    WebGpu,
    DirectMl,
    CoreMl,
}

impl Backend {
    /// `auto|cpu|cuda|rocm|webgpu|directml|coreml`. `skip` never reaches here (no local
    /// processor is started).
    pub fn parse(s: &str) -> Backend {
        match s.trim().to_ascii_lowercase().as_str() {
            "cpu" => Backend::Cpu,
            "cuda" => Backend::Cuda,
            "rocm" | "hip" => Backend::Rocm,
            "webgpu" => Backend::WebGpu,
            "directml" | "dml" => Backend::DirectMl,
            "coreml" => Backend::CoreMl,
            _ => Backend::Auto,
        }
    }

    /// The provider name this backend is limited to (`None`: any; `Some("cpu")`: none).
    pub fn only(self) -> Option<&'static str> {
        match self {
            Backend::Auto => None,
            Backend::Cpu => Some("cpu"),
            Backend::Cuda => Some("cuda"),
            Backend::Rocm => Some("rocm"),
            Backend::WebGpu => Some("webgpu"),
            Backend::DirectMl => Some("directml"),
            Backend::CoreMl => Some("coreml"),
        }
    }

    /// Whether a device of `provider` (`cpu`, `cuda`, `rocm`, ...) may be used.
    pub fn allows(self, provider: &str) -> bool {
        provider == "cpu"
            || match self.only() {
                None => true,
                Some(p) => p == provider,
            }
    }
}

/// Hand the C heap's free pages back to the OS.
///
/// The PP-OCR sessions run without ONNX Runtime's CPU arena (see
/// `bunko_ocr::runtime`), so their tensors come from the C allocator, and glibc keeps
/// freed chunks below its adaptive (up to 32 MB) mmap threshold in its per-thread
/// arenas: hundreds of MB after a volume. `malloc_trim` returns those pages
/// (`madvise(DONTNEED)` on the free ranges) in a few milliseconds. No-op off glibc.
pub fn trim_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        // SAFETY: glibc's `malloc_trim` takes no pointers and locks each arena itself.
        unsafe {
            malloc_trim(0);
        }
    }
}

/// Create the ORT environment once (idempotent). The binary calls it at start-up so
/// its options apply; engines call it too in case it did not.
pub fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = ort::init().with_name("mokuro-bunko").commit();
        // ORT's info-level chatter (arena growth, ...) is not worth formatting.
        if let Ok(env) = ort::environment::Environment::current() {
            env.set_log_level(ort::logging::LogLevel::Warning);
        }
    });
}

/// The linked ONNX Runtime's version string (e.g. `1.28.0`).
pub fn ort_version() -> String {
    // SAFETY: `OrtGetApiBase` is ONNX Runtime's C entry point (statically linked by
    // `ort`'s download-binaries); the returned struct and string are static.
    unsafe {
        let base = ort::sys::OrtGetApiBase();
        if base.is_null() {
            return "unknown".into();
        }
        let s = ((*base).GetVersionString)();
        if s.is_null() {
            return "unknown".into();
        }
        CStr::from_ptr(s).to_string_lossy().into_owned()
    }
}

/// Execution providers the linked ONNX Runtime reports (`CPUExecutionProvider`, ...).
pub fn ort_providers() -> Vec<String> {
    init();
    let api = ort::api();
    let mut list: *mut *mut std::ffi::c_char = std::ptr::null_mut();
    let mut n: std::ffi::c_int = 0;
    // SAFETY: the C API fills `list` with `n` NUL-terminated strings that we release
    // with the matching call below.
    unsafe {
        let status = (api.GetAvailableProviders)(&mut list, &mut n);
        if ort::Error::result_from_status(status).is_err() || list.is_null() {
            return Vec::new();
        }
        let out = (0..n as isize)
            .map(|i| {
                CStr::from_ptr(*list.offset(i))
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let _ = ort::Error::result_from_status((api.ReleaseAvailableProviders)(list, n));
        out
    }
}

/// The devices this machine reports for `backend`: the CPU, plus the GPUs of the
/// providers the backend allows.
pub fn devices(backend: Backend) -> Vec<Device> {
    init();
    bunko_ocr::runtime::device_catalog()
        .into_iter()
        .filter(|d| d.id == "cpu" || backend.allows(d.provider.as_deref().unwrap_or("")))
        .map(|d| Device {
            id: d.id,
            label: d.label,
            formats: d.formats,
            provider: d.provider,
            arch: None,
        })
        .collect()
}

fn cpu_name() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(info) = std::fs::read_to_string("/proc/cpuinfo")
        && let Some(line) = info.lines().find(|l| l.starts_with("model name"))
        && let Some((_, v)) = line.split_once(':')
    {
        return v.trim().to_string();
    }
    std::env::consts::ARCH.to_string()
}

/// 0.5.2's `bench.cpu_label`: the CPU's name with its physical core count,
/// `"AMD Ryzen 7 5800X 8-Core Processor (8 cores)"` (the server reads the count back).
pub fn cpu_label(name: &str, cores: usize) -> String {
    if cores == 0 {
        return name.to_string();
    }
    format!("{name} ({cores} core{})", if cores == 1 { "" } else { "s" })
}

/// The `host` block of a registration. `torch`: the loaded libtorch's version; `cpu`:
/// the CPU's name as the backend found it (else `/proc/cpuinfo`).
pub fn host_info(devices: &[Device], torch: Option<&str>, cpu: Option<&str>) -> HostInfo {
    let gpu = devices.iter().find(|d| d.id != "cpu");
    let version = env!("CARGO_PKG_VERSION");
    let mut runtimes = format!("onnxruntime {}", ort_version());
    if let Some(t) = torch {
        runtimes.push_str(&format!(", libtorch {t}"));
    }
    HostInfo {
        cpu: cpu_label(
            &cpu.map_or_else(cpu_name, str::to_string),
            crate::plan::physical_cpu_count(),
        ),
        gpu: gpu.map(|d| d.label.clone()),
        backend: gpu
            .and_then(|d| d.provider.clone())
            .unwrap_or_else(|| "cpu".into()),
        version: version.into(),
        runner_build: format!("mokuro-bunko {version} ({runtimes})"),
        os: Some(std::env::consts::OS.into()),
        cores: std::thread::available_parallelism()
            .ok()
            .map(|n| n.get() as u32),
    }
}

/// Where the recognizer of a session runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// `cpu` or `gpu:<n>` (reported in `ready.stage_device`).
    pub device: String,
    /// The device as the recognizer backend numbers it (per provider).
    pub vlm_device: bunko_vlm::Device,
    /// `cpu`, `cuda`, `rocm`, `webgpu`, `directml`, `coreml`.
    pub provider: String,
}

impl Placement {
    pub fn cpu() -> Placement {
        Placement {
            device: "cpu".into(),
            vlm_device: bunko_vlm::Device::Cpu,
            provider: "cpu".into(),
        }
    }

    pub fn is_gpu(&self) -> bool {
        self.device != "cpu"
    }

    /// The ONNX Runtime execution provider of this placement.
    #[cfg(feature = "onnx-vlm")]
    pub fn ort_provider(&self) -> Option<bunko_vlm::Provider> {
        use bunko_vlm::Provider;
        match self.provider.as_str() {
            "cpu" => Some(Provider::Cpu),
            "cuda" => Some(Provider::Cuda),
            "webgpu" => Some(Provider::WebGpu),
            "directml" => Some(Provider::DirectMl),
            "coreml" => Some(Provider::CoreMl),
            _ => None,
        }
    }
}

const GPU_PROVIDERS: [&str; 5] = ["cuda", "rocm", "webgpu", "directml", "coreml"];

/// Resolve `pools.stage_device.engine` (`auto`, `cpu`, `gpu:<n>`) against the devices:
/// `auto` = `gpu:0` when there is one, else the CPU. A GPU the catalog does not list
/// falls back to the CPU with a reason.
pub fn place(asked: Option<&str>, devices: &[Device]) -> (Placement, Option<String>) {
    let asked = asked.map(|s| s.trim().to_ascii_lowercase());
    let want = match asked.as_deref() {
        None | Some("") | Some("auto") => match devices.iter().find(|d| d.id != "cpu") {
            Some(d) => d.id.clone(),
            None => return (Placement::cpu(), None),
        },
        Some("cpu") => return (Placement::cpu(), None),
        Some("gpu") | Some("cuda") => "gpu:0".into(),
        Some(other) => other.replace("cuda:", "gpu:"),
    };
    let Some(dev) = devices.iter().find(|d| d.id == want) else {
        return (
            Placement::cpu(),
            Some(format!("{want} is not a device of this machine")),
        );
    };
    let Some(provider) = dev
        .provider
        .as_deref()
        .filter(|p| GPU_PROVIDERS.contains(p))
    else {
        return (
            Placement::cpu(),
            Some(format!("{want} has no usable execution provider")),
        );
    };
    // bunko-vlm numbers devices per provider; the catalog numbers GPUs across all of
    // them. Count this provider's devices before `want`.
    let ordinal = devices
        .iter()
        .filter(|d| d.id != "cpu" && d.provider == dev.provider)
        .position(|d| d.id == want)
        .unwrap_or(0) as u8;
    (
        Placement {
            device: want,
            vlm_device: bunko_vlm::Device::Gpu(ordinal),
            provider: provider.to_string(),
        },
        None,
    )
}

/// [`place`], then the CPU when the chosen GPU cannot run the engine in any format
/// (no compiled package for its architecture here): the engine keeps running, on the
/// CPU packages, with the reason logged.
pub fn place_runnable(
    asked: Option<&str>,
    devices: &[Device],
    formats: impl Fn(&Placement) -> Vec<&'static str>,
) -> (Placement, Option<String>) {
    let (p, why) = place(asked, devices);
    if p.is_gpu() && formats(&p).is_empty() {
        let label = devices
            .iter()
            .find(|d| d.id == p.device)
            .map_or_else(String::new, |d| format!(" ({})", d.label));
        return (
            Placement::cpu(),
            Some(format!(
                "{}{label} has no compiled package for this engine here",
                p.device
            )),
        );
    }
    (p, why)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, provider: &str) -> Device {
        Device {
            id: id.into(),
            label: id.into(),
            formats: vec!["fp32".into()],
            provider: Some(provider.into()),
            arch: None,
        }
    }

    #[test]
    fn placement() {
        let cpu_only = vec![dev("cpu", "cpu")];
        assert_eq!(place(None, &cpu_only).0, Placement::cpu());
        let (p, why) = place(Some("gpu:0"), &cpu_only);
        assert_eq!(p, Placement::cpu());
        assert!(why.unwrap().contains("gpu:0"));
        let gpus = vec![
            dev("cpu", "cpu"),
            dev("gpu:0", "cuda"),
            dev("gpu:1", "webgpu"),
            dev("gpu:2", "webgpu"),
        ];
        let (p, _) = place(Some("auto"), &gpus);
        assert_eq!(p.device, "gpu:0");
        assert_eq!(p.provider, "cuda");
        let (p, _) = place(Some("gpu:2"), &gpus);
        assert_eq!(p.provider, "webgpu");
        assert_eq!(p.vlm_device, bunko_vlm::Device::Gpu(1));
        assert_eq!(place(Some("cpu"), &gpus).0, Placement::cpu());
        // libtorch numbers its GPUs itself: gpu:<n> is torch device n.
        let rocm = vec![
            dev("cpu", "cpu"),
            dev("gpu:0", "rocm"),
            dev("gpu:1", "rocm"),
        ];
        let (p, _) = place(Some("gpu:1"), &rocm);
        assert_eq!(
            (p.provider.as_str(), p.vlm_device),
            ("rocm", bunko_vlm::Device::Gpu(1))
        );
        assert_eq!(Backend::parse("ROCm"), Backend::Rocm);
        assert!(Backend::Rocm.allows("rocm") && Backend::Rocm.allows("cpu"));
        assert!(!Backend::Cuda.allows("rocm"));
        assert_eq!(Backend::parse("whatever"), Backend::Auto);
    }

    #[test]
    fn runtime_probe() {
        init();
        let v = ort_version();
        assert!(v.starts_with("1."), "{v}");
        let providers = ort_providers();
        assert!(
            providers.iter().any(|p| p == "CPUExecutionProvider"),
            "{providers:?}"
        );
        let devices = devices(Backend::Cpu);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "cpu");
        let host = host_info(&devices, Some("2.13.0+cpu"), Some("Test CPU"));
        assert!(
            host.cpu.starts_with("Test CPU (") && host.cpu.ends_with(")"),
            "{}",
            host.cpu
        );
        assert_eq!(
            cpu_label("Threadripper 7960X", 24),
            "Threadripper 7960X (24 cores)"
        );
        assert_eq!(cpu_label("Atom", 1), "Atom (1 core)");
        assert_eq!(host.backend, "cpu");
        assert!(host.runner_build.contains("onnxruntime 1."));
        assert!(host.runner_build.ends_with(", libtorch 2.13.0+cpu)"));
    }
}
