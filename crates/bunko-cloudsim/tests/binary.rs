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

/// A symlinked folder in `--seed-dir` is followed; a broken link and a link back up
/// the tree are skipped with a warning, never a failed start.
#[cfg(unix)]
#[tokio::test]
async fn seed_dir_follows_symlinks_and_skips_broken_ones() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("Vol 9.cbz"), b"nine").unwrap();
    let seed = tempfile::tempdir().unwrap();
    let lib = seed.path().join("mokuro-reader");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("catalog.json"), b"[]").unwrap();
    std::os::unix::fs::symlink(outside.path(), lib.join("Linked")).unwrap();
    std::os::unix::fs::symlink(seed.path().join("nowhere"), lib.join("broken")).unwrap();
    std::os::unix::fs::symlink(&lib, lib.join("loop")).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_bunko-cloudsim"))
        .args(["--port", "0", "--seed-dir"])
        .arg(seed.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let _kill = Kill(child);
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    let base = line
        .trim()
        .strip_prefix("listening on ")
        .unwrap_or_else(|| panic!("first line: {line:?}"))
        .to_string();
    let files: Value = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{base}/_sim/files?provider=google"))
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
            "mokuro-reader/Linked/Vol 9.cbz",
            "mokuro-reader/catalog.json"
        ]
    );
    let mut warnings = String::new();
    for l in BufReader::new(stderr).lines().take(3) {
        warnings.push_str(&l.unwrap());
        warnings.push('\n');
    }
    assert!(warnings.contains("skipping"), "{warnings}");
    assert!(warnings.contains("broken"), "{warnings}");
    assert!(warnings.contains("loop"), "{warnings}");
}

/// Wherever it listens (here the IPv6 loopback, which has no second loopback name),
/// OneDrive's pre-authenticated URLs are on a second listener, another origin: a
/// client following the download redirect drops its token and the download works.
#[tokio::test]
async fn onedrive_downloads_redirect_to_another_origin_on_any_host() {
    if std::net::TcpListener::bind("[::1]:0").is_err() {
        eprintln!("skipped: no IPv6 loopback here");
        return;
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_bunko-cloudsim"))
        .args(["--port", "0", "--host", "::1"])
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
        .unwrap()
        .to_string();
    assert!(base.starts_with("http://[::1]:"), "{base}");
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let health: Value = http
        .get(format!("{base}/_sim/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let preauth = health["preauth"].as_str().unwrap().to_string();
    assert!(preauth.starts_with("http://[::1]:"), "{preauth}");
    assert_ne!(preauth, base);
    http.post(format!("{base}/_sim/seed"))
        .json(&serde_json::json!({"provider": "microsoft",
            "files": [{"path": "v.cbz", "content_base64": "dm9sdW1l"}]}))
        .send()
        .await
        .unwrap();
    let t: Value = http
        .post(format!("{base}/_sim/token"))
        .json(&serde_json::json!({"provider": "microsoft"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = t["access_token"].as_str().unwrap();
    let item: Value = http
        .get(format!("{base}/graph/v1.0/me/drive/root:/v.cbz"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // reqwest follows the 302 and, the origin changing, drops the token.
    let r = http
        .get(format!(
            "{base}/graph/v1.0/me/drive/items/{}/content",
            item["id"].as_str().unwrap()
        ))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.url().as_str().starts_with(&preauth), "{}", r.url());
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"volume");
    let v: Value = http
        .get(format!("{base}/_sim/violations"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v, serde_json::json!([]));
}

/// The same in-process: the library form has the second listener too.
#[tokio::test]
async fn in_process_downloads_redirect_to_the_second_listener() {
    let sim = bunko_cloudsim::start().await;
    assert_ne!(sim.preauth_base, sim.base);
    let t = sim
        .token(bunko_cloudsim::Provider::Microsoft, "a")
        .access_token;
    let id = sim
        .onedrive("a")
        .put(bunko_cloudsim::onedrive::ROOT, "v.cbz", b"volume");
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!(
            "{}/me/drive/items/{id}/content",
            sim.graph_api_base()
        ))
        .bearer_auth(&t)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.url().as_str().starts_with(&sim.preauth_base));
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"volume");
    sim.assert_clean();
}
