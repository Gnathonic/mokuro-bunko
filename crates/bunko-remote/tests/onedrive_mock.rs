//! OneDriveBackend against cloudsim's Microsoft Graph v1.0 (`bunko_cloudsim`), with the
//! pre-authenticated download, upload-session and copy-monitor URLs; the simulator the
//! reader and the interop rig use too. Dropping the simulator fails the test if it saw
//! a request it could not accept.
#![cfg(feature = "onedrive")]

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bunko_cloudsim::onedrive::{ROOT, Req};
use bunko_cloudsim::{CloudSim, OneDriveHandle};
use bunko_remote::backend::{
    BErr, Backend, ChangeItem, ChangePage, Cursor, HashKind, OpCtx, RNode, UploadSource,
    UploadTarget, read_to_vec,
};
use bunko_remote::onedrive::api::units;
use bunko_remote::onedrive::quickxor::quickxor_base64;
use bunko_remote::onedrive::{
    FRAGMENT_MULTIPLE, OneDriveBackend, OneDriveConfig, StaticToken, TokenSourceDyn,
};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------- helpers

struct Env {
    mock: OneDriveHandle,
    be: OneDriveBackend,
    sim: CloudSim,
}

fn cfg_for(sim: &CloudSim) -> OneDriveConfig {
    OneDriveConfig {
        api_base: sim.graph_api_base(),
        copy_poll: Duration::from_millis(10),
        ..OneDriveConfig::default()
    }
}

async fn env_with(tune: impl FnOnce(&mut OneDriveConfig)) -> Env {
    let sim = bunko_cloudsim::start().await;
    let mock = sim.onedrive("default");
    let mut cfg = cfg_for(&sim);
    tune(&mut cfg);
    let be = OneDriveBackend::new(Arc::new(StaticToken::new("tok")), cfg).unwrap();
    Env { mock, be, sim }
}

async fn env() -> Env {
    env_with(|_| {}).await
}

struct Src {
    _dir: tempfile::TempDir,
    path: PathBuf,
    size: u64,
    sha256: [u8; 32],
}

fn src(bytes: &[u8]) -> Src {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("src.bin");
    std::fs::write(&path, bytes).unwrap();
    Src {
        _dir: dir,
        path,
        size: bytes.len() as u64,
        sha256: Sha256::digest(bytes).into(),
    }
}

impl Src {
    fn source(&self) -> UploadSource<'_> {
        UploadSource::File {
            path: &self.path,
            size: self.size,
            sha256: self.sha256,
        }
    }
}

fn op() -> OpCtx {
    OpCtx {
        op_id: "op-1".into(),
        resume: None,
    }
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[1] ^ seed)
        .collect()
}

fn name(n: &RNode) -> String {
    String::from_utf8_lossy(&n.name_raw).into_owned()
}

async fn new_file(
    be: &OneDriveBackend,
    parent: &str,
    nm: &str,
    data: &[u8],
) -> Result<RNode, BErr> {
    let s = src(data);
    be.upload(
        UploadTarget::New { parent, name: nm },
        s.source(),
        &mut op(),
    )
    .await
}

async fn drain(be: &OneDriveBackend, mut c: Cursor) -> (Vec<ChangeItem>, Cursor, usize) {
    let mut all = Vec::new();
    let mut pages = 0;
    loop {
        match be.changes(&c).await.unwrap() {
            ChangePage::Page { items, next, more } => {
                pages += 1;
                all.extend(items);
                c = next;
                if !more {
                    return (all, c, pages);
                }
            }
            ChangePage::CursorLost => panic!("cursor lost"),
        }
    }
}

fn upserts(items: &[ChangeItem]) -> HashMap<String, RNode> {
    items
        .iter()
        .filter_map(|i| match i {
            ChangeItem::Upsert(n) => Some((n.pid.clone(), n.clone())),
            _ => None,
        })
        .collect()
}

fn removed(items: &[ChangeItem]) -> HashSet<String> {
    items
        .iter()
        .filter_map(|i| match i {
            ChangeItem::Removed { pid } => Some(pid.clone()),
            _ => None,
        })
        .collect()
}

/// `mokuro-reader/{A/{v1.cbz, v1.mokuro, sub/x.webp}, B/, catalog.json}` and
/// `Documents/secret.txt` outside.
struct Lib {
    root: String,
    a: String,
    b: String,
    sub: String,
    v1: String,
    v1m: String,
    x: String,
    catalog: String,
    docs: String,
    secret: String,
}

fn library(m: &OneDriveHandle) -> Lib {
    let root = m.mkdir(ROOT, "mokuro-reader");
    let a = m.mkdir(&root, "A");
    let v1 = m.put(&a, "v1.cbz", &pattern(3000, 1));
    let v1m = m.put(&a, "v1.mokuro", b"{}");
    let sub = m.mkdir(&a, "sub");
    let x = m.put(&sub, "x.webp", b"webp");
    let b = m.mkdir(&root, "B");
    let catalog = m.put(&root, "catalog.json", b"[]");
    let docs = m.mkdir(ROOT, "Documents");
    let secret = m.put(&docs, "secret.txt", b"private");
    Lib {
        root,
        a,
        b,
        sub,
        v1,
        v1m,
        x,
        catalog,
        docs,
        secret,
    }
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn roots_are_located_and_created() {
    let e = env().await;
    assert_eq!(e.be.locate_roots("mokuro-reader").await.unwrap(), vec![]);
    let r = e.be.create_root("mokuro-reader").await.unwrap();
    assert_eq!(name(&r), "mokuro-reader");
    assert!(r.is_dir);
    assert_eq!(r.parent.as_deref(), Some(ROOT));
    let found = e.be.locate_roots("mokuro-reader").await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].pid, r.pid);
    assert_eq!(e.mock.count("GET", "/me/drive/root:/mokuro-reader"), 2);
    assert_eq!(
        e.be.create_root("mokuro-reader").await,
        Err(BErr::NameConflict)
    );
    // A file at the root path is not a root.
    e.mock.put(ROOT, "notes", b"x");
    assert_eq!(e.be.locate_roots("notes").await.unwrap(), vec![]);
    assert_eq!(e.be.locate_roots("a:b").await, Err(BErr::InvalidName));
}

#[tokio::test]
async fn list_all_walks_children_with_paging() {
    let e = env().await;
    e.mock.lock().page_size = 2;
    let l = library(&e.mock);
    let mut nodes = Vec::new();
    e.be.list_all(std::slice::from_ref(&l.root), &mut |n| nodes.push(n))
        .await
        .unwrap();
    // Parents before children, each once, roots excluded.
    let mut seen: HashSet<String> = HashSet::from([l.root.clone()]);
    for n in &nodes {
        assert!(
            seen.contains(n.parent.as_ref().unwrap()),
            "{} early",
            name(n)
        );
        assert!(seen.insert(n.pid.clone()), "{} twice", name(n));
    }
    let mut names: Vec<String> = nodes.iter().map(name).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "A",
            "B",
            "catalog.json",
            "sub",
            "v1.cbz",
            "v1.mokuro",
            "x.webp"
        ]
    );
    assert!(!seen.contains(&l.secret) && !seen.contains(&l.docs));
    assert!(
        e.mock
            .requests()
            .iter()
            .any(|r| r.query.contains_key("$skiptoken"))
    );
    // Every children listing asks for a page size.
    assert!(
        e.mock
            .requests()
            .iter()
            .filter(|r| r.method == "GET" && r.path.ends_with("/children"))
            .all(|r| r.query.contains_key("$top"))
    );

    // Field mapping.
    let v1 = nodes.iter().find(|n| n.pid == l.v1).unwrap();
    let (etag, ctag, created, modified, data) = {
        let st = e.mock.lock();
        let f = &st.items[&l.v1];
        (
            st.etag(&l.v1),
            st.ctag(&l.v1),
            f.created,
            f.modified,
            f.data.clone(),
        )
    };
    assert_eq!(v1.parent.as_deref(), Some(l.a.as_str()));
    assert_eq!(v1.version, etag);
    assert_eq!(v1.cver, ctag);
    assert_eq!(v1.pin.as_deref(), Some(ctag.as_str()));
    assert_eq!(v1.size, 3000);
    assert_eq!(v1.created_ms, created);
    assert_eq!(v1.base_mtime_ms, modified, "top-level lastModifiedDateTime");
    assert_eq!(v1.hash, Some((HashKind::QuickXor, quickxor_base64(&data))));
    let a = nodes.iter().find(|n| n.pid == l.a).unwrap();
    assert!(a.is_dir);
    assert_eq!(a.cver, e.mock.lock().etag(&l.a), "folders use the eTag");
    assert_eq!(a.pin, None);

    let mut kids = Vec::new();
    e.be.list_dir(&l.root, &mut |n| kids.push(name(&n)))
        .await
        .unwrap();
    kids.sort();
    assert_eq!(kids, ["A", "B", "catalog.json"]);

    // find_named: exact names only, though Graph resolves case-insensitively.
    let (hit, at) = e.be.find_named(&l.a, "v1.cbz").await.unwrap();
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].pid, l.v1);
    assert!(at.0 > 0);
    assert!(e.be.find_named(&l.a, "V1.CBZ").await.unwrap().0.is_empty());
    assert!(e.be.find_named(&l.a, "nope").await.unwrap().0.is_empty());
}

#[tokio::test]
async fn delta_is_filtered_to_the_subtree() {
    let e = env().await;
    let l = library(&e.mock);
    e.be.locate_roots("mokuro-reader").await.unwrap();
    let mut nodes = Vec::new();
    e.be.list_all(std::slice::from_ref(&l.root), &mut |n| nodes.push(n))
        .await
        .unwrap();
    let c0 = e.be.start_cursor().await.unwrap();
    assert!(c0.0.contains("token="));
    e.mock.lock().page_size = 3;

    let newf = e.mock.put(&l.a, "new.cbz", b"new");
    let private = e.mock.put(&l.docs, "private.txt", b"secret");
    e.mock.rewrite(&l.catalog, b"[1]");
    e.mock.rename(&l.b, "B2");
    let c = e.mock.mkdir(&l.root, "C");
    let c1 = e.mock.put(&c, "c1.cbz", b"c1");
    e.mock.delete(&l.v1m);
    // A folder moves out, then its file changes while outside.
    e.mock.mv(&l.sub, &l.docs);
    e.mock.rewrite(&l.x, b"webp2");
    // A folder (with a file) moves in from outside.
    let incoming = e.mock.mkdir(&l.docs, "In");
    let inner = e.mock.put(&incoming, "inner.cbz", b"in");
    e.mock.mv(&incoming, &l.root);

    let (items, c1_cursor, pages) = drain(&e.be, c0).await;
    assert!(pages > 2, "pages: {pages}");
    let up = upserts(&items);
    let gone = removed(&items);
    for pid in [&newf, &l.catalog, &l.b, &c, &c1, &incoming, &l.root] {
        assert!(up.contains_key(pid), "{pid} should be upserted");
    }
    assert_eq!(name(&up[&l.b]), "B2");
    assert_eq!(up[&incoming].parent.as_deref(), Some(l.root.as_str()));
    // Nothing outside ever surfaces as a node.
    for pid in [&private, &l.docs, &l.secret, &l.sub, &l.x] {
        assert!(!up.contains_key(pid), "{pid} must not surface");
    }
    for n in up.values() {
        assert!(!["private.txt", "Documents", "secret.txt", "root"].contains(&name(n).as_str()));
    }
    // Deletes, the move out and the change while outside reach the index as ids.
    for pid in [&l.v1m, &l.sub, &l.x] {
        assert!(gone.contains(pid), "{pid} should be removed");
    }
    // The last upsert of an item wins; deleted items carry no name in the mock.
    assert!(!up.contains_key(&l.v1m));

    // `inner.cbz` was created outside; delta reports the latest state, in which it is
    // inside. A listing of the moved-in folder (the engine's batch-edge rule) finds it.
    assert!(up.contains_key(&inner));
    let mut kids = Vec::new();
    e.be.list_dir(&incoming, &mut |n| kids.push(n.pid))
        .await
        .unwrap();
    assert_eq!(kids, vec![inner.clone()]);
    e.mock.rewrite(&inner, b"in2");
    let (items, c2, _) = drain(&e.be, c1_cursor).await;
    assert!(upserts(&items).contains_key(&inner));

    // A restarted backend knows only the roots: unknown parents are fetched once.
    let be2 = OneDriveBackend::new(Arc::new(StaticToken::new("tok")), cfg_for(&e.sim)).unwrap();
    be2.locate_roots("mokuro-reader").await.unwrap();
    let deep = e.mock.mkdir(&l.a, "deep");
    let new2 = e.mock.put(&deep, "new2.cbz", b"n2");
    let outside2 = e.mock.put(&l.sub, "y.webp", b"y");
    let before = e.mock.requests().len();
    let (items, _, _) = drain(&be2, c2).await;
    let up = upserts(&items);
    assert!(up.contains_key(&new2) && up.contains_key(&deep));
    assert!(!up.contains_key(&outside2));
    let walks: Vec<Req> = e.mock.requests()[before..]
        .iter()
        .filter(|r| r.query.contains_key("$select"))
        .cloned()
        .collect();
    assert!(!walks.is_empty());
    let walked: HashSet<&str> = walks.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(walked.len(), walks.len(), "each folder is fetched once");
}

#[tokio::test]
async fn a_lost_delta_token_is_cursor_lost() {
    let e = env().await;
    let l = library(&e.mock);
    e.be.locate_roots("mokuro-reader").await.unwrap();
    let c = e.be.start_cursor().await.unwrap();
    e.mock.put(&l.a, "n.cbz", b"n");
    e.mock.lock().lost_below = usize::MAX;
    assert_eq!(e.be.changes(&c).await, Ok(ChangePage::CursorLost));
    // Before locate_roots the feed cannot be filtered: refused, not guessed.
    let be2 = OneDriveBackend::new(Arc::new(StaticToken::new("tok")), cfg_for(&e.sim)).unwrap();
    assert!(matches!(be2.changes(&c).await, Err(BErr::Fatal(_))));
    // A cursor pointing anywhere but the API base never gets the token.
    let foreign = Cursor("https://evil.example/v1.0/me/drive/root/delta?token=1".into());
    assert!(matches!(e.be.changes(&foreign).await, Err(BErr::Fatal(_))));
}

#[tokio::test]
async fn downloads_follow_the_redirect_without_the_token() {
    let e = env().await;
    let l = library(&e.mock);
    let data = pattern(3000, 1);
    let got = read_to_vec(e.be.read(&l.v1, None, None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got, data);
    let reqs = e.mock.requests();
    let content = reqs.iter().find(|r| r.path.ends_with("/content")).unwrap();
    assert_eq!(content.auth.as_deref(), Some("Bearer tok"));
    let pre = reqs
        .iter()
        .find(|r| r.path.starts_with("/preauth/"))
        .unwrap();
    assert_eq!(
        pre.auth, None,
        "the pre-authenticated URL never sees the token"
    );
    assert!(!e.mock.lock().preauth_saw_auth);

    let mid = read_to_vec(e.be.read(&l.v1, None, Some((10, Some(19)))).await.unwrap())
        .await
        .unwrap();
    assert_eq!(mid, data[10..20]);
    let tail = read_to_vec(e.be.read(&l.v1, None, Some((2990, None))).await.unwrap())
        .await
        .unwrap();
    assert_eq!(tail, data[2990..]);
    // The download host "may" ignore Range and send 200: sliced locally.
    e.mock.lock().ignore_range = true;
    let mid = read_to_vec(
        e.be.read(&l.v1, None, Some((100, Some(1099))))
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(mid, data[100..1100]);
    // The provider hash checks a whole download.
    let n = e.be.stat(&l.v1).await.unwrap().unwrap();
    assert_eq!(n.hash.unwrap().1, quickxor_base64(&got));
    assert_eq!(
        e.be.read("missing", None, None).await.err(),
        Some(BErr::NotFound)
    );
}

#[tokio::test]
async fn pinned_reads_compare_the_ctag() {
    let e = env().await;
    let l = library(&e.mock);
    let n = e.be.stat(&l.v1).await.unwrap().unwrap();
    let pin = n.pin.clone().unwrap();
    let got = read_to_vec(e.be.read(&l.v1, Some(&pin), None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got, pattern(3000, 1));
    e.mock.rewrite(&l.v1, b"changed");
    assert_eq!(
        e.be.read(&l.v1, Some(&pin), Some((0, Some(3)))).await.err(),
        Some(BErr::PreconditionFailed)
    );

    // Without a CTag header (the ETag is the eTag): re-stat after the last byte.
    e.mock.lock().send_ctag = false;
    let cur = e.be.stat(&l.v1).await.unwrap().unwrap();
    let got = read_to_vec(e.be.read(&l.v1, cur.pin.as_deref(), None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got, b"changed");
    let mut body = e.be.read(&l.v1, Some(&pin), None).await.unwrap();
    let mut last = None;
    while let Some(chunk) = body.next().await {
        last = Some(chunk);
    }
    let e2 = last.unwrap().unwrap_err();
    assert_eq!(
        e2.get_ref().unwrap().downcast_ref::<BErr>(),
        Some(&BErr::PreconditionFailed)
    );
}

/// Personal OneDrive's download host sends `CTag: {id},N,1`, a form the JSON `cTag`
/// (`"c:{id},N"`) never matches: the read is re-checked after the last byte instead of
/// refused, and a changed file still ends in `PreconditionFailed`.
#[tokio::test]
async fn pinned_reads_take_the_storage_hosts_ctag_form() {
    let e = env().await;
    let l = library(&e.mock);
    e.mock.lock().storage_ctag = true;
    let pin = e.be.stat(&l.v1).await.unwrap().unwrap().pin.unwrap();
    let got = read_to_vec(e.be.read(&l.v1, Some(&pin), None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got, pattern(3000, 1));
    e.mock.rewrite(&l.v1, b"changed");
    let mut body = e.be.read(&l.v1, Some(&pin), None).await.unwrap();
    let mut last = None;
    while let Some(chunk) = body.next().await {
        last = Some(chunk);
    }
    let err = last.unwrap().unwrap_err();
    assert_eq!(
        err.get_ref().unwrap().downcast_ref::<BErr>(),
        Some(&BErr::PreconditionFailed)
    );
}

#[tokio::test]
async fn small_creates_are_exclusive() {
    let e = env().await;
    let l = library(&e.mock);
    let n = new_file(&e.be, &l.a, "v2.mokuro", b"{\"v\":2}")
        .await
        .unwrap();
    assert_eq!(name(&n), "v2.mokuro");
    assert_eq!(n.parent.as_deref(), Some(l.a.as_str()));
    assert_eq!(e.mock.data(&n.pid), b"{\"v\":2}");
    assert_eq!(
        n.hash,
        Some((HashKind::QuickXor, quickxor_base64(b"{\"v\":2}")))
    );
    let put = e
        .mock
        .requests()
        .into_iter()
        .find(|r| r.method == "PUT")
        .unwrap();
    assert!(put.path.ends_with("/v2.mokuro:/content"), "{}", put.path);
    assert_eq!(
        put.query
            .get("@microsoft.graph.conflictBehavior")
            .map(String::as_str),
        Some("fail")
    );
    // Different bytes under the name: a conflict, nothing overwritten.
    assert_eq!(
        new_file(&e.be, &l.a, "v2.mokuro", b"other").await,
        Err(BErr::NameConflict)
    );
    // Names fold: a case variant is the same name on OneDrive.
    assert_eq!(
        new_file(&e.be, &l.a, "V2.MOKURO", b"other").await,
        Err(BErr::NameConflict)
    );
    assert_eq!(e.mock.data(&n.pid), b"{\"v\":2}");
    // A retry of a create that already landed returns it.
    let again = new_file(&e.be, &l.a, "v2.mokuro", b"{\"v\":2}")
        .await
        .unwrap();
    assert_eq!(again.pid, n.pid);
    // Empty files go up in one PUT too.
    let empty = new_file(&e.be, &l.a, "empty", b"").await.unwrap();
    assert_eq!(empty.size, 0);
}

#[tokio::test]
async fn large_uploads_resume_after_an_interrupted_fragment() {
    let e = env_with(|c| c.fragment_size = FRAGMENT_MULTIPLE + 1000).await;
    let l = library(&e.mock);
    let data = pattern((5 << 20) + 12_345, 7);
    e.mock.lock().interrupt_fragment = Some(3);
    let n = new_file(&e.be, &l.a, "big.cbz", &data).await.unwrap();
    assert_eq!(e.mock.data(&n.pid), data);
    assert_eq!(n.size, data.len() as u64);
    assert_eq!(e.mock.count("POST", "createUploadSession"), 1);
    // The interrupted fragment was followed by a status query and a resume from the
    // byte the session asked for.
    assert!(e.mock.count("GET", "/upload/") >= 1);
    let puts: Vec<Req> = e
        .mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PUT" && r.path.starts_with("/upload/"))
        .collect();
    assert!(puts.iter().all(|r| r.auth.is_none()));
    assert!(puts.len() >= 17);
    // The fragment after the interrupt starts where the session said (half-way into
    // the interrupted one), not at a fragment boundary.
    let starts: Vec<u64> = puts
        .iter()
        .map(|r| {
            let cr = r.content_range.as_deref().unwrap();
            cr["bytes ".len()..]
                .split('-')
                .next()
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    assert_eq!(starts[3], 2 * FRAGMENT_MULTIPLE + FRAGMENT_MULTIPLE / 2);
    assert!(!e.mock.lock().preauth_saw_auth);

    // The service stays down after the interrupt: the call fails with the session kept,
    // and the next call (a restart) resumes it instead of starting over.
    {
        let mut st = e.mock.lock();
        st.interrupt_fragment = Some(st.fragment_puts + 2);
        st.down_after_interrupt = true;
    }
    let s = src(&data);
    let mut ctx = op();
    let r =
        e.be.upload(
            UploadTarget::New {
                parent: &l.a,
                name: "big2.cbz",
            },
            s.source(),
            &mut ctx,
        )
        .await;
    assert!(
        matches!(r, Err(BErr::Transient(_)) | Err(BErr::Throttled(_))),
        "{r:?}"
    );
    assert!(
        ctx.resume
            .as_deref()
            .is_some_and(|r| r.contains("/upload/"))
    );
    {
        let mut st = e.mock.lock();
        st.session_down = false;
        st.down_after_interrupt = false;
    }
    let n2 =
        e.be.upload(
            UploadTarget::New {
                parent: &l.a,
                name: "big2.cbz",
            },
            s.source(),
            &mut ctx,
        )
        .await
        .unwrap();
    assert_eq!(e.mock.data(&n2.pid), data);
    assert_eq!(e.mock.count("POST", "createUploadSession"), 2);
    assert_eq!(ctx.resume, None);
}

#[tokio::test]
async fn replaces_use_a_session_with_if_match() {
    let e = env().await;
    let l = library(&e.mock);
    let old = e.be.stat(&l.catalog).await.unwrap().unwrap();
    let s = src(b"[\"new\"]");
    let n =
        e.be.upload(
            UploadTarget::Replace {
                pid: &l.catalog,
                if_cver: Some(&old.cver),
            },
            s.source(),
            &mut op(),
        )
        .await
        .unwrap();
    assert_eq!(n.pid, l.catalog, "the id survives a replace");
    assert_ne!(n.cver, old.cver);
    assert_eq!(e.mock.data(&l.catalog), b"[\"new\"]");
    let sess = e
        .mock
        .requests()
        .into_iter()
        .find(|r| r.path.ends_with("/createUploadSession"))
        .unwrap();
    assert_eq!(sess.if_match.as_deref(), Some(old.cver.as_str()));
    assert_eq!(e.mock.count("PUT", "/upload/"), 1, "one fragment");
    assert_eq!(e.mock.count("PUT", "/content"), 0, "never a simple PUT");

    // A stale base: 412, nothing written.
    let s2 = src(b"[\"stale\"]");
    assert_eq!(
        e.be.upload(
            UploadTarget::Replace {
                pid: &l.catalog,
                if_cver: Some(&old.cver),
            },
            s2.source(),
            &mut op(),
        )
        .await,
        Err(BErr::PreconditionFailed)
    );
    assert_eq!(e.mock.data(&l.catalog), b"[\"new\"]");
    // The same bytes again under the old base: an earlier attempt that landed.
    let again =
        e.be.upload(
            UploadTarget::Replace {
                pid: &l.catalog,
                if_cver: Some(&old.cver),
            },
            s.source(),
            &mut op(),
        )
        .await
        .unwrap();
    assert_eq!(again.cver, n.cver);
    // Unconditional and empty replaces.
    e.be.upload(
        UploadTarget::Replace {
            pid: &l.catalog,
            if_cver: None,
        },
        s2.source(),
        &mut op(),
    )
    .await
    .unwrap();
    assert_eq!(e.mock.data(&l.catalog), b"[\"stale\"]");
    let cur = e.be.stat(&l.catalog).await.unwrap().unwrap();
    let empty = src(b"");
    let n =
        e.be.upload(
            UploadTarget::Replace {
                pid: &l.catalog,
                if_cver: Some(&cur.cver),
            },
            empty.source(),
            &mut op(),
        )
        .await
        .unwrap();
    assert_eq!(n.size, 0);
    assert_eq!(
        e.be.upload(
            UploadTarget::Replace {
                pid: &l.catalog,
                if_cver: Some(&cur.cver),
            },
            empty.source(),
            &mut op(),
        )
        .await,
        Err(BErr::PreconditionFailed)
    );
}

#[tokio::test]
async fn folders_rename_and_move_by_patch() {
    let e = env().await;
    let l = library(&e.mock);
    let d = e.be.mkdir(&l.root, "D", &op()).await.unwrap();
    assert!(d.is_dir);
    assert_eq!(
        e.be.mkdir(&l.root, "d", &op()).await,
        Err(BErr::NameConflict)
    );

    let cur = e.be.stat(&l.v1).await.unwrap().unwrap();
    let n =
        e.be.update(&l.v1, None, Some("Vol 1.cbz"), Some(&cur.cver))
            .await
            .unwrap();
    assert_eq!(name(&n), "Vol 1.cbz");
    assert_eq!(n.pid, l.v1);
    let patch = e
        .mock
        .requests()
        .into_iter()
        .find(|r| r.method == "PATCH")
        .unwrap();
    assert_eq!(patch.if_match.as_deref(), Some(cur.cver.as_str()));
    // Case-only rename by id.
    let n =
        e.be.update(&l.v1, None, Some("VOL 1.cbz"), None)
            .await
            .unwrap();
    assert_eq!(name(&n), "VOL 1.cbz");
    // Move.
    let n = e.be.update(&l.v1, Some(&d.pid), None, None).await.unwrap();
    assert_eq!(n.parent.as_deref(), Some(d.pid.as_str()));
    // Onto an existing name: conflict.
    e.mock.put(&l.a, "taken.cbz", b"t");
    assert_eq!(
        e.be.update(&l.v1, Some(&l.a), Some("taken.cbz"), None)
            .await,
        Err(BErr::NameConflict)
    );
    // A stale If-Match.
    e.mock.rewrite(&l.catalog, b"[2]");
    let stale = "\"c:{stale},1\"";
    assert_eq!(
        e.be.update(&l.catalog, None, Some("c2.json"), Some(stale))
            .await,
        Err(BErr::PreconditionFailed)
    );
    assert_eq!(
        e.be.update("missing", None, Some("x"), None).await,
        Err(BErr::NotFound)
    );
}

#[tokio::test]
async fn copy_waits_for_the_monitor() {
    let e = env().await;
    let l = library(&e.mock);
    let n = e.be.copy(&l.v1, &l.b, "copy.cbz").await.unwrap();
    assert_ne!(n.pid, l.v1);
    assert_eq!(name(&n), "copy.cbz");
    assert_eq!(n.parent.as_deref(), Some(l.b.as_str()));
    assert_eq!(e.mock.data(&n.pid), pattern(3000, 1));
    let polls: Vec<Req> = e
        .mock
        .requests()
        .into_iter()
        .filter(|r| r.path.starts_with("/monitor/"))
        .collect();
    assert_eq!(polls.len(), 2);
    assert!(polls.iter().all(|r| r.auth.is_none()));
    assert_eq!(
        e.be.copy(&l.v1, &l.b, "copy.cbz").await,
        Err(BErr::NameConflict)
    );
}

#[tokio::test]
async fn trash_and_permanent_delete() {
    let e = env().await;
    let l = library(&e.mock);
    let cur = e.be.stat(&l.v1).await.unwrap().unwrap();
    e.mock.rewrite(&l.v1, b"newer");
    assert_eq!(
        e.be.trash(&l.v1, Some(&cur.cver)).await,
        Err(BErr::PreconditionFailed)
    );
    let cur = e.be.stat(&l.v1).await.unwrap().unwrap();
    e.be.trash(&l.v1, Some(&cur.cver)).await.unwrap();
    assert_eq!(e.be.stat(&l.v1).await, Ok(None));
    let del = e
        .mock
        .requests()
        .into_iter()
        .rfind(|r| r.method == "DELETE")
        .unwrap();
    assert_eq!(del.if_match.as_deref(), Some(cur.cver.as_str()));
    assert_eq!(e.be.trash(&l.v1, None).await, Err(BErr::NotFound));

    e.be.delete_permanent(&l.v1m).await.unwrap();
    assert_eq!(e.be.stat(&l.v1m).await, Ok(None));
    assert_eq!(e.mock.count("POST", "permanentDelete"), 1);

    // OneDrive Personal: permanentDelete is refused; fall back to DELETE, and remember.
    e.mock.lock().permanent_delete = false;
    let deletes = e.mock.count("DELETE", "/me/drive/items/");
    e.be.delete_permanent(&l.x).await.unwrap();
    e.be.delete_permanent(&l.catalog).await.unwrap();
    assert_eq!(e.mock.count("POST", "permanentDelete"), 2);
    assert_eq!(e.mock.count("DELETE", "/me/drive/items/"), deletes + 2);
    assert_eq!(e.be.stat(&l.catalog).await, Ok(None));
}

#[tokio::test]
async fn forbidden_names_are_refused_before_any_request() {
    let e = env().await;
    let l = library(&e.mock);
    let before = e.mock.requests().len();
    for bad in [
        "a\"b", "a*b", "a:b", "a<b", "a>b", "a?b", "a/b", "a\\b", "a|b", " lead", "trail ", "",
    ] {
        assert_eq!(
            new_file(&e.be, &l.a, bad, b"x").await,
            Err(BErr::InvalidName),
            "{bad:?}"
        );
        assert_eq!(e.be.mkdir(&l.a, bad, &op()).await, Err(BErr::InvalidName));
        assert_eq!(
            e.be.update(&l.v1, None, Some(bad), None).await,
            Err(BErr::InvalidName)
        );
        assert_eq!(e.be.copy(&l.v1, &l.a, bad).await, Err(BErr::InvalidName));
        assert_eq!(
            e.be.find_named(&l.a, bad).await.err(),
            Some(BErr::InvalidName)
        );
    }
    assert_eq!(e.mock.requests().len(), before, "no request was sent");
}

/// Hands out `tok-0`, `tok-1`, … advancing on `invalidate`.
struct Rotating {
    n: AtomicUsize,
    invalidated: AtomicUsize,
}

impl TokenSourceDyn for Rotating {
    fn access_token(&self) -> Pin<Box<dyn Future<Output = Result<String, BErr>> + Send + '_>> {
        Box::pin(async move { Ok(format!("tok-{}", self.n.load(Ordering::SeqCst))) })
    }
    fn invalidate(&self) {
        self.n.fetch_add(1, Ordering::SeqCst);
        self.invalidated.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_401_refreshes_the_token_once() {
    let sim = bunko_cloudsim::start().await;
    let mock = sim.onedrive("default");
    let r = mock.mkdir(ROOT, "mokuro-reader");
    mock.lock().valid_token = "tok-1".into();
    let tokens = Arc::new(Rotating {
        n: AtomicUsize::new(0),
        invalidated: AtomicUsize::new(0),
    });
    let be = OneDriveBackend::new(tokens.clone(), cfg_for(&sim)).unwrap();
    assert!(be.stat(&r).await.unwrap().is_some());
    assert_eq!(tokens.invalidated.load(Ordering::SeqCst), 1);
    assert!(be.stat(&r).await.unwrap().is_some());
    assert_eq!(tokens.invalidated.load(Ordering::SeqCst), 1);
    assert_eq!(be.units_used(), 3 * units::GET);
    mock.lock().valid_token = "something-else".into();
    assert_eq!(be.stat(&r).await, Err(BErr::AuthExpired));
    assert_eq!(tokens.invalidated.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn errors_map_to_backend_errors() {
    let e = env().await;
    let l = library(&e.mock);
    let path = format!("/me/drive/items/{}", l.v1);

    e.mock
        .fault_ra(None, &path, 429, "activityLimitReached", Some(7));
    assert_eq!(
        e.be.stat(&l.v1).await,
        Err(BErr::Throttled(Some(Duration::from_secs(7))))
    );
    e.mock
        .fault_ra(None, &path, 503, "serviceNotAvailable", Some(3));
    assert_eq!(
        e.be.stat(&l.v1).await,
        Err(BErr::Throttled(Some(Duration::from_secs(3))))
    );
    e.mock.fault(None, &path, 429, "activityLimitReached");
    assert!(matches!(
        e.be.stat(&l.v1).await,
        Err(BErr::Throttled(Some(_)))
    ));
    e.mock.fault(None, &path, 503, "serviceNotAvailable");
    assert!(matches!(e.be.stat(&l.v1).await, Err(BErr::Transient(_))));
    e.mock.fault(None, &path, 500, "generalException");
    assert!(matches!(e.be.stat(&l.v1).await, Err(BErr::Transient(_))));
    e.mock.fault(None, &path, 403, "accessDenied");
    assert_eq!(e.be.stat(&l.v1).await, Err(BErr::AuthRevoked));
    e.mock.fault(None, &path, 423, "resourceLocked");
    assert!(matches!(
        e.be.update(&l.v1, None, Some("z"), None).await,
        Err(BErr::Transient(_))
    ));
    e.mock.fault(None, &path, 412, "preconditionFailed");
    assert_eq!(e.be.trash(&l.v1, None).await, Err(BErr::PreconditionFailed));
    e.mock
        .fault(None, ":/v2.cbz:/content", 507, "insufficientStorage");
    assert_eq!(
        new_file(&e.be, &l.a, "v2.cbz", b"x").await,
        Err(BErr::StorageFull)
    );
    e.mock
        .fault(None, ":/v2.cbz:/content", 413, "maxFileSizeExceeded");
    assert_eq!(
        new_file(&e.be, &l.a, "v2.cbz", b"x").await,
        Err(BErr::TooLarge)
    );
    assert_eq!(e.be.stat("missing").await, Ok(None));
    // Everything still works afterwards.
    assert!(e.be.stat(&l.v1).await.unwrap().is_some());
}

#[tokio::test]
async fn units_user_agent_quota_and_account() {
    let e = env().await;
    let l = library(&e.mock);
    let u0 = e.be.units_used();
    e.be.stat(&l.v1).await.unwrap();
    assert_eq!(e.be.units_used() - u0, units::GET);
    let u0 = e.be.units_used();
    e.be.list_dir(&l.root, &mut |_| {}).await.unwrap();
    assert_eq!(e.be.units_used() - u0, units::LIST);
    let u0 = e.be.units_used();
    read_to_vec(e.be.read(&l.v1, None, None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(e.be.units_used() - u0, units::DOWNLOAD);
    let u0 = e.be.units_used();
    e.be.update(&l.v1, None, Some("w.cbz"), None).await.unwrap();
    assert_eq!(e.be.units_used() - u0, units::UPDATE);
    e.be.locate_roots("mokuro-reader").await.unwrap();
    let u0 = e.be.units_used();
    let c = e.be.start_cursor().await.unwrap();
    e.be.changes(&c).await.unwrap();
    assert_eq!(e.be.units_used() - u0, 2 * units::DELTA_TOKEN);

    let ua = format!("ISV|Mokuro|Bunko/{}", env!("CARGO_PKG_VERSION"));
    for r in e.mock.requests() {
        assert_eq!(
            r.user_agent.as_deref(),
            Some(ua.as_str()),
            "{} {}",
            r.method,
            r.path
        );
    }

    let q = e.be.quota().await.unwrap().unwrap();
    assert_eq!(q.total, 1 << 30);
    assert!(q.available < q.total);
    let acct = e.be.account().await.unwrap();
    assert_eq!(acct.id, "user-1");
    assert_eq!(acct.label, "me@example.com");
    let caps = e.be.caps();
    assert!(caps.cas_create && caps.cas_overwrite && caps.atomic_replace && caps.pinned_read);
    assert_eq!(caps.hash_kind, Some(HashKind::QuickXor));
    assert_eq!(caps.mtime_granularity_ns, 1_000_000_000);
    assert_eq!(caps.chunk_multiple, 320 << 10);
    assert_eq!(
        e.be.wait_change(&c, Duration::from_millis(1)).await,
        Err(BErr::Unsupported)
    );
}
