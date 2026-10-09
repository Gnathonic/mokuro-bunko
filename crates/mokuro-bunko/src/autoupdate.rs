//! Automatic updates in the binary (opt-in: `update.auto` for the server,
//! `processor.auto_update` for a processor; docs/configuration.md "Automatic updates").
//!
//! A release is one unit: the `mokuro-bunko` binary, its OCR backend pack (a pack runs
//! only with the binary of its own release) and the models its compiled-in manifests
//! name. [`Installer`] installs one:
//!
//! 1. fetch release X's signed `release.json`, download and check X's executable next
//!    to the running one (`bunko_update::Updater::stage`) — never a downgrade;
//! 2. run that NEW executable's `update prefetch` (hidden command): it stages X's pack
//!    for the variant installed here (`<backends>/.staging-<variant>`, signature +
//!    sha256 + every file, the host's requirements, the disk space), loads it in that
//!    process (a GPU pack must find its GPU), and fetches X's models with it. Nothing
//!    installed has changed yet; any failure ends the attempt here;
//! 3. switch both together: the running executable is copied aside
//!    (`.mokuro-bunko-previous`), the installed pack moves to `.prev-<name>`, the staged
//!    one takes its place, the executable is replaced, and `<storage>/.update.json`
//!    ([`Marker`]) says what happened. The caller restarts.
//!
//! At the next start [`after_restart`] reads the marker. Release X checks its pack in a
//! child process (`install-ocr --probe`, the pack this role will really open). If it
//! does not load, X rolls back BOTH (the previous executable and pack come back), marks
//! X blocked (`.update-blocked.json`: no automatic retry of X) and restarts into the
//! previous release, which raises a `fail` problem ("update to X rolled back: …"). If
//! it loads, the kept copies are deleted and the status says "Updated to X".

use bunko_control::{Problem, UpdateView};
use bunko_update::auto::{Blocked, InstallFailure, Marker, ReleaseInstaller};
use futures_util::future::BoxFuture;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Exit code of `update prefetch` when only the owner can fix what failed.
pub const PREFETCH_NEEDS_OWNER: i32 = 3;

/// Which instance installs: what the prefetch child and the probe child are told.
#[derive(Debug, Clone)]
pub enum Who {
    /// The library server: `-c <config>` (as this process got it, if it did).
    Library { cli_config: Option<PathBuf> },
    /// A processor: its `processor.yaml` (full build).
    #[cfg_attr(not(feature = "ocr"), allow(dead_code))]
    Processor { config: PathBuf },
}

impl Who {
    fn args(&self) -> Vec<std::ffi::OsString> {
        match self {
            Who::Library {
                cli_config: Some(c),
            } => vec!["-c".into(), c.clone().into()],
            _ => Vec::new(),
        }
    }
}

/// The result line of `update prefetch`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrefetchResult {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub needs_owner: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// The staged pack (absent: no pack installed here, or the lite build).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack: Option<StagedPackInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StagedPackInfo {
    /// The backends directory (the staging and the installed pack are in it).
    pub root: PathBuf,
    pub staging: PathBuf,
    /// The directory name it takes when switched in (`torch-<variant>-<torch>`).
    pub name: String,
    pub variant: String,
}

/// The real [`ReleaseInstaller`] of this binary.
#[derive(Clone)]
pub struct Installer {
    inner: Arc<InstallerInner>,
}

struct InstallerInner {
    updater: bunko_update::Updater,
    who: Who,
    /// Where the marker goes (the instance's storage).
    storage: PathBuf,
}

impl Installer {
    /// `manifest_url` names the latest release; `public_key` is the config file's
    /// `update.public_key` (empty: the compiled-in key).
    pub fn new(
        manifest_url: &str,
        channel: &str,
        public_key: &str,
        who: Who,
        storage: &Path,
    ) -> Installer {
        let (key, custom) = bunko_update::auto::release_key(public_key);
        if custom {
            tracing::warn!(
                "{}",
                bunko_update::auto::custom_key_warning(
                    &key,
                    "update.public_key in the config file"
                )
            );
        }
        let updater = bunko_update::Updater::new(manifest_url, channel, crate::update_flavor())
            .with_public_key(key);
        Installer {
            inner: Arc::new(InstallerInner {
                updater,
                who,
                storage: storage.to_path_buf(),
            }),
        }
    }

    async fn run(&self, version: String) -> Result<String, InstallFailure> {
        let i = &self.inner;
        let manifest = i
            .updater
            .fetch_version(&version)
            .await
            .map_err(update_failure)?;
        tracing::info!(
            "Automatic update: downloading mokuro-bunko {}",
            manifest.version
        );
        let staged = i.updater.stage(&manifest).await.map_err(update_failure)?;
        tracing::info!(
            "Update: {} checked; fetching its OCR backend pack and models with it",
            staged.binary.display()
        );
        let prefetch = match self.prefetch(&staged.binary).await {
            Ok(p) => p,
            Err(f) => {
                staged.discard();
                return Err(f);
            }
        };
        let to = staged.version.clone();
        let switched = tokio::task::spawn_blocking({
            let storage = i.storage.clone();
            move || switch(staged, prefetch.pack.as_ref(), &storage)
        })
        .await
        .map_err(|e| InstallFailure::retry(format!("the switch stopped: {e}")))?;
        switched?;
        Ok(to)
    }

    /// Run the staged executable's `update prefetch` and read its result line.
    async fn prefetch(&self, binary: &Path) -> Result<PrefetchResult, InstallFailure> {
        let i = &self.inner;
        let mut cmd = tokio::process::Command::new(binary);
        cmd.args(i.who.args())
            .arg("update")
            .arg("prefetch")
            .arg("--manifest-url")
            .arg(i.updater.manifest_url());
        if let Who::Processor { config } = &i.who {
            cmd.arg("--processor-config").arg(config);
        }
        cmd.env("NO_COLOR", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            InstallFailure::retry(format!("could not run {}: {e}", binary.display()))
        })?;
        // Its progress ("pack.tar.zst [1/2]: 25% of 900 MB") goes to the log as it comes,
        // not all at once when it is done.
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let lines = async {
            let mut result: Option<PrefetchResult> = None;
            if let Some(out) = stdout {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(out).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.starts_with('{') {
                        if let Ok(r) = serde_json::from_str(&line) {
                            result = Some(r);
                        }
                    } else if !line.trim().is_empty() {
                        tracing::info!("prefetch: {line}");
                    }
                }
            }
            result
        };
        let errors = async {
            let mut last = String::new();
            if let Some(err) = stderr {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(err).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        tracing::debug!("prefetch (stderr): {line}");
                        last = line;
                    }
                }
            }
            last
        };
        let (result, last_err) = tokio::join!(lines, errors);
        let status = child
            .wait()
            .await
            .map_err(|e| InstallFailure::retry(format!("waiting for {}: {e}", binary.display())))?;
        match result {
            Some(r) if r.ok && status.success() => Ok(r),
            Some(r) => Err(InstallFailure {
                message: r
                    .message
                    .unwrap_or_else(|| "the new release's prefetch failed".into()),
                needs_owner: r.needs_owner,
                action: r.action,
            }),
            None => Err(InstallFailure::retry(format!(
                "the new release's prefetch failed ({}): {}",
                status,
                if last_err.is_empty() {
                    "no output"
                } else {
                    &last_err
                }
            ))),
        }
    }
}

impl ReleaseInstaller for Installer {
    fn install(&self, version: String) -> BoxFuture<'static, Result<String, InstallFailure>> {
        let me = self.clone();
        Box::pin(async move { me.run(version).await })
    }
}

/// An updater error as an automatic-update failure (the owner's part named).
pub fn update_failure(e: bunko_update::UpdateError) -> InstallFailure {
    use bunko_update::UpdateError as E;
    let message = e.to_string();
    match &e {
        E::BadSignature => InstallFailure::owner(
            message,
            "The release is not signed by the release key: it is not installed. If you use a mirror or a fork, check update.manifest_url (and update.public_key) in the config file.",
        ),
        E::NoSpace(_) => InstallFailure::owner(
            message,
            "Free disk space next to the mokuro-bunko executable, then wait for the next try (or install by hand).",
        ),
        E::Managed(by) => InstallFailure::owner(
            message.clone(),
            format!("This install is managed by {by}: update it there."),
        ),
        E::NoArtifact { .. } => InstallFailure::owner(
            message,
            "This release has no download for this platform/flavor: install it by hand.",
        ),
        _ => InstallFailure::retry(message),
    }
}

/// Switch the binary and the pack together (see the module docs). On an error nothing
/// has changed.
fn switch(
    staged: bunko_update::Staged,
    pack: Option<&StagedPackInfo>,
    storage: &Path,
) -> Result<(), InstallFailure> {
    let from = bunko_core::VERSION.to_string();
    let to = staged.version.clone();
    if let Err(e) = bunko_update::backup_running() {
        staged.discard();
        return Err(InstallFailure::retry(format!(
            "could not keep a copy of the running executable: {e}"
        )));
    }
    // The pack first (easy to undo), then the executable.
    let mut moved: Option<(PathBuf, PathBuf)> = None; // (installed, kept as .prev-)
    let mut placed: Option<PathBuf> = None;
    let mut prev_name = None;
    if let Some(p) = pack {
        match switch_pack(p) {
            Ok((m, dest, prev)) => {
                moved = m;
                placed = Some(dest);
                prev_name = prev;
            }
            Err(e) => {
                staged.discard();
                bunko_update::drop_previous();
                return Err(InstallFailure::retry(format!(
                    "could not switch the backend pack: {e}"
                )));
            }
        }
    }
    let undo_pack = |moved: &Option<(PathBuf, PathBuf)>,
                     placed: &Option<PathBuf>,
                     p: Option<&StagedPackInfo>| {
        if let (Some(dest), Some(p)) = (placed, p) {
            let _ = std::fs::rename(dest, &p.staging);
            let _ = std::fs::remove_dir_all(&p.staging);
        }
        if let Some((installed, kept)) = moved {
            let _ = std::fs::rename(kept, installed);
        }
    };
    let marker = Marker {
        kind: "binary".into(),
        from: from.clone(),
        to: to.clone(),
        at: bunko_update::auto::now_rfc3339(),
        pack: pack.map(|p| p.name.clone()),
        prev_pack: prev_name,
        pack_root: pack.map(|p| p.root.clone()),
        reason: None,
    };
    if let Err(e) = marker.write(storage) {
        undo_pack(&moved, &placed, pack);
        staged.discard();
        bunko_update::drop_previous();
        return Err(InstallFailure::retry(format!(
            "could not write {}: {e}",
            Marker::path(storage).display()
        )));
    }
    if let Err(e) = staged.commit() {
        undo_pack(&moved, &placed, pack);
        let _ = std::fs::remove_file(Marker::path(storage));
        bunko_update::drop_previous();
        return Err(InstallFailure::retry(format!(
            "could not replace the executable: {e}"
        )));
    }
    if let Some(p) = pack {
        // The downloaded archive is unpacked and switched in: not needed any more.
        let _ = std::fs::remove_dir_all(p.root.join(".download"));
    }
    tracing::info!(
        "Update: switched to mokuro-bunko {to}{} (kept {from} until it has loaded)",
        pack.map(|p| format!(" with its {} backend pack", p.variant))
            .unwrap_or_default()
    );
    Ok(())
}

/// Move the installed pack of the staged variant aside as `.prev-<name>` and the staged
/// one in. Returns (moved: (installed, kept)), the new directory, the kept name.
#[allow(clippy::type_complexity)]
fn switch_pack(
    p: &StagedPackInfo,
) -> std::io::Result<(Option<(PathBuf, PathBuf)>, PathBuf, Option<String>)> {
    let current = installed_pack_of(&p.root, &p.variant);
    let mut moved = None;
    let mut prev_name = None;
    if let Some(dir) = current {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let kept = p.root.join(format!(".prev-{name}"));
        let _ = std::fs::remove_dir_all(&kept);
        std::fs::rename(&dir, &kept)?;
        moved = Some((dir, kept));
        prev_name = Some(name);
    }
    let dest = p.root.join(&p.name);
    if let Err(e) = std::fs::rename(&p.staging, &dest) {
        if let Some((installed, kept)) = &moved {
            let _ = std::fs::rename(kept, installed);
        }
        return Err(e);
    }
    Ok((moved, dest, prev_name))
}

/// The installed pack directory of `variant` under `root` (any release), if any.
fn installed_pack_of(root: &Path, variant: &str) -> Option<PathBuf> {
    bunko_update::backend::installed(root)
        .into_iter()
        .find(|(_, m)| m.variant == variant)
        .map(|(d, _)| d)
}

/// What [`after_restart`] found, for the status.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StartReport {
    pub view: Option<UpdateView>,
    pub problems: Vec<Problem>,
    /// An update was switched to and its pack loads: the old release's model files may
    /// go ([`prune_models`]).
    pub proven: bool,
}

/// Exit so that the new tray starts this instance again (`migrate::Outcome::HandOver`).
/// An update this start just finished is said again by the next one ("Updated to X"):
/// its marker is written back without a pack, so nothing is checked twice.
#[cfg_attr(not(feature = "tray"), allow(dead_code))]
pub fn hand_over(storage: &Path, started: &StartReport) -> ! {
    if let Some(v) = started.view.as_ref().filter(|v| v.state == "updated")
        && let (Some(to), Some(from)) = (&v.version, &v.from)
    {
        let _ = Marker {
            kind: "binary".into(),
            from: from.clone(),
            to: to.clone(),
            at: bunko_update::auto::now_rfc3339(),
            pack: None,
            prev_pack: None,
            pack_root: None,
            reason: None,
        }
        .write(storage);
    }
    tracing::info!("handing over to the new tray: exiting; it starts this instance again");
    std::process::exit(0)
}

/// After a proven update: delete the model files this release's manifest no longer
/// names from the role's store (`<storage>/models`). Full build only.
pub fn prune_models(storage: &Path) {
    #[cfg(feature = "ocr")]
    {
        let store = bunko_ocr::models::ModelStore::new(
            bunko_ocr::models::StoreOptions {
                root: storage.join("models"),
                override_dir: None,
                download: false,
            },
            bunko_ocr::models::Manifest::builtin(),
        );
        for p in store.prune() {
            tracing::info!(
                "Automatic update: removed {} (no longer part of this release)",
                p.display()
            );
        }
    }
    #[cfg(not(feature = "ocr"))]
    let _ = storage;
}

fn view(
    state: &str,
    version: &str,
    from: Option<&str>,
    message: Option<String>,
    auto: bool,
) -> UpdateView {
    UpdateView {
        state: state.into(),
        version: Some(version.to_string()),
        from: from.map(str::to_string),
        message,
        auto,
        since: Some(bunko_update::auto::now_rfc3339()),
    }
}

/// The problem a rolled-back (or refused) version raises while it is blocked.
pub fn blocked_problem(b: &Blocked) -> Problem {
    Problem::update_needs_you(
        format!("The update to {} was rolled back: {}", b.version, b.reason),
        format!(
            "This machine stays on {}. Fix the cause, then install {} by hand ('mokuro-bunko update apply', or the admin panel's Updates card); automatic updates skip it until a newer release.",
            bunko_core::VERSION,
            b.version
        ),
    )
}

/// At start, before OCR or the processor loop: finish (or undo) an automatic update the
/// previous process made. `probe` checks the pack this role will open (a child
/// process); `Err(reason)` rolls back and does not return (it restarts into the
/// previous release). Only the full build has packs to check.
pub fn after_restart(
    storage: &Path,
    auto: bool,
    probe: impl FnOnce() -> Result<(), String>,
) -> StartReport {
    let blocked = Blocked::read(storage);
    let Some(m) = Marker::take(storage) else {
        return match blocked {
            Some(b) if bunko_update::auto::is_newer(&b.version, bunko_core::VERSION) => {
                StartReport {
                    view: Some(view(
                        "blocked",
                        &b.version,
                        None,
                        Some(b.reason.clone()),
                        auto,
                    )),
                    problems: vec![blocked_problem(&b)],
                    proven: false,
                }
            }
            Some(_) => {
                // This release is at or past the blocked one: it is history.
                Blocked::clear(storage);
                StartReport::default()
            }
            None => StartReport::default(),
        };
    };
    match m.kind.as_str() {
        "binary" if m.took(bunko_core::VERSION) => {
            if m.pack.is_some()
                && let Err(reason) = probe()
            {
                tracing::error!(
                    "Automatic update: mokuro-bunko {} cannot load its OCR backend here ({reason}); rolling back to {}",
                    m.to,
                    m.from
                );
                rollback(storage, &m, &reason);
            }
            if let (Some(root), Some(prev)) = (&m.pack_root, &m.prev_pack) {
                let _ = std::fs::remove_dir_all(root.join(format!(".prev-{prev}")));
            }
            bunko_update::drop_previous();
            Blocked::clear(storage);
            tracing::info!(
                "Automatic update: now running mokuro-bunko {} (was {}){}",
                m.to,
                m.from,
                if m.pack.is_some() {
                    "; its OCR backend pack loads"
                } else {
                    ""
                }
            );
            StartReport {
                view: Some(view(
                    "updated",
                    &m.to,
                    Some(&m.from),
                    Some(format!("updated from {}", m.from)),
                    auto,
                )),
                problems: Vec::new(),
                proven: true,
            }
        }
        "binary" => {
            // The executable did not change (a launcher started an old copy?).
            let reason = format!(
                "after installing it, this machine still started {}",
                bunko_core::VERSION
            );
            tracing::error!("Automatic update to {}: {reason}", m.to);
            let b = Blocked {
                version: m.to.clone(),
                reason: reason.clone(),
                at: bunko_update::auto::now_rfc3339(),
            };
            let _ = b.write(storage);
            StartReport {
                view: Some(view("blocked", &m.to, None, Some(reason), auto)),
                problems: vec![blocked_problem(&b)],
                proven: false,
            }
        }
        "rollback" => {
            let reason = m
                .reason
                .clone()
                .unwrap_or_else(|| "it did not start".into());
            let b = Blocked {
                version: m.from.clone(),
                reason: reason.clone(),
                at: bunko_update::auto::now_rfc3339(),
            };
            let _ = b.write(storage);
            tracing::error!(
                "Automatic update: the update to {} was rolled back ({reason}); running {} again",
                m.from,
                bunko_core::VERSION
            );
            StartReport {
                view: Some(view(
                    "blocked",
                    &m.from,
                    None,
                    Some(format!("rolled back: {reason}")),
                    auto,
                )),
                problems: vec![blocked_problem(&b)],
                proven: false,
            }
        }
        _ => StartReport::default(),
    }
}

/// Undo the switch `m` describes (pack and executable), leave a `rollback` marker and
/// restart into the previous release. Returns only when that fails (then this release
/// keeps running, without OCR, and says so).
fn rollback(storage: &Path, m: &Marker, reason: &str) {
    let reason = format!("its OCR backend failed to load on this machine: {reason}");
    if let (Some(root), Some(pack)) = (&m.pack_root, &m.pack) {
        let failed = root.join(format!(".failed-{pack}"));
        let _ = std::fs::remove_dir_all(&failed);
        let _ = std::fs::rename(root.join(pack), &failed);
        if let Some(prev) = &m.prev_pack
            && let Err(e) = std::fs::rename(root.join(format!(".prev-{prev}")), root.join(prev))
        {
            tracing::error!("rollback: could not restore the backend pack {prev}: {e}");
        }
        let _ = std::fs::remove_dir_all(&failed);
    }
    let back = Marker {
        kind: "rollback".into(),
        from: m.to.clone(),
        to: m.from.clone(),
        at: bunko_update::auto::now_rfc3339(),
        pack: m.prev_pack.clone(),
        prev_pack: None,
        pack_root: m.pack_root.clone(),
        reason: Some(reason.clone()),
    };
    let _ = Blocked {
        version: m.to.clone(),
        reason: reason.clone(),
        at: back.at.clone(),
    }
    .write(storage);
    match bunko_update::restore_previous() {
        Ok(()) => {
            let _ = back.write(storage);
            tracing::error!(
                "Automatic update: rolled back to mokuro-bunko {}; restarting into it",
                m.from
            );
            let Err(e) = bunko_update::restart();
            tracing::error!("rollback: could not restart: {e}; start mokuro-bunko again");
            std::process::exit(bunko_update::RESTART_EXIT_CODE);
        }
        Err(e) => tracing::error!(
            "rollback: could not restore mokuro-bunko {}: {e}; this release runs on without its OCR backend",
            m.from
        ),
    }
}

/// The probe child: `<exe> [-c config] install-ocr --probe [--processor]` (the pack the
/// role really opens, with this process's environment).
pub fn probe_child(who: &Who) -> Result<(), String> {
    let exe = bunko_update::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(who.args()).arg("install-ocr").arg("--probe");
    if let Who::Processor { config } = who {
        cmd.arg("--processor")
            .env("MOKURO_PROCESSOR_CONFIG", config);
    }
    let out = cmd
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run the probe: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        tracing::info!("backend probe: {line}");
    }
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("the probe failed")
        .trim()
        .trim_start_matches("Error: ")
        .trim_start_matches("the OCR backend does not load: ")
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefetch_result_line() {
        let r = PrefetchResult {
            ok: true,
            pack: Some(StagedPackInfo {
                root: "/s/backends".into(),
                staging: "/s/backends/.staging-cpu".into(),
                name: "torch-cpu-2.13.0".into(),
                variant: "cpu".into(),
            }),
            ..Default::default()
        };
        let text = serde_json::to_string(&r).unwrap();
        assert!(text.starts_with("{\"ok\":true"), "{text}");
        assert_eq!(serde_json::from_str::<PrefetchResult>(&text).unwrap(), r);
    }

    #[test]
    fn pack_switch_keeps_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write_pack = |d: &Path, version: &str| {
            std::fs::create_dir_all(d).unwrap();
            let m = serde_json::json!({
                "format": 1, "name": "torch-cpu-2.13.0", "variant": "cpu", "torch": "2.13.0",
                "target": bunko_update::TARGET, "os": "linux", "arch": "x86_64", "abi": 1,
                "library": "libbunko_torch.so", "bunko_version": version, "files": []
            });
            std::fs::write(d.join("pack.json"), m.to_string()).unwrap();
        };
        write_pack(&root.join("torch-cpu-2.13.0"), "0.7.0-alpha.1");
        write_pack(&root.join(".staging-cpu"), "0.7.0-alpha.2");
        let info = StagedPackInfo {
            root: root.to_path_buf(),
            staging: root.join(".staging-cpu"),
            name: "torch-cpu-2.13.0".into(),
            variant: "cpu".into(),
        };
        let (moved, dest, prev) = switch_pack(&info).unwrap();
        assert_eq!(prev.as_deref(), Some("torch-cpu-2.13.0"));
        assert!(moved.is_some());
        let now = std::fs::read_to_string(dest.join("pack.json")).unwrap();
        assert!(now.contains("0.7.0-alpha.2"));
        let kept = std::fs::read_to_string(root.join(".prev-torch-cpu-2.13.0/pack.json")).unwrap();
        assert!(kept.contains("0.7.0-alpha.1"));
        assert!(!root.join(".staging-cpu").exists());
    }

    #[test]
    fn a_blocked_version_is_reported_until_passed() {
        let dir = tempfile::tempdir().unwrap();
        let b = Blocked {
            version: "99.0.0".into(),
            reason: "its OCR backend failed to load on this machine: boom".into(),
            at: "x".into(),
        };
        b.write(dir.path()).unwrap();
        let r = after_restart(dir.path(), true, || Ok(()));
        assert_eq!(r.view.as_ref().unwrap().state, "blocked");
        assert!(
            r.problems[0].text.starts_with(
                "The update to 99.0.0 was rolled back: its OCR backend failed to load"
            )
        );
        assert_eq!(r.problems[0].kind.as_deref(), Some("update"));
        // A blocked version this release has passed is dropped.
        Blocked {
            version: "0.0.1".into(),
            ..b
        }
        .write(dir.path())
        .unwrap();
        assert_eq!(
            after_restart(dir.path(), true, || Ok(())),
            StartReport::default()
        );
        assert!(Blocked::read(dir.path()).is_none());
    }

    #[test]
    fn a_rollback_marker_blocks_that_version() {
        let dir = tempfile::tempdir().unwrap();
        Marker {
            kind: "rollback".into(),
            from: "99.0.0".into(),
            to: bunko_core::VERSION.into(),
            at: "x".into(),
            pack: None,
            prev_pack: None,
            pack_root: None,
            reason: Some("its OCR backend failed to load on this machine: no GPU".into()),
        }
        .write(dir.path())
        .unwrap();
        let r = after_restart(dir.path(), true, || Ok(()));
        let v = r.view.unwrap();
        assert_eq!(
            (v.state.as_str(), v.version.as_deref()),
            ("blocked", Some("99.0.0"))
        );
        assert_eq!(
            r.problems[0].text,
            "The update to 99.0.0 was rolled back: its OCR backend failed to load on this machine: no GPU"
        );
        assert!(Blocked::read(dir.path()).unwrap().blocks("99.0.0"));
    }

    #[test]
    fn a_proven_update_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("backends");
        std::fs::create_dir_all(root.join(".prev-torch-cpu-2.13.0")).unwrap();
        Marker {
            kind: "binary".into(),
            from: "0.0.1".into(),
            to: bunko_core::VERSION.into(),
            at: "x".into(),
            pack: Some("torch-cpu-2.13.0".into()),
            prev_pack: Some("torch-cpu-2.13.0".into()),
            pack_root: Some(root.clone()),
            reason: None,
        }
        .write(dir.path())
        .unwrap();
        let mut probed = false;
        let r = after_restart(dir.path(), true, || {
            probed = true;
            Ok(())
        });
        assert!(probed, "the new pack is checked");
        let v = r.view.unwrap();
        assert_eq!(v.state, "updated");
        assert_eq!(v.from.as_deref(), Some("0.0.1"));
        assert!(!root.join(".prev-torch-cpu-2.13.0").exists());
    }
}
