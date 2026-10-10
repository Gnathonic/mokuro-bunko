//! `serve` installs a missing OCR backend in the background: the server answers at
//! once while the install hangs on a slow download, the control API shows the install
//! running, a failed install is a problem with a retry (`POST /control/ocr-install`).
#![cfg(feature = "ocr")]

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// A release server that holds every request until released, then answers 503.
struct SlowRelease {
    port: u16,
    release: Arc<AtomicBool>,
    hits: Arc<AtomicUsize>,
}

fn slow_release() -> SlowRelease {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let release = Arc::new(AtomicBool::new(false));
    let hits = Arc::new(AtomicUsize::new(0));
    let (r, h) = (release.clone(), hits.clone());
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let (r, h) = (r.clone(), h.clone());
            std::thread::spawn(move || {
                let _ = read_head(&mut conn);
                h.fetch_add(1, Ordering::SeqCst);
                while !r.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = conn.write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            });
        }
    });
    SlowRelease {
        port,
        release,
        hits,
    }
}

fn read_head(conn: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if conn.read(&mut byte)? == 0 {
            break;
        }
        buf.push(byte[0]);
    }
    Ok(buf)
}

/// A plain HTTP/1.1 request: (status, body).
fn http(port: u16, method: &str, path: &str, bearer: Option<&str>) -> Option<(u16, String)> {
    let mut conn = TcpStream::connect(("127.0.0.1", port)).ok()?;
    conn.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    let auth = bearer
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    write!(
        conn,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut text = String::new();
    conn.read_to_string(&mut text).ok()?;
    let status = text.split(' ').nth(1)?.parse().ok()?;
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string())?;
    // Chunked bodies (axum's JSON is sized; keep it simple): strip a chunk header.
    let body = match body.split_once("\r\n") {
        Some((size, rest)) if u64::from_str_radix(size.trim(), 16).is_ok() => {
            rest.trim_end_matches("\r\n0\r\n\r\n").to_string()
        }
        _ => body,
    };
    Some((status, body))
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait<T>(secs: u64, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

struct Killed(std::process::Child);
impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn serves_at_once_while_the_backend_installs_and_a_failure_is_a_problem() {
    let env = common::Env::new();
    let port = free_port();
    env.write_config(&format!(
        "server:\n  host: 127.0.0.1\n  port: {port}\nupdate:\n  check: false\nocr:\n  local_processing: true\n  autobench: false\n  generations:\n    - {{name: hayai, engine: hayai-nova, primary: true, enabled: true}}\n"
    ));
    let release = slow_release();
    let log = env.root().join("serve.log");
    let out = std::fs::File::create(&log).unwrap();
    let started = Instant::now();
    let child = env
        .std_cmd()
        .arg("serve")
        .env("MOKURO_OCR_AUTO_INSTALL", "true")
        .env(
            "MOKURO_BACKEND_MANIFEST",
            format!("http://127.0.0.1:{}/release.json", release.port),
        )
        .env("MOKURO_MODELS_DOWNLOAD", "0")
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .unwrap();
    let _server = Killed(child);
    let show = || std::fs::read_to_string(&log).unwrap_or_default();

    // The server answers while the install hangs on its download.
    wait(60, "/api/health", || {
        http(port, "GET", "/api/health", None).filter(|(s, _)| *s == 200)
    });
    let up = started.elapsed();
    wait(30, "the install to reach the release", || {
        (release.hits.load(Ordering::SeqCst) > 0).then_some(())
    });
    let control = wait(30, ".control.json", || {
        bunko_control::read_control_file(&env.storage())
    });
    let status = |control: &bunko_control::ControlFile| -> serde_json::Value {
        let (code, body) = http(control.port, "GET", "/control/status", Some(&control.token))
            .unwrap_or((0, String::new()));
        assert_eq!(code, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    };
    let s = status(&control);
    assert_eq!(s["install"]["state"], "running", "{s}\n{}", show());
    assert!(
        http(port, "GET", "/api/health", None).is_some_and(|(c, _)| c == 200),
        "still serving while it installs"
    );
    assert!(
        up < Duration::from_secs(60),
        "served after {up:?}, not after the install"
    );

    // The download fails: a problem ("needs you") with a retry.
    release.release.store(true, Ordering::SeqCst);
    let s = wait(60, "the failed install", || {
        let s = status(&control);
        (s["install"]["state"] == "failed").then_some(s)
    });
    let problems = s["problems"].as_array().cloned().unwrap_or_default();
    assert!(
        problems
            .iter()
            .any(|p| p["kind"] == "ocr-install" && p["severity"] == "fail"),
        "{s}"
    );
    assert!(show().contains("OCR install failed"), "{}", show());
    let (code, body) = http(
        control.port,
        "POST",
        "/control/ocr-install",
        Some(&control.token),
    )
    .expect("retry");
    assert_eq!(code, 202, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["installing"], true, "{v}");
    let hits = release.hits.load(Ordering::SeqCst);
    wait(60, "the retry to reach the release", || {
        (release.hits.load(Ordering::SeqCst) > hits).then_some(())
    });
    wait(60, "the retry to fail again", || {
        (status(&control)["install"]["state"] == "failed").then_some(())
    });
    // The server never stopped answering.
    assert!(http(port, "GET", "/api/health", None).is_some_and(|(c, _)| c == 200));
}
