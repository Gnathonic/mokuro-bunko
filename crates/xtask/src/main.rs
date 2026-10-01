//! Release tooling for mokuro-bunko. See docs/rust-port/PACKAGING.md.
//!
//! ```text
//! cargo run -p xtask -- dist --target x86_64-unknown-linux-musl --flavor lite
//! cargo run -p xtask -- manifest --version 0.7.0 --dir dist
//! cargo run -p xtask -- sign dist/release.json --key ~/.config/mokuro-bunko-release/signing.key
//! cargo run -p xtask -- verify dist/release.json --dir dist
//! ```

mod archive;
mod dist;
mod docker;
mod licenses;
mod manifest;
mod names;
mod sign;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "xtask", about = "mokuro-bunko release tooling")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build one target/flavor and package it (tar.gz on unix, zip on Windows) with
    /// README, LICENSE and THIRD-PARTY-LICENSES.md.
    Dist(dist::DistArgs),
    /// Write release.json (and SHA256SUMS) from the archives in a directory.
    Manifest(manifest::ManifestArgs),
    /// Sign a file (release.json) with the release ed25519 key; writes `<file>.sig`.
    Sign(sign::SignArgs),
    /// Verify release.json's signature (and optionally the archives' checksums).
    Verify(sign::VerifyArgs),
    /// Generate a new signing key (for forks and CI round-trip tests).
    Keygen(sign::KeygenArgs),
    /// Print the licence report for a build and fail on copyleft dependencies.
    Licenses(LicensesArgs),
    /// Lay out the Linux release binaries for the Dockerfiles' prebuilt stage.
    DockerContext(docker::DockerContextArgs),
}

#[derive(clap::Args)]
struct LicensesArgs {
    #[arg(long)]
    target: Option<String>,
    #[arg(long, value_enum)]
    flavor: names::Flavor,
    #[arg(long, value_enum)]
    ep: Option<names::Ep>,
    /// Write the THIRD-PARTY-LICENSES.md text here.
    #[arg(long)]
    out: Option<PathBuf>,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Dist(a) => dist::run(&a).map(drop),
        Cmd::Manifest(a) => manifest::run(&a).map(drop),
        Cmd::Sign(a) => sign::sign(&a).map(drop),
        Cmd::Verify(a) => sign::verify(&a),
        Cmd::Keygen(a) => sign::keygen(&a),
        Cmd::Licenses(a) => licenses_cmd(&a),
        Cmd::DockerContext(a) => docker::run(&a),
    }
}

fn licenses_cmd(a: &LicensesArgs) -> Result<()> {
    let root = util::workspace_root();
    let target = match &a.target {
        Some(t) => t.clone(),
        None => util::host_triple()?,
    };
    let build = names::Build::new(&target, a.flavor, a.ep)?;
    let version = util::workspace_version(&root)?;
    let report = licenses::collect(&root, &build, &version, &[])?;
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for c in &report.components {
        *counts.entry(c.license.as_str()).or_default() += 1;
    }
    for (lic, n) in &counts {
        println!("{n:>4}  {lic}");
    }
    for c in report.unknown() {
        eprintln!(
            "warning: unreviewed licence: {} {} ({})",
            c.name, c.version, c.license
        );
    }
    for c in report.missing_text() {
        eprintln!(
            "warning: no licence file: {} {} ({})",
            c.name, c.version, c.license
        );
    }
    if let Some(out) = &a.out {
        std::fs::write(out, &report.markdown)?;
    }
    let copyleft = report.copyleft();
    if !copyleft.is_empty() {
        for c in &copyleft {
            eprintln!("error: copyleft: {} {} ({})", c.name, c.version, c.license);
        }
        anyhow::bail!(
            "{} copyleft dependencies in {target} {}",
            copyleft.len(),
            build.manifest_flavor()
        );
    }
    eprintln!(
        "{} crates for {target} {}: no copyleft",
        report.components.len(),
        build.manifest_flavor()
    );
    Ok(())
}
