//! A library in a folder chosen by path (`storage.library.backend: folder`): the
//! checks around it. The folder is opened like any cloud folder
//! (`bunko_remote::folder::FolderBackend` behind a `RemoteStore`); what is particular
//! is only where it may be: never Bunko's own folder, nor a folder that holds it.
//!
//! The path is kept as written (`~` expanded, `.` and `..` removed: the store id's
//! account, [`bunko_remote::folder::account_id`]); the checks resolve whatever of it
//! exists, so a symlink or a second spelling cannot slip past them.

use std::path::{Component, Path, PathBuf};

use bunko_remote::auth::{CredentialStore, StoredCredential};

/// The `credentials.json` provider name of a folder store.
pub const PROVIDER: &str = "folder";

/// "Enter the folder's full path."
pub const EMPTY: &str = "Enter the folder's full path.";
/// A relative path.
pub const RELATIVE: &str = "Enter the full path: it starts with / or a drive letter.";

/// Who names the library folder, which decides which folders it may name.
///
/// A folder library makes Bunko read, write, rename and trash files in that folder, so
/// naming one is as strong as a shell on the server for whoever may name any folder.
/// Only the server's own operator may: its config, its command line, and a browser on
/// the server itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Asker {
    /// `config.yaml` or the environment, the CLI (`library connect folder`) and the
    /// desktop app: any folder but those refused for everyone, a Windows network share
    /// included.
    Operator,
    /// The setup wizard or the admin panel in a browser on this machine
    /// ([`crate::accounts::local::browser_on_this_machine`]): any folder but those
    /// refused for everyone; never a network share or device path (Windows would
    /// connect to it, sending this server's credentials, before anything checks it).
    BrowserHere,
    /// Any other HTTP caller (an admin, or a setup-code holder, on another computer; a
    /// request a proxy forwarded): only a folder at or under a root of
    /// `storage.library.folder_roots`, which only the operator sets; and every refusal
    /// reads the same whatever is on the disk.
    Elsewhere,
}

impl Asker {
    /// Any folder (but those refused for everyone) may be named.
    pub fn anywhere(self) -> bool {
        self != Asker::Elsewhere
    }

    /// The path came in through the setup or admin HTTP API.
    pub fn http(self) -> bool {
        self != Asker::Operator
    }
}

/// The folder library at `path` for `asker`, checked against Bunko's own folder `base`
/// and the operator's `roots` (`storage.library.folder_roots`). Refused:
///
/// - for everyone: `base`, a folder holding it, anything inside it but `<base>/library`
///   (allowed, so a library on this computer can become a folder library where it is);
///   Bunko's settings folder, inside it or holding it; the top of the system disk and
///   the system's own folders ([`system_folder`]);
/// - from the HTTP API: a Windows network share or device path, before anything on
///   the disk is looked at ([`windows_network_or_device`]);
/// - from [`Asker::Elsewhere`]: anything not at or under a root, checked as written
///   before anything on the disk is looked at, then again resolved (a symbolic link in
///   a root cannot lead out of it).
///
/// Every check after the first ones resolves what exists of the path, so a symlink or
/// a second spelling cannot slip past it. Returns the path as the store id names it
/// (absolute, `~` expanded, lexically normalised, never resolved).
pub fn check_path(
    path: &Path,
    base: &Path,
    roots: &[PathBuf],
    asker: Asker,
) -> Result<PathBuf, String> {
    let typed = path.to_string_lossy();
    if path.as_os_str().is_empty() || typed.trim().is_empty() {
        return Err(EMPTY.into());
    }
    if asker.http() && cfg!(windows) && windows_network_or_device(&typed) {
        return Err(network_refusal(path));
    }
    let full = bunko_core::storage::expand_user(path);
    if !full.is_absolute() {
        return Err(RELATIVE.into());
    }
    let full = PathBuf::from(bunko_remote::folder::account_id(&full));
    if asker.http() && !local_disk_path(&full) {
        return Err(network_refusal(&full));
    }
    if asker == Asker::Elsewhere {
        // As written first: nothing outside the roots is ever looked at.
        let lexical: Vec<PathBuf> = roots.iter().map(|r| written(r)).collect();
        if !lexical.iter().any(|r| inside_root(&full, r)) {
            return Err(outside_roots(&full, roots));
        }
    }
    let base_shown = base.display().to_string();
    let base_r = fold(&resolve(base));
    let lib_r = fold(&resolve(&base.join("library")));
    let resolved = resolve(&full);
    let path_r = fold(&resolved);
    // Bunko's own library folder may sit under a system folder (a service's data in
    // /var/lib): it is Bunko's, so it stays allowed.
    if !path_r.starts_with(&lib_r) && (system_folder(&full) || system_folder(&resolved)) {
        return Err(format!(
            "{} is a system folder: Bunko never keeps a library there. Choose a folder of your own.",
            full.display()
        ));
    }
    if path_r == base_r || (path_r.starts_with(&base_r) && !path_r.starts_with(&lib_r)) {
        return Err(format!(
            "That folder is part of Bunko's own folder ({base_shown}). Choose a folder outside it, or {}.",
            base.join("library").display()
        ));
    }
    if base_r.starts_with(&path_r) {
        return Err(format!(
            "That folder holds Bunko's own folder ({base_shown}). Choose the manga folder itself."
        ));
    }
    if let Some(settings) = bunko_core::storage::default_config_path().parent() {
        let settings_r = fold(&resolve(settings));
        if path_r.starts_with(&settings_r) {
            return Err(format!(
                "That folder is part of Bunko's settings folder ({}). Choose a folder outside it.",
                settings.display()
            ));
        }
        if settings_r.starts_with(&path_r) {
            return Err(format!(
                "That folder holds Bunko's settings folder ({}). Choose the manga folder itself.",
                settings.display()
            ));
        }
    }
    if asker == Asker::Elsewhere && !roots.iter().any(|r| inside_root(&resolved, &resolve(r))) {
        return Err(outside_roots(&full, roots));
    }
    Ok(full)
}

/// A root of `storage.library.folder_roots` as written: `~` expanded, normalised.
fn written(root: &Path) -> PathBuf {
    PathBuf::from(bunko_remote::folder::account_id(
        &bunko_core::storage::expand_user(root),
    ))
}

/// `p` is `root` or inside it. Exact on hosts whose paths keep case (a folder that
/// differs from a root only in case is another folder there); without case on Windows.
fn inside_root(p: &Path, root: &Path) -> bool {
    if cfg!(windows) {
        fold(p).starts_with(fold(root))
    } else {
        p.starts_with(root)
    }
}

/// The refusal for a folder outside the roots, saying how to allow it.
fn outside_roots(path: &Path, roots: &[PathBuf]) -> String {
    let shown = path.display();
    let how = format!(
        "To allow it, set storage.library.folder_roots in config.yaml on the server (or \
         MOKURO_LIBRARY_FOLDER_ROOTS) to it or a folder that holds it and restart Bunko; or \
         connect it from a browser on the server itself, or there with `mokuro-bunko library \
         connect folder --path {shown}`."
    );
    if roots.is_empty() {
        format!(
            "This server lets a browser on another computer connect a library folder only \
             inside the folders its operator allows, and it allows none. {how}"
        )
    } else {
        let list: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
        format!(
            "{shown} is not inside the folders this server lets a browser on another computer \
             connect ({}). {how}",
            list.join(", ")
        )
    }
}

/// The refusal for a network share or a device path from the setup or admin pages.
fn network_refusal(path: &Path) -> String {
    format!(
        "{} is a network share or a device: from the setup or admin pages Bunko takes a folder \
         on this server's own disks only. Set a share on the server itself \
         (storage.library.path in config.yaml, MOKURO_LIBRARY_PATH, or `mokuro-bunko library \
         connect folder --path`).",
        path.display()
    )
}

/// A path in Windows' syntax that names another computer or a device, from its text
/// alone: `\\server\share` and `//server/share`, the verbatim `\\?\…` forms (`\\?\UNC\…`
/// included), device paths `\\.\…` and NT paths `\??\…`. Windows opens such a path by
/// connecting to that computer, sending this server's credentials (NTLM), so the
/// setup and admin pages refuse it before anything looks at it. Pure: tested on every
/// host.
pub fn windows_network_or_device(text: &str) -> bool {
    let t = text.trim_start();
    let mut c = t.chars();
    let sep = |c: Option<char>| matches!(c, Some('\\' | '/'));
    (sep(c.next()) && sep(c.next())) || t.starts_with(r"\??\")
}

/// `p` (absolute) is on one of this machine's disks: on Windows, its prefix is a plain
/// drive letter (`C:`), not a share, a verbatim or a device prefix. Always on other
/// hosts.
fn local_disk_path(p: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::path::Prefix;
        match p.components().next() {
            Some(Component::Prefix(pre)) => matches!(pre.kind(), Prefix::Disk(_)),
            _ => false,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = p;
        true
    }
}

/// The top of the system disk, or a system folder (or a folder in one): no library
/// may be there, whoever asks. Matched without case, on the path as written and as
/// resolved.
pub fn system_folder(p: &Path) -> bool {
    #[cfg(windows)]
    {
        windows_system_folder(&p.to_string_lossy(), &|k| std::env::var(k).ok())
    }
    #[cfg(not(windows))]
    {
        unix_system_folder(&p.to_string_lossy(), cfg!(target_os = "macos"))
    }
}

/// `/var`'s system parts (also under macOS's `/private/var`). The rest of `/var` holds
/// people's own folders on some systems (`/var/home`, `/var/mnt` on Fedora Atomic).
const VAR_SYSTEM: [&str; 9] = [
    "lib", "log", "cache", "spool", "run", "lock", "db", "empty", "mail",
];

/// [`system_folder`] for Linux and the BSDs, and macOS when `macos`. Pure: tested on
/// every host.
// Called on Windows by its tests only.
#[cfg_attr(windows, allow(dead_code))]
fn unix_system_folder(p: &str, macos: bool) -> bool {
    let p = p.to_lowercase();
    let p = p.trim_end_matches('/');
    let p = if p.is_empty() { "/" } else { p };
    let under = |top: &str| p == top || p.strip_prefix(top).is_some_and(|r| r.starts_with('/'));
    // Removable drives udisks mounts (`/run/media/<user>/<label>`) are people's own.
    if under("/run/media") {
        return false;
    }
    if ["/", "/var", "/private", "/private/var"].contains(&p) {
        return true;
    }
    const UNDER: [&str; 14] = [
        "/bin", "/boot", "/dev", "/etc", "/lib", "/lib32", "/lib64", "/libx32", "/proc", "/run",
        "/sbin", "/sys", "/usr", "/root",
    ];
    if UNDER.iter().any(|t| under(t)) {
        return true;
    }
    if VAR_SYSTEM
        .iter()
        .any(|v| under(&format!("/var/{v}")) || under(&format!("/private/var/{v}")))
    {
        return true;
    }
    macos
        && ["/system", "/library", "/private/etc", "/private/var/root"]
            .iter()
            .any(|t| under(t))
}

/// [`system_folder`] for Windows, its folders read from `env` (`SystemDrive`,
/// `SystemRoot`, `ProgramFiles`, ...; the usual ones when unset): the system drive's
/// top (another drive's top is a fine library: a disk of its own), the Windows and
/// program folders, `ProgramData`, and any drive's recycle bin and `System Volume
/// Information`. Pure: tested on every host.
// Called elsewhere by its tests only.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_system_folder(p: &str, env: &dyn Fn(&str) -> Option<String>) -> bool {
    fn norm(s: &str) -> String {
        let s = s.replace('/', "\\").to_lowercase();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s).to_string();
        let t = s.trim_end_matches('\\');
        // `c:` and `c:\` are both the drive's top.
        if t.len() == 2 && t.ends_with(':') {
            format!("{t}\\")
        } else {
            t.to_string()
        }
    }
    let p = norm(p);
    let under = |top: &str| {
        let top = norm(top);
        let top = top.trim_end_matches('\\');
        p.trim_end_matches('\\') == top || p.strip_prefix(top).is_some_and(|r| r.starts_with('\\'))
    };
    let drive = env("SystemDrive").unwrap_or_else(|| "C:".into());
    if p == norm(&drive) {
        return true;
    }
    let windir = env("SystemRoot")
        .or_else(|| env("windir"))
        .unwrap_or_else(|| format!(r"{drive}\Windows"));
    let mut tops = vec![windir];
    for (var, fallback) in [
        ("ProgramFiles", r"\Program Files"),
        ("ProgramFiles(x86)", r"\Program Files (x86)"),
        ("ProgramW6432", r"\Program Files"),
        ("ProgramData", r"\ProgramData"),
    ] {
        tops.push(env(var).unwrap_or_else(|| format!("{drive}{fallback}")));
    }
    if tops.iter().any(|t| under(t)) {
        return true;
    }
    // Any drive's recycle bin and volume information.
    let b = p.as_bytes();
    b.len() >= 3
        && b[1] == b':'
        && b[2] == b'\\'
        && ["$recycle.bin", "system volume information"]
            .iter()
            .any(|d| under(&format!("{}{d}", &p[..3])))
}

/// What the connect Check says before the folder is opened: a file in its place, or
/// neither the folder nor its parent (a typo, a drive not connected). `None`: go on
/// (the folder is there, or only it is missing: "Create it?"). An [`Asker::Elsewhere`]
/// gets one answer for both, so the Check tells it nothing of what is on the disk.
// unmounted: the library folder itself is a plain path on this computer.
pub fn precheck(path: &Path, asker: Asker) -> Option<String> {
    match bunko_store::fs::metadata(path) {
        Ok(m) if m.is_dir() => None,
        Ok(_) if !asker.anywhere() => Some(unusable(path)),
        Ok(_) => Some(format!("{} is a file, not a folder.", path.display())),
        Err(_) => match path.parent() {
            Some(parent) if !bunko_store::fs::is_dir(parent) => Some(missing_parent(path, asker)),
            _ => None,
        },
    }
}

/// The Check's copy for a folder whose parent is missing too (for an
/// [`Asker::Elsewhere`], the one answer it gets for any folder it cannot use).
pub fn missing_parent(path: &Path, asker: Asker) -> String {
    if !asker.anywhere() {
        return unusable(path);
    }
    let parent = path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    format!(
        "There is no folder at {}, and {parent} is missing too: check the path, or connect the drive.",
        path.display()
    )
}

/// The one answer an [`Asker::Elsewhere`] gets for a folder it cannot use.
fn unusable(path: &Path) -> String {
    format!(
        "There is no folder Bunko can use at {}: check the path.",
        path.display()
    )
}

/// The credentials stub of a folder store (§2.4): no secret, only the path, so the
/// machinery every cloud store has (the `use_local` refusal, Disconnect, an account
/// change) treats it like one.
pub fn stub(account: &str) -> StoredCredential {
    StoredCredential {
        provider: PROVIDER.into(),
        account_id: Some(account.to_string()),
        account_label: Some(account.to_string()),
        ..StoredCredential::default()
    }
}

/// Write the stub of store `id` when `cloud` has none (an env-only Docker setup never
/// ran `library connect`). Failures are logged: the store opens without it.
pub fn ensure_stub(cloud: &Path, id: &str, account: &str) {
    let creds = match CredentialStore::open(cloud) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("[STORAGE] The folder library's credentials entry: {e}");
            return;
        }
    };
    match creds.get(id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            if let Err(e) = creds.put(id, stub(account)) {
                tracing::warn!(
                    "[STORAGE] Could not write the folder library's credentials entry: {e}"
                );
            }
        }
        Err(e) => tracing::warn!("[STORAGE] The folder library's credentials entry: {e}"),
    }
}

/// `p` with whatever of it exists resolved (symlinks, `..`, the on-disk spelling where
/// the filesystem gives it), the rest appended as written. A link whose target is
/// missing is followed too: it leads there the moment that target is made.
// unmounted: Bunko's own folder and the library folder are plain paths.
fn resolve(p: &Path) -> PathBuf {
    resolve_in(p, 0)
}

/// The most links [`resolve`] follows by hand, as many as the kernel does.
const MAX_LINKS: u32 = 40;

fn resolve_in(p: &Path, links: u32) -> PathBuf {
    let p = PathBuf::from(bunko_remote::folder::account_id(p));
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.as_path();
    loop {
        if let Ok(c) = bunko_store::fs::canonicalize(cur) {
            let mut out = strip_verbatim(c);
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        // A dangling link (canonicalize found no target): where it points, from its
        // resolved folder, then the rest.
        if links < MAX_LINKS
            && let Ok(target) = bunko_store::fs::read_link(cur)
        {
            let mut next = match cur.parent() {
                Some(parent) => resolve_in(parent, links + 1).join(target),
                None => target,
            };
            for part in rest.iter().rev() {
                next.push(part);
            }
            return resolve_in(&next, links + 1);
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return p,
        }
    }
}

/// Windows' `canonicalize` answers `\\?\C:\…`; the paths compared are plain.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    p
}

/// Paths compared without case: library paths follow NTFS rules on every host, and a
/// folder that differs from Bunko's own only in case is refused rather than risked.
fn fold(p: &Path) -> PathBuf {
    // Every component, the drive letter included (`c:` is `C:`).
    p.components()
        .map(|c: Component<'_>| -> std::ffi::OsString {
            c.as_os_str().to_string_lossy().to_lowercase().into()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`check_path`] for the operator (config, CLI): any folder but those refused for
    /// everyone.
    fn op(p: &Path, base: &Path) -> Result<PathBuf, String> {
        check_path(p, base, &[], Asker::Operator)
    }

    #[test]
    fn check_path_refuses_bunkos_own_folder() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("bunko");
        bunko_store::fs::create_dir_all(base.join("library")).unwrap();
        bunko_store::fs::create_dir_all(dir.path().join("manga")).unwrap();
        // Outside: taken, as written.
        let manga = dir.path().join("manga");
        assert_eq!(op(&manga, &base).unwrap(), manga);
        // A missing folder outside is fine too ("Create it?" comes later).
        assert!(op(&dir.path().join("new/manga"), &base).is_ok());
        // <base>/library and inside it: allowed.
        assert!(op(&base.join("library"), &base).is_ok());
        assert!(op(&base.join("library/sub"), &base).is_ok());
        // The base itself, anything else inside it, and a folder holding it: refused.
        for (p, says) in [
            (base.clone(), "part of Bunko's own folder"),
            (base.join("cloud"), "part of Bunko's own folder"),
            (base.join("users/x"), "part of Bunko's own folder"),
            (base.join("library/../cloud"), "part of Bunko's own folder"),
            (dir.path().to_path_buf(), "holds Bunko's own folder"),
        ] {
            let e = op(&p, &base).unwrap_err();
            assert!(e.contains(says), "{p:?}: {e}");
        }
        // Another spelling of the base is the base.
        let upper = dir.path().join("BUNKO").join("cloud");
        assert!(op(&upper, &base).is_err());
        assert_eq!(op(Path::new(""), &base).unwrap_err(), EMPTY);
        assert_eq!(op(Path::new("manga"), &base).unwrap_err(), RELATIVE);
    }

    #[cfg(unix)]
    #[test]
    fn check_path_sees_through_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("bunko");
        bunko_store::fs::create_dir_all(base.join("cloud")).unwrap();
        let link = dir.path().join("sneaky");
        std::os::unix::fs::symlink(base.join("cloud"), &link).unwrap();
        assert!(op(&link, &base).is_err());
        // A link to a system folder is that folder.
        let etc = dir.path().join("etc-link");
        std::os::unix::fs::symlink("/etc", &etc).unwrap();
        let e = op(&etc, &base).unwrap_err();
        assert!(e.contains("is a system folder"), "{e}");
    }

    /// Bunko's settings folder (where config.yaml lives by default): refused, inside it
    /// and holding it, for everyone.
    #[test]
    fn check_path_refuses_the_settings_folder() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("bunko");
        let settings = bunko_core::storage::default_config_path()
            .parent()
            .unwrap()
            .to_path_buf();
        for p in [settings.clone(), settings.join("sub")] {
            let e = op(&p, &base).unwrap_err();
            assert!(e.contains("part of Bunko's settings folder"), "{p:?}: {e}");
        }
        // Its parent may hold Bunko's own folder too (on Windows both are in
        // %LOCALAPPDATA%): either refusal.
        let p = settings.parent().unwrap();
        let e = op(p, &base).unwrap_err();
        assert!(e.contains("holds Bunko's"), "{p:?}: {e}");
    }

    #[test]
    fn unix_system_folders() {
        for p in [
            "/",
            "//",
            "/etc",
            "/etc/",
            "/ETC",
            "/usr/lib/x",
            "/proc/1",
            "/run",
            "/run/user/1000",
            "/var",
            "/var/lib",
            "/var/lib/docker/volumes",
            "/var/log/x",
            "/var/run",
            "/root",
            "/boot/efi",
            "/dev/shm",
            "/sys",
            "/sbin",
            "/bin",
            "/lib64",
        ] {
            assert!(unix_system_folder(p, false), "{p}");
        }
        for p in [
            "/home/me/Manga",
            "/library",
            "/Library",
            "/srv/manga",
            "/mnt/disk/manga",
            "/media/me/USB",
            "/run/media/me/USB",
            "/tmp/x",
            "/var/home/me/Manga",
            "/var/mnt/disk",
            "/opt/manga",
            "/data",
            "/volume1/manga",
            "/etcetera",
            "/usrdata",
            "/private/etc",
        ] {
            assert!(!unix_system_folder(p, false), "{p}");
        }
        // macOS adds its own; its temporary folders (/private/var/folders) stay fine.
        for p in [
            "/System",
            "/System/Library",
            "/Library",
            "/library/x",
            "/private",
            "/private/etc",
            "/private/var",
            "/private/var/db/x",
            "/private/var/root",
        ] {
            assert!(unix_system_folder(p, true), "{p}");
        }
        for p in [
            "/Users/me/Manga",
            "/Volumes/Manga",
            "/private/var/folders/x/T/y",
            "/var/folders/x",
            "/Users/me/Library-Manga",
        ] {
            assert!(!unix_system_folder(p, true), "{p}");
        }
    }

    #[test]
    fn windows_system_folders() {
        let none = |_: &str| None;
        for p in [
            r"C:\",
            "C:",
            "c:/",
            r"\\?\C:\",
            r"C:\Windows",
            r"c:\windows\System32",
            r"C:\Program Files",
            r"C:\Program Files\x",
            r"C:\Program Files (x86)",
            r"C:\ProgramData",
            r"C:\ProgramData\x",
            r"D:\$Recycle.Bin",
            r"E:\System Volume Information\x",
        ] {
            assert!(windows_system_folder(p, &none), "{p}");
        }
        for p in [
            r"D:\",
            "D:",
            r"E:\Manga",
            r"C:\Users\me\Manga",
            r"C:\Manga",
            r"C:\Windows Manga",
            r"C:\Program FilesX",
            r"D:\Windows",
        ] {
            assert!(!windows_system_folder(p, &none), "{p}");
        }
        // The folders come from the environment where it names them.
        let env = |k: &str| match k {
            "SystemDrive" => Some("D:".to_string()),
            "SystemRoot" => Some(r"D:\WINNT".to_string()),
            "ProgramFiles" => Some(r"D:\Apps".to_string()),
            _ => None,
        };
        for p in [r"D:\", r"D:\WINNT\x", r"D:\Apps", r"D:\ProgramData"] {
            assert!(windows_system_folder(p, &env), "{p}");
        }
        for p in [r"C:\", r"C:\Windows", r"D:\Manga"] {
            assert!(!windows_system_folder(p, &env), "{p}");
        }
    }

    /// Network shares and device paths, in every spelling Windows takes, from their
    /// text alone; drive paths and Unix paths are neither.
    #[test]
    fn windows_network_and_device_paths() {
        for p in [
            r"\\files.example.com\manga",
            r"\\files.example.com\manga\Series",
            "//files.example.com/manga",
            r"/\files.example.com\manga",
            r"\\?\UNC\files.example.com\manga",
            r"\\?\C:\Manga",
            r"\\.\pipe\x",
            r"\\.\C:\Manga",
            r"\??\C:\Manga",
            r"  \\192.0.2.7\share",
        ] {
            assert!(windows_network_or_device(p), "{p}");
        }
        for p in [
            r"C:\Manga",
            "D:/Manga",
            r"\Manga",
            "/srv/manga",
            "manga",
            "",
        ] {
            assert!(!windows_network_or_device(p), "{p}");
        }
    }

    /// The setup and admin pages never name a network share or a device path: refused
    /// before anything looks at it, even inside a root that names the share. The
    /// operator (config, CLI) still may.
    #[cfg(windows)]
    #[test]
    fn check_path_refuses_shares_and_devices_over_http() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("bunko");
        let roots = [PathBuf::from(r"\\files.example.com\manga")];
        for asker in [Asker::BrowserHere, Asker::Elsewhere] {
            for p in [
                r"\\files.example.com\manga",
                r"\\files.example.com\manga\Series",
                "//files.example.com/manga",
                r"\\?\UNC\files.example.com\manga",
                r"\\?\C:\Manga",
                r"\\.\C:\Manga",
            ] {
                let e = check_path(Path::new(p), &base, &roots, asker).unwrap_err();
                assert!(
                    e.contains("network share or a device"),
                    "{asker:?} {p}: {e}"
                );
            }
        }
    }

    /// A caller elsewhere: only a folder at or under a root, as written and as
    /// resolved; nothing at all without roots. Every refusal says how to allow it.
    #[test]
    fn a_caller_elsewhere_stays_inside_the_roots() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("bunko");
        let root = dir.path().join("library");
        let outside = dir.path().join("elsewhere");
        bunko_store::fs::create_dir_all(root.join("Manga")).unwrap();
        bunko_store::fs::create_dir_all(&outside).unwrap();
        let ok = |p: &Path, roots: &[PathBuf]| check_path(p, &base, roots, Asker::Elsewhere);
        // No roots: nothing, with the way to allow it.
        let e = ok(&root, &[]).unwrap_err();
        for says in [
            "allows none",
            "storage.library.folder_roots",
            "MOKURO_LIBRARY_FOLDER_ROOTS",
            "a browser on the server itself",
            "mokuro-bunko library connect folder --path",
        ] {
            assert!(e.contains(says), "{says}: {e}");
        }
        let roots = [root.clone()];
        assert_eq!(ok(&root, &roots).unwrap(), root);
        assert!(ok(&root.join("Manga"), &roots).is_ok());
        assert!(ok(&root.join("New/Deeper"), &roots).is_ok());
        for p in [
            outside.clone(),
            dir.path().to_path_buf(),
            root.join("../elsewhere"),
            dir.path().join("library-2"),
        ] {
            let e = ok(&p, &roots).unwrap_err();
            assert!(e.contains("is not inside the folders"), "{p:?}: {e}");
            assert!(e.contains(&root.display().to_string()), "{p:?}: {e}");
        }
        // The rules for everyone still hold inside a root.
        let wide = [dir.path().to_path_buf()];
        let e = ok(&base.join("cloud"), &wide).unwrap_err();
        assert!(e.contains("part of Bunko's own folder"), "{e}");
        // The operator and a browser here are not held to the roots.
        assert!(check_path(&outside, &base, &[], Asker::Operator).is_ok());
        assert!(check_path(&outside, &base, &[], Asker::BrowserHere).is_ok());
    }

    /// A symbolic link in a root that leads out of it: refused for a caller elsewhere
    /// (the check is made again on the resolved path); a case variant of a root is
    /// another folder on a host that keeps case.
    #[cfg(unix)]
    #[test]
    fn a_link_out_of_a_root_is_outside_it() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("bunko");
        let root = dir.path().join("library");
        let outside = dir.path().join("elsewhere");
        bunko_store::fs::create_dir_all(&root).unwrap();
        bunko_store::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
        let roots = [root.clone()];
        let e = check_path(&root.join("out"), &base, &roots, Asker::Elsewhere).unwrap_err();
        assert!(e.contains("is not inside the folders"), "{e}");
        let e = check_path(&root.join("out/sub"), &base, &roots, Asker::Elsewhere).unwrap_err();
        assert!(e.contains("is not inside the folders"), "{e}");
        if !cfg!(target_os = "macos") {
            let upper = dir.path().join("LIBRARY");
            assert!(check_path(&upper, &base, &roots, Asker::Elsewhere).is_err());
        }
        // A root that is itself a link is followed (a path is named under the root as
        // the operator wrote it).
        let linked = dir.path().join("root-link");
        std::os::unix::fs::symlink(&root, &linked).unwrap();
        let roots = [linked.clone()];
        assert!(check_path(&linked.join("Manga"), &base, &roots, Asker::Elsewhere).is_ok());
        assert!(check_path(&linked.join("out"), &base, &roots, Asker::Elsewhere).is_err());
    }

    /// A link in a root whose target is missing still leads where it points: refused
    /// for a caller elsewhere when that is out of the roots (it would lead out the
    /// moment the target is made), and for everyone when it points into a system
    /// folder or Bunko's own.
    #[cfg(unix)]
    #[test]
    fn a_dangling_link_leads_where_it_points() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("bunko");
        let root = dir.path().join("library");
        let outside = dir.path().join("elsewhere");
        bunko_store::fs::create_dir_all(root.join("Inside")).unwrap();
        bunko_store::fs::create_dir_all(&outside).unwrap();
        let link = |name: &str, to: &Path| std::os::unix::fs::symlink(to, root.join(name)).unwrap();
        link("out", &outside.join("later"));
        link("out-deep", &outside.join("a/b"));
        link("out-rel", Path::new("../elsewhere/later"));
        link("chain", Path::new("out"));
        link("in", Path::new("Inside/later"));
        link("etc", Path::new("/etc/bunko-later"));
        link("own", &base.join("cloud/later"));
        let roots = [root.clone()];
        let elsewhere = |p: &Path| check_path(p, &base, &roots, Asker::Elsewhere);
        for name in [
            "out",
            "out-deep",
            "out-rel",
            "chain",
            "out/sub",
            "chain/sub",
        ] {
            let e = elsewhere(&root.join(name)).unwrap_err();
            assert!(e.contains("is not inside the folders"), "{name}: {e}");
        }
        // One that stays inside the root is fine.
        assert!(elsewhere(&root.join("in")).is_ok());
        let e = op(&root.join("etc"), &base).unwrap_err();
        assert!(e.contains("is a system folder"), "{e}");
        let e = op(&root.join("own"), &base).unwrap_err();
        assert!(e.contains("part of Bunko's own folder"), "{e}");
        // A loop resolves to nothing usable, and never hangs.
        link("loop", Path::new("loop"));
        let _ = elsewhere(&root.join("loop"));
    }

    #[test]
    fn precheck_names_a_file_and_a_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        bunko_store::fs::write(&file, b"x").unwrap();
        for asker in [Asker::Operator, Asker::BrowserHere] {
            assert!(
                precheck(&file, asker)
                    .unwrap()
                    .contains("is a file, not a folder")
            );
            assert_eq!(precheck(dir.path(), asker), None);
            assert_eq!(precheck(&dir.path().join("new"), asker), None);
            let e = precheck(&dir.path().join("gone/manga"), asker).unwrap();
            assert!(e.contains("is missing too"), "{e}");
        }
    }

    /// For a caller elsewhere the Check is no oracle of the disk: a file and a path
    /// whose parent is missing read the same.
    #[test]
    fn precheck_tells_a_caller_elsewhere_nothing_of_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        bunko_store::fs::write(&file, b"x").unwrap();
        let shape = |p: &Path| {
            precheck(p, Asker::Elsewhere)
                .unwrap()
                .replace(&p.display().to_string(), "<path>")
        };
        let a = shape(&file);
        let b = shape(&dir.path().join("gone/manga"));
        assert_eq!(a, b);
        assert_eq!(
            a,
            "There is no folder Bunko can use at <path>: check the path."
        );
        assert_eq!(
            missing_parent(&file, Asker::Elsewhere).replace(&file.display().to_string(), "<path>"),
            a
        );
        assert_eq!(precheck(dir.path(), Asker::Elsewhere), None);
    }

    #[test]
    fn ensure_stub_writes_once() {
        let dir = tempfile::tempdir().unwrap();
        let cloud = dir.path().join("cloud");
        ensure_stub(&cloud, "0123456789abcdef", "/srv/manga");
        let creds = CredentialStore::open(&cloud).unwrap();
        let c = creds.get("0123456789abcdef").unwrap().unwrap();
        assert_eq!(c.provider, "folder");
        assert_eq!(c.account_id.as_deref(), Some("/srv/manga"));
        assert_eq!(c.account_label.as_deref(), Some("/srv/manga"));
        assert_eq!(c.webdav_password, None);
        // An entry on file is left alone.
        ensure_stub(&cloud, "0123456789abcdef", "/elsewhere");
        let c = creds.get("0123456789abcdef").unwrap().unwrap();
        assert_eq!(c.account_id.as_deref(), Some("/srv/manga"));
    }
}
