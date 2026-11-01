//! A reader renaming a volume moves its cover first and the archive last. A cover Bunko
//! makes in between, from the archive still under the old name, must not outlive the
//! archive's move: left beside nothing, the reader takes it for a volume it does not
//! have and refuses to rename the series (found by the interop rig's S4 on dav).

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::io::Write as _;
use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::{Request, StatusCode};
use tokio_util::sync::CancellationToken;
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

fn enc(name: &str) -> String {
    name.replace(' ', "%20")
}

fn png() -> Vec<u8> {
    let mut out = Vec::new();
    image::RgbImage::from_fn(12, 16, |x, y| image::Rgb([x as u8, y as u8, 7]))
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
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
        services
            .db
            .create_user("editor", "pass1234", Role::Editor, UserStatus::Active, "")
            .unwrap();
        let app = assemble(&services, &opts);
        let env = Env {
            _dir: dir,
            storage,
            services,
            app,
        };
        let f = std::fs::File::create(env.lib("Vol 1.cbz")).unwrap();
        let mut z = zip::ZipWriter::new(f);
        z.start_file("001.png", zip::write::SimpleFileOptions::default())
            .unwrap();
        z.write_all(&png()).unwrap();
        z.finish().unwrap();
        env
    }

    fn lib(&self, name: &str) -> PathBuf {
        self.storage.join("library/S").join(name)
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.storage.join("library/S"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// One pass of the cover worker: the covers it made.
    fn covers(&self) -> usize {
        self.services
            .thumbs
            .scan_once(&self.storage.join("library"), &CancellationToken::new())
    }

    async fn mv(&self, from: &str, to: &str) -> StatusCode {
        let req = Request::builder()
            .method("MOVE")
            .uri(format!("/mokuro-reader/S/{}", enc(from)))
            .header("host", "localhost:8080")
            .header("authorization", basic("editor"))
            .header(
                "destination",
                format!("http://localhost:8080/mokuro-reader/S/{}", enc(to)),
            )
            .body(Body::empty())
            .unwrap();
        self.app.clone().oneshot(req).await.unwrap().status()
    }

    async fn delete(&self, name: &str) -> StatusCode {
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/mokuro-reader/S/{}", enc(name)))
            .header("host", "localhost:8080")
            .header("authorization", basic("editor"))
            .body(Body::empty())
            .unwrap();
        self.app.clone().oneshot(req).await.unwrap().status()
    }
}

/// The reader moved its cover, Bunko made one for the archive still under the old name,
/// then the reader moved the archive: the cover Bunko made goes, the reader's stays.
#[tokio::test]
async fn a_cover_made_between_a_renames_moves_goes_when_the_archive_moves() {
    let env = Env::new();
    std::fs::write(env.lib("Vol 1.webp"), b"the reader's cover").unwrap();
    assert_eq!(
        env.mv("Vol 1.webp", "Vol 1 renamed.webp").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(env.covers(), 1);
    assert!(env.lib("Vol 1.webp").exists());

    assert_eq!(
        env.mv("Vol 1.cbz", "Vol 1 renamed.cbz").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(env.names(), ["Vol 1 renamed.cbz", "Vol 1 renamed.webp"]);
    assert_eq!(
        std::fs::read(env.lib("Vol 1 renamed.webp")).unwrap(),
        b"the reader's cover"
    );
}

/// No cover at the new name (the reader had none to move, or moves it after the
/// archive): the one Bunko made goes along with the archive. A reader moving it after
/// the archive then finds it already there.
#[tokio::test]
async fn a_cover_bunko_made_follows_its_archive_to_a_name_without_one() {
    let env = Env::new();
    assert_eq!(env.covers(), 1);
    let made = std::fs::read(env.lib("Vol 1.webp")).unwrap();

    assert_eq!(
        env.mv("Vol 1.cbz", "Vol 1 renamed.cbz").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(env.names(), ["Vol 1 renamed.cbz", "Vol 1 renamed.webp"]);
    assert_eq!(std::fs::read(env.lib("Vol 1 renamed.webp")).unwrap(), made);
    // Nothing left for the worker to make.
    assert_eq!(env.covers(), 0);
}

/// A cover a client wrote is the client's to move (sidecars stay pinned on a MOVE).
#[tokio::test]
async fn a_clients_own_cover_stays_where_it_is() {
    let env = Env::new();
    std::fs::write(env.lib("Vol 1.webp"), b"the reader's cover").unwrap();
    assert_eq!(env.covers(), 0);

    assert_eq!(
        env.mv("Vol 1.cbz", "Vol 1 renamed.cbz").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(env.names(), ["Vol 1 renamed.cbz", "Vol 1.webp"]);
}

/// A cover Bunko replaced by a client's upload is the client's too.
#[tokio::test]
async fn a_cover_a_client_replaced_stays_where_it_is() {
    let env = Env::new();
    assert_eq!(env.covers(), 1);
    std::fs::write(env.lib("Vol 1.webp"), b"the reader's own, a few bytes").unwrap();

    assert_eq!(
        env.mv("Vol 1.cbz", "Vol 1 renamed.cbz").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(env.names(), ["Vol 1 renamed.cbz", "Vol 1.webp"]);
}

/// A deleted archive takes its sidecars, a cover Bunko made included.
#[tokio::test]
async fn a_deleted_archive_leaves_no_cover() {
    let env = Env::new();
    assert_eq!(env.covers(), 1);
    assert_eq!(env.delete("Vol 1.cbz").await, StatusCode::NO_CONTENT);
    assert!(env.names().is_empty(), "{:?}", env.names());
}
