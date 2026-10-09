//! `xtask torch-pack`: build an OCR backend pack (docs/rust-port/TORCH-BACKEND.md,
//! PACKAGING.md §8) from the official libtorch 2.13.0 distribution.
//!
//! 1. Download (cached, sha256-pinned) the libtorch zip of the variant, and for CUDA the
//!    NVIDIA wheels from PyPI ([`crate::torch_specs`]).
//! 2. Stage the minimal file set: the spec's `keep` list from libtorch's `lib/` (ROCm
//!    kernel data trimmed to the supported GPU architectures).
//! 3. Build `libbunko_torch` (`cargo build -p bunko-torch --features libtorch`) against
//!    exactly that libtorch (`LIBTORCH` = an unpacked copy with the headers).
//! 4. Check the dynamic-link closure: every library a staged binary needs is in the
//!    pack, comes from an external wheel, or is an allowed system library.
//! 5. Licences: PyTorch's LICENSE + bundled third-party licences (from the CPU wheel's
//!    dist-info; the zips carry none), the NVIDIA wheels' licence files.
//! 6. Write `pack.json` (sha256 of every file; the NVIDIA wheels as `external` unless
//!    `--bundle-external`) and `mokuro-bunko-backend-<ver>-<platform>-<variant>.tar.zst`
//!    (split in parts above `--max-part`; `<platform>`: `linux-x64`, `windows`, `macos`).

use crate::archive;
use crate::torch_specs::{self, Layout, Spec, Upstream, Wheel};
use crate::util;
use anyhow::{Context, Result, bail};
use bunko_update::backend::{
    ExternalArchive, ExternalFile, PACK_FORMAT, PACK_JSON, PackFile, PackLink, PackManifest,
    Requires,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// GitHub release assets must be smaller than 2 GiB.
const DEFAULT_MAX_PART: u64 = 1900 * 1024 * 1024;

#[derive(Debug, clap::Args)]
pub struct TorchPackArgs {
    /// Pack variant: cpu, cu130 (Linux, Windows), rocm7.1 (Linux).
    #[arg(long)]
    pub variant: String,
    /// Rust target triple (default: the host).
    #[arg(long)]
    pub target: Option<String>,
    /// Output directory for the archive (and its `.sha256`).
    #[arg(long, default_value = "dist")]
    pub out: PathBuf,
    /// Download cache (default: $BUNKO_PACK_CACHE, else <target dir>/torch-pack-cache).
    #[arg(long)]
    pub cache: Option<PathBuf>,
    /// A local libtorch zip, unpacked libtorch directory or torch wheel to use instead
    /// of the pinned download (must be libtorch 2.13.0 of the same variant).
    #[arg(long)]
    pub source: Option<PathBuf>,
    /// Use this prebuilt libbunko_torch instead of building it.
    #[arg(long, conflicts_with = "no_cdylib")]
    pub cdylib: Option<PathBuf>,
    /// Make a pack without libbunko_torch (runtime-only; for testing the file set).
    #[arg(long)]
    pub no_cdylib: bool,
    /// Put the NVIDIA libraries in the archive instead of listing them for the
    /// installer to fetch from PyPI (Docker images, offline installs). Check the
    /// licence notes in PACKAGING.md §8 before publishing such an archive.
    #[arg(long)]
    pub bundle_external: bool,
    /// Keep every library of libtorch's lib/ (to find the minimal set; never released).
    #[arg(long)]
    pub keep_all: bool,
    /// GPU architectures to keep ROCm kernels for (default: the spec's list).
    #[arg(long, value_delimiter = ',')]
    pub gpu_archs: Vec<String>,
    /// Stop after staging: write the unpacked pack to `<out>/<pack name>/`, no archive.
    #[arg(long)]
    pub no_archive: bool,
    /// zstd level (1-22).
    #[arg(long, default_value_t = 19)]
    pub level: i32,
    /// Split archives larger than this many bytes into `.001`, `.002`, ... parts.
    #[arg(long, default_value_t = DEFAULT_MAX_PART)]
    pub max_part: u64,
    /// Pass `--locked` to cargo.
    #[arg(long)]
    pub locked: bool,
    /// Windows: do not ship the Visual C++ runtime in the pack (it is then needed from
    /// the system, the "VC++ Redistributable").
    #[arg(long)]
    pub no_vc_runtime: bool,
    /// A compiled model package (`.pt2` zip, unpacked `.pt2` directory or a package
    /// directory) whose libraries' imports the closure check must also resolve: what the
    /// AOTInductor model libraries need (e.g. `cudart64_13.dll`, `vcomp140.dll` on
    /// Windows) is invisible in the pack's own libraries. Repeatable.
    #[arg(long, value_name = "PATH")]
    pub sample_package: Vec<PathBuf>,
}

/// A staged file: pack-relative path → source file on disk.
type Staged = BTreeMap<String, PathBuf>;

pub fn run(args: &TorchPackArgs) -> Result<PathBuf> {
    let root = util::workspace_root();
    let target = match &args.target {
        Some(t) => t.clone(),
        None => util::host_triple()?,
    };
    let spec = torch_specs::find(&args.variant, &target).with_context(|| {
        format!(
            "no {} pack for {target}; known: {}",
            args.variant,
            torch_specs::variants().join(", ")
        )
    })?;
    let version = util::workspace_version(&root)?;
    let cache = match &args.cache {
        Some(c) => abs(&root, c),
        None => default_cache(&root),
    };
    std::fs::create_dir_all(&cache)?;
    let out_dir = abs(&root, &args.out);
    std::fs::create_dir_all(&out_dir)?;
    let name = PackManifest::dir_name(spec.variant, torch_specs::TORCH_VERSION);
    let (os, arch) = os_arch(&target);
    eprintln!("==> {name} for {target} (mokuro-bunko {version})");

    // 1. libtorch.
    let source = match &args.source {
        Some(s) => abs(&root, s),
        None => fetch(&spec.libtorch, &cache)?,
    };
    let layout = if source.is_dir() {
        Layout::LibtorchZip
    } else if source.extension().is_some_and(|e| e == "whl") {
        Layout::Wheel
    } else {
        spec.layout
    };
    let work = cache.join(format!("work-{}-{target}", spec.variant));
    let unpacked = work.join("libtorch");
    let archs: Vec<String> = if args.gpu_archs.is_empty() {
        spec.gpu_archs.iter().map(|s| s.to_string()).collect()
    } else {
        args.gpu_archs.clone()
    };
    let (lib_files, torch_build) =
        unpack_libtorch(&source, layout, spec, args.keep_all, &archs, &unpacked)?;
    let mut staged: Staged = lib_files
        .iter()
        .map(|rel| (format!("lib/{rel}"), unpacked.join("lib").join(rel)))
        .collect();

    // NVIDIA wheels: fetched for the closure check, the licences and pack.json.
    let mut external = Vec::new();
    let ext_dir = work.join("external");
    for w in spec.wheels {
        let wheel = fetch(&w.upstream, &cache)?;
        let (ext, files) = unpack_wheel(&wheel, w, &ext_dir)?;
        if args.bundle_external {
            for (rel, src) in files {
                staged.insert(rel, src);
            }
        } else {
            external.push(ext);
        }
    }

    // Stubs for libraries that are linked but never called (spec.stubs).
    if !spec.stubs.is_empty() {
        let mut importers: Vec<PathBuf> = staged.values().cloned().collect();
        importers.extend(
            external
                .iter()
                .flat_map(|e| &e.files)
                .filter(|f| f.path.starts_with("lib/"))
                .map(|f| ext_dir.join(&f.path)),
        );
        let stub_dir = work.join("stubs");
        for (soname, path) in make_stubs(spec.stubs, &importers, &stub_dir)? {
            eprintln!("    stub lib/{soname}");
            staged.insert(format!("lib/{soname}"), path);
        }
    }

    // 2. libbunko_torch.
    let library = crate_library_name(&target);
    let mut abi = 0;
    if !args.no_cdylib {
        let cdylib = match &args.cdylib {
            Some(p) => abs(&root, p),
            None => build_cdylib(&root, &target, spec, &unpacked, &ext_dir, args.locked)?,
        };
        staged.insert(library.clone(), cdylib);
        abi = abi_version(&root)?;
    }

    // The Visual C++ runtime, app-local (Windows).
    if os_arch(&target).0 == "windows" && !args.no_vc_runtime {
        for dll in crate::vcredist::find(crate::vcredist::PACK_DLLS)? {
            let name = base(&dll.to_string_lossy().replace('\\', "/")).to_string();
            eprintln!("    VC++ runtime lib/{name}");
            staged.insert(format!("lib/{name}"), dll);
        }
    }

    // Every ELF library under its SONAME too (the dynamic loader opens NEEDED names as
    // files when they are not loaded yet).
    let links = soname_links(&staged)?;
    for l in &links {
        eprintln!("    link {} -> {}", l.path, l.target);
    }

    // 3. Closure.
    let mut available: Vec<PathBuf> = staged.values().cloned().collect();
    let samples = work.join("sample-packages");
    for (i, p) in args.sample_package.iter().enumerate() {
        available.extend(package_libraries(
            &abs(&root, p),
            &samples.join(i.to_string()),
        )?);
    }
    if !args.bundle_external {
        available.extend(
            external
                .iter()
                .flat_map(|e| &e.files)
                .map(|f| ext_dir.join(&f.path)),
        );
    }
    let link_names: Vec<String> = links.iter().map(|l| base(&l.path).to_string()).collect();
    let problems = closure_check(&available, &link_names, spec, &target)?;
    if !problems.is_empty() {
        let msg = problems.join("\n  ");
        if args.keep_all {
            eprintln!("warning: unresolved libraries:\n  {msg}");
        } else {
            bail!("unresolved libraries (add them to the spec or the system list):\n  {msg}");
        }
    }

    // 4. Licences.
    let lic = work.join("licenses");
    let _ = std::fs::remove_dir_all(&lic);
    let lic_wheel = fetch(&torch_specs::TORCH_LICENSES_WHEEL, &cache)?;
    for (rel, src) in torch_licenses(&lic_wheel, &lic)? {
        staged.insert(rel, src);
    }
    if args.bundle_external {
        for w in spec.wheels {
            for f in w.license_files {
                let src = ext_dir.join(format!("licenses/{}/{}", w.name, base(f)));
                staged.insert(format!("licenses/{}/{}", w.name, base(f)), src);
            }
        }
    }
    if let Some(dir) = spec.license_dir {
        let src = root.join(dir);
        let base_name = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .context("license_dir name")?;
        let files =
            archive::list_files(&src).with_context(|| format!("reading {}", src.display()))?;
        if files.is_empty() {
            bail!("{} has no licence texts", src.display());
        }
        for rel in files {
            let r = rel.to_string_lossy().replace('\\', "/");
            staged.insert(format!("licenses/{base_name}/{r}"), src.join(&rel));
        }
    }
    if let Some(dir) = spec.share_dir {
        let src = root.join(dir);
        let files =
            archive::list_files(&src).with_context(|| format!("reading {}", src.display()))?;
        for rel in files {
            let r = rel.to_string_lossy().replace('\\', "/");
            staged.insert(format!("share/{r}"), src.join(&rel));
        }
    }
    if os_arch(&target).0 == "windows" && !args.no_vc_runtime {
        let note = lic.join("microsoft-vc-runtime.txt");
        std::fs::write(&note, format!("{}\n", crate::vcredist::NOTICE))?;
        staged.insert("licenses/microsoft-vc-runtime.txt".into(), note);
    }
    let readme = lic.join("README.md");
    std::fs::write(
        &readme,
        licenses_readme(spec, &torch_build, args.bundle_external),
    )?;
    staged.insert("licenses/README.md".into(), readme);

    // 5. pack.json.
    let mut files = Vec::new();
    for (rel, src) in &staged {
        let (sha256, size) = util::sha256_file(src)?;
        files.push(PackFile {
            path: rel.clone(),
            sha256,
            size,
        });
    }
    let mut system_libs: Vec<String> = spec.system_libs.iter().map(|s| s.to_string()).collect();
    system_libs.sort();
    let manifest = PackManifest {
        format: PACK_FORMAT,
        name: name.clone(),
        variant: spec.variant.into(),
        torch: torch_specs::TORCH_VERSION.into(),
        torch_build,
        target: target.clone(),
        os: os.into(),
        arch: arch.into(),
        abi,
        library: if args.no_cdylib {
            String::new()
        } else {
            library.clone()
        },
        lib_dir: "lib".into(),
        bunko_version: version.clone(),
        requires: Requires {
            nvidia_driver: spec.nvidia_driver.map(str::to_string),
            system_libs,
            gpu_archs: if spec.arch_dirs.is_empty() {
                Vec::new()
            } else {
                archs.clone()
            },
        },
        files,
        external,
        links,
    };
    let json_path = work.join(PACK_JSON);
    let mut json = serde_json::to_vec_pretty(&manifest)?;
    json.push(b'\n');
    std::fs::write(&json_path, &json)?;
    let unpacked_size = manifest.files.iter().map(|f| f.size).sum::<u64>();
    eprintln!(
        "    {} files, {} in the pack + {} fetched from PyPI ({} download) = {} installed",
        manifest.files.len(),
        human(unpacked_size),
        human(manifest.installed_size() - unpacked_size),
        human(manifest.external_size()),
        human(manifest.installed_size()),
    );

    if args.no_archive {
        let dest = out_dir.join(&name);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        std::fs::create_dir_all(&dest)?;
        std::fs::copy(&json_path, dest.join(PACK_JSON))?;
        for (rel, src) in &staged {
            let d = dest.join(rel);
            std::fs::create_dir_all(d.parent().context("parent")?)?;
            link_or_copy(src, &d)?;
        }
        bunko_update::backend::make_links(&dest, &manifest).map_err(|e| anyhow::anyhow!("{e}"))?;
        println!("{}", dest.display());
        return Ok(dest);
    }

    // 6. Archive.
    let stem = archive_stem(&version, &target, spec.variant);
    let archive = out_dir.join(format!("{stem}.tar.zst"));
    for old in pack_parts(&out_dir, &stem)? {
        std::fs::remove_file(old)?;
    }
    eprintln!("    zstd -{} ...", args.level);
    write_tar_zst(
        &json,
        &staged,
        &manifest.links,
        &name,
        &archive,
        args.level,
        util::build_epoch(),
    )?;
    let (sha256, size) = util::sha256_file(&archive)?;
    let file = format!("{stem}.tar.zst");
    // No `.sha256` file: SHA256SUMS and the signed release.json carry every checksum.
    let parts = split(&archive, args.max_part)?;
    eprintln!(
        "    {}: {} compressed ({} parts), {} unpacked",
        file,
        human(size),
        parts.len(),
        human(unpacked_size)
    );
    println!("{}  {sha256}  {size} bytes", archive.display());
    Ok(parts.into_iter().next().unwrap_or(archive))
}

fn abs(root: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

/// The prefix of the backend packs' file names: after every download in a release's
/// alphabetical file list.
pub const PACK_PREFIX: &str = "mokuro-bunko-backend";

/// `mokuro-bunko-backend-<ver>-<platform>-<variant>` (`<platform>`:
/// [`crate::names::pack_platform`]; a target without one keeps the triple).
pub fn archive_stem(version: &str, target: &str, variant: &str) -> String {
    let platform = crate::names::pack_platform(target).unwrap_or(target);
    format!(
        "{PACK_PREFIX}-{}-{platform}-{variant}",
        crate::names::strip_v(version)
    )
}

/// A pack archive (or part) name → `(target, variant, part)`:
/// `mokuro-bunko-backend-<ver>-<platform>-<variant>.tar.zst[.NNN]`.
pub fn parse_pack_name<'a>(file: &'a str, version: &str) -> Option<(&'a str, &'a str, u32)> {
    let rest = file
        .strip_prefix(PACK_PREFIX)?
        .strip_prefix('-')?
        .strip_prefix(crate::names::strip_v(version))?
        .strip_prefix('-')?;
    let (stem, part) = match rest.rsplit_once(".tar.zst") {
        Some((stem, "")) => (stem, 0),
        Some((stem, suffix)) => {
            let n = suffix.strip_prefix('.')?;
            if n.len() != 3 || !n.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            (stem, n.parse().ok()?)
        }
        None => return None,
    };
    for platform in ["linux-x64", "windows", "macos"] {
        if let Some(variant) = stem
            .strip_prefix(platform)
            .and_then(|v| v.strip_prefix('-'))
            .filter(|v| !v.is_empty() && !v.contains('-'))
        {
            return Some((crate::names::pack_target(platform)?, variant, part));
        }
    }
    // A target without a platform name keeps its triple.
    let (target, variant) = stem.rsplit_once('-')?;
    (target.split('-').count() >= 3 && !variant.is_empty()).then_some((target, variant, part))
}

/// The archive and any parts of it in `dir`.
fn pack_parts(dir: &Path, stem: &str) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with(&format!("{stem}.tar.zst")) {
            out.push(e.path());
        }
    }
    out.sort();
    Ok(out)
}

fn os_arch(target: &str) -> (&'static str, &'static str) {
    let os = if target.contains("windows") {
        "windows"
    } else if target.contains("apple") {
        "macos"
    } else {
        "linux"
    };
    let arch = if target.starts_with("aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    (os, arch)
}

/// The functions (and their symbol versions) that `importers` import from `soname`,
/// read from their GNU version-needs tables.
fn stub_imports(soname: &str, importers: &[PathBuf]) -> Result<BTreeSet<(String, Option<String>)>> {
    let mut out = BTreeSet::new();
    for f in importers {
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if !name.contains(".so") || name == soname {
            continue;
        }
        let bytes = std::fs::read(f)?;
        let Ok(goblin::Object::Elf(elf)) = goblin::Object::parse(&bytes) else {
            continue;
        };
        if !elf.libraries.contains(&soname) {
            continue;
        }
        // version index -> version name, for this soname's entries
        let mut versions: BTreeMap<u16, String> = BTreeMap::new();
        if let Some(vn) = &elf.verneed {
            for need in vn.iter() {
                if elf.dynstrtab.get_at(need.vn_file) != Some(soname) {
                    continue;
                }
                for aux in need.iter() {
                    if let Some(v) = elf.dynstrtab.get_at(aux.vna_name) {
                        versions.insert(aux.vna_other, v.to_string());
                    }
                }
            }
        }
        let Some(versym) = &elf.versym else { continue };
        for (sym, vs) in elf.dynsyms.iter().zip(versym.iter()) {
            if sym.st_shndx != 0 || sym.st_name == 0 {
                continue;
            }
            if let Some(v) = versions.get(&vs.version())
                && let Some(n) = elf.dynstrtab.get_at(sym.st_name)
            {
                out.insert((n.to_string(), Some(v.clone())));
            }
        }
    }
    Ok(out)
}

/// Build a stub shared library for each SONAME: the functions the pack's libraries
/// import from it, with the same symbol versions, each printing which function was
/// called and aborting (the libraries are linked by libtorch but never called by OCR;
/// a call would be a bug to report, not a silent wrong result). Needs a C compiler.
fn make_stubs(
    sonames: &[&str],
    importers: &[PathBuf],
    dir: &Path,
) -> Result<Vec<(String, PathBuf)>> {
    std::fs::create_dir_all(dir)?;
    let mut out = Vec::new();
    for soname in sonames {
        let syms = stub_imports(soname, importers)?;
        let mut c = String::from(
            "/* Generated by `xtask torch-pack`: a stand-in for a library libtorch links but\n              * OCR never calls. mokuro-bunko, MPL-2.0. */\n\
             #include <stdio.h>\n#include <stdlib.h>\n\
             static void bunko_stub_called(const char *f) {\n\
             \tfprintf(stderr, \"mokuro-bunko: %s (",
        );
        c.push_str(soname);
        c.push_str(
            ") was called, but this OCR backend pack carries a stub for it. Please report this.\\n\", f);\n\
             \tabort();\n}\n",
        );
        let mut by_version: BTreeMap<Option<String>, Vec<String>> = BTreeMap::new();
        for (name, ver) in &syms {
            c.push_str(&format!(
                "void {name}(void) {{ bunko_stub_called(\"{name}\"); }}\n"
            ));
            by_version
                .entry(ver.clone())
                .or_default()
                .push(name.clone());
        }
        let mut map = String::new();
        for (ver, names) in &by_version {
            let ver = ver.as_deref().unwrap_or("BUNKO_STUB");
            map.push_str(&format!("{ver} {{\n  global:\n"));
            for n in names {
                map.push_str(&format!("    {n};\n"));
            }
            map.push_str("  local: *;\n};\n");
        }
        if by_version.is_empty() {
            map.push_str("BUNKO_STUB {\n  local: *;\n};\n");
        }
        let src = dir.join(format!("{soname}.c"));
        let vmap = dir.join(format!("{soname}.map"));
        let lib = dir.join(soname);
        std::fs::write(&src, c)?;
        std::fs::write(&vmap, map)?;
        let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
        let mut cmd = Command::new(&cc);
        cmd.args(["-shared", "-fPIC", "-O2", "-o"])
            .arg(&lib)
            .arg(&src)
            .arg(format!("-Wl,-soname,{soname}"))
            .arg(format!("-Wl,--version-script,{}", vmap.display()))
            .arg("-Wl,-z,noexecstack");
        util::run(&mut cmd)?;
        eprintln!("    {soname}: {} stubbed function(s)", syms.len());
        out.push((soname.to_string(), lib));
    }
    Ok(out)
}

/// The cdylib's file name on the target (as `std::env::consts` would say there).
fn crate_library_name(target: &str) -> String {
    match os_arch(target).0 {
        "windows" => "bunko_torch.dll".into(),
        "macos" => "libbunko_torch.dylib".into(),
        _ => "libbunko_torch.so".into(),
    }
}

fn base(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

pub fn human(n: u64) -> String {
    if n >= 1 << 30 {
        format!("{:.2} GiB", n as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.1} MiB", n as f64 / (1u64 << 20) as f64)
    }
}

/// Download `up` into the cache (resuming; `curl` is on every CI runner), check it.
pub fn fetch(up: &Upstream, cache: &Path) -> Result<PathBuf> {
    fetch_url(up.url, up.sha256, up.size, cache)
}

/// [`fetch`] for any pinned URL.
pub fn fetch_url(url: &str, sha256: &str, size: u64, cache: &Path) -> Result<PathBuf> {
    let name = url.rsplit('/').next().unwrap_or(url).replace("%2B", "+");
    std::fs::create_dir_all(cache)?;
    let dest = cache.join(&name);
    if dest.is_file() {
        let (sha, n) = util::sha256_file(&dest)?;
        if n == size && sha == sha256 {
            return Ok(dest);
        }
        eprintln!(
            "    cached {} does not match its pin; downloading again",
            dest.display()
        );
        std::fs::remove_file(&dest)?;
    }
    let part = cache.join(format!("{name}.part"));
    eprintln!("    downloading {url} ({})", human(size));
    let status = Command::new("curl")
        .args([
            "-fL",
            "--retry",
            "5",
            "--retry-delay",
            "5",
            "-C",
            "-",
            "-sS",
            "-o",
        ])
        .arg(&part)
        .arg(url)
        .status()
        .context("running curl")?;
    if !status.success() {
        bail!("downloading {url} failed: {status}");
    }
    let (sha, n) = util::sha256_file(&part)?;
    if n != size || sha != sha256 {
        let _ = std::fs::remove_file(&part);
        bail!("{url}: got {n} bytes sha256 {sha}, pinned {size} bytes {sha256}");
    }
    std::fs::rename(&part, &dest)?;
    Ok(dest)
}

/// The default download cache.
pub fn default_cache(root: &Path) -> PathBuf {
    match std::env::var_os("BUNKO_PACK_CACHE") {
        Some(c) => PathBuf::from(c),
        None => util::target_dir(root).join("torch-pack-cache"),
    }
}

/// Unpack a pack archive (whole or its parts) into `<dest>/<pack name>/`, fetching
/// and unpacking its external libraries too: a complete, installed pack (the Docker
/// images bake one in). Returns the pack directory.
pub fn install_complete(parts: &[PathBuf], dest: &Path, cache: &Path) -> Result<PathBuf> {
    let staging = dest.join(".staging");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    let m = bunko_update::backend::unpack_archive(parts, None, &staging)
        .map_err(|e| anyhow::anyhow!("{}: {e}", parts[0].display()))?;
    for e in &m.external {
        let wheel = fetch_url(&e.url, &e.sha256, e.size, cache)?;
        bunko_update::backend::unpack_external(&wheel, e, &staging)
            .map_err(|err| anyhow::anyhow!("{}: {err}", e.name))?;
    }
    let dir = bunko_update::backend::activate(&staging, dest, &m.name)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(dir)
}

/// Whether a `lib/`-relative path is one the spec keeps.
fn keep_lib(spec: &Spec, rel: &str, keep_all: bool, archs: &[String]) -> bool {
    if let Some((dir, rest)) = rel.split_once('/') {
        if !spec.arch_dirs.contains(&dir) {
            return keep_all && !dir.starts_with("libshm");
        }
        return match torch_specs::file_arch(rest) {
            None => true,
            Some(a) => keep_all || archs.iter().any(|k| arch_matches(k, a)),
        };
    }
    let is_lib = rel.contains(".so") || rel.ends_with(".dll") || rel.ends_with(".dylib");
    if keep_all {
        return is_lib;
    }
    spec.keep.iter().any(|k| match k.strip_suffix('*') {
        Some(prefix) => rel.starts_with(prefix),
        None => rel == *k,
    })
}

/// A Windows import library (`lib/torch_cpu.lib`) of a kept DLL: unpacked for linking
/// libbunko_torch, never put in the pack.
fn link_only(spec: &Spec, rel: &str) -> bool {
    !rel.contains('/')
        && rel
            .strip_suffix(".lib")
            .is_some_and(|stem| keep_lib(spec, &format!("{stem}.dll"), false, &[]))
}

/// `gfx1201` keeps `gfx1201` files and family files such as aotriton's `gfx120x`.
fn arch_matches(keep: &str, file: &str) -> bool {
    if keep == file {
        return true;
    }
    let family = file.trim_end_matches('x');
    family.len() < file.len() && family.len() > 3 && keep.starts_with(family)
}

/// Unpack what the pack and the cdylib build need into `dest` (`lib/` = the kept
/// files, `include/`, `build-version`, `build-hash`). Returns the kept `lib/` files
/// and the build hash. Re-uses a previous unpack of the same source and selection.
fn unpack_libtorch(
    source: &Path,
    layout: Layout,
    spec: &Spec,
    keep_all: bool,
    archs: &[String],
    dest: &Path,
) -> Result<(Vec<String>, String)> {
    let stamp_text = format!(
        "{}\n{keep_all}\n{}\n{}\nimplib\n",
        source.display(),
        archs.join(","),
        spec.keep.join(",")
    );
    let stamp = dest.join(".xtask-stamp");
    let fresh = std::fs::read_to_string(&stamp).ok().as_deref() == Some(stamp_text.as_str());
    if !fresh {
        if dest.exists() {
            std::fs::remove_dir_all(dest)?;
        }
        std::fs::create_dir_all(dest)?;
        if source.is_dir() {
            copy_tree_filtered(source, dest, spec, keep_all, archs)?;
        } else {
            eprintln!("    unpacking {}", source.display());
            let mut zip = zip::ZipArchive::new(std::fs::File::open(source)?)?;
            let prefix = layout.prefix();
            for i in 0..zip.len() {
                let mut f = zip.by_index(i)?;
                if f.is_dir() {
                    continue;
                }
                let Some(rel) = f.name().strip_prefix(prefix).map(str::to_string) else {
                    continue;
                };
                let want = if let Some(l) = rel.strip_prefix("lib/") {
                    keep_lib(spec, l, keep_all, archs) || link_only(spec, l)
                } else {
                    rel.starts_with("include/")
                        || matches!(rel.as_str(), "build-version" | "build-hash" | "version.py")
                };
                if !want || rel.contains("..") {
                    continue;
                }
                let out = dest.join(&rel);
                std::fs::create_dir_all(out.parent().context("parent")?)?;
                let mut w = std::io::BufWriter::new(std::fs::File::create(&out)?);
                std::io::copy(&mut f, &mut w)?;
                w.flush()?;
            }
        }
        if layout == Layout::Wheel && !dest.join("build-version").exists() {
            // torch-sys's version check reads build-version; a wheel has version.py.
            if let Ok(py) = std::fs::read_to_string(dest.join("version.py"))
                && let Some(v) = py
                    .lines()
                    .find(|l| l.starts_with("__version__"))
                    .and_then(|l| l.split(['\'', '"']).nth(1))
            {
                std::fs::write(dest.join("build-version"), format!("{v}\n"))?;
            }
        }
        std::fs::write(&stamp, &stamp_text)?;
    }
    let version = std::fs::read_to_string(dest.join("build-version")).unwrap_or_default();
    if !version.trim().starts_with(torch_specs::TORCH_VERSION) {
        bail!(
            "{} is libtorch {:?}, the packs need {}",
            source.display(),
            version.trim(),
            torch_specs::TORCH_VERSION
        );
    }
    let build = std::fs::read_to_string(dest.join("build-hash"))
        .unwrap_or_default()
        .trim()
        .to_string();
    let lib = dest.join("lib");
    let files: Vec<String> = archive::list_files(&lib)?
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|f| !link_only(spec, f))
        .collect();
    if !keep_all {
        for k in spec.keep {
            let hit = match k.strip_suffix('*') {
                Some(prefix) => files.iter().any(|f| f.starts_with(prefix)),
                None => files.iter().any(|f| f == k),
            };
            if !hit {
                bail!(
                    "{k} (in the {} keep list) is not in {}",
                    spec.variant,
                    source.display()
                );
            }
        }
    }
    Ok((files, build))
}

fn copy_tree_filtered(
    src: &Path,
    dest: &Path,
    spec: &Spec,
    keep_all: bool,
    archs: &[String],
) -> Result<()> {
    for rel in archive::list_files(src)? {
        let r = rel.to_string_lossy().replace('\\', "/");
        let want = if let Some(l) = r.strip_prefix("lib/") {
            keep_lib(spec, l, keep_all, archs) || link_only(spec, l)
        } else {
            r.starts_with("include/")
                || matches!(r.as_str(), "build-version" | "build-hash" | "version.py")
        };
        if want {
            let d = dest.join(&rel);
            std::fs::create_dir_all(d.parent().context("parent")?)?;
            link_or_copy(&src.join(&rel), &d)?;
        }
    }
    Ok(())
}

fn link_or_copy(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_file(dest)?;
    }
    if std::fs::hard_link(src, dest).is_err() {
        std::fs::copy(src, dest).with_context(|| format!("copying {}", src.display()))?;
    }
    Ok(())
}

/// Unpack a wheel's libraries and licence files under `ext_dir` (`lib/<name>`,
/// `licenses/<wheel>/<name>`) and describe them as an [`ExternalArchive`]. Returns the
/// pack-relative paths too, for `--bundle-external`.
fn unpack_wheel(
    wheel: &Path,
    w: &Wheel,
    ext_dir: &Path,
) -> Result<(ExternalArchive, Vec<(String, PathBuf)>)> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(wheel)?)?;
    let mut files = Vec::new();
    let mut out = Vec::new();
    let members = w
        .libs
        .iter()
        .map(|m| (*m, format!("lib/{}", base(m))))
        .chain(
            w.license_files
                .iter()
                .map(|m| (*m, format!("licenses/{}/{}", w.name, base(m)))),
        );
    for (member, rel) in members {
        let dest = ext_dir.join(&rel);
        std::fs::create_dir_all(dest.parent().context("parent")?)?;
        {
            let mut f = zip
                .by_name(member)
                .with_context(|| format!("{member} is not in {}", wheel.display()))?;
            let mut o = std::io::BufWriter::new(std::fs::File::create(&dest)?);
            std::io::copy(&mut f, &mut o)?;
            o.flush()?;
        }
        let (sha256, size) = util::sha256_file(&dest)?;
        files.push(ExternalFile {
            from: member.to_string(),
            path: rel.clone(),
            sha256,
            size,
        });
        out.push((rel, dest));
    }
    Ok((
        ExternalArchive {
            name: w.name.into(),
            version: w.version.into(),
            url: w.upstream.url.into(),
            sha256: w.upstream.sha256.into(),
            size: w.upstream.size,
            license: w.license.into(),
            files,
        },
        out,
    ))
}

/// PyTorch's licence files from a torch wheel's dist-info, under `licenses/pytorch/`.
fn torch_licenses(wheel: &Path, dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(wheel)?)?;
    let mut out = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        let name = f.name().to_string();
        let Some((_, rest)) = name.split_once(".dist-info/licenses/") else {
            continue;
        };
        if f.is_dir() || rest.contains("..") || rest.is_empty() {
            continue;
        }
        let rel = format!("licenses/pytorch/{rest}");
        let dest = dir.join(format!("pytorch/{rest}"));
        std::fs::create_dir_all(dest.parent().context("parent")?)?;
        let mut o = std::fs::File::create(&dest)?;
        std::io::copy(&mut f, &mut o)?;
        out.push((rel, dest));
    }
    if out.is_empty() {
        bail!("{} has no dist-info licences", wheel.display());
    }
    Ok(out)
}

fn licenses_readme(spec: &Spec, build: &str, bundled: bool) -> String {
    let mut s = format!(
        "# Third-party licences of this OCR backend pack\n\n\
         - `pytorch/`: libtorch {} (PyTorch commit {build}), BSD-3-Clause, with the licences \
         of the libraries it bundles (`pytorch/third_party/`). From {}.\n",
        torch_specs::TORCH_VERSION,
        spec.libtorch.url
    );
    if !spec.wheels.is_empty() {
        s.push_str(&format!(
            "- NVIDIA CUDA libraries, {}: each wheel's licence text is in `<wheel name>/`.\n",
            if bundled {
                "bundled from NVIDIA's PyPI wheels"
            } else {
                "downloaded by `mokuro-bunko install-ocr` from NVIDIA's PyPI wheels (pinned by sha256 in pack.json)"
            }
        ));
        for w in spec.wheels {
            s.push_str(&format!("  - {} {} ({})\n", w.name, w.version, w.license));
        }
    }
    if !spec.arch_dirs.is_empty() {
        s.push_str(
            "- AMD ROCm libraries bundled in the libtorch ROCm build (HIP, HSA runtime, comgr, \
             rocBLAS, hipBLAS(Lt), MIOpen, RCCL, rocSOLVER, rocSPARSE, rocRAND, rocFFT, aotriton, \
             MAGMA, rocprofiler, rocm-smi, libdrm): ROCm 7.1.1. The libtorch zip ships no \
             licence texts for them; they are in `rocm/` (from the upstream sources, listed \
             with their URLs in `rocm/SOURCES.md`).\n",
        );
    }
    if !spec.stubs.is_empty() {
        s.push_str(&format!(
            "- `lib/{}` are stubs generated by mokuro-bunko (MPL-2.0), not NVIDIA's \
             libraries: libtorch links them but OCR never calls them, and any call aborts \
             with a message.\n",
            spec.stubs.join("`, `lib/")
        ));
    }
    if !spec.system_libs.is_empty() {
        s.push_str(&format!(
            "- Taken from the host system, not shipped: {}.\n",
            spec.system_libs.join(", ")
        ));
    }
    s
}

/// Build libbunko_torch for `target` against the unpacked libtorch.
fn build_cdylib(
    root: &Path,
    target: &str,
    spec: &Spec,
    libtorch: &Path,
    ext_dir: &Path,
    locked: bool,
) -> Result<PathBuf> {
    if !root.join("crates/bunko-torch/Cargo.toml").is_file() {
        bail!("crates/bunko-torch does not exist yet (pass --cdylib <file> or --no-cdylib)");
    }
    let mut cmd = util::cargo();
    cmd.current_dir(root)
        .args([
            "build",
            "--release",
            "-p",
            "bunko-torch",
            "--features",
            "libtorch",
            "--lib",
            "--target",
            target,
            "--message-format=json-render-diagnostics",
        ])
        // One target directory per variant: each links a different libtorch.
        .env(
            "CARGO_TARGET_DIR",
            util::target_dir(root).join(format!("torch-pack-{}", spec.variant)),
        )
        .env("LIBTORCH", libtorch)
        .env_remove("LIBTORCH_USE_PYTORCH")
        .env_remove("LIBTORCH_LIB")
        .env_remove("LIBTORCH_INCLUDE");
    if locked {
        cmd.arg("--locked");
    }
    if target.contains("-linux-") && ext_dir.join("lib").is_dir() {
        // Let the linker see the CUDA libraries libtorch_cuda.so needs (not linked).
        let var = format!(
            "CARGO_TARGET_{}_RUSTFLAGS",
            target.to_uppercase().replace(['-', '.'], "_")
        );
        let mut flags = std::env::var(&var).unwrap_or_default();
        flags.push_str(&format!(
            " -C link-arg=-Wl,-rpath-link,{}",
            ext_dir.join("lib").display()
        ));
        cmd.env(var, flags.trim());
    }
    cmd.stdout(Stdio::piped());
    eprintln!("+ LIBTORCH={} {cmd:?}", libtorch.display());
    let mut child = cmd.spawn().context("starting cargo")?;
    let stdout = child.stdout.take().context("cargo stdout")?;
    let mut lib = None;
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line?;
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if msg["reason"] == "compiler-artifact" && msg["target"]["name"] == "bunko_torch" {
            for f in msg["filenames"].as_array().into_iter().flatten() {
                if let Some(f) = f.as_str()
                    && (f.ends_with(".so") || f.ends_with(".dll") || f.ends_with(".dylib"))
                {
                    lib = Some(PathBuf::from(f));
                }
            }
        }
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("building bunko-torch failed: {status}");
    }
    lib.context("cargo built no libbunko_torch")
}

/// `BT_ABI_VERSION` from bunko-torch's source (the cdylib cannot be loaded here: a GPU
/// pack needs the driver).
fn abi_version(root: &Path) -> Result<u32> {
    let src = std::fs::read_to_string(root.join("crates/bunko-torch/src/abi.rs"))
        .context("reading crates/bunko-torch/src/abi.rs")?;
    src.lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("pub const BT_ABI_VERSION: u32 =")
                .map(|v| v.trim().trim_end_matches(';').trim().parse::<u32>())
        })
        .context("no BT_ABI_VERSION in bunko-torch")?
        .context("BT_ABI_VERSION is not a number")
}

/// The shared libraries of a compiled model package (a `.pt2` zip, an unpacked `.pt2`
/// directory or a directory of them), unpacked from zips into `scratch`.
fn package_libraries(path: &Path, scratch: &Path) -> Result<Vec<PathBuf>> {
    let is_lib = |n: &str| {
        n.ends_with(".so") || n.ends_with(".dll") || n.ends_with(".pyd") || n.ends_with(".dylib")
    };
    let mut out = Vec::new();
    if path.is_dir() {
        for rel in archive::list_files(path)? {
            let full = path.join(&rel);
            let name = rel.to_string_lossy().to_string();
            if is_lib(&name) {
                out.push(full);
            } else if name.ends_with(".pt2") {
                out.extend(package_libraries(
                    &full,
                    &scratch.join(out.len().to_string()),
                )?);
            }
        }
        return Ok(out);
    }
    let mut zip = zip::ZipArchive::new(
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?,
    )?;
    std::fs::create_dir_all(scratch)?;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        let name = f.name().to_string();
        if f.is_dir() || !is_lib(&name) {
            continue;
        }
        let dest = scratch.join(format!("{i}-{}", base(&name)));
        let mut o = std::fs::File::create(&dest)?;
        std::io::copy(&mut f, &mut o)?;
        out.push(dest);
    }
    Ok(out)
}

/// For each staged `lib/` ELF whose SONAME differs from its file name and that some
/// staged binary needs by that SONAME, a link `lib/<SONAME>` -> file (the dynamic loader
/// looks for the needed name on disk when the library is not loaded yet).
fn soname_links(staged: &Staged) -> Result<Vec<PackLink>> {
    let mut sonames: BTreeMap<String, String> = BTreeMap::new();
    let mut needed: BTreeSet<String> = BTreeSet::new();
    for (rel, src) in staged {
        let is_lib = rel.contains(".so");
        if !is_lib {
            continue;
        }
        let bytes = std::fs::read(src)?;
        if let Ok(goblin::Object::Elf(elf)) = goblin::Object::parse(&bytes) {
            if let (Some(so), Some(file)) = (elf.soname, rel.strip_prefix("lib/"))
                && so != file
                && !file.contains('/')
            {
                sonames.insert(so.to_string(), file.to_string());
            }
            needed.extend(elf.libraries.iter().map(|s| s.to_string()));
        }
    }
    let _ = &needed;
    Ok(sonames
        .into_iter()
        .filter(|(so, _)| !staged.contains_key(&format!("lib/{so}")))
        .map(|(so, file)| PackLink {
            path: format!("lib/{so}"),
            target: file,
        })
        .collect())
}

/// Every dynamic library a staged binary needs must be another staged file (by file
/// name or soname), one of the spec's system libraries, or part of the platform.
/// Returns the problems found.
/// NEEDED / imported names resolve by **file name** inside the pack (a file or a SONAME
/// link): the dynamic loader opens a NEEDED name as a file when no loaded library has
/// that name yet, so a library that merely carries the SONAME under another file name
/// does not count (that is how a pack worked on hosts with /opt/rocm and failed on
/// hosts without it). Names resolving only through the host's system paths must be in
/// the spec's `system_libs`, or the check fails.
pub fn closure_check(
    files: &[PathBuf],
    link_names: &[String],
    spec: &Spec,
    target: &str,
) -> Result<Vec<String>> {
    let os = os_arch(target).0;
    let mut have: BTreeSet<String> = link_names.iter().map(|n| n.to_ascii_lowercase()).collect();
    let mut needs: Vec<(String, Vec<String>)> = Vec::new();
    for f in files {
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let is_bin = name.contains(".so")
            || name.ends_with(".dll")
            || name.ends_with(".pyd")
            || name.ends_with(".dylib");
        if !is_bin {
            continue;
        }
        have.insert(name.to_ascii_lowercase());
        let mut bytes = Vec::new();
        std::fs::File::open(f)?.read_to_end(&mut bytes)?;
        let deps = match goblin::Object::parse(&bytes) {
            Ok(goblin::Object::Elf(elf)) => elf.libraries.iter().map(|s| s.to_string()).collect(),
            Ok(goblin::Object::PE(pe)) => pe.libraries.iter().map(|s| s.to_string()).collect(),
            // goblin lists the image itself ("self", and for a dylib its own install
            // name from LC_ID_DYLIB) among `libs`: that is not a dependency, and a
            // sample package's libraries are unpacked under other file names.
            Ok(goblin::Object::Mach(goblin::mach::Mach::Binary(m))) => m
                .libs
                .iter()
                .filter(|l| **l != "self" && Some(**l) != m.name)
                .map(|s| s.to_string())
                .collect(),
            // Kernel data (.hsaco, .co) that happens to match the name test.
            _ => continue,
        };
        needs.push((name, deps));
    }
    let system: BTreeSet<String> = torch_specs::LINUX_BASE_LIBS
        .iter()
        .chain(spec.system_libs)
        .map(|s| s.to_ascii_lowercase())
        .collect();
    let mut problems = Vec::new();
    for (name, deps) in needs {
        for d in deps {
            let base = d.rsplit('/').next().unwrap_or(&d).to_ascii_lowercase();
            let ok = have.contains(&base)
                || system.contains(&base)
                || match os {
                    "windows" => windows_system_dll(&base),
                    "macos" => {
                        d.starts_with("/usr/lib/") || d.starts_with("/System/") || d == "self"
                    }
                    _ => false,
                };
            if !ok {
                problems.push(format!("{name} needs {d}"));
            }
        }
    }
    problems.sort();
    problems.dedup();
    Ok(problems)
}

/// DLLs every Windows 10+ host has (the OS, the UCRT, the VC++ 2015-2022 runtime that
/// the installer checks for).
fn windows_system_dll(name: &str) -> bool {
    name.starts_with("api-ms-win-")
        || name.starts_with("ext-ms-")
        || [
            "kernel32.dll",
            "user32.dll",
            "advapi32.dll",
            "shell32.dll",
            "ole32.dll",
            "oleaut32.dll",
            "ws2_32.dll",
            "bcrypt.dll",
            "bcryptprimitives.dll",
            "ntdll.dll",
            "psapi.dll",
            "dbghelp.dll",
            "imagehlp.dll",
            "wintrust.dll",
            "shlwapi.dll",
            "iphlpapi.dll",
            "userenv.dll",
            "crypt32.dll",
            "secur32.dll",
            "version.dll",
            "setupapi.dll",
            "cfgmgr32.dll",
            "ucrtbase.dll",
            "vcruntime140.dll",
            "vcruntime140_1.dll",
            "msvcp140.dll",
            "msvcp140_1.dll",
            "msvcp140_2.dll",
            "vcomp140.dll",
            "concrt140.dll",
            "nvcuda.dll",
        ]
        .contains(&name)
}

/// tar (pack.json first, then the files sorted, all under `top/`), zstd-compressed.
pub fn write_tar_zst(
    json: &[u8],
    staged: &Staged,
    links: &[PackLink],
    top: &str,
    out: &Path,
    level: i32,
    mtime: u64,
) -> Result<()> {
    let file = std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut enc = zstd::stream::write::Encoder::new(std::io::BufWriter::new(file), level)?;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()) as u32;
    enc.multithread(threads)?;
    // A 128 MiB window finds the redundancy across the big libraries and stays
    // inside what every zstd decoder accepts by default (2^27).
    enc.window_log(27)?;
    enc.long_distance_matching(true)?;
    enc.include_checksum(true)?;
    let mut tar = tar::Builder::new(enc);
    tar.mode(tar::HeaderMode::Deterministic);
    let header = |size: u64, mode: u32| {
        let mut h = tar::Header::new_gnu();
        h.set_size(size);
        h.set_mode(mode);
        h.set_mtime(mtime);
        h.set_cksum();
        h
    };
    let mut h = header(json.len() as u64, 0o644);
    tar.append_data(&mut h, format!("{top}/{PACK_JSON}"), json)?;
    for (rel, src) in staged {
        let size = std::fs::metadata(src)?.len();
        let mode = if archive::is_executable(Path::new(rel)) {
            0o755
        } else {
            0o644
        };
        let mut h = header(size, mode);
        tar.append_data(
            &mut h,
            format!("{top}/{rel}"),
            std::io::BufReader::new(std::fs::File::open(src)?),
        )?;
    }
    // The SONAME links as symlinks too, so a plain `tar -x` gives a loadable pack
    // (install-ocr also creates them from pack.json).
    for l in links {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        h.set_mtime(mtime);
        tar.append_link(&mut h, format!("{top}/{}", l.path), &l.target)?;
    }
    let enc = tar.into_inner()?;
    enc.finish()?.flush()?;
    Ok(())
}

/// Split `archive` into `<archive>.001`, `.002`, ... when it is larger than `max`;
/// the parts replace it. Returns the files to publish, in order.
pub fn split(archive: &Path, max: u64) -> Result<Vec<PathBuf>> {
    let size = std::fs::metadata(archive)?.len();
    if size <= max {
        return Ok(vec![archive.to_path_buf()]);
    }
    let mut input = std::io::BufReader::new(std::fs::File::open(archive)?);
    let mut parts = Vec::new();
    let mut i = 1;
    loop {
        let part = PathBuf::from(format!("{}.{i:03}", archive.display()));
        let mut out = std::io::BufWriter::new(std::fs::File::create(&part)?);
        let n = std::io::copy(&mut (&mut input).take(max), &mut out)?;
        out.flush()?;
        if n == 0 {
            std::fs::remove_file(&part)?;
            break;
        }
        parts.push(part);
        i += 1;
    }
    std::fs::remove_file(archive)?;
    Ok(parts)
}

/// Read `pack.json` (the first entry) from a pack archive or its first part.
pub fn read_pack_json(first_part: &Path) -> Result<PackManifest> {
    let dec = zstd::stream::read::Decoder::new(std::fs::File::open(first_part)?)?;
    let mut tar = tar::Archive::new(dec);
    let mut entry = tar.entries()?.next().context("empty pack archive")??;
    let path = entry.path()?.to_string_lossy().to_string();
    if !path.ends_with(&format!("/{PACK_JSON}")) {
        bail!(
            "{}: the first entry is {path}, not pack.json",
            first_part.display()
        );
    }
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes)?;
    PackManifest::parse(&bytes).map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal arm64 Mach-O dylib: LC_ID_DYLIB `id`, then one LC_LOAD_DYLIB per dep.
    fn macho_dylib(id: &str, deps: &[&str]) -> Vec<u8> {
        fn dylib_cmd(cmd: u32, name: &str) -> Vec<u8> {
            let mut n = name.as_bytes().to_vec();
            n.push(0);
            while !(24 + n.len()).is_multiple_of(8) {
                n.push(0);
            }
            let size = (24 + n.len()) as u32;
            let mut c = Vec::new();
            for v in [cmd, size, 24, 2, 0x10000, 0x10000] {
                c.extend_from_slice(&v.to_le_bytes());
            }
            c.extend(n);
            c
        }
        let mut cmds = dylib_cmd(0xd, id); // LC_ID_DYLIB
        for d in deps {
            cmds.extend(dylib_cmd(0xc, d)); // LC_LOAD_DYLIB
        }
        let mut out = Vec::new();
        // mach_header_64: magic, cputype arm64, subtype, MH_DYLIB, ncmds, sizeofcmds,
        // flags, reserved
        for v in [
            0xfeed_facf_u32,
            0x0100_000c,
            0,
            6,
            1 + deps.len() as u32,
            cmds.len() as u32,
            0,
            0,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend(cmds);
        out
    }

    #[test]
    fn macho_own_install_name_is_not_a_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = torch_specs::find("cpu", "aarch64-apple-darwin").unwrap();
        // As package_libraries unpacks it: renamed, so its install name's basename
        // matches no file.
        let model = tmp.path().join("3-model.wrapper.so");
        std::fs::write(
            &model,
            macho_dylib(
                "@rpath/model.wrapper.so",
                &["@rpath/libtorch_cpu.dylib", "/usr/lib/libSystem.B.dylib"],
            ),
        )
        .unwrap();
        let torch = tmp.path().join("libtorch_cpu.dylib");
        std::fs::write(&torch, macho_dylib("@rpath/libtorch_cpu.dylib", &[])).unwrap();
        let problems =
            closure_check(&[model.clone(), torch], &[], spec, "aarch64-apple-darwin").unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        // A real missing dependency is still reported.
        let problems = closure_check(&[model], &[], spec, "aarch64-apple-darwin").unwrap();
        assert_eq!(
            problems,
            ["3-model.wrapper.so needs @rpath/libtorch_cpu.dylib"]
        );
    }

    #[test]
    fn pack_names_roundtrip() {
        let stem = archive_stem("v0.7.0", "x86_64-unknown-linux-gnu", "rocm7.1");
        assert_eq!(stem, "mokuro-bunko-backend-0.7.0-linux-x64-rocm7.1");
        for (t, v, name) in [
            (
                "x86_64-pc-windows-msvc",
                "cu130",
                "mokuro-bunko-backend-0.7.0-beta.3-windows-cu130",
            ),
            (
                "aarch64-apple-darwin",
                "cpu",
                "mokuro-bunko-backend-0.7.0-beta.3-macos-cpu",
            ),
        ] {
            assert_eq!(archive_stem("0.7.0-beta.3", t, v), name);
            assert_eq!(
                parse_pack_name(&format!("{name}.tar.zst.001"), "0.7.0-beta.3"),
                Some((t, v, 1))
            );
        }
        assert_eq!(
            parse_pack_name(
                "mokuro-bunko-0.7.0-x86_64-unknown-linux-gnu-torch-cpu.tar.zst",
                "0.7.0"
            ),
            None
        );
        assert_eq!(
            parse_pack_name(&format!("{stem}.tar.zst"), "0.7.0"),
            Some(("x86_64-unknown-linux-gnu", "rocm7.1", 0))
        );
        assert_eq!(
            parse_pack_name(&format!("{stem}.tar.zst.002"), "0.7.0"),
            Some(("x86_64-unknown-linux-gnu", "rocm7.1", 2))
        );
        assert_eq!(
            parse_pack_name(&format!("{stem}.tar.zst.sha256"), "0.7.0"),
            None
        );
        assert_eq!(parse_pack_name(&format!("{stem}.tar.zst"), "0.7.1"), None);
        assert_eq!(
            parse_pack_name(
                "mokuro-bunko-0.7.0-x86_64-unknown-linux-gnu-full.tar.gz",
                "0.7.0"
            ),
            None
        );
    }

    #[test]
    fn keep_rules() {
        let cu = torch_specs::find("cu130", "x86_64-unknown-linux-gnu").unwrap();
        let none: Vec<String> = vec![];
        assert!(keep_lib(cu, "libtorch_cuda.so", false, &none));
        assert!(!keep_lib(cu, "libtorch_cuda_linalg.so", false, &none));
        assert!(!keep_lib(cu, "libshm/x", true, &none));
        let rocm = torch_specs::find("rocm7.1", "x86_64-unknown-linux-gnu").unwrap();
        let archs = vec!["gfx1201".to_string(), "gfx1030".to_string()];
        assert!(keep_lib(
            rocm,
            "rocblas/library/Kernels.so-000-gfx1201.hsaco",
            false,
            &archs
        ));
        assert!(!keep_lib(
            rocm,
            "rocblas/library/Kernels.so-000-gfx942.hsaco",
            false,
            &archs
        ));
        assert!(keep_lib(
            rocm,
            "rocblas/library/TensileManifest.txt",
            false,
            &archs
        ));
        assert!(keep_lib(
            rocm,
            "aotriton.images/amd-gfx120x/flash/x.aks2",
            false,
            &archs
        ));
        assert!(!keep_lib(
            rocm,
            "aotriton.images/amd-gfx110x/flash/x.aks2",
            false,
            &archs
        ));
        assert!(!keep_lib(
            rocm,
            "aotriton.images/amd-gfx90a/flash/x.aks2",
            false,
            &archs
        ));
        assert!(keep_lib(
            rocm,
            "aotriton.images/amd-gfx120x/__signature__",
            false,
            &archs
        ));
        assert!(!keep_lib(rocm, "libmagma_unused.so", false, &archs));
    }

    #[test]
    fn archive_split_and_pack_json() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.so");
        // Incompressible, so the archive is big enough to split after pack.json.
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let noise: Vec<u8> = (0..300_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        std::fs::write(&a, noise).unwrap();
        let mut staged = Staged::new();
        staged.insert("lib/a.so".into(), a);
        let m = PackManifest {
            format: PACK_FORMAT,
            name: "torch-cpu-2.13.0".into(),
            variant: "cpu".into(),
            torch: "2.13.0".into(),
            torch_build: String::new(),
            target: "x86_64-unknown-linux-gnu".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            abi: 1,
            library: String::new(),
            lib_dir: "lib".into(),
            bunko_version: "0.7.0".into(),
            requires: Requires::default(),
            files: vec![],
            external: vec![],
            links: vec![],
        };
        let json = serde_json::to_vec(&m).unwrap();
        let out = tmp.path().join("p.tar.zst");
        let links = vec![PackLink {
            path: "lib/a.so.1".into(),
            target: "a.so".into(),
        }];
        write_tar_zst(&json, &staged, &links, "torch-cpu-2.13.0", &out, 3, 0).unwrap();
        assert_eq!(read_pack_json(&out).unwrap().name, "torch-cpu-2.13.0");
        let whole = std::fs::read(&out).unwrap();
        let parts = split(&out, 120_000).unwrap();
        assert!(parts.len() > 1 && !out.exists());
        let joined: Vec<u8> = parts
            .iter()
            .flat_map(|p| std::fs::read(p).unwrap())
            .collect();
        assert_eq!(joined, whole);
        assert_eq!(read_pack_json(&parts[0]).unwrap().variant, "cpu");
    }
}
