//! OAuth 2.0 for an installed app (§7): the loopback redirect with PKCE (RFC 8252,
//! RFC 7636), the same redirect with the address pasted back by hand (headless hosts),
//! the device authorization grant (RFC 8628) and refresh.
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

/// How long a loopback sign-in waits for the browser to come back.
/// What a sign-in or a stored Google credential says when this build has no Google
/// client secret ([`OAuthClient::lacks_google_secret`]).
pub const NO_GOOGLE_SECRET: &str = "this build of Bunko was made without the Google client secrets (BUNKO_GOOGLE_*_SECRET at build time), so it cannot sign in to Google Drive with the built-in clients: use an official release, or set your own client in storage.library.oauth.google";

pub const LOOPBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long a pasted-address sign-in waits for the address: the sign-in happens on
/// another device, and the authorization code lives about ten minutes.
pub const PASTE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The redirect of a loopback sign-in on `port`. Google's Desktop clients take
/// `http://127.0.0.1:<port>/`. Microsoft's registration has `http://localhost/bunko`
/// under Mobile and desktop applications, and Microsoft ignores the port of a loopback
/// redirect, so Bunko sends `http://localhost:<port>/bunko` (D17). The path matters: a
/// bare `http://localhost:<port>` also matches the reader's single-page-app entries,
/// and a code issued to those redeems only from a browser (AADSTS90023).
pub fn loopback_redirect(provider: Provider, port: u16) -> String {
    match provider {
        Provider::Google => format!("http://127.0.0.1:{port}/"),
        Provider::Microsoft => format!("http://localhost:{port}/bunko"),
    }
}

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

    /// A Google client headed for Google's own token endpoint without a secret: an
    /// embedded client of a build made without its `BUNKO_GOOGLE_*_SECRET`. Google
    /// refuses such a sign-in only after the browser round trip, so callers refuse it
    /// first ([`NO_GOOGLE_SECRET`]), and a refresh with it fails
    /// without a request ([`AuthError::NoGoogleSecret`]).
    pub fn lacks_google_secret(&self) -> bool {
        self.provider == Provider::Google
            && self.client_secret.is_none()
            && self.token_url == GOOGLE_TOKEN_URL
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

/// What a personal Microsoft account gets from a device-code sign-in with Bunko's
/// client (AADSTS70002): the client is not enabled for consumers' device codes.
pub const PERSONAL_ACCOUNT_NO_CODE: &str = "Personal Microsoft accounts can't sign in with a code: sign in through the browser (or paste the redirect URL)";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The user refused consent (`access_denied`, `authorization_declined`).
    #[error("sign-in was refused")]
    Denied,
    /// Microsoft's AADSTS70002 on a device-code request or poll: the client does not
    /// run the device flow for personal accounts. The browser (loopback or pasted
    /// address) flow does.
    #[error("{PERSONAL_ACCOUNT_NO_CODE}")]
    PersonalAccountNoCode,
    /// The device code expired before the user entered it.
    #[error("the sign-in code expired")]
    Expired,
    /// The browser did not come back to the loopback redirect in time.
    #[error("sign-in timed out")]
    Timeout,
    /// The grant is gone (revoked, expired, password changed): sign in again.
    #[error("the sign-in is no longer valid (invalid_grant)")]
    InvalidGrant,
    /// The client has no Google secret ([`OAuthClient::lacks_google_secret`]): Google
    /// would refuse the request, so it is not sent.
    #[error("{NO_GOOGLE_SECRET}")]
    NoGoogleSecret,
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
    Err(error_response(status.as_u16(), body))
}

/// `AADSTS<code>` appears in `text` as a whole number (`AADSTS70002`, not
/// `AADSTS7000215`).
fn has_aadsts(text: &str, code: &str) -> bool {
    let needle = format!("AADSTS{code}");
    text.match_indices(&needle).any(|(i, m)| {
        !text[i + m.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    })
}

/// The error of a failed token or device-code call, from its status and body.
pub fn error_response(status: u16, body: String) -> AuthError {
    match serde_json::from_str::<ErrorResponse>(&body) {
        Ok(err) if err.error == "invalid_grant" => AuthError::InvalidGrant,
        // "AADSTS70002: The client does not exist or is not enabled for consumers."
        // (`invalid_client`): only Microsoft numbers its errors AADSTS.
        Ok(err)
            if err
                .error_description
                .as_deref()
                .is_some_and(|d| has_aadsts(d, "70002")) =>
        {
            AuthError::PersonalAccountNoCode
        }
        Ok(err) => AuthError::OAuth {
            status,
            error: err.error,
            description: err.error_description.map(|d| truncate(d, 300)),
        },
        Err(_) => AuthError::Http {
            status,
            body: truncate(body, 300),
        },
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
    if client.lacks_google_secret() {
        return Err(AuthError::NoGoogleSecret);
    }
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

/// The authorization URL of one sign-in, and the secrets it is redeemed with.
struct Authorization {
    url: String,
    state: String,
    verifier: String,
}

fn authorization(client: &OAuthClient, redirect_uri: &str) -> Result<Authorization, AuthError> {
    let verifier = pkce_verifier();
    let state = random_state();
    let mut query: Vec<(&str, String)> = vec![
        ("response_type", "code".into()),
        ("client_id", client.client_id.clone()),
        ("redirect_uri", redirect_uri.to_string()),
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
    Ok(Authorization {
        url: url.into(),
        state,
        verifier,
    })
}

/// Redeem an authorization code with its PKCE verifier and the exact redirect URI of
/// the authorization request.
async fn redeem(
    client: &OAuthClient,
    redirect_uri: &str,
    verifier: &str,
    code: String,
) -> Result<Tokens, AuthError> {
    let mut params = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("redirect_uri", redirect_uri.to_string()),
        ("code_verifier", verifier.to_string()),
    ];
    client.credentials(&mut params);
    if client.provider == Provider::Microsoft {
        params.push(("scope", client.scope()));
    }
    token_request(client, &params).await
}

/// The loopback redirect flow with PKCE.
pub struct LoopbackFlow;

impl LoopbackFlow {
    /// Bind the redirect listener and build the authorization URL to open in the
    /// browser ([`loopback_redirect`]). Microsoft's `localhost` may resolve to either
    /// stack, so its listener takes both 127.0.0.1 and ::1 (where IPv6 is available).
    pub async fn start(client: &OAuthClient) -> Result<(String, Pending), AuthError> {
        let listeners = match client.provider {
            Provider::Google => {
                vec![TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?]
            }
            Provider::Microsoft => bind_dual_stack().await?,
        };
        let port = listeners[0].local_addr()?.port();
        let redirect_uri = loopback_redirect(client.provider, port);
        let a = authorization(client, &redirect_uri)?;
        Ok((
            a.url,
            Pending {
                client: client.clone(),
                listeners,
                redirect_uri,
                state: a.state,
                verifier: a.verifier,
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
        let want = reqwest::Url::parse(&self.redirect_uri);
        if want.map_or(true, |want| url.path() != want.path()) {
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
        redeem(&self.client, &self.redirect_uri, &self.verifier, code).await
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

// ---------------------------------------------------------------- pasted address

/// The loopback redirect without a listener, for a host the browser cannot reach
/// (Docker, SSH, a NAS, a LAN browser): the person signs in on any device, their
/// browser ends on an `http://localhost:<port>/?code=…&state=…` page that does not
/// load, and they paste that address back (rclone's pattern). Bunko checks `state`
/// and redeems the code with the PKCE verifier and the same `redirect_uri`.
pub struct PasteFlow;

impl PasteFlow {
    /// Build the authorization URL. Nothing listens on the redirect's port: it is
    /// random so that it is unlikely to reach a real service on the person's device.
    pub fn start(client: &OAuthClient) -> Result<(String, PastePending), AuthError> {
        let port: u16 = rand::rng().random_range(49152..=65535);
        let redirect_uri = loopback_redirect(client.provider, port);
        let a = authorization(client, &redirect_uri)?;
        Ok((
            a.url,
            PastePending {
                client: client.clone(),
                redirect_uri,
                state: a.state,
                verifier: a.verifier,
                timeout: PASTE_TIMEOUT,
            },
        ))
    }
}

/// A pasted-address sign-in waiting for the address. The verifier lives here, in
/// memory, and nowhere else; dropping this ends the sign-in.
pub struct PastePending {
    client: OAuthClient,
    redirect_uri: String,
    state: String,
    verifier: String,
    timeout: Duration,
}

impl fmt::Debug for PastePending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PastePending")
            .field("client", &self.client)
            .field("redirect_uri", &self.redirect_uri)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// Why a pasted address was not taken. All but [`PasteError::Ended`] leave the
/// sign-in waiting for another paste.
#[derive(Debug, thiserror::Error)]
pub enum PasteError {
    #[error("that is not a web address: copy the whole address from the browser's address bar")]
    NotAUrl,
    #[error("that is not the address the sign-in ended on: it starts with {expected}")]
    NotTheRedirect { expected: String },
    #[error(
        "that address belongs to another sign-in: paste the address the latest sign-in page ended on"
    )]
    WrongState,
    #[error(
        "that address carries no sign-in code: finish signing in, then paste the address the browser ends on"
    )]
    NoCode,
    /// The provider sent an error back (with this sign-in's `state`): it is over.
    #[error(transparent)]
    Ended(AuthError),
}

impl PasteError {
    /// The sign-in is over (the provider refused it); otherwise paste again.
    pub fn is_final(&self) -> bool {
        matches!(self, PasteError::Ended(_))
    }
}

/// The longest address taken (a code is a few hundred bytes; Microsoft's ~1 KiB).
const MAX_PASTED: usize = 8 * 1024;

/// Take the authorization code out of a pasted redirect address, strictly: an
/// `http` URL on exactly the redirect's host, port and path, with this sign-in's
/// `state`. Only `code`, `state`, `error` and `error_description` are read; other
/// parameters (Google's `scope`, `iss`) are ignored, a repeated one is refused.
pub fn parse_pasted_redirect(
    pasted: &str,
    redirect_uri: &str,
    expected_state: &str,
) -> Result<String, PasteError> {
    let s = pasted.trim();
    if s.is_empty()
        || s.len() > MAX_PASTED
        || s.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(PasteError::NotAUrl);
    }
    let url = reqwest::Url::parse(s).map_err(|_| PasteError::NotAUrl)?;
    let want = reqwest::Url::parse(redirect_uri).map_err(|_| PasteError::NotAUrl)?;
    let not_it = || PasteError::NotTheRedirect {
        expected: redirect_uri.to_string(),
    };
    if url.scheme() != "http"
        || url.host_str() != want.host_str()
        || url.port_or_known_default() != want.port_or_known_default()
        || url.path() != want.path()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(not_it());
    }
    let (mut code, mut state, mut error, mut description) = (None, None, None, None);
    for (k, v) in url.query_pairs() {
        let slot = match &*k {
            "code" => &mut code,
            "state" => &mut state,
            "error" => &mut error,
            "error_description" => &mut description,
            _ => continue,
        };
        if slot.replace(v.into_owned()).is_some() {
            return Err(not_it());
        }
    }
    if code.is_none() && error.is_none() {
        return Err(PasteError::NoCode);
    }
    if !state.is_some_and(|s| constant_time_eq(s.as_bytes(), expected_state.as_bytes())) {
        return Err(PasteError::WrongState);
    }
    if let Some(e) = error {
        return Err(PasteError::Ended(if e == "access_denied" {
            AuthError::Denied
        } else {
            AuthError::OAuth {
                status: 400,
                error: e,
                description: description.map(|d| truncate(d, 300)),
            }
        }));
    }
    match code {
        Some(c) if !c.is_empty() => Ok(c),
        _ => Err(PasteError::NoCode),
    }
}

impl PastePending {
    /// Where the person's browser ends: what the pasted address starts with.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Replace the default 10-minute timeout.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// The code of a pasted address ([`parse_pasted_redirect`] against this sign-in).
    pub fn code_from(&self, pasted: &str) -> Result<String, PasteError> {
        parse_pasted_redirect(pasted, &self.redirect_uri, &self.state)
    }

    /// Redeem the code with this sign-in's verifier and redirect URI. Single use:
    /// the sign-in ends here whatever the provider answers.
    pub async fn redeem(self, code: String) -> Result<Tokens, AuthError> {
        redeem(&self.client, &self.redirect_uri, &self.verifier, code).await
    }
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

    const MS_REDIRECT: &str = "http://localhost:51234/bunko";
    const STATE: &str = "the-state_0123456789";

    fn pasted(url: &str) -> Result<String, PasteError> {
        parse_pasted_redirect(url, MS_REDIRECT, STATE)
    }

    #[test]
    fn pasted_redirect_good() {
        assert_eq!(
            pasted("http://localhost:51234/bunko?code=M.C507_abc-123&state=the-state_0123456789")
                .unwrap(),
            "M.C507_abc-123"
        );
        // What browsers show: no trailing slash, padding whitespace, percent-escapes.
        assert_eq!(
            pasted("  http://localhost:51234/bunko?code=a%2Fb&state=the-state_0123456789\n")
                .unwrap(),
            "a/b"
        );
        // Google's redirect, with its extra parameters (ignored).
        let google = loopback_redirect(Provider::Google, 40000);
        assert_eq!(google, "http://127.0.0.1:40000/");
        assert_eq!(
            parse_pasted_redirect(
                "http://127.0.0.1:40000/?state=s1&iss=https%3A%2F%2Faccounts.google.com&code=4%2F0Ab&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive.file",
                &google,
                "s1"
            )
            .unwrap(),
            "4/0Ab"
        );
    }

    #[test]
    fn pasted_redirect_wrong_state() {
        assert!(matches!(
            pasted("http://localhost:51234/bunko?code=c&state=another-sign-in"),
            Err(PasteError::WrongState)
        ));
        assert!(matches!(
            pasted("http://localhost:51234/bunko?code=c"),
            Err(PasteError::WrongState)
        ));
        // A state that is a prefix is not the state.
        assert!(matches!(
            pasted("http://localhost:51234/bunko?code=c&state=the-state"),
            Err(PasteError::WrongState)
        ));
        assert!(!PasteError::WrongState.is_final());
    }

    #[test]
    fn pasted_redirect_access_denied() {
        let e = pasted(
            "http://localhost:51234/bunko?error=access_denied&error_description=The+user+has+denied+access&state=the-state_0123456789",
        )
        .unwrap_err();
        assert!(e.is_final());
        assert!(matches!(e, PasteError::Ended(AuthError::Denied)), "{e:?}");
        let e =
            pasted("http://localhost:51234/bunko?error=server_error&state=the-state_0123456789")
                .unwrap_err();
        assert!(
            matches!(e, PasteError::Ended(AuthError::OAuth { ref error, .. }) if error == "server_error")
        );
        // An error with another sign-in's state does not end this one.
        assert!(matches!(
            pasted("http://localhost:51234/bunko?error=access_denied&state=x"),
            Err(PasteError::WrongState)
        ));
    }

    #[test]
    fn pasted_redirect_with_junk() {
        // Text around or inside the address.
        for junk in [
            "here: http://localhost:51234/bunko?code=c&state=the-state_0123456789",
            "http://localhost:51234/bunko?code=c&state=the-state_0123456789 thanks",
            "\"http://localhost:51234/bunko?code=c&state=the-state_0123456789\"",
            "<http://localhost:51234/bunko?code=c&state=the-state_0123456789>",
            "",
            "c",
        ] {
            assert!(
                matches!(
                    pasted(junk),
                    Err(PasteError::NotAUrl | PasteError::NotTheRedirect { .. })
                ),
                "{junk:?}: {:?}",
                pasted(junk)
            );
        }
        // Unknown parameters and a fragment are ignored; the code still counts.
        assert_eq!(
            pasted(
                "http://localhost:51234/bunko?session_state=x&code=c&state=the-state_0123456789#junk"
            )
            .unwrap(),
            "c"
        );
        // A repeated code or state is ambiguous: refused.
        assert!(matches!(
            pasted("http://localhost:51234/bunko?code=a&code=b&state=the-state_0123456789"),
            Err(PasteError::NotTheRedirect { .. })
        ));
        assert!(matches!(
            pasted("http://localhost:51234/bunko?code=a&state=x&state=the-state_0123456789"),
            Err(PasteError::NotTheRedirect { .. })
        ));
        // No code at all.
        assert!(matches!(
            pasted("http://localhost:51234/bunko"),
            Err(PasteError::NoCode)
        ));
        assert!(matches!(
            pasted("http://localhost:51234/bunko?code=&state=the-state_0123456789"),
            Err(PasteError::NoCode)
        ));
        let huge = format!(
            "http://localhost:51234/bunko?code={}&state=the-state_0123456789",
            "a".repeat(MAX_PASTED)
        );
        assert!(matches!(pasted(&huge), Err(PasteError::NotAUrl)));
    }

    #[test]
    fn pasted_redirect_elsewhere_is_refused() {
        for other in [
            "https://evil.example/?code=c&state=the-state_0123456789",
            "http://evil.example:51234/?code=c&state=the-state_0123456789",
            "https://localhost:51234/?code=c&state=the-state_0123456789",
            "http://localhost:51235/?code=c&state=the-state_0123456789",
            "http://127.0.0.1:51234/?code=c&state=the-state_0123456789",
            "http://localhost/bunko?code=c&state=the-state_0123456789",
            "http://localhost:51234/?code=c&state=the-state_0123456789",
            "http://localhost:51234/bunko/x?code=c&state=the-state_0123456789",
            "http://user@localhost:51234/?code=c&state=the-state_0123456789",
            "http://localhost.evil.example:51234/?code=c&state=the-state_0123456789",
            "javascript:alert(1)//localhost:51234/?code=c&state=the-state_0123456789",
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize?code=c&state=the-state_0123456789",
        ] {
            match pasted(other) {
                Err(PasteError::NotTheRedirect { expected }) => assert_eq!(expected, MS_REDIRECT),
                r => panic!("{other}: {r:?}"),
            }
        }
    }

    #[test]
    fn paste_flow_builds_the_loopback_authorization() {
        let c = OAuthClient::microsoft("ms-cid");
        let (url, p) = PasteFlow::start(&c).unwrap();
        let u = reqwest::Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(q["redirect_uri"], p.redirect_uri());
        let port: u16 = p
            .redirect_uri()
            .strip_prefix("http://localhost:")
            .and_then(|p| p.strip_suffix("/bunko"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(port >= 49152);
        assert_eq!(q["code_challenge"], pkce_challenge(&p.verifier));
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], p.state);
        assert_eq!(q["scope"], "Files.ReadWrite offline_access User.Read");
        assert_eq!(p.timeout(), PASTE_TIMEOUT);
        // The pending sign-in's Debug shows neither the state nor the verifier.
        let d = format!("{p:?}");
        assert!(!d.contains(&p.verifier) && !d.contains(&p.state), "{d}");
        let ok = format!("{}?code=abc&state={}", p.redirect_uri(), q["state"]);
        assert_eq!(p.code_from(&ok).unwrap(), "abc");
    }

    #[test]
    fn aadsts70002_means_use_the_browser() {
        let body = json_body(
            "invalid_client",
            "AADSTS70002: The client does not exist or is not enabled for consumers. If you are the developer of this application, configure a new application through the App Registrations in the Azure Portal at https://go.microsoft.com/fwlink/?linkid=2083908. Trace ID: x Correlation ID: y Timestamp: 2026-10-14 10:00:00Z",
        );
        let e = error_response(400, body);
        assert!(matches!(e, AuthError::PersonalAccountNoCode), "{e:?}");
        assert_eq!(e.to_string(), PERSONAL_ACCOUNT_NO_CODE);
        assert!(e.to_string().contains("through the browser"));
        // Other errors keep their own mapping.
        assert!(matches!(
            error_response(400, json_body("invalid_grant", "AADSTS70000")),
            AuthError::InvalidGrant
        ));
        assert!(matches!(
            error_response(
                401,
                json_body("invalid_client", "AADSTS7000215: Invalid client secret")
            ),
            AuthError::OAuth { status: 401, .. }
        ));
        assert!(has_aadsts("x AADSTS7000215 y AADSTS70002: z", "70002"));
        assert!(has_aadsts("AADSTS70002", "70002"));
        assert!(!has_aadsts("AADSTS700021 AADSTS7000215", "70002"));
        assert!(matches!(
            error_response(502, "<html>bad gateway</html>".into()),
            AuthError::Http { status: 502, .. }
        ));
    }

    fn json_body(error: &str, description: &str) -> String {
        serde_json::json!({"error": error, "error_description": description}).to_string()
    }

    #[test]
    fn redirects_per_provider() {
        assert_eq!(
            loopback_redirect(Provider::Microsoft, 8080),
            "http://localhost:8080/bunko"
        );
        assert_eq!(
            loopback_redirect(Provider::Google, 8080),
            "http://127.0.0.1:8080/"
        );
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
