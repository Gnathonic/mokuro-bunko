//! `models list|download|verify` (full build): the ONNX model store under
//! `<storage>/models/` (ARCHITECTURE §7), over `bunko-ocr`'s `ModelStore` and its
//! builtin manifest (PP-OCR pinned on Hugging Face, hayai-nova / paddle-manga from the
//! `models-v1` release). `MOKURO_MODELS_DIR` names a directory used instead of the store
//! for the files it holds; `MOKURO_MODELS_DOWNLOAD=0` forbids downloads.
//!
//! `<storage>` is the library server's, or the processor's with `--processor` (or on a
//! processor-only machine): see [`crate::ocr_target`].

use super::Ctx;
use crate::cli::ModelsCmd;
use crate::ocr_target::{self, OcrTarget, Role};
use crate::out::{CmdResult, Fail};
use bunko_engines::{Backend, models};
use bunko_ocr::models::ModelStore;
use serde::Serialize;
use std::path::PathBuf;

pub fn run(ctx: &Ctx, cmd: ModelsCmd) -> CmdResult {
    crate::logging::init_console(ctx.verbose);
    match cmd {
        ModelsCmd::List { target } => {
            let target = ocr_target::resolve(ctx, target.processor)?;
            println!("{}", target.describe());
            println!("Models directory: {}", target.models_dir().display());
            list(&target)
        }
        ModelsCmd::Download { engine, target } => {
            let target = ocr_target::resolve(ctx, target.processor)?;
            println!("{}", target.describe());
            download(&target, engine.as_deref())
        }
        ModelsCmd::Verify { target } => {
            let target = ocr_target::resolve(ctx, target.processor)?;
            println!("{}", target.describe());
            verify(&target.engine_config(Backend::Auto).store())
        }
    }
}

fn list(target: &OcrTarget) -> CmdResult {
    let store = target.engine_config(Backend::Auto).store();
    if let Some(dir) = &store.options().override_dir {
        println!("Override directory (MOKURO_MODELS_DIR): {}", dir.display());
    }
    let plan = plan(target, &wanted_rows(target, None), true);
    for line in plan.table_lines() {
        println!("{line}");
    }
    println!();
    for e in &plan.engines {
        println!("{}{}:", e.engine, if e.used { "" } else { " (not in use)" });
        for p in &e.packages {
            println!("  package: {}", p.describe());
        }
        if let Some(why) = &e.package_error {
            println!("  package: {why}");
        }
        for f in &e.files {
            let state = match (&f.path, f.present) {
                (Some(path), true) => format!("present  {}", path.display()),
                _ => "missing".to_string(),
            };
            println!("  {:<52} {:>10}  {}", f.id, size(f.size), state);
        }
        for f in &e.extra {
            println!(
                "  {:<52} {:>10}  on disk, not needed here",
                f.id,
                size(f.disk)
            );
        }
    }
    if plan.total.to_download > 0 {
        println!(
            "Missing: {} (downloaded on first use{}; or run 'mokuro-bunko models download')",
            size(plan.total.to_download),
            if plan.downloads {
                ""
            } else {
                " — but downloads are off here"
            }
        );
    }
    Ok(())
}

/// Bytes in binary units, as `du -h` and the admin panel show them ("334.2 MB").
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// One manifest file of a [`ModelsPlan`].
#[derive(Debug, Clone, Serialize)]
pub struct PlanFile {
    pub id: String,
    /// The download's size (the manifest's).
    pub size: u64,
    /// Bytes its local copy takes in the models folder (0 when it is not there).
    pub disk: u64,
    pub present: bool,
    /// The local copy is outside the models folder (`MOKURO_MODELS_DIR`,
    /// `MOKURO_TORCH_MODELS_DIR`): it counts as present, not in the folder's size.
    pub elsewhere: bool,
    #[serde(skip)]
    pub path: Option<PathBuf>,
}

impl PlanFile {
    fn new(store: &ModelStore, id: &str) -> Option<PlanFile> {
        Self::with_fallback(store, id, None)
    }

    /// `fallback`: where a package found by its directories (no unpack stamp: copied
    /// in by hand, or the override directory) keeps this file.
    fn with_fallback(store: &ModelStore, id: &str, fallback: Option<PathBuf>) -> Option<PlanFile> {
        let f = store.manifest().get(id)?;
        let local = store.local_size(id).or_else(|| {
            let p = fallback?;
            let n = if p.is_dir() {
                bunko_ocr::models::tree_size(&p)
            } else {
                std::fs::metadata(&p).ok()?.len()
            };
            Some((p, n))
        });
        let elsewhere = local
            .as_ref()
            .is_some_and(|(p, _)| !p.starts_with(&store.options().root));
        Some(PlanFile {
            id: f.id.clone(),
            size: f.size,
            disk: match &local {
                Some((_, n)) if !elsewhere => *n,
                _ => 0,
            },
            present: local.is_some(),
            elsewhere,
            path: local.map(|(p, _)| p),
        })
    }
}

/// The compiled package one row of an engine runs here.
#[derive(Debug, Clone, Serialize)]
pub struct PlanPackage {
    /// The rows' precision mode (`auto-accuracy`, `bf16`, ...).
    pub mode: String,
    pub precision: String,
    pub device: String,
    pub label: String,
    /// The package target counted (on disk, else the one a download fetches).
    pub target: Option<String>,
    pub present: bool,
    /// Why it runs on the CPU although there is a GPU.
    pub fallback: Option<String>,
}

impl PlanPackage {
    /// "bf16 · linux-cuda-sm_80 on gpu:0 (NVIDIA A100)", and whether it is here.
    pub fn describe(&self) -> String {
        format!(
            "{}{}",
            self.describe_target(),
            if self.present {
                ""
            } else {
                ", not downloaded yet"
            }
        )
    }

    /// "bf16 · linux-cuda-sm_80 on gpu:0 (NVIDIA A100)".
    pub fn describe_target(&self) -> String {
        format!(
            "{} · {} on {} ({}){}",
            self.precision,
            self.target
                .as_deref()
                .unwrap_or("no package in the release"),
            self.device,
            self.label,
            match &self.fallback {
                Some(why) => format!(" (the CPU: {why})"),
                None => String::new(),
            }
        )
    }
}

/// One engine's row: the files it needs on this machine (the models-v1 files and the
/// compiled packages for each device and precision it runs at), present or not.
#[derive(Debug, Clone, Serialize)]
pub struct EnginePlan {
    pub engine: &'static str,
    /// An enabled generation runs it (PP-OCR: any does, every engine reads lines with it).
    pub used: bool,
    pub packages: Vec<PlanPackage>,
    /// Why no package could be planned (no OCR backend loaded, no device runs it).
    pub package_error: Option<String>,
    /// What the engine needs here; for an engine not in use, what enabling it needs.
    pub files: Vec<PlanFile>,
    /// This engine's files in the folder that no row needs (another precision or device).
    pub extra: Vec<PlanFile>,
    pub needed: usize,
    pub present: usize,
    /// Bytes of `files` on disk.
    pub disk: u64,
    /// Bytes of `files` still to download (for an engine not in use: what enabling it
    /// downloads). Files another engine already has count as present.
    pub missing: u64,
    pub extra_disk: u64,
    /// Engines that need some of the same files (counted once in the total).
    pub shares_with: Vec<&'static str>,
    /// The row in words (the packages, what is missing, what is shared).
    pub details: Vec<String>,
}

impl EnginePlan {
    /// The row's details in words: `models list` prints them, the admin panel shows them.
    fn describe(&self) -> Vec<String> {
        let mut what: Vec<String> = self
            .packages
            .iter()
            .map(|p| {
                if self.used {
                    p.describe()
                } else {
                    format!("would run as {}", p.describe_target())
                }
            })
            .collect();
        if let Some(why) = &self.package_error {
            what.push(why.clone());
        }
        if self.engine == models::PPOCR {
            what.push("line detection and reading, for every engine".into());
        }
        if self.missing > 0 {
            what.push(self.missing_text());
        }
        if self.extra_disk > 0 {
            what.push(format!(
                "{} more on disk that nothing here needs",
                size(self.extra_disk)
            ));
        }
        if !self.shares_with.is_empty() {
            what.push(format!(
                "shares files with {} (counted once)",
                self.shares_with.join(", ")
            ));
        }
        what
    }

    /// What is still to download, in words.
    pub fn missing_text(&self) -> String {
        let unknown = if self.package_error.is_some() && self.engine != models::PPOCR {
            " besides its compiled package"
        } else {
            ""
        };
        if self.used {
            format!("{} to download{unknown}", size(self.missing))
        } else {
            format!(
                "enabling it would download about {}{unknown}",
                size(self.missing)
            )
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PlanTotals {
    /// Everything in the models folder (`du --apparent-size`).
    pub disk: u64,
    /// Distinct files the engines in use need, on disk.
    pub in_use: u64,
    /// Distinct files of engines not in use (that no engine in use needs), on disk.
    pub not_in_use: u64,
    /// Engine files on disk no row needs.
    pub not_needed: u64,
    /// The rest: checksum stamps, partial downloads, files no manifest names.
    pub other: u64,
    /// Distinct files the engines in use still need.
    pub to_download: u64,
}

/// A file more than one engine needs.
#[derive(Debug, Clone, Serialize)]
pub struct SharedFile {
    pub id: String,
    pub engines: Vec<&'static str>,
}

/// The models folder against what this machine needs: one plan for `models list`,
/// `models download` and the admin panel's "Engines and models".
#[derive(Debug, Clone, Serialize)]
pub struct ModelsPlan {
    pub dir: PathBuf,
    pub downloads: bool,
    pub engines: Vec<EnginePlan>,
    pub total: PlanTotals,
    pub shared: Vec<SharedFile>,
}

/// Plan `rows` (as [`wanted_rows`]) on this machine. `open_backend`: the compiled
/// packages are planned on the libtorch backend (opened if needed); false plans them
/// only when this process already has it open (the server's admin page does not load
/// libtorch on its own), else says why not.
pub fn plan(target: &OcrTarget, rows: &[(String, String)], open_backend: bool) -> ModelsPlan {
    let config = target.engine_config(Backend::parse(&target.backend_preference()));
    let store = config.store();
    let pipeline = (open_backend || bunko_engines::torch::backend_if_open().is_some())
        .then(|| bunko_engines::EnginePipeline::new(config.clone()));
    let backend = match &pipeline {
        Some(p) => p.torch().map(|_| ()).map_err(|e| {
            format!("the OCR backend is not installed here ({e}): run 'mokuro-bunko install-ocr'")
        }),
        None => Err("not known until the OCR backend has loaded".to_string()),
    };
    let gpu = bunko_ocr::runtime::ep_compiled().len() > 1;
    let mut engines = Vec::new();
    for engine in models::ENGINES {
        let used = if engine == models::PPOCR {
            !rows.is_empty()
        } else {
            rows.iter().any(|(e, _)| e == engine)
        };
        let mut ids: Vec<String> = Vec::new();
        let mut fallbacks: Vec<(String, PathBuf)> = Vec::new();
        let mut add = |id: &str| {
            if !ids.iter().any(|i| i == id) {
                ids.push(id.to_string());
            }
        };
        let ppocr = models::ppocr_ids();
        for id in models::download_plan(Some(engine), gpu) {
            if engine == models::PPOCR || !ppocr.contains(&id) {
                add(id);
            }
        }
        let mut packages = Vec::new();
        let mut package_error = None;
        if engine != models::PPOCR {
            let mut modes: Vec<&str> = rows
                .iter()
                .filter(|(e, _)| e == engine)
                .map(|(_, m)| m.as_str())
                .collect();
            if modes.is_empty() {
                modes.push("auto-accuracy");
            }
            modes.dedup();
            match (&pipeline, &backend) {
                (Some(p), Ok(())) => {
                    for mode in modes {
                        match p.package_plan(engine, mode) {
                            Ok(pp) => {
                                for id in pp.package_ids.iter().map(String::as_str) {
                                    add(id);
                                    if let Some(dir) = &pp.package {
                                        let name = id.rsplit('/').next().unwrap_or(id);
                                        let at = bunko_ocr::models::torch_graph_path(dir, name)
                                            .or_else(|| dir.parent().map(|p| p.join(name)))
                                            .filter(|p| p.exists());
                                        if let Some(at) = at {
                                            fallbacks.push((id.to_string(), at));
                                        }
                                    }
                                }
                                for id in &pp.host_ids {
                                    add(id);
                                }
                                let pkg = PlanPackage {
                                    mode: mode.to_string(),
                                    precision: pp.need.precision.as_str().to_string(),
                                    device: pp.need.device.clone(),
                                    label: pp.need.label.clone(),
                                    target: pp.target.clone(),
                                    present: pp.package.is_some(),
                                    fallback: pp.need.fallback.clone(),
                                };
                                if !packages.iter().any(|q: &PlanPackage| {
                                    q.precision == pkg.precision
                                        && q.device == pkg.device
                                        && q.target == pkg.target
                                }) {
                                    packages.push(pkg);
                                }
                            }
                            Err(e) => package_error = Some(e),
                        }
                    }
                }
                (_, Err(e)) => package_error = Some(format!("compiled packages: {e}")),
                (None, Ok(())) => {}
            }
        }
        let files = ids
            .iter()
            .filter_map(|id| {
                let fb = fallbacks
                    .iter()
                    .find(|(f, _)| f == id)
                    .map(|(_, p)| p.clone());
                PlanFile::with_fallback(&store, id, fb)
            })
            .collect();
        engines.push(EnginePlan {
            engine,
            used,
            packages,
            package_error,
            files,
            extra: Vec::new(),
            needed: 0,
            present: 0,
            disk: 0,
            missing: 0,
            extra_disk: 0,
            shares_with: Vec::new(),
            details: Vec::new(),
        });
    }
    // Each engine's files in the folder that no row needs.
    let planned: std::collections::HashSet<String> = engines
        .iter()
        .flat_map(|e| e.files.iter().map(|f| f.id.clone()))
        .collect();
    for e in &mut engines {
        e.extra = store
            .manifest()
            .engine_files(e.engine)
            .into_iter()
            .filter(|f| !planned.contains(&f.id))
            .filter_map(|f| PlanFile::new(&store, &f.id))
            .filter(|f| f.present && !f.elsewhere)
            .collect();
    }
    let mut p = ModelsPlan {
        dir: store.options().root.clone(),
        downloads: store.can_download(),
        engines,
        total: PlanTotals::default(),
        shared: Vec::new(),
    };
    p.total.disk = bunko_ocr::models::tree_size(&p.dir);
    p.tally();
    p
}

impl ModelsPlan {
    /// The per-engine counts, the shared files and the totals, each distinct file
    /// counted once.
    pub fn tally(&mut self) {
        let mut owners: Vec<(String, Vec<&'static str>)> = Vec::new();
        for e in &self.engines {
            for f in &e.files {
                match owners.iter_mut().find(|(id, _)| *id == f.id) {
                    Some((_, v)) => v.push(e.engine),
                    None => owners.push((f.id.clone(), vec![e.engine])),
                }
            }
        }
        self.shared = owners
            .iter()
            .filter(|(_, v)| v.len() > 1)
            .map(|(id, v)| SharedFile {
                id: id.clone(),
                engines: v.clone(),
            })
            .collect();
        for e in &mut self.engines {
            e.needed = e.files.len();
            e.present = e.files.iter().filter(|f| f.present).count();
            e.disk = e.files.iter().map(|f| f.disk).sum();
            e.missing = e.files.iter().filter(|f| !f.present).map(|f| f.size).sum();
            e.extra_disk = e.extra.iter().map(|f| f.disk).sum();
            let mut with: Vec<&'static str> = self
                .shared
                .iter()
                .filter(|s| s.engines.contains(&e.engine))
                .flat_map(|s| s.engines.iter().copied())
                .filter(|o| *o != e.engine)
                .collect();
            with.sort_unstable();
            with.dedup();
            e.shares_with = with;
            e.details = e.describe();
        }
        let mut seen: std::collections::HashSet<&str> = Default::default();
        let (mut in_use, mut not_in_use, mut not_needed, mut to_download) = (0, 0, 0, 0);
        // Engines in use first: a file both need counts as in use.
        let mut order: Vec<&EnginePlan> = self.engines.iter().collect();
        order.sort_by_key(|e| !e.used);
        for e in &order {
            for f in &e.files {
                if !seen.insert(f.id.as_str()) {
                    continue;
                }
                if e.used {
                    in_use += f.disk;
                    if !f.present {
                        to_download += f.size;
                    }
                } else {
                    not_in_use += f.disk;
                }
            }
        }
        for e in &order {
            for f in &e.extra {
                if seen.insert(f.id.as_str()) {
                    not_needed += f.disk;
                }
            }
        }
        self.total.in_use = in_use;
        self.total.not_in_use = not_in_use;
        self.total.not_needed = not_needed;
        self.total.to_download = to_download;
        self.total.other = self
            .total
            .disk
            .saturating_sub(in_use + not_in_use + not_needed);
    }

    /// The folder's size and what it holds, in words: "333.0 MB in <dir>: 333.0 MB
    /// for the engines in use, ...". Every file counted once; the parts add up to
    /// the folder's size.
    pub fn total_text(&self) -> String {
        let t = &self.total;
        let mut parts = vec![format!("{} for the engines in use", size(t.in_use))];
        if t.not_in_use > 0 {
            parts.push(format!("{} for engines not in use", size(t.not_in_use)));
        }
        if t.not_needed > 0 {
            parts.push(format!("{} no engine here needs", size(t.not_needed)));
        }
        parts.push(format!(
            "{} other (checksum stamps, partial downloads)",
            size(t.other)
        ));
        let mut s = format!(
            "{} in {}: {}",
            size(t.disk),
            self.dir.display(),
            parts.join(", ")
        );
        if t.to_download > 0 {
            s.push_str(&format!("; {} still to download", size(t.to_download)));
        }
        s
    }

    /// The table `models list` prints (the admin panel shows the same rows).
    pub fn table_lines(&self) -> Vec<String> {
        let mut out = vec![format!(
            "{:<14} {:<5} {:<9} {:>10}  {}",
            "Engine", "Used", "Files", "On disk", "Runs as"
        )];
        for e in &self.engines {
            out.push(format!(
                "{:<14} {:<5} {:<9} {:>10}  {}",
                e.engine,
                if e.used { "yes" } else { "-" },
                format!("{} of {}", e.present, e.needed),
                size(e.disk),
                e.details.join("; ")
            ));
        }
        out.push(format!("Total: {}", self.total_text()));
        if !self.shared.is_empty() {
            out.push(format!(
                "Shared: {} (each counted once)",
                self.shared
                    .iter()
                    .map(|f| format!("{} ({})", f.id, f.engines.join(", ")))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        out
    }
}

/// What to fetch for, as `(engine, precision mode)` rows: `--engine` (its default row),
/// else the library's enabled generations (each at its precision mode), else (a
/// processor, which runs whatever its library asks for) every engine's default row.
pub fn wanted_rows(target: &OcrTarget, engine: Option<&str>) -> Vec<(String, String)> {
    match (engine, target.library.as_ref()) {
        (Some(e), _) => vec![(e.to_string(), "auto-accuracy".to_string())],
        (None, Some(cfg)) if target.role == Role::Library => cfg
            .ocr
            .generations
            .iter()
            .filter(|g| g.runnable())
            .map(|g| (g.engine.clone(), g.precision.clone()))
            .collect(),
        (None, _) => models::ENGINES
            .iter()
            .map(|e| (e.to_string(), "auto-accuracy".to_string()))
            .collect(),
    }
}

/// The device-independent files those rows need: PP-OCR (every engine reads lines with
/// it) and the host files of the recognizer engines they use (fp32, and fp16 with `gpu`).
/// `models download` fetches these and `doctor` checks them, so an engine no enabled
/// generation runs is neither fetched nor missed.
pub fn planned_ids(rows: &[(String, String)], gpu: bool) -> Vec<&'static str> {
    let mut ids: Vec<&'static str> = Vec::new();
    for e in models::ENGINES {
        let wanted = e == models::PPOCR || rows.iter().any(|(r, _)| r == e);
        if !wanted {
            continue;
        }
        for id in models::download_plan(Some(e), gpu) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

/// Also used by `install-ocr`.
pub fn download(target: &OcrTarget, engine: Option<&str>) -> CmdResult {
    if let Some(e) = engine.filter(|e| !models::ENGINES.contains(e)) {
        return Err(Fail::msg(format!(
            "Unknown engine '{e}' (expected one of: {})",
            models::ENGINES.join(", ")
        )));
    }
    let store = target.engine_config(Backend::Auto).store();
    bunko_engines::runtime::init();
    let rows = wanted_rows(target, engine);
    let recognizer_rows: Vec<(&str, &str)> = rows
        .iter()
        .filter(|(e, _)| e == models::HAYAI || e == models::PADDLE)
        .map(|(e, m)| (e.as_str(), m.as_str()))
        .collect();
    let gpu = bunko_ocr::runtime::ep_compiled().len() > 1;
    let ids = planned_ids(&rows, gpu);
    // The same plan as `models list` and the admin panel: host files and packages.
    let total = plan(target, &rows, true).total.to_download;
    println!(
        "Fetching {} into {} ({} to download)",
        engine.unwrap_or("the enabled generations"),
        store.options().root.display(),
        size(total)
    );
    // Every failure is reported at the end; one file failing does not stop the others.
    let mut failed: Vec<String> = Vec::new();
    // For the background installer: the files and packages done, of how many.
    let items = ids.len() + recognizer_rows.len();
    let mut done_items = 0usize;
    let mut step = |label: &str| {
        done_items += 1;
        super::install_ocr::event(serde_json::json!({
            "stage": "models", "label": label, "done": done_items, "total": items, "unit": "files",
        }));
    };
    for id in ids {
        let result = store.ensure(id);
        step(id);
        match result {
            Ok(r) => println!(
                "  {id:<34} {}{}",
                if r.verified {
                    "ok"
                } else {
                    "present (not the manifest's bytes)"
                },
                if store.options().override_dir.is_some() {
                    format!("  {}", r.path.display())
                } else {
                    String::new()
                }
            ),
            Err(e) => {
                println!("  {id:<34} FAILED: {e}");
                failed.push(id.to_string());
            }
        }
    }
    if !recognizer_rows.is_empty() {
        // The compiled libtorch packages depend on this machine's devices: the backend
        // pack (install-ocr) says which, limited to the devices `ocr.backend` allows
        // (`cpu` beside a GPU pack fetches the CPU packages).
        let pipeline = bunko_engines::EnginePipeline::new(
            target.engine_config(Backend::parse(&target.backend_preference())),
        );
        match pipeline.torch() {
            Err(e) => {
                println!(
                    "  compiled packages: FAILED: the OCR backend is not installed ({e}); run 'mokuro-bunko install-ocr'"
                );
                failed.push("compiled packages (no OCR backend)".into());
            }
            Ok(_) => {
                for (e, r) in pipeline.prefetch_rows(&recognizer_rows) {
                    step(e);
                    match r {
                        Ok(p) => {
                            println!(
                                "  {e:<14} {} on {} ({}): {} [{}]",
                                p.need.precision,
                                p.need.device,
                                p.need.label,
                                p.package.display(),
                                p.target
                            );
                            if let Some(why) = &p.need.fallback {
                                println!("  {e:<14} note: running on the CPU: {why}");
                            }
                        }
                        Err(err) => {
                            println!("  {e:<14} FAILED: {err}");
                            failed.push(format!("{e} compiled package"));
                        }
                    }
                }
            }
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Fail::msg(format!(
            "{} item(s) could not be fetched: {}",
            failed.len(),
            failed.join(", ")
        )))
    }
}

fn verify(store: &ModelStore) -> CmdResult {
    let (mut ok, mut bad, mut absent) = (0, 0, 0);
    for file in &store.manifest().files {
        match store.verify(&file.id) {
            Ok(Some((path, true))) => {
                ok += 1;
                println!("  OK        {:<34} {}", file.id, path.display());
            }
            Ok(Some((path, false))) => {
                bad += 1;
                println!("  MISMATCH  {:<34} {}", file.id, path.display());
            }
            Ok(None) => absent += 1,
            Err(e) => {
                bad += 1;
                println!("  ERROR     {:<34} {e}", file.id);
            }
        }
    }
    println!("{ok} verified, {bad} bad, {absent} not downloaded");
    if bad > 0 {
        Err(Fail::msg(
            "some model files do not match the manifest; delete them and run 'mokuro-bunko models download'",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str, size: u64, present: bool) -> PlanFile {
        PlanFile {
            id: id.into(),
            size,
            disk: if present { size } else { 0 },
            present,
            elsewhere: false,
            path: None,
        }
    }

    fn engine(engine: &'static str, used: bool, files: Vec<PlanFile>) -> EnginePlan {
        EnginePlan {
            engine,
            used,
            packages: Vec::new(),
            package_error: None,
            files,
            extra: Vec::new(),
            needed: 0,
            present: 0,
            disk: 0,
            missing: 0,
            extra_disk: 0,
            shares_with: Vec::new(),
            details: Vec::new(),
        }
    }

    /// A file two engines need is counted once in the total (as in use when one of
    /// them is), each row says whom it shares with, and the parts add up to the folder.
    #[test]
    fn shared_files_count_once() {
        let mut p = ModelsPlan {
            dir: PathBuf::from("/m"),
            downloads: true,
            engines: vec![
                engine(
                    models::HAYAI,
                    false,
                    vec![file("w", 100, true), file("h", 7, true)],
                ),
                engine(
                    models::PADDLE,
                    true,
                    vec![
                        file("a", 10, true),
                        file("w", 100, true),
                        file("b", 50, false),
                    ],
                ),
                engine(models::PPOCR, true, vec![file("p", 3, true)]),
            ],
            total: PlanTotals {
                disk: 100 + 7 + 10 + 3 + 5,
                ..Default::default()
            },
            shared: Vec::new(),
        };
        p.engines[2].extra = vec![file("old", 0, true)];
        p.tally();
        let t = &p.total;
        assert_eq!(t.in_use, 10 + 100 + 3, "w counts once, as in use");
        assert_eq!(t.not_in_use, 7);
        assert_eq!(t.other, 5);
        assert_eq!(t.to_download, 50);
        assert_eq!(t.in_use + t.not_in_use + t.not_needed + t.other, t.disk);
        assert_eq!(p.shared.len(), 1);
        assert_eq!(p.shared[0].id, "w");
        assert_eq!(p.shared[0].engines, vec![models::HAYAI, models::PADDLE]);
        assert_eq!(p.engines[0].shares_with, vec![models::PADDLE]);
        assert_eq!(p.engines[1].shares_with, vec![models::HAYAI]);
        assert!(
            p.engines[1]
                .details
                .iter()
                .any(|d| d.contains("hayai-nova (counted once)"))
        );
        // Each row still counts what it needs.
        assert_eq!((p.engines[1].present, p.engines[1].needed), (2, 3));
        assert_eq!(p.engines[1].disk, 110);
        assert_eq!(p.engines[1].missing, 50);
        assert_eq!(
            p.engines[0].details,
            vec!["shares files with paddle-manga (counted once)"]
        );
        let lines = p.table_lines();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("Shared: w (hayai-nova, paddle-manga)"))
        );
    }

    /// Without the OCR backend the rows still count the models-v1 files, say why the
    /// packages are not counted, and the total is still the folder (stamps as other).
    #[test]
    fn without_a_backend_the_total_is_still_the_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = bunko_core::Config::default();
        // hayai-nova, the default row.
        config.ocr.generations = vec![bunko_core::generations::default_generation("g-1")];
        assert_eq!(config.ocr.generations[0].engine, models::HAYAI);
        let target = OcrTarget {
            role: Role::Library,
            storage: tmp.path().to_path_buf(),
            processor_config: None,
            library: Some(config),
            reason: String::new(),
        };
        let store = target.engine_config(Backend::Auto).store();
        let mut want = 0;
        for id in models::ppocr_ids() {
            let f = store.manifest().get(id).unwrap();
            let p = store.store_path(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::File::create(&p).unwrap().set_len(f.size).unwrap();
            std::fs::write(p.with_extension("verified"), "ok").unwrap();
            want += f.size;
        }
        let p = plan(&target, &wanted_rows(&target, None), false);
        assert_eq!(p.total.in_use, want);
        assert_eq!(p.total.other, 3 * 2);
        assert_eq!(p.total.disk, want + 6);
        let hayai = p
            .engines
            .iter()
            .find(|e| e.engine == models::HAYAI)
            .unwrap();
        assert!(hayai.used);
        assert!(
            hayai
                .package_error
                .as_deref()
                .is_some_and(|e| e.contains("not known until the OCR backend has loaded")),
            "{hayai:?}"
        );
        assert!(
            hayai
                .details
                .iter()
                .any(|d| d.ends_with("to download besides its compiled package")),
            "{:?}",
            hayai.details
        );
    }
}
