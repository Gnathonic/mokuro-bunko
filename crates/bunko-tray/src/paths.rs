//! Where things live on this machine: the default storage and config locations of the
//! library server and the processor (the same rules as `bunko_core::storage` and
//! `bunko_processor::config`, repeated here so the tray does not link the server
//! crates), the Windows portable layout, the tray's own config and log files, and the
//! `mokuro-bunko` executable the tray runs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Environment lookups, injectable for tests.
pub trait Env {
    fn var(&self, name: &str) -> Option<OsString>;

    fn path(&self, name: &str) -> Option<PathBuf> {
        self.var(name).filter(|v| !v.is_empty()).map(PathBuf::from)
    }
}

/// The real process environment.
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn var(&self, name: &str) -> Option<OsString> {
        std::env::var_os(name)
    }
}

pub fn home(env: &dyn Env) -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env.path(var).unwrap_or_else(|| PathBuf::from("."))
}

/// `%LOCALAPPDATA%` on Windows, `$XDG_DATA_HOME` (`~/.local/share`) elsewhere.
pub fn data_base(env: &dyn Env) -> PathBuf {
    if cfg!(windows) {
        env.path("LOCALAPPDATA")
            .unwrap_or_else(|| home(env).join("AppData").join("Local"))
    } else {
        env.path("XDG_DATA_HOME")
            .unwrap_or_else(|| home(env).join(".local").join("share"))
    }
}

/// `%LOCALAPPDATA%` on Windows, `$XDG_CONFIG_HOME` (`~/.config`) elsewhere.
pub fn config_base(env: &dyn Env) -> PathBuf {
    if cfg!(windows) {
        env.path("LOCALAPPDATA")
            .unwrap_or_else(|| home(env).join("AppData").join("Local"))
    } else {
        env.path("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home(env).join(".config"))
    }
}

/// The library server's default storage (`bunko_core::storage::default_storage_path`).
pub fn server_default_storage(env: &dyn Env) -> PathBuf {
    data_base(env).join("mokuro-bunko")
}

/// The processor's default storage (`bunko_processor::config::default_storage_path`).
pub fn processor_default_storage(env: &dyn Env) -> PathBuf {
    data_base(env).join("mokuro-bunko-processor")
}

/// The server's default `config.yaml` (`bunko_core::storage::default_config_path`).
pub fn server_default_config(env: &dyn Env) -> PathBuf {
    config_base(env).join("mokuro-bunko").join("config.yaml")
}

/// The processor config `install.sh --processor` and the wizard write by default.
pub fn processor_default_config(env: &dyn Env) -> PathBuf {
    config_base(env).join("mokuro-bunko").join("processor.yaml")
}

/// How this copy of mokuro-bunko is laid out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The folder holding the tray executable.
    pub exe_dir: PathBuf,
    /// Windows portable zip (`PORTABLE.txt` next to the program): the `data\` folder
    /// that `_env.cmd` points `MOKURO_STORAGE`/`MOKURO_CONFIG` at.
    pub portable_data: Option<PathBuf>,
}

impl Layout {
    pub fn detect(exe_dir: &Path) -> Layout {
        let portable_data = exe_dir
            .join("PORTABLE.txt")
            .is_file()
            .then(|| exe_dir.join("data"));
        Layout {
            exe_dir: exe_dir.to_path_buf(),
            portable_data,
        }
    }

    /// `tray.json`: next to `config.yaml` (portable: in `data\`).
    pub fn tray_config(&self, env: &dyn Env) -> PathBuf {
        match &self.portable_data {
            Some(data) => data.join("tray.json"),
            None => config_base(env).join("mokuro-bunko").join("tray.json"),
        }
    }

    /// The tray's own log files: the server storage's `logs/` (portable: `data\logs`).
    pub fn log_dir(&self, env: &dyn Env) -> PathBuf {
        match &self.portable_data {
            Some(data) => data.join("logs"),
            None => env
                .path("MOKURO_STORAGE")
                .unwrap_or_else(|| server_default_storage(env))
                .join("logs"),
        }
    }

    /// [`Self::log_dir`], created; when that cannot be made (a folder above it is not
    /// writable), `<config>/mokuro-bunko/logs`, then the temp folder: the tray keeps its
    /// lock and logs somewhere rather than not starting.
    pub fn writable_log_dir(&self, env: &dyn Env) -> PathBuf {
        let user = env
            .var(if cfg!(windows) { "USERNAME" } else { "USER" })
            .map(|u| u.to_string_lossy().into_owned())
            .unwrap_or_default();
        let candidates = [
            self.log_dir(env),
            config_base(env).join("mokuro-bunko").join("logs"),
            std::env::temp_dir().join(format!("mokuro-bunko-tray-{user}")),
        ];
        for dir in &candidates {
            if std::fs::create_dir_all(dir).is_ok() {
                return dir.clone();
            }
        }
        candidates[0].clone()
    }

    /// Variables for the `mokuro-bunko` processes the tray starts: `_env.cmd`'s
    /// portable settings, and `MOKURO_LAUNCHER` so an in-place update exits with 75
    /// ("start me again") instead of re-spawning itself out of the tray's sight.
    pub fn child_env(&self, env: &dyn Env) -> Vec<(String, OsString)> {
        let mut vars = vec![("MOKURO_LAUNCHER".to_string(), OsString::from("tray"))];
        if cfg!(windows) && env.var("MOKURO_INSTALL_KIND").is_none() {
            vars.push(("MOKURO_INSTALL_KIND".into(), OsString::from("self")));
        }
        if let Some(data) = &self.portable_data {
            if env.var("MOKURO_CONFIG").is_none() {
                vars.push((
                    "MOKURO_CONFIG".into(),
                    data.join("config.yaml").into_os_string(),
                ));
            }
            if env.var("MOKURO_STORAGE").is_none() {
                vars.push(("MOKURO_STORAGE".into(), data.clone().into_os_string()));
            }
        }
        vars
    }
}

pub fn cli_name() -> &'static str {
    if cfg!(windows) {
        "mokuro-bunko.exe"
    } else {
        "mokuro-bunko"
    }
}

/// The `mokuro-bunko` executable: next to the tray (archives, the Windows folder, the
/// macOS bundle's `Contents/MacOS`), the CLI next to an unpacked `mokuro-bunko.app`,
/// then `PATH`, then `~/.local/bin`.
pub fn cli_exe(exe_dir: &Path, env: &dyn Env) -> Option<PathBuf> {
    let name = cli_name();
    let mut candidates = vec![exe_dir.join(name)];
    // mokuro-bunko.app/Contents/MacOS → the archive folder holding the .app. The CLI there
    // comes first: `mokuro-bunko update` replaces that file, and the copy inside the bundle
    // (a hard link when unpacked) keeps the old version.
    let in_bundle = exe_dir.ends_with("Contents/MacOS")
        && exe_dir
            .ancestors()
            .nth(2)
            .and_then(|a| a.extension())
            .is_some_and(|e| e == "app");
    if let Some(outer) = exe_dir.ancestors().nth(3) {
        if in_bundle {
            candidates.insert(0, outer.join(name));
        } else {
            candidates.push(outer.join(name));
        }
    }
    if let Some(path) = env.var("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|d| d.join(name)));
    }
    candidates.push(home(env).join(".local").join("bin").join(name));
    candidates.into_iter().find(|p| p.is_file())
}

#[cfg(test)]
pub(crate) mod test_env {
    use super::Env;
    use std::collections::HashMap;
    use std::ffi::OsString;

    #[derive(Default)]
    pub struct FakeEnv(pub HashMap<String, OsString>);

    impl FakeEnv {
        pub fn with(mut self, k: &str, v: impl Into<OsString>) -> Self {
            self.0.insert(k.into(), v.into());
            self
        }
    }

    impl Env for FakeEnv {
        fn var(&self, name: &str) -> Option<OsString> {
            self.0.get(name).cloned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_env::FakeEnv;
    use super::*;

    #[test]
    fn defaults_follow_the_server_and_processor_rules() {
        let env = if cfg!(windows) {
            FakeEnv::default().with("LOCALAPPDATA", r"C:\Users\a\AppData\Local")
        } else {
            FakeEnv::default().with("HOME", "/home/a")
        };
        let base = data_base(&env);
        assert_eq!(server_default_storage(&env), base.join("mokuro-bunko"));
        assert_eq!(
            processor_default_storage(&env),
            base.join("mokuro-bunko-processor")
        );
        if !cfg!(windows) {
            assert_eq!(base, PathBuf::from("/home/a/.local/share"));
            assert_eq!(
                server_default_config(&env),
                PathBuf::from("/home/a/.config/mokuro-bunko/config.yaml")
            );
            let env = env.with("XDG_DATA_HOME", "/x");
            assert_eq!(
                server_default_storage(&env),
                PathBuf::from("/x/mokuro-bunko")
            );
        }
    }

    #[test]
    fn portable_layout_keeps_everything_in_data() {
        let dir = tempfile::tempdir().unwrap();
        let env = FakeEnv::default().with("HOME", "/home/a");
        let plain = Layout::detect(dir.path());
        assert_eq!(plain.portable_data, None);
        assert!(
            plain
                .child_env(&env)
                .iter()
                .all(|(k, _)| k != "MOKURO_STORAGE")
        );
        std::fs::write(dir.path().join("PORTABLE.txt"), "").unwrap();
        let portable = Layout::detect(dir.path());
        let data = dir.path().join("data");
        assert_eq!(portable.portable_data.as_deref(), Some(data.as_path()));
        assert_eq!(portable.tray_config(&env), data.join("tray.json"));
        assert_eq!(portable.log_dir(&env), data.join("logs"));
        assert_eq!(portable.writable_log_dir(&env), data.join("logs"));
        assert!(data.join("logs").is_dir());
        let vars = portable.child_env(&env);
        assert!(vars.contains(&("MOKURO_STORAGE".into(), data.clone().into_os_string())));
        assert!(vars.contains(&("MOKURO_LAUNCHER".into(), "tray".into())));
        // A variable the user set wins (as in _env.cmd).
        let env = env.with("MOKURO_STORAGE", "/elsewhere");
        assert!(
            portable
                .child_env(&env)
                .iter()
                .all(|(k, _)| k != "MOKURO_STORAGE")
        );
    }

    #[test]
    fn finds_the_cli_next_to_the_tray_or_outside_the_app_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let env = FakeEnv::default().with("HOME", dir.path().join("nohome"));
        let macos = dir
            .path()
            .join("mokuro-bunko.app")
            .join("Contents")
            .join("MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        assert_eq!(cli_exe(&macos, &env), None);
        std::fs::write(dir.path().join(cli_name()), "").unwrap();
        assert_eq!(cli_exe(&macos, &env), Some(dir.path().join(cli_name())));
        // Inside a bundle the outer CLI wins: `update` replaces it, not the bundle copy.
        std::fs::write(macos.join(cli_name()), "").unwrap();
        assert_eq!(cli_exe(&macos, &env), Some(dir.path().join(cli_name())));
        // Outside a bundle the sibling wins.
        let plain = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join(cli_name()), "").unwrap();
        assert_eq!(cli_exe(&plain, &env), Some(plain.join(cli_name())));
    }
}
