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
