//! A server built without a config file never writes one, least of all the user's own.
//!
//! `Services::new(config, None, ..)` is how most tests build a server. Saving such a
//! server's settings (an admin edit does) used to fall back to the default config
//! path, which on a developer's machine is their own `config.yaml`: running the tests
//! replaced it with a test's settings, storage in a deleted temporary folder included.
//!
//! The check runs this test program again with every folder a default path is derived
//! from pointed inside an empty one, saves there, and looks at what was written.

// Test setup and assertions read the disk directly.
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::process::Command;

use bunko_core::Config;
use bunko_server::app::{ServeOptions, Services};

const STAGE: &str = "BUNKO_CONFIG_FILE_STAGE";
const TEST: &str = "a_server_without_a_config_file_saves_nowhere";

fn server(storage: &Path, config_path: Option<PathBuf>) -> Services {
    let mut config = Config::default();
    config.storage.base_path = storage.to_path_buf();
    config.ocr.autobench = false;
    config.catalog.enrich_community = false;
    Services::new(config, config_path, &ServeOptions::default()).expect("services")
}

/// Every file under `dir`.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[tokio::test]
async fn a_server_without_a_config_file_saves_nowhere() {
    if let Ok(storage) = std::env::var(STAGE) {
        // The child: the default config path now lies in the parent's empty folder.
        let services = server(Path::new(&storage), None);
        services
            .core
            .save_config()
            .expect("a save with no file is not an error");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST])
        .env(STAGE, dir.path().join("storage"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("LOCALAPPDATA", home.join("local"))
        .env("APPDATA", home.join("roaming"))
        .status()
        .unwrap();
    assert!(status.success(), "the child: {status}");
    assert_eq!(
        files(&home),
        Vec::<PathBuf>::new(),
        "a server with no config file wrote into the user's folders"
    );
}

#[tokio::test]
async fn a_server_with_a_config_file_saves_there() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let services = server(&dir.path().join("storage"), Some(path.clone()));
    services.core.config.write().server.port = 18_123;
    services.core.save_config().unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("port: 18123"), "{saved}");
}
