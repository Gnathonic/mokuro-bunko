//! A console program started from a process that has no console must not get a console
//! window (the app, and the server it starts, are such processes on Windows: every
//! `nvidia-smi` the server's load probe ran flashed a window).
//!
//! The test runs itself twice more to stand in for those processes:
//!
//! * `middle`: this test program started detached, so it has no console, as the server
//!   started by the app has none;
//! * `leaf`: this test program again (a console program, as `nvidia-smi` is), started
//!   by the middle one. It says whether it was given a console window.
//!
//! The middle process starts the leaf twice: plainly, which is the fault (the leaf gets
//! a window), and with [`Windowless`], which must give it none.
#![cfg(windows)]

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use bunko_update::tools::Windowless;

const STAGE: &str = "BUNKO_WINDOWLESS_STAGE";
const OUT: &str = "BUNKO_WINDOWLESS_OUT";
const TEST: &str = "a_console_program_started_without_a_console_gets_no_window";
const DETACHED_PROCESS: u32 = 0x0000_0008;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetConsoleWindow() -> *mut core::ffi::c_void;
}

fn console_window() -> &'static str {
    // SAFETY: takes nothing and returns a window handle or null.
    if unsafe { GetConsoleWindow() }.is_null() {
        "none"
    } else {
        "window"
    }
}

/// This test program, run as `stage`, writing what it finds to `out`.
fn stage(stage: &str, out: &Path) -> Command {
    let mut cmd = Command::new(std::env::current_exe().expect("this test program"));
    cmd.args(["--exact", TEST])
        .env(STAGE, stage)
        .env(OUT, out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

#[test]
fn a_console_program_started_without_a_console_gets_no_window() {
    let out = std::env::var_os(OUT).map(PathBuf::from);
    match (std::env::var(STAGE).as_deref(), out) {
        (Ok("leaf"), Some(out)) => {
            std::fs::write(out, console_window()).expect("write");
            return;
        }
        (Ok("middle"), Some(dir)) => {
            std::fs::write(dir.join("middle"), console_window()).expect("write");
            let plain = stage("leaf", &dir.join("plain")).status();
            let quiet = stage("leaf", &dir.join("windowless")).windowless().status();
            std::fs::write(
                dir.join("ran"),
                format!(
                    "{:?} {:?}",
                    plain.map(|s| s.success()),
                    quiet.map(|s| s.success())
                ),
            )
            .expect("write");
            return;
        }
        _ => {}
    }
    let dir = tempfile::tempdir().expect("tmp");
    let status = stage("middle", dir.path())
        .creation_flags(DETACHED_PROCESS)
        .status()
        .expect("start the middle process");
    assert!(status.success(), "the middle process: {status}");
    let read = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap_or_default();
    assert_eq!(read("ran"), "Ok(true) Ok(true)", "both leaves ran");
    assert_eq!(read("middle"), "none", "the middle process has no console");
    assert_eq!(
        read("plain"),
        "window",
        "a console program started plainly from a process without a console gets a \
         window: that is the fault this guards against, and the test must see it"
    );
    assert_eq!(read("windowless"), "none");
}
