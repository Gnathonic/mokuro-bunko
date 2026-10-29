//! Critique C5: Mokuro Reader 1.10.0 regenerates a volume's main sidecar on a rename
//! (new title, new name) and writes it without the `ocr_engine` block Bunko sealed into
//! it. When such a PUT's pages are exactly the sealed main's (the same JCS), at that
//! path or as the main of the same `volume_uuid` in the folder, the server puts the
//! block back and seals it again: same pages, same model output, so a rename keeps its
//! provenance. Anything else is stored as sent.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

/// What Bunko installed: the runner's block, sealed below.
const MAIN: &str = r#"{"version":"0.2.5","title":"A","volume":"V1","volume_uuid":"vu","ocr_engine":{"id":"hayai-nova","revision":1,"detector":"ppocr-manga","patch_budget":512,"created":"2026-10-01T00:00:00Z"},"pages":[{"img_path":"001.jpg","img_width":1200,"img_height":1700,"blocks":[{"box":[10,20,30.5,40],"vertical":true,"font_size":24.0,"lines":["橋"]}]},{"img_path":"002.jpg","blocks":[]}]}"#;
/// MAIN as the 1.10.0 reader writes it after a rename: new names, its own spacing and
/// number spelling, no block.
const RENAMED: &str = r#"{"version": "0.2.5", "title": "B", "volume": "V2", "volume_uuid": "vu", "pages": [{"img_path": "001.jpg", "img_width": 1200, "img_height": 1700, "blocks": [{"box": [10, 20, 30.5, 40], "vertical": true, "font_size": 24, "lines": ["橋"]}]}, {"img_path": "002.jpg", "blocks": []}]}"#;
/// RENAMED with a person's correction.
const CORRECTED: &str = r#"{"version": "0.2.5", "title": "B", "volume": "V2", "volume_uuid": "vu", "pages": [{"img_path": "001.jpg", "img_width": 1200, "img_height": 1700, "blocks": [{"box": [10, 20, 30.5, 40], "vertical": true, "font_size": 24, "lines": ["端"]}]}, {"img_path": "002.jpg", "blocks": []}]}"#;

fn sealed(content: &str) -> String {
    let mut v = bunko_layout::json::Value::parse(content).unwrap();
    bunko_layout::seal::seal(&mut v).expect("an ocr_engine block to seal")
}

struct Env {
    _dir: tempfile::TempDir,
    storage: PathBuf,
    _services: Services,
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
        services
            .db
            .create_user("admin", "pass1234", Role::Admin, UserStatus::Active, "")
            .unwrap();
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

    fn write(&self, rel: &str, content: &str) {
        let path = self.lib(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.lib(rel)).unwrap()
    }

    fn json(&self, rel: &str) -> Value {
        serde_json::from_str(&self.read(rel)).unwrap()
    }

    async fn put(&self, rel: &str, body: &str) -> StatusCode {
        use base64::Engine;
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("admin:pass1234")
        );
        let req = Request::builder()
            .method("PUT")
            .uri(format!("/mokuro-reader/{rel}"))
            .header("host", "localhost:8080")
            .header("authorization", auth)
            .header("content-length", body.len().to_string())
            .body(Body::from(body.to_string()))
            .unwrap();
        self.app.clone().oneshot(req).await.unwrap().status()
    }
}

/// The block MAIN carries once sealed.
fn sealed_block() -> Value {
    serde_json::from_str::<Value>(&sealed(MAIN)).unwrap()["ocr_engine"].clone()
}

/// The file as sent, plus the sealed block: the reader's names and values are kept.
fn assert_reattached(env: &Env, rel: &str, sent: &str) {
    let text = env.read(rel);
    assert!(
        bunko_layout::seal::is_sealed(text.as_bytes()),
        "{rel}: sealed"
    );
    let mut got: Value = serde_json::from_str(&text).unwrap();
    let block = got
        .as_object_mut()
        .unwrap()
        .shift_remove("ocr_engine")
        .expect("the block is back");
    assert_eq!(
        block,
        sealed_block(),
        "{rel}: Bunko's block, its seal unchanged"
    );
    assert_eq!(got, serde_json::from_str::<Value>(sent).unwrap());
}

/// A sealed main at the PUT's own path: a rename that kept the volume's name, a
/// re-upload, a title change.
#[tokio::test]
async fn a_main_rewritten_in_place_with_the_same_pages_keeps_its_block() {
    let env = Env::new();
    env.write("A/V1.cbz", "PK");
    env.write("A/V1.mokuro", &sealed(MAIN));
    assert_eq!(env.put("A/V1.mokuro", RENAMED).await, 204);
    assert_reattached(&env, "A/V1.mokuro", RENAMED);
}

/// The 1.10.0 rename uploads the new main BEFORE it moves the archive and deletes the
/// old main: the sealed main of the same volume is still in the folder.
#[tokio::test]
async fn a_renamed_main_beside_the_sealed_main_of_its_volume_keeps_the_block() {
    let env = Env::new();
    env.write("A/V1.cbz", "PK");
    env.write("A/V1.mokuro", &sealed(MAIN));
    assert_eq!(env.put("A/V2.mokuro", RENAMED).await, 201);
    assert_reattached(&env, "A/V2.mokuro", RENAMED);
    assert_eq!(
        env.read("A/V1.mokuro"),
        sealed(MAIN),
        "the old main is untouched"
    );
}

/// Never a block on anything else: other pages, another volume, an unsealed main, a
/// layer, a file a reader marked EDITED, one that has a block of its own.
#[tokio::test]
async fn a_put_whose_pages_differ_or_that_is_no_main_gets_no_block() {
    let env = Env::new();
    env.write("A/V1.cbz", "PK");
    env.write("A/V1.mokuro", &sealed(MAIN));
    // A person's correction, in place and as the renamed main.
    assert_eq!(env.put("A/V2.mokuro", CORRECTED).await, 201);
    assert_eq!(env.read("A/V2.mokuro"), CORRECTED);
    // The same pages under another volume's uuid.
    let other = RENAMED.replace(r#""volume_uuid": "vu""#, r#""volume_uuid": "other""#);
    assert_eq!(env.put("A/V3.mokuro", &other).await, 201);
    assert_eq!(env.read("A/V3.mokuro"), other);
    // A layer of the volume with the main's pages.
    assert_eq!(env.put("A/V1.fast.mokuro", RENAMED).await, 201);
    assert_eq!(env.read("A/V1.fast.mokuro"), RENAMED);
    // Marked EDITED: a reader's own word on it.
    let edited = RENAMED.replacen('{', r#"{"edited": {"by": "mokuro-reader"}, "#, 1);
    assert_eq!(env.put("A/V4.mokuro", &edited).await, 201);
    assert_eq!(env.read("A/V4.mokuro"), edited);
    // A block of its own (even an empty one) stays as sent.
    let own = RENAMED.replacen('{', r#"{"ocr_engine": {}, "#, 1);
    assert_eq!(env.put("A/V5.mokuro", &own).await, 201);
    assert_eq!(env.read("A/V5.mokuro"), own);
    // In place, with other pages.
    assert_eq!(env.put("A/V1.mokuro", CORRECTED).await, 204);
    assert_eq!(env.read("A/V1.mokuro"), CORRECTED);
    assert!(env.json("A/V1.mokuro").get("ocr_engine").is_none());

    // A stamp without a seal proves nothing: nothing to carry over.
    env.write("B/V1.cbz", "PK");
    env.write("B/V1.mokuro", MAIN);
    assert_eq!(env.put("B/V1.mokuro", RENAMED).await, 204);
    assert_eq!(env.read("B/V1.mokuro"), RENAMED);
    assert_eq!(env.put("B/V2.mokuro", RENAMED).await, 201);
    assert_eq!(env.read("B/V2.mokuro"), RENAMED);
}
