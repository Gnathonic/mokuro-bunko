//! "Action needed" desktop notifications, shown without a new crate: the freedesktop
//! D-Bus `Notifications.Notify` call through `gdbus` (Linux; `notify-send` as the
//! fallback), `osascript` (macOS) and a PowerShell toast (Windows, unverified on a
//! real machine). They fire whatever `tray.json`'s `notifications` says: they are
//! alarms, not chatter. The argument builders are pure and unit-tested.

use std::process::{Command, Stdio};

/// A GVariant text-format string literal: single-quoted, `\` and `'` escaped,
/// control characters spelled out.
pub fn gvariant_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Notification servers render a small markup subset in the body: escape it.
pub fn body_markup_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// argv (after `gdbus`) for `Notify(app, replaces_id, icon, summary, body, actions,
/// hints{urgency: critical}, expire_timeout)`.
pub fn gdbus_args(summary: &str, body: &str) -> Vec<String> {
    let mut a: Vec<String> = [
        "call",
        "--session",
        "--dest",
        "org.freedesktop.Notifications",
        "--object-path",
        "/org/freedesktop/Notifications",
        "--method",
        "org.freedesktop.Notifications.Notify",
    ]
    .map(String::from)
    .to_vec();
    a.push(gvariant_str("mokuro-bunko"));
    a.push("uint32 0".into());
    a.push(gvariant_str("")); // icon: none
    a.push(gvariant_str(summary));
    a.push(gvariant_str(&body_markup_escape(body)));
    a.push("@as []".into());
    a.push("{'urgency': <byte 2>}".into());
    a.push("int32 0".into()); // never expires on its own
    a
}

/// argv for the `notify-send` fallback.
pub fn notify_send_args(summary: &str, body: &str) -> Vec<String> {
    vec![
        "-u".into(),
        "critical".into(),
        "-a".into(),
        "mokuro-bunko".into(),
        "--".into(),
        summary.into(),
        body_markup_escape(body),
    ]
}

/// The AppleScript for `osascript -e`.
pub fn osascript(summary: &str, body: &str) -> String {
    let q = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "display notification \"{}\" with title \"{}\"",
        q(body),
        q(summary)
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The PowerShell script (for `-Command`) that shows a toast. Text is XML-escaped,
/// then PowerShell-single-quote-escaped.
pub fn powershell_toast(summary: &str, body: &str) -> String {
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(summary),
        xml_escape(body)
    );
    let ps = xml.replace('\'', "''");
    format!(
        "$ErrorActionPreference='Stop'; \
         [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; \
         [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null; \
         $x = New-Object Windows.Data.Xml.Dom.XmlDocument; $x.LoadXml('{ps}'); \
         $t = [Windows.UI.Notifications.ToastNotification]::new($x); \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe').Show($t)"
    )
}

fn quiet(cmd: &mut Command) -> &mut Command {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
}

/// Show a critical-urgency "action needed" notification. Never blocks the caller:
/// the work runs on its own thread.
pub fn alarm(summary: &str, body: &str) {
    let (summary, body) = (summary.to_string(), body.to_string());
    std::thread::spawn(move || {
        if let Err(e) = show(&summary, &body) {
            tracing::warn!("could not show the notification \"{summary}\": {e}");
        }
    });
}

#[cfg(target_os = "linux")]
fn show(summary: &str, body: &str) -> Result<(), String> {
    let via_gdbus = quiet(&mut Command::new("gdbus"))
        .args(gdbus_args(summary, body))
        .status();
    match via_gdbus {
        Ok(s) if s.success() => return Ok(()),
        Ok(s) => tracing::debug!("gdbus notify: {s}"),
        Err(e) => tracing::debug!("gdbus not available: {e}"),
    }
    let s = quiet(&mut Command::new("notify-send"))
        .args(notify_send_args(summary, body))
        .status()
        .map_err(|e| format!("gdbus failed and notify-send: {e}"))?;
    if s.success() {
        Ok(())
    } else {
        Err(format!("gdbus failed and notify-send: {s}"))
    }
}

#[cfg(target_os = "macos")]
fn show(summary: &str, body: &str) -> Result<(), String> {
    let s = quiet(&mut Command::new("osascript"))
        .args(["-e", &osascript(summary, body)])
        .status()
        .map_err(|e| format!("osascript: {e}"))?;
    if s.success() {
        Ok(())
    } else {
        Err(format!("osascript: {s}"))
    }
}

#[cfg(windows)]
fn show(summary: &str, body: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let s = quiet(&mut Command::new("powershell"))
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &powershell_toast(summary, body),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("powershell: {e}"))?;
    if s.success() {
        Ok(())
    } else {
        Err(format!("powershell toast: {s}"))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn show(_summary: &str, _body: &str) -> Result<(), String> {
    Err("no notification mechanism on this OS".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gvariant_strings_are_quoted_and_escaped() {
        assert_eq!(gvariant_str("plain"), "'plain'");
        assert_eq!(gvariant_str("it's a \\ path"), "'it\\'s a \\\\ path'");
        assert_eq!(gvariant_str("a\nb\tc\u{7}d"), "'a\\nb\\tc d'");
        assert_eq!(gvariant_str(""), "''");
    }

    #[test]
    fn gdbus_argv_has_the_notify_call_and_one_entry_per_argument() {
        let a = gdbus_args("Mokuro Bunko needs you", "It's <bad> & done\nFix: x'y");
        assert_eq!(&a[..2], ["call", "--session"]);
        assert_eq!(a[7], "org.freedesktop.Notifications.Notify");
        let args = &a[8..];
        assert_eq!(
            args,
            [
                "'mokuro-bunko'",
                "uint32 0",
                "''",
                "'Mokuro Bunko needs you'",
                "'It\\'s &lt;bad&gt; &amp; done\\nFix: x\\'y'",
                "@as []",
                "{'urgency': <byte 2>}",
                "int32 0",
            ]
        );
    }

    #[test]
    fn notify_send_is_critical_and_ends_options_before_the_text() {
        let a = notify_send_args("-t", "a<b");
        assert_eq!(&a[..2], ["-u", "critical"]);
        let dd = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(&a[dd + 1..], ["-t", "a&lt;b"]);
    }

    #[test]
    fn osascript_escapes_quotes_and_backslashes() {
        assert_eq!(
            osascript("T\"x", "b\\ \"q\""),
            "display notification \"b\\\\ \\\"q\\\"\" with title \"T\\\"x\""
        );
    }

    #[test]
    fn the_toast_script_escapes_xml_and_quotes() {
        let s = powershell_toast("Needs <you>", "it's \"a\" & b");
        assert!(s.contains("<text>Needs &lt;you&gt;</text>"), "{s}");
        assert!(s.contains("it&apos;s &quot;a&quot; &amp; b"), "{s}");
        assert!(s.contains(
            "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe"
        ));
    }
}
