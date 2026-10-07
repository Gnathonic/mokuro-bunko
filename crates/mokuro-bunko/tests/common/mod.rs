//! Shared harness: every test gets its own HOME / XDG dirs / config path in a temp dir and
//! a cleared environment, so nothing of the developer's machine leaks in or out.

#![allow(dead_code)]

use assert_cmd::Command;
use std::path::{Path, PathBuf};

pub struct Env {
    pub dir: tempfile::TempDir,
}

impl Env {
    pub fn new() -> Env {
        Env {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// The config file every command uses (via `MOKURO_CONFIG`).
    pub fn config_path(&self) -> PathBuf {
        self.root().join("config.yaml")
    }

    /// The storage dir the tests point `storage.base_path` at.
    pub fn storage(&self) -> PathBuf {
        self.root().join("storage")
    }

    /// `XDG_DATA_HOME/mokuro-bunko`, on Windows `%LOCALAPPDATA%\mokuro-bunko` (default
    /// storage and auto-cert dir).
    pub fn data_dir(&self) -> PathBuf {
        self.root().join("data").join("mokuro-bunko")
    }

    pub fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("mokuro-bunko").expect("binary");
        c.env_clear()
            .env("HOME", self.root())
            .env("XDG_CONFIG_HOME", self.root().join("xdg-config"))
            .env("XDG_DATA_HOME", self.root().join("data"))
            .env("MOKURO_CONFIG", self.config_path())
            .env("NO_COLOR", "1")
            .current_dir(self.root());
        if cfg!(windows) {
            // Windows' counterparts of HOME and XDG_DATA_HOME, and SYSTEMROOT, without
            // which Winsock and other system DLLs fail to initialise.
            c.env("USERPROFILE", self.root())
                .env("LOCALAPPDATA", self.root().join("data"));
            if let Some(root) = std::env::var_os("SYSTEMROOT") {
                c.env("SYSTEMROOT", root);
            }
        }
        c
    }

    /// Write a config file with storage under the temp dir plus `extra` YAML.
    pub fn write_config(&self, extra: &str) {
        let text = format!(
            "storage:\n  base_path: {}\n{extra}",
            self.storage().display()
        );
        std::fs::write(self.config_path(), text).expect("write config");
    }

    pub fn config_yaml(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.config_path()).expect("read config");
        serde_yaml_ng::from_str(&text).expect("yaml")
    }
}

pub fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}
