//! Shared foundations of mokuro-bunko: configuration, the storage layout, user
//! roles and the OCR engine/generation registry.
//!
//! Nothing here touches the network or a database; every other crate builds on it.

pub mod casefold_table;
pub mod config;
pub mod engines;
pub mod fold_groups;
pub mod generations;
pub mod ignorable_table;
pub mod names;
pub mod reader_tree;
pub mod relpath;
pub mod roles;
pub mod series_json;
pub mod sidecar_names;
pub mod storage;

pub use config::{Config, ConfigError, LibraryBackend};
pub use roles::{Perm, Role, has_perm};
pub use storage::StorageLayout;

/// The version string reported everywhere (`generator`, update checks, `/api/version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
