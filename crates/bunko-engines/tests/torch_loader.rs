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
