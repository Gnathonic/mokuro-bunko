//! ONNX Runtime set-up and what this machine offers: version and providers (for
//! `doctor`), the device catalog filtered by the configured backend, the host block
//! of a registration, and where a session's recognizer runs.

use std::ffi::CStr;

use bunko_proto::{Device, HostInfo};
use bunko_vlm::Provider;

/// `ocr.backend` (local) or the processor's choice: which execution providers may
/// be used. `Auto` takes the first compiled-in GPU provider that has a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Auto,
    Cpu,
    Cuda,
    WebGpu,
    DirectMl,
    CoreMl,
}

impl Backend {
    /// `auto|cpu|cuda|rocm|webgpu|directml|coreml` (`rocm` is 0.5's AMD spelling, which
    /// is WebGPU in 0.7). `skip` never reaches here (no local processor is started).
    pub fn parse(s: &str) -> Backend {
        match s.trim().to_ascii_lowercase().as_str() {
            "cpu" => Backend::Cpu,
            "cuda" => Backend::Cuda,
            "webgpu" | "rocm" => Backend::WebGpu,
            "directml" | "dml" => Backend::DirectMl,
            "coreml" => Backend::CoreMl,
            _ => Backend::Auto,
        }
    }

    /// The provider name this backend is limited to (`None`: any; `Some("cpu")`: none).
    fn only(self) -> Option<&'static str> {
        match self {
            Backend::Auto => None,
            Backend::Cpu => Some("cpu"),
            Backend::Cuda => Some("cuda"),
            Backend::WebGpu => Some("webgpu"),
            Backend::DirectMl => Some("directml"),
            Backend::CoreMl => Some("coreml"),
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
        .filter(|d| {
            d.id == "cpu"
                || match backend.only() {
                    None => true,
                    Some(p) => d.provider.as_deref() == Some(p),
                }
        })
        .map(|d| Device {
            id: d.id,
            label: d.label,
            formats: d.formats,
            provider: d.provider,
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

/// The `host` block of a registration.
pub fn host_info(devices: &[Device]) -> HostInfo {
    let gpu = devices.iter().find(|d| d.id != "cpu");
    let version = env!("CARGO_PKG_VERSION");
    HostInfo {
        cpu: cpu_name(),
        gpu: gpu.map(|d| d.label.clone()),
        backend: gpu
            .and_then(|d| d.provider.clone())
            .unwrap_or_else(|| "cpu".into()),
        version: version.into(),
        runner_build: format!("mokuro-bunko {version} (onnxruntime {})", ort_version()),
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
    pub vlm_device: bunko_vlm::Device,
    pub provider: Provider,
}

impl Placement {
    pub fn cpu() -> Placement {
        Placement {
            device: "cpu".into(),
            vlm_device: bunko_vlm::Device::Cpu,
            provider: Provider::Cpu,
        }
    }

    pub fn is_gpu(&self) -> bool {
        self.device != "cpu"
    }
}

fn provider_of(name: &str) -> Option<Provider> {
    match name {
        "cuda" => Some(Provider::Cuda),
        "webgpu" => Some(Provider::WebGpu),
        "directml" => Some(Provider::DirectMl),
        "coreml" => Some(Provider::CoreMl),
        _ => None,
    }
}

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
    let Some(provider) = dev.provider.as_deref().and_then(provider_of) else {
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
            provider,
        },
        None,
    )
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
        assert_eq!(p.provider, Provider::Cuda);
        let (p, _) = place(Some("gpu:2"), &gpus);
        assert_eq!(p.provider, Provider::WebGpu);
        assert_eq!(p.vlm_device, bunko_vlm::Device::Gpu(1));
        assert_eq!(place(Some("cpu"), &gpus).0, Placement::cpu());
        assert_eq!(Backend::parse("ROCm"), Backend::WebGpu);
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
        let host = host_info(&devices);
        assert_eq!(host.backend, "cpu");
        assert!(host.runner_build.contains("onnxruntime 1."));
    }
}
