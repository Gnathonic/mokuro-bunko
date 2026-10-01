//! Conditional requests (`If-Match`, `If-None-Match`, `If-Modified-Since`,
//! `If-Unmodified-Since`, DAV `If:`) and `Range` (spec §8.3.3, §8.4).

use http::{HeaderMap, Method, StatusCode};

use crate::paths;
use crate::response::DavError;

/// WsgiDAV `parse_if_match_header`: comma-split, `W/` and quotes stripped (weak tags
/// compare equal to the strong ones).
fn parse_etag_list(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in value.split(',') {
        let mut tag = raw.trim();
        tag = tag.strip_prefix("W/").unwrap_or(tag);
        let unquoted = if tag.len() >= 2 && tag.starts_with('"') && tag.ends_with('"') {
            &tag[1..tag.len() - 1]
        } else {
            tag
        };
        if !unquoted.is_empty() {
            out.push(unquoted.to_string());
        }
    }
    out
}

pub fn parse_http_date(value: &str) -> Option<i64> {
    let t = httpdate::parse_http_date(value.trim()).ok()?;
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The HTTP conditionals against an existing resource (WsgiDAV
/// `evaluate_http_conditionals`, in RFC 9110 §13.2.2 precedence), with the
/// `If-Modified-Since` equal-date bug FIXED: a date equal to the resource's second is
/// "not modified" (spec 14.5). `If-None-Match`, when present, decides alone (a browser
/// revalidating sends both, and WsgiDAV's mixed rule would turn that into a 412 once the
/// date comparison is fixed). `If-Modified-Since` applies only to GET/HEAD.
///
/// `etag` is the resource's unquoted tag (`None` for virtual folders: nothing matches it
/// except `*`); `last_modified` its whole second.
pub fn evaluate_http(
    headers: &HeaderMap,
    method: &Method,
    etag: Option<&str>,
    last_modified: i64,
) -> Result<(), DavError> {
    let etag = etag.unwrap_or("[]");
    let read = method == Method::GET || method == Method::HEAD;
    if let Some(v) = header(headers, "if-match")
        && !parse_etag_list(v).iter().any(|t| t == etag || t == "*")
    {
        return Err(DavError::new(
            StatusCode::PRECONDITION_FAILED,
            "If-Match header condition failed",
        ));
    }
    if let Some(v) = header(headers, "if-unmodified-since")
        && let Some(since) = parse_http_date(v)
        && since < last_modified
    {
        return Err(DavError::new(
            StatusCode::PRECONDITION_FAILED,
            "If-Unmodified-Since header condition failed",
        ));
    }
    if let Some(v) = header(headers, "if-none-match") {
        if parse_etag_list(v).iter().any(|t| t == etag || t == "*") {
            if read {
                return Err(DavError::status(StatusCode::NOT_MODIFIED));
            }
            return Err(DavError::new(
                StatusCode::PRECONDITION_FAILED,
                "If-None-Match header condition failed",
            ));
        }
        return Ok(());
    }
    if read
        && let Some(v) = header(headers, "if-modified-since")
        && let Some(since) = parse_http_date(v)
        && since >= last_modified
    {
        return Err(DavError::status(StatusCode::NOT_MODIFIED));
    }
    Ok(())
}

/// One condition of a DAV `If:` list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cond {
    Token { not: bool, token: String },
    Etag { not: bool, etag: String },
}

/// Parsed DAV `If:` header: `(resource tag or "*", lists)` (WsgiDAV `parse_if_header_dict`).
#[derive(Debug, Clone, Default)]
pub struct IfHeader {
    entries: Vec<(String, Vec<Vec<Cond>>)>,
    /// Every lock token named anywhere (WsgiDAV `ifLockTokenList`).
    pub tokens: Vec<String>,
}

impl IfHeader {
    pub fn parse(headers: &HeaderMap) -> Option<IfHeader> {
        let raw = header(headers, "if")?.trim().to_string();
        let text = if raw.starts_with('<') {
            raw
        } else {
            format!("<*>{raw}")
        };
        let mut out = IfHeader::default();
        let mut resource = "*".to_string();
        let mut chars = text.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            match c {
                '<' => {
                    let rest = &text[i + 1..];
                    if let Some(end) = rest.find('>') {
                        resource = normalize_tag(&rest[..end]);
                        for _ in 0..end + 1 {
                            chars.next();
                        }
                    }
                }
                '(' => {
                    let rest = &text[i + 1..];
                    let Some(end) = rest.find(')') else { break };
                    let content = &rest[..end];
                    for _ in 0..end + 1 {
                        chars.next();
                    }
                    let mut list = Vec::new();
                    let mut not = false;
                    for item in content.split_whitespace() {
                        if item.eq_ignore_ascii_case("not") {
                            not = true;
                            continue;
                        }
                        if item.starts_with('[') {
                            let etag = item
                                .trim_matches(|c| c == '"' || c == '[' || c == ']')
                                .trim_start_matches("W/")
                                .trim_matches('"');
                            list.push(Cond::Etag {
                                not,
                                etag: etag.to_string(),
                            });
                        } else {
                            let token = item.trim_matches(|c| c == '<' || c == '>').to_string();
                            out.tokens.push(token.clone());
                            list.push(Cond::Token { not, token });
                        }
                        not = false;
                    }
                    match out.entries.iter_mut().find(|(r, _)| *r == resource) {
                        Some((_, lists)) => lists.push(list),
                        None => out.entries.push((resource.clone(), vec![list])),
                    }
                }
                _ => {}
            }
        }
        Some(out)
    }

    /// WsgiDAV `test_if_header_dict`: does the condition hold for the resource at
    /// `path` (normalised) with `etag`, given the lock tokens currently protecting it?
    pub fn test(&self, path: &str, etag: Option<&str>, lock_tokens: &[String]) -> bool {
        let key = paths::normalize(path);
        let lists = match self
            .entries
            .iter()
            .find(|(r, _)| *r == key)
            .or_else(|| self.entries.iter().find(|(r, _)| r == "*"))
        {
            Some((_, lists)) => lists,
            None => return true,
        };
        lists.iter().any(|conds| {
            conds.iter().all(|c| match c {
                Cond::Etag { not, etag: want } => {
                    let hit = etag.is_some_and(|e| e == want);
                    hit != *not
                }
                Cond::Token { not, token } => {
                    let hit = lock_tokens.iter().any(|t| t == token);
                    hit != *not
                }
            })
        })
    }
}

/// A tagged-list resource (`<http://host/path>` or `</path>`) as a normalised path.
fn normalize_tag(tag: &str) -> String {
    if tag == "*" {
        return "*".to_string();
    }
    let path = match tag.find("://") {
        Some(i) => match tag[i + 3..].find('/') {
            Some(j) => &tag[i + 3 + j..],
            None => "/",
        },
        None => tag,
    };
    paths::normalize(&paths::decode_request_path(path).unwrap_or_else(|| path.to_string()))
}

/// What to send of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeOutcome {
    Full,
    /// Inclusive byte range.
    Partial {
        start: u64,
        end: u64,
    },
}

/// `Range` for a file of `size` bytes. One range is served: overlapping/adjacent ranges
/// merge, and several disjoint ones are coalesced into the one range spanning them all
/// (a superset, which RFC 9110 allows), instead of 0.5.2's "the highest range only".
/// A unit other than `bytes` is ignored (full answer); no satisfiable range -> 416.
pub fn parse_range(value: &str, size: u64) -> Result<RangeOutcome, DavError> {
    let Some((unit, specs)) = value.split_once('=') else {
        return Ok(RangeOutcome::Full);
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return Ok(RangeOutcome::Full);
    }
    let unsatisfiable = || {
        let mut e = DavError::new(StatusCode::RANGE_NOT_SATISFIABLE, "No valid ranges present");
        e.context = format!("bytes */{size}");
        e
    };
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for spec in specs.split(',') {
        let spec = spec.trim();
        let Some((a, b)) = spec.split_once('-') else {
            continue;
        };
        let (a, b) = (a.trim(), b.trim());
        if a.is_empty() {
            let Ok(n) = b.parse::<u64>() else { continue };
            if n == 0 || size == 0 {
                continue;
            }
            ranges.push((size.saturating_sub(n), size - 1));
        } else {
            let Ok(start) = a.parse::<u64>() else {
                continue;
            };
            if start >= size {
                continue;
            }
            let end = if b.is_empty() {
                size - 1
            } else {
                match b.parse::<u64>() {
                    Ok(e) if e >= start => e.min(size - 1),
                    _ => continue,
                }
            };
            ranges.push((start, end));
        }
    }
    if ranges.is_empty() {
        return Err(unsatisfiable());
    }
    let start = ranges.iter().map(|r| r.0).min().unwrap_or(0);
    let end = ranges.iter().map(|r| r.1).max().unwrap_or(0);
    Ok(RangeOutcome::Partial { start, end })
}

/// Should `If-Range` let the `Range` through? (date: equal to the whole second of the
/// resource; else an entity tag equal to the current one.)
pub fn if_range_holds(value: &str, etag: &str, last_modified: i64) -> bool {
    match parse_http_date(value) {
        Some(secs) => secs == last_modified,
        None => value.trim_matches(|c| c == '"' || c == ' ') == etag,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn if_none_match_and_modified_since() {
        let lm = 1790878208;
        let date = "Thu, 01 Oct 2026 18:10:08 GMT";
        let get = Method::GET;
        assert_eq!(
            evaluate_http(&headers(&[("if-none-match", "\"x\"")]), &get, Some("x"), lm)
                .unwrap_err()
                .status,
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(
            evaluate_http(
                &headers(&[("if-none-match", "W/\"x\"")]),
                &get,
                Some("x"),
                lm
            )
            .unwrap_err()
            .status,
            StatusCode::NOT_MODIFIED
        );
        assert!(
            evaluate_http(&headers(&[("if-none-match", "\"y\"")]), &get, Some("x"), lm).is_ok()
        );
        assert_eq!(
            evaluate_http(
                &headers(&[("if-none-match", "*")]),
                &Method::PUT,
                Some("x"),
                lm
            )
            .unwrap_err()
            .status,
            StatusCode::PRECONDITION_FAILED
        );
        // Equal date: 304 (0.5.2 answered 200).
        assert_eq!(
            evaluate_http(
                &headers(&[("if-modified-since", date)]),
                &get,
                Some("x"),
                lm
            )
            .unwrap_err()
            .status,
            StatusCode::NOT_MODIFIED
        );
        assert!(
            evaluate_http(
                &headers(&[("if-modified-since", date)]),
                &get,
                Some("x"),
                lm + 1
            )
            .is_ok()
        );
        assert_eq!(
            evaluate_http(&headers(&[("if-match", "\"y\"")]), &get, Some("x"), lm)
                .unwrap_err()
                .status,
            StatusCode::PRECONDITION_FAILED
        );
        assert!(evaluate_http(&headers(&[("if-match", "*")]), &get, None, lm).is_ok());
        assert_eq!(
            evaluate_http(
                &headers(&[("if-unmodified-since", date)]),
                &get,
                Some("x"),
                lm + 1
            )
            .unwrap_err()
            .status,
            StatusCode::PRECONDITION_FAILED
        );
    }

    #[test]
    fn ranges() {
        assert_eq!(
            parse_range("bytes=0-0", 10).unwrap(),
            RangeOutcome::Partial { start: 0, end: 0 }
        );
        assert_eq!(
            parse_range("bytes=5-", 10).unwrap(),
            RangeOutcome::Partial { start: 5, end: 9 }
        );
        assert_eq!(
            parse_range("bytes=-3", 10).unwrap(),
            RangeOutcome::Partial { start: 7, end: 9 }
        );
        assert_eq!(
            parse_range("bytes=-30", 10).unwrap(),
            RangeOutcome::Partial { start: 0, end: 9 }
        );
        assert_eq!(
            parse_range("bytes=2-100", 10).unwrap(),
            RangeOutcome::Partial { start: 2, end: 9 }
        );
        assert_eq!(
            parse_range("bytes=0-0,5-9", 10).unwrap(),
            RangeOutcome::Partial { start: 0, end: 9 }
        );
        assert_eq!(parse_range("items=0-1", 10).unwrap(), RangeOutcome::Full);
        assert_eq!(
            parse_range("bytes=10-", 10).unwrap_err().status,
            StatusCode::RANGE_NOT_SATISFIABLE
        );
        assert_eq!(
            parse_range("bytes=junk", 10).unwrap_err().status,
            StatusCode::RANGE_NOT_SATISFIABLE
        );
    }

    #[test]
    fn if_header() {
        let h = headers(&[("if", "(<opaquelocktoken:abc>)")]);
        let parsed = IfHeader::parse(&h).unwrap();
        assert_eq!(parsed.tokens, vec!["opaquelocktoken:abc".to_string()]);
        assert!(parsed.test("/x", None, &["opaquelocktoken:abc".to_string()]));
        assert!(!parsed.test("/x", None, &[]));
        let h = headers(&[(
            "if",
            "<http://h/mokuro-reader/a%20b> (Not <opaquelocktoken:zz> [\"1-2\"])",
        )]);
        let parsed = IfHeader::parse(&h).unwrap();
        assert!(parsed.test("/mokuro-reader/a b", Some("1-2"), &[]));
        assert!(!parsed.test("/mokuro-reader/a b", Some("1-3"), &[]));
        assert!(parsed.test("/other", Some("1-3"), &[]));
    }
}
