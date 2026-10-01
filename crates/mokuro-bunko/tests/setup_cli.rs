//! `setup` — ports of 0.5.2 tests/unit/test_setup_cli.py (answers piped on stdin, in
//! the same prompt order) plus admin creation and auto-cert.

mod common;
use common::{Env, stderr, stdout};
use predicates::prelude::*;

/// Storage answer first so the wizard never touches the default data dir.
fn answers(env: &Env, rest: &str) -> String {
    format!("{}\n{rest}", env.storage().display())
}

#[test]
fn defaults() {
    let env = Env::new();
    // storage, port (default), SSL n, admin n, registration (default), access (default), CORS n, save y
    let out = env
        .cmd()
        .arg("setup")
        .write_stdin(answers(&env, "\nn\nn\n\n\nn\ny\n"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let s = stdout(&out);
    assert!(s.starts_with("=== mokuro-bunko setup ===\n\n"), "{s}");
    assert!(
        s.contains(&format!("Storage path [{}]: ", env.data_dir().display())),
        "{s}"
    );
    assert!(
        s.contains("Server port [8080]: Enable SSL? [y/N]: Create an admin user? [Y/n]: "),
        "{s}"
    );
    assert!(
        s.contains("Registration mode (disabled, self, invite, approval) [self]: "),
        "{s}"
    );
    assert!(s.contains("\nConnectivity options:\nAccess method (lan, cloudflare, dyndns, reverse-proxy) [lan]: "), "{s}");
    assert!(s.contains("\n=== Configuration Summary ===\n"), "{s}");
    assert!(
        s.contains("Setup complete! Run 'mokuro-bunko serve' to start the server."),
        "{s}"
    );
    let v = env.config_yaml();
    assert_eq!(v["server"]["port"], 8080);
    assert_eq!(
        v["storage"]["base_path"],
        env.storage().display().to_string()
    );
    assert_eq!(v["ocr"]["generations"][0]["engine"], "hayai-nova");
    assert!(!env.storage().join("mokuro.db").exists());
}

#[test]
fn custom_port_and_invite_mode() {
    let env = Env::new();
    env.cmd()
        .arg("setup")
        .write_stdin(answers(&env, "abc\n9090\nn\nn\ninvite\n\nn\ny\n"))
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Error: 'abc' is not a valid integer.",
        ));
    let v = env.config_yaml();
    assert_eq!(v["server"]["port"], 9090);
    assert_eq!(v["registration"]["mode"], "invite");
}

#[test]
fn skip_if_exists() {
    let env = Env::new();
    env.write_config("");
    env.cmd()
        .args(["setup", "--skip-if-exists"])
        .assert()
        .success()
        .stdout(format!(
            "Config file already exists at {}, skipping setup.\n",
            env.config_path().display()
        ));
}

#[test]
fn cancel_overwrite() {
    let env = Env::new();
    std::fs::write(env.config_path(), "existing: true").unwrap();
    env.cmd().arg("setup").write_stdin("n\n").assert().success();
    assert_eq!(
        std::fs::read_to_string(env.config_path()).unwrap(),
        "existing: true"
    );
}

#[test]
fn cancel_save() {
    let env = Env::new();
    env.cmd()
        .arg("setup")
        .write_stdin(answers(&env, "\nn\nn\n\n\nn\nn\n"))
        .assert()
        .success()
        .stdout(predicate::str::ends_with("Setup cancelled.\n"));
    assert!(!env.config_path().exists());
}

#[test]
fn cors_origins() {
    let env = Env::new();
    env.cmd()
        .arg("setup")
        .write_stdin(answers(
            &env,
            "\nn\nn\n\n\ny\nhttps://custom.example.com\n\ny\n",
        ))
        .assert()
        .success();
    let origins = env.config_yaml()["cors"]["allowed_origins"].clone();
    assert!(
        origins
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o == "https://custom.example.com")
    );
    assert!(
        origins
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o == "https://reader.mokuro.app")
    );
}

#[test]
fn admin_ssl_and_dyndns() {
    let env = Env::new();
    // port default; SSL y + self-signed y; admin y, name "x" refused then "boss",
    // password short (refused) then good+confirm; registration default; dyndns duckdns;
    // CORS n; save y.
    let input = answers(
        &env,
        "\ny\n\ny\nx\nboss\nshort\nshort\ngoodpassword\ngoodpassword\n\ndyndns\n\nme.duckdns.org\nsecret-token\nn\ny\n",
    );
    let out = env.cmd().arg("setup").write_stdin(input).output().unwrap();
    let s = stdout(&out);
    assert!(out.status.success(), "{s}\n{}", stderr(&out));
    assert!(s.contains("Error: Username must be 3-32 characters"), "{s}");
    assert!(
        s.contains("Error: Password must be at least 8 characters"),
        "{s}"
    );
    assert!(s.contains("Admin user 'boss' created"), "{s}");
    let cert = env.data_dir().join("certs").join("cert.pem");
    assert!(
        s.contains(&format!("SSL certificate generated at {}", cert.display())),
        "{s}"
    );
    assert!(cert.exists());
    let v = env.config_yaml();
    assert_eq!(v["ssl"]["enabled"], true);
    assert_eq!(v["ssl"]["auto_cert"], true);
    assert_eq!(v["dyndns"]["enabled"], true);
    assert_eq!(v["dyndns"]["domain"], "me.duckdns.org");
    assert_eq!(v["dyndns"]["token"], "secret-token");
    assert_eq!(v["dyndns"]["interval"], 300);
    // The admin exists in the new storage's database.
    env.cmd()
        .args(["admin", "list-users"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "{:<20} {:<12} {:<10}",
            "boss", "admin", "active"
        )));
}

#[test]
fn eof_aborts() {
    let env = Env::new();
    env.cmd()
        .arg("setup")
        .write_stdin("")
        .assert()
        .code(1)
        .stderr("Aborted!\n");
}
