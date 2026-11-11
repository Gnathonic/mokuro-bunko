//! What a request was (design §4.3): its protocol, category and detail, its method
//! (one of 14 or `OTHER`), and the series and volume it is attributed to. Pure string
//! work on the path as the router saw it (a reverse proxy strips any mount prefix),
//! plus what the DAV layer said it resolved ([`DavInfo`]).

use bunko_core::names::is_fixed_name;
use bunko_core::sidecar_names::ARCHIVE_EXTENSIONS;
use bunko_dav::{DavDepth, DavInfo, DavTarget};

pub const PROTOCOL_DAV: &str = "dav";
pub const PROTOCOL_API: &str = "api";
pub const PROTOCOL_WEB: &str = "web";
pub const PROTOCOL_PROCESSOR: &str = "processor";
pub const PROTOCOLS: [&str; 4] = [PROTOCOL_DAV, PROTOCOL_API, PROTOCOL_WEB, PROTOCOL_PROCESSOR];

pub mod category {
    pub const VOLUME: &str = "volume";
    pub const SIDECAR: &str = "sidecar";
    pub const COVER: &str = "cover";
    pub const METADATA: &str = "metadata";
    pub const PROGRESS_SYNC: &str = "progress_sync";
    pub const PROFILE_SYNC: &str = "profile_sync";
    pub const PERSONAL: &str = "personal";
    pub const UPLOAD: &str = "upload";
    pub const AUTH: &str = "auth";
    pub const ADMIN: &str = "admin";
    pub const WEB: &str = "web";
    pub const PROCESSOR: &str = "processor";
    pub const OTHER: &str = "other";
}

/// Every category, in the order the API lists them.
pub const CATEGORIES: [&str; 13] = [
    category::VOLUME,
    category::SIDECAR,
    category::COVER,
    category::METADATA,
    category::PROGRESS_SYNC,
    category::PROFILE_SYNC,
    category::PERSONAL,
    category::UPLOAD,
    category::AUTH,
    category::ADMIN,
    category::WEB,
    category::PROCESSOR,
    category::OTHER,
];

/// The methods counted by name; anything else is `OTHER`.
pub const METHODS: [&str; 14] = [
    "GET",
    "HEAD",
    "OPTIONS",
    "POST",
    "PUT",
    "DELETE",
    "PATCH",
    "PROPFIND",
    "PROPPATCH",
    "MKCOL",
    "COPY",
    "MOVE",
    "LOCK",
    "UNLOCK",
];

/// Methods only WebDAV has: such a request is WebDAV wherever it went.
const DAV_ONLY: [&str; 7] = [
    "PROPFIND",
    "PROPPATCH",
    "MKCOL",
    "COPY",
    "MOVE",
    "LOCK",
    "UNLOCK",
];
/// Methods that write the library (`upload`).
const WRITES: [&str; 8] = [
    "PUT",
    "MKCOL",
    "COPY",
    "MOVE",
    "DELETE",
    "LOCK",
    "UNLOCK",
    "PROPPATCH",
];

/// The reader's OCR queue file, polled with `If-None-Match`.
const QUEUE_FILE: &str = ".mokuro-queue.json";

/// The method as counted: its upper-case name when it is one of [`METHODS`].
pub fn method_name(method: &str) -> &'static str {
    METHODS
        .iter()
        .find(|m| m.eq_ignore_ascii_case(method))
        .copied()
        .unwrap_or("OTHER")
}

/// A series' identity in the analytics: one key for every spelling of its folder
/// that the catalog takes for one series (`bunko_db::fold_series_title_key`: case,
/// whitespace runs, NFC) or that any host takes for one folder (the filename fold,
/// [`bunko_core::names::fold_name`]: `Straße/` and `STRASSE/`, `ſ`, ignorables), so a
/// case-only rename never splits a series' numbers. An ASCII title keys exactly as
/// `fold_series_title_key` does.
pub fn series_key(title: &str) -> String {
    bunko_db::fold_series_title_key(&bunko_core::names::fold_name(title))
}

/// A library archive's identity (`Series/Vol 1.cbz`): its folder by [`series_key`],
/// its file name by the filename fold, so every spelling of one archive (a case-only
/// rename, a client asking in another case) is one volume.
pub fn volume_identity(rel: &str) -> String {
    let rel = rel.trim_matches('/');
    match rel.rsplit_once('/') {
        Some((parent, name)) => format!(
            "{}/{}",
            series_key(parent),
            bunko_core::names::fold_name(name)
        ),
        None => bunko_core::names::fold_name(rel),
    }
}

/// A series a request is attributed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesRef {
    /// [`series_key`] of the folder path: one key for every spelling.
    pub key: String,
    /// The folder as spelled on disk (or as the request named it).
    pub title: String,
}

impl SeriesRef {
    fn of(title: &str) -> Option<SeriesRef> {
        let title = title.trim_matches('/');
        if title.is_empty() {
            return None;
        }
        Some(SeriesRef {
            key: series_key(title),
            title: title.to_string(),
        })
    }
}

/// What a request was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Class {
    pub protocol: &'static str,
    pub category: &'static str,
    pub detail: &'static str,
    pub series: Option<SeriesRef>,
    /// The archive's library-relative path (`volume` only).
    pub volume_key: Option<String>,
    /// Where in the DAV tree it went ([`DavTarget::Other`] outside it).
    pub target: DavTarget,
    /// The library- or per-user-relative path.
    pub rel: Option<String>,
}

/// The facts classification needs.
#[derive(Debug, Clone, Copy)]
pub struct Input<'a> {
    /// [`method_name`].
    pub method: &'static str,
    /// The request path, percent-decoded.
    pub path: &'a str,
    pub query: Option<&'a str>,
    /// What DAV resolved, when the request reached it.
    pub dav: Option<&'a DavInfo>,
    /// The request's `Depth` header (a PROPFIND refused before DAV).
    pub depth: Option<&'a str>,
    /// The principal is a processor account.
    pub processor: bool,
    pub status: u16,
    /// The request had a `Range` header.
    pub range: bool,
    /// nginx sends the body (`X-Accel-Redirect`).
    pub accel: bool,
}

/// `/mokuro-reader`, `/inbox` and below.
fn under_dav_tree(path: &str) -> bool {
    bunko_dav::is_dav_path(path) && !matches!(path, "" | "/")
}

/// A JSON API path: `/api/...` or `/<module>/api/...` (as `base_href` tells pages from
/// APIs).
fn is_api_path(path: &str) -> bool {
    let mut segs = path.trim_start_matches('/').split('/');
    let (first, second) = (segs.next(), segs.next());
    first == Some("api") || second == Some("api")
}

fn starts(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `name` ends with `suffix`, ASCII case folded (no allocation: this runs per request).
fn ends_with_ci(name: &str, suffix: &str) -> bool {
    name.len() >= suffix.len()
        && name.as_bytes()[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

/// The last extension, as written.
fn ext(name: &str) -> &str {
    name.rsplit_once('.').map(|(_, e)| e).unwrap_or("")
}

fn ext_is(name: &str, any: &[&str]) -> bool {
    let e = ext(name);
    any.iter().any(|x| e.eq_ignore_ascii_case(x))
}

fn is_archive(name: &str) -> bool {
    ARCHIVE_EXTENSIONS.iter().any(|e| ends_with_ci(name, e))
}

fn is_sidecar(name: &str) -> bool {
    ends_with_ci(name, ".mokuro") || ends_with_ci(name, ".mokuro.gz")
}

fn is_cover(name: &str) -> bool {
    ext_is(name, &["webp", "nocover"])
}

/// A name a library file has (a folder is anything else: `Vol. 1` is a folder).
fn looks_like_file(name: &str) -> bool {
    is_archive(name)
        || is_sidecar(name)
        || ext_is(
            name,
            &[
                "webp", "nocover", "json", "gz", "jpg", "jpeg", "png", "avif", "txt",
            ],
        )
}

fn parent_of(rel: &str) -> &str {
    rel.trim_matches('/')
        .rsplit_once('/')
        .map(|(p, _)| p)
        .unwrap_or("")
}

fn name_of(rel: &str) -> &str {
    rel.trim_matches('/').rsplit('/').next().unwrap_or("")
}

/// One query parameter, `+` as space and percent-decoded (empty = missing).
pub fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(&s.replace('+', " "))
            .decode_utf8_lossy()
            .into_owned()
    };
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (decode(k) == name && !v.is_empty()).then(|| decode(v))
    })
}

fn propfind_detail(dav: Option<&DavInfo>, depth: Option<&str>) -> &'static str {
    let depth = match dav {
        Some(i) => i.depth,
        None => match depth.map(|d| d.trim().to_ascii_lowercase()).as_deref() {
            None | Some("infinity") => Some(DavDepth::Infinity),
            Some("0") => Some(DavDepth::Zero),
            Some("1") => Some(DavDepth::One),
            Some(_) => None,
        },
    };
    match depth {
        Some(DavDepth::Zero) => "propfind_0",
        Some(DavDepth::One) => "propfind_1",
        Some(DavDepth::Infinity) => "propfind_inf",
        None => "",
    }
}

pub fn classify(i: &Input<'_>) -> Class {
    let path = i.path;
    let (target, rel) = match i.dav {
        // DAV canonicalises its rel already; again here so no caller can key one file
        // (or make up a series) by `//` or `/.` segments.
        Some(info) => (
            info.target,
            info.rel
                .as_deref()
                .map(|r| bunko_dav::canonical_path(r).into_owned()),
        ),
        None if bunko_dav::is_dav_path(path) => bunko_dav::dav_target_of(path),
        None => (DavTarget::Other, None),
    };
    let protocol = if i.processor || starts(path, "/_processor") {
        PROTOCOL_PROCESSOR
    } else if i.dav.is_some() || DAV_ONLY.contains(&i.method) || under_dav_tree(path) {
        PROTOCOL_DAV
    } else if path == "/" && !matches!(i.method, "GET" | "HEAD") {
        // `GET /` from a browser is the home page; anything else at `/` is WebDAV's.
        PROTOCOL_DAV
    } else if is_api_path(path) {
        PROTOCOL_API
    } else {
        PROTOCOL_WEB
    };
    let mut c = Class {
        protocol,
        category: category::OTHER,
        detail: "",
        series: None,
        volume_key: None,
        target,
        rel,
    };
    if protocol == PROTOCOL_PROCESSOR {
        c.category = category::PROCESSOR;
        return c;
    }
    if starts(path, "/_admin/api") {
        c.category = category::ADMIN;
        return c;
    }
    let get = matches!(i.method, "GET" | "HEAD");
    match target {
        DavTarget::Progress => {
            let rel = c.rel.as_deref().unwrap_or("");
            (c.category, c.detail) = match rel {
                "volume-data.json" => (category::PROGRESS_SYNC, "volume_data"),
                "positions.json" => (category::PROGRESS_SYNC, "positions"),
                "profiles.json" => (category::PROFILE_SYNC, "profiles"),
                "goals.json" => (category::PROFILE_SYNC, "goals"),
                r if r == "history" || r.starts_with("history/") => {
                    (category::PROGRESS_SYNC, "history")
                }
                _ => (category::PROGRESS_SYNC, ""),
            };
            return c;
        }
        DavTarget::Personal => {
            c.category = category::PERSONAL;
            return c;
        }
        _ => {}
    }
    if i.method == "OPTIONS" {
        (c.category, c.detail) = (category::METADATA, "options");
        return c;
    }
    match target {
        DavTarget::Library => classify_library(i, &mut c, get),
        DavTarget::Root | DavTarget::ReaderRoot => {
            if i.method == "PROPFIND" {
                (c.category, c.detail) = (category::METADATA, propfind_detail(i.dav, i.depth));
            } else if path == "/" && get && i.dav.is_none() {
                c.category = category::WEB;
            }
        }
        DavTarget::Inbox | DavTarget::Progress | DavTarget::Personal => {}
        DavTarget::Other => classify_outside_dav(i, &mut c),
    }
    c
}

fn classify_library(i: &Input<'_>, c: &mut Class, get: bool) {
    let rel = c.rel.take().unwrap_or_default();
    classify_library_rel(i, c, get, &rel);
    c.rel = Some(rel);
}

fn classify_library_rel(i: &Input<'_>, c: &mut Class, get: bool, rel: &str) {
    let name = name_of(rel);
    let parent = parent_of(rel);
    if i.method == "PROPFIND" {
        (c.category, c.detail) = (category::METADATA, propfind_detail(i.dav, i.depth));
        c.series = SeriesRef::of(if looks_like_file(name) { parent } else { rel });
        return;
    }
    if get {
        if rel == QUEUE_FILE {
            (c.category, c.detail) = (category::METADATA, "queue_poll");
        } else if !parent.is_empty() && is_fixed_name(name, bunko_core::names::SERIES_QUEUE_FILE) {
            // A series' queue file v2, read on the reader's series page.
            (c.category, c.detail) = (category::METADATA, "queue_poll");
            c.series = SeriesRef::of(parent);
        } else if is_archive(name) {
            let ranged = i.status == 206 || (i.accel && i.range);
            c.category = category::VOLUME;
            c.detail = if ranged { "range" } else { "full" };
            c.series = SeriesRef::of(parent);
            c.volume_key = Some(volume_identity(rel));
        } else if is_sidecar(name) {
            c.category = category::SIDECAR;
            c.detail = if bunko_db::layer_sidecar_volume_path(rel).is_some() {
                "layer"
            } else {
                "primary"
            };
            c.series = SeriesRef::of(parent);
        } else if is_cover(name) {
            c.category = category::COVER;
            c.series = SeriesRef::of(parent);
        } else if is_fixed_name(name, "series.json") && !parent.is_empty() {
            (c.category, c.detail) = (category::METADATA, "series_json");
            c.series = SeriesRef::of(parent);
        } else if is_fixed_name(rel, "catalog.json") {
            (c.category, c.detail) = (category::METADATA, "catalog_json");
        }
        return;
    }
    if WRITES.contains(&i.method) {
        c.category = category::UPLOAD;
        let folder = i.method == "MKCOL" || !looks_like_file(name);
        c.detail = if is_archive(name) {
            "archive"
        } else if is_sidecar(name) {
            "sidecar"
        } else if is_fixed_name(name, "series.json") {
            "series_json"
        } else if folder {
            "folder"
        } else {
            "other"
        };
        c.series = SeriesRef::of(if folder { rel } else { parent });
    }
}

fn classify_outside_dav(i: &Input<'_>, c: &mut Class) {
    let path = i.path;
    if path == "/login/api/me" {
        (c.category, c.detail) = (category::METADATA, "login_me");
    } else if starts(path, "/login")
        || starts(path, "/register")
        || starts(path, "/api/register")
        || starts(path, "/setup")
    {
        c.category = category::AUTH;
    } else if path == "/catalog/api/manifest" {
        (c.category, c.detail) = (category::METADATA, "manifest");
        c.series = query_param(i.query, "series").and_then(|s| SeriesRef::of(&s));
    } else if starts(path, "/catalog/api/cover") {
        c.category = category::COVER;
        let cover = match path.strip_prefix("/catalog/api/cover/") {
            Some(tail) => Some(tail.to_string()),
            None => query_param(i.query, "path"),
        };
        c.series = cover.and_then(|p| SeriesRef::of(parent_of(&p)));
    } else if starts(path, "/catalog/api/series") {
        (c.category, c.detail) = (category::METADATA, "catalog_api");
        let name = match path.strip_prefix("/catalog/api/series/") {
            Some(tail) => Some(tail.to_string()),
            None => query_param(i.query, "name"),
        };
        c.series = name.and_then(|n| SeriesRef::of(&n));
    } else if starts(path, "/catalog/api") {
        (c.category, c.detail) = (category::METADATA, "catalog_api");
    } else if starts(path, "/queue/api") {
        (c.category, c.detail) = (category::METADATA, "queue_poll");
    } else if c.protocol == PROTOCOL_WEB {
        c.category = category::WEB;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(method: &'static str, path: &'a str) -> Input<'a> {
        Input {
            method,
            path,
            query: None,
            dav: None,
            depth: None,
            processor: false,
            status: 200,
            range: false,
            accel: false,
        }
    }

    fn dav(target: DavTarget, rel: Option<&str>, depth: Option<DavDepth>) -> DavInfo {
        DavInfo {
            target,
            rel: rel.map(str::to_string),
            depth,
        }
    }

    fn pcd(c: &Class) -> (&str, &str, &str) {
        (c.protocol, c.category, c.detail)
    }

    /// One series and one volume for every spelling a host takes for one folder or
    /// file (a case-only rename, the filename fold's wider joins), and for every
    /// spelling the catalog takes for one series; ASCII keys as the catalog's.
    #[test]
    fn one_identity_per_spelling() {
        for (a, b) in [
            ("Kingdom", "KINGDOM"),
            ("Kingdom", "kingdom"),
            ("Dr Stone", "dr  stone"),
            ("Straße", "STRASSE"),
            ("ſeries", "Series"),
            ("ΟΔΟΣ", "οδοσ"),
            ("Vol\u{200B}ume", "VOLUME"),
            ("Cafe\u{301}", "CAFÉ"),
        ] {
            assert_eq!(series_key(a), series_key(b), "{a:?} {b:?}");
        }
        for t in ["Kingdom", "Dr  Stone ", "The KINGDOM/Arc 2"] {
            assert_eq!(series_key(t), bunko_db::fold_series_title_key(t), "{t:?}");
        }
        assert_ne!(series_key("Kingdom"), series_key("Kingdoms"));
        for (a, b) in [
            ("Kingdom/Vol 1.cbz", "KINGDOM/VOL 1.CBZ"),
            ("Straße/Straße 01.cbz", "STRASSE/STRASSE 01.cbz"),
            ("/Kingdom/ſ.cbz", "kingdom/S.CBZ"),
        ] {
            assert_eq!(volume_identity(a), volume_identity(b), "{a:?} {b:?}");
        }
        assert_eq!(volume_identity("Kingdom/Vol 1.cbz"), "kingdom/vol 1.cbz");
        // Two names no host joins stay two volumes.
        assert_ne!(
            volume_identity("Kingdom/Vol 1.cbz"),
            volume_identity("Kingdom/Vol  1.cbz")
        );
        assert_ne!(
            volume_identity("Kingdom/Vol 1.cbz"),
            volume_identity("Kingdom/Vol 2.cbz")
        );
    }

    #[test]
    fn methods() {
        assert_eq!(method_name("propfind"), "PROPFIND");
        assert_eq!(method_name("GET"), "GET");
        assert_eq!(method_name("BREW"), "OTHER");
    }

    #[test]
    fn volumes_sidecars_covers() {
        let info = dav(DavTarget::Library, Some("Kingdom/Kingdom 01.CBZ"), None);
        let c = classify(&Input {
            dav: Some(&info),
            ..input("GET", "/mokuro-reader/kingdom/Kingdom 01.CBZ")
        });
        assert_eq!(pcd(&c), ("dav", "volume", "full"));
        // The archive's identity, one for every spelling of it.
        assert_eq!(c.volume_key.as_deref(), Some("kingdom/kingdom 01.cbz"));
        let s = c.series.unwrap();
        assert_eq!((s.key.as_str(), s.title.as_str()), ("kingdom", "Kingdom"));
        let c = classify(&Input {
            dav: Some(&info),
            status: 206,
            range: true,
            ..input("GET", "/x")
        });
        assert_eq!(c.detail, "range");
        let c = classify(&Input {
            dav: Some(&info),
            accel: true,
            range: true,
            ..input("GET", "/x")
        });
        assert_eq!(c.detail, "range");
        // Nested series folders are `/` paths.
        let c = classify(&input("HEAD", "/mokuro-reader/A/B/v.rar"));
        assert_eq!(pcd(&c), ("dav", "volume", "full"));
        assert_eq!(c.series.unwrap().title, "A/B");
        for (path, cat, detail) in [
            ("/mokuro-reader/S/v.mokuro", "sidecar", "primary"),
            ("/mokuro-reader/S/v.mokuro.gz", "sidecar", "primary"),
            (
                "/mokuro-reader/S/v.hayai-nova.mokuro.gz",
                "sidecar",
                "layer",
            ),
            ("/mokuro-reader/S/Vol 01.5.mokuro", "sidecar", "primary"),
            ("/mokuro-reader/S/v.webp", "cover", ""),
            ("/mokuro-reader/S/v.nocover", "cover", ""),
            ("/mokuro-reader/S/series.json", "metadata", "series_json"),
            // Every spelling the filename fold joins, as the server treats them.
            (
                "/mokuro-reader/S/\u{17f}eries.json",
                "metadata",
                "series_json",
            ),
            (
                "/mokuro-reader/S/SERIES\u{200B}.json",
                "metadata",
                "series_json",
            ),
            ("/mokuro-reader/catalog.json", "metadata", "catalog_json"),
            ("/mokuro-reader/Catalog.JSON", "metadata", "catalog_json"),
            (
                "/mokuro-reader/.mokuro-queue.json",
                "metadata",
                "queue_poll",
            ),
            (
                "/mokuro-reader/S/.mokuro-series-queue.json",
                "metadata",
                "queue_poll",
            ),
            (
                "/mokuro-reader/S/.MOKURO-Series-Queue.json",
                "metadata",
                "queue_poll",
            ),
            ("/mokuro-reader/S/notes.txt", "other", ""),
        ] {
            let c = classify(&input("GET", path));
            assert_eq!(pcd(&c), ("dav", cat, detail), "{path}");
        }
        let c = classify(&Input {
            query: Some("path=S%2Fv.webp"),
            ..input("GET", "/catalog/api/cover")
        });
        assert_eq!(pcd(&c), ("api", "cover", ""));
        assert_eq!(c.series.unwrap().title, "S");
    }

    #[test]
    fn listings_and_metadata() {
        for (depth, detail) in [
            (DavDepth::Zero, "propfind_0"),
            (DavDepth::One, "propfind_1"),
            (DavDepth::Infinity, "propfind_inf"),
        ] {
            let info = dav(DavTarget::Library, Some("Some  Series"), Some(depth));
            let c = classify(&Input {
                dav: Some(&info),
                ..input("PROPFIND", "/mokuro-reader/Some  Series/")
            });
            assert_eq!(pcd(&c), ("dav", "metadata", detail));
            assert_eq!(c.series.as_ref().unwrap().key, "some series");
        }
        // The reader's whole listing: navigation, no series.
        let info = dav(DavTarget::ReaderRoot, None, Some(DavDepth::Infinity));
        let c = classify(&Input {
            dav: Some(&info),
            ..input("PROPFIND", "/mokuro-reader/")
        });
        assert_eq!(pcd(&c), ("dav", "metadata", "propfind_inf"));
        assert!(c.series.is_none());
        // Refused before DAV: the header says the depth.
        let c = classify(&Input {
            depth: Some("1"),
            status: 401,
            ..input("PROPFIND", "/mokuro-reader/S")
        });
        assert_eq!(pcd(&c), ("dav", "metadata", "propfind_1"));
        let c = classify(&input("PROPFIND", "/mokuro-reader/S/v.cbz"));
        assert_eq!(c.series.unwrap().title, "S");
        assert_eq!(
            pcd(&classify(&input("OPTIONS", "/mokuro-reader/"))),
            ("dav", "metadata", "options")
        );
        assert_eq!(
            pcd(&classify(&input("OPTIONS", "/login/api/token"))),
            ("api", "metadata", "options")
        );
        assert_eq!(
            pcd(&classify(&input("GET", "/login/api/me"))),
            ("api", "metadata", "login_me")
        );
        let c = classify(&Input {
            query: Some("name=Kingdom"),
            ..input("GET", "/catalog/api/series")
        });
        assert_eq!(pcd(&c), ("api", "metadata", "catalog_api"));
        assert_eq!(c.series.unwrap().key, "kingdom");
        let c = classify(&Input {
            query: Some("series=A+B&volume=v"),
            ..input("GET", "/catalog/api/manifest")
        });
        assert_eq!(pcd(&c), ("api", "metadata", "manifest"));
        assert_eq!(c.series.unwrap().title, "A B");
        assert_eq!(
            pcd(&classify(&input("GET", "/catalog/api/library"))),
            ("api", "metadata", "catalog_api")
        );
        assert_eq!(
            pcd(&classify(&input("GET", "/queue/api/status"))),
            ("api", "metadata", "queue_poll")
        );
    }

    #[test]
    fn per_user_files() {
        for (path, cat, detail) in [
            (
                "/mokuro-reader/volume-data.json",
                "progress_sync",
                "volume_data",
            ),
            (
                "/mokuro-reader/positions.json",
                "progress_sync",
                "positions",
            ),
            (
                "/mokuro-reader/history/dev-1/2026-10.events",
                "progress_sync",
                "history",
            ),
            ("/mokuro-reader/history", "progress_sync", "history"),
            ("/mokuro-reader/profiles.json", "profile_sync", "profiles"),
            ("/mokuro-reader/goals.json", "profile_sync", "goals"),
        ] {
            // An OPTIONS (a sync's CORS preflight) counts with the sync it precedes,
            // not under `metadata` / `options` (§4).
            for method in ["GET", "PUT", "PROPFIND", "DELETE", "OPTIONS"] {
                let c = classify(&input(method, path));
                assert_eq!(pcd(&c), ("dav", cat, detail), "{method} {path}");
                assert_eq!(c.target, DavTarget::Progress);
            }
        }
        let info = dav(DavTarget::Personal, Some("S/v.edit.mokuro"), None);
        let c = classify(&Input {
            dav: Some(&info),
            ..input("PUT", "/mokuro-reader/S/v.edit.mokuro")
        });
        assert_eq!(pcd(&c), ("dav", "personal", ""));
        // A preflight to a personal file (no DAV info: answered before WebDAV) counts
        // there too; one to a shared library file is `metadata` / `options`.
        assert_eq!(
            pcd(&classify(&input(
                "OPTIONS",
                "/mokuro-reader/S/v.edit.mokuro"
            ))),
            ("dav", "personal", "")
        );
        assert_eq!(
            pcd(&classify(&input("OPTIONS", "/mokuro-reader/S/v.cbz"))),
            ("dav", "metadata", "options")
        );
    }

    #[test]
    fn uploads() {
        for (method, path, detail, series) in [
            ("PUT", "/mokuro-reader/S/v.cbz", "archive", "S"),
            ("PUT", "/mokuro-reader/S/v.mokuro", "sidecar", "S"),
            ("PUT", "/mokuro-reader/S/series.json", "series_json", "S"),
            (
                "PUT",
                "/mokuro-reader/S/\u{17f}eries.json",
                "series_json",
                "S",
            ),
            ("MKCOL", "/mokuro-reader/S", "folder", "S"),
            ("MKCOL", "/mokuro-reader/S/Sub", "folder", "S/Sub"),
            ("DELETE", "/mokuro-reader/Vol. 1", "folder", "Vol. 1"),
            ("MOVE", "/mokuro-reader/S/v.webp", "other", "S"),
            ("LOCK", "/mokuro-reader/S/v.cbz", "archive", "S"),
        ] {
            let c = classify(&input(method, path));
            assert_eq!(pcd(&c), ("dav", "upload", detail), "{method} {path}");
            assert_eq!(c.series.unwrap().title, series, "{method} {path}");
        }
    }

    #[test]
    fn protocols_and_the_rest() {
        assert_eq!(pcd(&classify(&input("GET", "/"))), ("web", "web", ""));
        let root = dav(DavTarget::Root, None, Some(DavDepth::One));
        assert_eq!(
            pcd(&classify(&Input {
                dav: Some(&root),
                ..input("PROPFIND", "/")
            })),
            ("dav", "metadata", "propfind_1")
        );
        assert_eq!(pcd(&classify(&input("PUT", "/"))).0, "dav");
        assert_eq!(
            pcd(&classify(&input("GET", "/catalog"))),
            ("web", "web", "")
        );
        assert_eq!(
            pcd(&classify(&input("GET", "/_static/shared.css"))),
            ("web", "web", "")
        );
        assert_eq!(
            pcd(&classify(&input("GET", "/_admin/"))),
            ("web", "web", "")
        );
        assert_eq!(
            pcd(&classify(&input("GET", "/_admin/api/status"))),
            ("api", "admin", "")
        );
        assert_eq!(
            pcd(&classify(&input("POST", "/login/api/token"))),
            ("api", "auth", "")
        );
        assert_eq!(pcd(&classify(&input("GET", "/login"))), ("web", "auth", ""));
        assert_eq!(
            pcd(&classify(&input("GET", "/setup/api/status"))),
            ("api", "auth", "")
        );
        assert_eq!(
            pcd(&classify(&input("POST", "/api/register"))),
            ("api", "auth", "")
        );
        assert_eq!(
            pcd(&classify(&input("GET", "/api/health"))),
            ("api", "other", "")
        );
        assert_eq!(
            pcd(&classify(&input("GET", "/_processor/x/socket"))),
            ("processor", "processor", "")
        );
        // A processor account's archive fetch is processor traffic.
        let c = classify(&Input {
            processor: true,
            ..input("GET", "/mokuro-reader/S/v.cbz")
        });
        assert_eq!(pcd(&c), ("processor", "processor", ""));
        // A DAV-only method anywhere is WebDAV.
        assert_eq!(classify(&input("PROPFIND", "/login")).protocol, "dav");
        assert_eq!(
            pcd(&classify(&input("GET", "/inbox/x"))),
            ("dav", "other", "")
        );
        assert_eq!(
            pcd(&classify(&input("GET", "/mokuro-reader"))),
            ("dav", "other", "")
        );
        assert_eq!(
            query_param(Some("a=1&name=A%20B&x="), "name").as_deref(),
            Some("A B")
        );
        assert_eq!(query_param(Some("x="), "x"), None);
    }
}
