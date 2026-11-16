//! The library scan stays small on a large library (docs/design/analytics.md §2, §7):
//! 12 000 volumes in 1 000 series (36 000 files). A counting allocator records the
//! peak heap the scan adds over what the server held before it, for the whole-server
//! file pass and for the walk with every archive queued for page reading (a first scan
//! of a library with page formats on).
//!
//! Alone in its binary: the allocator counts the whole process.

// Fixtures are set up with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Instant;

use bunko_core::Config;
use bunko_server::analytics::library::Outcome;
use bunko_server::analytics::library::walk::Walk;
use bunko_server::app::{ServeOptions, Services};

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

fn grew(by: isize) {
    let now = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

// SAFETY: every call is forwarded to `System` unchanged; only counters are added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            grew(l.size() as isize);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            grew(l.size() as isize);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size() as isize, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            grew(new as isize - l.size() as isize);
        }
        q
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const SERIES: usize = 1000;
const VOLUMES: usize = 12;
const MIB: f64 = 1024.0 * 1024.0;

fn tiny_cbz() -> Vec<u8> {
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let o =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for i in 0..3 {
        z.start_file(format!("{i:03}.jpg"), o).unwrap();
        z.write_all(&[0xFF, 0xD8, 0xFF, 0xE0]).unwrap();
    }
    z.finish().unwrap().into_inner()
}

fn build(library: &Path) {
    let cbz = tiny_cbz();
    for s in 0..SERIES {
        let dir = library.join(format!("A Rather Long Series Title Number {s:04}"));
        std::fs::create_dir_all(&dir).unwrap();
        for v in 0..VOLUMES {
            let stem = dir.join(format!("Series {s:04} Volume {v:02}"));
            std::fs::write(stem.with_extension("cbz"), &cbz).unwrap();
            std::fs::write(stem.with_extension("mokuro"), b"{}").unwrap();
            std::fs::write(stem.with_extension("webp"), b"RIFF").unwrap();
        }
    }
}

/// Peak heap above `LIVE` at the start of `f`, in MiB.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    let peak = PEAK.load(Ordering::Relaxed) - base;
    (out, peak as f64 / MIB)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ten_thousand_volume_library_scans_in_a_few_mib() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    let library: PathBuf = storage.join("library");
    build(&library);

    // The whole-server file pass (page formats off: a page pass reads 20 archives a
    // second, which this test does not wait for).
    let mut config = Config::default();
    config.storage.base_path = storage.clone();
    config.analytics.library_scan.page_formats = false;
    let services = Services::new(config, None, &ServeOptions::default()).unwrap();
    let scanner = services.analytics.library_scanner();
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let t = Instant::now();
    let (outcome, stats) = scanner.scan_now(&services.analytics).await.unwrap();
    let pass_ms = t.elapsed().as_millis();
    let pass_peak = (PEAK.load(Ordering::Relaxed) - base) as f64 / MIB;
    assert_eq!(outcome, Outcome::Done);
    assert_eq!(
        (stats.series, stats.volumes, stats.files),
        (
            SERIES as u64,
            (SERIES * VOLUMES) as u64,
            (SERIES * VOLUMES * 3) as u64
        )
    );

    // The walk with page formats on: every archive queued (a first scan).
    let db = bunko_db::analytics::AnalyticsDb::open(dir.path().join("walk.sqlite")).unwrap();
    let t = Instant::now();
    let (w, walk_peak) = peak_of(|| {
        let mut w = Walk::new(true);
        let mut stack = vec![(library.clone(), String::new())];
        while let Some((d, rel)) = stack.pop() {
            let subs = w.folder(&d, &rel, Some(&db));
            stack.extend(subs.into_iter().rev());
        }
        w
    });
    let walk_ms = t.elapsed().as_millis();
    assert_eq!(w.queue.len(), SERIES * VOLUMES);
    eprintln!(
        "library scan of {} volumes / {} files: whole-server file pass {pass_ms} ms, peak +{pass_peak:.2} MiB; \
         walk with all {} archives queued {walk_ms} ms, peak +{walk_peak:.2} MiB",
        stats.volumes,
        stats.files,
        w.queue.len()
    );
    assert!(pass_peak < 16.0, "file pass peak {pass_peak:.2} MiB");
    assert!(walk_peak < 16.0, "walk peak {walk_peak:.2} MiB");
}
