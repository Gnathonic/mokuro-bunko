//! `xtask vulkan-pack`: build the Vulkan backend pack (docs/rust-port/VULKAN.md §1): the
//! `bunko-vulkan` cdylib with both runtimes (ncnn statically, ONNX Runtime's WebGPU build
//! statically) and Dawn beside it in `lib/`, licences, `pack.json`, and
//! `mokuro-bunko-backend-<ver>-<platform>-vulkan.tar.zst`.
//!
//! ncnn comes from `--ncnn-dir` (or `BUNKO_NCNN_DIR`): the install prefix
//! `packaging/vulkan/build-ncnn.sh` makes (pinned tag, static, Vulkan + glslang).

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use bunko_update::backend::{PACK_FORMAT, PACK_JSON, PackFile, PackManifest, Requires};

use crate::torch_pack::{abi_version, archive_stem, human, link_or_copy, split, write_tar_zst};
use crate::util;

/// The ncnn tag `packaging/vulkan/build-ncnn.sh` builds.
const NCNN_TAG: &str = "20260526";
const VARIANT: &str = "vulkan";

#[derive(Debug, clap::Args)]
pub struct VulkanPackArgs {
    /// ncnn install prefix (default: $BUNKO_NCNN_DIR).
    #[arg(long)]
    pub ncnn_dir: Option<PathBuf>,
    /// Rust target triple (default: the host).
    #[arg(long)]
    pub target: Option<String>,
    #[arg(long, default_value = "dist")]
    pub out: PathBuf,
    /// Write the unpacked pack to `<out>/<name>/`, no archive.
    #[arg(long)]
    pub no_archive: bool,
    #[arg(long, default_value_t = 19)]
    pub level: i32,
    #[arg(long, default_value_t = 1900 * 1024 * 1024)]
    pub max_part: u64,
    #[arg(long)]
    pub locked: bool,
    /// Without ncnn (the WebGPU runtime alone): a platform whose ncnn build is not ready.
    #[arg(long)]
    pub no_ncnn: bool,
    /// Use this prebuilt bunko_vulkan library (a cross build: `cargo xwin build -p
    /// bunko-vulkan --features ncnn,webgpu`); Dawn's libraries are taken from beside it.
    #[arg(long)]
    pub cdylib: Option<PathBuf>,
}

fn build(
    root: &Path,
    target: &str,
    ncnn: Option<&Path>,
    locked: bool,
) -> Result<(PathBuf, PathBuf)> {
    let tdir = util::target_dir(root).join("vulkan-pack");
    let mut cmd = util::cargo();
    cmd.current_dir(root)
        .args([
            "build",
            "--release",
            "-p",
            "bunko-vulkan",
            "--features",
            if ncnn.is_some() {
                "ncnn,webgpu"
            } else {
                "webgpu"
            },
            "--lib",
            "--target",
            target,
            "--message-format=json-render-diagnostics",
        ])
        // Its own target directory: the `webgpu` ORT build must never be unified into
        // the binary's.
        .env("CARGO_TARGET_DIR", &tdir)
        .stdout(Stdio::piped());
    match ncnn {
        Some(n) => cmd.env("BUNKO_NCNN_DIR", n),
        None => cmd.env_remove("BUNKO_NCNN_DIR"),
    };
    if locked {
        cmd.arg("--locked");
    }
    eprintln!("+ {cmd:?}");
    let mut child = cmd.spawn().context("starting cargo")?;
    let stdout = child.stdout.take().context("cargo stdout")?;
    let mut lib = None;
    for line in std::io::BufReader::new(stdout).lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line?) else {
            continue;
        };
        if msg["reason"] == "compiler-artifact" && msg["target"]["name"] == "bunko_vulkan" {
            for f in msg["filenames"].as_array().into_iter().flatten() {
                if let Some(f) = f.as_str()
                    && (f.ends_with(".so") || f.ends_with(".dll") || f.ends_with(".dylib"))
                {
                    lib = Some(PathBuf::from(f));
                }
            }
        }
    }
    if !child.wait()?.success() {
        bail!("building bunko-vulkan failed");
    }
    let lib = lib.context("cargo built no bunko_vulkan library")?;
    // `ort`'s copy-dylibs puts Dawn beside the build outputs.
    let dawn_name = if target.contains("windows") {
        "webgpu_dawn.dll"
    } else {
        "libwebgpu_dawn.so"
    };
    let dawn = lib
        .parent()
        .map(|d| d.join(dawn_name))
        .filter(|p| p.exists())
        .with_context(|| format!("no {dawn_name} beside {}", lib.display()))?;
    Ok((lib, std::fs::canonicalize(dawn)?))
}

/// The host's libgomp (ncnn's OpenMP), bundled so a slim system needs none.
fn libgomp() -> Option<PathBuf> {
    let out = std::process::Command::new("gcc")
        .arg("-print-file-name=libgomp.so.1")
        .output()
        .ok()?;
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    p.is_absolute()
        .then(|| std::fs::canonicalize(&p).ok())
        .flatten()
}

pub fn run(args: &VulkanPackArgs) -> Result<PathBuf> {
    let root = util::workspace_root();
    let target = match &args.target {
        Some(t) => t.clone(),
        None => util::host_triple()?,
    };
    let ncnn = if args.no_ncnn || args.cdylib.is_some() {
        None
    } else {
        Some(
            args.ncnn_dir
                .clone()
                .or_else(|| std::env::var_os("BUNKO_NCNN_DIR").map(PathBuf::from))
                .context(
                    "--ncnn-dir or BUNKO_NCNN_DIR (packaging/vulkan/build-ncnn.sh), or --no-ncnn",
                )?,
        )
    };
    let version = util::workspace_version(&root)?;
    let (lib, dawn) = match &args.cdylib {
        Some(lib) => {
            let name = if target.contains("windows") {
                "webgpu_dawn.dll"
            } else {
                "libwebgpu_dawn.so"
            };
            let dawn = lib
                .parent()
                .map(|d| d.join(name))
                .filter(|p| p.exists())
                .with_context(|| format!("no {name} beside {}", lib.display()))?;
            (lib.clone(), std::fs::canonicalize(dawn)?)
        }
        None => build(&root, &target, ncnn.as_deref(), args.locked)?,
    };
    let library = lib
        .file_name()
        .context("name")?
        .to_string_lossy()
        .into_owned();

    let mut staged: BTreeMap<String, PathBuf> = BTreeMap::new();
    staged.insert(library.clone(), lib.clone());
    staged.insert(
        format!(
            "lib/{}",
            dawn.file_name().context("name")?.to_string_lossy()
        ),
        dawn.clone(),
    );
    if target.contains("-linux-")
        && let Some(g) = libgomp()
    {
        staged.insert("lib/libgomp.so.1".into(), g);
    }
    // Dawn's D3D12 backend compiles shaders with DXC (Windows).
    if target.contains("windows") {
        for f in ["dxcompiler.dll", "dxil.dll"] {
            if let Some(p) = dawn.parent().map(|d| d.join(f)).filter(|p| p.exists()) {
                staged.insert(format!("lib/{f}"), std::fs::canonicalize(p)?);
            }
        }
    }
    let lic = root.join("packaging/vulkan/licenses");
    for f in [
        "ncnn-LICENSE.txt",
        "glslang-LICENSE.txt",
        "dawn-LICENSE.txt",
    ] {
        staged.insert(format!("licenses/{f}"), lic.join(f));
    }
    let work = util::target_dir(&root).join("vulkan-pack-stage");
    std::fs::create_dir_all(&work)?;
    std::fs::write(
        work.join("onnxruntime-LICENSE.txt"),
        crate::licenses::ORT_MIT,
    )?;
    staged.insert(
        "licenses/onnxruntime-LICENSE.txt".into(),
        work.join("onnxruntime-LICENSE.txt"),
    );
    let readme = format!(
        "# Vulkan backend pack {version}\n\n\
         Runtimes: ncnn {NCNN_TAG} (BSD-3-Clause, with glslang: BSD-3-Clause/MIT/Apache-2.0 parts), \
         ONNX Runtime (MIT) with its WebGPU execution provider and Dawn (BSD-3-Clause; Dawn \
         bundles Abseil and SPIRV-Tools/Tint parts under Apache-2.0). libgomp (Linux): GCC \
         runtime library, GPL-3.0 with the GCC Runtime Library Exception.\n\n\
         The Rust code of `libbunko_vulkan` is mokuro-bunko's (MPL-2.0) and crates listed in the \
         release's THIRD-PARTY-LICENSES.md, plus `ash` (MIT OR Apache-2.0).\n"
    );
    std::fs::write(work.join("README.md"), readme)?;
    staged.insert("licenses/README.md".into(), work.join("README.md"));

    let mut files = Vec::new();
    for (rel, src) in &staged {
        let (sha256, size) = util::sha256_file(src)?;
        files.push(PackFile {
            path: rel.clone(),
            sha256,
            size,
        });
    }
    let (os, arch) = if target.contains("windows") {
        ("windows", "x86_64")
    } else {
        (
            "linux",
            if target.starts_with("aarch64") {
                "aarch64"
            } else {
                "x86_64"
            },
        )
    };
    let name = format!("{VARIANT}-{}", crate::names::strip_v(&version));
    let manifest = PackManifest {
        format: PACK_FORMAT,
        name: name.clone(),
        variant: VARIANT.into(),
        torch: if ncnn.is_some() || (args.cdylib.is_some() && !args.no_ncnn) {
            format!("ncnn {NCNN_TAG}, onnxruntime webgpu")
        } else {
            "onnxruntime webgpu".into()
        },
        torch_build: String::new(),
        target: target.clone(),
        os: os.into(),
        arch: arch.into(),
        abi: abi_version(&root)?,
        library,
        lib_dir: "lib".into(),
        bunko_version: version.clone(),
        requires: Requires::default(),
        files,
        external: Vec::new(),
        links: Vec::new(),
    };
    let mut json = serde_json::to_vec_pretty(&manifest)?;
    json.push(b'\n');
    let size = manifest.files.iter().map(|f| f.size).sum::<u64>();
    eprintln!("    {} files, {}", manifest.files.len(), human(size));
    let out_dir = &args.out;
    std::fs::create_dir_all(out_dir)?;
    if args.no_archive {
        let dest = out_dir.join(&name);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        std::fs::create_dir_all(&dest)?;
        std::fs::write(dest.join(PACK_JSON), &json)?;
        for (rel, src) in &staged {
            let d = dest.join(rel);
            std::fs::create_dir_all(d.parent().context("parent")?)?;
            link_or_copy(src, &d)?;
        }
        println!("{}", dest.display());
        return Ok(dest);
    }
    let stem = archive_stem(&version, &target, VARIANT);
    let archive = out_dir.join(format!("{stem}.tar.zst"));
    write_tar_zst(
        &json,
        &staged,
        &[],
        &name,
        &archive,
        args.level,
        util::build_epoch(),
    )?;
    let (sha256, asize) = util::sha256_file(&archive)?;
    let parts = split(&archive, args.max_part)?;
    eprintln!(
        "    {}: {} compressed ({} parts)",
        archive.display(),
        human(asize),
        parts.len()
    );
    println!("{}  {sha256}  {asize} bytes", archive.display());
    Ok(parts.into_iter().next().unwrap_or(archive))
}
