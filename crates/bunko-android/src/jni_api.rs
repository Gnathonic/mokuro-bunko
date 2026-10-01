//! JNI entry points for `app.mokuro.bunko.BunkoNative` (Kotlin `object` with
//! `@JvmStatic external fun`s, so each receives the class, not an instance).
//!
//! ```kotlin
//! external fun start(storageDir: String, configPath: String, port: Int, lan: Boolean): String
//! external fun stop(): Boolean
//! external fun isRunning(): Boolean
//! external fun logTail(): String
//! ```
//!
//! `start` answers `"ok:<local url>"` or `"error:<message>"`. It blocks until the server
//! listens (or fails), so the app calls it off the main thread; `stop` blocks for the
//! graceful shutdown (≤ ~5 s). A panic never crosses into the JVM: it becomes an error.

use crate::config::StartOptions;
use crate::{logs, server};
use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean, jint, jstring};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;

fn guarded<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fallback)
}

fn to_jstring(env: &mut JNIEnv, s: &str) -> jstring {
    match env.new_string(s) {
        Ok(j) => j.into_raw(),
        // An exception (OOM) is pending in the JVM; it is thrown when we return.
        Err(_) => std::ptr::null_mut(),
    }
}

fn read_string(env: &mut JNIEnv, s: &JString) -> Result<String, String> {
    env.get_string(s).map(Into::into).map_err(|e| format!("bad string argument: {e}"))
}

/// The `start` body, shared with the host tests.
pub fn start_status(storage_dir: String, config_path: String, port: i32, lan: bool) -> String {
    let port = match u16::try_from(port) {
        Ok(p) if p > 0 => p,
        _ => return format!("error:invalid port {port}"),
    };
    let opts = StartOptions { storage_dir: PathBuf::from(storage_dir), config_path: PathBuf::from(config_path), port, lan };
    match catch_unwind(|| server::start(opts)) {
        Ok(Ok(url)) => format!("ok:{url}"),
        Ok(Err(e)) => format!("error:{e}"),
        Err(_) => "error:the server panicked while starting (see the log)".into(),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_mokuro_bunko_BunkoNative_start<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    storage_dir: JString<'l>,
    config_path: JString<'l>,
    port: jint,
    lan: jboolean,
) -> jstring {
    let status = match (read_string(&mut env, &storage_dir), read_string(&mut env, &config_path)) {
        (Ok(s), Ok(c)) => start_status(s, c, port, lan != JNI_FALSE),
        (Err(e), _) | (_, Err(e)) => format!("error:{e}"),
    };
    to_jstring(&mut env, &status)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_mokuro_bunko_BunkoNative_stop<'l>(_env: JNIEnv<'l>, _class: JClass<'l>) -> jboolean {
    if guarded(false, server::stop) { JNI_TRUE } else { JNI_FALSE }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_mokuro_bunko_BunkoNative_isRunning<'l>(_env: JNIEnv<'l>, _class: JClass<'l>) -> jboolean {
    if guarded(false, server::is_running) { JNI_TRUE } else { JNI_FALSE }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_mokuro_bunko_BunkoNative_logTail<'l>(mut env: JNIEnv<'l>, _class: JClass<'l>) -> jstring {
    let text = guarded(String::new(), logs::tail);
    to_jstring(&mut env, &text)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_mokuro_bunko_BunkoNative_lastError<'l>(mut env: JNIEnv<'l>, _class: JClass<'l>) -> jstring {
    let text = guarded(None, server::last_error).unwrap_or_default();
    to_jstring(&mut env, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_ports_before_starting() {
        assert_eq!(start_status("/x".into(), "/x/c.yaml".into(), 0, false), "error:invalid port 0");
        assert_eq!(start_status("/x".into(), "/x/c.yaml".into(), 70000, false), "error:invalid port 70000");
    }
}
