//! ONNX Runtime plumbing: execution-provider choice with CPU fallback, shared
//! sessions, and the device catalog a processor reports.
//!
//! One [`Model`] is loaded per (model file, device) and shared by every worker
//! thread. `ort` 2.0 makes `Session::run` take `&mut self` (its authors do not
//! trust ORT's own thread-safety claim); [`Model::run_f32`] calls the C API's `Run`
//! through a shared reference instead, for the EPs whose concurrent `Run` ONNX
//! Runtime documents and bunko-vlm relies on too (CPU, CUDA). There a `Model` is one
//! session whatever `copies` says; other EPs get `copies` sessions behind locks.
//!
//! ## Memory (CPU sessions)
//!
//! A session's CPU memory arena grows to the largest working set it has seen
//! (rounded up to powers of two) and never shrinks, and the detector's input size
//! changes with every page, so each detector copy used to settle at hundreds of MB.
//! CPU sessions therefore run with the arena and the memory pattern off: tensors come
//! from the C allocator and are freed after each run. Measured on a 20-page volume
//! (ppocr-manga, 3 detect workers): peak RSS 1073 → ~580 MB, same output, same
//! pages/s. GPU sessions keep both (their arenas hold device memory).
//!
//! The library never creates the ORT environment (`ort::init()`); the binary does,
//! so its logging and global options apply.

mod devices;

pub use devices::{DeviceInfo, device_catalog, ep_compiled};

use std::ffi::CString;
use std::path::Path;
use std::ptr::{self, NonNull};
use std::str::FromStr;

use ort::AsPointer;
use ort::ep::{self, ExecutionProviderDispatch};
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::{DynValue, Tensor};
use parking_lot::Mutex;
use tracing::{info, warn};

use crate::error::{Error, Result};

/// Where a session runs. Parsed from `cpu`, `cuda[:n]`, `webgpu[:n]`,
/// `directml[:n]`, `coreml`, `nnapi`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionTarget {
    Cpu,
    Cuda(i32),
    WebGpu(i32),
    DirectMl(i32),
    CoreMl,
    Nnapi,
}

impl ExecutionTarget {
    /// The provider name used in reports (`cpu`, `cuda`, ...).
    pub fn provider(&self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Cuda(_) => "cuda",
            Self::WebGpu(_) => "webgpu",
            Self::DirectMl(_) => "directml",
            Self::CoreMl => "coreml",
            Self::Nnapi => "nnapi",
        }
    }

    /// Whether ONNX Runtime's documented thread-safe `Run` is relied on for this EP
    /// (CPU, CUDA: what bunko-vlm and the 0.5/0.6 spike run concurrently). Other EPs
    /// get one session per concurrent caller, each behind a lock.
    fn concurrent_run(&self) -> bool {
        matches!(self, Self::Cpu | Self::Cuda(_))
    }

    /// The provider registration, or `None` when this build's onnxruntime lacks it.
    fn dispatch(&self) -> Option<ExecutionProviderDispatch> {
        match *self {
            Self::Cpu => Some(ep::CPU::default().build().error_on_failure()),
            #[cfg(feature = "cuda")]
            Self::Cuda(n) => Some(
                ep::CUDA::default()
                    .with_device_id(n)
                    .build()
                    .error_on_failure(),
            ),
            #[cfg(feature = "webgpu")]
            Self::WebGpu(n) => Some(
                ep::WebGPU::default()
                    .with_device_id(n)
                    .build()
                    .error_on_failure(),
            ),
            #[cfg(feature = "directml")]
            Self::DirectMl(n) => Some(
                ep::DirectML::default()
                    .with_device_id(n)
                    .build()
                    .error_on_failure(),
            ),
            #[cfg(feature = "coreml")]
            Self::CoreMl => Some(ep::CoreML::default().build().error_on_failure()),
            #[cfg(feature = "nnapi")]
            Self::Nnapi => Some(ep::NNAPI::default().build().error_on_failure()),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}

impl std::fmt::Display for ExecutionTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cuda(n) | Self::WebGpu(n) | Self::DirectMl(n) => {
                write!(f, "{}:{n}", self.provider())
            }
            _ => f.write_str(self.provider()),
        }
    }
}

impl FromStr for ExecutionTarget {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim().to_ascii_lowercase();
        let (name, index) = match s.split_once(':') {
            Some((n, i)) => (
                n.to_string(),
                Some(
                    i.parse::<i32>()
                        .map_err(|_| Error::Invalid(format!("bad device index in '{s}'")))?,
                ),
            ),
            None => (s.clone(), None),
        };
        let idx = index.unwrap_or(0);
        Ok(match name.as_str() {
            "cpu" => Self::Cpu,
            "cuda" | "gpu" => Self::Cuda(idx),
            "webgpu" => Self::WebGpu(idx),
            "directml" | "dml" => Self::DirectMl(idx),
            "coreml" => Self::CoreMl,
            "nnapi" => Self::Nnapi,
            _ => return Err(Error::Invalid(format!("unknown execution provider '{s}'"))),
        })
    }
}

/// How sessions are built.
#[derive(Debug, Clone)]
pub struct RuntimeOptions {
    /// ORT intra-op threads per session (0 = ORT's default: one per core).
    pub intra_threads: usize,
    /// Preference order; the first that registers wins, CPU is always the last resort.
    pub targets: Vec<ExecutionTarget>,
    /// Concurrent runs per model: sessions for EPs whose concurrent `Run` is not
    /// relied on (one session serves any number of callers on the CPU and CUDA).
    pub copies: usize,
    /// Let idle ORT worker threads spin (ORT's default). Off on shared hosts: spinning
    /// burns CPU other work could use, for a few percent of latency.
    pub spinning: bool,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            intra_threads: 4,
            targets: vec![ExecutionTarget::Cpu],
            copies: 1,
            spinning: true,
        }
    }
}

/// A loaded model shared across threads.
pub struct Model {
    /// One session where concurrent `Run` is relied on; else `copies` sessions.
    sessions: Vec<Session>,
    /// One lock per session for EPs whose concurrent `Run` is not relied on.
    locks: Option<Vec<Mutex<()>>>,
    input_name: CString,
    output_name: CString,
    target: ExecutionTarget,
    fallback_reason: Option<String>,
}

impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model")
            .field("target", &self.target)
            .field("sessions", &self.sessions.len())
            .field("concurrent", &self.locks.is_none())
            .finish()
    }
}

fn build_session(path: &Path, opts: &RuntimeOptions, target: ExecutionTarget) -> Result<Session> {
    let threads = opts.intra_threads;
    let mut builder = Session::builder()?
        .with_optimization_level(GraphOptimizationLevel::All)?
        .with_log_level(ort::logging::LogLevel::Error)?;
    if threads > 0 {
        builder = builder.with_intra_threads(threads)?;
    }
    if !opts.spinning {
        builder = builder
            .with_intra_op_spinning(false)?
            .with_inter_op_spinning(false)?;
    }
    if target != ExecutionTarget::Cpu {
        let dispatch = target.dispatch().ok_or_else(|| {
            Error::Invalid(format!(
                "execution provider '{}' is not compiled into this build",
                target.provider()
            ))
        })?;
        builder = builder.with_execution_providers([dispatch])?;
    } else {
        // See "Memory" in the module docs.
        builder = builder
            .with_memory_pattern(false)?
            .with_execution_providers([ep::CPU::default()
                .with_arena_allocator(false)
                .build()
                .error_on_failure()])?;
    }
    Ok(builder.commit_from_file(path)?)
}

impl Model {
    /// Load `path` on the first target of `opts.targets` that works; CPU otherwise.
    pub fn load(path: &Path, opts: &RuntimeOptions) -> Result<Self> {
        let mut reasons: Vec<String> = Vec::new();
        let mut chosen: Option<(ExecutionTarget, Session)> = None;
        for &target in &opts.targets {
            match build_session(path, opts, target) {
                Ok(s) => {
                    chosen = Some((target, s));
                    break;
                }
                Err(e) => {
                    warn!(model = %path.display(), %target, error = %e, "execution provider unusable, trying the next");
                    reasons.push(format!("{target}: {e}"));
                }
            }
        }
        let (target, first) = match chosen {
            Some(c) => c,
            None => (
                ExecutionTarget::Cpu,
                build_session(path, opts, ExecutionTarget::Cpu)?,
            ),
        };
        let fallback_reason = (!reasons.is_empty()).then(|| reasons.join("; "));
        let name = |n: Option<&str>, what: &str| {
            let n =
                n.ok_or_else(|| Error::ModelOutput(format!("{} has no {what}", path.display())))?;
            CString::new(n).map_err(|_| Error::ModelOutput(format!("bad {what} name {n:?}")))
        };
        let input_name = name(first.inputs().first().map(|i| i.name()), "inputs")?;
        let output_name = name(first.outputs().first().map(|o| o.name()), "outputs")?;
        let concurrent = target.concurrent_run();
        let mut sessions = vec![first];
        if !concurrent {
            for _ in 1..opts.copies.max(1) {
                sessions.push(build_session(path, opts, target)?);
            }
        }
        let locks = (!concurrent).then(|| sessions.iter().map(|_| Mutex::new(())).collect());
        info!(model = %path.display(), %target, sessions = sessions.len(), concurrent, "model loaded");
        Ok(Self {
            sessions,
            locks,
            input_name,
            output_name,
            target,
            fallback_reason,
        })
    }

    /// The target the sessions run on.
    pub fn target(&self) -> ExecutionTarget {
        self.target
    }

    /// Why a preferred target was skipped, if one was.
    pub fn fallback_reason(&self) -> Option<&str> {
        self.fallback_reason.as_deref()
    }

    /// Run the model on one float32 input; returns the first output's shape and data.
    pub fn run_f32(&self, shape: &[usize], data: Vec<f32>) -> Result<(Vec<usize>, Vec<f32>)> {
        let dims: Vec<i64> = shape.iter().map(|&d| d as i64).collect();
        let tensor = Tensor::from_array((dims, data))?;
        // Concurrent EPs: the one session. Else a free session if there is one, else
        // wait for the first.
        let (session, _guard) = match &self.locks {
            None => (&self.sessions[0], None),
            Some(locks) => {
                let (i, guard) = locks
                    .iter()
                    .enumerate()
                    .find_map(|(i, l)| l.try_lock().map(|g| (i, g)))
                    .unwrap_or_else(|| (0, locks[0].lock()));
                (&self.sessions[i], Some(guard))
            }
        };
        let in_names = [self.input_name.as_ptr()];
        let in_vals = [tensor.ptr()];
        let out_names = [self.output_name.as_ptr()];
        let mut out_vals: [*mut ort::sys::OrtValue; 1] = [ptr::null_mut()];
        // SAFETY: the names and the input value outlive the call, and `out_vals` has
        // one (null) slot per requested output. `ort` 2.0 makes `Session::run` take
        // `&mut self`; ONNX Runtime documents `Run` as thread-safe, which is relied on
        // only for the EPs of `ExecutionTarget::concurrent_run` (others hold a lock).
        let status = unsafe {
            (ort::api().Run)(
                session.ptr().cast_mut(),
                ptr::null(),
                in_names.as_ptr(),
                in_vals.as_ptr(),
                1,
                out_names.as_ptr(),
                1,
                out_vals.as_mut_ptr(),
            )
        };
        // SAFETY: `status` comes straight from the C API.
        unsafe { ort::Error::result_from_status(status) }?;
        let out = NonNull::new(out_vals[0])
            .ok_or_else(|| Error::ModelOutput("run returned a null output".into()))?;
        // SAFETY: an output ONNX Runtime allocated for this call; we own it now and keep
        // the session alive as long as the value.
        let out: DynValue = unsafe { DynValue::from_ptr(out, Some(session.inner())) };
        let (out_shape, out) = out.try_extract_tensor::<f32>()?;
        let out_shape: Vec<usize> = out_shape.iter().map(|&d| d.max(0) as usize).collect();
        Ok((out_shape, out.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse_and_print() {
        assert_eq!(
            "cpu".parse::<ExecutionTarget>().unwrap(),
            ExecutionTarget::Cpu
        );
        assert_eq!(
            "CUDA:1".parse::<ExecutionTarget>().unwrap(),
            ExecutionTarget::Cuda(1)
        );
        assert_eq!(
            "webgpu".parse::<ExecutionTarget>().unwrap(),
            ExecutionTarget::WebGpu(0)
        );
        assert_eq!(ExecutionTarget::DirectMl(2).to_string(), "directml:2");
        assert!("tpu".parse::<ExecutionTarget>().is_err());
        assert!("cuda:x".parse::<ExecutionTarget>().is_err());
    }

    #[test]
    fn catalog_lists_the_cpu_first() {
        let devices = device_catalog();
        assert_eq!(devices[0].id, "cpu");
        let json = serde_json::to_value(&devices[0]).unwrap();
        assert_eq!(json["provider"], "cpu");
        assert!(ep_compiled().contains(&"cpu"));
    }

    #[test]
    fn unavailable_provider_falls_back_to_cpu_with_a_reason() {
        let dir = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".cache/huggingface/hub/models--Kellenok--PP-OCRv6_manga/snapshots/ba1d479e8a61a20e8318c9758c73fbbbd290b98d/det/manga_det_v0.2.onnx");
        if !dir.is_file() {
            return;
        }
        let opts = RuntimeOptions {
            intra_threads: 1,
            targets: vec![ExecutionTarget::Cuda(0), ExecutionTarget::Cpu],
            copies: 2,
            spinning: false,
        };
        let m = Model::load(&dir, &opts).unwrap();
        if !cfg!(feature = "cuda") {
            assert_eq!(m.target(), ExecutionTarget::Cpu);
            assert!(m.fallback_reason().is_some_and(|r| r.contains("cuda")));
        }
        let (shape, out) = m.run_f32(&[1, 3, 64, 64], vec![0.0; 3 * 64 * 64]).unwrap();
        assert_eq!(shape, vec![1, 1, 64, 64]);
        assert_eq!(out.len(), 64 * 64);
    }
}
