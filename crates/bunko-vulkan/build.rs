fn main() {
    // Dawn (`libwebgpu_dawn`, a NEEDED library of ONNX Runtime's WebGPU build) sits in
    // the pack's lib/: find it there whatever the process's search path.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-rpath,$ORIGIN/lib");
    }
}
