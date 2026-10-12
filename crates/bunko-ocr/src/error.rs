//! The crate's error type.

use std::path::PathBuf;

/// Everything that can go wrong in `bunko-ocr`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("onnx runtime: {0}")]
    Ort(#[from] ort::Error),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot decode image: {0}")]
    Decode(String),
    #[error("cannot encode image: {0}")]
    Encode(String),
    #[error("archive {path}: {msg}")]
    Archive { path: PathBuf, msg: String },
    #[error("model file '{id}': {msg}")]
    Model { id: String, msg: String },
    #[error("download of {url} failed: {msg}")]
    Download { url: String, msg: String },
    #[error("unexpected model output: {0}")]
    ModelOutput(String),
    #[error("{0}")]
    Invalid(String),
}

impl From<ort::Error<ort::session::builder::SessionBuilder>> for Error {
    fn from(e: ort::Error<ort::session::builder::SessionBuilder>) -> Self {
        Error::Ort(e.into())
    }
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
