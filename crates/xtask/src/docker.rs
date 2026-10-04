//! `xtask docker-context`: lay out the Linux release binaries for the Dockerfiles'
//! `BIN_FROM=prebuilt` stage, so images reuse the signed release builds instead of
//! compiling again (and arm64 images need no emulated compile).
//!
//! Layout: `<out>/<amd64|arm64>/<lite|full|cuda>/` holding the archive's files plus
//! `bunko-init` (the PUID/PGID entrypoint from `packaging/docker-init`). The full and
//! cuda images run the same `full` binary; each also gets its OCR backend pack,
//! installed complete under `backends/` (full: `cpu`, cuda: `cu130` with the NVIDIA
//! libraries fetched from PyPI and baked in).

use crate::archive;
use crate::names;
use crate::util;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

#[derive(Debug, clap::Args)]
pub struct DockerContextArgs {
    /// Release version of the archives.
    #[arg(long)]
    pub version: Option<String>,
    /// Directory holding the release archives.
    #[arg(long, default_value = "dist")]
    pub dir: PathBuf,
    /// Output directory (the Dockerfiles expect `dist/docker`).
    #[arg(long, default_value = "dist/docker")]
    pub out: PathBuf,
    /// Build bunko-init with `cargo zigbuild` (needed for arm64 on an x86_64 host).
    #[arg(long)]
    pub zig: bool,
    /// Don't build bunko-init (it must then be copied in by hand).
    #[arg(long)]
    pub no_init: bool,
}

fn docker_arch(target: &str) -> Option<&'static str> {
    match target.split('-').next()? {
        "x86_64" => Some("amd64"),
        "aarch64" => Some("arm64"),
        _ => None,
    }
}

/// Docker platform arch and the image flavor dirs a release archive goes into.
fn slots(target: &str, flavor: &str) -> Vec<(&'static str, &'static str)> {
    let Some(arch) = docker_arch(target) else {
        return vec![];
    };
    match (
        flavor,
        target.ends_with("-unknown-linux-musl"),
        target.ends_with("-unknown-linux-gnu"),
    ) {
        ("lite", true, _) => vec![(arch, "lite")],
        // The CUDA image is the full binary with the cu130 pack (amd64 only).
        ("full", _, true) if arch == "amd64" => vec![(arch, "full"), (arch, "cuda")],
        ("full", _, true) => vec![(arch, "full")],
        _ => vec![],
    }
}

/// The image flavor dir a backend pack goes into.
fn pack_slot(target: &str, variant: &str) -> Option<(&'static str, &'static str)> {
    if !target.ends_with("-unknown-linux-gnu") {
        return None;
    }
    let arch = docker_arch(target)?;
    match variant {
        "cpu" => Some((arch, "full")),
        "cu130" if arch == "amd64" => Some((arch, "cuda")),
        _ => None,
    }
}

pub fn run(args: &DockerContextArgs) -> Result<()> {
    let root = util::workspace_root();
    let version = match &args.version {
        Some(v) => names::strip_v(v).to_string(),
        None => util::workspace_version(&root)?,
    };
    let dir = if args.dir.is_absolute() {
        args.dir.clone()
    } else {
        root.join(&args.dir)
    };
    let out = if args.out.is_absolute() {
        args.out.clone()
    } else {
        root.join(&args.out)
    };
    let mut slots: Vec<(String, PathBuf)> = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some((target, flavor)) = names::parse_archive_name(&name, &version) else {
            continue;
        };
        for (arch, flavor_dir) in self::slots(target, flavor) {
            let dest = out.join(arch).join(flavor_dir);
            if dest.exists() {
                std::fs::remove_dir_all(&dest)?;
            }
            let files = archive::extract_flat(&path, &dest)?;
            if !dest.join(names::BIN).is_file() {
                bail!("{name} has no {} at its top level", names::BIN);
            }
            eprintln!("    {arch}/{flavor_dir}: {} files from {name}", files.len());
            slots.push((arch.to_string(), dest));
        }
    }
    // Backend packs, after the binaries (whose extraction resets the slot dirs).
    let mut packs: std::collections::BTreeMap<(String, String), Vec<PathBuf>> =
        std::collections::BTreeMap::new();
    for e in std::fs::read_dir(&dir)?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some((target, variant, _)) = crate::torch_pack::parse_pack_name(&name, &version) {
            packs
                .entry((target.to_string(), variant.to_string()))
                .or_default()
                .push(e.path());
        }
    }
    let cache = crate::torch_pack::default_cache(&root);
    for ((target, variant), mut parts) in packs {
        let Some((arch, flavor_dir)) = pack_slot(&target, &variant) else {
            continue;
        };
        let slot_dir = out.join(arch).join(flavor_dir);
        if !slot_dir.join(names::BIN).is_file() {
            eprintln!("    skipping the {variant} pack: no {arch}/{flavor_dir} binary");
            continue;
        }
        parts.sort();
        let pack = crate::torch_pack::install_complete(&parts, &slot_dir.join("backends"), &cache)?;
        eprintln!("    {arch}/{flavor_dir}: backend pack {}", pack.display());
    }
    if slots.is_empty() {
        bail!(
            "no Linux release archives for {version} in {}",
            dir.display()
        );
    }
    if !args.no_init {
        let mut arches: Vec<_> = slots.iter().map(|(a, _)| a.clone()).collect();
        arches.dedup();
        for arch in arches {
            let init = build_init(&root, &arch, args.zig)?;
            for (_, dest) in slots.iter().filter(|(a, _)| *a == arch) {
                std::fs::copy(&init, dest.join("bunko-init"))?;
            }
        }
    }
    println!("{}", out.display());
    Ok(())
}

fn build_init(root: &Path, arch: &str, zig: bool) -> Result<PathBuf> {
    let target = match arch {
        "amd64" => "x86_64-unknown-linux-musl",
        _ => "aarch64-unknown-linux-musl",
    };
    let manifest = root.join("packaging/docker-init/Cargo.toml");
    let target_dir = util::target_dir(root).join("docker-init");
    let mut cmd = util::cargo();
    cmd.arg(if zig { "zigbuild" } else { "build" })
        .args([
            "--release",
            "--locked",
            "--target",
            target,
            "--manifest-path",
        ])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target_dir);
    util::run(&mut cmd)?;
    let bin = target_dir.join(target).join("release/bunko-init");
    bin.is_file()
        .then_some(bin.clone())
        .with_context(|| format!("{} was not built", bin.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_slots() {
        assert_eq!(
            slots("x86_64-unknown-linux-musl", "lite"),
            vec![("amd64", "lite")]
        );
        assert_eq!(
            slots("x86_64-unknown-linux-gnu", "full"),
            vec![("amd64", "full"), ("amd64", "cuda")]
        );
        assert_eq!(
            slots("aarch64-unknown-linux-gnu", "full"),
            vec![("arm64", "full")]
        );
        assert!(slots("x86_64-unknown-linux-gnu", "full-cuda").is_empty());
        assert!(slots("x86_64-unknown-linux-gnu", "lite").is_empty());
        assert!(slots("x86_64-pc-windows-msvc", "full").is_empty());
        assert_eq!(
            pack_slot("x86_64-unknown-linux-gnu", "cpu"),
            Some(("amd64", "full"))
        );
        assert_eq!(
            pack_slot("x86_64-unknown-linux-gnu", "cu130"),
            Some(("amd64", "cuda"))
        );
        assert_eq!(pack_slot("x86_64-unknown-linux-gnu", "rocm7.1"), None);
        assert_eq!(pack_slot("x86_64-pc-windows-msvc", "cpu"), None);
    }
}
