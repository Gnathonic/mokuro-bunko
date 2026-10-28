//! What the per-volume background readers cost on a cloud library: a cover, a page
//! count, a compiled catalog entry of one large archive must be a few ranged reads
//! (the central directory at the tail plus, for a cover, one member), never the whole
//! archive. A store over the Drive-like `FakeBackend`, mounted, driven exactly as the
//! server drives it (`thumbs::cover_image`, `owed::archive_pages`,
//! `archive::reader_image_names`, `compiler::compile_volume`); the bytes asked of the
//! backend come from its read log.

// Fixtures are checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
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
    /// Library-relative path → provider id.
    pids: HashMap<String, String>,
    store: Arc<RemoteStore<FakeBackend>>,
    _mount: MountGuard,
}

impl T {
    fn start(files: &[(&str, &[u8])]) -> T {
        let fake = FakeBackend::drive_like();
        let mut pids = HashMap::new();
        for (rel, bytes) in files {
            let node = fake.reader_put(&format!("{FOLDER}/{rel}"), bytes);
            pids.insert(rel.to_string(), node.pid);
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
            pids,
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

/// A small volume whose pages are `{prefix}/NNN.jpg`, and its `.mokuro`.
fn small_volume(seed: u64, pages: usize) -> (Vec<u8>, Vec<u8>) {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut listed = Vec::new();
    for i in 0..pages {
        let name = format!("p/{i:03}.jpg");
        w.start_file(&name, opts).unwrap();
        w.write_all(&noise(20_000, seed + i as u64)).unwrap();
        listed.push(serde_json::json!({
            "img_path": name,
            "img_width": 800,
            "img_height": 1200,
            "blocks": [{"lines": [format!("ページ{seed}-{i}")]}],
        }));
    }
    let mokuro = serde_json::json!({
        "version": "0.2.2",
        "volume_uuid": format!("00000000-0000-4000-8000-{seed:012}"),
        "pages": listed,
    });
    (
        w.finish().unwrap().into_inner(),
        serde_json::to_vec(&mokuro).unwrap(),
    )
}

/// A cloud series compiled by another Bunko compiles again without reading its
/// volumes: each volume whose `.mokuro` the listing hashes to the `series.json`
/// entry's `mokuro_sha256` (and whose archive has its `archive_size`) is taken from
/// that entry. A volume that changed since is read and compiled.
#[test]
fn a_compiled_series_json_spares_reading_the_volumes_it_lists() {
    use bunko_library::compiler::{SeriesFolder, compile_series_volumes, entry_json};
    use bunko_library::schema::{SeriesFacts, SeriesIndexData, dump_series_file};
    use bunko_library::store::MetadataStore;

    let (a, a_mokuro) = small_volume(1, 12);
    let (b, b_mokuro) = small_volume(2, 9);
    let (_, b_edited) = small_volume(3, 9);
    let fresh = || {
        let dir = TempDir::new().unwrap();
        let db = bunko_db::Database::open(dir.path().join("bunko.db")).unwrap();
        (
            dir,
            bunko_server::library::DbMetadataStore::new(Arc::new(db)),
        )
    };
    let folder = |t: &T| SeriesFolder {
        title: "S".into(),
        path: t.root.join("S"),
    };
    let objects =
        |v: &[bunko_library::schema::VolumeEntry]| v.iter().map(entry_json).collect::<Vec<_>>();
    let volumes: [(&str, &[u8]); 4] = [
        ("S/a.cbz", &a),
        ("S/a.mokuro", &a_mokuro),
        ("S/b.cbz", &b),
        ("S/b.mokuro", &b_mokuro),
    ];

    // Another Bunko compiled the series: every volume read.
    let first = T::start(&volumes);
    let (_d, meta) = fresh();
    let (compiled, log) = first.measure(|| {
        compile_series_volumes(&folder(&first), Some(&meta as &dyn MetadataStore), true).unwrap()
    });
    assert_eq!(compiled.len(), 2);
    assert!(compiled.iter().all(|e| e.mokuro_sha256.is_some()));
    assert!(
        log.iter().any(|r| r.pid == first.pids["S/a.mokuro"]),
        "a cold compile reads the sidecars: {log:?}"
    );
    let series_json = dump_series_file(
        "S",
        &SeriesFacts::default(),
        &SeriesIndexData::default(),
        &compiled,
    )
    .unwrap();

    // A new install of the same library: no volume file is read (series.json is
    // read once, or already cached by the store's first sync).
    let mut files = volumes.to_vec();
    files.push(("S/series.json", &series_json));
    let second = T::start(&files);
    let (_d2, meta2) = fresh();
    let (seeded, log) = second.measure(|| {
        compile_series_volumes(&folder(&second), Some(&meta2 as &dyn MetadataStore), true).unwrap()
    });
    let read: Vec<&str> = log
        .iter()
        .filter_map(|r| {
            second
                .pids
                .iter()
                .find(|(_, pid)| **pid == r.pid)
                .map(|(rel, _)| rel.as_str())
        })
        .collect();
    assert!(read.iter().all(|rel| *rel == "S/series.json"), "{read:?}");
    assert_eq!(objects(&seeded), objects(&compiled));
    // The seeded entries are cached: the next compile reads nothing.
    let (_, log) = second.measure(|| {
        compile_series_volumes(&folder(&second), Some(&meta2 as &dyn MetadataStore), true).unwrap()
    });
    assert!(log.is_empty(), "{log:?}");
    // The OCR walk's page check on such an install, before any compile: the same.
    let (_d4, meta4) = fresh();
    let (missing, log) = second.measure(|| {
        bunko_library::compiler::missing_pages_now(
            &meta4 as &dyn MetadataStore,
            &second.root,
            &second.root.join("S/a.cbz"),
        )
        .unwrap()
    });
    assert_eq!(missing, 0);
    assert!(
        log.iter().all(|r| r.pid == second.pids["S/series.json"]),
        "{log:?}"
    );

    // b's sidecar changed since that compile: b is read, a is not.
    let files: [(&str, &[u8]); 5] = [
        ("S/a.cbz", &a),
        ("S/a.mokuro", &a_mokuro),
        ("S/b.cbz", &b),
        ("S/b.mokuro", &b_edited),
        ("S/series.json", &series_json),
    ];
    let third = T::start(&files);
    let (_d3, meta3) = fresh();
    let (mixed, log) = third.measure(|| {
        compile_series_volumes(&folder(&third), Some(&meta3 as &dyn MetadataStore), true).unwrap()
    });
    let read_pid = |rel: &str| log.iter().any(|r| r.pid == third.pids[rel]);
    assert!(!read_pid("S/a.mokuro") && !read_pid("S/a.cbz"), "{log:?}");
    assert!(read_pid("S/b.mokuro") && read_pid("S/b.cbz"), "{log:?}");
    assert_eq!(objects(&mixed[..1]), objects(&compiled[..1]));
    assert_ne!(mixed[1].mokuro_sha256, compiled[1].mokuro_sha256);
    assert_eq!(mixed[1].page_count, 9);
}
