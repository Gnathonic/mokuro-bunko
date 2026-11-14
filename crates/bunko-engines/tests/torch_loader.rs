//! The backend-pack loader without libtorch: missing packs, bad `pack.json`, a library
//! with the wrong ABI or missing entry points. Fixture libraries are compiled with
//! `rustc` at test time (skipped, with a note, when no `rustc` is on the PATH).
#![cfg(feature = "torch")]

use std::path::{Path, PathBuf};
use std::process::Command;

use bunko_engines::torch::{Pack, PackLoadError, discover};
use bunko_torch::abi::{BT_ABI_VERSION, PACK_JSON, library_file_name};

fn manifest(abi: u32, os: &str) -> serde_json::Value {
    serde_json::json!({
        "format": 1,
        "name": "torch-cpu-2.13.0",
        "variant": "cpu",
        "torch": "2.13.0",
        "abi": abi,
        "os": os,
        "arch": std::env::consts::ARCH,
        "target": "x86_64-unknown-linux-gnu",
        "library": library_file_name(),
        "lib_dir": "lib",
        "files": [],
        "requires": {}
    })
}

fn pack_dir(root: &Path, m: &serde_json::Value) -> PathBuf {
    let d = root.join("torch-cpu-2.13.0");
    std::fs::create_dir_all(d.join("lib")).unwrap();
    std::fs::write(d.join(PACK_JSON), m.to_string()).unwrap();
    d
}

/// Compiles `src` into the pack's library; false when there is no rustc.
fn fixture_library(dir: &Path, src: &str) -> bool {
    let rs = dir.join("fixture.rs");
    std::fs::write(&rs, src).unwrap();
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    match Command::new(rustc)
        .args(["--edition", "2024", "--crate-type", "cdylib", "-o"])
        .arg(dir.join(library_file_name()))
        .arg(&rs)
        .status()
    {
        Ok(s) if s.success() => true,
        Ok(s) => panic!("rustc failed: {s}"),
        Err(e) => {
            eprintln!("skipping: no rustc ({e})");
            false
        }
    }
}

#[test]
fn missing_pack() {
    let tmp = tempfile::tempdir().unwrap();
    let err = Pack::open(&tmp.path().join("nothing"), true).unwrap_err();
    assert!(matches!(err, PackLoadError::NotFound(_)), "{err}");
    assert!(discover(tmp.path()).is_empty());
    // a directory without pack.json is not a pack
    std::fs::create_dir_all(tmp.path().join("torch-cpu-2.13.0/lib")).unwrap();
    assert!(discover(tmp.path()).is_empty());
}

#[test]
fn bad_pack_json() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("p");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join(PACK_JSON), "{ not json").unwrap();
    let err = Pack::open(&d, true).unwrap_err();
    assert!(matches!(err, PackLoadError::Manifest { .. }), "{err}");

    let d = pack_dir(
        tmp.path(),
        &manifest(BT_ABI_VERSION + 1, std::env::consts::OS),
    );
    let err = Pack::open(&d, true).unwrap_err().to_string();
    assert!(err.contains("ABI"), "{err}");

    let d = pack_dir(tmp.path(), &manifest(BT_ABI_VERSION, "plan9"));
    let err = Pack::open(&d, true).unwrap_err().to_string();
    assert!(err.contains("plan9"), "{err}");

    // A pack of a release older than OLDEST_PACK_RELEASE is refused, before its ABI.
    let mut m = manifest(BT_ABI_VERSION + 1, std::env::consts::OS);
    m["bunko_version"] = "0.6.9".into();
    let d = pack_dir(tmp.path(), &m);
    let err = Pack::open(&d, true).unwrap_err().to_string();
    assert!(
        err.contains(&format!(
            "the backend pack is from mokuro-bunko 0.6.9, this is {}",
            bunko_engines::torch::loader::RELEASE
        )),
        "{err}"
    );
    // This release's pack, and one of the oldest release that builds the same pack (or
    // a development pack without one), get past it.
    for v in [
        bunko_engines::torch::loader::RELEASE,
        bunko_engines::torch::abi::OLDEST_PACK_RELEASE,
    ] {
        m["bunko_version"] = v.into();
        let d = pack_dir(tmp.path(), &m);
        let err = Pack::open(&d, true).unwrap_err().to_string();
        assert!(err.contains("ABI"), "{v}: {err}");
    }

    let mut m = manifest(BT_ABI_VERSION, std::env::consts::OS);
    m["library"] = "../../escape.so".into();
    let d = pack_dir(tmp.path(), &m);
    let err = Pack::open(&d, true).unwrap_err().to_string();
    assert!(err.contains("unsafe path"), "{err}");

    // a good manifest whose library is missing, or not a library
    let d = pack_dir(tmp.path(), &manifest(BT_ABI_VERSION, std::env::consts::OS));
    let err = Pack::open(&d, true).unwrap_err();
    assert!(matches!(err, PackLoadError::Library { .. }), "{err}");
    std::fs::write(d.join(library_file_name()), b"not a shared object").unwrap();
    let err = Pack::open(&d, true).unwrap_err();
    assert!(matches!(err, PackLoadError::Library { .. }), "{err}");
}

#[test]
fn abi_mismatch_and_missing_symbols() {
    let tmp = tempfile::tempdir().unwrap();
    let d = pack_dir(tmp.path(), &manifest(BT_ABI_VERSION, std::env::consts::OS));
    let wrong = "#[unsafe(no_mangle)] pub extern \"C\" fn bt_abi_version() -> u32 { 999 }\n";
    if !fixture_library(&d, wrong) {
        return;
    }
    match Pack::open(&d, true) {
        Err(PackLoadError::Abi { found, wanted, .. }) => {
            assert_eq!((found, wanted), (999, BT_ABI_VERSION));
        }
        other => panic!("expected an ABI mismatch, got {other:?}"),
    }

    // The right ABI version but no other entry point. A fresh directory: a library
    // path that was loaded once stays loaded in this process.
    let tmp2 = tempfile::tempdir().unwrap();
    let d = pack_dir(tmp2.path(), &manifest(BT_ABI_VERSION, std::env::consts::OS));
    let partial = format!(
        "#[unsafe(no_mangle)] pub extern \"C\" fn bt_abi_version() -> u32 {{ {BT_ABI_VERSION} }}\n"
    );
    assert!(fixture_library(&d, &partial));
    match Pack::open(&d, true) {
        Err(PackLoadError::Symbol { symbol, .. }) => assert_eq!(symbol, "bt_init"),
        other => panic!("expected a missing symbol, got {other:?}"),
    }
}

/// Every ABI-1 entry point after `bt_abi_version`: `bt_init` succeeds, `bt_devices`
/// reports a CPU, `bt_load` hands out a static handle, `bt_info` describes it, `bt_read`
/// reads nothing, and `bt_free_str` frees what the others handed out.
const ABI1_BODY: &str = r##"
use std::ffi::{c_char, c_void, CString};

static HANDLE: u8 = 0;

fn give(out: *mut *mut c_char, s: &str) {
    unsafe { *out = CString::new(s).unwrap().into_raw() }
}

#[unsafe(no_mangle)] pub extern "C" fn bt_init(_cfg: *const c_char, _e: *mut *mut c_char) -> i32 { 0 }

#[unsafe(no_mangle)] pub extern "C" fn bt_devices(out: *mut *mut c_char, _e: *mut *mut c_char) -> i32 {
    give(out, r#"{"abi":1,"torch":"2.13.0","devices":[{"id":"cpu","kind":"cpu","name":"cpu","arch":"x86_64","formats":["fp32"]}]}"#);
    0
}

#[unsafe(no_mangle)] pub extern "C" fn bt_load(_engine: *const c_char, _dir: *const c_char, _prec: *const c_char, _dev: *const c_char, _opts: *const c_char, _e: *mut *mut c_char) -> *mut c_void {
    &HANDLE as *const u8 as *mut c_void
}

#[unsafe(no_mangle)] pub extern "C" fn bt_info(_h: *mut c_void, out: *mut *mut c_char, _e: *mut *mut c_char) -> i32 {
    give(out, r#"{"engine":"nar-small","precision":"fp32","device":"cpu","batch":64,"load_seconds":0.0,"torch":"2.13.0"}"#);
    0
}

#[unsafe(no_mangle)] pub extern "C" fn bt_read(_h: *mut c_void, _c: *const u8, _n: usize, _caps: *const u32, out: *mut *mut c_char, _e: *mut *mut c_char) -> i32 {
    give(out, "[]");
    0
}

#[unsafe(no_mangle)] pub extern "C" fn bt_free(_h: *mut c_void) {}

#[unsafe(no_mangle)] pub unsafe extern "C" fn bt_free_str(p: *mut c_char) {
    if !p.is_null() {
        drop(unsafe { CString::from_raw(p) });
    }
}
"##;

/// A library source exporting every ABI-1 entry point, `bt_abi_version` first, then
/// `extra`.
fn abi1_fixture(extra: &str) -> String {
    let mut src = format!(
        "#[unsafe(no_mangle)] pub extern \"C\" fn bt_abi_version() -> u32 {{ {BT_ABI_VERSION} }}\n"
    );
    src.push_str(ABI1_BODY);
    src.push_str(extra);
    src.push('\n');
    src
}

/// `bt_read_chars` answering `count` (a Rust expression over `n`, the number of crops)
/// entries of `{"text":"ab","chars":null}`.
fn read_chars_fixture(count: &str) -> String {
    let extra = r##"
#[unsafe(no_mangle)] pub unsafe extern "C" fn bt_read_chars(_h: *mut std::ffi::c_void, _c: *const u8, n: usize, out: *mut *mut std::ffi::c_char, _e: *mut *mut std::ffi::c_char) -> i32 {
    let one = r#"{"text":"ab","chars":null}"#;
    let v = vec![one; COUNT].join(",");
    unsafe { *out = std::ffi::CString::new(format!("[{v}]")).unwrap().into_raw() };
    let _ = n;
    0
}
"##;
    abi1_fixture(&extra.replace("COUNT", count))
}

#[test]
fn a_pack_without_bt_read_chars_reads_no_chars() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = pack_dir(tmp.path(), &manifest(BT_ABI_VERSION, std::env::consts::OS));
    let src = abi1_fixture("");
    assert!(src.contains(&format!(
        "fn bt_abi_version() -> u32 {{ {BT_ABI_VERSION} }}"
    )));
    if !fixture_library(&dir, &src) {
        return;
    }
    let pack = Pack::open(&dir, true).unwrap();
    assert!(!pack.reads_chars());
    let h = pack
        .load("nar-small", tmp.path(), "fp32", "cpu", &Default::default())
        .unwrap();
    // The rest of ABI 1 still works on such a pack.
    assert_eq!(h.info().unwrap().batch, 64);
    let e = h.read_chars(&[(&[0u8; 3], 1, 1)]).unwrap_err();
    assert!(e.contains("predates GPU text-box reading"), "{e}");
}

#[test]
fn bt_read_chars_results_come_back_per_crop() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = pack_dir(tmp.path(), &manifest(BT_ABI_VERSION, std::env::consts::OS));
    if !fixture_library(&dir, &read_chars_fixture("n")) {
        return;
    }
    let pack = Pack::open(&dir, true).unwrap();
    assert!(pack.reads_chars());
    let h = pack
        .load("nar-small", tmp.path(), "fp32", "cpu", &Default::default())
        .unwrap();
    let px = [0u8; 3];
    let r = h.read_chars(&[(&px, 1, 1), (&px, 1, 1)]).unwrap();
    assert_eq!(
        r.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
        ["ab", "ab"]
    );
    assert!(r.iter().all(|c| c.chars.is_none()));
}

#[test]
fn a_wrong_result_count_is_an_error() {
    // The fixture answers one entry whatever n is (Review Focus 3).
    let tmp = tempfile::tempdir().unwrap();
    let dir = pack_dir(tmp.path(), &manifest(BT_ABI_VERSION, std::env::consts::OS));
    if !fixture_library(&dir, &read_chars_fixture("1")) {
        return;
    }
    let pack = Pack::open(&dir, true).unwrap();
    assert!(pack.reads_chars());
    let h = pack
        .load("nar-small", tmp.path(), "fp32", "cpu", &Default::default())
        .unwrap();
    let px = [0u8; 3];
    let e = h.read_chars(&[(&px, 1, 1), (&px, 1, 1)]).unwrap_err();
    assert!(e.contains("1 results for 2 crops"), "{e}");
}

/// A real pack (`MOKURO_TORCH_PACK`) with fake libtorch libraries first on
/// `LD_LIBRARY_PATH`: the loader must bind the pack's own. Run with
/// `MOKURO_TORCH_PACK=<pack> cargo test -p bunko-engines --test torch_loader -- --ignored`.
#[test]
#[ignore = "needs a real backend pack in MOKURO_TORCH_PACK"]
#[cfg(target_os = "linux")]
fn real_pack_ignores_conflicting_libtorch() {
    let Some(pack) = std::env::var_os("MOKURO_TORCH_PACK") else {
        eprintln!("skipping: MOKURO_TORCH_PACK is not set");
        return;
    };
    let pack = PathBuf::from(pack);
    if std::env::var_os("BT_CONFLICT_CHILD").is_some() {
        let p = Pack::open(&pack, true).expect("pack opens");
        let report = p.devices().expect("devices");
        assert_eq!(report.devices[0].id, "cpu");
        assert!(
            p.foreign_libraries().is_empty(),
            "{:?}",
            p.foreign_libraries()
        );
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        let lib = std::fs::canonicalize(pack.join("lib")).unwrap();
        let ours = maps
            .lines()
            .filter(|l| l.contains("/libtorch_cpu.so"))
            .all(|l| l.contains(&*lib.to_string_lossy()));
        assert!(ours, "libtorch_cpu.so not from the pack");
        return;
    }
    // Fake libtorch, libtorch_cpu, libc10 (empty libraries with the real sonames).
    let tmp = tempfile::tempdir().unwrap();
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let src = tmp.path().join("fake.rs");
    std::fs::write(
        &src,
        "#[unsafe(no_mangle)] pub extern \"C\" fn fake_torch() {}\n",
    )
    .unwrap();
    for name in ["libtorch.so", "libtorch_cpu.so", "libc10.so"] {
        let ok = Command::new(&rustc)
            .args(["--edition", "2024", "--crate-type", "cdylib"])
            .arg(format!("-Clink-arg=-Wl,-soname,{name}"))
            .arg("-o")
            .arg(tmp.path().join(name))
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("skipping: cannot build fake libraries");
            return;
        }
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "real_pack_ignores_conflicting_libtorch",
            "--ignored",
            "--nocapture",
        ])
        .env("BT_CONFLICT_CHILD", "1")
        .env("LD_LIBRARY_PATH", tmp.path())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success() && text.contains("1 passed"), "{text}");
}

/// hayai-nova's self-test goldens read through `p` on `device` at patch budget `budget`:
/// (what was read, the goldens' texts). The goldens are libtorch fp32 on the CPU at
/// patch budget 512 (`vulkan_check golden`).
fn read_goldens(
    p: &bunko_engines::EnginePipeline,
    device: &str,
    budget: u32,
) -> Result<(Vec<String>, Vec<String>), String> {
    use base64::Engine as _;
    let golden = p
        .store()
        .ensure_vulkan_golden("hayai-nova")
        .map_err(|e| e.to_string())?;
    let g = bunko_engines::vulkan::Golden::load(&golden)?;
    let crops: Vec<bunko_vlm::CropSet> = g
        .crops
        .iter()
        .map(|c| {
            let b = base64::engine::general_purpose::STANDARD
                .decode(&c.rgb)
                .unwrap();
            bunko_vlm::CropSet::one(
                bunko_vlm::Rgb::from_raw(c.width as usize, c.height as usize, b).unwrap(),
            )
        })
        .collect();
    let rec = p.recognizer_for("hayai-nova", bunko_vlm::Precision::Fp32, device, budget)?;
    let got = rec.read(&crops, None).map_err(|e| e.to_string())?;
    Ok((got, g.crops.iter().map(|c| c.text.clone()).collect()))
}

/// 512 reads the goldens exactly; 384 and 256 (offered in the settings) load into the
/// same 512-patch graphs and read every crop.
fn budgets_read(p: &bunko_engines::EnginePipeline, device: &str) {
    let (got, want) = read_goldens(p, device, 512).unwrap();
    assert_eq!(got, want, "512 reads the goldens");
    eprintln!("512: {}/{} crops read the goldens", want.len(), want.len());
    for budget in [384, 256] {
        let (got, want) = read_goldens(p, device, budget).unwrap();
        assert_eq!(got.len(), want.len());
        let empty = got.iter().filter(|t| t.is_empty()).count();
        let same = got.iter().zip(&want).filter(|(a, b)| a == b).count();
        eprintln!(
            "{budget}: {same}/{} crops as at 512, {empty} empty",
            want.len()
        );
        for (a, b) in got.iter().zip(&want).filter(|(a, b)| a != b) {
            eprintln!("  {budget}: {a:?} (512: {b:?})");
        }
        assert!(
            empty * 10 <= want.len(),
            "{budget}: {empty} crops read nothing"
        );
    }
}

/// hayai-nova on a real libtorch pack at the settings' patch budgets. Run with
/// `MOKURO_TORCH_PACK=<pack> MOKURO_MODELS_ROOT=<models dir> [MOKURO_TEST_DEVICE=gpu:0]
/// cargo test -p bunko-engines --test torch_loader -- --ignored --exact
/// real_pack_reads_hayai_nova_at_every_patch_budget --nocapture` (the models download
/// into the directory when they are not there).
#[test]
#[ignore = "needs a real backend pack in MOKURO_TORCH_PACK and a models directory"]
fn real_pack_reads_hayai_nova_at_every_patch_budget() {
    let (Some(_), Some(models)) = (
        std::env::var_os("MOKURO_TORCH_PACK"),
        std::env::var_os("MOKURO_MODELS_ROOT"),
    ) else {
        eprintln!("skipping: MOKURO_TORCH_PACK or MOKURO_MODELS_ROOT is not set");
        return;
    };
    let device = std::env::var("MOKURO_TEST_DEVICE").unwrap_or_else(|_| "cpu".into());
    let p = bunko_engines::EnginePipeline::new(bunko_engines::EngineConfig::new(
        PathBuf::from(models),
        bunko_engines::Backend::Auto,
    ));
    budgets_read(&p, &device);
}

/// The same on a Vulkan pack's runtime (`MOKURO_TEST_RUNTIME`, default ncnn). Run with
/// `MOKURO_VULKAN_PACK=<pack> MOKURO_MODELS_ROOT=<models dir> MOKURO_BACKENDS_DIR=<scratch>
/// [MOKURO_TEST_DEVICE=gpu:0] cargo test -p bunko-engines --test torch_loader -- --ignored
/// --exact vulkan_pack_reads_hayai_nova_at_every_patch_budget --nocapture`.
#[test]
#[ignore = "needs a real Vulkan pack in MOKURO_VULKAN_PACK and a models directory"]
fn vulkan_pack_reads_hayai_nova_at_every_patch_budget() {
    let (Some(_), Some(models)) = (
        std::env::var_os("MOKURO_VULKAN_PACK"),
        std::env::var_os("MOKURO_MODELS_ROOT"),
    ) else {
        eprintln!("skipping: MOKURO_VULKAN_PACK or MOKURO_MODELS_ROOT is not set");
        return;
    };
    let device = std::env::var("MOKURO_TEST_DEVICE").unwrap_or_else(|_| "gpu:0".into());
    let mut cfg =
        bunko_engines::EngineConfig::new(PathBuf::from(models), bunko_engines::Backend::Vulkan);
    cfg.vulkan_backends =
        vec![std::env::var("MOKURO_TEST_RUNTIME").unwrap_or_else(|_| "ncnn".into())];
    budgets_read(&bunko_engines::EnginePipeline::new(cfg), &device);
}
