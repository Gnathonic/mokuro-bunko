//! Shared foundations of mokuro-bunko: configuration, the storage layout, user
//! roles and the OCR engine/generation registry.
//!
//! Nothing here touches the network or a database; every other crate builds on it.

pub mod config;
pub mod engines;
pub mod generations;
pub mod roles;
pub mod storage;

pub use config::{Config, ConfigError};
pub use roles::Role;
pub use storage::StorageLayout;

/// The version string reported everywhere (`generator`, update checks, `/api/version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
