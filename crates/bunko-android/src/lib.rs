//! The lite mokuro-bunko server packaged as a shared library for the Android app
//! (`packaging/android`, class `app.mokuro.bunko.BunkoNative`).
//!
//! The app's foreground service calls [`jni_api`]'s `start`/`stop`; the server runs on
//! its own OS thread with its own tokio runtime, exactly as `mokuro-bunko serve` does
//! (`Services::new` → `assemble` → `serve_router`), and stops through the services'
//! cancellation token instead of SIGTERM. There is no ONNX Runtime here: OCR is done by
//! remote processors (`mokuro-bunko processor serve` on another machine).
//!
//! The Rust side is plain functions ([`server`]) so the lifecycle is unit-tested on the
//! host; [`jni_api`] is a thin, panic-safe wrapper.

pub mod config;
pub mod jni_api;
pub mod logs;
pub mod server;

/// The updater flavour this library reports (`InstallKind::Mobile` on Android anyway).
pub const FLAVOR: &str = "lite";
