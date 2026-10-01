//! Registration API. Ports tests/integration/test_registration.py.

mod accounts_support;

use accounts_support::*;
use bunko_core::Role;
use bunko_db::{InviteStatus, UserStatus};
use serde_json::json;

fn env(mode: &str, default_role: &str) -> Env {
    Env::with_config(|c| {
        c.registration.mode = mode.into();
        c.registration.default_role = default_role.into();
    })
}

async fn register(env: &Env, body: serde_json::Value) -> Resp {
    env.send(json_body(req("POST", "/api/register"), body)).await
}

#[tokio::test]
async fn self_registration() {
    let env = env("self", "registered");
    let r = register(&env, json!({"username": "newuser", "password": "password123"})).await;
    assert_eq!(r.status, 201);
    assert_eq!(
        r.json(),
        json!({"success": true, "message": "Registration successful", "username": "newuser", "status": "active"})
    );
    let u = env.db.get_user("newuser").unwrap().unwrap();
    assert_eq!((u.role, u.status), (Role::Registered, UserStatus::Active));
    assert!(env.db.authenticate_user("newuser", "password123").unwrap().is_some(), "hashed, and it verifies");

    let r = register(&env, json!({"username": "newuser", "password": "password456"})).await;
    assert_eq!(r.status, 409);
    assert!(r.json()["error"].as_str().unwrap().contains("already exists"));
}

#[tokio::test]
async fn username_is_stripped_before_validation() {
    let env = env("self", "registered");
    let r = register(&env, json!({"username": "  spaced  ", "password": "password123"})).await;
    assert_eq!(r.status, 201);
    assert_eq!(r.json()["username"], "spaced");
    assert!(env.db.get_user("spaced").unwrap().is_some());
}

#[tokio::test]
async fn validation_errors_keep_the_words_register_js_routes_on() {
    let env = env("self", "registered");
    let cases = [
        (json!({"password": "password123"}), "Username"),
        (json!({"username": "testuser"}), "Password"),
        (json!({"username": "testuser", "password": "short"}), "8 characters"),
        (json!({"username": "ab", "password": "password123"}), "3-32 characters"),
        (json!({"username": "test@user!", "password": "password123"}), "3-32 characters"),
        (json!({"username": 5, "password": "password123"}), "Username is required"),
    ];
    for (body, needle) in cases {
        let r = register(&env, body.clone()).await;
        assert_eq!(r.status, 400, "{body}");
        assert!(r.json()["error"].as_str().unwrap().contains(needle), "{body} -> {}", r.text());
    }
    for bad in ["a".repeat(33), "user name".into(), "user/name".into(), "user.name".into()] {
        assert_eq!(register(&env, json!({"username": bad, "password": "password123"})).await.status, 400);
    }
    for good in ["user123", "user_name", "user-name"] {
        assert_eq!(register(&env, json!({"username": good, "password": "password123"})).await.status, 201);
    }
    let r = env.send(raw(req("POST", "/api/register"), b"not json")).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Invalid JSON body"})));
    let r = env.send(raw(req("POST", "/api/register"), b"[1]")).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Invalid JSON body"})));
    let r = env.send(empty(req("POST", "/api/register"))).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Invalid JSON body"})));
}

#[tokio::test]
async fn disabled_mode() {
    let env = env("disabled", "registered");
    let r = register(&env, json!({"username": "newuser", "password": "password123"})).await;
    assert_eq!((r.status, r.json()), (403, json!({"error": "Registration is disabled"})));
}

#[tokio::test]
async fn invite_mode() {
    let env = env("invite", "registered");
    env.user("boss", "boss-password-1", Role::Admin);
    let code = env.db.create_invite(Role::Editor, "7d", Some("boss")).unwrap();

    let r = register(&env, json!({"username": "noinvite", "password": "password123"})).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Invite code is required"})));
    let r = register(&env, json!({"username": "badinvite", "password": "password123", "invite_code": "nope"})).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Invalid or expired invite code"})));
    let r = register(&env, json!({"username": "nullinvite", "password": "password123", "invite_code": null})).await;
    assert_eq!(r.status, 400, "null is the string 'None', not a missing code");
    assert_eq!(r.json()["error"], "Invalid or expired invite code");

    let r = register(&env, json!({"username": "invited", "password": "password123", "invite_code": code})).await;
    assert_eq!(r.status, 201);
    assert_eq!(r.json()["status"], "active");
    assert_eq!(env.db.get_user("invited").unwrap().unwrap().role, Role::Editor);
    let info = env.db.invite_info(&code).unwrap().unwrap();
    assert_eq!(info.used_by.as_deref(), Some("invited"));
    assert_eq!(info.status, InviteStatus::Used);

    let r = register(&env, json!({"username": "second", "password": "password123", "invite_code": code})).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Invalid or expired invite code"})));
}

#[tokio::test]
async fn approval_mode() {
    let env = env("approval", "editor");
    let r = register(&env, json!({"username": "pending1", "password": "password123"})).await;
    assert_eq!(r.status, 201);
    assert_eq!(
        r.json(),
        json!({"success": true, "message": "Registration submitted for approval", "username": "pending1", "status": "pending"})
    );
    let u = env.db.get_user("pending1").unwrap().unwrap();
    assert_eq!((u.role, u.status), (Role::Editor, UserStatus::Pending));
    assert!(env.db.authenticate_user("pending1", "password123").unwrap().is_none());
    env.db.approve_user("pending1").unwrap();
    assert!(env.db.authenticate_user("pending1", "password123").unwrap().is_some());
}

#[tokio::test]
async fn default_role_is_read_live_and_legacy_writer_maps_to_uploader() {
    let env = env("self", "uploader");
    register(&env, json!({"username": "up1", "password": "password123"})).await;
    assert_eq!(env.db.get_user("up1").unwrap().unwrap().role, Role::Uploader);
    env.config().registration.default_role = "writer".into();
    register(&env, json!({"username": "up2", "password": "password123"})).await;
    assert_eq!(env.db.get_user("up2").unwrap().unwrap().role, Role::Uploader);
    env.config().registration.mode = "disabled".into();
    assert_eq!(register(&env, json!({"username": "up3", "password": "password123"})).await.status, 403);
}

#[tokio::test]
async fn info_endpoints() {
    for (mode, expected) in [
        ("self", json!({"mode": "self", "enabled": true})),
        ("disabled", json!({"mode": "disabled", "enabled": false})),
        ("invite", json!({"mode": "invite", "enabled": true, "requires_invite": true})),
        ("approval", json!({"mode": "approval", "enabled": true})),
    ] {
        let env = env(mode, "registered");
        for path in ["/api/register", "/api/register/config"] {
            let r = env.send(empty(req("GET", path))).await;
            assert_eq!((r.status, r.json()), (200, expected.clone()), "{mode} {path}");
        }
    }
}

#[tokio::test]
async fn methods_and_pages() {
    let env = env("self", "registered");
    for path in ["/api/register", "/api/register/config", "/register", "/register/"] {
        let r = env.send(empty(req("OPTIONS", path))).await;
        assert_eq!((r.status, r.header("allow")), (204, Some("GET, POST, OPTIONS")), "{path}");
    }
    for (method, path) in [("PUT", "/api/register"), ("DELETE", "/api/register"), ("POST", "/api/register/config")] {
        let r = env.send(empty(req(method, path))).await;
        assert_eq!((r.status, r.json()), (405, json!({"error": "Method not allowed"})), "{method} {path}");
    }
    for path in ["/register", "/register/"] {
        let r = env.send(empty(req("GET", path))).await;
        assert_eq!(r.status, 200);
        assert!(r.text().contains("register.js"));
        assert!(r.header("cache-control").is_none(), "registration sends no Cache-Control");
    }
    let r = env.send(empty(req("GET", "/register/register.js"))).await;
    assert_eq!(r.status, 200);
    let r = env.send(empty(req("GET", "/register/missing.js"))).await;
    assert_eq!((r.status, r.json()), (404, json!({"error": "File not found"})));
    // Unrelated paths are not ours.
    assert_eq!(env.send(empty(req("GET", "/api/other"))).await.status, 418);
}
