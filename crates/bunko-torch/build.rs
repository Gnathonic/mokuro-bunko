//! With the `libtorch` feature: compile the AOTInductor shim against the libtorch that
//! `torch-sys` found, record that libtorch's version, and give the cdylib a run path to
//! the pack's `lib/` directory. Without it: nothing (the stub needs no libtorch).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(feature = "libtorch")]
    libtorch::build();
}

#[cfg(feature = "libtorch")]
mod libtorch {
    use std::env;
    use std::path::{Path, PathBuf};

    /// The libtorch `lib/` directory: torch-sys exports it on Linux/macOS
    /// (`links = "tch"`); on Windows we read the same variables torch-sys reads.
    fn lib_dir() -> PathBuf {
        if let Some(d) = env::var_os("DEP_TCH_LIBTORCH_LIB") {
            return PathBuf::from(d);
        }
        if let Some(d) = env::var_os("LIBTORCH_LIB") {
            return PathBuf::from(d).join("lib");
        }
        if let Some(d) = env::var_os("LIBTORCH") {
            return PathBuf::from(d).join("lib");
        }
        panic!(
            "bunko-torch: cannot find libtorch (set LIBTORCH, or LIBTORCH_USE_PYTORCH=1 in a torch venv)"
        )
    }

    /// `2.13.0+rocm7.1`: from a libtorch zip's `build-version` or a wheel's `version.py`.
    fn version(root: &Path) -> String {
        if let Ok(v) = std::fs::read_to_string(root.join("build-version")) {
            return v.trim().to_string();
        }
        if let Ok(py) = std::fs::read_to_string(root.join("version.py"))
            && let Some(line) = py.lines().find(|l| l.starts_with("__version__"))
            && let Some(v) = line.split(['\'', '"']).nth(1)
        {
            return v.to_string();
        }
        "unknown".into()
    }

    pub fn build() {
        let lib = lib_dir();
        let root = lib
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| lib.clone());
        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        println!("cargo:rerun-if-changed=shim/aoti_shim.cpp");
        println!("cargo:rerun-if-env-changed=LIBTORCH");
        println!(
            "cargo:rustc-env=BUNKO_TORCH_LIBTORCH_VERSION={}",
            version(&root)
        );
        let mut b = cc::Build::new();
        b.cpp(true)
            .pic(true)
            .warnings(false)
            .include(root.join("include"))
            .include(root.join("include/torch/csrc/api/include"))
            .file("shim/aoti_shim.cpp");
        if target_os == "windows" {
            b.flag("/std:c++17")
                .flag("/EHsc")
                .flag("/DGLOG_USE_GLOG_EXPORT")
                .flag("/DNOMINMAX");
        } else {
            b.flag("-std=c++17")
                .flag_if_supported("-D_GLIBCXX_USE_CXX11_ABI=1")
                .flag("-DGLOG_USE_GLOG_EXPORT");
        }
        b.compile("bunko_aoti_shim");
        println!("cargo:rustc-link-search=native={}", lib.display());
        println!("cargo:rustc-link-lib=torch_cpu");
        println!("cargo:rustc-link-lib=c10");
        match target_os.as_str() {
            // The pack layout: <pack>/libbunko_torch.so next to <pack>/lib/.
            "linux" => {
                // (Unit tests of the rlib run with LD_LIBRARY_PATH=<libtorch>/lib.)
                println!("cargo:rustc-link-arg-cdylib=-Wl,-rpath,$ORIGIN/lib");
            }
            "macos" => {
                println!("cargo:rustc-link-arg-cdylib=-Wl,-rpath,@loader_path/lib");
            }
            // Windows: the loader adds <pack>/lib to the DLL search path.
            _ => {}
        }
    }
}
