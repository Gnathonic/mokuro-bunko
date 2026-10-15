//! The marker lease (spec §9.2, D9): FakeBackend, paused tokio time, two Bunkos on one
//! provider whose local clocks are ±10 min off the provider's.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bunko_remote::backend::{Backend, OpCtx, Pid, RNode, UploadSource, UploadTarget};
use bunko_remote::fake::{EPOCH_MS, FakeBackend, FakeOp};
use bunko_remote::status::{
    Heartbeat, Lease, LeaseConfig, LeaseEvent, LeaseRecordState, LeaseState, LeaseStore,
    MARKER_NAME, MEGA_TTL, Marker, MarkerState, MemoryLeaseStore, SqliteLeaseStore, WallClock,
    format_iso_ms, parse_iso_ms,
};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::time::Instant;

const TEN_MIN_MS: i64 = 10 * 60 * 1000;

/// A local clock `skew_ms` off the fake provider's (both run on tokio time).
struct Skewed {
    t0: Instant,
    skew_ms: i64,
}

impl WallClock for Skewed {
    fn now_ms(&self) -> i64 {
        EPOCH_MS + self.t0.elapsed().as_millis() as i64 + self.skew_ms
    }
}

fn clock(skew_ms: i64) -> Arc<Skewed> {
    Arc::new(Skewed {
        t0: Instant::now(),
        skew_ms,
    })
}

fn cfg(id: &str, boot: &str) -> LeaseConfig {
    let mut c = LeaseConfig::new(id, boot, format!("host-{id}"), "0.8.0");
    c.feed_interval = Duration::from_secs(30);
    c
}

struct Env {
    fake: Arc<FakeBackend>,
    root: Pid,
    scratch: tempfile::TempDir,
}

impl Env {
    async fn new(fake: FakeBackend) -> Env {
        let fake = Arc::new(fake);
        let root = fake.create_root("mokuro-reader").await.unwrap().pid;
        Env {
            fake,
            root,
            scratch: tempfile::tempdir().unwrap(),
        }
    }

    fn lease(
        &self,
        cfg: LeaseConfig,
        skew_ms: i64,
        store: Arc<dyn LeaseStore>,
    ) -> Lease<FakeBackend> {
        let dir = self.scratch.path().join(&cfg.boot_id);
        Lease::new(self.fake.clone(), self.root.clone(), cfg, store, dir).with_clock(clock(skew_ms))
    }

    fn lease_mem(&self, cfg: LeaseConfig, skew_ms: i64) -> Lease<FakeBackend> {
        self.lease(cfg, skew_ms, Arc::new(MemoryLeaseStore::new()))
    }

    /// Every marker node, as a reader would list them.
    async fn markers(&self) -> Vec<(RNode, Marker)> {
        let (nodes, _) = self.fake.find_named(&self.root, MARKER_NAME).await.unwrap();
        nodes
            .into_iter()
            .map(|n| {
                let m = Marker::parse(&self.fake.content(&n.pid).unwrap()).unwrap();
                (n, m)
            })
            .collect()
    }

    /// Fresh active markers by the provider clock.
    async fn fresh(&self) -> Vec<Marker> {
        let now = self.fake.now_ms();
        self.markers()
            .await
            .into_iter()
            .filter(|(n, m)| {
                m.state == MarkerState::Active && now - n.base_mtime_ms < m.ttl_s as i64 * 1000
            })
            .map(|(_, m)| m)
            .collect()
    }

    /// Upload a foreign marker as a new node (a duplicate name on Drive and MEGA).
    async fn inject(&self, m: &Marker) -> RNode {
        let bytes = serde_json::to_vec(m).unwrap();
        let p = self.scratch.path().join(format!("inject-{}.json", m.epoch));
        std::fs::write(&p, &bytes).unwrap();
        self.fake
            .upload(
                UploadTarget::New {
                    parent: &self.root,
                    name: MARKER_NAME,
                },
                UploadSource::File {
                    path: Path::new(&p),
                    size: bytes.len() as u64,
                    sha256: Sha256::digest(&bytes).into(),
                },
                &mut OpCtx::default(),
            )
            .await
            .unwrap()
    }
}

fn foreign(id: &str, epoch: u64, now_ms: i64) -> Marker {
    Marker {
        version: 1,
        kind: "mokuro-bunko".into(),
        instance_id: id.into(),
        boot_id: format!("{id}-boot"),
        instance_name: format!("host-{id}"),
        bunko_version: "0.8.0".into(),
        epoch,
        claimed_at: format_iso_ms(now_ms),
        heartbeat_at: format_iso_ms(now_ms),
        ttl_s: 900,
        state: MarkerState::Active,
        compiles: vec![],
        ocr: true,
        queue: None,
        took_over_from: None,
    }
}

fn holder_of(s: &LeaseState) -> Option<&Marker> {
    match s {
        LeaseState::Holder => None,
        LeaseState::Standby { holder } => Some(holder),
    }
}

type Profile = (&'static str, fn() -> FakeBackend);

fn profiles() -> Vec<Profile> {
    vec![
        ("drive_like", FakeBackend::drive_like),
        ("webdav_plain", FakeBackend::webdav_plain),
        ("webdav_cas", FakeBackend::webdav_cas),
        ("onedrive_like", FakeBackend::onedrive_like),
        ("mega_like", FakeBackend::mega_like),
    ]
}

#[tokio::test(start_paused = true)]
async fn simultaneous_claims_leave_exactly_one_holder() {
    for (name, make) in profiles() {
        let env = Env::new(make()).await;
        // Every call yields, so both listings see an empty set.
        env.fake.set_latency(Duration::from_millis(50));
        let mut a = env.lease_mem(cfg("aaaa", "a-boot"), TEN_MIN_MS);
        let mut b = env.lease_mem(cfg("bbbb", "b-boot"), -TEN_MIN_MS);
        let (ra, rb) = tokio::join!(a.claim(), b.claim());
        let (ra, rb) = (ra.unwrap(), rb.unwrap());
        let holders = [&ra, &rb]
            .iter()
            .filter(|s| ***s == LeaseState::Holder)
            .count();
        assert_eq!(holders, 1, "{name}: {ra:?} / {rb:?}");
        let (holder, standby, standby_state) = if ra == LeaseState::Holder {
            (&mut a, &mut b, rb)
        } else {
            (&mut b, &mut a, ra)
        };
        assert_eq!(
            holder_of(&standby_state).unwrap().instance_id,
            holder.config().instance_id,
            "{name}"
        );
        // Both really claimed: two marker writes before anyone decided.
        assert!(env.fake.calls(FakeOp::Upload) >= 2, "{name}");
        // On duplicate-name providers the loser deleted its own separate node; where
        // names are unique there was only ever one node and nothing is deleted.
        let duplicates = matches!(name, "drive_like" | "mega_like");
        assert_eq!(
            env.fake.deleted_log().len(),
            usize::from(duplicates),
            "{name}"
        );
        // One heartbeat later there is still exactly one holder, and exactly one
        // fresh marker (a loser on a duplicate-name provider deleted its node).
        assert_eq!(holder.heartbeat().await.unwrap(), Heartbeat::Held, "{name}");
        assert!(standby.heartbeat().await.is_err(), "{name}");
        let fresh = env.fresh().await;
        assert_eq!(fresh.len(), 1, "{name}: {fresh:?}");
        assert_eq!(fresh[0].instance_id, holder.config().instance_id, "{name}");
        assert_eq!(fresh[0].epoch, 1, "{name}");
        assert_eq!(holder.record().lease_state, LeaseRecordState::Holder);
        assert_eq!(standby.record().lease_state, LeaseRecordState::Standby);
    }
}

#[tokio::test(start_paused = true)]
async fn restart_with_same_boot_lineage_resumes_at_once() {
    let env = Env::new(FakeBackend::drive_like()).await;
    let store: Arc<dyn LeaseStore> = Arc::new(MemoryLeaseStore::new());
    let mut a = env.lease(cfg("aaaa", "boot-1"), 0, store.clone());
    assert_eq!(a.claim().await.unwrap(), LeaseState::Holder);
    let first = env.markers().await;
    assert_eq!(first.len(), 1);
    drop(a);

    tokio::time::advance(Duration::from_secs(120)).await;
    let mut c = cfg("aaaa", "boot-2");
    c.last_boot_id = store.load().unwrap().boot_id().map(str::to_string);
    assert_eq!(c.last_boot_id.as_deref(), Some("boot-1"));
    let mut a2 = env.lease(c, 0, store.clone());
    let t = Instant::now();
    assert_eq!(a2.claim().await.unwrap(), LeaseState::Holder);
    assert!(t.elapsed() < Duration::from_secs(1), "resume must not wait");
    let after = env.markers().await;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].0.pid, first[0].0.pid, "rewritten in place");
    assert_eq!(after[0].1.boot_id, "boot-2");
    assert_eq!(after[0].1.epoch, first[0].1.epoch);
    assert_eq!(after[0].1.claimed_at, first[0].1.claimed_at);
    assert!(a2.fence_ok(EPOCH_MS + 120_000));
    assert_eq!(store.load().unwrap().boot_id(), Some("boot-2"));
}

#[tokio::test(start_paused = true)]
async fn cloned_data_folder_is_treated_as_foreign() {
    let env = Env::new(FakeBackend::drive_like()).await;
    let mut a = env.lease_mem(cfg("aaaa", "boot-1"), 0);
    assert_eq!(a.claim().await.unwrap(), LeaseState::Holder);
    // Same instance id (a copied state.sqlite), a boot id this run does not know.
    let mut c = cfg("aaaa", "boot-clone");
    c.last_boot_id = Some("boot-other".into());
    let mut clone = env.lease_mem(c, 0);
    let s = clone.claim().await.unwrap();
    let holder = holder_of(&s).expect("standby");
    assert_eq!(
        (holder.instance_id.as_str(), holder.boot_id.as_str()),
        ("aaaa", "boot-1")
    );
    assert_eq!(env.markers().await.len(), 1, "the clone wrote nothing");
    assert_eq!(a.heartbeat().await.unwrap(), Heartbeat::Held);
}

#[tokio::test(start_paused = true)]
async fn take_over_wins_regardless_of_clocks() {
    for (name, make) in profiles() {
        let env = Env::new(make()).await;
        // The holder's clock is behind, the taker's ahead: on claimed_at alone the
        // holder would rank first.
        let mut a = env.lease_mem(cfg("aaaa", "a-boot"), -TEN_MIN_MS);
        let mut b = env.lease_mem(cfg("bbbb", "b-boot"), TEN_MIN_MS);
        assert_eq!(a.claim().await.unwrap(), LeaseState::Holder, "{name}");
        let s = b.claim().await.unwrap();
        assert_eq!(holder_of(&s).unwrap().instance_id, "aaaa", "{name}");
        assert_eq!(b.take_over().await.unwrap(), LeaseState::Holder, "{name}");
        let bm = b.marker().unwrap().clone();
        let am_claimed = env
            .markers()
            .await
            .into_iter()
            .find(|(_, m)| m.instance_id == "aaaa")
            .map(|(_, m)| m.claimed_at);
        if let Some(at) = am_claimed {
            assert!(parse_iso_ms(&bm.claimed_at) > parse_iso_ms(&at), "{name}");
        }
        assert_eq!(bm.epoch, 2, "{name}");
        assert_eq!(bm.took_over_from.as_deref(), Some("aaaa"), "{name}");
        match a.heartbeat().await.unwrap() {
            Heartbeat::Lost { holder } => assert_eq!(holder.instance_id, "bbbb", "{name}"),
            other => panic!("{name}: {other:?}"),
        }
        assert!(!a.fence_ok(EPOCH_MS - TEN_MIN_MS), "{name}");
        assert_eq!(b.heartbeat().await.unwrap(), Heartbeat::Held, "{name}");
        let top = env.fresh().await;
        assert!(top.iter().all(|m| m.epoch <= 2), "{name}");
        assert!(top.iter().any(|m| m.instance_id == "bbbb"), "{name}");
    }
}

#[tokio::test(start_paused = true)]
async fn stale_holder_returning_steps_down_on_first_heartbeat() {
    let env = Env::new(FakeBackend::drive_like()).await;
    let mut a = env.lease_mem(cfg("aaaa", "a-boot"), TEN_MIN_MS);
    let mut b = env.lease_mem(cfg("bbbb", "b-boot"), -TEN_MIN_MS);
    assert_eq!(a.claim().await.unwrap(), LeaseState::Holder);
    let s = b.claim().await.unwrap();
    assert!(holder_of(&s).is_some());
    // A sleeps past its TTL; B finds no fresh marker and claims with a higher epoch.
    tokio::time::advance(Duration::from_secs(901)).await;
    assert_eq!(b.claim().await.unwrap(), LeaseState::Holder);
    assert_eq!(b.marker().unwrap().epoch, 2);
    match a.heartbeat().await.unwrap() {
        Heartbeat::Lost { holder } => assert_eq!(holder.instance_id, "bbbb"),
        other => panic!("{other:?}"),
    }
    assert_eq!(a.record().lease_state, LeaseRecordState::Lost);
    // A wrote nothing on the way down: its node is still the stale epoch-1 marker.
    let fresh = env.fresh().await;
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].instance_id, "bbbb");
    assert!(a.heartbeat().await.is_err(), "no heartbeat after a loss");
}

#[tokio::test(start_paused = true)]
async fn released_marker_is_not_fresh() {
    let env = Env::new(FakeBackend::webdav_cas()).await;
    let mut a = env.lease_mem(cfg("aaaa", "a-boot"), 0);
    assert_eq!(a.claim().await.unwrap(), LeaseState::Holder);
    a.release().await.unwrap();
    assert_eq!(a.record().lease_state, LeaseRecordState::Released);
    assert!(!a.fence_ok(EPOCH_MS + 60_000));
    let all = env.markers().await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].1.state, MarkerState::Released);
    assert!(env.fresh().await.is_empty());
    // Well within the TTL, a second Bunko claims as if no marker were there.
    let mut b = env.lease_mem(cfg("bbbb", "b-boot"), 0);
    assert_eq!(b.claim().await.unwrap(), LeaseState::Holder);
    assert_eq!(b.marker().unwrap().epoch, 2, "max(seen) + 1");
}

#[tokio::test(start_paused = true)]
async fn duplicate_marker_nodes_on_drive_are_all_considered() {
    let env = Env::new(FakeBackend::drive_like()).await;
    let mut a = env.lease_mem(cfg("aaaa", "a-boot"), 0);
    assert_eq!(a.claim().await.unwrap(), LeaseState::Holder);
    // A duplicate-name node with a higher epoch, created after ours (the index would
    // pick it, but the protocol must not depend on that) …
    env.inject(&foreign("zzzz", 5, env.fake.now_ms())).await;
    match a.heartbeat().await.unwrap() {
        Heartbeat::Lost { holder } => assert_eq!(holder.epoch, 5),
        other => panic!("{other:?}"),
    }

    // … and one shadowed by a newer, lower-epoch duplicate.
    let env = Env::new(FakeBackend::drive_like()).await;
    env.inject(&foreign("hhhh", 7, env.fake.now_ms())).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    env.inject(&foreign("llll", 1, env.fake.now_ms())).await;
    assert_eq!(env.markers().await.len(), 2);
    let mut c = env.lease_mem(cfg("cccc", "c-boot"), 0);
    let s = c.claim().await.unwrap();
    assert_eq!(holder_of(&s).unwrap().instance_id, "hhhh");
    assert_eq!(env.markers().await.len(), 2, "standby writes nothing");
}

#[tokio::test(start_paused = true)]
async fn mega_heartbeat_writes_a_new_node_and_deletes_the_previous_one() {
    let env = Env::new(FakeBackend::mega_like()).await;
    let mut c = cfg("aaaa", "a-boot");
    c.ttl = MEGA_TTL;
    c.heartbeat_new_node = true;
    let mut a = env.lease_mem(c, 0);
    assert_eq!(a.claim().await.unwrap(), LeaseState::Holder);
    let before = env.markers().await;
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].1.ttl_s, 3600);
    let old = before[0].0.pid.clone();
    tokio::time::advance(Duration::from_secs(1200)).await;
    assert_eq!(a.heartbeat().await.unwrap(), Heartbeat::Held);
    let after = env.markers().await;
    assert_eq!(after.len(), 1, "{after:?}");
    assert_ne!(after[0].0.pid, old);
    assert_eq!(env.fake.deleted_log(), vec![old]);
    assert_eq!(
        a.record().marker_pid.as_deref(),
        Some(after[0].0.pid.as_str())
    );
    // Again: the node written by the first heartbeat goes.
    let second = after[0].0.pid.clone();
    assert_eq!(a.heartbeat().await.unwrap(), Heartbeat::Held);
    assert_eq!(env.fake.deleted_log().last(), Some(&second));
    assert_eq!(env.markers().await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn fence_expires_half_a_ttl_after_the_last_heartbeat() {
    let env = Env::new(FakeBackend::onedrive_like()).await;
    let clk = clock(TEN_MIN_MS);
    let mut a = Lease::new(
        env.fake.clone(),
        env.root.clone(),
        cfg("aaaa", "a-boot"),
        Arc::new(MemoryLeaseStore::new()),
        env.scratch.path().join("a"),
    )
    .with_clock(clk.clone());
    let fence = a.fence();
    assert!(!fence.ok(), "no lease yet");
    assert_eq!(a.claim().await.unwrap(), LeaseState::Holder);
    assert!(fence.ok() && a.fence_ok(clk.now_ms()));
    tokio::time::advance(Duration::from_secs(449)).await;
    assert!(fence.ok(), "within ttl/2");
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!fence.ok(), "past ttl/2 without a heartbeat");
    assert!(!a.fence_ok(clk.now_ms()));
    assert_eq!(a.heartbeat().await.unwrap(), Heartbeat::Held);
    assert!(fence.ok(), "renewed");
}

#[tokio::test(start_paused = true)]
async fn sqlite_store_persists_the_lease() {
    let env = Env::new(FakeBackend::drive_like()).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let open = || {
        let conn = rusqlite::Connection::open(&path).unwrap();
        Arc::new(SqliteLeaseStore::new(Arc::new(Mutex::new(conn))).unwrap())
    };
    let store = open();
    assert_eq!(store.load(), None);
    let mut a = env.lease(cfg("aaaa", "boot-1"), 0, store.clone());
    assert_eq!(a.claim().await.unwrap(), LeaseState::Holder);
    let rec = a.record();
    drop(a);
    drop(store);
    let store = open();
    let loaded = store.load().unwrap();
    assert_eq!(loaded, rec);
    assert_eq!(loaded.lease_state, LeaseRecordState::Holder);
    assert_eq!(loaded.lease_epoch, 1);
    assert_eq!(loaded.boot_id(), Some("boot-1"));
    assert_eq!(
        loaded.marker_pid.as_deref(),
        Some(env.markers().await[0].0.pid.as_str())
    );
    let mut c = cfg("aaaa", "boot-2");
    c.last_boot_id = loaded.boot_id().map(str::to_string);
    let mut a2 = env.lease(c, 0, store.clone());
    assert_eq!(a2.claim().await.unwrap(), LeaseState::Holder);
    assert_eq!(env.markers().await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn background_tasks_settle_on_one_holder_and_hand_over() {
    let env = Env::new(FakeBackend::drive_like()).await;
    env.fake.set_latency(Duration::from_millis(20));
    let ha = env.lease_mem(cfg("aaaa", "a-boot"), TEN_MIN_MS).spawn();
    let hb = env.lease_mem(cfg("bbbb", "b-boot"), -TEN_MIN_MS).spawn();
    tokio::time::sleep(Duration::from_secs(120)).await;
    let (ea, eb) = (ha.event(), hb.event());
    let acquired = [&ea, &eb]
        .iter()
        .filter(|e| ***e == LeaseEvent::Acquired)
        .count();
    assert_eq!(acquired, 1, "{ea:?} / {eb:?}");
    let (holder, standby) = if ea == LeaseEvent::Acquired {
        (&ha, &hb)
    } else {
        (&hb, &ha)
    };
    assert!(matches!(standby.event(), LeaseEvent::Standby { .. }));
    assert!(holder.fence().ok() && !standby.fence().ok());
    // Several heartbeats later nothing has changed.
    tokio::time::sleep(Duration::from_secs(3 * 300 + 10)).await;
    assert_eq!(holder.event(), LeaseEvent::Acquired);
    assert!(holder.fence().ok());
    assert_eq!(env.fresh().await.len(), 1);

    // Take over through the handle: the old holder sees it on its next heartbeat.
    assert_eq!(standby.take_over().await.unwrap(), LeaseState::Holder);
    assert!(standby.fence().ok());
    tokio::time::sleep(Duration::from_secs(310)).await;
    assert!(
        matches!(
            holder.event(),
            LeaseEvent::Lost { .. } | LeaseEvent::Standby { .. }
        ),
        "{:?}",
        holder.event()
    );
    assert!(!holder.fence().ok());
    assert_eq!(standby.event(), LeaseEvent::Acquired);

    // Disconnect: released, and the other Bunko claims within one standby round.
    standby.release().await.unwrap();
    assert_eq!(standby.event(), LeaseEvent::Released);
    assert!(!standby.fence().ok());
    tokio::time::sleep(Duration::from_secs(400)).await;
    assert_eq!(holder.event(), LeaseEvent::Acquired);
    assert!(holder.fence().ok());
    let calls = env.fake.calls(FakeOp::Upload);
    assert!(calls > 0);
    ha.shutdown().await;
    hb.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_run_stopped_during_the_claim_settle_resumes_on_restart() {
    let env = Env::new(FakeBackend::drive_like()).await;
    let store: Arc<dyn LeaseStore> = Arc::new(MemoryLeaseStore::new());
    let mut a = env.lease(cfg("aaaa", "boot-1"), 0, store.clone());
    // Stopped mid-settle (60 s): the marker is written, the claim not yet decided.
    assert!(
        tokio::time::timeout(Duration::from_secs(5), a.claim())
            .await
            .is_err()
    );
    drop(a);
    let first = env.markers().await;
    assert_eq!(first.len(), 1);
    assert_eq!(
        (first[0].1.boot_id.as_str(), first[0].1.epoch),
        ("boot-1", 1)
    );
    // The boot id and epoch were recorded before the marker write.
    let rec = store.load().unwrap();
    assert_eq!((rec.boot_id(), rec.lease_epoch), (Some("boot-1"), 1));

    // The next run knows nothing but its record (no `last_boot_id`).
    let mut a2 = env.lease(cfg("aaaa", "boot-2"), 0, store.clone());
    let t = Instant::now();
    assert_eq!(a2.claim().await.unwrap(), LeaseState::Holder);
    assert!(t.elapsed() < Duration::from_secs(1), "resume must not wait");
    let after = env.markers().await;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].0.pid, first[0].0.pid, "rewritten in place");
    assert_eq!(
        (after[0].1.boot_id.as_str(), after[0].1.epoch),
        ("boot-2", 1)
    );
    let rec = store.load().unwrap();
    assert_eq!(rec.lease_state, LeaseRecordState::Holder);
    assert_eq!((rec.boot_id(), rec.lease_epoch), (Some("boot-2"), 1));
    assert!(rec.boot_ids.contains(&"boot-1".to_string()));
}

#[tokio::test(start_paused = true)]
async fn a_record_from_another_data_folder_is_not_ours() {
    let env = Env::new(FakeBackend::drive_like()).await;
    let store = Arc::new(MemoryLeaseStore::new());
    let mut c = cfg("aaaa", "boot-1");
    c.home = Some("/data/a".into());
    let mut a = env.lease(c, 0, store.clone());
    assert_eq!(a.claim().await.unwrap(), LeaseState::Holder);

    // A copy of state.sqlite in another folder, while the first one runs.
    let copy = Arc::new(MemoryLeaseStore::new());
    copy.save(&store.load().unwrap()).unwrap();
    let mut c = cfg("aaaa", "boot-clone");
    c.home = Some("/data/b".into());
    let mut clone = env.lease(c, 0, copy.clone());
    let s = clone.claim().await.unwrap();
    let holder = holder_of(&s).expect("standby");
    assert_eq!(holder.boot_id, "boot-1");
    assert_eq!(env.markers().await.len(), 1, "the clone wrote nothing");
    let copied = copy.load().unwrap();
    assert_eq!(copied.home.as_deref(), Some("/data/b"));
    assert!(!copied.boot_ids.contains(&"boot-1".to_string()));
    assert_eq!(a.heartbeat().await.unwrap(), Heartbeat::Held);

    // The same folder (spelled in another case) is this install's restart.
    let mut c = cfg("aaaa", "boot-2");
    c.home = Some("/DATA/A".into());
    let mut a2 = env.lease(c, 0, store.clone());
    assert_eq!(a2.claim().await.unwrap(), LeaseState::Holder);
}

#[tokio::test(start_paused = true)]
async fn a_twin_that_resumed_our_marker_makes_us_step_down() {
    let env = Env::new(FakeBackend::drive_like()).await;
    let mut a = env.lease_mem(cfg("aaaa", "boot-1"), 0);
    assert_eq!(a.claim().await.unwrap(), LeaseState::Holder);
    // A clone the folder check cannot see (the same path on another host) resumes it.
    let mut c = cfg("aaaa", "boot-twin");
    c.last_boot_id = Some("boot-1".into());
    let mut twin = env.lease_mem(c, 0);
    assert_eq!(twin.claim().await.unwrap(), LeaseState::Holder);
    match a.heartbeat().await.unwrap() {
        Heartbeat::Lost { holder } => assert_eq!(holder.boot_id, "boot-twin"),
        h => panic!("{h:?}"),
    }
    assert!(holder_of(&a.claim().await.unwrap()).is_some(), "standby");
    assert_eq!(twin.heartbeat().await.unwrap(), Heartbeat::Held);
    let fresh = env.fresh().await;
    assert_eq!(fresh.len(), 1, "{fresh:?}");
    assert_eq!(fresh[0].boot_id, "boot-twin");
}
