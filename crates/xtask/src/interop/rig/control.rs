//! The rig's control port (`RIG_CONTROL_URL`): what a browser spec cannot do from the
//! browser or through a party's own API, done by the runner that owns the processes.
//! One request at a time, JSON in and out; a failed action answers 500 with
//! `{"error": …}`, an unknown one 404.
//!
//! - `POST /bunko/restart {stale?: bool, tune?: {key: value}}`: stop the suite's Bunko
//!   server and start it again on the same config and data, answering once it serves
//!   HTTP (its lease is the spec's to wait on, through the Storage panel). `stale`
//!   stays down until the lease marker it last wrote is past its ttl, so the new
//!   process claims the library afresh (§9.2 step 1) instead of resuming its own
//!   marker; `tune` sets `MOKURO_TEST_CLOUD_TUNE` keys for the new process (a longer
//!   `lease_settle_ms` holds the claim open), over the suite's own.
//! - `POST /bunko/stop`: stop the suite's Bunko server (SIGINT: it writes no `released`,
//!   so its marker simply stops moving) and leave it down; answers `stopped_at_ms`, the
//!   Unix time it was down. `POST /bunko/start` starts it again on the suite's own tune,
//!   answering once it serves HTTP. A stop of a stopped server, or a start of a running
//!   one, is refused.

use super::super::server::Server;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The store tune's environment variable (cloud.rs `tune`).
pub const TUNE_VAR: &str = "MOKURO_TEST_CLOUD_TUNE";

/// What answers a request: `(method, path, JSON body or Null)` to a JSON answer, `None`
/// for a path it does not serve.
pub type Handler = dyn Fn(&str, &str, Value) -> Option<Result<Value>> + Send + Sync;

pub struct Control {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Control {
    /// Serve `handler` on a loopback port of its own.
    pub fn start(handler: Arc<Handler>) -> Result<Control> {
        let listener = TcpListener::bind("127.0.0.1:0").context("binding the control port")?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if flag.load(Ordering::Acquire) {
                    break;
                }
                if let Ok(s) = stream {
                    serve(s, &*handler);
                }
            }
        });
        Ok(Control {
            addr,
            stop,
            thread: Some(thread),
        })
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One request: its method, path and body (JSON, or Null when empty).
fn read_request(s: &mut TcpStream) -> Result<(String, String, Value)> {
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = s.read(&mut buf)?;
        if n == 0 {
            bail!("the request ended inside its head");
        }
        raw.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let method = first.next().unwrap_or_default().to_string();
    let path = first.next().unwrap_or_default().to_string();
    let length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.trim().parse::<usize>())
        .transpose()
        .context("a Content-Length that is no number")?
        .unwrap_or(0);
    while raw.len() < head_end + length {
        let n = s.read(&mut buf)?;
        if n == 0 {
            bail!("the request ended inside its body");
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let body = &raw[head_end..head_end + length];
    let body = if body.iter().all(u8::is_ascii_whitespace) {
        Value::Null
    } else {
        serde_json::from_slice(body).context("a body that is not JSON")?
    };
    Ok((method, path, body))
}

fn serve(mut s: TcpStream, handler: &Handler) {
    let (status, answer) = match read_request(&mut s) {
        Err(e) => (400, json!({"error": format!("{e:#}")})),
        Ok((method, path, body)) => match handler(&method, &path, body) {
            None => (404, json!({"error": format!("no control {method} {path}")})),
            Some(Ok(v)) => (200, v),
            Some(Err(e)) => (500, json!({"error": format!("{e:#}")})),
        },
    };
    let body = answer.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let _ = s.write_all(
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    );
}

/// `tune` (`key=value,…`) with `over`'s keys set: replaced in place, new ones appended.
pub fn merge_tune(tune: &str, over: &serde_json::Map<String, Value>) -> Result<String> {
    let mut pairs: Vec<(String, String)> = tune
        .split(',')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (k.to_string(), v.to_string())
        })
        .collect();
    for (k, v) in over {
        let v = match v {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            other => bail!("tune {k}: {other} is not a number, string or bool"),
        };
        match pairs.iter_mut().find(|(name, _)| name == k) {
            Some(p) => p.1 = v,
            None => pairs.push((k.clone(), v)),
        }
    }
    Ok(pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(","))
}

/// One key of a tune string, as a number.
pub fn tune_value(tune: &str, key: &str) -> Option<u64> {
    tune.split(',')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.parse().ok())
}

/// A claimant writes no heartbeat while its claim settles, so a settle wait as long as
/// the ttl finds its own marker stale and claims again, forever.
fn check_settle(tune: &str) -> Result<()> {
    if let (Some(settle_ms), Some(ttl_s)) = (
        tune_value(tune, "lease_settle_ms"),
        tune_value(tune, "lease_ttl_s"),
    ) && settle_ms >= ttl_s * 1000
    {
        bail!(
            "lease_settle_ms={settle_ms} is not under the lease ttl ({ttl_s} s): the claim \
             would never settle"
        );
    }
    Ok(())
}

/// What the handler does to the suite's server (the rig's [`Server`]).
pub trait BunkoProcess: Send {
    fn stop(&mut self);
    fn start_again(&mut self) -> Result<()>;
    fn is_running(&self) -> bool;
    fn env_var(&self, key: &str) -> Option<&str>;
    fn set_env(&mut self, key: &str, value: &str);
}

impl BunkoProcess for Server {
    fn stop(&mut self) {
        Server::stop(self)
    }
    fn start_again(&mut self) -> Result<()> {
        Server::start_again(self)
    }
    fn is_running(&self) -> bool {
        Server::is_running(self)
    }
    fn env_var(&self, key: &str) -> Option<&str> {
        Server::env_var(self, key)
    }
    fn set_env(&mut self, key: &str, value: &str) {
        Server::set_env(self, key, value)
    }
}

/// The handler of a suite with a Bunko server: `POST /bunko/restart`, `/bunko/stop`
/// and `/bunko/start`.
pub fn bunko_handler<S: BunkoProcess + 'static>(server: Arc<Mutex<S>>) -> Arc<Handler> {
    // Each restart's tune is over the suite's, never over an earlier restart's.
    let suite_tune = server
        .lock()
        .ok()
        .and_then(|s| s.env_var(TUNE_VAR).map(str::to_string))
        .unwrap_or_default();
    Arc::new(move |method, path, body| match (method, path) {
        ("POST", "/bunko/restart") => Some(restart(&*server, &suite_tune, &body)),
        ("POST", "/bunko/stop") => Some(stop(&*server)),
        ("POST", "/bunko/start") => Some(start(&*server, &suite_tune)),
        _ => None,
    })
}

fn stop(server: &Mutex<impl BunkoProcess>) -> Result<Value> {
    let mut s = server
        .lock()
        .map_err(|_| anyhow::anyhow!("the server lock"))?;
    if !s.is_running() {
        bail!("the server is not running");
    }
    s.stop();
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64;
    Ok(json!({"stopped_at_ms": at}))
}

fn start(server: &Mutex<impl BunkoProcess>, tune: &str) -> Result<Value> {
    let mut s = server
        .lock()
        .map_err(|_| anyhow::anyhow!("the server lock"))?;
    if s.is_running() {
        bail!("the server is running");
    }
    // Never an earlier restart's tune.
    if !tune.is_empty() {
        s.set_env(TUNE_VAR, tune);
    }
    s.start_again()?;
    Ok(json!({}))
}

fn restart(server: &Mutex<impl BunkoProcess>, tune: &str, body: &Value) -> Result<Value> {
    let stale = match &body["stale"] {
        Value::Null => false,
        Value::Bool(b) => *b,
        other => bail!("stale: {other} is not true or false"),
    };
    let over = match &body["tune"] {
        Value::Null => serde_json::Map::new(),
        Value::Object(m) => m.clone(),
        other => bail!("tune: {other} is not an object"),
    };
    let mut s = server
        .lock()
        .map_err(|_| anyhow::anyhow!("the server lock"))?;
    let ttl = tune_value(tune, "lease_ttl_s");
    if stale && ttl.is_none() {
        bail!("stale needs the server's lease ttl ({TUNE_VAR} lease_ttl_s), and it has none");
    }
    let next = merge_tune(tune, &over)?;
    check_settle(&next)?;
    s.stop();
    let stopped = Instant::now();
    if let Some(ttl) = ttl.filter(|_| stale) {
        // Its last heartbeat was before the stop: a second past ttl from here, the
        // marker is stale to anyone reading it.
        std::thread::sleep(Duration::from_secs(ttl + 1).saturating_sub(stopped.elapsed()));
    }
    s.set_env(TUNE_VAR, &next);
    s.start_again()?;
    Ok(json!({"down_ms": stopped.elapsed().as_millis() as u64, "tune": next}))
}

#[cfg(test)]
mod tests {
    use super::super::super::http;
    use super::*;

    #[test]
    fn a_tune_key_is_replaced_in_place_and_a_new_one_appended() {
        let over = json!({"lease_settle_ms": 15000, "extra": "x"});
        assert_eq!(
            merge_tune(
                "lease_ttl_s=12,lease_settle_ms=1000,retry_base_ms=200",
                over.as_object().unwrap()
            )
            .unwrap(),
            "lease_ttl_s=12,lease_settle_ms=15000,retry_base_ms=200,extra=x"
        );
        assert_eq!(
            merge_tune("", over.as_object().unwrap()).unwrap(),
            "lease_settle_ms=15000,extra=x"
        );
        assert!(merge_tune("a=1", json!({"a": [1]}).as_object().unwrap()).is_err());
        assert_eq!(
            tune_value("lease_ttl_s=12,lease_settle_ms=1000", "lease_ttl_s"),
            Some(12)
        );
        assert_eq!(tune_value("lease_settle_ms=1000", "lease_ttl_s"), None);
    }

    #[test]
    fn a_settle_wait_must_be_under_the_ttl() {
        assert!(check_settle("lease_ttl_s=12,lease_settle_ms=6000").is_ok());
        assert!(check_settle("lease_settle_ms=60000").is_ok());
        let e = check_settle("lease_ttl_s=12,lease_settle_ms=12000").unwrap_err();
        assert!(e.to_string().contains("never settle"), "{e}");
    }

    /// The server as the handler drives it: what it was told, in order.
    #[derive(Default)]
    struct FakeServer {
        running: bool,
        env: Vec<(String, String)>,
        calls: Vec<String>,
    }

    impl BunkoProcess for FakeServer {
        fn stop(&mut self) {
            self.calls.push("stop".into());
            self.running = false;
        }
        fn start_again(&mut self) -> Result<()> {
            if self.running {
                bail!("the server is running");
            }
            self.calls.push("start".into());
            self.running = true;
            Ok(())
        }
        fn is_running(&self) -> bool {
            self.running
        }
        fn env_var(&self, key: &str) -> Option<&str> {
            self.env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        }
        fn set_env(&mut self, key: &str, value: &str) {
            self.env.retain(|(k, _)| k != key);
            self.env.push((key.into(), value.into()));
        }
    }

    fn fake() -> Arc<Mutex<FakeServer>> {
        Arc::new(Mutex::new(FakeServer {
            running: true,
            env: vec![(TUNE_VAR.into(), "lease_ttl_s=45".into())],
            calls: Vec::new(),
        }))
    }

    #[test]
    fn stop_leaves_the_server_down_until_start() {
        let s = fake();
        let h = bunko_handler(s.clone());
        let answer = h("POST", "/bunko/stop", Value::Null).unwrap().unwrap();
        assert!(answer["stopped_at_ms"].as_u64().unwrap() > 0, "{answer}");
        assert!(!s.lock().unwrap().running);
        // A second stop, or a restart's start on a running one, is the spec's mistake.
        assert!(h("POST", "/bunko/stop", Value::Null).unwrap().is_err());
        h("POST", "/bunko/start", Value::Null).unwrap().unwrap();
        assert!(s.lock().unwrap().running);
        assert!(h("POST", "/bunko/start", Value::Null).unwrap().is_err());
        assert_eq!(s.lock().unwrap().calls, ["stop", "start"]);
        // On the suite's own tune.
        assert_eq!(s.lock().unwrap().env_var(TUNE_VAR), Some("lease_ttl_s=45"));
    }

    #[test]
    fn a_restart_of_a_stopped_server_starts_it() {
        let s = fake();
        let h = bunko_handler(s.clone());
        h("POST", "/bunko/stop", Value::Null).unwrap().unwrap();
        h("POST", "/bunko/restart", Value::Null).unwrap().unwrap();
        assert!(s.lock().unwrap().running);
        assert!(h("GET", "/bunko/stop", Value::Null).is_none());
    }

    #[test]
    fn the_port_answers_its_handler_in_json_and_404s_the_rest() {
        let handler: Arc<Handler> = Arc::new(|method, path, body| match (method, path) {
            ("POST", "/echo") => Some(Ok(json!({"got": body}))),
            ("POST", "/fail") => Some(Err(anyhow::anyhow!("it broke"))),
            _ => None,
        });
        let c = Control::start(handler).unwrap();
        let addr: SocketAddr = c.url().trim_start_matches("http://").parse().unwrap();
        let post = |path: &str, body: &[u8]| http::request(addr, "POST", path, &[], body).unwrap();

        let r = post("/echo", br#"{"stale":true}"#);
        assert_eq!(r.status, 200);
        assert_eq!(r.json().unwrap(), json!({"got": {"stale": true}}));
        // An empty body is Null, not an error.
        assert_eq!(post("/echo", b"").json().unwrap(), json!({"got": null}));

        let r = post("/fail", b"");
        assert_eq!(r.status, 500);
        assert_eq!(r.json().unwrap()["error"], "it broke");
        assert_eq!(post("/nothing", b"").status, 404);
        assert_eq!(post("/echo", b"{not json").status, 400);
        // Dropping it stops the thread (the test would hang otherwise).
        drop(c);
    }
}
