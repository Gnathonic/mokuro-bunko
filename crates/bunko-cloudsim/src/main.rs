//! `bunko-cloudsim`: the cloud simulator as a standalone server, for the reader's
//! provider suite and the interop rig. Prints `listening on http://HOST:PORT` as its
//! first stdout line, then logs one line per request to stderr.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use axum::extract::Request;
use axum::middleware::{Next, from_fn};
use axum::response::Response;
use bunko_cloudsim::{Provider, SimState, router};

const USAGE: &str = "\
usage: bunko-cloudsim [--port N] [--host 127.0.0.1] [--base-url URL]
                      [--seed-dir DIR [--seed-account NAME]]

  --port N            listen on port N (0, the default, picks a free one)
  --host ADDR         listen address (default 127.0.0.1)
  --base-url URL      the address clients reach this server at, used in the URLs it
                      hands out (default http://HOST:PORT)
  --seed-dir DIR      seed every provider's account NAME with DIR's files, and create
                      the MEGA account NAME@cloudsim.test with password `cloudsim`
  --seed-account NAME the account --seed-dir fills (default `default`)";

struct Args {
    port: u16,
    host: String,
    base_url: Option<String>,
    seed_dir: Option<PathBuf>,
    seed_account: String,
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        port: 0,
        host: "127.0.0.1".into(),
        base_url: None,
        seed_dir: None,
        seed_account: "default".into(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--port" => {
                let v = value()?;
                a.port = v.parse().map_err(|_| format!("--port {v:?}: not a port"))?;
            }
            "--host" => a.host = value()?,
            "--base-url" => a.base_url = Some(value()?.trim_end_matches('/').to_string()),
            "--seed-dir" => a.seed_dir = Some(PathBuf::from(value()?)),
            "--seed-account" => a.seed_account = value()?,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(a)
}

/// Every file under `dir`, as (path relative to `dir` with `/` separators, bytes).
fn read_tree(dir: &Path) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let p = e.path();
            if e.file_type()?.is_dir() {
                stack.push(p);
            } else {
                let rel = p.strip_prefix(dir).unwrap_or(&p);
                let rel: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                out.push((rel.join("/"), std::fs::read(&p)?));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

async fn log_line(req: Request, next: Next) -> Response {
    let (method, path) = (req.method().clone(), req.uri().path().to_string());
    let r = next.run(req).await;
    eprintln!("{method} {path} {}", r.status().as_u16());
    r
}

async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("bunko-cloudsim: {e}");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(if e.is_empty() { 0 } else { 2 });
        }
    };
    let listener = match tokio::net::TcpListener::bind((args.host.as_str(), args.port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bunko-cloudsim: bind {}:{}: {e}", args.host, args.port);
            return ExitCode::FAILURE;
        }
    };
    let addr = listener.local_addr().expect("local addr");
    let base = args
        .base_url
        .clone()
        .unwrap_or_else(|| format!("http://{addr}"));
    let sim = SimState::new(base.clone());
    if let Some(dir) = &args.seed_dir {
        let files = match read_tree(dir) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("bunko-cloudsim: --seed-dir {}: {e}", dir.display());
                return ExitCode::FAILURE;
            }
        };
        let name = &args.seed_account;
        for p in Provider::ALL {
            sim.seed(p, name, &files);
        }
        sim.mega(name)
            .set_account(&format!("{name}@cloudsim.test"), "cloudsim", None);
        eprintln!(
            "seeded {} file(s) into account {name:?} on every provider",
            files.len()
        );
    }
    println!("listening on http://{addr}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let app = router(sim).layer(from_fn(log_line));
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
    {
        eprintln!("bunko-cloudsim: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
