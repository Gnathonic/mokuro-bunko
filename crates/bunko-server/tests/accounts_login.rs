//! Login API: `/login/api/{check,token,me}`, `/api/nav/config`, the login page.
//! Ports tests/integration/test_login_me.py and tests/unit/test_auth_token_requests.py.

mod accounts_support;

use accounts_support::*;
use base64::Engine as _;
use bunko_core::Role;
use parking_lot::Mutex;
use serde_json::json;
use std::sync::Arc;

const PW: &str = "pass1234";

fn me_env() -> Env {
    let env = Env::new();
    env.user("reg", PW, Role::Registered);
    env.user("upl", PW, Role::Uploader);
    env.user("edi", PW, Role::Editor);
    env.user("adm", PW, Role::Admin);
    env.user("umlaut", "pässwörd", Role::Registered);
    env.user("inv", PW, Role::Inviter);
    env
}

async fn me(env: &Env, auth: Option<&str>) -> Resp {
    let mut b = from_peer("GET", "/login/api/me", "192.0.2.77");
    if let Some(a) = auth {
        b = b.header("authorization", a);
    }
    env.send(empty(b)).await
}

#[tokio::test]
async fn me_valid_creds_returns_identity_and_permissions() {
    let env = me_env();
    let r = me(&env, Some(&basic("reg", PW))).await;
    assert_eq!(r.status, 200);
    let b = r.json();
    assert_eq!(b["authenticated"], true);
    assert_eq!(b["username"], "reg");
    assert_eq!(b["role"], "registered");
    assert!(b["created_at"].as_str().is_some_and(|s| s.len() == 19));
    assert_eq!(
        b["permissions"],
        json!({"canWriteProgress": true, "canAddFiles": false, "canModifyDelete": false, "metadata": {"scope": "none"}})
    );
    // 0.5.2 key order (account.js only reads keys, but keep the Python dict order).
    let text = r.text();
    assert!(text.starts_with(r#"{"authenticated":true,"username":"reg","role":"registered","created_at":""#), "{text}");
    assert!(text.contains(r#""permissions":{"canWriteProgress":true,"canAddFiles":false,"canModifyDelete":false,"metadata":{"scope":"none"}}"#));
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert!(r.header("www-authenticate").is_none());
}

#[tokio::test]
async fn me_permissions_per_role() {
    let env = me_env();
    for (user, role, wp, add, md, meta) in [
        ("reg", "registered", true, false, false, json!({"scope": "none"})),
        ("upl", "uploader", true, true, false, json!({"scope": "owned", "ownedSeries": []})),
        ("edi", "editor", true, true, true, json!({"scope": "all"})),
        ("adm", "admin", true, true, true, json!({"scope": "all"})),
        ("inv", "inviter", true, true, true, json!({"scope": "all"})),
    ] {
        let r = me(&env, Some(&basic(user, PW))).await;
        assert_eq!(r.status, 200);
        let b = r.json();
        assert_eq!(b["role"], role);
        assert_eq!(
            b["permissions"],
            json!({"canWriteProgress": wp, "canAddFiles": add, "canModifyDelete": md, "metadata": meta}),
            "{user}"
        );
    }
}

#[tokio::test]
async fn me_utf8_password_and_latin1_header() {
    let env = me_env();
    let r = me(&env, Some(&basic("umlaut", "pässwörd"))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["username"], "umlaut");
    // Latin-1 encoded credentials are malformed: 401, not authenticated.
    let latin1: Vec<u8> = "umlaut:pässwörd".chars().map(|c| c as u32 as u8).collect();
    let header = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(latin1));
    let r = me(&env, Some(&header)).await;
    assert_eq!(r.status, 401);
    assert_eq!(r.json(), json!({"authenticated": false, "error": "Invalid credentials"}));
}

#[tokio::test]
async fn me_failures_and_anonymous() {
    let env = me_env();
    let r = me(&env, Some(&basic("reg", "wrongpass"))).await;
    assert_eq!((r.status, r.json()), (401, json!({"authenticated": false, "error": "Invalid credentials"})));
    let r = me(&env, Some("Basic !!!notb64!!!")).await;
    assert_eq!((r.status, r.json()), (401, json!({"authenticated": false, "error": "Invalid credentials"})));
    let anon = json!({
        "authenticated": false,
        "role": "anonymous",
        "permissions": {"canWriteProgress": false, "canAddFiles": false, "canModifyDelete": false, "metadata": {"scope": "none"}},
    });
    let r = me(&env, None).await;
    assert_eq!((r.status, r.json()), (200, anon.clone()));
    let r = me(&env, Some("Negotiate xyz")).await;
    assert_eq!((r.status, r.json()), (200, anon));
    let r = me(&env, Some("Bearer xyz")).await;
    assert_eq!((r.status, r.json()), (401, json!({"authenticated": false, "error": "Invalid or expired token"})));
}

#[tokio::test]
async fn me_rate_limited_and_garbage_does_not_count() {
    let env = me_env();
    for _ in 0..5 {
        assert_eq!(me(&env, Some("Basic !!!notb64!!!")).await.status, 401);
    }
    for _ in 0..10 {
        assert_eq!(me(&env, Some(&basic("reg", "wrongpass"))).await.status, 401);
    }
    let r = me(&env, Some(&basic("reg", "wrongpass"))).await;
    assert_eq!(r.status, 429);
    let b = r.json();
    assert_eq!(b["authenticated"], false);
    assert!(b["error"].as_str().unwrap().starts_with("Too many failed attempts. Retry in "));
    // The login limiter is not the WebDAV one.
    assert!(env.deps.core.dav_limiter.allow("192.0.2.77:reg").is_ok());
}

#[tokio::test]
async fn me_uploader_sees_exactly_the_folders_it_owns() {
    let env = me_env();
    env.db.record_volume_upload("Dr Stone/Volume 01.cbz", "upl").unwrap();
    env.db.record_volume_upload("Aria/v1.cbz", "upl").unwrap();
    env.db.record_volume_upload("Shared/v1.cbz", "upl").unwrap();
    env.db.record_volume_upload("Shared/v2.cbz", "edi").unwrap();
    let r = me(&env, Some(&basic("upl", PW))).await;
    assert_eq!(r.json()["permissions"]["metadata"], json!({"scope": "owned", "ownedSeries": ["Aria", "Dr Stone"]}));
}

// --- tokens (test_auth_token_requests.py) -----------------------------------------

const APW: &str = "alice-password-1";

fn token_env() -> Env {
    let env = Env::new();
    env.user("alice", APW, Role::Admin);
    env.user("bob", "bob-password-12", Role::Registered);
    env
}

async fn issue(env: &Env, body: serde_json::Value) -> Resp {
    env.send(json_body(req("POST", "/login/api/token"), body)).await
}

#[tokio::test]
async fn a_password_buys_a_token() {
    let env = token_env();
    let r = issue(&env, json!({"username": "alice", "password": APW})).await;
    assert_eq!(r.status, 200);
    let b = r.json();
    assert_eq!(b["token_type"], "Bearer");
    assert_eq!(b["token"].as_str().unwrap().len(), 43);
    assert_eq!(b["kind"], "web");
    assert!(b["expires_at"].as_f64().unwrap() > 0.0);
    assert_eq!(b["user"], json!({"username": "alice", "role": "admin"}));
    assert!(r.text().starts_with(r#"{"token":""#), "key order: {}", r.text());
    // The token works where tokens are accepted.
    let t = b["token"].as_str().unwrap();
    let r = me(&env, Some(&bearer(t))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["username"], "alice");
}

#[tokio::test]
async fn kinds_labels_and_lifetimes() {
    let env = token_env();
    let r = issue(&env, json!({"username": "alice", "password": APW, "kind": "reader", "label": "phone"})).await;
    assert_eq!(r.status, 200);
    let b = r.json();
    assert_eq!(b["kind"], "reader");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    let lifetime = b["expires_at"].as_f64().unwrap() - now;
    assert!((lifetime - 90.0 * 86400.0).abs() < 60.0, "{lifetime}");
    let label: String = env.db.with_writer_connection(|c| {
        c.query_row("SELECT label FROM auth_tokens WHERE kind = 'reader'", [], |r| r.get(0)).unwrap()
    });
    assert_eq!(label, "phone");
    // A falsy kind is the default; an unknown or non-string one is refused.
    assert_eq!(issue(&env, json!({"username": "alice", "password": APW, "kind": ""})).await.json()["kind"], "web");
    for kind in [json!("forever"), json!(5)] {
        let r = issue(&env, json!({"username": "alice", "password": APW, "kind": kind})).await;
        assert_eq!((r.status, r.json()), (400, json!({"error": "kind must be one of web, reader, processor"})));
    }
}

#[tokio::test]
async fn basic_credentials_buy_one_too() {
    let env = token_env();
    let r = env.send(empty(req("POST", "/login/api/token").header("authorization", basic("alice", APW)))).await;
    assert_eq!(r.status, 200);
    assert!(r.json()["token"].is_string());
    let r = env.send(empty(req("POST", "/login/api/token").header("authorization", "Basic %%%"))).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Invalid credentials"})));
    let r = env.send(empty(req("POST", "/login/api/token").header("authorization", "Bearer abc"))).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Missing credentials"})));
}

#[tokio::test]
async fn token_request_errors() {
    let env = token_env();
    let r = issue(&env, json!({"username": "alice", "password": "wrong-password"})).await;
    assert_eq!((r.status, r.json()), (401, json!({"error": "Invalid credentials"})));
    let r = env.send(raw(req("POST", "/login/api/token"), b"{nope")).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Invalid request"})));
    let r = env.send(raw(req("POST", "/login/api/token"), b"[1,2]")).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Invalid request"})));
    let r = issue(&env, json!({"username": "alice"})).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Missing credentials"})));
    let big = vec![b' '; 70 * 1024];
    let r = env
        .send(req("POST", "/login/api/token").header("content-length", big.len().to_string()).body(big.into()).unwrap())
        .await;
    assert_eq!((r.status, r.json()), (413, json!({"error": "Request body too large"})));
}

#[tokio::test]
async fn guessing_is_rate_limited_and_processor_refusals_are_reported() {
    let env = token_env();
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let mut deps = env.deps.clone();
    let sink = seen.clone();
    deps.hooks.on_processor_login_refused = Some(Arc::new(move |u: &str, ip: &str| sink.lock().push((u.into(), ip.into()))));
    let app = app(deps);
    let mut statuses = vec![];
    for n in 0..12 {
        let body = json!({"username": "alice", "password": format!("wrong-{n}"), "kind": "processor"});
        let r = send(app.clone(), json_body(from_peer("POST", "/login/api/token", "198.51.100.4"), body)).await;
        statuses.push(r.status);
    }
    assert_eq!(&statuses[..10], &[401; 10]);
    assert_eq!(&statuses[10..], &[429, 429]);
    {
        let seen = seen.lock();
        assert_eq!(seen.len(), 12, "every refused processor login is reported, limited ones included");
        assert_eq!(seen[0], ("alice".to_string(), "198.51.100.4".to_string()));
    }
    // A web-kind refusal is not reported.
    let body = json!({"username": "bob", "password": "x-wrong-1"});
    let r = send(app.clone(), json_body(from_peer("POST", "/login/api/token", "198.51.100.5"), body)).await;
    assert_eq!(r.status, 401);
    assert_eq!(seen.lock().len(), 12);
}

#[tokio::test]
async fn logout_revokes_the_token_at_once() {
    let env = token_env();
    let t = token(&env, "alice", APW).await;
    let r = env.send(empty(req("DELETE", "/login/api/token").header("authorization", bearer(&t)))).await;
    assert_eq!((r.status, r.json()), (200, json!({"revoked": true})));
    let r = env.send(empty(req("DELETE", "/login/api/token").header("authorization", bearer(&t)))).await;
    assert_eq!((r.status, r.json()), (200, json!({"revoked": false})));
    assert_eq!(me(&env, Some(&bearer(&t))).await.status, 401);
    for auth in [None, Some("Bearer "), Some("Basic abc")] {
        let mut b = req("DELETE", "/login/api/token");
        if let Some(a) = auth {
            b = b.header("authorization", a);
        }
        let r = env.send(empty(b)).await;
        assert_eq!((r.status, r.json()), (400, json!({"error": "No bearer token"})));
    }
}

#[tokio::test]
async fn expired_tokens_are_pruned_on_issue() {
    let env = token_env();
    env.db.create_auth_token("alice", bunko_db::TokenKind::Web, "", Some(-10.0)).unwrap();
    let count = |env: &Env| -> i64 {
        env.db.with_writer_connection(|c| c.query_row("SELECT COUNT(*) FROM auth_tokens", [], |r| r.get(0)).unwrap())
    };
    assert_eq!(count(&env), 1);
    token(&env, "bob", "bob-password-12").await;
    assert_eq!(count(&env), 1, "the expired row is gone, the new one is there");
}

// --- legacy check -----------------------------------------------------------------

#[tokio::test]
async fn check_endpoint() {
    let env = token_env();
    let r = env.send(json_body(req("POST", "/login/api/check"), json!({"username": "alice", "password": APW}))).await;
    assert_eq!((r.status, r.json()), (200, json!({"success": true, "user": {"username": "alice", "role": "admin"}})));
    let r = env.send(json_body(req("POST", "/login/api/check"), json!({"username": "alice", "password": "nope-nope"}))).await;
    assert_eq!((r.status, r.json()), (401, json!({"error": "Invalid credentials"})));
    let r = env.send(empty(req("POST", "/login/api/check"))).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Missing credentials"})));
    let r = env.send(json_body(req("POST", "/login/api/check"), json!({"username": "alice", "password": ""}))).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Missing credentials"})));
    let r = env.send(raw(req("POST", "/login/api/check"), b"\"str\"")).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Invalid request"})));
}

// --- nav config and pages -----------------------------------------------------------

#[tokio::test]
async fn nav_config_reads_the_live_config() {
    let env = Env::new();
    let r = env.send(empty(req("GET", "/api/nav/config"))).await;
    assert_eq!(
        r.text(),
        r#"{"home_enabled":true,"catalog_enabled":false,"queue_show_in_nav":false,"queue_public_access":true,"registration_enabled":true}"#
    );
    {
        let mut c = env.config();
        c.catalog.enabled = true;
        c.catalog.use_as_homepage = true;
        c.queue.show_in_nav = true;
        c.registration.mode = "disabled".into();
    }
    let b = env.send(empty(req("GET", "/api/nav/config"))).await.json();
    assert_eq!(b["home_enabled"], false);
    assert_eq!(b["catalog_enabled"], true);
    assert_eq!(b["queue_show_in_nav"], true);
    assert_eq!(b["registration_enabled"], false);
}

#[tokio::test]
async fn login_pages() {
    let env = Env::new();
    for path in ["/login", "/login/"] {
        let r = env.send(empty(req("GET", path))).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
        assert_eq!(r.header("cache-control"), Some("no-cache"));
        assert!(r.text().contains("/login/login.js"));
    }
    let r = env.send(empty(req("GET", "/login/login.js"))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("application/javascript; charset=utf-8"));
    let r = env.send(empty(req("GET", "/login/missing.css"))).await;
    assert_eq!((r.status, r.text().as_str()), (404, "Not found"));
    let r = env.send(empty(req("GET", "/login/..%2F..%2Fadmin%2Fadmin.js"))).await;
    assert_eq!((r.status, r.text().as_str()), (403, "Forbidden"));
}
