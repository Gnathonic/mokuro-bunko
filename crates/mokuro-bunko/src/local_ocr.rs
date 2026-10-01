//! Local OCR: the in-process processor over the real engines (full build only).

use bunko_server::ocr::LocalProcessorFactory;
use std::sync::Arc;

/// The lite build has no local OCR: the server waits for remote processors.
#[cfg(not(feature = "ocr"))]
pub fn factory() -> Option<Arc<dyn LocalProcessorFactory>> {
    None
}

/// Full build: TODO(engines) — wired once the engine pipeline lands.
#[cfg(feature = "ocr")]
pub fn factory() -> Option<Arc<dyn LocalProcessorFactory>> {
    None
}
