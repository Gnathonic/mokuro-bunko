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

/// A request path as a client sends it: spaces and every non-ASCII byte
/// percent-encoded (a `%` already there is kept).
fn enc(path: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS.add(b' ');
    percent_encoding::utf8_percent_encode(path, SET).to_string()
}

/// The names `dir` lists.
fn names(dir: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    out.sort();
    out
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
        let uri = enc(path);
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
        let dest = format!("http://localhost:8080{}", enc(to));
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

/// `/login/api/me` lists the volumes an uploader may publish the curated layer of in a
/// shared series (D1), and auth agrees: its publish lands on each one listed, and is
/// refused on every other volume there.
#[tokio::test]
async fn me_lists_the_shared_volumes_whose_curated_layer_it_may_publish() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("Shared")).unwrap();
    for (user, name) in [
        ("alice", "Vol 1.cbz"),
        ("bob", "Vol 2.cbz"),
        ("alice", "Vol 2.5.cbz"),
    ] {
        let r = env
            .put(user, &format!("/mokuro-reader/Shared/{name}"), cbz(1))
            .await;
        assert_eq!(r.status, 201, "{name}: {}", r.text());
    }
    // Her own series is `ownedSeries`, not a volume list.
    assert_eq!(env.mkcol("alice", "/mokuro-reader/Alice").await.status, 201);
    let r = env
        .put("alice", "/mokuro-reader/Alice/Vol 1.cbz", cbz(1))
        .await;
    assert_eq!(r.status, 201, "{}", r.text());

    let r = env
        .send("alice", "GET", "/login/api/me", &[], Vec::new())
        .await;
    let p = &r.json()["permissions"];
    assert_eq!(
        p["curated"],
        serde_json::json!({"scope": "owned", "volumes": [
            {"series": "Shared", "volume": "Vol 1"},
            {"series": "Shared", "volume": "Vol 2.5"},
        ]})
    );
    assert_eq!(
        p["metadata"],
        serde_json::json!({"scope": "owned", "ownedSeries": ["Alice"]})
    );
    for (volume, status) in [("Vol 1", 201), ("Vol 2.5", 201), ("Vol 2", 403)] {
        let r = env
            .put(
                "alice",
                &format!("/mokuro-reader/Shared/{volume}.curated.mokuro"),
                br#"{"pages":[]}"#.to_vec(),
            )
            .await;
        assert_eq!(r.status, status, "{volume}: {}", r.text());
    }
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
        "V.curated-2.cbz",
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

/// Owner decision, 2026-11-01: neither id is reserved, so a client's
/// `<Vol>.pre-upgrade.mokuro` or `<Vol>.previous-ocr.mokuro` is a hand-made layer, added
/// by whoever may add files to that volume and refused only for the usual ownership and
/// permission reasons.
#[tokio::test]
async fn pre_upgrade_and_previous_ocr_layers_are_hand_made() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("S")).unwrap();
    let r = env.put("alice", "/mokuro-reader/S/V.cbz", cbz(2)).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let body = || br#"{"version":"0.2.1","pages":[{"blocks":[]},{"blocks":[]}]}"#.to_vec();
    // What any other hand-made layer of V gets, account by account.
    let mut hand_made = std::collections::HashMap::new();
    for user in ["bob", "reader"] {
        let r = env
            .put(user, "/mokuro-reader/S/V.mine.mokuro", body())
            .await;
        assert_eq!(r.status, 403, "{user}: {}", r.text());
        hand_made.insert(user, r.json()["detail"].clone());
    }
    for (user, name, status) in [
        ("alice", "V.pre-upgrade.mokuro", 201),
        ("alice", "V.previous-ocr.mokuro", 201),
        ("alice", "V.pre-upgrade.mokuro", 204),
        ("editor", "V.previous-ocr-2.mokuro", 201),
        ("bob", "V.pre-upgrade-2.mokuro", 403),
        ("bob", "V.previous-ocr-3.mokuro", 403),
        ("reader", "V.pre-upgrade-3.mokuro", 403),
    ] {
        let r = env
            .put(user, &format!("/mokuro-reader/S/{name}"), body())
            .await;
        assert_eq!(r.status, status, "{user} {name}: {}", r.text());
        if status == 403 {
            assert_eq!(r.json()["detail"], hand_made[user], "{user} {name}");
        }
    }
    // Overwriting one is refused like overwriting any other hand-made layer of V.
    let r = env
        .put("alice", "/mokuro-reader/S/V.other.mokuro", body())
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let other = env
        .put("bob", "/mokuro-reader/S/V.other.mokuro", body())
        .await;
    let r = env
        .put("bob", "/mokuro-reader/S/V.previous-ocr.mokuro", body())
        .await;
    assert_eq!((r.status, r.json()), (other.status, other.json()));
    assert_eq!(r.status, 403, "{}", r.text());
    assert!(env.lib("S/V.pre-upgrade.mokuro").exists());
    assert!(env.lib("S/V.previous-ocr-2.mokuro").exists());
    assert!(!env.lib("S/V.pre-upgrade-2.mokuro").exists());
    assert!(!env.lib("S/V.pre-upgrade-3.mokuro").exists());
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

/// What review found past the finding: a later, shorter archive takes nobody's volume
/// (OWN-1); not even a volume's own uploader makes an archive that hides one of its
/// reserved layers (OWN-2); a nested id folds like a stem, NFC first (F1).
#[tokio::test]
async fn no_archive_name_takes_another_volume_or_hides_a_layer() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("S")).unwrap();
    let r = env
        .put("alice", "/mokuro-reader/S/Berserk.v01.cbz", cbz(2))
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = env.put("bob", "/mokuro-reader/S/Berserk.cbz", cbz(1)).await;
    assert_eq!(r.status, 201, "a new volume: {}", r.text());
    let r = env.delete("bob", "/mokuro-reader/S/Berserk.v01.cbz").await;
    assert_eq!(r.status, 403, "{}", r.text());
    assert!(env.lib("S/Berserk.v01.cbz").exists());
    for path in [
        "/mokuro-reader/S/Berserk.v01.mokuro",
        "/mokuro-reader/S/Berserk.v01.curated.mokuro",
    ] {
        let r = env.put("bob", path, br#"{"pages":[]}"#.to_vec()).await;
        assert_eq!(r.status, 403, "{path}: {}", r.text());
    }

    // A Kelvin sign is a `k`: `V.<KELVIN>anji.cbz` is V's, not a volume of its own.
    let r = env.put("alice", "/mokuro-reader/S/V.cbz", cbz(2)).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = env
        .put("bob", "/mokuro-reader/S/V.%E2%84%AAanji.cbz", cbz(1))
        .await;
    assert_eq!(r.status, 403, "{}", r.text());

    // Alice may not hide a correction of her own volume, and bob's stays his.
    let r = env
        .put("alice", "/mokuro-reader/S/V.edit.cbz", cbz(1))
        .await;
    assert_eq!(r.status, 403, "{}", r.text());
    assert_eq!(
        r.json()["detail"],
        "This name is kept for a layer of the volume beside it."
    );
    assert!(!env.lib("S/V.edit.cbz").exists());
    let r = env
        .put(
            "bob",
            "/mokuro-reader/S/V.edit.mokuro",
            br#"{"pages":[]}"#.to_vec(),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert!(env.storage.join("users/bob/edits/S/V.edit.mokuro").exists());
}

/// S1's merge rule for an editor's MOVE onto another spelling of a series (OWN-5): it
/// keeps its owner when what joined it was the owner's (or nobody's volumes), and
/// loses it when another account's volume joined. (A case-only spelling is the same
/// folder to WebDAV, an overwrite; whitespace is not.)
#[tokio::test]
async fn a_folder_joining_a_series_keeps_its_owner_only_when_it_agrees() {
    let env = Env::new();
    assert_eq!(
        env.mkcol("alice", "/mokuro-reader/Two Words").await.status,
        201
    );
    std::fs::create_dir_all(env.lib("Empty")).unwrap();
    let r = env
        .mv(
            "editor",
            "/mokuro-reader/Empty",
            "/mokuro-reader/Two  Words",
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert!(env.lib("Two Words").is_dir() && env.lib("Two  Words").is_dir());
    assert_eq!(env.owner("Two Words").as_deref(), Some("alice"));

    std::fs::create_dir_all(env.lib("Bobs")).unwrap();
    let r = env
        .put("bob", "/mokuro-reader/Bobs/Vol 1.cbz", cbz(1))
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = env
        .mv(
            "editor",
            "/mokuro-reader/Bobs",
            "/mokuro-reader/Two   Words",
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(env.owner("Two Words"), None);
}

/// Every host makes `STRASSE.mokuro` one name with `Straße.mokuro`, `Vol\u{200B}1.webp`
/// one with `Vol1.webp` (Linux casefold drops the zero width space), `οδοσ` one with
/// `ΟΔΟΣ`, `ı` one with `i`: names a listed volume's files would have. Rule v1, Mokuro
/// Reader's, does not give them to the volume. Such a name is governed by the volume
/// for authorization (an account that does not control it gets 403), and written for
/// nobody (an account that does gets 409 `ambiguous-name`), so no file ever takes a
/// volume's name without being its file. The same on every host and for every role.
#[tokio::test]
async fn names_the_wider_fold_joins_with_a_volume_are_governed_and_never_written() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("Shared")).unwrap();
    for name in [
        "Stra\u{df}e.cbz",
        "Vol1.cbz",
        "\u{39f}\u{394}\u{39f}\u{3a3}.cbz",
        "i.cbz",
        "V.cbz",
    ] {
        let r = env
            .put("alice", &format!("/mokuro-reader/Shared/{name}"), cbz(1))
            .await;
        assert_eq!(r.status, 201, "{name}: {}", r.text());
    }
    let listed = names(&env.lib("Shared"));
    let disputed = [
        "STRASSE.mokuro",
        "STRASSE.webp",
        "strasse.nocover",
        "STRASSE.hayai.mokuro",
        "Stra\u{17F}\u{17F}e.mokuro.gz",
        "Vol\u{200B}1.mokuro",
        "Vol1\u{FE0F}.webp",
        "\u{3bf}\u{3b4}\u{3bf}\u{3c3}.mokuro",
        "\u{131}.webp",
        "V.EDIT.mokuro",
        "V.edit\u{AD}.mokuro",
    ];
    let body = br#"{"pages":[]}"#.to_vec();
    for name in disputed {
        let path = format!("/mokuro-reader/Shared/{name}");
        // bob controls none of these volumes.
        let r = env.put("bob", &path, body.clone()).await;
        assert_eq!(r.status, 403, "bob {name:?}: {}", r.text());
        assert_eq!(
            r.json()["detail"],
            "This file belongs to a volume this account did not upload.",
            "bob {name:?}"
        );
        // alice controls them, and the editor everything: written for nobody.
        for user in ["alice", "editor"] {
            let r = env.put(user, &path, body.clone()).await;
            assert_eq!(r.status, 409, "{user} {name:?}: {}", r.text());
            assert_eq!(r.json()["reason"], "ambiguous-name", "{user} {name:?}");
        }
        let r = env.mkcol("editor", &path).await;
        assert_eq!(r.status, 409, "MKCOL {name:?}: {}", r.text());
        let to = format!("http://localhost:8080{}", enc(&path));
        let r = env
            .send(
                "editor",
                "COPY",
                "/mokuro-reader/Shared/V.cbz",
                &[("destination", to.as_str())],
                Vec::new(),
            )
            .await;
        assert_eq!(r.status, 409, "COPY onto {name:?}: {}", r.text());
    }
    // A name only the fold gives a volume file suffix (a Kelvin sign, a zero width
    // space).
    for name in ["W.mo\u{212A}uro", "W.cb\u{200B}z"] {
        let path = format!("/mokuro-reader/Shared/{name}");
        let r = env.put("editor", &path, cbz(1)).await;
        assert_eq!(r.status, 409, "{name:?}: {}", r.text());
        assert_eq!(r.json()["reason"], "ambiguous-name", "{name:?}");
    }
    assert_eq!(names(&env.lib("Shared")), listed, "nothing was written");
    // The spellings rule v1 gives the volumes are its files, as ever.
    let r = env
        .put(
            "alice",
            "/mokuro-reader/Shared/stra\u{df}e.mokuro",
            body.clone(),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = env
        .put(
            "bob",
            "/mokuro-reader/Shared/STRA\u{df}E.webp",
            b"x".to_vec(),
        )
        .await;
    assert_eq!(r.status, 403, "{}", r.text());
    // The file now listed is reached in any spelling, by its controller.
    let r = env
        .put(
            "alice",
            "/mokuro-reader/Shared/STRASSE.MOKURO",
            body.clone(),
        )
        .await;
    assert!(matches!(r.status, 201 | 204), "{}: {}", r.status, r.text());
    let r = env
        .put("bob", "/mokuro-reader/Shared/STRASSE.MOKURO", body.clone())
        .await;
    assert_eq!(r.status, 403, "{}", r.text());
    assert!(names(&env.lib("Shared")).contains(&"stra\u{df}e.mokuro".to_string()));
}

/// A name the filename fold ties to a volume's files without rule v1 doing so is
/// refused only on its own side: an archive is never refused for one already listed
/// beside it. Whoever makes `V.EDIT.mokuro` or `STRASSE.mokuro` in a shared series
/// before the volume exists (nothing governs it then: 201) does not keep the volume's
/// uploader, an editor or an admin from adding `V.cbz` or `Straße.cbz`, or from
/// replacing it; the stray then counts as that volume's for authorization (its
/// uploader may delete it, its maker no longer), and Bunko writes nothing for the
/// volume in its place. The same for a hand-made stray (`X.Edit.mokuro`, from 0.7).
#[tokio::test]
async fn a_stray_made_first_never_keeps_a_volume_out() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("Shared")).unwrap();
    std::fs::create_dir_all(env.lib("Other")).unwrap();
    let body = br#"{"pages":[]}"#.to_vec();
    let strays = [
        ("V.EDIT.mokuro", "V.cbz"),
        ("W.Fast.mokuro", "W.cbz"),
        ("Y.HAYAI.mokuro", "Y.cbz"),
        ("STRASSE.mokuro", "Stra\u{df}e.cbz"),
        ("Vol\u{200B}1.webp", "Vol1.cbz"),
        ("\u{131}.nocover", "i.cbz"),
    ];
    for (stray, _) in strays {
        let path = format!("/mokuro-reader/Shared/{stray}");
        // A name the fold joins with an account's correction is made by nobody, its
        // volume listed or not: this one is made by hand (or by 0.7).
        if stray == "V.EDIT.mokuro" {
            let r = env.put("bob", &path, body.clone()).await;
            assert_eq!(r.status, 409, "bob {stray:?}: {}", r.text());
            std::fs::write(env.lib(&format!("Shared/{stray}")), &body).unwrap();
            continue;
        }
        let r = env.put("bob", &path, body.clone()).await;
        assert_eq!(r.status, 201, "bob {stray:?}: {}", r.text());
    }
    for (stray, archive) in strays {
        let path = format!("/mokuro-reader/Shared/{archive}");
        let r = env.put("alice", &path, cbz(1)).await;
        assert_eq!(
            r.status,
            201,
            "alice {archive:?} beside {stray:?}: {}",
            r.text()
        );
        // Replaced by its uploader and by an editor, the stray still listed.
        let r = env.put("alice", &path, cbz(2)).await;
        assert_eq!(r.status, 204, "alice again {archive:?}: {}", r.text());
        let r = env.put("editor", &path, cbz(1)).await;
        assert_eq!(r.status, 204, "editor {archive:?}: {}", r.text());
    }
    // An editor's MOVE into the folder, beside a stray made first (by hand: nobody
    // makes a name the fold joins with the curated layer).
    let r = env
        .put(
            "bob",
            "/mokuro-reader/Shared/Z.Curated.mokuro",
            body.clone(),
        )
        .await;
    assert_eq!(r.status, 409, "{}", r.text());
    std::fs::write(env.lib("Shared/Z.Curated.mokuro"), &body).unwrap();
    let r = env
        .put("editor", "/mokuro-reader/Other/Z.cbz", cbz(1))
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = env
        .mv(
            "editor",
            "/mokuro-reader/Other/Z.cbz",
            "/mokuro-reader/Shared/Z.cbz",
        )
        .await;
    assert!(matches!(r.status, 201 | 204), "{}: {}", r.status, r.text());
    assert!(env.lib("Shared/Z.cbz").is_file());
    // A hand-made stray beside a listed volume: re-uploaded as ever.
    std::fs::write(env.lib("Shared/X.cbz"), cbz(1)).unwrap();
    std::fs::write(env.lib("Shared/X.Edit.mokuro"), b"{}").unwrap();
    std::fs::write(env.lib("Shared/X.Old.mokuro"), b"{}").unwrap();
    let r = env
        .put("editor", "/mokuro-reader/Shared/X.cbz", cbz(2))
        .await;
    assert_eq!(r.status, 204, "{}", r.text());
    // The strays are now the volumes': their maker neither writes nor deletes them,
    // the volume's uploader and an editor delete them.
    for (stray, _) in strays {
        let path = format!("/mokuro-reader/Shared/{stray}");
        let r = env.put("bob", &path, body.clone()).await;
        assert_eq!(r.status, 403, "bob PUT {stray:?}: {}", r.text());
        let r = env.delete("bob", &path).await;
        assert_eq!(r.status, 403, "bob DELETE {stray:?}: {}", r.text());
        // Still written for nobody.
        let r = env.put("alice", &path, body.clone()).await;
        assert_eq!(r.status, 409, "alice PUT {stray:?}: {}", r.text());
        assert_eq!(r.json()["reason"], "ambiguous-name", "{stray:?}");
    }
    for (user, stray) in [
        ("alice", "V.EDIT.mokuro"),
        ("alice", "STRASSE.mokuro"),
        ("editor", "Vol\u{200B}1.webp"),
        ("editor", "Z.Curated.mokuro"),
    ] {
        let r = env
            .delete(user, &format!("/mokuro-reader/Shared/{stray}"))
            .await;
        assert_eq!(r.status, 204, "{user} DELETE {stray:?}: {}", r.text());
        assert!(!names(&env.lib("Shared")).contains(&stray.to_string()));
    }
}

/// A series folder's owner is found by the folder a request reaches, however it spells
/// it: `STRASSE/` reaches `Straße/` on every host, though their series keys
/// (`fold_series_title_key`) differ. (The request path takes the listed spelling before
/// auth reads it: `bunko_dav::PathCase::rewrite_request`.)
#[tokio::test]
async fn an_owned_series_is_closed_in_every_spelling_the_fold_joins() {
    let env = Env::new();
    assert_eq!(
        env.mkcol("alice", "/mokuro-reader/Stra\u{df}e")
            .await
            .status,
        201
    );
    for path in [
        "/mokuro-reader/STRASSE/Vol 9.cbz",
        "/mokuro-reader/Stra\u{17F}\u{17F}e/Vol 9.cbz",
        "/mokuro-reader/Stra\u{df}e\u{200B}/Vol 9.cbz",
    ] {
        let r = env.put("bob", path, cbz(1)).await;
        assert_eq!(r.status, 403, "{path:?}: {}", r.text());
        assert_eq!(
            r.json()["detail"],
            "This series belongs to another account."
        );
    }
    let r = env.mkcol("bob", "/mokuro-reader/STRASSE/Extras").await;
    assert_eq!(r.status, 403, "{}", r.text());
    assert!(names(&env.lib("Stra\u{df}e")).is_empty());
}

/// `series.json` and `catalog.json` are recognised in every spelling the filename fold
/// joins (`ſeries.json`, `series\u{200B}.json`, `catalog\u{200B}.json`): every host makes
/// them one file with the compiled one. An account without the metadata right gets no
/// raw file of such a name; an update goes to the compiler as one to `series.json`.
#[tokio::test]
async fn compiled_files_are_recognised_in_every_spelling_the_fold_joins() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("T")).unwrap();
    let r = env.put("alice", "/mokuro-reader/T/Vol 1.cbz", cbz(1)).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let update = br#"{"version":2,"series_title":"x","updated_at":"2026-10-31T00:00:00.000Z","external_ids":{},"titles":{},"synonyms":[]}"#;
    for path in [
        "/mokuro-reader/T/\u{17F}eries.json",
        "/mokuro-reader/T/series\u{200B}.json",
        "/mokuro-reader/T/SERIES\u{AD}.JSON",
    ] {
        let r = env.put("bob", path, update.to_vec()).await;
        assert_eq!(r.status, 403, "{path:?}: {}", r.text());
        assert!(
            r.text().contains("cannot submit metadata updates"),
            "{path:?}: {}",
            r.text()
        );
        // An editor's is an update request to the compiler, never a raw write.
        let r = env.put("editor", path, update.to_vec()).await;
        assert!(
            matches!(r.status, 204 | 400),
            "{path:?}: {} {}",
            r.status,
            r.text()
        );
    }
    for path in [
        "/mokuro-reader/catalog\u{200B}.json",
        "/mokuro-reader/CATALOG\u{FE0F}.json",
    ] {
        let r = env.put("bob", path, b"{}".to_vec()).await;
        assert_eq!(r.status, 403, "{path:?}: {}", r.text());
        assert!(r.text().contains("compiled by the server"), "{}", r.text());
    }
    let t = names(&env.lib("T"));
    assert!(
        t.iter().all(|n| n == "Vol 1.cbz" || n == "series.json"),
        "{t:?}"
    );
    let root = names(&env.lib(""));
    assert!(
        root.iter().all(|n| n == "T" || n == "catalog.json"),
        "{root:?}"
    );
}

/// The OCR queue file and the cloud lease marker are recognised in every spelling the
/// filename fold joins (`.MOKURO-QUEUE.JSON`, `.mokuro-queue.jſon`, a zero width space
/// in it): every host makes them one name with Bunko's file. No account, an editor
/// included, makes a file of such a name on a local library (the queue file answers
/// 405, the marker 403, as a cloud store does), and a GET of any spelling serves the
/// generated queue document, never a file a client or a hand made.
#[tokio::test]
async fn control_files_are_recognised_in_every_spelling_the_fold_joins() {
    let env = Env::new();
    let fake = br#"{"version":1,"fake":"bob"}"#.to_vec();
    let r = env.put("bob", "/mokuro-reader/x.json", fake.clone()).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let queue = [
        ".mokuro-queue.json",
        ".MOKURO-QUEUE.JSON",
        ".mokuro-queue.j\u{17F}on",
        ".mokuro\u{200B}-queue.json",
    ];
    let marker = [
        ".mokuro-bunko.json",
        ".MOKURO-BUNKO.JSON",
        ".mokuro-bunko\u{200B}.json",
    ];
    for (names, status) in [(&queue[..], 405), (&marker[..], 403)] {
        for name in names {
            let path = format!("/mokuro-reader/{name}");
            for user in ["bob", "editor"] {
                let r = env.put(user, &path, fake.clone()).await;
                assert_eq!(r.status, status, "{user} PUT {name:?}: {}", r.text());
                let r = env.mkcol(user, &path).await;
                assert_eq!(r.status, status, "{user} MKCOL {name:?}: {}", r.text());
                let r = env.mv(user, "/mokuro-reader/x.json", &path).await;
                assert_eq!(r.status, status, "{user} MOVE onto {name:?}: {}", r.text());
            }
        }
    }
    assert_eq!(names(&env.lib("")), ["x.json"]);
    // One made by hand (or by an earlier beta) is never served for the queue file.
    std::fs::write(env.lib(".MOKURO-QUEUE.JSON"), &fake).unwrap();
    for name in queue {
        let r = env
            .send(
                "reader",
                "GET",
                &format!("/mokuro-reader/{name}"),
                &[],
                Vec::new(),
            )
            .await;
        assert_eq!(r.status, 200, "GET {name:?}: {}", r.text());
        let doc = r.json();
        assert_eq!(doc["version"], 1, "{name:?}");
        assert!(doc.get("fake").is_none(), "GET {name:?}: {doc}");
        assert!(doc.get("volumes").is_some(), "GET {name:?}: {doc}");
    }
}

/// The repair doctor and MIGRATING-0.7 name for a stray (a file the filename fold ties
/// to a volume's files without rule v1 doing so): rename it to the volume's exact
/// spelling. A MOVE whose destination folds like its own source respells that entry
/// and is judged as the source renamed: onto a name rule v1 gives the volume, it is
/// done (a case-only or fold-only rename, on every host); onto another stray spelling,
/// it is refused as a PUT of that name is. Only an account that may MOVE (an editor)
/// makes it; the freeze still runs first.
#[tokio::test]
async fn a_stray_renamed_to_its_volumes_spelling_becomes_its_file() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("S")).unwrap();
    for archive in ["Stra\u{df}e.cbz", "Vol1.cbz", "V.cbz"] {
        let r = env
            .put("alice", &format!("/mokuro-reader/S/{archive}"), cbz(1))
            .await;
        assert_eq!(r.status, 201, "{archive}: {}", r.text());
    }
    let repairs = [
        ("STRASSE.mokuro", "strasse.mokuro", "Stra\u{df}e.mokuro"),
        ("Vol\u{200B}1.webp", "VOL\u{200B}1.webp", "Vol1.webp"),
        ("V.Fast.mokuro", "V.FAST.mokuro", "V.fast.mokuro"),
    ];
    for (stray, _, _) in repairs {
        std::fs::write(env.lib(&format!("S/{stray}")), br#"{"pages":[]}"#).unwrap();
    }
    for (stray, other, fixed) in repairs {
        let from = format!("/mokuro-reader/S/{stray}");
        // Respelled as another stray: still a name written for nobody.
        let r = env
            .mv("editor", &from, &format!("/mokuro-reader/S/{other}"))
            .await;
        assert_eq!(r.status, 409, "MOVE {stray:?} to {other:?}: {}", r.text());
        // An uploader moves nothing.
        let r = env
            .mv("alice", &from, &format!("/mokuro-reader/S/{fixed}"))
            .await;
        assert_eq!(r.status, 403, "alice MOVE {stray:?}: {}", r.text());
        let r = env
            .mv("editor", &from, &format!("/mokuro-reader/S/{fixed}"))
            .await;
        assert!(
            matches!(r.status, 201 | 204),
            "MOVE {stray:?} to {fixed:?}: {} {}",
            r.status,
            r.text()
        );
        let listed = names(&env.lib("S"));
        assert!(listed.contains(&fixed.to_string()), "{fixed:?}: {listed:?}");
        assert!(
            !listed.contains(&stray.to_string()),
            "{stray:?}: {listed:?}"
        );
        assert!(
            !listed.iter().any(|n| n.starts_with(".bunko-case-")),
            "{listed:?}"
        );
    }
    // Now the volume's own files: their controller writes them.
    let r = env
        .put(
            "alice",
            "/mokuro-reader/S/Stra\u{df}e.mokuro",
            br#"{"pages":[1]}"#.to_vec(),
        )
        .await;
    assert_eq!(r.status, 204, "{}", r.text());
}

/// Names the filename fold makes one with a personal correction or the curated layer
/// while rule v1 reads no such layer in them.
const LAYER_STRAYS: [&str; 9] = [
    "X.ed\u{200B}it.mokuro",
    "X.ed\u{131}t.mokuro",
    "X.edit\u{200B}.mokuro",
    "X.e\u{34F}dit.mokuro",
    "X.EDIT.mokuro",
    "X.Edit-2.mokuro",
    "X.cura\u{200B}ted.mokuro",
    "X.Curated.mokuro",
    "X.CURATED-2.mokuro",
];

/// A name the filename fold makes one with a personal correction or the curated layer
/// (`X.ed\u{200B}it.mokuro`, `X.ed\u{131}t.mokuro`, `X.EDIT.mokuro`,
/// `X.cura\u{200B}ted.mokuro`) while rule v1 reads no such layer in it is created by
/// nobody, its volume listed or not: it could only ever stand in for those layers.
#[tokio::test]
async fn a_name_the_fold_joins_with_a_reserved_layer_is_made_by_nobody() {
    let env = Env::new();
    let body = br#"{"pages":[]}"#.to_vec();
    let strays = LAYER_STRAYS;
    std::fs::create_dir_all(env.lib("U")).unwrap();
    std::fs::create_dir_all(env.lib("W")).unwrap();
    let r = env.put("alice", "/mokuro-reader/W/X.cbz", cbz(1)).await;
    assert_eq!(r.status, 201, "{}", r.text());
    for stray in strays {
        // Before its volume exists (nothing governs it), and beside it.
        for (folder, users) in [
            ("U", &["bob", "editor"][..]),
            ("W", &["alice", "editor"][..]),
        ] {
            let path = format!("/mokuro-reader/{folder}/{stray}");
            for user in users {
                let r = env.put(user, &path, body.clone()).await;
                assert_eq!(r.status, 409, "{user} PUT {path:?}: {}", r.text());
                assert_eq!(r.json()["reason"], "ambiguous-name", "{user} {path:?}");
            }
            let r = env.mkcol("editor", &path).await;
            assert_eq!(r.status, 409, "MKCOL {path:?}: {}", r.text());
        }
    }
    assert!(names(&env.lib("U")).is_empty());
    assert_eq!(names(&env.lib("W")), ["X.cbz"]);
}

/// A name the fold joins with a personal correction or the curated layer, already
/// there (made by hand, or before this release, before its volume), never captures
/// them once the volume arrives: a request for `X.edit.mokuro` stays each account's
/// own correction (the stray's bytes are no reader's), and the curated layer is refused
/// naming the stray, never written over it.
#[tokio::test]
async fn a_name_the_fold_joins_with_a_reserved_layer_never_stands_in_for_it() {
    let env = Env::new();
    let body = br#"{"pages":[]}"#.to_vec();
    let strays = LAYER_STRAYS;
    // Strays already there, one folder each: the volume arrives after them.
    for (i, stray) in strays.iter().enumerate() {
        let folder = format!("F{i}");
        std::fs::create_dir_all(env.lib(&folder)).unwrap();
        std::fs::write(env.lib(&format!("{folder}/{stray}")), b"{\"bob\":1}").unwrap();
        let r = env
            .put("alice", &format!("/mokuro-reader/{folder}/X.cbz"), cbz(1))
            .await;
        assert_eq!(r.status, 201, "{stray:?}: {}", r.text());
        let edit = format!("/mokuro-reader/{folder}/X.edit.mokuro");
        let curated = format!("/mokuro-reader/{folder}/X.curated.mokuro");
        for path in [&edit, &curated] {
            let r = env.send("reader", "GET", path, &[], Vec::new()).await;
            assert_eq!(r.status, 404, "GET {path:?} beside {stray:?}: {}", r.text());
        }
        // Each account's own correction, beside any stray.
        for user in ["reader", "bob"] {
            let mine = format!(r#"{{"pages":[],"by":"{user}"}}"#).into_bytes();
            let r = env.put(user, &edit, mine.clone()).await;
            assert_eq!(
                r.status,
                201,
                "{user} PUT {edit:?} beside {stray:?}: {}",
                r.text()
            );
            let r = env.send(user, "GET", &edit, &[], Vec::new()).await;
            assert_eq!(r.status, 200, "{user} GET {edit:?}: {}", r.text());
            assert_eq!(r.body, mine, "{user} GET {edit:?} beside {stray:?}");
        }
        // The shared layer is never written over a stray that is one name with it.
        let r = env.put("alice", &curated, body.clone()).await;
        let joined =
            bunko_core::names::fold_name(stray) == bunko_core::names::fold_name("X.curated.mokuro");
        if joined {
            assert_eq!(
                r.status,
                409,
                "PUT {curated:?} beside {stray:?}: {}",
                r.text()
            );
            assert_eq!(r.json()["reason"], "ambiguous-name", "{stray:?}");
        } else {
            assert_eq!(
                r.status,
                201,
                "PUT {curated:?} beside {stray:?}: {}",
                r.text()
            );
        }
        let listed = names(&env.lib(&folder));
        assert!(listed.contains(&stray.to_string()), "{listed:?}");
        assert_eq!(
            std::fs::read(env.lib(&format!("{folder}/{stray}"))).unwrap(),
            b"{\"bob\":1}",
            "{stray:?} untouched"
        );
    }
}

/// An archive only the filename fold makes `<s>.<layer>.cbz` (`X.ed\u{200B}it.cbz`,
/// `X.ed\u{131}t.cbz`, `X.cura\u{200B}ted.cbz`): whoever uploads it first, every
/// account's own correction of `X`, and the curated publish of its controller, are
/// written once `X.cbz` is there, as before such names were looked at.
#[tokio::test]
async fn an_archive_the_fold_ties_to_a_layer_never_blocks_that_layer() {
    let env = Env::new();
    let cases = [
        ("X.ed\u{200B}it.cbz", "X.edit.mokuro"),
        ("X.ed\u{131}t.cbz", "X.edit.mokuro"),
        ("X.e\u{34F}dit.cbz", "X.edit.mokuro"),
        ("X.edit-\u{200B}2.cbz", "X.edit-2.mokuro"),
        ("X.cura\u{200B}ted.cbz", "X.curated.mokuro"),
    ];
    for (i, (archive, layer)) in cases.iter().enumerate() {
        // A shared folder, made by hand: anyone may add to it.
        let folder = format!("U{i}");
        std::fs::create_dir_all(env.lib(&folder)).unwrap();
        let r = env
            .put("bob", &format!("/mokuro-reader/{folder}/{archive}"), cbz(1))
            .await;
        assert!(
            r.status == 201 || r.status == 409,
            "bob PUT {archive:?}: {} {}",
            r.status,
            r.text()
        );
        let r = env
            .put("alice", &format!("/mokuro-reader/{folder}/X.cbz"), cbz(1))
            .await;
        assert_eq!(
            r.status,
            201,
            "alice PUT X.cbz beside {archive:?}: {}",
            r.text()
        );
        let path = format!("/mokuro-reader/{folder}/{layer}");
        let users: &[&str] = if layer.contains("curated") {
            &["alice"]
        } else {
            &["reader", "alice", "bob"]
        };
        for user in users {
            let mine = format!(r#"{{"pages":[],"by":"{user}"}}"#).into_bytes();
            let r = env.put(user, &path, mine.clone()).await;
            assert!(
                r.status == 201 || r.status == 204,
                "{user} PUT {path:?} after {archive:?}: {} {}",
                r.status,
                r.text()
            );
            let r = env.send(user, "GET", &path, &[], Vec::new()).await;
            assert_eq!(r.status, 200, "{user} GET {path:?}: {}", r.text());
            assert_eq!(r.body, mine, "{user} GET {path:?} after {archive:?}");
        }
    }
    // Such an archive is created by nobody, its volume listed or not.
    std::fs::create_dir_all(env.lib("N")).unwrap();
    for (archive, _) in cases {
        // (Beside `U0/X.cbz`, bob does not control `X`: only the editor gets this far.)
        for (folder, users) in [("N", &["bob", "editor"][..]), ("U0", &["editor"][..])] {
            let path = format!("/mokuro-reader/{folder}/{archive}");
            for user in users {
                let r = env.put(user, &path, cbz(1)).await;
                assert_eq!(r.status, 409, "{user} PUT {path:?}: {}", r.text());
                assert_eq!(r.json()["reason"], "ambiguous-name", "{user} {path:?}");
            }
        }
        // Nor by a MOVE, nor its primary beside one made by hand.
        let r = env.put("alice", "/mokuro-reader/N/Y.cbz", cbz(1)).await;
        assert!(r.status == 201 || r.status == 204, "{}", r.text());
        let to = format!("/mokuro-reader/N/{}", archive.replacen('X', "Y", 1));
        let r = env.mv("editor", "/mokuro-reader/N/Y.cbz", &to).await;
        assert_eq!(r.status, 409, "MOVE to {to:?}: {}", r.text());
    }
    assert_eq!(names(&env.lib("N")), ["Y.cbz"]);
    std::fs::write(env.lib("N/Z.ed\u{200B}it.cbz"), cbz(1)).unwrap();
    let r = env
        .put(
            "editor",
            "/mokuro-reader/N/Z.ed\u{200B}it.mokuro",
            br#"{"pages":[]}"#.to_vec(),
        )
        .await;
    assert_eq!(r.status, 409, "{}", r.text());
    assert_eq!(r.json()["reason"], "ambiguous-name");
}

/// Beside such an archive made by hand (outside Bunko, or before 0.8), each account's
/// own correction still goes to its own copy: nothing in the library is written, so
/// nothing listed there refuses it.
#[tokio::test]
async fn a_hand_made_archive_the_fold_ties_to_a_layer_never_blocks_corrections() {
    let env = Env::new();
    let cases = [
        ("X.ed\u{200B}it.cbz", "X.edit.mokuro"),
        ("X.ed\u{131}t.cbz", "X.edit.mokuro"),
        ("X.e\u{34F}dit.cbz", "X.edit.mokuro"),
        ("X.edit\u{200B}.cbz", "X.edit.mokuro"),
        ("X.edit-\u{200B}2.cbz", "X.edit-2.mokuro"),
    ];
    for (i, (archive, layer)) in cases.iter().enumerate() {
        let folder = format!("H{i}");
        std::fs::create_dir_all(env.lib(&folder)).unwrap();
        std::fs::write(env.lib(&format!("{folder}/{archive}")), cbz(1)).unwrap();
        // Its OCR file too, made by hand: no request for the layer reaches it.
        let primary = archive.replace(".cbz", ".mokuro");
        std::fs::write(env.lib(&format!("{folder}/{primary}")), b"{\"hand\":1}").unwrap();
        let r = env
            .put("alice", &format!("/mokuro-reader/{folder}/X.cbz"), cbz(1))
            .await;
        assert_eq!(
            r.status,
            201,
            "alice PUT X.cbz beside {archive:?}: {}",
            r.text()
        );
        let path = format!("/mokuro-reader/{folder}/{layer}");
        for user in ["reader", "alice", "bob"] {
            let mine = format!(r#"{{"pages":[],"by":"{user}"}}"#).into_bytes();
            let r = env.put(user, &path, mine.clone()).await;
            assert!(
                r.status == 201 || r.status == 204,
                "{user} PUT {path:?} beside {archive:?}: {} {}",
                r.status,
                r.text()
            );
            let r = env.send(user, "GET", &path, &[], Vec::new()).await;
            assert_eq!(r.status, 200, "{user} GET {path:?}: {}", r.text());
            assert_eq!(r.body, mine, "{user} GET {path:?} beside {archive:?}");
        }
        let mut want = vec![archive.to_string(), primary.clone(), "X.cbz".to_string()];
        want.sort();
        assert_eq!(
            names(&env.lib(&folder)),
            want,
            "nothing written in the library"
        );
        assert_eq!(
            std::fs::read(env.lib(&format!("{folder}/{primary}"))).unwrap(),
            b"{\"hand\":1}"
        );
    }
}
