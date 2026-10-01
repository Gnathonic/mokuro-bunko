//! Server lifecycle: one server per process, on its own thread and tokio runtime.

use crate::config::{self, StartOptions};
use crate::logs;
use bunko_server::app::{self, ServeOptions, Services};
use parking_lot::Mutex;
use std::net::{SocketAddr, TcpStream};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// How long `start` waits for the listener before reporting failure.
const READY_TIMEOUT: Duration = Duration::from_secs(15);

struct Running {
    stop: CancellationToken,
    thread: JoinHandle<()>,
    port: u16,
}

static STATE: Mutex<Option<Running>> = Mutex::new(None);
/// Why the last server thread ended, if it failed.
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Start the server and wait until it accepts connections. Returns the local URL.
/// Starting while running is a no-op that returns the running server's URL.
pub fn start(opts: StartOptions) -> Result<String, String> {
    let mut state = STATE.lock();
    if let Some(r) = state.as_ref() {
        if !r.thread.is_finished() {
            return Ok(local_url(r.port));
        }
        // The thread died on its own (bind lost, fatal error): reap it and start again.
        if let Some(old) = state.take() {
            let _ = old.thread.join();
        }
    }
    *LAST_ERROR.lock() = None;
    let result = start_locked(&opts);
    match result {
        Ok(running) => {
            let url = local_url(running.port);
            *state = Some(running);
            Ok(url)
        }
        Err(e) => {
            error!("Could not start the server: {e}");
            logs::note(format!("ERROR could not start the server: {e}"));
            *LAST_ERROR.lock() = Some(e.clone());
            Err(e)
        }
    }
}

fn start_locked(opts: &StartOptions) -> Result<Running, String> {
    let config = config::prepare(opts)?;
    logs::init(&config.storage.base_path);
    app::validate_startup(&config).map_err(|m| format!("Startup validation failed: {m}"))?;
    // Fail early (and readably) when the port is taken: serve_router would only log it.
    std::net::TcpListener::bind((opts.host(), opts.port)).map_err(|e| format!("Port {} is not available: {e}", opts.port))?;
    for w in &config.warnings {
        warn!("{w}");
    }
    let threads = match config.server.threads {
        0 => std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).min(4),
        n => n as usize,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .max_blocking_threads(16)
        .thread_name("bunko")
        .enable_all()
        .build()
        .map_err(|e| format!("Could not start the runtime: {e}"))?;
    info!("mokuro-bunko {} ({}, Android)", bunko_core::VERSION, crate::FLAVOR);
    info!("Storage path: {}", config.storage.base_path.display());
    // No local OCR on Android: `local: None`, the lite server's remote-processor path.
    let serve_opts = ServeOptions { verbose: false, flavor: crate::FLAVOR, local: None };
    let services = {
        let _guard = runtime.enter();
        Services::new(config, Some(opts.config_path.clone()), &serve_opts).map_err(|e| format!("{e:#}"))?
    };
    let stop = services.stop.clone();
    let port = opts.port;
    let thread = std::thread::Builder::new()
        .name("bunko-server".into())
        .spawn(move || run(runtime, services, serve_opts))
        .map_err(|e| format!("Could not start the server thread: {e}"))?;
    let running = Running { stop, thread, port };
    wait_ready(&running)?;
    Ok(running)
}

fn run(runtime: tokio::runtime::Runtime, services: Services, opts: ServeOptions) {
    let result = runtime.block_on(async {
        app::announce_setup(&services);
        let router = app::assemble(&services, &opts);
        app::serve_router(&services, router).await
    });
    if let Err(e) = result {
        error!("Server stopped: {e:#}");
        *LAST_ERROR.lock() = Some(format!("{e:#}"));
    } else {
        info!("Server stopped");
    }
    if services.restart_requested.load(std::sync::atomic::Ordering::SeqCst) {
        // Updates come from the store or a new APK; the app restarts the service.
        info!("Restart requested: start the server again from the app");
    }
    drop(services);
    runtime.shutdown_timeout(Duration::from_secs(5));
}

fn wait_ready(r: &Running) -> Result<(), String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], r.port));
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if r.thread.is_finished() {
            r.stop.cancel();
            return Err(LAST_ERROR.lock().clone().unwrap_or_else(|| "the server stopped during startup".into()));
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    r.stop.cancel();
    Err(format!("the server did not start listening on port {} within {}s", r.port, READY_TIMEOUT.as_secs()))
}

/// Stop the server (graceful: in-flight requests get up to 5 s) and wait for its thread.
/// Returns whether a server was running.
pub fn stop() -> bool {
    let Some(r) = STATE.lock().take() else { return false };
    info!("Stopping the server");
    r.stop.cancel();
    let _ = r.thread.join();
    true
}

pub fn is_running() -> bool {
    STATE.lock().as_ref().is_some_and(|r| !r.thread.is_finished())
}

pub fn last_error() -> Option<String> {
    LAST_ERROR.lock().clone()
}

pub fn local_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn get(port: u16, path: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/html\r\nConnection: close\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    /// One test for the whole lifecycle: the server is a process-wide singleton.
    #[test]
    fn lifecycle() {
        let d = tempfile::tempdir().unwrap();
        let port = free_port();
        let opts = StartOptions { storage_dir: d.path().join("storage"), config_path: d.path().join("config.yaml"), port, lan: false };

        let url = start(opts.clone()).unwrap();
        assert_eq!(url, format!("http://127.0.0.1:{port}/"));
        assert!(is_running());
        assert_eq!(start(opts.clone()).unwrap(), url, "second start is a no-op");

        let health = get(port, "/api/health");
        assert!(health.starts_with("HTTP/1.1 200"), "{health}");
        // First run: the root sends the WebView (Accept: text/html) to the setup wizard.
        let root = get(port, "/");
        assert!(root.starts_with("HTTP/1.1 302") && root.contains("/setup"), "{}", &root[..root.len().min(300)]);
        let setup = get(port, "/setup");
        assert!(setup.starts_with("HTTP/1.1 200"), "{}", &setup[..setup.len().min(300)]);
        assert!(d.path().join("storage/logs/server.log").exists());
        assert!(d.path().join("config.yaml").exists());

        assert!(stop());
        assert!(!is_running());
        assert!(!stop());
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err(), "port released");
        assert!(logs::tail().contains("Server stopped"), "{}", logs::tail());

        // Restart in the same process (the service is stopped and started again).
        start(opts.clone()).unwrap();
        assert!(get(port, "/api/health").starts_with("HTTP/1.1 200"));
        assert!(stop());

        // A taken port is reported, not hung on.
        let blocker = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let err = start(opts).unwrap_err();
        assert!(err.contains("not available"), "{err}");
        assert!(!is_running());
        assert_eq!(last_error().as_deref(), Some(err.as_str()));
        drop(blocker);
    }
}
