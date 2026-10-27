//! The large-catalog scenario (docs/design/interop-testing.md §8.1): the catalog and
//! series views, warm, cold, and while WebDAV writes keep invalidating the library
//! index, on a 2,000-series library and on a 200-series one for the scaling check.

use super::genlib::{self, Generated, SMALL_SERIES};
use super::http;
use super::server::Server;
use super::{Metrics, percentile};
use anyhow::{Context, Result, bail};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct Shape {
    pub series: usize,
    pub volumes: usize,
    pub small_series: usize,
    pub load: Duration,
    pub samples: usize,
}

impl Shape {
    pub const FULL: Shape = Shape {
        series: 2000,
        volumes: 10,
        small_series: 200,
        // Past the index's 30 s lifetime, so an expiry falls inside the window.
        load: Duration::from_secs(40),
        samples: 30,
    };
    pub const QUICK: Shape = Shape {
        series: 200,
        volumes: 3,
        small_series: 20,
        load: Duration::from_secs(5),
        samples: 10,
    };
}

/// The servers and libraries for every run of the scenario.
pub struct Rig {
    big: Server,
    small: Server,
    library: Generated,
    shape: Shape,
    browser: Option<PathBuf>,
}

fn encode(segment: &str) -> String {
    let mut out = String::new();
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn series_path(name: &str) -> String {
    format!("/catalog/api/series?name={}", encode(name))
}

fn ok(reply: http::Reply, what: &str) -> Result<http::Reply> {
    if reply.status != 200 {
        bail!(
            "{what} answered {}: {}",
            reply.status,
            String::from_utf8_lossy(&reply.body)
        );
    }
    Ok(reply)
}

fn median_of(addr: SocketAddr, path: &str, samples: usize) -> Result<f64> {
    for _ in 0..5 {
        ok(http::get(addr, path)?, path)?;
    }
    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        times.push(ok(http::get(addr, path)?, path)?.ms());
    }
    Ok(percentile(&mut times, 50.0))
}

impl Rig {
    pub fn prepare(bin: &Path, work: &Path, shape: Shape, browser: Option<PathBuf>) -> Result<Rig> {
        if work.exists() {
            std::fs::remove_dir_all(work)
                .with_context(|| format!("clearing {}", work.display()))?;
        }
        let big_dir = work.join("big");
        let small_dir = work.join("small");
        let started = Instant::now();
        let library = genlib::generate(&big_dir, shape.series, shape.volumes)?;
        genlib::generate(&small_dir, shape.small_series, shape.volumes)?;
        eprintln!(
            "generated {} series / {} volumes (+ {} series) in {:.1} s",
            library.series.len(),
            library.volumes,
            shape.small_series + 1,
            started.elapsed().as_secs_f64()
        );
        let mut big = Server::start(bin, &big_dir)?;
        let mut small = Server::start(bin, &small_dir)?;
        let ready = big.wait_ready(library.series.len())?;
        small.wait_ready(shape.small_series + 1)?;
        eprintln!(
            "first compile of the large library: {:.1} s",
            ready.as_secs_f64()
        );
        Ok(Rig {
            big,
            small,
            library,
            shape,
            browser,
        })
    }

    /// One run: every metric once.
    pub fn run(&mut self) -> Result<Metrics> {
        let mut m = Metrics::new();
        let addr = self.big.addr;
        let samples = self.shape.samples;

        m.insert(
            "library_warm_ms".into(),
            median_of(addr, "/catalog/api/library", samples)?,
        );
        let series_big = median_of(addr, &series_path(SMALL_SERIES), samples)?;
        let series_small = median_of(self.small.addr, &series_path(SMALL_SERIES), samples)?;
        m.insert("series_warm_ms".into(), series_big);
        m.insert(
            "series_scale_ratio".into(),
            series_big.max(1.0) / series_small.max(1.0),
        );

        if let Some(script) = &self.browser {
            m.extend(browser(script, addr)?);
        }

        self.under_load(&mut m)?;

        // Cold: the first requests after a restart, while startup work still runs.
        self.big.restart()?;
        m.insert(
            "library_cold_ms".into(),
            ok(http::get(addr, "/catalog/api/library")?, "library")?.ms(),
        );
        m.insert(
            "series_cold_ms".into(),
            ok(http::get(addr, &series_path(SMALL_SERIES))?, "series")?.ms(),
        );
        self.big.wait_ready(self.library.series.len())?;
        Ok(m)
    }

    /// Sample both views while one thread uploads sidecars and another lists the
    /// library over WebDAV, the traffic that invalidates the index on a live server.
    fn under_load(&mut self, m: &mut Metrics) -> Result<()> {
        let load = Load::start(&self.big, &self.library, self.shape.volumes);
        let addr = self.big.addr;
        let series = series_path(SMALL_SERIES);
        let mut series_times = Vec::new();
        let mut library_times = Vec::new();
        let start = Instant::now();
        while start.elapsed() < self.shape.load {
            series_times.push(ok(http::get(addr, &series)?, "series")?.ms());
            library_times.push(ok(http::get(addr, "/catalog/api/library")?, "library")?.ms());
            std::thread::sleep(Duration::from_millis(50));
        }
        let (writes, errors) = load.stop()?;
        m.insert(
            "series_load_p95_ms".into(),
            percentile(&mut series_times, 95.0),
        );
        m.insert(
            "series_load_max_ms".into(),
            percentile(&mut series_times, 100.0),
        );
        m.insert(
            "library_load_p95_ms".into(),
            percentile(&mut library_times, 95.0),
        );
        m.insert("load_writes".into(), writes as f64);
        m.insert("load_errors".into(), errors as f64);
        // Let the compile passes the writes queued finish before the next phase.
        std::thread::sleep(Duration::from_secs(3));
        Ok(())
    }
}

/// The background writers; stopped and joined on drop, whatever path leaves the scope.
struct Load {
    stop: Arc<AtomicBool>,
    writes: Arc<AtomicU64>,
    errors: Arc<AtomicU64>,
    threads: Vec<JoinHandle<()>>,
}

impl Load {
    fn start(server: &Server, library: &Generated, per_series: usize) -> Load {
        let stop = Arc::new(AtomicBool::new(false));
        let writes = Arc::new(AtomicU64::new(0));
        let errors = Arc::new(AtomicU64::new(0));
        let addr = server.addr;
        let auth = server.auth.clone();
        let targets: Vec<(String, String)> = (0..500)
            .map(|n| library.volume(n * 7 + 1, per_series))
            .collect();

        let writer = {
            let (stop, writes, errors, auth) =
                (stop.clone(), writes.clone(), errors.clone(), auth.clone());
            std::thread::spawn(move || {
                let mut n = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let (series, volume) = &targets[n % targets.len()];
                    let path = format!(
                        "/mokuro-reader/{}/{}.mokuro",
                        encode(series),
                        encode(volume)
                    );
                    let body = genlib::mokuro_doc(series, volume, 900_000 + n);
                    match http::request(addr, "PUT", &path, &[("Authorization", &auth)], &body) {
                        Ok(reply) if (200..300).contains(&reply.status) => {
                            writes.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(reply) => {
                            if errors.fetch_add(1, Ordering::Relaxed) == 0 {
                                eprintln!("load: PUT {path} answered {}", reply.status);
                            }
                        }
                        Err(error) => {
                            if errors.fetch_add(1, Ordering::Relaxed) == 0 {
                                eprintln!("load: PUT {path} failed: {error}");
                            }
                        }
                    }
                    n += 1;
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
        };
        let lister = {
            let (stop, errors) = (stop.clone(), errors.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let reply = http::request(
                        addr,
                        "PROPFIND",
                        "/mokuro-reader/",
                        &[("Authorization", &auth), ("Depth", "1")],
                        &[],
                    );
                    if !matches!(&reply, Ok(r) if r.status == 207)
                        && errors.fetch_add(1, Ordering::Relaxed) == 0
                    {
                        eprintln!("load: PROPFIND answered {:?}", reply.map(|r| r.status));
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            })
        };
        Load {
            stop,
            writes,
            errors,
            threads: vec![writer, lister],
        }
    }

    fn stop(mut self) -> Result<(u64, u64)> {
        self.join();
        Ok((
            self.writes.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
        ))
    }

    fn join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Drop for Load {
    fn drop(&mut self) {
        self.join();
    }
}

fn browser(script: &Path, addr: SocketAddr) -> Result<Metrics> {
    let dir = script
        .parent()
        .and_then(Path::parent)
        .context("the browser script lives in interop/e2e")?;
    let out = std::process::Command::new("node")
        .arg(script)
        .current_dir(dir)
        .env("BUNKO_URL", format!("http://{addr}"))
        .env("SMALL_SERIES", SMALL_SERIES)
        .output()
        .context("running node (needed for --browser)")?;
    if !out.status.success() {
        bail!(
            "the browser timings failed ({}):\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .last()
        .context("the browser script printed nothing")?;
    let values: std::collections::BTreeMap<String, f64> =
        serde_json::from_str(line).with_context(|| format!("browser output: {line}"))?;
    Ok(values.into_iter().collect())
}
