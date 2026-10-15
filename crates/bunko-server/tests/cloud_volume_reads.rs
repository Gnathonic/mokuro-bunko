//! What the per-volume background readers cost on a cloud library: a cover, a page
//! count, a compiled catalog entry of one large archive must be a few ranged reads
//! (the central directory at the tail plus, for a cover, one member), never the whole
//! archive. A store over the Drive-like `FakeBackend`, mounted, driven exactly as the
//! server drives it (`thumbs::cover_image`, `owed::archive_pages`,
//! `archive::reader_image_names`, `compiler::compile_volume`); the bytes asked of the
//! backend come from its read log.

// Fixtures are checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bunko_remote::fake::{FakeBackend, FakeRead};
use bunko_remote::{RemoteStore, RemoteStoreConfig};
use bunko_server::thumbs::{Cover, cover_image};
use bunko_store::{LibraryStore, MountGuard};
use tempfile::TempDir;

const FOLDER: &str = "mokuro-reader";
const WAIT: Duration = Duration::from_secs(20);
const MIB: u64 = 1 << 20;

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

/// A realistic volume: 40 pages of 1 MiB, alternately stored and deflated, written
/// out of name order (the cover is the first SORTED image name, not the first
/// member), plus a small non-image member.
fn volume(seed: u64) -> (Vec<u8>, Vec<u8>) {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let mut cover = Vec::new();
    for i in (0..40u64).rev() {
        let method = if i % 2 == 0 {
            zip::CompressionMethod::Stored
        } else {
            zip::CompressionMethod::Deflated
        };
        let opts = zip::write::SimpleFileOptions::default().compression_method(method);
        let data = noise(MIB as usize, seed + i);
        w.start_file(format!("Vol/{:03}.jpg", i + 1), opts).unwrap();
        w.write_all(&data).unwrap();
        if i == 0 {
            cover = data;
        }
    }
    w.start_file("Vol/info.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    w.write_all(b"scanlated").unwrap();
    (w.finish().unwrap().into_inner(), cover)
}

struct T {
    _tmp: TempDir,
    root: PathBuf,
    store: Arc<RemoteStore<FakeBackend>>,
    _mount: MountGuard,
}

impl T {
    fn start(files: &[(&str, &[u8])]) -> T {
        let fake = FakeBackend::drive_like();
        for (rel, bytes) in files {
            fake.reader_put(&format!("{FOLDER}/{rel}"), bytes);
        }
        let tmp = TempDir::new().unwrap();
        let mut cfg = RemoteStoreConfig::new(
            tmp.path().join("cloud/s/library"),
            tmp.path().join("cloud/s"),
            FOLDER,
        );
        cfg.poll_active = Duration::from_secs(3600);
        cfg.poll_idle = Duration::from_secs(3600);
        cfg.lease = false;
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

    /// Run `f`; the reads it asked of the backend.
    fn measure<R>(&self, f: impl FnOnce() -> R) -> (R, Vec<FakeRead>) {
        self.store.backend().reset_calls();
        let out = f();
        (out, self.store.backend().read_log())
    }
}

fn fetched(name: &str, log: &[FakeRead], archive: u64) -> u64 {
    log.iter()
        .map(|r| match r.range {
            Some((first, Some(last))) => last + 1 - first,
            Some((first, None)) => archive - first,
            None => panic!("{name}: a whole-file read: {log:?}"),
        })
        .sum()
}

#[test]
fn a_cloud_cover_costs_the_directory_and_one_member() {
    let (zip, cover) = volume(1);
    let size = zip.len() as u64;
    assert!(size > 40 * MIB);
    let t = T::start(&[("S/v1.cbz", &zip)]);
    let p = t.root.join("S/v1.cbz");
    let (got, log) = t.measure(|| cover_image(&p));
    let bytes = fetched("cover", &log, size);
    eprintln!(
        "cover: {} reads, {bytes} bytes of a {size}-byte archive",
        log.len()
    );
    match got {
        Cover::Image(b) => assert!(b == cover, "the first sorted image"),
        Cover::None(why) => panic!("no cover: {why}"),
    }
    assert!(
        bytes < 3 * MIB + cover.len() as u64,
        "a cover fetched {bytes} bytes in {} reads: {log:?}",
        log.len()
    );
    assert_eq!(t.store.cache_stats().spools, 0, "no download");
}

#[test]
fn cloud_page_counts_and_catalog_entries_cost_the_directory_only() {
    let (a, _) = volume(100);
    let (b, _) = volume(200);
    let (c, _) = volume(300);
    let size = a.len() as u64;
    let t = T::start(&[("S/a.cbz", &a), ("S/b.cbz", &b), ("S/c.cbz", &c)]);

    let p = t.root.join("S/a.cbz");
    let (pages, log) = t.measure(|| bunko_server::ocr::owed::archive_pages(&p));
    assert_eq!(pages, Some(40));
    let bytes = fetched("archive_pages", &log, size);
    eprintln!("archive_pages: {} reads, {bytes} bytes", log.len());
    assert!(bytes < 3 * MIB, "archive_pages fetched {bytes}: {log:?}");

    let p = t.root.join("S/b.cbz");
    let (names, log) = t.measure(|| bunko_library::archive::reader_image_names(&p));
    assert_eq!(names.map(|n| n.len()), Some(40));
    let bytes = fetched("reader_image_names", &log, size);
    eprintln!("reader_image_names: {} reads, {bytes} bytes", log.len());
    assert!(
        bytes < 3 * MIB,
        "reader_image_names fetched {bytes}: {log:?}"
    );

    let p = t.root.join("S/c.cbz");
    let ((entry, cacheable), log) =
        t.measure(|| bunko_library::compiler::compile_volume("S", &p, None, None));
    assert!(cacheable);
    assert_eq!(entry.page_count, 40);
    let bytes = fetched("compile_volume", &log, size);
    eprintln!("compile_volume: {} reads, {bytes} bytes", log.len());
    assert!(bytes < 3 * MIB, "compile_volume fetched {bytes}: {log:?}");
    assert_eq!(t.store.cache_stats().spools, 0, "no download");
}

/// The cloud cover is the member a local library picks: names out of order, a
/// duplicate name (the last entry wins), a non-image member sorting first.
#[test]
fn a_cloud_cover_is_the_member_a_local_cover_is() {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    for (name, seed) in [
        ("b/002.png", 1u64),
        ("a/001.JPG", 2),
        ("a/000.txt", 3),
        ("c/000.jpg", 4),
        // Renamed to a second `a/001.JPG` below (the writer refuses duplicates).
        ("a/001.JPX", 5),
    ] {
        w.start_file(name, opts).unwrap();
        w.write_all(&noise(300_000, seed)).unwrap();
    }
    let mut zip = w.finish().unwrap().into_inner();
    assert_eq!(
        rename_all(&mut zip, b"a/001.JPX", b"a/001.JPG"),
        2,
        "local header and central directory"
    );

    let local = TempDir::new().unwrap();
    let lp = local.path().join("v.cbz");
    std::fs::write(&lp, &zip).unwrap();
    let Cover::Image(want) = cover_image(&lp) else {
        panic!("no local cover")
    };
    assert!(
        want == noise(300_000, 5),
        "the zip crate keeps the last duplicate"
    );

    let t = T::start(&[("S/v.cbz", &zip)]);
    let Cover::Image(got) = cover_image(&t.root.join("S/v.cbz")) else {
        panic!("no cloud cover")
    };
    assert!(got == want, "the same member");
}

fn rename_all(bytes: &mut [u8], from: &[u8], to: &[u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i + from.len() <= bytes.len() {
        if &bytes[i..i + from.len()] == from {
            bytes[i..i + from.len()].copy_from_slice(to);
            n += 1;
            i += from.len();
        } else {
            i += 1;
        }
    }
    n
}
