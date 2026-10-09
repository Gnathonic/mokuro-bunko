//! Where the files of an installed copy are, and how an update replaces them.
//!
//! The tray is part of the program (`mokuro-bunko tray`, and `mokuro-bunko` with no
//! arguments in a desktop session). What an update replaces besides the running
//! executable:
//!
//! * **Windows**: the pair in the install folder, `mokuro-bunko.exe` (the app: the
//!   program built for the GUI subsystem, no console window) and `mokuro-bunko-cli.exe`
//!   (the console build, for terminals and scripts): same code, the PE header's
//!   subsystem field is the only difference ([`set_pe_subsystem`]).
//! * **macOS**: the app bundle around the program (`Mokuro Bunko.app`): its
//!   `Info.plist`, `PkgInfo` and `Resources` come from the new release (the disk image),
//!   and the bundle is sealed again (`codesign --force --deep -s -`), so
//!   `codesign --verify` passes after the update as it did before.
//! * **Linux**: just the executable.
//!
//! A release download is unpacked into a staging folder next to the executable first
//! ([`unpack`]); [`Plan::commit`] then puts the files in place, and [`Plan::seal`]
//! seals a bundle once the staging folder is gone.

use crate::UpdateError;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The Windows app (GUI subsystem): what shortcuts, Startup and a double-click run.
pub const WINDOWS_GUI_EXE: &str = "mokuro-bunko.exe";
/// The Windows console build, for terminals and scripts.
pub const WINDOWS_CLI_EXE: &str = "mokuro-bunko-cli.exe";
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

/// Set the subsystem of a Windows executable. `mokuro-bunko.exe` is `mokuro-bunko-cli.exe`
/// with the GUI subsystem: Rust links both with the same entry point (`mainCRTStartup`),
/// so the field is the only difference (`windows_subsystem` only changes the linker's
/// `/SUBSYSTEM`), and Windows does not check the PE checksum of an application.
pub fn set_pe_subsystem(bytes: &mut [u8], subsystem: u16) -> Result<(), String> {
    let at = pe_subsystem_offset(bytes).ok_or("not a Windows executable (PE)")?;
    bytes[at..at + 2].copy_from_slice(&subsystem.to_le_bytes());
    Ok(())
}

/// Whether the Windows executable at `path` is a GUI-subsystem build.
pub fn is_gui_exe(path: &Path) -> bool {
    std::fs::read(path).is_ok_and(|b| pe_subsystem(&b) == Some(PE_SUBSYSTEM_GUI))
}

/// Write a copy of the Windows executable `from` with the subsystem `subsystem` to `out`.
pub fn write_subsystem_copy(from: &Path, subsystem: u16, out: &Path) -> std::io::Result<()> {
    let mut bytes = std::fs::read(from)?;
    set_pe_subsystem(&mut bytes, subsystem)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(out, bytes)
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
    /// Other copies of the console program in this install (replaced by rename).
    pub cli_copies: Vec<PathBuf>,
    /// Copies of the Windows GUI build (`mokuro-bunko.exe`).
    pub gui_copies: Vec<PathBuf>,
    /// macOS app bundles of this install (refreshed and sealed again).
    pub bundles: Vec<PathBuf>,
}

impl Plan {
    fn empty(exe: &Path) -> Plan {
        Plan {
            exe: exe.to_path_buf(),
            exe_is_gui: false,
            cli_copies: Vec::new(),
            gui_copies: Vec::new(),
            bundles: Vec::new(),
        }
    }

    /// The plan for the installed copy whose executable is `exe`, as laid out on disk now.
    pub fn for_exe(exe: &Path) -> Plan {
        let mut plan = Plan::empty(exe);
        if cfg!(windows) {
            plan.add_windows();
        } else if cfg!(target_os = "macos") {
            plan.add_macos();
        }
        plan
    }

    /// The Windows pair next to `exe` (every platform, for tests).
    pub fn add_windows(&mut self) {
        let Some(dir) = self.exe.parent() else {
            return;
        };
        self.exe_is_gui = is_gui_exe(&self.exe);
        for name in [WINDOWS_GUI_EXE, WINDOWS_CLI_EXE] {
            let p = dir.join(name);
            if !p.is_file() || same_file(&p, &self.exe) {
                continue;
            }
            if is_gui_exe(&p) {
                self.gui_copies.push(p);
            } else {
                self.cli_copies.push(p);
            }
        }
    }

    /// The macOS layout (every platform, for tests): the bundle around `exe`.
    pub fn add_macos(&mut self) {
        if let Some(b) = bundle_of(&self.exe) {
            self.bundles.push(b);
        }
    }

    /// Every file of this install that holds the program, the running one first.
    pub fn programs(&self) -> Vec<PathBuf> {
        let mut out = vec![self.exe.clone()];
        out.extend(self.cli_copies.iter().cloned());
        out.extend(self.gui_copies.iter().cloned());
        out
    }

    /// Put the unpacked release in place: the running executable first (its failure
    /// changes nothing), then the other copies and the bundles' files, best effort.
    /// Seal the bundles afterwards ([`Plan::seal`]), once the staging folder is gone.
    pub fn commit(&self, new: &Unpacked) -> Result<(), UpdateError> {
        let want_gui = self.exe_is_gui || !self.gui_copies.is_empty();
        let gui_new = match &new.gui {
            Some(g) => Some(g.clone()),
            None if want_gui => {
                let g = new.dir.join(WINDOWS_GUI_EXE);
                write_subsystem_copy(&new.cli, PE_SUBSYSTEM_GUI, &g)?;
                Some(g)
            }
            None => None,
        };
        let main = match (&gui_new, self.exe_is_gui) {
            (Some(g), true) => g.clone(),
            _ => new.cli.clone(),
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
        }
        Ok(())
    }

    /// Seal the bundles again ([`seal_bundle`]): after an update's files are in place
    /// and its staging folder is gone, and again when the previous release's backups
    /// go (`drop_previous`) or come back (`restore_previous`).
    pub fn seal(&self) {
        for b in &self.bundles {
            seal_bundle(b);
        }
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
    /// The new console program (runnable: the prefetch step runs it).
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

/// Unpack the downloaded `archive` (kind `kind`) into the folder `out`: the program
/// named `binary` (found by file name; in a Windows zip the console build
/// `mokuro-bunko-cli.exe` and the app `binary`), and the macOS app's `Contents`.
pub fn unpack(
    archive: &Path,
    kind: Kind,
    binary: &str,
    out: &Path,
) -> Result<Unpacked, UpdateError> {
    let _ = std::fs::remove_dir_all(out);
    std::fs::create_dir_all(out)?;
    let cli = out.join(if kind == Kind::Zip {
        WINDOWS_CLI_EXE
    } else {
        binary
    });
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
        // The program: the first regular file of that name (the top-level one; the copy
        // in an archive's app bundle is a hard link to it).
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
        // The app bundle's Contents, without its program (a link entry has no data).
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
    let find = |file: &str| {
        names
            .iter()
            .find(|n| n.rsplit('/').next().unwrap_or(n).eq_ignore_ascii_case(file))
            .cloned()
    };
    let mut read = |name: &str, out: &Path| -> Result<(), UpdateError> {
        let mut bytes = Vec::new();
        zip.by_name(name)
            .map_err(|e| UpdateError::Unpack(e.to_string()))?
            .read_to_end(&mut bytes)?;
        std::fs::write(out, bytes)?;
        Ok(())
    };
    // The console build `mokuro-bunko-cli.exe` (what the prefetch step runs) and the
    // app `binary`; a zip with one program (a lite build) has just `binary`.
    match (find(WINDOWS_CLI_EXE), find(binary)) {
        (Some(cli), gui) => {
            read(&cli, &un.cli)?;
            if let Some(gui) = gui.filter(|g| *g != cli) {
                let out = un.dir.join(WINDOWS_GUI_EXE);
                read(&gui, &out)?;
                un.gui = Some(out);
            }
        }
        (None, Some(only)) => read(&only, &un.cli)?,
        (None, None) => {
            return Err(UpdateError::Unpack(format!(
                "{binary} is not in the archive"
            )));
        }
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
        let dir = d.path();
        std::fs::write(dir.join(WINDOWS_GUI_EXE), fake_exe(PE_SUBSYSTEM_GUI)).unwrap();
        std::fs::write(dir.join(WINDOWS_CLI_EXE), fake_exe(PE_SUBSYSTEM_CONSOLE)).unwrap();
        // A server the tray started: the app runs, the console build is a copy.
        let mut p = Plan::empty(&dir.join(WINDOWS_GUI_EXE));
        p.add_windows();
        assert!(p.exe_is_gui);
        assert_eq!(p.cli_copies, vec![dir.join(WINDOWS_CLI_EXE)]);
        assert!(p.gui_copies.is_empty());
        // `mokuro-bunko-cli update apply` in a terminal: the other way round.
        let mut c = Plan::empty(&dir.join(WINDOWS_CLI_EXE));
        c.add_windows();
        assert!(!c.exe_is_gui);
        assert_eq!(c.gui_copies, vec![dir.join(WINDOWS_GUI_EXE)]);
        assert!(c.cli_copies.is_empty());
    }

    #[test]
    fn macos_plan() {
        let d = tempfile::tempdir().unwrap();
        let app = d.path().join("Mokuro Bunko.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(app.join("Contents/MacOS/mokuro-bunko"), b"x").unwrap();
        let mut p = Plan::empty(&app.join("Contents/MacOS/mokuro-bunko"));
        p.add_macos();
        assert_eq!(p.bundles, vec![app.clone()]);
        let mut q = Plan::empty(&d.path().join("mokuro-bunko"));
        q.add_macos();
        assert!(q.bundles.is_empty());
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
        let app = d.path().join("Mokuro Bunko.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(app.join("Contents/Info.plist"), b"plist-old").unwrap();
        refresh_bundle(&app, &contents).unwrap();
        assert_eq!(
            std::fs::read(app.join("Contents/Info.plist")).unwrap(),
            b"plist-new"
        );
        assert!(app.join("Contents/Resources/x.icns").is_file());
        un.remove();
        assert!(!d.path().join("stage").exists());
    }

    #[test]
    fn unpack_windows_zip_takes_the_pair() {
        use std::io::Write;
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&a).unwrap());
            let o = zip::write::SimpleFileOptions::default();
            for (n, b) in [
                ("top/mokuro-bunko.exe", &b"gui"[..]),
                ("top/mokuro-bunko-cli.exe", b"cli"),
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
        // A zip with one program (a lite build).
        let b = d.path().join("b.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&b).unwrap());
            z.start_file(
                "top/mokuro-bunko.exe",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            z.write_all(b"only").unwrap();
            z.finish().unwrap();
        }
        let un = unpack(&b, Kind::Zip, "mokuro-bunko.exe", &d.path().join("t")).unwrap();
        assert_eq!(std::fs::read(&un.cli).unwrap(), b"only");
        assert!(un.gui.is_none());
        assert!(unpack(&b, Kind::Zip, "nope.exe", &d.path().join("u")).is_err());
        assert!(!d.path().join("u").exists());
    }

    #[test]
    fn install_copy_replaces_through_a_rename() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path();
        std::fs::write(dir.join(WINDOWS_GUI_EXE), b"old-gui").unwrap();
        let new = dir.join("stage-gui");
        std::fs::write(&new, b"new-gui").unwrap();
        install_copy(&new, &dir.join(WINDOWS_GUI_EXE)).unwrap();
        assert_eq!(
            std::fs::read(dir.join(WINDOWS_GUI_EXE)).unwrap(),
            b"new-gui"
        );
        assert!(!dir.join(".mokuro-bunko.exe.new").exists());
    }

    #[test]
    fn kinds() {
        assert_eq!(Kind::of("https://x/a-macos.dmg"), Kind::Dmg);
        assert_eq!(Kind::of("https://x/A.TAR.GZ"), Kind::TarGz);
        assert_eq!(Kind::of("x.zip"), Kind::Zip);
        assert_eq!(Kind::of("x"), Kind::Bare);
    }
}
