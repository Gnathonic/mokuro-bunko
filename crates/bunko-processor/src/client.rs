//! The processor's HTTP side of the library: token login, registration, the
//! WebSocket, result uploads (protocol v3, `docs/rust-port/PROTOCOL.md`).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use bunko_proto::{PROCESSOR_ROOT, RegisterReply, RegisterRequest};
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use parking_lot::RwLock;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio_tungstenite::tungstenite;

use crate::config::LibrarySettings;
use crate::tls::{self, TlsError};

/// What a refused or failed exchange means for the serve loop.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ClientError {
    /// The account was refused (401/403 at login or registration, twice): final,
    /// `processor serve` exits 1.
    #[error("{0}")]
    LoginRefused(String),
    /// The library wants a fresh registration now (409 on the socket, a vanished
    /// entry): re-register after the 1 s floor, no backoff.
    #[error("{0}")]
    Reregister(String),
    /// Anything else (unreachable, 5xx, 429, a protocol mismatch, a 409 name): back off.
    #[error("{0}")]
    Library(String),
}

impl From<TlsError> for ClientError {
    fn from(e: TlsError) -> Self {
        ClientError::Library(e.0)
    }
}

/// Percent-encode a URL path the way 0.5.2's `quote(path, safe="/")` did.
const PATH_SAFE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

pub fn quote_path(path: &str) -> String {
    utf8_percent_encode(path, PATH_SAFE).to_string()
}

/// The `Authorization` every request carries: the bearer token once issued.
#[derive(Debug)]
pub struct Credentials {
    basic: String,
    bearer: RwLock<Option<String>>,
}

impl Credentials {
    pub fn new(username: &str, password: &str) -> Credentials {
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        Credentials {
            basic: format!("Basic {encoded}"),
            bearer: RwLock::new(None),
        }
    }

    /// `Bearer <token>`, or Basic before a token was issued.
    pub fn header(&self) -> String {
        self.bearer
            .read()
            .as_ref()
            .map(|t| format!("Bearer {t}"))
            .unwrap_or_else(|| self.basic.clone())
    }

    pub fn basic(&self) -> &str {
        &self.basic
    }

    pub fn has_token(&self) -> bool {
        self.bearer.read().is_some()
    }

    fn set_token(&self, token: Option<String>) {
        *self.bearer.write() = token;
    }
}

/// One HTTP client over the shared TLS settings. `pooled = false` gives every request
/// a fresh connection (each archive download attempt is its own connection, as in
/// 0.5.2). Redirects are never followed and nothing is decompressed (a gzip
/// transfer-encoding would hide `Content-Length` and break `Range` resumes).
pub fn http_client(
    tls: &Arc<rustls::ClientConfig>,
    connect_timeout: Duration,
    pooled: bool,
) -> Result<reqwest::Client, ClientError> {
    let mut builder = reqwest::Client::builder()
        .use_preconfigured_tls((**tls).clone())
        .connect_timeout(connect_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .no_gzip()
        .user_agent(concat!(
            "mokuro-bunko-processor/",
            env!("CARGO_PKG_VERSION")
        ));
    if !pooled {
        builder = builder.pool_max_idle_per_host(0);
    }
    builder
        .build()
        .map_err(|e| ClientError::Library(format!("could not build the HTTP client: {e}")))
}

pub(crate) fn describe_reqwest(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "timed out".to_string();
    }
    let mut text = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        text = format!("{text}: {s}");
        source = s.source();
    }
    text
}

/// Register, connect, upload: everything the processor says to the library.
#[derive(Clone)]
pub struct LibraryClient {
    inner: Arc<Inner>,
}

struct Inner {
    /// The library URL without a trailing slash (any path prefix kept).
    url: String,
    name: String,
    http: reqwest::Client,
    tls: Arc<rustls::ClientConfig>,
    creds: Arc<Credentials>,
}

pub(crate) const JSON_TIMEOUT: Duration = Duration::from_secs(30);
const UPLOAD_ATTEMPTS: usize = 3;

impl LibraryClient {
    /// A client for `library`, registering as `name` (the token's label).
    pub fn new(library: &LibrarySettings, name: &str) -> Result<LibraryClient, ClientError> {
        let tls = tls::client_config(&library.tls_verify)?;
        let http = http_client(&tls, Duration::from_secs(15), true)?;
        Ok(LibraryClient {
            inner: Arc::new(Inner {
                url: library.url.trim_end_matches('/').to_string(),
                name: name.to_string(),
                http,
                tls,
                creds: Arc::new(Credentials::new(&library.username, &library.password)),
            }),
        })
    }

    pub fn url(&self) -> &str {
        &self.inner.url
    }

    pub fn credentials(&self) -> Arc<Credentials> {
        self.inner.creds.clone()
    }

    pub fn tls(&self) -> Arc<rustls::ClientConfig> {
        self.inner.tls.clone()
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.inner.url, path)
    }

    /// Trade the password for a bearer token (`POST /login/api/token`).
    pub async fn issue_token(&self) -> Result<(), ClientError> {
        let body = serde_json::json!({"kind": "processor", "label": self.inner.name});
        let response = self
            .inner
            .http
            .post(self.endpoint("/login/api/token"))
            .header(AUTHORIZATION, self.inner.creds.basic())
            .header(ACCEPT, "application/json")
            .json(&body)
            .timeout(JSON_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                ClientError::Library(format!(
                    "could not reach {}: {}",
                    self.inner.url,
                    describe_reqwest(&e)
                ))
            })?;
        let status = response.status().as_u16();
        let raw = response.bytes().await.unwrap_or_default();
        if status == 404 || status == 405 {
            return Err(ClientError::Library(format!(
                "the library at {} issues no processor tokens ({status}); update it to this machine's release",
                self.inner.url
            )));
        }
        if status != 200 {
            return Err(refusal(status, &raw));
        }
        let token = serde_json::from_slice::<Value>(&raw)
            .ok()
            .and_then(|v| v.get("token").and_then(Value::as_str).map(str::to_string))
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                ClientError::Library("the library's token reply carried no token".to_string())
            })?;
        self.inner.creds.set_token(Some(token));
        Ok(())
    }

    /// Log in (a token, once) and register. A refusal with a held token gets one fresh
    /// token and one more try; a second refusal is [`ClientError::LoginRefused`].
    pub async fn register(&self, request: &RegisterRequest) -> Result<RegisterReply, ClientError> {
        if !self.inner.creds.has_token() {
            self.issue_token().await?;
        }
        match self.register_once(request).await {
            Err(ClientError::LoginRefused(_)) => {
                self.issue_token().await?;
                self.register_once(request).await
            }
            other => other,
        }
    }

    async fn register_once(&self, request: &RegisterRequest) -> Result<RegisterReply, ClientError> {
        let response = self
            .inner
            .http
            .post(self.endpoint(&format!("{PROCESSOR_ROOT}/register")))
            .header(AUTHORIZATION, self.inner.creds.header())
            .header(ACCEPT, "application/json")
            .json(request)
            .timeout(JSON_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                ClientError::Library(format!(
                    "could not reach {}: {}",
                    self.inner.url,
                    describe_reqwest(&e)
                ))
            })?;
        let status = response.status().as_u16();
        let raw = response.bytes().await.unwrap_or_default();
        if status != 200 {
            return Err(refusal(status, &raw));
        }
        let reply: RegisterReply = serde_json::from_slice(&raw).map_err(|e| {
            ClientError::Library(format!(
                "the library's registration reply is not readable: {e}"
            ))
        })?;
        if reply.processor_id.is_empty() || reply.socket.is_empty() {
            return Err(ClientError::Library(
                "the library's registration reply named no channels".to_string(),
            ));
        }
        Ok(reply)
    }

    /// Open the processor's WebSocket (`socket` is the registration's path). A 401
    /// gets a fresh token and one more try.
    pub async fn connect_socket(&self, socket: &str) -> Result<WebSocket, ClientError> {
        match self.connect_socket_once(socket).await {
            Err(SocketRefusal::Unauthorized(_)) => {
                self.issue_token().await?;
                self.connect_socket_once(socket)
                    .await
                    .map_err(SocketRefusal::into_error)
            }
            other => other.map_err(SocketRefusal::into_error),
        }
    }

    async fn connect_socket_once(&self, socket: &str) -> Result<WebSocket, SocketRefusal> {
        use tungstenite::client::IntoClientRequest;
        let url = self.endpoint(socket);
        let ws_url = if let Some(rest) = url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            url
        };
        let mut request = ws_url.as_str().into_client_request().map_err(|e| {
            SocketRefusal::Other(ClientError::Library(format!(
                "bad socket URL {ws_url}: {e}"
            )))
        })?;
        let auth = http::HeaderValue::from_str(&self.inner.creds.header()).map_err(|_| {
            SocketRefusal::Other(ClientError::Library(
                "the credentials are not a valid header".into(),
            ))
        })?;
        request.headers_mut().insert(AUTHORIZATION, auth);
        let config = tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(16 << 20))
            .max_frame_size(Some(16 << 20));
        let connector = tokio_tungstenite::Connector::Rustls(self.inner.tls.clone());
        let connect = tokio_tungstenite::connect_async_tls_with_config(
            request,
            Some(config),
            false,
            Some(connector),
        );
        let result = tokio::time::timeout(JSON_TIMEOUT, connect)
            .await
            .map_err(|_| {
                SocketRefusal::Other(ClientError::Library(
                    "the processor socket timed out opening".into(),
                ))
            })?;
        match result {
            Ok((stream, _response)) => Ok(stream),
            Err(tungstenite::Error::Http(response)) => {
                let status = response.status().as_u16();
                let body = response.body().clone().unwrap_or_default();
                let detail = refusal_text(&body);
                Err(match status {
                    401 => SocketRefusal::Unauthorized(detail),
                    409 => SocketRefusal::Other(ClientError::Reregister(format!(
                        "the library wants a fresh registration: {detail}"
                    ))),
                    403 | 404 => SocketRefusal::Other(ClientError::Reregister(format!(
                        "the library no longer knows this registration ({status}): {detail}"
                    ))),
                    _ => SocketRefusal::Other(ClientError::Library(format!(
                        "the library answered {status} to the processor socket: {detail}"
                    ))),
                })
            }
            Err(e) => Err(SocketRefusal::Other(ClientError::Library(format!(
                "could not open the processor socket at {}: {e}",
                self.inner.url
            )))),
        }
    }

    /// `PUT` the finished sidecar (streamed from disk) to the registration's results
    /// path; returns its sha256 (lowercase hex). Transient failures are retried; a 401
    /// gets a fresh token once.
    pub async fn upload_result(
        &self,
        template: &str,
        sid: &str,
        claim: &str,
        file: &Path,
        name: &str,
    ) -> Result<String, String> {
        let sha = sha256_file(file)
            .await
            .map_err(|e| format!("could not read {}: {e}", file.display()))?;
        let length = tokio::fs::metadata(file)
            .await
            .map_err(|e| format!("could not read {}: {e}", file.display()))?
            .len();
        let url = self.endpoint(&template.replace("{sid}", sid).replace("{claim}", claim));
        // Generous, but never forever: two minutes plus 10 s per MB.
        let timeout = Duration::from_secs(120 + length / 100_000);
        let mut reissued = false;
        let mut failures = 0usize;
        loop {
            let handle = tokio::fs::File::open(file)
                .await
                .map_err(|e| format!("could not read {}: {e}", file.display()))?;
            let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::with_capacity(
                handle,
                256 * 1024,
            ));
            let sent = self
                .inner
                .http
                .put(&url)
                .header(AUTHORIZATION, self.inner.creds.header())
                .header(CONTENT_TYPE, "application/octet-stream")
                .header(CONTENT_LENGTH, length)
                .header(bunko_proto::HEADER_RESULT_NAME, quote_path(name))
                .header(bunko_proto::HEADER_RESULT_SHA256, &sha)
                .timeout(timeout)
                .body(body)
                .send()
                .await;
            let failure = match sent {
                Err(e) => describe_reqwest(&e),
                Ok(response) => {
                    let status = response.status().as_u16();
                    if (200..300).contains(&status) {
                        return Ok(sha);
                    }
                    let raw = response.bytes().await.unwrap_or_default();
                    let message = format!("the library answered {status}: {}", refusal_text(&raw));
                    match status {
                        401 if !reissued => {
                            reissued = true;
                            self.issue_token().await.map_err(|e| e.to_string())?;
                            continue;
                        }
                        408 | 429 | 500..=599 => message,
                        _ => return Err(message),
                    }
                }
            };
            failures += 1;
            if failures >= UPLOAD_ATTEMPTS {
                return Err(failure);
            }
            tracing::warn!("uploading the sidecar of {claim} failed ({failure}); trying again");
            tokio::time::sleep(Duration::from_secs(failures as u64)).await;
        }
    }

    /// One JSON exchange with Basic credentials and no redirects (the setup wizard's
    /// checks): `(status, JSON object or None, Location)`.
    pub async fn exchange_basic(
        &self,
        method: http::Method,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<(u16, Option<serde_json::Map<String, Value>>, Option<String>), reqwest::Error> {
        let mut request = self
            .inner
            .http
            .request(method, self.endpoint(path))
            .header(AUTHORIZATION, self.inner.creds.basic())
            .header(ACCEPT, "application/json")
            .timeout(timeout);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let raw = response.bytes().await?;
        let parsed = serde_json::from_slice::<Value>(&raw)
            .ok()
            .and_then(|v| match v {
                Value::Object(m) => Some(m),
                _ => None,
            });
        Ok((status, parsed, location))
    }
}

/// The library's WebSocket (over TLS when the URL is https).
pub type WebSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

enum SocketRefusal {
    Unauthorized(String),
    Other(ClientError),
}

impl SocketRefusal {
    fn into_error(self) -> ClientError {
        match self {
            SocketRefusal::Unauthorized(detail) => ClientError::LoginRefused(format!(
                "the library refused this account (401): {detail}"
            )),
            SocketRefusal::Other(e) => e,
        }
    }
}

fn refusal_text(raw: &[u8]) -> String {
    match serde_json::from_slice::<Value>(raw) {
        Ok(Value::Object(m)) => m
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| String::from_utf8_lossy(raw).chars().take(200).collect()),
        _ => String::from_utf8_lossy(raw).chars().take(200).collect(),
    }
}

/// 0.5.2 `LibraryClient._refusal`.
fn refusal(status: u16, raw: &[u8]) -> ClientError {
    let body = serde_json::from_slice::<Value>(raw).ok();
    let detail = refusal_text(raw);
    if status == 401 || status == 403 {
        return ClientError::LoginRefused(format!(
            "the library refused this account ({status}): {detail}"
        ));
    }
    if let Some(versions) = body
        .as_ref()
        .and_then(|b| b.get("protocols"))
        .filter(|v| !v.is_null())
    {
        return ClientError::Library(format!(
            "{detail} (this library speaks protocol {versions})"
        ));
    }
    ClientError::Library(format!("the library answered {status}: {detail}"))
}

/// sha256 of a file, lowercase hex, read in 1 MiB steps.
pub async fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_matches_python() {
        assert_eq!(
            quote_path("/mokuro-reader/A b/Vol 1~.cbz"),
            "/mokuro-reader/A%20b/Vol%201~.cbz"
        );
        assert_eq!(quote_path("/x/漫画.cbz"), "/x/%E6%BC%AB%E7%94%BB.cbz");
    }

    #[test]
    fn refusals_are_classified() {
        assert!(
            matches!(refusal(401, br#"{"error":"nope"}"#), ClientError::LoginRefused(m) if m.contains("nope"))
        );
        match refusal(400, br#"{"error":"this server speaks protocol 3, not 2","protocols":[3],"version":"0.7.0"}"#) {
            ClientError::Library(m) => assert_eq!(m, "this server speaks protocol 3, not 2 (this library speaks protocol [3])"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            refusal(503, b"busy"),
            ClientError::Library("the library answered 503: busy".into())
        );
    }

    #[test]
    fn credentials_prefer_the_token() {
        let c = Credentials::new("u", "p");
        assert_eq!(c.header(), "Basic dTpw");
        c.set_token(Some("t".into()));
        assert_eq!(c.header(), "Bearer t");
    }
}
