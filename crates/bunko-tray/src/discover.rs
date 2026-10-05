//! Finding the running instances: every long-running `mokuro-bunko` writes
//! `<storage>/.control.json` (GUI.md §1). The tray looks in every storage this machine
//! might use: the library server's (`MOKURO_STORAGE`, `config.yaml`'s
//! `storage.base_path`, the default, the portable `data\`) and the processor's (the
//! `processor.storage` of every `processor.yaml` it can find — `MOKURO_PROCESSOR_CONFIG`,
//! the default, the ones named by an installed service or by `tray.json` — and the
//! default processor storage).

use crate::paths::{self, Env, Layout};
use crate::status::ControlFile;
use crate::trayconf::TrayConfig;
use std::path::{Path, PathBuf};

pub const CONTROL_FILE: &str = ".control.json";

/// Read `<storage>/.control.json` (None when absent or unreadable, or on Unix when it
/// is a symlink, not ours, or readable by others: see
/// `bunko_control::read_private_file`).
pub fn read_control(storage: &Path) -> Option<ControlFile> {
    let text = bunko_control::read_private_file(&storage.join(CONTROL_FILE))?;
    serde_json::from_str(&text).ok()
}

/// Every storage directory worth looking at, most specific first, without duplicates.
pub fn candidate_storages(
    env: &dyn Env,
    layout: &Layout,
    tray: Option<&TrayConfig>,
    extra: &[PathBuf],
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = extra.to_vec();
    if let Some(data) = &layout.portable_data {
        out.push(data.clone());
    }
    if let Some(s) = env.path("MOKURO_STORAGE") {
        out.push(s);
    }
    let mut server_configs = Vec::new();
    if let Some(c) = env.path("MOKURO_CONFIG") {
        server_configs.push(c);
    }
    if let Some(data) = &layout.portable_data {
        server_configs.push(data.join("config.yaml"));
    }
    server_configs.push(paths::server_default_config(env));
    server_configs.extend(configs_named_by_services(env, "server"));
    for c in &server_configs {
        if let Some(s) = server_storage_from_config(c, env) {
            out.push(s);
        }
    }
    out.push(paths::server_default_storage(env));

    for c in processor_configs(env, tray) {
        if let Some(s) = processor_storage_from_config(&c, env) {
            out.push(s);
        }
    }
    out.push(paths::processor_default_storage(env));
    // `mokuro-bunko gui` with no storage of its own to use.
    out.push(paths::gui_fallback_storage(env));
    if cfg!(target_os = "linux") {
        // install.sh --systemd as root: the system units' storage.
        out.push(PathBuf::from("/var/lib/mokuro-bunko/storage"));
    }

    let mut seen = Vec::new();
    out.retain(|p| {
        let key = normalize(p);
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    out
}

fn normalize(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn load_yaml(path: &Path) -> Option<serde_yaml_ng::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_yaml_ng::from_str(&text).ok()
}

fn yaml_path(v: &serde_yaml_ng::Value, section: &str, key: &str) -> Option<String> {
    v.get(section)?
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn expand_user(raw: &str, env: &dyn Env) -> PathBuf {
    if raw == "~" {
        return paths::home(env);
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return paths::home(env).join(rest);
    }
    PathBuf::from(raw)
}

/// `storage.base_path` of a server `config.yaml`.
pub fn server_storage_from_config(path: &Path, env: &dyn Env) -> Option<PathBuf> {
    let v = load_yaml(path)?;
    yaml_path(&v, "storage", "base_path").map(|s| expand_user(&s, env))
}

/// `processor.storage` of a `processor.yaml` (the default storage when the file exists
/// but does not set it).
pub fn processor_storage_from_config(path: &Path, env: &dyn Env) -> Option<PathBuf> {
    let v = load_yaml(path)?;
    Some(
        yaml_path(&v, "processor", "storage")
            .map(|s| expand_user(&s, env))
            .unwrap_or_else(|| paths::processor_default_storage(env)),
    )
}

/// Every `processor.yaml` this machine names somewhere.
pub fn processor_configs(env: &dyn Env, tray: Option<&TrayConfig>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(c) = env.path("MOKURO_PROCESSOR_CONFIG") {
        out.push(c);
    }
    if let Some(tray) = tray {
        out.extend(
            tray.managed
                .iter()
                .filter(|m| m.role == "processor")
                .filter_map(|m| m.config_arg(env)),
        );
    }
    out.push(paths::processor_default_config(env));
    out.extend(configs_named_by_services(env, "processor"));
    if cfg!(target_os = "linux") {
        out.push(PathBuf::from("/etc/mokuro-bunko/processor.yaml"));
    }
    out
}

/// The configs that installed services of `role` pass with `--config` / `-c`.
pub fn configs_named_by_services(env: &dyn Env, role: &str) -> Vec<PathBuf> {
    service_files(env, role)
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .filter_map(|text| config_from_service(&text))
        .collect()
}

pub const PROCESSOR_LAUNCHD_LABEL: &str = "io.github.gnathonic.mokuro-bunko-processor";
pub const SERVER_LAUNCHD_LABEL: &str = "io.github.gnathonic.mokuro-bunko";

/// The files that start `role` with the machine: `processor service --install`,
/// `install.sh --systemd`, and the setup wizard's "start with the machine".
pub fn service_files(env: &dyn Env, role: &str) -> Vec<PathBuf> {
    let processor = role == "processor";
    let mut out = Vec::new();
    if cfg!(windows) {
        if let Some(appdata) = env.path("APPDATA") {
            out.push(startup_dir(&appdata).join(if processor {
                "mokuro-bunko-processor.cmd"
            } else {
                "mokuro-bunko-server.cmd"
            }));
        }
    } else if cfg!(target_os = "macos") {
        let label = if processor {
            PROCESSOR_LAUNCHD_LABEL
        } else {
            SERVER_LAUNCHD_LABEL
        };
        out.push(
            paths::home(env)
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist")),
        );
    } else {
        let unit = if processor {
            "mokuro-bunko-processor.service"
        } else {
            "mokuro-bunko.service"
        };
        out.push(paths::config_base(env).join("systemd/user").join(unit));
        out.push(Path::new("/etc/systemd/system").join(unit));
    }
    out
}

pub fn startup_dir(appdata: &Path) -> PathBuf {
    appdata
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup")
}

/// The `--config` (or `-c`) argument in a systemd unit's `ExecStart`, a Startup `.cmd`
/// or a launchd plist's `ProgramArguments`.
pub fn config_from_service(text: &str) -> Option<PathBuf> {
    let words: Vec<String> = if text.contains("<plist") {
        text.split("<string>")
            .skip(1)
            .filter_map(|s| s.split_once("</string>").map(|(w, _)| xml_unescape(w)))
            .collect()
    } else {
        let line = text.lines().find(|l| {
            let l = l.trim_start();
            l.starts_with("ExecStart=") || (l.contains("mokuro-bunko") && l.contains(" serve"))
        })?;
        split_words(line)
    };
    let i = words.iter().position(|w| w == "--config" || w == "-c")?;
    let value = words.get(i + 1)?;
    // systemd's `%%` and cmd's `%%` are a literal `%`; other specifiers are left as is.
    Some(PathBuf::from(value.replace("%%", "%")))
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Shell-ish word splitting: double quotes group (with `\"`/`""` escapes), else spaces.
fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                chars.next();
                cur.push('"');
            }
            '"' => {
                in_quotes = !in_quotes;
                any = true;
            }
            '\\' if in_quotes && matches!(chars.peek(), Some('"') | Some('\\')) => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c if c.is_whitespace() && !in_quotes => {
                if any || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// Whether process `pid` exists (Unix; elsewhere the HTTP probe decides).
pub fn pid_alive(pid: u32) -> Option<bool> {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return Some(false);
        };
        // SAFETY: kill with signal 0 only checks for existence and permission.
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 {
            return Some(true);
        }
        let err = std::io::Error::last_os_error().raw_os_error();
        Some(err == Some(libc::EPERM))
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

/// A `.control.json` that is still worth probing (its process may exist).
pub fn found(storages: &[PathBuf]) -> Vec<(PathBuf, ControlFile)> {
    storages
        .iter()
        .filter_map(|s| read_control(s).map(|c| (s.clone(), c)))
        .filter(|(_, c)| pid_alive(c.pid) != Some(false))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::test_env::FakeEnv;
    use crate::trayconf::Managed;

    #[test]
    fn service_files_name_their_config() {
        let unit = "[Service]\nType=simple\nExecStart=\"/opt/my apps/mokuro-bunko\" processor serve --config \"/home/a/b c/processor.yaml\"\nRestart=on-failure\n";
        assert_eq!(
            config_from_service(unit),
            Some(PathBuf::from("/home/a/b c/processor.yaml"))
        );
        let unit2 = "ExecStart=/usr/bin/mokuro-bunko processor serve --config /etc/mokuro-bunko/processor.yaml\n";
        assert_eq!(
            config_from_service(unit2),
            Some(PathBuf::from("/etc/mokuro-bunko/processor.yaml"))
        );
        let cmd = "@echo off\r\nrem x\r\nstart \"mokuro-bunko processor\" /min \"C:\\a\\mokuro-bunko.exe\" processor serve --config \"C:\\Users\\a\\100%% sure\\processor.yaml\"\r\n";
        assert_eq!(
            config_from_service(cmd),
            Some(PathBuf::from("C:\\Users\\a\\100% sure\\processor.yaml"))
        );
        let plist = "<?xml?><plist><dict><key>ProgramArguments</key><array>\n<string>/x/mokuro-bunko</string>\n<string>processor</string><string>serve</string><string>--config</string><string>/Users/a/R&amp;D/processor.yaml</string></array></dict></plist>";
        assert_eq!(
            config_from_service(plist),
            Some(PathBuf::from("/Users/a/R&D/processor.yaml"))
        );
        assert_eq!(config_from_service("ExecStart=/x serve\n"), None);
        let server = "@echo off\r\nstart \"mokuro-bunko\" /min \"C:\\mb\\mokuro-bunko.exe\" -c \"C:\\Users\\a\\config.yaml\" serve\r\n";
        assert_eq!(
            config_from_service(server),
            Some(PathBuf::from("C:\\Users\\a\\config.yaml"))
        );
    }

    #[test]
    fn candidates_cover_server_and_processor_storages() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cfg = dir.path().join("cfg");
        std::fs::create_dir_all(cfg.join("mokuro-bunko")).unwrap();
        std::fs::write(
            cfg.join("mokuro-bunko/config.yaml"),
            "storage:\n  base_path: ~/lib\n",
        )
        .unwrap();
        let proc_yaml = dir.path().join("p.yaml");
        std::fs::write(&proc_yaml, "processor:\n  storage: /srv/proc\n").unwrap();
        let env = FakeEnv::default()
            .with("HOME", &home)
            .with("USERPROFILE", &home)
            .with("XDG_CONFIG_HOME", &cfg)
            .with("LOCALAPPDATA", &cfg)
            .with("MOKURO_STORAGE", "/env/storage");
        let tray = TrayConfig {
            managed: vec![Managed {
                role: "processor".into(),
                args: vec![
                    "processor".into(),
                    "serve".into(),
                    "--config".into(),
                    proc_yaml.to_string_lossy().into_owned(),
                ],
            }],
            notifications: false,
        };
        let layout = Layout::detect(dir.path());
        let got = candidate_storages(&env, &layout, Some(&tray), &[]);
        assert_eq!(got[0], PathBuf::from("/env/storage"));
        if !cfg!(windows) {
            assert_eq!(got[1], home.join("lib"));
        }
        assert!(got.contains(&PathBuf::from("/srv/proc")));
        assert!(got.contains(&paths::server_default_storage(&env)));
        assert!(got.contains(&paths::processor_default_storage(&env)));
        assert!(got.contains(&paths::gui_fallback_storage(&env)));
        let tmp = std::env::temp_dir();
        assert!(
            !got.iter()
                .any(|p| p.starts_with(&tmp) && !p.starts_with(dir.path())),
            "{got:?}"
        );
        let mut dedup = got.clone();
        dedup.dedup();
        assert_eq!(dedup.len(), got.len());
    }

    /// Write a control file as an instance does (0600 on Unix).
    fn write_private(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn reads_control_files_of_live_processes() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let file = dir.path().join(CONTROL_FILE);
        let text = format!(
            r#"{{"role":"processor","pid":{me},"port":4321,"token":"abc","version":"0.7.0","started_at":"x"}}"#
        );
        write_private(&file, &text);
        let got = found(&[dir.path().to_path_buf()]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1.port, 4321);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Planted: readable by others, or a symlink to a good file.
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(found(&[dir.path().to_path_buf()]).is_empty());
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
            let other = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(&file, other.path().join(CONTROL_FILE)).unwrap();
            assert!(found(&[other.path().to_path_buf()]).is_empty());

            // A pid that cannot exist (beyond pid_max) is filtered out.
            write_private(
                &file,
                r#"{"role":"processor","pid":2147483600,"port":1,"token":"t"}"#,
            );
            assert!(found(&[dir.path().to_path_buf()]).is_empty());
        }
    }
}
