//! hayai-nova and paddle-manga on **libtorch** (mokuro-bunko 0.7's recognizer backend;
//! design: `docs/rust-port/TORCH-BACKEND.md`).
//!
//! The recognizers run AOTInductor packages (`.pt2`: vision, decoder prefill, decoder
//! step) compiled with the exact torch of the pack, through `tch` 0.26 (libtorch 2.13)
//! and a small C++ shim over `AOTIModelPackageLoader`. Host preprocessing and the token
//! loops are bunko-vlm's, so a crop reads the same text as on 0.5.2's torch at the same
//! precision.
//!
//! This crate is built **per backend pack** (`--features libtorch`, against that pack's
//! libtorch) as a cdylib that `mokuro-bunko` opens at run time; the binary itself never
//! links libtorch. The C ABI is in [`abi`] (always built, no libtorch needed, also used by
//! the loader in `bunko-engines`). Without the `libtorch` feature the cdylib exports
//! nothing.

pub mod abi;

#[cfg(feature = "libtorch")]
mod aoti;
#[cfg(feature = "libtorch")]
mod ffi;
#[cfg(feature = "libtorch")]
mod gpu;
#[cfg(feature = "libtorch")]
mod hayai;
#[cfg(feature = "libtorch")]
mod package;
#[cfg(feature = "libtorch")]
mod paddle;
#[cfg(feature = "libtorch")]
mod par;
#[cfg(feature = "libtorch")]
mod runtime;
#[cfg(feature = "libtorch")]
mod unpack;

#[cfg(feature = "libtorch")]
pub use runtime::{Handle, devices, init, load, torch_version};

/// Errors of the libtorch side; each maps to one ABI status code.
#[derive(Debug, thiserror::Error)]
pub enum TorchError {
    /// Bad arguments ([`abi::BT_ERR_ARG`]).
    #[error("{0}")]
    Arg(String),
    /// A model, table or library could not be loaded ([`abi::BT_ERR_LOAD`]).
    #[error("{0}")]
    Load(String),
    /// Inference failed ([`abi::BT_ERR_RUN`]).
    #[error("{0}")]
    Run(String),
}

impl TorchError {
    pub fn code(&self) -> i32 {
        match self {
            TorchError::Arg(_) => abi::BT_ERR_ARG,
            TorchError::Load(_) => abi::BT_ERR_LOAD,
            TorchError::Run(_) => abi::BT_ERR_RUN,
        }
    }
}

impl From<bunko_vlm::VlmError> for TorchError {
    fn from(e: bunko_vlm::VlmError) -> Self {
        use bunko_vlm::VlmError as V;
        match e {
            V::Io(..) | V::Asset(_) => TorchError::Load(e.to_string()),
            V::Config(_) => TorchError::Arg(e.to_string()),
            V::Runtime(_) | V::Crop(_) => TorchError::Run(e.to_string()),
        }
    }
}
