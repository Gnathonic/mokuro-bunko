//! libtorch plumbing: AOTInductor packages through the C++ shim, precisions, devices,
//! host↔device tensor helpers.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;

use tch::{Device as TDev, Kind, Tensor};
use torch_sys::C_tensor;

use crate::TorchError;

unsafe extern "C" {
    fn bt_aoti_load(path: *const c_char, device: c_int, err: *mut *mut c_char) -> *mut c_void;
    fn bt_aoti_drop(h: *mut c_void);
    fn bt_aoti_run(
        h: *mut c_void,
        ins: *const *mut C_tensor,
        n_in: c_int,
        outs: *mut *mut C_tensor,
        max_out: c_int,
        err: *mut *mut c_char,
    ) -> c_int;
    fn bt_aoti_free_err(e: *mut c_char);
    fn bt_aoti_metadata(h: *mut c_void, key: *const c_char) -> *mut c_char;
    fn bt_aoti_constant_fqns(h: *mut c_void, err: *mut *mut c_char) -> *mut c_char;
    fn bt_aoti_load_constants(
        h: *mut c_void,
        names: *const *const c_char,
        tensors: *const *mut C_tensor,
        n: c_int,
        err: *mut *mut c_char,
    ) -> c_int;
}

fn take_err(e: *mut c_char) -> String {
    if e.is_null() {
        return "unknown error".into();
    }
    // SAFETY: a NUL-terminated string the shim malloc'ed for us; freed right after.
    let s = unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned();
    unsafe { bt_aoti_free_err(e) };
    s
}

/// Most outputs a graph has (paddle step: logits + 2 x 18 KV).
const MAX_OUTPUTS: usize = 64;

/// One AOTInductor package.
pub struct Aoti {
    h: *mut c_void,
    name: String,
}

// SAFETY: `AOTIModelPackageLoader::run` is called only under the owning recognizer's
// device lock; the handle itself is an immutable pointer.
unsafe impl Send for Aoti {}
unsafe impl Sync for Aoti {}

impl Drop for Aoti {
    fn drop(&mut self) {
        // SAFETY: `h` came from `bt_aoti_load` and is dropped once.
        unsafe { bt_aoti_drop(self.h) }
    }
}

impl Aoti {
    /// `path`: a `.pt2` zip or the zip unpacked into a directory (loaded in place).
    /// `device`: the libtorch device index, -1 for the CPU.
    pub fn load(path: &Path, device: i32) -> Result<Self, TorchError> {
        if !path.exists() {
            return Err(TorchError::Load(format!("{} is missing", path.display())));
        }
        let c = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| TorchError::Arg(format!("bad path {}", path.display())))?;
        let mut err = std::ptr::null_mut();
        // SAFETY: `c` outlives the call; `err` is an out pointer.
        let h = unsafe { bt_aoti_load(c.as_ptr(), device, &mut err) };
        if h.is_null() {
            let msg = take_err(err);
            let msg = crate::abi::explain_load_error(&msg).unwrap_or(msg);
            return Err(TorchError::Load(format!("{}: {msg}", path.display())));
        }
        #[cfg(windows)]
        pin_msvc_openmp();
        Ok(Self {
            h,
            name: path.display().to_string(),
        })
    }

    /// A package metadata value (`bunko.io`, `bunko.weights`, ...).
    pub fn metadata(&self, key: &str) -> Option<String> {
        let k = CString::new(key).ok()?;
        // SAFETY: live handle, NUL-terminated key; the result is ours to free.
        let p = unsafe { bt_aoti_metadata(self.h, k.as_ptr()) };
        (!p.is_null()).then(|| take_err(p))
    }

    /// The names of the package's constants (weights).
    pub fn constant_fqns(&self) -> Result<Vec<String>, TorchError> {
        let mut err = std::ptr::null_mut();
        // SAFETY: live handle, out pointer.
        let p = unsafe { bt_aoti_constant_fqns(self.h, &mut err) };
        if p.is_null() {
            return Err(TorchError::Load(format!(
                "{}: {}",
                self.name,
                take_err(err)
            )));
        }
        let s = take_err(p);
        Ok(s.split('\n')
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Binds every constant to a caller-owned tensor (`user_managed`: no copy, the
    /// tensors must outlive the package).
    pub fn bind(&self, constants: &[(String, &Tensor)]) -> Result<(), TorchError> {
        let names: Vec<CString> = constants
            .iter()
            .map(|(n, _)| CString::new(n.as_str()))
            .collect::<Result<_, _>>()
            .map_err(|_| TorchError::Load("constant name with a NUL byte".into()))?;
        let name_ptrs: Vec<*const c_char> = names.iter().map(|c| c.as_ptr()).collect();
        let tensors: Vec<*mut C_tensor> = constants
            .iter()
            .map(|(_, t)| t.as_ptr() as *mut C_tensor)
            .collect();
        let mut err = std::ptr::null_mut();
        // SAFETY: equal-length arrays that outlive the call; the shim copies the
        // tensor handles (reference-counted), not the data.
        let r = unsafe {
            bt_aoti_load_constants(
                self.h,
                name_ptrs.as_ptr(),
                tensors.as_ptr(),
                constants.len() as c_int,
                &mut err,
            )
        };
        if r != 0 {
            return Err(TorchError::Load(format!(
                "{}: binding weights: {}",
                self.name,
                take_err(err)
            )));
        }
        Ok(())
    }

    /// Runs the graph. Inputs must be contiguous (the package bakes the example strides
    /// in); outputs are made contiguous for the same reason.
    pub fn run(&self, ins: &[&Tensor]) -> Result<Vec<Tensor>, TorchError> {
        let ptrs: Vec<*mut C_tensor> = ins.iter().map(|t| t.as_ptr() as *mut C_tensor).collect();
        let mut outs: Vec<*mut C_tensor> = vec![std::ptr::null_mut(); MAX_OUTPUTS];
        let mut err = std::ptr::null_mut();
        // SAFETY: the tensors outlive the call; `outs` has MAX_OUTPUTS slots.
        let n = unsafe {
            bt_aoti_run(
                self.h,
                ptrs.as_ptr(),
                ptrs.len() as c_int,
                outs.as_mut_ptr(),
                MAX_OUTPUTS as c_int,
                &mut err,
            )
        };
        if n < 0 {
            return Err(TorchError::Run(format!("{}: {}", self.name, take_err(err))));
        }
        // AOTI may hand outputs back in a non-dense layout (seen: paddle prefill KV
        // (10240,128,256,1)) while the next graph assumes the dense example strides.
        Ok(outs[..n as usize]
            .iter()
            // SAFETY: each slot is a heap `at::Tensor*` the shim gave us to own.
            .map(|&p| unsafe { Tensor::from_ptr(p) }.contiguous())
            .collect())
    }
}

/// Which precision a package set was compiled for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TPrec {
    Fp32,
    Bf16,
    Fp16,
}

impl TPrec {
    pub fn parse(s: &str) -> Result<Self, TorchError> {
        match s {
            "fp32" => Ok(Self::Fp32),
            "bf16" => Ok(Self::Bf16),
            "fp16" => Ok(Self::Fp16),
            _ => Err(TorchError::Arg(format!("unknown precision {s:?}"))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fp32 => "fp32",
            Self::Bf16 => "bf16",
            Self::Fp16 => "fp16",
        }
    }

    pub fn kind(self) -> Kind {
        match self {
            Self::Fp32 => Kind::Float,
            Self::Bf16 => Kind::BFloat16,
            Self::Fp16 => Kind::Half,
        }
    }

    /// `torch.finfo(dtype).min` (paddle's additive mask value).
    pub fn finfo_min(self) -> f32 {
        match self {
            Self::Fp32 => f32::MIN,
            Self::Bf16 => -3.389_531_4e38,
            Self::Fp16 => -65504.0,
        }
    }
}

/// Where a recognizer runs: libtorch's device and the AOTI device index (-1 = CPU).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub dev: TDev,
    pub index: i32,
}

impl Placement {
    /// `cpu` or `gpu:<n>` (n = libtorch's CUDA/HIP device index).
    pub fn parse(s: &str) -> Result<Self, TorchError> {
        let s = s.trim().to_ascii_lowercase();
        if s == "cpu" {
            return Ok(Self {
                dev: TDev::Cpu,
                index: -1,
            });
        }
        let n = s
            .strip_prefix("gpu:")
            .or_else(|| s.strip_prefix("cuda:"))
            .and_then(|n| n.parse::<usize>().ok())
            .ok_or_else(|| TorchError::Arg(format!("unknown device {s:?} (cpu, gpu:<n>)")))?;
        if !crate::gpu::loaded() {
            return Err(TorchError::Load(format!(
                "{s}: this backend has no GPU support loaded"
            )));
        }
        let count = tch::Cuda::device_count();
        if n as i64 >= count {
            return Err(TorchError::Arg(format!(
                "{s}: libtorch sees {count} GPU(s)"
            )));
        }
        Ok(Self {
            dev: TDev::Cuda(n),
            index: n as i32,
        })
    }

    pub fn label(&self) -> String {
        match self.dev {
            TDev::Cuda(n) => format!("gpu:{n}"),
            _ => "cpu".into(),
        }
    }
}

/// A host f32 slice as a tensor on `dev`.
pub fn dev_f32(v: &[f32], shape: &[i64], dev: TDev) -> Tensor {
    Tensor::from_slice(v).view(shape).to_device(dev)
}

/// A host i64 slice as a tensor on `dev`.
pub fn dev_i64(v: &[i64], shape: &[i64], dev: TDev) -> Tensor {
    Tensor::from_slice(v).view(shape).to_device(dev)
}

/// A (small) tensor's values on the host.
pub fn host_i64(t: &Tensor) -> Result<Vec<i64>, TorchError> {
    Vec::<i64>::try_from(&t.to_device(TDev::Cpu)).map_err(|e| TorchError::Run(e.to_string()))
}

/// Sets the OpenMP/ATen thread count for the calling thread's next parallel regions
/// (AOTInductor CPU kernels use `#pragma omp parallel`, whose width is a per-thread
/// OpenMP setting). 0 leaves libtorch's default.
pub fn set_threads(n: u32) {
    if n > 0 {
        tch::set_num_threads(n as i32);
    }
}

/// OpenMP runtimes a package may run its parallel loops on: libtorch's own (libgomp,
/// libomp, libiomp5) and, on Windows, MSVC's `vcomp140.dll` that MSVC-compiled CPU
/// packages import (libtorch's `set_num_threads` never reaches that one).
const OPENMP_RUNTIMES: &[&str] = if cfg!(windows) {
    &["vcomp140.dll", "libiomp5md.dll"]
} else if cfg!(target_os = "macos") {
    &["libomp.dylib", "libiomp5.dylib"]
} else {
    &["libgomp.so.1", "libiomp5.so"]
};

type OmpSetNumThreads = unsafe extern "C" fn(std::ffi::c_int);

/// `omp_set_num_threads` of every OpenMP runtime already loaded in the process.
fn loaded_omp_setters() -> Vec<OmpSetNumThreads> {
    let mut out = Vec::new();
    for &name in OPENMP_RUNTIMES {
        #[cfg(unix)]
        let lib = {
            // RTLD_NOLOAD: only a library that is already loaded.
            const RTLD_NOLOAD: std::ffi::c_int = if cfg!(target_os = "macos") { 0x10 } else { 0x4 };
            // SAFETY: no library is loaded (NOLOAD), so no initialiser runs.
            unsafe {
                libloading::os::unix::Library::open(
                    Some(name),
                    libloading::os::unix::RTLD_LAZY | RTLD_NOLOAD,
                )
            }
            .ok()
        };
        #[cfg(windows)]
        let lib = libloading::os::windows::Library::open_already_loaded(name).ok();
        let Some(lib) = lib else { continue };
        // SAFETY: the OpenMP API's `void omp_set_num_threads(int)`.
        if let Ok(f) = unsafe { lib.get::<OmpSetNumThreads>(b"omp_set_num_threads\0") } {
            out.push(*f);
        }
        // The runtime stays loaded (the package or libtorch holds it); dropping our
        // extra reference does not unload it.
    }
    out
}

/// Sets the calling thread's OpenMP width in every loaded OpenMP runtime (the setting
/// is per thread, and per runtime). Cheap: no libtorch cache is touched.
pub fn set_omp_threads(n: u32) {
    if n == 0 {
        return;
    }
    for f in loaded_omp_setters() {
        // SAFETY: a plain C call with an int.
        unsafe { f(n as std::ffi::c_int) };
    }
}

/// Windows CPU packages are compiled by MSVC with `/openmp`: their model DLL imports
/// MSVC's OpenMP runtime (`vcomp140.dll`), which nothing else in the process uses
/// (libtorch has its own, libiomp5md). When the last package is freed (a session
/// closes), Windows unloads vcomp140.dll while its worker threads are still parked in
/// it, and the process dies with an access violation (`VCOMP140.DLL_unloaded`,
/// 0xC0000005, measured on pimax after every CPU volume). Pin it once loaded: it then
/// stays mapped for the life of the process. A no-op for packages that do not use it.
#[cfg(windows)]
fn pin_msvc_openmp() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static PINNED: AtomicBool = AtomicBool::new(false);
    if PINNED.load(Ordering::Relaxed) {
        return;
    }
    const GET_MODULE_HANDLE_EX_FLAG_PIN: u32 = 0x1;
    unsafe extern "system" {
        fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut c_void) -> i32;
    }
    let name: Vec<u16> = "vcomp140.dll".encode_utf16().chain([0]).collect();
    let mut module = std::ptr::null_mut();
    // SAFETY: a NUL-terminated wide string and an out pointer, both live for the call;
    // pinning only adds a permanent reference to an already loaded module.
    if unsafe { GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN, name.as_ptr(), &mut module) } != 0
    {
        PINNED.store(true, Ordering::Relaxed);
    }
}
