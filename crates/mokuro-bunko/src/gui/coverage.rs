//! The CLI's GUI coverage: every command, flag and argument of the clap tree has a row
//! in `docs/rust-port/GUI-COVERAGE.md` — the server's own pages (its `/setup`, the
//! admin panel `/_admin#tab`), a local processor page (`/app/...`), a tray item
//! (`Tray → "…"`), or `CLI-only because …`. The test below walks the tree and fails
//! on a missing row, a row without a home, a link to a page, admin tab or tray item
//! that does not exist, and (full build, which has every command) a stale row.

#[cfg(test)]
mod tests {
    use crate::cli::Cli;
    use clap::CommandFactory;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    /// `install-ocr`, `install-ocr --from`, `admin add-user <USERNAME>`, `--config`.
    fn walk(cmd: &clap::Command, path: &str, out: &mut Vec<String>) {
        let join = |s: &str| {
            if path.is_empty() {
                s.to_string()
            } else {
                format!("{path} {s}")
            }
        };
        for a in cmd.get_arguments() {
            let id = a.get_id().as_str();
            if id == "help" {
                continue;
            }
            if let Some(long) = a.get_long() {
                out.push(join(&format!("--{long}")));
            } else if let Some(short) = a.get_short() {
                out.push(join(&format!("-{short}")));
            } else {
                let name = a
                    .get_value_names()
                    .and_then(|v| v.first().map(|s| s.to_string()))
                    .unwrap_or_else(|| id.to_uppercase());
                out.push(join(&format!("<{name}>")));
            }
        }
        for sub in cmd.get_subcommands() {
            if sub.get_name() == "help" {
                continue;
            }
            let p = join(sub.get_name());
            out.push(p.clone());
            walk(sub, &p, out);
        }
    }

    fn cli_keys() -> Vec<String> {
        let mut out = Vec::new();
        walk(&Cli::command(), "", &mut out);
        out
    }

    /// `| \`key\` | target |` rows of the coverage table.
    fn rows(doc: &str) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for line in doc.lines() {
            let Some(rest) = line.trim().strip_prefix("| `") else {
                continue;
            };
            let Some((key, rest)) = rest.split_once("` |") else {
                continue;
            };
            let target = rest.trim().trim_end_matches('|').trim().to_string();
            assert!(
                m.insert(key.to_string(), target).is_none(),
                "GUI-COVERAGE.md lists `{key}` twice"
            );
        }
        m
    }

    /// The `/app/...` paths a target links (`](/app/settings/https)`), without `/app/`.
    fn app_links(target: &str) -> Vec<String> {
        let mut v = Vec::new();
        let mut s = target;
        while let Some(i) = s.find("](/app/") {
            let rest = &s[i + 7..];
            let end = rest.find([')', '#', '?']).unwrap_or(rest.len());
            v.push(rest[..end].to_string());
            s = &rest[end..];
        }
        v
    }

    /// The admin panel tabs a target links (`](/_admin#server)`).
    fn admin_tabs(target: &str) -> Vec<String> {
        let mut v = Vec::new();
        let mut s = target;
        while let Some(i) = s.find("](/_admin#") {
            let rest = &s[i + 10..];
            let end = rest.find(')').unwrap_or(rest.len());
            v.push(rest[..end].to_string());
            s = &rest[end..];
        }
        v
    }

    /// The tray items a target names (`Tray → "Start at login"`).
    fn tray_items(target: &str) -> Vec<String> {
        let mut v = Vec::new();
        let mut s = target;
        while let Some(i) = s.find("Tray → \"") {
            let rest = &s[i + "Tray → \"".len()..];
            let end = rest.find('"').unwrap_or(rest.len());
            v.push(rest[..end].to_string());
            s = &rest[end..];
        }
        v
    }

    #[test]
    fn every_command_and_flag_has_a_gui_entry() {
        let doc_path = repo_root().join("docs/rust-port/GUI-COVERAGE.md");
        let doc = std::fs::read_to_string(&doc_path).expect("docs/rust-port/GUI-COVERAGE.md");
        let table = rows(&doc);
        let keys = cli_keys();
        if std::env::var_os("GUI_COVERAGE_PRINT").is_some() {
            for k in &keys {
                println!("{k}");
            }
        }
        assert!(
            keys.len() > 50,
            "the clap walk found only {} keys",
            keys.len()
        );
        let settings = std::fs::read_to_string(repo_root().join("web/app/settings.html"))
            .expect("web/app/settings.html");
        let admin = std::fs::read_to_string(repo_root().join("web/admin/index.html"))
            .expect("web/admin/index.html");
        // The tray's menu labels (the Linux menu; the Windows/macOS one has the same).
        let tray = [
            "crates/bunko-tray/src/app/sni.rs",
            "crates/bunko-tray/src/model.rs",
        ]
        .map(|f| std::fs::read_to_string(repo_root().join(f)).expect(f))
        .join("\n");
        assert!(repo_root().join("web/setup/index.html").is_file());

        let mut problems = Vec::new();
        for k in &keys {
            match table.get(k) {
                None => problems.push(format!("missing row: | `{k}` | … |")),
                Some(t) => {
                    let linked = t.contains("](/app/")
                        || t.contains("](/_admin#")
                        || t.contains("](/setup)")
                        || t.contains("Tray → \"");
                    if !linked && !t.contains("CLI-only because") {
                        problems.push(format!(
                            "`{k}`: link an /app/, /_admin# or /setup page, name a tray item, or say \"CLI-only because …\""
                        ));
                    }
                }
            }
        }
        for (k, t) in &table {
            for link in app_links(t) {
                let Some(file) = super::super::page_file(&link) else {
                    problems.push(format!("`{k}`: bad app link /app/{link}"));
                    continue;
                };
                if !repo_root().join("web/app").join(&file).is_file() {
                    problems.push(format!(
                        "`{k}`: /app/{link} → web/app/{file} does not exist"
                    ));
                }
                if let Some(section) = link.strip_prefix("settings/") {
                    let section = section.trim_end_matches('/');
                    if !settings.contains(&format!("data-section=\"{section}\"")) {
                        problems.push(format!("`{k}`: settings has no section {section}"));
                    }
                }
            }
            for tab in admin_tabs(t) {
                if !admin.contains(&format!("data-tab=\"{tab}\"")) {
                    problems.push(format!("`{k}`: the admin panel has no tab #{tab}"));
                }
            }
            for item in tray_items(t) {
                if !tray.contains(&format!("\"{item}\"")) {
                    problems.push(format!("`{k}`: the tray has no item \"{item}\""));
                }
            }
            // The full build has every command: a row it does not know is stale.
            #[cfg(feature = "ocr")]
            if !keys.contains(k) {
                problems.push(format!("stale row (no such command or flag): `{k}`"));
            }
        }
        assert!(
            problems.is_empty(),
            "{}:\n  {}",
            doc_path.display(),
            problems.join("\n  ")
        );
    }

    #[test]
    fn parses_rows_and_links() {
        let t = rows(
            "| Command | Where |\n|---|---|\n\
             | `ssl enable` | [HTTPS](/app/settings/https) |\n\
             | `--version` | CLI-only because scripts read it |\n",
        );
        assert_eq!(t.len(), 2);
        assert_eq!(app_links(&t["ssl enable"]), vec!["settings/https"]);
        assert!(app_links(&t["--version"]).is_empty());
        assert_eq!(
            admin_tabs("[This server](/_admin#server) → OCR; [Users](/_admin#users)"),
            vec!["server", "users"]
        );
        assert_eq!(
            tray_items("`Tray → \"Start at login\"` and `Tray → \"Quit\"`"),
            vec!["Start at login", "Quit"]
        );
        let keys = cli_keys();
        for k in [
            "--config",
            "--version",
            "serve",
            "serve --port",
            "gui",
            "gui --open",
        ] {
            assert!(keys.iter().any(|x| x == k), "{k} not walked");
        }
    }
}
