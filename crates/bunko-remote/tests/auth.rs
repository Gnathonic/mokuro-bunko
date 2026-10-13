//! OAuth flows against a mock authorization server, the credential store and
//! refreshing tokens (§7).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Form, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use bunko_remote::BErr;
use bunko_remote::auth::oauth::pkce_challenge;
use bunko_remote::auth::{
    AuthError, CredError, CredentialStore, DeviceFlow, LoopbackFlow, OAuthClient, Provider,
    RefreshingToken, SecretKey, StoredCredential, TokenSource,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use time::OffsetDateTime;

// ---------------------------------------------------------------- mock server

#[derive(Default)]
struct Mock {
    /// Scripted responses, served in order to any endpoint.
    script: Mutex<VecDeque<(u16, Value)>>,
    /// (path, form) of every request.
    seen: Mutex<Vec<(String, HashMap<String, String>)>>,
}

impl Mock {
    fn push(&self, status: u16, body: Value) {
        self.script.lock().push_back((status, body));
    }
    fn requests(&self, path: &str) -> Vec<HashMap<String, String>> {
        self.seen
            .lock()
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, f)| f.clone())
            .collect()
    }
}

async fn serve(m: &Mock, path: &str, form: HashMap<String, String>) -> Response {
    m.seen.lock().push((path.to_string(), form));
    let next = m.script.lock().pop_front();
    match next {
        Some((status, body)) => (StatusCode::from_u16(status).unwrap(), Json(body)).into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "script exhausted").into_response(),
    }
}

async fn token(State(m): State<Arc<Mock>>, Form(f): Form<HashMap<String, String>>) -> Response {
    serve(&m, "/token", f).await
}

async fn device(State(m): State<Arc<Mock>>, Form(f): Form<HashMap<String, String>>) -> Response {
    serve(&m, "/device", f).await
}

/// A mock authorization server on 127.0.0.1; returns its base URL.
async fn mock_server() -> (Arc<Mock>, String) {
    let m = Arc::new(Mock::default());
    let app = Router::new()
        .route("/token", post(token))
        .route("/device", post(device))
        .with_state(m.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (m, base)
}

fn client(provider: Provider, base: &str) -> OAuthClient {
    let mut c = match provider {
        Provider::Google => OAuthClient::google("google-cid", "google-csecret"),
        Provider::Microsoft => OAuthClient::microsoft("ms-cid"),
    };
    c.auth_url = format!("{base}/authorize");
    c.token_url = format!("{base}/token");
    c.device_url = format!("{base}/device");
    c
}

fn browser() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

fn query(url: &str) -> HashMap<String, String> {
    reqwest::Url::parse(url)
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn tokens_json(access: &str, refresh: Option<&str>) -> Value {
    let mut v = json!({"access_token": access, "expires_in": 3599, "token_type": "Bearer"});
    if let Some(r) = refresh {
        v["refresh_token"] = json!(r);
    }
    v
}

// ---------------------------------------------------------------- loopback

#[tokio::test]
async fn google_loopback_end_to_end() {
    let (m, base) = mock_server().await;
    m.push(
        200,
        json!({"access_token": "at1", "refresh_token": "rt1", "expires_in": 3599,
               "scope": "https://www.googleapis.com/auth/drive.file", "token_type": "Bearer"}),
    );
    let c = client(Provider::Google, &base);
    let (url, pending) = LoopbackFlow::start(&c).await.unwrap();
    assert!(url.starts_with(&format!("{base}/authorize?")), "{url}");
    let q = query(&url);
    let redirect = q["redirect_uri"].clone();
    assert_eq!(redirect, pending.redirect_uri());
    assert!(
        redirect.starts_with("http://127.0.0.1:") && redirect.ends_with('/'),
        "{redirect}"
    );
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], "google-cid");
    assert_eq!(q["scope"], "https://www.googleapis.com/auth/drive.file");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["access_type"], "offline");
    assert!(!q.contains_key("client_secret"));
    let state = q["state"].clone();
    let challenge = q["code_challenge"].clone();

    let started = OffsetDateTime::now_utc();
    let wait = tokio::spawn(pending.wait());
    // An idle connection (a browser preconnect) must not hold up the redirect.
    let host = redirect.trim_start_matches("http://").trim_end_matches('/');
    let _idle = tokio::net::TcpStream::connect(host).await.unwrap();
    let t0 = std::time::Instant::now();
    let b = browser();
    // Not the redirect.
    let r = b
        .get(format!("{redirect}favicon.ico"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    // A wrong state is refused and the sign-in keeps waiting.
    let r = b
        .get(format!("{redirect}?code=stolen&state=not-{state}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = b
        .get(format!("{redirect}?code=stolen"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(
        m.requests("/token").is_empty(),
        "a refused state exchanged a code"
    );
    // The real redirect.
    let r = b
        .get(format!("{redirect}?code=the-code&state={state}&scope=x"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("Signed in"));
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());

    let t = wait.await.unwrap().unwrap();
    assert_eq!(t.access_token, "at1");
    assert_eq!(t.refresh_token.as_deref(), Some("rt1"));
    assert_eq!(
        t.scope.as_deref(),
        Some("https://www.googleapis.com/auth/drive.file")
    );
    let lifetime = (t.expires_at - started).whole_seconds();
    assert!((3590..=3600).contains(&lifetime), "{lifetime}");

    let reqs = m.requests("/token");
    assert_eq!(reqs.len(), 1);
    let f = &reqs[0];
    assert_eq!(f["grant_type"], "authorization_code");
    assert_eq!(f["code"], "the-code");
    assert_eq!(f["redirect_uri"], redirect);
    assert_eq!(f["client_id"], "google-cid");
    assert_eq!(f["client_secret"], "google-csecret");
    let verifier = &f["code_verifier"];
    assert!((43..=128).contains(&verifier.len()));
    assert_eq!(pkce_challenge(verifier), challenge);
}

#[tokio::test]
async fn microsoft_loopback_uses_localhost_bunko_on_both_stacks() {
    let (m, base) = mock_server().await;
    m.push(200, tokens_json("ms-at", Some("ms-rt")));
    let c = client(Provider::Microsoft, &base);
    let (url, pending) = LoopbackFlow::start(&c).await.unwrap();
    let q = query(&url);
    let redirect = q["redirect_uri"].clone();
    let port: u16 = redirect
        .strip_prefix("http://localhost:")
        .and_then(|r| r.strip_suffix("/bunko"))
        .unwrap_or_else(|| panic!("{redirect}"))
        .parse()
        .unwrap();
    assert_eq!(q["scope"], "Files.ReadWrite offline_access User.Read");
    assert!(!q.contains_key("access_type"));
    let state = q["state"].clone();
    let wait = tokio::spawn(pending.wait());
    let b = browser();

    // ::1 answers too, where the host has an IPv6 loopback.
    let v6 = tokio::net::TcpListener::bind("[::1]:0").await.is_ok();
    if v6 {
        let r = b
            .get(format!("http://[::1]:{port}/bunko?code=x&state=wrong"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
    } else {
        eprintln!("no IPv6 loopback here: ::1 not checked");
    }
    // The registered path only.
    let r = b
        .get(format!("http://127.0.0.1:{port}/?code=c&state={state}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let r = b
        .get(format!(
            "http://127.0.0.1:{port}/bunko?code=ms-code&state={state}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let t = wait.await.unwrap().unwrap();
    assert_eq!(t.access_token, "ms-at");

    let f = &m.requests("/token")[0];
    assert_eq!(f["client_id"], "ms-cid");
    assert!(
        !f.contains_key("client_secret"),
        "a public client sent a secret"
    );
    assert_eq!(f["redirect_uri"], redirect);
    assert_eq!(f["scope"], "Files.ReadWrite offline_access User.Read");
    assert_eq!(pkce_challenge(&f["code_verifier"]), q["code_challenge"]);
}

#[tokio::test]
async fn loopback_denied_and_timeout() {
    let (m, base) = mock_server().await;
    let c = client(Provider::Google, &base);

    let (url, pending) = LoopbackFlow::start(&c).await.unwrap();
    let q = query(&url);
    let wait = tokio::spawn(pending.wait());
    let r = browser()
        .get(format!(
            "{}?error=access_denied&state={}",
            q["redirect_uri"], q["state"]
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(matches!(wait.await.unwrap(), Err(AuthError::Denied)));

    let (_, mut pending) = LoopbackFlow::start(&c).await.unwrap();
    pending.set_timeout(Duration::from_millis(200));
    let t0 = std::time::Instant::now();
    assert!(matches!(pending.wait().await, Err(AuthError::Timeout)));
    assert!(t0.elapsed() >= Duration::from_millis(190));
    assert!(m.requests("/token").is_empty());
}

#[tokio::test]
async fn loopback_failed_exchange_is_reported() {
    let (m, base) = mock_server().await;
    m.push(
        400,
        json!({"error": "invalid_grant", "error_description": "Bad Request"}),
    );
    let c = client(Provider::Google, &base);
    let (url, pending) = LoopbackFlow::start(&c).await.unwrap();
    let q = query(&url);
    let wait = tokio::spawn(pending.wait());
    let r = browser()
        .get(format!("{}?code=c&state={}", q["redirect_uri"], q["state"]))
        .send()
        .await
        .unwrap();
    assert!(r.text().await.unwrap().contains("Sign-in failed"));
    assert!(matches!(wait.await.unwrap(), Err(AuthError::InvalidGrant)));
}

// ---------------------------------------------------------------- device flow

const UNIT: Duration = Duration::from_millis(10);

#[tokio::test]
async fn google_device_pending_slow_down_success() {
    let (m, base) = mock_server().await;
    m.push(
        200,
        json!({"device_code": "dev-code", "user_code": "ABCD-EFGH",
               "verification_url": "https://www.google.com/device",
               "expires_in": 1800, "interval": 1}),
    );
    m.push(428, json!({"error": "authorization_pending"}));
    m.push(403, json!({"error": "slow_down"}));
    m.push(400, json!({"error": "authorization_pending"}));
    m.push(200, tokens_json("dev-at", Some("dev-rt")));

    let c = client(Provider::Google, &base);
    let mut flow = DeviceFlow::start(&c).await.unwrap();
    assert_eq!(flow.code().user_code, "ABCD-EFGH");
    assert_eq!(
        flow.code().verification_url,
        "https://www.google.com/device"
    );
    assert_eq!(flow.code().expires_in, Duration::from_secs(1800));
    assert_eq!(flow.code().interval, Duration::from_secs(1));
    let d = &m.requests("/device")[0];
    assert_eq!(d["client_id"], "google-cid");
    assert_eq!(d["scope"], "https://www.googleapis.com/auth/drive.file");

    flow.set_time_unit(UNIT);
    let t0 = std::time::Instant::now();
    let t = flow.poll().await.unwrap();
    assert_eq!(t.access_token, "dev-at");
    assert_eq!(t.refresh_token.as_deref(), Some("dev-rt"));
    // slow_down added 5 s: 1 + 1 + 6 + 6 units at least.
    assert_eq!(flow.interval_secs(), 6);
    assert!(t0.elapsed() >= UNIT * 14, "{:?}", t0.elapsed());

    let polls = m.requests("/token");
    assert_eq!(polls.len(), 4);
    for p in &polls {
        assert_eq!(
            p["grant_type"],
            "urn:ietf:params:oauth:grant-type:device_code"
        );
        assert_eq!(p["device_code"], "dev-code");
        assert_eq!(p["client_secret"], "google-csecret");
    }
}

#[tokio::test]
async fn microsoft_device_expired_and_denied() {
    let (m, base) = mock_server().await;
    let c = client(Provider::Microsoft, &base);
    let start = json!({"device_code": "d", "user_code": "MSCODE",
                       "verification_uri": "https://microsoft.com/devicelogin",
                       "expires_in": 900, "interval": 1, "message": "go"});

    // The provider says the code expired.
    m.push(200, start.clone());
    m.push(400, json!({"error": "authorization_pending"}));
    m.push(400, json!({"error": "expired_token"}));
    let mut flow = DeviceFlow::start(&c).await.unwrap();
    assert_eq!(
        flow.code().verification_url,
        "https://microsoft.com/devicelogin"
    );
    flow.set_time_unit(UNIT);
    assert!(matches!(flow.poll().await, Err(AuthError::Expired)));
    assert!(!m.requests("/token")[0].contains_key("client_secret"));

    // The user refused.
    m.push(200, start.clone());
    m.push(400, json!({"error": "authorization_declined"}));
    let mut flow = DeviceFlow::start(&c).await.unwrap();
    flow.set_time_unit(UNIT);
    assert!(matches!(flow.poll().await, Err(AuthError::Denied)));

    // The code's own lifetime runs out while the user does nothing.
    m.push(
        200,
        json!({"device_code": "d", "user_code": "U", "verification_uri": "https://v",
               "expires_in": 5, "interval": 1}),
    );
    for _ in 0..10 {
        m.push(400, json!({"error": "authorization_pending"}));
    }
    let mut flow = DeviceFlow::start(&c).await.unwrap();
    flow.set_time_unit(UNIT);
    assert!(matches!(flow.poll().await, Err(AuthError::Expired)));
}

// ---------------------------------------------------------------- refresh

fn onedrive_cred(refresh: &str, access: &str, expires_at: OffsetDateTime) -> StoredCredential {
    StoredCredential {
        provider: "onedrive".into(),
        refresh_token: Some(refresh.into()),
        access_token: Some(access.into()),
        expires_at: Some(expires_at),
        account_id: Some("acct".into()),
        account_label: Some("someone@example.com".into()),
        client_id: Some("ms-cid".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn refresh_rotation_is_persisted_before_use() {
    let (m, base) = mock_server().await;
    let dir = tempfile::tempdir().unwrap();
    let cloud = dir.path().join("cloud");
    let store = Arc::new(CredentialStore::open_with_key(&cloud, None).unwrap());
    let expired = OffsetDateTime::now_utc() - Duration::from_secs(60);
    store
        .put("od-1", onedrive_cred("r1", "a1", expired))
        .unwrap();

    m.push(200, tokens_json("a2", Some("r2")));
    let tok = RefreshingToken::new(client(Provider::Microsoft, &base), "od-1", store.clone());
    assert_eq!(tok.access_token().await.unwrap(), "a2");
    // On disk, through a separate handle, by the time the token was returned.
    let disk = CredentialStore::open_with_key(&cloud, None)
        .unwrap()
        .get("od-1")
        .unwrap()
        .unwrap();
    assert_eq!(disk.refresh_token.as_deref(), Some("r2"));
    assert_eq!(disk.access_token.as_deref(), Some("a2"));
    assert!(disk.expires_at.unwrap() > OffsetDateTime::now_utc() + Duration::from_secs(3000));
    assert_eq!(disk.account_label.as_deref(), Some("someone@example.com"));
    let f = &m.requests("/token")[0];
    assert_eq!(f["grant_type"], "refresh_token");
    assert_eq!(f["refresh_token"], "r1");
    assert_eq!(f["scope"], "Files.ReadWrite offline_access User.Read");
    assert!(!f.contains_key("client_secret"));

    // Cached: no second request.
    assert_eq!(tok.access_token().await.unwrap(), "a2");
    assert_eq!(m.requests("/token").len(), 1);

    // A 401 upstream: invalidate refreshes with the rotated token; a response
    // without a refresh token keeps it.
    tok.invalidate();
    m.push(200, tokens_json("a3", None));
    assert_eq!(tok.access_token().await.unwrap(), "a3");
    assert_eq!(m.requests("/token")[1]["refresh_token"], "r2");
    assert_eq!(
        store.get("od-1").unwrap().unwrap().refresh_token.as_deref(),
        Some("r2")
    );

    // A revoked grant.
    tok.invalidate();
    m.push(
        400,
        json!({"error": "invalid_grant", "error_description": "AADSTS70000"}),
    );
    assert!(matches!(tok.access_token().await, Err(BErr::AuthRevoked)));
    // A provider outage is transient.
    m.push(503, json!({"error": "temporarily_unavailable"}));
    assert!(matches!(tok.access_token().await, Err(BErr::Transient(_))));
}

#[tokio::test]
async fn fresh_disk_token_is_used_without_refresh() {
    let (m, base) = mock_server().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(CredentialStore::open_with_key(dir.path(), None).unwrap());
    let later = OffsetDateTime::now_utc() + Duration::from_secs(3600);
    store
        .put("g", onedrive_cred("r", "disk-at", later))
        .unwrap();
    let tok = RefreshingToken::new(client(Provider::Google, &base), "g", store.clone());
    assert_eq!(tok.access_token().await.unwrap(), "disk-at");
    assert!(m.requests("/token").is_empty());
    // Inside the 5-minute margin it refreshes; Google keeps its refresh token.
    let soon = OffsetDateTime::now_utc() + Duration::from_secs(200);
    store.put("g", onedrive_cred("r", "old", soon)).unwrap();
    let tok = RefreshingToken::new(client(Provider::Google, &base), "g", store.clone());
    m.push(200, tokens_json("new", None));
    assert_eq!(tok.access_token().await.unwrap(), "new");
    let f = &m.requests("/token")[0];
    assert_eq!(f["client_secret"], "google-csecret");
    assert!(!f.contains_key("scope"));
    assert_eq!(
        store.get("g").unwrap().unwrap().refresh_token.as_deref(),
        Some("r")
    );
    // No credentials (disconnected): sign in again.
    let tok = RefreshingToken::new(client(Provider::Google, &base), "nope", store);
    assert!(matches!(tok.access_token().await, Err(BErr::AuthRevoked)));
}

#[cfg(unix)]
#[tokio::test]
async fn a_rotation_that_cannot_be_saved_is_not_used() {
    use std::os::unix::fs::PermissionsExt;
    let (m, base) = mock_server().await;
    let dir = tempfile::tempdir().unwrap();
    let cloud = dir.path().join("cloud");
    let store = Arc::new(CredentialStore::open_with_key(&cloud, None).unwrap());
    let expired = OffsetDateTime::now_utc() - Duration::from_secs(60);
    store.put("od", onedrive_cred("r1", "a1", expired)).unwrap();
    // The directory can be read but not written.
    std::fs::set_permissions(&cloud, std::fs::Permissions::from_mode(0o500)).unwrap();
    let tok = RefreshingToken::new(client(Provider::Microsoft, &base), "od", store.clone());
    m.push(200, tokens_json("a2", Some("r2")));
    let r = tok.access_token().await;
    std::fs::set_permissions(&cloud, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(r, Err(BErr::Fatal(_))), "{r:?}");
    assert_eq!(
        store.get("od").unwrap().unwrap().refresh_token.as_deref(),
        Some("r1")
    );
    // Nothing was cached: the next call refreshes again and saves.
    m.push(200, tokens_json("a3", Some("r3")));
    assert_eq!(tok.access_token().await.unwrap(), "a3");
    assert_eq!(m.requests("/token").len(), 2);
    assert_eq!(
        store.get("od").unwrap().unwrap().refresh_token.as_deref(),
        Some("r3")
    );
}

// ---------------------------------------------------------------- credentials

fn full_cred() -> StoredCredential {
    StoredCredential {
        provider: "gdrive".into(),
        refresh_token: Some("1//refresh-SECRET".into()),
        access_token: Some("ya29.access-SECRET".into()),
        expires_at: Some(OffsetDateTime::from_unix_timestamp(1_900_000_000).unwrap()),
        account_id: Some("1234".into()),
        account_label: Some("reader@example.com".into()),
        client_id: Some("cid.apps.googleusercontent.com".into()),
        mega_session: Some("mega-SECRET".into()),
        webdav_password: Some("dav-SECRET".into()),
    }
}

#[cfg(unix)]
#[test]
fn credentials_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let cloud = dir.path().join("cloud");
    let store = CredentialStore::open_with_key(&cloud, None).unwrap();
    assert!(!cloud.exists(), "open must not create anything");
    store.put("s", full_cred()).unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&cloud.join("credentials.json")), 0o600);
    assert_eq!(mode(&cloud), 0o700);
    // An existing, looser directory is tightened.
    std::fs::set_permissions(&cloud, std::fs::Permissions::from_mode(0o755)).unwrap();
    store.put("t", full_cred()).unwrap();
    assert_eq!(mode(&cloud), 0o700);
    assert_eq!(mode(&cloud.join("credentials.json")), 0o600);
    // No temp files left behind.
    let names: Vec<_> = std::fs::read_dir(&cloud)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["credentials.json"]);
}

#[test]
fn plain_store_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = CredentialStore::open_with_key(dir.path(), None).unwrap();
    assert!(!store.is_sealed());
    assert_eq!(store.get("a").unwrap(), None);
    store.put("a", full_cred()).unwrap();
    store
        .put(
            "b",
            StoredCredential {
                provider: "webdav".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(store.store_ids().unwrap(), ["a", "b"]);
    let again = CredentialStore::open_with_key(dir.path(), None).unwrap();
    assert_eq!(again.get("a").unwrap().unwrap(), full_cred());
    assert!(
        again
            .update("b", |c| c.webdav_password = Some("pw".into()))
            .unwrap()
    );
    assert!(!again.update("zzz", |_| {}).unwrap());
    assert_eq!(
        store.get("b").unwrap().unwrap().webdav_password.as_deref(),
        Some("pw")
    );
    assert!(store.remove("a").unwrap());
    assert!(!store.remove("a").unwrap());
    assert_eq!(store.store_ids().unwrap(), ["b"]);
    let raw: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("credentials.json")).unwrap())
            .unwrap();
    assert_eq!(raw["version"], 1);
    assert_eq!(raw["stores"]["b"]["provider"], "webdav");
}

#[test]
fn sealed_round_trip_and_wrong_key() {
    let dir = tempfile::tempdir().unwrap();
    let key = SecretKey::from_material("a long passphrase");
    let store = CredentialStore::open_with_key(dir.path(), Some(key.clone())).unwrap();
    assert!(store.is_sealed());
    store.put("s", full_cred()).unwrap();

    let raw = std::fs::read_to_string(dir.path().join("credentials.json")).unwrap();
    assert!(!raw.contains("SECRET") && !raw.contains("gdrive"), "{raw}");
    let v: Value = serde_json::from_str(&raw).unwrap();
    assert!(v["sealed"].is_string());
    assert_eq!(v.as_object().unwrap().len(), 1);

    let again = CredentialStore::open_with_key(dir.path(), Some(key.clone())).unwrap();
    assert_eq!(again.get("s").unwrap().unwrap(), full_cred());
    // A fresh nonce on every write.
    again.put("s", full_cred()).unwrap();
    assert_ne!(
        std::fs::read_to_string(dir.path().join("credentials.json")).unwrap(),
        raw
    );

    let wrong = CredentialStore::open_with_key(dir.path(), Some(SecretKey::from_material("nope")));
    assert!(matches!(wrong, Err(CredError::WrongKey)), "{wrong:?}");
    let none = CredentialStore::open_with_key(dir.path(), None);
    assert!(matches!(none, Err(CredError::Sealed)), "{none:?}");

    // A damaged blob fails cleanly too.
    let mut v: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("credentials.json")).unwrap(),
    )
    .unwrap();
    let mut s = v["sealed"].as_str().unwrap().to_string();
    let last = s.pop().unwrap();
    s.push(if last == 'A' { 'B' } else { 'A' });
    v["sealed"] = json!(s);
    std::fs::write(dir.path().join("credentials.json"), v.to_string()).unwrap();
    assert!(matches!(
        CredentialStore::open_with_key(dir.path(), Some(key)),
        Err(CredError::WrongKey | CredError::Corrupt(_))
    ));
}

#[test]
fn plain_file_is_sealed_on_next_write_once_a_key_is_set() {
    let dir = tempfile::tempdir().unwrap();
    CredentialStore::open_with_key(dir.path(), None)
        .unwrap()
        .put("s", full_cred())
        .unwrap();
    let key = SecretKey::from_bytes([9; 32]);
    let store = CredentialStore::open_with_key(dir.path(), Some(key.clone())).unwrap();
    assert_eq!(store.get("s").unwrap().unwrap(), full_cred());
    store
        .update("s", |c| c.account_label = Some("x".into()))
        .unwrap();
    let raw = std::fs::read_to_string(dir.path().join("credentials.json")).unwrap();
    assert!(raw.contains("\"sealed\"") && !raw.contains("SECRET"));
    assert!(CredentialStore::open_with_key(dir.path(), Some(key)).is_ok());
}

#[test]
fn debug_never_shows_secrets() {
    let c = full_cred();
    let s = format!("{c:?} {:?}", c.clone());
    assert!(!s.contains("SECRET"), "{s}");
    assert!(s.contains("reader@example.com") && s.contains("<redacted>"));
    let dir = tempfile::tempdir().unwrap();
    let store =
        CredentialStore::open_with_key(dir.path(), Some(SecretKey::from_bytes([1; 32]))).unwrap();
    let s = format!("{store:?}");
    assert!(s.contains("sealed: true"), "{s}");
    let tok = RefreshingToken::new(OAuthClient::google("id", "GOCSPX-x"), "s", Arc::new(store));
    let s = format!("{tok:?}");
    assert!(!s.contains("GOCSPX"), "{s}");
    let s = format!("{:?}", bunko_remote::auth::StaticToken("tok-SECRET".into()));
    assert!(!s.contains("SECRET"), "{s}");
}
