// The Mokuro Bunko Android app: the lite server (crates/bunko-android, a Rust cdylib)
// in a foreground service, with its web UI in a WebView. Build with ./build.sh, which
// cross-compiles the native library first. See docs/rust-port/MOBILE.md.
plugins {
    id("com.android.application") version "9.4.1" apply false
}
