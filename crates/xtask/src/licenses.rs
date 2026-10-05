//! Third-party licence collection from `cargo metadata`.
//!
//! Walks the resolved dependency graph of `mokuro-bunko` for one target and feature set
//! (normal dependencies only: build scripts and dev-dependencies are not shipped),
//! classifies each crate's SPDX expression, and renders `THIRD-PARTY-LICENSES.md` with
//! the licence files each crate ships. Copyleft licences fail the build: the project's
//! rule is that nothing GPL/LGPL/AGPL ends up in a release artifact.

use crate::names::{BIN, Build, Ep, Flavor, TRAY_PKG};
use crate::util;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Licences that are fine to ship (CONVENTIONS.md: MIT/Apache-2.0/BSD/ISC/Zlib/MPL-2.0/Unicode).
const PERMISSIVE: &[&str] = &[
    "MIT",
    "MIT-0",
    "Apache-2.0",
    "BSD-1-Clause",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "0BSD",
    "ISC",
    "Zlib",
    "MPL-2.0",
    "Unicode-3.0",
    "Unicode-DFS-2016",
    "CC0-1.0",
    "Unlicense",
    "BSL-1.0",
    "CDLA-Permissive-2.0",
    "bzip2-1.0.6",
    "NCSA",
    // libjpeg-turbo (via mozjpeg): permissive, requires an acknowledgement in the docs.
    "IJG",
];

/// Licence ids (prefixes) that must never ship.
const COPYLEFT: &[&str] = &[
    "GPL", "LGPL", "AGPL", "SSPL", "EUPL", "OSL", "CDDL", "EPL", "CC-BY-SA", "CPAL", "RPL",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    // Ordered worst → best so OR picks the max and AND the min.
    Copyleft,
    Unknown,
    Permissive,
}

/// Classify an SPDX expression (also the legacy `MIT/Apache-2.0` form).
pub fn classify(expr: &str) -> Class {
    let tokens = tokenize(expr);
    let mut pos = 0;
    let class = parse_or(&tokens, &mut pos);
    if pos != tokens.len() {
        Class::Unknown
    } else {
        class
    }
}

fn tokenize(expr: &str) -> Vec<String> {
    let spaced = expr
        .replace('(', " ( ")
        .replace(')', " ) ")
        .replace('/', " OR ");
    spaced.split_whitespace().map(str::to_string).collect()
}

fn parse_or(t: &[String], pos: &mut usize) -> Class {
    let mut c = parse_and(t, pos);
    while t.get(*pos).is_some_and(|s| s.eq_ignore_ascii_case("OR")) {
        *pos += 1;
        c = c.max(parse_and(t, pos));
    }
    c
}

fn parse_and(t: &[String], pos: &mut usize) -> Class {
    let mut c = parse_atom(t, pos);
    while t.get(*pos).is_some_and(|s| s.eq_ignore_ascii_case("AND")) {
        *pos += 1;
        c = c.min(parse_atom(t, pos));
    }
    c
}

fn parse_atom(t: &[String], pos: &mut usize) -> Class {
    let Some(tok) = t.get(*pos) else {
        return Class::Unknown;
    };
    *pos += 1;
    let c = if tok == "(" {
        let c = parse_or(t, pos);
        if t.get(*pos).map(String::as_str) == Some(")") {
            *pos += 1;
        }
        c
    } else {
        classify_id(tok)
    };
    // `X WITH exception`: an exception only adds permissions, so the class is X's.
    if t.get(*pos).is_some_and(|s| s.eq_ignore_ascii_case("WITH")) {
        *pos += 2;
    }
    c
}

fn classify_id(id: &str) -> Class {
    let id = id.trim_end_matches('+');
    if PERMISSIVE.iter().any(|p| p.eq_ignore_ascii_case(id)) {
        Class::Permissive
    } else if COPYLEFT
        .iter()
        .any(|p| id.to_ascii_uppercase().starts_with(&p.to_ascii_uppercase()))
    {
        Class::Copyleft
    } else {
        Class::Unknown
    }
}

#[derive(Debug, Clone)]
pub struct Component {
    pub name: String,
    pub version: String,
    pub license: String,
    pub class: Class,
    pub repository: String,
    /// `(file name, text)` of every licence/notice file the crate ships.
    pub texts: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct Report {
    pub components: Vec<Component>,
    pub markdown: String,
}

impl Report {
    pub fn copyleft(&self) -> Vec<&Component> {
        self.components
            .iter()
            .filter(|c| c.class == Class::Copyleft)
            .collect()
    }
    pub fn unknown(&self) -> Vec<&Component> {
        self.components
            .iter()
            .filter(|c| c.class == Class::Unknown)
            .collect()
    }
    pub fn missing_text(&self) -> Vec<&Component> {
        self.components
            .iter()
            .filter(|c| c.texts.is_empty())
            .collect()
    }
}

/// Collect the licences for `build`. `native_dirs` are ONNX Runtime prebuilt directories
/// whose notice files are bundled too.
pub fn collect(
    root: &Path,
    build: &Build,
    version: &str,
    native_dirs: &[PathBuf],
    tray_target: Option<&str>,
) -> Result<Report> {
    let (no_default, features) = build.cargo_features();
    let mut components = crates_of(root, BIN, &build.target, no_default, &features)?;
    // The desktop tray ships in the same archive: its crates are listed too (for the
    // Linux musl lite archive, those of its glibc build).
    if let Some(t) = tray_target {
        for c in crates_of(root, TRAY_PKG, t, false, &[])? {
            if !components
                .iter()
                .any(|o| o.name == c.name && o.version == c.version)
            {
                components.push(c);
            }
        }
    }
    components.sort_by(|a, b| {
        (a.name.as_str(), a.version.as_str()).cmp(&(b.name.as_str(), b.version.as_str()))
    });

    // Only when ONNX Runtime is really in this build's graph (the binary's `ocr` feature
    // pulls it in through bunko-ocr).
    let ort_version = components
        .iter()
        .find(|c| c.name == "ort-sys")
        .map(|c| c.version.as_str());
    let markdown = render(
        build,
        version,
        &components,
        ort_version,
        native_dirs,
        tray_target,
    );
    Ok(Report {
        components,
        markdown,
    })
}

/// The third-party crates in the normal-dependency graph of workspace package `pkg`
/// for `target` and the given features.
fn crates_of(
    root: &Path,
    pkg: &str,
    target: &str,
    no_default: bool,
    features: &[&str],
) -> Result<Vec<Component>> {
    let mut args = vec!["--filter-platform".to_string(), target.to_string()];
    if no_default {
        args.push("--no-default-features".into());
    }
    if !features.is_empty() {
        args.push("--features".into());
        args.push(
            features
                .iter()
                .map(|f| format!("{pkg}/{f}"))
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    let meta = util::metadata(root, &args)?;
    let packages: HashMap<&str, &Value> = meta["packages"]
        .as_array()
        .context("metadata: packages")?
        .iter()
        .filter_map(|p| Some((p["id"].as_str()?, p)))
        .collect();
    let nodes: HashMap<&str, &Value> = meta["resolve"]["nodes"]
        .as_array()
        .context("metadata: resolve")?
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n)))
        .collect();
    let root_id = packages
        .iter()
        .find(|(_, p)| p["name"] == pkg && p["source"].is_null())
        .map(|(id, _)| *id)
        .with_context(|| format!("{pkg} is not in the metadata"))?;

    // Normal (non-dev, non-build) dependencies reachable from the binary.
    let mut seen = BTreeSet::new();
    let mut stack = vec![root_id];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(node) = nodes.get(id) else { continue };
        for dep in node["deps"].as_array().into_iter().flatten() {
            let normal = dep["dep_kinds"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|k| k["kind"].is_null());
            if normal && let Some(pkg) = dep["pkg"].as_str() {
                stack.push(pkg);
            }
        }
    }

    let mut components = Vec::new();
    for id in seen {
        let p = packages[id];
        if p["source"].is_null() {
            continue; // our own workspace crates: MPL-2.0, covered by LICENSE
        }
        let license = p["license"].as_str().unwrap_or("").to_string();
        let manifest = PathBuf::from(p["manifest_path"].as_str().unwrap_or_default());
        let dir = manifest.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut texts = licence_files(&dir);
        if let Some(lf) = p["license_file"].as_str() {
            let path = dir.join(lf);
            if !texts.iter().any(|(n, _)| {
                Path::new(lf)
                    .file_name()
                    .is_some_and(|f| f.to_string_lossy() == *n)
            }) && let Ok(t) = std::fs::read_to_string(&path)
            {
                texts.push((lf.to_string(), t));
            }
        }
        // A crate with only `license-file` needs a human to read it.
        let class = if license.is_empty() {
            Class::Unknown
        } else {
            classify(&license)
        };
        components.push(Component {
            name: p["name"].as_str().unwrap_or_default().to_string(),
            version: p["version"].as_str().unwrap_or_default().to_string(),
            license: if license.is_empty() {
                "(see licence file)".into()
            } else {
                license
            },
            class,
            repository: p["repository"].as_str().unwrap_or_default().to_string(),
            texts,
        });
    }
    Ok(components)
}

fn licence_files(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut names: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name())
        .collect();
    names.sort();
    for name in names {
        let n = name.to_string_lossy().to_string();
        let upper = n.to_ascii_uppercase();
        let is_licence = [
            "LICENSE",
            "LICENCE",
            "COPYING",
            "NOTICE",
            "COPYRIGHT",
            "UNLICENSE",
            "AUTHORS",
        ]
        .iter()
        .any(|p| upper.starts_with(p));
        if is_licence && let Ok(t) = std::fs::read_to_string(dir.join(&name)) {
            out.push((n, t));
        }
    }
    out
}

const ORT_MIT: &str = "MIT License\n\nCopyright (c) Microsoft Corporation\n\nPermission is hereby granted, free of charge, to any person obtaining a copy\nof this software and associated documentation files (the \"Software\"), to deal\nin the Software without restriction, including without limitation the rights\nto use, copy, modify, merge, publish, distribute, sublicense, and/or sell\ncopies of the Software, and to permit persons to whom the Software is\nfurnished to do so, subject to the following conditions:\n\nThe above copyright notice and this permission notice shall be included in all\ncopies or substantial portions of the Software.\n\nTHE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR\nIMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,\nFITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE\nAUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER\nLIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,\nOUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE\nSOFTWARE.\n";

fn fence(text: &str) -> String {
    // A fence longer than any backtick run inside the text.
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

fn render(
    build: &Build,
    version: &str,
    components: &[Component],
    ort_version: Option<&str>,
    native_dirs: &[PathBuf],
    tray_target: Option<&str>,
) -> String {
    let mut md = String::new();
    let _ = writeln!(md, "# Third-party licences\n");
    let _ = writeln!(
        md,
        "mokuro-bunko {version} ({}, {}). mokuro-bunko itself is licensed under the \
         Mozilla Public License 2.0 (see the LICENSE file). This build contains the third-party \
         components listed below; each one's licence text follows the table.\n",
        build.target,
        build.manifest_flavor()
    );
    let full = build.flavor == Flavor::Full && ort_version.is_some();
    if let Some(t) = tray_target {
        let _ = writeln!(md, "## Desktop tray (`mokuro-bunko-tray`)\n");
        let _ = writeln!(
            md,
            "The archive also holds `mokuro-bunko-tray` ({t}), the desktop tray; its crates \
             are in the table below. Its icons are original artwork of this project \
             (`packaging/icons/`, MPL-2.0)."
        );
        if t.contains("linux") {
            let _ = writeln!(
                md,
                "\nOn Linux it uses the system's **GTK 3** (and its GLib, Pango, Cairo, \
                 GDK-Pixbuf, ATK) and **libayatana-appindicator3** (or libappindicator3), \
                 dynamically linked / loaded at run time from the distribution's packages \
                 (LGPL-2.1+/LGPL-3); none of them is shipped in this archive."
            );
        }
        let _ = writeln!(md);
    }
    if full {
        let _ = writeln!(md, "## Native components\n");
        let _ = writeln!(
            md,
            "- **ONNX Runtime** (prebuilt by pyke for ort-sys {}), © Microsoft Corporation, MIT. \
             Statically linked{}.",
            ort_version.unwrap_or_default(),
            if build.ep == Ep::Cuda {
                "; its CUDA execution provider ships as shared libraries next to the executable"
            } else {
                ""
            }
        );
        if matches!(build.ep, Ep::Directml) || (build.ep == Ep::Cuda && build.is_windows()) {
            let _ = writeln!(
                md,
                "- **DirectML** (`DirectML.dll`, when present), © Microsoft Corporation, redistributed \
                 under the Microsoft DirectML licence (not an open-source licence; see \
                 https://www.nuget.org/packages/Microsoft.AI.DirectML)."
            );
        }
        if build.ep == Ep::Cuda {
            let _ = writeln!(
                md,
                "- **NVIDIA CUDA / cuDNN** are NOT included: the CUDA execution provider loads them \
                 from the system (or from the `nvidia/cuda` base image, under NVIDIA's licence)."
            );
        }
        let _ = writeln!(md);
    }
    let _ = writeln!(md, "## Rust crates\n");
    let _ = writeln!(md, "| Crate | Version | Licence |\n|---|---|---|");
    for c in components {
        let flag = match c.class {
            Class::Permissive => "",
            Class::Unknown => " ⚠ unreviewed",
            Class::Copyleft => " ⛔ copyleft",
        };
        let _ = writeln!(md, "| {} | {} | {}{flag} |", c.name, c.version, c.license);
    }

    let _ = writeln!(md, "\n## Licence texts\n");
    if full {
        let _ = writeln!(
            md,
            "### ONNX Runtime\n\n{f}text\n{ORT_MIT}{f}\n",
            f = fence(ORT_MIT)
        );
        for dir in native_dirs {
            for (name, text) in licence_files(dir).into_iter().chain(notice_txts(dir)) {
                let f = fence(&text);
                let _ = writeln!(
                    md,
                    "#### ONNX Runtime: {name}\n\n{f}text\n{}\n{f}\n",
                    text.trim_end()
                );
            }
        }
    }
    // Group crates that ship byte-identical texts (most Apache-2.0 copies are).
    let mut by_text: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    for c in components {
        let id = format!("{} {}", c.name, c.version);
        if c.texts.is_empty() {
            let text = format!(
                "(no licence file in the published crate; licence: {}{})",
                c.license,
                if c.repository.is_empty() {
                    String::new()
                } else {
                    format!(", source: {}", c.repository)
                }
            );
            by_text
                .entry(text.clone())
                .or_insert_with(|| ("licence".into(), Vec::new()))
                .1
                .push(id.clone());
            continue;
        }
        for (name, text) in &c.texts {
            by_text
                .entry(text.trim().to_string())
                .or_insert_with(|| (name.clone(), Vec::new()))
                .1
                .push(id.clone());
        }
    }
    let mut groups: Vec<_> = by_text.into_iter().collect();
    groups.sort_by(|a, b| a.1.1.first().cmp(&b.1.1.first()));
    for (text, (name, users)) in groups {
        let f = fence(&text);
        let _ = writeln!(
            md,
            "### {} ({name})\n\n{f}text\n{text}\n{f}\n",
            users.join(", ")
        );
    }
    md
}

/// `ThirdPartyNotices.txt` and the like inside an ONNX Runtime prebuilt directory.
fn notice_txts(dir: &Path) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_ascii_lowercase();
            n.ends_with(".txt")
                && (n.contains("notice") || n.contains("license") || n.contains("licence"))
        })
        .filter_map(|e| {
            Some((
                e.file_name().to_string_lossy().to_string(),
                std::fs::read_to_string(e.path()).ok()?,
            ))
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spdx_classes() {
        assert_eq!(classify("MIT"), Class::Permissive);
        assert_eq!(classify("MIT OR Apache-2.0"), Class::Permissive);
        assert_eq!(classify("MIT/Apache-2.0"), Class::Permissive);
        assert_eq!(
            classify("Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT"),
            Class::Permissive
        );
        assert_eq!(
            classify("(MIT OR Apache-2.0) AND Unicode-3.0"),
            Class::Permissive
        );
        assert_eq!(classify("Apache-2.0 AND ISC"), Class::Permissive);
        assert_eq!(classify("GPL-3.0-only"), Class::Copyleft);
        assert_eq!(classify("MIT OR GPL-3.0"), Class::Permissive);
        assert_eq!(classify("MIT AND LGPL-2.1-or-later"), Class::Copyleft);
        assert_eq!(
            classify("MIT OR Apache-2.0 OR LGPL-2.1-or-later"),
            Class::Permissive
        );
        assert_eq!(classify("LicenseRef-ring"), Class::Unknown);
        assert_eq!(classify("MIT AND LicenseRef-x"), Class::Unknown);
        assert_eq!(classify("GPL-2.0+"), Class::Copyleft);
    }

    #[test]
    fn fences_outgrow_backticks() {
        assert_eq!(fence("plain"), "```");
        assert_eq!(fence("has ```` four"), "`````");
    }
}
