//! `install-ocr`: install the OCR backend pack for this machine (TORCH-BACKEND.md,
//! PACKAGING.md §8) and fetch the models.
//!
//! 1. Pick the pack variant: `cu130` (NVIDIA, driver ≥ 580), `rocm7.1` (supported AMD
//!    GPU, Linux) or `cpu` ([`crate::hwdetect`]); `--variant` overrides.
//! 2. Find it in the signed `release.json` of **this** version (`backends[target]`;
//!    a pack belongs to exactly one release: this binary opens no other release's),
//!    download the archive (resuming), check its sha256, unpack it into
//!    `<storage>/backends/.staging-*`, check every file against `pack.json`.
//! 3. Fetch the libraries `pack.json` lists as external (NVIDIA's CUDA wheels on PyPI,
//!    pinned by sha256) and take the needed files out of them.
//! 4. Swap the verified pack into `<storage>/backends/torch-<variant>-<torch>/`.
//! 5. `models download` (the configured engines' models).
//!
//! Whose storage: the library server's, or with `--processor` (automatic on a machine
//! with a processor.yaml and no library configuration) the processor's
//! ([`crate::ocr_target`]). A processor also uses a pack found in the library's storage.
//!
//! `--from <dir>` installs from local files instead (air-gapped hosts, tests): the pack
//! archive (or its parts), optionally `release.json` + `.sig` (then checked like a
//! download) and the wheels. The 0.5.2 options `--backend cuda|rocm|cpu|auto` map to
//! `--variant`; `--engines`/`--detector` are ignored.

use super::Ctx;
use crate::cli::InstallOcrArgs;
use crate::out::CmdResult;

#[cfg(not(feature = "ocr"))]
pub fn run(ctx: &Ctx, args: InstallOcrArgs) -> CmdResult {
    let _ = (ctx, args);
    println!(
        "This is the lite build: OCR runs on remote processors ('mokuro-bunko processor serve' on a full build)."
    );
    Ok(())
}

#[cfg(feature = "ocr")]
pub use full::*;

#[cfg(feature = "ocr")]
mod full {
    use super::*;
    use crate::hwdetect;
    use crate::out::Fail;
    use bunko_update::backend::{self as pack, BackendArtifact, PackManifest};
    use std::path::{Path, PathBuf};

    /// `$MOKURO_BACKEND_MANIFEST`: release.json URL or path to use instead of this
    /// version's GitHub release (mirrors, tests). Its `.sig` must sit next to it.
    const MANIFEST_ENV: &str = "MOKURO_BACKEND_MANIFEST";

    pub fn run(ctx: &Ctx, args: InstallOcrArgs) -> CmdResult {
        crate::logging::init_console(ctx.verbose);
        if args.engines.is_some() || args.detector.is_some() {
            println!("Note: --engines/--detector are 0.5 options and are ignored.");
        }
        let ocr = crate::ocr_target::resolve(ctx, args.processor)?;
        println!("{}", ocr.describe());
        let root = args.dir.clone().unwrap_or_else(|| ocr.backends_dir());
        // Where an installed pack counts: `--dir` alone, else every directory this
        // role's OCR runtime searches.
        let search = match &args.dir {
            Some(d) => vec![d.clone()],
            None => ocr.backends_dirs(),
        };
        let target = bunko_update::TARGET;
        let hw = hwdetect::detect();
        let auto = hwdetect::choose(&hw, target);
        let requested = match (&args.variant, args.backend.as_deref()) {
            (Some(v), _) => v.clone(),
            (None, Some(b)) => legacy_backend(b)?.to_string(),
            (None, None) => "auto".into(),
        };
        let variant = if requested == "auto" {
            auto.variant.to_string()
        } else {
            requested.clone()
        };
        if args.list {
            print_status(&hw, &auto, &root, &search);
            return Ok(());
        }
        if args.probe {
            return probe(&ocr, args.dir.as_deref());
        }
        println!(
            "OCR backend: {variant}{}",
            if requested == "auto" {
                format!(" ({})", auto.reason)
            } else {
                String::new()
            }
        );
        if requested == "auto"
            && let Some(h) = &auto.hint
        {
            println!("  {h}");
        }

        // No `--from`: the offline OCR files bundled with this copy (the macOS app), if
        // any, else the network.
        let from: Option<PathBuf> = match &args.from {
            Some(f) => Some(f.clone()),
            None => bundled_offline_dir().inspect(|d| {
                println!(
                    "Installing from the OCR files bundled with this app: {}",
                    d.display()
                )
            }),
        };

        // Already there (installed, or baked into a Docker image)?
        let pack_dir = if !args.force
            && let Some((dir, m)) = find_installed(&search, &variant)
        {
            println!(
                "Already installed: {} ({}, mokuro-bunko {})",
                dir.display(),
                m.name,
                m.bunko_version
            );
            dir
        } else {
            let rt = crate::out::runtime()?;
            let source = ReleaseSource::for_target(&ocr, bunko_core::VERSION);
            let dir = rt
                .block_on(install(&root, &variant, target, from.as_deref(), &source))
                .map_err(|f| Fail::msg(f.to_string()))?;
            println!("Installed {}", dir.display());
            dir
        };
        // What this machine's GPU called for when the pack went in: a later change of
        // GPU is then told apart from a deliberate choice (doctor, GUI.md "Automatic
        // updates").
        HardwareRecord {
            auto_variant: auto.variant.to_string(),
            reason: auto.reason.clone(),
        }
        .write(&root);
        if let Some((_, m)) = find_installed(&search, &variant) {
            let missing = missing_system_libs(&m.requires.system_libs);
            if !missing.is_empty() {
                println!(
                    "Warning: this {} pack needs these libraries from the system, which were not found: {}",
                    m.variant,
                    missing.join(", ")
                );
                println!("  {}", system_lib_hint(&missing));
            }
        }

        if args.no_models {
            return Ok(());
        }
        println!("Fetching the OCR models for this machine (mokuro-bunko models download)...");
        // Prefetch for this pack: left alone, the loader would pick by its own order
        // (a GPU pack before cpu, only in the configured backends dir), so
        // `--variant cpu` beside an installed cu130 pack, or `--dir`, would fetch
        // the compiled packages of another pack.
        // SAFETY: set before this process opens the backend or starts any thread that
        // reads the environment (the install runtime above has shut down); only this
        // process sees it.
        unsafe { std::env::set_var(bunko_engines::torch::PACK_ENV, &pack_dir) };
        // `--from <dir>` (or the bundled folder): it may hold the model files under their
        // release names (an offline package); try it before the network.
        if let Some(from) = from.as_deref() {
            let from = std::path::absolute(from).unwrap_or_else(|_| from.to_path_buf());
            for var in [
                bunko_ocr::models::TORCH_MIRROR_ENV,
                bunko_ocr::models::MODELS_MIRROR_ENV,
            ] {
                if std::env::var_os(var).is_none_or(|v| v.is_empty()) {
                    // SAFETY: as above.
                    unsafe { std::env::set_var(var, &from) };
                }
            }
        }
        super::super::models::download(&ocr, None)
    }

    /// The pack's host libraries that the dynamic loader cannot find (`ldconfig -p`,
    /// then the usual library directories). The NVIDIA driver library is skipped: the
    /// container toolkit only mounts it into running GPU containers.
    pub fn missing_system_libs(libs: &[String]) -> Vec<String> {
        if !cfg!(target_os = "linux") {
            return Vec::new();
        }
        let cache = ["/sbin/ldconfig", "/usr/sbin/ldconfig", "ldconfig"]
            .iter()
            .find_map(|c| std::process::Command::new(c).arg("-p").output().ok())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let dirs = [
            "/lib",
            "/lib64",
            "/usr/lib",
            "/usr/lib64",
            "/lib/x86_64-linux-gnu",
            "/usr/lib/x86_64-linux-gnu",
            "/usr/local/lib",
        ];
        libs.iter()
            .filter(|l| l.as_str() != "libcuda.so.1")
            .filter(|l| {
                !cache
                    .lines()
                    .any(|line| line.trim_start().starts_with(&format!("{l} ")))
                    && !dirs.iter().any(|d| Path::new(d).join(l.as_str()).exists())
            })
            .cloned()
            .collect()
    }

    fn system_lib_hint(missing: &[String]) -> String {
        let mut deb = Vec::new();
        let mut arch = Vec::new();
        for m in missing {
            let (d, a) = match m.as_str() {
                "libnuma.so" => ("libnuma-dev", "numactl"),
                "libnuma.so.1" => ("libnuma1", "numactl"),
                "libelf.so.1" | "libdw.so.1" => ("libelf1 libdw1", "elfutils"),
                "libzstd.so.1" => ("libzstd1", "zstd"),
                "liblzma.so.5" => ("liblzma5", "xz"),
                "libbz2.so.1" => ("libbz2-1.0", "bzip2"),
                "libatomic.so.1" => ("libatomic1", "gcc-libs"),
                "libz.so.1" => ("zlib1g", "zlib"),
                _ => continue,
            };
            if !deb.contains(&d) {
                deb.push(d);
            }
            if !arch.contains(&a) {
                arch.push(a);
            }
        }
        format!(
            "Debian/Ubuntu: sudo apt install {}; Arch: sudo pacman -S {}",
            deb.join(" "),
            arch.join(" ")
        )
    }

    /// The offline OCR files (pack archive + model files, what `--from` takes) shipped
    /// with this copy of the program: inside the macOS app
    /// (`Mokuro Bunko.app/Contents/Resources/ocr-offline`), or `ocr-offline` next to the
    /// executable. `install-ocr` and the setup wizard use it when no `--from` is given.
    pub fn bundled_offline_dir() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        bundled_offline_in(exe.parent()?)
    }

    /// [`bundled_offline_dir`] for an executable in `exe_dir`: a folder counts when it
    /// holds a backend pack archive (`*-torch-*.tar.zst`, or its first part).
    pub fn bundled_offline_in(exe_dir: &Path) -> Option<PathBuf> {
        let mut candidates = Vec::new();
        if exe_dir.ends_with("Contents/MacOS")
            && let Some(contents) = exe_dir.parent()
        {
            candidates.push(contents.join("Resources").join("ocr-offline"));
        }
        candidates.push(exe_dir.join("ocr-offline"));
        candidates.into_iter().find(|d| holds_pack_archive(d))
    }

    fn holds_pack_archive(dir: &Path) -> bool {
        std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.filter_map(|e| e.ok()).any(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.contains("-torch-")
                    && (name.ends_with(".tar.zst") || name.ends_with(".tar.zst.001"))
                    && e.path().is_file()
            })
        })
    }

    /// Packs baked into a Docker image or shipped next to the executable.
    pub fn bundled_dir() -> Option<PathBuf> {
        std::env::current_exe()
            .ok()?
            .parent()
            .map(|d| d.join("backends"))
    }

    fn legacy_backend(b: &str) -> Result<&'static str, Fail> {
        Ok(match b {
            "auto" => "auto",
            "cuda" => "cu130",
            "rocm" | "hip" => "rocm7.1",
            "cpu" => "cpu",
            "skip" => {
                return Err(Fail::msg(
                    "--backend skip: nothing to install (set ocr.backend: skip in the config instead)",
                ));
            }
            other => {
                return Err(Fail::msg(format!(
                    "unknown backend '{other}' (auto, cuda, rocm, cpu)"
                )));
            }
        })
    }

    /// An installed pack of `variant` for this target, in `roots` (in order) or the
    /// bundled directory, whose files are all present with the right sizes (hashing
    /// gigabytes on every call would be slow; `install-ocr --force` re-verifies
    /// everything).
    pub fn find_installed(roots: &[PathBuf], variant: &str) -> Option<(PathBuf, PackManifest)> {
        // MOKURO_TORCH_PACK (the loader's override; set by the Docker images) first.
        let pinned = std::env::var_os("MOKURO_TORCH_PACK")
            .map(PathBuf::from)
            .filter(|d| d.join(pack::PACK_JSON).is_file());
        let mut candidates: Vec<(PathBuf, PackManifest)> = pinned
            .and_then(|d| {
                let m = PackManifest::parse(&std::fs::read(d.join(pack::PACK_JSON)).ok()?).ok()?;
                Some((d, m))
            })
            .into_iter()
            .collect();
        for r in roots.iter().cloned().chain(bundled_dir()) {
            candidates.extend(pack::installed(&r));
        }
        {
            for (dir, m) in candidates {
                if m.variant != variant || m.target != bunko_update::TARGET {
                    continue;
                }
                // A pack belongs to one release: another release's does not count.
                if !bunko_engines::torch::abi::same_release(&m.bunko_version, bunko_core::VERSION) {
                    continue;
                }
                if pack_complete(&dir, &m) {
                    return Some((dir, m));
                }
            }
        }
        None
    }

    /// Every file of the pack (archive and external) is there with its size.
    pub fn pack_complete(dir: &Path, m: &PackManifest) -> bool {
        m.files
            .iter()
            .map(|f| (&f.path, f.size))
            .chain(
                m.external
                    .iter()
                    .flat_map(|e| &e.files)
                    .map(|f| (&f.path, f.size)),
            )
            .all(|(p, size)| std::fs::metadata(dir.join(p)).is_ok_and(|md| md.len() == size))
    }

    fn print_status(
        hw: &hwdetect::Hardware,
        auto: &hwdetect::Choice,
        root: &Path,
        search: &[PathBuf],
    ) {
        println!("Target: {}", bunko_update::TARGET);
        match &hw.nvidia_driver {
            Some(d) => println!(
                "NVIDIA: driver {}{}",
                if d.is_empty() { "present" } else { d },
                if hw.nvidia_gpus.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", hw.nvidia_gpus.join(", "))
                }
            ),
            None => println!("NVIDIA: none"),
        }
        println!(
            "AMD ROCm: {}",
            if hw.amd_gfx.is_empty() {
                "none".to_string()
            } else {
                hw.amd_gfx.join(", ")
            }
        );
        for h in &hw.hidden {
            println!("Hidden: {h}");
        }
        println!("Would install: {} ({})", auto.variant, auto.reason);
        if let Some(h) = &auto.hint {
            println!("  {h}");
        }
        println!("Variants: cpu, cu130 (NVIDIA, Linux/Windows), rocm7.1 (AMD, Linux)");
        let mut any = false;
        let mut seen: Vec<PathBuf> = Vec::new();
        for r in search.iter().cloned().chain(bundled_dir()) {
            if seen.contains(&r) {
                continue;
            }
            seen.push(r.clone());
            for (dir, m) in pack::installed(&r) {
                any = true;
                println!(
                    "Installed: {} ({} {}, ABI {}, built by mokuro-bunko {})",
                    dir.display(),
                    m.variant,
                    m.target,
                    m.abi,
                    m.bunko_version
                );
            }
        }
        if !any {
            println!("Installed: none (in {})", root.display());
        }
    }

    /// `<backends>/.hardware.json`: the variant this machine's hardware called for when
    /// a pack was last installed there.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct HardwareRecord {
        pub auto_variant: String,
        #[serde(default)]
        pub reason: String,
    }

    impl HardwareRecord {
        pub const FILE: &'static str = ".hardware.json";

        pub fn write(&self, root: &Path) {
            let _ = std::fs::create_dir_all(root);
            let _ = std::fs::write(
                root.join(Self::FILE),
                serde_json::to_string_pretty(self).unwrap_or_default(),
            );
        }

        pub fn read(root: &Path) -> Option<HardwareRecord> {
            serde_json::from_str(&std::fs::read_to_string(root.join(Self::FILE)).ok()?).ok()
        }
    }

    /// Where a release's signed `release.json` is and the key that checks it.
    #[derive(Debug, Clone)]
    pub struct ReleaseSource {
        /// URL or path of the release's `release.json` (its `.sig` beside it).
        pub manifest: String,
        pub public_key: String,
    }

    impl ReleaseSource {
        /// Release `version`'s manifest for this role: `$MOKURO_BACKEND_MANIFEST`
        /// (when installing this binary's own release), else derived from the role's
        /// `update.manifest_url` (config.yaml, or processor.yaml's `update:`), checked
        /// with its `update.public_key` (config file only) or the compiled-in key.
        pub fn for_target(ocr: &crate::ocr_target::OcrTarget, version: &str) -> ReleaseSource {
            let (url, key) = role_update_settings(ocr);
            let manifest = match std::env::var(MANIFEST_ENV) {
                Ok(m) if !m.trim().is_empty() && version == bunko_core::VERSION => m,
                _ => bunko_update::auto::release_manifest_url(&url, version),
            };
            let (public_key, custom) = bunko_update::auto::release_key(&key);
            if custom {
                tracing::warn!(
                    "{}",
                    bunko_update::auto::custom_key_warning(&public_key, "update.public_key")
                );
            }
            ReleaseSource {
                manifest,
                public_key,
            }
        }
    }

    /// `(update.manifest_url, update.public_key)` of the role's config file.
    pub fn role_update_settings(ocr: &crate::ocr_target::OcrTarget) -> (String, String) {
        match ocr.role {
            crate::ocr_target::Role::Library => {
                let c = ocr.library.clone().unwrap_or_default();
                (c.update.manifest_url, c.update.public_key)
            }
            crate::ocr_target::Role::Processor => {
                let c = ocr
                    .processor_config
                    .as_deref()
                    .and_then(|p| bunko_processor::load_processor_config(p).ok())
                    .map(|c| c.update)
                    .unwrap_or_default();
                (c.manifest_url, c.public_key)
            }
        }
    }

    /// Why a pack was not installed; `needs_owner`: retrying will not help.
    #[derive(Debug, Clone)]
    pub struct PackFailure {
        pub message: String,
        pub needs_owner: bool,
        pub action: Option<String>,
    }

    impl PackFailure {
        fn retry(message: impl std::fmt::Display) -> PackFailure {
            PackFailure {
                message: message.to_string(),
                needs_owner: false,
                action: None,
            }
        }

        fn owner(message: impl Into<String>, action: impl Into<String>) -> PackFailure {
            PackFailure {
                message: message.into(),
                needs_owner: true,
                action: Some(action.into()),
            }
        }
    }

    impl std::fmt::Display for PackFailure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.message)?;
            if let Some(a) = &self.action {
                write!(f, " ({a})")?;
            }
            Ok(())
        }
    }

    /// The signed release manifest's entry for this target and variant, from release
    /// `version` only (a pack belongs to one release).
    async fn signed_artifact(
        client: &reqwest::Client,
        source: &ReleaseSource,
        version: &str,
        target: &str,
        variant: &str,
    ) -> Result<BackendArtifact, PackFailure> {
        let loc = &source.manifest;
        let m = match bunko_update::fetch_signed(client, loc, &source.public_key).await {
            Ok(m) => m,
            Err(e @ bunko_update::UpdateError::BadSignature) => {
                return Err(PackFailure::owner(
                    format!("{loc}: {e}"),
                    "The release manifest is not signed by the release key: do not install it. If you use a mirror or a fork, check update.manifest_url (and update.public_key) in the config file.",
                ));
            }
            Err(e) => return Err(PackFailure::retry(format!("{loc}: {e}"))),
        };
        if m.version.trim_start_matches('v') != version.trim_start_matches('v') {
            return Err(PackFailure::retry(format!(
                "{loc} is release {}, not {version}: the backend pack must come from the same release",
                m.version
            )));
        }
        m.backends
            .get(target)
            .and_then(|v| v.get(variant))
            .cloned()
            .ok_or_else(|| {
                let have: Vec<String> = m
                    .backends
                    .get(target)
                    .map(|v| v.keys().cloned().collect())
                    .unwrap_or_default();
                PackFailure::owner(
                    format!(
                        "release {} has no {variant} backend pack for {target} (it has: {})",
                        m.version,
                        if have.is_empty() {
                            "none".into()
                        } else {
                            have.join(", ")
                        }
                    ),
                    "Install another variant with 'mokuro-bunko install-ocr --variant <variant>', or stay on this release.",
                )
            })
    }

    /// The host requirements a pack states that this machine does not meet (the
    /// NVIDIA driver version): the owner must act.
    pub fn unmet_requirements(
        requires: &bunko_update::backend::Requires,
        hw: &hwdetect::Hardware,
    ) -> Option<PackFailure> {
        let need = requires.nvidia_driver.as_deref()?.trim();
        if need.is_empty() {
            return None;
        }
        let parse = |v: &str| -> Vec<u32> {
            v.split('.')
                .map(|p| p.trim().parse::<u32>().unwrap_or(0))
                .collect()
        };
        match hw.nvidia_driver.as_deref() {
            Some(have) if !have.is_empty() && parse(have) >= parse(need) => None,
            Some("") => None,
            Some(have) => Some(PackFailure::owner(
                format!(
                    "this backend pack needs NVIDIA driver {need} or newer; this machine has {have}"
                ),
                format!(
                    "Update the NVIDIA driver to {need} or newer, then restart; until then this machine stays on mokuro-bunko {}.",
                    bunko_core::VERSION
                ),
            )),
            None => Some(PackFailure::owner(
                format!(
                    "this backend pack needs NVIDIA driver {need} or newer; no NVIDIA driver is loaded"
                ),
                "Install the NVIDIA driver, or switch to another variant with 'mokuro-bunko install-ocr --variant cpu'.",
            )),
        }
    }

    /// `update prefetch` (run by a downloaded release before it is installed; see
    /// `crate::autoupdate`): stage THIS release's pack for the variant installed here,
    /// load it in this process, fetch this release's models with it. Never touches the
    /// installed pack.
    pub fn prefetch(
        ocr: &crate::ocr_target::OcrTarget,
        manifest_url: &str,
    ) -> crate::autoupdate::PrefetchResult {
        use crate::autoupdate::{PrefetchResult, StagedPackInfo};
        let version = bunko_core::VERSION;
        let fail = |f: PackFailure| PrefetchResult {
            ok: false,
            message: Some(f.message),
            needs_owner: f.needs_owner,
            action: f.action,
            pack: None,
        };
        // The pack the running release uses: its variant is the one to bring along.
        let current = std::env::var_os(bunko_engines::torch::PACK_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                bunko_engines::torch::discover_all(&ocr.backends_dirs())
                    .into_iter()
                    .next()
            });
        let variant = current.as_ref().and_then(|d| {
            std::fs::read(d.join(pack::PACK_JSON))
                .ok()
                .and_then(|b| PackManifest::parse(&b).ok())
                .map(|m| m.variant)
        });
        let Some(variant) = variant else {
            // No OCR backend here: only the models that need none.
            println!("No OCR backend pack installed here: fetching the models only");
            if let Err(e) = super::super::models::download(ocr, None) {
                println!("Note: {e} (no backend pack: the compiled packages are not needed)");
            }
            return PrefetchResult {
                ok: true,
                ..Default::default()
            };
        };
        let hw = hwdetect::detect();
        let want = hwdetect::choose(&hw, bunko_update::TARGET);
        if want.variant != "cpu" && want.variant != variant {
            println!(
                "Note: this machine now fits the {} pack ({}); the update keeps the installed {variant} pack",
                want.variant, want.reason
            );
        }
        let (_, key) = role_update_settings(ocr);
        let source = ReleaseSource {
            manifest: bunko_update::auto::release_manifest_url(manifest_url, version),
            public_key: bunko_update::auto::release_key(&key).0,
        };
        let root = ocr.backends_dir();
        println!(
            "Staging the {variant} backend pack of {version} in {}",
            root.display()
        );
        let rt = match crate::out::runtime() {
            Ok(rt) => rt,
            Err(e) => return fail(PackFailure::retry(format!("{e:?}"))),
        };
        let staged = rt.block_on(stage(
            &root,
            &variant,
            bunko_update::TARGET,
            None,
            &source,
            version,
        ));
        drop(rt);
        let staged = match staged {
            Ok(s) => s,
            Err(f) => return fail(f),
        };
        // Load it here, in the new release, and fetch the models with it.
        // SAFETY: no other thread reads the environment now (the runtime above is gone).
        unsafe { std::env::set_var(bunko_engines::torch::PACK_ENV, &staged.staging) };
        let pipeline =
            bunko_engines::EnginePipeline::new(ocr.engine_config(bunko_engines::Backend::Auto));
        match probe_pipeline(&pipeline) {
            Ok(lines) => lines.iter().for_each(|l| println!("{l}")),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staged.staging);
                return fail(PackFailure::owner(
                    format!(
                        "its OCR backend ({variant} for {version}) does not load on this machine: {e}"
                    ),
                    format!(
                        "This machine stays on its current release. Check 'mokuro-bunko doctor'; release {version} may not support this GPU/driver."
                    ),
                ));
            }
        }
        if let Err(e) = super::super::models::download(ocr, None) {
            let _ = std::fs::remove_dir_all(&staged.staging);
            return fail(PackFailure::retry(format!(
                "its models could not be fetched: {e}"
            )));
        }
        PrefetchResult {
            ok: true,
            pack: Some(StagedPackInfo {
                root,
                staging: staged.staging,
                name: staged.manifest.name,
                variant,
            }),
            ..Default::default()
        }
    }

    /// `install-ocr --probe`: open the pack this role would use (or `dir`) here and
    /// list its devices. Exit 1 when it does not load.
    fn probe(ocr: &crate::ocr_target::OcrTarget, dir: Option<&Path>) -> CmdResult {
        if let Some(d) = dir {
            // SAFETY: set before the backend is opened; this process only probes.
            unsafe { std::env::set_var(bunko_engines::torch::PACK_ENV, d) };
        }
        let pipeline =
            bunko_engines::EnginePipeline::new(ocr.engine_config(bunko_engines::Backend::Auto));
        match probe_pipeline(&pipeline) {
            Ok(lines) => {
                for l in lines {
                    println!("{l}");
                }
                Ok(())
            }
            Err(e) => Err(Fail::msg(format!("the OCR backend does not load: {e}"))),
        }
    }

    /// Open the backend of `pipeline` and check it drives what its variant promises (a
    /// GPU pack must list a GPU here). Ok: a line per device.
    pub fn probe_pipeline(pipeline: &bunko_engines::EnginePipeline) -> Result<Vec<String>, String> {
        let tb = pipeline.torch()?;
        let m = &tb.pack.manifest;
        let release = if m.bunko_version.is_empty() {
            "a development build".to_string()
        } else {
            m.bunko_version.clone()
        };
        let mut lines = vec![format!(
            "pack: {} ({} for {release}, torch {}) at {}",
            m.dir_name(),
            m.variant,
            tb.report.torch,
            tb.pack.dir.display()
        )];
        let gpus = tb.report.devices.iter().filter(|d| d.kind != "cpu").count();
        for d in &tb.report.devices {
            lines.push(format!("device: {} {} ({})", d.kind, d.name, d.arch));
        }
        if m.variant != "cpu" && gpus == 0 {
            return Err(format!(
                "the {} pack loaded but found no GPU it can drive on this machine",
                m.variant
            ));
        }
        Ok(lines)
    }

    /// Pack archive files for `variant` in a local directory: the whole archive or its
    /// numbered parts, preferring this version's.
    fn local_parts(dir: &Path, target: &str, variant: &str, version: &str) -> Vec<PathBuf> {
        let suffix = format!("-{target}-torch-{variant}.tar.zst");
        let ours = format!("mokuro-bunko-{version}{suffix}");
        let mut whole: Vec<PathBuf> = Vec::new();
        let mut parts: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n == ours {
                    return vec![e.path()];
                }
                if n.ends_with(&suffix) {
                    whole.push(e.path());
                } else if n.starts_with(&ours)
                    && n.len() == ours.len() + 4
                    && n[ours.len() + 1..].chars().all(|c| c.is_ascii_digit())
                {
                    parts.push(e.path());
                }
            }
        }
        if !parts.is_empty() {
            parts.sort();
            return parts;
        }
        whole.sort();
        whole.into_iter().last().into_iter().collect()
    }

    /// A line every 25% (`MOKURO_PROGRESS_STEP` sets the step: the desktop app asks for
    /// 2 to drive its progress bar).
    fn progress(label: String) -> impl FnMut(u64, u64) {
        let step = std::env::var("MOKURO_PROGRESS_STEP")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|s| (1..=100).contains(s))
            .unwrap_or(25);
        let mut last = 0u64;
        move |done, total| {
            let pct = (done * 100).checked_div(total).unwrap_or(100);
            if pct >= last + step || (done == total && last < 100) {
                last = pct;
                println!("  {label}: {pct}% of {}", mb(total));
            }
        }
    }

    fn mb(n: u64) -> String {
        format!("{:.0} MB", n as f64 / 1e6)
    }

    /// Download (or take from `from`), verify and activate this release's pack.
    /// Returns its dir.
    pub async fn install(
        root: &Path,
        variant: &str,
        target: &str,
        from: Option<&Path>,
        source: &ReleaseSource,
    ) -> Result<PathBuf, PackFailure> {
        let staged = stage(root, variant, target, from, source, bunko_core::VERSION).await?;
        activate_staged(root, &staged).map_err(PackFailure::retry)
    }

    /// A verified pack waiting in `<root>/.staging-<variant>`.
    #[derive(Debug, Clone)]
    pub struct StagedPack {
        pub staging: PathBuf,
        pub manifest: PackManifest,
    }

    /// Put a staged pack in place as `<root>/<name>` and remove the other packs of its
    /// variant (older releases).
    pub fn activate_staged(root: &Path, staged: &StagedPack) -> Result<PathBuf, String> {
        let dir = pack::activate(&staged.staging, root, &staged.manifest.name)
            .map_err(|e| e.to_string())?;
        for (other, m) in pack::installed(root) {
            if m.variant == staged.manifest.variant && other != dir {
                println!("Removing the old {}", other.display());
                let _ = std::fs::remove_dir_all(&other);
            }
        }
        let _ = std::fs::remove_dir_all(root.join(".download"));
        Ok(dir)
    }

    /// Download (or take from `from`) release `version`'s pack for `variant`, check it
    /// (signature, sha256, every file, the host's requirements, the disk space) and
    /// unpack it into `<root>/.staging-<variant>`, without touching the installed packs.
    pub async fn stage(
        root: &Path,
        variant: &str,
        target: &str,
        from: Option<&Path>,
        source: &ReleaseSource,
        version: &str,
    ) -> Result<StagedPack, PackFailure> {
        let io = |e: std::io::Error| PackFailure::retry(format!("{}: {e}", root.display()));
        std::fs::create_dir_all(root).map_err(io)?;
        let downloads = root.join(".download");
        std::fs::create_dir_all(&downloads).map_err(io)?;
        let client = reqwest::Client::builder()
            .user_agent(format!("mokuro-bunko/{}", bunko_core::VERSION))
            .build()
            .map_err(PackFailure::retry)?;
        let hw = hwdetect::detect();

        // The archive parts and, when signed, the whole-archive checksum.
        let (parts, whole): (Vec<PathBuf>, Option<(String, u64)>) = match from {
            Some(dir) => {
                let files = local_parts(dir, target, variant, version);
                if files.is_empty() {
                    return Err(PackFailure::owner(
                        format!(
                            "no mokuro-bunko-*-{target}-torch-{variant}.tar.zst in {}",
                            dir.display()
                        ),
                        "Put the pack archive of this release in that folder, or install without --from.",
                    ));
                }
                let manifest = dir.join("release.json");
                let whole = if manifest.is_file() && dir.join("release.json.sig").is_file() {
                    let local = ReleaseSource {
                        manifest: manifest.display().to_string(),
                        public_key: source.public_key.clone(),
                    };
                    let a = signed_artifact(&client, &local, version, target, variant).await?;
                    println!(
                        "Checking {} against the signed release.json",
                        files[0].display()
                    );
                    Some((a.sha256, a.size))
                } else {
                    println!(
                        "Warning: no signed release.json in {}: only pack.json's checksums are checked.",
                        dir.display()
                    );
                    None
                };
                (files, whole)
            }
            None => {
                println!("Release manifest: {}", source.manifest);
                let a = signed_artifact(&client, source, version, target, variant).await?;
                if let Some(f) = a.requires.as_ref().and_then(|r| unmet_requirements(r, &hw)) {
                    return Err(f);
                }
                let need = a.size + a.external_size + a.installed_size;
                if let Err(e) = bunko_update::auto::check_space(root, need) {
                    return Err(PackFailure::owner(
                        e,
                        format!(
                            "Free disk space where {} is (or move the storage), then try again.",
                            root.display()
                        ),
                    ));
                }
                println!(
                    "Downloading {} ({} archive, {} more from NVIDIA/PyPI, {} on disk)",
                    a.name,
                    mb(a.size),
                    mb(a.external_size),
                    mb(a.installed_size)
                );
                let mut files = Vec::new();
                for (i, p) in a.parts.iter().enumerate() {
                    let name = p.url.rsplit('/').next().unwrap_or("pack").to_string();
                    let dest = downloads.join(&name);
                    pack::download(
                        &client,
                        &p.url,
                        &dest,
                        &p.sha256,
                        p.size,
                        progress(format!("{name} [{}/{}]", i + 1, a.parts.len())),
                    )
                    .await
                    .map_err(|e| match e {
                        pack::PackError::Checksum { .. } | pack::PackError::Size { .. } => {
                            PackFailure::retry(format!("{name}: {e} (downloaded again next time)"))
                        }
                        other => PackFailure::retry(format!("{name}: {other}")),
                    })?;
                    files.push(dest);
                }
                (files, Some((a.sha256, a.size)))
            }
        };

        let staging = root.join(format!(".staging-{variant}"));
        if staging.exists() {
            std::fs::remove_dir_all(&staging).map_err(io)?;
        }
        println!("Unpacking and verifying...");
        let staging2 = staging.clone();
        let manifest = tokio::task::spawn_blocking(move || {
            pack::unpack_archive_progress(
                &parts,
                whole.as_ref().map(|(s, n)| (s.as_str(), *n)),
                &staging2,
                progress("unpacking".into()),
            )
        })
        .await
        .map_err(PackFailure::retry)?
        .map_err(PackFailure::retry)?;
        if manifest.variant != variant || manifest.target != target {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(PackFailure::owner(
                format!(
                    "the archive holds a {} pack for {}, not {variant} for {target}",
                    manifest.variant, manifest.target
                ),
                "Use the pack archive of this machine's variant and platform.",
            ));
        }
        if !bunko_engines::torch::abi::same_release(&manifest.bunko_version, version) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(PackFailure::owner(
                format!(
                    "the archive holds the backend pack of mokuro-bunko {}, not {version}: each release runs only its own pack",
                    manifest.bunko_version
                ),
                format!("Use the pack archive of release {version}."),
            ));
        }
        if let Some(f) = unmet_requirements(&manifest.requires, &hw) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(f);
        }

        if !manifest.external.is_empty() {
            println!(
                "Fetching {} from NVIDIA's packages on PyPI (NVIDIA licence terms; texts in {}/licenses/):",
                mb(manifest.external_size()),
                manifest.name
            );
        }
        for ext in &manifest.external {
            let file = ext.url.rsplit('/').next().unwrap_or(&ext.name).to_string();
            let local = from.map(|d| d.join(&file)).filter(|p| p.is_file());
            let dest = downloads.join(&file);
            let src = match local {
                Some(p) => p.display().to_string(),
                None => ext.url.clone(),
            };
            pack::download(
                &client,
                &src,
                &dest,
                &ext.sha256,
                ext.size,
                progress(format!("{} {} ({})", ext.name, ext.version, ext.license)),
            )
            .await
            .map_err(PackFailure::retry)?;
            let (e2, st) = (ext.clone(), staging.clone());
            tokio::task::spawn_blocking(move || pack::unpack_external(&dest, &e2, &st))
                .await
                .map_err(PackFailure::retry)?
                .map_err(PackFailure::retry)?;
        }
        Ok(StagedPack { staging, manifest })
    }
}

#[cfg(all(test, feature = "ocr"))]
mod tests {
    use super::*;
    use std::path::Path;

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn a_driver_too_old_for_the_pack_needs_the_owner() {
        let need = bunko_update::backend::Requires {
            nvidia_driver: Some("580.65.06".into()),
            ..Default::default()
        };
        let hw = |d: Option<&str>| crate::hwdetect::Hardware {
            nvidia_driver: d.map(str::to_string),
            ..Default::default()
        };
        assert!(unmet_requirements(&need, &hw(Some("595.58.03"))).is_none());
        assert!(unmet_requirements(&need, &hw(Some("580.65.06"))).is_none());
        let old = unmet_requirements(&need, &hw(Some("570.1"))).unwrap();
        assert!(old.needs_owner);
        assert!(
            old.message
                .contains("needs NVIDIA driver 580.65.06 or newer; this machine has 570.1")
        );
        assert!(old.action.unwrap().starts_with("Update the NVIDIA driver"));
        assert!(unmet_requirements(&need, &hw(None)).unwrap().needs_owner);
        assert!(unmet_requirements(&Default::default(), &hw(None)).is_none());
    }

    #[test]
    fn another_releases_pack_does_not_count_as_installed() {
        let dir = tempfile::tempdir().unwrap();
        let pack = dir.path().join("torch-cpu-2.13.0");
        std::fs::create_dir_all(&pack).unwrap();
        let write = |v: &str| {
            let m = serde_json::json!({
                "format": 1, "name": "torch-cpu-2.13.0", "variant": "cpu", "torch": "2.13.0",
                "target": bunko_update::TARGET, "os": "linux", "arch": "x86_64", "abi": 1,
                "library": "", "bunko_version": v, "files": []
            });
            std::fs::write(pack.join("pack.json"), m.to_string()).unwrap();
        };
        let roots = vec![dir.path().to_path_buf()];
        write("0.0.1");
        assert!(
            find_installed(&roots, "cpu").is_none(),
            "another release's pack"
        );
        write(bunko_core::VERSION);
        assert!(find_installed(&roots, "cpu").is_some());
        write("");
        assert!(
            find_installed(&roots, "cpu").is_some(),
            "a development pack"
        );
    }

    #[test]
    fn finds_the_ocr_files_bundled_with_the_program() {
        let dir = tempfile::tempdir().unwrap();
        // The macOS app: Contents/MacOS/mokuro-bunko + Contents/Resources/ocr-offline.
        let app = dir.path().join("Mokuro Bunko.app/Contents");
        let macos = app.join("MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        assert_eq!(bundled_offline_in(&macos), None);
        let offline = app.join("Resources/ocr-offline");
        // A folder without a pack archive does not count.
        touch(&offline.join("hayai-nova_config.json"));
        assert_eq!(bundled_offline_in(&macos), None);
        touch(&offline.join("mokuro-bunko-0.7.0-aarch64-apple-darwin-torch-cpu.tar.zst"));
        assert_eq!(bundled_offline_in(&macos), Some(offline));

        // Elsewhere: ocr-offline next to the executable (split archives count too).
        let plain = dir.path().join("mb");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(bundled_offline_in(&plain), None);
        touch(&plain.join(
            "ocr-offline/mokuro-bunko-0.7.0-x86_64-unknown-linux-gnu-torch-rocm7.1.tar.zst.001",
        ));
        assert_eq!(bundled_offline_in(&plain), Some(plain.join("ocr-offline")));
        // `Resources/` is only looked at inside an app bundle.
        let outside = dir.path().join("lib/bin");
        std::fs::create_dir_all(&outside).unwrap();
        touch(
            &dir.path()
                .join("lib/Resources/ocr-offline/x-torch-cpu.tar.zst"),
        );
        assert_eq!(bundled_offline_in(&outside), None);
    }
}
