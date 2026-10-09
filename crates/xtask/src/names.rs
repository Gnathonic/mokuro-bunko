//! Build flavours, cargo features and release file names.
//!
//! Release downloads are named for people (0.7.0-beta.3): `mokuro-bunko-<ver>-windows.zip`,
//! `-macos.dmg`, `-linux-x64.tar.gz`, `-linux-arm64-server.tar.gz` ([`platform_label`]).
//! The OCR backend packs are `mokuro-bunko-backend-<ver>-<platform>-<variant>.tar.zst`
//! (after every download in GitHub's alphabetical list), and the macOS archive that the
//! updater of 0.7.0-beta.2 and earlier installs from is `mokuro-bunko-update-<ver>-macos.tar.gz`.
//! A build without a label (an unreleased flavor, a local build) keeps the old name
//! `mokuro-bunko-<ver>-<target>-<flavor>`; [`parse_archive_name`] reads both.
//!
//! The release manifest keys artifacts by `target triple → flavor`, where the flavor is
//! what the running server asks the updater for: `lite` or `full`. Since 0.7's libtorch
//! backend (TORCH-BACKEND.md) one `full` binary serves every GPU: the recognizers come
//! from a backend pack that `install-ocr` downloads, and ONNX Runtime runs only the
//! CPU PP-OCR stages. `full-<ep>` (an ONNX Runtime GPU execution provider: `--ep cuda`,
//! `directml`, `coreml`, `webgpu`) can still be built locally but is not released.

use std::fmt;

pub const BIN: &str = "mokuro-bunko";
/// The Windows build of the program for the GUI subsystem (the tray, no console window).
pub const WINDOWS_GUI_EXE: &str = bunko_update::layout::WINDOWS_GUI_EXE;
pub const DEFAULT_DOCKER_REPO: &str = "ghcr.io/gnathonic/mokuro-bunko";
pub const DEFAULT_GITHUB_REPO: &str = "Gnathonic/mokuro-bunko";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Flavor {
    /// Server only (no ONNX Runtime): `--no-default-features`.
    Lite,
    /// Server + local OCR (`ocr` feature) with an execution provider.
    Full,
}

/// ONNX Runtime GPU execution provider linked into a full build (deferred in 0.7: not
/// released; `None` is the release build).
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

/// The execution provider a plain `full` build gets: none (ONNX Runtime on the CPU)
/// on every platform. GPUs are served by the libtorch backend packs.
pub fn default_ep(_target: &str) -> Ep {
    Ep::None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub target: String,
    pub flavor: Flavor,
    pub ep: Ep,
    /// Built without the desktop tray (`xtask dist --no-tray`: the Docker images).
    pub no_tray: bool,
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
            no_tray: false,
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

    /// The full build has the desktop tray (`mokuro-bunko tray`, the default `tray`
    /// feature) on the desktop platforms; the lite build and `--no-tray` do not.
    pub fn has_tray(&self) -> bool {
        self.flavor == Flavor::Full
            && !self.no_tray
            && (self.target.contains("-linux-gnu")
                || self.is_windows()
                || self.target.contains("-apple-darwin"))
    }

    /// `(no_default_features, features)` for `-p mokuro-bunko`.
    pub fn cargo_features(&self) -> (bool, Vec<&'static str>) {
        match self.flavor {
            Flavor::Lite => (true, vec![]),
            Flavor::Full if self.no_tray => {
                let (_, f) = Build {
                    no_tray: false,
                    ..self.clone()
                }
                .cargo_features();
                (true, f)
            }
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
        archive_stem(version, &self.target, &self.manifest_flavor())
    }

    pub fn archive_name(&self, version: &str) -> String {
        format!(
            "{}.{}",
            self.archive_stem(version),
            archive_ext(&self.target)
        )
    }
}

/// The name a release download has for people: `windows`, `macos`, `linux-x64`,
/// `linux-arm64-server`, and `linux-x64-server` (the static server the lite Docker image
/// is made from; not a release download). `None`: no released build of that kind.
pub fn platform_label(target: &str, flavor: &str) -> Option<&'static str> {
    PLATFORMS
        .iter()
        .find(|(t, f, _)| *t == target && *f == flavor)
        .map(|(_, _, l)| *l)
}

const PLATFORMS: &[(&str, &str, &str)] = &[
    ("x86_64-pc-windows-msvc", "full", "windows"),
    ("aarch64-apple-darwin", "full", "macos"),
    ("x86_64-unknown-linux-gnu", "full", "linux-x64"),
    ("aarch64-unknown-linux-musl", "lite", "linux-arm64-server"),
    ("x86_64-unknown-linux-musl", "lite", "linux-x64-server"),
];

/// The prefix of the macOS archive the updater of 0.7.0-beta.2 and earlier takes (they
/// cannot install from the disk image): after the downloads in the release's file list.
pub const MACOS_UPDATE_PREFIX: &str = "mokuro-bunko-update";

/// `mokuro-bunko-<ver>-<label>` (the macOS archive: `mokuro-bunko-update-<ver>-macos`),
/// or `mokuro-bunko-<ver>-<target>-<flavor>` for a build without a label.
pub fn archive_stem(version: &str, target: &str, flavor: &str) -> String {
    let v = strip_v(version);
    match platform_label(target, flavor) {
        Some(l) if target.ends_with("-apple-darwin") => format!("{MACOS_UPDATE_PREFIX}-{v}-{l}"),
        Some(l) => format!("{BIN}-{v}-{l}"),
        None => format!("{BIN}-{v}-{target}-{flavor}"),
    }
}

/// The macOS disk image of a build: `mokuro-bunko-<ver>-macos.dmg` (no label:
/// `mokuro-bunko-<ver>-<target>-<flavor>.dmg`).
pub fn dmg_name(version: &str, target: &str, flavor: &str) -> String {
    let v = strip_v(version);
    match platform_label(target, flavor) {
        Some(l) => format!("{BIN}-{v}-{l}.dmg"),
        None => format!("{BIN}-{v}-{target}-{flavor}.dmg"),
    }
}

/// The OCR backend pack platform of `target` (`linux-x64`, `windows`, `macos`).
pub fn pack_platform(target: &str) -> Option<&'static str> {
    PLATFORMS
        .iter()
        .find(|(t, f, _)| *t == target && *f == "full")
        .map(|(_, _, l)| *l)
}

/// The target of a pack platform name.
pub fn pack_target(platform: &str) -> Option<&'static str> {
    PLATFORMS
        .iter()
        .find(|(_, f, l)| *l == platform && *f == "full")
        .map(|(t, _, _)| *t)
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

/// A release archive's `(target, flavor)` from its file name: a labelled name
/// ([`archive_stem`]) or `mokuro-bunko-<version>-<target>-<flavor>.<ext>`.
pub fn parse_archive_name(file: &str, version: &str) -> Option<(&'static str, String)> {
    let (stem, ext) = if let Some(s) = file.strip_suffix(".tar.gz") {
        (s, "tar.gz")
    } else {
        (file.strip_suffix(".zip")?, "zip")
    };
    let v = strip_v(version);
    for (t, f, _) in PLATFORMS {
        if archive_stem(v, t, f) == stem {
            return (archive_ext(t) == ext).then(|| (*t, f.to_string()));
        }
    }
    let (target, flavor) = parse_stem(stem, version)?;
    if archive_ext(target) != ext {
        return None;
    }
    let target = leak_target(target);
    Some((target, flavor.to_string()))
}

/// A target triple named in a file, as a `'static` string (the known ones are
/// constants; any other is leaked once, which xtask can afford).
fn leak_target(target: &str) -> &'static str {
    const KNOWN: &[&str] = &[
        "x86_64-pc-windows-msvc",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
    ];
    KNOWN
        .iter()
        .find(|k| **k == target)
        .copied()
        .unwrap_or_else(|| Box::leak(target.to_string().into_boxed_str()))
}

/// A macOS disk image's `(target, flavor)` (Apple targets only): `mokuro-bunko-<ver>-macos.dmg`,
/// or `mokuro-bunko-<version>-<target>-<flavor>.dmg` made from an unlabelled archive.
pub fn parse_dmg_name(file: &str, version: &str) -> Option<(&'static str, String)> {
    let stem = file.strip_suffix(".dmg")?;
    for (t, f, _) in PLATFORMS {
        if t.ends_with("-apple-darwin") && dmg_name(version, t, f) == file {
            return Some((*t, f.to_string()));
        }
    }
    let (target, flavor) = parse_stem(stem, version)?;
    target
        .ends_with("-apple-darwin")
        .then(|| (leak_target(target), flavor.to_string()))
}

/// `mokuro-bunko-<version>-<target>-<flavor>` → `(target, flavor)`.
fn parse_stem<'a>(stem: &'a str, version: &str) -> Option<(&'a str, &'a str)> {
    let stem = stem
        .strip_prefix(BIN)?
        .strip_prefix('-')?
        .strip_prefix(strip_v(version))?
        .strip_prefix('-')?;
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
    if target.split('-').count() < 3 {
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
        assert_eq!(b.cargo_features(), (false, vec!["ocr"]));
        let b = Build::new("x86_64-pc-windows-msvc", Flavor::Full, Some(Ep::Directml)).unwrap();
        assert_eq!(b.manifest_flavor(), "full-directml");
        assert_eq!(b.cargo_features(), (false, vec!["ocr", "directml"]));
        let b = Build::new("x86_64-pc-windows-msvc", Flavor::Full, Some(Ep::Cuda)).unwrap();
        assert_eq!(b.manifest_flavor(), "full-cuda");
        assert_eq!(b.cargo_features(), (false, vec!["ocr", "cuda", "directml"]));
        let b = Build::new("x86_64-unknown-linux-gnu", Flavor::Full, Some(Ep::Cuda)).unwrap();
        assert_eq!(b.cargo_features(), (false, vec!["ocr", "cuda"]));
        let b = Build::new("aarch64-apple-darwin", Flavor::Full, None).unwrap();
        assert_eq!(b.cargo_features(), (false, vec!["ocr"]));
        let b = Build::new("x86_64-unknown-linux-musl", Flavor::Lite, None).unwrap();
        assert_eq!(b.cargo_features(), (true, vec![]));
        assert!(Build::new("x86_64-unknown-linux-musl", Flavor::Lite, Some(Ep::Cuda)).is_err());
    }

    #[test]
    fn archive_names_roundtrip() {
        for (target, flavor, ep) in [
            ("x86_64-unknown-linux-musl", Flavor::Lite, None),
            ("aarch64-unknown-linux-musl", Flavor::Lite, None),
            ("x86_64-unknown-linux-gnu", Flavor::Full, None),
            ("x86_64-unknown-linux-gnu", Flavor::Full, Some(Ep::Cuda)),
            ("x86_64-pc-windows-msvc", Flavor::Full, None),
            ("x86_64-pc-windows-msvc", Flavor::Full, Some(Ep::Cuda)),
            ("x86_64-pc-windows-msvc", Flavor::Lite, None),
            ("aarch64-apple-darwin", Flavor::Full, None),
            ("x86_64-apple-darwin", Flavor::Lite, None),
        ] {
            let b = Build::new(target, flavor, ep).unwrap();
            for v in ["0.7.0", "v0.7.0-alpha.1"] {
                let name = b.archive_name(v);
                assert_eq!(
                    parse_archive_name(&name, v),
                    Some((target, b.manifest_flavor())),
                    "{name}"
                );
            }
        }
        let name = |t, f| Build::new(t, f, None).unwrap().archive_name("0.7.0-beta.3");
        assert_eq!(
            name("x86_64-pc-windows-msvc", Flavor::Full),
            "mokuro-bunko-0.7.0-beta.3-windows.zip"
        );
        assert_eq!(
            name("x86_64-unknown-linux-gnu", Flavor::Full),
            "mokuro-bunko-0.7.0-beta.3-linux-x64.tar.gz"
        );
        assert_eq!(
            name("aarch64-unknown-linux-musl", Flavor::Lite),
            "mokuro-bunko-0.7.0-beta.3-linux-arm64-server.tar.gz"
        );
        assert_eq!(
            name("aarch64-apple-darwin", Flavor::Full),
            "mokuro-bunko-update-0.7.0-beta.3-macos.tar.gz"
        );
        // Unreleased kinds keep the triple.
        assert_eq!(
            name("x86_64-pc-windows-msvc", Flavor::Lite),
            "mokuro-bunko-0.7.0-beta.3-x86_64-pc-windows-msvc-lite.zip"
        );
        // The names of beta.2 still parse (xtask verify of an old release directory).
        assert_eq!(
            parse_archive_name(
                "mokuro-bunko-0.7.0-x86_64-unknown-linux-gnu-full.tar.gz",
                "0.7.0"
            ),
            Some(("x86_64-unknown-linux-gnu", "full".into()))
        );
        assert_eq!(
            parse_archive_name(
                "mokuro-bunko-0.7.0-x86_64-unknown-linux-gnu-full.tar.gz",
                "0.7.1"
            ),
            None
        );
        assert_eq!(
            parse_archive_name("mokuro-bunko-0.7.0-windows.tar.gz", "0.7.0"),
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
        assert_eq!(
            parse_archive_name("mokuro-bunko-backend-0.7.0-linux-x64-cpu.tar.zst", "0.7.0"),
            None
        );
    }

    #[test]
    fn dmg_names() {
        assert_eq!(
            dmg_name("v0.7.0-beta.3", "aarch64-apple-darwin", "full"),
            "mokuro-bunko-0.7.0-beta.3-macos.dmg"
        );
        assert_eq!(
            parse_dmg_name("mokuro-bunko-0.7.0-beta.3-macos.dmg", "0.7.0-beta.3"),
            Some(("aarch64-apple-darwin", "full".into()))
        );
        // Never an updater archive.
        assert_eq!(
            parse_archive_name("mokuro-bunko-0.7.0-beta.3-macos.dmg", "0.7.0-beta.3"),
            None
        );
        // Unlabelled builds (and beta.2's names).
        assert_eq!(
            parse_dmg_name("mokuro-bunko-0.7.0-x86_64-apple-darwin-lite.dmg", "0.7.0"),
            Some(("x86_64-apple-darwin", "lite".into()))
        );
        assert_eq!(
            parse_dmg_name(
                "mokuro-bunko-0.7.0-x86_64-pc-windows-msvc-full.dmg",
                "0.7.0"
            ),
            None
        );
        assert_eq!(
            parse_dmg_name("mokuro-bunko-update-0.7.0-macos.tar.gz", "0.7.0"),
            None
        );
    }

    #[test]
    fn pack_platforms() {
        assert_eq!(pack_platform("x86_64-unknown-linux-gnu"), Some("linux-x64"));
        assert_eq!(pack_platform("x86_64-pc-windows-msvc"), Some("windows"));
        assert_eq!(pack_platform("aarch64-apple-darwin"), Some("macos"));
        assert_eq!(pack_platform("aarch64-unknown-linux-musl"), None);
        assert_eq!(pack_target("linux-x64"), Some("x86_64-unknown-linux-gnu"));
        assert_eq!(pack_target("linux-arm64-server"), None);
    }

    #[test]
    fn docker_tags() {
        assert_eq!(docker_tag("v0.7.0", "full"), "0.7.0");
        assert_eq!(docker_tag("0.7.0", "lite"), "0.7.0-lite");
        assert_eq!(docker_tag("0.7.0", "full-cuda"), "0.7.0-cuda");
    }
}
