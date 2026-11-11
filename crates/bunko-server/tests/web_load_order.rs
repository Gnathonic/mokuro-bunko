//! A page script's `state` literal runs once, when the script loads: whatever it
//! reads then must already be declared. A `const` or `let` read before its line is
//! a ReferenceError (the temporal dead zone), and a `try` around the read turns
//! that into a silent default. That is how the Analytics tab once forgot that the
//! anonymous visitors' row had been left open: `anonOpenPref()` read
//! `ANON_OPEN_KEY` a few lines before it was declared, so it always said closed.

use bunko_server::http::static_files::WebAssets;

fn ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

/// Every identifier in `text`, with whether a `(` follows it.
fn identifiers(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut prev = ' ';
    while let Some((i, c)) = chars.next() {
        if ident_char(c) && !ident_char(prev) && !c.is_ascii_digit() && prev != '.' {
            let mut end = i + c.len_utf8();
            while let Some(&(j, d)) = chars.peek() {
                if !ident_char(d) {
                    break;
                }
                end = j + d.len_utf8();
                chars.next();
            }
            let call = text[end..].trim_start().starts_with('(');
            out.push((text[i..end].to_string(), call));
            prev = 'a';
        } else {
            prev = c;
        }
    }
    out
}

/// From `start` (a line at `indent`) to the line that closes it at the same indent.
fn block<'a>(text: &'a str, start: usize, indent: &str, close: &str) -> &'a str {
    let closing = format!("\n{indent}{close}");
    let end = text[start..]
        .find(&closing)
        .map_or(text.len(), |e| start + e + closing.len());
    &text[start..end]
}

/// Names a `state` literal reads at load (directly, or inside a function it calls)
/// that are declared with `const`/`let` only after it.
fn read_before_declared(text: &str) -> Vec<String> {
    let Some(at) = text.find("const state = {") else {
        return Vec::new();
    };
    let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
    let indent = &text[line_start..at];
    let literal = block(text, at, indent, "};");

    let mut read: Vec<String> = Vec::new();
    for (name, call) in identifiers(literal) {
        if call {
            let def = format!("\n{indent}function {name}(");
            if let Some(d) = text.find(&def) {
                let body = block(text, d + 1, indent, "}");
                read.extend(identifiers(body).into_iter().map(|(n, _)| n));
            }
        }
        read.push(name);
    }

    let after = &text[at + literal.len()..];
    let mut late = Vec::new();
    for line in after.lines() {
        let Some(rest) = line.strip_prefix(indent) else {
            continue;
        };
        let decl = rest
            .strip_prefix("const ")
            .or_else(|| rest.strip_prefix("let "));
        let Some(decl) = decl else { continue };
        let name: String = decl.chars().take_while(|&c| ident_char(c)).collect();
        if !name.is_empty() && read.contains(&name) && !late.contains(&name) {
            late.push(name);
        }
    }
    late
}

#[test]
fn the_check_sees_a_late_declaration() {
    let bad = "(function () {\n    const state = {\n        open: pref(),\n        n: LIMIT,\n    };\n\n    const KEY = 'k';\n    const LIMIT = 3;\n\n    function pref() {\n        try {\n            return localStorage.getItem(KEY) === '1';\n        } catch (_) {\n            return false;\n        }\n    }\n})();\n";
    assert_eq!(read_before_declared(bad), ["KEY", "LIMIT"]);
    let good = bad
        .replace("    const KEY = 'k';\n    const LIMIT = 3;\n", "")
        .replace(
            "(function () {\n",
            "(function () {\n    const KEY = 'k';\n    const LIMIT = 3;\n",
        );
    assert!(read_before_declared(&good).is_empty());
}

#[test]
fn page_state_reads_nothing_declared_after_it() {
    let mut found = Vec::new();
    let mut checked = 0;
    for name in WebAssets::iter() {
        if !name.ends_with(".js") {
            continue;
        }
        let Some(file) = WebAssets::get(&name) else {
            continue;
        };
        let text = String::from_utf8_lossy(&file.data).into_owned();
        if text.contains("const state = {") {
            checked += 1;
        }
        for late in read_before_declared(&text) {
            found.push(format!(
                "{name}: `state` reads {late} before it is declared"
            ));
        }
    }
    assert!(checked > 0, "no script with a `state` literal was checked");
    assert!(found.is_empty(), "{}", found.join("\n"));
}
