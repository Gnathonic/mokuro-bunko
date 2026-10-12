//! Whose storage the OCR tools (`install-ocr`, `models`, `doctor`) work on: the
//! library server's (`<storage.base_path>`, default `~/.local/share/mokuro-bunko`) or
//! the processor's (`processor.storage`, default `~/.local/share/mokuro-bunko-processor`,
//! `%LOCALAPPDATA%\mokuro-bunko-processor`). Each keeps `backends/` (the libtorch pack)
//! and `models/` there.
//!
//! `--processor` picks the processor; without it, a machine with a `processor.yaml`
//! and no library configuration is a processor machine. A processor also looks for
//! packs in the library's storage ([`library_fallback_backends`]), so a pack installed
//! the earlier 0.7 development builds' way (into the library storage) is not wasted.

use crate::cfgfile;
use crate::cmd::Ctx;
use crate::machine;
use crate::out::Fail;
use bunko_engines::{Backend, EngineConfig};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Library,
    Processor,
}

/// The decision, separate from the file system for the tests.
pub fn decide(processor_flag: bool, library_configured: bool, has_processor_config: bool) -> Role {
    if processor_flag || (!library_configured && has_processor_config) {
        Role::Processor
    } else {
        Role::Library
    }
}

pub struct OcrTarget {
    pub role: Role,
    /// `<storage>`: `backends/` and `models/` live under it.
    pub storage: PathBuf,
    /// The processor's config file (processor role, when there is one).
    pub processor_config: Option<PathBuf>,
    /// The library's effective configuration (library role).
    pub library: Option<bunko_core::Config>,
    /// Why this target, for the first line the tools print.
    pub reason: String,
}

impl OcrTarget {
    pub fn models_dir(&self) -> PathBuf {
        self.storage.join("models")
    }

    /// The engines' configuration for this storage: a processor's also searches the
    /// library's backends directories.
    pub fn engine_config(&self, backend: Backend) -> EngineConfig {
        let mut c = EngineConfig::new(self.models_dir(), backend);
        if self.role == Role::Processor {
            c.fallback_backends = library_fallback_backends();
        }
        c.fallback_backends.extend(shipped_backends());
        c
    }

    /// The owner's device preference (`ocr.backend`): the library's configuration
    /// (with its `MOKURO_OCR_BACKEND` override); for a processor `MOKURO_OCR_BACKEND`,
    /// as `processor serve` reads it, else `auto` (what its hardware offers).
    pub fn backend_preference(&self) -> String {
        match (&self.role, &self.library) {
            (Role::Library, Some(c)) => c.ocr.effective_backend().to_string(),
            _ => std::env::var("MOKURO_OCR_BACKEND")
                .ok()
                .filter(|b| !b.trim().is_empty())
                .unwrap_or_else(|| "auto".into()),
        }
    }

    /// Why this role runs no local OCR (`ocr.backend: skip`, `ocr.local_processing:
    /// false`), or None. A processor always runs OCR.
    pub fn local_ocr_off(&self) -> Option<String> {
        let c = self
            .library
            .as_ref()
            .filter(|_| self.role == Role::Library)?;
        if c.ocr.backend == "skip" {
            Some("ocr.backend is skip".into())
        } else if !c.ocr.local_processing {
            Some("ocr.local_processing is false (remote processors do the OCR)".into())
        } else {
            None
        }
    }

    /// Where `install-ocr` installs (`MOKURO_BACKENDS_DIR`, else `<storage>/backends`).
    pub fn backends_dir(&self) -> PathBuf {
        self.engine_config(Backend::Auto).backends_dir()
    }

    /// Every directory the OCR runtime of this role looks in, in order.
    pub fn backends_dirs(&self) -> Vec<PathBuf> {
        self.engine_config(Backend::Auto).backends_dirs()
    }

    pub fn describe(&self) -> String {
        match self.role {
            Role::Library => format!(
                "For the library server (storage: {})",
                self.storage.display()
            ),
            Role::Processor => format!(
                "For the processor{} (storage: {}){}",
                self.processor_config
                    .as_ref()
                    .map(|p| format!(" of {}", p.display()))
                    .unwrap_or_default(),
                self.storage.display(),
                if self.reason.is_empty() {
                    String::new()
                } else {
                    format!(": {}", self.reason)
                }
            ),
        }
    }
}

/// Packs shipped next to the executable (`<exe dir>/backends`: a Docker image built
/// with `BAKE_PACK=1`), searched after the storage's, so a pack installed into the
/// storage (`install-ocr`, the images' automatic install) comes first. None when there
/// is no such directory.
pub fn shipped_backends() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.join("backends");
    dir.is_dir().then_some(dir)
}

/// The library's backends directories a processor also searches: the configured
/// library storage's (when a library config loads here) and the default one.
pub fn library_fallback_backends() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let config_path = cfgfile::resolve(None);
    if config_path.is_file()
        && let Ok(c) = bunko_core::config::load_config(Some(&config_path))
    {
        dirs.push(c.storage.base_path.join("backends"));
    }
    dirs.push(bunko_core::storage::default_storage_path().join("backends"));
    dirs.dedup();
    dirs
}

/// The processor's storage from its config (the default storage without one).
fn processor_storage(config: Option<&Path>) -> Result<PathBuf, Fail> {
    match config {
        Some(path) => bunko_processor::load_processor_config(path)
            .map(|c| c.processor.storage)
            .map_err(|e| Fail::msg(format!("{}: {e}", path.display()))),
        None => Ok(bunko_processor::config::default_storage_path()),
    }
}

/// Pick the target for this run.
pub fn resolve(ctx: &Ctx, processor_flag: bool) -> Result<OcrTarget, Fail> {
    let library_configured = machine::library_configured(&ctx.config_path);
    let found = machine::find_processor_config();
    match decide(processor_flag, library_configured, found.is_some()) {
        Role::Processor => Ok(OcrTarget {
            role: Role::Processor,
            storage: processor_storage(found.as_deref())?,
            reason: if processor_flag {
                String::new()
            } else {
                "this machine has a processor.yaml and no library configuration".into()
            },
            processor_config: found,
            library: None,
        }),
        Role::Library => {
            let config = cfgfile::load_effective(&ctx.config_path)?;
            Ok(OcrTarget {
                role: Role::Library,
                storage: config.storage.base_path.clone(),
                processor_config: found,
                library: Some(config),
                reason: String::new(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn who_gets_the_install() {
        // The flag always wins.
        assert_eq!(decide(true, true, false), Role::Processor);
        assert_eq!(decide(true, false, false), Role::Processor);
        // A processor-only machine.
        assert_eq!(decide(false, false, true), Role::Processor);
        // A library (with or without a processor too), or nothing set up yet.
        assert_eq!(decide(false, true, true), Role::Library);
        assert_eq!(decide(false, true, false), Role::Library);
        assert_eq!(decide(false, false, false), Role::Library);
    }

    #[test]
    fn preference_and_local_ocr_switches() {
        let lib = |f: &dyn Fn(&mut bunko_core::Config)| {
            let mut c = bunko_core::Config::default();
            f(&mut c);
            OcrTarget {
                role: Role::Library,
                storage: PathBuf::from("/srv/lib"),
                processor_config: None,
                library: Some(c),
                reason: String::new(),
            }
        };
        let t = lib(&|_| {});
        assert_eq!(t.backend_preference(), "auto");
        assert_eq!(t.local_ocr_off(), None);
        let t = lib(&|c| c.ocr.backend = "rocm".into());
        // The AMD GPU, not WebGPU (the ONNX-only design's alias).
        assert_eq!(t.backend_preference(), "rocm");
        let t = lib(&|c| c.ocr.backend = "cpu".into());
        assert_eq!(t.backend_preference(), "cpu");
        let t = lib(&|c| c.ocr.backend = "skip".into());
        assert!(t.local_ocr_off().unwrap().contains("skip"));
        let t = lib(&|c| c.ocr.local_processing = false);
        assert!(t.local_ocr_off().unwrap().contains("local_processing"));
        // A processor always runs OCR.
        let p = OcrTarget {
            role: Role::Processor,
            ..lib(&|c| c.ocr.backend = "skip".into())
        };
        assert_eq!(p.local_ocr_off(), None);
    }

    #[test]
    fn processor_searches_its_own_then_the_library_backends() {
        if std::env::var_os(bunko_engines::BACKENDS_DIR_ENV).is_some() {
            return;
        }
        let t = OcrTarget {
            role: Role::Processor,
            storage: PathBuf::from("/srv/proc"),
            processor_config: None,
            library: None,
            reason: String::new(),
        };
        let dirs = t.backends_dirs();
        assert_eq!(dirs[0], PathBuf::from("/srv/proc/backends"));
        assert_eq!(t.backends_dir(), PathBuf::from("/srv/proc/backends"));
        assert!(
            dirs.contains(&bunko_core::storage::default_storage_path().join("backends")),
            "{dirs:?}"
        );
        assert_eq!(t.models_dir(), PathBuf::from("/srv/proc/models"));
        let lib = OcrTarget {
            role: Role::Library,
            storage: PathBuf::from("/srv/lib"),
            processor_config: None,
            library: None,
            reason: String::new(),
        };
        assert_eq!(
            lib.backends_dirs(),
            vec![PathBuf::from("/srv/lib/backends")]
        );
    }
}
