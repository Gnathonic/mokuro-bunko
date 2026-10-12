//! `healthcheck` — GET /api/health on the configured port; exit 0 on 2xx, 1 otherwise.

mod common;
use common::Env;
use predicates::prelude::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

/// A one-shot HTTP server answering `status`; sends back the request head it saw.
fn serve_once(status: &'static str) -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
            let body = "{\"status\":\"ok\"}";
            let _ = write!(
                sock,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (port, rx)
}

#[test]
fn healthy_server() {
    let env = Env::new();
    let (port, rx) = serve_once("200 OK");
    env.write_config(&format!("server:\n  port: {port}\n"));
    env.cmd()
        .arg("healthcheck")
        .assert()
        .success()
        .stdout(format!(
            "healthy: http://127.0.0.1:{port}/api/health (200)\n"
        ));
    let req = rx.recv().unwrap();
    assert!(req.starts_with("GET /api/health HTTP/1.1\r\n"), "{req}");
}

#[test]
fn port_from_env_override() {
    let env = Env::new();
    let (port, _rx) = serve_once("200 OK");
    env.write_config("server:\n  port: 1\n");
    env.cmd()
        .env("MOKURO_PORT", port.to_string())
        .arg("healthcheck")
        .assert()
        .success();
}

#[test]
fn error_status_is_unhealthy() {
    let env = Env::new();
    let (port, _rx) = serve_once("503 Service Unavailable");
    env.write_config("");
    env.cmd()
        .args([
            "healthcheck",
            "--url",
            &format!("http://127.0.0.1:{port}/api/health"),
        ])
        .assert()
        .code(1)
        .stderr(format!(
            "unhealthy: http://127.0.0.1:{port}/api/health returned HTTP 503\n"
        ));
}

#[test]
fn nothing_listening_is_unhealthy() {
    let env = Env::new();
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    env.write_config(&format!("server:\n  port: {port}\n"));
    env.cmd()
        .arg("healthcheck")
        .assert()
        .code(1)
        .stderr(predicate::str::starts_with(format!(
            "unhealthy: http://127.0.0.1:{port}/api/health: "
        )));
}
