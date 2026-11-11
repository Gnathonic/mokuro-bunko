//! Browse… in the admin Storage panel (`/_admin/api/fs`): the subfolders of a folder
//! on the server, for an admin on that machine only. An admin elsewhere types the
//! path: listing the server's disk to them would be a new exposure.

// Fixtures are set up with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod admin_support;

use admin_support::Harness;
use axum::body::Body;
use axum::extract::ConnectInfo;
use bunko_core::Role;
use http::{Method, Request, StatusCode, header};
use serde_json::json;
use std::net::SocketAddr;
use std::path::Path;

/// `?path=` for a path: every byte but `A-Za-z0-9-_.~/` percent-encoded.
fn query(p: &Path) -> String {
    let mut q = String::from("/_admin/api/fs?path=");
    for b in p.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            q.push(b as char);
        } else {
            q.push_str(&format!("%{b:02X}"));
        }
    }
    q
}

fn get(uri: &str, token: Option<&str>, peer: &str) -> Request<Body> {
    let addr: SocketAddr = format!("{peer}:50000")
        .parse()
        .unwrap_or_else(|_| format!("[{peer}]:50000").parse().expect("peer"));
    let mut b = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .extension(ConnectInfo(addr));
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

fn disk(h: &Harness) -> std::path::PathBuf {
    let root = h.dir.path().join("disk");
    std::fs::create_dir_all(root.join("Manga").join("Series A")).unwrap();
    std::fs::create_dir_all(root.join("comics")).unwrap();
    std::fs::create_dir_all(root.join(".cache")).unwrap();
    std::fs::write(root.join("notes.txt"), "x").unwrap();
    root
}

#[tokio::test]
async fn an_admin_on_this_machine_browses_folders() {
    let h = Harness::new();
    let admin = h.admin();
    let root = disk(&h);
    for peer in ["127.0.0.1", "::1"] {
        let r = h.send(get(&query(&root), Some(&admin), peer)).await;
        assert_eq!(r.status, StatusCode::OK, "{peer}: {}", r.text());
        let v = r.json();
        // Folders only: no dot folders, no files.
        assert_eq!(v["dirs"], json!(["comics", "Manga"]));
        assert_eq!(v["files"], json!([]));
        let path = v["path"].as_str().unwrap();
        assert!(path.ends_with("disk"), "{path}");
        assert!(v["parent"].as_str().is_some());
        assert_eq!(v["sep"], json!(std::path::MAIN_SEPARATOR.to_string()));
    }
    // Into a subfolder, then a path that is not a folder, then one that is not full.
    let r = h
        .send(get(&query(&root.join("Manga")), Some(&admin), "127.0.0.1"))
        .await;
    assert_eq!(r.json()["dirs"], json!(["Series A"]));
    let r = h
        .send(get(
            &query(&root.join("notes.txt")),
            Some(&admin),
            "127.0.0.1",
        ))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let e = r.json()["error"].as_str().unwrap().to_string();
    assert!(e.ends_with("is a file, not a folder"), "{e}");
    let r = h
        .send(get(&query(&root.join("nope")), Some(&admin), "127.0.0.1"))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.json()["error"].as_str().unwrap().contains("nope"));
    let r = h
        .send(get("/_admin/api/fs?path=manga", Some(&admin), "127.0.0.1"))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(
        r.json()["error"]
            .as_str()
            .unwrap()
            .starts_with("manga: not a full path")
    );
}

#[tokio::test]
async fn an_admin_elsewhere_types_the_path() {
    let h = Harness::new();
    let admin = h.admin();
    let root = disk(&h);
    for peer in ["203.0.113.10", "192.0.2.7", "2001:db8::1"] {
        let r = h.send(get(&query(&root), Some(&admin), peer)).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{peer}");
        assert_eq!(
            r.json(),
            json!({"error": "Browse works on this computer only: type the folder's path."})
        );
        assert!(!r.text().contains("Manga"));
    }
}

#[tokio::test]
async fn only_admins_browse() {
    let h = Harness::new();
    let root = disk(&h);
    let reader = h.login("reader", Role::Registered);
    let r = h.send(get(&query(&root), Some(&reader), "127.0.0.1")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(!r.text().contains("Manga"));
    let r = h.send(get(&query(&root), None, "127.0.0.1")).await;
    assert!(
        r.status == StatusCode::UNAUTHORIZED || r.status == StatusCode::FORBIDDEN,
        "{}",
        r.status
    );
    assert!(!r.text().contains("Manga"));
    // A POST is not a listing.
    let admin = h.admin();
    let r = h
        .call("POST", &query(&root), Some(&admin), Some(json!({})))
        .await;
    assert_ne!(r.status, StatusCode::OK);
    assert!(!r.text().contains("Manga"));
}

/// Browse… wants a browser on this machine: a loopback connection a local proxy
/// forwarded, or one from a page naming another host (DNS rebinding), sees nothing.
#[tokio::test]
async fn a_forwarded_or_rebound_request_does_not_browse() {
    let h = Harness::new();
    let admin = h.admin();
    let root = disk(&h);
    for (name, value) in [
        ("x-forwarded-for", "203.0.113.10"),
        ("forwarded", "for=203.0.113.10"),
        ("host", "rebind.example.com:18100"),
    ] {
        let mut r = get(&query(&root), Some(&admin), "127.0.0.1");
        r.headers_mut().insert(name, value.parse().unwrap());
        let r = h.send(r).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{name}: {}", r.text());
        assert!(!r.text().contains("Manga"), "{name}");
    }
    let mut r = get(&query(&root), Some(&admin), "127.0.0.1");
    r.headers_mut()
        .insert(header::HOST, "localhost:18100".parse().unwrap());
    assert_eq!(h.send(r).await.status, StatusCode::OK);
}

fn post_json(uri: &str, token: &str, peer: &str, body: serde_json::Value) -> Request<Body> {
    let addr: SocketAddr = format!("{peer}:50000")
        .parse()
        .unwrap_or_else(|_| format!("[{peer}]:50000").parse().expect("peer"));
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .extension(ConnectInfo(addr))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// The Storage panel's Connect, settled (no longer starting or checking).
async fn settled(h: &Harness, admin: &str) -> serde_json::Value {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let r = h
            .send(get("/_admin/api/storage/connect", Some(admin), "127.0.0.1"))
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text());
        let v = r.json();
        if !matches!(v["state"].as_str(), Some("starting" | "checking")) {
            return v;
        }
        assert!(std::time::Instant::now() < until, "{v}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

fn with_storage() -> Harness {
    Harness::with(admin_support::Options {
        storage: Some(Box::new(|core: &bunko_server::core::Core| {
            bunko_server::storage::StorageControl::for_core(core, None)
        })),
        ..admin_support::Options::default()
    })
}

/// An admin elsewhere (another computer, or a request a local proxy forwarded) may
/// connect a folder library only inside `storage.library.folder_roots`, which no HTTP
/// call sets: with none set, the Connect is refused before the folder is opened and
/// says how to allow it. An admin in a browser on this machine connects it.
#[tokio::test]
async fn an_admin_elsewhere_needs_a_folder_root() {
    let h = with_storage();
    let admin = h.admin();
    let manga = disk(&h).join("Manga");
    let body = json!({"provider": "folder", "path": manga.to_string_lossy()});
    let elsewhere = [
        post_json(
            "/_admin/api/storage/connect",
            &admin,
            "203.0.113.10",
            body.clone(),
        ),
        {
            let mut r = post_json(
                "/_admin/api/storage/connect",
                &admin,
                "127.0.0.1",
                body.clone(),
            );
            r.headers_mut()
                .insert("x-forwarded-for", "203.0.113.10".parse().unwrap());
            r
        },
    ];
    for r in elsewhere {
        let r = h.send(r).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text());
        let v = settled(&h, &admin).await;
        assert_eq!(v["state"], "failed", "{v}");
        let e = v["error"].as_str().unwrap();
        assert!(e.contains("storage.library.folder_roots"), "{e}");
        assert!(e.contains("MOKURO_LIBRARY_FOLDER_ROOTS"), "{e}");
        assert!(v["check"].is_null(), "the folder was never opened: {v}");
    }
    assert_eq!(
        h.core.config.read().storage.library.backend,
        bunko_core::LibraryBackend::Local
    );
    let r = h
        .send(post_json(
            "/_admin/api/storage/connect",
            &admin,
            "127.0.0.1",
            body,
        ))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let v = settled(&h, &admin).await;
    assert_eq!(v["state"], "connected", "{v}");
}

/// `storage.library.folder_roots` is the operator's alone: no admin call, from this
/// machine or elsewhere, changes it, whatever a body names.
#[tokio::test]
async fn no_admin_call_sets_the_folder_roots() {
    let h = with_storage();
    let admin = h.admin();
    let roots = json!(["/", "C:\\"]);
    let body = json!({
        "folder_roots": roots,
        "storage": {"library": {"folder_roots": roots, "path": "/"}},
        "library": {"folder_roots": roots},
        "provider": "folder",
        "path": "/",
    });
    for (method, path) in [
        ("PUT", "/_admin/api/settings/registration"),
        ("PUT", "/_admin/api/settings/cors"),
        ("PUT", "/_admin/api/settings/catalog"),
        ("PUT", "/_admin/api/settings/queue"),
        ("PUT", "/_admin/api/settings/ocr"),
        ("PUT", "/_admin/api/settings/dyndns"),
        ("PUT", "/_admin/api/settings/analytics"),
        ("PUT", "/_admin/api/storage/readers"),
        ("PUT", "/_admin/api/machine/ocr"),
        ("POST", "/_admin/api/storage/connect"),
    ] {
        for peer in ["127.0.0.1", "203.0.113.10"] {
            let mut r = post_json(path, &admin, peer, body.clone());
            *r.method_mut() = Method::from_bytes(method.as_bytes()).unwrap();
            h.send(r).await;
            assert!(
                h.core.config.read().storage.library.folder_roots.is_empty(),
                "{method} {path} from {peer}"
            );
        }
    }
    settled(&h, &admin).await;
    assert!(h.core.config.read().storage.library.folder_roots.is_empty());
    if h.config_path.exists() {
        assert!(h.saved_config().storage.library.folder_roots.is_empty());
    }
}
