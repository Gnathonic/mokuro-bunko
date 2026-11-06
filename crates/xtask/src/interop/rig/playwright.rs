//! The browser suites' specs (`interop/e2e/rig/<suite>/`), run by Bunko's own
//! Playwright install in `interop/` with the parties' addresses in the environment
//! (C6-contract), and read back from Playwright's JSON report into the rig's results.

use super::results::{Outcome, Scenario};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What the rig passes to the specs (interop/e2e/rig/fixtures.mjs reads it).
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub reader_url: String,
    pub bunko_url: Option<String>,
    pub cloudsim_url: Option<String>,
    pub preauth_url: Option<String>,
    pub rclone_url: Option<String>,
    pub provider: String,
    /// A JSON file: `[{username, password, role}]`.
    pub accounts: PathBuf,
    pub workdir: PathBuf,
    /// processor-fake holds each volume until this file exists.
    pub fakeproc_hold: Option<PathBuf>,
    /// `RIG_CONTEXT`: the cloud account, its credentials, the seeded library.
    pub context: Value,
    /// `RIG_CONTROL_URL`: the runner's control port (control.rs), with a Bunko server.
    pub control_url: Option<String>,
}

/// Every variable [`Env::vars`] may set: all are cleared first, so one a suite does not
/// have never leaks in from the caller's shell.
const VARS: [&str; 11] = [
    "READER_URL",
    "BUNKO_URL",
    "CLOUDSIM_URL",
    "CLOUDSIM_PREAUTH_URL",
    "RCLONE_URL",
    "RIG_PROVIDER",
    "RIG_ACCOUNTS",
    "RIG_WORKDIR",
    "RIG_FAKEPROC_HOLD",
    "RIG_CONTEXT",
    "RIG_CONTROL_URL",
];

impl Env {
    pub fn vars(&self) -> Vec<(&'static str, String)> {
        let path = |p: &Path| p.display().to_string();
        let mut v = vec![
            ("READER_URL", self.reader_url.clone()),
            ("RIG_PROVIDER", self.provider.clone()),
            ("RIG_ACCOUNTS", path(&self.accounts)),
            ("RIG_WORKDIR", path(&self.workdir)),
            ("RIG_CONTEXT", self.context.to_string()),
        ];
        for (name, value) in [
            ("BUNKO_URL", &self.bunko_url),
            ("CLOUDSIM_URL", &self.cloudsim_url),
            ("CLOUDSIM_PREAUTH_URL", &self.preauth_url),
            ("RCLONE_URL", &self.rclone_url),
            ("RIG_CONTROL_URL", &self.control_url),
        ] {
            if let Some(value) = value {
                v.push((name, value.clone()));
            }
        }
        if let Some(hold) = &self.fakeproc_hold {
            v.push(("RIG_FAKEPROC_HOLD", path(hold)));
        }
        v
    }
}

/// `interop/pending-reader-caps.txt`: one capability per line, `#` comments; the same
/// rule as `parsePendingCaps` in interop/e2e/rig/caps.mjs. Read before anything
/// starts, so a malformed file fails the run, not every spec one by one.
pub fn parse_caps(text: &str) -> Result<Vec<String>> {
    let mut caps: Vec<String> = Vec::new();
    for (i, raw) in text.split('\n').enumerate() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let valid = line.bytes().next().is_some_and(|b| b != b'-')
            && line
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !valid {
            bail!(
                "pending-reader-caps.txt line {}: {line:?} is not a capability name",
                i + 1
            );
        }
        if caps.iter().any(|c| c == line) {
            bail!(
                "pending-reader-caps.txt line {}: {line} is listed twice",
                i + 1
            );
        }
        caps.push(line.to_string());
    }
    Ok(caps)
}

/// The specs of one suite's folder.
pub fn spec_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.to_string_lossy().ends_with(".spec.mjs") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// The scenarios a Playwright JSON report describes (one per test), the hosts its
/// specs were refused (each spec's `refused-hosts` annotation) and the Google scripts
/// they were answered empty (`stubbed-urls`).
#[derive(Debug, Default)]
pub struct Parsed {
    pub scenarios: Vec<Scenario>,
    pub refused_hosts: Vec<String>,
    pub stubbed_urls: Vec<String>,
}

pub fn parse_report(v: &Value) -> Parsed {
    let mut p = Parsed::default();
    for suite in v["suites"].as_array().into_iter().flatten() {
        let file = suite["file"].as_str().unwrap_or_default().to_string();
        walk(suite, &file, &[], &mut p);
    }
    let errors: Vec<&str> = v["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["message"].as_str())
        .collect();
    if !errors.is_empty() {
        p.scenarios.push(Scenario {
            name: "playwright".into(),
            outcome: Outcome::Failed,
            ms: 0,
            detail: Some(errors.join("\n")),
            capability: None,
            notes: Vec::new(),
        });
    }
    for list in [&mut p.refused_hosts, &mut p.stubbed_urls] {
        list.sort();
        list.dedup();
    }
    p
}

/// One suite of the report: its specs, then its nested suites (`describe`s), in the
/// report's order. The file-level suite's title is the file, named once.
fn walk(suite: &Value, file: &str, titles: &[String], p: &mut Parsed) {
    for spec in suite["specs"].as_array().into_iter().flatten() {
        let mut path = titles.to_vec();
        path.push(spec["title"].as_str().unwrap_or_default().to_string());
        let name = format!("{file} › {}", path.join(" › "));
        for test in spec["tests"].as_array().into_iter().flatten() {
            let mut between = Vec::new();
            p.scenarios.push(scenario(
                &name,
                test,
                &mut p.refused_hosts,
                &mut p.stubbed_urls,
                &mut between,
            ));
            // cloudsim violations recorded before the spec started are nobody's spec's:
            // they fail on their own, named after the spec that found them.
            for v in between {
                p.scenarios.push(Scenario {
                    name: format!("cloudsim violations before {name}"),
                    outcome: Outcome::Failed,
                    ms: 0,
                    detail: Some(v),
                    capability: None,
                    notes: Vec::new(),
                });
            }
        }
    }
    for child in suite["suites"].as_array().into_iter().flatten() {
        let mut path = titles.to_vec();
        path.push(child["title"].as_str().unwrap_or_default().to_string());
        walk(child, file, &path, p);
    }
}

/// Annotation types a spec leaves for whoever triages, kept in the results whatever the
/// outcome: cloud-owner's `restore-unseen` (a file Bunko did not list), which a passed
/// spec would otherwise drop and a failed one leave out of its errors.
const NOTES: &[&str] = &["restore-unseen"];

fn scenario(
    name: &str,
    test: &Value,
    refused: &mut Vec<String>,
    stubbed: &mut Vec<String>,
    between: &mut Vec<String>,
) -> Scenario {
    let results: Vec<&Value> = test["results"].as_array().into_iter().flatten().collect();
    // Annotations pushed while a test runs (the fixtures') are on its results.
    let annotations: Vec<&Value> = test["annotations"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(
            results
                .iter()
                .flat_map(|r| r["annotations"].as_array().into_iter().flatten()),
        )
        .collect();
    let notes = |kind: &str| {
        let mut out: Vec<String> = Vec::new();
        for a in &annotations {
            if a["type"] == kind
                && let Some(d) = a["description"].as_str()
                && !out.iter().any(|x| x == d)
            {
                out.push(d.to_string());
            }
        }
        out
    };
    for hosts in notes("refused-hosts") {
        refused.extend(hosts.split_whitespace().map(str::to_string));
    }
    for urls in notes("stubbed-urls") {
        stubbed.extend(urls.split_whitespace().map(str::to_string));
    }
    between.extend(notes("cloudsim-violations-between"));
    let ms = results.iter().filter_map(|r| r["duration"].as_u64()).sum();
    let error = || {
        let messages: Vec<&str> = results
            .iter()
            .flat_map(|r| r["errors"].as_array().into_iter().flatten())
            .filter_map(|e| e["message"].as_str())
            .collect();
        (!messages.is_empty()).then(|| messages.join("\n"))
    };
    let (outcome, detail, capability) = match test["status"].as_str() {
        Some("expected") => (Outcome::Passed, None, None),
        Some("skipped") => {
            let caps = notes("pending-reader-cap");
            let why = notes("skip");
            (
                Outcome::Skipped,
                (!why.is_empty()).then(|| why.join("; ")),
                (!caps.is_empty()).then(|| caps.join(",")),
            )
        }
        Some("flaky") => (
            Outcome::Failed,
            Some(format!(
                "flaky: failed, then passed on a retry\n{}",
                error().unwrap_or_default()
            )),
            None,
        ),
        other => (
            Outcome::Failed,
            Some(error().unwrap_or_else(|| format!("status {other:?}"))),
            None,
        ),
    };
    Scenario {
        name: name.to_string(),
        outcome,
        ms,
        detail,
        capability,
        notes: NOTES
            .iter()
            .flat_map(|kind| notes(kind).into_iter().map(move |d| format!("{kind}: {d}")))
            .collect(),
    }
}

/// `npm ci` in `interop/` when Playwright is not installed there yet.
pub fn ensure_installed(interop: &Path) -> Result<()> {
    if interop.join("node_modules/@playwright/test").is_dir() {
        return Ok(());
    }
    crate::util::run(
        Command::new("npm")
            .args(["ci", "--no-audit", "--no-fund"])
            .current_dir(interop),
    )
    .context("npm ci in interop/ (the rig's Playwright)")
}

/// How one suite's specs run.
pub struct Run<'a> {
    pub interop: &'a Path,
    /// The suite (its specs are `e2e/rig/<suite>/`; RIG_SUITE).
    pub suite: &'a str,
    pub env: &'a Env,
    /// Playwright's JUnit XML, beside the results JSON.
    pub junit: &'a Path,
    pub grep: Option<&'a str>,
    pub headed: bool,
}

/// `--grep` as the cloud suite means it: a case-sensitive substring of the spec's
/// name. Playwright reads a bare value as a case-insensitive regex and `/…/` as one with
/// no flags, so the value goes inside slashes, escaped. A suite it matches nothing in
/// has no scenarios; Playwright would fail it ("No tests found").
pub fn grep_args(grep: &str) -> [String; 3] {
    let mut re = String::from("/");
    for c in grep.chars() {
        if "\\^$.|?*+()[]{}/".contains(c) {
            re.push('\\');
        }
        re.push(c);
    }
    re.push('/');
    ["--grep".into(), re, "--pass-with-no-tests".into()]
}

/// Run the suite's specs; the scenarios come from Playwright's JSON report. A run that
/// exits non-zero without a failed spec in its report still fails.
pub fn run(r: &Run) -> Result<Parsed> {
    let json = r.env.workdir.join("playwright-report.json");
    let _ = std::fs::remove_file(&json);
    let mut cmd = Command::new("npx");
    cmd.args([
        "playwright",
        "test",
        "--config",
        "e2e/rig/playwright.config.mjs",
    ])
    .current_dir(r.interop);
    if let Some(g) = r.grep {
        cmd.args(grep_args(g));
    }
    for name in VARS {
        cmd.env_remove(name);
    }
    cmd.envs(r.env.vars())
        .env("RIG_SUITE", r.suite)
        .env("RIG_PW_JSON", &json)
        .env("RIG_JUNIT", r.junit);
    if r.headed {
        cmd.env("RIG_HEADED", "1");
    } else {
        cmd.env_remove("RIG_HEADED");
    }
    eprintln!("+ npx playwright test ({})", r.suite);
    let status = cmd.status().context("starting npx playwright")?;
    let text = std::fs::read(&json).with_context(|| {
        format!(
            "Playwright exited ({status}) without its JSON report {}",
            json.display()
        )
    })?;
    let mut parsed = parse_report(&serde_json::from_slice(&text).context("Playwright's report")?);
    if !status.success()
        && !parsed
            .scenarios
            .iter()
            .any(|s| s.outcome == Outcome::Failed)
    {
        parsed.scenarios.push(Scenario {
            name: "playwright".into(),
            outcome: Outcome::Failed,
            ms: 0,
            detail: Some(format!("exited {status} with no failed spec in its report")),
            capability: None,
            notes: Vec::new(),
        });
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Value {
        serde_json::from_str(include_str!("testdata/playwright-report.json")).unwrap()
    }

    #[test]
    fn every_test_in_the_report_is_a_scenario() {
        let p = parse_report(&report());
        let got: Vec<(&str, Outcome)> = p
            .scenarios
            .iter()
            .map(|s| (s.name.as_str(), s.outcome))
            .collect();
        assert_eq!(
            got,
            [
                ("sample.spec.mjs › passes", Outcome::Passed),
                // test.fail: failing is what it expects.
                ("sample.spec.mjs › expected failure", Outcome::Passed),
                ("sample.spec.mjs › plain skip", Outcome::Skipped),
                ("sample.spec.mjs › refuses", Outcome::Passed),
                ("sample.spec.mjs › outer › inner › fails", Outcome::Failed),
                (
                    "sample.spec.mjs › pending › skips with two caps",
                    Outcome::Skipped
                ),
            ]
        );
        let by = |n: &str| p.scenarios.iter().find(|s| s.name.ends_with(n)).unwrap();
        let failed = by("fails");
        assert!(
            failed.detail.as_deref().unwrap().contains("one is two"),
            "{failed:?}"
        );
        assert_eq!(by("plain skip").capability, None);
        assert_eq!(
            by("plain skip").detail.as_deref(),
            Some("not on this provider")
        );
        assert_eq!(
            by("skips with two caps").capability.as_deref(),
            Some("history,dedup")
        );
        assert_eq!(by("passes").detail, None);
        assert_eq!(by("refuses").ms, 53);
        assert_eq!(p.refused_hosts, ["fonts.googleapis.com"]);
        assert_eq!(
            p.stubbed_urls,
            [
                "https://accounts.google.com/gsi/client",
                "https://apis.google.com/js/api.js"
            ]
        );
    }

    #[test]
    fn a_report_level_error_is_a_failed_scenario() {
        let v = serde_json::json!({
            "suites": [],
            "errors": [{"message": "Error: No tests found"}],
            "stats": {"expected": 0, "unexpected": 0, "skipped": 0, "flaky": 0},
        });
        let p = parse_report(&v);
        assert_eq!(p.scenarios.len(), 1);
        assert_eq!(p.scenarios[0].name, "playwright");
        assert_eq!(p.scenarios[0].outcome, Outcome::Failed);
        assert!(
            p.scenarios[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("No tests found")
        );
    }

    #[test]
    fn violations_recorded_between_specs_fail_on_their_own() {
        // The fixtures take what cloudsim recorded before a spec started (the server's
        // startup, its polling since the last spec) and annotate that spec with it.
        let v = serde_json::json!({"suites": [{"title": "a.spec.mjs", "file": "a.spec.mjs",
            "specs": [{"title": "t", "tests": [{"status": "expected", "annotations": [],
                "results": [{"status": "passed", "duration": 5, "errors": [],
                    "annotations": [{"type": "cloudsim-violations-between",
                                     "description": "[\"drive GET /files: bad q\"]"}]}]}]}]}],
            "errors": []});
        let p = parse_report(&v);
        let got: Vec<(&str, Outcome)> = p
            .scenarios
            .iter()
            .map(|s| (s.name.as_str(), s.outcome))
            .collect();
        assert_eq!(
            got,
            [
                ("a.spec.mjs › t", Outcome::Passed),
                ("cloudsim violations before a.spec.mjs › t", Outcome::Failed),
            ]
        );
        assert_eq!(
            p.scenarios[1].detail.as_deref(),
            Some("[\"drive GET /files: bad q\"]")
        );
    }

    #[test]
    fn grep_is_a_substring_and_a_miss_is_no_failure() {
        // As in the cloud suite: a plain, case-sensitive substring (Playwright reads a
        // bare --grep as a case-insensitive regex), and a suite it matches nothing in
        // passes with no scenarios instead of failing on "No tests found".
        assert_eq!(
            grep_args("Vol 1 (ocr)"),
            ["--grep", r"/Vol 1 \(ocr\)/", "--pass-with-no-tests"]
        );
        assert_eq!(
            grep_args(r"/a.b\c/"),
            ["--grep", r"/\/a\.b\\c\//", "--pass-with-no-tests"]
        );
        assert_eq!(
            grep_args("^[x]{1}|y*+?$"),
            [
                "--grep",
                r"/\^\[x\]\{1\}\|y\*\+\?\$/",
                "--pass-with-no-tests"
            ]
        );
    }

    #[test]
    fn a_skip_inside_the_test_reaches_the_results_with_its_reason() {
        // `test.skip(cond, reason)` in the body, as Playwright 1.62 reports it: the
        // annotation on the result (and, from 1.62, on the test too).
        let annotation = serde_json::json!({"type": "skip",
            "description": "pending a reader fix on microsoft: got {\"direct\":4}"});
        let v = serde_json::json!({"suites": [{"title": "a.spec.mjs", "file": "a.spec.mjs",
            "specs": [
                {"title": "late", "tests": [{"status": "skipped", "annotations": [annotation],
                    "results": [{"status": "skipped", "duration": 12, "errors": [],
                                 "annotations": [annotation]}]}]},
                {"title": "older", "tests": [{"status": "skipped", "annotations": [],
                    "results": [{"status": "skipped", "duration": 3, "errors": [],
                                 "annotations": [annotation]}]}]},
            ]}],
            "errors": []});
        let mut r = super::super::results::Report::new("shared", "microsoft", Path::new("x"));
        for s in parse_report(&v).scenarios {
            r.add(s);
        }
        r.finish();
        let v = serde_json::to_value(&r).unwrap();
        let reason = "pending a reader fix on microsoft: got {\"direct\":4}";
        assert_eq!(
            v["skipped"],
            serde_json::json!([
                {"spec": "a.spec.mjs › late", "capability": null, "reason": reason},
                {"spec": "a.spec.mjs › older", "capability": null, "reason": reason},
            ])
        );
        assert_eq!(v["scenarios"][0]["detail"], reason);
    }

    #[test]
    fn a_restore_note_reaches_the_results_whatever_the_outcome() {
        // cloud-owner's `restore-unseen`: evidence of a Bunko that did not list a file,
        // which a passed spec (it cleared) or a failed one (only its errors) dropped.
        let note = serde_json::json!({"type": "restore-unseen",
            "description": "positions.json: Bunko answered 404 for 40 s while the cloud held it, then listed it"});
        let v = serde_json::json!({"suites": [{"title": "a.spec.mjs", "file": "a.spec.mjs",
            "specs": [
                {"title": "passed", "tests": [{"status": "expected", "annotations": [],
                    "results": [{"status": "passed", "duration": 5, "errors": [],
                                 "annotations": [note]}]}]},
                {"title": "failed", "tests": [{"status": "unexpected", "annotations": [],
                    "results": [{"status": "failed", "duration": 5,
                                 "errors": [{"message": "added positions.json"}],
                                 "annotations": [note, {"type": "refused-hosts", "description": ""}]}]}]},
                {"title": "plain", "tests": [{"status": "expected", "annotations": [],
                    "results": [{"status": "passed", "duration": 5, "errors": []}]}]},
            ]}],
            "errors": []});
        let scenarios: Vec<Value> = parse_report(&v)
            .scenarios
            .iter()
            .map(|s| serde_json::to_value(s).unwrap())
            .collect();
        let want = serde_json::json!([format!(
            "restore-unseen: {}",
            note["description"].as_str().unwrap()
        )]);
        assert_eq!(scenarios[0]["notes"], want);
        assert_eq!(scenarios[1]["notes"], want);
        assert_eq!(scenarios[1]["detail"], "added positions.json");
        assert!(scenarios[2].get("notes").is_none(), "{}", scenarios[2]);
    }

    #[test]
    fn a_flaky_test_is_a_failure() {
        let v = serde_json::json!({"suites": [{"title": "a.spec.mjs", "file": "a.spec.mjs",
            "specs": [{"title": "t", "tests": [{"status": "flaky", "annotations": [],
                "results": [{"status": "failed", "duration": 5, "errors": [{"message": "boom"}]},
                            {"status": "passed", "duration": 4, "errors": []}]}]}]}],
            "errors": []});
        let p = parse_report(&v);
        assert_eq!(p.scenarios[0].outcome, Outcome::Failed);
        assert_eq!(p.scenarios[0].ms, 9);
        assert!(p.scenarios[0].detail.as_deref().unwrap().contains("flaky"));
    }

    #[test]
    fn the_pending_caps_file_follows_the_fixtures_rule() {
        assert_eq!(parse_caps("").unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_caps("# header\n\nhistory\n  dedup  # until reader.ref has it\r\n#positions\n")
                .unwrap(),
            ["history", "dedup"]
        );
        let err = parse_caps("edit layers\n").unwrap_err().to_string();
        assert!(err.contains("line 1"), "{err}");
        assert!(parse_caps("History\n").is_err());
        let err = parse_caps("history\nhistory\n").unwrap_err().to_string();
        assert!(err.contains("line 2") && err.contains("twice"), "{err}");
        // The committed file parses (and names nothing while I3 is done).
        let committed = std::fs::read_to_string(
            crate::util::workspace_root().join("interop/pending-reader-caps.txt"),
        )
        .unwrap();
        parse_caps(&committed).unwrap();
    }

    #[test]
    fn the_env_names_every_party_and_clears_the_absent_ones() {
        let e = Env {
            reader_url: "http://127.0.0.1:1".into(),
            bunko_url: Some("http://127.0.0.1:2".into()),
            cloudsim_url: None,
            preauth_url: None,
            rclone_url: Some("http://127.0.0.1:3".into()),
            provider: "rclone".into(),
            accounts: "/w/accounts.json".into(),
            workdir: "/w".into(),
            fakeproc_hold: Some("/w/processor/release".into()),
            context: serde_json::json!({"sim_account": "bunko"}),
            control_url: Some("http://127.0.0.1:4".into()),
        };
        let vars = e.vars();
        let get = |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("READER_URL"), Some("http://127.0.0.1:1"));
        assert_eq!(get("BUNKO_URL"), Some("http://127.0.0.1:2"));
        assert_eq!(get("RCLONE_URL"), Some("http://127.0.0.1:3"));
        assert_eq!(get("CLOUDSIM_URL"), None);
        assert_eq!(get("CLOUDSIM_PREAUTH_URL"), None);
        assert_eq!(get("RIG_PROVIDER"), Some("rclone"));
        assert_eq!(get("RIG_ACCOUNTS"), Some("/w/accounts.json"));
        assert_eq!(get("RIG_WORKDIR"), Some("/w"));
        assert_eq!(get("RIG_FAKEPROC_HOLD"), Some("/w/processor/release"));
        assert_eq!(get("RIG_CONTEXT"), Some(r#"{"sim_account":"bunko"}"#));
        assert_eq!(get("RIG_CONTROL_URL"), Some("http://127.0.0.1:4"));
        for (name, _) in &vars {
            assert!(VARS.contains(name), "{name} is not cleared first");
        }
    }

    #[test]
    fn a_suite_folder_lists_only_its_specs() {
        let d = tempfile::tempdir().unwrap();
        assert!(spec_files(&d.path().join("missing")).is_empty());
        std::fs::create_dir_all(d.path().join("sub")).unwrap();
        for f in [
            "b.spec.mjs",
            "a.spec.mjs",
            "helper.mjs",
            "sub/c.spec.mjs",
            "x.test.mjs",
        ] {
            std::fs::write(d.path().join(f), "").unwrap();
        }
        let names: Vec<String> = spec_files(d.path())
            .iter()
            .map(|p| {
                let rel = p.strip_prefix(d.path()).unwrap();
                let parts: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                parts.join("/")
            })
            .collect();
        assert_eq!(names, ["a.spec.mjs", "b.spec.mjs", "sub/c.spec.mjs"]);
    }
}
