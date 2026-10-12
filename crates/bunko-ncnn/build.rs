use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=BUNKO_NCNN_DIR");
    println!("cargo:rerun-if-changed=shim/bunko_ncnn.cpp");
    println!("cargo:rerun-if-changed=shim/bunko_ncnn.h");
    println!("cargo::rustc-check-cfg=cfg(bunko_ncnn)");
    let Ok(dir) = std::env::var("BUNKO_NCNN_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    // ncnn is built with OpenMP on Linux (CPU layers only: the GPU path never uses it) and
    // without it for MSVC (cross builds: clang-cl + xwin, `-DNCNN_OPENMP=OFF`).
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|e| e == "msvc");
    let mut b = cc::Build::new();
    b.cpp(true)
        .std("c++17")
        .opt_level(2)
        .include(dir.join("include/ncnn"))
        .file("shim/bunko_ncnn.cpp");
    if msvc {
        b.flag("/EHsc")
            .define("NOMINMAX", None)
            .define("_CRT_SECURE_NO_WARNINGS", None);
    } else {
        b.flag("-fopenmp");
    }
    b.compile("bunko_ncnn_shim");
    println!(
        "cargo:rustc-link-search=native={}",
        dir.join("lib").display()
    );
    for l in [
        "ncnn",
        "glslang",
        "SPIRV",
        "MachineIndependent",
        "GenericCodeGen",
        "OSDependent",
        "glslang-default-resource-limits",
    ] {
        println!("cargo:rustc-link-lib=static={l}");
    }
    if !msvc {
        println!("cargo:rustc-link-lib=dylib=gomp");
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }
    println!("cargo:rustc-cfg=bunko_ncnn");
}
