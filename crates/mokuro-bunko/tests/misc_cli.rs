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

#[test]
fn install_ocr_is_a_deprecated_alias() {
    let env = Env::new();
    env.write_config("");
    env.cmd()
        .args(["install-ocr", "--backend", "cuda", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with(
            "install-ocr is deprecated: OCR is built into mokuro-bunko",
        ));
}

#[test]
fn serve_applies_flags_then_hands_over() {
    let env = Env::new();
    env.write_config("");
    // The serve body is a stub until the orchestrator wires it: flags must still parse
    // and be validated before the hand-over.
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
        .code(1)
        .stdout(format!(
            "Verbose mode enabled\nStorage path: {}\n",
            env.storage().display()
        ))
        .stderr("Error: not wired yet\n");
    env.cmd()
        .args(["serve", "--ocr", "mokuro"])
        .assert()
        .code(2);
    env.cmd()
        .args(["serve", "--generations", "[{\"engine\": \"nope\"}]"])
        .assert()
        .code(1)
        .stderr(
            predicate::str::starts_with("Error: ").and(predicate::str::contains("not wired").not()),
        );
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
