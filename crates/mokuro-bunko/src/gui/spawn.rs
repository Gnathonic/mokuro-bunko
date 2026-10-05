//! Starting the library server or the processor from the pages: this executable,
//! detached (its own process group / no console), so it keeps running when the `gui`
//! command exits. Its stdout and stderr go to a file in its logs directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Start `exe args...` in the background; returns its pid.
pub fn spawn_detached(exe: &Path, args: &[String], log: &Path) -> std::io::Result<u32> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    let err = out.try_clone()?;
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn()?;
    let pid = child.id();
    // Reap it when it exits while we are still running (no zombie).
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

/// The URL that reaches the library server on this machine.
pub fn local_server_url(config: &bunko_core::Config) -> String {
    let scheme = if config.ssl.enabled { "https" } else { "http" };
    let host = match config.server.host.as_str() {
        "0.0.0.0" | "" | "::" => "127.0.0.1".to_string(),
        h if h.contains(':') => format!("[{h}]"),
        h => h.to_string(),
    };
    format!("{scheme}://{host}:{}", config.server.port)
}

/// `GET <url>/api/health` answers 2xx (our own certificate is not checked).
pub async fn healthy(url: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .danger_accept_invalid_certs(true)
        .no_proxy()
        .build()
    else {
        return false;
    };
    client
        .get(format!("{url}/api/health"))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

/// Wait up to `timeout` for the server to answer.
pub async fn wait_healthy(url: &str, timeout: Duration) -> bool {
    let end = tokio::time::Instant::now() + timeout;
    loop {
        if healthy(url).await {
            return true;
        }
        if tokio::time::Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

/// Whether a process with this pid is alive (best effort).
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        true
    }
}

/// The last `n` lines of a text file (bounded read from the end).
pub fn tail_file(path: &Path, n: usize) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    const MAX: u64 = 512 * 1024;
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(MAX);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // a partial first line
    }
    let from = lines.len().saturating_sub(n);
    Ok(lines[from..].join("\n"))
}

/// `<logs>/<name>` for a started instance's own output.
pub fn stdout_log(storage: &Path, name: &str) -> PathBuf {
    storage.join("logs").join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tails() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.log");
        std::fs::write(&p, "1\n2\n3\n4\n").unwrap();
        assert_eq!(tail_file(&p, 2).unwrap(), "3\n4");
        assert_eq!(tail_file(&p, 10).unwrap(), "1\n2\n3\n4");
    }

    #[test]
    fn local_urls() {
        let mut c = bunko_core::Config::default();
        c.server.port = 9000;
        assert_eq!(local_server_url(&c), "http://127.0.0.1:9000");
        c.ssl.enabled = true;
        c.server.host = "::1".into();
        assert_eq!(local_server_url(&c), "https://[::1]:9000");
    }
}
