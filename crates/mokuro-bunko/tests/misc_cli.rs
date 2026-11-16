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

/// No arguments starts the app (src/no_args.rs). Headless with nothing set up and no
/// terminal to run the setup in: a short message, exit 0.
#[test]
fn no_arguments_headless_and_nothing_set_up_says_what_to_do() {
    Env::new()
        .cmd()
        .env("MOKURO_DESKTOP", "0")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Nothing is set up on this machine yet")
                .and(predicate::str::contains("mokuro-bunko setup"))
                .and(predicate::str::contains("mokuro-bunko --help")),
        );
}

#[test]
fn help_prints_the_help() {
    for arg in ["--help", "help"] {
        Env::new()
            .cmd()
            .arg(arg)
            .assert()
            .success()
            // `mokuro-bunko.exe` on Windows (clap names the program as it was started).
            .stdout(
                predicate::str::contains("Usage: mokuro-bunko")
                    .and(predicate::str::contains(" [OPTIONS] [COMMAND]")),
            );
    }
}

/// No arguments, headless, a library set up: it serves, in the foreground.
#[test]
fn no_arguments_headless_runs_the_configured_library() {
    use std::io::{Read, Write};
    let env = Env::new();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    env.write_config(&format!(
        "server:\n  host: 127.0.0.1\n  port: {port}\nocr:\n  local_processing: false\n  autobench: false\n"
    ));
    let out_path = env.root().join("noargs.out");
    let mut child = env
        .std_cmd()
        .env("MOKURO_DESKTOP", "0")
        .stdout(std::fs::File::create(&out_path).unwrap())
        .stderr(std::fs::File::create(env.root().join("noargs.err")).unwrap())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut up = false;
    while std::time::Instant::now() < deadline && !up {
        if let Ok(Some(status)) = child.try_wait() {
            panic!(
                "exited ({status}): {}",
                std::fs::read_to_string(&out_path).unwrap_or_default()
            );
        }
        if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            let _ = s.write_all(b"GET /api/health HTTP/1.0\r\nHost: x\r\n\r\n");
            let mut answer = String::new();
            let _ = s.read_to_string(&mut answer);
            up = answer.starts_with("HTTP/1.");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(up, "the configured library was not served");
}

#[test]
fn unknown_command_is_usage_error() {
    Env::new().cmd().arg("frobnicate").assert().code(2);
}

/// The rig's fake processor is in the testing build only, and never in the help.
#[test]
fn processor_fake_is_hidden_and_only_in_the_testing_build() {
    Env::new()
        .cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("processor-fake").not());
    let fake = Env::new().cmd().args(["processor-fake", "--help"]).assert();
    if cfg!(feature = "testing") {
        fake.success();
    } else {
        fake.code(2)
            .stderr(predicate::str::contains("unrecognized subcommand"));
    }
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
        .stderr(
            predicate::str::contains("mokuro-bunko-backend-")
                .and(predicate::str::contains("-cpu.tar.zst")),
        );
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

/// Regression (upgrade test): the config migration's warnings (a retired `mokuro` row, a
/// `ctd` detector switched to `ppocr-manga`, hayai made primary) were printed twice on
/// the console at startup: by the config loader and again by the server's log.
#[test]
fn serve_says_each_config_warning_once() {
    use std::io::{Read, Write};
    let env = Env::new();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    env.write_config(&format!(
        "server:\n  host: 127.0.0.1\n  port: {port}\nocr:\n  local_processing: false\n  autobench: false\n  generations:\n    - {{name: mokuro, engine: mokuro, primary: true, enabled: true}}\n    - {{name: hayai, engine: hayai-nova, detector: ctd, primary: false, enabled: true}}\n"
    ));
    let out_path = env.root().join("serve.out");
    let err_path = env.root().join("serve.err");
    let mut child = env
        .std_cmd()
        .arg("serve")
        .stdout(std::fs::File::create(&out_path).unwrap())
        .stderr(std::fs::File::create(&err_path).unwrap())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut up = false;
    while std::time::Instant::now() < deadline && !up {
        if let Ok(Some(status)) = child.try_wait() {
            panic!(
                "serve exited ({status}): {}{}",
                std::fs::read_to_string(&out_path).unwrap_or_default(),
                std::fs::read_to_string(&err_path).unwrap_or_default()
            );
        }
        if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            let _ = s.write_all(b"GET /api/health HTTP/1.0\r\nHost: x\r\n\r\n");
            let mut answer = String::new();
            let _ = s.read_to_string(&mut answer);
            up = answer.starts_with("HTTP/1.");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = child.kill();
    let _ = child.wait();
    let console = format!(
        "{}{}",
        std::fs::read_to_string(&out_path).unwrap(),
        std::fs::read_to_string(&err_path).unwrap()
    );
    assert!(up, "the server never answered: {console}");
    for warning in [
        "The row is kept but no longer runs",
        "this row now reads with the ppocr-manga detector",
        "is now the primary generation",
    ] {
        assert_eq!(console.matches(warning).count(), 1, "{warning}: {console}");
    }
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

/// A fake `cloudflared` on PATH that prints a quick-tunnel URL on stderr as the real one
/// does, then writes `marker` a moment later and exits. The marker exists only when
/// `tunnel cloudflare` let it run to the end instead of killing it after the URL.
#[cfg(unix)]
fn fake_cloudflared(env: &Env) -> (std::process::Command, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let bin = env.root().join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let marker = env.root().join("cloudflared-ran-to-the-end");
    let script = bin.join("cloudflared");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             echo 'INF |  https://fake-tunnel.trycloudflare.com  |' >&2\n\
             sleep 1\n\
             echo done > '{}'\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut cmd = env.std_cmd();
    cmd.env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .stdin(std::process::Stdio::null());
    (cmd, marker)
}

#[cfg(unix)]
const FAKE_TUNNEL_URL: &str = "https://fake-tunnel.trycloudflare.com";

/// A line of cloudflared's log that is not UTF-8 failed the read, and the command ended
/// with an error that took cloudflared down with it.
#[cfg(unix)]
#[test]
fn tunnel_cloudflare_keeps_running_past_a_log_line_that_is_not_utf8() {
    let env = Env::new();
    env.write_config("");
    let (mut cmd, marker) = fake_cloudflared(&env);
    let script = env.root().join("fake-bin").join("cloudflared");
    let body = std::fs::read_to_string(&script).unwrap();
    std::fs::write(
        &script,
        body.replacen(
            "#!/bin/sh\n",
            "#!/bin/sh\nprintf 'INF \\377 not utf-8\\n' >&2\n",
            1,
        ),
    )
    .unwrap();
    let out = cmd
        .args(["tunnel", "cloudflare", "--cors", "no"])
        .output()
        .unwrap();
    let (stdout, stderr) = (common::stdout(&out), common::stderr(&out));
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stderr.contains("not utf-8"), "{stderr}");
    assert!(
        stdout.contains(&format!("Tunnel URL: {FAKE_TUNNEL_URL}")),
        "{stdout}"
    );
    assert!(
        marker.exists(),
        "cloudflared was stopped early: {stdout}{stderr}"
    );
}

/// A tunnel started without a terminal (detached, stdin /dev/null) used to read end of
/// input at the CORS question, abort, and take cloudflared down with it.
#[cfg(unix)]
#[test]
fn tunnel_cloudflare_without_a_terminal_keeps_running_and_asks_nothing() {
    for args in [
        &["tunnel", "cloudflare"][..],
        &["tunnel", "cloudflare", "--cors", "ask"],
    ] {
        let env = Env::new();
        env.write_config("");
        let before = std::fs::read_to_string(env.config_path()).unwrap();
        let (mut cmd, marker) = fake_cloudflared(&env);
        let out = cmd.args(args).output().unwrap();
        let (stdout, stderr) = (common::stdout(&out), common::stderr(&out));
        assert!(out.status.success(), "{args:?}: {stdout}{stderr}");
        assert!(
            stdout.contains(&format!("Tunnel URL: {FAKE_TUNNEL_URL}")),
            "{stdout}"
        );
        assert!(
            stdout.contains(
                "Not adding the tunnel URL to CORS origins (stdin is not a terminal); \
                 pass --cors yes to add it."
            ),
            "{stdout}"
        );
        assert!(!stdout.contains("[Y/n]"), "{stdout}");
        assert!(!stderr.contains("Aborted!"), "{stderr}");
        assert!(
            marker.exists(),
            "cloudflared was stopped early: {stdout}{stderr}"
        );
        assert_eq!(std::fs::read_to_string(env.config_path()).unwrap(), before);
    }
}

#[cfg(unix)]
#[test]
fn tunnel_cloudflare_cors_no_adds_nothing() {
    let env = Env::new();
    env.write_config("");
    let before = std::fs::read_to_string(env.config_path()).unwrap();
    let (mut cmd, marker) = fake_cloudflared(&env);
    let out = cmd
        .args(["tunnel", "cloudflare", "--cors", "no"])
        .output()
        .unwrap();
    let (stdout, stderr) = (common::stdout(&out), common::stderr(&out));
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(
        stdout.contains(&format!("Not adding {FAKE_TUNNEL_URL} to CORS origins")),
        "{stdout}"
    );
    assert!(!stderr.contains("Aborted!"), "{stderr}");
    assert!(
        marker.exists(),
        "cloudflared was stopped early: {stdout}{stderr}"
    );
    assert_eq!(std::fs::read_to_string(env.config_path()).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn tunnel_cloudflare_cors_yes_adds_the_url_without_asking() {
    let env = Env::new();
    env.write_config("");
    for expected in [
        format!("Added {FAKE_TUNNEL_URL} to CORS origins"),
        format!("{FAKE_TUNNEL_URL} is already a CORS origin"),
    ] {
        let (mut cmd, marker) = fake_cloudflared(&env);
        let _ = std::fs::remove_file(&marker);
        let out = cmd
            .args(["tunnel", "cloudflare", "--cors", "yes"])
            .output()
            .unwrap();
        let (stdout, stderr) = (common::stdout(&out), common::stderr(&out));
        assert!(out.status.success(), "{stdout}{stderr}");
        assert!(stdout.contains(&expected), "{stdout}");
        assert!(!stdout.contains("[Y/n]"), "{stdout}");
        assert!(
            marker.exists(),
            "cloudflared was stopped early: {stdout}{stderr}"
        );
        let origins = env.config_yaml()["cors"]["allowed_origins"].clone();
        let origins = origins.as_array().unwrap();
        assert_eq!(
            origins.iter().filter(|o| *o == FAKE_TUNNEL_URL).count(),
            1,
            "{origins:?}"
        );
    }
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
