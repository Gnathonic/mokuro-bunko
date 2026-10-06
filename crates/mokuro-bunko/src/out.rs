//! Command results, exit codes and terminal styling.
//!
//! 0.5.2 (click) conventions: a usage error exits 2 (clap does the same), a reported
//! failure prints `Error: <msg>` on stderr and exits 1.

use std::fmt::Display;
use std::io::IsTerminal;

/// How a command failed.
#[derive(Debug)]
pub enum Fail {
    /// Print `Error: <msg>` on stderr, exit 1.
    Error(String),
    /// The message (if any) is already printed; exit with this code.
    Exit(i32),
}

pub type CmdResult = Result<(), Fail>;

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::Error(m) => f.write_str(m),
            Fail::Exit(code) => write!(f, "exit code {code}"),
        }
    }
}

impl Fail {
    pub fn msg(m: impl Display) -> Fail {
        Fail::Error(m.to_string())
    }
}

impl From<bunko_core::ConfigError> for Fail {
    fn from(e: bunko_core::ConfigError) -> Self {
        Fail::msg(e)
    }
}

impl From<bunko_db::DbError> for Fail {
    fn from(e: bunko_db::DbError) -> Self {
        Fail::msg(e)
    }
}

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Self {
        Fail::msg(e)
    }
}

impl From<anyhow::Error> for Fail {
    fn from(e: anyhow::Error) -> Self {
        Fail::Error(format!("{e:#}"))
    }
}

/// Print `msg` on stderr and fail with exit code 1 (click's `echo(err=True); exit(1)`).
pub fn exit_with(msg: impl Display) -> Fail {
    eprintln!("{msg}");
    Fail::Exit(1)
}

#[derive(Clone, Copy)]
pub enum Color {
    Red,
    Green,
    Yellow,
}

/// click.style: ANSI colour (+ bold) only when stdout is a terminal and `NO_COLOR` is unset.
pub fn style(text: &str, color: Color, bold: bool) -> String {
    if !color_enabled() {
        return text.to_string();
    }
    let code = match color {
        Color::Red => 31,
        Color::Green => 32,
        Color::Yellow => 33,
    };
    if bold {
        format!("\x1b[{code};1m{text}\x1b[0m")
    } else {
        format!("\x1b[{code}m{text}\x1b[0m")
    }
}

fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()) && std::io::stdout().is_terminal()
}

/// A tokio runtime for the few commands that do network I/O.
pub fn runtime() -> Result<tokio::runtime::Runtime, Fail> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Fail::from)
}
