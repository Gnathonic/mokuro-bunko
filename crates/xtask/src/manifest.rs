//! `xtask manifest`: write `release.json` (+ `SHA256SUMS`) from the built archives.

use crate::names::{self, DEFAULT_DOCKER_REPO, DEFAULT_GITHUB_REPO};
use crate::util;
use anyhow::{Context, Result, bail};
use bunko_update::backend::{BackendArtifact, Part};
use bunko_update::{Artifact, FileRef, Manifest};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, clap::Args)]
pub struct ManifestArgs {
    /// Release version (`0.7.0` or `v0.7.0`).
    #[arg(long)]
    pub version: String,
    /// Directory holding the archives, disk images and packs (crate::names).
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
    for (target, variants) in &manifest.backends {
        for (variant, b) in variants {
            eprintln!(
                "    {target:<28} torch-{variant:<8} {:>10} bytes in {} part(s)  {}",
                b.size,
                b.parts.len(),
                b.sha256
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
            flavor.clone(),
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
    let backends = collect_backends(&args.dir, version, base)?;
    let bundles = collect_bundles(&args.dir, version, base)?;
    for (target, flavors) in &bundles {
        for flavor in flavors.keys() {
            // The updaters of 0.7.0-beta.2 and earlier read only `artifacts`: a macOS
            // build needs its archive there too, or they could not update to it.
            if !artifacts
                .get(target)
                .is_some_and(|f| f.contains_key(flavor))
            {
                bail!(
                    "{target} {flavor}: a disk image without the archive the older updaters need"
                );
            }
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
        backends,
        bundles,
    })
}

/// The macOS disk images in `dir`: target → flavor → file (`bundles`: what the updater
/// from 0.7.0-beta.3 on installs from on a Mac).
fn collect_bundles(
    dir: &Path,
    version: &str,
    base: &str,
) -> Result<BTreeMap<String, BTreeMap<String, FileRef>>> {
    let mut out: BTreeMap<String, BTreeMap<String, FileRef>> = BTreeMap::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
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
        let Some((target, flavor)) = names::parse_dmg_name(&name, version) else {
            continue;
        };
        let (sha256, size) = util::sha256_file(&path)?;
        out.entry(target.to_string()).or_default().insert(
            flavor,
            FileRef {
                url: format!("{base}/{name}"),
                sha256,
                size,
            },
        );
    }
    Ok(out)
}

/// OCR backend packs (`xtask torch-pack` output, possibly split in parts) in `dir`:
/// target → variant → artifact. The pack's metadata comes from its pack.json.
fn collect_backends(
    dir: &Path,
    version: &str,
    base: &str,
) -> Result<BTreeMap<String, BTreeMap<String, BackendArtifact>>> {
    let mut groups: BTreeMap<(String, String), Vec<(u32, PathBuf)>> = BTreeMap::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some((target, variant, part)) = crate::torch_pack::parse_pack_name(&name, version) {
            groups
                .entry((target.to_string(), variant.to_string()))
                .or_default()
                .push((part, e.path()));
        }
    }
    let mut out: BTreeMap<String, BTreeMap<String, BackendArtifact>> = BTreeMap::new();
    for ((target, variant), mut files) in groups {
        files.sort();
        let numbered: Vec<u32> = files.iter().map(|(n, _)| *n).collect();
        let expected: Vec<u32> = if numbered == [0] {
            vec![0]
        } else {
            (1..=files.len() as u32).collect()
        };
        if numbered != expected {
            bail!(
                "{target} {variant}: parts {numbered:?} are not a complete set (or the archive is there both whole and split)"
            );
        }
        let pack = crate::torch_pack::read_pack_json(&files[0].1)?;
        if pack.target != target || pack.variant != variant {
            bail!(
                "{}: pack.json says {} {}",
                files[0].1.display(),
                pack.target,
                pack.variant
            );
        }
        let mut hasher = sha2::Sha256::new();
        let mut parts = Vec::new();
        let mut size = 0u64;
        for (_, path) in &files {
            use sha2::Digest;
            use std::io::Read;
            let mut f = std::fs::File::open(path)?;
            let mut part_hash = sha2::Sha256::new();
            let mut buf = vec![0u8; 1 << 20];
            let mut n_part = 0u64;
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                part_hash.update(&buf[..n]);
                n_part += n as u64;
            }
            size += n_part;
            let file = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            parts.push(Part {
                url: format!("{base}/{file}"),
                sha256: hex::encode(part_hash.finalize()),
                size: n_part,
            });
        }
        use sha2::Digest;
        out.entry(target).or_default().insert(
            variant,
            BackendArtifact {
                name: pack.name.clone(),
                torch: pack.torch.clone(),
                abi: (pack.abi > 0).then_some(pack.abi),
                sha256: hex::encode(hasher.finalize()),
                size,
                parts,
                external_size: pack.external_size(),
                installed_size: pack.installed_size(),
                requires: (pack.requires != Default::default()).then(|| pack.requires.clone()),
            },
        );
    }
    Ok(out)
}

/// Pretty JSON with a trailing newline. These exact bytes are what gets signed; the
/// one-key-per-line layout is also what `scripts/install.sh` parses without `jq`.
pub fn to_bytes(m: &Manifest) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(m)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// `SHA256SUMS`: every file of the manifest (archives, disk images, pack parts).
fn write_sha256sums(dir: &Path, m: &Manifest) -> Result<()> {
    let line = |sha: &str, url: &str| format!("{sha}  {}", url.rsplit('/').next().unwrap_or(url));
    let mut lines: Vec<String> = m
        .artifacts
        .values()
        .flat_map(|f| f.values())
        .map(|a| line(&a.sha256, &a.url))
        .chain(
            m.bundles
                .values()
                .flat_map(|f| f.values())
                .map(|d| line(&d.sha256, &d.url)),
        )
        .chain(
            m.backends
                .values()
                .flat_map(|v| v.values())
                .flat_map(|b| &b.parts)
                .map(|p| line(&p.sha256, &p.url)),
        )
        .collect();
    lines.sort();
    lines.dedup();
    std::fs::write(dir.join("SHA256SUMS"), lines.join("\n") + "\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let v = "0.7.0-beta.3";
        for (name, body) in [
            (
                "mokuro-bunko-0.7.0-beta.3-linux-arm64-server.tar.gz",
                &b"arm"[..],
            ),
            ("mokuro-bunko-0.7.0-beta.3-linux-x64.tar.gz", b"x64"),
            ("mokuro-bunko-0.7.0-beta.3-windows.zip", b"win"),
            ("mokuro-bunko-update-0.7.0-beta.3-macos.tar.gz", b"mac-tgz"),
            ("mokuro-bunko-0.7.0-beta.3-macos.dmg", b"dmg"),
            // Not this release's.
            (
                "mokuro-bunko-0.7.0-beta.2-x86_64-unknown-linux-gnu-full.tar.gz",
                b"old",
            ),
        ] {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        let args = ManifestArgs {
            version: format!("v{v}"),
            dir: dir.path().to_path_buf(),
            base_url: None,
            notes_url: None,
            published_at: Some("2026-10-09T00:00:00Z".into()),
            docker_repo: DEFAULT_DOCKER_REPO.into(),
            docker_flavors: vec!["lite".into(), "full".into(), "full-cuda".into()],
            out: None,
        };
        let m = build_manifest(&args, v, "https://example.test/dl").unwrap();
        let keys: Vec<String> = m
            .artifacts
            .iter()
            .flat_map(|(t, f)| f.keys().map(move |k| format!("{t}/{k}")))
            .collect();
        assert_eq!(
            keys,
            [
                "aarch64-apple-darwin/full",
                "aarch64-unknown-linux-musl/lite",
                "x86_64-pc-windows-msvc/full",
                "x86_64-unknown-linux-gnu/full"
            ]
        );
        // What an updater of 0.7.0-beta.2 reads on a Mac: an archive.
        let mac = m.artifact("aarch64-apple-darwin", "full").unwrap();
        assert_eq!(
            mac.url,
            "https://example.test/dl/mokuro-bunko-update-0.7.0-beta.3-macos.tar.gz"
        );
        assert_eq!(mac.binary, "mokuro-bunko");
        // What this release's updater reads: the disk image.
        let d = m.download("aarch64-apple-darwin", "full").unwrap();
        assert!(d.url.ends_with("/mokuro-bunko-0.7.0-beta.3-macos.dmg"));
        assert_eq!(d.size, 3);
        assert_eq!(
            m.download("x86_64-pc-windows-msvc", "full").unwrap().binary,
            "mokuro-bunko.exe"
        );
        write_sha256sums(dir.path(), &m).unwrap();
        let sums = std::fs::read_to_string(dir.path().join("SHA256SUMS")).unwrap();
        let names: Vec<&str> = sums
            .lines()
            .map(|l| l.split_once("  ").unwrap().1)
            .collect();
        assert_eq!(names.len(), 5, "{sums}");
        assert!(names.contains(&"mokuro-bunko-0.7.0-beta.3-macos.dmg"));
        assert_eq!(
            m.docker["full-cuda"],
            "ghcr.io/gnathonic/mokuro-bunko:0.7.0-beta.3-cuda"
        );
        let bytes = to_bytes(&m).unwrap();
        let back: Manifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, m);
        // A disk image without the archive the old updaters need is refused.
        std::fs::remove_file(
            dir.path()
                .join("mokuro-bunko-update-0.7.0-beta.3-macos.tar.gz"),
        )
        .unwrap();
        assert!(build_manifest(&args, v, "https://example.test/dl").is_err());
    }

    /// The manifest as the updater of 0.7.0-beta.2 parses it: its own `Manifest` had no
    /// `bundles` and took unknown fields silently (serde's default).
    #[test]
    fn older_updaters_read_the_new_manifest() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct Beta2Artifact {
            url: String,
            sha256: String,
            size: u64,
            #[serde(default)]
            binary: String,
        }
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct Beta2Manifest {
            version: String,
            #[serde(default)]
            artifacts: BTreeMap<String, BTreeMap<String, Beta2Artifact>>,
            #[serde(default)]
            docker: BTreeMap<String, String>,
            #[serde(default)]
            backends: BTreeMap<String, BTreeMap<String, bunko_update::backend::BackendArtifact>>,
        }
        let mut m = Manifest {
            version: "0.7.0-beta.3".into(),
            published_at: String::new(),
            notes_url: String::new(),
            artifacts: BTreeMap::new(),
            docker: BTreeMap::new(),
            backends: BTreeMap::new(),
            bundles: BTreeMap::new(),
        };
        m.bundles
            .entry("aarch64-apple-darwin".into())
            .or_default()
            .insert(
                "full".into(),
                FileRef {
                    url: "https://x/mokuro-bunko-0.7.0-beta.3-macos.dmg".into(),
                    sha256: "00".into(),
                    size: 1,
                },
            );
        let old: Beta2Manifest = serde_json::from_slice(&to_bytes(&m).unwrap()).unwrap();
        assert_eq!(old.version, "0.7.0-beta.3");
    }

    #[test]
    fn versions() {
        assert!(semver_check("0.7.0").is_ok());
        assert!(semver_check("0.7.0-alpha.1").is_ok());
        assert!(semver_check("0.7").is_err());
    }
}
