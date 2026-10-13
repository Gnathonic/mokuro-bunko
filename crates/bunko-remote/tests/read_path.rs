//! The RemoteStore read path (plan M2 task 4; spec §3.1, §5.2, §5.5, §5.6, D24):
//! a store over FakeBackend, mounted, driven through `bunko_store::fs` from std
//! threads, with real temp dirs.

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier, mpsc};
use std::time::{Duration, Instant};

use bunko_library::archive::zipdir::{open_member, read_central_directory};
use bunko_remote::fake::{FakeBackend, FakeOp};
use bunko_remote::{BErr, RemoteStore, RemoteStoreConfig};
use bunko_store::{LibraryStore, Meta, MountGuard, WatchEvent, fs};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const FOLDER: &str = "mokuro-reader";
const MOUNT_ID: u64 = 7;
const WAIT: Duration = Duration::from_secs(20);

struct T {
    _tmp: TempDir,
    root: PathBuf,
    store: Arc<RemoteStore<FakeBackend>>,
    _mount: MountGuard,
}

impl T {
    fn fake(&self) -> &FakeBackend {
        self.store.backend()
    }

    fn abs(&self, rel: &str) -> PathBuf {
        let mut p = self.root.clone();
        for s in rel.split('/').filter(|s| !s.is_empty()) {
            p.push(s);
        }
        p
    }

    /// The reader writes `rel` (under the root folder); the feed polls and the index
    /// shows the new version.
    fn reader_replace(&self, rel: &str, bytes: &[u8]) {
        let n = self.fake().reader_put(&format!("{FOLDER}/{rel}"), bytes);
        self.sync_to(rel, &n.cver);
    }

    fn sync_to(&self, rel: &str, cver: &str) {
        let p = self.abs(rel);
        self.store.feed().poll_now();
        wait_until(|| {
            fs::metadata(&p)
                .ok()
                .is_some_and(|m| m.version().map(|v| v.as_str()) == Some(cver))
        });
    }
}

fn wait_until(mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < WAIT, "timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn config(tmp: &TempDir) -> RemoteStoreConfig {
    let mut cfg = RemoteStoreConfig::new(
        tmp.path().join("cloud/s/library"),
        tmp.path().join("cloud/s"),
        FOLDER,
    );
    // The feed polls only when asked to.
    cfg.poll_active = Duration::from_secs(3600);
    cfg.poll_idle = Duration::from_secs(3600);
    cfg.mount_id = MOUNT_ID;
    cfg
}

fn start(
    fake: FakeBackend,
    files: &[(&str, &[u8])],
    tune: impl FnOnce(&mut RemoteStoreConfig),
) -> T {
    for (rel, bytes) in files {
        fake.reader_put(&format!("{FOLDER}/{rel}"), bytes);
    }
    let tmp = TempDir::new().unwrap();
    let mut cfg = config(&tmp);
    tune(&mut cfg);
    let root = cfg.root.clone();
    let store = RemoteStore::open(cfg, fake).unwrap();
    assert!(store.wait_steady(WAIT), "{:?}", store.engine_state());
    let mount = bunko_store::mount(store.clone() as Arc<dyn LibraryStore>);
    T {
        _tmp: tmp,
        root,
        store,
        _mount: mount,
    }
}

/// Deterministic incompressible bytes.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// A stored zip of `n` members of `each` bytes: `(bytes, members)`.
fn archive(n: usize, each: usize, seed: u64) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
    let members: Vec<(String, Vec<u8>)> = (0..n)
        .map(|i| (format!("{i:03}.jpg"), noise(each, seed + i as u64)))
        .collect();
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, data) in &members {
        w.start_file(name.as_str(), opts).unwrap();
        w.write_all(data).unwrap();
    }
    (w.finish().unwrap().into_inner(), members)
}

const SIDECAR: &[u8] = br#"{"version":"0.2.1","pages":[]}"#;

fn tree(zip: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("Series A/vol1.cbz", zip.to_vec()),
        ("Series A/vol1.mokuro", SIDECAR.to_vec()),
        ("Series A/cover.webp", noise(70_000, 1)),
        ("Series B/vol2.mokuro", br#"{"pages":[1]}"#.to_vec()),
        ("series.txt", b"top".to_vec()),
    ]
}

fn start_tree(fake: FakeBackend, zip: &[u8]) -> T {
    let files = tree(zip);
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (*p, b.as_slice())).collect();
    start(fake, &refs, |_| {})
}

type Profile = fn() -> FakeBackend;

fn profiles() -> [(&'static str, Profile); 2] {
    [
        ("drive_like", FakeBackend::drive_like as Profile),
        ("webdav_cas", FakeBackend::webdav_cas),
    ]
}

fn remote(m: &Meta) -> &bunko_store::RemoteMeta {
    match m {
        Meta::Remote(r) => r,
        Meta::Std(_) => panic!("a local Meta from the remote store"),
    }
}

fn berr(e: &std::io::Error) -> Option<&BErr> {
    e.get_ref().and_then(|x| x.downcast_ref::<BErr>())
}

#[test]
fn metadata_and_read_dir_match_the_fake_tree() {
    let (zip, _) = archive(3, 2 << 20, 10);
    for (name, mk) in profiles() {
        let t = start_tree(mk(), &zip);
        let root = fs::metadata(&t.root).unwrap();
        assert!(root.is_dir(), "{name}");
        let mut inos = std::collections::HashSet::new();
        for (rel, bytes) in tree(&zip) {
            let p = t.abs(rel);
            let node = t.fake().node_at(&format!("{FOLDER}/{rel}")).unwrap();
            let m = fs::metadata(&p).unwrap();
            let r = remote(&m);
            assert!(
                m.is_file() && !m.is_dir() && !m.is_symlink(),
                "{name} {rel}"
            );
            assert_eq!(m.len(), bytes.len() as u64, "{name} {rel}");
            assert_eq!(r.dev, MOUNT_ID);
            assert!(inos.insert(r.ino), "{name}: ino reused");
            assert_eq!(m.version().map(|v| v.as_str()), Some(node.cver.as_str()));
            assert!(!m.own());
            // D3: the listed seconds are what the reader compares.
            let (secs, _) = m.unix_mtime().unwrap();
            assert_eq!(secs, node.base_mtime_ms.div_euclid(1000), "{name} {rel}");
            assert_eq!(remote(&fs::symlink_metadata(&p).unwrap()), r);
            assert_eq!(fs::canonicalize(&p).unwrap(), p);
            let link = fs::read_link(&p).unwrap_err();
            assert_eq!(link.kind(), std::io::ErrorKind::InvalidInput);
        }
        // The root lists its folders and files by name, with their metadata.
        let top: Vec<(String, bool)> = fs::read_dir(&t.root)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().into_string().unwrap(),
                    e.file_type().unwrap().is_dir(),
                )
            })
            .collect();
        assert_eq!(
            top,
            vec![
                ("Series A".to_string(), true),
                ("Series B".to_string(), true),
                ("series.txt".to_string(), false)
            ],
            "{name}"
        );
        let a: Vec<(String, u64, PathBuf, u64)> = fs::read_dir(t.abs("Series A"))
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let m = e.metadata().unwrap();
                let ino = remote(&m).ino;
                (e.file_name().into_string().unwrap(), m.len(), e.path(), ino)
            })
            .collect();
        let names: Vec<&str> = a.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(names, ["cover.webp", "vol1.cbz", "vol1.mokuro"], "{name}");
        for (n, len, path, ino) in &a {
            assert_eq!(*path, t.abs(&format!("Series A/{n}")));
            let m = fs::metadata(path).unwrap();
            assert_eq!((m.len(), remote(&m).ino), (*len, *ino));
        }
        let dir = fs::metadata(t.abs("Series A")).unwrap();
        assert!(dir.is_dir() && remote(&dir).dev == MOUNT_ID);
        assert_eq!(
            fs::read_dir(t.abs("series.txt")).err().unwrap().kind(),
            std::io::ErrorKind::NotADirectory
        );
        // Lookup is byte-exact; the case winner answers a folded request.
        assert!(!fs::exists(t.abs("series a")));
        assert_eq!(
            t.store.case_winner(&t.root, "series a").as_deref(),
            Some("Series A")
        );
        assert_eq!(t.store.case_winner(&t.root, "nothing"), None);
        // Mutations wait for the write path; an indexed folder already exists.
        fs::create_dir_all(t.abs("Series A")).unwrap();
        assert_eq!(
            fs::write(t.abs("Series A/x.txt"), b"x").unwrap_err().kind(),
            std::io::ErrorKind::Unsupported
        );
        assert!(t.store.caps().remote && t.store.caps().case_sensitive);
        assert!(!t.store.caps().local_paths && !t.store.caps().symlinks);
        assert_eq!(t.store.health(), bunko_store::Health::Online);
    }
}

#[test]
fn paths_outside_the_root_are_not_found() {
    let t = start(FakeBackend::webdav_cas(), &[("S/a.mokuro", b"{}")], |_| {});
    let nf = std::io::ErrorKind::NotFound;
    for p in [
        PathBuf::from("/elsewhere/a.mokuro"),
        t.root.join("..").join("library").join("S"),
        t.root.join("S").join("..").join("S"),
        t.root.join("missing"),
        t.root.join("S/a.mokuro/deeper"),
        t.root.parent().unwrap().join("library2/S"),
    ] {
        assert_eq!(
            t.store.metadata(&p).unwrap_err().kind(),
            nf,
            "{}",
            p.display()
        );
        assert_eq!(t.store.open(&p).unwrap_err().kind(), nf, "{}", p.display());
        assert!(t.store.read_dir(&p).is_err(), "{}", p.display());
    }
    // The synthetic root never exists on disk.
    assert!(!t.root.exists());
}

#[test]
fn small_file_hits_the_backend_once_then_the_cache() {
    for (name, mk) in profiles() {
        let t = start(mk(), &[("S/vol.mokuro", SIDECAR)], |_| {});
        let p = t.abs("S/vol.mokuro");
        t.fake().reset_calls();
        assert_eq!(fs::read(&p).unwrap(), SIDECAR, "{name}");
        assert_eq!(t.fake().calls(FakeOp::Read), 1, "{name}");
        assert_eq!(fs::read_to_string(&p).unwrap().as_bytes(), SIDECAR);
        let mut f = fs::File::open(&p).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        assert_eq!(s.as_bytes(), SIDECAR);
        let node = t.fake().node_at(&format!("{FOLDER}/S/vol.mokuro")).unwrap();
        assert_eq!(
            f.metadata().unwrap().version().map(|v| v.as_str()),
            Some(node.cver.as_str())
        );
        let sha: [u8; 32] = Sha256::digest(SIDECAR).into();
        assert_eq!(t.store.content_sha256(&p).unwrap(), Some(sha));
        assert_eq!(
            t.fake().calls(FakeOp::Read),
            1,
            "{name}: served from the cache"
        );
        assert_eq!(t.store.cache_stats().blobs, 1);

        // A reader replace: the next read fetches the new version, once.
        let v2 = br#"{"version":"0.2.1","pages":[{"img_width":1}]}"#;
        t.reader_replace("S/vol.mokuro", v2);
        assert_eq!(fs::read(&p).unwrap(), v2, "{name}");
        assert_eq!(fs::read(&p).unwrap(), v2, "{name}");
        assert_eq!(t.fake().calls(FakeOp::Read), 2, "{name}");
        // The old file's version-bound handle keeps its metadata.
        assert_eq!(
            f.metadata().unwrap().version().map(|v| v.as_str()),
            Some(node.cver.as_str())
        );
    }
}

#[test]
fn archive_reads_are_ranged_and_pinned_and_reuse_the_zipdir() {
    let (zip, members) = archive(4, (3 << 20) / 2, 20);
    assert!(zip.len() > 4 << 20);
    for (name, mk) in profiles() {
        let t = start(mk(), &[("S/v.cbz", &zip)], |_| {});
        let p = t.abs("S/v.cbz");
        let node = t.fake().node_at(&format!("{FOLDER}/S/v.cbz")).unwrap();
        t.fake().reset_calls();
        let mut f = fs::File::open(&p).unwrap();
        assert_eq!(
            t.fake().calls(FakeOp::Read),
            0,
            "{name}: opening reads nothing"
        );
        let entries = read_central_directory(&mut f).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.filename.as_str()).collect();
        let want: Vec<&str> = members.iter().map(|m| m.0.as_str()).collect();
        assert_eq!(names, want, "{name}");
        let cd_reads = t.fake().read_log();
        assert_eq!(cd_reads.len(), 1, "{name}: one tail read: {cd_reads:?}");
        assert!(
            cd_reads
                .iter()
                .all(|r| r.range.is_some() && r.pin == node.pin)
        );

        // One member: ranged reads only, far less than the archive.
        let e = &entries[2];
        let mut got = Vec::new();
        open_member(&p, e).unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got, members[2].1, "{name}");
        let log = t.fake().read_log();
        let member_reads = &log[cd_reads.len()..];
        assert!(!member_reads.is_empty());
        let mut fetched = 0u64;
        for r in member_reads {
            let (first, last) = r
                .range
                .unwrap_or_else(|| panic!("{name}: a full read: {r:?}"));
            assert_eq!(r.pin, node.pin);
            fetched += last.unwrap() + 1 - first;
        }
        assert!(
            fetched < (zip.len() as u64) / 2,
            "{name}: fetched {fetched}"
        );
        assert_eq!(t.store.cache_stats().spools, 0, "{name}: no download");

        // Reading it again costs nothing; so does listing the archive again.
        t.fake().reset_calls();
        let mut again = Vec::new();
        open_member(&p, e).unwrap().read_to_end(&mut again).unwrap();
        assert_eq!(again, members[2].1);
        let mut f2 = fs::File::open(&p).unwrap();
        assert_eq!(
            read_central_directory(&mut f2).unwrap().len(),
            members.len()
        );
        assert_eq!(t.fake().calls(FakeOp::Read), 0, "{name}: caches reused");
        assert_eq!(t.store.cache_stats().zipdirs, 1);
    }
}

#[test]
fn zipdir_cache_survives_a_restart() {
    let (zip, members) = archive(3, 2 << 20, 30);
    let fake = FakeBackend::webdav_cas();
    fake.reader_put(&format!("{FOLDER}/S/v.cbz"), &zip);
    let tmp = TempDir::new().unwrap();
    {
        let store = RemoteStore::open(config(&tmp), fake).unwrap();
        assert!(store.wait_steady(WAIT));
        let p = store.root().join("S/v.cbz");
        let mut f = store.open(&p).unwrap();
        assert_eq!(read_central_directory(&mut f).unwrap().len(), members.len());
    }
    // Same dir, a fresh provider with the same bytes and versions.
    let fake = FakeBackend::webdav_cas();
    fake.reader_put(&format!("{FOLDER}/S/v.cbz"), &zip);
    let store = RemoteStore::open(config(&tmp), fake).unwrap();
    assert!(store.wait_steady(WAIT));
    store.backend().reset_calls();
    let mut f = store.open(&store.root().join("S/v.cbz")).unwrap();
    assert_eq!(read_central_directory(&mut f).unwrap().len(), members.len());
    assert_eq!(store.backend().calls(FakeOp::Read), 0);
}

#[test]
fn replace_between_directory_and_member_read_is_caught() {
    let (zip, members) = archive(3, 2 << 20, 40);
    let (zip2, members2) = archive(3, 2 << 20, 50);
    for (name, mk) in profiles() {
        let t = start(mk(), &[("S/v.cbz", &zip)], |_| {});
        let p = t.abs("S/v.cbz");
        let mut f = fs::File::open(&p).unwrap();
        let entries = read_central_directory(&mut f).unwrap();
        let before = t.store.cache_stats();
        // The reader replaces the archive; the index has not seen it yet.
        let n2 = t.fake().reader_put(&format!("{FOLDER}/S/v.cbz"), &zip2);
        let r = open_member(&p, &entries[1]).and_then(|mut m| {
            let mut v = Vec::new();
            m.read_to_end(&mut v)?;
            Ok(v)
        });
        match name {
            // Drive keeps the old revision readable by its pin: still consistent.
            "drive_like" => assert_eq!(r.unwrap(), members[1].1),
            _ => {
                let e = match r.unwrap_err() {
                    bunko_library::archive::zipdir::ZipError::Io(e) => e,
                    other => panic!("{name}: {other:?}"),
                };
                assert_eq!(e.kind(), std::io::ErrorKind::Other);
                assert_eq!(berr(&e), Some(&BErr::PreconditionFailed), "{name}: {e}");
                assert_eq!(t.store.cache_stats(), before, "{name}: caches untouched");
            }
        }
        // Once the feed sees the new version, reads follow it.
        t.sync_to("S/v.cbz", &n2.cver);
        let mut f = fs::File::open(&p).unwrap();
        let entries = read_central_directory(&mut f).unwrap();
        let mut v = Vec::new();
        open_member(&p, &entries[1])
            .unwrap()
            .read_to_end(&mut v)
            .unwrap();
        assert_eq!(v, members2[1].1, "{name}");
    }
}

#[test]
fn unpinned_reads_recheck_the_version() {
    let (zip, _) = archive(3, 2 << 20, 60);
    let t = start(
        FakeBackend::webdav_plain(),
        &[("S/v.cbz", &zip), ("S/a.mokuro", SIDECAR)],
        |_| {},
    );
    let p = t.abs("S/v.cbz");
    let mut f = fs::File::open(&p).unwrap();
    let entries = read_central_directory(&mut f).unwrap();
    assert!(
        t.fake().calls(FakeOp::Stat) >= 1,
        "the tail read was re-checked"
    );
    // A small file replaced behind the index's back.
    let n = t.fake().reader_put(&format!("{FOLDER}/S/a.mokuro"), b"{}");
    let e = fs::read(t.abs("S/a.mokuro")).unwrap_err();
    assert_eq!(berr(&e), Some(&BErr::PreconditionFailed));
    t.sync_to("S/a.mokuro", &n.cver);
    assert_eq!(fs::read(t.abs("S/a.mokuro")).unwrap(), b"{}");
    // An archive member.
    let before = t.store.cache_stats();
    t.fake()
        .reader_put(&format!("{FOLDER}/S/v.cbz"), &archive(3, 2 << 20, 61).0);
    let e = open_member(&p, &entries[0]).map(|_| ()).unwrap_err();
    let bunko_library::archive::zipdir::ZipError::Io(e) = e else {
        panic!("{e:?}")
    };
    assert_eq!(berr(&e), Some(&BErr::PreconditionFailed));
    assert_eq!(t.store.cache_stats(), before);
}

#[test]
fn local_path_spools_once_for_concurrent_callers() {
    let (zip, members) = archive(3, 2 << 20, 70);
    let t = start(FakeBackend::drive_like(), &[("S/v.cbz", &zip)], |_| {});
    let p = t.abs("S/v.cbz");
    t.fake().set_latency(Duration::from_millis(150));
    t.fake().reset_calls();
    let go = Arc::new(Barrier::new(2));
    let callers: Vec<_> = (0..2)
        .map(|_| {
            let (store, p, go) = (t.store.clone(), p.clone(), go.clone());
            std::thread::spawn(move || {
                go.wait();
                store.local_path(&p, None).unwrap()
            })
        })
        .collect();
    let pins: Vec<_> = callers.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(t.fake().calls(FakeOp::Read), 1, "one download");
    assert_eq!(t.fake().read_log()[0].range, None);
    assert_eq!(pins[0].path(), pins[1].path());
    let on_disk = std::fs::read(pins[0].path()).unwrap();
    assert_eq!(on_disk.len(), zip.len());
    assert_eq!(on_disk, zip, "verified bytes");
    assert_eq!(
        t.store.content_sha256(&p).unwrap(),
        Some(Sha256::digest(&zip).into())
    );
    let stats = t.store.cache_stats();
    assert_eq!((stats.spools, stats.spool_bytes), (1, zip.len() as u64));

    // A zip reader now uses the spool: no network at all.
    t.fake().set_latency(Duration::ZERO);
    t.fake().reset_calls();
    let mut f = fs::File::open(&p).unwrap();
    let entries = read_central_directory(&mut f).unwrap();
    let mut v = Vec::new();
    open_member(&p, &entries[2])
        .unwrap()
        .read_to_end(&mut v)
        .unwrap();
    assert_eq!(v, members[2].1);
    assert_eq!(t.fake().calls(FakeOp::Read), 0);

    // The pin keeps the file while held.
    let path = pins[0].path().to_path_buf();
    drop(pins);
    let held = t.store.local_path(&p, None).unwrap();
    assert_eq!(held.path(), path);
    assert!(path.exists());
    assert_eq!(t.fake().calls(FakeOp::Read), 0, "reused");

    // An expected version that is not the indexed one.
    let e = t
        .store
        .local_path(&p, Some(&bunko_store::Version::new("c-other")))
        .err()
        .unwrap();
    assert_eq!(berr(&e), Some(&BErr::PreconditionFailed));
}

#[test]
fn sequential_reader_streams_through_one_download() {
    let (zip, _) = archive(3, 2 << 20, 80);
    let t = start(FakeBackend::webdav_cas(), &[("S/v.cbz", &zip)], |_| {});
    let p = t.abs("S/v.cbz");
    t.fake().reset_calls();
    let readers: Vec<_> = (0..2)
        .map(|_| {
            let p = p.clone();
            std::thread::spawn(move || {
                let mut v = Vec::new();
                fs::File::open(&p).unwrap().read_to_end(&mut v).unwrap();
                v
            })
        })
        .collect();
    for r in readers {
        assert_eq!(r.join().unwrap(), zip);
    }
    let log = t.fake().read_log();
    assert_eq!(
        log.iter().filter(|r| r.range.is_none()).count(),
        1,
        "{log:?}"
    );
}

#[test]
fn spool_budget_admission() {
    let (a, _) = archive(3, 2 << 20, 90);
    let (b, _) = archive(3, 2 << 20, 91);
    let (big, _) = archive(5, 2 << 20, 92);
    let budget = a.len().max(b.len()) as u64 + 1024;
    let t = start(
        FakeBackend::webdav_cas(),
        &[("S/a.cbz", &a), ("S/b.cbz", &b), ("S/big.cbz", &big)],
        |cfg| {
            cfg.spool_bytes = budget;
            cfg.spool_wait = Duration::from_millis(300);
        },
    );
    let pa = t.store.local_path(&t.abs("S/a.cbz"), None).unwrap();
    // No room while `a` is pinned: the second spool waits, then gives up.
    let t0 = Instant::now();
    let e = t.store.local_path(&t.abs("S/b.cbz"), None).err().unwrap();
    assert_eq!(e.kind(), std::io::ErrorKind::StorageFull, "{e}");
    assert!(t0.elapsed() >= Duration::from_millis(250));
    // Larger than the whole budget: refused at once.
    let t0 = Instant::now();
    let e = t.store.local_path(&t.abs("S/big.cbz"), None).err().unwrap();
    assert_eq!(e.kind(), std::io::ErrorKind::StorageFull);
    assert!(t0.elapsed() < Duration::from_millis(250));
    // Released while a waiter waits: it is admitted, evicting `a`.
    let waiter = {
        let (store, p) = (t.store.clone(), t.abs("S/b.cbz"));
        std::thread::spawn(move || store.local_path(&p, None).map(|lp| lp.path().to_path_buf()))
    };
    std::thread::sleep(Duration::from_millis(50));
    let a_path = pa.path().to_path_buf();
    drop(pa);
    let b_path = waiter.join().unwrap().unwrap();
    assert_eq!(std::fs::read(&b_path).unwrap(), b);
    assert!(!a_path.exists(), "a was evicted");
    assert_eq!(t.store.cache_stats().spools, 1);
    // A whole-file reader falls back to ranged reads while the spool is full.
    let held = t.store.local_path(&t.abs("S/b.cbz"), None).unwrap();
    t.fake().reset_calls();
    let mut v = Vec::new();
    fs::File::open(t.abs("S/a.cbz"))
        .unwrap()
        .read_to_end(&mut v)
        .unwrap();
    assert_eq!(v, a);
    assert!(t.fake().read_log().iter().all(|r| r.range.is_some()));
    drop(held);
}

#[test]
fn watch_events_arrive_for_a_reader_upload() {
    let t = start(
        FakeBackend::drive_like(),
        &[("S/a.mokuro", SIDECAR)],
        |_| {},
    );
    let (tx, rx) = mpsc::channel();
    let guard = t
        .store
        .watch(Arc::new(move |ev: WatchEvent| {
            // Sinks run off the store runtime: they may read through the shim.
            let read = ev.path().filter(|p| p.extension().is_some()).map(fs::read);
            let _ = tx.send((ev, read.map(|r| r.map_err(|e| e.to_string()))));
        }))
        .unwrap();
    t.fake()
        .reader_put(&format!("{FOLDER}/S/new.mokuro"), b"{\"n\":1}");
    t.store.feed().poll_now();
    let want = t.abs("S/new.mokuro");
    let t0 = Instant::now();
    loop {
        let (ev, read) = rx.recv_timeout(WAIT - t0.elapsed()).expect("an event");
        if ev
            == (WatchEvent::Added {
                path: want.clone(),
                is_dir: false,
            })
        {
            assert_eq!(read, Some(Ok(b"{\"n\":1}".to_vec())));
            break;
        }
    }
    // After the guard is gone, nothing more arrives.
    drop(guard);
    while rx.try_recv().is_ok() {}
    t.fake()
        .reader_put(&format!("{FOLDER}/S/later.mokuro"), b"{}");
    t.store.feed().poll_now();
    wait_until(|| fs::exists(t.abs("S/later.mokuro")));
    std::thread::sleep(Duration::from_millis(100));
    assert!(rx.try_recv().is_err());
}

#[test]
fn content_methods_inside_an_async_task_fail_clearly() {
    let t = start(
        FakeBackend::webdav_cas(),
        &[("S/a.mokuro", SIDECAR), ("S/b.mokuro", SIDECAR)],
        |_| {},
    );
    let (a, b) = (t.abs("S/a.mokuro"), t.abs("S/b.mokuro"));
    // `a` is cached, so reading it would not need the network: debug builds still
    // refuse it at once. `b` needs the network, which tokio refuses in any build.
    assert_eq!(fs::read(&a).unwrap(), SIDECAR);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let in_task = |p: &Path| {
        let p = p.to_path_buf();
        rt.block_on(async move {
            let r = std::panic::catch_unwind(move || fs::read(&p));
            let payload = r.expect_err("a content call inside a task must panic");
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap()
        })
    };
    let mut cases = vec![in_task(&b)];
    if cfg!(debug_assertions) {
        cases.push(in_task(&a));
    }
    for msg in cases {
        assert!(msg.contains("inside an async task"), "{msg}");
        assert!(msg.contains("spawn_blocking"), "{msg}");
    }
    // From spawn_blocking it is fine; so are metadata calls inside the task.
    let got = rt.block_on(async {
        assert!(fs::metadata(&b).unwrap().is_file());
        let b = b.clone();
        tokio::task::spawn_blocking(move || fs::read(&b))
            .await
            .unwrap()
    });
    assert_eq!(got.unwrap(), SIDECAR);
}

struct Hooks(mpsc::Sender<(String, PathBuf, PathBuf)>);

impl bunko_remote::CloudHooks for Hooks {
    fn moved(&self, from: &Path, to: &Path, _is_dir: bool) {
        let thread = std::thread::current().name().unwrap_or("").to_string();
        let _ = self.0.send((thread, from.to_path_buf(), to.to_path_buf()));
    }
    fn removed(&self, _path: &Path, _is_dir: bool) {}
    fn sidecar_overwritten(&self, _path: &Path) {}
    fn series_submission(&self, _dir: &Path, _bytes: &[u8]) -> bunko_remote::SubmissionResult {
        bunko_remote::SubmissionResult::Applied
    }
    fn ocr_discarded(&self, _path: &Path) {}
}

#[test]
fn cloud_hooks_installed_late_get_earlier_notifications_on_the_events_thread() {
    let t = start(
        FakeBackend::drive_like(),
        &[("S/a.mokuro", SIDECAR)],
        |_| {},
    );
    t.fake()
        .reader_rename(&format!("{FOLDER}/S"), &format!("{FOLDER}/S2"));
    t.store.feed().poll_now();
    wait_until(|| fs::exists(t.abs("S2/a.mokuro")));
    let (tx, rx) = mpsc::channel();
    t.store.set_hooks(Arc::new(Hooks(tx)));
    let got = rx.recv_timeout(WAIT).unwrap();
    assert_eq!(
        got,
        ("bunko-store-events".to_string(), t.abs("S"), t.abs("S2"))
    );
    t.fake()
        .reader_rename(&format!("{FOLDER}/S2"), &format!("{FOLDER}/S3"));
    t.store.feed().poll_now();
    let got = rx.recv_timeout(WAIT).unwrap();
    assert_eq!(
        got,
        ("bunko-store-events".to_string(), t.abs("S2"), t.abs("S3"))
    );
}

#[test]
fn store_dir_layout() {
    let t = start(
        FakeBackend::webdav_cas(),
        &[("S/a.mokuro", SIDECAR)],
        |_| {},
    );
    fs::read(t.abs("S/a.mokuro")).unwrap();
    let dir: &Path = &t.store.config().dir;
    assert!(dir.join("index.sqlite").is_file());
    assert!(dir.join("blobs").is_dir() && dir.join("spool").is_dir());
    let blob = hex::encode(Sha256::digest(SIDECAR));
    assert_eq!(
        std::fs::read(dir.join("blobs").join(&blob[..2]).join(&blob)).unwrap(),
        SIDECAR
    );
}
