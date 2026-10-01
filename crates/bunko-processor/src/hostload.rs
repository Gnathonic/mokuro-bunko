//! Host-busy readouts for `stats` and `volume_done` (spec ocr-scheduling §9.7).
//!
//! The library does not learn a volume's speed when the host was busy: CPU pressure
//! ≥ 0.6, or other processes using ≥ half the CPU. `cpu_pressure` is Linux PSI
//! (`/proc/pressure/cpu`, `some avg10` / 100). `other_cpu` is the host's busy time
//! minus this process's own CPU time over a window, as a share of the host's total
//! (the in-process replacement for 0.5.2's walk over runner process trees). Both are
//! `None` where the kernel does not say (anything but Linux).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// `some avg10` of `/proc/pressure/cpu`, as a share (0–1, 3 dp).
pub fn cpu_pressure() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/pressure/cpu").ok()?;
    parse_psi_avg10(&text)
}

fn parse_psi_avg10(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    let value = line
        .split_whitespace()
        .find_map(|part| part.strip_prefix("avg10="))?;
    let avg: f64 = value.parse().ok()?;
    Some((avg / 100.0 * 1000.0).round() / 1000.0)
}

/// One reading of the host's and this process's CPU time (clock ticks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostSample {
    pub host_busy: u64,
    pub host_total: u64,
    pub own: u64,
}

impl HostSample {
    /// Now, or `None` off Linux.
    pub fn now() -> Option<HostSample> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let own = std::fs::read_to_string("/proc/self/stat").ok()?;
        let (host_busy, host_total) = parse_proc_stat(&stat)?;
        Some(HostSample {
            host_busy,
            host_total,
            own: parse_self_stat(&own)?,
        })
    }
}

/// `(busy, total)` ticks from the aggregate `cpu` line of `/proc/stat`.
fn parse_proc_stat(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    if fields.len() < 4 {
        return None;
    }
    // user nice system idle iowait irq softirq steal (guest time is inside user/nice).
    let total: u64 = fields.iter().take(8).sum();
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    Some((total.saturating_sub(idle), total))
}

/// utime + stime of `/proc/self/stat` (fields 14 and 15, counted after the `(comm)`).
fn parse_self_stat(text: &str) -> Option<u64> {
    let rest = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After ')' the first field is the state (field 3), so utime (14) is index 11.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// The share of the host's CPU (0–1, 3 dp) other processes used between two samples.
pub fn other_cpu_share(before: Option<HostSample>, after: Option<HostSample>) -> Option<f64> {
    let (before, after) = (before?, after?);
    let total = after.host_total.checked_sub(before.host_total)?;
    if total == 0 {
        return None;
    }
    let busy = after.host_busy.saturating_sub(before.host_busy) as f64;
    let own = after.own.saturating_sub(before.own) as f64;
    let share = ((busy - own) / total as f64).clamp(0.0, 1.0);
    Some((share * 1000.0).round() / 1000.0)
}

/// `other_cpu` over roughly the last ten seconds, for the periodic `stats` event.
#[derive(Debug, Default)]
pub struct HostMeter {
    samples: VecDeque<(Instant, HostSample)>,
}

/// The window `stats.other_cpu` covers.
const WINDOW: Duration = Duration::from_secs(10);

impl HostMeter {
    /// Take a sample and answer `other_cpu` against the newest sample at least
    /// [`WINDOW`] old (or the oldest kept, early in a session).
    pub fn other_cpu(&mut self) -> Option<f64> {
        let now = Instant::now();
        let sample = HostSample::now()?;
        while self.samples.len() > 1 && now.duration_since(self.samples[1].0) >= WINDOW {
            self.samples.pop_front();
        }
        let base = self.samples.front().map(|(_, s)| *s);
        self.samples.push_back((now, sample));
        other_cpu_share(base, Some(sample))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_psi() {
        let text = "some avg10=12.34 avg60=1.00 avg300=0.00 total=123\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(parse_psi_avg10(text), Some(0.123));
        assert_eq!(parse_psi_avg10("garbage"), None);
    }

    #[test]
    fn parses_proc_stat_and_self_stat() {
        let stat = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3 4\n";
        assert_eq!(parse_proc_stat(stat), Some((150, 1000)));
        let own = "1234 (my (odd) name) S 1 2 3 4 5 6 7 8 9 10 70 30 0 0 20 0";
        assert_eq!(parse_self_stat(own), Some(100));
    }

    #[test]
    fn other_share_subtracts_own_time() {
        let a = HostSample {
            host_busy: 100,
            host_total: 1000,
            own: 50,
        };
        let b = HostSample {
            host_busy: 700,
            host_total: 2000,
            own: 350,
        };
        assert_eq!(other_cpu_share(Some(a), Some(b)), Some(0.3));
        assert_eq!(other_cpu_share(None, Some(b)), None);
    }

    #[test]
    fn live_readouts_do_not_panic() {
        let _ = cpu_pressure();
        let mut meter = HostMeter::default();
        let _ = meter.other_cpu();
        let _ = meter.other_cpu();
    }
}
