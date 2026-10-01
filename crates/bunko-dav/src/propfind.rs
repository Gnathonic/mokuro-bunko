//! PROPFIND: property values and the multistatus writer (spec §8.2, §8.3.2).

use std::path::PathBuf;

use parking_lot::Mutex;
use std::collections::HashMap;

use crate::locks::LockManager;
use crate::resource::{FileKind, Resource, Roots, content_type, http_date, now_secs, rfc3339};
use crate::xml::{self, DAV_NS, Element};

/// What a PROPFIND asks for.
#[derive(Debug, Clone)]
pub enum Mode {
    AllProp,
    /// RFC `<propname/>` (and WsgiDAV's `<name/>`): names only.
    PropName,
    Named(Vec<Element>),
}

/// Parse a PROPFIND body (WsgiDAV rules): `None` = 400.
pub fn parse_mode(body: &[u8]) -> Option<Mode> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Some(Mode::AllProp);
    }
    let root = xml::parse(body).ok()?;
    if !root.is_dav("propfind") {
        return None;
    }
    let mut mode: Option<Mode> = None;
    for child in &root.children {
        if child.is_dav("allprop") {
            if mode.is_some() {
                return None;
            }
            mode = Some(Mode::AllProp);
        } else if child.is_dav("propname") || child.is_dav("name") {
            if mode.is_some() {
                return None;
            }
            mode = Some(Mode::PropName);
        } else if child.is_dav("prop") {
            match &mut mode {
                None => mode = Some(Mode::Named(child.children.clone())),
                Some(Mode::Named(list)) => list.extend(child.children.iter().cloned()),
                Some(_) => return None,
            }
        }
        // <include> and unknown elements: ignored.
    }
    // A propfind with no recognised child: WsgiDAV answers "named" with an empty list.
    Some(mode.unwrap_or(Mode::Named(Vec::new())))
}

const FILE_PROPS: [&str; 7] = [
    "resourcetype",
    "creationdate",
    "getcontentlength",
    "getcontenttype",
    "getlastmodified",
    "displayname",
    "getetag",
];
const FOLDER_PROPS: [&str; 5] = [
    "resourcetype",
    "creationdate",
    "getlastmodified",
    "displayname",
    "getetag",
];

/// In-memory dead properties (WsgiDAV `PropertyManager`), keyed by lock key. Lost on
/// restart, as in 0.5.2. Bounded.
#[derive(Debug, Default)]
pub struct DeadProps {
    map: Mutex<HashMap<String, Vec<Element>>>,
}

pub const MAX_DEAD_RESOURCES: usize = 10_000;
pub const MAX_DEAD_PER_RESOURCE: usize = 64;

impl DeadProps {
    pub fn get(&self, key: &str, ns: Option<&str>, local: &str) -> Option<Element> {
        self.map
            .lock()
            .get(key)?
            .iter()
            .find(|e| e.ns.as_deref() == ns && e.local == local)
            .cloned()
    }

    /// Set (`Some`) or remove (`None`) one property. `false` when the store is full.
    pub fn set(&self, key: &str, ns: Option<&str>, local: &str, value: Option<Element>) -> bool {
        let mut map = self.map.lock();
        if value.is_some() && !map.contains_key(key) && map.len() >= MAX_DEAD_RESOURCES {
            return false;
        }
        let list = map.entry(key.to_string()).or_default();
        list.retain(|e| !(e.ns.as_deref() == ns && e.local == local));
        if let Some(v) = value {
            if list.len() >= MAX_DEAD_PER_RESOURCE {
                return false;
            }
            list.push(v);
        }
        if list.is_empty() {
            map.remove(key);
        }
        true
    }

    pub fn remove_tree(&self, key: &str) {
        let prefix = format!("{}/", key.trim_end_matches('/'));
        self.map
            .lock()
            .retain(|k, _| k != key && !k.starts_with(&prefix));
    }

    pub fn can_hold(&self, key: &str) -> bool {
        let map = self.map.lock();
        map.contains_key(key) || map.len() < MAX_DEAD_RESOURCES
    }
}

/// The lock / dead-property key of a resource: its normalised path, with the username for
/// a per-user file (so two users' copies at the same URL are distinct).
pub fn resource_key(res: &Resource, username: Option<&str>) -> String {
    let path = crate::paths::normalize(res.path());
    match res {
        Resource::File {
            kind: FileKind::Progress,
            ..
        } => format!("{path}\0{}", username.unwrap_or("")),
        _ => path,
    }
}

/// Everything property values may consult besides the resource itself.
pub struct PropCtx<'a> {
    pub username: Option<&'a str>,
    pub locks: Option<&'a LockManager>,
    pub dead: Option<&'a DeadProps>,
}

/// One `<D:response>` for `res`.
pub fn write_response(out: &mut String, res: &Resource, mode: &Mode, ctx: &PropCtx<'_>) {
    out.push_str("<D:response><D:href>");
    out.push_str(&xml::escape_text(&res.href()));
    out.push_str("</D:href>");
    let names: &[&str] = if res.is_collection() {
        &FOLDER_PROPS
    } else {
        &FILE_PROPS
    };
    match mode {
        Mode::AllProp => {
            let mut ok = String::new();
            let mut missing = String::new();
            for name in names {
                match live_value(res, name) {
                    Some(v) => push_prop(&mut ok, Some(DAV_NS), name, Some(v.as_str())),
                    None => push_prop(&mut missing, Some(DAV_NS), name, None),
                }
            }
            push_propstat(out, &ok, "200 OK");
            push_propstat(out, &missing, "404 Not Found");
        }
        Mode::PropName => {
            let mut ok = String::new();
            for name in names {
                push_prop(&mut ok, Some(DAV_NS), name, None);
            }
            push_propstat(out, &ok, "200 OK");
        }
        Mode::Named(list) => {
            let mut ok = String::new();
            let mut missing = String::new();
            for el in list {
                let ns = el.ns.as_deref();
                let value = if ns == Some(DAV_NS) {
                    match el.local.as_str() {
                        "lockdiscovery" => ctx.locks.map(|l| lockdiscovery(res, l, ctx.username)),
                        "supportedlock" => Some(SUPPORTED_LOCK.to_string()),
                        name => live_value(res, name),
                    }
                } else {
                    ctx.dead
                        .and_then(|d| d.get(&resource_key(res, ctx.username), ns, &el.local))
                        .map(|e| dead_inner(&e))
                };
                match value {
                    Some(v) => push_prop(&mut ok, ns, &el.local, Some(v.as_str())),
                    None => push_prop(&mut missing, ns, &el.local, None),
                }
            }
            push_propstat(out, &ok, "200 OK");
            push_propstat(out, &missing, "404 Not Found");
        }
    }
    out.push_str("</D:response>");
}

fn push_prop(out: &mut String, ns: Option<&str>, local: &str, value: Option<&str>) {
    match value {
        Some("") => out.push_str(&xml::prop_open_tag(ns, local, true)),
        Some(v) => {
            out.push_str(&xml::prop_open_tag(ns, local, false));
            out.push_str(v);
            out.push_str(&xml::prop_close_tag(ns, local));
        }
        None => out.push_str(&xml::prop_open_tag(ns, local, true)),
    }
}

pub fn push_propstat(out: &mut String, props: &str, status: &str) {
    if props.is_empty() {
        return;
    }
    out.push_str("<D:propstat><D:prop>");
    out.push_str(props);
    out.push_str("</D:prop><D:status>HTTP/1.1 ");
    out.push_str(status);
    out.push_str("</D:status></D:propstat>");
}

/// A dead property's stored element, re-serialised as the content of its own element.
fn dead_inner(el: &Element) -> String {
    let mut s = xml::escape_text(&el.text);
    for c in &el.children {
        c.to_xml(&mut s);
    }
    s
}

/// The inner XML of a `DAV:` live property, or `None` when the resource has none.
pub fn live_value(res: &Resource, name: &str) -> Option<String> {
    let stat = res.stat();
    match name {
        "resourcetype" => Some(if res.is_collection() {
            "<D:collection />".to_string()
        } else {
            String::new()
        }),
        "creationdate" => Some(rfc3339(stat.map(|s| s.ctime_secs).unwrap_or_else(now_secs))),
        "getcontentlength" => match res {
            Resource::File { stat, .. } => Some(stat.size.to_string()),
            _ => None,
        },
        "getcontenttype" => match res {
            Resource::File { phys, .. } => {
                Some(content_type(&crate::paths::file_name(phys)).to_string())
            }
            _ => None,
        },
        "getlastmodified" => Some(http_date(res.last_modified())),
        "displayname" => Some(xml::escape_text(&res.display_name())),
        "getetag" => res.etag().map(|e| xml::escape_text(&e)),
        _ => None,
    }
}

pub const SUPPORTED_LOCK: &str = "<D:lockentry><D:lockscope><D:exclusive /></D:lockscope><D:locktype><D:write /></D:locktype></D:lockentry><D:lockentry><D:lockscope><D:shared /></D:lockscope><D:locktype><D:write /></D:locktype></D:lockentry>";

/// `<D:activelock>` elements of the locks set on `res`.
pub fn lockdiscovery(res: &Resource, locks: &LockManager, username: Option<&str>) -> String {
    let mut out = String::new();
    for lock in locks.locks_on(&resource_key(res, username)) {
        out.push_str(&activelock(&lock));
    }
    out
}

pub fn activelock(lock: &crate::locks::Lock) -> String {
    format!(
        "<D:activelock><D:locktype><D:write /></D:locktype><D:lockscope><D:{} /></D:lockscope><D:depth>{}</D:depth>{}<D:timeout>{}</D:timeout><D:locktoken><D:href>{}</D:href></D:locktoken><D:lockroot><D:href>{}</D:href></D:lockroot></D:activelock>",
        lock.scope.as_str(),
        if lock.infinite_depth { "infinity" } else { "0" },
        lock.owner_xml,
        lock.timeout_text(),
        xml::escape_text(&lock.token),
        xml::escape_text(&lock.root_href),
    )
}

/// Walk `res` to `depth` (`0`, `1`, or `None` = infinity) in pre-order, writing one
/// response per resource into `sink` in chunks. Blocking (stats and directory reads).
pub fn walk(
    roots: &Roots,
    res: &Resource,
    depth: Option<u32>,
    mode: &Mode,
    ctx: &PropCtx<'_>,
    sink: &mut dyn FnMut(&str),
) {
    let mut buf = String::with_capacity(64 * 1024);
    let mut ancestors: Vec<PathBuf> = Vec::new();
    match res.phys() {
        Some(p) => ancestors.extend(std::fs::canonicalize(p).ok()),
        // The virtual roots list the library itself.
        None => ancestors.push(roots.library.clone()),
    }
    walk_inner(roots, res, depth, mode, ctx, sink, &mut buf, &mut ancestors);
    if !buf.is_empty() {
        sink(&buf);
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_inner(
    roots: &Roots,
    res: &Resource,
    depth: Option<u32>,
    mode: &Mode,
    ctx: &PropCtx<'_>,
    sink: &mut dyn FnMut(&str),
    buf: &mut String,
    ancestors: &mut Vec<PathBuf>,
) {
    write_response(buf, res, mode, ctx);
    if buf.len() >= 64 * 1024 {
        sink(buf);
        buf.clear();
    }
    if !res.is_collection() || depth == Some(0) {
        return;
    }
    let child_depth = depth.map(|d| d.saturating_sub(1));
    for child in roots.members(res, ctx.username) {
        if !child.is_collection() || child_depth == Some(0) {
            write_response(buf, &child, mode, ctx);
            if buf.len() >= 64 * 1024 {
                sink(buf);
                buf.clear();
            }
            continue;
        }
        match &child {
            Resource::Folder { .. } => match roots.walkable(&child, ancestors) {
                Some(canon) => {
                    ancestors.push(canon);
                    walk_inner(roots, &child, child_depth, mode, ctx, sink, buf, ancestors);
                    ancestors.pop();
                }
                // Listed, not descended (symlink out of the library, or a loop).
                None => write_response(buf, &child, mode, ctx),
            },
            _ => walk_inner(roots, &child, child_depth, mode, ctx, sink, buf, ancestors),
        }
    }
}
