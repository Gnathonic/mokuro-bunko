//! Where this machine's files are, for the pages: the processor's config, the storage
//! of each role, their logs and their control files.

use std::path::{Path, PathBuf};

/// The `processor.yaml` the pages read and write: the one this machine has
/// ([`crate::machine::find_processor_config`]), else the default location.
pub fn processor_config_path() -> PathBuf {
    crate::machine::find_processor_config().unwrap_or_else(crate::machine::default_processor_config)
}

/// The library server's storage (`storage.base_path`, env overrides applied).
pub fn server_storage(config_path: &Path) -> PathBuf {
    bunko_core::config::load_config(Some(config_path))
        .map(|c| c.storage.base_path)
        .unwrap_or_else(|_| bunko_core::storage::default_storage_path())
}

/// The processor's storage (`processor.storage`; the default without a config).
pub fn processor_storage(processor_config: &Path) -> PathBuf {
    #[cfg(feature = "ocr")]
    {
        if processor_config.is_file()
            && let Ok(c) = bunko_processor::load_processor_config(processor_config)
        {
            return c.processor.storage;
        }
        bunko_processor::config::default_storage_path()
    }
    #[cfg(not(feature = "ocr"))]
    {
        let _ = processor_config;
        bunko_core::storage::default_storage_path().with_file_name("mokuro-bunko-processor")
    }
}

/// The control file a running instance writes into its storage (GUI.md §1).
pub const CONTROL_FILE: &str = ".control.json";

/// A file name a listing produced: one path component, no tricks.
pub fn plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_only() {
        assert!(plain_file_name("mokuro-bunko.log"));
        assert!(plain_file_name("server.log.1"));
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "C:x", "a\0"] {
            assert!(!plain_file_name(bad), "{bad:?}");
        }
    }
}
