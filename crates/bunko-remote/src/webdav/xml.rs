//! PROPFIND bodies and an incremental multistatus parser.
//!
//! The parser reads the response body as it arrives and hands each `<D:response>` to a
//! callback when its end tag is seen, so a Depth: infinity listing of a large library
//! never sits in memory whole.

use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use tokio::io::AsyncBufRead;

use crate::backend::HashKind;

const DAV_NS: &[u8] = b"DAV:";
const OC_NS: &[u8] = b"http://owncloud.org/ns";

/// The properties every listing asks for.
pub const PROPFIND_NODE: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop>"#,
    "<d:resourcetype/><d:getcontentlength/><d:getlastmodified/><d:getetag/>",
    "<d:creationdate/><oc:checksums/>",
    "</d:prop></d:propfind>"
);

/// RFC 4331 quota properties.
pub const PROPFIND_QUOTA: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:"><d:prop>"#,
    "<d:quota-available-bytes/><d:quota-used-bytes/>",
    "</d:prop></d:propfind>"
);

/// One `<D:response>`, with only the properties from 2xx propstats.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DavEntry {
    /// The raw `<D:href>` text (entity-unescaped, still percent-encoded).
    pub href: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub last_modified: Option<String>,
    pub creation_date: Option<String>,
    pub etag: Option<String>,
    /// `oc:checksums` text, e.g. `SHA1:… MD5:… SHA256:…`.
    pub checksums: Option<String>,
    pub quota_available: Option<i64>,
    pub quota_used: Option<i64>,
    /// The response-level `<D:status>` was not 2xx (a member the server could not
    /// describe).
    pub failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Multistatus,
    Response,
    Href,
    Propstat,
    Prop,
    Status,
    ResourceType,
    Collection,
    ContentLength,
    LastModified,
    CreationDate,
    Etag,
    Checksums,
    Checksum,
    QuotaAvailable,
    QuotaUsed,
    Other,
}

fn tag(ns: &ResolveResult, local: &[u8]) -> Tag {
    let ns = match ns {
        ResolveResult::Bound(n) => n.as_ref(),
        _ => b"",
    };
    if ns == DAV_NS {
        match local {
            b"multistatus" => Tag::Multistatus,
            b"response" => Tag::Response,
            b"href" => Tag::Href,
            b"propstat" => Tag::Propstat,
            b"prop" => Tag::Prop,
            b"status" => Tag::Status,
            b"resourcetype" => Tag::ResourceType,
            b"collection" => Tag::Collection,
            b"getcontentlength" => Tag::ContentLength,
            b"getlastmodified" => Tag::LastModified,
            b"creationdate" => Tag::CreationDate,
            b"getetag" => Tag::Etag,
            b"quota-available-bytes" => Tag::QuotaAvailable,
            b"quota-used-bytes" => Tag::QuotaUsed,
            _ => Tag::Other,
        }
    } else if ns == OC_NS {
        match local {
            b"checksums" => Tag::Checksums,
            b"checksum" => Tag::Checksum,
            _ => Tag::Other,
        }
    } else {
        Tag::Other
    }
}

/// `HTTP/1.1 200 OK` -> is it 2xx?
fn status_ok(s: &str) -> bool {
    s.split_whitespace()
        .nth(1)
        .is_some_and(|code| code.starts_with('2'))
}

/// Properties of one propstat, merged into the entry only when its status is 2xx.
#[derive(Default)]
struct Props {
    is_dir: bool,
    size: Option<u64>,
    last_modified: Option<String>,
    creation_date: Option<String>,
    etag: Option<String>,
    checksums: Option<String>,
    quota_available: Option<i64>,
    quota_used: Option<i64>,
}

impl Props {
    fn merge_into(self, e: &mut DavEntry) {
        e.is_dir |= self.is_dir;
        e.size = self.size.or(e.size);
        e.last_modified = self.last_modified.or(e.last_modified.take());
        e.creation_date = self.creation_date.or(e.creation_date.take());
        e.etag = self.etag.or(e.etag.take());
        e.checksums = self.checksums.or(e.checksums.take());
        e.quota_available = self.quota_available.or(e.quota_available);
        e.quota_used = self.quota_used.or(e.quota_used);
    }
}

/// A malformed multistatus.
#[derive(Debug)]
pub struct ParseError(pub String);

/// Read a multistatus from `src`, calling `on_entry` for each `<D:response>` as soon as
/// it ends.
pub async fn parse_multistatus<R: AsyncBufRead + Unpin + Send>(
    src: R,
    on_entry: &mut (dyn FnMut(DavEntry) + Send),
) -> Result<(), ParseError> {
    let mut reader = NsReader::from_reader(src);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut stack: Vec<Tag> = Vec::new();
    let mut entry: Option<DavEntry> = None;
    let mut props: Option<Props> = None;
    let mut propstat_status: Option<String> = None;
    let mut text = String::new();
    let mut saw_root = false;
    loop {
        buf.clear();
        let (ns, ev) = reader
            .read_resolved_event_into_async(&mut buf)
            .await
            .map_err(|e| ParseError(e.to_string()))?;
        match ev {
            Event::Start(s) => {
                let t = tag(&ns, s.local_name().as_ref());
                text.clear();
                match t {
                    Tag::Multistatus if stack.is_empty() => saw_root = true,
                    Tag::Response if stack.last() == Some(&Tag::Multistatus) => {
                        entry = Some(DavEntry::default());
                    }
                    Tag::Propstat if entry.is_some() => {
                        props = Some(Props::default());
                        propstat_status = None;
                    }
                    _ => {}
                }
                stack.push(t);
            }
            Event::Empty(s) => {
                let t = tag(&ns, s.local_name().as_ref());
                if t == Tag::Collection
                    && stack.last() == Some(&Tag::ResourceType)
                    && let Some(p) = props.as_mut()
                {
                    p.is_dir = true;
                }
                if t == Tag::Multistatus && stack.is_empty() {
                    saw_root = true;
                }
            }
            Event::Text(t) => {
                let s = t.unescape().map_err(|e| ParseError(e.to_string()))?;
                text.push_str(&s);
            }
            Event::CData(c) => {
                text.push_str(&String::from_utf8_lossy(&c.into_inner()));
            }
            Event::End(_) => {
                let Some(t) = stack.pop() else {
                    return Err(ParseError("unbalanced end tag".into()));
                };
                let parent = stack.last().copied();
                let value = text.trim().to_string();
                text.clear();
                match t {
                    Tag::Href if parent == Some(Tag::Response) => {
                        if let Some(e) = entry.as_mut()
                            && e.href.is_empty()
                        {
                            e.href = value;
                        }
                    }
                    Tag::Status if parent == Some(Tag::Propstat) => propstat_status = Some(value),
                    Tag::Status if parent == Some(Tag::Response) => {
                        if let Some(e) = entry.as_mut()
                            && !status_ok(&value)
                        {
                            e.failed = true;
                        }
                    }
                    Tag::Collection if parent == Some(Tag::ResourceType) => {
                        if let Some(p) = props.as_mut() {
                            p.is_dir = true;
                        }
                    }
                    Tag::ContentLength
                    | Tag::LastModified
                    | Tag::CreationDate
                    | Tag::Etag
                    | Tag::QuotaAvailable
                    | Tag::QuotaUsed
                        if parent == Some(Tag::Prop) =>
                    {
                        if let Some(p) = props.as_mut()
                            && !value.is_empty()
                        {
                            match t {
                                Tag::ContentLength => p.size = value.parse().ok(),
                                Tag::LastModified => p.last_modified = Some(value),
                                Tag::CreationDate => p.creation_date = Some(value),
                                Tag::Etag => p.etag = Some(value),
                                Tag::QuotaAvailable => p.quota_available = value.parse().ok(),
                                Tag::QuotaUsed => p.quota_used = value.parse().ok(),
                                _ => {}
                            }
                        }
                    }
                    Tag::Checksum if parent == Some(Tag::Checksums) => {
                        if let Some(p) = props.as_mut()
                            && !value.is_empty()
                        {
                            let all = p.checksums.get_or_insert_with(String::new);
                            if !all.is_empty() {
                                all.push(' ');
                            }
                            all.push_str(&value);
                        }
                    }
                    Tag::Propstat => {
                        let ok = propstat_status.as_deref().is_none_or(status_ok);
                        if let (Some(p), Some(e)) = (props.take(), entry.as_mut())
                            && ok
                        {
                            p.merge_into(e);
                        }
                    }
                    Tag::Response if parent == Some(Tag::Multistatus) => {
                        if let Some(e) = entry.take()
                            && !e.href.is_empty()
                        {
                            on_entry(e);
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !saw_root {
        return Err(ParseError("not a DAV:multistatus".into()));
    }
    Ok(())
}

/// The best hash in an `oc:checksums` value (`SHA256:…` over `SHA1:…` over `MD5:…`).
pub fn best_checksum(s: &str) -> Option<(HashKind, String)> {
    let mut best: Option<(u8, HashKind, String)> = None;
    for item in s.split_whitespace() {
        let Some((algo, hex)) = item.split_once(':') else {
            continue;
        };
        let (rank, kind) = match algo.to_ascii_uppercase().as_str() {
            "SHA256" => (3, HashKind::Sha256),
            "SHA1" => (2, HashKind::NextcloudSha1),
            "MD5" => (1, HashKind::Md5),
            _ => continue,
        };
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        if best.as_ref().is_none_or(|(r, _, _)| rank > *r) {
            best = Some((rank, kind, hex.to_ascii_lowercase()));
        }
    }
    best.map(|(_, k, h)| (k, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(xml: &str) -> Vec<DavEntry> {
        let mut out = Vec::new();
        parse_multistatus(xml.as_bytes(), &mut |e| out.push(e))
            .await
            .unwrap();
        out
    }

    #[tokio::test]
    async fn parses_bunko_style_and_nextcloud_style() {
        let xml = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:multistatus xmlns:D="DAV:"><D:response><D:href>/mokuro-reader/</D:href><D:propstat><D:prop><D:resourcetype><D:collection /></D:resourcetype><D:getlastmodified>Thu, 01 Oct 2026 18:10:08 GMT</D:getlastmodified><D:getetag>1790878208.881310</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>/mokuro-reader/a%20b.cbz</D:href><D:propstat><D:prop><D:resourcetype /><D:getcontentlength>8</D:getcontentlength><D:getetag>&quot;x-8&quot;</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat><D:propstat><D:prop><D:creationdate /></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat></D:response>
</D:multistatus>"#;
        let v = parse(xml).await;
        assert_eq!(v.len(), 2);
        assert!(v[0].is_dir);
        assert_eq!(v[0].etag.as_deref(), Some("1790878208.881310"));
        assert!(!v[1].is_dir);
        assert_eq!(v[1].size, Some(8));
        assert_eq!(v[1].etag.as_deref(), Some("\"x-8\""));
        assert_eq!(v[1].creation_date, None);

        let nc = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:response><d:href>/remote.php/dav/files/u/x.cbz</d:href><d:propstat><d:prop><d:resourcetype/><d:getetag>"abc"</d:getetag><oc:checksums><oc:checksum>SHA1:AA MD5:bb</oc:checksum></oc:checksums><d:quota-available-bytes>-3</d:quota-available-bytes></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
        let v = parse(nc).await;
        assert_eq!(v[0].checksums.as_deref(), Some("SHA1:AA MD5:bb"));
        assert_eq!(v[0].quota_available, Some(-3));
        assert_eq!(
            best_checksum(v[0].checksums.as_deref().unwrap()),
            Some((HashKind::NextcloudSha1, "aa".into()))
        );
    }

    #[tokio::test]
    async fn rejects_non_multistatus() {
        let mut n = 0;
        assert!(
            parse_multistatus(&b"<html></html>"[..], &mut |_| n += 1)
                .await
                .is_err()
        );
    }
}
