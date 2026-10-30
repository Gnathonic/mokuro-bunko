//! `mokuro-bunko processor-fake` (testing build): a remote OCR processor with no models,
//! held at a file so a scenario can act while a volume is mid-OCR.

use super::super::server::SEED_PASSWORD;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

pub struct FakeProcessor {
    child: Child,
    /// Each volume, once started, waits until this file exists.
    hold_file: PathBuf,
    pub log: PathBuf,
}

/// How the fake processor runs.
#[derive(Debug, Clone, Default)]
pub struct FakeOptions {
    /// Start held: volumes wait for [`FakeProcessor::release`].
    pub held: bool,
    /// `--engine-revision`.
    pub engine_revision: Option<u32>,
    /// `--page-delay-ms`.
    pub page_delay_ms: u64,
}

impl FakeProcessor {
    /// Connect to the server at `url` as the processor account `username` (a seed
    /// user with role `processor`), its storage under `dir`.
    pub fn start(
        bin: &Path,
        dir: &Path,
        url: &str,
        username: &str,
        o: &FakeOptions,
    ) -> Result<FakeProcessor> {
        std::fs::create_dir_all(dir)?;
        let hold_file = dir.join("release");
        if !o.held {
            std::fs::write(&hold_file, b"")?;
        }
        let log = dir.join("processor-fake.log");
        let file = std::fs::File::create(&log)?;
        let mut cmd = Command::new(bin);
        cmd.args(["processor-fake", "--url", url, "--username", username])
            .arg("--storage")
            .arg(dir.join("storage"))
            .arg("--hold-file")
            .arg(&hold_file)
            .args(["--page-delay-ms", &o.page_delay_ms.to_string()])
            .arg("-v")
            // Never on the command line, where `ps` shows it.
            .env("MOKURO_FAKE_PROCESSOR_PASSWORD", SEED_PASSWORD)
            .env("NO_COLOR", "1");
        if let Some(r) = o.engine_revision {
            cmd.args(["--engine-revision", &r.to_string()]);
        }
        let child = cmd
            .stdin(Stdio::null())
            .stdout(file.try_clone()?)
            .stderr(file)
            .spawn()
            .with_context(|| format!("starting {} processor-fake", bin.display()))?;
        Ok(FakeProcessor {
            child,
            hold_file,
            log,
        })
    }

    /// The file each volume waits on (the specs' `RIG_FAKEPROC_HOLD`).
    pub fn hold_file(&self) -> &Path {
        &self.hold_file
    }

    /// Let held volumes go on (and every later one start without waiting).
    #[allow(dead_code)] // the browser scenarios hold a volume mid-OCR (S6)
    pub fn release(&self) -> Result<()> {
        std::fs::write(&self.hold_file, b"")?;
        Ok(())
    }

    /// `Err` with its log once it has exited.
    pub fn check_alive(&mut self) -> Result<()> {
        if let Some(status) = self.child.try_wait()? {
            anyhow::bail!(
                "processor-fake exited ({status})\n{}",
                std::fs::read_to_string(&self.log).unwrap_or_default()
            );
        }
        Ok(())
    }
}

impl Drop for FakeProcessor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
