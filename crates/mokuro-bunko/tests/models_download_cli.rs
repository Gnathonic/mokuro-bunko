//! `models download` fetches what the enabled generations need, keeps going past a
//! failed file, and reports every failure at the end with a non-zero exit.
#![cfg(feature = "ocr")]

mod common;
use common::{Env, stdout};

#[test]
fn hayai_only_config_fetches_no_paddle_files_and_fails_late() {
    let env = Env::new();
    env.write_config(
        "ocr:\n  generations:\n    - {id: g-1, name: hayai-nova, engine: hayai-nova, primary: true, enabled: true}\n",
    );
    // Offline and empty: every file fails, nothing is downloaded.
    let out = env
        .cmd()
        .env("MOKURO_MODELS_DOWNLOAD", "0")
        .args(["models", "download"])
        .output()
        .unwrap();
    let text = stdout(&out);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("hayai-nova/tokenizer"), "{text}");
    assert!(text.contains("ppocr-manga/det-v0.2"), "{text}");
    assert!(
        !text.contains("paddle-manga"),
        "a hayai-only config fetches no paddle file:\n{text}"
    );
    // Past the failed files, the compiled packages are still attempted (and fail: no
    // backend pack here), and every failure is counted at the end.
    assert!(text.contains("compiled packages: FAILED"), "{text}");
    let err = common::stderr(&out);
    assert!(err.contains("item(s) could not be fetched"), "{err}");
}

#[test]
fn ppocr_only_config_needs_no_backend() {
    let env = Env::new();
    env.write_config(
        "ocr:\n  generations:\n    - {id: g-1, name: ppocr-manga, engine: ppocr-manga, primary: true, enabled: true}\n",
    );
    let out = env
        .cmd()
        .env("MOKURO_MODELS_DOWNLOAD", "0")
        .args(["models", "download"])
        .output()
        .unwrap();
    let text = stdout(&out);
    assert!(
        !text.contains("hayai-nova") && !text.contains("compiled packages"),
        "{text}"
    );
}
