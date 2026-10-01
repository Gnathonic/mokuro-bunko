//! A tiny namespace-aware XML tree for DAV request bodies, and the escaping helpers the
//! response writers use.
//!
//! Request bodies (PROPFIND, PROPPATCH, LOCK) are small; they are read whole (bounded by
//! the caller) and parsed into [`Element`]s. quick-xml does not expand DTD entities, so a
//! hostile body cannot blow up (0.5.2 used `defusedxml`).

use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

pub const DAV_NS: &str = "DAV:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    /// Namespace URI (`None` when unbound).
    pub ns: Option<String>,
    pub local: String,
    pub children: Vec<Element>,
    pub text: String,
}

impl Element {
    pub fn is(&self, ns: &str, local: &str) -> bool {
        self.ns.as_deref() == Some(ns) && self.local == local
    }

    pub fn is_dav(&self, local: &str) -> bool {
        self.is(DAV_NS, local)
    }

    /// Clark notation `{ns}local` (WsgiDAV's property keys).
    pub fn clark(&self) -> String {
        format!("{{{}}}{}", self.ns.as_deref().unwrap_or(""), self.local)
    }

    /// Serialise with explicit default-namespace declarations (prefix-free and therefore
    /// valid wherever it is embedded).
    pub fn to_xml(&self, out: &mut String) {
        let ns = self.ns.as_deref().unwrap_or("");
        out.push('<');
        out.push_str(&self.local);
        out.push_str(" xmlns=\"");
        out.push_str(&escape_attr(ns));
        out.push('"');
        if self.children.is_empty() && self.text.is_empty() {
            out.push_str("/>");
            return;
        }
        out.push('>');
        out.push_str(&escape_text(&self.text));
        for child in &self.children {
            child.to_xml(out);
        }
        out.push_str("</");
        out.push_str(&self.local);
        out.push('>');
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct XmlError;

/// Parse a whole document into its root element.
pub fn parse(body: &[u8]) -> Result<Element, XmlError> {
    let mut reader = NsReader::from_reader(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    loop {
        let (resolved, event) = reader
            .read_resolved_event_into(&mut buf)
            .map_err(|_| XmlError)?;
        match event {
            Event::Start(start) => {
                let el = element_of(&resolved, start.local_name().as_ref())?;
                stack.push(el);
            }
            Event::Empty(start) => {
                let el = element_of(&resolved, start.local_name().as_ref())?;
                attach(&mut stack, &mut root, el)?;
            }
            Event::End(_) => {
                let el = stack.pop().ok_or(XmlError)?;
                attach(&mut stack, &mut root, el)?;
            }
            Event::Text(text) => {
                let t = text.unescape().map_err(|_| XmlError)?;
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t);
                } else if !t.trim().is_empty() {
                    return Err(XmlError);
                }
            }
            Event::CData(data) => {
                let t = std::str::from_utf8(&data)
                    .map_err(|_| XmlError)?
                    .to_string();
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t);
                }
            }
            Event::Eof => break,
            // Declarations, comments, processing instructions, doctype: ignored.
            _ => {}
        }
        buf.clear();
    }
    if !stack.is_empty() {
        return Err(XmlError);
    }
    root.ok_or(XmlError)
}

fn element_of(resolved: &ResolveResult<'_>, local: &[u8]) -> Result<Element, XmlError> {
    let ns = match resolved {
        ResolveResult::Bound(ns) => Some(
            std::str::from_utf8(ns.as_ref())
                .map_err(|_| XmlError)?
                .to_string(),
        ),
        ResolveResult::Unbound => None,
        ResolveResult::Unknown(_) => return Err(XmlError),
    };
    let local = std::str::from_utf8(local)
        .map_err(|_| XmlError)?
        .to_string();
    Ok(Element {
        ns,
        local,
        children: Vec::new(),
        text: String::new(),
    })
}

fn attach(stack: &mut [Element], root: &mut Option<Element>, el: Element) -> Result<(), XmlError> {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(el);
        Ok(())
    } else if root.is_none() {
        *root = Some(el);
        Ok(())
    } else {
        Err(XmlError)
    }
}

pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

pub fn escape_attr(s: &str) -> String {
    escape_text(s).replace('"', "&quot;")
}

/// The opening of every multistatus document, byte for byte as WsgiDAV 4.3.5 sends it.
pub const XML_DECLARATION: &str = "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\n";
pub const MULTISTATUS_OPEN: &str =
    "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\n<D:multistatus xmlns:D=\"DAV:\">";
pub const MULTISTATUS_CLOSE: &str = "</D:multistatus>";

/// One property as it appears inside `<D:prop>`: either a `DAV:` element written with the
/// `D:` prefix, or a foreign one with its own namespace declaration.
pub fn prop_open_tag(ns: Option<&str>, local: &str, empty: bool) -> String {
    let end = if empty { " />" } else { ">" };
    match ns {
        Some(DAV_NS) => format!("<D:{local}{end}"),
        Some(ns) => format!("<ns0:{local} xmlns:ns0=\"{}\"{end}", escape_attr(ns)),
        None => format!("<{local}{end}"),
    }
}

pub fn prop_close_tag(ns: Option<&str>, local: &str) -> String {
    match ns {
        Some(DAV_NS) => format!("</D:{local}>"),
        Some(_) => format!("</ns0:{local}>"),
        None => format!("</{local}>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_namespaces() {
        let doc = br#"<?xml version="1.0"?><D:propfind xmlns:D="DAV:"><D:prop><D:getetag/><x:foo xmlns:x="urn:x">a &amp; b</x:foo></D:prop></D:propfind>"#;
        let root = parse(doc).unwrap();
        assert!(root.is_dav("propfind"));
        let prop = &root.children[0];
        assert!(prop.is_dav("prop"));
        assert!(prop.children[0].is_dav("getetag"));
        assert_eq!(prop.children[1].clark(), "{urn:x}foo");
        assert_eq!(prop.children[1].text, "a & b");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(b"not xml").is_err());
        assert!(parse(b"<a><b></a>").is_err());
        assert!(parse(b"<a/><b/>").is_err());
        assert!(parse(b"<p:a/>").is_err());
    }

    #[test]
    fn entities_are_not_expanded() {
        let doc = br#"<!DOCTYPE a [<!ENTITY x "boom">]><a>&x;</a>"#;
        assert!(parse(doc).is_err());
    }
}
