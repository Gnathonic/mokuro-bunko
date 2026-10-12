//! Automatic updates (opt-in: the server's `update.auto`, a processor's `auto_update`):
//! the pieces the server and the processor share. Docs: docs/configuration.md
//! "Automatic updates", docs/rust-port/PROTOCOL.md (the processor trigger).
//!
//! * [`release_manifest_url`]: the signed `release.json` of one exact version, derived
//!   from the configured (latest) manifest URL, for GitHub, a fork and a mirror alike.
//! * [`Marker`]: what an update leaves in the storage for the next start
//!   (`.update.json`), so the new process can say "Updated to X" and an update that did
//!   not take is never tried again in a loop.
//! * [`Retry`]: backoff after a failed attempt (10 min doubling to 12 h).
//! * [`processor_action`]: what a processor does about the library's version.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Installs a whole release (its binary, its backend pack for the installed variant, its
/// models) and switches to it; the caller restarts on `Ok`. The server and a processor
/// get the real one from `mokuro-bunko`, tests a fake.
pub trait ReleaseInstaller: Send + Sync {
    /// Download, verify and pre-fetch release `version`, then switch the binary and the
    /// pack together. Nothing is switched unless everything was fetched and the new
    /// pack loaded in the new binary. Ok: the version now installed (restart into it).
    fn install(
        &self,
        version: String,
    ) -> futures_util::future::BoxFuture<'static, Result<String, InstallFailure>>;
}

/// Why an automatic install did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallFailure {
    pub message: String,
    /// Retrying will not help: the owner must act (`action` says how).
    pub needs_owner: bool,
    pub action: Option<String>,
}

impl InstallFailure {
    pub fn retry(message: impl Into<String>) -> InstallFailure {
        InstallFailure {
            message: message.into(),
            needs_owner: false,
            action: None,
        }
    }

    pub fn owner(message: impl Into<String>, action: impl Into<String>) -> InstallFailure {
        InstallFailure {
            message: message.into(),
            needs_owner: true,
            action: Some(action.into()),
        }
    }
}

impl std::fmt::Display for InstallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// `<storage>/.update-blocked.json`: a version that was installed and rolled back (or
/// refused by the owner's machine): automatic updates skip it until a newer one appears
/// or someone installs it by hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocked {
    pub version: String,
    pub reason: String,
    pub at: String,
}

pub const BLOCKED_FILE: &str = ".update-blocked.json";

impl Blocked {
    pub fn read(storage: &Path) -> Option<Blocked> {
        serde_json::from_str(&std::fs::read_to_string(storage.join(BLOCKED_FILE)).ok()?).ok()
    }

    pub fn write(&self, storage: &Path) -> std::io::Result<()> {
        std::fs::write(
            storage.join(BLOCKED_FILE),
            serde_json::to_string_pretty(self).unwrap_or_default(),
        )
    }

    pub fn clear(storage: &Path) {
        let _ = std::fs::remove_file(storage.join(BLOCKED_FILE));
    }

    /// Whether `version` is the blocked one.
    pub fn blocks(&self, version: &str) -> bool {
        self.version.trim_start_matches('v') == version.trim_start_matches('v')
    }
}

/// File name of the [`Marker`] under an instance's storage.
pub const MARKER_FILE: &str = ".update.json";

/// The `release.json` of `version`, given the configured manifest URL (which names the
/// latest release):
///
/// * GitHub `https://github.com/<repo>/releases/latest/download/<file>` (or a
///   `releases/download/<tag>/<file>` URL) → `…/releases/download/v<version>/<file>`;
/// * any other URL with a `latest` path segment → that segment replaced by
///   `v<version>` (`https://mirror/bunko/latest/release.json`);
/// * a URL containing `{version}` → the placeholder replaced (`v` not added);
/// * otherwise a mirror laid out as `<base>/release.json` (latest) beside
///   `<base>/v<version>/release.json`.
///
/// Plain paths and `file://` URLs follow the same rules (tests, air-gapped mirrors).
pub fn release_manifest_url(manifest_url: &str, version: &str) -> String {
    let version = version.trim().trim_start_matches('v');
    let tag = format!("v{version}");
    if manifest_url.contains("{version}") {
        return manifest_url.replace("{version}", version);
    }
    if let Some((head, file)) = manifest_url.split_once("/releases/latest/download/") {
        return format!("{head}/releases/download/{tag}/{file}");
    }
    if let Some((head, rest)) = manifest_url.split_once("/releases/download/")
        && let Some((_old, file)) = rest.split_once('/')
    {
        return format!("{head}/releases/download/{tag}/{file}");
    }
    // Keep a query string out of the path surgery.
    let (path, query) = match manifest_url.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (manifest_url, None),
    };
    let segments: Vec<&str> = path.split('/').collect();
    let rebuilt = if let Some(i) = segments.iter().rposition(|s| *s == "latest") {
        let mut s: Vec<String> = segments.iter().map(|s| s.to_string()).collect();
        s[i] = tag;
        s.join("/")
    } else {
        match path.rsplit_once('/') {
            Some((dir, file)) => format!("{dir}/{tag}/{file}"),
            None => format!("{tag}/{path}"),
        }
    };
    match query {
        Some(q) => format!("{rebuilt}?{q}"),
        None => rebuilt,
    }
}

/// What a processor does about the library's version (the registration's
/// `version_mismatch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessorAction {
    /// Same version (or the library said nothing).
    Nothing,
    /// `auto_update` is on and the library is newer: drain, then install `version`.
    Update(String),
    /// The library is newer but `auto_update` is off: report only.
    ReportNewer(String),
    /// The library is older: a processor never downgrades.
    ReportOlder(String),
    /// A version that does not compare: report only.
    ReportUnknown(String),
}

pub fn processor_action(
    mismatch: Option<&bunko_proto::VersionMismatch>,
    auto_update: bool,
) -> ProcessorAction {
    let Some(m) = mismatch else {
        return ProcessorAction::Nothing;
    };
    let v = m.library_version.trim().trim_start_matches('v').to_string();
    match m.relation.as_str() {
        bunko_proto::VersionMismatch::LIBRARY_NEWER if auto_update => ProcessorAction::Update(v),
        bunko_proto::VersionMismatch::LIBRARY_NEWER => ProcessorAction::ReportNewer(v),
        bunko_proto::VersionMismatch::LIBRARY_OLDER => ProcessorAction::ReportOlder(v),
        _ => ProcessorAction::ReportUnknown(v),
    }
}

/// `true` when `candidate` is strictly newer than `current` (semver precedence). An
/// unparsable version is never newer: nothing is installed on a guess.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| semver::Version::parse(v.trim().trim_start_matches('v')).ok();
    match (parse(candidate), parse(current)) {
        (Some(c), Some(cur)) => c.cmp_precedence(&cur).is_gt(),
        _ => false,
    }
}

/// `<storage>/.update.json`: written just before an update restarts the process, read
/// (and removed) by the next start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// `binary`: a new release (binary + its pack) was switched to; `rollback`: the
    /// release `from` was undone and `to` (the one before) restored.
    pub kind: String,
    pub from: String,
    pub to: String,
    /// RFC 3339, UTC.
    pub at: String,
    /// The backend pack switched to (kind `pack`, or a binary update that also
    /// replaced the pack): the directory name, for the post-restart load check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack: Option<String>,
    /// The pack directory it replaced, kept as `.prev-<name>` until the new one loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_pack: Option<String>,
    /// The backends directory both live in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_root: Option<PathBuf>,
    /// Kind `rollback`: why the update to `from` was undone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Marker {
    pub fn path(storage: &Path) -> PathBuf {
        storage.join(MARKER_FILE)
    }

    pub fn write(&self, storage: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        std::fs::write(Self::path(storage), text)
    }

    /// The marker left by the previous run, removed as it is read.
    pub fn take(storage: &Path) -> Option<Marker> {
        let path = Self::path(storage);
        let text = std::fs::read_to_string(&path).ok()?;
        let _ = std::fs::remove_file(&path);
        serde_json::from_str(&text).ok()
    }

    /// Whether this process is what the update meant to start.
    pub fn took(&self, running: &str) -> bool {
        self.to.trim_start_matches('v') == running.trim_start_matches('v')
    }
}

/// When to try again after a failed automatic update: 10 min, doubling to 12 h
/// (`MOKURO_UPDATE_RETRY_SECONDS` sets the first wait, for tests and impatient
/// owners). A version that failed is not tried again before its wait is over, whatever
/// triggers it (a reconnect, a new check), so a broken release never loops.
#[derive(Debug, Clone, Default)]
pub struct Retry {
    /// The first wait (None: [`retry_first`]).
    first: Option<Duration>,
    failures: u32,
    not_before: Option<Instant>,
    version: Option<String>,
    last_error: Option<String>,
}

pub const RETRY_FIRST: Duration = Duration::from_secs(600);
pub const RETRY_MAX: Duration = Duration::from_secs(12 * 3600);
/// After this many failures in a row the owner is told (a `fail` problem).
pub const RETRY_TELL_AFTER: u32 = 3;

pub fn retry_first() -> Duration {
    std::env::var("MOKURO_UPDATE_RETRY_SECONDS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(RETRY_FIRST)
}

impl Retry {
    pub fn with_first(first: Duration) -> Retry {
        Retry {
            first: Some(first),
            ..Retry::default()
        }
    }

    /// The wait after failure number `n` (1-based).
    pub fn wait_after(n: u32, first: Duration) -> Duration {
        let factor = 1u32 << n.saturating_sub(1).min(16);
        first.saturating_mul(factor).min(RETRY_MAX.max(first))
    }

    /// May `version` be tried now?
    pub fn may_try(&self, version: &str, now: Instant) -> bool {
        match (&self.version, self.not_before) {
            (Some(v), Some(t)) if v == version => now >= t,
            _ => true,
        }
    }

    /// An attempt at `version` failed; returns the wait before the next one.
    pub fn failed(&mut self, version: &str, error: &str, now: Instant) -> Duration {
        if self.version.as_deref() != Some(version) {
            self.failures = 0;
        }
        self.failures += 1;
        let wait = Self::wait_after(self.failures, self.first.unwrap_or_else(retry_first));
        self.version = Some(version.to_string());
        self.not_before = Some(now + wait);
        self.last_error = Some(error.to_string());
        wait
    }

    pub fn succeeded(&mut self) {
        *self = Retry {
            first: self.first,
            ..Retry::default()
        };
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn next_try(&self) -> Option<Instant> {
        self.not_before
    }

    /// Enough failures in a row that the owner should hear about it.
    pub fn keeps_failing(&self) -> bool {
        self.failures >= RETRY_TELL_AFTER
    }
}

/// The ed25519 key releases are checked against: `configured` (a fork's or a test
/// release's own key, from the config FILE only — never the environment or the admin
/// API), else the key compiled into this build. The bool says it is not the compiled
/// key, which the caller logs loudly.
pub fn release_key(configured: &str) -> (String, bool) {
    let k = configured.trim();
    if k.is_empty() || k == crate::RELEASE_PUBLIC_KEY {
        (crate::RELEASE_PUBLIC_KEY.to_string(), false)
    } else {
        (k.to_string(), true)
    }
}

/// The warning every process logs once when it trusts a key other than the compiled one.
pub fn custom_key_warning(key: &str, origin: &str) -> String {
    format!(
        "UPDATES ARE TRUSTED FROM A NON-DEFAULT SIGNING KEY {key} (set by {origin}): anything signed with it can replace this program. Remove the setting unless this is your own fork or a test release."
    )
}

/// Free bytes where `dir` (or its nearest existing parent) lives.
pub fn free_space(dir: &Path) -> Option<u64> {
    let mut d = dir;
    loop {
        if d.exists() {
            return fs4::available_space(d).ok();
        }
        d = d.parent()?;
    }
}

/// `need` bytes must fit in `dir` with a 500 MB margin; Err says how much is missing.
pub fn check_space(dir: &Path, need: u64) -> Result<(), String> {
    const MARGIN: u64 = 500 * 1000 * 1000;
    let Some(free) = free_space(dir) else {
        return Ok(());
    };
    if free >= need.saturating_add(MARGIN) {
        return Ok(());
    }
    Err(format!(
        "not enough disk space in {}: {:.1} GB free, {:.1} GB needed",
        dir.display(),
        free as f64 / 1e9,
        (need + MARGIN) as f64 / 1e9
    ))
}

pub fn now_rfc3339() -> String {
    crate::now_iso()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bunko_proto::VersionMismatch;

    #[test]
    fn exact_release_urls() {
        let gh = "https://github.com/Gnathonic/mokuro-bunko/releases/latest/download/release.json";
        assert_eq!(
            release_manifest_url(gh, "0.7.1"),
            "https://github.com/Gnathonic/mokuro-bunko/releases/download/v0.7.1/release.json"
        );
        assert_eq!(
            release_manifest_url(gh, "v0.7.1"),
            "https://github.com/Gnathonic/mokuro-bunko/releases/download/v0.7.1/release.json"
        );
        // A fork, pinned to a tag.
        assert_eq!(
            release_manifest_url(
                "https://github.com/me/fork/releases/download/v0.7.0/release.json",
                "0.7.2"
            ),
            "https://github.com/me/fork/releases/download/v0.7.2/release.json"
        );
        // Mirrors.
        assert_eq!(
            release_manifest_url("https://m.example/bunko/latest/release.json", "0.8.0"),
            "https://m.example/bunko/v0.8.0/release.json"
        );
        assert_eq!(
            release_manifest_url("http://127.0.0.1:8000/release.json", "0.7.0-alpha.2"),
            "http://127.0.0.1:8000/v0.7.0-alpha.2/release.json"
        );
        assert_eq!(
            release_manifest_url("https://m.example/r/{version}/release.json?x=1", "1.0.0"),
            "https://m.example/r/1.0.0/release.json?x=1"
        );
        assert_eq!(
            release_manifest_url("https://m.example/release.json?token=a/b", "1.0.0"),
            "https://m.example/v1.0.0/release.json?token=a/b"
        );
        assert_eq!(
            release_manifest_url("/srv/mirror/release.json", "1.0.0"),
            "/srv/mirror/v1.0.0/release.json"
        );
    }

    #[test]
    fn processor_follows_newer_only() {
        let newer = VersionMismatch::between("0.7.1", "0.7.0");
        let older = VersionMismatch::between("0.6.0", "0.7.0");
        assert_eq!(processor_action(None, true), ProcessorAction::Nothing);
        assert_eq!(
            processor_action(newer.as_ref(), true),
            ProcessorAction::Update("0.7.1".into())
        );
        assert_eq!(
            processor_action(newer.as_ref(), false),
            ProcessorAction::ReportNewer("0.7.1".into())
        );
        // Never a downgrade, auto or not.
        assert_eq!(
            processor_action(older.as_ref(), true),
            ProcessorAction::ReportOlder("0.6.0".into())
        );
        assert!(is_newer("0.7.0-alpha.2", "0.7.0-alpha.1"));
        assert!(!is_newer("0.7.0-alpha.1", "0.7.0-alpha.2"));
        assert!(!is_newer("0.7.0", "0.7.0"));
        assert!(!is_newer("garbage", "0.7.0"));
    }

    #[test]
    fn retry_backs_off_and_never_loops() {
        let first = Duration::from_secs(600);
        assert_eq!(Retry::wait_after(1, first), first);
        assert_eq!(Retry::wait_after(2, first), first * 2);
        assert_eq!(Retry::wait_after(30, first), RETRY_MAX);
        let mut r = Retry::default();
        let t0 = Instant::now();
        assert!(r.may_try("0.7.1", t0));
        let wait = r.failed("0.7.1", "boom", t0);
        assert!(!r.may_try("0.7.1", t0 + wait / 2));
        assert!(r.may_try("0.7.1", t0 + wait));
        // Another version is not held back by this one's failure.
        assert!(r.may_try("0.7.2", t0));
        r.failed("0.7.1", "boom", t0);
        r.failed("0.7.1", "boom", t0);
        assert!(r.keeps_failing());
        assert_eq!(r.last_error(), Some("boom"));
        r.succeeded();
        assert_eq!(r.failures(), 0);
    }

    #[test]
    fn marker_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let m = Marker {
            kind: "binary".into(),
            from: "0.7.0".into(),
            to: "0.7.1".into(),
            at: now_rfc3339(),
            pack: None,
            prev_pack: None,
            pack_root: None,
            reason: None,
        };
        m.write(dir.path()).unwrap();
        let back = Marker::take(dir.path()).unwrap();
        assert_eq!(back, m);
        assert!(Marker::take(dir.path()).is_none(), "taken once");
        assert!(back.took("0.7.1"));
        assert!(!back.took("0.7.0"));
    }

    #[test]
    fn key_choice() {
        assert_eq!(
            release_key(""),
            (crate::RELEASE_PUBLIC_KEY.to_string(), false)
        );
        assert_eq!(release_key("abc="), ("abc=".to_string(), true));
    }

    #[test]
    fn space_check() {
        let dir = tempfile::tempdir().unwrap();
        assert!(check_space(dir.path(), 1).is_ok());
        let e = check_space(&dir.path().join("not/yet"), u64::MAX / 4).unwrap_err();
        assert!(e.starts_with("not enough disk space"), "{e}");
    }
}
