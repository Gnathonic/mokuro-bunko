//! Root options, `install-ocr`, `serve` flag handling, `tunnel status`, `dyndns`.

mod common;
use common::Env;
use predicates::prelude::*;

#[test]
fn version() {
    let flavor = if cfg!(feature = "ocr") {
        "full"
    } else {
        "lite"
    };
    Env::new()
        .cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with(format!(
            "mokuro-bunko, version {}\nflavor: {flavor}, target: ",
            env!("CARGO_PKG_VERSION")
        )));
}

#[test]
fn no_command_prints_help() {
    Env::new()
        .cmd()
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Usage: mokuro-bunko [OPTIONS] [COMMAND]",
        ));
}

#[test]
fn unknown_command_is_usage_error() {
    Env::new().cmd().arg("frobnicate").assert().code(2);
}

#[cfg(feature = "ocr")]
#[test]
fn install_ocr_lists_and_checks_its_sources() {
    let env = Env::new();
    env.write_config("");
    // --list: detected hardware and the pack it would install, no network.
    env.cmd()
        .args(["install-ocr", "--list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Would install: "));
    // 0.5.2's --backend values map to variants; unknown ones are refused.
    env.cmd()
        .args(["install-ocr", "--backend", "opencl"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown backend 'opencl'"));
    // --from a directory without a pack archive fails visibly (0.5.2 scripts used `|| true`).
    let empty = tempfile::tempdir().unwrap();
    env.cmd()
        .args(["install-ocr", "--backend", "cpu", "--no-models", "--from"])
        .arg(empty.path())
        .arg("--dir")
        .arg(empty.path().join("backends"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("torch-cpu.tar.zst"));
}

#[cfg(not(feature = "ocr"))]
#[test]
fn install_ocr_on_lite_says_so() {
    let env = Env::new();
    env.write_config("");
    env.cmd()
        .args(["install-ocr", "--backend", "cuda"])
        .assert()
        .success()
        .stdout(predicate::str::contains("This is the lite build"));
}

#[test]
fn serve_applies_flags_then_hands_over() {
    let env = Env::new();
    env.write_config("");
    // Storage is a FILE, so the hand-over reaches startup validation and stops there
    // (exit 2) instead of starting a server: flags parsed, verbose lines printed.
    std::fs::write(env.storage(), b"not a directory").unwrap();
    env.cmd()
        .args([
            "-v",
            "serve",
            "--port",
            "8080",
            "--host",
            "127.0.0.1",
            "--ocr",
            "cpu",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::starts_with(format!(
            "Verbose mode enabled\nStorage path: {}\nStartup validation failed: ",
            env.storage().display()
        )));
    env.cmd()
        .args(["serve", "--ocr", "mokuro"])
        .assert()
        .code(2);
    env.cmd()
        .args(["serve", "--generations", "[{\"engine\": \"nope\"}]"])
        .assert()
        .code(1)
        .stderr(predicate::str::starts_with("Error: "));
}

#[test]
fn tunnel_status_without_cloudflared() {
    Env::new()
        .cmd()
        .env("PATH", "")
        .args(["tunnel", "status"])
        .assert()
        .success()
        .stdout("cloudflared: not installed\nInstall from: https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/\n");
}

#[test]
fn dyndns_status_enable_update() {
    let env = Env::new();
    env.write_config("");
    env.cmd().args(["dyndns", "status"]).assert().success().stdout(
        "Enabled:   False\nProvider:  duckdns\nDomain:    (not set)\nToken:     (not set)\nInterval:  300s\n",
    );
    env.cmd()
        .args(["dyndns", "enable"])
        .assert()
        .success()
        .stdout("DynDNS enabled. Restart the server for changes to take effect.\n");
    assert_eq!(env.config_yaml()["dyndns"]["enabled"], true);
    env.cmd()
        .args(["dyndns", "update"])
        .assert()
        .success()
        .stderr("Error: DynDNS not configured. Run 'mokuro-bunko dyndns setup' first.\n");
    env.cmd()
        .args(["dyndns", "setup"])
        .write_stdin("generic\nhome.example\ntok\nhttps://dns.example/?ip={ip}\n10\n60\ny\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Error: DynDNS interval must be at least 30 seconds",
        ))
        .stdout(predicate::str::contains(format!(
            "DynDNS configuration saved to {}",
            env.config_path().display()
        )));
    let v = env.config_yaml();
    assert_eq!(v["dyndns"]["provider"], "generic");
    assert_eq!(v["dyndns"]["interval"], 60);
    assert_eq!(v["dyndns"]["update_url"], "https://dns.example/?ip={ip}");
    env.cmd()
        .args(["dyndns", "status"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Token:     ****\n").and(predicate::str::contains(
                "URL:       https://dns.example/?ip={ip}\n",
            )),
        );
}

/// The Windows portable zip: a command typed in its folder (README.txt's
/// `mokuro-bunko.exe install-ocr`) uses `data\` next to it, as run.bat does, not
/// %LOCALAPPDATA%; an explicit MOKURO_STORAGE / MOKURO_CONFIG still wins.
#[cfg(windows)]
#[test]
fn portable_copy_keeps_its_data_next_to_it() {
    let env = Env::new();
    let dir = env.root().join("portable copy");
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("mokuro-bunko.exe");
    std::fs::copy(assert_cmd::cargo::cargo_bin("mokuro-bunko"), &exe).unwrap();
    let local = env.root().join("local");
    let path = |extra: &[(&str, &std::path::Path)]| {
        let mut c = std::process::Command::new(&exe);
        c.args(["config", "path"])
            .env_clear()
            .env("LOCALAPPDATA", &local)
            .env("USERPROFILE", env.root())
            .env("NO_COLOR", "1");
        if let Some(root) = std::env::var_os("SystemRoot") {
            c.env("SystemRoot", root);
        }
        for (k, v) in extra {
            c.env(k, v);
        }
        let out = c.output().unwrap();
        assert!(out.status.success(), "{}", common::stderr(&out));
        common::stdout(&out)
    };
    let installed = path(&[]);
    assert!(
        installed.contains(&*local.join("mokuro-bunko").display().to_string()),
        "{installed}"
    );
    std::fs::write(dir.join("PORTABLE.txt"), "").unwrap();
    let portable = path(&[]);
    let data = dir.join("data");
    assert!(
        portable.contains(&*data.join("config.yaml").display().to_string()),
        "{portable}"
    );
    assert!(
        !portable.contains(&*local.display().to_string()),
        "{portable}"
    );
    let elsewhere = env.root().join("elsewhere");
    let pinned = path(&[("MOKURO_STORAGE", &elsewhere)]);
    assert!(
        pinned.contains(&*elsewhere.display().to_string()),
        "{pinned}"
    );
}
