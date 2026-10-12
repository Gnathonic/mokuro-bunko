//! A fake libtorch backend pack and model-store fixtures for the OCR tests (`doctor`'s
//! package check, the admin panel's "Engines and models").
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

use bunko_engines::torch::abi::{BT_ABI_VERSION, PACK_JSON, library_file_name};

/// Installs a fake pack in `<storage>/backends` reporting the CPU and, when `gpu`, an
/// sm_80 CUDA card. False when there is no rustc.
pub fn fake_pack(storage: &Path, gpu: bool) -> bool {
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
pub fn put(models: &Path, rel: &str, size: u64) {
    let p = models.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::File::create(&p).unwrap().set_len(size).unwrap();
}

pub fn put_id(models: &Path, id: &str) {
    let m = bunko_ocr::models::Manifest::builtin();
    let f = m
        .get(id)
        .unwrap_or_else(|| panic!("{id} not in the manifest"));
    put(models, &f.path, f.size);
}

/// An unpacked package's three graphs (directories count as graphs).
pub fn put_graphs(dir: &Path) {
    for g in ["vision.pt2", "prefill.pt2", "step.pt2"] {
        std::fs::create_dir_all(dir.join(g)).unwrap();
    }
}

/// A package graph as the installer leaves it: the zip unpacked into its `unpack_to`
/// directory with the `.unpacked` stamp naming the manifest's sha256, and `bytes` of
/// content. Returns the bytes the directory holds (content and stamp).
pub fn put_unpacked(models: &Path, id: &str, bytes: u64) -> u64 {
    let m = bunko_ocr::models::Manifest::builtin();
    let f = m
        .get(id)
        .unwrap_or_else(|| panic!("{id} not in the manifest"));
    let dir = models.join(f.unpack_to.as_deref().expect("a package graph"));
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::File::create(dir.join("data/model.so"))
        .unwrap()
        .set_len(bytes)
        .unwrap();
    let stamp = format!("{}\nlayout 2\n", f.sha256);
    std::fs::write(dir.join(bunko_ocr::models::UNPACKED_STAMP), &stamp).unwrap();
    bytes + stamp.len() as u64
}
