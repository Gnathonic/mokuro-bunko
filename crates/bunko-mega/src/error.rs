//! Errors. [`ErrorCode`] is vendored from mega-rs `error.rs` (codes and texts), with the
//! codes it lacked (-25, -400, -401) and a lossless `from_i64`; [`Error`] is new.

use std::time::Duration;

/// The `Result` type for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A failure, classified enough for a caller to map it to retry/auth/quota states.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// MEGA answered a negative code (for the whole request, one command or a transfer).
    #[error("MEGA error {0}")]
    Api(ErrorCode),
    /// An HTTP status other than 200 from the API or a transfer URL.
    #[error("HTTP {status}")]
    Http {
        status: u16,
        retry_after: Option<Duration>,
    },
    /// The transfer quota is used up (HTTP 509 or `-17` on a download).
    #[error("transfer quota exceeded")]
    OverQuota { retry_after: Option<Duration> },
    /// The connection failed or timed out.
    #[error("network: {0}")]
    Network(String),
    /// A response that does not have the expected shape.
    #[error("unexpected response: {0}")]
    Protocol(String),
    /// A key or attribute block that does not decrypt (wrong key, corrupt node).
    #[error("decryption failed: {0}")]
    Crypto(String),
    /// A full download whose condensed MAC differs from the node's.
    #[error("condensed MAC mismatch")]
    MacMismatch,
    /// A transfer that ended before (or after) the expected number of bytes.
    #[error("expected {expected} bytes, got {got}")]
    ShortRead { expected: u64, got: u64 },
    /// The serialised session cannot be used.
    #[error("invalid session: {0}")]
    InvalidSession(String),
    /// A hashcash challenge this crate cannot answer.
    #[error("hashcash: {0}")]
    Hashcash(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// The MEGA code, when this is one.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Error::Api(c) => Some(*c),
            _ => None,
        }
    }
}

impl From<ErrorCode> for Error {
    fn from(c: ErrorCode) -> Self {
        Error::Api(c)
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        // The URL carries the session id (`sid=`) or a transfer token: never keep it.
        let e = e.without_url();
        let kind = if e.is_timeout() {
            "timeout"
        } else if e.is_connect() {
            "connect"
        } else if e.is_body() || e.is_decode() {
            "body"
        } else {
            "request"
        };
        Error::Network(format!("{kind}: {e}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Protocol(json_err(&e))
    }
}

/// A JSON error without the text serde quotes from the input (which can be a key,
/// a session id or a file name): its category and position only.
pub(crate) fn json_err(e: &serde_json::Error) -> String {
    format!(
        "JSON {:?} error at line {} column {}",
        e.classify(),
        e.line(),
        e.column()
    )
}

impl From<base64::DecodeError> for Error {
    fn from(e: base64::DecodeError) -> Self {
        Error::Protocol(format!("base64: {e}"))
    }
}

/// An error code from MEGA's API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ErrorCode {
    #[error("no error")]
    OK,
    /// -1: internal error.
    #[error("internal error")]
    EINTERNAL,
    /// -2: invalid arguments.
    #[error("invalid argument")]
    EARGS,
    /// -3: request failed, but may be retried (no data was altered).
    #[error("request failed, retrying")]
    EAGAIN,
    /// -4: rate-limited.
    #[error("rate limit exceeded")]
    ERATELIMIT,
    /// -5: upload failed (restart it from scratch).
    #[error("failed permanently")]
    EFAILED,
    /// -6: too many IPs are accessing this resource.
    #[error("too many concurrent connections or transfers")]
    ETOOMANY,
    /// -7: the upload packet is out of range / not on a chunk boundary.
    #[error("out of range")]
    ERANGE,
    /// -8: the upload/download URL has expired.
    #[error("expired")]
    EEXPIRED,
    /// -9: object not found.
    #[error("not found")]
    ENOENT,
    /// -10: circular linkage attempted.
    #[error("circular linkage detected")]
    ECIRCULAR,
    /// -11: access violation.
    #[error("access denied")]
    EACCESS,
    /// -12: already exists.
    #[error("already exists")]
    EEXIST,
    /// -13: incomplete resource.
    #[error("incomplete")]
    EINCOMPLETE,
    /// -14: a decryption failed.
    #[error("invalid key / decryption error")]
    EKEY,
    /// -15: invalid or expired user session.
    #[error("bad session ID")]
    ESID,
    /// -16: user blocked.
    #[error("blocked")]
    EBLOCKED,
    /// -17: over quota.
    #[error("over quota")]
    EOVERQUOTA,
    /// -18: temporarily unavailable.
    #[error("temporarily not available")]
    ETEMPUNAVAIL,
    /// -19: too many connections to this resource.
    #[error("connection overflow")]
    ETOOMANYCONNECTIONS,
    /// -20: write failed.
    #[error("write error")]
    EWRITE,
    /// -21: read failed.
    #[error("read error")]
    EREAD,
    /// -22: invalid app key.
    #[error("invalid application key")]
    EAPPKEY,
    /// -23: SSL verification failed.
    #[error("SSL verification failed")]
    ESSL,
    /// -24: not enough quota.
    #[error("not enough quota")]
    EGOINGOVERQUOTA,
    /// -25: rolled back.
    #[error("rolled back")]
    EROLLEDBACK,
    /// -26: multi-factor authentication required.
    #[error("multi-factor authentication required")]
    EMFAREQUIRED,
    /// -27: access denied for sub-users.
    #[error("access denied for users")]
    EMASTERONLY,
    /// -28: business account expired.
    #[error("business account has expired")]
    EBUSINESSPASTDUE,
    /// -29: over-disk-quota paywall.
    #[error("storage quota exceeded")]
    EPAYWALL,
    /// -400.
    #[error("too many errors")]
    ETOOERR,
    /// -401.
    #[error("sharer over quota")]
    ESHAREROVERQUOTA,
    /// Any other value.
    #[error("unknown error {0}")]
    Other(i64),
}

impl ErrorCode {
    pub fn from_i64(n: i64) -> ErrorCode {
        use ErrorCode::*;
        match n {
            0 => OK,
            -1 => EINTERNAL,
            -2 => EARGS,
            -3 => EAGAIN,
            -4 => ERATELIMIT,
            -5 => EFAILED,
            -6 => ETOOMANY,
            -7 => ERANGE,
            -8 => EEXPIRED,
            -9 => ENOENT,
            -10 => ECIRCULAR,
            -11 => EACCESS,
            -12 => EEXIST,
            -13 => EINCOMPLETE,
            -14 => EKEY,
            -15 => ESID,
            -16 => EBLOCKED,
            -17 => EOVERQUOTA,
            -18 => ETEMPUNAVAIL,
            -19 => ETOOMANYCONNECTIONS,
            -20 => EWRITE,
            -21 => EREAD,
            -22 => EAPPKEY,
            -23 => ESSL,
            -24 => EGOINGOVERQUOTA,
            -25 => EROLLEDBACK,
            -26 => EMFAREQUIRED,
            -27 => EMASTERONLY,
            -28 => EBUSINESSPASTDUE,
            -29 => EPAYWALL,
            -400 => ETOOERR,
            -401 => ESHAREROVERQUOTA,
            n => Other(n),
        }
    }

    pub fn as_i64(self) -> i64 {
        use ErrorCode::*;
        match self {
            OK => 0,
            EINTERNAL => -1,
            EARGS => -2,
            EAGAIN => -3,
            ERATELIMIT => -4,
            EFAILED => -5,
            ETOOMANY => -6,
            ERANGE => -7,
            EEXPIRED => -8,
            ENOENT => -9,
            ECIRCULAR => -10,
            EACCESS => -11,
            EEXIST => -12,
            EINCOMPLETE => -13,
            EKEY => -14,
            ESID => -15,
            EBLOCKED => -16,
            EOVERQUOTA => -17,
            ETEMPUNAVAIL => -18,
            ETOOMANYCONNECTIONS => -19,
            EWRITE => -20,
            EREAD => -21,
            EAPPKEY => -22,
            ESSL => -23,
            EGOINGOVERQUOTA => -24,
            EROLLEDBACK => -25,
            EMFAREQUIRED => -26,
            EMASTERONLY => -27,
            EBUSINESSPASTDUE => -28,
            EPAYWALL => -29,
            ETOOERR => -400,
            ESHAREROVERQUOTA => -401,
            Other(n) => n,
        }
    }

    /// A JSON number as MEGA sends it (`-9`), or `None` for anything else.
    pub fn from_json(v: &serde_json::Value) -> Option<ErrorCode> {
        v.as_i64().map(ErrorCode::from_i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No variant's Display or Debug carries a secret, however the error was made.
    #[test]
    fn errors_never_format_secrets() {
        const SECRET: &str = "S3CRETsidVALUE";
        // A JSON error over a body that quotes the secret.
        let json = serde_json::from_str::<u64>(&format!("\"{SECRET}\"")).unwrap_err();
        let mut all = vec![
            Error::from(json),
            Error::Api(ErrorCode::ESID),
            Error::Http {
                status: 403,
                retry_after: None,
            },
            Error::OverQuota { retry_after: None },
            Error::MacMismatch,
            Error::ShortRead {
                expected: 1,
                got: 2,
            },
            Error::InvalidSession("too short".into()),
        ];
        // A transport error for a URL with the sid in its query.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let req = rt.block_on(async {
            reqwest::Client::new()
                .post(format!("http://127.0.0.1:9/cs?id=1&sid={SECRET}"))
                .send()
                .await
                .unwrap_err()
        });
        all.push(Error::from(req));
        for e in &all {
            for s in [e.to_string(), format!("{e:?}")] {
                assert!(!s.contains(SECRET), "{s}");
                assert!(!s.contains("sid="), "{s}");
            }
        }
    }

    #[test]
    fn codes_round_trip() {
        for n in [0, -1, -3, -9, -15, -17, -19, -26, -29, -400, -401, -77] {
            assert_eq!(ErrorCode::from_i64(n).as_i64(), n);
        }
        assert_eq!(ErrorCode::from_i64(-17), ErrorCode::EOVERQUOTA);
        assert_eq!(
            ErrorCode::from_json(&serde_json::json!(-15)),
            Some(ErrorCode::ESID)
        );
        assert_eq!(ErrorCode::from_json(&serde_json::json!("x")), None);
    }
}
