//! A 100k-node account (spec §14 M5: "under 64 MiB RSS"). Opt-in, Linux only: the
//! mock serves the tree from the same process, so the peak printed is an upper bound
//! for the driver.
//!
//! cargo test -p bunko-remote --release --test mega_scale -- --ignored --nocapture
#![cfg(all(feature = "mega", target_os = "linux"))]

use std::time::{Duration, Instant};

use bunko_mega::ClientConfig;
use bunko_mega::mock::{MockMega, ROOT};
use bunko_remote::backend::Backend;
use bunko_remote::mega::{MegaBackend, MegaConfig, SessionBlob};

fn rss_kib(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|l| l.starts_with(field))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// The mock half, run in a child process so the measured process holds only the
/// driver. Does nothing unless `BUNKO_MEGA_SCALE_SERVER` is set.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "helper of hundred_thousand_nodes"]
async fn scale_server() {
    if std::env::var_os("BUNKO_MEGA_SCALE_SERVER").is_none() {
        return;
    }
    let mock = MockMega::start().await;
    let lib = mock.mkdir(ROOT, "mokuro-reader");
    let other = mock.mkdir(ROOT, "Photos");
    // 20k nodes in the library (1k series of 19 files), 80k outside.
    for s in 0..1000 {
        let d = mock.mkdir(&lib, &format!("Series {s}"));
        for v in 0..19 {
            mock.put(&d, &format!("Volume {v} of a long series name.cbz"), b"x");
        }
    }
    for p in 0..800 {
        let d = mock.mkdir(&other, &format!("Album {p}"));
        for i in 0..99 {
            mock.put(&d, &format!("IMG_{i:05}.jpg"), b"y");
        }
    }
    println!("SCALE {} {}", mock.base, mock.session_blob());
    tokio::time::sleep(Duration::from_secs(600)).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: builds a 100k-node account"]
async fn hundred_thousand_nodes() {
    use std::io::BufRead;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "scale_server", "--ignored", "--nocapture"])
        .env("BUNKO_MEGA_SCALE_SERVER", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let line = std::io::BufReader::new(out)
        .lines()
        .map(Result::unwrap)
        .find(|l| l.starts_with("SCALE "))
        .unwrap();
    let mut parts = line.split_whitespace().skip(1);
    let (base, blob) = (
        parts.next().unwrap().to_string(),
        parts.next().unwrap().to_string(),
    );
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = MegaConfig::new(tmp.path());
    cfg.client = ClientConfig {
        api_origin: base,
        min_cs_interval: Duration::ZERO,
        https_transfers: false,
        ..ClientConfig::default()
    };
    let be = MegaBackend::resume(&SessionBlob::new(blob), cfg)
        .await
        .unwrap();
    let before = rss_kib("VmRSS:");
    let t = Instant::now();
    let roots = be.locate_roots("mokuro-reader").await.unwrap();
    let pids: Vec<String> = roots.iter().map(|r| r.pid.clone()).collect();
    let mut n = 0usize;
    be.list_all(&pids, &mut |_| n += 1).await.unwrap();
    let took = t.elapsed();
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(n, 20_000);
    let peak = rss_kib("VmHWM:");
    eprintln!(
        "100k-node tree: listed {n} in {took:?}; driver process RSS {} MiB before, {} MiB after, peak {} MiB",
        before / 1024,
        rss_kib("VmRSS:") / 1024,
        peak / 1024
    );
    assert!(peak < 64 * 1024, "peak RSS {} MiB", peak / 1024);
}
