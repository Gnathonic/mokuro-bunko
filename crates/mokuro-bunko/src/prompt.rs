//! Interactive prompts with click's look and behaviour, so the wizards read (and can be
//! scripted through stdin) exactly as in 0.5.2:
//!
//! * `Text [default]: ` — empty input takes the default; with no default it asks again.
//! * `Text (a, b, c) [b]: ` for choices; `Text [y/N]: ` / `[Y/n]: ` for confirmations.
//! * Hidden input on a terminal (rpassword); from a pipe, the next line of stdin.
//! * End of input aborts: `Aborted!` on stderr, exit 1.

use crate::out::Fail;
use std::io::{IsTerminal, Write};

fn abort() -> Fail {
    eprintln!("Aborted!");
    Fail::Exit(1)
}

fn show(prompt: &str) {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
}

/// One line of stdin without its line ending; `None` at end of input.
fn read_line() -> Result<Option<String>, Fail> {
    let mut line = String::new();
    let n = std::io::stdin().read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    Ok(Some(line))
}

fn build(text: &str, default: Option<&str>, show_default: bool) -> String {
    match default {
        Some(d) if show_default => format!("{text} [{d}]: "),
        _ => format!("{text}: "),
    }
}

/// `click.prompt(text, default=...)`.
pub fn text(text: &str, default: Option<&str>) -> Result<String, Fail> {
    text_opts(text, default, true)
}

/// `click.prompt(text, default=..., show_default=...)`.
pub fn text_opts(text: &str, default: Option<&str>, show_default: bool) -> Result<String, Fail> {
    let prompt = build(text, default, show_default);
    loop {
        show(&prompt);
        let value = read_line()?.ok_or_else(abort)?;
        if !value.is_empty() {
            return Ok(value);
        }
        if let Some(d) = default {
            return Ok(d.to_string());
        }
    }
}

/// A prompt whose answer must pass `parse` (re-asked with `Error: <msg>` otherwise).
pub fn parsed<T>(text: &str, default: Option<&str>, parse: impl Fn(&str) -> Result<T, String>) -> Result<T, Fail> {
    loop {
        let raw = self::text(text, default)?;
        match parse(&raw) {
            Ok(v) => return Ok(v),
            Err(e) => println!("Error: {e}"),
        }
    }
}

/// click's `type=Choice([...])`.
pub fn choice(text: &str, choices: &[&str], default: Option<&str>) -> Result<String, Fail> {
    let label = format!("{text} ({})", choices.join(", "));
    parsed(&label, default, |s| {
        if choices.contains(&s) {
            Ok(s.to_string())
        } else {
            let quoted: Vec<String> = choices.iter().map(|c| format!("'{c}'")).collect();
            Err(format!("'{s}' is not one of {}.", quoted.join(", ")))
        }
    })
}

/// `click.confirm(text, default=...)`. `None` default means an answer is required.
pub fn confirm(text: &str, default: Option<bool>) -> Result<bool, Fail> {
    let suffix = match default {
        Some(true) => "[Y/n]",
        Some(false) => "[y/N]",
        None => "[y/n]",
    };
    let prompt = format!("{text} {suffix}: ");
    loop {
        show(&prompt);
        let value = read_line()?.ok_or_else(abort)?.trim().to_lowercase();
        match value.as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            "" if default.is_some() => return Ok(default.unwrap_or(false)),
            _ => println!("Error: invalid input"),
        }
    }
}

/// `click.confirm(text, abort=True)`: "no" aborts with exit 1.
pub fn confirm_or_abort(text: &str) -> Result<(), Fail> {
    if confirm(text, Some(false))? { Ok(()) } else { Err(abort()) }
}

fn hidden_once(prompt: &str) -> Result<String, Fail> {
    if std::io::stdin().is_terminal() {
        match rpassword::prompt_password(prompt) {
            Ok(v) => Ok(v),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(abort()),
            Err(e) => Err(Fail::from(e)),
        }
    } else {
        show(prompt);
        let v = read_line()?.ok_or_else(abort)?;
        println!();
        Ok(v)
    }
}

/// `click.prompt(text, hide_input=True[, confirmation_prompt=True])`. Empty input asks again.
pub fn hidden(text: &str, confirmation: bool) -> Result<String, Fail> {
    let prompt = format!("{text}: ");
    loop {
        let value = hidden_once(&prompt)?;
        if value.is_empty() {
            continue;
        }
        if !confirmation {
            return Ok(value);
        }
        let again = hidden_once("Repeat for confirmation: ")?;
        if again == value {
            return Ok(value);
        }
        println!("Error: The two entered values do not match.");
    }
}

/// A password from `--password`, or asked for (hidden, confirmed) as click's
/// `prompt=True, hide_input=True, confirmation_prompt=True` option does.
pub fn password_option(given: Option<String>, text: &str) -> Result<String, Fail> {
    match given {
        Some(p) => Ok(p),
        None => hidden(text, true),
    }
}
