//! MegaBackend against cloudsim's MEGA (the `bunko_mega::mock` hub mounted under
//! `/mega/`): real node and file crypto, the `cs` commands, `sc` with a wait URL,
//! transfer URLs. Dropping the simulator fails the test if it saw a request it could
//! not accept.
#![cfg(feature = "mega")]

use std::time::Duration;

use bunko_cloudsim::CloudSim;
use bunko_cloudsim::mega::{MegaHandle, ROOT, RUBBISH};
use bunko_mega::ClientConfig;
use bunko_remote::backend::{
    BErr, Backend, ChangeItem, ChangePage, Cursor, OpCtx, RNode, UploadSource, UploadTarget,
    read_to_vec,
};
use bunko_remote::mega::{MegaBackend, MegaConfig, SessionBlob};
use sha2::{Digest, Sha256};

struct Env {
    mock: MegaHandle,
    _sim: CloudSim,
    be: MegaBackend,
    tmp: tempfile::TempDir,
    root: String,
    series: String,
    vol: String,
    sidecar: String,
    catalog: String,
    outside: String,
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 13 % 251) as u8).collect()
}

fn cfg(mock: &MegaHandle, dir: &std::path::Path) -> MegaConfig {
    let mut c = MegaConfig::new(dir);
    c.client = ClientConfig {
        api_origin: mock.base.clone(),
        min_cs_interval: Duration::ZERO,
        retry_base: Duration::from_millis(1),
        retry_max: Duration::from_millis(5),
        https_transfers: false,
        ..ClientConfig::default()
    };
    c.upload_post_max = 256 << 10;
    c
}

async fn env() -> Env {
    let sim = bunko_cloudsim::start().await;
    let mock = sim.mega("default");
    let root = mock.mkdir(ROOT, "mokuro-reader");
    let series = mock.mkdir(&root, "Series A");
    let vol = mock.put(&series, "Vol 1.cbz", &pattern(300_007));
    let sidecar = mock.put(&series, "Vol 1.mokuro", b"{\"pages\":[]}");
    let catalog = mock.put(&root, "catalog.json", b"{}");
    let other = mock.mkdir(ROOT, "Private");
    let outside = mock.put(&other, "taxes.pdf", b"not for bunko");
    // Not roots: a case variant, and one in the rubbish bin.
    mock.mkdir(ROOT, "Mokuro-Reader");
    mock.mkdir(RUBBISH, "mokuro-reader");
    let tmp = tempfile::tempdir().unwrap();
    let be = MegaBackend::resume(
        &SessionBlob::new(mock.session_blob()),
        cfg(&mock, tmp.path()),
    )
    .await
    .unwrap();
    Env {
        mock,
        _sim: sim,
        be,
        tmp,
        root,
        series,
        vol,
        sidecar,
        catalog,
        outside,
    }
}

async fn all(be: &MegaBackend, roots: &[String]) -> Vec<RNode> {
    let mut out = Vec::new();
    be.list_all(roots, &mut |n| out.push(n)).await.unwrap();
    out
}

fn names(nodes: &[RNode]) -> Vec<String> {
    let mut v: Vec<String> = nodes
        .iter()
        .map(|n| String::from_utf8_lossy(&n.name_raw).into_owned())
        .collect();
    v.sort();
    v
}

async fn drain(be: &MegaBackend, c: &mut Cursor) -> Vec<ChangeItem> {
    let mut items = Vec::new();
    for _ in 0..20 {
        match be.changes(c).await.unwrap() {
            ChangePage::Page {
                items: i,
                next,
                more,
            } => {
                items.extend(i);
                *c = next;
                if !more {
                    return items;
                }
            }
            ChangePage::CursorLost => panic!("cursor lost"),
        }
    }
    panic!("feed did not settle")
}

fn src(path: &std::path::Path) -> (u64, [u8; 32]) {
    let data = std::fs::read(path).unwrap();
    (data.len() as u64, Sha256::digest(&data).into())
}

#[tokio::test]
async fn locates_exact_roots_and_lists_only_the_subtree() {
    let e = env().await;
    let roots = e.be.locate_roots("mokuro-reader").await.unwrap();
    assert_eq!(roots.len(), 1, "{roots:?}");
    assert_eq!(roots[0].pid, e.root);
    assert_eq!(roots[0].parent.as_deref(), Some(ROOT));
    let c = e.be.start_cursor().await.unwrap();
    assert_eq!(c.0, e.mock.sn());
    let listed = all(&e.be, std::slice::from_ref(&e.root)).await;
    assert_eq!(
        names(&listed),
        vec!["Series A", "Vol 1.cbz", "Vol 1.mokuro", "catalog.json"]
    );
    assert!(listed.iter().all(|n| n.pid != e.outside));
    let vol = listed.iter().find(|n| n.pid == e.vol).unwrap();
    assert_eq!(vol.size, 300_007);
    assert_eq!(vol.cver, e.vol);
    assert_eq!(vol.pin.as_deref(), Some(e.vol.as_str()));
    assert_eq!(vol.hash, None);
    let ts = e.mock.node(&e.vol).unwrap().ts;
    assert_eq!(vol.base_mtime_ms, ts * 1000);
    assert_eq!(vol.created_ms, ts * 1000);
    assert_eq!(vol.version, format!("{}:{ts}", e.vol));
    // Parents come before children.
    let pos = |pid: &str| listed.iter().position(|n| n.pid == pid).unwrap();
    assert!(pos(&e.series) < pos(&e.vol));
    // One tree fetch served locate, cursor and listing.
    assert_eq!(
        e.mock.calls().iter().filter(|c| c.as_str() == "f").count(),
        1
    );
    let mut kids = Vec::new();
    e.be.list_dir(&e.series, &mut |n| kids.push(n))
        .await
        .unwrap();
    assert_eq!(names(&kids), vec!["Vol 1.cbz", "Vol 1.mokuro"]);
    let (found, t) = e.be.find_named(&e.series, "Vol 1.mokuro").await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].pid, e.sidecar);
    assert!(t.0 > 1_600_000_000_000);
    assert!(
        e.be.find_named(&e.series, "vol 1.mokuro")
            .await
            .unwrap()
            .0
            .is_empty()
    );
    assert_eq!(e.be.stat(&e.vol).await.unwrap().unwrap().pid, e.vol);
    assert_eq!(e.be.stat("NOSUCHH1").await.unwrap(), None);
    let acct = e.be.account().await.unwrap();
    assert_eq!(acct.id, bunko_mega::mock::USER);
    let q = e.be.quota().await.unwrap().unwrap();
    assert_eq!(q.total, 20 << 30);
    assert!(e.be.caps().push && e.be.caps().pinned_read && !e.be.caps().server_copy);
    drop(e.tmp);
}

#[tokio::test]
async fn ranged_and_pinned_reads() {
    let e = env().await;
    e.be.locate_roots("mokuro-reader").await.unwrap();
    let data = pattern(300_007);
    let full = read_to_vec(e.be.read(&e.vol, Some(&e.vol), None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(full, data);
    for (a, b) in [
        (17u64, Some(4095u64)),
        (131_071, Some(131_100)),
        (299_990, None),
    ] {
        let got = read_to_vec(e.be.read(&e.vol, Some(&e.vol), Some((a, b))).await.unwrap())
            .await
            .unwrap();
        let end = b.map_or(data.len(), |b| b as usize + 1);
        assert_eq!(got, &data[a as usize..end], "{a}-{b:?}");
    }
    assert!(e.mock.download_ranges().contains(&"16-4095".to_string()));
    assert_eq!(
        e.be.read(&e.vol, Some("OTHERVER"), None).await.err(),
        Some(BErr::PreconditionFailed)
    );
    assert_eq!(
        e.be.read(&e.series, None, None).await.err(),
        Some(BErr::NotFound)
    );
    // Transfer quota: `-17` from `g`, HTTP 509, and a short error body.
    e.mock.fail_command("g", -17);
    assert_eq!(
        e.be.read(&e.vol, None, None).await.err(),
        Some(BErr::TransferQuota(Some(Duration::from_secs(1800))))
    );
    e.mock.overquota(true);
    assert_eq!(
        e.be.read(&e.vol, None, None).await.err(),
        Some(BErr::TransferQuota(Some(Duration::from_secs(600))))
    );
    e.mock.overquota(false);
    e.mock.bandwidth_509(true);
    assert!(matches!(
        e.be.read(&e.vol, None, None).await.err(),
        Some(BErr::TransferQuota(Some(_)))
    ));
    e.mock.bandwidth_509(false);
    e.mock.garbage_download(true);
    assert!(matches!(
        e.be.read(&e.vol, None, None).await.err(),
        Some(BErr::Transient(_))
    ));
}

#[tokio::test]
async fn feed_reports_reader_changes() {
    let e = env().await;
    e.be.locate_roots("mokuro-reader").await.unwrap();
    let mut c = e.be.start_cursor().await.unwrap();
    all(&e.be, std::slice::from_ref(&e.root)).await;
    assert!(drain(&e.be, &mut c).await.is_empty());

    // Rename, a new series with a move into it, a new file, a delete.
    e.mock.rename(&e.sidecar, "Vol 1.edit.mokuro");
    let b = e.mock.mkdir(&e.root, "Series B");
    e.mock.move_to(&e.vol, &b);
    let new = e.mock.put(&b, "Vol 2.cbz", b"two");
    e.mock.delete(&e.catalog);
    let items = drain(&e.be, &mut c).await;
    let upsert = |pid: &str| {
        items.iter().rev().find_map(|i| match i {
            ChangeItem::Upsert(n) if n.pid == pid => Some(n.clone()),
            _ => None,
        })
    };
    assert_eq!(upsert(&e.sidecar).unwrap().name_raw, b"Vol 1.edit.mokuro");
    assert_eq!(upsert(&b).unwrap().name_raw, b"Series B");
    assert_eq!(upsert(&e.vol).unwrap().parent.as_deref(), Some(b.as_str()));
    assert_eq!(upsert(&new).unwrap().size, 3);
    assert!(items.contains(&ChangeItem::Removed {
        pid: e.catalog.clone()
    }));
    // The move is one upsert, not a removal.
    assert!(!items.contains(&ChangeItem::Removed { pid: e.vol.clone() }));

    // A folder moved out of the library: it and its children go, children first.
    let private = e.mock.find(ROOT, "Private")[0].clone();
    e.mock.move_to(&b, &private);
    let items = drain(&e.be, &mut c).await;
    let removed: Vec<&str> = items
        .iter()
        .filter_map(|i| match i {
            ChangeItem::Removed { pid } => Some(pid.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(removed.len(), 3, "{items:?}");
    assert_eq!(removed.last(), Some(&b.as_str()));
    // Nothing outside the library is reported.
    e.mock.rename(&e.outside, "taxes-2026.pdf");
    assert!(drain(&e.be, &mut c).await.is_empty());

    // stat drains the mirror ahead of the store's cursor; the feed still reports.
    let late = e.mock.put(&e.series, "Vol 3.cbz", b"three");
    assert_eq!(e.be.stat(&late).await.unwrap().unwrap().size, 5);
    let items = drain(&e.be, &mut c).await;
    assert!(
        items
            .iter()
            .any(|i| matches!(i, ChangeItem::Upsert(n) if n.pid == late))
    );

    // An unmodelled packet re-lists the subtree.
    e.mock
        .push_packet(serde_json::json!({"a": "zz-new", "n": "x"}));
    let items = drain(&e.be, &mut c).await;
    assert!(
        items
            .iter()
            .any(|i| matches!(i, ChangeItem::Upsert(n) if n.pid == e.series))
    );
    assert_eq!(c.0, e.mock.sn());

    // A stale sn is CursorLost.
    e.mock.lose_cursor();
    let old = Cursor("0".into());
    assert_eq!(e.be.changes(&old).await.unwrap(), ChangePage::CursorLost);
}

#[tokio::test]
async fn wait_change_long_polls() {
    let e = env().await;
    e.be.locate_roots("mokuro-reader").await.unwrap();
    let c = e.be.start_cursor().await.unwrap();
    let be = e.be.clone();
    let c2 = c.clone();
    let waiter = tokio::spawn(async move {
        let t = std::time::Instant::now();
        be.wait_change(&c2, Duration::from_secs(10)).await.unwrap();
        t.elapsed()
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    e.mock.put(&e.root, "profiles.json", b"{}");
    let took = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap();
    assert!(took < Duration::from_secs(5), "{took:?}");
    // With nothing new it returns after `max`.
    let c = e.be.start_cursor().await.unwrap();
    let mut c = c;
    drain(&e.be, &mut c).await;
    let t = std::time::Instant::now();
    e.be.wait_change(&c, Duration::from_millis(300))
        .await
        .unwrap();
    assert!(t.elapsed() >= Duration::from_millis(250));
}

#[tokio::test]
async fn upload_new_and_replace_make_new_nodes() {
    let e = env().await;
    e.be.locate_roots("mokuro-reader").await.unwrap();
    let path = e.tmp.path().join("Vol 2.mokuro");
    std::fs::write(&path, b"{\"v\":2}").unwrap();
    let (size, sha256) = src(&path);
    let mut op = OpCtx::default();
    let n =
        e.be.upload(
            UploadTarget::New {
                parent: &e.series,
                name: "Vol 2.mokuro",
            },
            UploadSource::File {
                path: &path,
                size,
                sha256,
            },
            &mut op,
        )
        .await
        .unwrap();
    assert_eq!(n.name_raw, b"Vol 2.mokuro");
    assert_eq!(n.parent.as_deref(), Some(e.series.as_str()));
    assert_eq!(op.resume, None);
    assert_eq!(
        e.mock.content(&n.pid).unwrap(),
        (b"{\"v\":2}".to_vec(), true)
    );

    // Replace: a new node under the same name; the old one stays for SUPERSEDE_OLD.
    std::fs::write(&path, b"{\"v\":3}").unwrap();
    let (size, sha256) = src(&path);
    assert_eq!(
        e.be.upload(
            UploadTarget::Replace {
                pid: &n.pid,
                if_cver: Some("STALEVER")
            },
            UploadSource::File {
                path: &path,
                size,
                sha256
            },
            &mut OpCtx::default(),
        )
        .await
        .err(),
        Some(BErr::PreconditionFailed)
    );
    let r =
        e.be.upload(
            UploadTarget::Replace {
                pid: &n.pid,
                if_cver: Some(&n.cver),
            },
            UploadSource::File {
                path: &path,
                size,
                sha256,
            },
            &mut OpCtx::default(),
        )
        .await
        .unwrap();
    assert_ne!(r.pid, n.pid);
    assert_eq!(r.name_raw, b"Vol 2.mokuro");
    assert_eq!(e.mock.find(&e.series, "Vol 2.mokuro").len(), 2);
    assert_eq!(e.mock.content(&r.pid).unwrap().0, b"{\"v\":3}");
    // The store then removes the old version.
    e.be.delete_permanent(&n.pid).await.unwrap();
    assert!(e.mock.node(&n.pid).is_none());
    assert_eq!(e.be.stat(&n.pid).await.unwrap(), None);
    // A replace of a vanished node.
    assert_eq!(
        e.be.upload(
            UploadTarget::Replace {
                pid: &n.pid,
                if_cver: None
            },
            UploadSource::File {
                path: &path,
                size,
                sha256
            },
            &mut OpCtx::default(),
        )
        .await
        .err(),
        Some(BErr::NotFound)
    );
}

#[tokio::test]
async fn interrupted_upload_resumes_from_opctx() {
    let e = env().await;
    e.be.locate_roots("mokuro-reader").await.unwrap();
    let mut c = cfg(&e.mock, e.tmp.path());
    c.upload_retries = 0;
    let be = MegaBackend::resume(&SessionBlob::new(e.mock.session_blob()), c)
        .await
        .unwrap();
    be.locate_roots("mokuro-reader").await.unwrap();
    let path = e.tmp.path().join("Vol 9.cbz");
    let data = pattern(2 << 20 | 777);
    std::fs::write(&path, &data).unwrap();
    let (size, sha256) = src(&path);
    e.mock.drop_upload_at(1 << 20);
    let mut op = OpCtx {
        op_id: "op1".into(),
        resume: None,
    };
    let to = UploadTarget::New {
        parent: &e.series,
        name: "Vol 9.cbz",
    };
    let s = UploadSource::File {
        path: &path,
        size,
        sha256,
    };
    let err = be.upload(to, s, &mut op).await.err().unwrap();
    assert!(matches!(err, BErr::Transient(_)), "{err:?}");
    let saved = op.resume.clone().expect("resume state kept");
    assert!(saved.contains("\"offset\":1310720"), "{saved}");
    let before = e.mock.chunk_posts().len();
    let n = be.upload(to, s, &mut op).await.unwrap();
    assert_eq!(e.mock.chunk_posts()[before].0, 1310720);
    assert_eq!(op.resume, None);
    assert_eq!(e.mock.content(&n.pid).unwrap(), (data, true));
    // Only one node, though `upload` ran twice.
    assert_eq!(e.mock.find(&e.series, "Vol 9.cbz").len(), 1);
}

#[tokio::test]
async fn uncertain_completion_is_matched_not_repeated() {
    let e = env().await;
    e.be.locate_roots("mokuro-reader").await.unwrap();
    let path = e.tmp.path().join("x.json");
    std::fs::write(&path, b"[1,2,3]").unwrap();
    let (size, sha256) = src(&path);
    let to = UploadTarget::New {
        parent: &e.root,
        name: "x.json",
    };
    let s = UploadSource::File {
        path: &path,
        size,
        sha256,
    };
    let mut op = OpCtx::default();
    e.be.upload(to, s, &mut op).await.unwrap();
    // Replay the persisted state of the moment `p` was sent (its answer "lost").
    let resume = serde_json::json!({
        "sha256": hex::encode(sha256), "parent": e.root, "name": "x.json",
        "idem": "abc", "mtime": std::fs::metadata(&path).unwrap().modified().unwrap()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
        "completing": true
    });
    let mut op = OpCtx {
        op_id: "op".into(),
        resume: Some(resume.to_string()),
    };
    let calls_before = e.mock.calls().len();
    let again = e.be.upload(to, s, &mut op).await.unwrap();
    assert_eq!(e.mock.find(&e.root, "x.json"), vec![again.pid.clone()]);
    assert!(
        !e.mock.calls()[calls_before..]
            .iter()
            .any(|c| c == "p" || c == "u")
    );
}

#[tokio::test]
async fn mkdir_rename_move_trash_delete() {
    let e = env().await;
    e.be.locate_roots("mokuro-reader").await.unwrap();
    let mut c = e.be.start_cursor().await.unwrap();
    all(&e.be, std::slice::from_ref(&e.root)).await;
    let d =
        e.be.mkdir(&e.root, "Series C", &OpCtx::default())
            .await
            .unwrap();
    assert!(d.is_dir);
    assert_eq!(
        e.mock.name_of(&d.pid).unwrap(),
        ("Series C".into(), e.root.clone())
    );
    // Case-only rename, then a move with a rename.
    let r =
        e.be.update(&d.pid, None, Some("series c"), Some(&d.cver))
            .await
            .unwrap();
    assert_eq!(r.name_raw, b"series c");
    assert_eq!(e.mock.name_of(&d.pid).unwrap().0, "series c");
    let m =
        e.be.update(&e.sidecar, Some(&d.pid), Some("Vol 1.mokuro"), None)
            .await
            .unwrap();
    assert_eq!(m.parent.as_deref(), Some(d.pid.as_str()));
    assert_eq!(e.mock.name_of(&e.sidecar).unwrap().1, d.pid);
    assert_eq!(
        e.be.update(&e.root, Some(&d.pid), None, None).await.err(),
        Some(BErr::Fatal("move into itself".into()))
    );
    assert_eq!(
        e.be.update(&d.pid, None, Some("x"), Some("NOTTHIS1"))
            .await
            .err(),
        Some(BErr::PreconditionFailed)
    );
    // Our own changes echo in the feed as upserts.
    let items = drain(&e.be, &mut c).await;
    assert!(items.iter().any(|i| matches!(i, ChangeItem::Upsert(n) if n.pid == e.sidecar && n.parent.as_deref() == Some(d.pid.as_str()))));
    // Trash = the rubbish bin.
    e.be.trash(&e.sidecar, None).await.unwrap();
    assert_eq!(e.mock.name_of(&e.sidecar).unwrap().1, RUBBISH);
    assert!(e.be.stat(&e.sidecar).await.unwrap().unwrap().trashed);
    let items = drain(&e.be, &mut c).await;
    assert!(items.contains(&ChangeItem::Removed {
        pid: e.sidecar.clone()
    }));
    assert_eq!(
        e.be.read(&e.sidecar, None, None).await.err(),
        Some(BErr::NotFound)
    );
    e.be.delete_permanent(&d.pid).await.unwrap();
    assert!(e.mock.node(&d.pid).is_none());
    let items = drain(&e.be, &mut c).await;
    assert!(items.contains(&ChangeItem::Removed { pid: d.pid.clone() }));
    assert_eq!(
        e.be.delete_permanent(&d.pid).await.err(),
        Some(BErr::NotFound)
    );
    assert_eq!(
        e.be.copy(&e.vol, &e.root, "copy.cbz").await.err(),
        Some(BErr::Unsupported)
    );
    // A new root.
    let nr = e.be.create_root("mokuro-reader").await.unwrap();
    assert_eq!(nr.parent.as_deref(), Some(ROOT));
    assert_eq!(e.be.locate_roots("mokuro-reader").await.unwrap().len(), 2);
}

#[tokio::test]
async fn killed_session_needs_reauth() {
    let e = env().await;
    e.be.locate_roots("mokuro-reader").await.unwrap();
    let c = e.be.start_cursor().await.unwrap();
    e.mock.kill_session();
    assert_eq!(e.be.stat(&e.vol).await.err(), Some(BErr::AuthRevoked));
    assert_eq!(e.be.changes(&c).await.err(), Some(BErr::AuthRevoked));
    assert_eq!(e.be.account().await.err(), Some(BErr::AuthRevoked));
    assert_eq!(
        MegaBackend::resume(
            &SessionBlob::new(e.mock.session_blob()),
            cfg(&e.mock, e.tmp.path())
        )
        .await
        .err(),
        Some(BErr::AuthRevoked)
    );
    // Only resumes were tried: no login with a password.
    assert!(!e.mock.calls().iter().any(|c| c == "us0"));
}

#[tokio::test]
async fn login_helper_produces_a_resumable_blob() {
    let sim = bunko_cloudsim::start().await;
    let mock = sim.mega("default");
    mock.set_account("me@example.com", "pw-pw-pw", Some("654321"));
    let tmp = tempfile::tempdir().unwrap();
    let c = cfg(&mock, tmp.path());
    let e = bunko_remote::mega::MegaSession::login_with(
        c.client.clone(),
        "me@example.com",
        "pw-pw-pw",
        None,
    )
    .await
    .err();
    assert_eq!(e, Some(bunko_remote::mega::LoginError::MfaRequired));
    let e = bunko_remote::mega::MegaSession::login_with(
        c.client.clone(),
        "me@example.com",
        "nope",
        Some("654321"),
    )
    .await
    .err();
    assert_eq!(e, Some(bunko_remote::mega::LoginError::BadCredentials));
    let blob = bunko_remote::mega::MegaSession::login_with(
        c.client.clone(),
        "me@example.com",
        "pw-pw-pw",
        Some("654321"),
    )
    .await
    .unwrap();
    assert_eq!(format!("{blob:?}"), "SessionBlob(<redacted>)");
    let be = MegaBackend::resume(&blob, c).await.unwrap();
    assert_eq!(be.account().await.unwrap().label, "me@example.com");
}
