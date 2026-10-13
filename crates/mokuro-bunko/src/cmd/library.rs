//! `library status` (0.8 §11): where the library lives and how its store is doing.
//!
//! A server running on this storage is asked through its local control API, the way
//! the tray asks it: `.control.json`'s token as a bearer, over plain http to
//! 127.0.0.1 only (the URL is built from the file's port; the file's own `url` is never
//! followed, so nothing leaves this machine). Its `status.library.storage` is the
//! admin panel's block (health, sync state, caches; never secrets). Without a running
//! server the configuration is read instead, with no network: backend, folder, store
//! id and whether a sign-in is on file. No credential is taken on the command line.

use super::Ctx;
use crate::cfgfile;
use crate::cli::LibraryCmd;
use crate::out::{CmdResult, Fail, runtime};
use serde_json::Value;
use std::time::Duration;

pub fn run(ctx: &Ctx, cmd: LibraryCmd) -> CmdResult {
    match cmd {
        LibraryCmd::Status { json } => status(ctx, json),
    }
}

fn status(ctx: &Ctx, json: bool) -> CmdResult {
    let config = cfgfile::load_effective(&ctx.config_path)?;
    let live = bunko_control::file::live_instance(&config.storage.base_path).and_then(|file| {
        match ask_control(&file) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Could not ask the running server ({e}); showing the configuration");
                None
            }
        }
    });
    let (block, source) = match live {
        Some(v) => (v, "running server"),
        None => (
            bunko_server::storage::describe_offline(&config),
            "configuration",
        ),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&block).map_err(Fail::msg)?
        );
        return Ok(());
    }
    for line in describe(&block, source) {
        println!("{line}");
    }
    Ok(())
}

/// The control API's status URL: loopback http on the file's port, whatever else the
/// file says.
pub fn control_status_url(file: &bunko_control::ControlFile) -> String {
    format!("http://127.0.0.1:{}/control/status", file.port)
}

/// `status.library.storage` of the running instance (`None`: it reports none, e.g. a
/// processor or an older version). Errors never carry the token.
fn ask_control(file: &bunko_control::ControlFile) -> Result<Option<Value>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let url = control_status_url(file);
    let rt = runtime().map_err(|e| e.to_string())?;
    let body: Value = rt.block_on(async {
        let resp = client
            .get(&url)
            .bearer_auth(&file.token)
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {}", status.as_u16()));
        }
        resp.json::<Value>()
            .await
            .map_err(|e| e.without_url().to_string())
    })?;
    Ok(body
        .get("library")
        .and_then(|l| l.get("storage"))
        .filter(|s| s.is_object())
        .cloned())
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut x = n as f64;
    let mut u = 0;
    while x >= 1024.0 && u + 1 < UNITS.len() {
        x /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{x:.1} {}", UNITS[u])
    }
}

/// The lines `library status` prints for a `library.storage` block.
pub fn describe(v: &Value, source: &str) -> Vec<String> {
    let backend = text(&v["backend"]).unwrap_or_else(|| "?".into());
    let mut out = Vec::new();
    if backend == "local" {
        out.push("Library: on this disk".to_string());
        if let Some(folder) = text(&v["folder"]) {
            out.push(format!("Folder:  {folder}"));
        }
        out.push(format!("(from the {source})"));
        return out;
    }
    let mut first = format!(
        "Library: {backend}, folder {}",
        text(&v["folder"]).unwrap_or_default()
    );
    if let Some(p) = text(&v["pairing"]) {
        first.push_str(&format!(" (pairing {p})"));
    }
    out.push(first);
    if let Some(a) = text(&v["account"]) {
        out.push(format!("Account: {a}"));
    }
    match text(&v["store_id"]) {
        Some(id) => {
            let dir = text(&v["store_dir"])
                .map(|d| format!(" in {d}"))
                .unwrap_or_default();
            out.push(format!("Store:   {id}{dir}"));
        }
        None => out.push("Store:   not connected".to_string()),
    }
    if let Some(p) = text(&v["problem"]) {
        out.push(format!("Problem: {p}"));
    }
    if let Some(state) = text(&v["health"]["state"]) {
        let mut h = format!("Health:  {state}");
        if let Some(engine) = text(&v["engine"]) {
            h.push_str(&format!(" (feed: {engine})"));
        }
        if let (Some(done), total) = (v["sync"]["done"].as_u64(), v["sync"]["total"].as_u64()) {
            match total {
                Some(t) => h.push_str(&format!(", {done}/{t} synced")),
                None => h.push_str(&format!(", {done} synced")),
            }
        }
        out.push(h);
    } else if let Some(indexed) = v["indexed"].as_bool() {
        out.push(format!(
            "Index:   {}",
            if indexed {
                "present (a sync ran before)"
            } else {
                "none yet (the first sync runs when the server starts)"
            }
        ));
    }
    let c = &v["cache"];
    if c.is_object() {
        let n = |k: &str| c[k].as_u64().unwrap_or(0);
        out.push(format!(
            "Cache:   {} small files ({}), {} archive directories, {} spooled archives ({})",
            n("blobs"),
            bytes(n("blob_bytes")),
            n("zipdirs"),
            n("spools"),
            bytes(n("spool_bytes"))
        ));
    }
    out.push(format!("(from the {source})"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The control file's own `url` is never followed: the request goes to loopback
    /// http on its port, and the token is not part of the URL.
    #[test]
    fn the_control_api_is_only_asked_on_loopback() {
        let file = bunko_control::ControlFile {
            role: bunko_control::Role::Server,
            pid: 1,
            port: 4321,
            token: "sekrit-token".into(),
            version: "0.8.0".into(),
            started_at: "2026-10-12T00:00:00Z".into(),
            url: "https://evil.example:4321".into(),
            managed: false,
        };
        let url = control_status_url(&file);
        assert_eq!(url, "http://127.0.0.1:4321/control/status");
        assert!(!url.contains("sekrit"));
        assert!(bunko_control::http::is_loopback_host("127.0.0.1:4321"));
        // Nothing listens there: the error names no token.
        let err = ask_control(&bunko_control::ControlFile { port: 1, ..file }).unwrap_err();
        assert!(!err.contains("sekrit"), "{err}");
    }

    /// A store block never carries the credentials it was opened with.
    #[test]
    fn printed_lines_carry_no_secret() {
        let v = json!({"backend": "webdav", "folder": "mokuro-reader", "store_id": "x",
            "account": "me@https://dav.example/", "problem": null});
        let all = describe(&v, "configuration").join("\n");
        assert!(!all.contains("password"));
    }

    #[test]
    fn live_blocks_read_as_lines() {
        let v = json!({
            "backend": "gdrive", "folder": "mokuro-reader", "store_id": "0123456789abcdef",
            "account": "me@example.com", "pairing": "release", "connected": true, "problem": null,
            "health": {"state": "syncing", "done": 10, "total": 0}, "engine": "full-sync",
            "sync": {"done": 10, "total": null},
            "cache": {"blobs": 2, "blob_bytes": 2048, "zipdirs": 1, "blocks": 0, "block_bytes": 0, "spools": 1, "spool_bytes": 5242880},
        });
        let lines = describe(&v, "running server");
        assert_eq!(
            lines,
            [
                "Library: gdrive, folder mokuro-reader (pairing release)",
                "Account: me@example.com",
                "Store:   0123456789abcdef",
                "Health:  syncing (feed: full-sync), 10 synced",
                "Cache:   2 small files (2.0 KiB), 1 archive directories, 1 spooled archives (5.0 MiB)",
                "(from the running server)",
            ]
        );
    }

    #[test]
    fn offline_blocks_say_what_is_missing() {
        let v = json!({
            "backend": "onedrive", "folder": "mokuro-reader", "store_id": null, "account": null,
            "pairing": null, "problem": "not signed in to OneDrive: run `mokuro-bunko library connect onedrive`",
            "store_dir": null, "indexed": false,
        });
        let lines = describe(&v, "configuration");
        assert_eq!(lines[1], "Store:   not connected");
        assert!(lines[2].starts_with("Problem: not signed in to OneDrive"));
        assert!(lines[3].starts_with("Index:   none yet"));
        let local = describe(
            &json!({"backend": "local", "folder": "/data/library"}),
            "configuration",
        );
        assert_eq!(local[0], "Library: on this disk");
    }
}
