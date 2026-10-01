//! `ssl *` — ports of 0.5.2 tests/unit/test_ssl_cli.py.

mod common;
use common::{Env, stdout};
use predicates::prelude::*;

#[test]
fn enable_auto_cert_generates_and_saves() {
    let env = Env::new();
    env.write_config("");
    let cert = env.data_dir().join("certs").join("cert.pem");
    let key = env.data_dir().join("certs").join("key.pem");
    env.cmd()
        .args(["ssl", "enable", "--auto-cert"])
        .assert()
        .success()
        .stdout(format!(
            "Generating self-signed certificate...\nCertificate: {}\nKey: {}\nSSL enabled\n",
            cert.display(),
            key.display()
        ));
    assert!(cert.exists() && key.exists());
    let v = env.config_yaml();
    assert_eq!(v["ssl"]["enabled"], true);
    assert_eq!(v["ssl"]["auto_cert"], true);
    assert_eq!(v["ssl"]["cert_file"], "");

    // Existing cert: not regenerated.
    env.cmd()
        .args(["ssl", "enable", "--auto-cert"])
        .assert()
        .success()
        .stdout("SSL enabled\n");

    let s = stdout(&env.cmd().args(["ssl", "status"]).output().unwrap());
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines[0], "SSL: enabled");
    assert_eq!(lines[1], "Mode: auto-cert");
    assert_eq!(lines[2], format!("Certificate: {}", cert.display()));
    assert_eq!(lines[3], "Subject: O=mokuro-bunko,CN=localhost");
    assert!(
        lines[4].starts_with("Not before: ") && lines[4].ends_with("+00:00"),
        "{s}"
    );
    assert!(lines[5].starts_with("Not after: "), "{s}");
    assert!(lines[6].starts_with("SANs: localhost"), "{s}");
}

#[test]
fn enable_custom_cert() {
    let env = Env::new();
    env.write_config("");
    let (c, k) = (env.root().join("c.pem"), env.root().join("k.pem"));
    std::fs::write(&c, "x").unwrap();
    std::fs::write(&k, "x").unwrap();
    env.cmd()
        .args(["ssl", "enable", "--cert"])
        .arg(&c)
        .arg("--key")
        .arg(&k)
        .assert()
        .success()
        .stdout("SSL enabled\n");
    let v = env.config_yaml();
    assert_eq!(v["ssl"]["auto_cert"], false);
    assert_eq!(v["ssl"]["cert_file"], c.display().to_string());
    assert_eq!(v["ssl"]["key_file"], k.display().to_string());
    // Unparseable cert file: status reports it on stderr, exit 0.
    env.cmd()
        .args(["ssl", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Mode: custom certificate"))
        .stderr(predicate::str::starts_with("Could not read certificate: "));
}

#[test]
fn enable_argument_errors() {
    let env = Env::new();
    env.write_config("");
    env.cmd()
        .args(["ssl", "enable"])
        .assert()
        .code(1)
        .stderr("Error: Provide --auto-cert or both --cert and --key\n");
    let c = env.root().join("c.pem");
    std::fs::write(&c, "x").unwrap();
    env.cmd()
        .args(["ssl", "enable", "--cert"])
        .arg(&c)
        .assert()
        .code(1)
        .stderr("Error: Provide --auto-cert or both --cert and --key\n");
    env.cmd()
        .args(["ssl", "enable", "--auto-cert", "--cert"])
        .arg(&c)
        .assert()
        .code(1)
        .stderr("Error: Both --cert and --key are required\n");
    // click Path(exists=True): usage error.
    env.cmd()
        .args([
            "ssl",
            "enable",
            "--cert",
            "/nope/c.pem",
            "--key",
            "/nope/k.pem",
        ])
        .assert()
        .code(2);
}

#[test]
fn disable_and_status() {
    let env = Env::new();
    env.write_config("ssl:\n  enabled: true\n  auto_cert: true\n");
    env.cmd().args(["ssl", "status"]).assert().success().stdout(
        predicate::str::contains("SSL: enabled").and(predicate::str::contains(
            "Certificate file not found (will be generated on server start)",
        )),
    );
    env.cmd()
        .args(["ssl", "disable"])
        .assert()
        .success()
        .stdout("SSL disabled\n");
    let v = env.config_yaml();
    assert_eq!(v["ssl"]["enabled"], false);
    assert_eq!(v["ssl"]["auto_cert"], false);
    env.cmd()
        .args(["ssl", "status"])
        .assert()
        .success()
        .stdout("SSL: disabled\n");
}

#[test]
fn generate_and_overwrite_prompt() {
    let env = Env::new();
    let cert = env.data_dir().join("certs").join("cert.pem");
    env.cmd()
        .args(["ssl", "generate", "--hostname", "myhost.local"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with(
            "Generating self-signed certificate for 'myhost.local'...\nCertificate: ",
        ));
    assert!(cert.exists());
    std::fs::write(&cert, "existing").unwrap();
    env.cmd()
        .args(["ssl", "generate"])
        .write_stdin("n\n")
        .assert()
        .success()
        .stdout(format!(
            "Certificate already exists at {}. Overwrite? [y/N]: ",
            cert.display()
        ));
    assert_eq!(std::fs::read_to_string(&cert).unwrap(), "existing");
    env.cmd()
        .args(["ssl", "generate"])
        .write_stdin("y\n")
        .assert()
        .success();
    assert_ne!(std::fs::read_to_string(&cert).unwrap(), "existing");
}
