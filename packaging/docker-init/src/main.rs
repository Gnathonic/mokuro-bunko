//! `bunko-init`: the entrypoint of the mokuro-bunko container images.
//!
//! It keeps the environment contract of the 0.5 images (docker-entrypoint.unraid.sh):
//!
//! * `PUID` / `PGID`: when started as root, the server runs as this uid/gid
//!   (image default 1000:1000; the Unraid template passes 99:100).
//! * `UMASK` (default `002`): file creation mask for the server.
//! * `TAKE_OWNERSHIP=true`: recursively chown the storage and config directories
//!   to PUID:PGID before starting. Without it only the two directories themselves
//!   are chowned (best effort), as before.
//! * `MOKURO_NGINX_ACCEL=1|true` is honoured only when the image's nginx is running
//!   (`BUNKO_NGINX_RUNNING=1`, set by the nginx entrypoint). Otherwise it is removed
//!   with a warning: without nginx the X-Accel-Redirect responses would be empty.
//! * `MOKURO_BUNKO_OCR_ENV`, `MOKURO_BUNKO_OCR_ENGINES_ENV`, `MOKURO_BUNKO_MOKURO_SPEC`
//!   (0.5 Python OCR environments) are accepted and ignored. The OCR backend download
//!   (`install-ocr --if-needed`, `MOKURO_OCR_AUTO_INSTALL`) is run through this program
//!   by the full image's entrypoint.sh before the server, so it runs as PUID:PGID too.
//!
//! * GPU device nodes passed in (`--device /dev/kfd --device /dev/dri`, or the NVIDIA
//!   toolkit's `/dev/nvidia*`) stay usable after the switch to PUID:PGID: the groups
//!   that own them (`render`, `video` on the host) and the groups given with
//!   `--group-add` are kept as supplementary groups (never group 0).
//!
//! Then it `exec`s `mokuro-bunko` (`BUNKO_EXEC`) with the container's arguments
//! (default: `serve`). Started as a non-root user (`docker run --user`), it only sets
//! the umask and execs.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_EXEC: &str = "/opt/mokuro-bunko/mokuro-bunko";
const RETIRED: &[&str] = &[
    "MOKURO_BUNKO_OCR_ENV",
    "MOKURO_BUNKO_OCR_ENGINES_ENV",
    "MOKURO_BUNKO_MOKURO_SPEC",
];

fn log(msg: &str) {
    eprintln!("[bunko-init] {msg}");
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn id_var(name: &str, default: u32) -> u32 {
    match env(name) {
        None => default,
        Some(v) => v.trim().parse().unwrap_or_else(|_| {
            log(&format!("{name}={v:?} is not a number; using {default}"));
            default
        }),
    }
}

fn truthy(v: Option<String>) -> bool {
    v.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

fn chown(path: &Path, uid: u32, gid: u32, follow: bool) -> std::io::Result<()> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
    let rc = unsafe {
        if follow {
            libc::chown(c.as_ptr(), uid, gid)
        } else {
            libc::lchown(c.as_ptr(), uid, gid)
        }
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// chown -R without following symlinks; returns (changed, failed).
fn chown_tree(root: &Path, uid: u32, gid: u32) -> (u64, u64) {
    let (mut ok, mut failed) = (0, 0);
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        match chown(&p, uid, gid, false) {
            Ok(()) => ok += 1,
            Err(_) => failed += 1,
        }
        if let Ok(meta) = std::fs::symlink_metadata(&p)
            && meta.is_dir()
            && let Ok(rd) = std::fs::read_dir(&p)
        {
            stack.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()));
        }
    }
    (ok, failed)
}

/// The supplementary groups the server keeps: `gid`, then the non-root groups this
/// process already has (`docker run --group-add`), then the non-root groups owning the
/// GPU device nodes in `devices` (ROCm needs `/dev/kfd` and `/dev/dri/renderD*`
/// read-write; on most hosts they belong to `render`/`video`, 0660).
fn supplementary_groups(gid: u32, current: &[u32], devices: &[(PathBuf, u32)]) -> Vec<u32> {
    let mut out = vec![gid];
    for g in current
        .iter()
        .copied()
        .chain(devices.iter().map(|(_, g)| *g))
    {
        if g != 0 && !out.contains(&g) {
            out.push(g);
        }
    }
    out
}

/// GPU device nodes in this container and their owning group.
fn gpu_devices() -> Vec<(PathBuf, u32)> {
    use std::os::unix::fs::MetadataExt;
    let mut paths = vec![PathBuf::from("/dev/kfd")];
    for dir in ["/dev/dri", "/dev"] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if (dir == "/dev/dri" && (n.starts_with("renderD") || n.starts_with("card")))
                    || (dir == "/dev" && n.starts_with("nvidia"))
                {
                    paths.push(e.path());
                }
            }
        }
    }
    paths
        .into_iter()
        .filter_map(|p| std::fs::metadata(&p).ok().map(|m| (p, m.gid())))
        .collect()
}

fn current_groups() -> Vec<u32> {
    // SAFETY: a size query, then a call with a buffer of that size.
    unsafe {
        let n = libc::getgroups(0, std::ptr::null_mut());
        if n <= 0 {
            return Vec::new();
        }
        // gid_t is u32 on Linux.
        let mut buf = vec![0u32; n as usize];
        let n = libc::getgroups(n, buf.as_mut_ptr().cast());
        buf.truncate(n.max(0) as usize);
        buf
    }
}

fn drop_privileges(uid: u32, gid: u32, extra: &[u32]) -> Result<(), String> {
    let groups = supplementary_groups(gid, extra, &[]);
    // SAFETY: plain syscalls with valid arguments; order matters (groups and gid
    // must be changed while we still have the privilege to).
    unsafe {
        if libc::setgroups(groups.len() as _, groups.as_ptr().cast()) != 0 {
            return Err(format!(
                "setgroups({gid}): {}",
                std::io::Error::last_os_error()
            ));
        }
        if libc::setgid(gid) != 0 {
            return Err(format!(
                "setgid({gid}): {}",
                std::io::Error::last_os_error()
            ));
        }
        if libc::setuid(uid) != 0 {
            return Err(format!(
                "setuid({uid}): {}",
                std::io::Error::last_os_error()
            ));
        }
        if uid != 0 && libc::setuid(0) == 0 {
            return Err("could regain root after dropping privileges".into());
        }
    }
    Ok(())
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        args.push("serve".into());
    }
    let exec = env("BUNKO_EXEC").unwrap_or_else(|| DEFAULT_EXEC.into());
    let mut cmd = Command::new(&exec);
    cmd.args(&args);

    for name in RETIRED {
        if std::env::var_os(name).is_some() {
            log(&format!(
                "{name} is ignored: 0.7 has no Python OCR environments"
            ));
            cmd.env_remove(name);
        }
    }
    if truthy(env("MOKURO_NGINX_ACCEL")) {
        if env("BUNKO_NGINX_RUNNING").as_deref() == Some("1") {
            cmd.env("MOKURO_NGINX_ACCEL", "1");
        } else {
            log(
                "MOKURO_NGINX_ACCEL is set but this image runs no nginx; the server serves downloads itself",
            );
            cmd.env_remove("MOKURO_NGINX_ACCEL");
        }
    }

    let umask = env("UMASK").unwrap_or_else(|| "002".into());
    match u32::from_str_radix(umask.trim(), 8) {
        // SAFETY: umask cannot fail.
        Ok(m) if m <= 0o777 => unsafe {
            libc::umask(m as libc::mode_t);
        },
        _ => log(&format!(
            "UMASK={umask:?} is not an octal mask; keeping the default"
        )),
    }

    // SAFETY: geteuid cannot fail.
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        let puid = id_var("PUID", 1000);
        let pgid = id_var("PGID", 1000);
        let storage = PathBuf::from(env("MOKURO_STORAGE").unwrap_or_else(|| "/data".into()));
        let config_dir = env("MOKURO_CONFIG")
            .map(PathBuf::from)
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let mut dirs = vec![storage];
        dirs.extend(config_dir.filter(|d| !d.as_os_str().is_empty()));
        dirs.dedup();
        for dir in &dirs {
            if let Err(e) = std::fs::create_dir_all(dir) {
                log(&format!("cannot create {}: {e}", dir.display()));
            }
        }
        if truthy(env("TAKE_OWNERSHIP")) {
            for dir in &dirs {
                log(&format!(
                    "fixing ownership of {} to {puid}:{pgid} ...",
                    dir.display()
                ));
                let (ok, failed) = chown_tree(dir, puid, pgid);
                log(&format!(
                    "ownership fix complete: {ok} entries{}",
                    if failed > 0 {
                        format!(", {failed} failed")
                    } else {
                        String::new()
                    }
                ));
            }
        } else {
            for dir in &dirs {
                let _ = chown(dir, puid, pgid, true); // best effort, as in 0.5
            }
        }
        let devices = gpu_devices();
        let groups = supplementary_groups(pgid, &current_groups(), &devices);
        if groups.len() > 1 && puid != 0 {
            log(&format!(
                "keeping supplementary groups {:?} (--group-add, GPU devices {})",
                &groups[1..],
                devices
                    .iter()
                    .filter(|(_, g)| *g != 0)
                    .map(|(p, _)| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if puid == 0 {
            log("PUID=0: running the server as root");
        } else if let Err(e) = drop_privileges(puid, pgid, &groups[1..]) {
            log(&format!("cannot switch to {puid}:{pgid}: {e}"));
            std::process::exit(126);
        }
        // root's HOME is not writable for PUID; 0.5's app user had /tmp.
        let home_ok = matches!(std::env::var("HOME").as_deref(), Ok(h) if h != "/root" && h != "/");
        if puid != 0 && !home_ok {
            cmd.env("HOME", "/tmp");
        }
    } else if env("PUID").is_some() || env("PGID").is_some() {
        log(&format!(
            "started as uid {euid} (not root): PUID/PGID are ignored"
        ));
    }

    let err = cmd.exec();
    log(&format!("cannot run {exec}: {err}"));
    std::process::exit(127);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_gpu_and_added_groups_never_root() {
        let devices = vec![
            (PathBuf::from("/dev/kfd"), 993),
            (PathBuf::from("/dev/dri/renderD128"), 993),
            (PathBuf::from("/dev/dri/card0"), 44),
            (PathBuf::from("/dev/nvidiactl"), 0),
        ];
        // root's own group 0 never survives; --group-add 44 and the device groups do.
        assert_eq!(
            supplementary_groups(100, &[0, 44], &devices),
            vec![100, 44, 993]
        );
        assert_eq!(supplementary_groups(1000, &[], &[]), vec![1000]);
        assert_eq!(supplementary_groups(993, &[993], &devices), vec![993, 44]);
    }
}
