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
    assert!(text.contains("ppocr-manga/det-v0.3"), "{text}");
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

/// A nar-small row reads on the CPU without a backend pack: no "compiled packages"
/// failure, only its own files (offline here, so they fail, late and counted).
#[cfg(feature = "nc-models")]
#[test]
fn a_text_box_config_needs_no_backend() {
    let env = Env::new();
    env.write_config(
        "ocr:\n  generations:\n    - {id: g-1, name: nar-small, engine: nar-small, primary: true, enabled: true}\n",
    );
    let out = env
        .cmd()
        .env("MOKURO_MODELS_DOWNLOAD", "0")
        .args(["models", "download"])
        .output()
        .unwrap();
    let text = stdout(&out);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("nar-small/graph-r16"), "{text}");
    assert!(!text.contains("compiled packages"), "{text}");
    assert!(!text.contains("hayai-nova"), "{text}");
    // The row's CPU route is fetched (and fails: offline, nothing here).
    assert!(text.contains("nar-small      FAILED"), "{text}");
}
