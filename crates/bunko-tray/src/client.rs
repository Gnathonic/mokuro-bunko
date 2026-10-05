//! The control API client (GUI.md §2): loopback HTTP with the instance's bearer token.

use crate::status::{ControlFile, Status};
use std::io::BufRead;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{0}")]
    Http(String),
    #[error("HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("unexpected answer: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<ureq::Error> for ClientError {
    fn from(e: ureq::Error) -> Self {
        ClientError::Http(e.to_string())
    }
}

/// What a pause asks for (`POST /control/pause`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PauseRequest {
    /// `after_volume` or `now`.
    pub mode: &'static str,
    /// ISO-8601 with offset, or none for "until resumed".
    pub until: Option<String>,
}

#[derive(Clone)]
pub struct Client {
    base: String,
    auth: String,
    agent: ureq::Agent,
    /// No overall timeout: an event stream stays open.
    stream_agent: ureq::Agent,
}

fn agent(timeout: Option<Duration>) -> ureq::Agent {
    ureq::Agent::config_builder()
        // Loopback only: never send the token through a configured HTTP proxy.
        .proxy(None)
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(2)))
        .timeout_global(timeout)
        .build()
        .into()
}

impl Client {
    pub fn new(control: &ControlFile) -> Client {
        Client::with_base(control.base_url(), &control.token)
    }

    pub fn with_base(base: String, token: &str) -> Client {
        Client {
            base,
            auth: format!("Bearer {token}"),
            agent: agent(Some(Duration::from_secs(5))),
            stream_agent: agent(None),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn finish(mut resp: ureq::http::Response<ureq::Body>) -> Result<String, ClientError> {
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string()?;
        if !(200..300).contains(&status) {
            return Err(ClientError::Status {
                status,
                body: body.chars().take(200).collect(),
            });
        }
        Ok(body)
    }

    pub fn status(&self) -> Result<Status, ClientError> {
        let resp = self
            .agent
            .get(format!("{}/control/status", self.base))
            .header("Authorization", &self.auth)
            .call()?;
        Ok(serde_json::from_str(&Self::finish(resp)?)?)
    }

    fn post(&self, path: &str, body: &str) -> Result<String, ClientError> {
        let resp = self
            .agent
            .post(format!("{}{path}", self.base))
            .header("Authorization", &self.auth)
            .header("Content-Type", "application/json")
            .send(body)?;
        Self::finish(resp)
    }

    pub fn pause(&self, req: &PauseRequest) -> Result<Status, ClientError> {
        let body = serde_json::to_string(req)?;
        Ok(serde_json::from_str(&self.post("/control/pause", &body)?)?)
    }

    pub fn resume(&self) -> Result<Status, ClientError> {
        Ok(serde_json::from_str(&self.post("/control/resume", "{}")?)?)
    }

    /// Ask a managed instance to shut down cleanly (the tray's Quit).
    pub fn stop(&self) -> Result<(), ClientError> {
        self.post("/control/stop", "{}").map(|_| ())
    }

    /// Follow `GET /control/events` and call `on_status` for every `status` event until
    /// the stream ends, `on_status` returns false, or an error occurs.
    pub fn follow_events(
        &self,
        mut on_status: impl FnMut(Status) -> bool,
    ) -> Result<(), ClientError> {
        let resp = self
            .stream_agent
            .get(format!("{}/control/events", self.base))
            .header("Authorization", &self.auth)
            .header("Accept", "text/event-stream")
            .call()?;
        let code = resp.status().as_u16();
        if !(200..300).contains(&code) {
            return Err(ClientError::Status {
                status: code,
                body: String::new(),
            });
        }
        let reader = std::io::BufReader::new(resp.into_body().into_reader());
        let mut parser = SseParser::default();
        for line in reader.lines() {
            let line = line.map_err(|e| ClientError::Http(e.to_string()))?;
            if let Some((event, data)) = parser.line(&line)
                && (event.is_empty() || event == "status")
            {
                let status: Status = serde_json::from_str(&data)?;
                if !on_status(status) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// A single-use sign-in code (`POST /control/login-code`, bearer only).
    pub fn login_code(&self) -> Result<String, ClientError> {
        #[derive(serde::Deserialize)]
        struct Code {
            code: String,
        }
        let code: Code = serde_json::from_str(&self.post("/control/login-code", "{}")?)?;
        Ok(code.code)
    }

    /// The browser URL that signs in with `code` and lands on `next` (GUI.md §1:
    /// `/app/login?c=`). The token itself never goes into a URL.
    pub fn login_url(&self, code: &str, next: &str) -> String {
        format!(
            "{}/app/login?c={}&next={}",
            self.base,
            url_escape(code),
            url_escape(next)
        )
    }

    /// A fresh code, then its sign-in URL: what "Open dashboard" opens.
    pub fn sign_in_url(&self, next: &str) -> Result<String, ClientError> {
        Ok(self.login_url(&self.login_code()?, next))
    }
}

fn url_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Server-Sent Events framing: `event:`/`data:` lines, a blank line dispatches.
#[derive(Default)]
pub struct SseParser {
    event: String,
    data: Vec<String>,
}

impl SseParser {
    /// Feed one line (without its line ending); returns `(event, data)` on dispatch.
    pub fn line(&mut self, line: &str) -> Option<(String, String)> {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            if self.data.is_empty() {
                self.event.clear();
                return None;
            }
            let data = self.data.join("\n");
            self.data.clear();
            return Some((std::mem::take(&mut self.event), data));
        }
        if line.starts_with(':') {
            return None; // comment / keep-alive
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_framing() {
        let mut p = SseParser::default();
        let mut got = Vec::new();
        for line in [
            ": keep-alive",
            "event: status",
            "data: {\"a\":",
            "data: 1}",
            "",
            "",
            "data: x\r",
            "\r",
        ] {
            if let Some(ev) = p.line(line) {
                got.push(ev);
            }
        }
        assert_eq!(
            got,
            [
                ("status".to_string(), "{\"a\":\n1}".to_string()),
                (String::new(), "x".to_string())
            ]
        );
    }

    #[test]
    fn login_url_escapes() {
        let c = Client::with_base("http://127.0.0.1:9".into(), "t");
        assert_eq!(
            c.login_url("a+b/c=", "/app/settings?x=1"),
            "http://127.0.0.1:9/app/login?c=a%2Bb/c%3D&next=/app/settings%3Fx%3D1"
        );
    }

    /// "Open dashboard": ask for a code with the bearer token, then a URL with the code
    /// (never the token).
    #[test]
    fn sign_in_asks_for_a_code_first() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for answer in [
                (
                    200,
                    r#"{"code":"0123456789abcdef0123456789abcdef","expires_in":300}"#,
                ),
                (401, r#"{"error":"no"}"#),
            ] {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !String::from_utf8_lossy(&buf).contains("\r\n\r\n") {
                    let n = s.read(&mut chunk).unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                requests.push(String::from_utf8_lossy(&buf).into_owned());
                let (code, body) = answer;
                write!(
                    s,
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            requests
        });
        let c = Client::with_base(format!("http://127.0.0.1:{port}"), "the-token");
        let url = c.sign_in_url("/app/dashboard").unwrap();
        assert_eq!(
            url,
            format!(
                "http://127.0.0.1:{port}/app/login?c=0123456789abcdef0123456789abcdef&next=/app/dashboard"
            )
        );
        assert!(!url.contains("the-token"));
        // Refused (an old instance, a wrong token): no URL to open.
        assert!(matches!(
            c.sign_in_url("/app/"),
            Err(ClientError::Status { status: 401, .. })
        ));
        let requests = server.join().unwrap();
        let first = requests[0].to_ascii_lowercase();
        assert!(first.starts_with("post /control/login-code "), "{first}");
        assert!(first.contains("authorization: bearer the-token"), "{first}");
    }
}
