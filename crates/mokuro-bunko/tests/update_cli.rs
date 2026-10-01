//! `update check|apply` — status lines, install-kind refusals, unreachable manifests.

mod common;
use common::{Env, stderr, stdout};
use predicates::prelude::*;

fn env_with_dead_manifest() -> Env {
    let env = Env::new();
    // Port 1 on loopback: connection refused, no network needed.
    env.write_config("update:\n  manifest_url: http://127.0.0.1:1/release.json\n");
    env
}

#[test]
fn check_reports_and_fails_when_unreachable() {
    let env = env_with_dead_manifest();
    let out = env
        .cmd()
        .env("MOKURO_INSTALL_KIND", "docker")
        .args(["update", "check"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let s = stdout(&out);
    assert_eq!(
        s,
        format!(
            "Current version: {}\nLatest version:  unknown\nUpdate available: no\nInstall kind: docker\n",
            env!("CARGO_PKG_VERSION")
        )
    );
    assert!(
        stderr(&out).starts_with("Error: could not check for updates: "),
        "{}",
        stderr(&out)
    );
}

#[test]
fn check_describes_install_kinds() {
    let env = env_with_dead_manifest();
    env.cmd()
        .env("MOKURO_INSTALL_KIND", "apt")
        .args(["update", "check"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("Install kind: managed by apt\n"));
    env.cmd()
        .env("MOKURO_INSTALL_KIND", "self")
        .args(["update", "check"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("Install kind: self-managed ("));
}

#[test]
fn manifest_url_env_override() {
    let env = Env::new();
    env.write_config("");
    env.cmd()
        .env("MOKURO_INSTALL_KIND", "docker")
        .env("MOKURO_UPDATE_MANIFEST_URL", "http://127.0.0.1:1/x.json")
        .args(["update", "check"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("127.0.0.1:1"));
}

#[test]
fn apply_refuses_unmanaged_installs() {
    let env = env_with_dead_manifest();
    env.cmd()
        .env("MOKURO_INSTALL_KIND", "docker")
        .args(["update", "apply", "--yes"])
        .assert()
        .code(1)
        .stderr("Error: this is a Docker install: pull the new image and recreate the container\n");
    env.cmd()
        .env("MOKURO_INSTALL_KIND", "the system package manager")
        .args(["update", "apply", "--yes"])
        .assert()
        .code(1)
        .stderr("Error: this install is managed by the system package manager: update it there\n");
}

#[test]
fn apply_self_managed_fails_cleanly_when_unreachable() {
    let env = env_with_dead_manifest();
    env.cmd()
        .env("MOKURO_INSTALL_KIND", "self")
        .args(["update", "apply", "--yes"])
        .assert()
        .code(1)
        .stderr(predicate::str::starts_with(
            "Error: could not check for updates: ",
        ));
}
