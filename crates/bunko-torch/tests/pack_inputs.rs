//! The sources a backend pack (libtorch or Vulkan) is compiled from, fingerprinted in
//! `tests/pack-inputs.txt`. A build opens the packs of every release from
//! `bunko_torch::abi::OLDEST_PACK_RELEASE` up to its own, which is right only while those
//! releases build the same packs; so any change to these files fails this test until
//! someone decides (docs/rust-port/PACKAGING.md §8):
//!
//! - Packs built before the change still work the same with this build (a comment, a
//!   host-only part, a refactor): record the new fingerprints,
//!   `BUNKO_BLESS=1 cargo test -p bunko-torch --test pack_inputs`.
//! - They do not (a fix inside a cdylib, another libtorch, ncnn or ONNX Runtime): set
//!   `OLDEST_PACK_RELEASE` to the release being made, then record them the same way.
//!
//! The checkout is byte for byte on every OS (`.gitattributes`: `* -text`), so the
//! fingerprints are too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const RECORD: &str = "tests/pack-inputs.txt";

/// Workspace-relative files, and directories taken whole.
const INPUTS: &[&str] = &[
    "crates/bunko-torch/Cargo.toml",
    "crates/bunko-torch/build.rs",
    "crates/bunko-torch/shim",
    "crates/bunko-torch/src",
    "crates/bunko-vlm/Cargo.toml",
    "crates/bunko-vlm/src",
    "crates/bunko-vulkan/Cargo.toml",
    "crates/bunko-vulkan/build.rs",
    "crates/bunko-vulkan/src",
    "crates/bunko-ncnn/Cargo.toml",
    "crates/bunko-ncnn/build.rs",
    "crates/bunko-ncnn/shim",
    "crates/bunko-ncnn/src",
    "crates/bunko-ocr/Cargo.toml",
    "crates/bunko-ocr/src",
    "crates/xtask/src/torch_specs.rs",
    "crates/xtask/src/torch_pack.rs",
    "crates/xtask/src/vulkan_pack.rs",
    "crates/xtask/src/vcredist.rs",
    "packaging/manylinux",
    "packaging/vulkan",
];

/// The workspace manifest's `[profile.*]` tables (optimisation, LTO, the panic strategy
/// the C ABI relies on), under this name. The rest of it changes with every release.
const PROFILES: &str = "Cargo.toml [profile.*]";

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn walk(root: &Path, rel: &str, out: &mut BTreeMap<String, String>) {
    let path = root.join(rel);
    if path.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            // Editor and OS droppings are no part of a pack.
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        for n in names {
            walk(root, &format!("{rel}/{n}"), out);
        }
    } else {
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        out.insert(rel.to_string(), hex(&bytes));
    }
}

fn profiles(manifest: &str) -> String {
    let mut keep = false;
    let mut out = String::new();
    for line in manifest.lines() {
        if line.starts_with('[') {
            keep = line.starts_with("[profile");
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn current() -> BTreeMap<String, String> {
    let root = workspace();
    let mut out = BTreeMap::new();
    for rel in INPUTS {
        assert!(root.join(rel).exists(), "{rel} is gone: update INPUTS");
        walk(&root, rel, &mut out);
    }
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    out.insert(PROFILES.into(), hex(profiles(&manifest).as_bytes()));
    out
}

fn recorded(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let (hash, path) = l.split_once("  ").expect("<sha256>  <path>");
            (path.to_string(), hash.to_string())
        })
        .collect()
}

#[test]
fn pack_inputs() {
    let now = current();
    let record = Path::new(env!("CARGO_MANIFEST_DIR")).join(RECORD);
    if std::env::var("BUNKO_BLESS").is_ok_and(|v| v == "1") {
        let mut text = String::from(
            "# The sources of a backend pack; see tests/pack_inputs.rs before changing.\n\
             # BUNKO_BLESS=1 cargo test -p bunko-torch --test pack_inputs\n",
        );
        for (path, hash) in &now {
            text.push_str(&format!("{hash}  {path}\n"));
        }
        std::fs::write(&record, text).unwrap();
        return;
    }
    let was = recorded(&std::fs::read_to_string(&record).unwrap_or_default());
    if now == was {
        return;
    }
    let mut lines = Vec::new();
    for (path, hash) in &now {
        match was.get(path) {
            None => lines.push(format!("  added    {path}")),
            Some(h) if h != hash => lines.push(format!("  changed  {path}")),
            _ => {}
        }
    }
    for path in was.keys().filter(|p| !now.contains_key(*p)) {
        lines.push(format!("  removed  {path}"));
    }
    panic!(
        "the sources of a backend pack changed:\n{}\n\
         This build opens the packs of mokuro-bunko {} up to its own release. If packs \
         built before this change still work the same with it, record the change: \
         BUNKO_BLESS=1 cargo test -p bunko-torch --test pack_inputs. If they do not, \
         set bunko_torch::abi::OLDEST_PACK_RELEASE to the release being made first. See \
         tests/pack_inputs.rs.",
        lines.join("\n"),
        bunko_torch::abi::OLDEST_PACK_RELEASE
    );
}

#[test]
fn profile_tables_are_cut_out_of_the_manifest() {
    let m = "[workspace]\nmembers = []\n\n[profile.release]\nlto = \"thin\"\n\n[workspace.dependencies]\nx = \"1\"\n[profile.dev]\nopt-level = 1\n";
    assert_eq!(
        profiles(m),
        "[profile.release]\nlto = \"thin\"\n\n[profile.dev]\nopt-level = 1\n"
    );
}
