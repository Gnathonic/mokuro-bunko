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

/// Whether a process with this pid is alive (best effort; true when it cannot tell).
/// On Windows an exited process whose handle is still held can be opened: its exit
/// code decides.
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 only checks for existence and permission.
        unsafe { libc::kill(pid, 0) == 0 }
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: plain Win32 calls on a handle this function owns and closes.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return std::io::Error::last_os_error().raw_os_error()
                    == Some(ERROR_ACCESS_DENIED as i32);
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code);
            CloseHandle(h);
            ok == 0 || code == STILL_ACTIVE as u32
        }
    }
    #[cfg(not(any(unix, windows)))]
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

    /// An exited child reads as dead even while its handle is still held (Windows).
    #[test]
    fn an_exited_child_is_not_alive() {
        assert!(pid_alive(std::process::id()));
        let mut child = if cfg!(windows) {
            Command::new("cmd").args(["/c", "exit 3"]).spawn().unwrap()
        } else {
            Command::new("sh").args(["-c", "exit 3"]).spawn().unwrap()
        };
        let pid = child.id();
        assert_eq!(child.wait().unwrap().code(), Some(3));
        assert!(!pid_alive(pid));
    }

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
