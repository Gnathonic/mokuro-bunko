//! The Connect state machine against a mock authorization server (the pattern of
//! bunko-remote's `tests/auth.rs`) and the in-memory `FakeBackend` for the Check.

use super::*;
use axum::extract::{Form, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use bunko_remote::fake::FakeBackend;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};

// ---------------------------------------------------------------- mock OAuth server

#[derive(Default)]
struct Mock {
    /// Scripted `/token` answers; when empty, `authorization_pending` (HTTP 428).
    token: Mutex<VecDeque<(u16, Value)>>,
    /// Device codes handed out so far (each `/device` call makes `CODE-<n>`).
    codes: Mutex<u32>,
    /// `expires_in` of every device code.
    expires_in: Mutex<u64>,
    seen: Mutex<Vec<(String, HashMap<String, String>)>>,
}

impl Mock {
    fn requests(&self, path: &str) -> Vec<HashMap<String, String>> {
        self.seen
            .lock()
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, f)| f.clone())
            .collect()
    }
}

async fn token(State(m): State<Arc<Mock>>, Form(f): Form<HashMap<String, String>>) -> Response {
    m.seen.lock().push(("/token".into(), f));
    let next = m.token.lock().pop_front();
    match next {
        Some((status, body)) => (StatusCode::from_u16(status).unwrap(), Json(body)).into_response(),
        None => (
            StatusCode::PRECONDITION_REQUIRED,
            Json(json!({"error": "authorization_pending"})),
        )
            .into_response(),
    }
}

async fn device(State(m): State<Arc<Mock>>, Form(f): Form<HashMap<String, String>>) -> Response {
    m.seen.lock().push(("/device".into(), f));
    let n = {
        let mut c = m.codes.lock();
        *c += 1;
        *c
    };
    let expires = *m.expires_in.lock();
    Json(json!({
        "device_code": format!("dev-code-{n}"),
        "user_code": format!("CODE-{n}"),
        "verification_url": "https://www.google.com/device",
        "expires_in": expires,
        "interval": 1,
    }))
    .into_response()
}

async fn mock_server(expires_in: u64) -> (Arc<Mock>, String) {
    let m = Arc::new(Mock::default());
    *m.expires_in.lock() = expires_in;
    let app = Router::new()
        .route("/token", post(token))
        .route("/device", post(device))
        .with_state(m.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (m, base)
}

fn tokens(access: &str, refresh: &str) -> (u16, Value) {
    (
        200,
        json!({"access_token": access, "refresh_token": refresh, "expires_in": 3599, "token_type": "Bearer"}),
    )
}

// ---------------------------------------------------------------- fake providers

/// The Check on a `FakeBackend`; the account it reports can be changed.
struct FakeOps {
    fake: Arc<FakeBackend>,
    account: Mutex<AccountInfo>,
    /// The MEGA login's answer.
    mega: Mutex<Result<String, LoginError>>,
    /// (email, password) of every MEGA login.
    mega_seen: Mutex<Vec<(String, String)>>,
}

impl FakeOps {
    fn new(fake: Arc<FakeBackend>) -> Arc<FakeOps> {
        Arc::new(FakeOps {
            fake,
            account: Mutex::new(AccountInfo {
                id: "acct-1".into(),
                label: "me@example.com".into(),
            }),
            mega: Mutex::new(Ok("mega-session-blob".into())),
            mega_seen: Mutex::new(Vec::new()),
        })
    }
}

impl ProviderOps for FakeOps {
    fn inspect<'a>(
        &'a self,
        _backend: LibraryBackend,
        auth: &'a Auth,
        folder: &'a str,
        create: bool,
        _scratch: &'a Path,
        ours: &'a HashSet<String>,
    ) -> BoxFuture<'a, Result<(AccountInfo, CheckReport), BErr>> {
        Box::pin(async move {
            if let Auth::WebDav { password, .. } = auth
                && password == "wrong"
            {
                return Err(BErr::AuthRevoked);
            }
            let (_, check) = check_backend(&*self.fake, folder, create, ours).await?;
            Ok((self.account.lock().clone(), check))
        })
    }

    fn mega_login<'a>(
        &'a self,
        email: &'a str,
        password: &'a str,
        _totp: Option<&'a str>,
    ) -> BoxFuture<'a, Result<SessionBlob, LoginError>> {
        Box::pin(async move {
            self.mega_seen
                .lock()
                .push((email.to_string(), password.to_string()));
            self.mega.lock().clone().map(SessionBlob::new)
        })
    }
}

fn library() -> Arc<FakeBackend> {
    let fake = Arc::new(FakeBackend::drive_like());
    fake.reader_put("mokuro-reader/Series A/Vol 1.cbz", b"zip");
    fake.reader_put("mokuro-reader/Series A/Vol 1.mokuro", b"{}");
    fake.reader_put("mokuro-reader/Series A/Vol 2.cbz", b"zip");
    fake.reader_put("mokuro-reader/Series B/Vol 1.zip", b"zip");
    fake
}

struct Env {
    dir: tempfile::TempDir,
    lib: Arc<Mutex<LibraryConfig>>,
    applied: Arc<Mutex<Vec<LibraryChoice>>>,
    ops: Arc<FakeOps>,
    connector: Connector,
}

impl Env {
    fn cloud(&self) -> PathBuf {
        self.dir.path().join("cloud")
    }

    fn creds(&self) -> CredentialStore {
        CredentialStore::open_with_key(&self.cloud(), None).unwrap()
    }
}

fn env_with(
    fake: Arc<FakeBackend>,
    lib: LibraryConfig,
    base: Option<String>,
    max_codes: u32,
) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let lib = Arc::new(Mutex::new(lib));
    let applied: Arc<Mutex<Vec<LibraryChoice>>> = Arc::default();
    let ops = FakeOps::new(fake);
    let (l, a) = (lib.clone(), applied.clone());
    let tuning = Tuning {
        time_unit: Duration::from_millis(2),
        max_codes,
        rewrite_client: base.map(|base| {
            Arc::new(move |mut c: OAuthClient| {
                c.auth_url = format!("{base}/authorize");
                c.token_url = format!("{base}/token");
                c.device_url = format!("{base}/device");
                c
            }) as Arc<dyn Fn(OAuthClient) -> OAuthClient + Send + Sync>
        }),
        loopback_timeout: Some(Duration::from_secs(20)),
        paste_timeout: Some(Duration::from_secs(20)),
    };
    let connector = Connector::with_ops(
        dir.path().join("cloud"),
        Arc::new(move || l.lock().clone()),
        Arc::new(move |c: &LibraryChoice| {
            a.lock().push(c.clone());
            Ok(())
        }),
        ops.clone(),
        tuning,
    );
    Env {
        dir,
        lib,
        applied,
        ops,
        connector,
    }
}

fn drive(project: GoogleProject) -> LibraryConfig {
    LibraryConfig {
        backend: LibraryBackend::GDrive,
        google_project: project,
        ..LibraryConfig::default()
    }
}

fn req(provider: &str) -> ConnectRequest {
    ConnectRequest {
        provider: provider.into(),
        ..ConnectRequest::default()
    }
}

async fn settle(c: &Connector) -> ConnectStatus {
    c.wait_for(|s| !s.in_progress(), Duration::from_secs(20))
        .await
}

/// Nothing a page or a log could show carries a secret.
fn assert_no_secret(s: &ConnectStatus, secrets: &[&str]) {
    let text = s.as_json().to_string();
    for secret in secrets {
        assert!(!text.contains(secret), "{secret} leaked into {text}");
    }
}

// ---------------------------------------------------------------- the tests

/// The device flow on Drive with the beta pairing: the live code is shown, the beta TV
/// client signs in, the credentials and `storage.library` are written, the Check counts
/// the folder and (10 volumes or fewer) offers the other pairing.
#[tokio::test]
async fn drive_device_flow_connects_with_the_pairings_client() {
    let (m, base) = mock_server(1800).await;
    let e = env_with(library(), drive(GoogleProject::Release), Some(base), 5);
    m.token
        .lock()
        .push_back((428, json!({"error": "authorization_pending"})));
    m.token.lock().push_back(tokens("at-secret", "rt-secret"));
    let mut r = req("gdrive");
    r.reader = Some("beta".into());
    let started = e.connector.start(r, RequestOrigin::cli()).unwrap();
    assert_eq!(started.state, "starting");
    let code = e
        .connector
        .wait_for(
            |s| s.state == "code" || !s.in_progress(),
            Duration::from_secs(10),
        )
        .await;
    assert_eq!(code.state, "code", "{code:?}");
    assert_eq!(code.flow, Some("device"));
    assert_eq!(code.user_code.as_deref(), Some("CODE-1"));
    assert_eq!(
        code.verification_url.as_deref(),
        Some("https://www.google.com/device")
    );
    assert_eq!(code.code_number, Some(1));
    assert!(code.expires_at.is_some());
    assert_eq!(code.pairing.as_deref(), Some("beta"));

    let done = settle(&e.connector).await;
    assert_eq!(done.state, "connected", "{done:?}");
    assert!(done.needs_restart);
    assert_eq!(done.account.as_deref(), Some("me@example.com"));
    assert_no_secret(&done, &["at-secret", "rt-secret"]);
    // The beta pairing's TV client asked for the code and redeemed it.
    let dev = m.requests("/device");
    assert_eq!(dev.len(), 1);
    assert_eq!(
        dev[0]["client_id"],
        bunko_remote::auth::clients::BETA_DEVICE_CLIENT_ID
    );
    let tok = m.requests("/token");
    assert_eq!(
        tok.last().unwrap()["client_id"],
        bunko_remote::auth::clients::BETA_DEVICE_CLIENT_ID
    );
    // Credentials under the beta store id; config names the beta pairing.
    let id = store_id("gdrive", "beta", "acct-1", "mokuro-reader");
    assert_eq!(done.store_id.as_deref(), Some(id.as_str()));
    let cred = e.creds().get(&id).unwrap().expect("stored");
    assert_eq!(cred.provider, "gdrive");
    assert_eq!(cred.refresh_token.as_deref(), Some("rt-secret"));
    assert_eq!(
        cred.client_id.as_deref(),
        Some(bunko_remote::auth::clients::BETA_DEVICE_CLIENT_ID)
    );
    assert_eq!(cred.account_label.as_deref(), Some("me@example.com"));
    let applied = e.applied.lock().clone();
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].backend, LibraryBackend::GDrive);
    assert_eq!(applied[0].google_project, Some(GoogleProject::Beta));
    let mut lib = LibraryConfig::default();
    applied[0].apply_to(&mut lib);
    assert_eq!(lib.google_project, GoogleProject::Beta);
    assert_eq!(lib.backend, LibraryBackend::GDrive);
    // The Check.
    let check = done.check.unwrap();
    assert!(check.found);
    assert_eq!((check.volumes, check.series), (3, 2));
    assert_eq!(
        check.summary,
        "Found mokuro-reader/ — 3 volumes in 2 series"
    );
    assert_eq!(check.pairing.as_deref(), Some("beta"));
    assert!(check.offer_switch);
    assert_eq!(check.other_pairing.as_deref(), Some("release"));
    assert!(check.warning.unwrap().contains("reader.mokuro.app"));
}

/// An expired code is replaced at once; after five unused codes the sign-in stops and
/// waits for a click; a click starts over.
#[tokio::test]
async fn unused_codes_are_reissued_then_the_sign_in_stops_after_five() {
    let (m, base) = mock_server(3).await;
    let e = env_with(
        library(),
        drive(GoogleProject::Release),
        Some(base),
        MAX_UNUSED_CODES,
    );
    e.connector
        .start(req("gdrive"), RequestOrigin::cli())
        .unwrap();
    let mut seen = Vec::new();
    let end = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let s = e.connector.status();
        if let Some(c) = &s.user_code
            && seen.last() != Some(c)
        {
            seen.push(c.clone());
        }
        if !s.in_progress() || tokio::time::Instant::now() > end {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let s = e.connector.status();
    assert_eq!(s.state, "stopped", "{s:?}");
    assert!(
        s.message.as_deref().unwrap().contains("Click Connect"),
        "{s:?}"
    );
    assert!(s.user_code.is_none());
    assert_eq!(m.requests("/device").len(), MAX_UNUSED_CODES as usize);
    assert_eq!(*seen.last().unwrap(), "CODE-5", "{seen:?}");
    assert!(seen.len() >= 2, "{seen:?}");
    assert!(e.creds().store_ids().unwrap().is_empty());
    assert!(e.applied.lock().is_empty());
    // The click: a new sign-in with new codes.
    m.token.lock().push_back(tokens("at", "rt"));
    let s = e
        .connector
        .start(req("gdrive"), RequestOrigin::cli())
        .unwrap();
    assert!(s.session > 1);
    let done = settle(&e.connector).await;
    assert_eq!(done.state, "connected", "{done:?}");
    assert_eq!(m.requests("/device").len(), 6);
}

/// From a browser on this machine: the loopback flow with PKCE, Microsoft's
/// `http://localhost:<port>` redirect (no path), OneDrive's own client.
#[tokio::test]
async fn a_local_request_signs_in_through_the_loopback_redirect() {
    let (m, base) = mock_server(900).await;
    let fake = Arc::new(FakeBackend::onedrive_like());
    fake.reader_put("mokuro-reader/S/V.cbz", b"z");
    let lib = LibraryConfig {
        backend: LibraryBackend::OneDrive,
        ..LibraryConfig::default()
    };
    let e = env_with(fake, lib, Some(base.clone()), 5);
    m.token.lock().push_back(tokens("ms-at", "ms-rt"));
    let origin = RequestOrigin::of(true, Some("Mozilla/5.0 (X11; Linux x86_64) Firefox/140.0"));
    e.connector.start(req("onedrive"), origin).unwrap();
    let s = e
        .connector
        .wait_for(
            |s| s.state == "browser" || !s.in_progress(),
            Duration::from_secs(10),
        )
        .await;
    assert_eq!(s.state, "browser", "{s:?}");
    assert_eq!(s.flow, Some("loopback"));
    let url = s.auth_url.clone().unwrap();
    assert!(url.starts_with(&format!("{base}/authorize?")), "{url}");
    let q: HashMap<String, String> = reqwest::Url::parse(&url)
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(
        q["client_id"],
        bunko_remote::auth::clients::MICROSOFT_CLIENT_ID
    );
    assert_eq!(q["code_challenge_method"], "S256");
    let redirect = q["redirect_uri"].clone();
    let port: u16 = redirect
        .strip_prefix("http://localhost:")
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("{redirect}"));
    // The browser comes back.
    let b = reqwest::Client::builder().no_proxy().build().unwrap();
    let r = b
        .get(format!(
            "http://127.0.0.1:{port}/?code=the-code&state={}",
            q["state"]
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let done = settle(&e.connector).await;
    assert_eq!(done.state, "connected", "{done:?}");
    assert_no_secret(&done, &["ms-at", "ms-rt"]);
    let f = &m.requests("/token")[0];
    assert_eq!(f["grant_type"], "authorization_code");
    assert!(!f.contains_key("client_secret"));
    assert!(m.requests("/device").is_empty());
    let id = store_id("onedrive", "", "acct-1", "mokuro-reader");
    assert!(e.creds().get(&id).unwrap().is_some());
    assert!(done.check.unwrap().pairing.is_none());
}

/// The flow: loopback only for a loopback request from a real browser; an Android
/// WebView, a LAN browser or "Use a code instead" get the device flow.
#[tokio::test]
async fn the_flow_follows_where_the_request_comes_from() {
    let (_m, base) = mock_server(1800).await;
    for (origin, flow_field, want) in [
        (
            RequestOrigin::of(
                true,
                Some("Mozilla/5.0 (Linux; Android 14; wv) AppleWebKit; wv)"),
            ),
            None,
            "code",
        ),
        (RequestOrigin::of(false, Some("Mozilla/5.0")), None, "code"),
        (
            RequestOrigin::of(true, Some("Mozilla/5.0")),
            Some("device"),
            "code",
        ),
        (
            RequestOrigin::of(true, Some("Mozilla/5.0")),
            None,
            "browser",
        ),
    ] {
        let e = env_with(
            library(),
            drive(GoogleProject::Release),
            Some(base.clone()),
            5,
        );
        let mut r = req("gdrive");
        r.flow = flow_field.map(str::to_string);
        e.connector.start(r, origin).unwrap();
        let s = e
            .connector
            .wait_for(
                |s| matches!(s.state, "code" | "browser") || !s.in_progress(),
                Duration::from_secs(10),
            )
            .await;
        assert_eq!(s.state, want, "{origin:?} {flow_field:?}: {s:?}");
        if want == "browser" {
            let q: HashMap<String, String> = reqwest::Url::parse(s.auth_url.as_deref().unwrap())
                .unwrap()
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            // The release Desktop client, its redirect on 127.0.0.1.
            assert_eq!(
                q["client_id"],
                bunko_remote::auth::clients::RELEASE_DESKTOP_CLIENT_ID
            );
            assert!(q["redirect_uri"].starts_with("http://127.0.0.1:"));
        }
        e.connector.cancel();
        assert_eq!(e.connector.status().state, "cancelled");
    }
}

// ---------------------------------------------------------------- the paste flow

fn onedrive_env(base: &str) -> Env {
    let fake = Arc::new(FakeBackend::onedrive_like());
    fake.reader_put("mokuro-reader/S/V.cbz", b"z");
    let lib = LibraryConfig {
        backend: LibraryBackend::OneDrive,
        ..LibraryConfig::default()
    };
    env_with(fake, lib, Some(base.to_string()), 5)
}

fn lan_browser() -> RequestOrigin {
    RequestOrigin::of(false, Some("Mozilla/5.0 (X11; Linux x86_64) Firefox/140.0"))
}

async fn until_paste(c: &Connector) -> ConnectStatus {
    c.wait_for(
        |s| s.state == "paste" || !s.in_progress(),
        Duration::from_secs(10),
    )
    .await
}

fn query_of(url: &str) -> HashMap<String, String> {
    reqwest::Url::parse(url)
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// OneDrive from a LAN browser (or Docker, a NAS): no device code (personal accounts
/// cannot use one) but the pasted redirect. The authorization URL is the loopback one;
/// a wrong paste is answered and waited past; the right one is redeemed with the PKCE
/// verifier and the same redirect URI; nothing secret shows.
#[tokio::test]
async fn onedrive_from_elsewhere_signs_in_with_the_pasted_address() {
    let (m, base) = mock_server(900).await;
    let e = onedrive_env(&base);
    m.token.lock().push_back(tokens("paste-at", "paste-rt"));
    e.connector.start(req("onedrive"), lan_browser()).unwrap();
    let s = until_paste(&e.connector).await;
    assert_eq!(s.state, "paste", "{s:?}");
    assert_eq!(s.flow, Some("paste"));
    assert!(s.in_progress());
    assert!(s.expires_at.is_some());
    let url = s.auth_url.clone().unwrap();
    assert!(url.starts_with(&format!("{base}/authorize?")), "{url}");
    let q = query_of(&url);
    assert_eq!(
        q["client_id"],
        bunko_remote::auth::clients::MICROSOFT_CLIENT_ID
    );
    let redirect = s.redirect_uri.clone().unwrap();
    assert_eq!(q["redirect_uri"], redirect);
    assert!(redirect.starts_with("http://localhost:"), "{redirect}");
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["scope"].contains("Files.ReadWrite") && q["scope"].contains("offline_access"));
    assert!(s.message.as_deref().unwrap().contains(&redirect));
    assert!(m.requests("/device").is_empty());

    // Not the redirect, another sign-in's state, no code: refused, still waiting.
    for wrong in [
        "hello".to_string(),
        format!("https://evil.example/?code=c&state={}", q["state"]),
        format!("{redirect}/?code=c&state=another"),
        format!("{redirect}/"),
    ] {
        let err = e.connector.paste(wrong.clone()).await.unwrap_err();
        assert!(err.starts_with("Not taken: "), "{wrong}: {err}");
        assert_eq!(e.connector.status().state, "paste");
    }
    assert!(m.requests("/token").is_empty());

    let st = e
        .connector
        .paste(format!(
            "  {redirect}/?code=the-code&state={}&session_state=x\n",
            q["state"]
        ))
        .await
        .unwrap();
    assert!(matches!(st.state, "checking" | "connected"), "{st:?}");
    let done = settle(&e.connector).await;
    assert_eq!(done.state, "connected", "{done:?}");
    assert_eq!(done.flow, Some("paste"));
    assert!(done.redirect_uri.is_none() && done.auth_url.is_none());
    assert_no_secret(&done, &["paste-at", "paste-rt", "the-code"]);
    let f = &m.requests("/token")[0];
    assert_eq!(f["grant_type"], "authorization_code");
    assert_eq!(f["code"], "the-code");
    assert_eq!(f["redirect_uri"], redirect);
    assert_eq!(
        bunko_remote::auth::oauth::pkce_challenge(&f["code_verifier"]),
        q["code_challenge"]
    );
    assert!(!f.contains_key("client_secret"));
    let id = store_id("onedrive", "", "acct-1", "mokuro-reader");
    let cred = e.creds().get(&id).unwrap().expect("stored");
    assert_eq!(cred.refresh_token.as_deref(), Some("paste-rt"));
    // The sign-in is over: another paste is refused.
    let err = e
        .connector
        .paste(format!("{redirect}/?code=again&state={}", q["state"]))
        .await
        .unwrap_err();
    assert!(err.contains("No sign-in is waiting"), "{err}");
    assert_eq!(m.requests("/token").len(), 1);
}

/// `error=access_denied` with this sign-in's state ends it; a failed redemption is
/// reported like the loopback flow's.
#[tokio::test]
async fn a_refused_paste_ends_the_sign_in() {
    let (m, base) = mock_server(900).await;
    let e = onedrive_env(&base);
    e.connector.start(req("onedrive"), lan_browser()).unwrap();
    let s = until_paste(&e.connector).await;
    let (redirect, state) = (
        s.redirect_uri.clone().unwrap(),
        query_of(s.auth_url.as_deref().unwrap())["state"].clone(),
    );
    let st = e
        .connector
        .paste(format!("{redirect}/?error=access_denied&state={state}"))
        .await
        .unwrap();
    assert_eq!(st.state, "failed", "{st:?}");
    assert!(st.error.unwrap().contains("refused or cancelled"));
    assert!(
        e.connector
            .paste(format!("{redirect}/?code=c&state={state}"))
            .await
            .is_err()
    );
    assert!(m.requests("/token").is_empty());

    // The provider refuses the code.
    m.token.lock().push_back((
        400,
        json!({"error": "invalid_grant", "error_description": "AADSTS54005: used"}),
    ));
    e.connector.start(req("onedrive"), lan_browser()).unwrap();
    let s = until_paste(&e.connector).await;
    let (redirect, state) = (
        s.redirect_uri.clone().unwrap(),
        query_of(s.auth_url.as_deref().unwrap())["state"].clone(),
    );
    e.connector
        .paste(format!("{redirect}/?code=c&state={state}"))
        .await
        .unwrap();
    let done = settle(&e.connector).await;
    assert_eq!(done.state, "failed", "{done:?}");
    assert!(done.error.unwrap().contains("OneDrive sign-in failed"));
    assert!(e.creds().store_ids().unwrap().is_empty());
}

/// The pasted-address sign-in times out; Cancel ends it too; a new sign-in's paste
/// does not accept the old one's address.
#[tokio::test]
async fn a_paste_times_out_or_is_cancelled() {
    let (_m, base) = mock_server(900).await;
    let mut e = onedrive_env(&base);
    // A short timeout: rebuild the connector with it.
    let lib = e.lib.clone();
    let (l, a) = (lib.clone(), e.applied.clone());
    e.connector = Connector::with_ops(
        e.cloud(),
        Arc::new(move || l.lock().clone()),
        Arc::new(move |c: &LibraryChoice| {
            a.lock().push(c.clone());
            Ok(())
        }),
        e.ops.clone(),
        Tuning {
            rewrite_client: {
                let base = base.clone();
                Some(Arc::new(move |mut c: OAuthClient| {
                    c.auth_url = format!("{base}/authorize");
                    c.token_url = format!("{base}/token");
                    c
                }))
            },
            paste_timeout: Some(Duration::from_millis(300)),
            ..Tuning::default()
        },
    );
    e.connector.start(req("onedrive"), lan_browser()).unwrap();
    let s = until_paste(&e.connector).await;
    assert_eq!(s.state, "paste");
    let (redirect, state) = (
        s.redirect_uri.clone().unwrap(),
        query_of(s.auth_url.as_deref().unwrap())["state"].clone(),
    );
    let done = settle(&e.connector).await;
    assert_eq!(done.state, "failed", "{done:?}");
    assert!(done.error.unwrap().contains("timed out"));
    assert!(
        e.connector
            .paste(format!("{redirect}/?code=c&state={state}"))
            .await
            .is_err()
    );

    e.connector.start(req("onedrive"), lan_browser()).unwrap();
    let s = until_paste(&e.connector).await;
    // The previous sign-in's address is another sign-in's state.
    let err = e
        .connector
        .paste(format!(
            "{}/?code=c&state={state}",
            s.redirect_uri.as_deref().unwrap()
        ))
        .await
        .unwrap_err();
    assert!(err.contains("another sign-in"), "{err}");
    e.connector.cancel();
    assert_eq!(e.connector.status().state, "cancelled");
    assert!(e.connector.status().auth_url.is_none());
    let err = e
        .connector
        .paste("http://localhost:1/?code=c&state=s".into())
        .await
        .unwrap_err();
    assert!(err.contains("No sign-in is waiting"), "{err}");
}

/// Which flow runs, by provider, origin and the request's `flow`.
#[tokio::test]
async fn the_flow_by_provider_origin_and_request() {
    let (_m, base) = mock_server(900).await;
    let local = RequestOrigin::of(true, Some("Mozilla/5.0"));
    let webview = RequestOrigin::of(
        true,
        Some("Mozilla/5.0 (Linux; Android 14; wv) AppleWebKit; wv)"),
    );
    for (provider, origin, flow, want) in [
        ("onedrive", lan_browser(), None, "paste"),
        ("onedrive", lan_browser(), Some("device"), "code"),
        ("onedrive", lan_browser(), Some("paste"), "paste"),
        ("onedrive", webview, None, "paste"),
        ("onedrive", local, None, "browser"),
        ("onedrive", local, Some("paste"), "paste"),
        ("onedrive", RequestOrigin::cli(), None, "paste"),
        ("onedrive", RequestOrigin::cli(), Some("browser"), "browser"),
        (
            "onedrive",
            RequestOrigin::cli(),
            Some("loopback"),
            "browser",
        ),
        ("onedrive", RequestOrigin::cli(), Some("paste"), "paste"),
        ("onedrive", RequestOrigin::cli(), Some("device"), "code"),
        ("gdrive", lan_browser(), None, "code"),
        ("gdrive", lan_browser(), Some("paste"), "paste"),
        ("gdrive", RequestOrigin::cli(), None, "code"),
        ("gdrive", RequestOrigin::cli(), Some("loopback"), "browser"),
        ("gdrive", local, None, "browser"),
        // A LAN browser cannot come back to the server's loopback.
        ("onedrive", lan_browser(), Some("loopback"), "failed"),
    ] {
        let e = if provider == "onedrive" {
            onedrive_env(&base)
        } else {
            env_with(
                library(),
                drive(GoogleProject::Release),
                Some(base.clone()),
                5,
            )
        };
        let mut r = req(provider);
        r.flow = flow.map(str::to_string);
        e.connector.start(r, origin).unwrap();
        let s = e
            .connector
            .wait_for(
                |s| matches!(s.state, "code" | "browser" | "paste") || !s.in_progress(),
                Duration::from_secs(10),
            )
            .await;
        assert_eq!(s.state, want, "{provider} {origin:?} {flow:?}: {s:?}");
        if want == "paste" {
            let q = query_of(s.auth_url.as_deref().unwrap());
            assert_eq!(Some(&q["redirect_uri"]), s.redirect_uri.as_ref());
            let r = &q["redirect_uri"];
            assert!(
                if provider == "onedrive" {
                    r.starts_with("http://localhost:")
                } else {
                    r.starts_with("http://127.0.0.1:") && r.ends_with('/')
                },
                "{r}"
            );
        }
        if want == "failed" {
            assert!(s.error.unwrap().contains("paste the address"));
        }
        e.connector.cancel();
    }
    let e = onedrive_env(&base);
    let mut r = req("onedrive");
    r.flow = Some("magic".into());
    assert!(
        e.connector
            .start(r, lan_browser())
            .unwrap_err()
            .contains("loopback, device or paste")
    );
}

/// AADSTS70002: a personal Microsoft account's device code fails at the poll with the
/// "use the browser" error, and the state suggests the flow that works from there.
#[tokio::test]
async fn a_personal_account_code_suggests_the_browser() {
    let refused = json!({"error": "invalid_client",
        "error_description": "AADSTS70002: The client does not exist or is not enabled for consumers. Trace ID: t"});
    for (origin, suggest) in [(lan_browser(), "paste"), (RequestOrigin::cli(), "loopback")] {
        let (m, base) = mock_server(900).await;
        let e = onedrive_env(&base);
        m.token.lock().push_back((400, refused.clone()));
        let mut r = req("onedrive");
        r.flow = Some("device".into());
        e.connector.start(r, origin).unwrap();
        let s = settle(&e.connector).await;
        assert_eq!(s.state, "failed", "{s:?}");
        assert_eq!(
            s.error.as_deref(),
            Some(bunko_remote::auth::oauth::PERSONAL_ACCOUNT_NO_CODE)
        );
        assert_eq!(s.suggest_flow, Some(suggest));
        assert!(s.user_code.is_none());
        // A new sign-in starts without the suggestion.
        e.connector.start(req("onedrive"), origin).unwrap();
        assert_eq!(e.connector.status().suggest_flow, None);
        e.connector.cancel();
    }
}

/// D25: overrides replace both embedded clients and make the pairing `custom`; the
/// reader question does not apply. A device-only override cannot run the loopback
/// flow, and a loopback-only one refuses a remote request.
#[tokio::test]
async fn overrides_make_the_pairing_custom() {
    let (m, base) = mock_server(1800).await;
    let mut lib = drive(GoogleProject::Custom);
    lib.oauth.google.device_client_id = Some("dev-device".into());
    lib.oauth.google.device_client_secret = Some("dev-secret".into());
    let e = env_with(library(), lib.clone(), Some(base.clone()), 5);
    m.token.lock().push_back(tokens("a", "r"));
    let mut r = req("gdrive");
    r.reader = Some("beta".into()); // ignored: the overrides decide
    e.connector
        .start(r, RequestOrigin::of(true, Some("Mozilla/5.0")))
        .unwrap();
    let done = settle(&e.connector).await;
    assert_eq!(done.state, "connected", "{done:?}");
    assert_eq!(done.pairing.as_deref(), Some("custom:dev-device"));
    assert_eq!(m.requests("/device")[0]["client_id"], "dev-device");
    let id = store_id("gdrive", "custom:dev-device", "acct-1", "mokuro-reader");
    assert!(e.creds().get(&id).unwrap().is_some());
    assert_eq!(e.applied.lock()[0].google_project, None);
    assert!(
        !done.check.unwrap().offer_switch,
        "no other pairing to offer"
    );

    // A loopback-only override: a remote request pastes the redirect instead of a
    // code; asking for a code by name fails.
    let mut lib = drive(GoogleProject::Custom);
    lib.oauth.google.client_id = Some("dev-desktop".into());
    lib.oauth.google.client_secret = Some("s".into());
    let e = env_with(library(), lib.clone(), Some(base.clone()), 5);
    e.connector
        .start(req("gdrive"), RequestOrigin::cli())
        .unwrap();
    let s = e
        .connector
        .wait_for(
            |s| s.state == "paste" || !s.in_progress(),
            Duration::from_secs(10),
        )
        .await;
    assert_eq!(s.state, "paste", "{s:?}");
    let mut r = req("gdrive");
    r.flow = Some("device".into());
    e.connector.start(r, RequestOrigin::cli()).unwrap();
    let s = settle(&e.connector).await;
    assert_eq!(s.state, "failed");
    assert!(s.error.unwrap().contains("device client"));
    // A device-only override cannot paste either.
    let mut lib = drive(GoogleProject::Custom);
    lib.oauth.google.device_client_id = Some("dev-device".into());
    lib.oauth.google.device_client_secret = Some("dev-secret".into());
    let e = env_with(library(), lib, Some(base), 5);
    let mut r = req("gdrive");
    r.flow = Some("paste".into());
    e.connector.start(r, RequestOrigin::cli()).unwrap();
    let s = settle(&e.connector).await;
    assert_eq!(s.state, "failed");
    assert!(
        s.error.as_deref().unwrap().contains("no Desktop client"),
        "{s:?}"
    );
}

/// §7: a library of another account that ran here is not replaced unless the admin
/// chooses "use this account's library instead", which disconnects it first.
#[tokio::test]
async fn another_account_needs_the_admins_choice() {
    let (m, base) = mock_server(1800).await;
    let e = env_with(library(), drive(GoogleProject::Release), Some(base), 5);
    let old = store_id("gdrive", "release", "acct-old", "mokuro-reader");
    e.creds()
        .put(
            &old,
            StoredCredential {
                provider: "gdrive".into(),
                account_id: Some("acct-old".into()),
                account_label: Some("old@example.com".into()),
                refresh_token: Some("old-rt".into()),
                client_id: Some(bunko_remote::auth::clients::RELEASE_DEVICE_CLIENT_ID.into()),
                ..Default::default()
            },
        )
        .unwrap();
    bunko_store::fs::create_dir_all(e.cloud().join(&old)).unwrap();
    bunko_store::fs::write(e.cloud().join(&old).join("state.sqlite"), b"").unwrap();
    m.token.lock().push_back(tokens("a", "r"));
    e.connector
        .start(req("gdrive"), RequestOrigin::cli())
        .unwrap();
    let s = settle(&e.connector).await;
    assert_eq!(s.state, "account-mismatch", "{s:?}");
    assert_eq!(s.previous_account.as_deref(), Some("old@example.com"));
    assert_eq!(s.account.as_deref(), Some("me@example.com"));
    // Nothing written yet.
    assert_eq!(e.creds().store_ids().unwrap(), vec![old.clone()]);
    assert!(e.applied.lock().is_empty());
    // Without a way to disconnect the old store, the choice fails.
    let err = e.connector.decide_account(true).await.unwrap_err();
    assert!(err.contains("disconnect it first"), "{err}");

    // Again, with the owner's Disconnect hook.
    m.token.lock().push_back(tokens("a2", "r2"));
    let released: Arc<Mutex<Vec<String>>> = Arc::default();
    let rel = released.clone();
    e.connector.set_release_old(Arc::new(move |id: &str| {
        rel.lock().push(id.to_string());
        Ok(())
    }));
    e.connector
        .start(req("gdrive"), RequestOrigin::cli())
        .unwrap();
    assert_eq!(settle(&e.connector).await.state, "account-mismatch");
    let s = e.connector.decide_account(true).await.unwrap();
    assert_eq!(s.state, "connected", "{s:?}");
    assert_eq!(*released.lock(), vec![old.clone()]);
    let ids = e.creds().store_ids().unwrap();
    assert_eq!(
        ids,
        vec![store_id("gdrive", "release", "acct-1", "mokuro-reader")]
    );
    assert!(bunko_store::fs::is_file(
        e.cloud().join(&old).join(DISCONNECTED_MARK)
    ));

    // "No": the sign-in is dropped.
    m.token.lock().push_back(tokens("a3", "r3"));
    *e.ops.account.lock() = AccountInfo {
        id: "acct-3".into(),
        label: "third@example.com".into(),
    };
    let cur = store_id("gdrive", "release", "acct-1", "mokuro-reader");
    bunko_store::fs::create_dir_all(e.cloud().join(&cur)).unwrap();
    bunko_store::fs::write(e.cloud().join(&cur).join("state.sqlite"), b"").unwrap();
    e.connector
        .start(req("gdrive"), RequestOrigin::cli())
        .unwrap();
    assert_eq!(settle(&e.connector).await.state, "account-mismatch");
    assert_eq!(
        e.connector.decide_account(false).await.unwrap().state,
        "cancelled"
    );
    assert_eq!(e.creds().store_ids().unwrap(), vec![cur]);
}

/// The same account again ("Sign in again"): the tokens of its store are replaced.
/// A store of the other pairing that never ran (the wizard's "try the other
/// pairing") goes silently; one that ran is a different library: disconnect first.
#[tokio::test]
async fn reconnects_and_pairing_switches() {
    let (m, base) = mock_server(1800).await;
    let e = env_with(library(), drive(GoogleProject::Release), Some(base), 5);
    for (at, rt) in [("a1", "r1"), ("a2", "r2")] {
        m.token.lock().push_back(tokens(at, rt));
        e.connector
            .start(req("gdrive"), RequestOrigin::cli())
            .unwrap();
        assert_eq!(settle(&e.connector).await.state, "connected");
    }
    let release = store_id("gdrive", "release", "acct-1", "mokuro-reader");
    assert_eq!(e.creds().store_ids().unwrap(), vec![release.clone()]);
    assert_eq!(
        e.creds()
            .get(&release)
            .unwrap()
            .unwrap()
            .refresh_token
            .as_deref(),
        Some("r2")
    );

    // Try the other pairing: the release store never ran, so it is replaced.
    m.token.lock().push_back(tokens("b1", "rb1"));
    let mut r = req("gdrive");
    r.reader = Some("beta".into());
    e.connector.start(r.clone(), RequestOrigin::cli()).unwrap();
    assert_eq!(settle(&e.connector).await.state, "connected");
    let beta = store_id("gdrive", "beta", "acct-1", "mokuro-reader");
    assert_eq!(e.creds().store_ids().unwrap(), vec![beta.clone()]);

    // The beta store runs; switching back is refused until it is disconnected.
    bunko_store::fs::create_dir_all(e.cloud().join(&beta)).unwrap();
    bunko_store::fs::write(e.cloud().join(&beta).join("state.sqlite"), b"").unwrap();
    m.token.lock().push_back(tokens("c1", "rc1"));
    e.connector
        .start(req("gdrive"), RequestOrigin::cli())
        .unwrap();
    let s = settle(&e.connector).await;
    assert_eq!(s.state, "failed");
    let err = s.error.unwrap();
    assert!(
        err.contains("pairing") && err.contains("Disconnect it first"),
        "{err}"
    );
    assert_eq!(e.creds().store_ids().unwrap(), vec![beta]);

    // Another provider on top of a running library: refused too.
    let w = ConnectRequest {
        provider: "webdav".into(),
        webdav: Some(WebDavForm {
            url: "https://dav.example/remote.php/dav/files/me".into(),
            username: "me".into(),
            password: "app-pass".into(),
            allow_http: false,
        }),
        ..ConnectRequest::default()
    };
    e.connector.start(w, RequestOrigin::cli()).unwrap();
    let s = settle(&e.connector).await;
    assert_eq!(s.state, "failed");
    assert!(s.error.unwrap().contains("different library"));
}

/// MEGA: email, password and 2FA once; the session blob is kept, the password never.
#[tokio::test]
async fn mega_keeps_the_session_never_the_password() {
    let fake = Arc::new(FakeBackend::mega_like());
    fake.reader_put("mokuro-reader/S/V.cbz", b"z");
    let e = env_with(fake, LibraryConfig::default(), None, 5);
    let r = ConnectRequest {
        provider: "mega".into(),
        mega: Some(MegaForm {
            email: " me@example.com ".into(),
            password: "hunter2-password".into(),
            totp: Some("123456".into()),
        }),
        ..ConnectRequest::default()
    };
    assert!(!format!("{r:?}").contains("hunter2"));
    e.connector.start(r, RequestOrigin::cli()).unwrap();
    let s = settle(&e.connector).await;
    assert_eq!(s.state, "connected", "{s:?}");
    assert_eq!(s.flow, Some("form"));
    assert_no_secret(&s, &["hunter2", "mega-session-blob"]);
    assert_eq!(e.ops.mega_seen.lock()[0].0, "me@example.com");
    let id = store_id("mega", "", "acct-1", "mokuro-reader");
    let cred = e.creds().get(&id).unwrap().unwrap();
    assert_eq!(cred.mega_session.as_deref(), Some("mega-session-blob"));
    let file = bunko_store::fs::read_to_string(e.cloud().join("credentials.json")).unwrap();
    assert!(!file.contains("hunter2"), "the MEGA password was stored");
    // 2FA needed: said so.
    *e.ops.mega.lock() = Err(LoginError::MfaRequired);
    let r = ConnectRequest {
        provider: "mega".into(),
        mega: Some(MegaForm {
            email: "me@example.com".into(),
            password: "pw".into(),
            totp: None,
        }),
        ..ConnectRequest::default()
    };
    e.connector.start(r, RequestOrigin::cli()).unwrap();
    let s = settle(&e.connector).await;
    assert_eq!(s.state, "failed");
    assert!(s.error.unwrap().contains("two-factor"));
    // An empty form never starts.
    assert!(
        e.connector
            .start(req("mega"), RequestOrigin::cli())
            .is_err()
    );
}

/// WebDAV: URL, user and app password with the live probe; `http://` refused unless
/// allowed; the password goes to the credentials file, the URL and user to config.
#[tokio::test]
async fn webdav_connects_with_its_form() {
    let fake = Arc::new(FakeBackend::webdav_cas());
    let e = env_with(fake, LibraryConfig::default(), None, 5);
    let form = |url: &str, pw: &str, allow_http: bool| ConnectRequest {
        provider: "webdav".into(),
        webdav: Some(WebDavForm {
            url: url.into(),
            username: "me".into(),
            password: pw.into(),
            allow_http,
        }),
        ..ConnectRequest::default()
    };
    e.connector
        .start(
            form("http://nas.local/dav", "app-pass", false),
            RequestOrigin::cli(),
        )
        .unwrap();
    let s = settle(&e.connector).await;
    assert_eq!(s.state, "failed");
    assert!(s.error.unwrap().contains("http://"));
    for url in ["http://192.168.1.5/dav", "http://localhost.example/dav"] {
        e.connector
            .start(form(url, "app-pass", false), RequestOrigin::cli())
            .unwrap();
        let s = settle(&e.connector).await;
        assert_eq!(s.state, "failed", "{url}");
        assert!(s.error.unwrap().contains("http://"), "{url}");
    }
    e.connector
        .start(
            form("https://nas.local/dav", "wrong", false),
            RequestOrigin::cli(),
        )
        .unwrap();
    let s = settle(&e.connector).await;
    assert_eq!(s.state, "failed");
    assert!(s.error.unwrap().contains("refused the sign-in"));
    e.connector
        .start(
            form("https://nas.local/dav", "app-pass", false),
            RequestOrigin::cli(),
        )
        .unwrap();
    let s = settle(&e.connector).await;
    assert_eq!(s.state, "connected", "{s:?}");
    assert_no_secret(&s, &["app-pass"]);
    let id = store_id("webdav", "", "me@https://nas.local/dav", "mokuro-reader");
    assert_eq!(
        e.creds()
            .get(&id)
            .unwrap()
            .unwrap()
            .webdav_password
            .as_deref(),
        Some("app-pass")
    );
    let c = e.applied.lock().last().cloned().unwrap();
    assert_eq!(c.webdav_url.as_deref(), Some("https://nas.local/dav"));
    assert_eq!(c.webdav_username.as_deref(), Some("me"));
    // The id the server opens it by is the same (config + the password on file).
    let mut lib = e.lib.lock().clone();
    c.apply_to(&mut lib);
    assert_eq!(lib.webdav.url.as_deref(), Some("https://nas.local/dav"));
    // "Create it?": no folder yet; Create makes it.
    let check = s.check.unwrap();
    assert!(!check.found);
    assert_eq!(check.summary, "No mokuro-reader/ folder yet. Create it?");
    let check = e.connector.create_folder().await.unwrap();
    assert!(check.found && check.created, "{check:?}");
    assert_eq!(e.connector.status().check.unwrap(), check);
}

/// A WebDAV server on this machine (localhost, 127.0.0.0/8, ::1) connects over plain
/// http without "Allow http": nothing travels over a network. The config it writes
/// keeps `allow_http` off.
#[tokio::test]
async fn a_loopback_webdav_url_needs_no_allow_http() {
    for url in [
        "http://127.0.0.1:8080/dav",
        "http://localhost:8080/dav",
        "http://[::1]:8080/dav",
    ] {
        let fake = Arc::new(FakeBackend::webdav_cas());
        let e = env_with(fake, LibraryConfig::default(), None, 5);
        e.connector
            .start(
                ConnectRequest {
                    provider: "webdav".into(),
                    webdav: Some(WebDavForm {
                        url: url.into(),
                        username: "me".into(),
                        password: "app-pass".into(),
                        allow_http: false,
                    }),
                    ..ConnectRequest::default()
                },
                RequestOrigin::cli(),
            )
            .unwrap();
        let s = settle(&e.connector).await;
        assert_eq!(s.state, "connected", "{url}: {s:?}");
        let c = e.applied.lock().last().cloned().unwrap();
        assert!(!c.webdav_allow_http);
        let mut lib = e.lib.lock().clone();
        c.apply_to(&mut lib);
        assert_eq!(lib.webdav.url.as_deref(), Some(url));
        assert!(!lib.webdav.allow_http);
        // That config passes validation as written (bunko-core exempts loopback too).
        assert!(bunko_core::config::is_loopback_http_url(url));
    }
}

/// The Check names another Bunko's fresh lease, and ignores this machine's own.
#[tokio::test]
async fn the_check_reports_another_bunkos_lease() {
    use bunko_remote::status::{MARKER_KIND, MARKER_NAME, Marker, MarkerState};
    let fake = library();
    let marker = Marker {
        version: 1,
        kind: MARKER_KIND.into(),
        instance_id: "other-install".into(),
        boot_id: "b".into(),
        instance_name: "nas".into(),
        bunko_version: "0.8.0".into(),
        epoch: 1,
        claimed_at: String::new(),
        heartbeat_at: String::new(),
        ttl_s: 900,
        state: MarkerState::Active,
        compiles: vec![],
        ocr: true,
        queue: None,
        took_over_from: None,
    };
    fake.reader_put(
        &format!("mokuro-reader/{MARKER_NAME}"),
        &serde_json::to_vec(&marker).unwrap(),
    );
    let (_, check) = check_backend(&*fake, "mokuro-reader", false, &HashSet::new())
        .await
        .unwrap();
    assert!(
        check
            .lease
            .as_deref()
            .unwrap()
            .contains("Another Bunko (nas)"),
        "{check:?}"
    );
    let ours: HashSet<String> = ["other-install".to_string()].into();
    let (_, check) = check_backend(&*fake, "mokuro-reader", false, &ours)
        .await
        .unwrap();
    assert!(check.lease.is_none());
}

#[test]
fn pairings_and_providers_read_well() {
    assert_eq!(
        pairing_of_client(bunko_remote::auth::clients::BETA_DESKTOP_CLIENT_ID),
        Some(GoogleProject::Beta)
    );
    assert_eq!(
        pairing_of_client(bunko_remote::auth::clients::RELEASE_DEVICE_CLIENT_ID),
        Some(GoogleProject::Release)
    );
    assert_eq!(pairing_of_client("dev"), None);
    assert_eq!(pairing_label("custom:abc"), "custom (abc)");
    assert!(is_webview(
        "Mozilla/5.0 (Linux; Android 14; Pixel; wv) AppleWebKit/537.36"
    ));
    assert!(!is_webview("Mozilla/5.0 (X11; Linux x86_64)"));
    assert!(is_volume(b"Vol 1.CBZ"));
    assert!(!is_volume(b"Vol 1.mokuro"));
    let mut r = CheckReport {
        folder: "mokuro-reader".into(),
        found: true,
        roots: 3,
        volumes: 1,
        series: 1,
        ..Default::default()
    };
    r.finish();
    assert_eq!(
        r.summary,
        "Found mokuro-reader/ — 1 volume in 1 series (3 folders of that name, merged into one library)"
    );
    // More than ten volumes: no offer.
    let mut big = CheckReport {
        found: true,
        volumes: 11,
        ..Default::default()
    };
    big.pairing_advice("release");
    assert!(!big.offer_switch && big.warning.is_none());
    let mut none = CheckReport::default();
    none.pairing_advice("release");
    assert!(none.offer_switch);
    assert!(none.warning.unwrap().starts_with(
        "No Mokuro Reader folder is visible with the reader.mokuro.app (release) pairing"
    ));
}

#[test]
fn bad_requests_never_start() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let e = env_with(library(), LibraryConfig::default(), None, 5);
        assert!(
            e.connector
                .start(req("dropbox"), RequestOrigin::cli())
                .is_err()
        );
        assert!(
            e.connector
                .start(req("local"), RequestOrigin::cli())
                .is_err()
        );
        let mut r = req("gdrive");
        r.reader = Some("nightly".into());
        assert!(e.connector.start(r, RequestOrigin::cli()).is_err());
        let mut r = req("gdrive");
        r.folder = Some("a/b".into());
        assert!(e.connector.start(r, RequestOrigin::cli()).is_err());
        assert!(
            e.connector
                .start(req("webdav"), RequestOrigin::cli())
                .is_err()
        );
        assert_eq!(e.connector.status().state, "idle");
    });
}
