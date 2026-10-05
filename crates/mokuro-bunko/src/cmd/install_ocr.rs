//! `install-ocr`: install the OCR backend pack for this machine (TORCH-BACKEND.md,
//! PACKAGING.md §8) and fetch the models.
//!
//! 1. Pick the pack variant: `cu130` (NVIDIA, driver ≥ 580), `rocm7.1` (supported AMD
//!    GPU, Linux) or `cpu` ([`crate::hwdetect`]); `--variant` overrides.
//! 2. Find it in the signed `release.json` of **this** version (`backends[target]`),
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
            let dir = rt.block_on(install(&root, &variant, target, args.from.as_deref()))?;
            println!("Installed {}", dir.display());
            dir
        };
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

    fn manifest_location() -> String {
        std::env::var(MANIFEST_ENV).unwrap_or_else(|_| {
            format!(
                "https://github.com/Gnathonic/mokuro-bunko/releases/download/v{}/release.json",
                bunko_core::VERSION
            )
        })
    }

    async fn read_location(client: &reqwest::Client, loc: &str) -> Result<Vec<u8>, Fail> {
        if loc.contains("://") && !loc.starts_with("file://") {
            let r = client
                .get(loc)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| Fail::msg(format!("{loc}: {e}")))?;
            Ok(r.bytes().await.map_err(Fail::msg)?.to_vec())
        } else {
            let p = loc.strip_prefix("file://").unwrap_or(loc);
            std::fs::read(p).map_err(|e| Fail::msg(format!("{p}: {e}")))
        }
    }

    /// The signed release manifest's entry for this target and variant.
    async fn signed_artifact(
        client: &reqwest::Client,
        loc: &str,
        target: &str,
        variant: &str,
    ) -> Result<BackendArtifact, Fail> {
        let bytes = read_location(client, loc).await?;
        let sig = read_location(client, &format!("{loc}.sig")).await?;
        let m = bunko_update::parse_manifest(
            &bytes,
            &String::from_utf8_lossy(&sig),
            bunko_update::RELEASE_PUBLIC_KEY,
        )
        .map_err(|e| Fail::msg(format!("{loc}: {e}")))?;
        if m.version.trim_start_matches('v') != bunko_core::VERSION {
            return Err(Fail::msg(format!(
                "{loc} is release {}, this is mokuro-bunko {}: the backend pack must come from the same release",
                m.version,
                bunko_core::VERSION
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
                Fail::msg(format!(
                    "release {} has no {variant} backend pack for {target} (it has: {})",
                    m.version,
                    if have.is_empty() {
                        "none".into()
                    } else {
                        have.join(", ")
                    }
                ))
            })
    }

    /// Pack archive files for `variant` in a local directory: the whole archive or its
    /// numbered parts, preferring this version's.
    fn local_parts(dir: &Path, target: &str, variant: &str) -> Vec<PathBuf> {
        let suffix = format!("-{target}-torch-{variant}.tar.zst");
        let ours = format!("mokuro-bunko-{}{suffix}", bunko_core::VERSION);
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

    /// Download (or take from `from`), verify and activate the pack. Returns its dir.
    pub async fn install(
        root: &Path,
        variant: &str,
        target: &str,
        from: Option<&Path>,
    ) -> Result<PathBuf, Fail> {
        std::fs::create_dir_all(root).map_err(|e| Fail::msg(format!("{}: {e}", root.display())))?;
        let downloads = root.join(".download");
        std::fs::create_dir_all(&downloads)?;
        let client = reqwest::Client::builder()
            .user_agent(format!("mokuro-bunko/{}", bunko_core::VERSION))
            .build()
            .map_err(Fail::msg)?;

        // The archive parts and, when signed, the whole-archive checksum.
        let (parts, whole): (Vec<PathBuf>, Option<(String, u64)>) = match from {
            Some(dir) => {
                let files = local_parts(dir, target, variant);
                if files.is_empty() {
                    return Err(Fail::msg(format!(
                        "no mokuro-bunko-*-{target}-torch-{variant}.tar.zst in {}",
                        dir.display()
                    )));
                }
                let manifest = dir.join("release.json");
                let whole = if manifest.is_file() && dir.join("release.json.sig").is_file() {
                    let a =
                        signed_artifact(&client, &manifest.display().to_string(), target, variant)
                            .await?;
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
                let loc = manifest_location();
                println!("Release manifest: {loc}");
                let a = signed_artifact(&client, &loc, target, variant).await?;
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
                    .map_err(Fail::msg)?;
                    files.push(dest);
                }
                (files, Some((a.sha256, a.size)))
            }
        };

        let staging = root.join(format!(".staging-{variant}"));
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
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
        .map_err(Fail::msg)?
        .map_err(Fail::msg)?;
        if manifest.variant != variant || manifest.target != target {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(Fail::msg(format!(
                "the archive holds a {} pack for {}, not {variant} for {target}",
                manifest.variant, manifest.target
            )));
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
            .map_err(Fail::msg)?;
            let (e2, st) = (ext.clone(), staging.clone());
            tokio::task::spawn_blocking(move || pack::unpack_external(&dest, &e2, &st))
                .await
                .map_err(Fail::msg)?
                .map_err(Fail::msg)?;
        }

        let dir = pack::activate(&staging, root, &manifest.name).map_err(Fail::msg)?;
        // Other packs of the same variant are older versions now.
        for (other, m) in pack::installed(root) {
            if m.variant == variant && other != dir {
                println!("Removing the old {}", other.display());
                let _ = std::fs::remove_dir_all(&other);
            }
        }
        let _ = std::fs::remove_dir_all(&downloads);
        Ok(dir)
    }
}
