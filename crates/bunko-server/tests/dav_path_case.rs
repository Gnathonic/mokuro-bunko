//! Library paths behave as on NTFS through the whole server (0.5.3
//! `tests/integration/test_path_case.py`, which drives `create_app`): the path-case
//! rewrite sits in front of the auth gate, so ownership checks, uploads and the database
//! rows keyed by library path all see one spelling for one file.
//!
//! Prod 2026-10-06: an upload spelled `kingdom/` created a second folder beside `Kingdom/`
//! on the case-sensitive host, and the catalog then showed only the stray folder's one
//! volume.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::Body;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

struct Env {
    _dir: tempfile::TempDir,
    storage: PathBuf,
    services: Services,
    app: Router,
}

fn basic(user: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:pass1234"))
    )
}

fn cbz() -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        zw.start_file("000.jpg", zip::write::SimpleFileOptions::default())
            .unwrap();
        zw.write_all(b"fake image bytes").unwrap();
        zw.finish().unwrap();
    }
    buf.into_inner()
}

impl Env {
    /// The fixture of `test_path_case.py`.
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let library = storage.join("library");
        std::fs::create_dir_all(library.join("series")).unwrap();
        std::fs::write(library.join("series/vol1.cbz"), b"volume 1").unwrap();
        std::fs::write(library.join("manga1.cbz"), b"fake cbz content 1").unwrap();
        std::fs::write(library.join("manga2.cbz"), b"fake cbz content 2").unwrap();
        let mut config = Config::default();
        config.storage.base_path = storage.clone();
        let opts = ServeOptions::default();
        let services = Services::new(config, None, &opts).unwrap();
        for (name, role) in [("uploader", Role::Uploader), ("admin", Role::Admin)] {
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

    fn library_names(&self) -> Vec<String> {
        names(&self.lib(""))
    }

    async fn req(
        &self,
        user: Option<&str>,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> (StatusCode, Vec<u8>) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost:8080");
        if let Some(u) = user {
            b = b.header("authorization", basic(u));
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        if !body.is_empty() {
            b = b.header("content-length", body.len().to_string());
        }
        let resp = self
            .app
            .clone()
            .oneshot(b.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    fn owner(&self, rel: &str) -> Option<String> {
        self.services.db.get_volume_owner(rel).unwrap()
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

fn written(status: StatusCode) -> bool {
    matches!(status.as_u16(), 200 | 201 | 204)
}

const DEST: &str = "http://localhost:8080";

// --- TestRequestsReachTheExistingSpelling ------------------------------------------------

#[tokio::test]
async fn put_into_a_case_variant_folder_lands_in_the_existing_one() {
    let env = Env::new();
    let (status, body) = env
        .req(
            Some("uploader"),
            "PUT",
            "/mokuro-reader/SERIES/vol2.cbz",
            &[],
            cbz(),
        )
        .await;
    assert!(
        written(status),
        "{status} {}",
        String::from_utf8_lossy(&body)
    );
    assert!(env.lib("series/vol2.cbz").is_file());
    assert!(!env.library_names().contains(&"SERIES".into()));
    assert_eq!(env.owner("series/vol2.cbz").as_deref(), Some("uploader"));
}

/// The reader's order: make the series folder (a 405 means it exists), then upload into
/// it.
#[tokio::test]
async fn a_new_folder_keeps_the_spelling_it_was_created_with() {
    let env = Env::new();
    let up = Some("uploader");
    assert_eq!(
        env.req(up, "MKCOL", "/mokuro-reader/Kingdom", &[], vec![])
            .await
            .0,
        201
    );
    assert_eq!(
        env.req(up, "MKCOL", "/mokuro-reader/kingdom", &[], vec![])
            .await
            .0,
        405
    );
    for path in [
        "/mokuro-reader/Kingdom/v01.cbz",
        "/mokuro-reader/kingdom/v80.cbz",
    ] {
        let (status, _) = env.req(up, "PUT", path, &[], cbz()).await;
        assert!(written(status), "{path}: {status}");
    }
    assert_eq!(names(&env.lib("Kingdom")), ["v01.cbz", "v80.cbz"]);
    assert!(!env.library_names().contains(&"kingdom".into()));
    assert_eq!(env.owner("Kingdom/v80.cbz").as_deref(), Some("uploader"));
}

#[tokio::test]
async fn get_and_propfind_find_a_case_variant() {
    let env = Env::new();
    let (status, body) = env
        .req(None, "GET", "/mokuro-reader/Series/VOL1.cbz", &[], vec![])
        .await;
    assert_eq!(status, 200);
    assert_eq!(body, b"volume 1");

    let (status, body) = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader/SERIES/",
            &[("depth", "1")],
            vec![],
        )
        .await;
    assert_eq!(status, 207);
    assert!(String::from_utf8_lossy(&body).contains("vol1.cbz"));
}

#[tokio::test]
async fn mkcol_of_a_case_variant_is_an_existing_folder() {
    let env = Env::new();
    let (status, _) = env
        .req(
            Some("uploader"),
            "MKCOL",
            "/mokuro-reader/SERIES",
            &[],
            vec![],
        )
        .await;
    assert_eq!(status, 405);
    assert!(!env.library_names().contains(&"SERIES".into()));
}

/// 0.5.3 pinned PATH_INFO's latin-1 WSGI form; here a percent-encoded UTF-8 path.
#[tokio::test]
async fn a_non_ascii_case_variant_is_rewritten() {
    let env = Env::new();
    std::fs::create_dir(env.lib("\u{c9}lan")).unwrap(); // Élan
    let (status, _) = env
        .req(
            Some("uploader"),
            "PUT",
            "/mokuro-reader/%C3%A9lan/%E7%AC%AC80%E5%B7%BB.cbz",
            &[],
            cbz(),
        )
        .await;
    assert!(written(status), "{status}");
    assert!(env.lib("\u{c9}lan/第80巻.cbz").is_file());
    assert!(!env.library_names().contains(&"\u{e9}lan".into()));
}

/// An uploader may replace only their own volumes; spelling the path differently used to
/// make it a NEW file in a new folder instead.
#[tokio::test]
async fn a_case_variant_cannot_sidestep_volume_ownership() {
    let env = Env::new();
    env.services
        .db
        .create_user(
            "uploader2",
            "pass1234",
            Role::Uploader,
            UserStatus::Active,
            "",
        )
        .unwrap();
    env.req(
        Some("uploader"),
        "MKCOL",
        "/mokuro-reader/Owned",
        &[],
        vec![],
    )
    .await;
    let (first, _) = env
        .req(
            Some("uploader"),
            "PUT",
            "/mokuro-reader/Owned/v1.cbz",
            &[],
            cbz(),
        )
        .await;
    assert!(written(first), "{first}");

    let (other, _) = env
        .req(
            Some("uploader2"),
            "PUT",
            "/mokuro-reader/OWNED/V1.cbz",
            &[],
            cbz(),
        )
        .await;
    assert_eq!(other, 403);
    let top = env.library_names();
    assert_eq!(top.iter().filter(|n| *n == "Owned").count(), 1);
    assert!(!top.contains(&"OWNED".into()));

    let (own, _) = env
        .req(
            Some("uploader"),
            "PUT",
            "/mokuro-reader/OWNED/V1.cbz",
            &[],
            cbz(),
        )
        .await;
    assert!(written(own), "{own}");
    assert_eq!(names(&env.lib("Owned")), ["v1.cbz"]);
}

// --- TestCaseOnlyRenames -------------------------------------------------------------

#[tokio::test]
async fn renaming_a_folder_to_fix_its_case() {
    let env = Env::new();
    env.services
        .db
        .record_volume_upload("series/vol1.cbz", "uploader")
        .unwrap();
    std::fs::write(env.lib("series/vol1.mokuro"), "{}").unwrap();
    let dest = format!("{DEST}/mokuro-reader/Series");
    let (status, body) = env
        .req(
            Some("admin"),
            "MOVE",
            "/mokuro-reader/series",
            &[
                ("destination", &dest),
                ("overwrite", "T"),
                ("depth", "infinity"),
            ],
            vec![],
        )
        .await;
    // 0.5.2 answered 423 lock_conflict: its case-folded write locks blocked the rename.
    assert!(
        matches!(status.as_u16(), 201 | 204),
        "{status} {}",
        String::from_utf8_lossy(&body)
    );
    let top = env.library_names();
    assert!(top.contains(&"Series".into()));
    assert!(!top.contains(&"series".into()));
    assert_eq!(names(&env.lib("Series")), ["vol1.cbz", "vol1.mokuro"]);
    assert_eq!(env.owner("Series/vol1.cbz").as_deref(), Some("uploader"));
    assert_eq!(env.owner("series/vol1.cbz"), None);
}

#[tokio::test]
async fn renaming_a_file_to_fix_its_case() {
    let env = Env::new();
    let dest = format!("{DEST}/mokuro-reader/series/Vol1.cbz");
    let (status, _) = env
        .req(
            Some("admin"),
            "MOVE",
            "/mokuro-reader/series/vol1.cbz",
            &[("destination", &dest), ("overwrite", "F")],
            vec![],
        )
        .await;
    assert!(matches!(status.as_u16(), 201 | 204), "{status}");
    assert_eq!(names(&env.lib("series")), ["Vol1.cbz"]);
}

#[tokio::test]
async fn a_case_fix_through_a_differently_spelled_parent() {
    let env = Env::new();
    let dest = format!("{DEST}/mokuro-reader/Series/VOL1.cbz");
    let (status, _) = env
        .req(
            Some("admin"),
            "MOVE",
            "/mokuro-reader/SERIES/vol1.cbz",
            &[("destination", &dest), ("overwrite", "F")],
            vec![],
        )
        .await;
    assert!(matches!(status.as_u16(), 201 | 204), "{status}");
    assert_eq!(
        env.library_names()
            .iter()
            .filter(|n| *n == "series")
            .count(),
        1
    );
    assert_eq!(names(&env.lib("series")), ["VOL1.cbz"]);
}

#[tokio::test]
async fn moving_onto_a_case_variant_of_another_file_is_moving_onto_it() {
    let env = Env::new();
    let dest = format!("{DEST}/mokuro-reader/MANGA2.cbz");
    let (status, _) = env
        .req(
            Some("admin"),
            "MOVE",
            "/mokuro-reader/manga1.cbz",
            &[("destination", &dest), ("overwrite", "F")],
            vec![],
        )
        .await;
    assert_eq!(status, 412);
    assert_eq!(
        std::fs::read(env.lib("manga2.cbz")).unwrap(),
        b"fake cbz content 2"
    );
}

#[tokio::test]
async fn copying_onto_its_own_case_variant_is_refused() {
    let env = Env::new();
    let dest = format!("{DEST}/mokuro-reader/MANGA1.cbz");
    let (status, _) = env
        .req(
            Some("admin"),
            "COPY",
            "/mokuro-reader/manga1.cbz",
            &[("destination", &dest), ("overwrite", "T")],
            vec![],
        )
        .await;
    assert_eq!(status, 403);
    assert!(!env.library_names().contains(&"MANGA1.cbz".into()));
}

// --- TestCaseInsensitiveHost (the request-level half) ------------------------------------

/// A COPY writes THROUGH a symlink at the destination: where it leads must be inside the
/// library. (0.5.3 answered as if done and wrote nothing; refused with 403 here.)
#[cfg(unix)]
#[tokio::test]
async fn a_destination_symlink_leading_outside_is_refused() {
    let env = Env::new();
    let outside = env._dir.path().join("outside.cbz");
    std::fs::write(&outside, b"untouched").unwrap();
    std::os::unix::fs::symlink(&outside, env.lib("series/link.cbz")).unwrap();
    let dest = format!("{DEST}/mokuro-reader/series/link.cbz");
    env.req(
        Some("admin"),
        "COPY",
        "/mokuro-reader/manga1.cbz",
        &[("destination", &dest), ("overwrite", "T")],
        vec![],
    )
    .await;
    assert_eq!(std::fs::read(&outside).unwrap(), b"untouched");
    assert!(
        std::fs::symlink_metadata(env.lib("series/link.cbz"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

// --- the prod data after the 0.5.3 cutover -----------------------------------------------

/// `Kingdom/` holds volumes 1-80 after the merge (the stray `kingdom/` folded in, its
/// rows prefix-renamed), and a reader that cached `kingdom/` paths still asks for them.
#[tokio::test]
async fn the_kingdom_scenario() {
    let env = Env::new();
    std::fs::create_dir(env.lib("Kingdom")).unwrap();
    for v in 1..=80 {
        let rel = format!("Kingdom/第{v:02}巻.cbz");
        std::fs::write(env.lib(&rel), cbz()).unwrap();
        env.services
            .db
            .record_volume_upload(&rel, "uploader")
            .unwrap();
    }
    std::fs::write(env.lib("Kingdom/第80巻.mokuro"), "{\"title\": \"Kingdom\"}").unwrap();

    // Anonymous and signed-in reads of the old spelling.
    for user in [None, Some("admin")] {
        let (status, body) = env
            .req(
                user,
                "GET",
                "/mokuro-reader/kingdom/%E7%AC%AC80%E5%B7%BB.mokuro",
                &[],
                vec![],
            )
            .await;
        assert_eq!(status, 200, "{user:?}");
        assert_eq!(body, b"{\"title\": \"Kingdom\"}");
    }
    let (status, body) = env
        .req(
            Some("admin"),
            "PROPFIND",
            "/mokuro-reader/kingdom/",
            &[("depth", "1")],
            vec![],
        )
        .await;
    assert_eq!(status, 207);
    let text = String::from_utf8_lossy(&body);
    assert_eq!(text.matches("/mokuro-reader/Kingdom/").count(), 82);

    // The owner replacing a volume through the old spelling replaces it in place.
    let (status, _) = env
        .req(
            Some("uploader"),
            "PUT",
            "/mokuro-reader/kingdom/%E7%AC%AC80%E5%B7%BB.cbz",
            &[],
            cbz(),
        )
        .await;
    assert!(written(status), "{status}");
    assert_eq!(
        names(&env.lib(""))
            .iter()
            .filter(|n| n.to_lowercase() == "kingdom")
            .count(),
        1
    );
    assert_eq!(env.owner("Kingdom/第80巻.cbz").as_deref(), Some("uploader"));
    assert_eq!(env.owner("kingdom/第80巻.cbz"), None);
}
