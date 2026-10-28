//! The standalone binary: the first stdout line names the URL, `--seed-dir` fills every
//! provider, and it serves the control API.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use serde_json::Value;

struct Kill(std::process::Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn serves_a_seeded_tree_on_every_provider() {
    let seed = tempfile::tempdir().unwrap();
    let series = seed.path().join("mokuro-reader").join("Series A");
    std::fs::create_dir_all(&series).unwrap();
    std::fs::write(series.join("Vol 1.cbz"), b"volume one").unwrap();
    std::fs::write(series.join("Vol 1.mokuro"), b"{}").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_bunko-cloudsim"))
        .args(["--port", "0", "--seed-dir"])
        .arg(seed.path())
        .args(["--seed-account", "alice"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let _kill = Kill(child);
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    let base = line
        .trim()
        .strip_prefix("listening on ")
        .unwrap_or_else(|| panic!("first line: {line:?}"))
        .to_string();
    assert!(base.starts_with("http://127.0.0.1:"), "{base}");

    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let health: Value = http
        .get(format!("{base}/_sim/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
    for p in ["google", "microsoft", "mega"] {
        let files: Value = http
            .get(format!("{base}/_sim/files?provider={p}&account=alice"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let paths: Vec<&str> = files
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                "mokuro-reader/Series A/Vol 1.cbz",
                "mokuro-reader/Series A/Vol 1.mokuro"
            ],
            "{p}"
        );
    }
    // The MEGA account exists for password login.
    let v: Value = http
        .post(format!("{base}/mega/cs?id=1"))
        .body(r#"[{"a":"us0","user":"alice@cloudsim.test"}]"#)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v[0]["v"], 2, "{v}");
    // A token for the seeded account reads the seeded file.
    let t: Value = http
        .post(format!("{base}/_sim/token"))
        .json(&serde_json::json!({"provider": "microsoft", "account": "alice"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let r = http
        .get(format!("{base}/graph/v1.0/me/drive/root:/mokuro-reader"))
        .bearer_auth(t["access_token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[test]
fn a_bad_argument_is_refused_with_usage() {
    let out = Command::new(env!("CARGO_BIN_EXE_bunko-cloudsim"))
        .arg("--bogus")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown argument \"--bogus\""), "{err}");
    assert!(err.contains("usage: bunko-cloudsim"), "{err}");
    assert!(out.stdout.is_empty());
}
