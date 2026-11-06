//! Auth admits a progress writer to `<Vol>.edit.mokuro` only as that account's own
//! correction (D27), from a listing of the folder (`auth::Grant::Personal`). The WebDAV
//! handler hands that on to DAV, which lists the folder again where it writes: an
//! archive `<Vol>.edit.cbz` that landed in between makes the path its shared primary,
//! and the write is refused rather than made there on that grant.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::net::{IpAddr, Ipv4Addr};

use axum::body::Body;
use bunko_core::{Config, Role};
use bunko_server::app::{ServeOptions, Services, dav_handler};
use bunko_server::auth::{AuthUser, Grant, Identity};
use bunko_server::core::RequestCtx;
use http::{Request, StatusCode};

const PRIMARY: &str = "/mokuro-reader/S/V.edit.mokuro";

fn reader() -> RequestCtx {
    RequestCtx {
        peer: IpAddr::V4(Ipv4Addr::LOCALHOST),
        client_ip: "127.0.0.1".into(),
        identity: Identity {
            user: Some(AuthUser {
                id: 1,
                username: "reader".into(),
                role: Role::Registered,
            }),
            ..Identity::default()
        },
    }
}

fn put(grant: Option<Grant>) -> Request<Body> {
    let mut req = Request::builder()
        .method("PUT")
        .uri(PRIMARY)
        .header("host", "localhost:8080")
        .body(Body::from(r#"{"pages":[]}"#))
        .unwrap();
    if let Some(grant) = grant {
        req.extensions_mut().insert(grant);
    }
    req
}

#[tokio::test]
async fn a_correction_grant_reaches_dav_and_never_writes_the_shared_primary() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    std::fs::create_dir_all(storage.join("library/S")).unwrap();
    let lib = |rel: &str| storage.join("library").join(rel);
    // As DAV finds the folder: `V.edit.mokuro` is the archive `V.edit.cbz`'s primary.
    std::fs::write(lib("S/V.edit.cbz"), b"volume").unwrap();
    std::fs::write(lib("S/V.edit.mokuro"), b"shared").unwrap();
    let mut config = Config::default();
    config.storage.base_path = storage.clone();
    config.ocr.autobench = false;
    let services = Services::new(config, None, &ServeOptions::default()).unwrap();
    let dav = dav_handler(
        services.dav.clone(),
        services.dav_hooks.clone(),
        services.core.clone(),
    );

    let resp = dav(put(Some(Grant::Personal)), reader()).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(std::fs::read(lib("S/V.edit.mokuro")).unwrap(), b"shared");
    let overlay = storage.join("users/reader/edits/S/V.edit.mokuro");
    assert!(!overlay.exists());

    // Granted by the table instead (what auth answers for an account with the right
    // to write that file), the same request is DAV's to carry out.
    for grant in [Some(Grant::Table), None] {
        let resp = dav(put(grant), reader()).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "{grant:?}");
    }
    assert_eq!(
        std::fs::read(lib("S/V.edit.mokuro")).unwrap(),
        br#"{"pages":[]}"#
    );
}
