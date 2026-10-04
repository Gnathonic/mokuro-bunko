//! `admin.enabled: false` takes the admin panel and API away, as in 0.5.2 (server.py
//! mounted `AdminAPI` only when enabled): `/_admin…` falls through to WebDAV.

use axum::body::Body;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::{Request, StatusCode};
use tower::ServiceExt;

fn basic(user: &str, pass: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
    )
}

async fn admin_status(enabled: bool) -> StatusCode {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.storage.base_path = dir.path().to_path_buf();
    config.admin.enabled = enabled;
    let layout = config.storage.layout();
    std::fs::create_dir_all(layout.library()).unwrap();
    let opts = ServeOptions::default();
    let services = Services::new(config, None, &opts).unwrap();
    services
        .db
        .create_user(
            "boss",
            "boss-password-1",
            Role::Admin,
            UserStatus::Active,
            "",
        )
        .unwrap();
    let app = assemble(&services, &opts);
    let req = Request::get("/_admin/api/status")
        .header("authorization", basic("boss", "boss-password-1"))
        .body(Body::empty())
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn admin_api_is_mounted_only_when_enabled() {
    assert_eq!(admin_status(true).await, StatusCode::OK);
    let off = admin_status(false).await;
    assert_ne!(
        off,
        StatusCode::OK,
        "admin API answered with admin.enabled = false"
    );
    assert_eq!(off, StatusCode::NOT_FOUND, "falls through to WebDAV");
}
