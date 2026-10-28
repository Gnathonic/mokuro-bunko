//! The machine the server runs on, as the admin panel's "This server" tab and the
//! first-run setup see it: the hardware, the OCR backend pack (preference, install,
//! removal), the engines and models, the doctor and the server log.
//!
//! The binary implements [`Machine`] (it owns the hardware detection, `install-ocr`,
//! `models` and `doctor`); [`crate::app::ServeOptions::machine`] hands it in. Without one
//! (tests, embedders) the tab answers 404 and the setup has no OCR step.
//!
//! Every method may block (files, child processes): callers run them off the async
//! workers.

use crate::ocr::OcrControl;
use bunko_core::Config;
use serde_json::Value;
use std::sync::Arc;

/// What the server hands the machine once its services exist.
#[derive(Clone)]
pub struct MachineHooks {
    /// Restart the server gracefully (re-exec after shutdown).
    pub restart: Arc<dyn Fn() + Send + Sync>,
    /// Stop the server gracefully, for good (no re-exec): the setup made this machine
    /// a processor instead.
    pub stop: Arc<dyn Fn() + Send + Sync>,
    /// This server's OCR (restart its local processor, apply settings).
    pub ocr: OcrControl,
}

/// What the setup's processor step answers on a build that runs no processor.
pub const NO_PROCESSOR: &str = "This build cannot read OCR for another library (lite build)";

/// The machine-side half of the admin panel and the setup (see the module docs).
pub trait Machine: Send + Sync {
    /// Called once, when the router is assembled.
    fn attach(&self, _hooks: MachineHooks) {}

    /// `GET /_admin/api/machine`: hardware, OCR backend, engines and models.
    fn overview(&self) -> Value;

    /// The setup's OCR step: the hardware, whether OCR starts on (a usable GPU), the
    /// backend choices, and what the environment pins. `null` when this build runs no
    /// OCR of its own.
    fn setup_options(&self) -> Value;

    /// Why `ocr.backend` cannot be changed here (the environment sets it), or None.
    fn backend_locked(&self) -> Option<String>;

    /// `ocr.backend` or `ocr.local_processing` changed and is saved (`config` is the
    /// saved config): install what is needed and make this server's OCR use it.
    /// `from_setup`: the first-run setup's choice (automatic installs decide whether
    /// the install starts by itself). Returns `{installing, restarting, message}`.
    fn ocr_changed(&self, config: &Config, backend_changed: bool, from_setup: bool) -> Value;

    /// Install (or retry) the OCR backend now; `reinstall`: even if it is in place.
    fn install(&self, reinstall: bool) -> Result<Value, String>;

    /// Remove the installed backend packs of this server's storage.
    fn remove(&self) -> Result<Value, String>;

    /// Start a job (`doctor`, `models-download`, `models-verify`); its summary.
    fn start_job(&self, kind: &str, engine: Option<&str>) -> Result<Value, String>;

    /// A job's summary and its output lines from line `from` on.
    fn job(&self, id: u64, from: u64) -> Option<Value>;

    /// The last `lines` lines of the server log.
    fn logs(&self, lines: usize) -> Value;

    /// The setup's "this computer reads OCR for another Bunko": check the processor
    /// account in `form` (`url`, `username`, `password`, `tls_verify`) against that
    /// library; registers nothing. The library's answer, or why not.
    fn processor_test(&self, _form: &Value) -> Result<Value, String> {
        Err(NO_PROCESSOR.into())
    }

    /// The same step's finish: check the account again, write this machine's
    /// `processor.yaml`, start the processor, and retire this setup-only server (its
    /// config file moves aside; it stops once this answer went out). Nothing is
    /// written when the library refuses.
    fn processor_setup(&self, _form: &Value) -> Result<Value, String> {
        Err(NO_PROCESSOR.into())
    }
}
