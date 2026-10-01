//! `xtask dist`: build one target/flavor and package it as a release archive.

use crate::archive;
use crate::licenses;
use crate::names::{BIN, Build, Ep, Flavor};
use crate::util;
use anyhow::{Context, Result, bail};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Debug, clap::Args)]
pub struct DistArgs {
    /// Rust target triple (default: the host).
    #[arg(long)]
    pub target: Option<String>,
    #[arg(long, value_enum)]
    pub flavor: Flavor,
    /// Execution provider for a full build (default: DirectML on Windows, CoreML on
    /// macOS, CPU elsewhere). `cuda` makes the `full-cuda` variant.
    #[arg(long, value_enum)]
    pub ep: Option<Ep>,
    /// Output directory for the archive and its `.sha256`.
    #[arg(long, default_value = "dist")]
    pub out: PathBuf,
    /// Build with `cargo zigbuild` (cross-compiling the musl/aarch64 targets).
    #[arg(long)]
    pub zig: bool,
    /// Pass `--locked` to cargo (CI).
    #[arg(long)]
    pub locked: bool,
    /// Package an existing build instead of running cargo (expects the binary in the
    /// target directory).
    #[arg(long)]
    pub no_build: bool,
    /// Don't run `--version` on the packaged binary even when the host can.
    #[arg(long)]
    pub no_smoke: bool,
    /// Package even if a dependency's licence is copyleft (never for a release).
    #[arg(long)]
    pub allow_copyleft: bool,
    /// Shared libraries next to the binary whose names contain one of these strings are
    /// left out (TensorRT providers: unused and very large).
    #[arg(long, value_delimiter = ',', default_value = "tensorrt")]
    pub exclude_lib: Vec<String>,
}

/// Returns the archive written.
pub fn run(args: &DistArgs) -> Result<PathBuf> {
    let root = util::workspace_root();
    let host = util::host_triple()?;
    let target = args.target.clone().unwrap_or_else(|| host.clone());
    let build = Build::new(&target, args.flavor, args.ep)?;
    let version = util::workspace_version(&root)?;
    let out_dir = if args.out.is_absolute() {
        args.out.clone()
    } else {
        root.join(&args.out)
    };
    std::fs::create_dir_all(&out_dir)?;

    eprintln!(
        "==> mokuro-bunko {version} for {target} ({})",
        build.manifest_flavor()
    );
    let (exe, native_dirs) = if args.no_build {
        let exe = util::target_dir(&root)
            .join(&target)
            .join("release")
            .join(build.exe_name());
        if !exe.is_file() {
            bail!("--no-build: {} does not exist", exe.display());
        }
        (exe, Vec::new())
    } else {
        cargo_build(&root, &build, args)?
    };

    // Licences first: a copyleft dependency stops the release before anything is written.
    let report = licenses::collect(&root, &build, &version, &native_dirs)?;
    for c in report.unknown() {
        eprintln!(
            "warning: {} {} has an unreviewed licence: {}",
            c.name, c.version, c.license
        );
    }
    for c in report.missing_text() {
        eprintln!(
            "warning: {} {} ships no licence file ({})",
            c.name, c.version, c.license
        );
    }
    let copyleft = report.copyleft();
    if !copyleft.is_empty() {
        let list: Vec<_> = copyleft
            .iter()
            .map(|c| format!("{} {} ({})", c.name, c.version, c.license))
            .collect();
        if args.allow_copyleft {
            eprintln!(
                "warning: copyleft dependencies (allowed by --allow-copyleft): {}",
                list.join(", ")
            );
        } else {
            bail!("copyleft dependencies must not ship: {}", list.join(", "));
        }
    }
    eprintln!(
        "    {} third-party crates, all licences classified permissive: {}",
        report.components.len(),
        report.unknown().is_empty()
    );

    let stem = build.archive_stem(&version);
    let stage_root = out_dir.join(".stage");
    let stage = stage_root.join(&stem);
    if stage.exists() {
        std::fs::remove_dir_all(&stage)?;
    }
    std::fs::create_dir_all(&stage)?;

    std::fs::copy(&exe, stage.join(build.exe_name()))
        .with_context(|| format!("copying {}", exe.display()))?;
    for dir in &native_dirs {
        for lib in shared_libs(dir, &args.exclude_lib)? {
            let name = lib.file_name().context("library name")?;
            // fs::copy follows ort's symlinks, so the archive holds real files.
            std::fs::copy(&lib, stage.join(name))?;
            eprintln!("    bundling {}", name.to_string_lossy());
        }
    }
    stage_docs(&root, &build, &version, &stage, &report.markdown)?;

    if !args.no_smoke && util::can_run(&host, &target) {
        smoke_test(&stage.join(build.exe_name()), &version, &build)?;
    }

    let archive = out_dir.join(build.archive_name(&version));
    if build.is_windows() {
        archive::write_zip(&stage, &stem, &archive)?;
    } else {
        archive::write_tar_gz(&stage, &stem, &archive, util::build_epoch())?;
    }
    std::fs::remove_dir_all(&stage)?;
    let _ = std::fs::remove_dir(&stage_root);
    let (sha256, size) = util::sha256_file(&archive)?;
    let file_name = archive
        .file_name()
        .context("archive name")?
        .to_string_lossy()
        .to_string();
    std::fs::write(
        out_dir.join(format!("{file_name}.sha256")),
        format!("{sha256}  {file_name}\n"),
    )?;
    println!("{}  {sha256}  {size} bytes", archive.display());
    Ok(archive)
}

/// Run cargo; return the built executable and the ONNX Runtime prebuilt directories that
/// ort-sys linked from (their shared libraries are bundled).
fn cargo_build(root: &Path, build: &Build, args: &DistArgs) -> Result<(PathBuf, Vec<PathBuf>)> {
    let mut cmd = util::cargo();
    cmd.current_dir(root);
    cmd.arg(if args.zig { "zigbuild" } else { "build" });
    cmd.args([
        "--release",
        "-p",
        BIN,
        "--bin",
        BIN,
        "--target",
        &build.target,
        "--message-format=json-render-diagnostics",
    ]);
    if args.locked {
        cmd.arg("--locked");
    }
    let (no_default, features) = build.cargo_features();
    if no_default {
        cmd.arg("--no-default-features");
    }
    if !features.is_empty() {
        cmd.args(["--features", &features.join(",")]);
    }
    if build.ep == Ep::Cuda && std::env::var_os("ORT_CUDA_VERSION").is_none() {
        // ort ships CUDA 13 builds of ONNX Runtime only; say so instead of letting its
        // build script guess from the build machine.
        cmd.env("ORT_CUDA_VERSION", "13");
    }
    if build.flavor == Flavor::Full && build.target.contains("-linux-gnu") {
        // ONNX Runtime dlopen()s its provider libraries (CUDA) by bare name; an $ORIGIN
        // runpath finds them next to the real executable, even through a symlink.
        let var = format!(
            "CARGO_TARGET_{}_RUSTFLAGS",
            build.target.to_uppercase().replace(['-', '.'], "_")
        );
        let mut flags = std::env::var(&var).unwrap_or_default();
        if !flags.contains("rpath") {
            flags.push_str(" -C link-arg=-Wl,-rpath,$ORIGIN");
        }
        cmd.env(var, flags.trim());
    }
    cmd.stdout(Stdio::piped());
    eprintln!("+ {cmd:?}");
    let mut child = cmd.spawn().context("starting cargo")?;
    let stdout = child.stdout.take().context("cargo stdout")?;
    let mut exe = None;
    let mut native = Vec::new();
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line?;
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match msg["reason"].as_str() {
            Some("compiler-artifact")
                if msg["target"]["name"] == BIN && msg["executable"].is_string() =>
            {
                exe = msg["executable"].as_str().map(PathBuf::from);
            }
            Some("build-script-executed")
                if msg["package_id"]
                    .as_str()
                    .is_some_and(|id| id.contains("ort-sys")) =>
            {
                for p in msg["linked_paths"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p.as_str())
                {
                    let dir = PathBuf::from(p.split_once('=').map_or(p, |(_, d)| d));
                    if dir.is_dir() && !native.contains(&dir) {
                        native.push(dir);
                    }
                }
            }
            _ => {}
        }
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("cargo build failed: {status}");
    }
    let exe = exe.context("cargo reported no mokuro-bunko executable")?;
    if build.flavor == Flavor::Full && native.is_empty() {
        eprintln!(
            "warning: a full build that does not link ONNX Runtime (ort-sys reported no library \
             directory); mokuro-bunko's `ocr` feature is not wired to bunko-ocr yet?"
        );
    }
    Ok((exe, native))
}

fn shared_libs(dir: &Path, exclude: &[String]) -> Result<Vec<PathBuf>> {
    let mut libs = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let is_lib = name.ends_with(".dll")
            || name.ends_with(".dylib")
            || name.ends_with(".so")
            || name.contains(".so.");
        if is_lib
            && p.is_file()
            && !exclude
                .iter()
                .any(|x| !x.is_empty() && name.contains(x.as_str()))
        {
            libs.push(p);
        }
    }
    libs.sort();
    Ok(libs)
}

fn stage_docs(
    root: &Path,
    build: &Build,
    version: &str,
    stage: &Path,
    third_party: &str,
) -> Result<()> {
    let packaging = root.join("packaging");
    let subst = |text: &str| {
        text.replace("@VERSION@", version)
            .replace("@TARGET@", &build.target)
            .replace("@FLAVOR@", &build.manifest_flavor())
    };
    if build.is_windows() {
        // Windows: the portable layout (run.bat, doctor.bat, data\ next to them).
        for name in [
            "run.bat",
            "doctor.bat",
            "_env.cmd",
            "README.txt",
            "PORTABLE.txt",
        ] {
            let text = std::fs::read_to_string(packaging.join("windows").join(name))
                .with_context(|| format!("reading packaging/windows/{name}"))?;
            std::fs::write(stage.join(name), crlf(&subst(&text)))?;
        }
        std::fs::write(
            stage.join("LICENSE.txt"),
            crlf(&std::fs::read_to_string(root.join("LICENSE"))?),
        )?;
        std::fs::write(stage.join("THIRD-PARTY-LICENSES.md"), crlf(third_party))?;
    } else {
        let readme = std::fs::read_to_string(packaging.join("dist/README.md"))
            .context("reading packaging/dist/README.md")?;
        std::fs::write(stage.join("README.md"), subst(&readme))?;
        std::fs::copy(root.join("LICENSE"), stage.join("LICENSE"))?;
        std::fs::write(stage.join("THIRD-PARTY-LICENSES.md"), third_party)?;
    }
    Ok(())
}

/// Windows batch files must have CRLF line endings (cmd.exe misparses labels otherwise).
fn crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn smoke_test(exe: &Path, version: &str, build: &Build) -> Result<()> {
    let out = std::process::Command::new(exe)
        .arg("--version")
        .output()
        .with_context(|| format!("running {}", exe.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || !text.contains(version) {
        bail!(
            "smoke test: `{} --version` printed {:?} (status {})",
            exe.display(),
            text.trim(),
            out.status
        );
    }
    // The flavor is part of --version today; don't fail if the CLI later drops it.
    let want = if build.flavor == Flavor::Lite {
        "lite"
    } else {
        "full"
    };
    let other = if build.flavor == Flavor::Lite {
        "full"
    } else {
        "lite"
    };
    if text.contains(other) && !text.contains(want) {
        bail!(
            "smoke test: a {want} build reports itself as {other}: {:?}",
            text.trim()
        );
    }
    eprintln!("    smoke test: {}", text.trim());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_is_idempotent() {
        assert_eq!(crlf("a\nb\r\nc"), "a\r\nb\r\nc");
    }

    #[test]
    fn shared_lib_filter() {
        let dir = tempfile::tempdir().unwrap();
        for n in [
            "libonnxruntime.a",
            "libonnxruntime_providers_cuda.so",
            "libonnxruntime_providers_shared.so",
            "libonnxruntime_providers_tensorrt.so",
            "DirectML.dll",
            "x.so.1",
        ] {
            std::fs::write(dir.path().join(n), b"").unwrap();
        }
        let names: Vec<_> = shared_libs(dir.path(), &["tensorrt".into()])
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "DirectML.dll",
                "libonnxruntime_providers_cuda.so",
                "libonnxruntime_providers_shared.so",
                "x.so.1"
            ]
        );
    }
}
