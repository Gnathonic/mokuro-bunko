use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The workspace root (xtask lives in `crates/xtask`).
pub fn workspace_root() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    dir.ancestors().nth(2).unwrap_or(dir).to_path_buf()
}

pub fn cargo() -> Command {
    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

/// The cargo target directory builds land in (honours `CARGO_TARGET_DIR`).
pub fn target_dir(root: &Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(d) => {
            let d = PathBuf::from(d);
            if d.is_absolute() { d } else { root.join(d) }
        }
        None => root.join("target"),
    }
}

pub fn host_triple() -> Result<String> {
    let out = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("-vV")
        .output()
        .context("running rustc -vV")?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(str::to_string)
        .context("rustc -vV printed no host line")
}

/// Whether a binary built for `target` can run on this host (for the `--version` smoke test).
pub fn can_run(host: &str, target: &str) -> bool {
    host == target
        || (host.ends_with("-unknown-linux-gnu")
            && target.ends_with("-unknown-linux-musl")
            && host.split('-').next() == target.split('-').next())
}

pub fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((hex::encode(hasher.finalize()), size))
}

pub fn run(cmd: &mut Command) -> Result<()> {
    eprintln!("+ {cmd:?}");
    let status = cmd.status().with_context(|| format!("starting {cmd:?}"))?;
    if !status.success() {
        bail!("{cmd:?} failed: {status}");
    }
    Ok(())
}

/// `cargo metadata` JSON for the workspace.
pub fn metadata(root: &Path, extra: &[String]) -> Result<serde_json::Value> {
    let mut cmd = cargo();
    cmd.current_dir(root)
        .args(["metadata", "--format-version", "1"])
        .args(extra);
    let out = cmd.output().with_context(|| format!("running {cmd:?}"))?;
    if !out.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// The version of the `mokuro-bunko` package (the workspace version).
pub fn workspace_version(root: &Path) -> Result<String> {
    let meta = metadata(root, &["--no-deps".into()])?;
    meta["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["name"] == crate::names::BIN)
        .and_then(|p| p["version"].as_str())
        .map(str::to_string)
        .context("no mokuro-bunko package in the workspace")
}

/// UTC `YYYY-MM-DDTHH:MM:SSZ` for `secs` since the epoch.
pub fn iso_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `SOURCE_DATE_EPOCH` when set (reproducible builds), else now.
pub fn build_epoch() -> u64 {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(1_790_000_000), "2026-09-21T14:13:20Z");
    }

    #[test]
    fn runnable() {
        assert!(can_run(
            "x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl"
        ));
        assert!(!can_run(
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-musl"
        ));
        assert!(can_run("aarch64-apple-darwin", "aarch64-apple-darwin"));
    }
}
