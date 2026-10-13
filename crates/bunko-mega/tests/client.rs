//! The client against the in-process mock (feature `mock`).

use std::time::Duration;

use bunko_mega::mock::{MockMega, ROOT};
use bunko_mega::{
    Client, ClientConfig, Error, ErrorCode, Event, NodeKey, Poll, UploadState, file_fingerprint,
    idempotence_id,
};
use futures_util::StreamExt;

fn cfg(mock: &MockMega) -> ClientConfig {
    ClientConfig {
        api_origin: mock.base.clone(),
        min_cs_interval: Duration::ZERO,
        retry_base: Duration::from_millis(1),
        retry_max: Duration::from_millis(5),
        https_transfers: false,
        ..ClientConfig::default()
    }
}

async fn collect(s: bunko_mega::ByteStream) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut s = s;
    while let Some(c) = s.next().await {
        out.extend_from_slice(&c?);
    }
    Ok(out)
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 % 251) as u8).collect()
}

#[tokio::test]
async fn login_v2_with_2fa_then_resume_without_the_password() {
    let mock = MockMega::start().await;
    mock.set_account("Reader@Example.com", "hunter2 but longer", Some("123456"));
    let c = cfg(&mock);
    let e = Client::login(c.clone(), "reader@example.com", "hunter2 but longer", None)
        .await
        .err()
        .unwrap();
    assert_eq!(e.code(), Some(ErrorCode::EMFAREQUIRED));
    let e = Client::login(c.clone(), "reader@example.com", "wrong", Some("123456"))
        .await
        .err()
        .unwrap();
    assert_eq!(e.code(), Some(ErrorCode::ENOENT));
    let client = Client::login(
        c.clone(),
        "READER@example.com",
        "hunter2 but longer",
        Some("123456"),
    )
    .await
    .unwrap();
    let session = client.session().unwrap();
    assert_eq!(session.user_handle(), bunko_mega::mock::USER);
    let blob = session.serialize().unwrap();
    assert!(!blob.contains("hunter2"));
    let resumed = Client::resume(c.clone(), &blob).await.unwrap();
    assert_eq!(resumed.session().unwrap().sid(), session.sid());
    assert_eq!(
        resumed.user_info().await.unwrap().email,
        "reader@example.com"
    );
    // A killed session is ESID, on resume and on use.
    resumed.logout().await.unwrap();
    let e = Client::resume(c, &blob).await.err().unwrap();
    assert_eq!(e.code(), Some(ErrorCode::ESID));
}

#[tokio::test]
async fn hashcash_challenge_and_eagain_are_retried() {
    let mock = MockMega::start().await;
    let client = Client::resume(cfg(&mock), &mock.session_blob())
        .await
        .unwrap();
    mock.require_hashcash(255);
    let q = client.storage_quota().await.unwrap();
    assert_eq!(q.total, 20 << 30);
    assert_eq!(mock.hashcash_answered(), 1);
    mock.eagain(3);
    client.user_info().await.unwrap();
    // A per-command error is the command's, not retried.
    mock.fail_command("ug", -9);
    assert_eq!(
        client.user_info().await.err().unwrap().code(),
        Some(ErrorCode::ENOENT)
    );
}

#[tokio::test]
async fn fetch_scan_decrypt() {
    let mock = MockMega::start().await;
    let dir = mock.mkdir(ROOT, "mokuro-reader");
    let f = mock.put(&dir, "Vol 1.cbz", b"hello");
    let client = Client::resume(cfg(&mock), &mock.session_blob())
        .await
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let snap = client.fetch_nodes(tmp.path()).await.unwrap();
    let s = client.session().unwrap();
    let mut names = Vec::new();
    let sn = snap
        .scan(&mut |n| {
            if let Some(node) = s.decrypt_node(&n).unwrap() {
                names.push((node.handle, node.name, node.size));
            }
        })
        .unwrap();
    assert_eq!(sn, mock.sn());
    assert!(names.contains(&(dir.clone(), "mokuro-reader".into(), 0)));
    assert!(names.contains(&(f.clone(), "Vol 1.cbz".into(), 5)));
    assert_eq!(names.len(), 5, "{names:?}");
}

async fn file_key(client: &Client, mock: &MockMega, h: &str) -> NodeKey {
    let tmp = tempfile::tempdir().unwrap();
    let snap = client.fetch_nodes(tmp.path()).await.unwrap();
    let s = client.session().unwrap();
    let mut key = None;
    snap.scan(&mut |n| {
        if n.handle == h {
            key = Some(s.decrypt_node(&n).unwrap().unwrap().key);
        }
    })
    .unwrap();
    let _ = mock;
    key.unwrap()
}

#[tokio::test]
async fn ranged_reads_equal_slices_of_the_full_read() {
    let mock = MockMega::start().await;
    let data = pattern(300_007);
    let h = mock.put(ROOT, "big.cbz", &data);
    let client = Client::resume(cfg(&mock), &mock.session_blob())
        .await
        .unwrap();
    let key = file_key(&client, &mock, &h).await;
    let t = client.download_target(&h).await.unwrap();
    assert_eq!(t.size, data.len() as u64);
    let full = collect(client.read_range(&key, &t, 0, None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(full, data);
    for (first, last) in [
        (1u64, Some(1u64)),
        (15, Some(16)),
        (17, Some(4095)),
        (131_071, Some(131_073)),
        (299_999, None),
        (7, Some(10_000_000)),
    ] {
        let got = collect(client.read_range(&key, &t, first, last).await.unwrap())
            .await
            .unwrap();
        let end = last.map_or(data.len(), |l| (l as usize + 1).min(data.len()));
        assert_eq!(got, &data[first as usize..end], "{first}-{last:?}");
    }
    // The request starts on the AES block below the offset.
    assert!(mock.download_ranges().contains(&"16-4095".to_string()));
    assert!(client.read_range(&key, &t, 300_007, None).await.is_err());
    // A wrong MAC fails the whole read at its end.
    let NodeKey::File { aes, nonce, .. } = key else {
        panic!()
    };
    let bad = NodeKey::File {
        aes,
        nonce,
        mac: [0; 8],
    };
    let e = collect(client.read_range(&bad, &t, 0, None).await.unwrap())
        .await
        .err()
        .unwrap();
    assert!(e.to_string().contains("MAC"), "{e}");
    // ... but a ranged read cannot check it.
    collect(client.read_range(&bad, &t, 0, Some(99)).await.unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn over_quota_is_detected_before_bytes_are_trusted() {
    let mock = MockMega::start().await;
    let h = mock.put(ROOT, "a.cbz", &pattern(10_000));
    let client = Client::resume(cfg(&mock), &mock.session_blob())
        .await
        .unwrap();
    let key = file_key(&client, &mock, &h).await;
    let t = client.download_target(&h).await.unwrap();
    mock.garbage_download(true);
    let e = client.read_range(&key, &t, 0, None).await.err().unwrap();
    assert!(
        matches!(
            e,
            Error::ShortRead {
                expected: 10_000,
                got: 3
            }
        ),
        "{e:?}"
    );
    mock.garbage_download(false);
    mock.bandwidth_509(true);
    let e = client.read_range(&key, &t, 0, None).await.err().unwrap();
    assert!(matches!(e, Error::OverQuota { .. }), "{e:?}");
    mock.overquota(true);
    let e = client.download_target(&h).await.err().unwrap();
    assert!(
        matches!(e, Error::OverQuota { retry_after: Some(d) } if d == Duration::from_secs(600)),
        "{e:?}"
    );
}

#[tokio::test]
async fn chunked_upload_resumes_after_a_dropped_chunk() {
    let mock = MockMega::start().await;
    let client = Client::resume(cfg(&mock), &mock.session_blob())
        .await
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("v.cbz");
    let data = pattern(3 << 20 | 12_345);
    std::fs::write(&path, &data).unwrap();
    let mut st = client.upload_begin(data.len() as u64).await.unwrap();
    mock.drop_upload_at(1 << 20);
    let mut saved: Option<String> = None;
    let e = client
        .upload_send(&mut st, &path, 256 << 10, &mut |s| {
            saved = Some(serde_json::to_string(s).unwrap())
        })
        .await
        .err()
        .unwrap();
    assert!(matches!(e, Error::Http { status: 500, .. }), "{e:?}");
    // A fresh process picks the persisted state up.
    let mut st: UploadState = serde_json::from_str(saved.as_deref().unwrap()).unwrap();
    let resumed_at = st.offset;
    // POSTs of whole MAC chunks: 0-128K, -384K, -768K, -1280K; the one at 1280K dropped.
    assert_eq!(resumed_at, 1280 << 10);
    let posts_before = mock.chunk_posts().len();
    let token = client
        .upload_send(&mut st, &path, 256 << 10, &mut |_| {})
        .await
        .unwrap();
    assert_eq!(mock.chunk_posts()[posts_before].0, resumed_at);
    let fp = file_fingerprint(&path, 1_700_000_123).unwrap();
    let node = client
        .upload_complete(ROOT, "v.cbz", &st, fp, &idempotence_id())
        .await
        .unwrap();
    assert!(!token.is_empty());
    let (plain, mac_ok) = mock.content(&node.handle).unwrap();
    assert_eq!(plain, data);
    assert!(mac_ok);
    let n = client
        .session()
        .unwrap()
        .decrypt_node(&node)
        .unwrap()
        .unwrap();
    assert_eq!(n.name, "v.cbz");
    assert_eq!(n.fingerprint.unwrap(), fp);
    // Read back through the client.
    let t = client.download_target(&node.handle).await.unwrap();
    let back = collect(client.read_range(&n.key, &t, 0, None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(back, data);
}

#[tokio::test]
async fn empty_and_tiny_uploads() {
    let mock = MockMega::start().await;
    let client = Client::resume(cfg(&mock), &mock.session_blob())
        .await
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    for (name, data) in [("empty", Vec::new()), ("tiny", b"x".to_vec())] {
        let path = tmp.path().join(name);
        std::fs::write(&path, &data).unwrap();
        let mut st = client.upload_begin(data.len() as u64).await.unwrap();
        client
            .upload_send(&mut st, &path, 1 << 20, &mut |_| {})
            .await
            .unwrap();
        let fp = file_fingerprint(&path, 0).unwrap();
        let node = client
            .upload_complete(ROOT, name, &st, fp, "idem")
            .await
            .unwrap();
        assert_eq!(mock.content(&node.handle).unwrap(), (data, true));
    }
}

#[tokio::test]
async fn sc_events_are_typed_and_moves_fold() {
    let mock = MockMega::start().await;
    let dir = mock.mkdir(ROOT, "mokuro-reader");
    let other = mock.mkdir(ROOT, "elsewhere");
    let f = mock.put(&dir, "a.cbz", b"aaa");
    let client = Client::resume(cfg(&mock), &mock.session_blob())
        .await
        .unwrap();
    let sn = mock.sn();
    assert!(matches!(
        client.poll_events(&sn).await.unwrap(),
        Poll::Idle { .. }
    ));
    mock.rename(&f, "b.cbz");
    mock.move_to(&f, &other);
    let g = mock.put(&dir, "c.cbz", b"c");
    mock.delete(&g);
    let Poll::Batch(b) = client.poll_events(&sn).await.unwrap() else {
        panic!("events")
    };
    assert_eq!(b.sn, mock.sn());
    assert!(matches!(&b.events[0], Event::Updated { handle, .. } if *handle == f));
    assert!(matches!(&b.events[1], Event::Moved(n) if n.handle == f && n.parent == other));
    assert!(matches!(&b.events[2], Event::Added(n) if n.handle == g));
    assert!(matches!(&b.events[3], Event::Removed { handle, moved: false } if *handle == g));
    assert_eq!(b.events.len(), 4);
    // An unmodelled packet asks for a resync.
    mock.push_packet(serde_json::json!({"a": "brand-new", "x": 1}));
    let Poll::Batch(r) = client.poll_events(&b.sn).await.unwrap() else {
        panic!()
    };
    assert_eq!(
        r.events,
        vec![Event::Resync {
            packet: "brand-new".into()
        }]
    );
    // A stale cursor is an error the caller turns into a full sync.
    mock.lose_cursor();
    assert_eq!(
        client.poll_events(&sn).await.err().unwrap().code(),
        Some(ErrorCode::ETOOMANY)
    );
    // The long-polling stream wakes on a change.
    let mut s = client.event_stream(mock.sn(), Duration::from_secs(5));
    let h = tokio::spawn(async move { s.next().await.unwrap().unwrap() });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let x = mock.mkdir(&dir, "new series");
    let batch = tokio::time::timeout(Duration::from_secs(5), h)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(&batch.events[0], Event::Added(n) if n.handle == x));
}
