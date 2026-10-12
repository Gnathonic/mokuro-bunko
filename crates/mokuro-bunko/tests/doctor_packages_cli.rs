//! `doctor`'s compiled-package check: a package counts only with the shared weights its
//! graphs bind and the recognizer's host files, on the CPU as on a GPU. Runs over a fake
//! backend pack (a `rustc`-built library that reports devices
//! and loads nothing; skipped, with a note, when there is no `rustc`).
#![cfg(feature = "ocr")]

mod common;
mod support_ocr;
use common::{Env, stdout};
use support_ocr::{fake_pack, put, put_graphs, put_id};

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
