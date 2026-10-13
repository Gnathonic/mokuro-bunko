//! Drive v3 wire types, query strings, quota-unit costs and error classification.

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

use crate::backend::{BErr, HashKind, RNode};

/// The folder MIME type.
pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

/// The file fields every listing, get and write asks for.
pub const FILE_FIELDS: &str = "id,name,mimeType,parents,size,modifiedTime,createdTime,md5Checksum,sha256Checksum,version,headRevisionId,trashed";

/// Largest page Drive serves for `files.list` and `changes.list`.
pub const PAGE_SIZE: u32 = 1000;

/// How long a `dailyLimitExceeded` pauses the backend: the project cap resets daily,
/// so retrying sooner only burns calls.
pub const DAILY_LIMIT_PAUSE: Duration = Duration::from_secs(3600);

/// Drive quota-unit costs per request (limits page, updated 2026-09-11; spec §5.11).
/// The governor meters `GDriveBackend::units_used()` against its budget.
pub mod units {
    /// `files.get` metadata, and the other small reads (`about.get`,
    /// `changes.getStartPageToken`, `files.generateIds`, an upload-session status query).
    pub const GET: u64 = 5;
    /// `files.list`.
    pub const LIST: u64 = 100;
    /// A media download (`alt=media`, plain or pinned to a revision).
    pub const DOWNLOAD: u64 = 200;
    /// `files.update` (metadata or media), `files.delete`, and each resumable chunk PUT.
    pub const UPDATE: u64 = 50;
    /// `files.create` (metadata, multipart, or a resumable session start) and `files.copy`.
    pub const CREATE: u64 = 50;
    /// `changes.list`; not on the limits page [unverified], costed like a list.
    pub const CHANGES: u64 = 100;
}

fn de_num_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum N {
        S(String),
        I(i64),
    }
    Ok(match Option::<N>::deserialize(d)? {
        None => None,
        Some(N::I(i)) => Some(i),
        Some(N::S(s)) => s.parse().ok(),
    })
}

/// A `files` resource restricted to [`FILE_FIELDS`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DFile {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default, deserialize_with = "de_num_opt")]
    pub size: Option<i64>,
    pub modified_time: Option<String>,
    pub created_time: Option<String>,
    pub md5_checksum: Option<String>,
    pub sha256_checksum: Option<String>,
    #[serde(default, deserialize_with = "de_num_opt")]
    pub version: Option<i64>,
    pub head_revision_id: Option<String>,
    #[serde(default)]
    pub trashed: bool,
}

impl DFile {
    pub fn is_dir(&self) -> bool {
        self.mime_type == FOLDER_MIME
    }

    pub fn version_str(&self) -> String {
        self.version.map(|v| v.to_string()).unwrap_or_default()
    }

    /// The content version: `md5Checksum` for binary files, `version` for folders and
    /// for Google-native files, which have no md5.
    pub fn cver(&self) -> String {
        match (&self.md5_checksum, self.is_dir()) {
            (Some(md5), false) => md5.clone(),
            _ => self.version_str(),
        }
    }

    /// The provider hash: sha256 when Drive has it (older files may not), else md5.
    pub fn hash(&self) -> Option<(HashKind, String)> {
        if self.is_dir() {
            return None;
        }
        if let Some(s) = &self.sha256_checksum {
            return Some((HashKind::Sha256, s.to_ascii_lowercase()));
        }
        self.md5_checksum
            .as_ref()
            .map(|m| (HashKind::Md5, m.to_ascii_lowercase()))
    }

    pub fn to_rnode(&self, parent: Option<String>) -> RNode {
        RNode {
            pid: self.id.clone(),
            parent,
            name_raw: self.name.as_bytes().to_vec(),
            is_dir: self.is_dir(),
            size: self.size.unwrap_or(0).max(0) as u64,
            base_mtime_ms: self.modified_time.as_deref().map_or(0, rfc3339_ms),
            created_ms: self.created_time.as_deref().map_or(0, rfc3339_ms),
            version: self.version_str(),
            cver: self.cver(),
            pin: self.head_revision_id.clone(),
            hash: self.hash(),
            trashed: self.trashed,
        }
    }
}

/// Unix ms of an RFC 3339 timestamp; 0 when it does not parse.
pub fn rfc3339_ms(s: &str) -> i64 {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
        .unwrap_or(0)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileList {
    #[serde(default)]
    pub files: Vec<DFile>,
    pub next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DChange {
    #[serde(default)]
    pub removed: bool,
    pub file_id: Option<String>,
    pub file: Option<DFile>,
    pub change_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeList {
    #[serde(default)]
    pub changes: Vec<DChange>,
    pub next_page_token: Option<String>,
    pub new_start_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartToken {
    pub start_page_token: String,
}

#[derive(Debug, Deserialize)]
pub struct GeneratedIds {
    pub ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageQuota {
    #[serde(default, deserialize_with = "de_num_opt")]
    pub limit: Option<i64>,
    #[serde(default, deserialize_with = "de_num_opt")]
    pub usage: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub permission_id: Option<String>,
    pub email_address: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct About {
    pub storage_quota: Option<StorageQuota>,
    pub user: Option<User>,
}

#[derive(Debug, Default, Deserialize)]
struct ErrorItem {
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Default, Deserialize)]
struct ErrorInner {
    #[serde(default)]
    message: String,
    #[serde(default)]
    errors: Vec<ErrorItem>,
}

#[derive(Debug, Default, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: ErrorInner,
}

/// A parsed Drive error response.
#[derive(Debug, Clone, Default)]
pub struct ApiError {
    pub status: u16,
    pub reasons: Vec<String>,
    pub message: String,
}

impl ApiError {
    pub fn parse(status: u16, body: &[u8]) -> ApiError {
        let parsed: ErrorBody = serde_json::from_slice(body).unwrap_or_default();
        let message = if parsed.error.message.is_empty() {
            String::from_utf8_lossy(&body[..body.len().min(200)]).into_owned()
        } else {
            parsed.error.message
        };
        ApiError {
            status,
            reasons: parsed.error.errors.into_iter().map(|e| e.reason).collect(),
            message,
        }
    }

    pub fn has(&self, reason: &str) -> bool {
        self.reasons.iter().any(|r| r == reason)
    }

    /// A rate limit (back off and retry), as opposed to a hard 403.
    pub fn is_throttle(&self) -> bool {
        self.status == 429
            || (self.status == 403
                && (self.has("rateLimitExceeded")
                    || self.has("userRateLimitExceeded")
                    || self.has("sharingRateLimitExceeded")
                    || self.is_daily_limit()))
    }

    pub fn is_daily_limit(&self) -> bool {
        self.has("dailyLimitExceeded") || self.has("dailyLimitExceededUnreg")
    }

    /// Classify for the governor and the outbox. `retry_after` is the response's
    /// `Retry-After`; `backoff` the exponential suggestion when it has none.
    pub fn classify(&self, retry_after: Option<Duration>, backoff: Duration) -> BErr {
        let msg = || format!("drive {}: {}", self.status, self.message);
        match self.status {
            401 => BErr::AuthExpired,
            403 if self.is_daily_limit() => {
                tracing::error!(
                    reasons = ?self.reasons,
                    "Google Drive: the project's daily API limit is exhausted; every install \
                     sharing it is paused (dailyLimitExceeded)"
                );
                BErr::Throttled(Some(retry_after.unwrap_or(DAILY_LIMIT_PAUSE)))
            }
            403 | 429 if self.is_throttle() => {
                BErr::Throttled(Some(retry_after.unwrap_or(backoff)))
            }
            403 if self.has("storageQuotaExceeded") => BErr::StorageFull,
            403 if self.has("insufficientPermissions")
                || self.has("appNotAuthorized")
                || self.has("authError") =>
            {
                BErr::AuthRevoked
            }
            // Per file, not per grant: under `drive.file` a file this app cannot see is
            // as good as absent.
            403 if self.has("appNotAuthorizedToFile") => BErr::NotFound,
            404 | 410 => BErr::NotFound,
            409 => BErr::NameConflict,
            412 => BErr::PreconditionFailed,
            413 => BErr::TooLarge,
            408 | 500..=599 => BErr::Transient(msg()),
            _ => BErr::Fatal(msg()),
        }
    }
}

/// A `q` string literal: backslash and single quote escaped.
pub fn q_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\\' || c == '\'' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// A `multipart/related` body: JSON metadata, then the media.
pub fn multipart_related(meta: &serde_json::Value, media: &[u8], boundary: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(media.len() + 512);
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(b"Content-Type: application/json; charset=UTF-8\r\n\r\n");
    out.extend_from_slice(meta.to_string().as_bytes());
    out.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    out.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    out.extend_from_slice(media);
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    out
}

/// The next offset from a resumable session's `Range: bytes=0-N` header (0 when absent).
pub fn next_offset(range: Option<&str>) -> u64 {
    range
        .and_then(|r| r.trim().strip_prefix("bytes="))
        .and_then(|r| r.split('-').nth(1))
        .and_then(|n| n.trim().parse::<u64>().ok())
        .map_or(0, |last| last + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_literals_escape() {
        assert_eq!(q_str("a'b\\c"), r"'a\'b\\c'");
    }

    #[test]
    fn session_ranges() {
        assert_eq!(next_offset(None), 0);
        assert_eq!(next_offset(Some("bytes=0-524287")), 524288);
    }

    #[test]
    fn numbers_as_strings() {
        let f: DFile = serde_json::from_str(
            r#"{"id":"x","size":"12","version":7,"mimeType":"a","modifiedTime":"2026-10-12T01:02:03.456Z"}"#,
        )
        .unwrap();
        assert_eq!(f.size, Some(12));
        assert_eq!(f.version, Some(7));
        assert_eq!(f.to_rnode(None).base_mtime_ms, 1_791_766_923_456);
    }

    #[test]
    fn error_reasons() {
        let e = ApiError::parse(
            403,
            br#"{"error":{"code":403,"message":"m","errors":[{"reason":"storageQuotaExceeded"}]}}"#,
        );
        assert_eq!(e.classify(None, Duration::from_secs(1)), BErr::StorageFull);
    }
}
