//! A text-box row on the CPU whose block detector was planned for a GPU but could not
//! load there: the ONNX pool is sized from the session's final plan (detect on the CPU),
//! not from the plan with the GPU detect stage the pick promised.
//!
//! Runs over a fake backend pack (a `rustc`-built library that reports a GPU, exports
//! `bt_detect_blocks` and loads nothing) and needs the real nar-small and AnimeText
//! ONNX files, hence `#[ignore]`:
//!
//! ```text
//! MOKURO_MODELS_DIR=<a store holding nar-small and animetext> \
//!     cargo test -p bunko-engines --features gpl-models,nc-models \
//!     --test textbox_pool -- --ignored
//! ```
//!
//! Its own test binary: the libtorch backend is one per process.
#![cfg(all(
    feature = "torch",
    feature = "nc-models",
    feature = "gpl-models",
    target_os = "linux"
))]

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use bunko_engines::plan::{self, Road};
use bunko_engines::torch::abi::{BT_ABI_VERSION, PACK_JSON, library_file_name};
use bunko_engines::{Backend, EngineConfig, EnginePipeline};
use bunko_processor::PagePipeline;
use bunko_proto::{PoolsSpec, RowSpec};

/// A pack in `backends` reporting the CPU and an sm_80 card, exporting
/// `bt_detect_blocks`, whose `bt_load` fails. False when there is no rustc.
fn fake_pack(backends: &Path) -> bool {
    let dir = backends.join("torch-cpu-2.13.0");
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    let report = format!(
        r#"{{"abi":{BT_ABI_VERSION},"torch":"2.13.0","devices":[{{"id":"cpu","kind":"cpu","name":"test CPU","arch":"x86_64","isa":["avx2","fma"],"formats":["fp32"]}},{{"id":"gpu:0","kind":"cuda","name":"Test GPU","arch":"sm_80","vram_mb":40000,"formats":["fp32","fp16","bf16"]}}]}}"#
    );
    let src = format!(
        r##"use std::ffi::{{CString, c_char, c_void}};
#[unsafe(no_mangle)] pub extern "C" fn bt_abi_version() -> u32 {{ {BT_ABI_VERSION} }}
#[unsafe(no_mangle)] pub extern "C" fn bt_init(_c: *const c_char, _e: *mut *mut c_char) -> i32 {{ 0 }}
#[unsafe(no_mangle)] pub unsafe extern "C" fn bt_devices(out: *mut *mut c_char, _e: *mut *mut c_char) -> i32 {{
    unsafe {{ *out = CString::new(r#"{report}"#).unwrap().into_raw() }};
    0
}}
#[unsafe(no_mangle)] pub unsafe extern "C" fn bt_free_str(s: *mut c_char) {{
    if !s.is_null() {{ drop(unsafe {{ CString::from_raw(s) }}) }}
}}
#[unsafe(no_mangle)] pub unsafe extern "C" fn bt_load(
    _engine: *const c_char, _dir: *const c_char, _precision: *const c_char,
    _device: *const c_char, _opts: *const c_char, err: *mut *mut c_char,
) -> *mut c_void {{
    unsafe {{ *err = CString::new("the test pack loads nothing").unwrap().into_raw() }};
    std::ptr::null_mut()
}}
#[unsafe(no_mangle)] pub extern "C" fn bt_info() -> i32 {{ -1 }}
#[unsafe(no_mangle)] pub extern "C" fn bt_read() -> i32 {{ -1 }}
#[unsafe(no_mangle)] pub extern "C" fn bt_free() {{}}
#[unsafe(no_mangle)] pub extern "C" fn bt_detect_blocks() -> i32 {{ -1 }}
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

/// What the session logs, for the pool's "one shared session ... for N worker(s)".
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Write for Log {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The engine stage's width in the plan of nar-small on the CPU beside a detect stage
/// on `detect`, on this host, as `open_session` plans it.
fn engine_width(detect: &str, jobs: usize) -> u32 {
    let host = plan::Host {
        budget: plan::host_worker_budget(plan::logical_cpu_count(), jobs),
        physical_cores: plan::core_count(),
        jobs,
        fast_gpu: false,
    };
    plan::plan(
        "nar-small",
        Road::Reconciled,
        "cpu",
        detect,
        &PoolsSpec::default(),
        &host,
    )
    .iter()
    .find(|s| s.key == plan::STAGE_ENGINE)
    .map(|s| s.workers)
    .expect("an engine stage")
}

/// The threads of the pool's shared session for an engine stage of `workers`, as
/// `textbox_recognizer` sizes it.
fn pool_threads(workers: u32, jobs: usize) -> usize {
    let single = plan::cpu_engine_threads(
        plan::physical_cpu_count(),
        plan::performance_core_count(),
        jobs,
    );
    plan::textbox_session_threads(single, workers)
}

#[test]
#[ignore = "needs the nar-small and AnimeText model files (MOKURO_MODELS_DIR)"]
fn a_detector_that_falls_back_to_the_cpu_sizes_the_text_box_pool_from_the_final_plan() {
    // The job count where the two plans give the pool different threads, else the
    // first where they give it different widths: then the wrong plan shows.
    let widths = |jobs: usize| (engine_width("gpu:0", jobs), engine_width("cpu", jobs));
    let jobs = (1..=plan::core_count().max(1))
        .find(|&j| {
            let (gpu, cpu) = widths(j);
            pool_threads(gpu, j) != pool_threads(cpu, j)
        })
        .or_else(|| (1..=plan::core_count().max(1)).find(|&j| widths(j).0 != widths(j).1));
    let Some(jobs) = jobs else {
        eprintln!("skipping: on this host the pool is as wide beside either detect stage");
        return;
    };
    let (beside_gpu, beside_cpu) = widths(jobs);
    let threads = pool_threads(beside_cpu, jobs);
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: this binary's only test, set before anything reads the environment.
    unsafe {
        std::env::set_var("MOKURO_MODELS_DOWNLOAD", "0");
        std::env::remove_var("MOKURO_TORCH_PACK");
        std::env::remove_var(bunko_engines::BACKENDS_DIR_ENV);
    }
    if !fake_pack(&tmp.path().join("backends")) {
        return;
    }
    // AnimeText's package for the card is here, so the pick puts detect on gpu:0; the
    // pack cannot load it, so the detector reads on the CPU after all.
    let models = tmp.path().join("models");
    std::fs::create_dir_all(models.join(format!(
        "torch/animetext-blocks/fp32/{}-cuda-sm_80/detector.pt2",
        std::env::consts::OS
    )))
    .unwrap();
    let mut config = EngineConfig::new(models, Backend::Auto);
    config.jobs = jobs;
    let p = EnginePipeline::new(config);
    let spec = RowSpec {
        id: "g-1".into(),
        name: "nar-small".into(),
        engine: "nar-small".into(),
        detector: Some("animetext-blocks".into()),
        patch_budget: 512,
        precision: "auto-accuracy".into(),
        pools: PoolsSpec::default(),
        precision_pick: None,
        precision_why: String::new(),
        primary: true,
    };
    let log = Log::default();
    let sink = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || sink.clone())
        .finish();
    let runner = tracing::subscriber::with_default(subscriber, || p.open(&spec))
        .unwrap_or_else(|e| panic!("the session did not open: {e}"));
    let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("animetext-blocks: detecting on the CPU: the test pack loads nothing"),
        "the detector was to try gpu:0 and fall back:\n{text}"
    );
    let ready = runner.ready();
    assert_eq!(ready.stage_device["detect"], "cpu", "{ready:?}");
    assert_eq!(ready.stage_device["engine"], "cpu", "{ready:?}");
    assert_eq!(ready.stage_workers["engine"], beside_cpu, "{ready:?}");
    let pool = text
        .lines()
        .find(|l| l.contains("nar-small: one shared session of"))
        .unwrap_or_else(|| panic!("no pool line:\n{text}"));
    assert!(
        pool.ends_with(&format!(
            "one shared session of {threads} thread(s) for {beside_cpu} worker(s)"
        )),
        "the pool follows the final plan ({beside_cpu} workers, {threads} threads), not the GPU-detect one ({beside_gpu} workers, {} threads; {jobs} jobs): {pool}",
        pool_threads(beside_gpu, jobs)
    );
}
