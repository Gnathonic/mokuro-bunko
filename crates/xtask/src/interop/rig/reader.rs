//! The reader the browser suites drive: a checkout (`--reader-dir`, or the commit
//! `interop/reader.ref` pins, fetched into `target/interop/reader/<sha>`), built with
//! `vite build --mode interop` (the cloud test hooks and the rig driver built in) and
//! served by `vite preview` on a port the rig picks.

use super::ReaderSource;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Where a pinned commit is fetched from (`MOKURO_READER_REPO` overrides it, for a
/// mirror or a local clone).
const READER_REPO: &str = "https://github.com/Gnathonic/mokuro-reader";

/// A reader checkout and the commit it is at.
#[derive(Debug)]
pub struct Checkout {
    pub dir: PathBuf,
    pub head: String,
    /// `--reader-dir` only: the hash of its uncommitted changes, when it has any.
    pub dirty: Option<String>,
}

impl Checkout {
    /// The commit as the results name it; a dirty tree is not that commit.
    pub fn reader_sha(&self) -> String {
        match &self.dirty {
            Some(_) => format!("{}-dirty", self.head),
            None => self.head.clone(),
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {} in {} failed ({}): {}",
            args.join(" "),
            dir.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn head(dir: &Path) -> Result<String> {
    Ok(String::from_utf8(git(dir, &["rev-parse", "HEAD"])?)?
        .trim()
        .to_string())
}

/// The checkout `src` names: a `--reader-dir` as it is, or the pinned commit fetched
/// (once) into `cache/<sha>`.
pub fn checkout(src: &ReaderSource, cache: &Path) -> Result<Checkout> {
    match src {
        ReaderSource::Dir(d) => {
            let dir = std::fs::canonicalize(d)
                .with_context(|| format!("--reader-dir {}", d.display()))?;
            if !dir.join("package.json").is_file() {
                bail!("--reader-dir {} holds no package.json", dir.display());
            }
            let head = head(&dir).context("--reader-dir must be a git checkout")?;
            let dirty = dirty_tree_hash(&dir)?;
            Ok(Checkout { dir, head, dirty })
        }
        ReaderSource::Ref(sha) => {
            let repo = std::env::var("MOKURO_READER_REPO").unwrap_or_else(|_| READER_REPO.into());
            let dir = cache.join(sha);
            fetch(&repo, sha, &dir)?;
            Ok(Checkout {
                dir,
                head: sha.clone(),
                dirty: None,
            })
        }
    }
}

/// `git init` + a depth-1 fetch of exactly `sha`, checked out detached. Done in a
/// `.part` folder renamed into place, so a broken fetch never passes for a checkout.
fn fetch(repo: &str, sha: &str, dir: &Path) -> Result<()> {
    if dir.join(".git").exists() && head(dir).ok().as_deref() == Some(sha) {
        return Ok(());
    }
    let part = dir.with_extension("part");
    let _ = std::fs::remove_dir_all(&part);
    std::fs::create_dir_all(&part)?;
    eprintln!("fetching reader {sha} from {repo}");
    git(&part, &["init", "-q"])?;
    git(&part, &["fetch", "-q", "--depth", "1", repo, sha])
        .with_context(|| format!("fetching reader commit {sha} (is it pushed?)"))?;
    git(&part, &["checkout", "-q", "--detach", "FETCH_HEAD"])?;
    let got = head(&part)?;
    if got != sha {
        bail!("fetched {got}, not the pinned {sha}");
    }
    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(&part, dir)?;
    Ok(())
}

/// A hash of every uncommitted change (tracked edits, staged or not, and untracked
/// files git does not ignore), or `None` for a clean tree.
pub fn dirty_tree_hash(dir: &Path) -> Result<Option<String>> {
    let diff = git(dir, &["diff", "HEAD", "--binary"])?;
    let untracked = git(dir, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    if diff.is_empty() && untracked.is_empty() {
        return Ok(None);
    }
    let mut h = Sha256::new();
    h.update(&diff);
    for name in untracked.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        h.update([0u8]);
        h.update(name);
        h.update([0u8]);
        let path = dir.join(String::from_utf8_lossy(name).as_ref());
        // A file deleted between the listing and the read hashes as absent.
        h.update(std::fs::read(path).unwrap_or_default());
    }
    Ok(Some(hex::encode(h.finalize())))
}

/// The build cache key: the commit, the dirty tree on top of it, and the lockfile.
pub fn build_key(head: &str, dirty: Option<&str>, lock_sha: &str) -> String {
    let mut h = Sha256::new();
    for part in ["reader-build-v1", head, dirty.unwrap_or("clean"), lock_sha] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    hex::encode(h.finalize())
}

/// What the last build in a checkout was, kept beside the cache (never in the
/// checkout, where it would dirty a `--reader-dir`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub key: String,
    /// The lockfile hash of the last `npm ci` the rig ran there.
    pub lock: Option<String>,
    /// [`output_fingerprint`] right after that build: anything else that builds in
    /// the same checkout (a production `npm run build`) changes it.
    pub output: Option<String>,
}

/// The last build is still the one wanted, and its output is untouched.
pub fn is_cached(stamp: Option<&Stamp>, key: &str, output: Option<&str>) -> bool {
    stamp.is_some_and(|s| s.key == key && output.is_some() && s.output.as_deref() == output)
}

/// `npm ci` when nothing is installed, or when the lockfile moved since the rig's own
/// last install. A developer's `--reader-dir` with its own install is used as it is.
pub fn needs_install(installed: bool, stamp_lock: Option<&str>, lock: &str) -> bool {
    !installed || stamp_lock.is_some_and(|s| s != lock)
}

/// The build output (`.svelte-kit/output`: adapter-auto writes no host output
/// locally, so `vite preview` serves that), as every file's path, size and mtime.
pub fn output_fingerprint(dir: &Path) -> Option<String> {
    let out = dir.join(".svelte-kit").join("output");
    let mut files = Vec::new();
    let mut stack = vec![out.clone()];
    while let Some(d) = stack.pop() {
        let rd = std::fs::read_dir(&d).ok()?;
        for e in rd.flatten() {
            let p = e.path();
            let meta = e.metadata().ok()?;
            if meta.is_dir() {
                stack.push(p);
            } else {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                let rel = p
                    .strip_prefix(&out)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .into_owned();
                files.push((rel, meta.len(), mtime));
            }
        }
    }
    if files.is_empty() {
        return None;
    }
    files.sort();
    let mut h = Sha256::new();
    for (rel, len, mtime) in files {
        h.update(format!("{rel}\0{len}\0{mtime}\n").as_bytes());
    }
    Some(hex::encode(h.finalize()))
}

fn stamp_path(stamps: &Path, dir: &Path) -> PathBuf {
    let id = hex::encode(Sha256::digest(dir.to_string_lossy().as_bytes()));
    stamps.join(format!("{}.json", &id[..16]))
}

fn read_stamp(path: &Path) -> Option<Stamp> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Run `cmd` in `dir` with its output appended to `log`; the log's tail in the error.
fn run_logged(dir: &Path, program: &str, args: &[&str], log: &Path) -> Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    writeln!(file, "+ {program} {}", args.join(" "))?;
    eprintln!(
        "+ {program} {} (in {}, log {})",
        args.join(" "),
        dir.display(),
        log.display()
    );
    let status = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .status()
        .with_context(|| format!("starting {program}"))?;
    if !status.success() {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        bail!(
            "{program} {} failed ({status}) in {}\n{}",
            args.join(" "),
            dir.display(),
            lines[lines.len().saturating_sub(40)..].join("\n")
        );
    }
    Ok(())
}

/// `npm ci` (when [`needs_install`]) and `vite build --mode interop`, unless the
/// cached build is still the one wanted. Answers whether it built.
pub fn build(co: &Checkout, stamps: &Path, log: &Path) -> Result<bool> {
    std::fs::create_dir_all(stamps)?;
    let lock_path = co.dir.join("package-lock.json");
    let lock = hex::encode(Sha256::digest(
        std::fs::read(&lock_path).with_context(|| format!("reading {}", lock_path.display()))?,
    ));
    let key = build_key(&co.head, co.dirty.as_deref(), &lock);
    let sp = stamp_path(stamps, &co.dir);
    let stamp = read_stamp(&sp);
    if is_cached(stamp.as_ref(), &key, output_fingerprint(&co.dir).as_deref()) {
        eprintln!(
            "reader {} already built ({})",
            co.reader_sha(),
            co.dir.display()
        );
        return Ok(false);
    }
    let _ = std::fs::remove_file(log);
    let installed = co.dir.join("node_modules").is_dir();
    let mut stamp_lock = stamp.as_ref().and_then(|s| s.lock.clone());
    if needs_install(installed, stamp_lock.as_deref(), &lock) {
        run_logged(&co.dir, "npm", &["ci", "--no-audit", "--no-fund"], log)?;
        stamp_lock = Some(lock.clone());
    }
    run_logged(&co.dir, "npx", &["vite", "build", "--mode", "interop"], log)?;
    let output = output_fingerprint(&co.dir);
    if output.is_none() {
        bail!(
            "vite build wrote nothing to {}/.svelte-kit/output",
            co.dir.display()
        );
    }
    let new = Stamp {
        key,
        lock: stamp_lock,
        output,
    };
    std::fs::write(&sp, serde_json::to_vec_pretty(&new)?)?;
    Ok(true)
}

/// `vite preview`'s arguments: the interop mode (as built), loopback only, exactly
/// the port the rig picked (rclone's CORS origin names it before the reader starts).
pub fn serve_args(port: u16) -> Vec<String> {
    [
        "vite",
        "preview",
        "--mode",
        "interop",
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--strictPort",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// The built reader, served.
pub struct Preview {
    child: Child,
    pub url: String,
}

impl Preview {
    pub fn start(dir: &Path, port: u16, log: &Path) -> Result<Preview> {
        let file = std::fs::File::create(log)?;
        let mut cmd = Command::new("npx");
        cmd.args(serve_args(port))
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(file.try_clone()?)
            .stderr(file);
        // npx runs vite as a child: a group of its own lets the rig stop both.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }
        let child = cmd.spawn().context("starting npx vite preview")?;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let mut p = Preview {
            child,
            url: format!("http://{addr}"),
        };
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(60) {
            if let Ok(r) = super::super::http::get(addr, "/")
                && r.status == 200
            {
                return Ok(p);
            }
            if let Some(status) = p.child.try_wait()? {
                bail!(
                    "vite preview exited ({status})\n{}",
                    std::fs::read_to_string(log).unwrap_or_default()
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        p.stop();
        bail!(
            "vite preview did not serve {} in 60 s\n{}",
            p.url,
            std::fs::read_to_string(log).unwrap_or_default()
        )
    }

    fn stop(&mut self) {
        #[cfg(unix)]
        {
            let group = format!("-{}", self.child.id());
            let kill = |sig: &str| {
                let _ = Command::new("kill")
                    .args([sig, "--", &group])
                    .stderr(Stdio::null())
                    .status();
            };
            kill("-TERM");
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(5) {
                if let Ok(Some(_)) = self.child.try_wait() {
                    // npx is gone; whatever of its group is left goes too.
                    kill("-KILL");
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            kill("-KILL");
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=rig", "-c", "user.email=rig@test"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }

    /// A repo with one commit holding package.json and package-lock.json.
    fn repo() -> (tempfile::TempDir, String) {
        let d = tempfile::tempdir().unwrap();
        sh(d.path(), &["init", "-q"]);
        std::fs::write(d.path().join("package.json"), "{}").unwrap();
        std::fs::write(d.path().join("package-lock.json"), "{\"v\":1}").unwrap();
        std::fs::write(d.path().join(".gitignore"), "node_modules/\n.svelte-kit/\n").unwrap();
        sh(d.path(), &["add", "."]);
        sh(d.path(), &["commit", "-q", "-m", "one"]);
        let sha = head(d.path()).unwrap();
        (d, sha)
    }

    #[test]
    fn the_build_key_moves_with_the_commit_the_tree_and_the_lockfile() {
        let base = build_key("a", None, "l");
        assert_eq!(base, build_key("a", None, "l"));
        assert_eq!(base.len(), 64);
        for other in [
            build_key("b", None, "l"),
            build_key("a", Some("d1"), "l"),
            build_key("a", None, "m"),
        ] {
            assert_ne!(base, other);
        }
        assert_ne!(
            build_key("a", Some("d1"), "l"),
            build_key("a", Some("d2"), "l")
        );
        // "clean" is the marker for no changes, not a hash a dirty tree could have.
        assert_ne!(base, build_key("a", Some("clean2"), "l"));
    }

    #[test]
    fn a_dirty_tree_hashes_its_changes_and_ignores_ignored_files() {
        let (d, _) = repo();
        assert_eq!(dirty_tree_hash(d.path()).unwrap(), None);
        // Ignored: build output and an install change nothing.
        std::fs::create_dir_all(d.path().join(".svelte-kit/output")).unwrap();
        std::fs::write(d.path().join(".svelte-kit/output/x.js"), "x").unwrap();
        std::fs::create_dir_all(d.path().join("node_modules/a")).unwrap();
        assert_eq!(dirty_tree_hash(d.path()).unwrap(), None);
        std::fs::write(d.path().join("package.json"), "{\"a\":1}").unwrap();
        let edited = dirty_tree_hash(d.path()).unwrap().expect("dirty");
        std::fs::write(d.path().join("new.ts"), "one").unwrap();
        let untracked = dirty_tree_hash(d.path()).unwrap().expect("dirty");
        assert_ne!(edited, untracked);
        // An untracked file's contents count, not only its name.
        std::fs::write(d.path().join("new.ts"), "two").unwrap();
        assert_ne!(dirty_tree_hash(d.path()).unwrap().unwrap(), untracked);
        // Staged counts the same as unstaged.
        std::fs::remove_file(d.path().join("new.ts")).unwrap();
        let unstaged = dirty_tree_hash(d.path()).unwrap();
        sh(d.path(), &["add", "package.json"]);
        assert_eq!(dirty_tree_hash(d.path()).unwrap(), unstaged);
    }

    #[test]
    fn a_reader_dir_is_used_as_it_is() {
        let (d, sha) = repo();
        let co = checkout(
            &ReaderSource::Dir(d.path().into()),
            Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(co.head, sha);
        assert_eq!(co.reader_sha(), sha);
        assert_eq!(co.dir, std::fs::canonicalize(d.path()).unwrap());
        std::fs::write(d.path().join("x.ts"), "x").unwrap();
        let co = checkout(
            &ReaderSource::Dir(d.path().into()),
            Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(co.reader_sha(), format!("{sha}-dirty"));
        let empty = tempfile::tempdir().unwrap();
        let err = checkout(&ReaderSource::Dir(empty.path().into()), Path::new("/x")).unwrap_err();
        assert!(format!("{err:#}").contains("package.json"), "{err:#}");
    }

    #[test]
    fn a_pinned_commit_is_fetched_once_into_the_cache() {
        let (src, sha) = repo();
        // A second commit: the fetch must take the pinned one, not the tip.
        std::fs::write(src.path().join("later.txt"), "later").unwrap();
        sh(src.path(), &["add", "."]);
        sh(src.path(), &["commit", "-q", "-m", "two"]);
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path().join(&sha);
        let repo_url = format!("file://{}", src.path().display());
        fetch(&repo_url, &sha, &dir).unwrap();
        assert_eq!(head(&dir).unwrap(), sha);
        assert!(dir.join("package.json").is_file());
        assert!(!dir.join("later.txt").exists());
        assert!(!dir.with_extension("part").exists());
        // Already there: nothing fetched (the source could be gone by now).
        drop(src);
        fetch(&repo_url, &sha, &dir).unwrap();
        // An unknown commit fails and leaves no checkout behind.
        let missing = "0123456789abcdef0123456789abcdef01234567";
        let err = fetch(&repo_url, missing, &cache.path().join(missing)).unwrap_err();
        assert!(format!("{err:#}").contains(missing), "{err:#}");
        assert!(!cache.path().join(missing).exists());
    }

    #[test]
    fn a_build_is_reused_only_while_its_key_and_output_hold() {
        let s = Stamp {
            key: "k".into(),
            lock: Some("l".into()),
            output: Some("o".into()),
        };
        assert!(is_cached(Some(&s), "k", Some("o")));
        assert!(!is_cached(None, "k", Some("o")));
        assert!(!is_cached(Some(&s), "k2", Some("o")));
        // Something else built there since (or the output is gone).
        assert!(!is_cached(Some(&s), "k", Some("o2")));
        assert!(!is_cached(Some(&s), "k", None));
    }

    #[test]
    fn npm_ci_runs_without_an_install_or_after_the_lockfile_moved() {
        assert!(needs_install(false, None, "l"));
        assert!(needs_install(false, Some("l"), "l"));
        // A developer's own install, never touched by the rig: used as it is.
        assert!(!needs_install(true, None, "l"));
        assert!(!needs_install(true, Some("l"), "l"));
        assert!(needs_install(true, Some("old"), "l"));
    }

    #[test]
    fn the_output_fingerprint_changes_when_a_build_rewrites_it() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(output_fingerprint(d.path()), None);
        let out = d.path().join(".svelte-kit/output/client");
        std::fs::create_dir_all(&out).unwrap();
        assert_eq!(output_fingerprint(d.path()), None);
        std::fs::write(out.join("a.js"), "a").unwrap();
        let one = output_fingerprint(d.path()).unwrap();
        assert_eq!(output_fingerprint(d.path()).unwrap(), one);
        std::fs::write(out.join("b.js"), "b").unwrap();
        assert_ne!(output_fingerprint(d.path()).unwrap(), one);
    }

    #[test]
    fn stamps_live_outside_the_checkout() {
        let stamps = Path::new("/cache/stamps");
        let a = stamp_path(stamps, Path::new("/r/a"));
        assert!(a.starts_with(stamps));
        assert_ne!(a, stamp_path(stamps, Path::new("/r/b")));
    }

    #[test]
    fn preview_serves_the_interop_build_on_exactly_the_chosen_port() {
        assert_eq!(
            serve_args(4321),
            [
                "vite",
                "preview",
                "--mode",
                "interop",
                "--host",
                "127.0.0.1",
                "--port",
                "4321",
                "--strictPort"
            ]
        );
    }
}
