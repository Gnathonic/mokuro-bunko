//! `cargo xtask interop rig`: every party as a real process (a testing build of the
//! server, cloudsim, rclone, the fake OCR processor, the reader), one suite per
//! provider (docs/design/interop-testing.md §7.1). The `cloud` suite (Bunko and the
//! cloud, no reader) runs here in Rust; the browser suites drive the pinned reader
//! through Bunko's Playwright install in `interop/`. The `folder` provider is a library
//! folder on this disk that Bunko keeps as a storage backend, the way it keeps a cloud.

mod browser;
mod cloud;
mod cloudsim;
mod control;
mod fakeproc;
mod folder;
mod playwright;
mod rclone;
mod reader;
mod results;

use crate::util;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

#[derive(clap::Args)]
pub struct RigArgs {
    /// The suites to run (comma-separated or repeated): dav, api, bridge, direct,
    /// shared, cloud, or all.
    #[arg(value_enum, value_delimiter = ',', required = true)]
    suites: Vec<SuiteArg>,
    /// The providers to run them on; a suite runs on those it applies to (dav and api:
    /// local; direct and shared: the clouds and rclone; bridge and cloud: those and
    /// folder).
    #[arg(long, value_enum, value_delimiter = ',', default_value = "all")]
    provider: Vec<ProviderArg>,
    /// A testing-feature server (default: target/release/mokuro-bunko), built with
    /// `cargo build --release -p mokuro-bunko --no-default-features --features testing`.
    #[arg(long)]
    bin: Option<PathBuf>,
    /// The cloud simulator (default: target/release/bunko-cloudsim).
    #[arg(long)]
    cloudsim_bin: Option<PathBuf>,
    /// An installed rclone of the pinned release, instead of downloading it.
    #[arg(long)]
    rclone_bin: Option<PathBuf>,
    /// The browser suites: a reader checkout to build and serve.
    #[arg(long, conflicts_with = "reader_ref")]
    reader_dir: Option<PathBuf>,
    /// The browser suites: the reader commit to fetch (default: interop/reader.ref).
    #[arg(long)]
    reader_ref: Option<String>,
    /// Only the scenarios (and specs) whose name contains this, as plain text and
    /// case-sensitive (a spec's name, to Playwright, is its file and titles joined by
    /// spaces). A suite with no match passes with no scenarios.
    #[arg(long)]
    grep: Option<String>,
    /// The browser suites: show the browser.
    #[arg(long)]
    headed: bool,
    /// Keep each suite's work folder (data dirs, configs) even when it passed.
    #[arg(long)]
    keep: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum SuiteArg {
    Dav,
    Api,
    Bridge,
    Direct,
    Shared,
    Cloud,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ProviderArg {
    Local,
    Google,
    Microsoft,
    Mega,
    Rclone,
    Folder,
    All,
}

impl SuiteArg {
    /// The one suite named; `None` for `all`.
    fn suite(self) -> Option<Suite> {
        Some(match self {
            SuiteArg::Dav => Suite::Dav,
            SuiteArg::Api => Suite::Api,
            SuiteArg::Bridge => Suite::Bridge,
            SuiteArg::Direct => Suite::Direct,
            SuiteArg::Shared => Suite::Shared,
            SuiteArg::Cloud => Suite::Cloud,
            SuiteArg::All => return None,
        })
    }
}

impl ProviderArg {
    /// The one provider named; `None` for `all`.
    fn provider(self) -> Option<Provider> {
        Some(match self {
            ProviderArg::Local => Provider::Local,
            ProviderArg::Google => Provider::Google,
            ProviderArg::Microsoft => Provider::Microsoft,
            ProviderArg::Mega => Provider::Mega,
            ProviderArg::Rclone => Provider::Rclone,
            ProviderArg::Folder => Provider::Folder,
            ProviderArg::All => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Suite {
    Dav,
    Api,
    Bridge,
    Direct,
    Shared,
    Cloud,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Provider {
    Local,
    Google,
    Microsoft,
    Mega,
    Rclone,
    /// A library folder on this disk, Bunko's `folder` backend: no reader can be driven
    /// on it (the reader's Local Folder needs the File System Access picker).
    Folder,
}

const SUITES: [Suite; 6] = [
    Suite::Dav,
    Suite::Api,
    Suite::Bridge,
    Suite::Direct,
    Suite::Shared,
    Suite::Cloud,
];
const PROVIDERS: [Provider; 6] = [
    Provider::Local,
    Provider::Google,
    Provider::Microsoft,
    Provider::Mega,
    Provider::Rclone,
    Provider::Folder,
];
const CLOUDS: [Provider; 4] = [
    Provider::Google,
    Provider::Microsoft,
    Provider::Mega,
    Provider::Rclone,
];
/// Where a Bunko can keep its library apart from its own disk: the clouds and a folder.
const BACKENDS: [Provider; 5] = [
    Provider::Google,
    Provider::Microsoft,
    Provider::Mega,
    Provider::Rclone,
    Provider::Folder,
];

impl Suite {
    pub fn name(self) -> &'static str {
        match self {
            Suite::Dav => "dav",
            Suite::Api => "api",
            Suite::Bridge => "bridge",
            Suite::Direct => "direct",
            Suite::Shared => "shared",
            Suite::Cloud => "cloud",
        }
    }

    /// The providers this suite runs on. A folder runs the suites with no reader on
    /// the library itself (cloud) or the reader signed in to Bunko (bridge); direct and
    /// shared need the reader's Local Folder, which only the File System Access picker
    /// opens.
    fn providers(self) -> &'static [Provider] {
        match self {
            Suite::Dav | Suite::Api => &[Provider::Local],
            Suite::Cloud | Suite::Bridge => &BACKENDS,
            Suite::Direct | Suite::Shared => &CLOUDS,
        }
    }

    /// Drives the reader in a browser.
    fn browser(self) -> bool {
        self != Suite::Cloud
    }
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::Local => "local",
            Provider::Google => "google",
            Provider::Microsoft => "microsoft",
            Provider::Mega => "mega",
            Provider::Rclone => "rclone",
            Provider::Folder => "folder",
        }
    }

    /// The provider's name in cloudsim's control API. One cloudsim does not play keeps
    /// its own name, which cloudsim refuses, rather than another provider's.
    pub fn sim_name(self) -> &'static str {
        match self {
            Provider::Google => "google",
            Provider::Microsoft => "microsoft",
            Provider::Mega => "mega",
            Provider::Local | Provider::Rclone | Provider::Folder => self.name(),
        }
    }

    /// Played by cloudsim (not rclone, not this disk).
    pub fn simulated(self) -> bool {
        match self {
            Provider::Google | Provider::Microsoft | Provider::Mega => true,
            Provider::Local | Provider::Rclone | Provider::Folder => false,
        }
    }
}

/// Every (suite, provider) pair the arguments name, in a fixed order. `all` stands for
/// every suite or provider and keeps only the pairs that apply; a suite named on its
/// own that runs on none of the named providers is an error, not a silent no-op.
fn expand(suites: &[SuiteArg], providers: &[ProviderArg]) -> Result<Vec<(Suite, Provider)>> {
    let all_suites = suites.contains(&SuiteArg::All);
    let all_providers = providers.contains(&ProviderArg::All);
    let named = |s: Suite| suites.iter().any(|a| a.suite() == Some(s));
    let wanted = |p: Provider| all_providers || providers.iter().any(|a| a.provider() == Some(p));
    let mut out = Vec::new();
    for s in SUITES {
        if !all_suites && !named(s) {
            continue;
        }
        let here: Vec<Provider> = PROVIDERS
            .into_iter()
            .filter(|p| wanted(*p) && s.providers().contains(p))
            .collect();
        if here.is_empty() && named(s) {
            let on: Vec<&str> = s.providers().iter().map(|p| p.name()).collect();
            bail!(
                "the {} suite runs on {} only, none of which --provider names",
                s.name(),
                on.join(", ")
            );
        }
        out.extend(here.into_iter().map(|p| (s, p)));
    }
    if out.is_empty() {
        bail!("no suite runs on the providers named");
    }
    Ok(out)
}

/// `interop/reader.ref`: the reader commit the rig tests, one full SHA (40 lowercase
/// hex digits) and nothing else, so a branch name never moves the pin.
pub fn parse_reader_ref(text: &str) -> Result<String> {
    let sha = text.trim();
    if sha.len() != 40 || !sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        bail!("interop/reader.ref must hold one 40-hex commit SHA, not {sha:?}");
    }
    Ok(sha.to_string())
}

/// The reader the browser suites run.
#[derive(Debug)]
pub enum ReaderSource {
    Dir(PathBuf),
    Ref(String),
}

fn reader_source(a: &RigArgs, root: &Path) -> Result<ReaderSource> {
    if let Some(d) = &a.reader_dir {
        return Ok(ReaderSource::Dir(d.clone()));
    }
    let text = match &a.reader_ref {
        Some(r) => r.clone(),
        None => {
            let path = root.join("interop/reader.ref");
            match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
                    "no reader for the browser suites: {} does not exist (it pins a pushed \
                     reader commit) and no --reader-dir was given; pass --reader-dir \
                     <reader checkout> (or --reader-ref <sha>)",
                    path.display()
                ),
                Err(e) => return Err(e).with_context(|| path.display().to_string()),
            }
        }
    };
    Ok(ReaderSource::Ref(parse_reader_ref(&text)?))
}

/// The paths a run uses.
pub struct Paths {
    /// `target/interop/rig`: results and logs.
    pub out: PathBuf,
    /// `target/interop/tools`: downloaded tools (rclone).
    pub tools: PathBuf,
    pub bin: PathBuf,
    pub cloudsim_bin: PathBuf,
    /// `--rclone-bin`.
    pub rclone_bin: Option<PathBuf>,
}

fn release_bin(target: &Path, name: &str) -> PathBuf {
    target
        .join("release")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

/// A server built without `testing` has no `processor-fake` and ignores (refuses)
/// cloudsim's addresses: say so before anything starts.
fn check_testing_build(bin: &Path) -> Result<()> {
    let out = std::process::Command::new(bin)
        .args(["processor-fake", "--help"])
        .output()
        .with_context(|| format!("running {}", bin.display()))?;
    if !out.status.success() {
        bail!(
            "{} is not a testing build: build it with `cargo build --release -p mokuro-bunko \
             --no-default-features --features testing`",
            bin.display()
        );
    }
    Ok(())
}

/// `HEAD` of this checkout, `-dirty` with uncommitted changes to tracked files.
fn bunko_sha(root: &Path) -> Option<String> {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let head = git(&["rev-parse", "HEAD"])?;
    let dirty =
        git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    Some(if dirty { format!("{head}-dirty") } else { head })
}

pub fn run(a: &RigArgs) -> Result<()> {
    let root = util::workspace_root();
    let target = util::target_dir(&root);
    let pairs = expand(&a.suites, &a.provider)?;
    let paths = Paths {
        out: target.join("interop").join("rig"),
        tools: target.join("interop").join("tools"),
        bin: a
            .bin
            .clone()
            .unwrap_or_else(|| release_bin(&target, "mokuro-bunko")),
        cloudsim_bin: a
            .cloudsim_bin
            .clone()
            .unwrap_or_else(|| release_bin(&target, "bunko-cloudsim")),
        rclone_bin: a.rclone_bin.clone(),
    };
    if !paths.bin.exists() {
        bail!(
            "{} not found: build it with `cargo build --release -p mokuro-bunko \
             --no-default-features --features testing` or pass --bin",
            paths.bin.display()
        );
    }
    check_testing_build(&paths.bin)?;
    if pairs.iter().any(|(_, p)| p.simulated()) && !paths.cloudsim_bin.exists() {
        bail!(
            "{} not found: build it with `cargo build --release -p bunko-cloudsim` or pass \
             --cloudsim-bin",
            paths.cloudsim_bin.display()
        );
    }
    let interop = root.join("interop");
    let pending_caps = playwright::parse_caps(
        &std::fs::read_to_string(interop.join("pending-reader-caps.txt"))
            .context("interop/pending-reader-caps.txt")?,
    )?;
    // The reader is built and served once for every browser suite: each spec gets a
    // fresh browser context, so nothing carries over between suites through it.
    let mut served = None;
    if pairs.iter().any(|(s, _)| s.browser()) {
        let src = reader_source(a, &root)?;
        let cache = target.join("interop").join("reader");
        let co = reader::checkout(&src, &cache)?;
        std::fs::create_dir_all(&paths.out)?;
        reader::build(
            &co,
            &cache.join("stamps"),
            &paths.out.join("reader-build.log"),
        )?;
        playwright::ensure_installed(&interop)?;
        let port = super::server::free_port()?;
        let preview = reader::Preview::start(&co.dir, port, &paths.out.join("reader-preview.log"))?;
        eprintln!("reader {} at {}", co.reader_sha(), preview.url);
        served = Some((co, preview));
    }
    let bunko_sha = bunko_sha(&root);

    let mut failed = Vec::new();
    for (suite, provider) in pairs {
        let name = format!("{}-{}", suite.name(), provider.name());
        let work = paths.out.join("work").join(&name);
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work)?;
        eprintln!("== {name}");
        let mut report = results::Report::new(suite.name(), provider.name(), &paths.bin);
        report.bunko_sha = bunko_sha.clone();
        report.pending_caps = pending_caps.clone();
        match (suite, &served) {
            (Suite::Cloud, _) => {
                cloud::run(&paths, provider, &work, a.grep.as_deref(), &mut report)
            }
            (_, Some((co, preview))) => {
                report.reader_sha = Some(co.reader_sha());
                browser::run(
                    &paths,
                    &interop,
                    suite,
                    provider,
                    &work,
                    &preview.url,
                    a.grep.as_deref(),
                    a.headed,
                    &mut report,
                );
            }
            (_, None) => unreachable!("the reader is served for every browser suite"),
        }
        report.finish();
        let file = report.write(&paths.out, &work)?;
        for s in &report.scenarios {
            eprintln!(
                "  {:<16} {:<8} {:>6} ms{}",
                s.name,
                format!("{:?}", s.outcome).to_lowercase(),
                s.ms,
                s.detail
                    .as_deref()
                    .map(|d| format!("  {}", d.lines().next().unwrap_or("")))
                    .unwrap_or_default()
            );
        }
        eprintln!("  -> {}", file.display());
        let ok = report.outcome != results::Outcome::Failed;
        if ok && !a.keep {
            let _ = std::fs::remove_dir_all(&work);
        }
        if !ok {
            failed.push(name);
        }
    }
    if !failed.is_empty() {
        bail!("failed: {}", failed.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "4e5b7fd0a1b2c3d4e5f60718293a4b5c6d7e8f90";

    #[test]
    fn reader_ref_is_one_full_sha_only() {
        assert_eq!(parse_reader_ref(&format!("{SHA}\n")).unwrap(), SHA);
        assert_eq!(parse_reader_ref(&format!("  {SHA}  ")).unwrap(), SHA);
        for bad in [
            "",
            "4e5b7fd0",
            "develop",
            "feat/interop-i3",
            &SHA.to_uppercase(),
            &format!("{SHA}0"),
            &format!("{SHA}\n{SHA}"),
            "4e5b7fd0a1b2c3d4e5f60718293a4b5c6d7e8f9g",
        ] {
            assert!(parse_reader_ref(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_reader_dir_wins_and_a_bad_ref_is_refused() {
        let args = |dir: Option<&str>, r: Option<&str>| RigArgs {
            suites: vec![SuiteArg::Dav],
            provider: vec![ProviderArg::All],
            bin: None,
            cloudsim_bin: None,
            rclone_bin: None,
            reader_dir: dir.map(PathBuf::from),
            reader_ref: r.map(str::to_string),
            grep: None,
            headed: false,
            keep: false,
        };
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            reader_source(&args(Some("/r"), None), root.path()).unwrap(),
            ReaderSource::Dir(d) if d == Path::new("/r")
        ));
        assert!(matches!(
            reader_source(&args(None, Some(SHA)), root.path()).unwrap(),
            ReaderSource::Ref(s) if s == SHA
        ));
        assert!(reader_source(&args(None, Some("main")), root.path()).is_err());
        // No file and no flag: the error names both ways to give a reader.
        let err = format!(
            "{:#}",
            reader_source(&args(None, None), root.path()).unwrap_err()
        );
        assert!(
            err.contains("interop/reader.ref") && err.contains("--reader-dir"),
            "{err}"
        );
        std::fs::create_dir_all(root.path().join("interop")).unwrap();
        std::fs::write(root.path().join("interop/reader.ref"), format!("{SHA}\n")).unwrap();
        assert!(matches!(
            reader_source(&args(None, None), root.path()).unwrap(),
            ReaderSource::Ref(s) if s == SHA
        ));
    }

    fn pairs(s: &[SuiteArg], p: &[ProviderArg]) -> Vec<(&'static str, &'static str)> {
        expand(s, p)
            .unwrap()
            .into_iter()
            .map(|(s, p)| (s.name(), p.name()))
            .collect()
    }

    #[test]
    fn suites_and_providers_expand_to_the_pairs_that_apply() {
        use ProviderArg as P;
        use SuiteArg as S;
        assert_eq!(pairs(&[S::Cloud], &[P::Google]), [("cloud", "google")]);
        assert_eq!(
            pairs(&[S::Cloud], &[P::All]),
            [
                ("cloud", "google"),
                ("cloud", "microsoft"),
                ("cloud", "mega"),
                ("cloud", "rclone"),
                ("cloud", "folder")
            ]
        );
        assert_eq!(
            pairs(&[S::All], &[P::Local]),
            [("dav", "local"), ("api", "local")]
        );
        // Order is fixed, whatever order the arguments come in.
        assert_eq!(
            pairs(&[S::Shared, S::Dav], &[P::Rclone, P::Local]),
            [("dav", "local"), ("shared", "rclone")]
        );
        let all = expand(&[S::All], &[P::All]).unwrap();
        // dav and api on local; direct and shared on the four clouds; bridge and cloud
        // on those and a folder.
        assert_eq!(all.len(), 2 + 2 * 4 + 2 * 5);
        // A suite named on its own with no provider it runs on is an error.
        let err = expand(&[S::Dav], &[P::Google]).unwrap_err().to_string();
        assert!(err.contains("dav suite runs on local only"), "{err}");
        assert!(expand(&[S::Cloud], &[P::Local]).is_err());
        // With `all`, pairs that do not apply are left out quietly.
        assert_eq!(
            pairs(&[S::All], &[P::Mega]),
            [
                ("bridge", "mega"),
                ("direct", "mega"),
                ("shared", "mega"),
                ("cloud", "mega")
            ]
        );
        // A folder: cloud and bridge, in suite order; direct and shared cannot drive
        // the reader's Local Folder.
        assert_eq!(
            pairs(&[S::Cloud, S::Bridge], &[P::Folder]),
            [("bridge", "folder"), ("cloud", "folder")]
        );
        assert_eq!(
            pairs(&[S::All], &[P::Folder]),
            [("bridge", "folder"), ("cloud", "folder")]
        );
        for s in [S::Direct, S::Shared, S::Dav, S::Api] {
            assert!(expand(&[s], &[P::Folder]).is_err(), "{s:?}");
        }
        // The CI legs: the cloud leg on the four clouds, the folder leg on its own,
        // so no pair runs twice.
        let cloud_leg = pairs(&[S::Cloud], &[P::Google, P::Microsoft, P::Mega, P::Rclone]);
        let folder_leg = pairs(&[S::Cloud, S::Bridge], &[P::Folder]);
        assert!(cloud_leg.iter().all(|p| !folder_leg.contains(p)));
        assert_eq!(cloud_leg.len() + 1, pairs(&[S::Cloud], &[P::All]).len());
    }

    #[test]
    fn only_cloudsim_providers_have_a_cloudsim_name_of_their_own() {
        for p in PROVIDERS {
            let own = matches!(p.sim_name(), "google" | "microsoft" | "mega");
            assert_eq!(own, p.simulated(), "{p:?}");
            if !p.simulated() {
                assert_eq!(p.sim_name(), p.name());
            }
        }
    }

    #[test]
    fn rig_arguments_parse() {
        use clap::Parser as _;
        #[derive(clap::Parser)]
        struct T {
            #[command(flatten)]
            rig: RigArgs,
        }
        let t =
            T::try_parse_from(["x", "cloud,dav", "--provider", "google,local", "--keep"]).unwrap();
        assert_eq!(t.rig.suites, [SuiteArg::Cloud, SuiteArg::Dav]);
        assert_eq!(t.rig.provider, [ProviderArg::Google, ProviderArg::Local]);
        assert!(t.rig.keep);
        let t = T::try_parse_from(["x", "cloud,bridge", "--provider", "folder"]).unwrap();
        assert_eq!(t.rig.suites, [SuiteArg::Cloud, SuiteArg::Bridge]);
        assert_eq!(t.rig.provider, [ProviderArg::Folder]);
        let t = T::try_parse_from(["x", "all"]).unwrap();
        assert_eq!(t.rig.provider, [ProviderArg::All]);
        assert!(T::try_parse_from(["x"]).is_err());
        assert!(T::try_parse_from(["x", "perf"]).is_err());
        assert!(T::try_parse_from(["x", "dav", "--reader-dir", "r", "--reader-ref", SHA]).is_err());
    }
}
