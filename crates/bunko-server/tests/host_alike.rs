//! What Bunko writes, and what it reads as there, is decided from folder listings, so
//! every host answers alike (design 0.8 D5): a case-sensitive disk, Linux casefold,
//! APFS, NTFS and a remote store. Each case here once differed between a plain and a
//! casefold disk, or made a second spelling on one of them; the suite runs on both
//! (`TMPDIR` on each) and asserts one outcome.
//!
//! * a sidecar or cover name taken by a FOLDER in another spelling: nothing is written
//!   beside it;
//! * a client's upload that lands while the OCR install waits for the write lock, in
//!   the exact or another spelling: it stays, and the result is discarded;
//! * `.nocover` markers and covers spelled like the volume's name only by the wider
//!   filename fold (`STRASSE 1.nocover` beside `Straße 1.cbz`, a zero width space):
//!   no marker and no cover for that volume, on every host;
//! * the cover size the compile records: only from a listed cover of the volume;
//! * a per-user name listed in two spellings under `users/<u>/`: the canonical one
//!   Bunko has always written is read and written, the stray left alone.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::body::Body;
use bunko_core::generations::default_generation;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use bunko_server::ocr::collect::{CollectRequest, Outcome, SHADOWED, SUPERSEDED};
use bunko_server::ocr::types::{FileFacts, Job, PathLocks};
use http::Request;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const HAYAI: &str = r#"{"version":"0.2.5","title":"S","volume":"V","volume_uuid":"vu","ocr_engine":{"id":"hayai-nova","detector":"ppocr-manga","patch_budget":512},"pages":[{"img_path":"001.png","blocks":[]},{"img_path":"002.png","blocks":[]},{"img_path":"003.png","blocks":[]}]}"#;

/// A volume archive with `pages` PNG pages (a cover can be made from it).
fn cbz(pages: usize, tag: &str) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        let options = zip::write::SimpleFileOptions::default();
        zw.start_file(format!("{tag}.txt"), options).unwrap();
        zw.write_all(tag.as_bytes()).unwrap();
        for i in 0..pages {
            zw.start_file(format!("{:03}.png", i + 1), options).unwrap();
            let mut png = Vec::new();
            image::RgbImage::from_pixel(40, 56, image::Rgb([200, 100, 50]))
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            zw.write_all(&png).unwrap();
        }
        zw.finish().unwrap();
    }
    buf.into_inner()
}

struct Env {
    _dir: tempfile::TempDir,
    storage: PathBuf,
    services: Services,
    app: Router,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(storage.join("library")).unwrap();
        let mut config = Config::default();
        config.storage.base_path = storage.clone();
        config.ocr.autobench = false;
        let opts = ServeOptions::default();
        let services = Services::new(config, None, &opts).unwrap();
        for (name, role) in [("admin", Role::Admin), ("alice", Role::Uploader)] {
            services
                .db
                .create_user(name, "pass1234", role, UserStatus::Active, "")
                .unwrap();
        }
        let app = assemble(&services, &opts);
        Env {
            _dir: dir,
            storage,
            services,
            app,
        }
    }

    fn lib(&self, rel: &str) -> PathBuf {
        self.storage.join("library").join(rel)
    }

    fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) {
        let path = self.lib(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    async fn send(
        &self,
        user: &str,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> (u16, Vec<u8>) {
        use base64::Engine;
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:pass1234"))
        );
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost:8080")
            .header("authorization", auth)
            .header("content-length", body.len().to_string());
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = self
            .app
            .clone()
            .oneshot(req.body(Body::from(body.to_vec())).unwrap())
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        (status, bytes.to_vec())
    }

    /// An OCR result for the primary of `rel`, handed to the collector under `locks`.
    fn collect(&self, rel: &str, locks: PathLocks) -> Outcome {
        let dir = self.storage.join(".processing").join("s-1");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("result.mokuro");
        std::fs::write(&out, HAYAI).unwrap();
        let layout = self.services.core.layout.clone();
        let row = default_generation("g-1");
        let req = CollectRequest {
            job: Job::new(rel, &row.id),
            rows: vec![row.clone()],
            row,
            library: layout.library(),
            inbox: layout.inbox(),
            result: out,
            expected_sha256: None,
            stamp: None,
            machine: "local".into(),
            account: None,
            runner_build: None,
            pages: Some(3),
            failed_pages: Some(0),
            generator: "mokuro-bunko test".into(),
            db: Some(self.services.db.clone()),
            facts: Arc::new(FileFacts),
            locks,
            sid: "s-1".into(),
            claim: "c-1".into(),
            congestion: None,
            upgrade: None,
        };
        bunko_server::ocr::collect::run(req).outcome
    }
}

/// `dir`'s names, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    out.sort();
    out
}

/// A lock whose first `try_lock` lands a client's upload: a PUT that finished while
/// the install waited for the write lock.
struct Racing {
    upload: PathBuf,
    landed: AtomicBool,
}

impl bunko_library::service::PathWriteLocks for Racing {
    fn try_lock(&self, _path: &Path) -> Option<Box<dyn Send>> {
        if !self.landed.swap(true, Ordering::SeqCst) {
            std::fs::write(&self.upload, b"the client's upload").unwrap();
        }
        Some(Box::new(()))
    }
}

/// The install decides again under the write lock, from a fresh listing: an upload
/// that landed meanwhile, exactly or in another spelling, is served, and Bunko makes
/// no second spelling beside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upload_landing_while_the_install_waits_is_kept() {
    for upload in ["v.MOKURO", "V.mokuro"] {
        let env = Env::new();
        env.write("S/V.cbz", cbz(3, "v"));
        let locks: PathLocks = Arc::new(Racing {
            upload: env.lib("S").join(upload),
            landed: AtomicBool::new(false),
        });
        let outcome = tokio::task::block_in_place(|| env.collect("S/V.cbz", locks));
        assert_eq!(outcome, Outcome::Superseded(SUPERSEDED.into()), "{upload}");
        assert_eq!(names(&env.lib("S")), ["V.cbz", upload], "{upload}");
        let (code, body) = env
            .send(
                "admin",
                "GET",
                &format!("/mokuro-reader/S/{upload}"),
                &[],
                b"",
            )
            .await;
        assert_eq!((code, body.as_slice()), (200, &b"the client's upload"[..]));
        // The client's next PUT is an ordinary overwrite (no group was made).
        let (code, _) = env
            .send(
                "admin",
                "PUT",
                &format!("/mokuro-reader/S/{upload}"),
                &[],
                b"again",
            )
            .await;
        assert_eq!(code, 204, "{upload}");
    }
}

/// A folder spelled like a volume's sidecar or cover takes that name: the OCR result is
/// discarded and no cover is made, on every host, and the folder stays an ordinary one
/// (no case-variant group is made beside it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_spelled_like_a_sidecar_takes_its_name() {
    let env = Env::new();
    env.write("S/V.cbz", cbz(3, "v"));
    let (code, _) = env
        .send("admin", "MKCOL", "/mokuro-reader/S/v.MOKURO/", &[], b"")
        .await;
    assert_eq!(code, 201);
    let locks: PathLocks = Arc::new(bunko_library::service::NoPathLocks);
    let outcome = tokio::task::block_in_place(|| env.collect("S/V.cbz", locks));
    assert_eq!(outcome, Outcome::Superseded(SHADOWED.into()));
    assert_eq!(names(&env.lib("S")), ["V.cbz", "v.MOKURO"]);
    let (code, _) = env
        .send("admin", "DELETE", "/mokuro-reader/S/v.MOKURO/", &[], b"")
        .await;
    assert_eq!(code, 204);

    let (code, _) = env
        .send("admin", "MKCOL", "/mokuro-reader/S/v.WEBP/", &[], b"")
        .await;
    assert_eq!(code, 201);
    let library = env.lib("");
    let thumbs = bunko_server::thumbs::Thumbnails::default();
    assert_eq!(thumbs.scan_once(&library, &CancellationToken::new()), 0);
    assert_eq!(names(&env.lib("S")), ["V.cbz", "v.WEBP"]);
    let (code, _) = env
        .send("admin", "DELETE", "/mokuro-reader/S/v.WEBP/", &[], b"")
        .await;
    assert_eq!(code, 204);
}

/// `STRASSE 1.nocover` beside `Straße 1.cbz`, and `Vol1\u{200B}.nocover` beside
/// `Vol1.cbz`, are one name with the volume's marker by the filename fold but not the
/// volume's own by rule v1: no marker for it, on every host, so its cover is made (a
/// casefold disk used to open them for the built marker name and make none).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_marker_is_the_volumes_only_when_listed_as_its_own() {
    let env = Env::new();
    env.write("S/Straße 1.cbz", cbz(3, "a"));
    env.write("S/STRASSE 1.nocover", b"");
    env.write("S/Vol1.cbz", cbz(3, "b"));
    env.write("S/Vol1\u{200B}.nocover", b"");
    let library = env.lib("");
    let candidates = bunko_server::thumbs::thumbnail_candidates(&library);
    assert_eq!(
        candidates,
        [env.lib("S/Straße 1.cbz"), env.lib("S/Vol1.cbz")],
        "{candidates:?}"
    );
    let thumbs = bunko_server::thumbs::Thumbnails::default();
    assert_eq!(thumbs.scan_once(&library, &CancellationToken::new()), 2);
    assert_eq!(
        names(&env.lib("S")),
        [
            "STRASSE 1.nocover",
            "Straße 1.cbz",
            "Straße 1.webp",
            "Vol1.cbz",
            "Vol1.webp",
            "Vol1\u{200B}.nocover",
        ]
    );
}

/// The compile reads a cover's size and time only from a cover the folder lists for
/// the volume: `STRASSE 1.webp` beside `Straße 1.cbz`, `Vol2\u{FE0F}.webp` beside
/// `Vol2.cbz`, and `Vol3.webp` once `Vol3.cbz` is renamed `Vol\u{200B}3.cbz` are no
/// covers of theirs, on every host (a casefold disk opened them for the built name).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_compile_reads_only_a_listed_cover() {
    let env = Env::new();
    env.write("S/Straße 1.cbz", cbz(1, "a"));
    env.write("S/STRASSE 1.webp", b"client cover a");
    env.write("S/Vol2.cbz", cbz(1, "b"));
    env.write("S/Vol2\u{FE0F}.webp", b"client cover b");
    env.write("S/Vol3.cbz", cbz(1, "c"));
    env.write("S/Vol3.webp", b"client cover c");
    env.write("S/Vol4.cbz", cbz(1, "d"));
    env.write("S/Vol4.webp", b"its own cover");
    let (code, _) = env
        .send(
            "admin",
            "MOVE",
            "/mokuro-reader/S/Vol3.cbz",
            &[(
                "destination",
                "http://localhost:8080/mokuro-reader/S/Vol%E2%80%8B3.cbz",
            )],
            b"",
        )
        .await;
    assert!(matches!(code, 201 | 204), "{code}");
    let series = bunko_library::SeriesFolder {
        title: "S".into(),
        path: env.lib("S"),
    };
    let entries = bunko_library::compiler::compile_series_volumes(&series, None, false).unwrap();
    let covers: Vec<(String, Option<i64>)> = entries
        .iter()
        .map(|e| (e.volume_title.clone(), e.cover_size))
        .collect();
    let size = |title: &str| {
        covers
            .iter()
            .find(|(t, _)| t == title)
            .map(|(_, s)| *s)
            .unwrap_or_else(|| panic!("{title}: {covers:?}"))
    };
    assert_eq!(size("Straße 1"), None, "{covers:?}");
    assert_eq!(size("Vol2"), None, "{covers:?}");
    assert_eq!(size("Vol\u{200B}3"), None, "{covers:?}");
    assert_eq!(size("Vol4"), Some(b"its own cover".len() as i64));
}

/// Two spellings of a per-user name under `users/<u>/` (made outside Bunko; only a
/// case-sensitive disk can hold them): the canonical `goals.json` and `history/` Bunko
/// has always written are read and written, and the strays are left as they are.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stray_per_user_spelling_is_left_alone() {
    let env = Env::new();
    let (code, _) = env
        .send(
            "alice",
            "PUT",
            "/mokuro-reader/goals.json",
            &[],
            b"{\"goals\":\"bunko-written\"}",
        )
        .await;
    assert!(matches!(code, 201 | 204), "{code}");
    for folder in ["/mokuro-reader/history/", "/mokuro-reader/history/dev1/"] {
        let (code, _) = env.send("alice", "MKCOL", folder, &[], b"").await;
        assert_eq!(code, 201, "{folder}");
    }
    let (code, _) = env
        .send(
            "alice",
            "PUT",
            "/mokuro-reader/history/dev1/2026-10.events",
            &[],
            b"real-history",
        )
        .await;
    assert!(matches!(code, 201 | 204), "{code}");
    let alice = env.storage.join("users/alice");
    let stray = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(alice.join("Goals.json"))
        .map(|mut f| f.write_all(b"{\"goals\":\"stray\"}").unwrap())
        .is_ok();
    let stray_history = std::fs::create_dir(alice.join("History")).is_ok();
    if stray_history {
        std::fs::create_dir(alice.join("History/dev1")).unwrap();
        std::fs::write(alice.join("History/dev1/2026-10.events"), b"stray-history").unwrap();
    }
    let get = |path: &'static str| {
        let env = &env;
        async move { env.send("alice", "GET", path, &[], b"").await }
    };
    assert_eq!(
        get("/mokuro-reader/goals.json").await,
        (200, b"{\"goals\":\"bunko-written\"}".to_vec())
    );
    assert_eq!(
        get("/mokuro-reader/history/dev1/2026-10.events").await,
        (200, b"real-history".to_vec())
    );
    let (code, _) = env
        .send(
            "alice",
            "PUT",
            "/mokuro-reader/goals.json",
            &[],
            b"{\"goals\":\"new\"}",
        )
        .await;
    assert_eq!(code, 204);
    let (code, _) = env
        .send(
            "alice",
            "PUT",
            "/mokuro-reader/history/dev1/2026-10.events",
            &[],
            b"more-history",
        )
        .await;
    assert_eq!(code, 204);
    assert_eq!(
        std::fs::read(alice.join("goals.json")).unwrap(),
        b"{\"goals\":\"new\"}"
    );
    assert_eq!(
        std::fs::read(alice.join("history/dev1/2026-10.events")).unwrap(),
        b"more-history"
    );
    if stray {
        assert_eq!(
            std::fs::read(alice.join("Goals.json")).unwrap(),
            b"{\"goals\":\"stray\"}"
        );
    }
    if stray_history {
        assert_eq!(
            std::fs::read(alice.join("History/dev1/2026-10.events")).unwrap(),
            b"stray-history"
        );
    }
}

/// A lock that runs `on_lock` on every `try_lock` (a client's write landing just
/// before the name is locked), then grants it.
struct Landing<F: Fn(&Path) + Send + Sync>(F);

impl<F: Fn(&Path) + Send + Sync> bunko_library::service::PathWriteLocks for Landing<F> {
    fn try_lock(&self, path: &Path) -> Option<Box<dyn Send>> {
        (self.0)(path);
        Some(Box::new(()))
    }
}

/// The upgrade's swap looks the volume's primary up again under the write lock: a
/// client's `v.MOKURO` landing while it waited is the primary now, and stays; nothing
/// is written beside it (a case-sensitive disk got a second spelling `V.mokuro`).
#[test]
fn an_upload_landing_while_the_swap_waits_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let library = dir.path().join("library");
    std::fs::create_dir_all(library.join("S")).unwrap();
    std::fs::write(library.join("S/V.cbz"), cbz(3, "v")).unwrap();
    let upload = library.join("S/v.MOKURO");
    let landed = AtomicBool::new(false);
    let locks: PathLocks = Arc::new(Landing(move |_: &Path| {
        if !landed.swap(true, Ordering::SeqCst) {
            std::fs::write(&upload, b"the client's upload").unwrap();
        }
    }));
    let up = bunko_server::ocr::upgrade::Upgrade::new(
        library.clone(),
        None,
        Arc::new(FileFacts),
        locks,
        "mokuro-bunko test".into(),
    );
    let staged = dir.path().join("staged.mokuro");
    std::fs::write(&staged, HAYAI).unwrap();
    let swapped = up.swap(&library.join("S/V.cbz"), &staged, "generated");
    assert!(swapped.is_err(), "{swapped:?}");
    assert_eq!(names(&library.join("S")), ["V.cbz", "v.MOKURO"]);
    assert_eq!(
        std::fs::read(library.join("S/v.MOKURO")).unwrap(),
        b"the client's upload"
    );
}

/// A `.nocover` that appeared while the cover was made is removed after it only when,
/// under its write lock, the folder still lists it as the volume's marker: a client's
/// `v.NOCOVER` written in its place meanwhile stays (a casefold disk opened it for the
/// old name and removed it).
#[test]
fn a_marker_respelled_while_the_cover_is_made_stays() {
    let dir = tempfile::tempdir().unwrap();
    let library = dir.path().join("library");
    let s = library.join("S");
    std::fs::create_dir_all(&s).unwrap();
    std::fs::write(s.join("V.cbz"), cbz(3, "v")).unwrap();
    let marker_dir = s.clone();
    let marked = AtomicBool::new(false);
    let respelled = AtomicBool::new(false);
    let locks: PathLocks = Arc::new(Landing(move |path: &Path| {
        let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase());
        // While the cover is written, a marker lands for the volume.
        if name.as_deref() == Some("v.webp") && !marked.swap(true, Ordering::SeqCst) {
            std::fs::write(marker_dir.join("V.nocover"), b"").unwrap();
        }
        let marker = name.as_deref() == Some("v.nocover");
        if marker && !respelled.swap(true, Ordering::SeqCst) {
            std::fs::remove_file(marker_dir.join("V.nocover")).unwrap();
            std::fs::write(marker_dir.join("v.NOCOVER"), b"the client's marker").unwrap();
        }
    }));
    let thumbs = bunko_server::thumbs::Thumbnails::default();
    thumbs.set_locks(locks);
    assert_eq!(thumbs.scan_once(&library, &CancellationToken::new()), 1);
    assert_eq!(names(&s), ["V.cbz", "V.webp", "v.NOCOVER"]);
    assert_eq!(
        std::fs::read(s.join("v.NOCOVER")).unwrap(),
        b"the client's marker"
    );
}
