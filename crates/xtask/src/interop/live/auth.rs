//! The sign-in flows and checks of `live-auth` (the `live` feature): Bunko's own
//! OAuth code (`bunko_remote::auth`) for Drive and OneDrive, its MEGA client for MEGA.
//! The browser step is the one thing no test can run, so it stays a thin layer over
//! [`refresh_and_verify`] and [`mega_verify`], which the cloud simulator tests (and
//! the live runner will reuse).

use super::{LiveAuthArgs, Obtained, Secret, Target, finish};
use anyhow::{Context, Result, anyhow, bail};
use bunko_mega::{Client as MegaClient, ClientConfig, Error as MegaError, ErrorCode};
use bunko_remote::auth::clients::{
    BETA_DESKTOP_CLIENT_ID, BETA_DESKTOP_CLIENT_SECRET, MICROSOFT_CLIENT_ID,
};
use bunko_remote::auth::{AuthError, CredentialStore, LoopbackFlow, OAuthClient, refresh};
use std::io::{Read as _, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Drive's API, as `GDriveConfig.api_base` defaults.
pub const DRIVE_API: &str = "https://www.googleapis.com";
/// Microsoft Graph, as `OneDriveConfig.api_base` defaults.
pub const GRAPH_API: &str = "https://graph.microsoft.com/v1.0";
pub const GITHUB_API: &str = "https://api.github.com";
/// The beta Desktop client's secret at run time (the build-time one is
/// [`BETA_DESKTOP_CLIENT_SECRET`]).
pub const GOOGLE_SECRET_ENV: &str = "BUNKO_GOOGLE_BETA_DESKTOP_SECRET";

pub fn run(a: &LiveAuthArgs) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut out = std::io::stdout();
    let got = rt.block_on(obtain(a, &mut out))?;
    finish(a, &got, &mut std::io::stdin().lock(), &mut out)
}

async fn obtain(a: &LiveAuthArgs, out: &mut dyn Write) -> Result<Obtained> {
    match a.target {
        Target::Gdrive | Target::Onedrive => {
            let client = oauth_client(a)?;
            let refresh_token = if a.refresh_token_stdin {
                read_stdin_secret("a refresh token")?
            } else {
                sign_in(&client, a.no_browser, out).await?
            };
            let api = a.api_base.clone().unwrap_or_else(|| {
                if a.target == Target::Gdrive {
                    DRIVE_API
                } else {
                    GRAPH_API
                }
                .to_string()
            });
            refresh_and_verify(a.target, &client, &refresh_token, &api, out).await
        }
        Target::Mega => {
            let cfg = mega_config(a.mega_origin.as_deref());
            let blob = if a.interactive {
                mega_login(cfg.clone()).await?
            } else {
                let dir = match &a.from_saved {
                    Some(d) => d.clone(),
                    None => default_saved_dir()?,
                };
                read_saved(&dir)?
            };
            mega_verify(cfg, &blob, out).await
        }
        Target::GithubToken => {
            let token = prompt_hidden("Fine-grained GitHub token (not shown): ")?;
            let api = a.github_api.as_deref().unwrap_or(GITHUB_API);
            github_verify(api, &a.repo, &token, out).await
        }
    }
}

// ---------------------------------------------------------------- OAuth

fn oauth_client(a: &LiveAuthArgs) -> Result<OAuthClient> {
    let mut client = match a.target {
        Target::Gdrive => {
            let secret = google_secret(
                a.client_json.as_deref(),
                std::env::var(GOOGLE_SECRET_ENV).ok(),
                BETA_DESKTOP_CLIENT_SECRET,
            )?;
            OAuthClient::google(BETA_DESKTOP_CLIENT_ID, secret.expose())
        }
        _ => OAuthClient::microsoft(MICROSOFT_CLIENT_ID),
    };
    if let Some(url) = &a.token_url {
        client.token_url = url.clone();
    }
    Ok(client)
}

/// The beta Desktop client's secret: from `--client-json`, else the run-time
/// variable, else the one built in. The refresh token belongs to this client, and the
/// live runs refresh with this same client.
fn google_secret(
    client_json: Option<&Path>,
    env: Option<String>,
    built_in: Option<&str>,
) -> Result<Secret> {
    if let Some(path) = client_json {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("{}: not JSON (line {})", path.display(), e.line()))?;
        if v.get("web").is_some() {
            bail!(
                "{}: a web client; give the beta project's Desktop client",
                path.display()
            );
        }
        let c = v.get("installed").unwrap_or(&v);
        let id = c.get("client_id").and_then(|s| s.as_str()).unwrap_or("");
        if id != BETA_DESKTOP_CLIENT_ID {
            bail!(
                "{}: client {id:?} is not Bunko's beta Desktop client ({BETA_DESKTOP_CLIENT_ID}), \
                 the client the live runs refresh with",
                path.display()
            );
        }
        let secret = c.get("client_secret").and_then(|s| s.as_str());
        return match secret {
            Some(s) if !s.is_empty() => Ok(Secret::new(s)),
            _ => bail!("{}: no client_secret", path.display()),
        };
    }
    if let Some(s) = env.filter(|s| !s.trim().is_empty()) {
        return Ok(Secret::new(s.trim()));
    }
    match built_in {
        Some(s) => Ok(Secret::new(s)),
        None => bail!(
            "no secret for the beta Desktop client: pass --client-json <its JSON from Google's console>, \
             or set {GOOGLE_SECRET_ENV}"
        ),
    }
}

/// The loopback sign-in: open the browser at the provider, wait for it to come back.
async fn sign_in(client: &OAuthClient, no_browser: bool, out: &mut dyn Write) -> Result<Secret> {
    let (url, pending) = LoopbackFlow::start(client)
        .await
        .map_err(|e| anyhow!("starting the sign-in: {e}"))?;
    // Google issues a refresh token only at a consent; ask for one even when this
    // account consented before.
    let url = if client.provider == bunko_remote::auth::Provider::Google {
        let mut u = reqwest::Url::parse(&url)?;
        u.query_pairs_mut().append_pair("prompt", "consent");
        u.to_string()
    } else {
        url
    };
    writeln!(out, "Sign in with the TEST account at:\n  {url}")?;
    if !no_browser && let Err(e) = open_browser(&url) {
        writeln!(out, "(no browser opened: {e}; open the address yourself)")?;
    }
    writeln!(out, "Waiting for the browser (up to 5 minutes)...")?;
    out.flush()?;
    let tokens = pending
        .wait()
        .await
        .map_err(|e| anyhow!("the sign-in failed: {e}"))?;
    match tokens.refresh_token {
        Some(t) if !t.is_empty() => Ok(Secret::new(t)),
        _ => bail!("the provider gave no refresh token; sign in again"),
    }
}

/// Refresh once (the live runs' first step, so this proves the token works for them
/// and gives Microsoft's rotated token), then one harmless read with the new access
/// token: Drive `about`, Graph `/me/drive`.
pub async fn refresh_and_verify(
    target: Target,
    client: &OAuthClient,
    refresh_token: &Secret,
    api_base: &str,
    out: &mut dyn Write,
) -> Result<Obtained> {
    let label = provider_label(target);
    let t = refresh(client, refresh_token.expose())
        .await
        .map_err(|e| match e {
            AuthError::InvalidGrant => anyhow!(
                "the {label} sign-in is no longer valid (invalid_grant): {}",
                target.reauth_hint()
            ),
            e => anyhow!("refreshing the {label} sign-in failed: {e}"),
        })?;
    let access = Secret::new(t.access_token.clone());
    let newest = Secret::new(
        t.refresh_token
            .clone()
            .unwrap_or_else(|| refresh_token.expose().to_string()),
    );
    let rotated = newest != *refresh_token;
    let base = api_base.trim_end_matches('/');
    let account = match target {
        Target::Gdrive => {
            let v = get_json(
                &format!("{base}/drive/v3/about?fields=user(emailAddress,displayName)"),
                &access,
                label,
            )
            .await?;
            let u = &v["user"];
            u["emailAddress"]
                .as_str()
                .or(u["displayName"].as_str())
                .unwrap_or("(an account without a name)")
                .to_string()
        }
        _ => {
            let v = get_json(&format!("{base}/me/drive"), &access, label).await?;
            let u = &v["owner"]["user"];
            let who = u["email"]
                .as_str()
                .or(u["displayName"].as_str())
                .unwrap_or("(an account without a name)");
            match v["driveType"].as_str() {
                Some(kind) => format!("{who} ({kind} drive)"),
                None => who.to_string(),
            }
        }
    };
    writeln!(
        out,
        "{}: the refresh token works{}; {label} answered for {account}",
        target.name(),
        if rotated {
            " (rotated: the new one is what gets stored)"
        } else {
            ""
        }
    )?;
    Ok(Obtained {
        secret: newest,
        account,
    })
}

fn provider_label(t: Target) -> &'static str {
    match t {
        Target::Gdrive => "Drive",
        Target::Onedrive => "OneDrive",
        Target::Mega => "MEGA",
        Target::GithubToken => "GitHub",
    }
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .user_agent("mokuro-bunko-xtask")
        .build()?)
}

async fn get_json(url: &str, bearer: &Secret, label: &str) -> Result<serde_json::Value> {
    let resp = http()?
        .get(url)
        .bearer_auth(bearer.expose())
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| anyhow!("{label}: {}", e.without_url()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| anyhow!("{label}: {}", e.without_url()))?;
    if !status.is_success() {
        let mut said = super::redact(&body, &[bearer]);
        said.truncate(300);
        bail!("{label} refused the new access token (HTTP {status}): {said}");
    }
    serde_json::from_str(&body).map_err(|e| anyhow!("{label}: an answer that is not JSON ({e})"))
}

// ---------------------------------------------------------------- MEGA

fn mega_config(origin: Option<&str>) -> ClientConfig {
    let mut cfg = ClientConfig::default();
    if let Some(o) = origin {
        cfg.api_origin = o.to_string();
    }
    cfg
}

/// `~/.config/mokuro-bunko-dev/mega-live` (`$XDG_CONFIG_HOME` when set).
fn default_saved_dir() -> Result<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(c) => PathBuf::from(c),
        None => {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .context("no home folder: pass --from-saved <dir>")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(config.join("mokuro-bunko-dev").join("mega-live"))
}

/// The one MEGA session in `<dir>/credentials.json` (Bunko's credentials file; a sealed
/// one opens with MOKURO_SECRETS_KEY).
pub fn read_saved(dir: &Path) -> Result<Secret> {
    let file = dir.join(bunko_remote::auth::credentials::FILE_NAME);
    if !file.is_file() {
        bail!(
            "{}: no saved MEGA session there (pass --from-saved <dir>, or --interactive to sign in)",
            file.display()
        );
    }
    let store = CredentialStore::open(dir).map_err(|e| anyhow!("{}: {e}", file.display()))?;
    let mut sessions = Vec::new();
    for id in store
        .store_ids()
        .map_err(|e| anyhow!("{}: {e}", file.display()))?
    {
        if let Some(c) = store
            .get(&id)
            .map_err(|e| anyhow!("{}: {e}", file.display()))?
            && c.provider == "mega"
            && let Some(s) = c.mega_session.filter(|s| !s.is_empty())
        {
            sessions.push(Secret::new(s));
        }
    }
    match sessions.len() {
        1 => Ok(sessions.remove(0)),
        0 => bail!("{}: no MEGA session in it", file.display()),
        n => bail!(
            "{}: {n} MEGA sessions in it; keep the one to use",
            file.display()
        ),
    }
}

/// A fresh MEGA sign-in, everything asked for on the terminal without echo.
async fn mega_login(cfg: ClientConfig) -> Result<Secret> {
    let email = prompt_hidden("MEGA email (not shown): ")?;
    let password = prompt_hidden("MEGA password (not shown): ")?;
    let login = |mfa: Option<String>| {
        let cfg = cfg.clone();
        let (email, password) = (email.expose().to_string(), password.expose().to_string());
        async move { MegaClient::login(cfg, &email, &password, mfa.as_deref()).await }
    };
    let client = match login(None).await {
        Ok(c) => c,
        Err(MegaError::Api(ErrorCode::EMFAREQUIRED)) => {
            let code = prompt_hidden("MEGA two-factor code (not shown): ")?;
            login(Some(code.expose().to_string()))
                .await
                .map_err(mega_login_err)?
        }
        Err(e) => return Err(mega_login_err(e)),
    };
    let blob = client
        .session()
        .context("MEGA signed in without a session")?
        .serialize()
        .map_err(|e| anyhow!("serialising the MEGA session: {e}"))?;
    Ok(Secret::new(blob))
}

fn mega_login_err(e: MegaError) -> anyhow::Error {
    match e {
        MegaError::Api(ErrorCode::ENOENT | ErrorCode::EARGS) | MegaError::Crypto(_) => {
            anyhow!("MEGA: wrong email or password")
        }
        MegaError::Api(ErrorCode::EMFAREQUIRED) => anyhow!("MEGA: the two-factor code was wrong"),
        e => anyhow!("MEGA sign-in failed: {e}"),
    }
}

/// Resume the session (one `us`) and read the account (`ug`) and its storage (`uq`):
/// no listing of the account's files, no download.
pub async fn mega_verify(
    cfg: ClientConfig,
    blob: &Secret,
    out: &mut dyn Write,
) -> Result<Obtained> {
    let client = MegaClient::resume(cfg, blob.expose())
        .await
        .map_err(|e| match e {
            MegaError::Api(ErrorCode::ESID) => anyhow!(
                "the MEGA session is no longer valid (ESID): {} with --interactive",
                Target::Mega.reauth_hint()
            ),
            e => anyhow!("resuming the MEGA session failed: {e}"),
        })?;
    let user = client
        .user_info()
        .await
        .map_err(|e| anyhow!("MEGA account info: {e}"))?;
    let quota = client
        .storage_quota()
        .await
        .map_err(|e| anyhow!("MEGA storage quota: {e}"))?;
    let gb = |b: u64| b as f64 / (1u64 << 30) as f64;
    let account = if user.email.is_empty() {
        "(an account without an email)".to_string()
    } else {
        user.email
    };
    writeln!(
        out,
        "mega: the session resumes; MEGA answered for {account} ({:.1} of {:.1} GB used)",
        gb(quota.used),
        gb(quota.total)
    )?;
    Ok(Obtained {
        secret: blob.clone(),
        account,
    })
}

// ---------------------------------------------------------------- GitHub

/// The token can read the repository's Actions secrets key, which the Secrets
/// permission gives (and storing a secret needs).
async fn github_verify(
    api: &str,
    repo: &str,
    token: &Secret,
    out: &mut dyn Write,
) -> Result<Obtained> {
    if token.expose().starts_with("ghp_") {
        writeln!(
            out,
            "(this looks like a classic token; a fine-grained one limited to {repo} is safer)"
        )?;
    }
    let url = format!(
        "{}/repos/{repo}/actions/secrets/public-key",
        api.trim_end_matches('/')
    );
    let resp = http()?
        .get(&url)
        .bearer_auth(token.expose())
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|e| anyhow!("GitHub: {}", e.without_url()))?;
    let status = resp.status().as_u16();
    match status {
        200 => {}
        401 => bail!("GitHub refused the token (HTTP 401): wrong or expired"),
        403 | 404 => bail!(
            "the token cannot read {repo}'s Actions secrets (HTTP {status}): give it access to \
             {repo} and the repository permission Secrets, read and write"
        ),
        s => bail!("GitHub answered HTTP {s}"),
    }
    writeln!(
        out,
        "github-token: the token reads {repo}'s Actions secrets key"
    )?;
    Ok(Obtained {
        secret: token.clone(),
        account: repo.to_string(),
    })
}

// ---------------------------------------------------------------- terminal

fn prompt_hidden(prompt: &str) -> Result<Secret> {
    let v = rpassword::prompt_password(prompt).context("reading from the terminal")?;
    let v = v.trim();
    if v.is_empty() {
        bail!("nothing entered");
    }
    Ok(Secret::new(v))
}

fn read_stdin_secret(what: &str) -> Result<Secret> {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s)?;
    let s = s.trim();
    if s.is_empty() {
        bail!("no {what} on stdin");
    }
    Ok(Secret::new(s))
}

/// Open `url` in the default browser (as `mokuro-bunko`'s `open_browser`).
fn open_browser(url: &str) -> std::io::Result<()> {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler").arg(url);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bunko_cloudsim::Provider as Sim;

    const CLIENT_SECRET: &str = "SENTINEL-client-secret-do-not-print";

    fn google(sim: &bunko_cloudsim::CloudSim) -> OAuthClient {
        let mut c = OAuthClient::google(BETA_DESKTOP_CLIENT_ID, CLIENT_SECRET);
        c.token_url = sim.token_url(Sim::Google);
        c
    }

    fn microsoft(sim: &bunko_cloudsim::CloudSim) -> OAuthClient {
        let mut c = OAuthClient::microsoft(MICROSOFT_CLIENT_ID);
        c.token_url = sim.token_url(Sim::Microsoft);
        c
    }

    fn quiet(text: &str, secrets: &[&str]) {
        for s in secrets {
            assert!(!text.contains(s), "a secret in {text:?}");
        }
    }

    #[tokio::test]
    async fn drive_refreshes_and_reads_about() {
        let sim = bunko_cloudsim::start().await;
        let t = sim.token(Sim::Google, "live");
        let mut out = Vec::new();
        let got = refresh_and_verify(
            Target::Gdrive,
            &google(&sim),
            &Secret::new(&t.refresh_token),
            &sim.drive_api_base(),
            &mut out,
        )
        .await
        .unwrap();
        // Google keeps its refresh token.
        assert_eq!(got.secret.expose(), t.refresh_token);
        assert_eq!(got.account, "me@example.com");
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("Drive answered for me@example.com"), "{out}");
        assert!(!out.contains("rotated"));
        quiet(
            &format!("{out}{got:?}"),
            &[&t.refresh_token, &t.access_token, CLIENT_SECRET],
        );
        sim.assert_clean();
    }

    #[tokio::test]
    async fn onedrive_stores_the_rotated_token() {
        let sim = bunko_cloudsim::start().await;
        let t = sim.token(Sim::Microsoft, "live");
        let client = microsoft(&sim);
        let mut out = Vec::new();
        let got = refresh_and_verify(
            Target::Onedrive,
            &client,
            &Secret::new(&t.refresh_token),
            &sim.graph_api_base(),
            &mut out,
        )
        .await
        .unwrap();
        assert_ne!(got.secret.expose(), t.refresh_token, "not rotated");
        assert_eq!(got.account, "me@example.com (personal drive)");
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("rotated"), "{out}");
        quiet(
            &format!("{out}{got:?}"),
            &[&t.refresh_token, got.secret.expose(), &t.access_token],
        );
        // What gets stored is what the next run refreshes with.
        let again = refresh(&client, got.secret.expose()).await.unwrap();
        assert!(again.refresh_token.is_some());
        sim.assert_clean();
    }

    #[tokio::test]
    async fn a_revoked_grant_says_to_sign_in_again() {
        let sim = bunko_cloudsim::start().await;
        let t = sim.token(Sim::Google, "live");
        assert!(sim.revoke(&t.refresh_token));
        let mut out = Vec::new();
        let err = refresh_and_verify(
            Target::Gdrive,
            &google(&sim),
            &Secret::new(&t.refresh_token),
            &sim.drive_api_base(),
            &mut out,
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("invalid_grant"), "{msg}");
        assert!(
            msg.contains("cargo xtask-live interop live-auth gdrive"),
            "{msg}"
        );
        quiet(
            &format!("{msg}{}", String::from_utf8_lossy(&out)),
            &[&t.refresh_token, CLIENT_SECRET],
        );
    }

    #[tokio::test]
    async fn a_refused_access_token_is_reported_without_it() {
        let sim = bunko_cloudsim::start().await;
        let t = sim.token(Sim::Microsoft, "live");
        let mut out = Vec::new();
        // Graph at the Drive prefix: the token is no Graph token there.
        let err = refresh_and_verify(
            Target::Onedrive,
            &microsoft(&sim),
            &Secret::new(&t.refresh_token),
            &format!("{}/nowhere", sim.base),
            &mut out,
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("OneDrive"), "{msg}");
        quiet(&msg, &[&t.refresh_token, &t.access_token]);
        let _ = sim.take_violations();
    }

    fn saved(dir: &Path, sessions: &[&str]) {
        let stores: serde_json::Map<String, serde_json::Value> = sessions
            .iter()
            .enumerate()
            .map(|(i, s)| {
                (
                    format!("store{i}"),
                    serde_json::json!({"provider": "mega", "account_id": "u1", "mega_session": s}),
                )
            })
            .collect();
        let mut doc = serde_json::json!({"version": 1, "stores": stores});
        doc["stores"]["drive"] =
            serde_json::json!({"provider": "gdrive", "refresh_token": "not this one"});
        std::fs::write(
            dir.join("credentials.json"),
            serde_json::to_vec(&doc).unwrap(),
        )
        .unwrap();
    }

    fn fast_mega(sim: &bunko_cloudsim::CloudSim) -> ClientConfig {
        let mut cfg = mega_config(Some(&sim.mega_origin()));
        cfg.min_cs_interval = Duration::ZERO;
        cfg.retry_base = Duration::from_millis(1);
        cfg.max_retries = 0;
        cfg
    }

    #[tokio::test]
    async fn mega_from_a_saved_session() {
        let sim = bunko_cloudsim::start().await;
        let blob = sim.mega("live").session_blob();
        let dir = tempfile::tempdir().unwrap();
        saved(dir.path(), &[&blob]);
        let got = read_saved(dir.path()).unwrap();
        assert_eq!(got.expose(), blob);
        let mut out = Vec::new();
        let ok = mega_verify(fast_mega(&sim), &got, &mut out).await.unwrap();
        assert_eq!(ok.secret.expose(), blob);
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("the session resumes"), "{out}");
        quiet(&format!("{out}{ok:?}"), &[&blob]);
        // No listing of the account, no download: resume, user info, quota.
        let cmds: Vec<String> = sim
            .log()
            .iter()
            .filter(|e| e.provider == Sim::Mega)
            .map(|e| e.class.to_string())
            .collect();
        assert!(cmds.iter().any(|c| c == "auth"), "MEGA calls: {cmds:?}");
        assert!(
            cmds.iter().all(|c| c == "auth" || c == "metadata"),
            "MEGA calls: {cmds:?}"
        );
        sim.assert_clean();
    }

    #[tokio::test]
    async fn a_dead_or_missing_mega_session_fails_without_printing_it() {
        let dir = tempfile::tempdir().unwrap();
        let none = read_saved(dir.path()).unwrap_err().to_string();
        assert!(none.contains("no saved MEGA session"), "{none}");
        saved(dir.path(), &["AAAA", "BBBB"]);
        assert!(
            read_saved(dir.path())
                .unwrap_err()
                .to_string()
                .contains("2 MEGA sessions")
        );

        let sim = bunko_cloudsim::start().await;
        let junk = "AQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let err = mega_verify(fast_mega(&sim), &Secret::new(junk), &mut Vec::new())
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("MEGA"), "{msg}");
        quiet(&msg, &[junk]);
        let _ = sim.take_violations();
    }

    #[test]
    fn the_google_secret_comes_from_the_beta_desktop_client_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.json");
        std::fs::write(
            &p,
            serde_json::json!({"installed": {"client_id": BETA_DESKTOP_CLIENT_ID, "client_secret": CLIENT_SECRET}}).to_string(),
        )
        .unwrap();
        assert_eq!(
            google_secret(Some(&p), None, None).unwrap().expose(),
            CLIENT_SECRET
        );
        std::fs::write(
            &p,
            serde_json::json!({"installed": {"client_id": "another.apps.googleusercontent.com", "client_secret": CLIENT_SECRET}}).to_string(),
        )
        .unwrap();
        let msg = google_secret(Some(&p), None, None).unwrap_err().to_string();
        assert!(msg.contains("not Bunko's beta Desktop client"), "{msg}");
        quiet(&msg, &[CLIENT_SECRET]);
        std::fs::write(
            &p,
            serde_json::json!({"web": {"client_id": "x"}}).to_string(),
        )
        .unwrap();
        assert!(google_secret(Some(&p), None, None).is_err());
        std::fs::write(&p, format!("{{\"client_secret\": \"{CLIENT_SECRET}\"")).unwrap();
        quiet(
            &google_secret(Some(&p), None, None).unwrap_err().to_string(),
            &[CLIENT_SECRET],
        );

        assert_eq!(
            google_secret(None, Some(" s1 ".into()), Some("s2"))
                .unwrap()
                .expose(),
            "s1"
        );
        assert_eq!(
            google_secret(None, Some(String::new()), Some("s2"))
                .unwrap()
                .expose(),
            "s2"
        );
        let msg = google_secret(None, None, None).unwrap_err().to_string();
        assert!(
            msg.contains("--client-json") && msg.contains(GOOGLE_SECRET_ENV),
            "{msg}"
        );
    }
}
