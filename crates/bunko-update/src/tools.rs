//! Starting other programs from a process that may have no console.
//!
//! On Windows the app is `mokuro-bunko.exe`, a GUI-subsystem copy of the program, and
//! the tray starts the server and the processor from that same file: none of them has a
//! console. A console program started from such a process (`nvidia-smi`, `icacls`,
//! `cloudflared`) is given a console of its own, and Windows shows that console's
//! window for as long as the program runs: a flash for a probe, every few seconds for
//! a probe that repeats, a window that stays for a tunnel.
//!
//! [`Windowless::windowless`] starts the program with `CREATE_NO_WINDOW`: it gets its
//! console, and no window. It changes nothing for a program whose output is captured
//! or thrown away, which is the only kind to use it on; elsewhere it does nothing.

/// `CREATE_NO_WINDOW`: a console program runs without a console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// See the module docs.
pub trait Windowless {
    /// On Windows, start this program without a console window.
    fn windowless(&mut self) -> &mut Self;
}

impl Windowless for std::process::Command {
    fn windowless(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}

impl Windowless for tokio::process::Command {
    fn windowless(&mut self) -> &mut Self {
        #[cfg(windows)]
        self.creation_flags(CREATE_NO_WINDOW);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag changes nothing about what a program is given or returns.
    #[test]
    fn a_windowless_program_still_runs_and_is_read() {
        let (program, args): (&str, &[&str]) = if cfg!(windows) {
            ("cmd", &["/C", "echo here"])
        } else {
            ("sh", &["-c", "echo here"])
        };
        let out = std::process::Command::new(program)
            .args(args)
            .windowless()
            .output()
            .expect("run");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "here");
    }
}
