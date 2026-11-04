//! What a DAV response tells the server's request accounting (docs/design/analytics.md
//! §4.2): response extensions, never on the wire. [`DavInfo`] is on every response
//! [`crate::Dav::handle`] gives; [`Served`] on GET/HEAD/304 of a file; [`ListingInfo`]
//! on a PROPFIND answered from the listing cache.

use crate::paths::{self, Target};
use crate::personal;

/// What a WebDAV request addressed, after path-case resolution (the server rewrites a
/// library path to its on-disk spelling before DAV sees it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavInfo {
    pub target: DavTarget,
    /// The library- or per-user-relative path (no leading or trailing `/`): the
    /// on-disk spelling for library paths, the caller's own tree for
    /// [`DavTarget::Progress`]. `None` for the roots and anything unmapped.
    pub rel: Option<String>,
    /// PROPFIND only: its depth (a missing header is infinity, RFC 4918 9.1).
    pub depth: Option<DavDepth>,
}

/// The part of the DAV tree a path is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DavTarget {
    /// `/`.
    Root,
    /// `/mokuro-reader`.
    ReaderRoot,
    /// The caller's own tree: the root files (`volume-data.json`, `positions.json`,
    /// `profiles.json`, `goals.json`) and `history/**`.
    Progress,
    /// A library file or folder.
    Library,
    /// The caller's own correction layer at a library path (`<Vol>.edit[-n].mokuro`).
    Personal,
    /// `/inbox` and below.
    Inbox,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DavDepth {
    Zero,
    One,
    Infinity,
}

/// A file body this response serves (GET, HEAD or a 304), for byte estimates and
/// savings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Served {
    /// The whole file.
    pub size: u64,
    /// The bytes this response carries (a range's length); 0 for a 304.
    pub len: u64,
    /// `X-Accel-Redirect`: nginx sends the bytes, the response body is empty.
    pub accel: bool,
}

/// A PROPFIND listing served from the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListingInfo {
    /// The listing's uncompressed XML length.
    pub raw_len: u64,
    /// Sent gzip-encoded (the body is the compressed bytes).
    pub gzip: bool,
}

/// Where a decoded request path is in the DAV tree, listing-free: a library file named
/// like a personal correction layer (`.edit[-n].mokuro[.gz]`) is
/// [`DavTarget::Personal`] (GET/HEAD refine it from the resolved file).
pub fn dav_target_of(path: &str) -> (DavTarget, Option<String>) {
    match paths::classify(path) {
        Target::Root => (DavTarget::Root, None),
        Target::ReaderRoot => (DavTarget::ReaderRoot, None),
        Target::Progress(rel) => (DavTarget::Progress, Some(rel)),
        Target::Library(rel) => {
            let rel = rel.trim_matches('/').to_string();
            let name = rel.rsplit('/').next().unwrap_or("");
            let target = if personal::personal_name(name).is_some() {
                DavTarget::Personal
            } else {
                DavTarget::Library
            };
            (target, Some(rel))
        }
        Target::Inbox => (DavTarget::Inbox, None),
        Target::Other => (DavTarget::Other, None),
    }
}

/// The `Depth` of a PROPFIND as DAV reads it (`None`: an invalid value, refused 400).
pub(crate) fn propfind_depth(header: Option<&str>) -> Option<DavDepth> {
    match header.map(|h| h.trim().to_ascii_lowercase()).as_deref() {
        None | Some("infinity") => Some(DavDepth::Infinity),
        Some("0") => Some(DavDepth::Zero),
        Some("1") => Some(DavDepth::One),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(dav_target_of("/"), (DavTarget::Root, None));
        assert_eq!(
            dav_target_of("/mokuro-reader/"),
            (DavTarget::ReaderRoot, None)
        );
        assert_eq!(
            dav_target_of("/mokuro-reader/positions.json"),
            (DavTarget::Progress, Some("positions.json".into()))
        );
        assert_eq!(
            dav_target_of("/mokuro-reader/history/dev/2026-10.events"),
            (
                DavTarget::Progress,
                Some("history/dev/2026-10.events".into())
            )
        );
        assert_eq!(
            dav_target_of("/mokuro-reader/S/v.cbz"),
            (DavTarget::Library, Some("S/v.cbz".into()))
        );
        assert_eq!(
            dav_target_of("/mokuro-reader/S/"),
            (DavTarget::Library, Some("S".into()))
        );
        assert_eq!(
            dav_target_of("/mokuro-reader/S/v.edit.mokuro"),
            (DavTarget::Personal, Some("S/v.edit.mokuro".into()))
        );
        assert_eq!(
            dav_target_of("/mokuro-reader/S/v.hayai-nova.mokuro"),
            (DavTarget::Library, Some("S/v.hayai-nova.mokuro".into()))
        );
        assert_eq!(dav_target_of("/inbox/x"), (DavTarget::Inbox, None));
        assert_eq!(dav_target_of("/elsewhere"), (DavTarget::Other, None));
        assert_eq!(propfind_depth(None), Some(DavDepth::Infinity));
        assert_eq!(propfind_depth(Some("1")), Some(DavDepth::One));
        assert_eq!(propfind_depth(Some("Infinity")), Some(DavDepth::Infinity));
        assert_eq!(propfind_depth(Some("2")), None);
    }
}
