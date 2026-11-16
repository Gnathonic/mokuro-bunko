//! `cargo xtask-live interop live-auth <target>`: the owner's sign-in commands for the
//! scheduled live runs (docs/design/plans/2026-11-15-i5-live-runs.md, Task 1; spec
//! `docs/design/interop-testing.md` §7.2).
//!
//! Each command gets one credential (a refresh token from a browser sign-in, the saved
//! MEGA session, a GitHub token), checks it with one harmless read, shows which account
//! it belongs to, asks, and stores it as an Actions secret with `gh secret set`, the
//! value on stdin. No value is ever printed, logged, written to disk or put on a
//! command line; `--dry-run` does everything but store.
//!
//! This module is always built (arguments, secret names, the `gh` writer); the sign-in
//! flows are in `auth`, behind the `live` feature, so the release jobs that run xtask
//! don't build Bunko's OAuth and MEGA code.

// Without `live` only `run` (which says how to turn it on) and the tests use the rest.
#![cfg_attr(not(feature = "live"), allow(dead_code))]

#[cfg(feature = "live")]
mod auth;
#[cfg(feature = "live")]
mod runner;
pub mod snapshot;

use anyhow::{Context, Result, bail};
use std::fmt;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The repository whose Actions secrets the live runs read.
pub const REPO: &str = "Gnathonic/mokuro-bunko";

/// What a `live-auth` command signs in to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Target {
    /// The Drive test account, through Bunko's beta Desktop client.
    Gdrive,
    /// The OneDrive test account, through Bunko's Microsoft client.
    Onedrive,
    /// The MEGA account the live runs use (a saved session, or a new sign-in).
    Mega,
    /// The fine-grained GitHub token the runs store a rotated OneDrive token with.
    GithubToken,
}

impl Target {
    /// The command-line name.
    pub fn name(self) -> &'static str {
        match self {
            Target::Gdrive => "gdrive",
            Target::Onedrive => "onedrive",
            Target::Mega => "mega",
            Target::GithubToken => "github-token",
        }
    }

    /// The Actions secret this target's credential is stored as.
    pub fn secret_name(self) -> &'static str {
        match self {
            Target::Gdrive => "BUNKO_LIVE_GDRIVE_REFRESH_TOKEN",
            Target::Onedrive => "BUNKO_LIVE_ONEDRIVE_REFRESH_TOKEN",
            Target::Mega => "BUNKO_LIVE_MEGA_SESSION",
            Target::GithubToken => "BUNKO_LIVE_SECRETS_TOKEN",
        }
    }

    /// What a dead credential's error tells the owner to do.
    pub fn reauth_hint(self) -> String {
        format!(
            "run `cargo xtask-live interop live-auth {}` again",
            self.name()
        )
    }
}

#[derive(clap::Args, Debug)]
pub struct LiveAuthArgs {
    #[arg(value_enum)]
    pub target: Target,
    /// Do everything but store the secret: report only that it was obtained and
    /// checked, and its length.
    #[arg(long)]
    pub dry_run: bool,
    /// Store without asking (the account is still shown).
    #[arg(long)]
    pub yes: bool,
    /// gdrive, onedrive: print the sign-in address instead of opening a browser.
    #[arg(long)]
    pub no_browser: bool,
    /// The repository to store the secret in.
    #[arg(long, default_value = REPO)]
    pub repo: String,
    /// gdrive: the beta project's Desktop client as Google's console downloads it
    /// (`{"installed": {"client_id", "client_secret", ..}}`). Without it, the secret
    /// comes from BUNKO_GOOGLE_BETA_DESKTOP_SECRET, at run time or at build time.
    #[arg(long, value_name = "PATH")]
    pub client_json: Option<PathBuf>,
    /// mega: the folder whose credentials.json holds the saved session (default
    /// ~/.config/mokuro-bunko-dev/mega-live). A sealed file needs MOKURO_SECRETS_KEY.
    #[arg(long, value_name = "DIR", conflicts_with = "interactive")]
    pub from_saved: Option<PathBuf>,
    /// mega: sign in afresh; email, password and 2FA code are asked for on the
    /// terminal, not shown.
    #[arg(long)]
    pub interactive: bool,

    // Test knobs (the cloud simulator, a fake `gh`), not for the owner.
    /// The GitHub CLI to run.
    #[arg(long, hide = true, default_value = "gh")]
    pub gh: PathBuf,
    /// gdrive, onedrive: read a refresh token from stdin instead of signing in.
    #[arg(long, hide = true)]
    pub refresh_token_stdin: bool,
    /// gdrive, onedrive: the OAuth token endpoint.
    #[arg(long, hide = true)]
    pub token_url: Option<String>,
    /// gdrive, onedrive: the API base (Drive `https://www.googleapis.com`, Graph
    /// `https://graph.microsoft.com/v1.0`).
    #[arg(long, hide = true)]
    pub api_base: Option<String>,
    /// mega: the API origin.
    #[arg(long, hide = true)]
    pub mega_origin: Option<String>,
    /// github-token: the REST API base.
    #[arg(long, hide = true)]
    pub github_api: Option<String>,
}

impl LiveAuthArgs {
    /// Options that belong to another target are mistakes, not no-ops.
    pub fn validate(&self) -> Result<()> {
        let t = self.target;
        let oauth = matches!(t, Target::Gdrive | Target::Onedrive);
        let only = |set: bool, flag: &str, ok: bool, which: &str| -> Result<()> {
            if set && !ok {
                bail!("{flag} is for {which} only, not {}", t.name());
            }
            Ok(())
        };
        only(
            self.client_json.is_some(),
            "--client-json",
            t == Target::Gdrive,
            "gdrive",
        )?;
        only(
            self.from_saved.is_some(),
            "--from-saved",
            t == Target::Mega,
            "mega",
        )?;
        only(self.interactive, "--interactive", t == Target::Mega, "mega")?;
        only(
            self.no_browser,
            "--no-browser",
            oauth,
            "gdrive and onedrive",
        )?;
        only(
            self.refresh_token_stdin,
            "--refresh-token-stdin",
            oauth,
            "gdrive and onedrive",
        )?;
        if !self.repo.contains('/') {
            bail!("--repo {}: give it as owner/name", self.repo);
        }
        Ok(())
    }

    pub fn writer(&self) -> SecretWriter {
        SecretWriter {
            gh: self.gh.clone(),
            repo: self.repo.clone(),
            dry_run: self.dry_run,
        }
    }
}

/// A credential. It has no `Display`, and `Debug` shows only its length, so no
/// format string can print it by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Secret {
        Secret(value.into())
    }

    /// The value, for the one place that sends it somewhere.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(<{} chars>)", self.len())
    }
}

/// `text` with every secret's value replaced by `<redacted>` (values under 4
/// characters are left: they would match everywhere and are no credential).
pub fn redact(text: &str, secrets: &[&Secret]) -> String {
    let mut s = text.to_string();
    for secret in secrets {
        if secret.expose().len() >= 4 {
            s = s.replace(secret.expose(), "<redacted>");
        }
    }
    s
}

/// A credential that was obtained and checked, and the account it belongs to (shown
/// on the owner's terminal only).
#[derive(Debug)]
pub struct Obtained {
    pub secret: Secret,
    pub account: String,
}

/// Stores Actions secrets through the GitHub CLI.
#[derive(Debug, Clone)]
pub struct SecretWriter {
    pub gh: PathBuf,
    pub repo: String,
    pub dry_run: bool,
}

impl SecretWriter {
    /// `gh`'s arguments: the name and the repository, never the value (gh reads the
    /// value from stdin when stdin is not a terminal and no `--body` is given).
    pub fn gh_args(&self, name: &str) -> Vec<String> {
        ["secret", "set", name, "--repo", &self.repo]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    /// Store `value` as `name`, or with `--dry-run` say what would be stored.
    pub fn store(&self, name: &str, value: &Secret, out: &mut dyn Write) -> Result<()> {
        if value.is_empty() {
            bail!("{name}: nothing to store (the value is empty)");
        }
        if self.dry_run {
            writeln!(
                out,
                "dry run: {name} ({} chars) not stored in {}",
                value.len(),
                self.repo
            )?;
            return Ok(());
        }
        let mut child = Command::new(&self.gh)
            .args(self.gh_args(name))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "running {} (the GitHub CLI: install it and run `gh auth login`)",
                    self.gh.display()
                )
            })?;
        {
            let mut stdin = child.stdin.take().context("gh's stdin")?;
            match stdin.write_all(value.expose().as_bytes()) {
                // gh quit before reading: its exit status says why.
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                other => other.context("writing the secret to gh")?,
            }
            // Dropped here: gh sees the end of its input.
        }
        let o = child.wait_with_output().context("waiting for gh")?;
        let said = redact(
            &format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
            &[value],
        );
        if !o.status.success() {
            bail!(
                "gh secret set {name} failed ({}): {}",
                o.status,
                said.trim()
            );
        }
        writeln!(out, "stored {name} in {}", self.repo)?;
        Ok(())
    }
}

/// Ask a yes/no question; anything but `y`/`yes` (and the end of input) is no.
pub fn confirm(question: &str, input: &mut dyn BufRead, out: &mut dyn Write) -> Result<bool> {
    write!(out, "{question} [y/N] ")?;
    out.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        writeln!(out)?;
        return Ok(false);
    }
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// After a credential is obtained and checked: report it, ask (unless `--yes` or
/// `--dry-run`), store it.
pub fn finish(
    a: &LiveAuthArgs,
    got: &Obtained,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<()> {
    let name = a.target.secret_name();
    writeln!(
        out,
        "{}: obtained and verified for {} ({} chars)",
        a.target.name(),
        got.account,
        got.secret.len()
    )?;
    if !a.dry_run && !a.yes && !confirm(&format!("Store it as {name} in {}?", a.repo), input, out)?
    {
        bail!("not stored");
    }
    a.writer().store(name, &got.secret, out)
}

/// The providers a live run runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum LiveProvider {
    Gdrive,
    Onedrive,
    Mega,
}

impl LiveProvider {
    pub fn name(self) -> &'static str {
        self.target().name()
    }

    /// The sign-in command and secret of this provider.
    pub fn target(self) -> Target {
        match self {
            LiveProvider::Gdrive => Target::Gdrive,
            LiveProvider::Onedrive => Target::Onedrive,
            LiveProvider::Mega => Target::Mega,
        }
    }

    /// `storage.library.backend`.
    pub fn backend(self) -> &'static str {
        match self {
            LiveProvider::Gdrive => "gdrive",
            LiveProvider::Onedrive => "onedrive",
            LiveProvider::Mega => "mega",
        }
    }

    pub fn rig_provider(self) -> super::rig::Provider {
        match self {
            LiveProvider::Gdrive => super::rig::Provider::Google,
            LiveProvider::Onedrive => super::rig::Provider::Microsoft,
            LiveProvider::Mega => super::rig::Provider::Mega,
        }
    }
}

/// `cargo xtask-live interop live`: one scheduled live run (docs/design/plans/
/// 2026-11-15-i5-live-runs.md). Reads `BUNKO_LIVE_<PROVIDER>_*` (and, for Drive,
/// `BUNKO_GOOGLE_BETA_DESKTOP_SECRET`) from the environment.
#[derive(clap::Args, Debug)]
pub struct LiveArgs {
    #[arg(long, value_enum)]
    pub provider: LiveProvider,
    /// A testing-feature server (default: target/release/mokuro-bunko).
    #[arg(long)]
    pub bin: Option<PathBuf>,
    /// The run id in the sandbox's name (default: the UTC time and the workflow run's
    /// id, or `local`).
    #[arg(long)]
    pub run_id: Option<String>,
    /// Only the cloud suite's scenarios whose name contains this.
    #[arg(long)]
    pub grep: Option<String>,
    /// Keep the work folder (credentials and configs are removed whatever happens).
    #[arg(long)]
    pub keep: bool,
    /// Where `live-<provider>.json` goes (default: target/interop/live).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// The repository a rotated OneDrive token is stored in.
    #[arg(long, default_value = REPO)]
    pub repo: String,
    /// Do not store a rotated token (a local run; the stored one keeps working until
    /// it expires).
    #[arg(long)]
    pub no_store: bool,
    /// The previous run's `live-<provider>.json`: a second transfer-quota skip in a
    /// row fails.
    #[arg(long)]
    pub previous: Option<PathBuf>,
    /// The GitHub CLI to run.
    #[arg(long, hide = true, default_value = "gh")]
    pub gh: PathBuf,
    /// cloudsim's address: every provider address and token endpoint under it, the
    /// server's too (this command's end-to-end test).
    #[arg(long, hide = true)]
    pub sim_base: Option<String>,
    /// With --sim-base only: write one file outside the sandbox before the diff, which
    /// must then fail the run.
    #[arg(long, hide = true, requires = "sim_base")]
    pub inject_leak: bool,
}

pub fn run_live(a: &LiveArgs) -> Result<()> {
    if !a.repo.contains('/') {
        bail!("--repo {}: give it as owner/name", a.repo);
    }
    #[cfg(feature = "live")]
    {
        runner::run(a)
    }
    #[cfg(not(feature = "live"))]
    {
        bail!(
            "live needs xtask's `live` feature: run `cargo xtask-live interop live --provider {}`",
            a.provider.name()
        )
    }
}

pub fn run(a: &LiveAuthArgs) -> Result<()> {
    a.validate()?;
    #[cfg(feature = "live")]
    {
        auth::run(a)
    }
    #[cfg(not(feature = "live"))]
    {
        bail!(
            "live-auth needs xtask's `live` feature: run `cargo xtask-live interop live-auth {}`",
            a.target.name()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        a: LiveAuthArgs,
    }

    fn parse(args: &[&str]) -> Result<LiveAuthArgs, clap::Error> {
        Cli::try_parse_from(std::iter::once("live-auth").chain(args.iter().copied())).map(|c| c.a)
    }

    const SENTINEL: &str = "SENTINEL-1//0gLiveRefreshToken-do-not-print";

    #[test]
    fn arguments_parse() {
        let a = parse(&["gdrive", "--dry-run"]).unwrap();
        assert_eq!(a.target, Target::Gdrive);
        assert!(a.dry_run && !a.yes);
        assert_eq!(a.repo, REPO);
        assert_eq!(a.gh, PathBuf::from("gh"));
        a.validate().unwrap();

        let a = parse(&["mega", "--interactive"]).unwrap();
        assert!(a.interactive);
        a.validate().unwrap();
        let a = parse(&["mega", "--from-saved", "/x"]).unwrap();
        assert_eq!(a.from_saved, Some(PathBuf::from("/x")));
        assert_eq!(
            parse(&["github-token"]).unwrap().target,
            Target::GithubToken
        );
        assert_eq!(parse(&["onedrive"]).unwrap().target, Target::Onedrive);

        assert!(parse(&["dropbox"]).is_err());
        assert!(parse(&[]).is_err());
        assert!(parse(&["mega", "--from-saved", "/x", "--interactive"]).is_err());
        // No option takes a credential: there is nowhere to pass one.
        assert!(parse(&["github-token", "--token", "x"]).is_err());
        assert!(parse(&["mega", "--password", "x"]).is_err());
    }

    #[test]
    fn options_of_another_target_are_refused() {
        for args in [
            &["onedrive", "--client-json", "c.json"][..],
            &["gdrive", "--interactive"],
            &["github-token", "--from-saved", "/x"],
            &["mega", "--no-browser"],
            &["mega", "--refresh-token-stdin"],
            &["gdrive", "--repo", "mokuro-bunko"],
        ] {
            let a = parse(args).unwrap();
            assert!(a.validate().is_err(), "{args:?}");
        }
    }

    #[test]
    fn secret_names_are_distinct_and_namespaced() {
        let all = [
            Target::Gdrive,
            Target::Onedrive,
            Target::Mega,
            Target::GithubToken,
        ];
        let mut names: Vec<&str> = all.iter().map(|t| t.secret_name()).collect();
        assert!(names.iter().all(|n| n.starts_with("BUNKO_LIVE_")));
        names.sort();
        names.dedup();
        assert_eq!(names.len(), all.len());
        assert_eq!(
            Target::Onedrive.secret_name(),
            "BUNKO_LIVE_ONEDRIVE_REFRESH_TOKEN"
        );
        assert!(Target::Gdrive.reauth_hint().contains("live-auth gdrive"));
    }

    #[test]
    fn a_secret_never_formats_its_value() {
        let s = Secret::new(SENTINEL);
        let shown = format!(
            "{s:?} {:?}",
            Obtained {
                secret: s.clone(),
                account: "someone".into()
            }
        );
        assert!(!shown.contains(SENTINEL), "{shown}");
        assert!(shown.contains(&format!("<{} chars>", SENTINEL.len())));
        assert_eq!(redact(&format!("x {SENTINEL} y"), &[&s]), "x <redacted> y");
    }

    #[test]
    fn the_gh_command_line_carries_no_value() {
        let w = SecretWriter {
            gh: "gh".into(),
            repo: REPO.into(),
            dry_run: false,
        };
        assert_eq!(
            w.gh_args("BUNKO_LIVE_MEGA_SESSION"),
            ["secret", "set", "BUNKO_LIVE_MEGA_SESSION", "--repo", REPO]
        );
    }

    #[test]
    fn a_dry_run_never_runs_gh() {
        let w = SecretWriter {
            gh: "/nonexistent/gh-must-not-run".into(),
            repo: REPO.into(),
            dry_run: true,
        };
        let mut out = Vec::new();
        w.store(
            "BUNKO_LIVE_GDRIVE_REFRESH_TOKEN",
            &Secret::new(SENTINEL),
            &mut out,
        )
        .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("dry run") && out.contains(&SENTINEL.len().to_string()));
        assert!(!out.contains(SENTINEL));
        assert!(
            w.store("X", &Secret::new(""), &mut Vec::new()).is_err(),
            "an empty value is never stored"
        );
    }

    #[cfg(unix)]
    mod fake_gh {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;

        /// A `gh` that records its arguments and stdin, then prints `say` and exits
        /// with `code`.
        fn fake(dir: &Path, say: &str, code: i32) -> PathBuf {
            let p = dir.join("gh");
            let d = dir.display();
            std::fs::write(
                &p,
                format!(
                    "#!/bin/sh\n[ \"$1\" = --probe ] && exit 0\nprintf '%s\\n' \"$@\" > '{d}/args'\ncat > '{d}/stdin'\n{say}\nexit {code}\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            // A test thread forking while the script was open for writing makes its
            // first exec fail with ETXTBSY until that child execs: wait it out.
            for _ in 0..100 {
                match Command::new(&p).arg("--probe").status() {
                    Ok(_) => break,
                    Err(e) if e.raw_os_error() == Some(26) => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            p
        }

        fn args(gh: PathBuf, extra: &[&str]) -> LiveAuthArgs {
            let mut a = parse(&[&["gdrive"][..], extra].concat()).unwrap();
            a.gh = gh;
            a
        }

        #[test]
        fn the_value_goes_to_gh_on_stdin_only() {
            let dir = tempfile::tempdir().unwrap();
            let gh = fake(dir.path(), "echo 'Set Actions secret'", 0);
            let w = SecretWriter {
                gh,
                repo: REPO.into(),
                dry_run: false,
            };
            let mut out = Vec::new();
            w.store(
                "BUNKO_LIVE_GDRIVE_REFRESH_TOKEN",
                &Secret::new(SENTINEL),
                &mut out,
            )
            .unwrap();
            let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
            assert_eq!(
                args,
                format!("secret\nset\nBUNKO_LIVE_GDRIVE_REFRESH_TOKEN\n--repo\n{REPO}\n")
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("stdin")).unwrap(),
                SENTINEL
            );
            let out = String::from_utf8(out).unwrap();
            assert!(
                out.contains("stored BUNKO_LIVE_GDRIVE_REFRESH_TOKEN"),
                "{out}"
            );
            assert!(!out.contains(SENTINEL));
        }

        #[test]
        fn a_failing_gh_is_reported_without_the_value() {
            let dir = tempfile::tempdir().unwrap();
            // A gh that echoes its input back in its error.
            let gh = fake(dir.path(), "cat \"$(dirname \"$0\")/stdin\" >&2", 1);
            let w = SecretWriter {
                gh,
                repo: REPO.into(),
                dry_run: false,
            };
            let err = w
                .store(
                    "BUNKO_LIVE_MEGA_SESSION",
                    &Secret::new(SENTINEL),
                    &mut Vec::new(),
                )
                .unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("BUNKO_LIVE_MEGA_SESSION") && msg.contains("<redacted>"));
            assert!(!msg.contains(SENTINEL), "{msg}");
        }

        #[test]
        fn finish_asks_and_a_no_stores_nothing() {
            let dir = tempfile::tempdir().unwrap();
            let gh = fake(dir.path(), "true", 0);
            let got = Obtained {
                secret: Secret::new(SENTINEL),
                account: "test account".into(),
            };
            let mut out = Vec::new();
            let a = args(gh.clone(), &[]);
            let err = finish(&a, &got, &mut &b"n\n"[..], &mut out).unwrap_err();
            assert_eq!(err.to_string(), "not stored");
            assert!(!dir.path().join("args").exists(), "gh ran after a no");
            // The end of input is a no too.
            assert!(finish(&a, &got, &mut &b""[..], &mut out).is_err());
            assert!(!dir.path().join("args").exists());

            finish(&a, &got, &mut &b"y\n"[..], &mut out).unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.path().join("stdin")).unwrap(),
                SENTINEL
            );
            let out = String::from_utf8(out).unwrap();
            assert!(out.contains("for test account"), "{out}");
            assert!(out.contains("Store it as BUNKO_LIVE_GDRIVE_REFRESH_TOKEN"));
            assert!(!out.contains(SENTINEL), "{out}");
        }

        #[test]
        fn yes_skips_the_question_and_dry_run_skips_gh() {
            let dir = tempfile::tempdir().unwrap();
            let gh = fake(dir.path(), "true", 0);
            let got = Obtained {
                secret: Secret::new(SENTINEL),
                account: "test account".into(),
            };
            let mut out = Vec::new();
            finish(
                &args(gh.clone(), &["--dry-run"]),
                &got,
                &mut &b""[..],
                &mut out,
            )
            .unwrap();
            assert!(!dir.path().join("args").exists());
            finish(&args(gh, &["--yes"]), &got, &mut &b""[..], &mut out).unwrap();
            assert!(dir.path().join("args").exists());
            let out = String::from_utf8(out).unwrap();
            assert!(!out.contains("[y/N]"), "{out}");
            assert!(!out.contains(SENTINEL));
        }
    }
}
