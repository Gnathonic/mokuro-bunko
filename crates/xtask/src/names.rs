//! Build flavours, cargo features and release file names.
//!
//! The release manifest keys artifacts by `target triple → flavor`, where the flavor is
//! what the running server asks the updater for: `lite`, `full` (the platform's default
//! execution provider) or `full-<ep>` for an extra variant such as `full-cuda`.

use std::fmt;

pub const BIN: &str = "mokuro-bunko";
pub const DEFAULT_DOCKER_REPO: &str = "ghcr.io/gnathonic/mokuro-bunko";
pub const DEFAULT_GITHUB_REPO: &str = "Gnathonic/mokuro-bunko";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Flavor {
    /// Server only (no ONNX Runtime): `--no-default-features`.
    Lite,
    /// Server + local OCR (`ocr` feature) with an execution provider.
    Full,
}

/// ONNX Runtime execution provider linked into a full build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Ep {
    /// CPU only.
    None,
    Cuda,
    Directml,
    Coreml,
    Webgpu,
}

impl fmt::Display for Ep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Ep::None => "cpu",
            Ep::Cuda => "cuda",
            Ep::Directml => "directml",
            Ep::Coreml => "coreml",
            Ep::Webgpu => "webgpu",
        })
    }
}

/// The execution provider a plain `full` build gets on each platform. ort's prebuilt
/// Windows binaries all include DirectML and its macOS ones CoreML; Linux is CPU.
pub fn default_ep(target: &str) -> Ep {
    if target.contains("windows") {
        Ep::Directml
    } else if target.contains("apple") {
        Ep::Coreml
    } else {
        Ep::None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub target: String,
    pub flavor: Flavor,
    pub ep: Ep,
}

impl Build {
    pub fn new(target: &str, flavor: Flavor, ep: Option<Ep>) -> anyhow::Result<Build> {
        let ep = match (flavor, ep) {
            (Flavor::Lite, None | Some(Ep::None)) => Ep::None,
            (Flavor::Lite, Some(ep)) => {
                anyhow::bail!("a lite build has no execution provider (got --ep {ep})")
            }
            (Flavor::Full, ep) => ep.unwrap_or_else(|| default_ep(target)),
        };
        Ok(Build {
            target: target.to_string(),
            flavor,
            ep,
        })
    }

    /// The flavor key in `release.json` (`lite`, `full`, `full-cuda`, ...).
    pub fn manifest_flavor(&self) -> String {
        match self.flavor {
            Flavor::Lite => "lite".into(),
            Flavor::Full if self.ep == default_ep(&self.target) => "full".into(),
            Flavor::Full => format!("full-{}", self.ep),
        }
    }

    /// `(no_default_features, features)` for `-p mokuro-bunko`.
    pub fn cargo_features(&self) -> (bool, Vec<&'static str>) {
        match self.flavor {
            Flavor::Lite => (true, vec![]),
            Flavor::Full => {
                let mut f = vec!["ocr"];
                match self.ep {
                    Ep::None => {}
                    // ort's only Windows CUDA build also contains DirectML, so ask for both
                    // or its build script finds no matching prebuilt ONNX Runtime.
                    Ep::Cuda if self.is_windows() => f.extend(["cuda", "directml"]),
                    Ep::Cuda => f.push("cuda"),
                    Ep::Directml => f.push("directml"),
                    Ep::Coreml => f.push("coreml"),
                    Ep::Webgpu => f.push("webgpu"),
                }
                (false, f)
            }
        }
    }

    pub fn is_windows(&self) -> bool {
        self.target.contains("windows")
    }

    pub fn exe_name(&self) -> String {
        exe_name(&self.target)
    }

    pub fn archive_stem(&self, version: &str) -> String {
        format!(
            "{BIN}-{}-{}-{}",
            strip_v(version),
            self.target,
            self.manifest_flavor()
        )
    }

    pub fn archive_name(&self, version: &str) -> String {
        format!(
            "{}.{}",
            self.archive_stem(version),
            archive_ext(&self.target)
        )
    }
}

pub fn exe_name(target: &str) -> String {
    if target.contains("windows") {
        format!("{BIN}.exe")
    } else {
        BIN.to_string()
    }
}

pub fn archive_ext(target: &str) -> &'static str {
    if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    }
}

pub fn strip_v(version: &str) -> &str {
    version.strip_prefix('v').unwrap_or(version)
}

/// Split `mokuro-bunko-<version>-<target>-<flavor>.<ext>` back into `(target, flavor)`.
pub fn parse_archive_name<'a>(file: &'a str, version: &str) -> Option<(&'a str, &'a str)> {
    let rest = file
        .strip_prefix(BIN)?
        .strip_prefix('-')?
        .strip_prefix(strip_v(version))?
        .strip_prefix('-')?;
    let stem = rest
        .strip_suffix(".tar.gz")
        .or_else(|| rest.strip_suffix(".zip"))?;
    // The flavor is the part after the target triple: `lite`, `full` or `full-<ep>`.
    let (target, flavor) = if let Some(i) = stem.rfind("-full-") {
        (&stem[..i], &stem[i + 1..])
    } else {
        let i = stem.rfind('-')?;
        (&stem[..i], &stem[i + 1..])
    };
    if !matches!(flavor, "lite" | "full") && !flavor.starts_with("full-") {
        return None;
    }
    if target.split('-').count() < 3
        || archive_ext(target) != &file[file.len() - archive_ext(target).len()..]
    {
        return None;
    }
    Some((target, flavor))
}

/// Docker image tag suffix for a manifest flavor (`:<ver>`, `:<ver>-lite`, `:<ver>-cuda`).
pub fn docker_tag(version: &str, flavor: &str) -> String {
    let v = strip_v(version);
    match flavor {
        "full" => v.to_string(),
        "lite" => format!("{v}-lite"),
        other => format!("{v}-{}", other.trim_start_matches("full-")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavors_and_features() {
        let b = Build::new("x86_64-pc-windows-msvc", Flavor::Full, None).unwrap();
        assert_eq!(b.manifest_flavor(), "full");
        assert_eq!(b.cargo_features(), (false, vec!["ocr", "directml"]));
        let b = Build::new("x86_64-pc-windows-msvc", Flavor::Full, Some(Ep::Cuda)).unwrap();
        assert_eq!(b.manifest_flavor(), "full-cuda");
        assert_eq!(b.cargo_features(), (false, vec!["ocr", "cuda", "directml"]));
        let b = Build::new("x86_64-unknown-linux-gnu", Flavor::Full, Some(Ep::Cuda)).unwrap();
        assert_eq!(b.cargo_features(), (false, vec!["ocr", "cuda"]));
        let b = Build::new("aarch64-apple-darwin", Flavor::Full, None).unwrap();
        assert_eq!(b.cargo_features(), (false, vec!["ocr", "coreml"]));
        let b = Build::new("x86_64-unknown-linux-musl", Flavor::Lite, None).unwrap();
        assert_eq!(b.cargo_features(), (true, vec![]));
        assert!(Build::new("x86_64-unknown-linux-musl", Flavor::Lite, Some(Ep::Cuda)).is_err());
    }

    #[test]
    fn archive_names_roundtrip() {
        for (target, flavor, ep) in [
            ("x86_64-unknown-linux-musl", Flavor::Lite, None),
            ("x86_64-unknown-linux-gnu", Flavor::Full, None),
            ("x86_64-unknown-linux-gnu", Flavor::Full, Some(Ep::Cuda)),
            ("x86_64-pc-windows-msvc", Flavor::Full, Some(Ep::Cuda)),
            ("aarch64-apple-darwin", Flavor::Full, None),
        ] {
            let b = Build::new(target, flavor, ep).unwrap();
            for v in ["0.7.0", "v0.7.0-alpha.1"] {
                let name = b.archive_name(v);
                assert_eq!(
                    parse_archive_name(&name, v),
                    Some((target, b.manifest_flavor().as_str())),
                    "{name}"
                );
            }
        }
        assert_eq!(
            parse_archive_name(
                "mokuro-bunko-0.7.0-x86_64-unknown-linux-gnu-full.tar.gz",
                "0.7.1"
            ),
            None
        );
        assert_eq!(
            parse_archive_name(
                "mokuro-bunko-0.7.0-x86_64-unknown-linux-gnu-full.zip",
                "0.7.0"
            ),
            None
        );
        assert_eq!(parse_archive_name("release.json", "0.7.0"), None);
    }

    #[test]
    fn docker_tags() {
        assert_eq!(docker_tag("v0.7.0", "full"), "0.7.0");
        assert_eq!(docker_tag("0.7.0", "lite"), "0.7.0-lite");
        assert_eq!(docker_tag("0.7.0", "full-cuda"), "0.7.0-cuda");
    }
}
