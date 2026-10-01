//! Response builders: WsgiDAV-style status pages and the DAV error bodies (spec §8.5).

use axum::body::Body;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderValue};
use http::{Response, StatusCode};

use crate::xml;

pub type Resp = Response<Body>;

/// A failed (or bodiless) outcome of a DAV method.
#[derive(Debug, Clone)]
pub struct DavError {
    pub status: StatusCode,
    pub context: String,
    /// A DAV precondition body (`no-conflicting-lock` with the conflicting hrefs).
    pub lock_conflict: Option<Vec<String>>,
}

impl DavError {
    pub fn new(status: StatusCode, context: impl Into<String>) -> Self {
        Self {
            status,
            context: context.into(),
            lock_conflict: None,
        }
    }

    pub fn status(status: StatusCode) -> Self {
        Self::new(status, "")
    }

    pub fn locked_by(hrefs: Vec<String>) -> Self {
        Self {
            status: StatusCode::LOCKED,
            context: String::new(),
            lock_conflict: Some(hrefs),
        }
    }

    pub fn into_response(self) -> Resp {
        if let Some(hrefs) = &self.lock_conflict {
            let mut body = String::from(xml::XML_DECLARATION);
            body.push_str("<D:error xmlns:D=\"DAV:\"><D:no-conflicting-lock>");
            for h in hrefs {
                body.push_str("<D:href>");
                body.push_str(&xml::escape_text(h));
                body.push_str("</D:href>");
            }
            body.push_str("</D:no-conflicting-lock></D:error>");
            return bytes_response(
                self.status,
                "application/xml; charset=utf-8",
                body.into_bytes(),
            );
        }
        status_page(self.status, &self.context)
    }
}

pub type DavResult<T> = Result<T, DavError>;

impl From<std::io::Error> for DavError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            DavError::new(StatusCode::FORBIDDEN, e.to_string())
        } else {
            DavError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        }
    }
}

/// WsgiDAV's status phrase table (others print `Status`).
pub fn status_text(status: StatusCode) -> String {
    let phrase = match status.as_u16() {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        207 => "Multi-Status",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        415 => "Media Type Not Supported",
        416 => "Range Not Satisfiable",
        423 => "Locked",
        424 => "Failed Dependency",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        507 => "Insufficient Storage",
        _ => "Status",
    };
    format!("{} {}", status.as_u16(), phrase)
}

/// A status answer: `204`/`304` bodiless with `Content-Length: 0`, everything else a small
/// HTML page (clients read only the status).
pub fn status_page(status: StatusCode, context: &str) -> Resp {
    if status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED {
        return empty(status);
    }
    let text = status_text(status);
    let detail = if context.is_empty() {
        text.clone()
    } else {
        format!("{text}: {}", xml::escape_text(context))
    };
    let body = format!(
        "<!DOCTYPE HTML PUBLIC \"-//W3C//DTD HTML 4.01//EN\" \"http://www.w3.org/TR/html4/strict.dtd\">\n<html><head>\n  <meta http-equiv=\"Content-Type\" content=\"text/html; charset=utf-8\">\n  <title>{text}</title>\n</head><body>\n  <h1>{text}</h1>\n  <p>{detail}</p>\n<hr/>\n<p>mokuro-bunko</p>\n</body></html>"
    );
    bytes_response(status, "text/html; charset=utf-8", body.into_bytes())
}

pub fn empty(status: StatusCode) -> Resp {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
    resp
}

pub fn bytes_response(status: StatusCode, content_type: &'static str, body: Vec<u8>) -> Resp {
    let len = body.len();
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = status;
    let headers = resp.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(CONTENT_LENGTH, HeaderValue::from(len));
    resp
}

pub fn set_header(resp: &mut Resp, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        resp.headers_mut().insert(name, v);
    }
}
