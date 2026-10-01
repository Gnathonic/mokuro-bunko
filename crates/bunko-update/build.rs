fn main() {
    // The target triple, so the updater picks the matching release artifact.
    println!(
        "cargo:rustc-env=BUNKO_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
    println!("cargo:rerun-if-env-changed=BUNKO_RELEASE_PUBLIC_KEY");
}
