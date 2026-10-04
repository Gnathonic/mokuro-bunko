//! The Microsoft Visual C++ 2015-2022 runtime, shipped app-local in the Windows release
//! (next to `mokuro-bunko.exe`) and the Windows OCR backend packs (`lib/`), so neither
//! needs the "VC++ Redistributable" installed: libtorch's DLLs import `msvcp140.dll` /
//! `vcruntime140*.dll`, its CPU kernels and the compiled CPU packages `vcomp140.dll`
//! (OpenMP), and a Rust MSVC binary `vcruntime140*.dll`.
//!
//! Licence: these files are "Distributable Code" of Visual Studio 2022 (its licence
//! terms, "Distributable Code" section; the list:
//! <https://learn.microsoft.com/visualstudio/releases/2022/redistribution#visual-c-runtime-files>),
//! and app-local deployment (copying them next to the application) is a supported way
//! to deploy them (<https://learn.microsoft.com/cpp/windows/deployment-in-visual-cpp>).
//! They are taken from the `VC\Redist\MSVC\<version>\x64\Microsoft.VC143.CRT` and
//! `...\Microsoft.VC143.OpenMP` folders of the Visual Studio that builds the release.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Every runtime DLL the release may need, in the redist folders.
pub const VC_DLLS: &[&str] = &[
    "vcruntime140.dll",
    "vcruntime140_1.dll",
    "msvcp140.dll",
    "msvcp140_1.dll",
    "msvcp140_2.dll",
    "msvcp140_atomic_wait.dll",
    "msvcp140_codecvt_ids.dll",
    "concrt140.dll",
    "vcomp140.dll",
];

/// What a Windows backend pack ships: the C++ runtime libtorch imports and OpenMP, which
/// the compiled CPU packages import (the pack's own libraries do not show that).
pub const PACK_DLLS: &[&str] = &[
    "vcruntime140.dll",
    "vcruntime140_1.dll",
    "msvcp140.dll",
    "msvcp140_1.dll",
    "msvcp140_2.dll",
    "vcomp140.dll",
];

/// The licence note written next to the files.
pub const NOTICE: &str = "Microsoft Visual C++ 2015-2022 runtime (vcruntime140*.dll, msvcp140*.dll, \
vcomp140.dll): Distributable Code of Microsoft Visual Studio 2022, deployed app-local under its \
licence terms (https://learn.microsoft.com/visualstudio/releases/2022/redistribution#visual-c-runtime-files). \
Copyright (c) Microsoft Corporation.";

pub fn is_vc_dll(name: &str) -> bool {
    VC_DLLS.iter().any(|d| d.eq_ignore_ascii_case(name))
}

/// The x64 redist folders: `$VC_REDIST_DIR` (a folder holding the DLLs, or the
/// `...\Redist\MSVC\<version>` folder), `$VCToolsRedistDir` (set in a VS developer
/// prompt), else the newest `VC\Redist\MSVC\14.*` of any installed Visual Studio.
pub fn redist_dirs() -> Result<Vec<PathBuf>> {
    let roots: Vec<PathBuf> = ["VC_REDIST_DIR", "VCToolsRedistDir"]
        .iter()
        .filter_map(|v| {
            std::env::var_os(v)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .collect();
    let candidates = if roots.is_empty() {
        installed_redist_roots()
    } else {
        roots
    };
    for root in candidates {
        let dirs = folders_under(&root);
        if !dirs.is_empty() {
            return Ok(dirs);
        }
    }
    bail!(
        "no Visual C++ redist folder found (install Visual Studio 2022 with the C++ tools, \
         or set VC_REDIST_DIR to the folder holding vcruntime140.dll)"
    )
}

/// `<root>` itself when it holds the DLLs, else its `x64\Microsoft.VC14*.CRT` and
/// `x64\Microsoft.VC14*.OpenMP` folders.
fn folders_under(root: &Path) -> Vec<PathBuf> {
    if root.join("vcruntime140.dll").is_file() {
        return vec![root.to_path_buf()];
    }
    let x64 = root.join("x64");
    let Ok(rd) = std::fs::read_dir(&x64) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            n.starts_with("Microsoft.VC14") && (n.ends_with(".CRT") || n.ends_with(".OpenMP"))
        })
        .collect();
    dirs.sort();
    dirs
}

fn installed_redist_roots() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for pf in ["ProgramFiles", "ProgramFiles(x86)"] {
        let Some(base) = std::env::var_os(pf) else {
            continue;
        };
        let vs = Path::new(&base).join("Microsoft Visual Studio");
        for year in read_sorted(&vs) {
            for edition in read_sorted(&year) {
                let msvc = edition.join("VC").join("Redist").join("MSVC");
                // Newest version first; skip the `v143` alias folders without DLLs.
                for v in read_sorted(&msvc).into_iter().rev() {
                    out.push(v);
                }
            }
        }
    }
    out
}

fn read_sorted(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Find each of `names` in the redist folders.
pub fn find(names: &[&str]) -> Result<Vec<PathBuf>> {
    let dirs = redist_dirs()?;
    names
        .iter()
        .map(|n| {
            dirs.iter()
                .map(|d| d.join(n))
                .find(|p| p.is_file())
                .with_context(|| {
                    format!(
                        "{n} is not in the Visual C++ redist folders ({})",
                        dirs.iter()
                            .map(|d| d.display().to_string())
                            .collect::<Vec<_>>()
                            .join("; ")
                    )
                })
        })
        .collect()
}

/// The VC runtime DLLs a PE file imports.
pub fn imported_by(file: &Path) -> Result<Vec<String>> {
    let bytes = std::fs::read(file)?;
    let mut out = Vec::new();
    if let Ok(goblin::Object::PE(pe)) = goblin::Object::parse(&bytes) {
        for lib in pe.libraries {
            if is_vc_dll(lib) && !out.iter().any(|o: &String| o.eq_ignore_ascii_case(lib)) {
                out.push(lib.to_ascii_lowercase());
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redist_folder_layouts() {
        let tmp = tempfile::tempdir().unwrap();
        // A plain folder with the DLLs.
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("vcruntime140.dll"), b"x").unwrap();
        assert_eq!(folders_under(&plain), vec![plain.clone()]);
        // A VS `Redist\MSVC\<version>` folder.
        let ver = tmp.path().join("14.44.35112");
        for d in [
            "x64/Microsoft.VC143.CRT",
            "x64/Microsoft.VC143.OpenMP",
            "x64/Microsoft.VC143.MFC",
        ] {
            std::fs::create_dir_all(ver.join(d)).unwrap();
        }
        let dirs = folders_under(&ver);
        assert_eq!(dirs.len(), 2);
        assert!(
            dirs[0].ends_with("Microsoft.VC143.CRT") && dirs[1].ends_with("Microsoft.VC143.OpenMP")
        );
        assert!(is_vc_dll("VCOMP140.DLL") && !is_vc_dll("kernel32.dll"));
        assert!(PACK_DLLS.iter().all(|d| is_vc_dll(d)));
    }
}
