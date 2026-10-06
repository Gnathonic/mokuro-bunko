//! `update check|apply`: the signed-release updater from the command line.
//!
//! `apply` only replaces a self-managed binary; Docker, system packages and app stores
//! are refused with what to do instead. It never touches a running server: it installs,
//! then says to restart — or, with `--restart`, replaces this process with the new
//! binary's `serve` (same global options), for scripts that update and start in one go.

use super::Ctx;
use crate::cfgfile;
use crate::cli::UpdateCmd;
use crate::out::{CmdResult, Fail, exit_with, runtime};
use crate::prompt;
use bunko_update::{InstallKind, UpdateStatus, Updater};

pub fn run(ctx: &Ctx, cmd: UpdateCmd) -> CmdResult {
    if let UpdateCmd::Prefetch {
        processor_config,
        manifest_url,
    } = cmd
    {
        return prefetch(ctx, processor_config, &manifest_url);
    }
    let config = cfgfile::load_effective(&ctx.config_path)?;
    crate::logging::init_console(ctx.verbose);
    let (key, custom) = bunko_update::auto::release_key(&config.update.public_key);
    if custom {
        eprintln!(
            "Warning: {}",
            bunko_update::auto::custom_key_warning(&key, "update.public_key in the config file")
        );
    }
    let updater = Updater::new(
        config.update.manifest_url.clone(),
        config.update.channel.clone(),
        crate::update_flavor(),
    )
    .with_public_key(key);
    match cmd {
        UpdateCmd::Check => check(&updater),
        UpdateCmd::Apply { yes, restart } => apply(ctx, &config, &updater, yes, restart),
        UpdateCmd::Prefetch { .. } => unreachable!("handled above"),
    }
}

/// `update prefetch` (hidden): see `crate::autoupdate`. One JSON result line last.
fn prefetch(
    ctx: &Ctx,
    processor_config: Option<std::path::PathBuf>,
    manifest_url: &str,
) -> CmdResult {
    crate::logging::init_console(ctx.verbose);
    #[cfg(feature = "ocr")]
    let result = {
        let target = match &processor_config {
            Some(p) => {
                let cfg = bunko_processor::load_processor_config(p).map_err(Fail::msg)?;
                crate::ocr_target::OcrTarget {
                    role: crate::ocr_target::Role::Processor,
                    storage: cfg.processor.storage,
                    processor_config: Some(p.clone()),
                    library: None,
                    reason: String::new(),
                }
            }
            None => {
                let config = cfgfile::load_effective(&ctx.config_path)?;
                crate::ocr_target::OcrTarget {
                    role: crate::ocr_target::Role::Library,
                    storage: config.storage.base_path.clone(),
                    processor_config: None,
                    library: Some(config),
                    reason: String::new(),
                }
            }
        };
        super::install_ocr::prefetch(&target, manifest_url)
    };
    #[cfg(not(feature = "ocr"))]
    let result = {
        // The lite build has no OCR backend or models to bring along.
        let _ = (processor_config, manifest_url);
        crate::autoupdate::PrefetchResult {
            ok: true,
            ..Default::default()
        }
    };
    println!("{}", serde_json::to_string(&result).unwrap_or_default());
    match (result.ok, result.needs_owner) {
        (true, _) => Ok(()),
        (false, true) => Err(Fail::Exit(crate::autoupdate::PREFETCH_NEEDS_OWNER)),
        (false, false) => Err(Fail::Exit(1)),
    }
}

pub fn describe_install(kind: &InstallKind) -> String {
    match kind {
        InstallKind::SelfManaged { exe } => format!("self-managed ({})", exe.display()),
        InstallKind::Docker => "docker".into(),
        InstallKind::Managed { by } => format!("managed by {by}"),
        InstallKind::Mobile => "mobile app".into(),
    }
}

fn print_status(s: &UpdateStatus) {
    println!("Current version: {}", s.current);
    println!(
        "Latest version:  {}",
        s.latest.as_deref().unwrap_or("unknown")
    );
    println!(
        "Update available: {}",
        if s.available { "yes" } else { "no" }
    );
    println!("Install kind: {}", describe_install(&s.install));
    if let Some(notes) = &s.notes_url {
        println!("Release notes: {notes}");
    }
    if s.available {
        match &s.install {
            InstallKind::Docker => {
                if let Some(image) = &s.docker_image {
                    println!("Docker image: {image}");
                }
            }
            _ if s.can_apply => println!("Install it with: mokuro-bunko update apply"),
            _ => {}
        }
    }
}

fn check(updater: &Updater) -> CmdResult {
    let status = runtime()?.block_on(updater.check());
    print_status(&status);
    match status.error {
        Some(e) => Err(exit_with(format!(
            "Error: could not check for updates: {e}"
        ))),
        None => Ok(()),
    }
}

/// Why this install cannot update itself, or `None` when it can.
fn refusal(kind: &InstallKind) -> Option<String> {
    match kind {
        InstallKind::SelfManaged { .. } => None,
        InstallKind::Docker => {
            Some("this is a Docker install: pull the new image and recreate the container".into())
        }
        InstallKind::Managed { by } => {
            Some(format!("this install is managed by {by}: update it there"))
        }
        InstallKind::Mobile => Some("this app updates through its app store".into()),
    }
}

fn apply(
    ctx: &Ctx,
    config: &bunko_core::Config,
    updater: &Updater,
    yes: bool,
    restart: bool,
) -> CmdResult {
    let kind = InstallKind::detect();
    if let Some(why) = refusal(&kind) {
        return Err(Fail::msg(why));
    }
    let rt = runtime()?;
    let status = rt.block_on(updater.check());
    if let Some(e) = status.error {
        return Err(Fail::msg(format!("could not check for updates: {e}")));
    }
    let latest = status.latest.clone().unwrap_or_default();
    if !status.available {
        println!("mokuro-bunko {} is up to date.", status.current);
        return Ok(());
    }
    if !status.can_apply {
        return Err(Fail::msg(format!(
            "release {latest} has no {} build for {}",
            crate::update_flavor(),
            bunko_update::TARGET
        )));
    }
    if !yes
        && !prompt::confirm(
            &format!("Install mokuro-bunko {latest} over {}?", status.current),
            Some(false),
        )?
    {
        println!("Update cancelled.");
        return Ok(());
    }
    println!("Downloading mokuro-bunko {latest} (with its OCR backend pack and models)...");
    // The release as one unit, as the automatic update installs it: the binary, its
    // backend pack for the variant installed here, its models.
    let installer = crate::autoupdate::Installer::new(
        &config.update.manifest_url,
        &config.update.channel,
        &config.update.public_key,
        crate::autoupdate::Who::Library {
            cli_config: Some(ctx.config_path.clone()),
        },
        &config.storage.base_path,
    );
    let installed = rt
        .block_on(bunko_update::auto::ReleaseInstaller::install(
            &installer,
            latest.clone(),
        ))
        .map_err(|f| match f.action {
            Some(a) => Fail::msg(format!("{} ({a})", f.message)),
            None => Fail::msg(f.message),
        })?;
    bunko_update::auto::Blocked::clear(&config.storage.base_path);
    println!("Installed mokuro-bunko {installed}.");
    if !restart {
        println!("Restart mokuro-bunko to run the new version.");
        return Ok(());
    }
    println!("Starting the new server...");
    exec_serve(ctx)
}

/// Replace this process with `<exe> [-c PATH] [-v] serve`.
fn exec_serve(ctx: &Ctx) -> CmdResult {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    if let Some(c) = &ctx.cli_config {
        cmd.arg("-c").arg(c);
    }
    if ctx.verbose {
        cmd.arg("-v");
    }
    cmd.arg("serve");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(Fail::msg(cmd.exec()))
    }
    #[cfg(not(unix))]
    {
        cmd.spawn()?;
        Ok(())
    }
}
