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
