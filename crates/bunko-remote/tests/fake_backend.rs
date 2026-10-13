//! FakeBackend behaves like the providers it stands in for (plan M2 task 1).

use std::path::PathBuf;
use std::time::Duration;

use bunko_remote::backend::{
    BErr, Backend, ChangeItem, ChangePage, Cursor, OpCtx, RNode, UploadSource, UploadTarget,
    read_to_vec,
};
use bunko_remote::fake::{FakeBackend, FakeOp};
use sha2::{Digest, Sha256};

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

async fn upload_new(
    b: &FakeBackend,
    parent: &str,
    name: &str,
    bytes: &[u8],
) -> Result<RNode, BErr> {
    let s = src(bytes);
    b.upload(UploadTarget::New { parent, name }, s.source(), &mut op())
        .await
}

async fn drain(b: &FakeBackend, mut c: Cursor) -> (Vec<ChangeItem>, Cursor) {
    let mut all = Vec::new();
    loop {
        match b.changes(&c).await.unwrap() {
            ChangePage::Page { items, next, more } => {
                all.extend(items);
                c = next;
                if !more {
                    return (all, c);
                }
            }
            ChangePage::CursorLost => panic!("cursor lost"),
        }
    }
}

fn upserted(items: &[ChangeItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|i| match i {
            ChangeItem::Upsert(n) => Some(String::from_utf8_lossy(&n.name_raw).into_owned()),
            ChangeItem::Removed { .. } => None,
        })
        .collect()
}

async fn list_dir(b: &FakeBackend, dir: &str) -> Vec<RNode> {
    let mut out = Vec::new();
    b.list_dir(dir, &mut |n| out.push(n)).await.unwrap();
    out
}

#[tokio::test(start_paused = true)]
async fn create_list_changes_round_trip() {
    for b in [
        FakeBackend::drive_like(),
        FakeBackend::webdav_cas(),
        FakeBackend::webdav_plain(),
        FakeBackend::onedrive_like(),
        FakeBackend::mega_like(),
    ] {
        let c0 = b.start_cursor().await.unwrap();
        let root = b.create_root("mokuro-reader").await.unwrap();
        assert!(root.is_dir);
        let roots = b.locate_roots("mokuro-reader").await.unwrap();
        assert_eq!(roots, vec![root.clone()]);

        let series = b.mkdir(&root.pid, "Series", &op()).await.unwrap();
        let vol = upload_new(&b, &series.pid, "Vol 1.cbz", b"zipbytes")
            .await
            .unwrap();
        assert_eq!(vol.size, 8);
        assert_eq!(vol.parent.as_deref(), Some(series.pid.as_str()));

        let mut all = Vec::new();
        b.list_all(std::slice::from_ref(&root.pid), &mut |n| all.push(n))
            .await
            .unwrap();
        let mut names: Vec<_> = all
            .iter()
            .map(|n| String::from_utf8_lossy(&n.name_raw).into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["Series", "Vol 1.cbz"]);
        assert_eq!(list_dir(&b, &series.pid).await, vec![vol.clone()]);

        let (items, _) = drain(&b, c0).await;
        assert_eq!(upserted(&items), ["mokuro-reader", "Series", "Vol 1.cbz"]);

        let body = b.read(&vol.pid, vol.pin.as_deref(), None).await.unwrap();
        assert_eq!(read_to_vec(body).await.unwrap(), b"zipbytes");
        assert_eq!(b.stat(&vol.pid).await.unwrap(), Some(vol));
    }
}

#[tokio::test(start_paused = true)]
async fn cursor_is_monotonic() {
    let b = FakeBackend::drive_like();
    b.set_page_size(2);
    let c0 = b.start_cursor().await.unwrap();
    let root = b.create_root("mokuro-reader").await.unwrap();
    for i in 0..5 {
        upload_new(&b, &root.pid, &format!("f{i}"), b"x")
            .await
            .unwrap();
    }
    let mut c = c0.clone();
    let mut seen = 0;
    let mut pages = 0;
    loop {
        let ChangePage::Page { items, next, more } = b.changes(&c).await.unwrap() else {
            panic!("cursor lost");
        };
        assert!(items.len() <= 2);
        assert!(
            next.0.parse::<u64>().unwrap() > c.0.parse::<u64>().unwrap(),
            "cursor advanced"
        );
        seen += items.len();
        pages += 1;
        c = next;
        if !more {
            break;
        }
    }
    assert_eq!(seen, 6);
    assert_eq!(pages, 3);
    // Nothing new: an empty page that keeps the cursor.
    let ChangePage::Page { items, next, more } = b.changes(&c).await.unwrap() else {
        panic!()
    };
    assert!(items.is_empty() && !more);
    assert_eq!(next, c);
    // A later start cursor is never behind an earlier one.
    let c1 = b.start_cursor().await.unwrap();
    assert!(c1.0.parse::<u64>().unwrap() >= c.0.parse::<u64>().unwrap());
}

#[tokio::test(start_paused = true)]
async fn lose_cursor_gives_cursor_lost() {
    let b = FakeBackend::webdav_cas();
    let c0 = b.start_cursor().await.unwrap();
    b.create_root("mokuro-reader").await.unwrap();
    b.lose_cursor();
    assert!(matches!(
        b.changes(&c0).await.unwrap(),
        ChangePage::CursorLost
    ));
    // A fresh cursor works again.
    let c1 = b.start_cursor().await.unwrap();
    assert!(matches!(
        b.changes(&c1).await.unwrap(),
        ChangePage::Page { .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn drive_like_allows_duplicate_names() {
    let b = FakeBackend::drive_like();
    let root = b.create_root("mokuro-reader").await.unwrap();
    let a1 = upload_new(&b, &root.pid, "a", b"one").await.unwrap();
    let a2 = upload_new(&b, &root.pid, "a", b"two").await.unwrap();
    assert_ne!(a1.pid, a2.pid);
    assert!(a2.created_ms > a1.created_ms);
    let (found, _) = b.find_named(&root.pid, "a").await.unwrap();
    assert_eq!(found.len(), 2);
    // Two roots with the same name are both located.
    b.create_root("mokuro-reader").await.unwrap();
    assert_eq!(b.locate_roots("mokuro-reader").await.unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn name_rules_per_profile() {
    let wd = FakeBackend::webdav_cas();
    let root = wd.create_root("mokuro-reader").await.unwrap();
    upload_new(&wd, &root.pid, "a", b"one").await.unwrap();
    // If-None-Match: * on an existing name.
    assert!(matches!(
        upload_new(&wd, &root.pid, "a", b"two").await,
        Err(BErr::PreconditionFailed)
    ));
    // Case-sensitive: a case variant is a second file.
    upload_new(&wd, &root.pid, "A", b"two").await.unwrap();
    assert!(matches!(
        wd.mkdir(&root.pid, "a", &op()).await,
        Err(BErr::NameConflict)
    ));

    // Without CAS a PUT to an existing path overwrites it in place.
    let plain = FakeBackend::webdav_plain();
    let root = plain.create_root("mokuro-reader").await.unwrap();
    let first = upload_new(&plain, &root.pid, "a", b"one").await.unwrap();
    let second = upload_new(&plain, &root.pid, "a", b"two!").await.unwrap();
    assert_eq!(first.pid, second.pid);
    assert_ne!(first.cver, second.cver);
    assert_eq!(second.size, 4);

    let od = FakeBackend::onedrive_like();
    let root = od.create_root("mokuro-reader").await.unwrap();
    upload_new(&od, &root.pid, "Vol 1.cbz", b"one")
        .await
        .unwrap();
    assert!(matches!(
        upload_new(&od, &root.pid, "vol 1.CBZ", b"two").await,
        Err(BErr::NameConflict)
    ));
}

#[tokio::test(start_paused = true)]
async fn webdav_plain_path_id_move_changes_pid() {
    let b = FakeBackend::webdav_plain();
    let root = b.create_root("mokuro-reader").await.unwrap();
    let s = b.mkdir(&root.pid, "Series", &op()).await.unwrap();
    let v = upload_new(&b, &s.pid, "Vol 1.cbz", b"zip").await.unwrap();
    assert_eq!(v.pid, "mokuro-reader/Series/Vol 1.cbz");
    let c = b.start_cursor().await.unwrap();

    let moved = b.update(&s.pid, None, Some("Renamed"), None).await.unwrap();
    assert_eq!(moved.pid, "mokuro-reader/Renamed");
    assert_eq!(b.stat(&s.pid).await.unwrap(), None);
    assert_eq!(b.stat(&v.pid).await.unwrap(), None);
    let child = b
        .stat("mokuro-reader/Renamed/Vol 1.cbz")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.cver, v.cver, "a move is not a content change");

    // The feed reports the old pids removed (children first) and the new ones added.
    let (items, _) = drain(&b, c).await;
    let removed: Vec<_> = items
        .iter()
        .filter_map(|i| match i {
            ChangeItem::Removed { pid } => Some(pid.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(removed, [v.pid.as_str(), s.pid.as_str()]);
    assert_eq!(upserted(&items), ["Renamed", "Vol 1.cbz"]);

    // Opaque ids keep the pid across a move.
    let d = FakeBackend::drive_like();
    let root = d.create_root("mokuro-reader").await.unwrap();
    let s = d.mkdir(&root.pid, "Series", &op()).await.unwrap();
    let moved = d.update(&s.pid, None, Some("Renamed"), None).await.unwrap();
    assert_eq!(moved.pid, s.pid);
    assert_eq!(moved.name_raw, b"Renamed");
}

#[tokio::test(start_paused = true)]
async fn ranged_read_returns_the_slice() {
    let b = FakeBackend::webdav_cas();
    let root = b.create_root("mokuro-reader").await.unwrap();
    let data: Vec<u8> = (0..=255u8).cycle().take(200_000).collect();
    let f = upload_new(&b, &root.pid, "big.cbz", &data).await.unwrap();
    let pin = f.pin.as_deref();
    let got = read_to_vec(b.read(&f.pid, pin, Some((10, Some(19)))).await.unwrap())
        .await
        .unwrap();
    assert_eq!(got, &data[10..20]);
    let tail = read_to_vec(b.read(&f.pid, pin, Some((199_990, None))).await.unwrap())
        .await
        .unwrap();
    assert_eq!(tail, &data[199_990..]);
    let past_end = read_to_vec(
        b.read(&f.pid, pin, Some((150_000, Some(999_999))))
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(past_end, &data[150_000..]);
    assert!(matches!(
        b.read(&f.pid, pin, Some((200_000, None))).await,
        Err(BErr::Fatal(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn pinned_reads_follow_the_profile() {
    // WebDAV with If-Match: a replaced file fails the old pin.
    let b = FakeBackend::webdav_cas();
    let root = b.create_root("mokuro-reader").await.unwrap();
    let f = upload_new(&b, &root.pid, "a", b"one").await.unwrap();
    let s = src(b"two");
    let f2 = b
        .upload(
            UploadTarget::Replace {
                pid: &f.pid,
                if_cver: Some(&f.cver),
            },
            s.source(),
            &mut op(),
        )
        .await
        .unwrap();
    assert_ne!(f2.cver, f.cver);
    assert!(matches!(
        b.read(&f.pid, f.pin.as_deref(), None).await,
        Err(BErr::PreconditionFailed)
    ));
    // A stale CAS base is refused.
    let s = src(b"three");
    assert!(matches!(
        b.upload(
            UploadTarget::Replace {
                pid: &f.pid,
                if_cver: Some(&f.cver)
            },
            s.source(),
            &mut op()
        )
        .await,
        Err(BErr::PreconditionFailed)
    ));

    // Drive keeps revisions: the old pin still reads the old bytes.
    let d = FakeBackend::drive_like();
    let root = d.create_root("mokuro-reader").await.unwrap();
    let f = upload_new(&d, &root.pid, "a", b"one").await.unwrap();
    d.reader_put("mokuro-reader/a", b"two");
    let old = read_to_vec(d.read(&f.pid, f.pin.as_deref(), None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(old, b"one");
    let new = read_to_vec(d.read(&f.pid, None, None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(new, b"two");
}

#[tokio::test(start_paused = true)]
async fn mega_like_replace_is_a_new_node() {
    let b = FakeBackend::mega_like();
    let root = b.create_root("mokuro-reader").await.unwrap();
    let f = upload_new(&b, &root.pid, "a", b"one").await.unwrap();
    let s = src(b"two");
    let f2 = b
        .upload(
            UploadTarget::Replace {
                pid: &f.pid,
                if_cver: None,
            },
            s.source(),
            &mut op(),
        )
        .await
        .unwrap();
    assert_ne!(f2.pid, f.pid);
    assert_eq!(b.stat(&f.pid).await.unwrap(), None);
    assert!(b.caps().push);
}

#[tokio::test(start_paused = true)]
async fn fail_next_returns_the_injected_error_once() {
    let b = FakeBackend::drive_like();
    b.fail_next(FakeOp::StartCursor, BErr::Transient("boom".into()));
    assert!(matches!(b.start_cursor().await, Err(BErr::Transient(m)) if m == "boom"));
    assert!(b.start_cursor().await.is_ok());
    assert_eq!(b.calls(FakeOp::StartCursor), 2);
}

#[tokio::test(start_paused = true)]
async fn throttle_and_latency() {
    let b = FakeBackend::drive_like();
    b.throttle_for(Duration::from_secs(30));
    assert!(
        matches!(b.account().await, Err(BErr::Throttled(Some(d))) if d <= Duration::from_secs(30))
    );
    tokio::time::advance(Duration::from_secs(31)).await;
    assert!(b.account().await.is_ok());

    b.set_latency(Duration::from_millis(250));
    let t0 = tokio::time::Instant::now();
    b.account().await.unwrap();
    assert!(t0.elapsed() >= Duration::from_millis(250));
}

#[tokio::test(start_paused = true)]
async fn reader_helpers_act_on_paths() {
    let b = FakeBackend::drive_like();
    let root = b.create_root("mokuro-reader").await.unwrap();
    let c = b.start_cursor().await.unwrap();
    let vol = b.reader_put("mokuro-reader/Series/Vol 1.cbz", b"zip");
    let series = b.node_at("mokuro-reader/Series").unwrap();
    assert!(series.is_dir);
    assert_eq!(series.parent.as_deref(), Some(root.pid.as_str()));
    let moved = b.reader_rename(
        "mokuro-reader/Series/Vol 1.cbz",
        "mokuro-reader/Other/Vol 2.cbz",
    );
    assert_eq!(moved.pid, vol.pid);
    assert!(b.node_at("mokuro-reader/Series/Vol 1.cbz").is_none());
    b.reader_mkdir("mokuro-reader/Empty");
    b.reader_delete("mokuro-reader/Other/Vol 2.cbz");
    assert!(b.node_at("mokuro-reader/Other/Vol 2.cbz").is_none());
    // The call logs record Bunko's calls, not the reader's.
    assert!(b.trashed_log().is_empty());
    // Drive reports a trash as an upsert with `trashed`.
    let (items, _) = drain(&b, c).await;
    let last = items.last().unwrap();
    assert!(matches!(last, ChangeItem::Upsert(n) if n.trashed && n.pid == vol.pid));
}

#[tokio::test(start_paused = true)]
async fn reorder_and_duplicate_deliveries() {
    let b = FakeBackend::onedrive_like();
    let c = b.start_cursor().await.unwrap();
    b.reader_put("mokuro-reader/a", b"1");
    b.reader_put("mokuro-reader/b", b"2");
    b.reorder_changes(true);
    b.duplicate_deliveries(true);
    let (items, _) = drain(&b, c).await;
    assert_eq!(
        upserted(&items),
        ["b", "b", "a", "a", "mokuro-reader", "mokuro-reader"]
    );
}

#[tokio::test(start_paused = true)]
async fn push_profile_wakes_wait_change() {
    let b = std::sync::Arc::new(FakeBackend::mega_like());
    let c = b.start_cursor().await.unwrap();
    let b2 = b.clone();
    let waiter = tokio::spawn(async move {
        let t0 = tokio::time::Instant::now();
        b2.wait_change(&c, Duration::from_secs(600)).await.unwrap();
        t0.elapsed()
    });
    tokio::time::sleep(Duration::from_secs(5)).await;
    b.reader_mkdir("mokuro-reader");
    let waited = waiter.await.unwrap();
    assert!(waited < Duration::from_secs(10));
    // Poll-only profiles do not long-poll.
    let d = FakeBackend::drive_like();
    let c = d.start_cursor().await.unwrap();
    assert!(matches!(
        d.wait_change(&c, Duration::from_secs(1)).await,
        Err(BErr::Unsupported)
    ));
}

#[tokio::test(start_paused = true)]
async fn trash_and_delete_are_recorded_apart() {
    let b = FakeBackend::webdav_cas();
    let root = b.create_root("mokuro-reader").await.unwrap();
    let a = upload_new(&b, &root.pid, "a", b"1").await.unwrap();
    let x = upload_new(&b, &root.pid, "x", b"2").await.unwrap();
    b.trash(&a.pid, Some(&a.cver)).await.unwrap();
    b.delete_permanent(&x.pid).await.unwrap();
    assert_eq!(b.trashed_log(), vec![a.pid]);
    assert_eq!(b.deleted_log(), vec![x.pid]);
    assert!(matches!(b.trash("nope", None).await, Err(BErr::NotFound)));
}

#[tokio::test(start_paused = true)]
async fn quota_and_storage_full() {
    let b = FakeBackend::drive_like();
    assert_eq!(b.quota().await.unwrap(), None);
    b.set_quota(Some(10));
    let root = b.create_root("mokuro-reader").await.unwrap();
    upload_new(&b, &root.pid, "a", b"123456").await.unwrap();
    let q = b.quota().await.unwrap().unwrap();
    assert_eq!((q.total, q.available), (10, 4));
    assert!(matches!(
        upload_new(&b, &root.pid, "b", b"123456").await,
        Err(BErr::StorageFull)
    ));
}
