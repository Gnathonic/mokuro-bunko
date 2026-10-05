//! Small files in the instance's storage: written whole (temp file + rename), the
//! control file readable by its owner only.

use std::io::Write;
use std::path::Path;

use crate::types::{CONTROL_FILE, ControlFile};

/// Write `bytes` to `path` through a sibling temp file and a rename, so a reader never
/// sees half a file. `private`: mode 0600 on Unix from the first byte (on Windows the
/// per-user storage directory is already private to its owner).
pub fn write_atomic(path: &Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let tmp = path.with_file_name(format!("{name}.{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let result = (|| {
        let mut f = options.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Write `<storage>/.control.json` (0600).
pub fn write_control_file(storage: &Path, file: &ControlFile) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(file).map_err(std::io::Error::other)?;
    write_atomic(&storage.join(CONTROL_FILE), text.as_bytes(), true)
}

/// Another process's live control listener on this storage: its `.control.json` names
/// a pid that is not ours and its port answers on 127.0.0.1.
pub fn live_instance(storage: &Path) -> Option<ControlFile> {
    let file = crate::types::read_control_file(storage)?;
    if file.pid == std::process::id() || file.port == 0 {
        return None;
    }
    let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, file.port));
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300))
        .ok()
        .map(|_| file)
}

/// Remove `<storage>/.control.json` if it is still ours (a newer instance on the same
/// storage may have replaced it).
pub fn remove_control_file(storage: &Path, token: &str) {
    let path = storage.join(CONTROL_FILE);
    match crate::types::read_control_file(storage) {
        Some(f) if f.token != token => {}
        _ => {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Role;

    #[test]
    fn control_file_is_private_and_only_ours_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = ControlFile {
            role: Role::Processor,
            pid: 1,
            port: 4000,
            token: "t1".into(),
            version: "0.7.0".into(),
            started_at: "2026-10-04T10:00:00Z".into(),
            url: "http://127.0.0.1:4000".into(),
            managed: false,
        };
        write_control_file(dir.path(), &file).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(CONTROL_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(crate::types::read_control_file(dir.path()), Some(file));
        remove_control_file(dir.path(), "someone-else");
        assert!(dir.path().join(CONTROL_FILE).exists());
        remove_control_file(dir.path(), "t1");
        assert!(!dir.path().join(CONTROL_FILE).exists());
    }
}
