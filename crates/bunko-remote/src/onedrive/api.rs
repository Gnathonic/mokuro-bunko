//! Graph v1.0 wire types, resource-unit costs, name rules and error classification.

use std::time::Duration;

use serde::{Deserialize, Deserializer};

use crate::backend::{BErr, HashKind, RNode};

/// Largest `children` page asked for.
pub const PAGE_TOP: u32 = 200;

/// Resource-unit costs per request, from the SharePoint/OneDrive throttling page
/// ("how to avoid getting throttled"; prov-onedrive B, G). The governor meters
/// `OneDriveBackend::units_used()` against its budget.
pub mod units {
    /// Get an item (stat, root lookup, ancestry walk, `/me/drive`).
    pub const GET: u64 = 1;
    /// `delta` with a token (every cursor this driver holds has one).
    pub const DELTA_TOKEN: u64 = 1;
    /// `delta` without a token (a full enumeration; not used by this driver).
    pub const DELTA_NO_TOKEN: u64 = 2;
    /// `/content` (the 302 to the pre-authenticated URL; that GET itself is not Graph).
    pub const DOWNLOAD: u64 = 1;
    /// One `children` page.
    pub const LIST: u64 = 2;
    /// Create a folder, start a copy.
    pub const CREATE: u64 = 2;
    /// PATCH (rename, move).
    pub const UPDATE: u64 = 2;
    /// DELETE, permanentDelete.
    pub const DELETE: u64 = 2;
    /// A simple PUT, `createUploadSession`.
    pub const UPLOAD: u64 = 2;
    /// A fragment PUT or a status GET on an `uploadUrl`: unpublished, costed like a get.
    pub const FRAGMENT: u64 = 1;
}

/// The cost of a `delta` request at `url`.
pub fn delta_units(url: &str) -> u64 {
    let q = url.split_once('?').map_or("", |(_, q)| q);
    if q.split('&').any(|kv| kv.starts_with("token=")) {
        units::DELTA_TOKEN
    } else {
        units::DELTA_NO_TOKEN
    }
}

fn de_i64_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum N {
        I(i64),
        F(f64),
        S(String),
    }
    Ok(match Option::<N>::deserialize(d)? {
        None => None,
        Some(N::I(i)) => Some(i),
        Some(N::F(f)) => Some(f as i64),
        Some(N::S(s)) => s.parse().ok(),
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParentRef {
    pub id: Option<String>,
    pub drive_id: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hashes {
    pub quick_xor_hash: Option<String>,
    pub sha1_hash: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileFacet {
    pub mime_type: Option<String>,
    pub hashes: Option<Hashes>,
}

/// A `driveItem`, restricted to what the driver reads.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "eTag")]
    pub e_tag: Option<String>,
    #[serde(rename = "cTag")]
    pub c_tag: Option<String>,
    #[serde(default, deserialize_with = "de_i64_opt")]
    pub size: Option<i64>,
    pub created_date_time: Option<String>,
    /// The top-level (server) time; `fileSystemInfo` holds the client-settable one,
    /// which the reader does not list (spec §5.2).
    pub last_modified_date_time: Option<String>,
    pub parent_reference: Option<ParentRef>,
    pub file: Option<FileFacet>,
    pub folder: Option<serde_json::Value>,
    pub deleted: Option<serde_json::Value>,
    pub root: Option<serde_json::Value>,
}

impl Item {
    pub fn is_dir(&self) -> bool {
        self.folder.is_some() || self.root.is_some()
    }

    pub fn parent_id(&self) -> Option<String> {
        self.parent_reference.as_ref().and_then(|p| p.id.clone())
    }

    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or("")
    }

    /// `cTag` for files (content only); folders use `eTag`, because Business returns
    /// no folder cTag and a folder's cTag moves with any descendant.
    pub fn cver(&self) -> String {
        let etag = self.e_tag.clone().unwrap_or_default();
        if self.is_dir() {
            etag
        } else {
            self.c_tag.clone().unwrap_or(etag)
        }
    }

    /// QuickXor where Graph has it (always, per the docs), else SHA-1.
    pub fn hash(&self) -> Option<(HashKind, String)> {
        let h = self.file.as_ref()?.hashes.as_ref()?;
        if let Some(q) = h.quick_xor_hash.as_ref().filter(|q| !q.is_empty()) {
            return Some((HashKind::QuickXor, q.clone()));
        }
        h.sha1_hash
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|s| (HashKind::Sha1, s.to_ascii_lowercase()))
    }

    pub fn quickxor(&self) -> Option<&str> {
        self.file
            .as_ref()?
            .hashes
            .as_ref()?
            .quick_xor_hash
            .as_deref()
    }

    pub fn to_rnode(&self, parent: Option<String>) -> RNode {
        let is_dir = self.is_dir();
        RNode {
            pid: self.id.clone(),
            parent,
            name_raw: self.name().as_bytes().to_vec(),
            is_dir,
            size: if is_dir {
                0
            } else {
                self.size.unwrap_or(0).max(0) as u64
            },
            base_mtime_ms: self
                .last_modified_date_time
                .as_deref()
                .map_or(0, rfc3339_ms),
            created_ms: self.created_date_time.as_deref().map_or(0, rfc3339_ms),
            version: self.e_tag.clone().unwrap_or_default(),
            cver: self.cver(),
            pin: if is_dir { None } else { self.c_tag.clone() },
            hash: self.hash(),
            trashed: false,
        }
    }
}

/// Unix ms of an RFC 3339 timestamp; 0 when it does not parse.
pub fn rfc3339_ms(s: &str) -> i64 {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
        .unwrap_or(0)
}

/// A collection page (`children`, `delta`).
#[derive(Debug, Deserialize)]
pub struct Page {
    #[serde(default)]
    pub value: Vec<Item>,
    #[serde(rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

/// An upload session (create response and status).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadSession {
    pub upload_url: Option<String>,
    #[serde(default)]
    pub next_expected_ranges: Vec<String>,
}

impl UploadSession {
    /// The first byte the session still wants (`"12345-"` or `"12345-67890"`).
    pub fn next_offset(&self) -> Option<u64> {
        self.next_expected_ranges
            .iter()
            .filter_map(|r| r.split('-').next()?.trim().parse::<u64>().ok())
            .min()
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub id: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct IdentitySet {
    pub user: Option<Identity>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Quota {
    #[serde(default, deserialize_with = "de_i64_opt")]
    pub total: Option<i64>,
    #[serde(default, deserialize_with = "de_i64_opt")]
    pub used: Option<i64>,
    #[serde(default, deserialize_with = "de_i64_opt")]
    pub remaining: Option<i64>,
}

/// `/me/drive`.
#[derive(Debug, Default, Deserialize)]
pub struct Drive {
    pub id: Option<String>,
    pub owner: Option<IdentitySet>,
    pub quota: Option<Quota>,
}

#[derive(Debug, Default, Deserialize)]
pub struct MonitorError {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
}

/// A long-running action's monitor (copy).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Monitor {
    pub status: Option<String>,
    pub resource_id: Option<String>,
    pub error: Option<MonitorError>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InnerErr {
    #[serde(default)]
    code: String,
    inner_error: Option<Box<InnerErr>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorInner {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
    inner_error: Option<Box<InnerErr>>,
}

#[derive(Debug, Default, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: ErrorInner,
}

/// A parsed Graph error response.
#[derive(Debug, Clone, Default)]
pub struct ApiError {
    pub status: u16,
    /// The top-level code first, then the nested `innerError` codes.
    pub codes: Vec<String>,
    pub message: String,
}

impl ApiError {
    pub fn parse(status: u16, body: &[u8]) -> ApiError {
        let parsed: ErrorBody = serde_json::from_slice(body).unwrap_or_default();
        let mut codes = Vec::new();
        if !parsed.error.code.is_empty() {
            codes.push(parsed.error.code.clone());
        }
        let mut inner = parsed.error.inner_error;
        while let Some(e) = inner {
            if !e.code.is_empty() {
                codes.push(e.code.clone());
            }
            inner = e.inner_error;
        }
        let message = if parsed.error.message.is_empty() {
            String::from_utf8_lossy(&body[..body.len().min(200)]).into_owned()
        } else {
            parsed.error.message
        };
        ApiError {
            status,
            codes,
            message,
        }
    }

    pub fn has(&self, code: &str) -> bool {
        self.codes.iter().any(|c| c.eq_ignore_ascii_case(code))
    }

    /// A rate limit: 429 always, 503 when it says when to come back.
    pub fn is_throttle(&self, retry_after: Option<Duration>) -> bool {
        self.status == 429 || (self.status == 503 && retry_after.is_some())
    }

    /// Classify for the governor and the outbox. `retry_after` is the response's
    /// `Retry-After`; `backoff` the exponential suggestion when it has none.
    pub fn classify(&self, retry_after: Option<Duration>, backoff: Duration) -> BErr {
        let msg = || format!("onedrive {}: {}", self.status, self.message);
        match self.status {
            401 => BErr::AuthExpired,
            403 if self.has("accessDenied") => BErr::AuthRevoked,
            403 if self.has("quotaLimitReached") || self.has("insufficientStorage") => {
                BErr::StorageFull
            }
            404 | 410 => BErr::NotFound,
            409 => BErr::NameConflict,
            412 => BErr::PreconditionFailed,
            413 => BErr::TooLarge,
            423 => BErr::Transient(msg()),
            429 | 503 if self.is_throttle(retry_after) => {
                BErr::Throttled(Some(retry_after.unwrap_or(backoff)))
            }
            507 => BErr::StorageFull,
            408 | 500..=599 => BErr::Transient(msg()),
            _ => BErr::Fatal(msg()),
        }
    }
}

/// Characters OneDrive refuses in a name (all account types; Business adds more).
pub const FORBIDDEN_CHARS: &[char] = &['"', '*', ':', '<', '>', '?', '/', '\\', '|'];

/// A name OneDrive would refuse: empty, `.`/`..`, a forbidden character or a control
/// character, or a leading or trailing space. Refused before any request is made.
pub fn check_name(name: &str) -> Result<(), BErr> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.starts_with(' ')
        || name.ends_with(' ')
        || name
            .chars()
            .any(|c| FORBIDDEN_CHARS.contains(&c) || c.is_control());
    if bad { Err(BErr::InvalidName) } else { Ok(()) }
}

/// A URL path segment: everything but RFC 3986 unreserved bytes percent-encoded.
pub fn seg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// An entity tag with any `W/` prefix and surrounding quotes removed, for comparing a
/// response header with a `cTag` from JSON.
pub fn bare_tag(t: &str) -> &str {
    let t = t.trim();
    let t = t.strip_prefix("W/").unwrap_or(t);
    t.trim_matches('"')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for bad in [
            "", ".", "..", " a", "a ", "a\"b", "a*b", "a:b", "a<b", "a>b", "a?b", "a/b", "a\\b",
            "a|b", "a\nb",
        ] {
            assert_eq!(check_name(bad), Err(BErr::InvalidName), "{bad:?}");
        }
        for ok in ["a", "Vol 1.cbz", "日本語.mokuro", "a#b%c", ".hidden"] {
            assert_eq!(check_name(ok), Ok(()), "{ok:?}");
        }
        assert_eq!(seg("a b#%/é"), "a%20b%23%25%2F%C3%A9");
    }

    #[test]
    fn tags_and_ranges() {
        assert_eq!(bare_tag("W/\"c:{A},2\""), "c:{A},2");
        assert_eq!(bare_tag("\"c:{A},2\""), bare_tag("c:{A},2"));
        let s = UploadSession {
            upload_url: None,
            next_expected_ranges: vec!["700-".into(), "12-55".into()],
        };
        assert_eq!(s.next_offset(), Some(12));
        assert_eq!(
            delta_units("https://g/v1.0/me/drive/root/delta?token=abc"),
            1
        );
        assert_eq!(delta_units("https://g/v1.0/me/drive/root/delta"), 2);
    }

    #[test]
    fn item_mapping() {
        let it: Item = serde_json::from_str(
            r#"{"id":"F1","name":"a.cbz","eTag":"\"{F1},3\"","cTag":"\"c:{F1},2\"","size":12,
                "createdDateTime":"2026-10-12T01:02:03Z","lastModifiedDateTime":"2026-10-12T01:02:04.5Z",
                "fileSystemInfo":{"lastModifiedDateTime":"2001-01-01T00:00:00Z"},
                "parentReference":{"id":"P"},"file":{"hashes":{"quickXorHash":"AAAAAAAAAAAAAAAAAAAAAAAAAAA=","sha1Hash":"AB"}}}"#,
        )
        .unwrap();
        let n = it.to_rnode(it.parent_id());
        assert_eq!(n.parent.as_deref(), Some("P"));
        assert_eq!(n.cver, "\"c:{F1},2\"");
        assert_eq!(n.version, "\"{F1},3\"");
        assert_eq!(n.pin.as_deref(), Some("\"c:{F1},2\""));
        assert_eq!(n.base_mtime_ms, 1_791_766_924_500);
        assert_eq!(n.hash.unwrap().0, HashKind::QuickXor);
        let e = ApiError::parse(
            403,
            br#"{"error":{"code":"accessDenied","message":"m","innerError":{"code":"x"}}}"#,
        );
        assert_eq!(e.codes, ["accessDenied", "x"]);
        assert_eq!(e.classify(None, Duration::ZERO), BErr::AuthRevoked);
    }
}
