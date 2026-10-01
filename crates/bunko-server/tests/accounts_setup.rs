//! First-run setup wizard. Ports tests/unit/test_setup_api.py and covers the 0.7 setup
//! token that lets a Docker host's browser through the localhost gate.

mod accounts_support;

use accounts_support::*;
use bunko_core::Role;
use bunko_server::accounts::{SETUP_TOKEN_FILE, ensure_setup_token};
use serde_json::json;

fn complete_body() -> serde_json::Value {
    json!({"admin": {"username": "admin", "password": "password123"}, "registration": {"mode": "invite"}})
}

async fn complete(env: &Env, b: http::request::Builder) -> Resp {
    env.send(json_body(b, complete_body())).await
}

#[tokio::test]
async fn complete_requires_localhost() {
    let env = Env::new();
    let r = complete(&env, from_peer("POST", "/setup/api/complete", "203.0.113.10")).await;
    assert_eq!((r.status, r.json()), (403, json!({"error": "Setup is only allowed from localhost"})));
    assert!(env.db.get_user("admin").unwrap().is_none());
}

#[tokio::test]
async fn complete_from_localhost_creates_the_admin_and_saves_the_mode() {
    let env = Env::new();
    let r = complete(&env, req("POST", "/setup/api/complete")).await;
    assert_eq!((r.status, r.json()), (201, json!({"success": true, "message": "Setup completed successfully"})));
    let admin = env.db.get_user("admin").unwrap().unwrap();
    assert_eq!(admin.role, Role::Admin);
    assert_eq!(env.deps.core.config.read().registration.mode, "invite");
    let saved = std::fs::read_to_string(env.dir.path().join("config.yaml")).expect("config saved");
    assert!(saved.contains("invite"), "{saved}");
    // Done: status says so, and a second completion is refused.
    let r = env.send(empty(req("GET", "/setup/api/status"))).await;
    assert_eq!((r.status, r.json()), (200, json!({"needs_setup": false})));
    let r = complete(&env, req("POST", "/setup/api/complete")).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Setup already completed"})));
    // Non-local callers get the status once an admin exists.
    let r = env.send(empty(from_peer("GET", "/setup/api/status", "203.0.113.10"))).await;
    assert_eq!((r.status, r.json()), (200, json!({"needs_setup": false})));
}

#[tokio::test]
async fn validation_and_body_errors() {
    let env = Env::new();
    let send = |body: serde_json::Value| env.send(json_body(req("POST", "/setup/api/complete"), body));
    let r = send(json!({"admin": {"username": "../admin", "password": "password123"}})).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["error"].as_str().unwrap().contains("3-32 characters"));
    let r = send(json!({"admin": {"username": "admin", "password": "short"}})).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Password must be at least 8 characters"})));
    let r = send(json!({})).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Username is required"})));
    let r = env.send(empty(req("POST", "/setup/api/complete"))).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Empty body"})));
    let r = env.send(raw(req("POST", "/setup/api/complete"), b"{nope")).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Invalid JSON"})));
    // An invalid registration mode is ignored; an existing non-admin name is a 409.
    env.user("taken", "password123", Role::Registered);
    let r = send(json!({"admin": {"username": "taken", "password": "password123"}, "registration": {"mode": "bogus"}})).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.json()["error"], "Username 'taken' already exists");
    let r = send(json!({"admin": {"username": "admin2", "password": "password123"}, "registration": {"mode": "bogus"}})).await;
    assert_eq!(r.status, 201);
    assert_eq!(env.deps.core.config.read().registration.mode, "self");
}

#[tokio::test]
async fn forwarded_clients() {
    let env = Env::new();
    let r = complete(&env, req("POST", "/setup/api/complete").header("x-forwarded-for", "203.0.113.10")).await;
    assert_eq!(r.status, 403, "a local proxy forwarding a public client");
    let r = complete(&env, req("POST", "/setup/api/complete").header("x-forwarded-for", "127.0.0.1")).await;
    assert_eq!(r.status, 201);
}

#[tokio::test]
async fn an_unknown_peer_is_not_local() {
    let env = Env::new();
    let b = http::Request::builder().method("POST").uri("/setup/api/complete");
    assert_eq!(complete(&env, b).await.status, 403, "no ConnectInfo: fail closed");
}

#[tokio::test]
async fn ipv6_loopback_is_local() {
    let env = Env::new();
    let r = env.send(empty(from_peer("GET", "/setup/api/status", "::1"))).await;
    assert_eq!((r.status, r.json()), (200, json!({"needs_setup": true})));
}

#[tokio::test]
async fn pages_are_gated_while_setup_is_needed() {
    let env = Env::new();
    for path in ["/setup", "/setup/", "/setup/setup.js", "/setup/api/status"] {
        let r = env.send(empty(from_peer("GET", path, "172.17.0.1"))).await;
        assert_eq!((r.status, r.json()), (403, json!({"error": "Setup is only allowed from localhost"})), "{path}");
    }
    let r = env.send(empty(req("GET", "/setup"))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("cache-control"), Some("no-cache"));
    assert!(r.text().contains("setup.js"));
    let r = env.send(empty(req("GET", "/setup/setup.css"))).await;
    assert_eq!(r.header("content-type"), Some("text/css; charset=utf-8"));
    let r = env.send(empty(req("GET", "/setup/missing.js"))).await;
    assert_eq!((r.status, r.json()), (404, json!({"error": "Not found"})));
    // After setup anyone may load the (now inert) page.
    env.user("root", "password123", Role::Admin);
    let r = env.send(empty(from_peer("GET", "/setup/", "172.17.0.1"))).await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn root_redirects_browsers_to_setup_until_an_admin_exists() {
    let env = Env::new();
    let r = env.send(empty(req("GET", "/").header("accept", "text/html"))).await;
    assert_eq!((r.status, r.header("location")), (302, Some("/setup")));
    assert!(r.body.is_empty());
    // Non-HTML clients are not redirected: the home page heuristics apply.
    let r = env.send(empty(req("GET", "/").header("user-agent", "davfs2/1.5"))).await;
    assert_eq!(r.status, 418);
    env.user("root", "password123", Role::Admin);
    let r = env.send(empty(req("GET", "/").header("accept", "text/html"))).await;
    assert_eq!(r.status, 200, "the home page now");
}

// --- the 0.7 setup token -------------------------------------------------------------

#[tokio::test]
async fn token_file_opens_the_gate_and_is_removed_on_completion() {
    let env = Env::new();
    let layout = env.deps.core.layout.clone();
    let token = ensure_setup_token(&layout).unwrap().expect("a fresh token");
    let docker = "172.17.0.1";

    // Wrong or missing token: still refused.
    let r = env.send(empty(from_peer("GET", "/setup/api/status", docker).header("x-setup-token", "wrong"))).await;
    assert_eq!(r.status, 403);

    // `?token=` on the page sets a cookie scoped to /setup.
    let r = env.send(empty(from_peer("GET", &format!("/setup?token={token}"), docker))).await;
    assert_eq!(r.status, 200);
    let cookie = r.header("set-cookie").expect("cookie").to_string();
    assert!(cookie.starts_with(&format!("mokuro_setup_token={token};")), "{cookie}");
    assert!(cookie.contains("Path=/setup") && cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));

    // The page's own fetches carry the cookie, not the query.
    let jar = format!("mokuro_setup_token={token}");
    let r = env.send(empty(from_peer("GET", "/setup/api/status", docker).header("cookie", jar.as_str()))).await;
    assert_eq!((r.status, r.json()), (200, json!({"needs_setup": true})));
    let r = env.send(empty(from_peer("GET", "/setup/setup.js", docker).header("x-setup-token", token.as_str()))).await;
    assert_eq!(r.status, 200);

    let r = complete(&env, from_peer("POST", "/setup/api/complete", docker).header("cookie", jar.as_str())).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert!(!layout.base.join(SETUP_TOKEN_FILE).exists(), "one-time: deleted on completion");
    let r = complete(&env, from_peer("POST", "/setup/api/complete", docker).header("cookie", jar.as_str())).await;
    assert_eq!(r.status, 403, "the spent token no longer opens the gate");
}

#[tokio::test]
async fn env_token_opens_the_gate() {
    let mut env = Env::new();
    env.deps.setup.env_token = Some("from-the-env".into());
    let r = complete(&env, from_peer("POST", "/setup/api/complete?token=from-the-env", "10.0.0.9")).await;
    assert_eq!(r.status, 201);
    assert_eq!(complete(&env, from_peer("POST", "/setup/api/complete?token=nope", "10.0.0.9")).await.status, 403);
}

#[tokio::test]
async fn cached_once_an_admin_is_seen() {
    let env = Env::new();
    env.user("root", "password123", Role::Admin);
    assert!(!env.deps.setup.needs_setup(&env.db).unwrap());
    // Even a deleted admin keeps setup complete (any status counts, as 0.5.2).
    env.db.delete_user("root").unwrap();
    let fresh = Env::new();
    fresh.user("gone", "password123", Role::Admin);
    fresh.db.delete_user("gone").unwrap();
    assert!(!fresh.deps.setup.needs_setup(&fresh.db).unwrap());
}
