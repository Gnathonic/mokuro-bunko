//! The sandbox rules of a live run (I5 Tasks 3 and 6), apart from any provider so they
//! are tested without one: the sandbox's name, what a path inside it may be, the
//! snapshot of the account taken before the run and compared after it, and how a run's
//! outcome reads with the one before it.

use anyhow::{Result, bail};
use std::collections::BTreeMap;

/// Every live run's sandbox folder is `bunko-interop-<run id>`, at the account's top
/// level: never `mokuro-reader`, never inside it.
pub const SANDBOX_PREFIX: &str = "bunko-interop-";
/// The reader's library folder: the snapshot lists its direct children, nothing more.
pub const READER_FOLDER: &str = "mokuro-reader";
/// A sandbox older than this is a crashed run's, and is removed before a run starts.
pub const STALE_AFTER_MS: i64 = 48 * 3600 * 1000;

/// A run id: letters, digits and `-` only, 1 to 64 of them.
pub fn check_run_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        bail!("run id {id:?}: use 1 to 64 letters, digits and '-'");
    }
    Ok(())
}

/// `<UTC yyyymmddThhmmss>-<suffix>` (the workflow's run id, or `local`).
pub fn run_id(now_unix_s: i64, suffix: &str) -> String {
    let days = now_unix_s.div_euclid(86_400);
    let secs = now_unix_s.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}t{:02}{:02}{:02}-{suffix}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// Howard Hinnant's days-to-civil (proleptic Gregorian).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn sandbox_name(run_id: &str) -> Result<String> {
    check_run_id(run_id)?;
    let name = format!("{SANDBOX_PREFIX}{}", run_id.to_ascii_lowercase());
    check_sandbox_name(&name)?;
    Ok(name)
}

/// The only folder names a live run may use as Bunko's library folder.
pub fn check_sandbox_name(name: &str) -> Result<()> {
    let lower = name.to_ascii_lowercase();
    let Some(id) = lower.strip_prefix(SANDBOX_PREFIX) else {
        bail!("{name:?} is not a sandbox folder (bunko-interop-<run id>)");
    };
    check_run_id(id)?;
    if lower.contains(READER_FOLDER) {
        bail!("{name:?} names the reader's library folder");
    }
    Ok(())
}

/// The segments of `rel`, a path inside the sandbox: no empty, `.` or `..` segment, no
/// leading `/`, no backslash.
pub fn sandbox_segments(rel: &str) -> Result<Vec<&str>> {
    if rel.starts_with('/') || rel.contains('\\') {
        bail!("{rel:?} is not a path inside the sandbox");
    }
    let segs: Vec<&str> = rel.split('/').collect();
    if segs.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        bail!("{rel:?} is not a path inside the sandbox");
    }
    Ok(segs)
}

/// One entry of the snapshot: what the provider lists of it (no content).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Entry {
    /// `name` at the top level, `mokuro-reader/name` below the reader's folder.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub version: String,
    pub cver: String,
    pub hash: Option<String>,
}

/// The account as the run found it, keyed by the provider's id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot(pub BTreeMap<String, Entry>);

/// One difference between two snapshots, by path only.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "change", rename_all = "lowercase")]
pub enum Change {
    Added { path: String },
    Removed { path: String },
    Changed { path: String, what: Vec<String> },
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Change::Added { path } => write!(f, "added: {path}"),
            Change::Removed { path } => write!(f, "removed: {path}"),
            Change::Changed { path, what } => write!(f, "changed: {path} ({})", what.join(", ")),
        }
    }
}

impl Snapshot {
    pub fn insert(&mut self, id: impl Into<String>, e: Entry) {
        self.0.insert(id.into(), e);
    }

    /// What differs in `after`: entries added, removed, renamed, rewritten.
    pub fn diff(&self, after: &Snapshot) -> Vec<Change> {
        let mut out = Vec::new();
        for (id, b) in &self.0 {
            match after.0.get(id) {
                None => out.push(Change::Removed {
                    path: b.path.clone(),
                }),
                Some(a) if a != b => {
                    let mut what = Vec::new();
                    if a.path != b.path {
                        what.push(format!("now {}", a.path));
                    }
                    if a.is_dir != b.is_dir {
                        what.push("kind".into());
                    }
                    if a.size != b.size {
                        what.push(format!("size {} -> {}", b.size, a.size));
                    }
                    if a.cver != b.cver || a.hash != b.hash {
                        what.push("content".into());
                    }
                    if a.version != b.version && what.is_empty() {
                        what.push("metadata".into());
                    }
                    out.push(Change::Changed {
                        path: b.path.clone(),
                        what,
                    });
                }
                Some(_) => {}
            }
        }
        for (id, a) in &after.0 {
            if !self.0.contains_key(id) {
                out.push(Change::Added {
                    path: a.path.clone(),
                });
            }
        }
        out
    }
}

/// How a live run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunOutcome {
    Passed,
    Failed,
    /// MEGA's transfer quota ran out: a warning, unless the run before was skipped too.
    Skipped,
}

/// This run's outcome given the one before it (`live-<provider>.json` of the last run,
/// when the workflow found one): a second skip in a row fails (spec §7.3).
pub fn with_previous(
    this: RunOutcome,
    previous: Option<RunOutcome>,
) -> (RunOutcome, Option<String>) {
    match (this, previous) {
        (RunOutcome::Skipped, Some(RunOutcome::Skipped)) => (
            RunOutcome::Failed,
            Some("skipped for the transfer quota two runs in a row".into()),
        ),
        (o, _) => (o, None),
    }
}

/// The text GitHub Actions masks in every later log line (`::add-mask::`), for each
/// value; nothing outside Actions.
pub fn mask_lines(values: &[&str], in_actions: bool) -> String {
    if !in_actions {
        return String::new();
    }
    values
        .iter()
        .filter(|v| v.len() >= 4)
        .map(|v| format!("::add-mask::{v}\n"))
        .collect()
}

/// Every secret a run has seen, and what it is replaced with wherever the run writes
/// text: the result file, the uploaded artifact, stdout and stderr. GitHub masks
/// secrets in its logs only, never in files.
#[derive(Debug, Default)]
pub struct Redactor {
    /// Longest first, so a secret that contains another is replaced whole.
    values: std::sync::Mutex<Vec<String>>,
}

/// Values shorter than this are no credential and would match everywhere.
const MIN_SECRET: usize = 6;

/// `s` percent-encoded as a URL query value would carry it (RFC 3986 unreserved kept).
fn percent_encoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl Redactor {
    /// Remember `value` and the forms it travels in (percent-encoded, as in a URL);
    /// answers the forms that were new, for the caller to mask in Actions.
    pub fn add(&self, value: &str) -> Vec<String> {
        let value = value.trim();
        if value.len() < MIN_SECRET {
            return Vec::new();
        }
        let mut forms = vec![value.to_string()];
        let enc = percent_encoded(value);
        if enc != value {
            forms.push(enc);
        }
        let mut v = self.values.lock().expect("redactor lock");
        let new: Vec<String> = forms.into_iter().filter(|f| !v.contains(f)).collect();
        v.extend(new.iter().cloned());
        v.sort_by_key(|s| std::cmp::Reverse(s.len()));
        new
    }

    /// A MEGA session blob and its parts: the session id travels on its own (`sid=`
    /// in every API URL), in the URL-safe and the standard base64 alphabets.
    pub fn add_mega_session(&self, blob: &str) -> Vec<String> {
        use base64::Engine as _;
        use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
        let mut new = self.add(blob);
        let raw = URL_SAFE_NO_PAD
            .decode(blob.trim().trim_end_matches('='))
            .or_else(|_| STANDARD_NO_PAD.decode(blob.trim().trim_end_matches('=')));
        if let Ok(raw) = raw
            && raw.len() > 17
        {
            for part in [&raw[1..17], &raw[17..]] {
                new.extend(self.add(&URL_SAFE_NO_PAD.encode(part)));
                new.extend(self.add(&STANDARD_NO_PAD.encode(part)));
            }
        }
        new
    }

    /// `text` with every known secret replaced by `<redacted>`.
    pub fn redact(&self, text: &str) -> String {
        let v = self.values.lock().expect("redactor lock");
        let mut s = text.to_string();
        for secret in v.iter() {
            if s.contains(secret.as_str()) {
                s = s.replace(secret.as_str(), "<redacted>");
            }
        }
        s
    }

    /// Every string in `v`, redacted.
    pub fn redact_json(&self, v: &serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        match v {
            Value::String(s) => Value::String(self.redact(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.redact_json(x)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| (self.redact(k), self.redact_json(x)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// What a result file may say of an error or a scenario's detail: its first line, at
/// most `max` characters, so no log tail (a server's, a response body's) is carried.
pub fn excerpt(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim_end();
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max || text.lines().nth(1).is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(path: &str, version: &str) -> Entry {
        Entry {
            path: path.into(),
            is_dir: false,
            size: 3,
            version: version.into(),
            cver: "c1".into(),
            hash: Some("h".into()),
        }
    }

    #[test]
    fn run_ids_and_sandbox_names() {
        // 2026-11-15T21:04:05Z
        assert_eq!(run_id(1_794_776_645, "local"), "20261115t210405-local");
        assert_eq!(run_id(0, "1"), "19700101t000000-1");
        let n = sandbox_name("20261115t210405-123").unwrap();
        assert_eq!(n, "bunko-interop-20261115t210405-123");
        check_sandbox_name(&n).unwrap();
        for bad in [
            "mokuro-reader",
            "Mokuro-Reader",
            "MOKURO-READER",
            "bunko-interop-",
            "bunko-interop",
            "bunko-interop-x/y",
            "bunko-interop-../mokuro-reader",
            "bunko-interop-mokuro-reader",
            "x-bunko-interop-1",
            "",
        ] {
            assert!(check_sandbox_name(bad).is_err(), "{bad:?}");
        }
        assert!(sandbox_name("a b").is_err());
        assert!(sandbox_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn sandbox_paths_stay_inside() {
        assert_eq!(
            sandbox_segments("Series/Vol 1.cbz").unwrap(),
            ["Series", "Vol 1.cbz"]
        );
        for bad in ["", "/x", "a//b", "../x", "a/../../b", "./a", "a\\b", "a/"] {
            assert!(sandbox_segments(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_diff_names_what_changed_and_nothing_else() {
        let mut before = Snapshot::default();
        before.insert("1", e("mokuro-reader", "v1"));
        before.insert("2", e("mokuro-reader/volume-data.json", "v1"));
        before.insert("3", e("notes.txt", "v1"));
        assert!(before.diff(&before.clone()).is_empty());

        let mut after = before.clone();
        after.0.remove("3");
        after.insert("4", e("bunko-interop-leak.txt", "v1"));
        let mut edited = e("mokuro-reader/volume-data.json", "v2");
        edited.cver = "c2".into();
        edited.size = 9;
        after.insert("2", edited);
        let mut touched = e("mokuro-reader", "v2");
        touched.path = "mokuro-reader".into();
        after.insert("1", touched);
        let d: Vec<String> = before.diff(&after).iter().map(|c| c.to_string()).collect();
        assert_eq!(
            d,
            [
                "changed: mokuro-reader (metadata)",
                "changed: mokuro-reader/volume-data.json (size 3 -> 9, content)",
                "removed: notes.txt",
                "added: bunko-interop-leak.txt",
            ]
        );
    }

    #[test]
    fn two_skips_in_a_row_fail() {
        use RunOutcome::*;
        assert_eq!(with_previous(Skipped, None).0, Skipped);
        assert_eq!(with_previous(Skipped, Some(Passed)).0, Skipped);
        assert_eq!(with_previous(Skipped, Some(Failed)).0, Skipped);
        let (o, why) = with_previous(Skipped, Some(Skipped));
        assert_eq!(o, Failed);
        assert!(why.unwrap().contains("two runs in a row"));
        assert_eq!(with_previous(Passed, Some(Skipped)).0, Passed);
        assert_eq!(with_previous(Failed, Some(Skipped)).0, Failed);
    }

    #[test]
    fn the_redactor_replaces_every_form_of_every_secret() {
        use base64::Engine as _;
        let r = Redactor::default();
        assert!(r.add("tiny").is_empty(), "too short to be a credential");
        let new = r.add("1//0g a+b/c");
        assert_eq!(new, ["1//0g a+b/c", "1%2F%2F0g%20a%2Bb%2Fc"]);
        assert!(r.add("1//0g a+b/c").is_empty(), "known already");
        assert_eq!(
            r.redact("x=1%2F%2F0g%20a%2Bb%2Fc&y=1//0g a+b/c"),
            "x=<redacted>&y=<redacted>"
        );
        // A MEGA blob: kind 1, wrapped master key, session id.
        let mut raw = vec![1u8];
        raw.extend([9u8; 16]);
        raw.extend(b"SESSION-ID-BYTES-0123456789abcdef");
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let blob = b64.encode(&raw);
        r.add_mega_session(&blob);
        let sid = b64.encode(&raw[17..]);
        assert_eq!(
            r.redact(&format!("cs?id=1&sid={sid}")),
            "cs?id=1&sid=<redacted>"
        );
        assert_eq!(r.redact(&blob), "<redacted>");
        let json = serde_json::json!({"a": [format!("t {sid}")], "b": 1});
        assert_eq!(
            r.redact_json(&json),
            serde_json::json!({"a": ["t <redacted>"], "b": 1})
        );
    }

    #[test]
    fn excerpts_keep_one_bounded_line() {
        assert_eq!(excerpt("short", 10), "short");
        assert_eq!(excerpt("first\nlog tail", 10), "first…");
        assert_eq!(excerpt("0123456789abc", 10), "0123456789…");
        assert_eq!(excerpt("", 10), "");
    }

    #[test]
    fn masks_only_in_actions() {
        assert_eq!(mask_lines(&["secret-value"], false), "");
        assert_eq!(
            mask_lines(&["secret-value", "ab"], true),
            "::add-mask::secret-value\n"
        );
    }
}
