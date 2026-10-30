//! `processor-fake` (testing build only): the interop rig's remote processor. Against a
//! real server it logs in, registers, OCRs a queued volume with no models (`--page-text`
//! on each page), and the server installs and seals the sidecar.

#![cfg(feature = "testing")]

mod common;
use common::Env;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

/// A child process killed when the test ends, pass or fail.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A volume of `pages` small images.
fn cbz(pages: usize) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    {
        let mut w = zip::ZipWriter::new(&mut out);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for i in 0..pages {
            w.start_file(format!("{:03}.jpg", i + 1), opts).unwrap();
            w.write_all(&[i as u8; 64]).unwrap();
        }
        w.finish().unwrap();
    }
    out.into_inner()
}

fn add_user(env: &Env, name: &str, role: &str) {
    env.cmd()
        .args(["admin", "add-user", name, "--role", role])
        .args(["--password", "password123"])
        .assert()
        .success();
}

fn log_of(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn a_fake_processor_ocrs_a_queued_volume_and_the_server_seals_it() {
    let env = Env::new();
    let port = free_port();
    env.write_config(&format!(
        "server:\n  host: \"127.0.0.1\"\n  port: {port}\n\
         ocr:\n  local_processing: false\n  autobench: false\n  poll_interval: 1\n\
         registration:\n  mode: disabled\n"
    ));
    add_user(&env, "admin", "admin");
    add_user(&env, "fake", "processor");
    let series = env.storage().join("library").join("Series A");
    std::fs::create_dir_all(&series).unwrap();
    std::fs::write(series.join("Vol 1.cbz"), cbz(2)).unwrap();

    let server_log = env.root().join("serve.log");
    let log = std::fs::File::create(&server_log).unwrap();
    let mut server = Running(
        env.std_cmd()
            .arg("serve")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let fake_log = env.root().join("fake.log");
    let log = std::fs::File::create(&fake_log).unwrap();
    let mut fake = Running(
        env.std_cmd()
            .args([
                "processor-fake",
                "--url",
                &format!("http://127.0.0.1:{port}"),
            ])
            .args(["--username", "fake", "--password", "password123"])
            .arg("--storage")
            .arg(env.root().join("processor"))
            .args(["--engine-revision", "2"])
            .args(["--page-text", "偽の文字"])
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    );

    let sidecar = series.join("Vol 1.mokuro");
    let start = Instant::now();
    while !sidecar.exists() {
        let exited = [&mut server, &mut fake]
            .into_iter()
            .find_map(|c| c.0.try_wait().unwrap());
        assert!(
            exited.is_none() && start.elapsed() < Duration::from_secs(120),
            "no sidecar ({exited:?} after {:?})\n--- server\n{}\n--- processor-fake\n{}",
            start.elapsed(),
            log_of(&server_log),
            log_of(&fake_log)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&sidecar).unwrap()).unwrap();
    assert_eq!(v["volume"], "Vol 1");
    assert_eq!(v["pages"].as_array().unwrap().len(), 2);
    // `--page-text`: each page's one block, kept through the server's install.
    assert_eq!(
        v["pages"][1]["blocks"][0]["lines"],
        serde_json::json!(["偽の文字"]),
        "{v}"
    );
    let engine = &v["ocr_engine"];
    assert_eq!(engine["id"], bunko_core::engines::DEFAULT_ENGINE, "{v}");
    assert_eq!(engine["revision"], 2, "{v}");
    assert!(sealed(engine), "the server's provenance seal: {engine}");
}

/// Bunko's seal on an installed sidecar: the byte seal (`content_sha256`, 64 hex) until
/// the content seal (`seal`, `v1:` + 64 hex) replaces it.
fn sealed(engine: &serde_json::Value) -> bool {
    let hex64 = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
    engine["content_sha256"].as_str().is_some_and(hex64)
        || engine["seal"]
            .as_str()
            .and_then(|s| s.strip_prefix("v1:"))
            .is_some_and(hex64)
}
