//! `config *` — ports of 0.5.2 tests/unit/test_config_cli.py plus the env-baking fix.

mod common;
use common::{Env, stderr, stdout};
use predicates::prelude::*;

#[test]
fn show_defaults_when_missing() {
    let env = Env::new();
    let out = env.cmd().args(["config", "show"]).output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_yaml_ng::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["server"]["port"], 8080);
    assert_eq!(v["server"]["host"], "0.0.0.0");
    assert_eq!(v["ocr"]["generations"][0]["engine"], "hayai-nova");
}

#[test]
fn show_includes_env_overrides() {
    let env = Env::new();
    env.write_config("server:\n  port: 9090\n");
    let out = env.cmd().args(["config", "show"]).output().unwrap();
    let v: serde_json::Value = serde_yaml_ng::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["server"]["port"], 9090);
    let out = env
        .cmd()
        .env("MOKURO_PORT", "7000")
        .args(["config", "show"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_yaml_ng::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["server"]["port"], 7000);
}

#[test]
fn set_value() {
    let env = Env::new();
    env.write_config("");
    env.cmd()
        .args(["config", "set", "server.port", "9090"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Set server.port = 9090"));
    assert_eq!(env.config_yaml()["server"]["port"], 9090);
    env.cmd()
        .args(["config", "set", "registration.mode", "invite"])
        .assert()
        .success();
    env.cmd()
        .args(["config", "set", "cors.enabled", "false"])
        .assert()
        .success();
    let v = env.config_yaml();
    assert_eq!(v["registration"]["mode"], "invite");
    assert_eq!(v["cors"]["enabled"], false);
}

#[test]
fn set_creates_missing_file() {
    let env = Env::new();
    env.cmd()
        .args(["config", "set", "server.port", "9191"])
        .assert()
        .success();
    assert_eq!(env.config_yaml()["server"]["port"], 9191);
}

/// Fix over 0.5.2: env overrides in force while a writer runs are NOT saved.
#[test]
fn set_does_not_bake_env_overrides() {
    let env = Env::new();
    env.write_config("server:\n  port: 8080\n");
    env.cmd()
        .env("MOKURO_PORT", "8081")
        .env("MOKURO_STORAGE", "/data")
        .env("MOKURO_REGISTRATION_MODE", "disabled")
        .args(["config", "set", "catalog.enabled", "true"])
        .assert()
        .success();
    let v = env.config_yaml();
    assert_eq!(v["server"]["port"], 8080);
    assert_eq!(
        v["storage"]["base_path"],
        env.storage().display().to_string()
    );
    assert_eq!(v["registration"]["mode"], "self");
    assert_eq!(v["catalog"]["enabled"], true);

    env.cmd()
        .env("MOKURO_PORT", "8081")
        .args(["config", "cors-add", "https://x.example"])
        .assert()
        .success();
    assert_eq!(env.config_yaml()["server"]["port"], 8080);
}

#[test]
fn set_invalid_key_and_value() {
    let env = Env::new();
    env.write_config("");
    env.cmd()
        .args(["config", "set", "invalid.key", "value"])
        .assert()
        .code(1)
        .stderr("Error: Unknown config section: invalid\n");
    env.cmd()
        .args(["config", "set", "server.nope", "1"])
        .assert()
        .code(1)
        .stderr("Error: Unknown field 'nope' in section 'server'\n");
    env.cmd()
        .args(["config", "set", "port", "1"])
        .assert()
        .code(1)
        .stderr("Error: Invalid key: port. Expected format: section.field\n");
    env.cmd()
        .args(["config", "set", "server.port", "notanumber"])
        .assert()
        .code(1)
        .stderr(predicate::str::starts_with("Error: "));
    env.cmd()
        .args(["config", "set", "ssl.enabled", "maybe"])
        .assert()
        .code(1)
        .stderr("Error: Invalid boolean value: maybe\n");
}

#[test]
fn path_shows_file_and_storage() {
    let env = Env::new();
    env.write_config("");
    env.cmd()
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(format!(
            "Config file: {}\nStorage dir: {}\n",
            env.config_path().display(),
            env.storage().display()
        ));
}

#[test]
fn path_with_unreadable_config() {
    let env = Env::new();
    std::fs::write(env.config_path(), "ocr:\n  engines: [mokuro]\n").unwrap();
    let out = env.cmd().args(["config", "path"]).output().unwrap();
    assert!(out.status.success());
    let s = stdout(&out);
    assert!(
        s.starts_with(&format!("Config file: {}\n", env.config_path().display())),
        "{s}"
    );
    assert!(
        s.contains("Storage dir: unknown -- the config file cannot be read ("),
        "{s}"
    );
}

#[test]
fn path_honours_dash_c_over_env() {
    let env = Env::new();
    let other = env.root().join("other.yaml");
    env.cmd()
        .arg("-c")
        .arg(&other)
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with(format!(
            "Config file: {}\n",
            other.display()
        )));
}

#[test]
fn init_and_force() {
    let env = Env::new();
    env.cmd()
        .args(["config", "init"])
        .assert()
        .success()
        .stdout(format!(
            "Created config file at {}\n",
            env.config_path().display()
        ));
    assert!(env.config_yaml()["server"].is_object());
    env.cmd()
        .args(["config", "init"])
        .assert()
        .code(1)
        .stderr(format!(
            "Error: Config file already exists at {}\nUse --force to overwrite\n",
            env.config_path().display()
        ));
    env.cmd()
        .args(["config", "init", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created config file"));
}

#[test]
fn cors_add_and_remove() {
    let env = Env::new();
    env.write_config("");
    env.cmd()
        .args(["config", "cors-add", "https://example.com"])
        .assert()
        .success()
        .stdout("Added CORS origin: https://example.com\n");
    let origins = env.config_yaml()["cors"]["allowed_origins"].clone();
    assert!(
        origins
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o == "https://example.com")
    );
    env.cmd()
        .args(["config", "cors-add", "https://reader.mokuro.app"])
        .assert()
        .success()
        .stdout("Origin already allowed: https://reader.mokuro.app\n");
    env.cmd()
        .args(["config", "cors-remove", "http://localhost:5173"])
        .assert()
        .success()
        .stdout("Removed CORS origin: http://localhost:5173\n");
    let out = env
        .cmd()
        .args(["config", "cors-remove", "http://nope"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stderr(&out), "Error: Origin not found: http://nope\n");
}

#[test]
fn bad_config_is_a_clean_error() {
    let env = Env::new();
    std::fs::write(env.config_path(), "ocr:\n  char_map: true\n").unwrap();
    let out = env.cmd().args(["config", "show"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).starts_with("Error: ocr.char_map"),
        "{}",
        stderr(&out)
    );
}
