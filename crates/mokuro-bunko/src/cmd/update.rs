//! `update check|apply`: the signed-release updater from the command line.
//!
//! `apply` only replaces a self-managed binary; Docker, system packages and app stores
//! are refused with what to do instead. It never touches a running server: it installs,
//! then says to restart — or, with `--restart`, replaces this process with the new
//! binary's `serve` (same global options), for scripts that update and start in one go.

use super::Ctx;
use crate::FLAVOR;
use crate::cfgfile;
use crate::cli::UpdateCmd;
use crate::out::{CmdResult, Fail, exit_with, runtime};
use crate::prompt;
use bunko_update::{InstallKind, UpdateStatus, Updater};

pub fn run(ctx: &Ctx, cmd: UpdateCmd) -> CmdResult {
    let config = cfgfile::load_effective(&ctx.config_path)?;
    crate::logging::init_console(ctx.verbose);
    let updater = Updater::new(
        config.update.manifest_url.clone(),
        config.update.channel.clone(),
        FLAVOR,
    );
    match cmd {
        UpdateCmd::Check => check(&updater),
        UpdateCmd::Apply { yes, restart } => apply(ctx, &updater, yes, restart),
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

fn apply(ctx: &Ctx, updater: &Updater, yes: bool, restart: bool) -> CmdResult {
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
            "release {latest} has no {FLAVOR} build for {}",
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
    println!("Downloading mokuro-bunko {latest}...");
    let installed = rt.block_on(updater.apply()).map_err(Fail::msg)?;
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
