//! `doctor`'s compiled-package check: a package counts only with the shared weights its
//! graphs bind and the recognizer's host files, on the CPU as on a GPU. Runs over a fake
//! backend pack (a `rustc`-built library that reports devices
//! and loads nothing; skipped, with a note, when there is no `rustc`).
#![cfg(feature = "ocr")]

mod common;
use common::{Env, stdout};
use std::path::Path;
use std::process::Command;

use bunko_engines::torch::abi::{BT_ABI_VERSION, PACK_JSON, library_file_name};

/// Installs a fake pack in `<storage>/backends` reporting the CPU and, when `gpu`, an
/// sm_80 CUDA card. False when there is no rustc.
fn fake_pack(storage: &Path, gpu: bool) -> bool {
    let dir = storage.join("backends/torch-cpu-2.13.0");
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    let (arch, isa) = match std::env::consts::ARCH {
        "aarch64" => ("aarch64", r#"["neon"]"#),
        _ => ("x86_64", r#"["avx2","fma"]"#),
    };
    let mut devices = format!(
        r#"{{"id":"cpu","kind":"cpu","name":"test CPU","arch":"{arch}","isa":{isa},"formats":["fp32"]}}"#
    );
    if gpu {
        devices.push_str(
            r#",{"id":"gpu:0","kind":"cuda","name":"Test GPU","arch":"sm_80","vram_mb":40000,"formats":["fp32","fp16","bf16"]}"#,
        );
    }
    let report = format!(r#"{{"abi":{BT_ABI_VERSION},"torch":"2.13.0","devices":[{devices}]}}"#);
    let src = format!(
        r##"use std::ffi::{{CString, c_char}};
#[unsafe(no_mangle)] pub extern "C" fn bt_abi_version() -> u32 {{ {BT_ABI_VERSION} }}
#[unsafe(no_mangle)] pub extern "C" fn bt_init(_c: *const c_char, _e: *mut *mut c_char) -> i32 {{ 0 }}
#[unsafe(no_mangle)] pub unsafe extern "C" fn bt_devices(out: *mut *mut c_char, _e: *mut *mut c_char) -> i32 {{
    unsafe {{ *out = CString::new(r#"{report}"#).unwrap().into_raw() }};
    0
}}
#[unsafe(no_mangle)] pub unsafe extern "C" fn bt_free_str(s: *mut c_char) {{
    if !s.is_null() {{ drop(unsafe {{ CString::from_raw(s) }}) }}
}}
#[unsafe(no_mangle)] pub extern "C" fn bt_load() -> usize {{ 0 }}
#[unsafe(no_mangle)] pub extern "C" fn bt_info() -> i32 {{ -1 }}
#[unsafe(no_mangle)] pub extern "C" fn bt_read() -> i32 {{ -1 }}
#[unsafe(no_mangle)] pub extern "C" fn bt_free() {{}}
"##
    );
    let rs = dir.join("fixture.rs");
    std::fs::write(&rs, src).unwrap();
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    match Command::new(rustc)
        .args(["--edition", "2024", "--crate-type", "cdylib", "-o"])
        .arg(dir.join(library_file_name()))
        .arg(&rs)
        .status()
    {
        Ok(s) => assert!(s.success(), "rustc failed: {s}"),
        Err(e) => {
            eprintln!("skipping: no rustc ({e})");
            return false;
        }
    }
    let manifest = serde_json::json!({
        "format": 1, "name": "torch-cpu-2.13.0", "variant": "cpu", "torch": "2.13.0",
        "abi": BT_ABI_VERSION, "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "target": "test", "library": library_file_name(), "lib_dir": "lib",
        "files": [], "requires": {}
    });
    std::fs::write(dir.join(PACK_JSON), manifest.to_string()).unwrap();
    true
}

/// A file of the manifest's size at its store path (sparse: only the size is checked).
fn put(models: &Path, rel: &str, size: u64) {
    let p = models.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::File::create(&p).unwrap().set_len(size).unwrap();
}

fn put_id(models: &Path, id: &str) {
    let m = bunko_ocr::models::Manifest::builtin();
    let f = m
        .get(id)
        .unwrap_or_else(|| panic!("{id} not in the manifest"));
    put(models, &f.path, f.size);
}

/// An unpacked package's three graphs (directories count as graphs).
fn put_graphs(dir: &Path) {
    for g in ["vision.pt2", "prefill.pt2", "step.pt2"] {
        std::fs::create_dir_all(dir.join(g)).unwrap();
    }
}

fn packages_line(env: &Env) -> String {
    let out = env.cmd().arg("doctor").output().unwrap();
    let s = stdout(&out);
    let line = s
        .lines()
        .skip_while(|l| !l.contains("Compiled packages"))
        .take_while(|l| l.contains("Compiled packages") || l.starts_with("        "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!line.is_empty(), "no package check:\n{s}");
    line
}

/// paddle-manga runs on the CPU (as in 0.5.2) from its weightless CPU package: the
/// graphs, the fp32 weights files they bind (the GPU packages' own; the decoder's carries
/// the input embeddings, so no separate table) and the tokenizer.
#[test]
fn paddle_on_the_cpu_runs_its_weightless_package() {
    let env = Env::new();
    env.write_config(
        "server:\n  port: 0\nocr:\n  backend: cpu\n  generations:\n    - {id: g-1, name: paddle-manga, engine: paddle-manga, primary: true, enabled: true, precision: fp32}\n",
    );
    if !fake_pack(&env.storage(), false) {
        return;
    }
    let models = env.storage().join("models");
    let target = match std::env::consts::ARCH {
        "aarch64" => format!("{}-cpu-arm64", std::env::consts::OS),
        _ => format!("{}-cpu-x86_64-v3", std::env::consts::OS),
    };
    put_graphs(&models.join(format!("torch/paddle-manga/fp32/{target}")));
    put_id(&models, "paddle-manga/tokenizer");
    let line = packages_line(&env);
    assert!(
        line.contains(" FAIL  Compiled packages")
            && line.contains("NOT DOWNLOADED: paddle-manga fp32 on cpu"),
        "graphs without their weights are not a package:\n{line}"
    );
    for w in ["weights-vision", "weights-decoder"] {
        put(
            &models,
            &format!("torch/paddle-manga/fp32/{w}.safetensors"),
            1,
        );
    }
    let line = packages_line(&env);
    assert!(
        line.contains(" PASS  Compiled packages") && line.contains("paddle-manga fp32 on cpu"),
        "{line}"
    );
    assert!(!line.contains("embed"), "{line}");
}

/// On a GPU host paddle-manga runs on the card: its package (graphs and the shared
/// weights they bind, the decoder's carrying the input embeddings) and its tokenizer.
#[cfg(target_os = "linux")]
#[test]
fn paddle_on_a_gpu_host_runs_on_the_card() {
    let env = Env::new();
    env.write_config(
        "server:\n  port: 0\nocr:\n  generations:\n    - {id: g-1, name: paddle-manga, engine: paddle-manga, primary: true, enabled: true, precision: fp32}\n",
    );
    if !fake_pack(&env.storage(), true) {
        return;
    }
    let models = env.storage().join("models");
    let line = packages_line(&env);
    assert!(
        line.contains(" FAIL  Compiled packages")
            && line.contains("NOT DOWNLOADED: paddle-manga fp32 on gpu:0"),
        "{line}"
    );
    put_graphs(&models.join("torch/paddle-manga/fp32/linux-cuda-sm_80"));
    put(
        &models,
        "torch/paddle-manga/fp32/weights-vision.safetensors",
        1,
    );
    put(
        &models,
        "torch/paddle-manga/fp32/weights-decoder.safetensors",
        1,
    );
    put_id(&models, "paddle-manga/tokenizer");
    let line = packages_line(&env);
    assert!(
        line.contains(" PASS  Compiled packages") && line.contains("paddle-manga fp32 on gpu:0"),
        "{line}"
    );
}

/// `models download --engine paddle-manga` on a host without a GPU goes for its CPU
/// package and tokenizer (downloads are off here, so they are what fails), not the
/// GPU-only refusal of 2026-10-08. (The release's CPU package entries:
/// bunko-ocr `paddle_manga_cpu_packages_share_the_gpu_weights`.)
#[test]
fn models_download_fetches_paddle_for_the_cpu() {
    let env = Env::new();
    env.write_config(
        "server:\n  port: 0\nocr:\n  generations:\n    - {id: g-1, name: ppocr-manga, engine: ppocr-manga, primary: true, enabled: true}\n    - {id: g-2, name: vl, engine: paddle-manga, enabled: true}\n",
    );
    if !fake_pack(&env.storage(), false) {
        return;
    }
    let out = env
        .cmd()
        .env("MOKURO_MODELS_DOWNLOAD", "0")
        .args(["models", "download", "--engine", "paddle-manga"])
        .output()
        .unwrap();
    let text = format!("{}{}", stdout(&out), common::stderr(&out));
    assert!(!out.status.success(), "{text}");
    let target = match std::env::consts::ARCH {
        "aarch64" => format!("{}-cpu-arm64", std::env::consts::OS),
        _ => format!("{}-cpu-x86_64-v3", std::env::consts::OS),
    };
    assert!(
        text.contains("paddle-manga/tokenizer")
            && text.contains("paddle-manga package for cpu")
            && text.contains(&format!("it needs one of: {target}"))
            && !text.contains("needs a GPU"),
        "{text}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_gpu_package_needs_the_weights_it_binds() {
    let env = Env::new();
    env.write_config(
        "server:\n  port: 0\nocr:\n  generations:\n    - {id: g-1, name: hayai-nova, engine: hayai-nova, primary: true, enabled: true, precision: fp32}\n",
    );
    if !fake_pack(&env.storage(), true) {
        return;
    }
    let models = env.storage().join("models");
    put_graphs(&models.join("torch/hayai-nova/fp32/linux-cuda-sm_80"));
    for id in [
        "hayai-nova/pos-table",
        "hayai-nova/token-embeddings",
        "hayai-nova/tokenizer",
    ] {
        put_id(&models, id);
    }
    let line = packages_line(&env);
    assert!(
        line.contains(" FAIL  Compiled packages")
            && line.contains("NOT DOWNLOADED: hayai-nova fp32 on gpu:0"),
        "graphs without their weights are not a package:\n{line}"
    );
    put(
        &models,
        "torch/hayai-nova/fp32/weights-vision.safetensors",
        1,
    );
    let line = packages_line(&env);
    assert!(
        line.contains(" FAIL  Compiled packages"),
        "one of two weights:\n{line}"
    );
    put(
        &models,
        "torch/hayai-nova/fp32/weights-decoder.safetensors",
        1,
    );
    let line = packages_line(&env);
    assert!(line.contains(" PASS  Compiled packages"), "{line}");
}
