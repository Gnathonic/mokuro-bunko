//! The governor (plan M2 task 6; spec §3.1, §5.11, D23): budgets, priorities,
//! pauses and hard stops on paused tokio time over the FakeBackend, and the store's
//! side of it (interactive fail-fast, the outbox limits, `api_stats`) on real time.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bunko_remote::fake::{FakeBackend, FakeOp};
use bunko_remote::governor::{Costs, scoped};
use bunko_remote::{
    ApiClass, BErr, Backend, Governed, Governor, Limits, OpCtx, RemoteStore, RemoteStoreConfig,
    UploadSource, UploadTarget,
};
use bunko_store::{
    Blocked, Durability, Health, LibraryStore, Mode, MountGuard, Origin, WriteIntent, fs,
};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::time::Instant;

const FOLDER: &str = "mokuro-reader";

fn governed(fake: FakeBackend, limits: Limits) -> (Arc<FakeBackend>, Arc<Governed<FakeBackend>>) {
    fake.reader_mkdir(FOLDER);
    fake.reader_put(&format!("{FOLDER}/a.json"), b"{\"a\":1}");
    let fake = Arc::new(fake);
    let gov = Governor::new(limits);
    (fake.clone(), Arc::new(Governed::new(fake, gov)))
}

fn pid(fake: &FakeBackend, rel: &str) -> String {
    fake.node_at(&format!("{FOLDER}/{rel}")).unwrap().pid
}

async fn read_all(g: &Governed<FakeBackend>, pid: &str) -> Result<Vec<u8>, BErr> {
    let mut body = g.read(pid, None, None).await?;
    let mut out = Vec::new();
    while let Some(c) = body.next().await {
        out.extend_from_slice(&c.unwrap());
    }
    Ok(out)
}

/// A local file to upload, kept alive with its dir.
struct Src {
    _dir: TempDir,
    path: PathBuf,
    bytes: Vec<u8>,
}

impl Src {
    fn new(bytes: &[u8]) -> Src {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("src");
        std::fs::write(&path, bytes).unwrap();
        Src {
            _dir: dir,
            path,
            bytes: bytes.to_vec(),
        }
    }

    fn source(&self) -> UploadSource<'_> {
        UploadSource::File {
            path: &self.path,
            size: self.bytes.len() as u64,
            sha256: Sha256::digest(&self.bytes).into(),
        }
    }
}

async fn upload(g: &Governed<FakeBackend>, fake: &FakeBackend, name: &str) -> Result<(), BErr> {
    let src = Src::new(b"bytes");
    let root = pid(fake, "");
    let mut op = OpCtx {
        op_id: name.into(),
        resume: None,
    };
    g.upload(
        UploadTarget::New {
            parent: &root,
            name,
        },
        src.source(),
        &mut op,
    )
    .await
    .map(|_| ())
}

#[tokio::test(start_paused = true)]
async fn a_429_with_retry_after_pauses_every_class() {
    let (fake, g) = governed(FakeBackend::drive_like(), Limits::unlimited());
    let a = pid(&fake, "a.json");
    fake.fail_next(FakeOp::Stat, BErr::Throttled(Some(Duration::from_secs(10))));
    assert_eq!(
        g.stat(&a).await,
        Err(BErr::Throttled(Some(Duration::from_secs(10))))
    );
    assert!(matches!(
        g.governor().health(),
        Some(Health::Degraded { .. })
    ));
    let calls = fake.calls(FakeOp::Stat);
    let t0 = Instant::now();
    for class in [ApiClass::Lease, ApiClass::Ocr, ApiClass::Background] {
        let r = scoped(class, g.stat(&a)).await;
        assert!(r.unwrap().is_some(), "{class:?}");
        assert!(t0.elapsed() >= Duration::from_secs(10), "{class:?} waited");
    }
    // Nothing reached the provider during the pause.
    assert_eq!(fake.calls(FakeOp::Stat), calls + 3);
    assert_eq!(g.governor().health(), None);
}

#[tokio::test(start_paused = true)]
async fn an_interactive_call_fails_fast_during_a_long_pause() {
    let (fake, g) = governed(FakeBackend::drive_like(), Limits::unlimited());
    let a = pid(&fake, "a.json");
    fake.fail_next(FakeOp::Stat, BErr::Throttled(Some(Duration::from_secs(30))));
    let _ = g.stat(&a).await;
    let calls = fake.calls(FakeOp::Stat);
    let t0 = Instant::now();
    match scoped(ApiClass::Interactive, g.stat(&a)).await {
        Err(BErr::Throttled(Some(d))) => assert!(d > Duration::from_secs(29), "{d:?}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(t0.elapsed(), Duration::ZERO);
    assert_eq!(fake.calls(FakeOp::Stat), calls);
    // A short pause (≤ 2 s) is waited out.
    tokio::time::advance(Duration::from_millis(28_500)).await;
    let t1 = Instant::now();
    assert!(scoped(ApiClass::Interactive, g.stat(&a)).await.is_ok());
    assert!(t1.elapsed() >= Duration::from_millis(1400));
}

#[tokio::test(start_paused = true)]
async fn an_exhausted_budget_delays_background_but_not_interactive() {
    let limits = Limits {
        background_units_per_min: Some(120),
        costs: Costs::flat(60),
        ..Limits::unlimited()
    };
    let (fake, g) = governed(FakeBackend::drive_like(), limits);
    let a = pid(&fake, "a.json");
    let t0 = Instant::now();
    // The bucket holds one minute: two calls now.
    for _ in 0..2 {
        scoped(ApiClass::Background, g.stat(&a)).await.unwrap();
    }
    assert_eq!(t0.elapsed(), Duration::ZERO);
    // Interactive (and lease) calls are metered, not budgeted.
    for class in [ApiClass::Interactive, ApiClass::Lease] {
        scoped(class, g.stat(&a)).await.unwrap();
        assert_eq!(t0.elapsed(), Duration::ZERO, "{class:?}");
    }
    // The third background call waits for 60 units at 2 units/s.
    scoped(ApiClass::Background, g.stat(&a)).await.unwrap();
    let waited = t0.elapsed();
    assert!(
        waited >= Duration::from_secs(29) && waited <= Duration::from_secs(31),
        "{waited:?}"
    );
    let s = g.governor().stats();
    assert_eq!(s.last_hour(ApiClass::Background).calls, 3);
    assert_eq!(s.last_hour(ApiClass::Background).units, 180);
    assert_eq!(s.last_hour(ApiClass::Interactive).calls, 1);
}

#[tokio::test(start_paused = true)]
async fn priority_lets_a_lease_call_ahead_of_waiting_background_calls() {
    let limits = Limits {
        concurrency: 1,
        ..Limits::unlimited()
    };
    let (fake, g) = governed(FakeBackend::drive_like(), limits);
    fake.set_latency(Duration::from_secs(1));
    let a = pid(&fake, "a.json");
    let order = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let run = |class: ApiClass, tag: &'static str| {
        let (g, a, order) = (g.clone(), a.clone(), order.clone());
        tokio::spawn(scoped(class, async move {
            g.stat(&a).await.unwrap();
            order.lock().push(tag);
        }))
    };
    let first = run(ApiClass::Background, "b1");
    tokio::task::yield_now().await;
    let b2 = run(ApiClass::Background, "b2");
    tokio::task::yield_now().await;
    let lease = run(ApiClass::Lease, "lease");
    for h in [first, b2, lease] {
        h.await.unwrap();
    }
    assert_eq!(*order.lock(), vec!["b1", "lease", "b2"]);
}

#[tokio::test(start_paused = true)]
async fn auth_revoked_means_needs_reauth_and_no_more_calls() {
    let (fake, g) = governed(FakeBackend::drive_like(), Limits::unlimited());
    let a = pid(&fake, "a.json");
    fake.fail_next(FakeOp::Stat, BErr::AuthRevoked);
    assert_eq!(g.stat(&a).await, Err(BErr::AuthRevoked));
    assert_eq!(g.governor().health(), Some(Health::NeedsReauth));
    fake.reset_calls();
    assert_eq!(g.stat(&a).await, Err(BErr::AuthRevoked));
    assert_eq!(read_all(&g, &a).await, Err(BErr::AuthRevoked));
    assert_eq!(upload(&g, &fake, "x").await, Err(BErr::AuthRevoked));
    assert_eq!(
        scoped(ApiClass::Interactive, g.account()).await.err(),
        Some(BErr::AuthRevoked)
    );
    for op in [FakeOp::Stat, FakeOp::Read, FakeOp::Upload, FakeOp::Account] {
        assert_eq!(fake.calls(op), 0, "{op:?}");
    }
    g.governor().reauthorized();
    assert!(g.stat(&a).await.unwrap().is_some());
    assert_eq!(g.governor().health(), None);
}

#[tokio::test(start_paused = true)]
async fn storage_full_stops_writes_until_a_probe_succeeds() {
    let limits = Limits {
        storage_probe: Duration::from_secs(300),
        ..Limits::unlimited()
    };
    let (fake, g) = governed(FakeBackend::drive_like(), limits);
    let a = pid(&fake, "a.json");
    fake.fail_next(FakeOp::Upload, BErr::StorageFull);
    assert_eq!(upload(&g, &fake, "x").await, Err(BErr::StorageFull));
    assert_eq!(
        g.governor().health(),
        Some(Health::Blocked(Blocked::StorageFull))
    );
    fake.reset_calls();
    // Writes that need space fail without a call; reads and deletes go on.
    assert_eq!(upload(&g, &fake, "y").await, Err(BErr::StorageFull));
    assert_eq!(fake.calls(FakeOp::Upload), 0);
    assert_eq!(read_all(&g, &a).await.unwrap(), b"{\"a\":1}");
    // After the probe interval one write probes; it succeeds and writes resume.
    tokio::time::advance(Duration::from_secs(301)).await;
    upload(&g, &fake, "z").await.unwrap();
    assert_eq!(g.governor().health(), None);
    upload(&g, &fake, "w").await.unwrap();
    // A failed probe doubles the interval.
    fake.fail_next(FakeOp::Upload, BErr::StorageFull);
    let _ = upload(&g, &fake, "v").await;
    tokio::time::advance(Duration::from_secs(301)).await;
    fake.fail_next(FakeOp::Upload, BErr::StorageFull);
    assert_eq!(upload(&g, &fake, "v").await, Err(BErr::StorageFull));
    tokio::time::advance(Duration::from_secs(301)).await;
    let n = fake.calls(FakeOp::Upload);
    assert_eq!(upload(&g, &fake, "v").await, Err(BErr::StorageFull));
    assert_eq!(
        fake.calls(FakeOp::Upload),
        n,
        "still inside the doubled interval"
    );
}

#[tokio::test(start_paused = true)]
async fn a_transfer_quota_pauses_downloads_only() {
    let (fake, g) = governed(FakeBackend::mega_like(), Limits::mega());
    let a = pid(&fake, "a.json");
    fake.fail_next(
        FakeOp::Read,
        BErr::TransferQuota(Some(Duration::from_secs(1800))),
    );
    assert!(matches!(
        read_all(&g, &a).await,
        Err(BErr::TransferQuota(_))
    ));
    assert_eq!(
        g.governor().health(),
        Some(Health::Blocked(Blocked::TransferQuota))
    );
    fake.reset_calls();
    match read_all(&g, &a).await {
        Err(BErr::TransferQuota(Some(d))) => assert!(d > Duration::from_secs(1700)),
        other => panic!("{other:?}"),
    }
    assert_eq!(fake.calls(FakeOp::Read), 0);
    // Metadata and uploads are not affected.
    assert!(g.stat(&a).await.unwrap().is_some());
    upload(&g, &fake, "x").await.unwrap();
    // The re-probe after 30 min hits the quota again: the wait doubles to 1 h.
    tokio::time::advance(Duration::from_secs(1801)).await;
    fake.fail_next(
        FakeOp::Read,
        BErr::TransferQuota(Some(Duration::from_secs(1800))),
    );
    assert!(read_all(&g, &a).await.is_err());
    assert_eq!(fake.calls(FakeOp::Read), 1);
    tokio::time::advance(Duration::from_secs(1801)).await;
    assert!(read_all(&g, &a).await.is_err());
    assert_eq!(fake.calls(FakeOp::Read), 1, "inside the doubled wait");
    tokio::time::advance(Duration::from_secs(1800)).await;
    assert_eq!(read_all(&g, &a).await.unwrap(), b"{\"a\":1}");
    assert_eq!(g.governor().health(), None);
}

#[tokio::test(start_paused = true)]
async fn a_daily_limit_blocks_every_class() {
    let (fake, g) = governed(FakeBackend::drive_like(), Limits::unlimited());
    let a = pid(&fake, "a.json");
    fake.fail_next(
        FakeOp::Stat,
        BErr::DailyLimit(Some(Duration::from_secs(3600))),
    );
    let _ = g.stat(&a).await;
    assert_eq!(
        g.governor().health(),
        Some(Health::Blocked(Blocked::DailyLimit))
    );
    assert!(matches!(
        scoped(ApiClass::Interactive, g.stat(&a)).await,
        Err(BErr::DailyLimit(Some(_)))
    ));
    let t0 = Instant::now();
    g.stat(&a).await.unwrap();
    assert!(t0.elapsed() >= Duration::from_secs(3599));
}

#[tokio::test(start_paused = true)]
async fn api_stats_count_calls_units_and_bytes_per_class() {
    let (fake, g) = governed(FakeBackend::drive_like(), Limits::unlimited());
    let a = pid(&fake, "a.json");
    scoped(ApiClass::Interactive, read_all(&g, &a))
        .await
        .unwrap();
    scoped(ApiClass::Ocr, upload(&g, &fake, "x")).await.unwrap();
    g.stat(&a).await.unwrap();
    fake.fail_next(FakeOp::Stat, BErr::Throttled(Some(Duration::from_secs(1))));
    let _ = g.stat(&a).await;
    let s = g.governor().stats();
    let i = s.last_hour(ApiClass::Interactive);
    assert_eq!((i.calls, i.bytes, i.errors), (1, 7, 0));
    let o = s.last_hour(ApiClass::Ocr);
    assert_eq!((o.calls, o.bytes), (1, 5));
    let b = s.last_hour(ApiClass::Background);
    assert_eq!((b.calls, b.errors, b.throttled), (2, 1, 1));
    assert_eq!(s.total_day().calls, 4);
    assert_eq!(
        s.total_hour().units,
        4,
        "one unit per call (no driver counter)"
    );
    // An hour later the hour is empty, the day is not; a day later both are.
    tokio::time::advance(Duration::from_secs(3601)).await;
    let s = g.governor().stats();
    assert_eq!(s.total_hour().calls, 0);
    assert_eq!(s.total_day().calls, 4);
    tokio::time::advance(Duration::from_secs(24 * 3600)).await;
    assert_eq!(g.governor().stats().total_day().calls, 0);
}

// ---- the store's side (real time) ---------------------------------------------------

struct S {
    _tmp: TempDir,
    root: PathBuf,
    fake: Arc<FakeBackend>,
    store: Arc<RemoteStore<FakeBackend>>,
    _mount: MountGuard,
}

fn store(fake: FakeBackend, tune: impl FnOnce(&mut RemoteStoreConfig)) -> S {
    fake.reader_mkdir(&format!("{FOLDER}/S"));
    fake.reader_put(&format!("{FOLDER}/S/a.json"), b"{\"a\":1}");
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("cloud/s/library");
    let mut cfg = RemoteStoreConfig::new(&root, tmp.path().join("cloud/s"), FOLDER);
    cfg.poll_active = Duration::from_secs(3600);
    cfg.poll_idle = Duration::from_secs(3600);
    cfg.lease = false;
    cfg.retry_base = Duration::from_millis(20);
    tune(&mut cfg);
    let fake = Arc::new(fake);
    let store = RemoteStore::open_shared(cfg, fake.clone()).unwrap();
    let mount = bunko_store::mount(store.clone() as Arc<dyn LibraryStore>);
    assert!(store.wait_steady(Duration::from_secs(20)));
    // The first adoption runs right after the first sync (health `Syncing` meanwhile):
    // every test starts from `Online`.
    let t0 = std::time::Instant::now();
    while !store.adopted() || store.health() != Health::Online {
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "{:?}",
            store.health()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    S {
        _tmp: tmp,
        root,
        fake,
        store,
        _mount: mount,
    }
}

impl S {
    fn abs(&self, rel: &str) -> PathBuf {
        let mut p = self.root.clone();
        for s in rel.split('/') {
            p.push(s);
        }
        p
    }

    fn local(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self._tmp.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }
}

#[test]
fn interactive_reads_answer_unavailable_with_the_delay_instead_of_waiting() {
    let s = store(FakeBackend::drive_like(), |_| {});
    s.fake.throttle_for(Duration::from_secs(30));
    let t0 = std::time::Instant::now();
    // The first read meets the 429 (or the pause a feed poll already met); later
    // ones do not even reach the provider.
    let mut reads = Vec::new();
    for _ in 0..3 {
        let e = bunko_store::interactive(|| fs::read(s.abs("S/a.json"))).unwrap_err();
        assert!(bunko_store::is_unavailable(&e), "{e}");
        let d = bunko_store::retry_after(&e).unwrap();
        assert!(d > Duration::from_secs(25), "{d:?}");
        reads.push(s.fake.calls(FakeOp::Read));
    }
    assert!(
        reads[0] <= 1 && reads[1] == reads[0] && reads[2] == reads[0],
        "{reads:?}"
    );
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert!(matches!(s.store.health(), Health::Degraded { .. }));
    assert!(!s.store.accepting_claims());
    // Metadata still answers from the index.
    assert!(fs::metadata(s.abs("S/a.json")).is_ok());
}

#[test]
fn a_full_provider_keeps_deferred_writes_in_the_outbox_and_recovers() {
    let s = store(FakeBackend::drive_like(), |c| {
        c.limits = Some(Limits {
            storage_probe: Duration::from_millis(300),
            storage_probe_max: Duration::from_millis(300),
            ..Limits::unlimited()
        });
    });
    s.fake.set_quota(Some(10));
    let w = WriteIntent::new(Mode::CreateNew, Origin::Thumbs, Durability::Deferred);
    let src = s.local("cover", b"a cover larger than the quota");
    fs::publish(&src, s.abs("S/cover.webp"), &w).unwrap();
    let t0 = std::time::Instant::now();
    while s.store.health() != Health::Blocked(Blocked::StorageFull) {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "{:?}",
            s.store.health()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
    let ops = s.store.outbox();
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_ne!(ops[0].state, "FAILED");
    assert!(
        fs::exists(s.abs("S/cover.webp")),
        "still visible while queued"
    );
    // A Sync write hears 507: at once while blocked, or from the provider itself when
    // it lands in a probe's window (it does not fit the quota either way).
    let src = s.local("x", b"a sync write larger than the quota");
    let sync = WriteIntent::new(
        Mode::Replace { base: None },
        Origin::Dav { user: None },
        Durability::Sync,
    );
    let e = fs::publish(&src, s.abs("S/x.json"), &sync).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::StorageFull, "{e}");
    // Space again: the next probe lands the queued write.
    s.fake.set_quota(None);
    assert!(s.store.wait_outbox_idle(Duration::from_secs(20)));
    assert!(s.fake.node_at(&format!("{FOLDER}/S/cover.webp")).is_some());
    assert_eq!(s.store.health(), Health::Online);
}

#[test]
fn the_outbox_limits_hold_claims_and_refuse_deferred_writes() {
    let s = store(FakeBackend::drive_like(), |c| {
        c.outbox_hold_bytes = 10;
        c.outbox_max_bytes = 30;
    });
    let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let g = gate.clone();
    s.store.set_lease_gate(Arc::new(move || {
        g.load(std::sync::atomic::Ordering::Acquire)
    }));
    assert!(s.store.accepting_claims());
    let w = WriteIntent::new(Mode::CreateNew, Origin::Thumbs, Durability::Deferred);
    let src = s.local("a", &[1u8; 20]);
    fs::publish(&src, s.abs("S/a.webp"), &w).unwrap();
    assert_eq!(s.store.outbox_bytes(), 20);
    assert!(!s.store.accepting_claims(), "above outbox_hold");
    let src = s.local("b", &[2u8; 20]);
    let e = fs::publish(&src, s.abs("S/b.webp"), &w).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::StorageFull, "{e}");
    assert!(src.exists(), "the caller keeps its bytes");
    gate.store(true, std::sync::atomic::Ordering::Release);
    assert!(s.store.wait_outbox_idle(Duration::from_secs(20)));
    assert!(s.store.accepting_claims());
}

#[test]
fn a_revoked_grant_stops_the_feed_the_outbox_and_reads_until_reauthorized() {
    let s = store(FakeBackend::drive_like(), |_| {});
    s.fake.fail_next(FakeOp::Changes, BErr::AuthRevoked);
    s.store.feed().poll_now();
    let t0 = std::time::Instant::now();
    while s.store.health() != Health::NeedsReauth {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "{:?}",
            s.store.health()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    s.fake.reset_calls();
    let w = WriteIntent::new(Mode::CreateNew, Origin::Thumbs, Durability::Deferred);
    let src = s.local("cover", b"webp");
    fs::publish(&src, s.abs("S/cover.webp"), &w).unwrap();
    assert!(fs::read(s.abs("S/a.json")).is_err());
    s.store.feed().poll_now();
    std::thread::sleep(Duration::from_millis(300));
    for op in [
        FakeOp::Changes,
        FakeOp::FindNamed,
        FakeOp::Upload,
        FakeOp::Read,
    ] {
        assert_eq!(s.fake.calls(op), 0, "{op:?}");
    }
    assert!(!s.store.accepting_claims());
    s.store.reauthorized();
    assert!(s.store.wait_outbox_idle(Duration::from_secs(20)));
    assert!(s.fake.node_at(&format!("{FOLDER}/S/cover.webp")).is_some());
    assert!(s.store.wait_steady(Duration::from_secs(20)));
    assert_eq!(s.store.health(), Health::Online);
}

#[test]
fn store_calls_are_logged_per_class() {
    let s = store(FakeBackend::drive_like(), |_| {});
    let before = s.store.api_stats();
    bunko_store::interactive(|| fs::read(s.abs("S/a.json"))).unwrap();
    let after = s.store.api_stats();
    let i = after.last_hour(ApiClass::Interactive);
    assert_eq!(i.calls, before.last_hour(ApiClass::Interactive).calls + 1);
    assert_eq!(i.bytes, 7);
    // The feed's first sync ran in the background class.
    assert!(after.last_day(ApiClass::Background).calls >= 3);
}
