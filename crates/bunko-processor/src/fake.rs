//! A [`PagePipeline`] with no models, for tests and for exercising a server's
//! scheduler end to end: configurable load and page delays and failures, and a
//! minimal valid `.mokuro` sidecar (one blank page per image member of the archive).

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bunko_proto::{Catalog, Device, HostInfo, RowSpec};
use parking_lot::Mutex;
use serde_json::json;

use crate::pipeline::{
    CancelToken, MachineInfo, PagePipeline, PageProgress, PipelineReport, ReadyInfo, RunError,
    StageReport, VolumeMeta, VolumeOutcome, VolumeRunner,
};

/// How the fake behaves.
#[derive(Debug, Clone, Default)]
pub struct FakeConfig {
    /// Model "load" time.
    pub load_delay: Duration,
    /// Time per page.
    pub page_delay: Duration,
    /// `open` fails with this error.
    pub fail_open: Option<String>,
    /// Claims (or archive stems) that fail with `RunError::Volume("every page failed")`.
    pub fail_volumes: HashSet<String>,
    /// Claims (or archive stems) that end the session with `RunError::Fatal`.
    pub fatal_volumes: HashSet<String>,
    /// Volumes allowed to run at once (1 or 2).
    pub overlap: usize,
    /// Engines the catalog lists (default `["fake"]`).
    pub engines: Vec<String>,
    /// Adds a `gpu:0` device computing in these formats; the engine runs there unless
    /// the row pins it to the CPU.
    pub gpu_formats: Option<Vec<String>>,
    /// Time per page at a resolved precision (instead of `page_delay`), for the
    /// precision engines (`hayai-nova`, `paddle-manga`).
    pub precision_page_delay: BTreeMap<String, Duration>,
    /// The engine stage's width (`pools.stage_workers.engine`) divides the page time,
    /// up to this many workers; each worker past it slows a page by a quarter (0:
    /// widths change nothing).
    pub useful_width: u32,
    /// What `width_ceilings` says of the engine stage (None: nothing).
    pub width_ceiling: Option<u32>,
    /// Formats the card reports but has no model files for: forcing one fails to load.
    pub missing_formats: Vec<String>,
    /// The GPU's architecture (default `sm_89`, where bf16 is native).
    pub gpu_arch: Option<String>,
}

/// The fake engine. Cheap to clone (clones share their counters).
#[derive(Debug, Clone, Default)]
pub struct FakePipeline {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    config: FakeConfig,
    opened: AtomicU64,
    ran: Mutex<Vec<String>>,
}

impl FakePipeline {
    pub fn new(config: FakeConfig) -> FakePipeline {
        FakePipeline {
            inner: Arc::new(Inner {
                config,
                opened: AtomicU64::new(0),
                ran: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn config(&self) -> &FakeConfig {
        &self.inner.config
    }

    /// How many sessions were opened.
    pub fn opened(&self) -> u64 {
        self.inner.opened.load(Ordering::SeqCst)
    }

    /// Every claim `run_volume` was called for, in call order.
    pub fn ran(&self) -> Vec<String> {
        self.inner.ran.lock().clone()
    }
}

fn sleep_cancellable(duration: Duration, cancel: &CancelToken) -> bool {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        if cancel.is_cancelled() {
            return false;
        }
        std::thread::sleep((until - Instant::now()).min(Duration::from_millis(5)));
    }
    !cancel.is_cancelled()
}

impl PagePipeline for FakePipeline {
    fn describe(&self) -> MachineInfo {
        let config = &self.inner.config;
        let engines = if config.engines.is_empty() {
            vec!["fake".to_string()]
        } else {
            config.engines.clone()
        };
        MachineInfo {
            host: HostInfo {
                cpu: "fake cpu".into(),
                gpu: None,
                backend: "cpu".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                runner_build: format!("bunko-processor {} (fake)", env!("CARGO_PKG_VERSION")),
                os: Some(std::env::consts::OS.into()),
                cores: std::thread::available_parallelism()
                    .ok()
                    .map(|n| n.get() as u32),
            },
            catalog: Catalog {
                engines,
                detectors: vec!["ppocr-manga".into()],
                devices: {
                    let mut devices = vec![Device {
                        id: "cpu".into(),
                        label: "CPU".into(),
                        formats: vec!["fp32".into()],
                        provider: Some("cpu".into()),
                        arch: Some(std::env::consts::ARCH.into()),
                    }];
                    if let Some(formats) = &config.gpu_formats {
                        devices.push(Device {
                            id: "gpu:0".into(),
                            label: "GPU 0 \u{2014} Fake GPU".into(),
                            formats: formats.clone(),
                            provider: Some("cuda".into()),
                            arch: Some(config.gpu_arch.clone().unwrap_or_else(|| "sm_89".into())),
                        });
                    }
                    devices
                },
            },
        }
    }

    fn open(&self, spec: &RowSpec) -> Result<Box<dyn VolumeRunner>, String> {
        self.inner.opened.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.inner.config.load_delay);
        if let Some(error) = &self.inner.config.fail_open {
            return Err(error.clone());
        }
        let config = &self.inner.config;
        let pinned = spec.pools.stage_device.get("engine").map(String::as_str);
        let device = match (pinned, &config.gpu_formats) {
            (Some("cpu"), _) | (_, None) => "cpu".to_string(),
            _ => "gpu:0".to_string(),
        };
        // As the engines resolve: the shared bf16 rule over the device's formats.
        let formats = match (&config.gpu_formats, device.as_str()) {
            (Some(f), "gpu:0") => bunko_sched::precision::device_formats(
                &spec.precision,
                f,
                Some("cuda"),
                Some(config.gpu_arch.as_deref().unwrap_or("sm_89")),
            ),
            _ => bunko_sched::precision::device_formats(&spec.precision, &[], Some("cpu"), None),
        };
        let precision = if bunko_sched::precision::is_precision_engine(&spec.engine) {
            let r = bunko_sched::precision::resolve_mode(
                &spec.engine,
                &spec.precision,
                Some(&formats),
                spec.precision_pick.as_deref(),
                &spec.precision_why,
            );
            if !r.eligible {
                return Err(format!(
                    "precision not available here: {} is asked for {}, and this device cannot run it ({})",
                    spec.engine, spec.precision, r.why
                ));
            }
            let precision = r.precision.unwrap_or_else(|| "fp32".to_string());
            if config.missing_formats.contains(&precision) {
                return Err(format!(
                    "could not load {}: no {precision} package for this card",
                    spec.engine
                ));
            }
            precision
        } else {
            "fp32".to_string()
        };
        let width = spec
            .pools
            .stage_workers
            .get("engine")
            .copied()
            .unwrap_or(1)
            .max(1);
        Ok(Box::new(FakeRunner {
            pipeline: self.inner.clone(),
            engine: spec.engine.clone(),
            started: Instant::now(),
            items: AtomicU64::new(0),
            device,
            precision,
            width,
        }))
    }
}

struct FakeRunner {
    pipeline: Arc<Inner>,
    engine: String,
    started: Instant,
    items: AtomicU64,
    device: String,
    precision: String,
    width: u32,
}

impl FakeRunner {
    fn page_delay(&self) -> Duration {
        let config = &self.pipeline.config;
        let base = config
            .precision_page_delay
            .get(&self.precision)
            .copied()
            .unwrap_or(config.page_delay);
        let useful = self.width.min(config.useful_width).max(1);
        // Workers past the useful width contend: each costs a quarter of a page.
        let over = if config.useful_width > 0 {
            self.width.saturating_sub(config.useful_width)
        } else {
            0
        };
        base / useful * (4 + over) / 4
    }
}

/// Image members of the archive, sorted by name.
fn pages(archive: &Path) -> Result<Vec<String>, String> {
    let file =
        std::fs::File::open(archive).map_err(|e| format!("could not open the archive: {e}"))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))
        .map_err(|e| format!("BadZipFile: {e}"))?;
    let mut names = Vec::new();
    for i in 0..zip.len() {
        let mut member = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = member.name().to_string();
        let lower = name.to_ascii_lowercase();
        if member.is_dir()
            || ![".jpg", ".jpeg", ".png", ".webp", ".gif", ".bmp", ".avif"]
                .iter()
                .any(|e| lower.ends_with(e))
        {
            continue;
        }
        // Read it through: a damaged member fails like a page that will not decode.
        std::io::copy(&mut member, &mut std::io::sink()).map_err(|e| format!("{name}: {e}"))?;
        let _ = member.read(&mut [0u8; 0]);
        names.push(name);
    }
    names.sort();
    Ok(names)
}

impl VolumeRunner for FakeRunner {
    fn ready(&self) -> ReadyInfo {
        let mut weights = BTreeMap::new();
        weights.insert("fake/model".to_string(), "0000000".to_string());
        let mut stage_workers = BTreeMap::new();
        stage_workers.insert("engine".to_string(), self.width);
        let mut stage_device = BTreeMap::new();
        stage_device.insert("engine".to_string(), self.device.clone());
        ReadyInfo {
            weights,
            stage_workers,
            queue_capacity: BTreeMap::new(),
            stage_device,
            pipeline: format!("engine ({} x{})", self.device, self.width),
            precision: Some(self.precision.clone()),
        }
    }

    fn overlap(&self) -> usize {
        self.pipeline.config.overlap.max(1)
    }

    fn width_ceilings(&self) -> BTreeMap<String, u32> {
        self.pipeline
            .config
            .width_ceiling
            .map(|c| BTreeMap::from([("engine".to_string(), c)]))
            .unwrap_or_default()
    }

    fn run_volume(
        &self,
        archive: &Path,
        meta: &VolumeMeta,
        out: &Path,
        progress: &dyn Fn(PageProgress),
        cancel: &CancelToken,
    ) -> Result<VolumeOutcome, RunError> {
        let config = &self.pipeline.config;
        self.pipeline.ran.lock().push(meta.claim.clone());
        let names = pages(archive).map_err(RunError::Volume)?;
        progress(PageProgress::Started {
            pages: names.len() as u32,
        });
        if names.is_empty() {
            return Err(RunError::Volume(format!(
                "no page images found in {}.cbz",
                meta.stem
            )));
        }
        let total = names.len() as u32;
        let mut out_pages = Vec::new();
        for (i, name) in names.iter().enumerate() {
            if !sleep_cancellable(self.page_delay(), cancel) {
                return Err(RunError::Cancelled);
            }
            if config.fatal_volumes.contains(&meta.claim)
                || config.fatal_volumes.contains(&meta.stem)
            {
                return Err(RunError::Fatal(
                    "fake failed to load: the recognizer is gone".to_string(),
                ));
            }
            self.items.fetch_add(1, Ordering::SeqCst);
            out_pages.push(json!({"version": "0.2.5", "img_width": 0, "img_height": 0, "blocks": [], "img_path": name}));
            progress(PageProgress::Page {
                done: i as u32 + 1,
                total,
            });
        }
        if config.fail_volumes.contains(&meta.claim) || config.fail_volumes.contains(&meta.stem) {
            return Err(RunError::Volume("every page failed".to_string()));
        }
        let sidecar = json!({
            "version": "0.2.5",
            "title": meta.title,
            "title_uuid": meta.title_uuid,
            "volume": meta.volume,
            "volume_uuid": meta.volume_uuid,
            "ocr_engine": {"id": self.engine, "generator": "mokuro-bunko", "precision": self.precision},
            "pages": out_pages,
        });
        let tmp = out.with_file_name(format!(
            "{}.tmp",
            out.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("sidecar.mokuro")
        ));
        std::fs::write(
            &tmp,
            serde_json::to_vec(&sidecar).map_err(|e| e.to_string())?,
        )
        .map_err(|e| RunError::Volume(format!("could not write {}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, out)
            .map_err(|e| RunError::Volume(format!("could not write {}: {e}", out.display())))?;
        Ok(VolumeOutcome {
            pages: total,
            failed_pages: 0,
            stats: self.stats(),
        })
    }

    fn stats(&self) -> Option<PipelineReport> {
        let elapsed = self.started.elapsed().as_secs_f64();
        let items = self.items.load(Ordering::SeqCst);
        Some(PipelineReport {
            elapsed_seconds: elapsed,
            items,
            stages: vec![StageReport {
                key: "engine".into(),
                name: "fake".into(),
                device: self.device.clone(),
                workers: self.width,
                items,
                busy_seconds: elapsed * f64::from(self.width),
                utilisation: 1.0,
                ..Default::default()
            }],
            queues: vec![crate::pipeline::QueueReport {
                name: "engine->out".into(),
                capacity: 1,
                puts: items,
                gets: items,
                ..Default::default()
            }],
        })
    }
}
