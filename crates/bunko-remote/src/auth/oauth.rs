//! OAuth 2.0 for an installed app (§7): the loopback redirect with PKCE (RFC 8252,
//! RFC 7636), the device authorization grant (RFC 8628) and refresh.
//!
//! No function here calls a revoke endpoint. For Google that is a rule, not an
//! omission: revocation removes every grant of the project, the reader's included.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

use crate::backend::BErr;

pub const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const GOOGLE_DEVICE_URL: &str = "https://oauth2.googleapis.com/device/code";
/// Files the app creates or the user opens with it; non-sensitive (prov-gdrive §A).
pub const GOOGLE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";

/// `/common`: personal and work accounts.
pub const MICROSOFT_AUTHORITY: &str = "https://login.microsoftonline.com/common";
pub const MICROSOFT_AUTH_URL: &str =
    "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
pub const MICROSOFT_TOKEN_URL: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/token";
pub const MICROSOFT_DEVICE_URL: &str =
    "https://login.microsoftonline.com/common/oauth2/v2.0/devicecode";
pub const MICROSOFT_SCOPES: [&str; 3] = ["Files.ReadWrite", "offline_access", "User.Read"];

/// The path of Bunko's Microsoft native redirect, `http://localhost/bunko` (D17).
/// Microsoft ignores the port when matching a loopback redirect.
pub const MICROSOFT_REDIRECT_PATH: &str = "/bunko";

/// How long a loopback sign-in waits for the browser to come back.
pub const LOOPBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long a token is assumed to live when the response does not say.
const DEFAULT_EXPIRES_IN: u64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Google,
    Microsoft,
}

/// One installed-app OAuth client and the endpoints it signs in at.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthClient {
    pub provider: Provider,
    pub client_id: String,
    /// Google installed-app clients have one (not confidential); Microsoft public
    /// clients have none.
    pub client_secret: Option<String>,
    pub auth_url: String,
    pub token_url: String,
    pub device_url: String,
    pub scopes: Vec<String>,
}

impl fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthClient")
            .field("provider", &self.provider)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("auth_url", &self.auth_url)
            .field("token_url", &self.token_url)
            .field("device_url", &self.device_url)
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl OAuthClient {
    /// A Google client with the production endpoints and the `drive.file` scope.
    pub fn google(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        OAuthClient {
            provider: Provider::Google,
            client_id: client_id.into(),
            client_secret: Some(client_secret.into()),
            auth_url: GOOGLE_AUTH_URL.into(),
            token_url: GOOGLE_TOKEN_URL.into(),
            device_url: GOOGLE_DEVICE_URL.into(),
            scopes: vec![GOOGLE_SCOPE.into()],
        }
    }

    /// A Microsoft public client (no secret) at the `/common` authority.
    pub fn microsoft(client_id: impl Into<String>) -> Self {
        OAuthClient {
            provider: Provider::Microsoft,
            client_id: client_id.into(),
            client_secret: None,
            auth_url: MICROSOFT_AUTH_URL.into(),
            token_url: MICROSOFT_TOKEN_URL.into(),
            device_url: MICROSOFT_DEVICE_URL.into(),
            scopes: MICROSOFT_SCOPES.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn scope(&self) -> String {
        self.scopes.join(" ")
    }

    /// The client's own fields of a token request.
    fn credentials(&self, params: &mut Vec<(&'static str, String)>) {
        params.push(("client_id", self.client_id.clone()));
        if let Some(secret) = &self.client_secret {
            params.push(("client_secret", secret.clone()));
        }
    }
}

/// What a token endpoint gave back.
#[derive(Clone, PartialEq, Eq)]
pub struct Tokens {
    pub access_token: String,
    /// `None` when the response carried none (a refresh that did not rotate gets the
    /// previous one filled in by [`refresh`]).
    pub refresh_token: Option<String>,
    pub expires_at: OffsetDateTime,
    pub scope: Option<String>,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The user refused consent (`access_denied`, `authorization_declined`).
    #[error("sign-in was refused")]
    Denied,
    /// The device code expired before the user entered it.
    #[error("the sign-in code expired")]
    Expired,
    /// The browser did not come back to the loopback redirect in time.
    #[error("sign-in timed out")]
    Timeout,
    /// The grant is gone (revoked, expired, password changed): sign in again.
    #[error("the sign-in is no longer valid (invalid_grant)")]
    InvalidGrant,
    /// Another OAuth error response.
    #[error("OAuth error {error} (HTTP {status}){}", description.as_deref().map(|d| format!(": {d}")).unwrap_or_default())]
    OAuth {
        status: u16,
        error: String,
        description: Option<String>,
    },
    /// A non-OAuth HTTP failure.
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("unexpected response: {0}")]
    Protocol(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl From<AuthError> for BErr {
    fn from(e: AuthError) -> BErr {
        match e {
            AuthError::InvalidGrant => BErr::AuthRevoked,
            AuthError::Network(m) => BErr::Transient(m),
            AuthError::OAuth { status: 429, .. } | AuthError::Http { status: 429, .. } => {
                BErr::Throttled(None)
            }
            AuthError::OAuth { status, .. } | AuthError::Http { status, .. } if status >= 500 => {
                BErr::Transient(e.to_string())
            }
            other => BErr::Fatal(other.to_string()),
        }
    }
}

fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .build()
            .expect("reqwest client")
    })
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
    s
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    /// A number from Google and Microsoft v2.0; a string from some older endpoints.
    expires_in: Option<serde_json::Value>,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
    error_description: Option<String>,
}

fn seconds(v: &Option<serde_json::Value>) -> Option<u64> {
    match v.as_ref()? {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// POST a form and read an OAuth JSON response.
async fn post_form<T: for<'de> Deserialize<'de>>(
    url: &str,
    params: &[(&'static str, String)],
) -> Result<T, AuthError> {
    let resp = http()
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(params)
        .send()
        .await
        .map_err(|e| AuthError::Network(e.without_url().to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| AuthError::Network(e.without_url().to_string()))?;
    if status.is_success() {
        return serde_json::from_str(&body)
            .map_err(|e| AuthError::Protocol(format!("token endpoint response: {e}")));
    }
    match serde_json::from_str::<ErrorResponse>(&body) {
        Ok(err) if err.error == "invalid_grant" => Err(AuthError::InvalidGrant),
        Ok(err) => Err(AuthError::OAuth {
            status: status.as_u16(),
            error: err.error,
            description: err.error_description.map(|d| truncate(d, 300)),
        }),
        Err(_) => Err(AuthError::Http {
            status: status.as_u16(),
            body: truncate(body, 300),
        }),
    }
}

async fn token_request(
    client: &OAuthClient,
    params: &[(&'static str, String)],
) -> Result<Tokens, AuthError> {
    let r: TokenResponse = post_form(&client.token_url, params).await?;
    let expires_in = seconds(&r.expires_in).unwrap_or(DEFAULT_EXPIRES_IN);
    Ok(Tokens {
        access_token: r.access_token,
        refresh_token: r.refresh_token.filter(|t| !t.is_empty()),
        expires_at: OffsetDateTime::now_utc() + Duration::from_secs(expires_in),
        scope: r.scope,
    })
}

/// Exchange a refresh token. The result always carries a refresh token: the rotated
/// one when the provider sent one (Microsoft rotates on every use), else the one
/// passed in. A caller that keeps credentials must persist a rotated token before it
/// uses the new access token ([`crate::auth::RefreshingToken`] does).
pub async fn refresh(client: &OAuthClient, refresh_token: &str) -> Result<Tokens, AuthError> {
    let mut params = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
    ];
    client.credentials(&mut params);
    if client.provider == Provider::Microsoft {
        params.push(("scope", client.scope()));
    }
    let mut t = token_request(client, &params).await?;
    if t.refresh_token.is_none() {
        t.refresh_token = Some(refresh_token.to_string());
    }
    Ok(t)
}

// ---------------------------------------------------------------- PKCE

/// The RFC 7636 unreserved alphabet.
const VERIFIER_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// A fresh PKCE code verifier: 64 characters (RFC 7636 allows 43-128).
pub fn pkce_verifier() -> String {
    let mut rng = rand::rng();
    (0..64)
        .map(|_| VERIFIER_CHARS[rng.random_range(0..VERIFIER_CHARS.len())] as char)
        .collect()
}

/// The S256 challenge of a verifier: `BASE64URL(SHA256(verifier))`, unpadded.
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn random_state() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

// ---------------------------------------------------------------- loopback

/// The loopback redirect flow with PKCE.
pub struct LoopbackFlow;

impl LoopbackFlow {
    /// Bind the redirect listener and build the authorization URL to open in the
    /// browser. Google redirects to `http://127.0.0.1:<port>/`; Microsoft to
    /// `http://localhost:<port>/bunko` (D17), so the listener takes both 127.0.0.1
    /// and ::1 (where IPv6 is available).
    pub async fn start(client: &OAuthClient) -> Result<(String, Pending), AuthError> {
        let (listeners, redirect_uri, path) = match client.provider {
            Provider::Google => {
                let l = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
                let port = l.local_addr()?.port();
                (vec![l], format!("http://127.0.0.1:{port}/"), "/")
            }
            Provider::Microsoft => {
                let ls = bind_dual_stack().await?;
                let port = ls[0].local_addr()?.port();
                (
                    ls,
                    format!("http://localhost:{port}{MICROSOFT_REDIRECT_PATH}"),
                    MICROSOFT_REDIRECT_PATH,
                )
            }
        };
        let verifier = pkce_verifier();
        let state = random_state();
        let mut query: Vec<(&str, String)> = vec![
            ("response_type", "code".into()),
            ("client_id", client.client_id.clone()),
            ("redirect_uri", redirect_uri.clone()),
            ("scope", client.scope()),
            ("state", state.clone()),
            ("code_challenge", pkce_challenge(&verifier)),
            ("code_challenge_method", "S256".into()),
        ];
        if client.provider == Provider::Google {
            query.push(("access_type", "offline".into()));
        }
        let url = reqwest::Url::parse_with_params(&client.auth_url, &query)
            .map_err(|e| AuthError::Protocol(format!("authorization URL: {e}")))?;
        Ok((
            url.into(),
            Pending {
                client: client.clone(),
                listeners,
                redirect_uri,
                path,
                state,
                verifier,
                timeout: LOOPBACK_TIMEOUT,
            },
        ))
    }
}

/// 127.0.0.1 and ::1 on the same port. Falls back to 127.0.0.1 alone where the
/// host has no IPv6 loopback.
async fn bind_dual_stack() -> Result<Vec<TcpListener>, AuthError> {
    for _ in 0..16 {
        let v4 = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
        let port = v4.local_addr()?.port();
        match TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port))).await {
            Ok(v6) => return Ok(vec![v4, v6]),
            // The port is taken on ::1: try another.
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(_) => return Ok(vec![v4]),
        }
    }
    Err(AuthError::Protocol(
        "no loopback port is free on both 127.0.0.1 and ::1".into(),
    ))
}

/// A loopback sign-in waiting for the browser to come back.
pub struct Pending {
    client: OAuthClient,
    listeners: Vec<TcpListener>,
    redirect_uri: String,
    path: &'static str,
    state: String,
    verifier: String,
    timeout: Duration,
}

impl fmt::Debug for Pending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pending")
            .field("client", &self.client)
            .field("redirect_uri", &self.redirect_uri)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

enum Callback {
    /// Not the redirect (favicon, another path, another method).
    NotFound,
    /// The redirect, but its `state` is not this sign-in's.
    BadState,
    /// The provider sent an error.
    Error(String, Option<String>),
    Code(String),
}

const PAGE_DONE: &str = "Signed in. You can close this tab and return to Mokuro Bunko.";
const PAGE_FAILED: &str = "Sign-in failed. Return to Mokuro Bunko and try again.";
const PAGE_REFUSED: &str = "Sign-in was cancelled. Return to Mokuro Bunko to try again.";

impl Pending {
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Replace the default 5-minute timeout.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Wait for the browser to hit the redirect with this sign-in's `state`, then
    /// exchange the code. Requests with another `state` are answered 400 and
    /// ignored; the `state` is single use, since the flow ends at the first match.
    pub async fn wait(mut self) -> Result<Tokens, AuthError> {
        let deadline = Instant::now() + self.timeout;
        // Each connection is read on its own task, so an idle one (a browser's
        // speculative preconnect) cannot hold up the redirect. Dropping the set
        // stops the acceptors when the sign-in ends.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, TcpStream)>(16);
        let mut acceptors = tokio::task::JoinSet::new();
        for listener in std::mem::take(&mut self.listeners) {
            let tx = tx.clone();
            acceptors.spawn(async move {
                loop {
                    let mut stream = match listener.accept().await {
                        Ok((stream, _)) => stream,
                        // A failed accept (fd pressure, a reset) does not end the sign-in.
                        Err(_) => {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                    };
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let read = read_request_target(&mut stream);
                        if let Ok(Some(target)) =
                            tokio::time::timeout(Duration::from_secs(10), read).await
                        {
                            let _ = tx.send((target, stream)).await;
                        }
                    });
                }
            });
        }
        drop(tx);
        loop {
            let (target, mut stream) = match tokio::time::timeout_at(deadline, rx.recv()).await {
                Err(_) => return Err(AuthError::Timeout),
                Ok(Some(req)) => req,
                Ok(None) => {
                    return Err(AuthError::Protocol("the redirect listener stopped".into()));
                }
            };
            match self.classify(&target) {
                Callback::NotFound => respond(&mut stream, 404, "Not found.").await,
                Callback::BadState => respond(&mut stream, 400, PAGE_FAILED).await,
                Callback::Error(error, description) => {
                    respond(&mut stream, 200, PAGE_REFUSED).await;
                    return Err(if error == "access_denied" {
                        AuthError::Denied
                    } else {
                        AuthError::OAuth {
                            status: 400,
                            error,
                            description,
                        }
                    });
                }
                Callback::Code(code) => {
                    let result = self.exchange(code).await;
                    let page = if result.is_ok() {
                        PAGE_DONE
                    } else {
                        PAGE_FAILED
                    };
                    respond(&mut stream, 200, page).await;
                    return result;
                }
            }
        }
    }

    fn classify(&self, target: &str) -> Callback {
        let Ok(url) = reqwest::Url::parse(&format!("http://loopback{target}")) else {
            return Callback::NotFound;
        };
        if url.path() != self.path {
            return Callback::NotFound;
        }
        let (mut code, mut state, mut error, mut description) = (None, None, None, None);
        for (k, v) in url.query_pairs() {
            match &*k {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                "error" => error = Some(v.into_owned()),
                "error_description" => description = Some(v.into_owned()),
                _ => {}
            }
        }
        if code.is_none() && error.is_none() {
            return Callback::NotFound;
        }
        if !state.is_some_and(|s| constant_time_eq(s.as_bytes(), self.state.as_bytes())) {
            return Callback::BadState;
        }
        match (code, error) {
            (_, Some(e)) => Callback::Error(e, description.map(|d| truncate(d, 300))),
            (Some(c), None) => Callback::Code(c),
            (None, None) => Callback::NotFound,
        }
    }

    async fn exchange(&self, code: String) -> Result<Tokens, AuthError> {
        let mut params = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code),
            ("redirect_uri", self.redirect_uri.clone()),
            ("code_verifier", self.verifier.clone()),
        ];
        self.client.credentials(&mut params);
        if self.client.provider == Provider::Microsoft {
            params.push(("scope", self.client.scope()));
        }
        token_request(&self.client, &params).await
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Read an HTTP request head and return the target of a `GET`.
async fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() > 16 * 1024 {
            return None;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = std::str::from_utf8(&buf).ok()?;
    let mut parts = head.lines().next()?.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    (method == "GET" && target.starts_with('/')).then(|| target.to_string())
}

async fn respond(stream: &mut TcpStream, status: u16, text: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Mokuro Bunko</title></head>\
         <body style=\"font-family:sans-serif;margin:3em\"><p>{text}</p></body></html>"
    );
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

// ---------------------------------------------------------------- device flow

/// What the user is shown during a device sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCode {
    pub user_code: String,
    /// Where the user types the code (`verification_url` from Google,
    /// `verification_uri` from Microsoft).
    pub verification_url: String,
    pub expires_in: Duration,
    /// The polling interval the provider asked for.
    pub interval: Duration,
}

#[derive(Deserialize)]
struct DeviceResponse {
    device_code: String,
    user_code: String,
    #[serde(alias = "verification_uri")]
    verification_url: String,
    expires_in: Option<serde_json::Value>,
    interval: Option<serde_json::Value>,
}

/// A device sign-in in progress.
pub struct DeviceFlow {
    client: OAuthClient,
    device_code: String,
    pub code: DeviceCode,
    interval_s: u64,
    /// One "second" of the provider's interval; shortened in tests only.
    unit: Duration,
    deadline: Instant,
}

impl fmt::Debug for DeviceFlow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceFlow")
            .field("client", &self.client)
            .field("code", &self.code)
            .field("interval_s", &self.interval_s)
            .finish_non_exhaustive()
    }
}

impl DeviceFlow {
    /// Ask the provider for a device code.
    pub async fn start(client: &OAuthClient) -> Result<DeviceFlow, AuthError> {
        let mut params = vec![("client_id", client.client_id.clone())];
        params.push(("scope", client.scope()));
        let r: DeviceResponse = post_form(&client.device_url, &params).await?;
        let expires_in = seconds(&r.expires_in).unwrap_or(match client.provider {
            Provider::Google => 1800,
            Provider::Microsoft => 900,
        });
        let interval_s = seconds(&r.interval).unwrap_or(5).max(1);
        Ok(DeviceFlow {
            client: client.clone(),
            device_code: r.device_code,
            code: DeviceCode {
                user_code: r.user_code,
                verification_url: r.verification_url,
                expires_in: Duration::from_secs(expires_in),
                interval: Duration::from_secs(interval_s),
            },
            interval_s,
            unit: Duration::from_secs(1),
            deadline: Instant::now() + Duration::from_secs(expires_in),
        })
    }

    pub fn code(&self) -> &DeviceCode {
        &self.code
    }

    /// The current polling interval in provider seconds (raised by `slow_down`).
    pub fn interval_secs(&self) -> u64 {
        self.interval_s
    }

    /// Make one provider second last `unit` (tests: polling and expiry in ms).
    #[doc(hidden)]
    pub fn set_time_unit(&mut self, unit: Duration) {
        let expires = self.code.expires_in.as_secs_f64() * unit.as_secs_f64();
        self.deadline = Instant::now() + Duration::from_secs_f64(expires);
        self.unit = unit;
    }

    /// Poll the token endpoint until the user finishes. `authorization_pending`
    /// (Google: HTTP 428) waits another interval; `slow_down` adds 5 s to it;
    /// `access_denied`/`authorization_declined` → [`AuthError::Denied`];
    /// `expired_token`, or the code's lifetime running out → [`AuthError::Expired`].
    /// A network failure is retried at the next interval.
    pub async fn poll(&mut self) -> Result<Tokens, AuthError> {
        loop {
            let wake = Instant::now() + self.unit * self.interval_s as u32;
            if wake >= self.deadline {
                tokio::time::sleep_until(self.deadline).await;
                return Err(AuthError::Expired);
            }
            tokio::time::sleep_until(wake).await;
            let mut params = vec![
                (
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:device_code".to_string(),
                ),
                ("device_code", self.device_code.clone()),
            ];
            self.client.credentials(&mut params);
            match token_request(&self.client, &params).await {
                Ok(t) => return Ok(t),
                Err(AuthError::Network(_)) => continue,
                Err(AuthError::OAuth { status: 428, .. } | AuthError::Http { status: 428, .. }) => {
                    continue;
                }
                Err(AuthError::OAuth {
                    error,
                    status,
                    description,
                }) => match error.as_str() {
                    "authorization_pending" => continue,
                    "slow_down" => self.interval_s += 5,
                    "access_denied" | "authorization_declined" => return Err(AuthError::Denied),
                    "expired_token" => return Err(AuthError::Expired),
                    _ => {
                        return Err(AuthError::OAuth {
                            status,
                            error,
                            description,
                        });
                    }
                },
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_rfc7636_appendix_b() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_shape() {
        let v = pkce_verifier();
        assert!((43..=128).contains(&v.len()));
        assert!(v.bytes().all(|b| VERIFIER_CHARS.contains(&b)));
        assert_ne!(v, pkce_verifier());
    }

    #[test]
    fn debug_redacts() {
        let t = Tokens {
            access_token: "ya29.secret-access".into(),
            refresh_token: Some("1//secret-refresh".into()),
            expires_at: OffsetDateTime::UNIX_EPOCH,
            scope: None,
        };
        let s = format!("{t:?}");
        assert!(!s.contains("secret"), "{s}");
        let c = OAuthClient::google("id", "GOCSPX-secret");
        let s = format!("{c:?}");
        assert!(!s.contains("GOCSPX"), "{s}");
    }

    #[test]
    fn error_mapping() {
        assert!(matches!(
            BErr::from(AuthError::InvalidGrant),
            BErr::AuthRevoked
        ));
        assert!(matches!(
            BErr::from(AuthError::Network("x".into())),
            BErr::Transient(_)
        ));
        assert!(matches!(
            BErr::from(AuthError::Http {
                status: 503,
                body: String::new()
            }),
            BErr::Transient(_)
        ));
        assert!(matches!(
            BErr::from(AuthError::OAuth {
                status: 401,
                error: "invalid_client".into(),
                description: None
            }),
            BErr::Fatal(_)
        ));
    }
}
