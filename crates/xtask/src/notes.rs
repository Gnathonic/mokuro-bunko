//! `xtask release-notes`: the top of every release's notes, a two-column "Which file do
//! I want?" table (system → its file, linked straight to
//! `releases/download/<tag>/<file>`; Docker → the pull command), the line "Other files
//! install automatically." and the changelog link.
//!
//! The file names come from [`crate::names`], the same functions `xtask dist` and the
//! disk image step name the files with, so a link cannot drift from its file; with
//! `--dir` (the release's files) every linked file must be there.

use crate::names::{self, DEFAULT_DOCKER_REPO, DEFAULT_GITHUB_REPO};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

#[derive(Debug, clap::Args)]
pub struct NotesArgs {
    /// Release version (`0.7.0` or `v0.7.0`).
    #[arg(long)]
    pub version: String,
    /// GitHub repository (owner/name) the release and its files are in.
    #[arg(long, default_value = DEFAULT_GITHUB_REPO)]
    pub repo: String,
    /// Docker repository of the release images.
    #[arg(long, default_value = DEFAULT_DOCKER_REPO)]
    pub docker_repo: String,
    /// The release's files: each linked file must be here.
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// A Markdown file appended below the table (the generated changelog).
    #[arg(long)]
    pub changelog: Option<PathBuf>,
}

pub fn run(args: &NotesArgs) -> Result<()> {
    let changelog = match &args.changelog {
        Some(p) => {
            std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?
        }
        None => String::new(),
    };
    let exists = |file: &str| -> Result<()> {
        if let Some(dir) = &args.dir
            && !dir.join(file).is_file()
        {
            bail!(
                "{} is linked in the notes but not a release file",
                dir.join(file).display()
            );
        }
        Ok(())
    };
    print!(
        "{}",
        render(
            &args.version,
            &args.repo,
            &args.docker_repo,
            &changelog,
            &exists
        )?
    );
    Ok(())
}

/// One download of the table.
pub struct Download {
    pub system: &'static str,
    pub file: String,
}

/// The release's downloads for people, in table order (names from [`crate::names`]).
pub fn downloads(version: &str) -> Vec<Download> {
    let v = names::strip_v(version);
    let archive = |target: &str, flavor: &str| {
        let stem = names::archive_stem(v, target, flavor);
        format!("{stem}.{}", names::archive_ext(target))
    };
    vec![
        Download {
            system: "Windows",
            file: archive("x86_64-pc-windows-msvc", "full"),
        },
        Download {
            system: "macOS",
            file: names::dmg_name(v, "aarch64-apple-darwin", "full"),
        },
        Download {
            system: "Linux x64",
            file: archive("x86_64-unknown-linux-gnu", "full"),
        },
        Download {
            system: "Linux arm64 (server)",
            file: archive("aarch64-unknown-linux-musl", "lite"),
        },
    ]
}

/// The notes. `exists` checks that a linked file is one of the release's (an error
/// when it is not). `changelog` is GitHub's generated notes: only its "Full Changelog"
/// link is kept.
pub fn render(
    version: &str,
    repo: &str,
    docker_repo: &str,
    changelog: &str,
    exists: &dyn Fn(&str) -> Result<()>,
) -> Result<String> {
    let v = names::strip_v(version);
    let url = |file: &str| format!("https://github.com/{repo}/releases/download/v{v}/{file}");
    let mut out = String::from("## Which file do I want?\n\n| System | File |\n|---|---|\n");
    for d in downloads(v) {
        exists(&d.file)?;
        out.push_str(&format!(
            "| {} | [{}]({}) |\n",
            d.system,
            d.file,
            url(&d.file)
        ));
    }
    out.push_str(&format!(
        "| Docker | `docker pull {docker_repo}:{}` |\n",
        names::docker_tag(v, "full")
    ));
    out.push_str("\nOther files install automatically.\n");
    if let Some(link) = changelog
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("**Full Changelog**"))
    {
        out.push('\n');
        out.push_str(link);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The files the notes link to (`releases/download/v<ver>/<file>`).
    pub fn linked_files(notes: &str, repo: &str, version: &str) -> Vec<String> {
        let prefix = format!(
            "https://github.com/{repo}/releases/download/v{}/",
            names::strip_v(version)
        );
        let mut out = Vec::new();
        let mut rest = notes;
        while let Some(i) = rest.find(&prefix) {
            let tail = &rest[i + prefix.len()..];
            let end = tail.find([')', ' ', '\n']).unwrap_or(tail.len());
            out.push(tail[..end].to_string());
            rest = &tail[end..];
        }
        out
    }
    use std::path::Path;

    /// Every file a release uploads that people download: what `xtask dist` and the
    /// disk image step write for the release matrix (release-build.yml), plus the
    /// manifest files.
    fn release_files(dir: &Path, version: &str) {
        use crate::names::{Build, Flavor};
        for (t, f) in [
            ("x86_64-pc-windows-msvc", Flavor::Full),
            ("aarch64-apple-darwin", Flavor::Full),
            ("x86_64-unknown-linux-gnu", Flavor::Full),
            ("aarch64-unknown-linux-musl", Flavor::Lite),
        ] {
            let b = Build::new(t, f, None).unwrap();
            std::fs::write(dir.join(b.archive_name(version)), vec![0u8; 1_500_000]).unwrap();
        }
        std::fs::write(
            dir.join(names::dmg_name(version, "aarch64-apple-darwin", "full")),
            vec![0u8; 2_500_000],
        )
        .unwrap();
        for f in ["release.json", "release.json.sig", "SHA256SUMS"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
    }

    fn exists_in(dir: &Path) -> impl Fn(&str) -> Result<()> + '_ {
        move |file: &str| {
            std::fs::metadata(dir.join(file))
                .with_context(|| format!("{file} is not a release file"))?;
            Ok(())
        }
    }

    #[test]
    fn every_link_names_a_file_the_release_uploads() {
        let dir = tempfile::tempdir().unwrap();
        let v = "0.7.0-beta.3";
        release_files(dir.path(), v);
        let repo = "Gnathonic/mokuro-bunko";
        let n = render(
            &format!("v{v}"),
            repo,
            "ghcr.io/gnathonic/mokuro-bunko",
            "## What's Changed\n\n**Full Changelog**: https://x/compare/a...b\n",
            &exists_in(dir.path()),
        )
        .unwrap();
        let links = linked_files(&n, repo, v);
        assert_eq!(
            links,
            [
                "mokuro-bunko-0.7.0-beta.3-windows.zip",
                "mokuro-bunko-0.7.0-beta.3-macos.dmg",
                "mokuro-bunko-0.7.0-beta.3-linux-x64.tar.gz",
                "mokuro-bunko-0.7.0-beta.3-linux-arm64-server.tar.gz",
            ]
        );
        for f in &links {
            assert!(
                dir.path().join(f).is_file(),
                "{f} is linked but not uploaded"
            );
        }
        assert_eq!(
            n,
            "## Which file do I want?\n\n| System | File |\n|---|---|\n\
             | Windows | [mokuro-bunko-0.7.0-beta.3-windows.zip](https://github.com/Gnathonic/mokuro-bunko/releases/download/v0.7.0-beta.3/mokuro-bunko-0.7.0-beta.3-windows.zip) |\n\
             | macOS | [mokuro-bunko-0.7.0-beta.3-macos.dmg](https://github.com/Gnathonic/mokuro-bunko/releases/download/v0.7.0-beta.3/mokuro-bunko-0.7.0-beta.3-macos.dmg) |\n\
             | Linux x64 | [mokuro-bunko-0.7.0-beta.3-linux-x64.tar.gz](https://github.com/Gnathonic/mokuro-bunko/releases/download/v0.7.0-beta.3/mokuro-bunko-0.7.0-beta.3-linux-x64.tar.gz) |\n\
             | Linux arm64 (server) | [mokuro-bunko-0.7.0-beta.3-linux-arm64-server.tar.gz](https://github.com/Gnathonic/mokuro-bunko/releases/download/v0.7.0-beta.3/mokuro-bunko-0.7.0-beta.3-linux-arm64-server.tar.gz) |\n\
             | Docker | `docker pull ghcr.io/gnathonic/mokuro-bunko:0.7.0-beta.3` |\n\
             \nOther files install automatically.\n\
             \n**Full Changelog**: https://x/compare/a...b\n"
        );
        // A linked file that is not there stops the notes.
        std::fs::remove_file(dir.path().join("mokuro-bunko-0.7.0-beta.3-macos.dmg")).unwrap();
        assert!(render(v, repo, "img", "", &exists_in(dir.path())).is_err());
        assert!(
            !render("0.7.0", "o/r", "img", "", &|_| Ok(()))
                .unwrap()
                .contains("Changelog")
        );
    }
}
