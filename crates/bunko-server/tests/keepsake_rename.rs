//! A reader renames a volume by moving every file it lists under the old stem (D27,
//! plan §1): Bunko's keepsakes (`pre-upgrade[-n]`) and the other reserved layers
//! (`curated-<n>`, `previous-ocr*`) included. Nothing new is written when the id stays
//! the same, so the 0.7 MOVE rule decides; a client still cannot move another file
//! into one of those names.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::{Request, StatusCode};
use tower::ServiceExt;

struct Env {
    _dir: tempfile::TempDir,
    storage: PathBuf,
    _services: Services,
    app: Router,
}

fn basic(user: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:pass1234"))
    )
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(storage.join("library/S")).unwrap();
        let mut config = Config::default();
        config.storage.base_path = storage.clone();
        config.ocr.autobench = false;
        let opts = ServeOptions::default();
        let services = Services::new(config, None, &opts).unwrap();
        for (name, role) in [("editor", Role::Editor), ("reader", Role::Registered)] {
            services
                .db
                .create_user(name, "pass1234", role, UserStatus::Active, "")
                .unwrap();
        }
        let app = assemble(&services, &opts);
        Env {
            _dir: dir,
            storage,
            _services: services,
            app,
        }
    }

    fn lib(&self, rel: &str) -> PathBuf {
        self.storage.join("library").join(rel)
    }

    async fn mv(&self, user: &str, from: &str, to: &str) -> StatusCode {
        let req = Request::builder()
            .method("MOVE")
            .uri(format!("/mokuro-reader/S/{from}"))
            .header("host", "localhost:8080")
            .header("authorization", basic(user))
            .header(
                "destination",
                format!("http://localhost:8080/mokuro-reader/S/{to}"),
            )
            .body(Body::empty())
            .unwrap();
        self.app.clone().oneshot(req).await.unwrap().status()
    }
}

const LAYERS: [&str; 4] = ["pre-upgrade", "pre-upgrade-2", "curated-2", "previous-ocr"];

#[tokio::test]
async fn a_volume_rename_carries_its_keepsakes_and_reserved_layers() {
    let env = Env::new();
    std::fs::write(env.lib("S/V.cbz"), b"volume").unwrap();
    std::fs::write(env.lib("S/V.mokuro"), b"{\"pages\":[]}").unwrap();
    for id in LAYERS {
        std::fs::write(env.lib(&format!("S/V.{id}.mokuro")), id).unwrap();
    }
    // A registered account renames nothing (the 0.7 rule), keepsakes included.
    assert_eq!(
        env.mv("reader", "V.pre-upgrade.mokuro", "W.pre-upgrade.mokuro")
            .await,
        StatusCode::FORBIDDEN
    );
    // The reader's order: the archive, then every listed sidecar.
    assert_eq!(
        env.mv("editor", "V.cbz", "W.cbz").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        env.mv("editor", "V.mokuro", "W.mokuro").await,
        StatusCode::NO_CONTENT
    );
    for id in LAYERS {
        let (from, to) = (format!("V.{id}.mokuro"), format!("W.{id}.mokuro"));
        assert_eq!(
            env.mv("editor", &from, &to).await,
            StatusCode::NO_CONTENT,
            "{id}"
        );
        assert_eq!(
            std::fs::read_to_string(env.lib(&format!("S/{to}"))).unwrap(),
            id
        );
        assert!(!env.lib(&format!("S/{from}")).exists(), "{id}");
    }
    // Still no way to mint a keepsake from another layer.
    std::fs::write(env.lib("S/W.hayai.mokuro"), b"{\"pages\":[]}").unwrap();
    assert_eq!(
        env.mv("editor", "W.hayai.mokuro", "W.pre-upgrade-3.mokuro")
            .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        env.mv("editor", "W.pre-upgrade.mokuro", "W.pre-upgrade-3.mokuro")
            .await,
        StatusCode::FORBIDDEN
    );
}
