//! The first start of a release with the tray in the program (0.7.0-beta.3) after an
//! update from one with a separate `mokuro-bunko-tray` (0.7.0-beta.2 and earlier).
//!
//! The old release's updater installed this program but knows nothing of the new
//! layout, so this finishes the move, for this install only (paths are compared, never
//! names alone: another install of the same user is left alone):
//!
//! * login items that start this install's old tray (the XDG autostart entry, the menu
//!   entry `install.sh` wrote, the LaunchAgent, the Windows Startup and Start-menu
//!   shortcuts) now start the tray of this program;
//! * macOS: the app bundle's main program is `mokuro-bunko` (Info.plist), the bundle
//!   holds this program and is sealed again;
//! * Windows: the layout of beta.3: `bin\mokuro-bunko.exe` (the command line) and
//!   `Mokuro Bunko.exe` (the tray, no console window) with `run.bat`/`doctor.bat`
//!   calling `bin\`; the old top-level `mokuro-bunko.exe` goes once nothing uses it;
//! * a running old tray of this install is stopped and the new one started in its
//!   place; an instance that the old tray was supervising then exits
//!   ([`Outcome::HandOver`]) so that the new tray starts it again under its own care;
//! * the old `mokuro-bunko-tray` program (and its update backup, and `install.sh`'s
//!   link to it) is removed.
//!
//! Every step looks at the disk first, so it runs at each start of `serve`,
//! `processor serve` and `tray` and costs a few file checks once there is nothing
//! left to do. It runs after a pending update has proven itself
//! (`autoupdate::after_restart`): an update that rolls back finds the old layout as
//! the old release left it.

use bunko_tray::paths::{self as tpaths, ProcessEnv};
use bunko_update::layout::{self, LEGACY_TRAY};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    Server,
    Processor,
    Tray,
}

/// What the caller does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Outcome {
    Continue,
    /// The old tray that started this instance was replaced by the new one: exit now
    /// (code 0); the new tray starts this role again.
    HandOver,
}

/// Run the migration for the program running now (see the module docs).
pub fn run(caller: Caller) -> Outcome {
    if !matches!(
        bunko_update::InstallKind::detect(),
        bunko_update::InstallKind::SelfManaged { .. }
    ) {
        return Outcome::Continue;
    }
    let Ok(exe) = bunko_update::current_exe() else {
        return Outcome::Continue;
    };
    let launcher = std::env::var("MOKURO_LAUNCHER").unwrap_or_default();
    let m = Migration::new(&exe, &ProcessEnv);
    m.run(caller, &launcher)
}

/// One install's migration.
pub struct Migration<'a> {
    /// This program.
    pub exe: PathBuf,
    /// Where this install's old tray programs would be.
    pub stale: Vec<PathBuf>,
    env: &'a dyn tpaths::Env,
}

impl<'a> Migration<'a> {
    pub fn new(exe: &Path, env: &'a dyn tpaths::Env) -> Migration<'a> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(d) = exe.parent() {
            dirs.push(d.to_path_buf());
        }
        if cfg!(windows)
            && let Some(root) = windows_root(exe)
        {
            dirs.push(root);
        }
        if cfg!(target_os = "macos") {
            let mut plan = layout::Plan {
                exe: exe.to_path_buf(),
                exe_is_gui: false,
                cli_copies: Vec::new(),
                gui_copies: Vec::new(),
                bundles: Vec::new(),
            };
            plan.add_macos();
            dirs.extend(plan.bundles.iter().map(|b| b.join("Contents/MacOS")));
        }
        dirs.dedup();
        Migration {
            exe: exe.to_path_buf(),
            stale: dirs.into_iter().map(|d| d.join(LEGACY_TRAY)).collect(),
            env,
        }
    }

    pub fn run(&self, caller: Caller, launcher: &str) -> Outcome {
        self.windows_layout(launcher);
        self.macos_bundles();
        self.login_items();
        let running = running_trays(&self.stale);
        let old_env = running.first().and_then(|t| process_env(t.0));
        if !running.is_empty() {
            #[cfg(target_os = "linux")]
            if launcher == "tray" {
                // The old tray started us with "SIGTERM me when you die": not any more,
                // it is about to be stopped and this process must see the handover through.
                // SAFETY: prctl on the calling process only.
                unsafe {
                    libc::prctl(libc::PR_SET_PDEATHSIG, 0);
                }
            }
            for (pid, path) in &running {
                tracing::info!(
                    "update: stopping the old tray (pid {pid}, {}); the tray is now part of mokuro-bunko",
                    path.display()
                );
                stop_process(*pid);
            }
        }
        for s in &self.stale {
            if let Some(dir) = s.parent() {
                for p in layout::remove_legacy_tray(dir) {
                    tracing::info!("update: removed {}", p.display());
                }
            }
        }
        self.remove_links();
        if cfg!(windows) {
            self.windows_remove_old_cli(launcher);
        }
        if running.is_empty() || caller == Caller::Tray {
            return Outcome::Continue;
        }
        match self.start_tray(old_env) {
            Ok(pid) => tracing::info!("update: started the new tray (pid {pid})"),
            Err(e) => tracing::warn!("update: could not start the new tray: {e}"),
        }
        if launcher == "tray" {
            tracing::info!(
                "update: the old tray ran this {}; handing it over to the new tray",
                match caller {
                    Caller::Processor => "processor",
                    _ => "server",
                }
            );
            return Outcome::HandOver;
        }
        Outcome::Continue
    }

    #[cfg_attr(windows, allow(dead_code))]
    /// The program a login item of the old tray at `stale` starts now: this install's
    /// `mokuro-bunko` next to it (on Windows `Mokuro Bunko.exe`), else this program.
    fn program_for(&self, stale: &Path) -> PathBuf {
        if cfg!(windows)
            && let Some(root) = windows_root(&self.exe)
            && root.join(layout::WINDOWS_GUI_EXE).is_file()
        {
            return root.join(layout::WINDOWS_GUI_EXE);
        }
        let sibling = stale.with_file_name(if cfg!(windows) {
            layout::WINDOWS_CLI_EXE
        } else {
            "mokuro-bunko"
        });
        if sibling.is_file() {
            sibling
        } else {
            self.exe.clone()
        }
    }

    #[cfg_attr(windows, allow(dead_code))]
    /// Whether `program` (named by a login item) is one of this install's old trays, or
    /// an old tray program that is gone.
    fn is_ours_or_gone(&self, program: &Path) -> Option<PathBuf> {
        if program.file_name()? != LEGACY_TRAY {
            return None;
        }
        let ours = self.stale.iter().find(|s| same_path(s, program)).cloned();
        if ours.is_some() {
            return ours;
        }
        (!program.exists()).then(|| self.stale.first().cloned().unwrap_or_default())
    }

    fn login_items(&self) {
        let env = self.env;
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let autostart = tpaths::config_base(env)
                .join("autostart")
                .join(bunko_tray::autostart::DESKTOP_NAME);
            let menu = tpaths::data_base(env)
                .join("applications")
                .join(bunko_tray::autostart::DESKTOP_NAME);
            let system_menu = PathBuf::from("/usr/local/share/applications")
                .join(bunko_tray::autostart::DESKTOP_NAME);
            for (path, is_autostart) in [(autostart, true), (menu, false), (system_menu, false)] {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Some(program) = desktop_exec_program(&text) else {
                    continue;
                };
                if let Some(stale) = self.is_ours_or_gone(&program) {
                    let mut entry = bunko_tray::autostart::desktop_entry(
                        &self.program_for(&stale),
                        is_autostart,
                    );
                    if !is_autostart {
                        entry.push_str("Keywords=manga;library;OCR;mokuro;\n");
                    }
                    match std::fs::write(&path, entry) {
                        Ok(()) => tracing::info!(
                            "update: {} starts `mokuro-bunko tray` now",
                            path.display()
                        ),
                        Err(e) => tracing::debug!("update: {} not rewritten: {e}", path.display()),
                    }
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            let path = tpaths::home(env)
                .join("Library/LaunchAgents")
                .join(format!("{}.plist", bunko_tray::autostart::LAUNCHD_LABEL));
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Some(program) = plist_program(&text)
                && let Some(stale) = self.is_ours_or_gone(&program)
            {
                // Only the file: launchd reads it at the next login (never launchctl here,
                // which acts on every agent of this user id).
                let plist = bunko_tray::autostart::launchd_plist(&self.program_for(&stale));
                match std::fs::write(&path, plist) {
                    Ok(()) => {
                        tracing::info!("update: {} starts `mokuro-bunko tray` now", path.display())
                    }
                    Err(e) => tracing::warn!("update: {} not rewritten: {e}", path.display()),
                }
            }
        }
        #[cfg(windows)]
        {
            let gui = windows_root(&self.exe).map(|r| r.join(layout::WINDOWS_GUI_EXE));
            let Some(gui) = gui.filter(|g| g.is_file()) else {
                return;
            };
            let mut links = Vec::new();
            if let Some(appdata) = env.path("APPDATA") {
                links.push(
                    bunko_tray::discover::startup_dir(&appdata)
                        .join(bunko_tray::autostart::WINDOWS_LINK),
                );
                links.push(
                    appdata
                        .join("Microsoft\\Windows\\Start Menu\\Programs\\Mokuro Bunko")
                        .join(bunko_tray::autostart::WINDOWS_LINK),
                );
            }
            for link in links {
                let Ok(bytes) = std::fs::read(&link) else {
                    continue;
                };
                if !self.stale.iter().any(|s| link_names(&bytes, s)) {
                    continue;
                }
                match write_shortcut(&link, &gui) {
                    Ok(()) => {
                        tracing::info!("update: {} starts {} now", link.display(), gui.display())
                    }
                    Err(e) => tracing::warn!("update: {} not rewritten: {e}", link.display()),
                }
            }
        }
        let _ = env;
    }

    /// macOS: a bundle whose main program was the old tray: `mokuro-bunko` now (this
    /// program, or a copy of it), sealed again.
    fn macos_bundles(&self) {
        if !cfg!(target_os = "macos") {
            return;
        }
        let mut plan = layout::Plan {
            exe: self.exe.clone(),
            exe_is_gui: false,
            cli_copies: Vec::new(),
            gui_copies: Vec::new(),
            bundles: Vec::new(),
        };
        plan.add_macos();
        for b in &plan.bundles {
            let info = b.join("Contents/Info.plist");
            let Ok(text) = std::fs::read_to_string(&info) else {
                continue;
            };
            let Some(new) = set_bundle_executable(&text, "mokuro-bunko") else {
                continue;
            };
            let program = b.join("Contents/MacOS/mokuro-bunko");
            if !same_path(&program, &self.exe)
                && !same_contents(&program, &self.exe)
                && let Err(e) = layout::install_copy(&self.exe, &program)
            {
                tracing::warn!("update: {} not replaced: {e}", program.display());
                continue;
            }
            if let Err(e) = std::fs::write(&info, new) {
                tracing::warn!("update: {} not rewritten: {e}", info.display());
                continue;
            }
            layout::remove_legacy_tray(&b.join("Contents/MacOS"));
            layout::seal_bundle(b);
            tracing::info!("update: {} opens the tray of mokuro-bunko now", b.display());
        }
    }

    /// Windows: create the beta.3 layout next to an install of the old one.
    fn windows_layout(&self, launcher: &str) {
        if !cfg!(windows) {
            return;
        }
        let Some(root) = windows_root(&self.exe) else {
            return;
        };
        // An install made from a release zip (not a build folder): its launchers are here.
        if !root.join("_env.cmd").is_file() && !root.join("run.bat").is_file() {
            return;
        }
        let bin = root.join("bin");
        let cli = bin.join(layout::WINDOWS_CLI_EXE);
        let gui = root.join(layout::WINDOWS_GUI_EXE);
        if !cli.is_file() {
            if let Err(e) =
                std::fs::create_dir_all(&bin).and_then(|()| layout::install_copy(&self.exe, &cli))
            {
                tracing::warn!("update: could not create {}: {e}", cli.display());
                return;
            }
            // The Visual C++ runtime the program loads from its own folder.
            if let Ok(rd) = std::fs::read_dir(&root) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().to_ascii_lowercase();
                    let vc = ["vcruntime140", "msvcp140", "vcomp140", "concrt140"]
                        .iter()
                        .any(|p| n.starts_with(p));
                    if vc && n.ends_with(".dll") && !bin.join(e.file_name()).exists() {
                        let _ = std::fs::copy(e.path(), bin.join(e.file_name()));
                    }
                }
            }
            tracing::info!("update: the command line is {} now", cli.display());
        }
        if !gui.is_file() {
            match layout::write_gui_copy(&self.exe, &gui) {
                Ok(()) => tracing::info!("update: created {}", gui.display()),
                Err(e) => tracing::warn!("update: could not create {}: {e}", gui.display()),
            }
        }
        for (name, text) in WINDOWS_LAUNCHERS {
            // run.bat is being read by cmd.exe right now when it started us: later.
            if *name == "run.bat" && launcher == "run.bat" {
                continue;
            }
            let path = root.join(name);
            let Ok(old) = std::fs::read_to_string(&path) else {
                continue;
            };
            if !old.contains("%~dp0mokuro-bunko.exe") {
                continue;
            }
            let new = text
                .replace("@VERSION@", bunko_core::VERSION)
                .replace("@FLAVOR@", crate::FLAVOR)
                .replace("@TARGET@", bunko_update::TARGET)
                .replace("\r\n", "\n")
                .replace('\n', "\r\n");
            match std::fs::write(&path, new) {
                Ok(()) => {
                    tracing::info!("update: {} runs bin\\mokuro-bunko.exe now", path.display())
                }
                Err(e) => tracing::warn!("update: {} not rewritten: {e}", path.display()),
            }
        }
    }

    /// Windows: the old top-level `mokuro-bunko.exe`, once `bin\` has the command line,
    /// nothing runs it and `run.bat` no longer names it.
    fn windows_remove_old_cli(&self, launcher: &str) {
        let Some(root) = windows_root(&self.exe) else {
            return;
        };
        let old = root.join(layout::WINDOWS_CLI_EXE);
        if !old.is_file()
            || !root.join("bin").join(layout::WINDOWS_CLI_EXE).is_file()
            || same_path(&old, &self.exe)
            || launcher == "run.bat"
        {
            return;
        }
        let run_bat = std::fs::read_to_string(root.join("run.bat")).unwrap_or_default();
        if run_bat.contains("%~dp0mokuro-bunko.exe") {
            return;
        }
        match std::fs::remove_file(&old) {
            Ok(()) => tracing::info!("update: removed the old {}", old.display()),
            Err(e) => tracing::debug!("update: {} stays for now: {e}", old.display()),
        }
        let _ = std::fs::remove_file(old.with_extension("exe.old"));
    }

    /// `install.sh`'s links to an old tray of this install (`~/.local/bin`, `PATH`).
    fn remove_links(&self) {
        if cfg!(windows) {
            return;
        }
        let mut dirs = vec![tpaths::home(self.env).join(".local/bin")];
        if let Some(path) = self.env.var("PATH") {
            dirs.extend(std::env::split_paths(&path));
        }
        dirs.push(PathBuf::from("/usr/local/bin"));
        for d in dirs {
            let link = d.join(LEGACY_TRAY);
            let Ok(target) = std::fs::read_link(&link) else {
                continue;
            };
            let target = if target.is_absolute() {
                target
            } else {
                d.join(target)
            };
            if self
                .stale
                .iter()
                .any(|s| same_path(s, &target) || *s == target)
                && std::fs::remove_file(&link).is_ok()
            {
                tracing::info!("update: removed the link {}", link.display());
            }
        }
    }

    /// Start the tray of this install, detached (with the old tray's environment when
    /// known: its display and session bus).
    fn start_tray(&self, env: Option<Vec<(String, String)>>) -> std::io::Result<u32> {
        let (program, args) = bunko_tray::autostart::launch_command(&self.exe);
        let mut cmd = std::process::Command::new(&program);
        cmd.args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(vars) = env {
            cmd.env_clear().envs(vars);
        }
        cmd.env_remove("MOKURO_LAUNCHER")
            .env_remove("MOKURO_CONTROL_MANAGED");
        if let Some(dir) = program.parent() {
            cmd.current_dir(dir);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: setsid is async-signal-safe; nothing else runs between fork and exec.
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }
        let child = cmd.spawn()?;
        Ok(child.id())
    }
}

/// The launchers of the Windows zip, as the release has them.
const WINDOWS_LAUNCHERS: &[(&str, &str)] = &[
    (
        "_env.cmd",
        include_str!("../../../packaging/windows/_env.cmd"),
    ),
    (
        "doctor.bat",
        include_str!("../../../packaging/windows/doctor.bat"),
    ),
    (
        "run.bat",
        include_str!("../../../packaging/windows/run.bat"),
    ),
];

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

fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    if cfg!(windows)
        && a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn same_contents(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) if x.len() == y.len() => {
            matches!((std::fs::read(a), std::fs::read(b)), (Ok(p), Ok(q)) if p == q)
        }
        _ => false,
    }
}

/// The program of a desktop entry's `Exec` (its first word, unquoted).
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
pub fn desktop_exec_program(text: &str) -> Option<PathBuf> {
    let exec = text.lines().find_map(|l| l.strip_prefix("Exec="))?.trim();
    let word = if let Some(rest) = exec.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => out.extend(chars.next()),
                '"' => break,
                c => out.push(c),
            }
        }
        out
    } else {
        exec.split_whitespace().next()?.to_string()
    };
    Some(PathBuf::from(word.replace("%%", "%")))
}

/// The program of a LaunchAgent plist (the first `ProgramArguments` string).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn plist_program(text: &str) -> Option<PathBuf> {
    let args = &text[text.find("<key>ProgramArguments</key>")?..];
    let start = args.find("<string>")? + "<string>".len();
    let end = start + args[start..].find("</string>")?;
    let raw = &args[start..end];
    Some(PathBuf::from(
        raw.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&"),
    ))
}

/// Info.plist with `CFBundleExecutable` set to `name`; None when it already is (or has
/// none).
pub fn set_bundle_executable(text: &str, name: &str) -> Option<String> {
    let key = text.find("<key>CFBundleExecutable</key>")?;
    let open = key + text[key..].find("<string>")? + "<string>".len();
    let close = open + text[open..].find("</string>")?;
    if &text[open..close] == name {
        return None;
    }
    Some(format!("{}{name}{}", &text[..open], &text[close..]))
}

#[cfg_attr(not(windows), allow(dead_code))]
/// Whether a Windows shortcut's bytes name `path` (as Windows stores it: ANSI in the
/// link info, UTF-16 in the string data), ignoring case.
pub fn link_names(bytes: &[u8], path: &Path) -> bool {
    let p = path.to_string_lossy().to_ascii_lowercase();
    let lower: Vec<u8> = bytes.iter().map(u8::to_ascii_lowercase).collect();
    let ansi = p.as_bytes();
    let wide: Vec<u8> = p.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let has =
        |needle: &[u8]| !needle.is_empty() && lower.windows(needle.len()).any(|w| w == needle);
    has(ansi) || has(&wide)
}

#[cfg(windows)]
fn write_shortcut(link: &Path, target: &Path) -> Result<(), String> {
    let mut l = mslnk::ShellLink::new(target)
        .map_err(|e| format!("could not make a shortcut to {}: {e}", target.display()))?;
    if let Some(dir) = target.parent() {
        l.set_working_dir(Some(dir.to_string_lossy().into_owned()));
        let ico = dir.join("mokuro-bunko.ico");
        if ico.is_file() {
            l.set_icon_location(Some(ico.to_string_lossy().into_owned()));
        }
    }
    l.set_name(Some("Mokuro Bunko".into()));
    l.create_lnk(link).map_err(|e| e.to_string())
}

/// Running processes whose program is one of `paths` (this user's): (pid, path).
fn running_trays(paths: &[PathBuf]) -> Vec<(u32, PathBuf)> {
    let me = std::process::id();
    let mut out = Vec::new();
    #[cfg(target_os = "linux")]
    {
        // SAFETY: getuid has no failure mode.
        let uid = unsafe { libc::getuid() };
        if let Ok(rd) = std::fs::read_dir("/proc") {
            for e in rd.flatten() {
                let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                    continue;
                };
                if pid == me
                    || std::os::unix::fs::MetadataExt::uid(&match e.metadata() {
                        Ok(m) => m,
                        Err(_) => continue,
                    }) != uid
                {
                    continue;
                }
                let Ok(exe) = std::fs::read_link(e.path().join("exe")) else {
                    continue;
                };
                let s = exe.to_string_lossy();
                let exe = PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(&s));
                if let Some(p) = paths.iter().find(|p| **p == exe || same_path(p, &exe)) {
                    out.push((pid, p.clone()));
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut pids = vec![0i32; 8192];
        // SAFETY: the buffer and its byte size match.
        let n = unsafe {
            libc::proc_listallpids(
                pids.as_mut_ptr().cast(),
                (pids.len() * std::mem::size_of::<i32>()) as i32,
            )
        };
        for &pid in pids.iter().take(n.max(0) as usize) {
            if pid <= 0 || pid as u32 == me {
                continue;
            }
            let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
            // SAFETY: the buffer and its size match.
            let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
            if len <= 0 {
                continue;
            }
            let exe = PathBuf::from(String::from_utf8_lossy(&buf[..len as usize]).into_owned());
            if let Some(p) = paths.iter().find(|p| **p == exe || same_path(p, &exe)) {
                out.push((pid as u32, p.clone()));
            }
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::ProcessStatus::EnumProcesses;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        };
        let mut pids = vec![0u32; 4096];
        let mut bytes = 0u32;
        // SAFETY: the buffer and its byte size match; `bytes` receives the size used.
        let ok = unsafe { EnumProcesses(pids.as_mut_ptr(), (pids.len() * 4) as u32, &mut bytes) };
        if ok != 0 {
            for &pid in pids.iter().take(bytes as usize / 4) {
                if pid == 0 || pid == me {
                    continue;
                }
                // SAFETY: plain handle calls; the handle is closed below.
                unsafe {
                    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                    if h.is_null() {
                        continue;
                    }
                    let mut buf = vec![0u16; 1024];
                    let mut len = buf.len() as u32;
                    let got = QueryFullProcessImageNameW(
                        h,
                        PROCESS_NAME_WIN32,
                        buf.as_mut_ptr(),
                        &mut len,
                    );
                    CloseHandle(h);
                    if got == 0 {
                        continue;
                    }
                    let exe = PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
                    if let Some(p) = paths.iter().find(|p| same_path(p, &exe)) {
                        out.push((pid, p.clone()));
                    }
                }
            }
        }
    }
    let _ = (paths, me);
    out
}

/// The environment of process `pid` (Linux: `/proc/<pid>/environ`), to start the new
/// tray in the old one's desktop session.
fn process_env(pid: u32) -> Option<Vec<(String, String)>> {
    #[cfg(target_os = "linux")]
    {
        let bytes = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        let vars: Vec<(String, String)> = bytes
            .split(|b| *b == 0)
            .filter_map(|kv| {
                let s = String::from_utf8_lossy(kv);
                let (k, v) = s.split_once('=')?;
                Some((k.to_string(), v.to_string()))
            })
            .collect();
        (!vars.is_empty()).then_some(vars)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Stop process `pid` (SIGTERM, then SIGKILL after 5 s; Windows: terminate) and wait for
/// it to be gone.
fn stop_process(pid: u32) {
    #[cfg(unix)]
    {
        let p = pid as libc::pid_t;
        // SAFETY: plain kill(2) calls on a process of this user.
        unsafe {
            libc::kill(p, libc::SIGTERM);
        }
        for i in 0..60 {
            // SAFETY: as above; signal 0 only checks that the process exists.
            if unsafe { libc::kill(p, 0) } != 0 {
                return;
            }
            if i == 50 {
                unsafe {
                    libc::kill(p, libc::SIGKILL);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
            WaitForSingleObject,
        };
        // SAFETY: plain handle calls; the handle is closed below.
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid);
            if h.is_null() {
                return;
            }
            TerminateProcess(h, 0);
            WaitForSingleObject(h, 5000);
            CloseHandle(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_of_desktop_entries() {
        assert_eq!(
            desktop_exec_program("[Desktop Entry]\nExec=/a/b/mokuro-bunko-tray\nIcon=x\n"),
            Some(PathBuf::from("/a/b/mokuro-bunko-tray"))
        );
        assert_eq!(
            desktop_exec_program("Exec=\"/home/a b/\\$x/100%%/mokuro-bunko\" tray\n"),
            Some(PathBuf::from("/home/a b/$x/100%/mokuro-bunko"))
        );
        assert_eq!(desktop_exec_program("Name=x\n"), None);
    }

    #[test]
    fn plist_program_and_bundle_executable() {
        let p = bunko_tray::autostart::launchd_plist(Path::new(
            "/Users/a/R&D/Mokuro Bunko.app/Contents/MacOS/mokuro-bunko",
        ));
        assert_eq!(
            plist_program(&p),
            Some(PathBuf::from(
                "/Users/a/R&D/Mokuro Bunko.app/Contents/MacOS/mokuro-bunko"
            ))
        );
        let info = "<dict>\n  <key>CFBundleExecutable</key>\n  <string>mokuro-bunko-tray</string>\n  <key>X</key><string>mokuro-bunko-tray</string>\n</dict>";
        let new = set_bundle_executable(info, "mokuro-bunko").unwrap();
        assert!(new.contains("<key>CFBundleExecutable</key>\n  <string>mokuro-bunko</string>"));
        assert!(new.contains("<key>X</key><string>mokuro-bunko-tray</string>"));
        assert_eq!(set_bundle_executable(&new, "mokuro-bunko"), None);
        assert_eq!(set_bundle_executable("<dict/>", "x"), None);
    }

    #[test]
    fn shortcut_bytes() {
        let path = Path::new(r"C:\Users\A\AppData\Local\mokuro-bunko\app\mokuro-bunko-tray.exe");
        let mut lnk = b"L\0\0\0junk".to_vec();
        lnk.extend(
            r"c:\users\a\appdata\local\MOKURO-BUNKO\app\mokuro-bunko-tray.exe"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        assert!(link_names(&lnk, path));
        let ansi = b"xx C:\\Users\\A\\AppData\\Local\\mokuro-bunko\\app\\mokuro-bunko-tray.exe\0";
        assert!(link_names(ansi, path));
        assert!(!link_names(b"C:\\other\\mokuro-bunko-tray.exe", path));
    }

    #[cfg(target_os = "linux")]
    struct Env(std::collections::HashMap<String, std::ffi::OsString>);

    #[cfg(target_os = "linux")]
    impl tpaths::Env for Env {
        fn var(&self, name: &str) -> Option<std::ffi::OsString> {
            self.0.get(name).cloned()
        }
    }

    /// The Linux install of `install.sh` after beta.2's updater put this release in:
    /// login items and the link of the old tray move to `mokuro-bunko tray`, the old
    /// tray goes; another install's entry is left alone.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_install_from_beta2() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let lib = home.join(".local/lib/mokuro-bunko");
        let bin = home.join(".local/bin");
        for p in [
            &lib,
            &bin,
            &home.join(".config/autostart"),
            &home.join(".local/share/applications"),
        ] {
            std::fs::create_dir_all(p).unwrap();
        }
        let exe = lib.join("mokuro-bunko");
        let tray = lib.join("mokuro-bunko-tray");
        std::fs::write(&exe, b"new").unwrap();
        std::fs::write(&tray, b"old tray").unwrap();
        std::fs::write(lib.join(".mokuro-bunko-tray.previous"), b"older").unwrap();
        std::os::unix::fs::symlink(&tray, bin.join("mokuro-bunko-tray")).unwrap();
        let entry =
            |exe: &Path| bunko_tray::autostart::desktop_entry(exe, true).replace(" tray\n", "\n");
        let autostart = home.join(".config/autostart/mokuro-bunko-tray.desktop");
        std::fs::write(&autostart, entry(&tray)).unwrap();
        let menu = home.join(".local/share/applications/mokuro-bunko-tray.desktop");
        std::fs::write(&menu, entry(&tray)).unwrap();
        let env = Env([
            ("HOME".to_string(), home.clone().into_os_string()),
            ("PATH".to_string(), bin.clone().into_os_string()),
        ]
        .into_iter()
        .collect());
        let m = Migration::new(&exe, &env);
        assert_eq!(m.stale, vec![tray.clone()]);
        assert_eq!(m.run(Caller::Server, ""), Outcome::Continue);
        let text = std::fs::read_to_string(&autostart).unwrap();
        assert!(
            text.contains(&format!("\nExec={} tray\n", exe.display())),
            "{text}"
        );
        assert!(text.contains("X-GNOME-Autostart-enabled=true"));
        let text = std::fs::read_to_string(&menu).unwrap();
        assert!(
            text.contains(&format!("\nExec={} tray\n", exe.display())),
            "{text}"
        );
        assert!(!text.contains("X-GNOME-Autostart"));
        assert!(!tray.exists() && !lib.join(".mokuro-bunko-tray.previous").exists());
        assert!(std::fs::symlink_metadata(bin.join("mokuro-bunko-tray")).is_err());
        // Nothing left: a second run changes nothing.
        let before = std::fs::read_to_string(&autostart).unwrap();
        assert_eq!(m.run(Caller::Server, ""), Outcome::Continue);
        assert_eq!(std::fs::read_to_string(&autostart).unwrap(), before);
        // Another install's tray entry stays.
        let other = d.path().join("other/mokuro-bunko-tray");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, b"x").unwrap();
        std::fs::write(&autostart, entry(&other)).unwrap();
        assert_eq!(m.run(Caller::Server, ""), Outcome::Continue);
        assert_eq!(std::fs::read_to_string(&autostart).unwrap(), entry(&other));
    }
}
