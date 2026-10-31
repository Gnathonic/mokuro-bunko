//! Series folder owners (0.8), through the whole server: the account whose MKCOL
//! creates a series folder owns it, and the record follows the folder.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

struct Env {
    _dir: tempfile::TempDir,
    storage: PathBuf,
    services: Services,
    app: Router,
}

struct Resp {
    status: u16,
    body: Vec<u8>,
}

impl Resp {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.text()))
    }
}

fn basic(user: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:pass1234"))
    )
}

/// A zip with `pages` stored pages: a `.cbz` PUT is checked whole.
fn cbz(pages: usize) -> Vec<u8> {
    use std::io::Write;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for i in 0..pages {
            zw.start_file(format!("{i:03}.jpg"), opts).unwrap();
            zw.write_all(format!("page {i}").as_bytes()).unwrap();
        }
        zw.finish().unwrap();
    }
    buf.into_inner()
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(storage.join("library")).unwrap();
        let mut config = Config::default();
        config.storage.base_path = storage.clone();
        config.ocr.autobench = false;
        config.ocr.local_processing = false;
        let opts = ServeOptions::default();
        let services = Services::new(config, None, &opts).unwrap();
        for (name, role) in [
            ("alice", Role::Uploader),
            ("bob", Role::Uploader),
            ("editor", Role::Editor),
            ("reader", Role::Registered),
        ] {
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

    fn owner(&self, folder: &str) -> Option<String> {
        self.services.db.series_folder_owner(folder).unwrap()
    }

    async fn send(
        &self,
        user: &str,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Resp {
        let uri = path.replace(' ', "%20");
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "localhost:8080")
            .header("authorization", basic(user))
            .header("content-length", body.len().to_string());
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let resp = self
            .app
            .clone()
            .oneshot(b.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let body = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        Resp { status, body }
    }

    async fn mkcol(&self, user: &str, path: &str) -> Resp {
        self.send(user, "MKCOL", path, &[], Vec::new()).await
    }

    async fn put(&self, user: &str, path: &str, body: Vec<u8>) -> Resp {
        self.send(user, "PUT", path, &[], body).await
    }

    async fn delete(&self, user: &str, path: &str) -> Resp {
        self.send(user, "DELETE", path, &[], Vec::new()).await
    }

    async fn mv(&self, user: &str, from: &str, to: &str) -> Resp {
        let dest = format!("http://localhost:8080{}", to.replace(' ', "%20"));
        self.send(
            user,
            "MOVE",
            from,
            &[("destination", dest.as_str())],
            Vec::new(),
        )
        .await
    }
}

#[tokio::test]
async fn the_creator_of_a_series_folder_owns_it_and_the_record_follows_it() {
    let env = Env::new();
    let r = env.mkcol("alice", "/mokuro-reader/Alice Series").await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(env.owner("alice series").as_deref(), Some("alice"));
    let r = env
        .put("alice", "/mokuro-reader/Alice Series/Vol 1.cbz", cbz(2))
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    // An editor's folder is the editor's; a subfolder is no series folder.
    let r = env.mkcol("editor", "/mokuro-reader/Edited").await;
    assert_eq!(r.status, 201);
    assert_eq!(env.owner("Edited").as_deref(), Some("editor"));
    let r = env
        .mkcol("alice", "/mokuro-reader/Alice Series/Extras")
        .await;
    assert_eq!(r.status, 201);
    assert_eq!(env.owner("Extras"), None);
    // A folder that was already there (made outside the server) is nobody's.
    std::fs::create_dir_all(env.lib("Outside")).unwrap();
    let r = env.mkcol("bob", "/mokuro-reader/Outside").await;
    assert_eq!(r.status, 405);
    assert_eq!(env.owner("Outside"), None);

    // An editor's rename carries the record; a case-only one keeps it, respelled.
    let r = env
        .mv(
            "editor",
            "/mokuro-reader/Alice Series",
            "/mokuro-reader/Renamed",
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(env.owner("Alice Series"), None);
    assert_eq!(env.owner("Renamed").as_deref(), Some("alice"));
    let r = env
        .mv("editor", "/mokuro-reader/Renamed", "/mokuro-reader/RENAMED")
        .await;
    assert!(matches!(r.status, 201 | 204), "{}: {}", r.status, r.text());
    let rows = env.services.db.list_series_folder_owners().unwrap();
    let renamed = rows.iter().find(|r| r.series_key == "renamed").unwrap();
    assert_eq!(
        (renamed.folder_name.as_str(), renamed.owner.as_str()),
        ("RENAMED", "alice")
    );
    // Deleted: forgotten, so the next creator owns it.
    let r = env.delete("editor", "/mokuro-reader/RENAMED").await;
    assert_eq!(r.status, 204);
    assert_eq!(env.owner("Renamed"), None);
    let r = env.mkcol("bob", "/mokuro-reader/Renamed").await;
    assert_eq!(r.status, 201);
    assert_eq!(env.owner("renamed").as_deref(), Some("bob"));
}

#[tokio::test]
async fn nobody_else_adds_to_an_owned_series() {
    let env = Env::new();
    assert_eq!(env.mkcol("alice", "/mokuro-reader/Alice").await.status, 201);
    let r = env
        .put("alice", "/mokuro-reader/Alice/Vol 1.cbz", cbz(2))
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = env
        .put("bob", "/mokuro-reader/Alice/Vol 9.cbz", cbz(1))
        .await;
    assert_eq!(r.status, 403, "{}", r.text());
    assert_eq!(
        r.json(),
        serde_json::json!({
            "ok": false,
            "reason": "forbidden",
            "detail": "This series belongs to another account.",
            "retry": false
        })
    );
    assert!(!env.lib("Alice/Vol 9.cbz").exists());
    // By any spelling of the path or the series.
    for path in [
        "/mokuro-reader//Alice/Vol 9.cbz",
        "/mokuro-reader/./Alice/Vol 9.cbz",
        "/mokuro-reader/Alice/Vol 1.webp",
    ] {
        let r = env.put("bob", path, b"x".to_vec()).await;
        assert_eq!(r.status, 403, "{path}: {}", r.text());
    }
    for path in ["/mokuro-reader/Alice/Extras", "/mokuro-reader/ALICE"] {
        let r = env.mkcol("bob", path).await;
        assert_eq!(r.status, 403, "{path}: {}", r.text());
        assert!(
            r.text().contains("belongs to another account"),
            "{}",
            r.text()
        );
    }
    // Bob's own correction of her volume is his.
    let r = env
        .put(
            "bob",
            "/mokuro-reader/Alice/Vol 1.edit.mokuro",
            br#"{"pages":[]}"#.to_vec(),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert!(
        env.storage
            .join("users/bob/edits/Alice/Vol 1.edit.mokuro")
            .exists()
    );
    // An editor adds there; the owner deletes what the editor added, then the folder.
    let r = env
        .put("editor", "/mokuro-reader/Alice/Vol 2.cbz", cbz(1))
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = env.delete("bob", "/mokuro-reader/Alice/Vol 2.cbz").await;
    assert_eq!(r.status, 403);
    let r = env.delete("alice", "/mokuro-reader/Alice/Vol 2.cbz").await;
    assert_eq!(r.status, 204, "{}", r.text());
    let r = env.delete("bob", "/mokuro-reader/Alice").await;
    assert_eq!(r.status, 403);
    let r = env.delete("alice", "/mokuro-reader/Alice").await;
    assert_eq!(r.status, 204, "{}", r.text());
    assert!(!env.lib("Alice").exists());
    assert_eq!(env.owner("Alice"), None);
}

/// The finding (authz-dotted-primary), in a shared folder and both upload orders: a
/// nested archive is the outer volume's, whichever came first.
#[tokio::test]
async fn a_nested_archive_is_the_outer_volumes_in_either_order() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("S")).unwrap();
    let r = env.put("alice", "/mokuro-reader/S/V.cbz", cbz(2)).await;
    assert_eq!(r.status, 201, "{}", r.text());
    for name in [
        "V.curated.cbz",
        "V.EDIT.cbz",
        "V.Curated.cbz",
        "V.pre-upgrade.cbz",
    ] {
        let r = env
            .put("bob", &format!("/mokuro-reader/S/{name}"), cbz(1))
            .await;
        assert_eq!(r.status, 403, "{name}: {}", r.text());
        assert_eq!(
            r.json()["detail"],
            "This file belongs to a volume this account did not upload."
        );
    }
    // A folder named like one is refused as well (and would claim nothing).
    let r = env.mkcol("bob", "/mokuro-reader/S/V.edit.cbz").await;
    assert_eq!(r.status, 403, "{}", r.text());

    // The other order: bob's nested archive was there first.
    std::fs::create_dir_all(env.lib("T")).unwrap();
    let r = env
        .put("bob", "/mokuro-reader/T/V.curated.cbz", cbz(1))
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = env
        .put(
            "bob",
            "/mokuro-reader/T/V.curated.mokuro",
            br#"{"pages":[]}"#.to_vec(),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = env.put("alice", "/mokuro-reader/T/V.cbz", cbz(2)).await;
    assert_eq!(r.status, 201, "{}", r.text());
    for path in [
        "/mokuro-reader/T/V.curated.cbz",
        "/mokuro-reader/T/V.curated.mokuro",
    ] {
        let r = env.put("bob", path, b"{}".to_vec()).await;
        assert_eq!(r.status, 403, "{path}: {}", r.text());
        let r = env.delete("bob", path).await;
        assert_eq!(r.status, 403, "{path}: {}", r.text());
    }
    // V's uploader may remove the squatter, and V's curated layer stays (C11).
    let r = env.delete("alice", "/mokuro-reader/T/V.curated.cbz").await;
    assert_eq!(r.status, 204, "{}", r.text());
    assert!(!env.lib("T/V.curated.cbz").exists());
    assert!(env.lib("T/V.curated.mokuro").exists());
}

/// An archive that lands while another account's nested archive is still streaming
/// governs it by the time it would be published: the upload is refused and nothing of
/// it stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_archive_that_lands_mid_upload_refuses_it() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("S")).unwrap();
    let body = cbz(2);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(4);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    let req = Request::builder()
        .method("PUT")
        .uri("/mokuro-reader/S/V.curated.cbz")
        .header("host", "localhost:8080")
        .header("authorization", basic("bob"))
        .header("content-length", body.len().to_string())
        .body(Body::from_stream(stream))
        .unwrap();
    let app = env.app.clone();
    let slow = tokio::spawn(async move {
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, body)
    });
    let (head, tail) = body.split_at(body.len() / 2);
    tx.send(Ok(axum::body::Bytes::copy_from_slice(head)))
        .await
        .unwrap();
    // Admitted and staging: the folder lists its staging file.
    let staging = || {
        std::fs::read_dir(env.lib("S"))
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".upload-"))
    };
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !staging() {
        assert!(
            std::time::Instant::now() < until,
            "the upload never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let r = env.put("alice", "/mokuro-reader/S/V.cbz", cbz(2)).await;
    assert_eq!(r.status, 201, "{}", r.text());
    tx.send(Ok(axum::body::Bytes::copy_from_slice(tail)))
        .await
        .unwrap();
    drop(tx);
    let (status, verdict) = slow.await.unwrap();
    assert_eq!(status, 403, "{}", String::from_utf8_lossy(&verdict));
    let names: Vec<String> = std::fs::read_dir(env.lib("S"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["V.cbz"]);
    assert_eq!(
        env.services.db.get_volume_owner("S/V.curated.cbz").unwrap(),
        None
    );
}

/// series.json update requests: the series' recorded owner, not its sole uploader.
#[tokio::test]
async fn the_series_owner_requests_metadata_updates() {
    let env = Env::new();
    assert_eq!(env.mkcol("alice", "/mokuro-reader/Owned").await.status, 201);
    std::fs::create_dir_all(env.lib("Sole")).unwrap();
    for series in ["Owned", "Sole"] {
        let r = env
            .put(
                "alice",
                &format!("/mokuro-reader/{series}/Vol 1.cbz"),
                cbz(1),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.text());
    }
    let update = br#"{"version":2,"series_title":"x","updated_at":"2026-10-31T00:00:00.000Z","external_ids":{},"titles":{},"synonyms":[]}"#;
    let r = env
        .put("alice", "/mokuro-reader/Sole/series.json", update.to_vec())
        .await;
    assert_eq!(r.status, 403, "{}", r.text());
    let r = env
        .put("bob", "/mokuro-reader/Owned/series.json", update.to_vec())
        .await;
    assert_eq!(r.status, 403, "{}", r.text());
    let r = env
        .put("alice", "/mokuro-reader/Owned/series.json", update.to_vec())
        .await;
    assert_ne!(r.status, 403, "{}", r.text());
}

/// Until the existing folders have their owners, an uploader's additions wait (C5): a
/// folder about to be someone's never becomes mixed meanwhile.
#[tokio::test]
async fn additions_wait_until_the_existing_series_have_owners() {
    let env = Env::new();
    // A library from before 0.8: alice uploaded Solo's only volume.
    std::fs::create_dir_all(env.lib("Solo")).unwrap();
    std::fs::write(env.lib("Solo/Vol 1.cbz"), cbz(1)).unwrap();
    let db = &env.services.db;
    db.record_volume_upload("Solo/Vol 1.cbz", "alice").unwrap();
    db.with_writer_connection(|c| c.execute("DELETE FROM series_folder_adoption", []))
        .unwrap();
    let r = env
        .put("bob", "/mokuro-reader/Solo/Vol 2.cbz", cbz(1))
        .await;
    assert_eq!(r.status, 503, "{}", r.text());
    let r = env.mkcol("bob", "/mokuro-reader/Other").await;
    assert_eq!(r.status, 503, "{}", r.text());
    assert!(!env.lib("Solo/Vol 2.cbz").exists());
    // Its own volume is still the uploader's to replace.
    let r = env
        .put("alice", "/mokuro-reader/Solo/Vol 1.cbz", cbz(2))
        .await;
    assert_eq!(r.status, 204, "{}", r.text());
    bunko_server::library::adopt_series_folder_owners(db, &env.lib("")).unwrap();
    assert_eq!(env.owner("Solo").as_deref(), Some("alice"));
    let r = env
        .put("bob", "/mokuro-reader/Solo/Vol 2.cbz", cbz(1))
        .await;
    assert_eq!(r.status, 403, "{}", r.text());
}
