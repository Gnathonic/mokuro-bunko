//! `PUT /mokuro-reader/<Series>/series.json` (0.5.2 `metadata/middleware.py`
//! `MetadataAPI`, spec metadata-catalog §6): a client's fact update is a REQUEST to the
//! compiler, never a raw write. The WebDAV fallback calls [`series_put`] after
//! `auth::authorize` allowed the PUT.

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use bunko_db::{AuditDetails, NewAuditEvent};
use bunko_library::UpdateError;
use bunko_library::paths::series_title_from_series_file_path;
use bunko_library::service::BUSY_RETRY_AFTER_SECONDS;
use bunko_library::update::MAX_UPDATE_BODY_BYTES;
use http::{Method, StatusCode, header};

use super::LibraryDeps;
use super::util::text_with;
use crate::core::RequestCtx;

const TEXT: &str = "text/plain; charset=utf-8";

fn text(code: u16, message: &str) -> Response {
    text_with(code, message, TEXT, &[])
}

/// Whether the DAV fallback must hand this request to [`series_put`]: a `PUT` whose
/// percent-decoded path is exactly `/mokuro-reader/<Series>/series.json` (lexically
/// normalised, file name case-insensitive).
pub fn is_series_put(method: &Method, decoded_path: &str) -> bool {
    *method == Method::PUT && series_title_from_series_file_path(decoded_path).is_some()
}

/// `Content-Length` as Python's `int()` reads it: surrounding whitespace, a sign and
/// `_` digit separators allowed.
fn parse_content_length(raw: &str) -> Option<i64> {
    let text = raw.trim();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return None;
    }
    let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
    if !cleaned.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let value = cleaned.parse::<i64>().unwrap_or(i64::MAX);
    Some(if negative { -value } else { value })
}

/// The handler. Order of checks and answers is 0.5.2's: 401 without a user, 411
/// without `Content-Length` (body not read), 400 for a bad length, 413 above 4 MiB
/// (body not read), 503 + `Retry-After: 30` when a compile holds the lock, then the
/// audit event, then 400 (refused) or 204 (accepted; also when nothing changed).
pub async fn series_put(deps: &LibraryDeps, req: Request, ctx: RequestCtx) -> Response {
    let Some(username) = ctx
        .identity
        .username()
        .filter(|u| !u.is_empty())
        .map(str::to_owned)
    else {
        return text(401, "Authentication required");
    };
    let Some(raw_length) = req.headers().get(header::CONTENT_LENGTH) else {
        return text(411, "Content-Length required");
    };
    let Some(length) = raw_length.to_str().ok().and_then(parse_content_length) else {
        return text(400, "Invalid Content-Length");
    };
    if length < 0 {
        return text(400, "Invalid Content-Length");
    }
    if length as u64 > MAX_UPDATE_BODY_BYTES {
        return text(413, "Metadata update too large");
    }
    let path = percent_encoding::percent_decode_str(req.uri().path())
        .decode_utf8_lossy()
        .into_owned();
    let Some(series_title) = series_title_from_series_file_path(&path) else {
        return text(400, "Invalid metadata path");
    };
    let body = match axum::body::to_bytes(req.into_body(), length as usize).await {
        Ok(bytes) => bytes.to_vec(),
        // Shorter or longer than declared: the body is not the update the client meant.
        Err(_) => return text(400, "Invalid metadata update"),
    };

    let accepted = match deps
        .runtime
        .on_series_put(&series_title, body, Some(&username))
        .await
    {
        Ok(accepted) => accepted,
        Err(UpdateError::Busy(_)) => {
            return text_with(
                503,
                "Server is busy compiling metadata; retry shortly",
                TEXT,
                &[("retry-after", retry_after())],
            );
        }
        Err(UpdateError::Store(error)) => {
            tracing::error!(%error, "[METADATA] storing a series update failed");
            return text(500, "Internal Server Error");
        }
    };

    let db = deps.db.clone();
    let audit_path = path.clone();
    let audit_user = username.clone();
    let audited = tokio::task::spawn_blocking(move || {
        db.log_audit_event(
            &NewAuditEvent::new(if accepted {
                "metadata_update"
            } else {
                "metadata_rejected"
            })
            .actor(Some(&audit_user))
            .target_type("library")
            .target_path(&audit_path)
            .details(AuditDetails::new().with("accepted", accepted)),
        )
    })
    .await;
    match audited {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => tracing::warn!("[METADATA] audit failed: {error}"),
        Err(error) => tracing::warn!("[METADATA] audit failed: {error}"),
    }

    if !accepted {
        return text(400, "Invalid metadata update");
    }
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::NO_CONTENT;
    resp
}

fn retry_after() -> &'static str {
    // BUSY_RETRY_AFTER_SECONDS is 30; a static header value avoids an allocation.
    const _: () = assert!(BUSY_RETRY_AFTER_SECONDS == 30);
    "30"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_length_like_python_int() {
        assert_eq!(parse_content_length("12"), Some(12));
        assert_eq!(parse_content_length(" +1_000 "), Some(1000));
        assert_eq!(parse_content_length("-1"), Some(-1));
        assert_eq!(parse_content_length("not a number"), None);
        assert_eq!(parse_content_length("1__0"), None);
        assert_eq!(parse_content_length(""), None);
    }

    #[test]
    fn interception() {
        assert!(is_series_put(
            &Method::PUT,
            "/mokuro-reader/Dr Stone/series.json"
        ));
        assert!(is_series_put(
            &Method::PUT,
            "/mokuro-reader//Dr Stone/./SERIES.json"
        ));
        assert!(!is_series_put(
            &Method::GET,
            "/mokuro-reader/Dr Stone/series.json"
        ));
        assert!(!is_series_put(
            &Method::PUT,
            "/mokuro-reader/A/B/series.json"
        ));
        assert!(!is_series_put(&Method::PUT, "/mokuro-reader/catalog.json"));
    }
}
