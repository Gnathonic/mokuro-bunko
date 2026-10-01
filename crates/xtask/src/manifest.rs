//! `xtask manifest`: write `release.json` (+ `SHA256SUMS`) from the built archives.

use crate::names::{self, DEFAULT_DOCKER_REPO, DEFAULT_GITHUB_REPO};
use crate::util;
use anyhow::{Context, Result, bail};
use bunko_update::{Artifact, Manifest};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, clap::Args)]
pub struct ManifestArgs {
    /// Release version (`0.7.0` or `v0.7.0`).
    #[arg(long)]
    pub version: String,
    /// Directory holding the archives (`mokuro-bunko-<ver>-<target>-<flavor>.tar.gz|zip`).
    #[arg(long, default_value = "dist")]
    pub dir: PathBuf,
    /// URL prefix the archives are downloaded from (default: the GitHub release of the tag).
    #[arg(long)]
    pub base_url: Option<String>,
    /// Release notes page (default: the GitHub release page of the tag).
    #[arg(long)]
    pub notes_url: Option<String>,
    /// Publication time, ISO-8601 (default: SOURCE_DATE_EPOCH or now).
    #[arg(long)]
    pub published_at: Option<String>,
    /// Docker repository for the `docker` map; empty for none.
    #[arg(long, default_value = DEFAULT_DOCKER_REPO)]
    pub docker_repo: String,
    /// Docker flavors to list (each must be pushed by the release workflow).
    #[arg(long, value_delimiter = ',', default_value = "lite,full,full-cuda")]
    pub docker_flavors: Vec<String>,
    /// Output file (default: <dir>/release.json).
    #[arg(long)]
    pub out: Option<PathBuf>,
}

pub fn run(args: &ManifestArgs) -> Result<PathBuf> {
    let version = names::strip_v(&args.version).to_string();
    semver_check(&version)?;
    let root = util::workspace_root();
    if let Ok(ws) = util::workspace_version(&root)
        && ws != version
    {
        eprintln!("warning: manifest version {version} differs from the workspace version {ws}");
    }
    let base = args.base_url.clone().unwrap_or_else(|| {
        format!("https://github.com/{DEFAULT_GITHUB_REPO}/releases/download/v{version}")
    });
    let base = base.trim_end_matches('/');
    let manifest = build_manifest(args, &version, base)?;

    let bytes = to_bytes(&manifest)?;
    // The updater must accept what we wrote (bar the signature, checked by `verify`).
    let parsed: Manifest = serde_json::from_slice(&bytes)?;
    parsed.semver().map_err(|e| anyhow::anyhow!("{e}"))?;

    let out = args
        .out
        .clone()
        .unwrap_or_else(|| args.dir.join("release.json"));
    std::fs::write(&out, &bytes).with_context(|| format!("writing {}", out.display()))?;
    write_sha256sums(&args.dir, &manifest)?;
    for (target, flavors) in &manifest.artifacts {
        for (flavor, a) in flavors {
            eprintln!(
                "    {target:<28} {flavor:<10} {:>10} bytes  {}",
                a.size, a.sha256
            );
        }
    }
    println!("{}", out.display());
    Ok(out)
}

fn semver_check(version: &str) -> Result<()> {
    semver::Version::parse(version)
        .map(drop)
        .with_context(|| format!("{version:?} is not a semver version"))
}

pub fn build_manifest(args: &ManifestArgs, version: &str, base: &str) -> Result<Manifest> {
    let mut artifacts: BTreeMap<String, BTreeMap<String, Artifact>> = BTreeMap::new();
    let mut entries: Vec<_> = std::fs::read_dir(&args.dir)
        .with_context(|| format!("reading {}", args.dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some((target, flavor)) = names::parse_archive_name(&name, version) else {
            continue;
        };
        let (sha256, size) = util::sha256_file(&path)?;
        let prev = artifacts.entry(target.to_string()).or_default().insert(
            flavor.to_string(),
            Artifact {
                url: format!("{base}/{name}"),
                sha256,
                size,
                binary: names::exe_name(target),
            },
        );
        if prev.is_some() {
            bail!("two archives for {target} {flavor}");
        }
    }
    if artifacts.is_empty() {
        bail!(
            "no mokuro-bunko-{version}-*.tar.gz|zip archives in {}",
            args.dir.display()
        );
    }
    let docker = if args.docker_repo.is_empty() {
        BTreeMap::new()
    } else {
        args.docker_flavors
            .iter()
            .filter(|f| !f.is_empty())
            .map(|f| {
                (
                    f.clone(),
                    format!("{}:{}", args.docker_repo, names::docker_tag(version, f)),
                )
            })
            .collect()
    };
    Ok(Manifest {
        version: version.to_string(),
        published_at: args
            .published_at
            .clone()
            .unwrap_or_else(|| util::iso_utc(util::build_epoch())),
        notes_url: args.notes_url.clone().unwrap_or_else(|| {
            format!("https://github.com/{DEFAULT_GITHUB_REPO}/releases/tag/v{version}")
        }),
        artifacts,
        docker,
    })
}

/// Pretty JSON with a trailing newline. These exact bytes are what gets signed; the
/// one-key-per-line layout is also what `scripts/install.sh` parses without `jq`.
pub fn to_bytes(m: &Manifest) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(m)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_sha256sums(dir: &Path, m: &Manifest) -> Result<()> {
    let mut lines: Vec<String> = m
        .artifacts
        .values()
        .flat_map(|f| f.values())
        .map(|a| {
            format!(
                "{}  {}",
                a.sha256,
                a.url.rsplit('/').next().unwrap_or(&a.url)
            )
        })
        .collect();
    lines.sort();
    std::fs::write(dir.join("SHA256SUMS"), lines.join("\n") + "\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path()
                .join("mokuro-bunko-0.7.0-x86_64-unknown-linux-musl-lite.tar.gz"),
            b"lite",
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join("mokuro-bunko-0.7.0-x86_64-pc-windows-msvc-full-cuda.zip"),
            b"cuda",
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join("mokuro-bunko-0.7.0-x86_64-pc-windows-msvc-full-cuda.zip.sha256"),
            b"x",
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join("mokuro-bunko-0.6.9-x86_64-unknown-linux-musl-lite.tar.gz"),
            b"old",
        )
        .unwrap();
        let args = ManifestArgs {
            version: "v0.7.0".into(),
            dir: dir.path().to_path_buf(),
            base_url: None,
            notes_url: None,
            published_at: Some("2026-10-01T00:00:00Z".into()),
            docker_repo: DEFAULT_DOCKER_REPO.into(),
            docker_flavors: vec!["lite".into(), "full".into(), "full-cuda".into()],
            out: None,
        };
        let m = build_manifest(&args, "0.7.0", "https://example.test/dl").unwrap();
        assert_eq!(m.artifacts.len(), 2);
        let lite = m.artifact("x86_64-unknown-linux-musl", "lite").unwrap();
        assert_eq!(
            lite.url,
            "https://example.test/dl/mokuro-bunko-0.7.0-x86_64-unknown-linux-musl-lite.tar.gz"
        );
        assert_eq!(lite.size, 4);
        assert_eq!(lite.binary, "mokuro-bunko");
        let cuda = m.artifact("x86_64-pc-windows-msvc", "full-cuda").unwrap();
        assert_eq!(cuda.binary, "mokuro-bunko.exe");
        assert_eq!(
            m.docker["full-cuda"],
            "ghcr.io/gnathonic/mokuro-bunko:0.7.0-cuda"
        );
        assert_eq!(
            m.docker["lite"],
            "ghcr.io/gnathonic/mokuro-bunko:0.7.0-lite"
        );
        let bytes = to_bytes(&m).unwrap();
        let back: Manifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn versions() {
        assert!(semver_check("0.7.0").is_ok());
        assert!(semver_check("0.7.0-alpha.1").is_ok());
        assert!(semver_check("0.7").is_err());
    }
}
