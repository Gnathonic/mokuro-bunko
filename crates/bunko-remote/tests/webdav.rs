//! The WebDAV driver against a real WebDAV server in-process: bunko-dav over a temp
//! directory, behind Basic auth and the server's NTFS-style path-case rewrite.
#![cfg(feature = "webdav")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use bunko_core::{Role, StorageLayout};
use bunko_dav::{Dav, DavConfig, DavContext, NoHooks, PathCase};
use bunko_remote::backend::read_to_vec;
use bunko_remote::webdav::{BUNKO_BACKEND_REFUSED, WebDavBackend, WebDavConfig, snapshot_path};
use bunko_remote::{
    BErr, Backend, ChangeItem, ChangePage, Cursor, OpCtx, RNode, UploadSource, UploadTarget,
};
use http::{Request, Response};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

/// `admin:secret`.
const AUTH: &str = "Basic YWRtaW46c2VjcmV0";

#[derive(Default)]
struct Flags {
    /// Answer `Depth: infinity` with 403 (Apache, SabreDAV defaults).
    refuse_infinity: AtomicBool,
    /// Drop `Range` headers (a server that answers 200 to a Range request).
    ignore_range: AtomicBool,
    /// Give folders ETags that change with anything below them (Nextcloud, ownCloud).
    propagate: AtomicBool,
    /// `METHOD path depth` of every request.
    log: Mutex<Vec<String>>,
    /// Answer a PROPFIND of this exact path with this body instead.
    inject: Mutex<Option<(String, String)>>,
}

#[derive(Clone)]
struct App {
    dav: Dav,
    lib: PathBuf,
    flags: Arc<Flags>,
}

async fn handle(State(app): State<App>, req: Request<Body>) -> Response<Body> {
    if req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        != Some(AUTH)
    {
        return Response::builder()
            .status(401)
            .header("www-authenticate", "Basic realm=\"test\"")
            .body(Body::empty())
            .unwrap();
    }
    let propfind = req.method().as_str() == "PROPFIND";
    let depth_header = req
        .headers()
        .get("depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();
    app.flags.log.lock().push(format!(
        "{} {} {depth_header}",
        req.method(),
        req.uri().path()
    ));
    let propagate = propfind && app.flags.propagate.load(Ordering::SeqCst);
    if propfind {
        let depth = req
            .headers()
            .get("depth")
            .and_then(|v| v.to_str().ok())
            .map(str::to_ascii_lowercase);
        if app.flags.refuse_infinity.load(Ordering::SeqCst)
            && matches!(depth.as_deref(), None | Some("infinity"))
        {
            return Response::builder().status(403).body(Body::empty()).unwrap();
        }
        let injected = app.flags.inject.lock().clone();
        if let Some((path, xml)) = injected
            && req.uri().path() == path
        {
            return Response::builder()
                .status(207)
                .header("content-type", "application/xml; charset=utf-8")
                .body(Body::from(xml))
                .unwrap();
        }
    }
    let (mut parts, body) = req.into_parts();
    if app.flags.ignore_range.load(Ordering::SeqCst) {
        parts.headers.remove("range");
    }
    if propagate {
        parts.headers.remove("accept-encoding");
    }
    if PathCase::may_rewrite(&parts) {
        let dav = app.dav.clone();
        parts = tokio::task::spawn_blocking(move || {
            dav.path_case().rewrite_request(&mut parts);
            parts
        })
        .await
        .unwrap();
    }
    let ctx = DavContext::user("admin", Role::Admin, Arc::new(NoHooks));
    let resp = app.dav.handle(Request::from_parts(parts, body), ctx).await;
    if propagate {
        return propagate_etags(&app.lib, resp).await;
    }
    resp
}

/// A digest of everything below `dir` (names, sizes, mtimes), as a folder ETag.
fn subtree_tag(dir: &Path) -> String {
    fn feed(dir: &Path, h: &mut Sha256) {
        let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let meta = e.metadata().unwrap();
            h.update(e.file_name().as_encoded_bytes());
            h.update([u8::from(meta.is_dir())]);
            if meta.is_dir() {
                feed(&e.path(), h);
            } else {
                let t = meta.modified().unwrap();
                h.update(format!("{t:?}-{}", meta.len()).as_bytes());
            }
            h.update([0]);
        }
    }
    let mut h = Sha256::new();
    feed(dir, &mut h);
    format!("\"p-{}\"", &hex::encode(h.finalize())[..16])
}

/// Rewrite each folder's ETag in a multistatus to [`subtree_tag`].
async fn propagate_etags(lib: &Path, resp: Response<Body>) -> Response<Body> {
    let (mut parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let mut out = String::new();
    for (i, chunk) in text.split("<D:response>").enumerate() {
        if i > 0 {
            out.push_str("<D:response>");
        }
        let open = "<D:getetag>";
        if chunk.contains("<D:collection />")
            && let (Some(a), Some(b)) = (chunk.find(open), chunk.find("</D:getetag>"))
            && let (Some(h0), Some(h1)) = (chunk.find("<D:href>"), chunk.find("</D:href>"))
        {
            let href = &chunk[h0 + "<D:href>".len()..h1];
            let rel = href
                .strip_prefix("/mokuro-reader")
                .unwrap()
                .trim_matches('/');
            let rel = percent_encoding::percent_decode_str(rel)
                .decode_utf8()
                .unwrap();
            out.push_str(&chunk[..a + open.len()]);
            out.push_str(&subtree_tag(&lib.join(rel.as_ref())));
            out.push_str(&chunk[b..]);
        } else {
            out.push_str(chunk);
        }
    }
    parts.headers.remove("content-length");
    Response::from_parts(parts, Body::from(out))
}

struct Server {
    _dir: tempfile::TempDir,
    lib: PathBuf,
    dav: Dav,
    url: String,
    flags: Arc<Flags>,
    state: tempfile::TempDir,
}

impl Server {
    async fn start() -> Server {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("storage");
        let lib = base.join("library");
        std::fs::create_dir_all(lib.join("series")).unwrap();
        std::fs::create_dir_all(lib.join("Other/a")).unwrap();
        std::fs::write(lib.join("series/vol1.cbz"), b"volume one bytes").unwrap();
        std::fs::write(lib.join("series/vol1.mokuro"), b"{\"pages\":[]}").unwrap();
        std::fs::write(lib.join("Other/a/b.txt"), b"deep").unwrap();
        std::fs::write(lib.join("catalog.json"), b"{}").unwrap();
        let dav = Dav::new(&StorageLayout::new(&base), DavConfig::default()).unwrap();
        let flags = Arc::new(Flags::default());
        let lib = std::fs::canonicalize(lib).unwrap();
        let app = Router::new().fallback(handle).with_state(App {
            dav: dav.clone(),
            lib: lib.clone(),
            flags: flags.clone(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server {
            _dir: dir,
            lib,
            dav,
            url: format!("http://{addr}/"),
            flags,
            state: tempfile::tempdir().unwrap(),
        }
    }

    fn cfg(&self) -> WebDavConfig {
        WebDavConfig {
            url: self.url.clone(),
            username: "admin".into(),
            password: "secret".into(),
            folder: "mokuro-reader".into(),
            allow_http: true,
            allow_bunko_backend_for_tests: true,
            state_dir: self.state.path().to_path_buf(),
            disable_cas: false,
        }
    }

    async fn connect(&self) -> WebDavBackend {
        WebDavBackend::connect(self.cfg()).await.unwrap()
    }

    /// A change made straight on the served disk, as another client or the host would;
    /// the listing cache is dropped as the server's watcher would.
    fn out_of_band(&self, f: impl FnOnce(&Path)) {
        f(&self.lib);
        self.dav.propfind_cache().invalidate();
    }
}

fn source(dir: &Path, name: &str, data: &[u8]) -> (PathBuf, u64, [u8; 32]) {
    let p = dir.join(name);
    std::fs::write(&p, data).unwrap();
    (p, data.len() as u64, Sha256::digest(data).into())
}

async fn upload(
    b: &WebDavBackend,
    to: UploadTarget<'_>,
    tmp: &Path,
    data: &[u8],
) -> Result<RNode, BErr> {
    let (path, size, sha256) = source(tmp, "src.bin", data);
    let mut op = OpCtx {
        op_id: "op1".into(),
        resume: None,
    };
    b.upload(
        to,
        UploadSource::File {
            path: &path,
            size,
            sha256,
        },
        &mut op,
    )
    .await
}

async fn read_all(b: &WebDavBackend, pid: &str) -> Vec<u8> {
    read_to_vec(b.read(pid, None, None).await.unwrap())
        .await
        .unwrap()
}

async fn listed(b: &WebDavBackend) -> BTreeMap<String, RNode> {
    let mut out = BTreeMap::new();
    b.list_all(&[String::new()], &mut |n| {
        out.insert(n.pid.clone(), n);
    })
    .await
    .unwrap();
    out
}

fn dotfiles(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect()
}

#[tokio::test]
async fn probe_reports_what_bunko_dav_does() {
    let s = Server::start().await;
    let b = s.connect().await;
    let p = b.probe();
    assert!(p.folder_exists && p.writable, "{p:?}");
    assert!(p.bunko, "X-Mokuro-Put on OPTIONS");
    assert!(p.depth_infinity);
    assert!(p.range);
    assert!(p.strong_etags);
    assert!(p.if_match);
    assert!(p.if_none_match);
    // The server resolves library paths as on NTFS.
    assert_eq!(p.case_sensitive, Some(false));
    // Folder ETags are the folder's own mtime: nothing propagates upwards.
    assert!(!p.nested_propagation);
    assert_eq!(p.hash_kind, None);
    let caps = b.caps();
    assert!(caps.cas_create && caps.cas_overwrite && caps.pinned_read && caps.range);
    assert!(caps.path_ids && caps.server_copy && !caps.push);
    assert!(caps.atomic_replace);
    assert_eq!(caps.mtime_granularity_ns, 1_000_000_000);
    // The probe cleans up after itself.
    assert!(dotfiles(&s.lib).is_empty(), "{:?}", dotfiles(&s.lib));
}

#[tokio::test]
async fn probe_sees_a_server_without_infinity_or_ranges() {
    let s = Server::start().await;
    s.flags.refuse_infinity.store(true, Ordering::SeqCst);
    s.flags.ignore_range.store(true, Ordering::SeqCst);
    let b = s.connect().await;
    let p = b.probe();
    assert!(!p.depth_infinity && !p.range, "{p:?}");
    assert!(p.if_match && p.if_none_match);
}

#[tokio::test]
async fn refusals_at_connect() {
    let s = Server::start().await;
    let err = WebDavBackend::connect(WebDavConfig {
        allow_bunko_backend_for_tests: false,
        ..s.cfg()
    })
    .await
    .err()
    .unwrap();
    assert_eq!(err, BErr::Fatal(BUNKO_BACKEND_REFUSED.into()));
    // Refused before the probe wrote anything.
    let log = s.flags.log.lock().clone();
    assert!(
        log.iter()
            .all(|l| l.starts_with("OPTIONS ") || l.starts_with("PROPFIND ")),
        "{log:?}"
    );

    let err = WebDavBackend::connect(WebDavConfig {
        allow_http: false,
        ..s.cfg()
    })
    .await
    .err()
    .unwrap();
    assert!(matches!(err, BErr::Fatal(m) if m.contains("http://")));

    let err = WebDavBackend::connect(WebDavConfig {
        password: "wrong".into(),
        ..s.cfg()
    })
    .await
    .err()
    .unwrap();
    assert_eq!(err, BErr::AuthExpired);
}

#[tokio::test]
async fn locate_and_create_roots() {
    let s = Server::start().await;
    let b = s.connect().await;
    let roots = b.locate_roots("mokuro-reader").await.unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].pid, "");
    assert_eq!(roots[0].parent, None);
    assert_eq!(roots[0].name_raw, b"mokuro-reader");
    assert!(roots[0].is_dir);
    assert_eq!(
        b.create_root("mokuro-reader").await.err(),
        Some(BErr::NameConflict)
    );

    // A folder that does not exist yet: nothing to probe in, until it is created.
    let b = WebDavBackend::connect(WebDavConfig {
        folder: "mokuro-reader/nested/lib".into(),
        ..s.cfg()
    })
    .await
    .unwrap();
    assert!(!b.probe().folder_exists);
    assert!(!b.caps().cas_create);
    assert!(b.locate_roots("lib").await.unwrap().is_empty());
    // Its parent is missing too.
    assert_eq!(b.create_root("lib").await.err(), Some(BErr::NotFound));
    std::fs::create_dir(s.lib.join("nested")).unwrap();
    let root = b.create_root("lib").await.unwrap();
    assert_eq!((root.pid.as_str(), root.parent.clone()), ("", None));
    assert_eq!(root.name_raw, b"lib");
    assert!(s.lib.join("nested/lib").is_dir());
    let p = b.probe();
    assert!(p.folder_exists && p.writable && p.if_match, "{p:?}");
    assert!(b.caps().cas_create);
    assert_eq!(b.locate_roots("lib").await.unwrap(), vec![root]);
    assert!(dotfiles(&s.lib.join("nested/lib")).is_empty());

    // A root that is a child of the folder (`folder` names its parent).
    let b = WebDavBackend::connect(WebDavConfig {
        folder: "mokuro-reader/nested".into(),
        ..s.cfg()
    })
    .await
    .unwrap();
    let roots = b.locate_roots("lib").await.unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].pid, "lib");
    assert_eq!(roots[0].parent.as_deref(), Some(""));
}

async fn check_listing(s: &Server) {
    let b = s.connect().await;
    let all = listed(&b).await;
    let pids: BTreeSet<&str> = all.keys().map(String::as_str).collect();
    let want: BTreeSet<&str> = [
        "Other",
        "Other/a",
        "Other/a/b.txt",
        "catalog.json",
        "series",
        "series/vol1.cbz",
        "series/vol1.mokuro",
        "thumbnails",
    ]
    .into_iter()
    .collect();
    assert_eq!(pids, want);
    let v = &all["series/vol1.cbz"];
    assert_eq!(v.parent.as_deref(), Some("series"));
    assert_eq!(v.name_raw, b"vol1.cbz");
    assert!(!v.is_dir);
    assert_eq!(v.size, 16);
    assert_eq!(v.version, v.cver);
    assert_eq!(v.pin.as_deref(), Some(v.cver.as_str()));
    assert!(v.base_mtime_ms > 0 && v.base_mtime_ms % 1000 == 0);
    assert!(v.created_ms > 0);
    assert!(all["Other/a"].is_dir);
    assert_eq!(all["Other/a"].pin, None);
    // A stat agrees with the listing.
    assert_eq!(b.stat("series/vol1.cbz").await.unwrap().as_ref(), Some(v));

    let mut children = Vec::new();
    b.list_dir("series", &mut |n| children.push(n.pid))
        .await
        .unwrap();
    children.sort();
    assert_eq!(children, ["series/vol1.cbz", "series/vol1.mokuro"]);
    assert_eq!(
        b.list_dir("series/vol1.cbz", &mut |_| {}).await.err(),
        Some(BErr::NotFound)
    );
    assert_eq!(
        b.list_dir("missing", &mut |_| {}).await.err(),
        Some(BErr::NotFound)
    );
}

#[tokio::test]
async fn list_all_with_depth_infinity() {
    let s = Server::start().await;
    check_listing(&s).await;
}

#[tokio::test]
async fn list_all_with_recursive_depth_1() {
    let s = Server::start().await;
    s.flags.refuse_infinity.store(true, Ordering::SeqCst);
    check_listing(&s).await;
}

#[tokio::test]
async fn find_named_is_exact() {
    let s = Server::start().await;
    let b = s.connect().await;
    let (found, t) = b.find_named("series", "vol1.cbz").await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].pid, "series/vol1.cbz");
    assert!(t.0 > 0);
    // The host folds case, but the name asked for is not the one stored.
    assert!(
        b.find_named("series", "VOL1.cbz")
            .await
            .unwrap()
            .0
            .is_empty()
    );
    assert!(b.find_named("series", "nope").await.unwrap().0.is_empty());
    assert_eq!(
        b.find_named("missing", "x").await.err(),
        Some(BErr::NotFound)
    );
}

#[tokio::test]
async fn upload_with_cas() {
    let s = Server::start().await;
    let b = s.connect().await;
    let tmp = tempfile::tempdir().unwrap();
    let new = UploadTarget::New {
        parent: "series",
        name: "vol2.mokuro",
    };
    let n = upload(&b, new, tmp.path(), b"first").await.unwrap();
    assert_eq!(n.pid, "series/vol2.mokuro");
    assert_eq!(n.size, 5);
    assert_eq!(read_all(&b, &n.pid).await, b"first");
    // If-None-Match: * — a second create loses.
    assert_eq!(
        upload(&b, new, tmp.path(), b"other").await.err(),
        Some(BErr::PreconditionFailed)
    );
    // A folder in the way.
    let dir = UploadTarget::New {
        parent: "",
        name: "series",
    };
    assert_eq!(
        upload(&b, dir, tmp.path(), b"x").await.err(),
        Some(BErr::NameConflict)
    );
    let r = UploadTarget::Replace {
        pid: &n.pid,
        if_cver: Some(&n.cver),
    };
    let n2 = upload(&b, r, tmp.path(), b"second!").await.unwrap();
    assert_ne!(n2.cver, n.cver);
    assert_eq!(read_all(&b, &n.pid).await, b"second!");
    // A stale base: If-Match fails.
    assert_eq!(
        upload(&b, r, tmp.path(), b"third").await.err(),
        Some(BErr::PreconditionFailed)
    );
    assert_eq!(read_all(&b, &n.pid).await, b"second!");
    // Without a base: a plain replace.
    let n3 = upload(
        &b,
        UploadTarget::Replace {
            pid: &n.pid,
            if_cver: None,
        },
        tmp.path(),
        b"fourth",
    )
    .await
    .unwrap();
    assert_ne!(n3.cver, n2.cver);
    assert_eq!(read_all(&b, &n.pid).await, b"fourth");
    // Missing parent.
    assert_eq!(
        upload(
            &b,
            UploadTarget::New {
                parent: "missing",
                name: "x"
            },
            tmp.path(),
            b"x"
        )
        .await
        .err(),
        Some(BErr::NotFound)
    );
    assert_eq!(
        upload(
            &b,
            UploadTarget::New {
                parent: "series",
                name: "a/b"
            },
            tmp.path(),
            b"x"
        )
        .await
        .err(),
        Some(BErr::InvalidName)
    );
    assert!(dotfiles(&s.lib.join("series")).is_empty());
}

#[tokio::test]
async fn upload_without_cas_goes_through_a_temporary_name() {
    let s = Server::start().await;
    let b = WebDavBackend::connect(WebDavConfig {
        disable_cas: true,
        ..s.cfg()
    })
    .await
    .unwrap();
    let caps = b.caps();
    assert!(!caps.cas_create && !caps.cas_overwrite && !caps.pinned_read);
    let tmp = tempfile::tempdir().unwrap();
    let new = UploadTarget::New {
        parent: "series",
        name: "vol2.mokuro",
    };
    let n = upload(&b, new, tmp.path(), b"first").await.unwrap();
    assert_eq!(n.pin, None);
    assert_eq!(read_all(&b, &n.pid).await, b"first");
    // MOVE Overwrite: F still refuses to replace on a create.
    assert_eq!(
        upload(&b, new, tmp.path(), b"other").await.err(),
        Some(BErr::PreconditionFailed)
    );
    assert_eq!(read_all(&b, &n.pid).await, b"first");
    // A replace is last-writer-wins: the stale base is not checked.
    let n2 = upload(
        &b,
        UploadTarget::Replace {
            pid: &n.pid,
            if_cver: Some("stale"),
        },
        tmp.path(),
        b"second!",
    )
    .await
    .unwrap();
    assert_ne!(n2.cver, n.cver);
    assert_eq!(read_all(&b, &n.pid).await, b"second!");
    // No staging file is left behind, and none is ever listed.
    assert!(dotfiles(&s.lib.join("series")).is_empty());
    std::fs::write(s.lib.join("series/.vol9.cbz.bunko-x.tmp"), b"half").unwrap();
    s.dav.propfind_cache().invalidate();
    assert!(!listed(&b).await.keys().any(|p| p.contains(".bunko-")));
}

#[tokio::test]
async fn ranged_and_pinned_reads() {
    let s = Server::start().await;
    let b = s.connect().await;
    let tmp = tempfile::tempdir().unwrap();
    let n = upload(
        &b,
        UploadTarget::New {
            parent: "",
            name: "digits.bin",
        },
        tmp.path(),
        b"0123456789",
    )
    .await
    .unwrap();
    let read = |pin: Option<String>, range| {
        let b = &b;
        async move {
            match b.read("digits.bin", pin.as_deref(), range).await {
                Ok(body) => Ok(read_to_vec(body).await.unwrap()),
                Err(e) => Err(e),
            }
        }
    };
    assert_eq!(read(None, Some((2, Some(5)))).await.unwrap(), b"2345");
    assert_eq!(read(None, Some((7, None))).await.unwrap(), b"789");
    assert_eq!(read(n.pin.clone(), None).await.unwrap(), b"0123456789");
    // A server that ignores Range: the driver skips to the offset itself.
    s.flags.ignore_range.store(true, Ordering::SeqCst);
    assert_eq!(read(None, Some((2, Some(5)))).await.unwrap(), b"2345");
    assert_eq!(read(n.pin.clone(), Some((7, None))).await.unwrap(), b"789");
    s.flags.ignore_range.store(false, Ordering::SeqCst);
    // Replaced: the old pin no longer reads.
    let n2 = upload(
        &b,
        UploadTarget::Replace {
            pid: &n.pid,
            if_cver: Some(&n.cver),
        },
        tmp.path(),
        b"abcdefghij",
    )
    .await
    .unwrap();
    assert_eq!(
        read(n.pin.clone(), Some((0, Some(3)))).await.err(),
        Some(BErr::PreconditionFailed)
    );
    assert_eq!(
        read(n2.pin.clone(), Some((0, Some(3)))).await.unwrap(),
        b"abcd"
    );
    assert_eq!(
        b.read("nope.bin", None, None).await.err(),
        Some(BErr::NotFound)
    );
}

#[tokio::test]
async fn mkdir_move_copy_delete() {
    let s = Server::start().await;
    let b = s.connect().await;
    let op = OpCtx::default();
    let d = b.mkdir("", "New Series", &op).await.unwrap();
    assert!(d.is_dir);
    assert_eq!(
        (d.pid.as_str(), d.parent.as_deref()),
        ("New Series", Some(""))
    );
    assert!(s.lib.join("New Series").is_dir());
    assert_eq!(
        b.mkdir("", "New Series", &op).await.err(),
        Some(BErr::NameConflict)
    );
    assert_eq!(
        b.mkdir("missing", "x", &op).await.err(),
        Some(BErr::NotFound)
    );

    // Move into another folder.
    let m = b.stat("series/vol1.mokuro").await.unwrap().unwrap();
    let moved = b
        .update(&m.pid, Some("New Series"), None, Some(&m.cver))
        .await
        .unwrap();
    assert_eq!(moved.pid, "New Series/vol1.mokuro");
    assert_eq!(moved.parent.as_deref(), Some("New Series"));
    assert!(s.lib.join("New Series/vol1.mokuro").is_file());
    assert!(!s.lib.join("series/vol1.mokuro").exists());
    // Rename in place.
    let renamed = b
        .update(&moved.pid, None, Some("v1.mokuro"), None)
        .await
        .unwrap();
    assert_eq!(renamed.pid, "New Series/v1.mokuro");
    // Overwrite: F.
    assert_eq!(
        b.update(
            "series/vol1.cbz",
            Some("New Series"),
            Some("v1.mokuro"),
            None
        )
        .await
        .err(),
        Some(BErr::NameConflict)
    );
    // A stale if_cver.
    assert_eq!(
        b.update("series/vol1.cbz", None, Some("x.cbz"), Some("\"stale\""))
            .await
            .err(),
        Some(BErr::PreconditionFailed)
    );
    assert_eq!(
        b.update("nope", None, Some("x"), None).await.err(),
        Some(BErr::NotFound)
    );
    // A folder rename moves its subtree (new pids).
    let f = b.update("Other", None, Some("Others"), None).await.unwrap();
    assert!(f.is_dir);
    assert!(b.stat("Others/a/b.txt").await.unwrap().is_some());
    assert!(b.stat("Other/a/b.txt").await.unwrap().is_none());

    // Copy.
    let c = b
        .copy("series/vol1.cbz", "New Series", "copy.cbz")
        .await
        .unwrap();
    assert_eq!(c.pid, "New Series/copy.cbz");
    assert_eq!(read_all(&b, &c.pid).await, b"volume one bytes");
    assert_eq!(
        b.copy("series/vol1.cbz", "New Series", "copy.cbz")
            .await
            .err(),
        Some(BErr::NameConflict)
    );

    // Delete, conditionally and not.
    assert_eq!(
        b.trash(&c.pid, Some("\"stale\"")).await.err(),
        Some(BErr::PreconditionFailed)
    );
    b.trash(&c.pid, Some(&c.cver)).await.unwrap();
    assert!(b.stat(&c.pid).await.unwrap().is_none());
    b.delete_permanent("New Series/v1.mokuro").await.unwrap();
    assert!(!s.lib.join("New Series/v1.mokuro").exists());
    assert_eq!(b.trash("nope", None).await.err(), Some(BErr::NotFound));
    assert_eq!(b.delete_permanent("nope").await.err(), Some(BErr::NotFound));
    b.trash("Others", None).await.unwrap();
    assert!(!s.lib.join("Others").exists());
}

fn page(p: ChangePage) -> (Vec<ChangeItem>, Cursor) {
    match p {
        ChangePage::Page { items, next, more } => {
            assert!(!more);
            (items, next)
        }
        ChangePage::CursorLost => panic!("cursor lost"),
    }
}

fn split(items: &[ChangeItem]) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut up = BTreeSet::new();
    let mut gone = BTreeSet::new();
    for i in items {
        match i {
            ChangeItem::Upsert(n) => {
                up.insert(n.pid.clone());
            }
            ChangeItem::Removed { pid } => {
                gone.insert(pid.clone());
            }
        }
    }
    (up, gone)
}

async fn check_changes(s: &Server) {
    let b = s.connect().await;
    b.locate_roots("mokuro-reader").await.unwrap();
    let c1 = b.start_cursor().await.unwrap();
    assert!(snapshot_path(s.state.path()).is_file());
    let (items, c2) = page(b.changes(&c1).await.unwrap());
    assert!(items.is_empty(), "{items:?}");

    s.out_of_band(|lib| {
        std::fs::write(lib.join("series/vol2.cbz"), b"volume two").unwrap();
        std::fs::write(lib.join("catalog.json"), b"{\"series\":[]}").unwrap();
        std::fs::remove_file(lib.join("Other/a/b.txt")).unwrap();
        std::fs::create_dir(lib.join("Fresh")).unwrap();
    });
    let (items, c3) = page(b.changes(&c2).await.unwrap());
    let (up, gone) = split(&items);
    for p in [
        "series/vol2.cbz",
        "catalog.json",
        "Fresh",
        "series",
        "Other/a",
    ] {
        assert!(up.contains(p), "{p} not upserted: {up:?}");
    }
    assert!(!up.contains("series/vol1.cbz"), "{up:?}");
    assert_eq!(gone, BTreeSet::from(["Other/a/b.txt".to_string()]));
    let vol2 = items
        .iter()
        .find_map(|i| match i {
            ChangeItem::Upsert(n) if n.pid == "series/vol2.cbz" => Some(n),
            _ => None,
        })
        .unwrap();
    assert_eq!(vol2.size, 10);

    // Delivery is at least once: the same cursor again gives the same diff.
    let (again, c3b) = page(b.changes(&c2).await.unwrap());
    assert_eq!(split(&again), (up.clone(), gone.clone()));
    assert_eq!(c3b, c3);
    let (items, c4) = page(b.changes(&c3).await.unwrap());
    assert!(items.is_empty(), "{items:?}");

    // A folder removed with its contents: children before the folder.
    s.out_of_band(|lib| std::fs::remove_dir_all(lib.join("Other")).unwrap());
    let (items, c5) = page(b.changes(&c4).await.unwrap());
    let removed: Vec<&str> = items
        .iter()
        .filter_map(|i| match i {
            ChangeItem::Removed { pid } => Some(pid.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(removed, ["Other/a", "Other"]);

    // Driver writes show up too (as the echoes the feed engine matches).
    let tmp = tempfile::tempdir().unwrap();
    let n = upload(
        &b,
        UploadTarget::New {
            parent: "Fresh",
            name: "v.mokuro",
        },
        tmp.path(),
        b"x",
    )
    .await
    .unwrap();
    let (items, _) = page(b.changes(&c5).await.unwrap());
    assert!(items.contains(&ChangeItem::Upsert(n)), "{items:?}");
}

#[tokio::test]
async fn changes_diff_out_of_band_writes_depth_infinity() {
    let s = Server::start().await;
    check_changes(&s).await;
}

#[tokio::test]
async fn changes_diff_out_of_band_writes_depth_1() {
    let s = Server::start().await;
    s.flags.refuse_infinity.store(true, Ordering::SeqCst);
    check_changes(&s).await;
}

#[tokio::test]
async fn unknown_generations_lose_the_cursor() {
    let s = Server::start().await;
    let b = s.connect().await;
    b.locate_roots("mokuro-reader").await.unwrap();
    assert_eq!(
        b.changes(&Cursor("whatever".into())).await.unwrap(),
        ChangePage::CursorLost
    );
    let c1 = b.start_cursor().await.unwrap();
    let (_, c2) = page(b.changes(&c1).await.unwrap());
    let (_, c3) = page(b.changes(&c2).await.unwrap());
    // c1 is two generations back now.
    assert_eq!(b.changes(&c1).await.unwrap(), ChangePage::CursorLost);
    let (epoch, _) = c3.0.rsplit_once(':').unwrap();
    assert_eq!(
        b.changes(&Cursor(format!("{epoch}:99"))).await.unwrap(),
        ChangePage::CursorLost
    );
    // The snapshot survives a restart of the driver.
    let b2 = s.connect().await;
    assert!(matches!(
        b2.changes(&c3).await.unwrap(),
        ChangePage::Page { .. }
    ));
    // A new feed invalidates every older cursor.
    let c_new = b2.start_cursor().await.unwrap();
    assert_ne!(c_new, c3);
    assert_eq!(b2.changes(&c3).await.unwrap(), ChangePage::CursorLost);
    // So does losing the snapshot.
    std::fs::remove_file(snapshot_path(s.state.path())).unwrap();
    let b3 = s.connect().await;
    assert_eq!(b3.changes(&c_new).await.unwrap(), ChangePage::CursorLost);
}

#[tokio::test]
async fn error_mapping() {
    let s = Server::start().await;
    let b = s.connect().await;
    // 404
    assert_eq!(b.stat("nope").await.unwrap(), None);
    assert_eq!(b.read("nope", None, None).await.err(), Some(BErr::NotFound));
    // 409 (missing parent) on MKCOL
    assert_eq!(
        b.mkdir("a/b/c", "d", &OpCtx::default()).await.err(),
        Some(BErr::NotFound)
    );
    // 412
    let n = b.stat("catalog.json").await.unwrap().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(
        upload(
            &b,
            UploadTarget::Replace {
                pid: &n.pid,
                if_cver: Some("\"not-the-tag\""),
            },
            tmp.path(),
            b"x",
        )
        .await
        .err(),
        Some(BErr::PreconditionFailed)
    );
    assert_eq!(
        b.read(&n.pid, Some("\"not-the-tag\""), None).await.err(),
        Some(BErr::PreconditionFailed)
    );
    // No quota properties on this server.
    assert_eq!(b.quota().await.unwrap(), None);
    let acct = b.account().await.unwrap();
    assert!(acct.id.starts_with("admin@http://127.0.0.1:"), "{acct:?}");
    assert!(acct.id.ends_with("/mokuro-reader/"));
    assert_eq!(
        b.wait_change(&Cursor("x".into()), std::time::Duration::ZERO)
            .await
            .err(),
        Some(BErr::Unsupported)
    );
}

#[tokio::test]
async fn hrefs_outside_the_folder_are_ignored() {
    let s = Server::start().await;
    let b = s.connect().await;
    let xml = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:multistatus xmlns:D="DAV:">
<D:response><D:href>/mokuro-reader/series/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype><D:getetag>"d1"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>/mokuro-reader/series/ok%20one.cbz</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>3</D:getcontentlength><D:getetag>"f1"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>/elsewhere/evil.cbz</D:href><D:propstat><D:prop><D:resourcetype/><D:getetag>"e"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>/mokuro-reader/../etc/passwd</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>/mokuro-reader/series/deeper/x.cbz</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>/mokuro-reader/series/gone.cbz</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response>
</D:multistatus>"#;
    *s.flags.inject.lock() = Some(("/mokuro-reader/series/".into(), xml.into()));
    let mut got = Vec::new();
    b.list_dir("series", &mut |n| got.push(n)).await.unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].pid, "series/ok one.cbz");
    assert_eq!(got[0].version, "\"f1\"");
    assert_eq!(got[0].size, 3);
}

#[tokio::test]
async fn changes_diff_out_of_band_writes_propagating_etags() {
    let s = Server::start().await;
    s.flags.refuse_infinity.store(true, Ordering::SeqCst);
    s.flags.propagate.store(true, Ordering::SeqCst);
    check_changes(&s).await;
}

/// Where folder ETags propagate, a poll lists only the folders on the changed path.
#[tokio::test]
async fn changes_skip_folders_whose_etag_held() {
    let s = Server::start().await;
    s.flags.refuse_infinity.store(true, Ordering::SeqCst);
    s.flags.propagate.store(true, Ordering::SeqCst);
    let b = s.connect().await;
    let p = b.probe();
    assert!(p.nested_propagation && !p.depth_infinity, "{p:?}");
    b.locate_roots("mokuro-reader").await.unwrap();
    let c1 = b.start_cursor().await.unwrap();
    let (_, c2) = page(b.changes(&c1).await.unwrap());

    s.flags.log.lock().clear();
    s.out_of_band(|lib| std::fs::write(lib.join("series/vol2.cbz"), b"two").unwrap());
    let (items, c3) = page(b.changes(&c2).await.unwrap());
    let (up, gone) = split(&items);
    assert_eq!(
        up,
        BTreeSet::from(["series".to_string(), "series/vol2.cbz".to_string()])
    );
    assert!(gone.is_empty());
    let log = s.flags.log.lock().clone();
    assert!(
        log.contains(&"PROPFIND /mokuro-reader/series/ 1".to_string()),
        "{log:?}"
    );
    assert!(!log.iter().any(|l| l.contains("/Other")), "{log:?}");

    // Two levels down: both folders on the path change, and only they are listed.
    s.flags.log.lock().clear();
    s.out_of_band(|lib| {
        std::fs::write(lib.join("Other/a/new.txt"), b"n").unwrap();
        std::fs::remove_file(lib.join("Other/a/b.txt")).unwrap();
    });
    let (items, c4) = page(b.changes(&c3).await.unwrap());
    let (up, gone) = split(&items);
    assert_eq!(
        up,
        BTreeSet::from([
            "Other".to_string(),
            "Other/a".to_string(),
            "Other/a/new.txt".to_string()
        ])
    );
    assert_eq!(gone, BTreeSet::from(["Other/a/b.txt".to_string()]));
    assert!(!s.flags.log.lock().iter().any(|l| l.contains("/series")));
    // The copied subtrees are still in the snapshot: nothing comes back as removed.
    let (items, _) = page(b.changes(&c4).await.unwrap());
    assert!(items.is_empty(), "{items:?}");
}

/// The reader root of this server is virtual: no ETag, and a last-modified of *now*.
/// That must not look like a change on every poll.
#[tokio::test]
async fn a_folder_without_an_etag_is_not_a_change_every_second() {
    let s = Server::start().await;
    let b = s.connect().await;
    let root = b.locate_roots("mokuro-reader").await.unwrap().remove(0);
    let c1 = b.start_cursor().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (items, _) = page(b.changes(&c1).await.unwrap());
    assert!(items.is_empty(), "{items:?}");
    assert_eq!(
        b.locate_roots("mokuro-reader").await.unwrap()[0].version,
        root.version
    );
}
