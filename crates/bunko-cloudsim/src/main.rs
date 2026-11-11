//! `bunko-cloudsim`: the cloud simulator as a standalone server, for the reader's
//! provider suite and the interop rig. Prints `listening on http://HOST:PORT` as its
//! first stdout line, then logs one line per request to stderr. A second listener
//! (another origin) serves OneDrive's pre-authenticated URLs; `/_sim/health` names it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use axum::extract::Request;
use axum::middleware::{Next, from_fn};
use axum::response::Response;
use bunko_cloudsim::{Provider, SimState, router};

const USAGE: &str = "\
usage: bunko-cloudsim [--port N] [--host 127.0.0.1] [--base-url URL]
                      [--preauth-port N] [--preauth-host NAME]
                      [--preauth-base-url URL]
                      [--seed-dir DIR [--seed-account NAME]]

  --port N            listen on port N (0, the default, picks a free one)
  --host ADDR         listen address (default 127.0.0.1)
  --base-url URL      the address clients reach this server at, used in the URLs it
                      hands out (default http://HOST:PORT)
  --preauth-port N    the second listener, another origin, that OneDrive's
                      pre-authenticated URLs point at (default 0: a free port)
  --preauth-host NAME the host name to advertise the second listener by (its port
                      stays its own); for clients that keep a token across a
                      redirect that changes only the port (jsdom)
  --preauth-base-url URL
                      the address clients reach the second listener at (default
                      --base-url with the second port, else http://HOST:PORT2)
  --seed-dir DIR      seed every provider's account NAME with DIR's files, and create
                      the MEGA account NAME@cloudsim.test with password `cloudsim`
  --seed-account NAME the account --seed-dir fills (default `default`)";

struct Args {
    port: u16,
    host: String,
    base_url: Option<String>,
    preauth_port: u16,
    preauth_host: Option<String>,
    preauth_base_url: Option<String>,
    seed_dir: Option<PathBuf>,
    seed_account: String,
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        port: 0,
        host: "127.0.0.1".into(),
        base_url: None,
        preauth_port: 0,
        preauth_host: None,
        preauth_base_url: None,
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
            "--preauth-port" => {
                let v = value()?;
                a.preauth_port = v
                    .parse()
                    .map_err(|_| format!("--preauth-port {v:?}: not a port"))?;
            }
            "--preauth-host" => a.preauth_host = Some(value()?),
            "--preauth-base-url" => {
                a.preauth_base_url = Some(value()?.trim_end_matches('/').to_string())
            }
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
/// Symlinks are followed; a broken one, or one leading back to a folder already on
/// the way down (a loop), is skipped with a warning on stderr.
fn read_tree(dir: &Path) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    let top = std::fs::canonicalize(dir)?;
    // (folder as listed, its path relative to `dir`, the real folders above it)
    let mut stack = vec![(dir.to_path_buf(), Vec::<String>::new(), vec![top])];
    while let Some((d, rel, seen)) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let p = e?.path();
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut child = rel.clone();
            child.push(name);
            let shown = child.join("/");
            // `metadata` follows links; a broken one has none.
            let Ok(meta) = std::fs::metadata(&p) else {
                eprintln!("seed: skipping {shown}: a broken link");
                continue;
            };
            if meta.is_dir() {
                let real = std::fs::canonicalize(&p)?;
                if seen.contains(&real) {
                    eprintln!("seed: skipping {shown}: a link back up the tree (a loop)");
                    continue;
                }
                let mut seen = seen.clone();
                seen.push(real);
                stack.push((p, child, seen));
            } else {
                out.push((shown, std::fs::read(&p)?));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// `url` (`scheme://host[:port]`) with its port replaced by `port`.
fn with_port(url: &str, port: u16) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    let host = match rest.rfind(':') {
        // `[::1]:80` or `name:80`, but not the colons inside `[::1]`.
        Some(i) if !rest[i..].contains(']') => &rest[..i],
        _ => rest,
    };
    format!("{scheme}://{host}:{port}")
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
    let second = match tokio::net::TcpListener::bind((addr.ip(), args.preauth_port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "bunko-cloudsim: bind {}:{}: {e}",
                addr.ip(),
                args.preauth_port
            );
            return ExitCode::FAILURE;
        }
    };
    let addr2 = second.local_addr().expect("local addr");
    let base = args
        .base_url
        .clone()
        .unwrap_or_else(|| format!("http://{addr}"));
    let preauth = match (&args.preauth_base_url, &args.preauth_host, &args.base_url) {
        (Some(u), _, _) => u.clone(),
        (None, Some(h), _) => format!("http://{h}:{}", addr2.port()),
        (None, None, Some(b)) => with_port(b, addr2.port()),
        (None, None, None) => format!("http://{addr2}"),
    };
    let sim = SimState::with_preauth(base.clone(), preauth.clone());
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
    eprintln!("pre-authenticated URLs on {preauth} (listening on http://{addr2})");
    let app = router(sim).layer(from_fn(log_line));
    let second_app = app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(second, second_app).await;
    });
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
    {
        eprintln!("bunko-cloudsim: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    #[test]
    fn with_port_replaces_or_adds_the_port() {
        assert_eq!(
            super::with_port("http://cloudsim:8080", 9),
            "http://cloudsim:9"
        );
        assert_eq!(super::with_port("http://cloudsim", 9), "http://cloudsim:9");
        assert_eq!(super::with_port("http://[::1]:80", 9), "http://[::1]:9");
        assert_eq!(super::with_port("http://[::1]", 9), "http://[::1]:9");
    }
}
