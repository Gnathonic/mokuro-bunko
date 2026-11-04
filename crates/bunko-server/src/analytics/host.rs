//! The host (design §8): process RSS and peak, CPU, connections and requests in
//! flight, sampled every 15 s into `host_minute` (and rolled into `host_hourly`).
//!
//! Every [`SAMPLE_EVERY`] the sampler (on a blocking thread) reads the process's memory
//! and CPU time and the host's memory, takes the [`Gauges`]' peaks (resetting them),
//! folds the sample into the current minute and upserts that minute's row (four
//! samples make a minute; the minute's hour is recomputed in the same transaction).
//! CPU is the process's CPU time over the interval as a percent of one core (two busy
//! cores read 200 %).
//!
//! Readers per platform, none needing a new dependency on Linux:
//! - Linux and Android (Docker and musl included): `/proc/self/status` (`VmRSS`,
//!   `VmHWM`), `/proc/self/stat` (utime + stime), `/proc/meminfo` (`MemTotal`,
//!   `MemAvailable`), and in a container the cgroup v2 limit (`memory.max`) when it is
//!   below the machine's memory;
//! - Windows: `K32GetProcessMemoryInfo`, `GetProcessTimes`, `GlobalMemoryStatusEx`;
//! - macOS: `proc_pidinfo(PROC_PIDTASKINFO)`, `getrusage`, `sysctl hw.memsize` (no
//!   cheap "available" figure: `None`);
//! - anything else: `None` everywhere, and `platform_supported` says so (never zeros).
//!
//! The sampler also refreshes the crawl detector's catalog count once a minute, so
//! that blocking read stays off the request path.

use crate::analytics::{Analytics, Gauges};
use bunko_db::analytics::host::HostMinute;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// How often the sampler reads the process.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(15);
/// Samples between two catalog counts for the crawl detector (one minute).
const CATALOG_EVERY: u64 = 4;

/// The host right now (`GET .../host` `now`, the overview's KPIs). `None`: this platform
/// has no reader for it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostNow {
    pub rss: Option<u64>,
    pub peak_rss: Option<u64>,
    /// Percent of one core over the last interval.
    pub cpu_pct: Option<f64>,
    pub connections: u64,
    pub inflight: u64,
    pub host_total: Option<u64>,
    pub host_available: Option<u64>,
}

/// What this build can read (`platform_supported`).
pub const SUPPORTS_RSS: bool = platform::RSS;
pub const SUPPORTS_CPU: bool = platform::CPU;

/// One reading of the process and the host.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Reading {
    pub rss: Option<u64>,
    pub peak_rss: Option<u64>,
    /// Process CPU time (user + system) since it started, seconds.
    pub cpu_s: Option<f64>,
    pub host_total: Option<u64>,
    pub host_available: Option<u64>,
}

/// Read the process and the host now (blocking; procfs reads on Linux).
pub fn read() -> Reading {
    platform::read()
}

/// One minute being gathered.
#[derive(Debug, Clone, Copy, Default)]
struct MinuteAgg {
    minute: i64,
    rss_sum: u128,
    rss_n: u64,
    rss_max: Option<u64>,
    cpu_sum: f64,
    cpu_n: u64,
    connections_max: u64,
    inflight_max: u64,
}

impl MinuteAgg {
    fn row(&self) -> HostMinute {
        HostMinute {
            minute: self.minute,
            rss_avg: (self.rss_n > 0).then(|| (self.rss_sum / u128::from(self.rss_n)) as u64),
            rss_max: self.rss_max,
            cpu_pct: (self.cpu_n > 0).then(|| round1(self.cpu_sum / self.cpu_n as f64)),
            connections_max: self.connections_max,
            inflight_max: self.inflight_max,
        }
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// The last sample, for [`Host::now`].
#[derive(Debug, Clone, Copy, Default)]
struct Latest {
    reading: Reading,
    cpu_pct: Option<f64>,
}

#[derive(Debug, Default)]
struct SamplerState {
    /// The CPU time at the previous sample, and when.
    cpu_mark: Option<(Instant, f64)>,
    minute: Option<MinuteAgg>,
    latest: Option<Latest>,
    samples: u64,
}

impl SamplerState {
    /// Fold a sample taken at `at_s` (epoch seconds) / `at` (monotonic); returns the
    /// minute's row so far.
    fn fold(
        &mut self,
        r: Reading,
        at_s: i64,
        at: Instant,
        connections_peak: u64,
        inflight_peak: u64,
    ) -> HostMinute {
        let cpu_pct = match (self.cpu_mark, r.cpu_s) {
            (Some((then, before)), Some(now_s)) => {
                let wall = at.saturating_duration_since(then).as_secs_f64();
                (wall > 0.5).then(|| round1(((now_s - before).max(0.0) / wall) * 100.0))
            }
            _ => None,
        };
        if let Some(c) = r.cpu_s {
            self.cpu_mark = Some((at, c));
        }
        let minute = at_s.div_euclid(60);
        let agg = match &mut self.minute {
            Some(m) if m.minute == minute => m,
            slot => slot.insert(MinuteAgg {
                minute,
                ..MinuteAgg::default()
            }),
        };
        if let Some(rss) = r.rss {
            agg.rss_sum += u128::from(rss);
            agg.rss_n += 1;
            agg.rss_max = Some(agg.rss_max.map_or(rss, |m| m.max(rss)));
        }
        if let Some(c) = cpu_pct {
            agg.cpu_sum += c;
            agg.cpu_n += 1;
        }
        agg.connections_max = agg.connections_max.max(connections_peak);
        agg.inflight_max = agg.inflight_max.max(inflight_peak);
        let row = agg.row();
        self.latest = Some(Latest {
            reading: r,
            cpu_pct,
        });
        self.samples += 1;
        row
    }
}

pub struct Host {
    gauges: Arc<Gauges>,
    state: Mutex<SamplerState>,
    /// Only the first sampler failure is a warning (one every 15 s would flood).
    warned: std::sync::atomic::AtomicBool,
}

impl Host {
    pub fn new(gauges: Arc<Gauges>) -> Host {
        Host {
            gauges,
            state: Mutex::new(SamplerState::default()),
            warned: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Start the 15 s sampler (it writes `host_minute` itself and resets the gauges'
    /// peaks with [`Gauges::take_peaks`]). The first sample is taken at once (it has no
    /// CPU interval yet).
    pub fn start(&self, a: &Analytics, stop: CancellationToken) {
        let a = a.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SAMPLE_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tick.tick() => {}
                }
                if let Err(e) = a.host().sample_now(&a).await {
                    if a.host()
                        .warned
                        .swap(true, std::sync::atomic::Ordering::Relaxed)
                    {
                        tracing::debug!("[ANALYTICS] host sample failed: {e}");
                    } else {
                        tracing::warn!(
                            "[ANALYTICS] host sample failed (further failures are logged at debug): {e}"
                        );
                    }
                }
            }
        });
    }

    /// Take one sample now and write its minute (the sampler's tick; tests). Nothing
    /// is written while analytics are disabled (the reading still feeds [`Host::now`]).
    pub async fn sample_now(&self, a: &Analytics) -> anyhow::Result<()> {
        let a = a.clone();
        tokio::task::spawn_blocking(move || {
            let host = a.host();
            let reading = read();
            let peaks = host.gauges.take_peaks();
            let now_s = super::now_s();
            let (row, samples) = {
                let mut s = host.state.lock();
                let row = s.fold(
                    reading,
                    now_s,
                    Instant::now(),
                    peaks.connections_peak,
                    peaks.inflight_peak,
                );
                (row, s.samples)
            };
            if samples % CATALOG_EVERY == 1 {
                match a.library_runtime() {
                    Some(rt) => a.crawl().refresh_catalog_from(rt.index(), now_s),
                    None => a.crawl().refresh_catalog(now_s),
                }
            }
            if a.enabled() {
                a.db().write(|tx| tx.put_host_minute(&row))?;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("host sample task: {e}"))?
    }

    pub fn now(&self) -> HostNow {
        let g = self.gauges.values();
        let latest = self.state.lock().latest;
        // Before the first sample (or without the sampler): read what needs no
        // interval now (procfs on Linux: microseconds).
        let (reading, cpu_pct) = match latest {
            Some(l) => (l.reading, l.cpu_pct),
            None => (read(), None),
        };
        HostNow {
            rss: reading.rss,
            peak_rss: reading.peak_rss,
            cpu_pct,
            connections: g.connections,
            inflight: g.inflight,
            host_total: reading.host_total,
            host_available: reading.host_available,
        }
    }
}

// --- platform readers -------------------------------------------------------------------

/// `Key:   123 kB` lines (`/proc/self/status`, `/proc/meminfo`), in bytes.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", test)),
    allow(dead_code)
)]
fn proc_kb(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let rest = l.strip_prefix(key)?.strip_prefix(':')?;
        let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
        kb.checked_mul(1024)
    })
}

/// utime + stime of `/proc/self/stat`, seconds (`USER_HZ` is 100 on every Linux ABI
/// this builds for). The command name may hold spaces and parentheses: fields are
/// counted after the last `)`.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", test)),
    allow(dead_code)
)]
fn proc_stat_cpu_s(text: &str) -> Option<f64> {
    let (_, rest) = text.rsplit_once(')')?;
    let mut f = rest.split_whitespace();
    // After the name: state (3), ppid, pgrp, session, tty_nr, tpgid, flags, minflt,
    // cminflt, majflt, cmajflt, utime (14), stime (15).
    let utime: u64 = f.nth(11)?.parse().ok()?;
    let stime: u64 = f.next()?.parse().ok()?;
    Some((utime + stime) as f64 / 100.0)
}

/// The memory a cgroup v2 container may use and has left: `memory.max` (`max`: no
/// limit), `memory.current` less the reclaimable page cache (`inactive_file` of
/// `memory.stat`, as `docker stats` counts it).
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", test)),
    allow(dead_code)
)]
fn cgroup_memory(max: &str, current: &str, stat: &str) -> Option<(u64, u64)> {
    let limit: u64 = max.trim().parse().ok()?;
    let current: u64 = current.trim().parse().ok()?;
    let inactive = stat
        .lines()
        .find_map(|l| l.strip_prefix("inactive_file ")?.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let used = current.saturating_sub(inactive);
    Some((limit, limit.saturating_sub(used)))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod platform {
    use super::Reading;

    pub const RSS: bool = true;
    pub const CPU: bool = true;

    fn text(path: &str) -> Option<String> {
        bunko_store::fs::read_to_string(path).ok()
    }

    pub fn read() -> Reading {
        let status = text("/proc/self/status").unwrap_or_default();
        let meminfo = text("/proc/meminfo").unwrap_or_default();
        let mut host_total = super::proc_kb(&meminfo, "MemTotal");
        let mut host_available = super::proc_kb(&meminfo, "MemAvailable");
        // In a container with a memory limit, that limit is the host that matters.
        if let (Some(max), Some(current)) = (
            text("/sys/fs/cgroup/memory.max"),
            text("/sys/fs/cgroup/memory.current"),
        ) {
            let stat = text("/sys/fs/cgroup/memory.stat").unwrap_or_default();
            if let Some((limit, left)) = super::cgroup_memory(&max, &current, &stat)
                && host_total.is_none_or(|t| limit < t)
            {
                host_total = Some(limit);
                host_available = Some(host_available.map_or(left, |a| a.min(left)));
            }
        }
        Reading {
            rss: super::proc_kb(&status, "VmRSS"),
            peak_rss: super::proc_kb(&status, "VmHWM"),
            cpu_s: text("/proc/self/stat").and_then(|s| super::proc_stat_cpu_s(&s)),
            host_total,
            host_available,
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::Reading;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    pub const RSS: bool = true;
    pub const CPU: bool = true;

    fn filetime_s(t: &FILETIME) -> f64 {
        // 100 ns units.
        ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) as f64 / 1e7
    }

    pub fn read() -> Reading {
        let mut r = Reading::default();
        // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing; every
        // out-parameter is a properly sized, zeroed struct owned by this frame, with its
        // size field set as the API requires.
        unsafe {
            let process = GetCurrentProcess();
            let mut pmc: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if K32GetProcessMemoryInfo(process, &mut pmc, pmc.cb) != 0 {
                r.rss = Some(pmc.WorkingSetSize as u64);
                r.peak_rss = Some(pmc.PeakWorkingSetSize as u64);
            }
            let mut created: FILETIME = std::mem::zeroed();
            let mut exited: FILETIME = std::mem::zeroed();
            let mut kernel: FILETIME = std::mem::zeroed();
            let mut user: FILETIME = std::mem::zeroed();
            if GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) != 0 {
                r.cpu_s = Some(filetime_s(&kernel) + filetime_s(&user));
            }
            let mut mem: MEMORYSTATUSEX = std::mem::zeroed();
            mem.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut mem) != 0 {
                r.host_total = Some(mem.ullTotalPhys);
                r.host_available = Some(mem.ullAvailPhys);
            }
        }
        r
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::Reading;

    pub const RSS: bool = true;
    pub const CPU: bool = true;

    fn timeval_s(t: &libc::timeval) -> f64 {
        t.tv_sec as f64 + f64::from(t.tv_usec) / 1e6
    }

    pub fn read() -> Reading {
        let mut r = Reading::default();
        // SAFETY: every out-parameter is a zeroed struct owned by this frame, passed with
        // its true size; the sysctl name is a NUL-terminated literal.
        unsafe {
            let mut info: libc::proc_taskinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
            if libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDTASKINFO,
                0,
                (&mut info as *mut libc::proc_taskinfo).cast(),
                size,
            ) == size
            {
                r.rss = Some(info.pti_resident_size);
            }
            let mut ru: libc::rusage = std::mem::zeroed();
            if libc::getrusage(libc::RUSAGE_SELF, &mut ru) == 0 {
                r.cpu_s = Some(timeval_s(&ru.ru_utime) + timeval_s(&ru.ru_stime));
                // Bytes on macOS (KiB on Linux).
                r.peak_rss = u64::try_from(ru.ru_maxrss).ok();
            }
            let mut total: u64 = 0;
            let mut len = std::mem::size_of::<u64>();
            if libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&mut total as *mut u64).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            ) == 0
            {
                r.host_total = Some(total);
            }
        }
        r
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    windows,
    target_os = "macos"
)))]
mod platform {
    use super::Reading;

    pub const RSS: bool = false;
    pub const CPU: bool = false;

    pub fn read() -> Reading {
        Reading::default()
    }
}

pub mod api {
    use super::{SUPPORTS_CPU, SUPPORTS_RSS};
    use crate::analytics::Analytics;
    use crate::analytics::api::{ApiQuery, answer, query};
    use axum::response::Response;
    use bunko_db::analytics::keys;
    use serde_json::{Value, json};

    /// A series of optional numbers on the axis `t` (`null`: no reading then, e.g. the
    /// server was not running; a reading is never invented as 0).
    fn series<T: Copy + Into<Value>>(key: &str, label: &str, values: Vec<Option<T>>) -> Value {
        json!({"key": key, "label": label, "values": values.into_iter().map(|v| v.map_or(Value::Null, Into::into)).collect::<Vec<_>>()})
    }

    /// `GET /_admin/api/analytics/host?range=24h|7d|30d` (design §10): the readings as
    /// time series, per minute for 24h and per hour for 7d and 30d (`resolution`, and
    /// `range.granularity` is `hour` for both, the browser's local time), plus `now`
    /// and what this platform can read.
    pub async fn host(a: &Analytics, q: &ApiQuery) -> Response {
        let minute = match q.range.key {
            "24h" => true,
            "7d" | "30d" => false,
            other => {
                return crate::admin::error(
                    400,
                    format!(
                        "range must be one of 24h, 7d, 30d for server readings (got '{other}')"
                    ),
                );
            }
        };
        let now = a.host().now();
        let (queued, reading_dropped) = (a.reading().queued(), a.reading().dropped());
        let db = a.db().clone();
        let (step, from, to) = if minute {
            (60, q.range.from.div_euclid(60), q.now.div_euclid(60))
        } else {
            (3600, q.range.from.div_euclid(3600), q.now.div_euclid(3600))
        };
        let got = query("analytics host", move || {
            let dropped = db.meta_i64(keys::DROPPED_TOTAL)?.unwrap_or(0);
            if minute {
                let rows = db.host_minutes(from, to)?;
                Ok((
                    rows.into_iter()
                        .map(|m| {
                            (
                                m.minute,
                                [
                                    m.rss_avg.map(|v| v as f64),
                                    m.rss_max.map(|v| v as f64),
                                    m.cpu_pct,
                                    m.cpu_pct,
                                    Some(m.connections_max as f64),
                                    Some(m.inflight_max as f64),
                                ],
                            )
                        })
                        .collect::<Vec<_>>(),
                    dropped,
                ))
            } else {
                let rows = db.host_hours(from, to)?;
                Ok((
                    rows.into_iter()
                        .map(|h| {
                            (
                                h.hour,
                                [
                                    h.rss_avg.map(|v| v as f64),
                                    h.rss_max.map(|v| v as f64),
                                    h.cpu_avg.map(super::round1),
                                    h.cpu_max.map(super::round1),
                                    Some(h.connections_max as f64),
                                    Some(h.inflight_max as f64),
                                ],
                            )
                        })
                        .collect(),
                    dropped,
                ))
            }
        })
        .await;
        let (rows, dropped) = match got {
            Ok(v) => v,
            Err(r) => return r,
        };
        let n = usize::try_from(to - from + 1).unwrap_or(0);
        let mut cols: [Vec<Option<f64>>; 6] = std::array::from_fn(|_| vec![None; n]);
        for (bucket, vals) in rows {
            let Ok(i) = usize::try_from(bucket - from) else {
                continue;
            };
            if i >= n {
                continue;
            }
            for (c, v) in cols.iter_mut().zip(vals) {
                c[i] = v;
            }
        }
        let [rss, rss_max, cpu, cpu_max, conns, inflight] = cols;
        let as_int = |v: Vec<Option<f64>>| -> Vec<Option<u64>> {
            v.into_iter().map(|x| x.map(|x| x as u64)).collect()
        };
        let t: Vec<i64> = (from..=to).map(|b| b * step).collect();
        let mut range = serde_json::Map::new();
        range.insert("key".into(), json!(q.range.key));
        range.insert("from".into(), json!(from * step));
        range.insert("to".into(), json!(q.range.to));
        range.insert("granularity".into(), json!("hour"));
        answer(
            a,
            q,
            json!({
                "range": range,
                "resolution": if minute { "minute" } else { "hour" },
                "step": step,
                "t": t,
                "series": [
                    series("rss", "Average RSS", as_int(rss)),
                    series("rss_max", "Highest RSS", as_int(rss_max)),
                    series("cpu_pct", "CPU", cpu),
                    series("cpu_max", "Highest CPU", cpu_max),
                    series("connections_max", "Connections", as_int(conns)),
                    series("inflight_max", "In flight", as_int(inflight)),
                ],
                "now": {
                    "rss": now.rss,
                    "peak_rss": now.peak_rss,
                    "cpu_pct": now.cpu_pct,
                    "connections": now.connections,
                    "inflight": now.inflight,
                    "host_total": now.host_total,
                    "host_available": now.host_available,
                    "reading_queue": queued,
                    "reading_dropped": reading_dropped,
                    "collector_dropped": dropped,
                },
                "platform_supported": {
                    "rss": SUPPORTS_RSS && now.rss.is_some(),
                    "cpu": SUPPORTS_CPU,
                },
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_parsers() {
        let status =
            "Name:\tmokuro-bunko\nVmPeak:\t  900 kB\nVmHWM:\t   2048 kB\nVmRSS:\t   1024 kB\n";
        assert_eq!(proc_kb(status, "VmRSS"), Some(1024 * 1024));
        assert_eq!(proc_kb(status, "VmHWM"), Some(2048 * 1024));
        assert_eq!(proc_kb(status, "VmSwap"), None);
        // A name with spaces and a parenthesis.
        let stat = "4242 (tokio (rt) w) S 1 4242 4242 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 9";
        assert_eq!(proc_stat_cpu_s(stat), Some(3.0));
        assert_eq!(proc_stat_cpu_s("garbage"), None);
        let meminfo = "MemTotal:       16000000 kB\nMemFree:  1 kB\nMemAvailable:    8000000 kB\n";
        assert_eq!(proc_kb(meminfo, "MemTotal"), Some(16_000_000 * 1024));
        assert_eq!(proc_kb(meminfo, "MemAvailable"), Some(8_000_000 * 1024));
        assert_eq!(
            cgroup_memory(
                "1073741824\n",
                "600000000\n",
                "anon 1\ninactive_file 100000000\n"
            ),
            Some((1_073_741_824, 1_073_741_824 - 500_000_000))
        );
        assert_eq!(cgroup_memory("max\n", "1", ""), None);
    }

    #[test]
    fn this_platform_reads_itself() {
        let r = read();
        if SUPPORTS_RSS {
            let rss = r.rss.expect("rss");
            assert!(rss > 1 << 20, "{rss}");
            assert!(r.peak_rss.unwrap_or(0) >= rss / 2);
            assert!(r.host_total.unwrap_or(0) > rss);
        }
        if SUPPORTS_CPU {
            assert!(r.cpu_s.is_some());
        }
    }

    #[test]
    fn four_samples_make_a_minute() {
        let mut s = SamplerState::default();
        let t0 = Instant::now();
        let m0 = 29_000_000 * 60;
        let read = |rss, cpu| Reading {
            rss: Some(rss),
            peak_rss: Some(rss * 2),
            cpu_s: Some(cpu),
            host_total: Some(1 << 34),
            host_available: Some(1 << 33),
        };
        // The first sample has no CPU interval yet.
        let row = s.fold(read(100, 10.0), m0, t0, 3, 1);
        assert_eq!((row.rss_avg, row.cpu_pct), (Some(100), None));
        // 1.5 s of CPU in 15 s: 10 % of one core.
        let row = s.fold(read(200, 11.5), m0 + 15, t0 + SAMPLE_EVERY, 1, 4);
        assert_eq!(row.cpu_pct, Some(10.0));
        let row = s.fold(read(300, 41.5), m0 + 30, t0 + SAMPLE_EVERY * 2, 7, 0);
        // 30 s of CPU in 15 s: two busy cores.
        assert_eq!(row.cpu_pct, Some(105.0));
        let row = s.fold(read(400, 41.5), m0 + 45, t0 + SAMPLE_EVERY * 3, 2, 2);
        assert_eq!(row.minute, m0 / 60);
        assert_eq!((row.rss_avg, row.rss_max), (Some(250), Some(400)));
        assert_eq!(row.cpu_pct, Some(70.0));
        assert_eq!((row.connections_max, row.inflight_max), (7, 4));
        // The next minute starts over.
        let row = s.fold(read(50, 41.5), m0 + 60, t0 + SAMPLE_EVERY * 4, 1, 1);
        assert_eq!(row.minute, m0 / 60 + 1);
        assert_eq!(
            (row.rss_avg, row.rss_max, row.cpu_pct),
            (Some(50), Some(50), Some(0.0))
        );
        assert_eq!(row.connections_max, 1);
        // An unsupported platform: no numbers, never zeros.
        let row = s.fold(Reading::default(), m0 + 120, t0 + SAMPLE_EVERY * 8, 0, 0);
        assert_eq!((row.rss_avg, row.rss_max, row.cpu_pct), (None, None, None));
    }

    /// `cargo test -p bunko-server --lib analytics::host::tests::read_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement, not a check"]
    fn read_cost() {
        let n = 2000;
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(read());
        }
        eprintln!(
            "host read: {:.1} us/sample",
            t.elapsed().as_secs_f64() * 1e6 / n as f64
        );
    }

    #[test]
    fn now_reads_before_the_first_sample() {
        let g = Arc::new(Gauges::default());
        let _c = g.connection();
        let h = Host::new(g);
        let n = h.now();
        assert_eq!((n.connections, n.inflight, n.cpu_pct), (1, 0, None));
        assert_eq!(n.rss.is_some(), SUPPORTS_RSS);
    }
}
