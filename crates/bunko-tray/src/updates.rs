//! "Check for updates": runs the existing updater (`mokuro-bunko update check`, which
//! verifies the signed release manifest) and reads its answer.

use crate::model::UpdateView;
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

/// Parse `update check`'s output ("Latest version: …", "Update available: yes|no").
pub fn parse_check(stdout: &str) -> UpdateView {
    let mut view = UpdateView::default();
    for line in stdout.lines() {
        if let Some(v) = line.strip_prefix("Latest version:") {
            let v = v.trim();
            if !v.is_empty() && v != "unknown" {
                view.latest = Some(v.to_string());
            }
        } else if let Some(v) = line.strip_prefix("Update available:") {
            view.available = v.trim() == "yes";
        }
    }
    if view.latest.is_none() && !view.available {
        view.error = Some("no answer from the update check".into());
    }
    view
}

pub fn check(cli: &Path, env: &[(String, OsString)]) -> UpdateView {
    let mut cmd = Command::new(cli);
    cmd.args(["update", "check"])
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    match cmd.output() {
        Ok(out) => {
            let mut view = parse_check(&String::from_utf8_lossy(&out.stdout));
            if !out.status.success() && view.error.is_none() && view.latest.is_none() {
                view.error = Some(String::from_utf8_lossy(&out.stderr).trim().to_string());
            }
            view
        }
        Err(e) => UpdateView {
            error: Some(format!("could not run {}: {e}", cli.display())),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_update_check() {
        let v = parse_check(
            "Current version: 0.7.0\nLatest version:  0.7.1\nUpdate available: yes\nInstall kind: self-managed (/x)\n",
        );
        assert!(v.available);
        assert_eq!(v.latest.as_deref(), Some("0.7.1"));
        let v =
            parse_check("Current version: 0.7.0\nLatest version:  0.7.0\nUpdate available: no\n");
        assert!(!v.available && v.error.is_none());
        let v =
            parse_check("Current version: 0.7.0\nLatest version:  unknown\nUpdate available: no\n");
        assert!(v.error.is_some());
    }
}
