//! Loading libtorch from the pack, and what the GPUs are.
//!
//! The linker drops `libtorch` and `libtorch_cuda` / `libtorch_hip` from the cdylib
//! (`--as-needed`: no symbol of ours references them), so [`init`] loads them by hand,
//! globally and by absolute path from the pack, before any AOTInductor package needs
//! them (the packages' model libraries name them by soname, without a run path). Device facts that libtorch's C API does not
//! give (name, architecture, memory) come from the vendor's stable C entry points: the
//! CUDA driver API (`libcuda` / `nvcuda.dll`), and the HIP runtime plus the KFD topology
//! in sysfs for the `gfx` target.

use std::ffi::{CStr, c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::abi::{
    DeviceEntry, core_libraries, foreign_libraries, gfx_from_override, gfx_from_target_version,
    umbrella_library,
};

/// `cuda` or `rocm`, once the GPU half is loaded.
static BACKEND: OnceLock<Option<&'static str>> = OnceLock::new();
/// Directory the GPU half was loaded from (to find the vendor runtime next to it).
static LIB_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Whether a GPU half of libtorch is loaded.
pub fn loaded() -> bool {
    backend().is_some()
}

/// `cuda` / `rocm`, or none.
pub fn backend() -> Option<&'static str> {
    BACKEND.get().copied().flatten()
}

/// Opens a shared library with its symbols global (the AOTI model `.so` files resolve
/// `aoti_torch_cuda_*` against it) and keeps it loaded for the life of the process.
fn open_global(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use libloading::os::unix::{Library, RTLD_GLOBAL, RTLD_NOW};
        // SAFETY: loading libtorch's own GPU library; its initialisers are libtorch's.
        let lib = unsafe { Library::open(Some(path), RTLD_NOW | RTLD_GLOBAL) }.map_err(|e| {
            let m = crate::abi::error_chain(&e);
            crate::abi::explain_load_error(&m).unwrap_or(m)
        })?;
        std::mem::forget(lib);
        Ok(())
    }
    #[cfg(windows)]
    {
        use libloading::os::windows::{LOAD_WITH_ALTERED_SEARCH_PATH, Library};
        // SAFETY: as above; the altered search path finds the DLL's own dependencies
        // (cudnn, cublas, ...) next to it.
        let lib = unsafe { Library::load_with_flags(path, LOAD_WITH_ALTERED_SEARCH_PATH) }
            .map_err(|e| crate::abi::error_chain(&e))?;
        std::mem::forget(lib);
        Ok(())
    }
}

/// Candidate GPU halves: (backend, file name).
fn gpu_halves() -> &'static [(&'static str, &'static str)] {
    if cfg!(target_os = "windows") {
        &[("cuda", "torch_cuda.dll")]
    } else if cfg!(target_os = "macos") {
        &[]
    } else {
        &[("cuda", "libtorch_cuda.so"), ("rocm", "libtorch_hip.so")]
    }
}

/// Problems that do not stop the CPU from working, for the devices report.
static NOTES: OnceLock<Vec<String>> = OnceLock::new();

pub fn notes() -> &'static [String] {
    NOTES.get().map_or(&[], Vec::as_slice)
}

/// Loads, once: the pack's CPU half, the GPU half (unless `cpu_only`), the umbrella
/// library, all from `lib_dir` by absolute path (by name when there is no `lib_dir`).
/// Returns why there is no GPU half.
pub fn init(lib_dir: Option<&Path>, cpu_only: bool) -> Result<(), String> {
    let mut why = String::new();
    let got = BACKEND.get_or_init(|| {
        let _ = LIB_DIR.set(lib_dir.map(Path::to_path_buf));
        let mut notes = Vec::new();
        if let Some(d) = lib_dir {
            for f in core_libraries() {
                let p = d.join(f);
                if p.exists()
                    && let Err(e) = open_global(&p)
                {
                    notes.push(format!("{}: {e}", p.display()));
                }
            }
        }
        let mut backend = None;
        if cpu_only {
            why = "CPU only (asked)".into();
        } else {
            for &(b, file) in gpu_halves() {
                let path = match lib_dir {
                    Some(d) => d.join(file),
                    None => PathBuf::from(file),
                };
                if lib_dir.is_some() && !path.exists() {
                    continue;
                }
                match open_global(&path) {
                    Ok(()) => {
                        backend = Some(b);
                        break;
                    }
                    Err(e) => why.push_str(&format!("{}: {e}; ", path.display())),
                }
            }
            // A pack without GPU libraries (the CPU variant, macOS) expects no GPU:
            // nothing to report.
        }
        let umbrella = match lib_dir {
            Some(d) => d.join(umbrella_library()),
            None => PathBuf::from(umbrella_library()),
        };
        if (lib_dir.is_none() || umbrella.exists())
            && let Err(e) = open_global(&umbrella)
        {
            notes.push(format!(
                "{}: {e} (the compiled models cannot load without it)",
                umbrella.display()
            ));
        }
        notes.extend(
            foreign_libraries(lib_dir)
                .into_iter()
                .map(|p| format!("a libtorch library from outside the pack is loaded: {p}")),
        );
        let _ = NOTES.set(notes);
        backend
    });
    match got {
        Some(_) => Ok(()),
        None if cpu_only => Ok(()),
        None if why.is_empty() => Ok(()),
        None => Err(why.trim_end_matches("; ").to_string()),
    }
}

fn vendor_lib(names: &[&str]) -> Option<libloading::Library> {
    let dir = LIB_DIR.get().cloned().flatten();
    for n in names {
        if let Some(d) = &dir {
            let p = d.join(n);
            if p.exists() {
                // SAFETY: the vendor runtime libtorch itself loads.
                if let Ok(l) = unsafe { libloading::Library::new(&p) } {
                    return Some(l);
                }
            }
        }
        // SAFETY: as above, by name through the system's search path.
        if let Ok(l) = unsafe { libloading::Library::new(*n) } {
            return Some(l);
        }
    }
    None
}

fn cstr(buf: &[c_char]) -> String {
    // SAFETY: the APIs NUL-terminate within the buffer we sized; the buffer ends in 0.
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .trim()
        .to_string()
}

/// Facts of each libtorch GPU, in libtorch's order.
pub fn probe(count: usize) -> (Vec<DeviceEntry>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut out: Vec<DeviceEntry> = (0..count)
        .map(|i| DeviceEntry {
            id: format!("gpu:{i}"),
            kind: backend().unwrap_or("gpu").into(),
            name: format!("GPU {i}"),
            arch: String::new(),
            isa: Vec::new(),
            vram_mb: None,
            pci: None,
            formats: vec!["fp32".into(), "fp16".into(), "bf16".into()],
        })
        .collect();
    let r = match backend() {
        Some("cuda") => probe_cuda(&mut out),
        Some("rocm") => probe_hip(&mut out),
        _ => Ok(()),
    };
    if let Err(e) = r {
        warnings.push(format!("GPU details unavailable: {e}"));
    }
    (out, warnings)
}

type CuInit = unsafe extern "C" fn(c_int) -> c_int;
type CuCount = unsafe extern "C" fn(*mut c_int) -> c_int;
type CuGet = unsafe extern "C" fn(*mut c_int, c_int) -> c_int;
type CuName = unsafe extern "C" fn(*mut c_char, c_int, c_int) -> c_int;
type CuMem = unsafe extern "C" fn(*mut usize, c_int) -> c_int;
type CuAttr = unsafe extern "C" fn(*mut c_int, c_int, c_int) -> c_int;
type CuPci = unsafe extern "C" fn(*mut c_char, c_int, c_int) -> c_int;

const CU_ATTR_CC_MAJOR: c_int = 75;
const CU_ATTR_CC_MINOR: c_int = 76;

fn probe_cuda(out: &mut [DeviceEntry]) -> Result<(), String> {
    let lib = vendor_lib(&["libcuda.so.1", "libcuda.so", "nvcuda.dll"])
        .ok_or("no CUDA driver library")?;
    // SAFETY: the CUDA driver API's documented C signatures (CUdevice is an int).
    unsafe {
        let init: libloading::Symbol<CuInit> = lib.get(b"cuInit\0").map_err(|e| e.to_string())?;
        let count: libloading::Symbol<CuCount> =
            lib.get(b"cuDeviceGetCount\0").map_err(|e| e.to_string())?;
        let get: libloading::Symbol<CuGet> =
            lib.get(b"cuDeviceGet\0").map_err(|e| e.to_string())?;
        let name: libloading::Symbol<CuName> =
            lib.get(b"cuDeviceGetName\0").map_err(|e| e.to_string())?;
        let mem: libloading::Symbol<CuMem> = lib
            .get(b"cuDeviceTotalMem_v2\0")
            .map_err(|e| e.to_string())?;
        let attr: libloading::Symbol<CuAttr> = lib
            .get(b"cuDeviceGetAttribute\0")
            .map_err(|e| e.to_string())?;
        let pci: libloading::Symbol<CuPci> = lib
            .get(b"cuDeviceGetPCIBusId\0")
            .map_err(|e| e.to_string())?;
        if init(0) != 0 {
            return Err("cuInit failed".into());
        }
        let mut n = 0;
        count(&mut n);
        for (i, d) in out.iter_mut().enumerate().take(n.max(0) as usize) {
            let mut dev = 0;
            if get(&mut dev, i as c_int) != 0 {
                continue;
            }
            let mut buf = [0 as c_char; 256];
            if name(buf.as_mut_ptr(), 255, dev) == 0 {
                d.name = cstr(&buf);
            }
            let mut bytes = 0usize;
            if mem(&mut bytes, dev) == 0 {
                d.vram_mb = Some(bytes as u64 / (1024 * 1024));
            }
            let (mut ma, mut mi) = (0, 0);
            if attr(&mut ma, CU_ATTR_CC_MAJOR, dev) == 0
                && attr(&mut mi, CU_ATTR_CC_MINOR, dev) == 0
            {
                d.arch = format!("sm_{ma}{mi}");
            }
            let mut buf = [0 as c_char; 64];
            if pci(buf.as_mut_ptr(), 63, dev) == 0 {
                d.pci = Some(cstr(&buf).to_ascii_lowercase());
            }
        }
    }
    Ok(())
}

type HipCount = unsafe extern "C" fn(*mut c_int) -> c_int;
type HipName = unsafe extern "C" fn(*mut c_char, c_int, c_int) -> c_int;
type HipMem = unsafe extern "C" fn(*mut usize, c_int) -> c_int;
type HipPci = unsafe extern "C" fn(*mut c_char, c_int, c_int) -> c_int;

fn probe_hip(out: &mut [DeviceEntry]) -> Result<(), String> {
    let lib = vendor_lib(&["libamdhip64.so", "libamdhip64.so.7", "libamdhip64.so.6"])
        .ok_or("no HIP runtime library")?;
    let override_arch = std::env::var("HSA_OVERRIDE_GFX_VERSION")
        .ok()
        .and_then(|v| gfx_from_override(&v));
    // SAFETY: the HIP runtime's documented C signatures.
    unsafe {
        let count: libloading::Symbol<HipCount> =
            lib.get(b"hipGetDeviceCount\0").map_err(|e| e.to_string())?;
        let name: libloading::Symbol<HipName> =
            lib.get(b"hipDeviceGetName\0").map_err(|e| e.to_string())?;
        let mem: libloading::Symbol<HipMem> =
            lib.get(b"hipDeviceTotalMem\0").map_err(|e| e.to_string())?;
        let pci: libloading::Symbol<HipPci> = lib
            .get(b"hipDeviceGetPCIBusId\0")
            .map_err(|e| e.to_string())?;
        let mut n = 0;
        count(&mut n);
        for (i, d) in out.iter_mut().enumerate().take(n.max(0) as usize) {
            let i = i as c_int;
            let mut buf = [0 as c_char; 256];
            if name(buf.as_mut_ptr(), 255, i) == 0 {
                d.name = cstr(&buf);
            }
            let mut bytes = 0usize;
            if mem(&mut bytes, i) == 0 {
                d.vram_mb = Some(bytes as u64 / (1024 * 1024));
            }
            let mut buf = [0 as c_char; 64];
            if pci(buf.as_mut_ptr(), 63, i) == 0 {
                let id = cstr(&buf).to_ascii_lowercase();
                if let Some(arch) = kfd_gfx_for_pci(&id) {
                    d.arch = arch;
                }
                d.pci = Some(id);
            }
            if let Some(a) = &override_arch {
                d.arch = a.clone();
            }
        }
    }
    Ok(())
}

/// PCI id `0000:03:00.0` → (domain, KFD `location_id` = bus<<8 | dev<<3 | fn).
pub fn pci_location(id: &str) -> Option<(u32, u32)> {
    let (dom, rest) = id.split_once(':')?;
    let (bus, rest) = rest.split_once(':')?;
    let (dev, func) = rest.split_once('.')?;
    let h = |s: &str| u32::from_str_radix(s, 16).ok();
    Some((h(dom)?, (h(bus)? << 8) | (h(dev)? << 3) | h(func)?))
}

fn kfd_gfx_for_pci(id: &str) -> Option<String> {
    let (domain, location) = pci_location(id)?;
    let nodes = std::fs::read_dir("/sys/class/kfd/kfd/topology/nodes").ok()?;
    for node in nodes.flatten() {
        let Ok(props) = std::fs::read_to_string(node.path().join("properties")) else {
            continue;
        };
        let field = |k: &str| {
            props.lines().find_map(|l| {
                let (key, v) = l.split_once(' ')?;
                (key == k).then(|| v.trim().parse::<u32>().ok()).flatten()
            })
        };
        if field("location_id") == Some(location) && field("domain").unwrap_or(0) == domain {
            return field("gfx_target_version").and_then(gfx_from_target_version);
        }
    }
    None
}

/// The CPU entry: model name, architecture, the ISA features that decide which compiled
/// CPU package runs, and the formats (bf16 only where the CPU computes it natively).
pub fn cpu_entry() -> DeviceEntry {
    let isa = cpu_isa();
    let mut formats = vec!["fp32".to_string()];
    // The bf16 CPU packages are x86-64-v4 + AVX512_BF16 code (tools/torch_export's
    // `cpu-x86_64-v4bf16`); without those the CPU computes in fp32 only, as 0.5.2's.
    let has = |f: &str| isa.iter().any(|i| i == f);
    if ["avx512f", "avx512bw", "avx512vl", "avx512dq", "avx512_bf16"]
        .iter()
        .all(|f| has(f))
    {
        formats.push("bf16".into());
    }
    DeviceEntry {
        id: "cpu".into(),
        kind: "cpu".into(),
        name: cpu_name(),
        arch: std::env::consts::ARCH.into(),
        isa,
        vram_mb: None,
        pci: None,
        formats,
    }
}

fn cpu_name() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(info) = std::fs::read_to_string("/proc/cpuinfo")
        && let Some(line) = info.lines().find(|l| l.starts_with("model name"))
        && let Some((_, v)) = line.split_once(':')
    {
        return v.trim().to_string();
    }
    // macOS: the brand string the kernel reports ("Apple M2 Pro").
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        && out.status.success()
    {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    // Windows (and Intel Macs): the processor brand string from CPUID, as /proc/cpuinfo
    // shows it on Linux.
    #[cfg(all(not(target_os = "linux"), target_arch = "x86_64"))]
    if let Some(brand) = cpuid_brand() {
        return brand;
    }
    format!("{} CPU", std::env::consts::ARCH)
}

/// CPUID leaves 0x80000002-4: the 48-byte processor brand string.
#[cfg(all(not(target_os = "linux"), target_arch = "x86_64"))]
fn cpuid_brand() -> Option<String> {
    use std::arch::x86_64::__cpuid;
    // SAFETY: CPUID exists on every x86_64 CPU; leaf 0x80000000 reports the highest
    // extended leaf, checked before the brand leaves are read.
    if unsafe { __cpuid(0x8000_0000) }.eax < 0x8000_0004 {
        return None;
    }
    let mut bytes = Vec::with_capacity(48);
    for leaf in 0x8000_0002u32..=0x8000_0004 {
        // SAFETY: as above.
        let r = unsafe { __cpuid(leaf) };
        for w in [r.eax, r.ebx, r.ecx, r.edx] {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
    }
    let s = String::from_utf8_lossy(&bytes);
    let s = s.trim_matches(char::from(0)).trim();
    (!s.is_empty()).then(|| s.split_whitespace().collect::<Vec<_>>().join(" "))
}

#[allow(clippy::vec_init_then_push)]
fn cpu_isa() -> Vec<String> {
    #[allow(unused_mut)]
    let mut out: Vec<String> = Vec::new();
    #[cfg(target_arch = "x86_64")]
    {
        for (name, on) in [
            ("avx2", std::arch::is_x86_feature_detected!("avx2")),
            ("fma", std::arch::is_x86_feature_detected!("fma")),
            ("f16c", std::arch::is_x86_feature_detected!("f16c")),
            ("bmi2", std::arch::is_x86_feature_detected!("bmi2")),
            ("avx512f", std::arch::is_x86_feature_detected!("avx512f")),
            ("avx512bw", std::arch::is_x86_feature_detected!("avx512bw")),
            ("avx512vl", std::arch::is_x86_feature_detected!("avx512vl")),
            ("avx512dq", std::arch::is_x86_feature_detected!("avx512dq")),
        ] {
            if on {
                out.push(name.into());
            }
        }
        // AVX512_BF16: CPUID.(EAX=7,ECX=1):EAX[5]; AMX-BF16: CPUID.(EAX=7,ECX=0):EDX[22].
        // Both also need the OS to save the AVX-512 / AMX state, which avx512f's
        // detection (XGETBV) covers for the first.
        // SAFETY: CPUID leaf 7 exists on every x86-64 CPU that has AVX2 (checked first).
        if out.iter().any(|f| f == "avx512f") {
            let l7_1 = unsafe { std::arch::x86_64::__cpuid_count(7, 1) };
            if l7_1.eax & (1 << 5) != 0 {
                out.push("avx512_bf16".into());
            }
            let l7_0 = unsafe { std::arch::x86_64::__cpuid_count(7, 0) };
            if l7_0.edx & (1 << 22) != 0 {
                out.push("amx_bf16".into());
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        out.push("neon".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gfx_names() {
        assert_eq!(gfx_from_target_version(120001).as_deref(), Some("gfx1201"));
        // (shared with abi.rs)
        assert_eq!(gfx_from_target_version(100300).as_deref(), Some("gfx1030"));
        assert_eq!(gfx_from_target_version(90010).as_deref(), Some("gfx90a"));
        assert_eq!(gfx_from_target_version(0), None);
        assert_eq!(gfx_from_override("10.3.0").as_deref(), Some("gfx1030"));
        assert_eq!(gfx_from_override("11.0.2").as_deref(), Some("gfx1102"));
        assert_eq!(gfx_from_override("junk"), None);
        assert_eq!(pci_location("0000:03:00.0"), Some((0, 768)));
        assert_eq!(
            pci_location("0001:0a:1f.7"),
            Some((1, (10 << 8) | (31 << 3) | 7))
        );
    }

    #[test]
    fn cpu_has_fp32() {
        let c = cpu_entry();
        assert_eq!(c.id, "cpu");
        assert_eq!(c.formats[0], "fp32");
    }
}
