//! The Vulkan route (docs/rust-port/VULKAN.md): GPUs no compiled libtorch package covers
//! read with the **Vulkan backend pack** (`<storage>/backends/vulkan-*`, crate
//! `bunko-vulkan`), which carries two interchangeable runtimes, ncnn and WebGPU.
//!
//! * Candidates: with the defaults every runtime that can drive a GPU is one (beside
//!   libtorch's own formats on a GPU libtorch drives, as `fp32@<runtime>`; alone on a
//!   GPU it does not); the machine's benchmark measures them and the fastest is used
//!   (bunko-sched `needs_bench`). [`EXCEPTIONS`] takes runtimes out or forces one;
//!   `ocr.backend: vulkan` pins the Vulkan runtimes in `ocr.vulkan_backends` order.
//! * Per (device, engine) the first runtime of `ocr.vulkan_backends` that loads **and
//!   passes the self-test** (the engine's golden crops read into the golden texts) is
//!   used ([`select`]). Verdicts are cached in `<backends>/vulkan-state.json`, keyed by
//!   runtime, pack, driver, device UUID and model files: a machine tests once per change.
//! * Crash safety: a marker is written before a runtime loads, self-tests and reads its
//!   first batch, and cleared after. A marker left by a process that died demotes that
//!   runtime on that device at the next start.
//! * A read that fails during a volume demotes the runtime on that device and the same
//!   read is retried on the next runtime ([`VulkanRecognizer`]); with none left the
//!   session falls back to the CPU (the caller's GPU→CPU fallback), saying why.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use bunko_torch::abi::{DeviceEntry, DevicesReport, LoadOptions, PACK_JSON};
use bunko_vlm::crop::{LINE_MARGIN_EM, Quad, SECOND_MARGIN_EM, hayai_line_crops, paddle_quad_crop};
use bunko_vlm::{Bgr, CropSet, Recognizer, RecognizerInfo, Rgb, VlmError, read_flat};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::torch::loader::{Loaded, Pack};

/// `pack.json` `variant` of the Vulkan pack.
pub const VARIANT: &str = "vulkan";
/// Use this pack directory instead of searching the backends directories.
pub const PACK_ENV: &str = "MOKURO_VULKAN_PACK";
/// The verdict cache and crash markers, in the first backends directory.
pub const STATE_FILE: &str = "vulkan-state.json";
/// `ocr.vulkan_backends` when unset.
pub const DEFAULT_ORDER: &[&str] = &["ncnn", "webgpu"];
/// PCI vendor ids.
pub const AMD: u32 = 0x1002;
pub const NVIDIA: u32 = 0x10de;
pub const INTEL: u32 = 0x8086;

// ---------------------------------------------------------------------------
// Candidates and exceptions

/// What a hardcoded [`Exception`] does to a runtime on matching devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Never a candidate there (the reason is shown instead).
    Exclude,
    /// The only fp32 candidate there: used without a benchmark.
    Force,
    /// Not benchmarked and not fetched ahead: fetched and self-tested only when every
    /// backend ranked above it (libtorch, the candidate runtimes) fails on that device.
    FallbackOnly,
}

/// One row of [`EXCEPTIONS`]; `None` fields match anything.
#[derive(Clone, Copy, Debug)]
pub struct Exception {
    pub runtime: &'static str,
    pub engine: Option<&'static str>,
    pub os: Option<&'static str>,
    /// PCI vendor id.
    pub vendor: Option<u32>,
    /// gfx / sm architecture prefix (`gfx8`, `gfx90`), when the host knows it.
    pub arch: Option<&'static str>,
    /// `Some(true)`: only on a GPU libtorch drives too (its pack covers it).
    pub beside_libtorch: Option<bool>,
    pub action: Action,
    pub why: &'static str,
}

/// Hardcoded overrides of the benchmark (docs/rust-port/VULKAN.md §4): with the defaults
/// every runtime that can drive a GPU is a candidate, the ones that load and pass the
/// self-test are measured by the machine's benchmark and the fastest is used. A row here
/// takes a runtime out, makes it the only one, or keeps it as a fallback only, where
/// measurements showed it offers nothing there. Each reason carries its evidence.
pub const EXCEPTIONS: &[Exception] = &[
    Exception {
        runtime: "ncnn",
        engine: None,
        os: None,
        vendor: Some(NVIDIA),
        arch: None,
        beside_libtorch: Some(true),
        action: Action::FallbackOnly,
        why: "the CUDA pack drives this NVIDIA GPU: ncnn measured 0.40-0.53x of CUDA (RTX 4060: hayai 1.006 vs 2.5 p/s, paddle 0.141 = 0.49x; RTX 3070 Windows hayai 0.925, paddle 0.146)",
    },
    Exception {
        runtime: "webgpu",
        engine: None,
        os: None,
        vendor: Some(NVIDIA),
        arch: None,
        beside_libtorch: Some(true),
        action: Action::FallbackOnly,
        why: "the CUDA pack drives this NVIDIA GPU: WebGPU measured 0.40-0.53x of CUDA (RTX 4060 paddle 0.154 p/s; hayai below ncnn there)",
    },
    Exception {
        runtime: "webgpu",
        engine: Some("paddle-manga"),
        os: None,
        vendor: Some(AMD),
        arch: None,
        beside_libtorch: None,
        action: Action::FallbackOnly,
        why: "ncnn reads paddle-manga faster on AMD (196 p: RX 6600 0.144 vs 0.122, 6900 XT 0.287 vs 0.211, 9070 XT 0.398 vs 0.215 p/s; server benchmarks 2026-10-12: RX 6600 0.175 vs 0.029, 9070 XT 0.47 vs 0.09) and WebGPU needs 7 GB of VRAM to ncnn's 4.6",
    },
];

/// How a runtime takes part on a device for an engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// Benchmarked (and fetched ahead) with the device's other formats.
    Candidate,
    /// The only fp32 candidate (an [`Action::Force`] row).
    Forced(&'static str),
    /// Tried only when everything ranked above it fails ([`Action::FallbackOnly`]).
    FallbackOnly(&'static str),
    /// Never used there ([`Action::Exclude`]).
    Excluded(&'static str),
}

/// A runtime's [`Standing`] from `table`.
pub fn standing(
    table: &[Exception],
    runtime: &str,
    engine: &str,
    os: &str,
    vendor: u32,
    arch: Option<&str>,
    beside_libtorch: bool,
) -> Standing {
    match exception(table, runtime, engine, os, vendor, arch, beside_libtorch) {
        None => Standing::Candidate,
        Some(e) => match e.action {
            Action::Exclude => Standing::Excluded(e.why),
            Action::Force => Standing::Forced(e.why),
            Action::FallbackOnly => Standing::FallbackOnly(e.why),
        },
    }
}

/// The first row of `table` matching a runtime of `engine` on a device.
pub fn exception<'a>(
    table: &'a [Exception],
    runtime: &str,
    engine: &str,
    os: &str,
    vendor: u32,
    arch: Option<&str>,
    beside_libtorch: bool,
) -> Option<&'a Exception> {
    table.iter().find(|e| {
        e.runtime == runtime
            && e.engine.is_none_or(|x| x == engine)
            && e.os.is_none_or(|x| x == os)
            && e.vendor.is_none_or(|x| x == vendor)
            && e.arch
                .is_none_or(|x| arch.is_some_and(|a| a.starts_with(x)))
            && e.beside_libtorch.is_none_or(|b| b == beside_libtorch)
    })
}

/// The backend-qualified format of a runtime: `fp32@ncnn`.
pub fn qualified(runtime: &str) -> &'static str {
    match runtime {
        "ncnn" => "fp32@ncnn",
        "webgpu" => "fp32@webgpu",
        _ => "fp32",
    }
}

/// PCI vendor of a pack device (`arch` is `1002:67df`).
pub fn vendor_of(d: &DeviceEntry) -> u32 {
    d.arch
        .split(':')
        .next()
        .and_then(|v| u32::from_str_radix(v, 16).ok())
        .unwrap_or(0)
}

/// `0000:03:00.0` → gfx target, from the KFD topology (Linux; empty elsewhere or
/// without the amdgpu KFD).
pub fn kfd_gfx_by_pci(nodes: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(rd) = std::fs::read_dir(nodes) else {
        return out;
    };
    for e in rd.flatten() {
        let Ok(props) = std::fs::read_to_string(e.path().join("properties")) else {
            continue;
        };
        let field = |k: &str| {
            props
                .lines()
                .find_map(|l| l.strip_prefix(k).map(str::trim))
                .and_then(|v| v.parse::<u64>().ok())
        };
        let (Some(v), Some(loc)) = (field("gfx_target_version "), field("location_id ")) else {
            continue;
        };
        if v == 0 {
            continue;
        }
        let domain = field("domain ").unwrap_or(0);
        let pci = format!(
            "{:04x}:{:02x}:{:02x}.{:x}",
            domain,
            loc >> 8,
            (loc >> 3) & 0x1f,
            loc & 0x7
        );
        let v = v as u32;
        out.insert(
            pci,
            format!("gfx{}{:x}{:x}", v / 10000, (v / 100) % 100, v % 100),
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Verdicts and crash markers

/// What a runtime proved on a device for an engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub pass: bool,
    pub reason: String,
    pub runtime: String,
    pub engine: String,
    pub device: String,
    /// Unix seconds.
    pub at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Marker {
    key: String,
    runtime: String,
    engine: String,
    device: String,
    stage: String,
    pid: u32,
    /// This process's token (a pid can be reused).
    process: String,
    at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    #[serde(default)]
    verdicts: BTreeMap<String, Verdict>,
    #[serde(default)]
    markers: Vec<Marker>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A marker of another process older than this is a crash even where liveness cannot be
/// checked (loads and self-tests take seconds).
const STALE_MARKER_SECS: u64 = 15 * 60;

/// Whether process `pid` exists and has not exited (`None`: cannot tell). The same
/// check as bunko-tray's `discover::pid_alive`.
fn pid_alive(pid: u32) -> Option<bool> {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return Some(false);
        };
        // SAFETY: kill with signal 0 only checks for existence and permission.
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 {
            return Some(true);
        }
        let err = std::io::Error::last_os_error().raw_os_error();
        Some(err == Some(libc::EPERM))
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: plain Win32 calls on a handle this function owns and closes.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                // No such process, or one of another user's that we may not open.
                let err = std::io::Error::last_os_error().raw_os_error();
                return Some(err == Some(ERROR_ACCESS_DENIED as i32));
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code);
            CloseHandle(h);
            if ok == 0 {
                return None;
            }
            Some(code == STILL_ACTIVE as u32)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        None
    }
}

/// The verdict cache and markers of one storage (`<backends>/vulkan-state.json`),
/// shared by the processes using it (each write re-reads the file first).
pub struct VerdictStore {
    path: PathBuf,
    process: String,
    lock: Mutex<()>,
}

impl VerdictStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            process: uuid::Uuid::new_v4().to_string(),
            lock: Mutex::new(()),
        }
    }

    fn read(&self) -> StateFile {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write(&self, s: &StateFile) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self
            .path
            .with_extension(format!("json.{}.tmp", std::process::id()));
        let ok = serde_json::to_vec_pretty(s)
            .ok()
            .is_some_and(|b| std::fs::write(&tmp, b).is_ok());
        if !ok || std::fs::rename(&tmp, &self.path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            warn!("could not write {}", self.path.display());
        }
    }

    fn update<T>(&self, f: impl FnOnce(&mut StateFile) -> T) -> T {
        let _g = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut s = self.read();
        let out = f(&mut s);
        self.write(&s);
        out
    }

    /// Turns markers left by processes that died into failed verdicts; returns them.
    pub fn reap_crashes(&self) -> Vec<Verdict> {
        let me = self.process.clone();
        let t = now();
        self.update(|s| {
            let mut out = Vec::new();
            s.markers.retain(|m| {
                if m.process == me {
                    return true;
                }
                // Another process's marker under this process's pid: that process is gone
                // (on every OS, not only where the pid can be looked up).
                let dead = m.pid == std::process::id()
                    || match pid_alive(m.pid) {
                        Some(alive) => !alive,
                        None => t.saturating_sub(m.at) > STALE_MARKER_SECS,
                    };
                if dead {
                    let v = Verdict {
                        pass: false,
                        reason: format!(
                            "the process stopped while {} was {} (pid {}): a crash in the runtime or the driver",
                            m.runtime, m.stage, m.pid
                        ),
                        runtime: m.runtime.clone(),
                        engine: m.engine.clone(),
                        device: m.device.clone(),
                        at: t,
                    };
                    s.verdicts.insert(m.key.clone(), v.clone());
                    out.push(v);
                }
                !dead
            });
            out
        })
    }

    pub fn verdict(&self, key: &str) -> Option<Verdict> {
        let _g = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        self.read().verdicts.get(key).cloned()
    }

    pub fn record(&self, key: &str, v: Verdict) {
        self.update(|s| {
            s.verdicts.insert(key.to_string(), v);
        });
    }

    /// Every verdict (doctor, the admin panel).
    pub fn all(&self) -> BTreeMap<String, Verdict> {
        let _g = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        self.read().verdicts
    }

    fn mark(&self, key: &str, c: &Candidate, engine: &str, device: &str, stage: &str) {
        let m = Marker {
            key: key.to_string(),
            runtime: c.runtime.clone(),
            engine: engine.to_string(),
            device: device.to_string(),
            stage: stage.to_string(),
            pid: std::process::id(),
            process: self.process.clone(),
            at: now(),
        };
        self.update(|s| {
            s.markers
                .retain(|x| !(x.key == m.key && x.process == m.process));
            s.markers.push(m);
        });
    }

    fn unmark(&self, key: &str) {
        let me = self.process.clone();
        self.update(|s| s.markers.retain(|x| !(x.key == key && x.process == me)));
    }
}

// ---------------------------------------------------------------------------
// Self-test goldens

/// One engine's golden crops and the texts libtorch fp32 reads from them
/// (`vulkan/selftest/<engine>.json`, tools/vulkan_selftest).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Golden {
    pub engine: String,
    pub crops: Vec<GoldenCrop>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoldenCrop {
    pub width: u32,
    pub height: u32,
    /// RGB8 rows, base64.
    pub rgb: String,
    pub text: String,
    /// paddle-manga's token cap for this crop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<u32>,
}

impl Golden {
    pub fn load(path: &Path) -> Result<Golden, String> {
        let s = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        serde_json::from_str(&s).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn decoded(&self) -> Result<Vec<(Vec<u8>, u32, u32)>, String> {
        use base64::Engine as _;
        self.crops
            .iter()
            .map(|c| {
                let b = base64::engine::general_purpose::STANDARD
                    .decode(&c.rgb)
                    .map_err(|e| format!("golden crop: {e}"))?;
                if b.len() != (c.width * c.height * 3) as usize {
                    return Err("golden crop: wrong size".into());
                }
                Ok((b, c.width, c.height))
            })
            .collect()
    }

    /// Reads the crops with `reader`; None when every text is the golden one, else what
    /// differs.
    pub fn check(&self, reader: &dyn Reader) -> Result<(), String> {
        let crops = self.decoded()?;
        let raw: Vec<(&[u8], u32, u32)> = crops
            .iter()
            .map(|(b, w, h)| (b.as_slice(), *w, *h))
            .collect();
        let caps: Option<Vec<u32>> = self
            .crops
            .iter()
            .map(|c| c.cap)
            .collect::<Option<Vec<u32>>>();
        let texts = reader.read(&raw, caps.as_deref())?;
        let bad: Vec<String> = self
            .crops
            .iter()
            .zip(&texts)
            .enumerate()
            .filter(|(_, (g, t))| g.text != **t)
            .map(|(i, (g, t))| format!("crop {i}: {t:?} instead of {:?}", g.text))
            .collect();
        if bad.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "self-test: {}/{} crops read differently ({})",
                bad.len(),
                self.crops.len(),
                bad.into_iter().take(3).collect::<Vec<_>>().join("; ")
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Selection

/// A loaded recognizer of one runtime (the pack's handle; fakes in tests).
pub trait Reader: Send + Sync {
    fn read(
        &self,
        crops: &[(&[u8], u32, u32)],
        caps: Option<&[u32]>,
    ) -> Result<Vec<String>, String>;
    fn batch(&self) -> usize;
}

/// Loads an engine with a runtime (the pack; fakes in tests).
pub trait Loader: Send + Sync {
    fn load(
        &self,
        runtime: &str,
        engine: &str,
        device: &str,
        model_dir: &Path,
        opts: &LoadOptions,
    ) -> Result<Box<dyn Reader>, String>;
}

struct PackReader {
    handle: Loaded,
    batch: usize,
}

impl Reader for PackReader {
    fn read(
        &self,
        crops: &[(&[u8], u32, u32)],
        caps: Option<&[u32]>,
    ) -> Result<Vec<String>, String> {
        self.handle.read(crops, caps)
    }

    fn batch(&self) -> usize {
        self.batch
    }
}

impl Loader for Pack {
    fn load(
        &self,
        runtime: &str,
        engine: &str,
        device: &str,
        model_dir: &Path,
        opts: &LoadOptions,
    ) -> Result<Box<dyn Reader>, String> {
        let mut o = opts.clone();
        o.backend = Some(runtime.to_string());
        let handle = Pack::load(self, engine, model_dir, "fp32", device, &o)?;
        let li = handle.info()?;
        info!(
            "{engine} on {device} with {runtime} ({}) loaded in {:.1}s",
            li.torch, li.load_seconds
        );
        Ok(Box::new(PackReader {
            handle,
            batch: li.batch as usize,
        }))
    }
}

/// One runtime that may serve a (device, engine).
#[derive(Clone, Debug)]
pub struct Candidate {
    pub runtime: String,
    /// The runtime's graphs of the engine; None when they are not here.
    pub model_dir: Option<PathBuf>,
    /// The verdict cache key ([`verdict_key`]).
    pub key: String,
    /// The pack part of the key (to key it again once the files are fetched).
    pub pack: String,
}

/// Fetches a runtime's files (graphs, the host files it reads) when [`select`] reaches a
/// candidate whose files are not here: a fallback-only runtime is fetched only then.
pub type Fetch = dyn Fn(&str) -> Result<PathBuf, String> + Send + Sync;

/// The cache key of a runtime's verdict: runtime, pack, driver, device UUID, engine and
/// a fingerprint of the model files (sizes and modification times).
pub fn verdict_key(
    runtime: &str,
    pack: &str,
    dev: &DeviceEntry,
    engine: &str,
    model_dir: Option<&Path>,
) -> String {
    let mut files: Vec<(String, u64, u64)> = Vec::new();
    if let Some(d) = model_dir
        && let Ok(rd) = std::fs::read_dir(d)
    {
        for e in rd.flatten() {
            // Follow links (a development models directory links the graphs in).
            if let Ok(m) = std::fs::metadata(e.path()) {
                let t = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                files.push((e.file_name().to_string_lossy().into_owned(), m.len(), t));
            }
        }
    }
    files.sort();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (n, s, t) in &files {
        for b in n.bytes().chain(s.to_le_bytes()).chain(t.to_le_bytes()) {
            h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
    }
    format!(
        "{runtime}|{pack}|{}|{}|{engine}|{h:016x}",
        dev.driver.as_deref().unwrap_or(""),
        dev.uuid.as_deref().unwrap_or(&dev.id)
    )
}

/// What [`select`] chose.
pub struct Selected {
    pub runtime: String,
    pub reader: Box<dyn Reader>,
    pub key: String,
}

/// The first candidate (in order) that is not excluded, has its files, has no failed
/// verdict, loads and passes the self-test (unless it passed before under the same key).
/// `Err`: why each candidate was not used.
#[allow(clippy::too_many_arguments)]
pub fn select(
    loader: &dyn Loader,
    store: &VerdictStore,
    candidates: &[Candidate],
    exclude: &[String],
    engine: &str,
    device: &DeviceEntry,
    pack_device: &str,
    opts: &LoadOptions,
    golden: Option<&Golden>,
    fetch: Option<&Fetch>,
) -> Result<Selected, Vec<String>> {
    let mut why = Vec::new();
    for c in candidates {
        // A candidate whose files are not here yet (a fallback): fetch them now and key
        // its verdict on them.
        let fetched;
        let c = match (&c.model_dir, fetch) {
            (None, Some(f)) if !exclude.contains(&c.runtime) => match f(&c.runtime) {
                Ok(dir) => {
                    fetched = Candidate {
                        key: verdict_key(&c.runtime, &c.pack, device, engine, Some(&dir)),
                        model_dir: Some(dir),
                        ..c.clone()
                    };
                    &fetched
                }
                Err(e) => {
                    why.push(format!(
                        "{}: could not fetch its {engine} files: {e}",
                        c.runtime
                    ));
                    continue;
                }
            },
            _ => c,
        };
        if exclude.contains(&c.runtime) {
            why.push(format!(
                "{}: failed on this device during this run",
                c.runtime
            ));
            continue;
        }
        let Some(dir) = &c.model_dir else {
            why.push(format!("{}: its {engine} files are not here", c.runtime));
            continue;
        };
        let cached = store.verdict(&c.key);
        if let Some(v) = cached.as_ref().filter(|v| !v.pass) {
            why.push(format!("{}: {}", c.runtime, v.reason));
            continue;
        }
        let fail = |reason: String| {
            store.record(
                &c.key,
                Verdict {
                    pass: false,
                    reason: reason.clone(),
                    runtime: c.runtime.clone(),
                    engine: engine.to_string(),
                    device: device.name.clone(),
                    at: now(),
                },
            );
            format!("{}: {reason}", c.runtime)
        };
        store.mark(&c.key, c, engine, &device.name, "loading");
        let reader = match loader.load(&c.runtime, engine, pack_device, dir, opts) {
            Ok(r) => r,
            Err(e) => {
                store.unmark(&c.key);
                why.push(fail(format!("could not load: {e}")));
                continue;
            }
        };
        if cached.is_none() {
            match golden {
                Some(g) => {
                    store.mark(&c.key, c, engine, &device.name, "self-testing");
                    if let Err(e) = g.check(reader.as_ref()) {
                        store.unmark(&c.key);
                        why.push(fail(e));
                        continue;
                    }
                    store.record(
                        &c.key,
                        Verdict {
                            pass: true,
                            reason: format!("self-test passed ({} crops)", g.crops.len()),
                            runtime: c.runtime.clone(),
                            engine: engine.to_string(),
                            device: device.name.clone(),
                            at: now(),
                        },
                    );
                }
                None => warn!(
                    "{engine} on {} with {}: no self-test file, not verified",
                    device.name, c.runtime
                ),
            }
        }
        // The marker stays until the first batch of a volume is read (`VulkanRecognizer`).
        store.mark(&c.key, c, engine, &device.name, "reading its first batch");
        return Ok(Selected {
            runtime: c.runtime.clone(),
            reader,
            key: c.key.clone(),
        });
    }
    Err(why)
}

/// Demote `runtime` (a read failed during a volume).
pub fn demote(
    store: &VerdictStore,
    key: &str,
    runtime: &str,
    engine: &str,
    device: &str,
    why: &str,
) {
    store.unmark(key);
    store.record(
        key,
        Verdict {
            pass: false,
            reason: format!("failed while reading: {why}"),
            runtime: runtime.to_string(),
            engine: engine.to_string(),
            device: device.to_string(),
            at: now(),
        },
    );
}

// ---------------------------------------------------------------------------
// The report (doctor, the admin panel)

/// The Vulkan route on this machine ([`crate::EnginePipeline::vulkan_report`]).
#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    /// `benchmark` (ocr.backend auto), `pinned` (vulkan) or `off`.
    pub mode: &'static str,
    /// The pack's folder and runtimes, when it is open.
    pub pack: Option<String>,
    /// Why there is no pack (not installed, failed to open).
    pub error: Option<String>,
    pub order: Vec<String>,
    pub warnings: Vec<String>,
    /// Runtimes a crash demoted at this start.
    pub crashes: Vec<Verdict>,
    pub devices: Vec<DeviceReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeviceReport {
    /// The catalog id (`gpu:<n>`).
    pub id: String,
    pub name: String,
    pub driver: String,
    /// Mesa's RADV (the AMD route was measured on it).
    pub radv: bool,
    /// `libtorch` when libtorch drives this GPU too (its formats are candidates as well).
    pub beside: Option<String>,
    pub gfx: Option<String>,
    pub runtimes: Vec<RuntimeStatus>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeStatus {
    pub runtime: String,
    pub engine: &'static str,
    /// `candidate` or `skipped`.
    pub status: &'static str,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// The pack in this process

/// The opened Vulkan pack and what it reported.
pub struct VulkanBackend {
    pub pack: Arc<Pack>,
    pub report: DevicesReport,
    pub store: Arc<VerdictStore>,
    /// Crashes found at start (markers of dead processes), for the log and doctor.
    pub crashes: Vec<Verdict>,
    loaded: Mutex<HashMap<(String, String, u32), Weak<VulkanRecognizer>>>,
}

static BACKEND: Mutex<Option<Result<Arc<VulkanBackend>, String>>> = Mutex::new(None);

/// Installed Vulkan packs in the directories, the first directory's first.
pub fn discover(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(d) else {
            continue;
        };
        let mut found: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                !p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            })
            .filter(|p| p.join(PACK_JSON).is_file())
            .filter(|p| crate::torch::loader::read_manifest(p).is_ok_and(|m| m.variant == VARIANT))
            .collect();
        found.sort();
        found.reverse();
        for p in found {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// The process's Vulkan pack (opened on first use, like the libtorch pack).
pub fn backend_in(dirs: &[PathBuf]) -> Result<Arc<VulkanBackend>, String> {
    let mut slot = BACKEND.lock().unwrap_or_else(|e| e.into_inner());
    slot.get_or_insert_with(|| open(dirs)).clone()
}

/// The pack if [`backend_in`] opened it already.
pub fn backend_if_open() -> Option<Arc<VulkanBackend>> {
    BACKEND
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|r| r.as_ref().ok().cloned())
}

/// Let the next call look for a pack again after a failure (installed meanwhile).
pub fn forget_failure() -> bool {
    let mut slot = BACKEND.lock().unwrap_or_else(|e| e.into_inner());
    match slot.as_ref() {
        Some(Ok(_)) => false,
        _ => {
            *slot = None;
            true
        }
    }
}

/// The pack's `bv_shutdown`, run from an exit handler of this executable: it tears the
/// runtimes down while the Vulkan driver is still loaded (on Windows the operating system
/// unloads the driver before the pack, whose static destructors then crash the exiting
/// process with 0xC0000005; on Linux the same order bit NVIDIA, see bunko-vlm's
/// `webgpu_exit`).
static SHUTDOWN: std::sync::OnceLock<extern "C" fn()> = std::sync::OnceLock::new();

extern "C" fn shutdown_at_exit() {
    shutdown();
}

/// Tear the Vulkan pack's runtimes down now when the pack was opened (idempotent): what
/// a process calls before `std::process::exit` (which skips C exit handlers on Windows).
pub fn shutdown() {
    if let Some(f) = SHUTDOWN.get() {
        f();
    }
}

fn register_shutdown(pack: &Pack) {
    let Some(sym) = pack.optional_symbol(b"bv_shutdown\0") else {
        return;
    };
    // SAFETY: `bv_shutdown` is `extern "C" fn()` in every pack that exports it.
    let f = unsafe { std::mem::transmute::<*const std::ffi::c_void, extern "C" fn()>(sym) };
    if SHUTDOWN.set(f).is_ok() {
        unsafe extern "C" {
            fn atexit(cb: extern "C" fn()) -> std::ffi::c_int;
        }
        // SAFETY: registers a static function with this executable's C runtime; it runs
        // before libraries are unloaded (last registered, first run: after the driver's
        // own handler was registered at the pack's start-up).
        unsafe { atexit(shutdown_at_exit) };
    }
}

fn open(dirs: &[PathBuf]) -> Result<Arc<VulkanBackend>, String> {
    let candidates = match std::env::var_os(PACK_ENV).filter(|v| !v.is_empty()) {
        Some(p) => vec![PathBuf::from(p)],
        None => discover(dirs),
    };
    if candidates.is_empty() {
        return Err("no Vulkan backend pack installed".into());
    }
    let state_dir = dirs.first().cloned().unwrap_or_else(|| PathBuf::from("."));
    let mut errors = Vec::new();
    for dir in candidates {
        match Pack::open(&dir, false).and_then(|p| {
            p.devices()
                .map(|r| (p, r))
                .map_err(crate::torch::PackLoadError::Init)
        }) {
            Ok((pack, report)) => {
                register_shutdown(&pack);
                for w in &report.warnings {
                    warn!("Vulkan backend: {w}");
                }
                let store = Arc::new(VerdictStore::new(state_dir.join(STATE_FILE)));
                let crashes = store.reap_crashes();
                for c in &crashes {
                    warn!(
                        "Vulkan backend: {} on {} demoted for {}: {}",
                        c.runtime, c.device, c.engine, c.reason
                    );
                }
                info!(
                    "Vulkan backend ({}) from {}: {}",
                    report.torch,
                    dir.display(),
                    report
                        .devices
                        .iter()
                        .map(|d| format!("{} {} [{}]", d.id, d.name, d.backends.join(",")))
                        .collect::<Vec<_>>()
                        .join("; ")
                );
                return Ok(Arc::new(VulkanBackend {
                    pack: Arc::new(pack),
                    report,
                    store,
                    crashes,
                    loaded: Mutex::new(HashMap::new()),
                }));
            }
            Err(e) => errors.push(e.to_string()),
        }
    }
    Err(errors.join("; "))
}

/// A GPU of the Vulkan pack as this host uses it: beside a libtorch GPU (the same PCI
/// device: its runtimes are more candidates of that device) or on its own.
#[derive(Clone, Debug)]
pub struct Pairing {
    pub entry: DeviceEntry,
    /// The libtorch catalog id of the same GPU.
    pub torch_id: Option<String>,
    /// Its gfx architecture (AMD, from KFD), for the exceptions.
    pub gfx: Option<String>,
}

impl VulkanBackend {
    /// The pack's GPUs under `backend`: with `auto` every one (paired with the libtorch
    /// GPU of the same PCI address in `torch`), with `vulkan` every one on its own (the
    /// libtorch GPUs are not used then); none otherwise.
    pub fn pairings(&self, backend: crate::Backend, torch: &[DeviceEntry]) -> Vec<Pairing> {
        let gfx = kfd_gfx_by_pci(Path::new("/sys/class/kfd/kfd/topology/nodes"));
        if !matches!(backend, crate::Backend::Auto | crate::Backend::Vulkan) {
            return Vec::new();
        }
        self.report
            .devices
            .iter()
            .map(|d| Pairing {
                entry: d.clone(),
                torch_id: (backend == crate::Backend::Auto)
                    .then(|| {
                        torch
                            .iter()
                            .find(|t| t.kind != "cpu" && t.pci.is_some() && t.pci == d.pci)
                            .map(|t| t.id.clone())
                    })
                    .flatten(),
                gfx: d.pci.as_ref().and_then(|p| gfx.get(p)).cloned(),
            })
            .collect()
    }

    /// The candidates of `engine` on `dev`, in `order` (`ocr.vulkan_backends`), runtimes
    /// the device does not list left out. `model_dir(runtime)` is where its files are.
    pub fn candidates(
        &self,
        order: &[String],
        engine: &str,
        dev: &DeviceEntry,
        golden: Option<&Path>,
        model_dir: impl Fn(&str) -> Option<PathBuf>,
    ) -> Vec<Candidate> {
        // The goldens are part of what a verdict proved: new goldens, new tests.
        let pack = format!(
            "{}@{}@{}",
            self.pack.manifest.bunko_version,
            pack_stamp(&self.pack.dir),
            golden.map(file_stamp).unwrap_or_default()
        );
        order
            .iter()
            .filter(|r| dev.backends.contains(r))
            .map(|r| {
                let dir = model_dir(r);
                Candidate {
                    runtime: r.clone(),
                    key: verdict_key(r, &pack, dev, engine, dir.as_deref()),
                    model_dir: dir,
                    pack: pack.clone(),
                }
            })
            .collect()
    }

    /// The shared recognizer of `engine` on pack device `dev` (selecting a runtime,
    /// self-testing it), kept while anyone holds it.
    #[allow(clippy::too_many_arguments)]
    pub fn recognizer(
        self: &Arc<Self>,
        engine: &'static str,
        dev: &DeviceEntry,
        candidates: Vec<Candidate>,
        opts: LoadOptions,
        golden: Option<Golden>,
        info: RecognizerInfo,
        fetch: Option<Arc<Fetch>>,
    ) -> Result<Arc<VulkanRecognizer>, String> {
        let budget = opts.patch_budget.unwrap_or(0);
        let first = candidates
            .first()
            .map(|c| c.runtime.clone())
            .unwrap_or_default();
        let k = (engine.to_string(), format!("{}/{first}", dev.id), budget);
        let mut map = self.loaded.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(r) = map.get(&k).and_then(Weak::upgrade) {
            return Ok(r);
        }
        let r = Arc::new(VulkanRecognizer::start(
            self.pack.clone(),
            self.store.clone(),
            engine,
            dev,
            candidates,
            opts,
            golden,
            info,
            fetch,
        )?);
        map.retain(|_, w| w.strong_count() > 0);
        map.insert(k, Arc::downgrade(&r));
        Ok(r)
    }

    /// The runtime each loaded recognizer reads with (the admin panel, doctor).
    pub fn active(&self) -> Vec<(String, String, String)> {
        let map = self.loaded.lock().unwrap_or_else(|p| p.into_inner());
        map.values()
            .filter_map(Weak::upgrade)
            .map(|r| (r.engine.to_string(), r.dev.name.clone(), r.runtime()))
            .collect()
    }
}

/// A pack's identity beyond its release: `pack.json`'s size and modification time (a
/// development pack rebuilt in place gets new verdicts).
fn pack_stamp(dir: &Path) -> String {
    file_stamp(&dir.join(PACK_JSON))
}

/// `<size>-<mtime>` of a file (empty when it is not there).
fn file_stamp(path: &Path) -> String {
    std::fs::metadata(path)
        .ok()
        .map(|m| {
            let t = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            format!("{}-{t}", m.len())
        })
        .unwrap_or_default()
}

struct Current {
    runtime: String,
    reader: Arc<dyn Reader>,
    key: String,
    /// The crash marker is still up (cleared after the first batch).
    first: bool,
    /// Runtimes that failed while reading in this process.
    failed: Vec<String>,
}

/// A recognizer on a Vulkan GPU: reads with the selected runtime and moves to the next
/// one when a read fails.
pub struct VulkanRecognizer {
    loader: Arc<dyn Loader>,
    store: Arc<VerdictStore>,
    engine: &'static str,
    dev: DeviceEntry,
    candidates: Vec<Candidate>,
    opts: LoadOptions,
    golden: Option<Golden>,
    info: RecognizerInfo,
    fetch: Option<Arc<Fetch>>,
    current: Mutex<Current>,
}

impl VulkanRecognizer {
    /// Select a runtime among `candidates` with `loader` and wrap it (what
    /// [`VulkanBackend::recognizer`] does with the pack).
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        loader: Arc<dyn Loader>,
        store: Arc<VerdictStore>,
        engine: &'static str,
        dev: &DeviceEntry,
        candidates: Vec<Candidate>,
        opts: LoadOptions,
        golden: Option<Golden>,
        info: RecognizerInfo,
        fetch: Option<Arc<Fetch>>,
    ) -> Result<VulkanRecognizer, String> {
        let sel = select(
            loader.as_ref(),
            &store,
            &candidates,
            &[],
            engine,
            dev,
            &dev.id,
            &opts,
            golden.as_ref(),
            fetch.as_deref(),
        )
        .map_err(|why| {
            format!(
                "no Vulkan runtime runs {engine} on {}: {}",
                dev.name,
                why.join("; ")
            )
        })?;
        info!("{engine} on {} reads with {}", dev.name, sel.runtime);
        Ok(VulkanRecognizer {
            loader,
            store,
            engine,
            dev: dev.clone(),
            candidates,
            opts,
            golden,
            info,
            fetch,
            current: Mutex::new(Current {
                runtime: sel.runtime,
                reader: Arc::from(sel.reader),
                key: sel.key,
                first: true,
                failed: Vec::new(),
            }),
        })
    }

    /// The runtime reading now.
    pub fn runtime(&self) -> String {
        self.current
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .runtime
            .clone()
    }

    fn read_crops(&self, crops: &[&Rgb], caps: Option<&[u32]>) -> Result<Vec<String>, VlmError> {
        let raw: Vec<(&[u8], u32, u32)> = crops
            .iter()
            .map(|c| (c.data.as_slice(), c.width as u32, c.height as u32))
            .collect();
        loop {
            let (reader, key, runtime) = {
                let c = self.current.lock().unwrap_or_else(|p| p.into_inner());
                (c.reader.clone(), c.key.clone(), c.runtime.clone())
            };
            match reader.read(&raw, caps) {
                Ok(t) => {
                    let mut c = self.current.lock().unwrap_or_else(|p| p.into_inner());
                    if c.first && c.key == key {
                        c.first = false;
                        self.store.unmark(&key);
                    }
                    return Ok(t);
                }
                Err(e) => {
                    warn!(
                        "{} on {} with {runtime} failed ({e}); trying the next runtime",
                        self.engine, self.dev.name
                    );
                    demote(&self.store, &key, &runtime, self.engine, &self.dev.name, &e);
                    let mut c = self.current.lock().unwrap_or_else(|p| p.into_inner());
                    if c.key != key {
                        continue; // another thread moved on already
                    }
                    c.failed.push(runtime);
                    let sel = select(
                        self.loader.as_ref(),
                        &self.store,
                        &self.candidates,
                        &c.failed,
                        self.engine,
                        &self.dev,
                        &self.dev.id,
                        &self.opts,
                        self.golden.as_ref(),
                        self.fetch.as_deref(),
                    )
                    .map_err(|why| {
                        VlmError::Runtime(format!(
                            "{e}; no other Vulkan runtime: {}",
                            why.join("; ")
                        ))
                    })?;
                    info!(
                        "{} on {} now reads with {}",
                        self.engine, self.dev.name, sel.runtime
                    );
                    c.runtime = sel.runtime;
                    c.reader = Arc::from(sel.reader);
                    c.key = sel.key;
                    c.first = true;
                }
            }
        }
    }
}

impl Recognizer for VulkanRecognizer {
    fn info(&self) -> &RecognizerInfo {
        &self.info
    }

    fn crop(&self, page: &Bgr, quad: &Quad, vertical: bool) -> CropSet {
        if self.engine == crate::models::PADDLE {
            CropSet::one(paddle_quad_crop(page, quad, LINE_MARGIN_EM))
        } else {
            CropSet {
                crops: hayai_line_crops(page, quad, vertical),
            }
        }
    }

    fn second_crop(&self, page: &Bgr, quad: &Quad, _vertical: bool) -> Option<CropSet> {
        (self.engine == crate::models::PADDLE)
            .then(|| CropSet::one(paddle_quad_crop(page, quad, SECOND_MARGIN_EM)))
    }

    fn read(&self, lines: &[CropSet], caps: Option<&[u32]>) -> Result<Vec<String>, VlmError> {
        let paddle = self.engine == crate::models::PADDLE;
        read_flat(lines, caps, self.info.default_max_tokens, |crops, caps| {
            self.read_crops(crops, paddle.then_some(caps))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A runtime that reads every crop as `text` (or fails), counting loads and reads.
    struct Fake {
        text: String,
        load_err: Option<String>,
        read_err_after: Option<usize>,
        loads: AtomicUsize,
        reads: Arc<AtomicUsize>,
    }

    impl Fake {
        fn new(text: &str) -> Arc<Fake> {
            Arc::new(Fake {
                text: text.into(),
                load_err: None,
                read_err_after: None,
                loads: AtomicUsize::new(0),
                reads: Arc::new(AtomicUsize::new(0)),
            })
        }
    }

    struct FakeReader {
        text: String,
        fail_after: Option<usize>,
        reads: Arc<AtomicUsize>,
    }

    impl Reader for FakeReader {
        fn read(
            &self,
            crops: &[(&[u8], u32, u32)],
            _caps: Option<&[u32]>,
        ) -> Result<Vec<String>, String> {
            let n = self.reads.fetch_add(1, Ordering::SeqCst);
            if self.fail_after.is_some_and(|f| n >= f) {
                return Err("VK_ERROR_DEVICE_LOST".into());
            }
            Ok(vec![self.text.clone(); crops.len()])
        }

        fn batch(&self) -> usize {
            16
        }
    }

    /// Runtimes by name.
    struct Fakes(Vec<(&'static str, Arc<Fake>)>);

    impl Loader for Fakes {
        fn load(
            &self,
            runtime: &str,
            _engine: &str,
            _device: &str,
            _model_dir: &Path,
            opts: &LoadOptions,
        ) -> Result<Box<dyn Reader>, String> {
            assert!(
                opts.backend.is_none(),
                "the pack sets the backend, not the caller"
            );
            let f = &self.0.iter().find(|(n, _)| *n == runtime).unwrap().1;
            f.loads.fetch_add(1, Ordering::SeqCst);
            if let Some(e) = &f.load_err {
                return Err(e.clone());
            }
            Ok(Box::new(FakeReader {
                text: f.text.clone(),
                fail_after: f.read_err_after,
                reads: f.reads.clone(),
            }))
        }
    }

    fn golden(text: &str) -> Golden {
        use base64::Engine as _;
        Golden {
            engine: "hayai-nova".into(),
            crops: vec![GoldenCrop {
                width: 2,
                height: 1,
                rgb: base64::engine::general_purpose::STANDARD.encode([0u8; 6]),
                text: text.into(),
                cap: None,
            }],
        }
    }

    fn dev() -> DeviceEntry {
        DeviceEntry {
            id: "gpu:0".into(),
            kind: "vulkan".into(),
            name: "AMD Radeon RX 580 (RADV POLARIS10)".into(),
            arch: "1002:67df".into(),
            driver: Some("radv Mesa 25.2".into()),
            uuid: Some("00000000030000000000000000000000".into()),
            backends: vec!["ncnn".into(), "webgpu".into()],
            formats: vec!["fp32".into()],
            ..Default::default()
        }
    }

    fn cands(dir: &Path) -> Vec<Candidate> {
        ["ncnn", "webgpu"]
            .iter()
            .map(|r| Candidate {
                runtime: r.to_string(),
                model_dir: Some(dir.to_path_buf()),
                key: verdict_key(r, "pack", &dev(), "hayai-nova", Some(dir)),
                pack: "pack".into(),
            })
            .collect()
    }

    fn pick(
        fakes: &Fakes,
        store: &VerdictStore,
        c: &[Candidate],
        g: Option<&Golden>,
    ) -> Result<Selected, Vec<String>> {
        select(
            fakes,
            store,
            c,
            &[],
            "hayai-nova",
            &dev(),
            "gpu:0",
            &LoadOptions::default(),
            g,
            None,
        )
    }

    #[test]
    fn the_first_runtime_that_passes_its_self_test_is_used_and_remembered() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VerdictStore::new(tmp.path().join(STATE_FILE));
        let (ncnn, wg) = (Fake::new("こんにちは"), Fake::new("こんにちは"));
        let fakes = Fakes(vec![("ncnn", ncnn.clone()), ("webgpu", wg.clone())]);
        let c = cands(tmp.path());
        let g = golden("こんにちは");
        let s = pick(&fakes, &store, &c, Some(&g)).unwrap();
        assert_eq!(s.runtime, "ncnn");
        assert_eq!(ncnn.reads.load(Ordering::SeqCst), 1, "self-tested once");
        assert!(store.verdict(&c[0].key).unwrap().pass);
        assert_eq!(wg.loads.load(Ordering::SeqCst), 0, "never tried");
        // Again (a new session): no second self-test under the same key.
        let s = pick(&fakes, &store, &c, Some(&g)).unwrap();
        assert_eq!(s.runtime, "ncnn");
        assert_eq!(ncnn.reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_self_test_mismatch_hands_over_to_the_next_runtime_for_good() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VerdictStore::new(tmp.path().join(STATE_FILE));
        let (ncnn, wg) = (Fake::new(""), Fake::new("おはよう"));
        let fakes = Fakes(vec![("ncnn", ncnn.clone()), ("webgpu", wg.clone())]);
        let c = cands(tmp.path());
        let g = golden("おはよう");
        let s = pick(&fakes, &store, &c, Some(&g)).unwrap();
        assert_eq!(s.runtime, "webgpu");
        let v = store.verdict(&c[0].key).unwrap();
        assert!(
            !v.pass && v.reason.contains("self-test: 1/1 crops"),
            "{v:?}"
        );
        // The failed runtime is not loaded again.
        let s = pick(&fakes, &store, &c, Some(&g)).unwrap();
        assert_eq!(s.runtime, "webgpu");
        assert_eq!(ncnn.loads.load(Ordering::SeqCst), 1);
        // ... until something under the key changes (a driver update: a new key).
        let mut d = dev();
        d.driver = Some("radv Mesa 25.3".into());
        let k = verdict_key("ncnn", "pack", &d, "hayai-nova", Some(tmp.path()));
        assert_ne!(k, c[0].key);
        assert!(store.verdict(&k).is_none());
    }

    #[test]
    fn load_failures_fall_through_and_all_failing_says_why() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VerdictStore::new(tmp.path().join(STATE_FILE));
        let ncnn = Arc::new(Fake {
            load_err: Some("vkCreateDevice failed".into()),
            ..Arc::into_inner(Fake::new("x")).unwrap()
        });
        let wg = Fake::new("wrong");
        let fakes = Fakes(vec![("ncnn", ncnn), ("webgpu", wg)]);
        let mut c = cands(tmp.path());
        let err = match pick(&fakes, &store, &c, Some(&golden("right"))) {
            Ok(_) => panic!("both runtimes are broken"),
            Err(e) => e,
        };
        assert!(
            err[0].starts_with("ncnn: could not load: vkCreateDevice"),
            "{err:?}"
        );
        assert!(err[1].starts_with("webgpu: self-test"), "{err:?}");
        // Missing files are a reason too, not a verdict.
        c[0].model_dir = None;
        c[0].key = "other".into();
        let err = match pick(&fakes, &store, &c, None) {
            Ok(_) => panic!(),
            Err(e) => e,
        };
        assert!(err[0].contains("files are not here"), "{err:?}");
        assert!(store.verdict("other").is_none());
    }

    #[test]
    fn pid_alive_tells_a_running_process_from_an_exited_one() {
        assert_eq!(pid_alive(std::process::id()), Some(true));
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", "exit"])
                .spawn()
        } else {
            std::process::Command::new("true").spawn()
        }
        .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        // Reaped (and its handle closed by `wait` on Windows): the pid is gone.
        drop(child);
        assert_eq!(pid_alive(pid), Some(false));
    }

    #[test]
    fn a_crash_while_testing_demotes_that_runtime_at_the_next_start() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(STATE_FILE);
        let c = cands(tmp.path());
        // A process selected ncnn and died before its first batch: the marker stays.
        {
            let store = VerdictStore::new(path.clone());
            let fakes = Fakes(vec![("ncnn", Fake::new("a")), ("webgpu", Fake::new("a"))]);
            let s = pick(&fakes, &store, &c, Some(&golden("a"))).unwrap();
            assert_eq!(s.runtime, "ncnn");
            assert!(store.reap_crashes().is_empty(), "its own markers are live");
        }
        // The next process (a new token; on Linux the old pid is checked).
        let store = VerdictStore::new(path);
        let crashed = store.reap_crashes();
        assert_eq!(crashed.len(), 1, "{crashed:?}");
        assert!(crashed[0].reason.contains("reading its first batch"));
        let fakes = Fakes(vec![("ncnn", Fake::new("a")), ("webgpu", Fake::new("a"))]);
        let s = pick(&fakes, &store, &c, Some(&golden("a"))).unwrap();
        assert_eq!(s.runtime, "webgpu");
        assert!(store.reap_crashes().is_empty(), "reaped once");
    }

    #[test]
    fn a_read_failure_mid_volume_moves_to_the_next_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(VerdictStore::new(tmp.path().join(STATE_FILE)));
        // ncnn passes its self-test (read 0) and its first page (read 1), then the
        // device is lost.
        let ncnn = Arc::new(Fake {
            read_err_after: Some(2),
            ..Arc::into_inner(Fake::new("a")).unwrap()
        });
        let wg = Fake::new("a");
        let loader: Arc<dyn Loader> =
            Arc::new(Fakes(vec![("ncnn", ncnn.clone()), ("webgpu", wg.clone())]));
        let c = cands(tmp.path());
        let info = crate::torch::recognizer_info(
            "hayai-nova",
            bunko_vlm::Precision::Fp32,
            bunko_vlm::Device::Gpu(0),
            16,
            512,
        );
        let r = VulkanRecognizer::start(
            loader,
            store.clone(),
            "hayai-nova",
            &dev(),
            c.clone(),
            LoadOptions::default(),
            Some(golden("a")),
            info,
            None,
        )
        .unwrap();
        let crop = Rgb::from_raw(2, 1, vec![0; 6]).unwrap();
        let lines = vec![CropSet::one(crop.clone()), CropSet::one(crop)];
        assert_eq!(r.runtime(), "ncnn");
        assert_eq!(r.read(&lines, None).unwrap(), vec!["a", "a"]);
        let file = std::fs::read_to_string(tmp.path().join(STATE_FILE)).unwrap();
        assert!(!file.contains("first batch"), "marker cleared: {file}");
        // The device is lost: the same read comes back from WebGPU.
        assert_eq!(r.read(&lines, None).unwrap(), vec!["a", "a"]);
        assert_eq!(r.runtime(), "webgpu");
        let v = store.verdict(&c[0].key).unwrap();
        assert!(
            !v.pass && v.reason.contains("VK_ERROR_DEVICE_LOST"),
            "{v:?}"
        );
        assert_eq!(wg.loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_three_standings_from_the_shipped_exceptions() {
        let st = |rt, e, v, beside| standing(EXCEPTIONS, rt, e, "linux", v, None, beside);
        // NVIDIA beside the CUDA pack: both runtimes fallback only, any engine.
        for rt in ["ncnn", "webgpu"] {
            for e in ["hayai-nova", "paddle-manga"] {
                assert!(
                    matches!(st(rt, e, NVIDIA, true), Standing::FallbackOnly(w) if w.contains("CUDA"))
                );
            }
        }
        // ... but an NVIDIA GPU nothing else drives (old driver, ocr.backend vulkan): candidates.
        assert_eq!(st("ncnn", "hayai-nova", NVIDIA, false), Standing::Candidate);
        // AMD: hayai-nova both runtimes; paddle-manga ncnn, WebGPU behind it.
        for beside in [true, false] {
            assert_eq!(st("ncnn", "hayai-nova", AMD, beside), Standing::Candidate);
            assert_eq!(st("webgpu", "hayai-nova", AMD, beside), Standing::Candidate);
            assert_eq!(st("ncnn", "paddle-manga", AMD, beside), Standing::Candidate);
            assert!(
                matches!(st("webgpu", "paddle-manga", AMD, beside), Standing::FallbackOnly(w) if w.contains("7 GB"))
            );
        }
        assert_eq!(
            st("webgpu", "paddle-manga", INTEL, false),
            Standing::Candidate
        );
        // Every reason carries its evidence.
        assert!(
            EXCEPTIONS
                .iter()
                .all(|e| e.why.contains("p/s") || e.why.contains("x of"))
        );
        // A test table: exclude and force.
        const T: &[Exception] = &[
            Exception {
                runtime: "webgpu",
                engine: None,
                os: Some("linux"),
                vendor: Some(AMD),
                arch: Some("gfx8"),
                beside_libtorch: None,
                action: Action::Exclude,
                why: "test",
            },
            Exception {
                runtime: "ncnn",
                engine: Some("paddle-manga"),
                os: None,
                vendor: None,
                arch: None,
                beside_libtorch: None,
                action: Action::Force,
                why: "test",
            },
        ];
        let t = |rt, e, os, a| standing(T, rt, e, os, AMD, a, false);
        assert_eq!(
            t("webgpu", "hayai-nova", "linux", Some("gfx803")),
            Standing::Excluded("test")
        );
        assert_eq!(
            t("webgpu", "hayai-nova", "linux", Some("gfx1030")),
            Standing::Candidate
        );
        assert_eq!(
            t("webgpu", "hayai-nova", "linux", None),
            Standing::Candidate,
            "arch unknown"
        );
        assert_eq!(
            t("webgpu", "hayai-nova", "windows", Some("gfx803")),
            Standing::Candidate
        );
        assert_eq!(
            t("ncnn", "paddle-manga", "linux", None),
            Standing::Forced("test")
        );
    }

    #[test]
    fn a_fallback_is_fetched_and_tested_only_when_the_ranked_runtimes_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VerdictStore::new(tmp.path().join(STATE_FILE));
        let ncnn = Arc::new(Fake {
            load_err: Some("VK_ERROR_OUT_OF_DEVICE_MEMORY".into()),
            ..Arc::into_inner(Fake::new("x")).unwrap()
        });
        let wg = Fake::new("a");
        let fakes = Fakes(vec![("ncnn", ncnn), ("webgpu", wg.clone())]);
        let mut c = cands(tmp.path());
        // webgpu is a fallback: its files are not here.
        c[1].model_dir = None;
        let fetched = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let (seen, dir) = (fetched.clone(), tmp.path().to_path_buf());
        let fetch = move |rt: &str| -> Result<PathBuf, String> {
            seen.lock().unwrap().push(rt.to_string());
            Ok(dir.clone())
        };
        // With a working ranked runtime the fallback is never fetched.
        let ok = Fakes(vec![("ncnn", Fake::new("a")), ("webgpu", wg.clone())]);
        let s = select(
            &ok,
            &store,
            &c,
            &[],
            "hayai-nova",
            &dev(),
            "gpu:0",
            &LoadOptions::default(),
            Some(&golden("a")),
            Some(&fetch),
        )
        .unwrap();
        assert_eq!(s.runtime, "ncnn");
        assert!(fetched.lock().unwrap().is_empty());
        // ncnn fails: webgpu is fetched, self-tested and used.
        let store = VerdictStore::new(tmp.path().join("other.json"));
        let s = select(
            &fakes,
            &store,
            &c,
            &[],
            "hayai-nova",
            &dev(),
            "gpu:0",
            &LoadOptions::default(),
            Some(&golden("a")),
            Some(&fetch),
        )
        .unwrap();
        assert_eq!(s.runtime, "webgpu");
        assert_eq!(*fetched.lock().unwrap(), ["webgpu"]);
        assert_eq!(wg.loads.load(Ordering::SeqCst), 1);
        assert!(
            store.verdict(&s.key).unwrap().pass,
            "keyed on the fetched files"
        );
    }

    #[test]
    fn kfd_topology_maps_pci_to_gfx() {
        let tmp = tempfile::tempdir().unwrap();
        for (n, props) in [
            (
                "0",
                "cpu_cores_count 16\ngfx_target_version 0\nlocation_id 0\n",
            ),
            ("1", "gfx_target_version 80003\nlocation_id 768\ndomain 0\n"),
            (
                "2",
                "gfx_target_version 120001\nlocation_id 2312\ndomain 1\n",
            ),
        ] {
            std::fs::create_dir_all(tmp.path().join(n)).unwrap();
            std::fs::write(tmp.path().join(n).join("properties"), props).unwrap();
        }
        let m = kfd_gfx_by_pci(tmp.path());
        assert_eq!(m.get("0000:03:00.0").map(String::as_str), Some("gfx803"));
        assert_eq!(m.get("0001:09:01.0").map(String::as_str), Some("gfx1201"));
        assert_eq!(m.len(), 2);
    }
}
