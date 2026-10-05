//! How busy the machine was WHILE a benchmark's pages were coming out (0.5.2
//! `ocr/utilization.py`, ADDENDUM 9).
//!
//! GPU busy and CPU busy are sampled once a second for the life of a benchmark and
//! averaged afterwards over exactly the window a trial was timed over — never over the
//! model load in front of it, never over the gap between trials. The probes are the
//! cheapest the platform offers: `/sys/class/drm/card*/device/gpu_busy_percent` (AMD),
//! `nvidia-smi` when that is missing and an NVIDIA driver answers, and the aggregate
//! `cpu` line of `/proc/stat` differenced between ticks (iowait counts as idle).
//!
//! The same ticks read the card's VRAM in use (`mem_info_vram_used` / `memory.used`):
//! the benchmark reports how far it rose above where it stood when the benchmark began
//! (`peak_vram_mb`; 0.5.2 reported torch's peak allocation inside its runner process,
//! which an in-process libtorch backend cannot ask for).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// An hour of samples at most: a wedged benchmark must not cost memory too.
const MAX_SAMPLES: usize = 4096;
const SYSFS_DRM: &str = "/sys/class/drm";
const PROC_STAT: &str = "/proc/stat";

/// One tick: seconds since the sampler's origin, and what each probe said.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub at: f64,
    pub gpu_pct: Option<f64>,
    pub cpu_pct: Option<f64>,
    pub vram_mb: Option<f64>,
}

/// `gpu_busy_files`: `cardN/device` directories with `gpu_busy_percent`, in numeric
/// card order (so `gpu:1` picks the same card torch's index 1 usually is).
pub fn gpu_cards(root: &Path) -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut cards: Vec<(u64, String, PathBuf)> = dir
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let digits = name.strip_prefix("card")?;
            let index = digits.parse::<u64>().unwrap_or(1 << 30);
            let device = e.path().join("device");
            device
                .join("gpu_busy_percent")
                .is_file()
                .then_some((index, name, device))
        })
        .collect();
    cards.sort();
    cards.into_iter().map(|(_, _, d)| d).collect()
}

fn read_number(path: &Path) -> Option<f64> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
}

fn clamp_pct(v: f64) -> f64 {
    v.clamp(0.0, 100.0)
}

/// `(busy %, VRAM MiB)` of one NVIDIA card through `nvidia-smi`.
fn nvidia_probe(index: Option<u32>) -> (Option<f64>, Option<f64>) {
    let mut cmd = std::process::Command::new("nvidia-smi");
    cmd.args([
        "--query-gpu=utilization.gpu,memory.used",
        "--format=csv,noheader,nounits",
    ]);
    if let Some(i) = index {
        cmd.args(["-i", &i.to_string()]);
    }
    cmd.stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let Ok(out) = cmd.output() else {
        return (None, None);
    };
    if !out.status.success() {
        return (None, None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(line) = text.lines().next() else {
        return (None, None);
    };
    let mut parts = line.split(',').map(|p| p.trim().parse::<f64>().ok());
    let busy = parts.next().flatten().map(clamp_pct);
    let vram = parts.next().flatten();
    (busy, vram)
}

/// Where GPU readings come from, decided ONCE at the start of a run.
#[derive(Debug, Clone)]
enum GpuSource {
    Sysfs(PathBuf),
    Nvidia(Option<u32>),
    None,
}

impl GpuSource {
    fn pick(index: Option<u32>, root: &Path) -> GpuSource {
        let cards = gpu_cards(root);
        if !cards.is_empty() {
            let chosen = index
                .and_then(|i| cards.get(i as usize))
                .unwrap_or(&cards[0]);
            return GpuSource::Sysfs(chosen.clone());
        }
        if nvidia_probe(index).0.is_some() {
            return GpuSource::Nvidia(index);
        }
        GpuSource::None
    }

    fn read(&self) -> (Option<f64>, Option<f64>) {
        match self {
            GpuSource::Sysfs(dir) => (
                read_number(&dir.join("gpu_busy_percent")).map(clamp_pct),
                read_number(&dir.join("mem_info_vram_used")).map(|b| b / (1024.0 * 1024.0)),
            ),
            GpuSource::Nvidia(i) => nvidia_probe(*i),
            GpuSource::None => (None, None),
        }
    }
}

/// `(busy, total)` jiffies off `/proc/stat`'s aggregate line.
fn cpu_totals(stat: &Path) -> Option<(f64, f64)> {
    let text = std::fs::read_to_string(stat).ok()?;
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<f64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    if fields.len() < 4 {
        return None;
    }
    let total: f64 = fields.iter().sum();
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0.0);
    Some((total - idle, total))
}

/// The card a benchmark should watch: the engine's pinned device, else card 0
/// (`first_gpu_device`).
pub fn device_index(device: Option<&str>) -> Option<u32> {
    device?.strip_prefix("gpu:")?.parse().ok()
}

/// 1 Hz GPU/CPU busy for the life of a benchmark. Samples continuously and averages
/// afterwards, because a trial's window is only known when the trial ends.
pub struct Sampler {
    origin: Instant,
    samples: Arc<Mutex<Vec<Sample>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Sampler {
    /// Start sampling the card `index` names (card 0 when None). Times are seconds
    /// since `origin`.
    pub fn start(index: Option<u32>, origin: Instant, interval: Duration) -> Sampler {
        Self::start_at(
            index,
            origin,
            interval,
            Path::new(SYSFS_DRM),
            Path::new(PROC_STAT),
        )
    }

    /// [`Sampler::start`] over other files (tests).
    pub fn start_at(
        index: Option<u32>,
        origin: Instant,
        interval: Duration,
        drm: &Path,
        stat: &Path,
    ) -> Sampler {
        let samples: Arc<Mutex<Vec<Sample>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (samples, stop) = (samples.clone(), stop.clone());
            let (drm, stat) = (drm.to_path_buf(), stat.to_path_buf());
            std::thread::Builder::new()
                .name("ocr-bench-util".into())
                .spawn(move || {
                    let gpu = GpuSource::pick(index, &drm);
                    let mut previous = cpu_totals(&stat);
                    while !stop.load(Ordering::SeqCst) {
                        let (gpu_pct, vram_mb) = gpu.read();
                        let now = cpu_totals(&stat);
                        let cpu_pct = match (now, previous) {
                            (Some((b1, t1)), Some((b0, t0))) if t1 > t0 => {
                                Some(clamp_pct(100.0 * (b1 - b0) / (t1 - t0)))
                            }
                            _ => None,
                        };
                        previous = now;
                        let mut list = samples.lock();
                        list.push(Sample {
                            at: origin.elapsed().as_secs_f64(),
                            gpu_pct,
                            cpu_pct,
                            vram_mb,
                        });
                        if list.len() > MAX_SAMPLES {
                            let excess = list.len() - MAX_SAMPLES;
                            list.drain(..excess);
                        }
                        drop(list);
                        let until = Instant::now() + interval;
                        while Instant::now() < until && !stop.load(Ordering::SeqCst) {
                            std::thread::sleep(
                                (until - Instant::now()).min(Duration::from_millis(50)),
                            );
                        }
                    }
                })
                .ok()
        };
        Sampler {
            origin,
            samples,
            stop,
            thread,
        }
    }

    /// Seconds since the origin.
    pub fn now(&self) -> f64 {
        self.origin.elapsed().as_secs_f64()
    }

    pub fn samples(&self) -> Vec<Sample> {
        self.samples.lock().clone()
    }

    /// `means(first, last)`: `(gpu_busy_pct, cpu_busy_pct)` over the ticks inside
    /// `[first, last]`, 1 decimal; None where nothing was sampled there.
    pub fn means(&self, first: Option<f64>, last: Option<f64>) -> (Option<f64>, Option<f64>) {
        let (Some(first), Some(last)) = (first, last) else {
            return (None, None);
        };
        if last < first {
            return (None, None);
        }
        let inside: Vec<Sample> = self
            .samples
            .lock()
            .iter()
            .filter(|s| first <= s.at && s.at <= last)
            .copied()
            .collect();
        let mean = |values: Vec<f64>| {
            (!values.is_empty())
                .then(|| bunko_sched::py::round_to(bunko_sched::py::mean(&values), 1))
        };
        (
            mean(inside.iter().filter_map(|s| s.gpu_pct).collect()),
            mean(inside.iter().filter_map(|s| s.cpu_pct).collect()),
        )
    }

    /// How far the card's VRAM in use rose above its first reading, in MiB.
    pub fn vram_rise_mb(&self) -> Option<i64> {
        let samples = self.samples.lock();
        let readings: Vec<f64> = samples.iter().filter_map(|s| s.vram_mb).collect();
        let first = *readings.first()?;
        let peak = readings.iter().copied().fold(first, f64::max);
        Some((peak - first).max(0.0).round() as i64)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The control API's live load (GUI.md §2 `stats.gpu_busy` / `cpu_cores`; stream G1):
/// a [`Sampler`] on card 0 every `interval` for the life of the instance, read as the
/// mean of the last few ticks. Starts sampling on the first read, so an instance nobody
/// looks at never polls `nvidia-smi`.
pub struct LiveLoad {
    interval: Duration,
    sampler: Mutex<Option<Sampler>>,
}

impl LiveLoad {
    pub fn new(interval: Duration) -> LiveLoad {
        LiveLoad {
            interval,
            sampler: Mutex::new(None),
        }
    }

    /// `(gpu busy %, CPU cores busy)` over the last three ticks.
    pub fn read(&self) -> (Option<f64>, Option<f64>) {
        let mut slot = self.sampler.lock();
        let sampler =
            slot.get_or_insert_with(|| Sampler::start(None, Instant::now(), self.interval));
        let now = sampler.now();
        let window = self.interval.as_secs_f64() * 3.5;
        let (gpu, cpu) = sampler.means(Some((now - window).max(0.0)), Some(now));
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as f64)
            .unwrap_or(1.0);
        (
            gpu,
            cpu.map(|pct| bunko_sched::py::round_to(pct * cores / 100.0, 1)),
        )
    }
}

impl bunko_control::LoadProbe for LiveLoad {
    fn sample(&self) -> (Option<f64>, Option<f64>) {
        self.read()
    }
}

/// This process at its largest, in MiB (`ru_maxrss`), or None off Linux.
pub fn peak_rss_mb() -> Option<i64> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: getrusage only writes into the struct we hand it.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: RUSAGE_SELF with a valid pointer is always a defined call.
        let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
        if rc == 0 {
            return Some(usage.ru_maxrss / 1024);
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_cards_in_numeric_order_and_averages_a_window() {
        let dir = tempfile::tempdir().unwrap();
        for (card, busy, vram) in [("card10", "90", "4096"), ("card2", "40", "2097152")] {
            let d = dir.path().join(card).join("device");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("gpu_busy_percent"), busy).unwrap();
            std::fs::write(d.join("mem_info_vram_used"), vram).unwrap();
        }
        // A connector directory has no busy file and is skipped.
        std::fs::create_dir_all(dir.path().join("card2-DP-1/device")).unwrap();
        let cards = gpu_cards(dir.path());
        assert_eq!(cards.len(), 2);
        assert!(cards[0].starts_with(dir.path().join("card2")));
        let stat = dir.path().join("stat");
        std::fs::write(&stat, "cpu  100 0 100 800 0 0 0 0 0 0\n").unwrap();
        let mut s = Sampler::start_at(
            Some(1),
            Instant::now(),
            Duration::from_millis(20),
            dir.path(),
            &stat,
        );
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(&stat, "cpu  150 0 150 900 0 0 0 0 0 0\n").unwrap();
        std::thread::sleep(Duration::from_millis(80));
        s.stop();
        let (gpu, cpu) = s.means(Some(0.0), Some(10.0));
        assert_eq!(gpu, Some(90.0), "gpu:1 is card10");
        assert!(cpu.is_some_and(|c| (0.0..=100.0).contains(&c)), "{cpu:?}");
        assert_eq!(s.means(Some(50.0), Some(60.0)), (None, None));
        assert_eq!(s.means(None, Some(1.0)), (None, None));
        assert_eq!(s.vram_rise_mb(), Some(0));
        assert_eq!(device_index(Some("gpu:3")), Some(3));
        assert_eq!(device_index(Some("auto")), None);
    }

    #[test]
    fn peak_rss_is_reported_on_linux() {
        if cfg!(target_os = "linux") {
            assert!(peak_rss_mb().is_some_and(|m| m > 0));
        }
    }
}
