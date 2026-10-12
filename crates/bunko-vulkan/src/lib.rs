//! The **Vulkan backend pack** (docs/rust-port/VULKAN.md): hayai-nova (and paddle-manga)
//! on GPUs that no compiled libtorch package covers — AMD Polaris/Vega/RDNA1 on Linux,
//! and on request any GPU with a Vulkan driver.
//!
//! Two runtimes for the same engines, both fp32 and text-identical to 0.5.2's torch:
//! **ncnn** Vulkan (`bunko-ncnn`, custom GEMM/attention kernels) and ONNX Runtime
//! **WebGPU** over Dawn (`bunko-vlm`'s GPU-resident hayai graphs). They implement one
//! contract ([`backend`]); the host (`bunko-engines::vulkan`) picks one per (device,
//! engine) by a self-test and falls back to the other when one fails on a machine.
//!
//! The library exports the libtorch pack's C ABI (`bunko_torch::abi`): `bt_devices`
//! lists the catalog GPUs ([`probe`]) a runtime serves as `gpu:<catalog index>`, kind
//! `vulkan`, with the runtimes that serve each; `bt_load` takes the runtime in
//! `LoadOptions::backend`.

pub mod backend;
mod ffi;
#[cfg(feature = "ncnn")]
mod ncnn;
pub mod probe;
pub mod runtime;
#[cfg(feature = "webgpu")]
mod webgpu;

use bunko_torch::abi;

/// Errors of the pack (their ABI status codes as the libtorch pack's).
#[derive(Debug, thiserror::Error)]
pub enum VkError {
    #[error("{0}")]
    Arg(String),
    #[error("{0}")]
    Load(String),
    #[error("{0}")]
    Run(String),
}

impl VkError {
    pub fn code(&self) -> i32 {
        match self {
            VkError::Arg(_) => abi::BT_ERR_ARG,
            VkError::Load(_) => abi::BT_ERR_LOAD,
            VkError::Run(_) => abi::BT_ERR_RUN,
        }
    }
}

/// The pack's library file name (`libbunko_vulkan.so`, `bunko_vulkan.dll`).
pub fn library_file_name() -> String {
    format!(
        "{}bunko_vulkan{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}
