//! `doctor` — row format, retuned checks, exit codes.

mod common;
use common::{Env, stdout};

#[test]
fn healthy_storage_passes_core_checks() {
    let env = Env::new();
    env.write_config("server:\n  port: 0\n");
    let out = env.cmd().arg("doctor").output().unwrap();
    let s = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{s}");
    assert!(
        s.starts_with(&format!(
            "mokuro-bunko {} - environment diagnostics\n\n",
            env!("CARGO_PKG_VERSION")
        )),
        "{s}"
    );
    assert!(
        s.contains(&format!(
            " PASS  Config: {} - storage: {}\n",
            env.config_path().display(),
            env.storage().display()
        )),
        "{s}"
    );
    assert!(s.contains(" PASS  Port: 0 available on 127.0.0.1\n"), "{s}");
    assert!(s.contains(" PASS  Failed volumes: none recorded\n"), "{s}");
    assert!(s.contains("Disk space: "), "{s}");
    if cfg!(feature = "ocr") {
        assert!(s.contains(" PASS  Build: full ("), "{s}");
        assert!(
            s.contains(" WARN  Models: ") && s.contains(" files not downloaded yet ("),
            "{s}"
        );
        assert!(
            s.contains("        -> Run: mokuro-bunko models download"),
            "{s}"
        );
    } else {
        assert!(
            s.contains(" WARN  Build: lite (") && s.contains("OCR runs only on remote processors"),
            "{s}"
        );
    }
    assert!(
        s.trim_end().ends_with("warning(s) - see WARN lines above."),
        "{s}"
    );
    assert!(env.storage().join("library").join("thumbnails").is_dir());
    assert!(!env.storage().join(".mokuro-doctor-probe").exists());
}

#[test]
fn missing_config_uses_defaults() {
    let env = Env::new();
    let s = stdout(&env.cmd().arg("doctor").output().unwrap());
    assert!(
        s.contains(&format!(
            "{} (not found; using defaults) - storage: {}",
            env.config_path().display(),
            env.data_dir().display()
        )),
        "{s}"
    );
}

#[test]
fn bad_config_fails() {
    let env = Env::new();
    std::fs::write(env.config_path(), "server:\n  port: 99999\n").unwrap();
    let out = env.cmd().arg("doctor").output().unwrap();
    let s = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{s}");
    assert!(
        s.contains(&format!(" FAIL  Config: {}: ", env.config_path().display())),
        "{s}"
    );
    assert!(
        s.contains("        -> Fix or delete the config file, then re-run 'mokuro-bunko setup'."),
        "{s}"
    );
    assert!(!s.contains("Disk space"), "storage checks are skipped: {s}");
    assert!(
        s.trim_end()
            .ends_with("1 problem(s) found - see FAIL lines above."),
        "{s}"
    );
}

#[test]
fn unwritable_storage_fails() {
    let env = Env::new();
    let blocker = env.root().join("file");
    std::fs::write(&blocker, "x").unwrap();
    std::fs::write(
        env.config_path(),
        format!("storage:\n  base_path: {}\n", blocker.join("sub").display()),
    )
    .unwrap();
    let out = env.cmd().arg("doctor").output().unwrap();
    let s = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{s}");
    assert!(
        s.contains(" FAIL  Storage: ") && s.contains("is not writable"),
        "{s}"
    );
    assert!(
        s.contains("-> Point storage.base_path at a writable directory."),
        "{s}"
    );
}

#[test]
fn failures_and_busy_port_warn() {
    let env = Env::new();
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = busy.local_addr().unwrap().port();
    env.write_config(&format!("server:\n  port: {port}\n"));
    std::fs::create_dir_all(env.storage()).unwrap();
    std::fs::write(
        env.storage().join(".ocr-failures.json"),
        r#"{"s/v1.cbz": {"error": "x"}}"#,
    )
    .unwrap();
    let out = env.cmd().arg("doctor").output().unwrap();
    let s = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{s}");
    assert!(
        s.contains(&format!(
            " WARN  Port: {port} is in use on 127.0.0.1 - is the server already running?"
        )),
        "{s}"
    );
    assert!(
        s.contains(" WARN  Failed volumes: 1 volume(s) failing OCR (see the Queue page)"),
        "{s}"
    );
    assert!(
        s.contains(&format!(
            "        -> Full per-volume logs: {}",
            env.storage().join("logs").join("ocr").display()
        )),
        "{s}"
    );
}
