//! The admin panel's "Engines and models" (`GET /_admin/api/machine`, `models`) tells
//! the truth about the models folder: each engine's files are the models-v1 files AND
//! the compiled package for the device and precision it runs at (graphs plus shared
//! weights), an engine not in use says what enabling it would download (manifest
//! sizes), and the total is the folder's size with every file counted once. `models
//! list` prints the same plan. Runs `serve` over a fake GPU backend pack (an sm_80
//! card; skipped, with a note, when there is no `rustc`).
#![cfg(all(feature = "ocr", target_os = "linux"))]

mod common;
mod support_ocr;

use bunko_ocr::models::Manifest;
use serde_json::Value;
use std::net::TcpListener;
use std::path::Path;
use std::time::{Duration, Instant};
use support_ocr::{fake_pack, put, put_id, put_unpacked};

struct Killed(std::process::Child);
impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Bytes under `dir`, as `du --apparent-size` counts them.
fn du(dir: &Path) -> u64 {
    let mut total = 0;
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let t = e.file_type().unwrap();
        if t.is_dir() {
            total += du(&e.path());
        } else if t.is_file() {
            total += e.metadata().unwrap().len();
        }
    }
    total
}

/// `models list`'s units (binary, as `du -h`).
fn size(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (bytes as f64, 0);
    while v >= 1024.0 && i + 1 < units.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn engines_and_models_count_the_compiled_packages_and_add_up_to_the_folder() {
    let env = common::Env::new();
    let port = free_port();
    env.write_config(&format!(
        "server:\n  host: 127.0.0.1\n  port: {port}\nupdate:\n  check: false\nocr:\n  local_processing: true\n  autobench: false\n  generations:\n    - {{id: g-1, name: hayai-nova, engine: hayai-nova, primary: true, enabled: true, precision: bf16}}\n"
    ));
    if !fake_pack(&env.storage(), true) {
        return;
    }
    let out = env
        .std_cmd()
        .args([
            "admin",
            "add-user",
            "admin",
            "--role",
            "admin",
            "--password",
            "admin-pass-123",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", common::stderr(&out));

    // hayai-nova bf16 on the sm_80 card, installed: its host files, the package's three
    // graphs (unpacked) and the two shared weights they bind.
    let models = env.storage().join("models");
    let manifest = Manifest::builtin();
    let host = [
        "hayai-nova/pos-table",
        "hayai-nova/token-embeddings",
        "hayai-nova/tokenizer",
    ];
    for id in host {
        put_id(&models, id);
    }
    let pkg = "torch/hayai-nova/bf16/linux-cuda-sm_80";
    let mut graphs = 0;
    for g in ["vision.pt2", "prefill.pt2", "step.pt2"] {
        graphs += put_unpacked(&models, &format!("{pkg}/{g}"), 4096);
    }
    let weights = [
        "torch/hayai-nova/bf16/weights-vision.safetensors",
        "torch/hayai-nova/bf16/weights-decoder.safetensors",
    ];
    for id in weights {
        put_id(&models, id);
    }
    let sz = |id: &str| manifest.get(id).unwrap().size;
    let host_bytes: u64 = host.iter().map(|id| sz(id)).sum();
    let hayai_bytes = host_bytes + graphs + weights.iter().map(|id| sz(id)).sum::<u64>();
    // PP-OCR, paddle-manga's tokenizer (the engine is not in use) and an fp32 weights
    // file no row needs.
    for id in [
        "ppocr-manga/det-v0.2",
        "ppocr-manga/rec-v0.2",
        "ppocr-manga/dict-v6",
    ] {
        put_id(&models, id);
    }
    put_id(&models, "paddle-manga/tokenizer");
    let stale = "torch/hayai-nova/fp32/weights-vision.safetensors";
    put(&models, &manifest.get(stale).unwrap().path, sz(stale));

    let log = env.root().join("serve.log");
    let file = std::fs::File::create(&log).unwrap();
    let child = env
        .std_cmd()
        .arg("serve")
        .env("MOKURO_OCR_AUTO_INSTALL", "false")
        .stdout(file.try_clone().unwrap())
        .stderr(file)
        .spawn()
        .unwrap();
    let _server = Killed(child);
    let show = || std::fs::read_to_string(&log).unwrap_or_default();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let end = Instant::now() + Duration::from_secs(90);
    let token = loop {
        let r = client
            .post(format!("{base}/login/api/token"))
            .basic_auth("admin", Some("admin-pass-123"))
            .send()
            .await;
        if let Ok(r) = r
            && r.status().is_success()
        {
            let v: Value = r.json().await.unwrap();
            break v["token"].as_str().unwrap().to_string();
        }
        assert!(
            Instant::now() < end,
            "the server did not come up:\n{}",
            show()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    let machine: Value = client
        .get(format!("{base}/_admin/api/machine"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let m = &machine["models"];
    let engine = |name: &str| -> &Value {
        m["engines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["engine"] == name)
            .unwrap_or_else(|| panic!("no {name} row: {m}"))
    };
    let ids = |e: &Value| -> Vec<String> {
        e["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["id"].as_str().unwrap().to_string())
            .collect()
    };

    // hayai-nova: the package is part of the row, size and count.
    let hayai = engine("hayai-nova");
    assert_eq!(hayai["used"], true, "{hayai}");
    let hayai_ids = ids(hayai);
    for g in ["vision.pt2", "prefill.pt2", "step.pt2"] {
        assert!(hayai_ids.contains(&format!("{pkg}/{g}")), "{hayai}");
    }
    for w in weights {
        assert!(hayai_ids.contains(&w.to_string()), "{hayai}");
    }
    assert_eq!(hayai["needed"], 8, "{hayai}");
    assert_eq!(hayai["present"], 8, "{hayai}");
    assert_eq!(hayai["disk"], hayai_bytes, "{hayai}");
    assert!(
        hayai_bytes > host_bytes * 2,
        "the packages are the big part"
    );
    assert_eq!(hayai["missing"], 0, "{hayai}");
    let pk = &hayai["packages"][0];
    assert_eq!(
        (
            pk["precision"].as_str(),
            pk["target"].as_str(),
            pk["device"].as_str()
        ),
        (Some("bf16"), Some("linux-cuda-sm_80"), Some("gpu:0")),
        "{hayai}"
    );
    assert!(
        hayai["details"][0]
            .as_str()
            .unwrap()
            .starts_with("bf16 · linux-cuda-sm_80 on gpu:0"),
        "{hayai}"
    );
    assert_eq!(hayai["extra_disk"], sz(stale), "{hayai}");

    // paddle-manga, not in use: what enabling it downloads, from the manifest.
    let paddle = engine("paddle-manga");
    assert_eq!(paddle["used"], false, "{paddle}");
    let paddle_ids = ids(paddle);
    assert!(
        paddle_ids
            .iter()
            .any(|id| id.starts_with("torch/paddle-manga/") && id.ends_with(".safetensors")),
        "the would-be package's weights are counted: {paddle}"
    );
    let would: u64 = paddle_ids
        .iter()
        .filter(|id| *id != "paddle-manga/tokenizer")
        .map(|id| sz(id))
        .sum();
    assert_eq!(paddle["missing"], would, "{paddle}");
    assert!(
        would > 1_000_000_000,
        "paddle's package is gigabytes: {would}"
    );
    assert_eq!(paddle["present"], 1, "{paddle}");
    assert_eq!(paddle["disk"], sz("paddle-manga/tokenizer"), "{paddle}");
    assert!(
        paddle["details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d.as_str().unwrap()
                == format!("enabling it would download about {}", size(would))),
        "{paddle}"
    );

    // The total is the folder, every file counted once.
    let t = &m["total"];
    let folder = du(&models);
    assert_eq!(t["disk"], folder, "{t}");
    let ppocr: u64 = [
        "ppocr-manga/det-v0.2",
        "ppocr-manga/rec-v0.2",
        "ppocr-manga/dict-v6",
    ]
    .iter()
    .map(|id| sz(id))
    .sum();
    assert_eq!(t["in_use"], hayai_bytes + ppocr, "{t}");
    assert_eq!(t["not_in_use"], sz("paddle-manga/tokenizer"), "{t}");
    assert_eq!(t["not_needed"], sz(stale), "{t}");
    assert_eq!(t["other"], 0, "{t}");
    assert_eq!(
        folder,
        hayai_bytes + ppocr + sz("paddle-manga/tokenizer") + sz(stale),
        "the folder is exactly the distinct files"
    );
    assert_eq!(m["shared"], serde_json::json!([]), "{m}");

    // `models list` prints the same plan.
    let out = env.cmd().args(["models", "list"]).output().unwrap();
    let text = common::stdout(&out);
    assert!(out.status.success(), "{text}{}", common::stderr(&out));
    assert!(
        text.contains(&format!("Total: {} in {}", size(folder), models.display())),
        "{text}"
    );
    let row = text
        .lines()
        .find(|l| l.starts_with("hayai-nova "))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(
        row.contains("8 of 8") && row.contains(&size(hayai_bytes)),
        "{row}"
    );
    assert!(
        text.contains(&format!("enabling it would download about {}", size(would))),
        "{text}"
    );
}
