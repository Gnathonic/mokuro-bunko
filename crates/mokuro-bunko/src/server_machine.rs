//! The library server's [`bunko_server::machine::Machine`]: what the admin panel's
//! "This server" tab and the first-run setup's OCR step show and do on the machine
//! the server runs on (hardware, the OCR backend pack, engines and models, `doctor`,
//! the server log). Long work runs as this same program in a child process, as the
//! CLI does it (`install-ocr` through the background installer, `models`, `doctor`).

use crate::gui::jobs::Jobs;
use bunko_core::Config;
use bunko_server::machine::{Machine, MachineHooks};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Job kinds the panel may start.
const JOB_KINDS: [&str; 4] = ["doctor", "models-list", "models-download", "models-verify"];

pub struct ServerMachine {
    config_path: PathBuf,
    storage: PathBuf,
    /// `serve --ocr`: the backend this run was started with (it wins over the file).
    cli_backend: Option<String>,
    exe: PathBuf,
    jobs: Jobs,
    hooks: Mutex<Option<MachineHooks>>,
    #[cfg(feature = "ocr")]
    installer: Option<crate::ocr_install::Installer>,
}

impl ServerMachine {
    pub fn new(
        config_path: PathBuf,
        storage: PathBuf,
        cli_backend: Option<String>,
        #[cfg(feature = "ocr")] installer: Option<crate::ocr_install::Installer>,
    ) -> ServerMachine {
        ServerMachine {
            config_path,
            storage,
            cli_backend,
            exe: bunko_update::current_exe()
                .or_else(|_| std::env::current_exe())
                .unwrap_or_else(|_| PathBuf::from("mokuro-bunko")),
            jobs: Jobs::default(),
            hooks: Mutex::new(None),
            #[cfg(feature = "ocr")]
            installer,
        }
    }

    fn config(&self) -> Config {
        crate::cfgfile::load_effective_quiet(&self.config_path).unwrap_or_default()
    }

    fn log_path(&self) -> PathBuf {
        self.storage
            .join("logs")
            .join(crate::logging::SERVER_LOG_NAME)
    }

    #[cfg_attr(not(feature = "ocr"), allow(dead_code))]
    fn hooks(&self) -> Option<MachineHooks> {
        self.hooks.lock().clone()
    }

    #[cfg_attr(not(feature = "ocr"), allow(dead_code))]
    /// Restart the whole server in a moment (after the current answer went out).
    fn restart_soon(&self, why: &str) {
        let Some(h) = self.hooks() else { return };
        tracing::info!("Restarting the server: {why}");
        let restart = h.restart.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                restart();
            });
        }
    }

    #[cfg_attr(not(feature = "ocr"), allow(dead_code))]
    /// Restart this server's OCR (its local processor) with what the config says now.
    fn restart_local(&self, why: &'static str) {
        let Some(h) = self.hooks() else { return };
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { h.ocr.restart_local(why).await });
        }
    }

    fn job_view(job: &crate::gui::jobs::Job, from: u64) -> Value {
        let mut v = job.summary();
        let (lines, next) = job.lines_from(from);
        if let Value::Object(o) = &mut v {
            o.insert(
                "output".into(),
                json!(lines.into_iter().map(|(_, l)| l).collect::<Vec<_>>()),
            );
            o.insert("next".into(), json!(next));
        }
        v
    }
}

impl Machine for ServerMachine {
    fn attach(&self, hooks: MachineHooks) {
        *self.hooks.lock() = Some(hooks);
    }

    fn overview(&self) -> Value {
        let config = self.config();
        #[cfg_attr(not(feature = "ocr"), allow(unused_mut))]
        let mut v = json!({
            "version": bunko_core::VERSION,
            "flavor": crate::FLAVOR,
            "target": bunko_update::TARGET,
            "ocr_build": cfg!(feature = "ocr"),
            "storage": self.storage,
            "log": self.log_path(),
            "local_ocr": config.processes_locally(cfg!(feature = "ocr")),
        });
        #[cfg(feature = "ocr")]
        if let Value::Object(o) = &mut v {
            for (k, x) in full::overview(self, &config) {
                o.insert(k, x);
            }
        }
        v
    }

    fn setup_options(&self) -> Value {
        #[cfg(feature = "ocr")]
        {
            full::setup_options(self)
        }
        #[cfg(not(feature = "ocr"))]
        Value::Null
    }

    fn backend_locked(&self) -> Option<String> {
        if self.cli_backend.is_some() {
            return Some("The OCR backend is set by serve --ocr for this run".into());
        }
        std::env::var("MOKURO_OCR_BACKEND")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| format!("The OCR backend is set by MOKURO_OCR_BACKEND ({v})"))
    }

    fn ocr_changed(&self, config: &Config, backend_changed: bool, from_setup: bool) -> Value {
        #[cfg(feature = "ocr")]
        {
            full::ocr_changed(self, config, backend_changed, from_setup)
        }
        #[cfg(not(feature = "ocr"))]
        {
            let _ = (config, backend_changed, from_setup);
            json!({"installing": false, "restarting": false,
                   "message": "This build runs no OCR of its own: processors read the volumes."})
        }
    }

    fn install(&self, reinstall: bool) -> Result<Value, String> {
        #[cfg(feature = "ocr")]
        {
            full::install(self, reinstall)
        }
        #[cfg(not(feature = "ocr"))]
        {
            let _ = reinstall;
            Err("This build runs no OCR of its own (lite build)".into())
        }
    }

    fn remove(&self) -> Result<Value, String> {
        #[cfg(feature = "ocr")]
        {
            full::remove(self)
        }
        #[cfg(not(feature = "ocr"))]
        Err("This build runs no OCR of its own (lite build)".into())
    }

    fn start_job(&self, kind: &str, engine: Option<&str>) -> Result<Value, String> {
        if !JOB_KINDS.contains(&kind) {
            return Err(format!("unknown job: {kind}"));
        }
        if kind != "doctor" && !cfg!(feature = "ocr") {
            return Err("This build has no OCR models (lite build)".into());
        }
        if let Some(j) = self.jobs.running(kind) {
            return Ok(Self::job_view(&j, u64::MAX));
        }
        #[cfg(feature = "ocr")]
        if kind == "models-download"
            && self
                .installer
                .as_ref()
                .is_some_and(bunko_server::ocr::BackgroundInstall::running)
        {
            return Err("The OCR backend install is running (it fetches the models too)".into());
        }
        if let Some(e) = engine
            && !engine_known(e)
        {
            return Err(format!("unknown engine: {e}"));
        }
        let mut args = vec!["-c".to_string(), self.config_path.display().to_string()];
        let title = match kind {
            "doctor" => {
                args.push("doctor".into());
                "Diagnostics"
            }
            "models-list" => {
                args.extend(["models".into(), "list".into()]);
                "Models"
            }
            "models-verify" => {
                args.extend(["models".into(), "verify".into()]);
                "Verify the models"
            }
            _ => {
                args.extend(["models".into(), "download".into()]);
                if let Some(e) = engine {
                    args.extend(["--engine".into(), e.to_string()]);
                }
                "Download the models"
            }
        };
        let mut envs = vec![(crate::cmd::doctor::SERVING_ENV.to_string(), "1".to_string())];
        if let Some(b) = &self.cli_backend {
            envs.push(("MOKURO_OCR_BACKEND".to_string(), b.clone()));
        }
        let job = self.jobs.start(kind, title, &self.exe, args, envs);
        Ok(Self::job_view(&job, 0))
    }

    fn job(&self, id: u64, from: u64) -> Option<Value> {
        self.jobs.get(id).map(|j| Self::job_view(&j, from))
    }

    fn logs(&self, lines: usize) -> Value {
        let path = self.log_path();
        match crate::gui::spawn::tail_file(&path, lines) {
            Ok(text) => json!({"path": path, "text": text}),
            Err(e) => json!({"path": path, "text": "", "error": e.to_string()}),
        }
    }
}

fn engine_known(e: &str) -> bool {
    #[cfg(feature = "ocr")]
    {
        bunko_engines::models::ENGINES.contains(&e)
    }
    #[cfg(not(feature = "ocr"))]
    {
        let _ = e;
        false
    }
}

/// The installed size of a directory tree.
#[cfg_attr(not(feature = "ocr"), allow(dead_code))]
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

#[cfg(feature = "ocr")]
mod full {
    use super::*;
    use crate::cmd::install_ocr;
    use crate::hwdetect;
    use crate::ocr_install::InstallRequest;
    use crate::ocr_target::{OcrTarget, Role};
    use bunko_server::ocr::BackgroundInstall;

    fn target(m: &ServerMachine, config: &Config) -> OcrTarget {
        let mut library = config.clone();
        if let Some(b) = &m.cli_backend {
            library.ocr.backend = b.clone();
        }
        OcrTarget {
            role: Role::Library,
            storage: m.storage.clone(),
            processor_config: None,
            library: Some(library),
            reason: String::new(),
        }
    }

    fn hardware_json(hw: &hwdetect::Hardware) -> Value {
        let choice = hwdetect::choose(hw, bunko_update::TARGET);
        json!({
            "nvidia_driver": hw.nvidia_driver,
            "nvidia_gpus": hw.nvidia_gpus,
            "amd_gfx": hw.amd_gfx,
            "hidden": hw.hidden,
            "auto_variant": choice.variant,
            "reason": choice.reason,
            "hint": choice.hint,
            "container": hwdetect::in_container(),
        })
    }

    /// The backends a page offers: auto, cpu, and the GPU kinds found here (plus the
    /// configured one, so the select always shows it).
    fn choices(hw: &hwdetect::Hardware, current: &str) -> Vec<Value> {
        let mut out = vec![
            json!({"id": "auto", "label": "Auto"}),
            json!({"id": "cpu", "label": "CPU"}),
        ];
        let nvidia = hw.nvidia_driver.is_some() || !hw.nvidia_gpus.is_empty();
        let amd = !hw.amd_gfx.is_empty();
        if nvidia || current == "cuda" {
            out.push(json!({"id": "cuda", "label": "NVIDIA (CUDA)", "detected": nvidia}));
        }
        if amd || current == "rocm" {
            out.push(json!({"id": "rocm", "label": "AMD (ROCm)", "detected": amd}));
        }
        out
    }

    /// The pack this process has loaded (None: no OCR ran yet, or none opened).
    /// (its folder, its variant, the release it belongs to).
    fn loaded() -> Option<(PathBuf, String, String)> {
        bunko_engines::torch::backend_if_open().map(|b| {
            (
                b.pack.dir.clone(),
                b.pack.manifest.variant.clone(),
                b.pack.manifest.bunko_version.clone(),
            )
        })
    }

    fn same_dir(a: &Path, b: &Path) -> bool {
        let c = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        c(a) == c(b)
    }

    pub(super) fn overview(m: &ServerMachine, config: &Config) -> Vec<(String, Value)> {
        let t = target(m, config);
        let hw = hwdetect::detect();
        let pref = t.backend_preference();
        let wanted = install_ocr::wanted_choice(&t, &hw, &t.backends_dir());
        let loaded = loaded();
        let mut packs = Vec::new();
        let mut seen: Vec<PathBuf> = Vec::new();
        let own = t.backends_dir();
        for dir in t.backends_dirs() {
            for (p, man) in bunko_update::backend::installed(&dir) {
                if seen.contains(&p) {
                    continue;
                }
                seen.push(p.clone());
                packs.push(json!({
                    "name": man.name,
                    "variant": man.variant,
                    "version": man.bunko_version,
                    "dir": p,
                    "size": super::dir_size(&p),
                    // The part NVIDIA's CUDA wheels added (fetched from PyPI at install).
                    "external_size": man.external.iter().flat_map(|e| &e.files).map(|f| f.size).sum::<u64>(),
                    "external": man.external.iter().map(|e| e.name.clone()).collect::<Vec<_>>(),
                    "complete": install_ocr::pack_complete(&p, &man),
                    "current": man.bunko_version == bunko_core::VERSION,
                    "removable": dir == own,
                    "in_use": loaded.as_ref().is_some_and(|(d, _, _)| same_dir(d, &p)),
                }));
            }
        }
        let (auto, _) = install_ocr::auto_install_setting(|n| std::env::var(n).ok());
        let need = match install_ocr::need(&t, &hw) {
            install_ocr::Need::Off(why) => json!({"state": "off", "text": why}),
            install_ocr::Need::Install { variant, reason } => {
                json!({"state": "install", "variant": variant, "text": reason})
            }
            install_ocr::Need::Satisfied(why) => json!({"state": "ok", "text": why}),
        };
        // What the enabled generations need here, engine by engine: the same plan as
        // `models list` / `models download`. The compiled packages need the libtorch
        // backend: planned when this process has it open, or opens it the way its own
        // OCR would (local OCR on, no install running); never loaded just for a page
        // otherwise.
        let store = t.engine_config(bunko_engines::Backend::Auto).store();
        let open =
            t.local_ocr_off().is_none() && !m.installer.as_ref().is_some_and(|i| i.running());
        let plan = crate::cmd::models::plan(&t, &crate::cmd::models::wanted_rows(&t, None), open);
        vec![
            ("hardware".into(), hardware_json(&hw)),
            (
                "backend".into(),
                json!({
                    "preference": pref,
                    "locked": m.backend_locked(),
                    "choices": choices(&hw, &pref),
                    "wants": wanted.variant,
                    "reason": wanted.reason,
                    "hint": wanted.hint,
                    "need": need,
                    "auto_install": auto,
                    "loaded": loaded.map(|(dir, variant, version)| json!({
                        "name": dir.file_name().map(|n| n.to_string_lossy().into_owned()),
                        "variant": variant,
                        "version": version,
                    })),
                }),
            ),
            ("packs".into(), Value::Array(packs)),
            (
                "install".into(),
                serde_json::to_value(m.installer.as_ref().and_then(|i| i.view()))
                    .unwrap_or(Value::Null),
            ),
            (
                "models".into(),
                json!({"dir": plan.dir, "engines": plan.engines, "total": plan.total,
                       "total_text": plan.total_text(),
                       "shared": plan.shared, "downloads": store.can_download()}),
            ),
        ]
    }

    pub(super) fn setup_options(m: &ServerMachine) -> Value {
        let config = m.config();
        let hw = hwdetect::detect();
        let auto_choice = hwdetect::choose(&hw, bunko_update::TARGET);
        let gpu = auto_choice.variant != "cpu";
        let (auto, _) = install_ocr::auto_install_setting(|n| std::env::var(n).ok());
        json!({
            "hardware": hardware_json(&hw),
            "default_on": gpu,
            "choices": choices(&hw, &config.ocr.backend),
            "backend": config.ocr.backend,
            "backend_locked": m.backend_locked(),
            "local_locked": std::env::var("MOKURO_OCR_LOCAL_PROCESSING").ok().filter(|v| !v.trim().is_empty()),
            "auto_install": auto,
        })
    }

    /// A process restart is needed to use `config`'s backend: a pack is loaded and
    /// it is not the GPU pack the preference now wants (a GPU pack also runs on the
    /// CPU, so going to `cpu` never needs one).
    fn needs_process_restart(m: &ServerMachine, config: &Config) -> bool {
        let Some((_, variant, _)) = loaded() else {
            return false;
        };
        let t = target(m, config);
        let want = install_ocr::wanted_choice(&t, &hwdetect::detect(), &t.backends_dir());
        want.variant != "cpu" && want.variant != variant
    }

    pub(super) fn ocr_changed(
        m: &ServerMachine,
        config: &Config,
        backend_changed: bool,
        from_setup: bool,
    ) -> Value {
        let Some(installer) = m.installer.clone() else {
            return json!({"installing": false, "restarting": false, "message": ""});
        };
        let on = config.processes_locally(true);
        let (auto, _) = install_ocr::auto_install_setting(|n| std::env::var(n).ok());
        let hooks = m.hooks();
        if from_setup {
            if on && !auto {
                // Automatic installs are off: say what is missing (an Install button).
                installer.start_if_needed();
                return json!({"installing": false, "restarting": false,
                    "message": "Automatic OCR installs are off here: install the backend in This server."});
            }
            // The scheduler takes OCR on or off (turning it on starts the install).
            if let Some(h) = &hooks {
                h.ocr.apply_config(config);
            }
            if !on {
                return json!({"installing": false, "restarting": false, "message": ""});
            }
        }
        let restart = backend_changed && needs_process_restart(m, config);
        match installer.start_manual() {
            Ok(true) => {
                if restart {
                    restart_after_install(m, &installer);
                }
                json!({"installing": true, "restarting": false,
                       "message": if restart { "Installing; the server restarts to use it." } else { "Installing in the background." }})
            }
            Ok(false) if restart => {
                m.restart_soon("another OCR backend was chosen");
                json!({"installing": false, "restarting": true, "message": "Restarting to use it."})
            }
            Ok(false) => {
                if backend_changed {
                    m.restart_local("the OCR backend preference changed");
                }
                json!({"installing": false, "restarting": false, "message": ""})
            }
            Err(e) => json!({"installing": false, "restarting": false, "message": e}),
        }
    }

    /// When this install ends well, restart the server (another backend is loaded).
    fn restart_after_install(m: &ServerMachine, installer: &crate::ocr_install::Installer) {
        let Some(h) = m.hooks() else { return };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut done = BackgroundInstall::finished(installer);
        done.borrow_and_update();
        let installer = installer.clone();
        rt.spawn(async move {
            if done.changed().await.is_err() {
                return;
            }
            if installer.view().is_some_and(|v| !v.failed()) {
                tracing::info!("Restarting the server to use the new OCR backend");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                (h.restart)();
            }
        });
    }

    pub(super) fn install(m: &ServerMachine, reinstall: bool) -> Result<Value, String> {
        let installer = m
            .installer
            .clone()
            .ok_or("This server installs no OCR backend")?;
        let config = m.config();
        let t = target(m, &config);
        if let Some(why) = t.local_ocr_off() {
            return Err(format!(
                "OCR on this machine is off ({why}): turn it on first"
            ));
        }
        if installer.running() {
            return Ok(json!({"installing": true, "install": installer.view()}));
        }
        if reinstall {
            InstallRequest {
                force: true,
                ..InstallRequest::default()
            }
            .write(&t.backends_dir())
            .map_err(|e| e.to_string())?;
        }
        let restart = needs_process_restart(m, &config);
        let running = installer.start_manual()?;
        if running && restart {
            restart_after_install(m, &installer);
        }
        Ok(json!({"installing": running, "install": installer.view(),
                  "message": if running { "" } else { "Nothing to install: the backend is in place." }}))
    }

    pub(super) fn remove(m: &ServerMachine) -> Result<Value, String> {
        if m.installer.as_ref().is_some_and(|i| i.running()) {
            return Err("An install is running: wait for it to end".into());
        }
        let config = m.config();
        let t = target(m, &config);
        let root = t.backends_dir();
        let loaded = loaded();
        let mut removed = Vec::new();
        let mut freed = 0u64;
        for (p, man) in bunko_update::backend::installed(&root) {
            let size = super::dir_size(&p);
            std::fs::remove_dir_all(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            tracing::info!("Removed the OCR backend pack {}", p.display());
            freed += size;
            removed.push(man.name);
        }
        let _ = std::fs::remove_file(InstallRequest::path(&root));
        let _ = std::fs::remove_file(root.join(install_ocr::HardwareRecord::FILE));
        let note = if loaded.is_some() {
            "This server keeps using the loaded pack until it restarts."
        } else {
            ""
        };
        Ok(json!({"removed": removed, "freed": freed, "message": note}))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn choices_offer_found_gpus_and_the_current_one() {
            let none = hwdetect::Hardware::default();
            let ids = |v: Vec<Value>| -> Vec<String> {
                v.iter()
                    .map(|c| c["id"].as_str().unwrap().to_string())
                    .collect()
            };
            assert_eq!(ids(choices(&none, "auto")), ["auto", "cpu"]);
            assert_eq!(ids(choices(&none, "rocm")), ["auto", "cpu", "rocm"]);
            let amd = hwdetect::Hardware {
                amd_gfx: vec!["gfx1201".into()],
                ..Default::default()
            };
            assert_eq!(ids(choices(&amd, "auto")), ["auto", "cpu", "rocm"]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_are_a_fixed_list() {
        let dir = tempfile::tempdir().unwrap();
        let m = ServerMachine::new(
            dir.path().join("config.yaml"),
            dir.path().to_path_buf(),
            None,
            #[cfg(feature = "ocr")]
            None,
        );
        assert!(m.start_job("rm", None).is_err());
        assert!(m.start_job("models-download", Some("../x")).is_err());
        let log = m.logs(5);
        assert_eq!(log["text"], "");
        assert!(log["error"].is_string());
        assert!(m.job(1, 0).is_none());
    }
}
