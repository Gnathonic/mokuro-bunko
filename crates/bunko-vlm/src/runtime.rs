//! ONNX Runtime sessions, behind a small seam so `bunko-ocr`'s shared runtime module
//! can replace this one later ([`SessionFactory`]).
//!
//! ## Concurrency
//!
//! ARCHITECTURE §3 wants N engine threads sharing one session per (model, device,
//! precision). `ort` 2.0.0-rc.13 makes `Session::run` take `&mut self` (its author does
//! not trust ONNX Runtime's documented thread-safe `Run` on every EP), which would
//! serialise those threads. [`SharedSession::run`] instead calls the C API's `Run`
//! through a shared reference for the EPs whose concurrent `Run` ONNX Runtime documents
//! and the 0.5/0.6 Python spike exercised (CPU, CUDA); every other EP is serialised by a
//! mutex. [`Sharing`] lets a caller force either policy.

use std::ffi::CString;
use std::fmt;
use std::path::Path;
use std::ptr::{self, NonNull};
use std::str::FromStr;
use std::sync::Mutex;

use ort::AsPointer;
use ort::memory::{AllocationDevice, Allocator, AllocatorType, MemoryInfo, MemoryType};
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::DynValue;

use crate::VlmError;

/// Where a model runs: `cpu` or `gpu:<n>` (spec §7.3 device ids; `gpu`/`cuda` = `gpu:0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Device {
    Cpu,
    Gpu(u8),
}

impl FromStr for Device {
    type Err = VlmError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "cpu" => return Ok(Device::Cpu),
            "gpu" | "cuda" => return Ok(Device::Gpu(0)),
            _ => {}
        }
        let n = s.strip_prefix("gpu:").or_else(|| s.strip_prefix("cuda:"));
        match n.and_then(|n| n.parse::<u8>().ok()) {
            Some(n) if n <= 15 => Ok(Device::Gpu(n)),
            _ => Err(VlmError::Config(format!(
                "unknown device {s:?} (cpu, gpu:<n>)"
            ))),
        }
    }
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Device::Cpu => f.write_str("cpu"),
            Device::Gpu(n) => write!(f, "gpu:{n}"),
        }
    }
}

/// ONNX Runtime execution provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Cpu,
    Cuda,
    WebGpu,
    DirectMl,
    CoreMl,
}

impl Provider {
    /// The provider a device id means on this build: the CPU EP for `cpu`; for a GPU the
    /// first compiled-in GPU EP (CUDA, then DirectML, CoreML, WebGPU).
    pub fn for_device(device: Device) -> Result<Provider, VlmError> {
        match device {
            Device::Cpu => Ok(Provider::Cpu),
            Device::Gpu(_) => {
                if cfg!(feature = "cuda") {
                    Ok(Provider::Cuda)
                } else if cfg!(feature = "directml") {
                    Ok(Provider::DirectMl)
                } else if cfg!(feature = "coreml") {
                    Ok(Provider::CoreMl)
                } else if cfg!(feature = "webgpu") {
                    Ok(Provider::WebGpu)
                } else {
                    Err(VlmError::Config(
                        "this build has no GPU execution provider".into(),
                    ))
                }
            }
        }
    }

    /// Whether ONNX Runtime's concurrent `Run` on one session is relied on for this EP.
    fn concurrent_run(self) -> bool {
        matches!(self, Provider::Cpu | Provider::Cuda)
    }
}

/// How concurrent callers share one session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Sharing {
    /// Concurrent `Run` where the EP supports it, else serialised.
    #[default]
    Auto,
    /// Always one `Run` at a time.
    Serial,
}

/// Session construction knobs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionOptions {
    pub device: Device,
    pub provider: Provider,
    /// ORT intra-op threads; 0 = ONNX Runtime's default (one per physical core).
    pub intra_threads: usize,
    /// Let idle intra-op threads spin (ORT default true). Off is kinder when several
    /// caller threads share the pool.
    pub spinning: bool,
    pub sharing: Sharing,
}

impl SessionOptions {
    pub fn new(device: Device) -> Result<Self, VlmError> {
        Ok(Self {
            device,
            provider: Provider::for_device(device)?,
            intra_threads: 0,
            spinning: true,
            sharing: Sharing::Auto,
        })
    }

    pub fn cpu(intra_threads: usize) -> Self {
        Self {
            device: Device::Cpu,
            provider: Provider::Cpu,
            intra_threads,
            spinning: true,
            sharing: Sharing::Auto,
        }
    }
}

/// Builds sessions. The seam `bunko-ocr`'s runtime module will implement.
pub trait SessionFactory: Send + Sync {
    fn open(&self, model: &Path, opts: &SessionOptions) -> Result<SharedSession, VlmError>;
}

/// The default factory: `ort` with the downloaded ONNX Runtime build.
#[derive(Clone, Copy, Debug, Default)]
pub struct OrtSessionFactory;

fn ort_err<E: fmt::Display>(what: &str) -> impl FnOnce(E) -> VlmError + '_ {
    move |e| VlmError::Runtime(format!("{what}: {e}"))
}

impl SessionFactory for OrtSessionFactory {
    fn open(&self, model: &Path, opts: &SessionOptions) -> Result<SharedSession, VlmError> {
        let what = format!("loading {}", model.display());
        let mut b = Session::builder()
            .map_err(ort_err(&what))?
            .with_optimization_level(GraphOptimizationLevel::All)
            .map_err(ort_err(&what))?
            .with_inter_threads(1)
            .map_err(ort_err(&what))?;
        if opts.intra_threads > 0 {
            b = b
                .with_intra_threads(opts.intra_threads)
                .map_err(ort_err(&what))?;
        }
        if !opts.spinning {
            b = b.with_intra_op_spinning(false).map_err(ort_err(&what))?;
        }
        b = register_provider(b, opts).map_err(ort_err(&what))?;
        let session = b.commit_from_file(model).map_err(ort_err(&what))?;
        SharedSession::new(session, opts)
    }
}

#[allow(unused_variables)]
fn register_provider(
    b: ort::session::builder::SessionBuilder,
    opts: &SessionOptions,
) -> Result<ort::session::builder::SessionBuilder, String> {
    let id = match opts.device {
        Device::Cpu => 0,
        Device::Gpu(n) => i32::from(n),
    };
    let ep: Option<ort::ep::ExecutionProviderDispatch> = match opts.provider {
        Provider::Cpu => None,
        #[cfg(feature = "cuda")]
        Provider::Cuda => Some(
            ort::ep::CUDA::default()
                .with_device_id(id)
                .build()
                .error_on_failure(),
        ),
        #[cfg(feature = "webgpu")]
        Provider::WebGpu => Some(
            ort::ep::WebGPU::default()
                .with_device_id(id)
                .build()
                .error_on_failure(),
        ),
        #[cfg(feature = "directml")]
        Provider::DirectMl => Some(
            ort::ep::DirectML::default()
                .with_device_id(id)
                .build()
                .error_on_failure(),
        ),
        #[cfg(feature = "coreml")]
        Provider::CoreMl => Some(ort::ep::CoreML::default().build().error_on_failure()),
        #[allow(unreachable_patterns)]
        other => {
            return Err(format!(
                "{other:?} execution provider is not compiled into this build"
            ));
        }
    };
    match ep {
        None => Ok(b),
        Some(ep) => b.with_execution_providers([ep]).map_err(|e| e.to_string()),
    }
}

/// One loaded graph that any number of threads may run.
pub struct SharedSession {
    session: Session,
    inputs: Vec<CString>,
    outputs: Vec<CString>,
    serial: Option<Mutex<()>>,
    device: Device,
    provider: Provider,
}

impl fmt::Debug for SharedSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedSession")
            .field("inputs", &self.inputs)
            .field("outputs", &self.outputs)
            .field("serial", &self.serial.is_some())
            .field("device", &self.device)
            .finish()
    }
}

impl SharedSession {
    /// Wraps an already built session (for factories other than [`OrtSessionFactory`]).
    pub fn new(session: Session, opts: &SessionOptions) -> Result<Self, VlmError> {
        let cs = |n: &str| {
            CString::new(n).map_err(|_| VlmError::Runtime(format!("bad tensor name {n:?}")))
        };
        let inputs = session
            .inputs()
            .iter()
            .map(|o| cs(o.name()))
            .collect::<Result<_, _>>()?;
        let outputs = session
            .outputs()
            .iter()
            .map(|o| cs(o.name()))
            .collect::<Result<_, _>>()?;
        let concurrent = opts.sharing == Sharing::Auto && opts.provider.concurrent_run();
        Ok(Self {
            session,
            inputs,
            outputs,
            serial: (!concurrent).then(|| Mutex::new(())),
            device: opts.device,
            provider: opts.provider,
        })
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn device(&self) -> Device {
        self.device
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    pub fn input_names(&self) -> impl Iterator<Item = &str> {
        self.inputs.iter().map(|c| c.to_str().unwrap_or(""))
    }

    pub fn output_names(&self) -> impl Iterator<Item = &str> {
        self.outputs.iter().map(|c| c.to_str().unwrap_or(""))
    }

    /// Element type of input `i`.
    pub fn input_type(&self, i: usize) -> Option<ort::value::TensorElementType> {
        self.session
            .inputs()
            .get(i)
            .and_then(|o| o.dtype().tensor_type())
    }

    /// Element type of output `i`.
    pub fn output_type(&self, i: usize) -> Option<ort::value::TensorElementType> {
        self.session
            .outputs()
            .get(i)
            .and_then(|o| o.dtype().tensor_type())
    }

    /// An allocator for device memory of this session's EP, when outputs should stay on
    /// the device (KV caches). `None` on the CPU.
    pub fn device_allocator(&self) -> Result<Option<Allocator>, VlmError> {
        let (dev, id) = match (self.provider, self.device) {
            (Provider::Cuda, Device::Gpu(n)) => (AllocationDevice::CUDA, i32::from(n)),
            _ => return Ok(None),
        };
        let info = MemoryInfo::new(dev, id, AllocatorType::Device, MemoryType::Default)
            .map_err(ort_err("memory info"))?;
        Allocator::new(&self.session, info)
            .map(Some)
            .map_err(ort_err("device allocator"))
    }

    /// Runs the graph with every input, in graph order. `prealloc[i]`, when given, is the
    /// value output `i` is written into (it must have the output's shape); other outputs
    /// are allocated by ONNX Runtime in CPU memory. Returns all outputs in graph order.
    pub fn run(
        &self,
        inputs: &[&DynValue],
        mut prealloc: Vec<Option<DynValue>>,
    ) -> Result<Vec<DynValue>, VlmError> {
        if inputs.len() != self.inputs.len() {
            return Err(VlmError::Runtime(format!(
                "{} inputs given, graph takes {}",
                inputs.len(),
                self.inputs.len()
            )));
        }
        prealloc.resize_with(self.outputs.len(), || None);
        let in_names: Vec<*const std::ffi::c_char> =
            self.inputs.iter().map(|c| c.as_ptr()).collect();
        let out_names: Vec<*const std::ffi::c_char> =
            self.outputs.iter().map(|c| c.as_ptr()).collect();
        let in_vals: Vec<*const ort::sys::OrtValue> = inputs.iter().map(|v| v.ptr()).collect();
        let mut out_vals: Vec<*mut ort::sys::OrtValue> = prealloc
            .iter_mut()
            .map(|v| v.as_mut().map_or(ptr::null_mut(), |v| v.ptr_mut()))
            .collect();
        let _guard = self
            .serial
            .as_ref()
            .map(|m| m.lock().unwrap_or_else(|p| p.into_inner()));
        // SAFETY: names and values outlive the call; `out_vals` has one slot per output,
        // pre-filled with owned values or null. Concurrency: see the module docs.
        let status = unsafe {
            (ort::api().Run)(
                self.session.ptr().cast_mut(),
                ptr::null(),
                in_names.as_ptr(),
                in_vals.as_ptr(),
                in_vals.len(),
                out_names.as_ptr(),
                out_names.len(),
                out_vals.as_mut_ptr(),
            )
        };
        // SAFETY: `status` comes straight from the C API.
        unsafe { ort::Error::result_from_status(status) }.map_err(ort_err("run"))?;
        let mut out = Vec::with_capacity(out_vals.len());
        for (slot, p) in prealloc.into_iter().zip(out_vals) {
            match slot {
                Some(v) => out.push(v),
                None => {
                    let p = NonNull::new(p)
                        .ok_or_else(|| VlmError::Runtime("run returned a null output".into()))?;
                    // SAFETY: an output ORT allocated for us; we take ownership, keeping
                    // the session alive as long as the value.
                    out.push(unsafe { DynValue::from_ptr(p, Some(self.session.inner())) });
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids() {
        assert_eq!("cpu".parse::<Device>().unwrap(), Device::Cpu);
        assert_eq!("GPU".parse::<Device>().unwrap(), Device::Gpu(0));
        assert_eq!("gpu:3".parse::<Device>().unwrap(), Device::Gpu(3));
        assert_eq!("cuda:1".parse::<Device>().unwrap(), Device::Gpu(1));
        assert!("gpu:16".parse::<Device>().is_err());
        assert!("tpu".parse::<Device>().is_err());
        assert_eq!(Device::Gpu(2).to_string(), "gpu:2");
    }
}
