//! Where the files of an installed copy are, and how an update replaces them.
//!
//! Since 0.7.0-beta.3 the tray is part of the program (`mokuro-bunko tray`); there is no
//! separate `mokuro-bunko-tray` any more. What an update replaces besides the running
//! executable:
//!
//! * **Windows**: the pair `bin\mokuro-bunko.exe` (the command line) and
//!   `Mokuro Bunko.exe` above it (the same program built for the GUI subsystem: it runs
//!   the tray without a console window). An install laid out before beta.3 has
//!   `mokuro-bunko.exe` at the top; whichever of the three exist are replaced.
//! * **macOS**: the app bundle around the program (`Mokuro Bunko.app` from the disk
//!   image, `mokuro-bunko.app` from an archive): its `Info.plist`, `PkgInfo` and
//!   `Resources` come from the new release too, a leftover `mokuro-bunko-tray` is
//!   removed, and the bundle is sealed again (`codesign --force --deep -s -`), so
//!   `codesign --verify` passes after the update as it did before.
//! * **Linux**: just the executable.
//!
//! A release archive is unpacked into a staging folder next to the executable first
//! ([`unpack`]); [`Plan::commit`] then puts the files in place.

use crate::UpdateError;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The Windows program that runs the tray (GUI subsystem: no console window).
pub const WINDOWS_GUI_EXE: &str = "Mokuro Bunko.exe";
/// The Windows command line, in the `bin` folder of an install.
pub const WINDOWS_CLI_EXE: &str = "mokuro-bunko.exe";
/// The separate tray program of 0.7.0-beta.2 and earlier.
pub const LEGACY_TRAY: &str = if cfg!(windows) {
    "mokuro-bunko-tray.exe"
} else {
    "mokuro-bunko-tray"
};
/// `IMAGE_SUBSYSTEM_WINDOWS_GUI` / `_CUI` in a PE optional header.
pub const PE_SUBSYSTEM_GUI: u16 = 2;
pub const PE_SUBSYSTEM_CONSOLE: u16 = 3;

/// The PE header's subsystem field: its offset in `bytes` (an `.exe`).
fn pe_subsystem_offset(bytes: &[u8]) -> Option<usize> {
    if bytes.get(..2)? != b"MZ" {
        return None;
    }
    let e_lfanew = u32::from_le_bytes(bytes.get(0x3c..0x40)?.try_into().ok()?) as usize;
    if bytes.get(e_lfanew..e_lfanew + 4)? != b"PE\0\0" {
        return None;
    }
    // Optional header after the 4-byte signature and the 20-byte file header; its magic
    // is 0x10b (PE32) or 0x20b (PE32+); Subsystem is at offset 68 in both.
    let opt = e_lfanew + 24;
    let magic = u16::from_le_bytes(bytes.get(opt..opt + 2)?.try_into().ok()?);
    if magic != 0x10b && magic != 0x20b {
        return None;
    }
    let at = opt + 68;
    bytes.get(at..at + 2)?;
    Some(at)
}

/// The subsystem of a Windows executable (2 = GUI, 3 = console), if it is one.
pub fn pe_subsystem(bytes: &[u8]) -> Option<u16> {
    let at = pe_subsystem_offset(bytes)?;
    Some(u16::from_le_bytes([bytes[at], bytes[at + 1]]))
}

/// Set the subsystem of a Windows executable. `Mokuro Bunko.exe` is `mokuro-bunko.exe`
/// with the GUI subsystem: Rust links both with the same entry point
/// (`mainCRTStartup`), so the field is the only difference (`windows_subsystem` only
/// changes the linker's `/SUBSYSTEM`), and Windows does not check the PE checksum of an
/// application.
pub fn set_pe_subsystem(bytes: &mut [u8], subsystem: u16) -> Result<(), String> {
    let at = pe_subsystem_offset(bytes).ok_or("not a Windows executable (PE)")?;
    bytes[at..at + 2].copy_from_slice(&subsystem.to_le_bytes());
    Ok(())
}

/// Write the GUI-subsystem copy of the Windows executable `cli` to `out`.
pub fn write_gui_copy(cli: &Path, out: &Path) -> std::io::Result<()> {
    let mut bytes = std::fs::read(cli)?;
    set_pe_subsystem(&mut bytes, PE_SUBSYSTEM_GUI)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(out, bytes)
}

/// `exe` is a Windows `bin\` folder's command line: the install root is above it.
fn windows_root(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let in_bin = dir
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case("bin"));
    Some(if in_bin {
        dir.parent()?.to_path_buf()
    } else {
        dir.to_path_buf()
    })
}

/// The app bundle `exe` is the main program of (`X.app/Contents/MacOS/<exe>`).
pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension().is_some_and(|e| e == "app"))
    .then(|| bundle.to_path_buf())
}

/// What an update of the program `exe` replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The running executable (replaced with `self_replace`).
    pub exe: PathBuf,
    /// Whether `exe` is the Windows GUI build (it then takes the new GUI build).
    pub exe_is_gui: bool,
    /// Other copies of the command line in this install (replaced by rename).
    pub cli_copies: Vec<PathBuf>,
    /// Copies of the Windows GUI build (`Mokuro Bunko.exe`).
    pub gui_copies: Vec<PathBuf>,
    /// macOS app bundles of this install (refreshed and sealed again).
    pub bundles: Vec<PathBuf>,
}

impl Plan {
    /// The plan for the installed copy whose executable is `exe`, as laid out on disk now.
    pub fn for_exe(exe: &Path) -> Plan {
        let mut plan = Plan {
            exe: exe.to_path_buf(),
            exe_is_gui: false,
            cli_copies: Vec::new(),
            gui_copies: Vec::new(),
            bundles: Vec::new(),
        };
        if cfg!(windows) {
            plan.add_windows();
        } else if cfg!(target_os = "macos") {
            plan.add_macos();
        }
        plan
    }

    /// The Windows layout under `root` (every platform, for tests).
    pub fn add_windows(&mut self) {
        let Some(root) = windows_root(&self.exe) else {
            return;
        };
        self.exe_is_gui = self
            .exe
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case(WINDOWS_GUI_EXE));
        for cli in [
            root.join("bin").join(WINDOWS_CLI_EXE),
            root.join(WINDOWS_CLI_EXE),
        ] {
            if cli.is_file() && !same_file(&cli, &self.exe) {
                self.cli_copies.push(cli);
            }
        }
        let gui = root.join(WINDOWS_GUI_EXE);
        if gui.is_file() && !same_file(&gui, &self.exe) {
            self.gui_copies.push(gui);
        }
    }

    /// The macOS layout (every platform, for tests): the bundle around `exe`, the
    /// archive's `mokuro-bunko.app` (or an app copied) next to it, and the command line
    /// next to an archive's bundle.
    pub fn add_macos(&mut self) {
        let mut bundles = Vec::new();
        if let Some(b) = bundle_of(&self.exe) {
            if b.file_name().is_some_and(|n| n == "mokuro-bunko.app")
                && let Some(outer) = b.parent().map(|d| d.join("mokuro-bunko"))
                && outer.is_file()
            {
                self.cli_copies.push(outer);
            }
            bundles.push(b);
        }
        if let Some(dir) = self.exe.parent() {
            for name in ["mokuro-bunko.app", "Mokuro Bunko.app"] {
                let b = dir.join(name);
                if b.join("Contents").is_dir() && !bundles.contains(&b) {
                    bundles.push(b);
                }
            }
        }
        for b in &bundles {
            let cli = b.join("Contents/MacOS/mokuro-bunko");
            if cli.is_file() && !same_file(&cli, &self.exe) && !self.cli_copies.contains(&cli) {
                self.cli_copies.push(cli);
            }
        }
        self.bundles = bundles;
    }

    /// Every file of this install that holds the program, the running one first.
    pub fn programs(&self) -> Vec<PathBuf> {
        let mut out = vec![self.exe.clone()];
        out.extend(self.cli_copies.iter().cloned());
        out.extend(self.gui_copies.iter().cloned());
        out
    }

    /// Put the unpacked release in place: the running executable first (its failure
    /// changes nothing), then the other copies and the bundles, best effort.
    pub fn commit(&self, new: &Unpacked) -> Result<(), UpdateError> {
        let gui_new = match (&new.gui, self.exe_is_gui || !self.gui_copies.is_empty()) {
            (Some(g), _) => Some(g.clone()),
            (None, true) => {
                let g = new.dir.join(WINDOWS_GUI_EXE);
                write_gui_copy(&new.cli, &g)?;
                Some(g)
            }
            (None, false) => None,
        };
        let main = if self.exe_is_gui {
            gui_new.clone().unwrap_or_else(|| new.cli.clone())
        } else {
            new.cli.clone()
        };
        self_replace::self_replace(&main).map_err(UpdateError::Io)?;
        for copy in &self.cli_copies {
            if let Err(e) = install_copy(&new.cli, copy) {
                tracing::warn!("update: {} not replaced: {e}", copy.display());
            }
        }
        if let Some(g) = &gui_new {
            for copy in &self.gui_copies {
                if let Err(e) = install_copy(g, copy) {
                    tracing::warn!("update: {} not replaced: {e}", copy.display());
                }
            }
        }
        for b in &self.bundles {
            if let Some(contents) = &new.app_contents
                && let Err(e) = refresh_bundle(b, contents)
            {
                tracing::warn!("update: {} not refreshed: {e}", b.display());
            }
            remove_legacy_tray(&b.join("Contents/MacOS"));
            seal_bundle(b);
        }
        Ok(())
    }
}

/// Two paths name the same file (or the same path).
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Copy `src` over `dest` (an executable) through a temporary file and a rename.
pub fn install_copy(src: &Path, dest: &Path) -> std::io::Result<()> {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dest.with_file_name(format!(".{name}.new"));
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(src, &tmp)?;
    set_executable(&tmp)?;
    let r = replace_file(&tmp, dest);
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Put `new` in place of `target`. Windows cannot overwrite a running executable but can
/// rename it, so the old one moves aside (`.old`, removed now or by the next update).
pub fn replace_file(new: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        if !target.exists() {
            return std::fs::rename(new, target);
        }
        let old = target.with_extension("exe.old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(target, &old)?;
        if let Err(e) = std::fs::rename(new, target) {
            let _ = std::fs::rename(&old, target);
            return Err(e);
        }
        let _ = std::fs::remove_file(&old);
        Ok(())
    }
    #[cfg(not(windows))]
    std::fs::rename(new, target)
}

pub fn set_executable(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Copy the new bundle's `Contents` (everything but the program in `MacOS/`, which
/// [`Plan::commit`] replaces) into the installed bundle `bundle`.
pub fn refresh_bundle(bundle: &Path, contents: &Path) -> std::io::Result<()> {
    let dest = bundle.join("Contents");
    let mut files = Vec::new();
    walk(contents, Path::new(""), &mut files)?;
    for rel in files {
        if rel.starts_with("MacOS") || rel.starts_with("_CodeSignature") {
            continue;
        }
        let to = dest.join(&rel);
        if let Some(dir) = to.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let name = to
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tmp = to.with_file_name(format!(".{name}.new"));
        std::fs::copy(contents.join(&rel), &tmp)?;
        std::fs::rename(&tmp, &to)?;
    }
    Ok(())
}

fn walk(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for e in std::fs::read_dir(root.join(rel))? {
        let e = e?;
        let r = rel.join(e.file_name());
        if e.file_type()?.is_dir() {
            walk(root, &r, out)?;
        } else {
            out.push(r);
        }
    }
    Ok(())
}

/// Remove the separate tray program of 0.7.0-beta.2 and earlier (and its update backup)
/// from `dir`. Returns what was removed.
pub fn remove_legacy_tray(dir: &Path) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    for name in [
        LEGACY_TRAY.to_string(),
        format!(".{LEGACY_TRAY}.previous"),
        format!(".{LEGACY_TRAY}.new"),
    ] {
        let p = dir.join(&name);
        if p.is_file() && std::fs::remove_file(&p).is_ok() {
            removed.push(p);
        }
    }
    removed
}

/// Seal a macOS app bundle again (ad hoc: `codesign --force --deep -s -`) after files in
/// it changed, so `codesign --verify` passes. codesign writes the signed program as a new
/// file, so a running copy is not disturbed. Best effort; nothing to do elsewhere.
pub fn seal_bundle(bundle: &Path) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let out = std::process::Command::new("codesign")
        .args(["--force", "--deep", "-s", "-"])
        .arg(bundle)
        .stdin(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => tracing::info!("sealed {} (ad hoc)", bundle.display()),
        Ok(o) => tracing::warn!(
            "codesign {}: {} {}",
            bundle.display(),
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => tracing::warn!("codesign {}: {e}", bundle.display()),
    }
}

/// A release unpacked next to the executable.
#[derive(Debug, Clone)]
pub struct Unpacked {
    /// The staging folder (removed by [`Unpacked::remove`]).
    pub dir: PathBuf,
    /// The new command line (runnable: the prefetch step runs it).
    pub cli: PathBuf,
    /// The new Windows GUI build, when the archive has one.
    pub gui: Option<PathBuf>,
    /// The new app bundle's `Contents` (macOS).
    pub app_contents: Option<PathBuf>,
}

impl Unpacked {
    pub fn remove(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The kind of a release download, from its URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    TarGz,
    Zip,
    Dmg,
    /// A bare executable.
    Bare,
}

impl Kind {
    pub fn of(url: &str) -> Kind {
        let lower = url.to_ascii_lowercase();
        if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            Kind::TarGz
        } else if lower.ends_with(".zip") {
            Kind::Zip
        } else if lower.ends_with(".dmg") {
            Kind::Dmg
        } else {
            Kind::Bare
        }
    }
}

/// Unpack the downloaded `archive` (kind `kind`) into the folder `out`: the command line
/// named `binary` (found by file name; in a Windows zip `bin\` first), the Windows GUI
/// build and the macOS app's `Contents`.
pub fn unpack(
    archive: &Path,
    kind: Kind,
    binary: &str,
    out: &Path,
) -> Result<Unpacked, UpdateError> {
    let _ = std::fs::remove_dir_all(out);
    std::fs::create_dir_all(out)?;
    let cli = out.join(binary);
    let mut un = Unpacked {
        dir: out.to_path_buf(),
        cli: cli.clone(),
        gui: None,
        app_contents: None,
    };
    let r = match kind {
        Kind::TarGz => unpack_tar(archive, binary, &mut un),
        Kind::Zip => unpack_zip(archive, binary, &mut un),
        Kind::Dmg => unpack_dmg(archive, binary, &mut un),
        Kind::Bare => std::fs::copy(archive, &cli)
            .map(drop)
            .map_err(UpdateError::Io),
    };
    if let Err(e) = r.and_then(|()| set_executable(&cli).map_err(UpdateError::Io)) {
        un.remove();
        return Err(e);
    }
    Ok(un)
}

/// The path inside an archive's top folder (`<top>/<rest>` → `<rest>`).
fn below_top(path: &Path) -> PathBuf {
    path.components().skip(1).collect()
}

fn unpack_tar(archive: &Path, binary: &str, un: &mut Unpacked) -> Result<(), UpdateError> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(archive)?));
    let mut found = false;
    let contents = un.dir.join("app").join("Contents");
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        let rel = below_top(&path);
        // The command line: the first regular file of that name (the top-level one; the
        // copy in an archive's app bundle is a hard link to it).
        if !found
            && entry.header().entry_type().is_file()
            && path.file_name().is_some_and(|n| n == binary)
        {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            std::fs::write(&un.cli, bytes)?;
            found = true;
            continue;
        }
        // The app bundle's Contents, without its programs (the command line above is the
        // one program; a link entry has no data of its own).
        if let Ok(r) = rel.strip_prefix("mokuro-bunko.app/Contents")
            && entry.header().entry_type().is_file()
            && !r.starts_with("MacOS")
            && !r.as_os_str().is_empty()
            && safe_relative(r)
        {
            let to = contents.join(r);
            if let Some(d) = to.parent() {
                std::fs::create_dir_all(d)?;
            }
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            std::fs::write(&to, bytes)?;
            un.app_contents = Some(contents.clone());
        }
    }
    if !found {
        return Err(UpdateError::Unpack(format!(
            "{binary} is not in the archive"
        )));
    }
    Ok(())
}

/// No `..`, no root: a path that stays inside the folder it is joined to.
fn safe_relative(p: &Path) -> bool {
    p.components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
}

fn unpack_zip(archive: &Path, binary: &str, un: &mut Unpacked) -> Result<(), UpdateError> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?)
        .map_err(|e| UpdateError::Unpack(e.to_string()))?;
    let names: Vec<String> = zip.file_names().map(str::to_string).collect();
    let last = |n: &str| n.rsplit('/').next().unwrap_or(n).to_string();
    // The command line: `<top>/bin/<binary>` (0.7.0-beta.3+), else `<top>/<binary>`.
    let mut cli: Vec<&String> = names.iter().filter(|n| last(n) == binary).collect();
    cli.sort_by_key(|n| !n.contains("/bin/"));
    let name = cli
        .first()
        .ok_or_else(|| UpdateError::Unpack(format!("{binary} is not in the archive")))?;
    let mut bytes = Vec::new();
    zip.by_name(name)
        .map_err(|e| UpdateError::Unpack(e.to_string()))?
        .read_to_end(&mut bytes)?;
    std::fs::write(&un.cli, &bytes)?;
    if let Some(gui) = names
        .iter()
        .find(|n| last(n).eq_ignore_ascii_case(WINDOWS_GUI_EXE))
    {
        let mut bytes = Vec::new();
        zip.by_name(gui)
            .map_err(|e| UpdateError::Unpack(e.to_string()))?
            .read_to_end(&mut bytes)?;
        let out = un.dir.join(WINDOWS_GUI_EXE);
        std::fs::write(&out, bytes)?;
        un.gui = Some(out);
    }
    Ok(())
}

/// macOS disk image: attach it read-only and invisibly (`-nobrowse`: no Finder window,
/// no desktop icon), copy the app's `Contents` and detach.
fn unpack_dmg(archive: &Path, binary: &str, un: &mut Unpacked) -> Result<(), UpdateError> {
    if !cfg!(target_os = "macos") {
        return Err(UpdateError::Unpack(
            "a disk image can only be installed on macOS".into(),
        ));
    }
    let mnt = un.dir.join("mnt");
    std::fs::create_dir_all(&mnt)?;
    let attach = std::process::Command::new("hdiutil")
        .args([
            "attach",
            "-nobrowse",
            "-readonly",
            "-noautoopen",
            "-noverify",
            "-quiet",
        ])
        .arg("-mountpoint")
        .arg(&mnt)
        .arg(archive)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| UpdateError::Unpack(format!("hdiutil: {e}")))?;
    if !attach.status.success() {
        return Err(UpdateError::Unpack(format!(
            "hdiutil attach: {} {}",
            attach.status,
            String::from_utf8_lossy(&attach.stderr).trim()
        )));
    }
    let result = (|| -> Result<(), UpdateError> {
        let app = std::fs::read_dir(&mnt)?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "app") && p.join("Contents").is_dir())
            .ok_or_else(|| UpdateError::Unpack("the disk image holds no app".into()))?;
        let contents = un.dir.join("app").join("Contents");
        copy_tree(&app.join("Contents"), &contents)?;
        let cli = contents.join("MacOS").join(binary);
        if !cli.is_file() {
            return Err(UpdateError::Unpack(format!(
                "{binary} is not in {}",
                app.display()
            )));
        }
        std::fs::copy(&cli, &un.cli)?;
        un.app_contents = Some(contents);
        Ok(())
    })();
    detach(&mnt);
    let _ = std::fs::remove_dir(&mnt);
    result
}

fn detach(mnt: &Path) {
    for force in [false, true] {
        let mut cmd = std::process::Command::new("hdiutil");
        cmd.arg("detach").arg("-quiet");
        if force {
            cmd.arg("-force");
        }
        if cmd
            .arg(mnt)
            .stdin(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    tracing::warn!("update: could not detach {}", mnt.display());
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let ft = e.file_type()?;
        let dest = to.join(e.file_name());
        if ft.is_dir() {
            copy_tree(&e.path(), &dest)?;
        } else if ft.is_file() {
            std::fs::copy(e.path(), &dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal PE32+ header: MZ, e_lfanew = 0x80, "PE\0\0", file header, optional
    /// header magic 0x20b and the subsystem.
    fn fake_exe(subsystem: u16) -> Vec<u8> {
        let mut b = vec![0u8; 0x200];
        b[0..2].copy_from_slice(b"MZ");
        b[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        b[0x98..0x9a].copy_from_slice(&0x20bu16.to_le_bytes());
        b[0x98 + 68..0x98 + 70].copy_from_slice(&subsystem.to_le_bytes());
        b
    }

    #[test]
    fn pe_subsystem_patch() {
        let mut b = fake_exe(PE_SUBSYSTEM_CONSOLE);
        assert_eq!(pe_subsystem(&b), Some(3));
        set_pe_subsystem(&mut b, PE_SUBSYSTEM_GUI).unwrap();
        assert_eq!(pe_subsystem(&b), Some(2));
        let orig = fake_exe(PE_SUBSYSTEM_CONSOLE);
        let diff: Vec<usize> = (0..b.len()).filter(|&i| b[i] != orig[i]).collect();
        assert_eq!(diff, [0x98 + 68]);
        assert!(set_pe_subsystem(&mut b"#!/bin/sh\n".to_vec(), 2).is_err());
        assert_eq!(pe_subsystem(b"MZ"), None);
    }

    #[test]
    fn windows_plan() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        for f in ["bin/mokuro-bunko.exe", "Mokuro Bunko.exe"] {
            std::fs::write(root.join(f), b"x").unwrap();
        }
        let mut p = Plan {
            exe: root.join("bin/mokuro-bunko.exe"),
            exe_is_gui: false,
            cli_copies: vec![],
            gui_copies: vec![],
            bundles: vec![],
        };
        p.add_windows();
        assert!(!p.exe_is_gui);
        assert!(p.cli_copies.is_empty());
        assert_eq!(p.gui_copies, [root.join("Mokuro Bunko.exe")]);
        // The tray's own update (rare): the GUI build runs, the CLI is a copy.
        let mut g = Plan {
            exe: root.join("Mokuro Bunko.exe"),
            ..p.clone()
        };
        g.cli_copies.clear();
        g.gui_copies.clear();
        g.add_windows();
        assert!(g.exe_is_gui);
        assert_eq!(g.cli_copies, [root.join("bin/mokuro-bunko.exe")]);
        // A layout from before beta.3 that the update reached first: the top-level
        // command line runs; the new pair is replaced too once it exists.
        std::fs::write(root.join("mokuro-bunko.exe"), b"x").unwrap();
        let mut l = Plan {
            exe: root.join("mokuro-bunko.exe"),
            exe_is_gui: false,
            cli_copies: vec![],
            gui_copies: vec![],
            bundles: vec![],
        };
        l.add_windows();
        assert_eq!(l.cli_copies, [root.join("bin/mokuro-bunko.exe")]);
        assert_eq!(l.gui_copies, [root.join("Mokuro Bunko.exe")]);
    }

    #[test]
    fn macos_plan() {
        let d = tempfile::tempdir().unwrap();
        // The disk image's app, wherever it was dragged.
        let app = d.path().join("Mokuro Bunko.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(app.join("Contents/MacOS/mokuro-bunko"), b"x").unwrap();
        let mut p = Plan {
            exe: app.join("Contents/MacOS/mokuro-bunko"),
            exe_is_gui: false,
            cli_copies: vec![],
            gui_copies: vec![],
            bundles: vec![],
        };
        p.add_macos();
        assert_eq!(p.bundles, vec![app.clone()]);
        assert!(p.cli_copies.is_empty());
        // An archive install: the CLI with mokuro-bunko.app next to it.
        let top = d.path().join("lib");
        let b = top.join("mokuro-bunko.app");
        std::fs::create_dir_all(b.join("Contents/MacOS")).unwrap();
        std::fs::write(top.join("mokuro-bunko"), b"x").unwrap();
        std::fs::write(b.join("Contents/MacOS/mokuro-bunko"), b"y").unwrap();
        let mut q = Plan {
            exe: top.join("mokuro-bunko"),
            ..p.clone()
        };
        q.bundles.clear();
        q.add_macos();
        assert_eq!(q.bundles, vec![b.clone()]);
        assert_eq!(q.cli_copies, [b.join("Contents/MacOS/mokuro-bunko")]);
        // Started from inside that bundle: the outer CLI is a copy.
        let mut r = Plan {
            exe: b.join("Contents/MacOS/mokuro-bunko"),
            ..p.clone()
        };
        r.bundles.clear();
        r.add_macos();
        assert_eq!(r.cli_copies, [top.join("mokuro-bunko")]);
        assert_eq!(r.bundles, [b]);
    }

    fn tar_gz(path: &Path, files: &[(&str, &[u8])], links: &[(&str, &str)]) {
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(gz);
        for (p, body) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            tar.append_data(&mut h, p, *body).unwrap();
        }
        for (p, target) in links {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Link);
            h.set_size(0);
            h.set_mode(0o755);
            tar.append_link(&mut h, p, target).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn unpack_macos_archive_and_refresh_the_bundle() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.tar.gz");
        tar_gz(
            &a,
            &[
                ("top/mokuro-bunko", b"cli-new"),
                ("top/mokuro-bunko.app/Contents/Info.plist", b"plist-new"),
                ("top/mokuro-bunko.app/Contents/Resources/x.icns", b"icns"),
            ],
            &[(
                "top/mokuro-bunko.app/Contents/MacOS/mokuro-bunko",
                "top/mokuro-bunko",
            )],
        );
        let un = unpack(&a, Kind::TarGz, "mokuro-bunko", &d.path().join("stage")).unwrap();
        assert_eq!(std::fs::read(&un.cli).unwrap(), b"cli-new");
        let contents = un.app_contents.clone().unwrap();
        assert!(!contents.join("MacOS").exists());
        // An installed bundle of the old layout: the tray as the main program.
        let app = d.path().join("Mokuro Bunko.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(app.join("Contents/Info.plist"), b"plist-old").unwrap();
        std::fs::write(app.join("Contents/MacOS/mokuro-bunko-tray"), b"tray").unwrap();
        refresh_bundle(&app, &contents).unwrap();
        assert_eq!(
            std::fs::read(app.join("Contents/Info.plist")).unwrap(),
            b"plist-new"
        );
        assert!(app.join("Contents/Resources/x.icns").is_file());
        assert_eq!(remove_legacy_tray(&app.join("Contents/MacOS")).len(), 1);
        un.remove();
        assert!(!d.path().join("stage").exists());
    }

    #[test]
    fn unpack_windows_zip_takes_bin_and_the_gui_build() {
        use std::io::Write;
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&a).unwrap());
            let o = zip::write::SimpleFileOptions::default();
            for (n, b) in [
                ("top/Mokuro Bunko.exe", &b"gui"[..]),
                ("top/bin/mokuro-bunko.exe", b"cli"),
                ("top/run.bat", b"bat"),
            ] {
                z.start_file(n, o).unwrap();
                z.write_all(b).unwrap();
            }
            z.finish().unwrap();
        }
        let un = unpack(&a, Kind::Zip, "mokuro-bunko.exe", &d.path().join("s")).unwrap();
        assert_eq!(std::fs::read(&un.cli).unwrap(), b"cli");
        assert_eq!(std::fs::read(un.gui.unwrap()).unwrap(), b"gui");
        // A zip of the old layout.
        let b = d.path().join("b.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&b).unwrap());
            z.start_file(
                "top/mokuro-bunko.exe",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            z.write_all(b"old").unwrap();
            z.finish().unwrap();
        }
        let un = unpack(&b, Kind::Zip, "mokuro-bunko.exe", &d.path().join("t")).unwrap();
        assert_eq!(std::fs::read(&un.cli).unwrap(), b"old");
        assert!(un.gui.is_none());
        assert!(unpack(&b, Kind::Zip, "nope.exe", &d.path().join("u")).is_err());
        assert!(!d.path().join("u").exists());
    }

    #[test]
    fn commit_replaces_the_copies() {
        // Every platform: the Windows pair with fake executables (the running one is
        // replaced by self_replace, which these tests cannot exercise: copies only).
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("Mokuro Bunko.exe"), b"old-gui").unwrap();
        let un = Unpacked {
            dir: root.join("stage"),
            cli: root.join("stage/mokuro-bunko.exe"),
            gui: Some(root.join("stage/Mokuro Bunko.exe")),
            app_contents: None,
        };
        std::fs::create_dir_all(&un.dir).unwrap();
        std::fs::write(&un.cli, b"new-cli").unwrap();
        std::fs::write(un.gui.as_ref().unwrap(), b"new-gui").unwrap();
        install_copy(un.gui.as_ref().unwrap(), &root.join("Mokuro Bunko.exe")).unwrap();
        assert_eq!(
            std::fs::read(root.join("Mokuro Bunko.exe")).unwrap(),
            b"new-gui"
        );
        assert!(!root.join(".Mokuro Bunko.exe.new").exists());
    }

    #[test]
    fn kinds() {
        assert_eq!(Kind::of("https://x/a-macos.dmg"), Kind::Dmg);
        assert_eq!(Kind::of("https://x/A.TAR.GZ"), Kind::TarGz);
        assert_eq!(Kind::of("x.zip"), Kind::Zip);
        assert_eq!(Kind::of("x"), Kind::Bare);
    }
}
