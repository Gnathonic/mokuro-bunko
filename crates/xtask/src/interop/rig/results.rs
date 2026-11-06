//! `target/interop/rig/<suite>-<provider>.json`, and the logs of every party beside it.
//! Its contract fields (C6-contract "Rig results artifact"): `suite`, `provider`,
//! `bunko_sha`, `reader_sha`, `passed`, `failed`, `skipped: [{spec, capability, reason}]`
//! and `duration_ms`; the rest is detail for whoever reads a failure (and a scenario's
//! `notes`, what a spec annotated for triage whatever its outcome).

use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub struct Scenario {
    pub name: String,
    pub outcome: Outcome,
    pub ms: u64,
    /// Why it failed or was skipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Skipped because the reader lacks this capability and
    /// `interop/pending-reader-caps.txt` lists it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    /// What a spec noted for whoever triages, whatever its outcome (`<type>: <words>`,
    /// from the annotation types playwright.rs keeps).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// One skipped scenario (spec), as the contract lists them.
#[derive(Debug, Clone, Serialize)]
pub struct Skip {
    pub spec: String,
    pub capability: Option<String>,
    /// The skip's own words: a capability skip's, or those of a `test.skip(cond,
    /// reason)` inside the test, which names no capability.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub suite: String,
    pub provider: String,
    /// `HEAD` of this checkout (`-dirty` with uncommitted changes).
    pub bunko_sha: Option<String>,
    /// The reader the browser suites ran; `None` for the `cloud` suite.
    pub reader_sha: Option<String>,
    pub passed: usize,
    pub failed: usize,
    pub skipped: Vec<Skip>,
    pub duration_ms: u64,
    pub outcome: Outcome,
    /// `interop/pending-reader-caps.txt` as the run read it: I3 is done only when it
    /// is empty.
    pub pending_caps: Vec<String>,
    /// Hosts the browser was refused (every request off this machine is): what the
    /// reader tried to reach, never what it reached. Each fails its spec.
    pub refused_hosts: Vec<String>,
    /// The Google scripts src/app.html loads, answered with an empty script: the only
    /// requests off this machine a spec may make.
    pub stubbed_urls: Vec<String>,
    pub bunko_bin: String,
    pub started_at: String,
    pub scenarios: Vec<Scenario>,
    /// The parties' logs, copied next to this file.
    pub logs: Vec<String>,
    #[serde(skip)]
    started: Option<Instant>,
}

impl Report {
    pub fn new(suite: &str, provider: &str, bin: &Path) -> Report {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Report {
            suite: suite.into(),
            provider: provider.into(),
            bunko_sha: None,
            reader_sha: None,
            passed: 0,
            failed: 0,
            skipped: Vec::new(),
            duration_ms: 0,
            outcome: Outcome::Passed,
            pending_caps: Vec::new(),
            refused_hosts: Vec::new(),
            stubbed_urls: Vec::new(),
            bunko_bin: bin.display().to_string(),
            started_at: crate::util::iso_utc(now),
            scenarios: Vec::new(),
            logs: Vec::new(),
            started: Some(Instant::now()),
        }
    }

    pub fn add(&mut self, s: Scenario) {
        self.scenarios.push(s);
    }

    /// Failed when any scenario failed, skipped when every one was skipped.
    pub fn finish(&mut self) {
        self.duration_ms = self.started.map_or(0, |t| t.elapsed().as_millis() as u64);
        let count = |o: Outcome| self.scenarios.iter().filter(|s| s.outcome == o).count();
        self.passed = count(Outcome::Passed);
        self.failed = count(Outcome::Failed);
        self.skipped = self
            .scenarios
            .iter()
            .filter(|s| s.outcome == Outcome::Skipped)
            .map(|s| Skip {
                spec: s.name.clone(),
                capability: s.capability.clone(),
                reason: s.detail.clone(),
            })
            .collect();
        self.outcome = if self.scenarios.iter().any(|s| s.outcome == Outcome::Failed) {
            Outcome::Failed
        } else if !self.scenarios.is_empty()
            && self.scenarios.iter().all(|s| s.outcome == Outcome::Skipped)
        {
            Outcome::Skipped
        } else {
            Outcome::Passed
        };
    }

    pub fn name(&self) -> String {
        format!("{}-{}", self.suite, self.provider)
    }

    /// Copy every `*.log` under `work` into `out/logs/<suite>-<provider>/` (named by
    /// their path below `work`), then write `out/<suite>-<provider>.json`.
    pub fn write(&mut self, out: &Path, work: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(out)?;
        let logs = out.join("logs").join(self.name());
        let _ = std::fs::remove_dir_all(&logs);
        for log in find_logs(work) {
            let rel = log.strip_prefix(work).unwrap_or(&log);
            let flat = rel.to_string_lossy().replace(['/', '\\'], "_");
            std::fs::create_dir_all(&logs)?;
            let dest = logs.join(&flat);
            if std::fs::copy(&log, &dest).is_ok() {
                self.logs.push(format!("logs/{}/{flat}", self.name()));
            }
        }
        self.logs.sort();
        let path = out.join(format!("{}.json", self.name()));
        std::fs::write(&path, serde_json::to_vec_pretty(self)?)?;
        Ok(path)
    }
}

fn find_logs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() && p.extension().is_some_and(|x| x == "log") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario(name: &str, outcome: Outcome) -> Scenario {
        Scenario {
            name: name.into(),
            outcome,
            ms: 1,
            detail: None,
            capability: None,
            notes: Vec::new(),
        }
    }

    #[test]
    fn the_report_carries_the_contract_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = Report::new("dav", "local", Path::new("/bin/x"));
        r.bunko_sha = Some("b".repeat(40));
        r.reader_sha = Some(format!("{}-dirty", "a".repeat(40)));
        r.pending_caps = vec!["history".into()];
        r.add(scenario(
            "smoke.spec.mjs: connect and list",
            Outcome::Passed,
        ));
        r.add(scenario("x.spec.mjs: plain skip", Outcome::Skipped));
        r.add(Scenario {
            detail: Some("pending reader capability: history".into()),
            capability: Some("history".into()),
            ..scenario("history.spec.mjs: two accounts", Outcome::Skipped)
        });
        // `test.skip(cond, reason)` inside the test: no capability, its reason only.
        r.add(Scenario {
            detail: Some("pending a reader fix on microsoft: got {}".into()),
            ..scenario("owner.spec.mjs: mid-test", Outcome::Skipped)
        });
        r.add(scenario("y.spec.mjs: broken", Outcome::Failed));
        r.finish();
        let path = r
            .write(&dir.path().join("rig"), &dir.path().join("work"))
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["suite"], "dav");
        assert_eq!(v["provider"], "local");
        assert_eq!(v["bunko_sha"], "b".repeat(40));
        assert_eq!(v["reader_sha"], format!("{}-dirty", "a".repeat(40)));
        assert_eq!(v["passed"], 1);
        assert_eq!(v["failed"], 1);
        assert_eq!(
            v["skipped"],
            serde_json::json!([
                {"spec": "x.spec.mjs: plain skip", "capability": null, "reason": null},
                {
                    "spec": "history.spec.mjs: two accounts",
                    "capability": "history",
                    "reason": "pending reader capability: history",
                },
                {
                    "spec": "owner.spec.mjs: mid-test",
                    "capability": null,
                    "reason": "pending a reader fix on microsoft: got {}",
                },
            ])
        );
        assert!(v["duration_ms"].is_u64());
        assert_eq!(v["pending_caps"], serde_json::json!(["history"]));
        assert_eq!(v["outcome"], "failed");
        // The cloud suite has no reader.
        let mut r = Report::new("cloud", "google", Path::new("/bin/x"));
        r.finish();
        let path = r
            .write(&dir.path().join("rig"), &dir.path().join("work"))
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["reader_sha"], serde_json::Value::Null);
        assert_eq!(v["skipped"], serde_json::json!([]));
    }

    #[test]
    fn a_report_is_written_with_the_logs_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(work.join("a")).unwrap();
        std::fs::write(work.join("a/serve.log"), "server").unwrap();
        std::fs::write(work.join("cloudsim.log"), "sim").unwrap();
        std::fs::write(work.join("a/config.yaml"), "not a log").unwrap();
        let mut r = Report::new("cloud", "google", Path::new("/bin/x"));
        r.add(scenario("cold-start", Outcome::Passed));
        r.add(scenario("quota-pause", Outcome::Skipped));
        r.finish();
        assert_eq!(r.outcome, Outcome::Passed);
        let out = dir.path().join("rig");
        let path = r.write(&out, &work).unwrap();
        assert_eq!(path, out.join("cloud-google.json"));
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["suite"], "cloud");
        assert_eq!(v["outcome"], "passed");
        assert_eq!(v["scenarios"][1]["outcome"], "skipped");
        assert_eq!(
            v["logs"],
            serde_json::json!([
                "logs/cloud-google/a_serve.log",
                "logs/cloud-google/cloudsim.log"
            ])
        );
        assert_eq!(
            std::fs::read_to_string(out.join("logs/cloud-google/a_serve.log")).unwrap(),
            "server"
        );
    }

    #[test]
    fn one_failed_scenario_fails_the_report() {
        let mut r = Report::new("cloud", "mega", Path::new("x"));
        r.add(scenario("a", Outcome::Passed));
        r.add(scenario("b", Outcome::Failed));
        r.finish();
        assert_eq!(r.outcome, Outcome::Failed);
        let mut r = Report::new("cloud", "rclone", Path::new("x"));
        r.add(scenario("a", Outcome::Skipped));
        r.finish();
        assert_eq!(r.outcome, Outcome::Skipped);
    }
}
