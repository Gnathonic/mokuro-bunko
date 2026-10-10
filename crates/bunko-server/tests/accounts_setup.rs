//! First-run setup wizard. Ports tests/unit/test_setup_api.py and covers the 0.7 setup
//! code that lets another computer (a Docker host's or a NAS's browser) through the
//! localhost gate.

mod accounts_support;

use accounts_support::*;
use bunko_core::Role;
use bunko_server::accounts::{
    ATTEMPTS_PER_IP, LEGACY_TOKEN_FILE, REMOTE_NEEDS_CODE, ROTATE_AFTER, SESSION_COOKIE,
    normalize_code, remove_legacy_token,
};
use serde_json::json;

/// A form POST of the code page.
fn code_form(peer: &str, code: &str) -> http::Request<axum::body::Body> {
    let body = format!("code={}", code.replace(' ', "+"));
    from_peer("POST", "/setup/code", peer)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(body))
        .expect("request")
}

fn remote_refusal() -> serde_json::Value {
    json!({ "error": REMOTE_NEEDS_CODE })
}

fn complete_body() -> serde_json::Value {
    json!({"admin": {"username": "admin", "password": "password123"}, "registration": {"mode": "invite"}})
}

async fn complete(env: &Env, b: http::request::Builder) -> Resp {
    env.send(json_body(b, complete_body())).await
}

#[tokio::test]
async fn complete_requires_localhost() {
    let env = Env::new();
    let r = complete(
        &env,
        from_peer("POST", "/setup/api/complete", "203.0.113.10"),
    )
    .await;
    assert_eq!((r.status, r.json()), (403, remote_refusal()));
    assert!(
        r.json()["error"]
            .as_str()
            .unwrap()
            .contains("see the server log for the setup code")
    );
    assert!(env.db.get_user("admin").unwrap().is_none());
}

#[tokio::test]
async fn complete_from_localhost_creates_the_admin_and_saves_the_mode() {
    let env = Env::new();
    let r = complete(&env, req("POST", "/setup/api/complete")).await;
    assert_eq!(
        (r.status, r.json()),
        (
            201,
            json!({"success": true, "message": "Setup completed successfully"})
        )
    );
    let admin = env.db.get_user("admin").unwrap().unwrap();
    assert_eq!(admin.role, Role::Admin);
    assert_eq!(env.deps.core.config.read().registration.mode, "invite");
    let saved = std::fs::read_to_string(env.dir.path().join("config.yaml")).expect("config saved");
    assert!(saved.contains("invite"), "{saved}");
    // Done: status says so, and a second completion is refused.
    let r = env.send(empty(req("GET", "/setup/api/status"))).await;
    assert_eq!((r.status, r.json()), (200, json!({"needs_setup": false})));
    let r = complete(&env, req("POST", "/setup/api/complete")).await;
    assert_eq!(
        (r.status, r.json()),
        (400, json!({"error": "Setup already completed"}))
    );
    // Non-local callers get the status once an admin exists.
    let r = env
        .send(empty(from_peer("GET", "/setup/api/status", "203.0.113.10")))
        .await;
    assert_eq!((r.status, r.json()), (200, json!({"needs_setup": false})));
}

#[tokio::test]
async fn validation_and_body_errors() {
    let env = Env::new();
    let send =
        |body: serde_json::Value| env.send(json_body(req("POST", "/setup/api/complete"), body));
    let r = send(json!({"admin": {"username": "../admin", "password": "password123"}})).await;
    assert_eq!(r.status, 400);
    assert!(
        r.json()["error"]
            .as_str()
            .unwrap()
            .contains("3-32 characters")
    );
    let r = send(json!({"admin": {"username": "admin", "password": "short"}})).await;
    assert_eq!(
        (r.status, r.json()),
        (
            400,
            json!({"error": "Password must be at least 8 characters"})
        )
    );
    let r = send(json!({})).await;
    assert_eq!(
        (r.status, r.json()),
        (400, json!({"error": "Username is required"}))
    );
    let r = env.send(empty(req("POST", "/setup/api/complete"))).await;
    assert_eq!((r.status, r.json()), (400, json!({"error": "Empty body"})));
    let r = env
        .send(raw(req("POST", "/setup/api/complete"), b"{nope"))
        .await;
    assert_eq!(
        (r.status, r.json()),
        (400, json!({"error": "Invalid JSON"}))
    );
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
    let r = complete(
        &env,
        req("POST", "/setup/api/complete").header("x-forwarded-for", "203.0.113.10"),
    )
    .await;
    assert_eq!(r.status, 403, "a local proxy forwarding a public client");
    let r = complete(
        &env,
        req("POST", "/setup/api/complete").header("x-forwarded-for", "127.0.0.1"),
    )
    .await;
    assert_eq!(r.status, 201);
}

#[tokio::test]
async fn an_unknown_peer_is_not_local() {
    let env = Env::new();
    let b = http::Request::builder()
        .method("POST")
        .uri("/setup/api/complete");
    assert_eq!(
        complete(&env, b).await.status,
        403,
        "no ConnectInfo: fail closed"
    );
}

#[tokio::test]
async fn ipv6_loopback_is_local() {
    let env = Env::new();
    let r = env
        .send(empty(from_peer("GET", "/setup/api/status", "::1")))
        .await;
    assert_eq!((r.status, r.json()), (200, json!({"needs_setup": true})));
}

#[tokio::test]
async fn pages_are_gated_while_setup_is_needed() {
    let env = Env::new();
    env.deps.setup.issue_code();
    for path in ["/setup/setup.js", "/setup/index.html", "/setup/api/status"] {
        let r = env.send(empty(from_peer("GET", path, "172.17.0.1"))).await;
        assert_eq!((r.status, r.json()), (403, remote_refusal()), "{path}");
    }
    // The wizard's address answers the code page instead (HTML, not JSON)...
    for path in ["/setup", "/setup/"] {
        let r = env.send(empty(from_peer("GET", path, "172.17.0.1"))).await;
        assert_eq!(r.status, 200, "{path}");
        assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
        assert!(r.text().contains(r#"name="code""#), "{}", r.text());
        assert!(!r.text().contains("setup.js"), "not the wizard");
    }
    // ...whose stylesheet is not gated.
    let r = env
        .send(empty(from_peer("GET", "/setup/setup.css", "172.17.0.1")))
        .await;
    assert_eq!(r.status, 200);
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
    let r = env
        .send(empty(from_peer("GET", "/setup/", "172.17.0.1")))
        .await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn root_redirects_browsers_to_setup_until_an_admin_exists() {
    let env = Env::new();
    let r = env
        .send(empty(req("GET", "/").header("accept", "text/html")))
        .await;
    assert_eq!((r.status, r.header("location")), (302, Some("/setup")));
    assert!(r.body.is_empty());
    // Non-HTML clients are not redirected: the home page heuristics apply.
    let r = env
        .send(empty(req("GET", "/").header("user-agent", "davfs2/1.5")))
        .await;
    assert_eq!(r.status, 418);
    env.user("root", "password123", Role::Admin);
    let r = env
        .send(empty(req("GET", "/").header("accept", "text/html")))
        .await;
    assert_eq!(r.status, 200, "the home page now");
}

// --- the 0.7 setup code --------------------------------------------------------------

const DOCKER: &str = "172.17.0.1";

/// The session cookie's `name=value` out of a Set-Cookie header.
fn jar_of(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap().trim().to_string()
}

#[tokio::test]
async fn a_remote_request_without_a_code_gets_the_code_page() {
    let env = Env::new();
    let code = env.deps.setup.issue_code();
    let r = env.send(empty(from_peer("GET", "/setup", DOCKER))).await;
    assert_eq!(r.status, 200);
    let page = r.text();
    assert!(
        page.contains(r#"<form method="post" action="/setup/code""#),
        "{page}"
    );
    assert!(page.contains("server log"), "{page}");
    assert!(!page.contains(&code) && !page.contains(&normalize_code(&code)));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    // The API says how to get one.
    let r = env
        .send(empty(from_peer("GET", "/setup/api/status", DOCKER)))
        .await;
    assert_eq!((r.status, r.json()), (403, remote_refusal()));
    // Without an active code (no startup announcement) the page says so.
    let fresh = Env::new();
    let r = fresh.send(empty(from_peer("GET", "/setup", DOCKER))).await;
    assert!(r.text().contains("No setup code is active"), "{}", r.text());
}

#[tokio::test]
async fn a_wrong_code_is_refused_then_rate_limited() {
    let env = Env::new();
    let code = env.deps.setup.issue_code();
    let peer = "198.51.100.7";
    for i in 0..ATTEMPTS_PER_IP {
        let r = env.send(code_form(peer, "ZZZZZ-ZZZZZ")).await;
        assert_eq!(r.status, 403, "attempt {i}");
        assert!(r.text().contains("not the setup code"));
        assert!(r.header("set-cookie").is_none());
    }
    // Over the limit: refused without looking, even the right code.
    let r = env.send(code_form(peer, &code)).await;
    assert_eq!(r.status, 429);
    assert!(r.text().contains("Too many attempts"));
    assert!(r.header("retry-after").is_some());
    assert!(r.header("set-cookie").is_none());
    // The API's header path counts against the same limit.
    let r = env
        .send(empty(
            from_peer("GET", "/setup/api/status", peer).header("x-setup-code", code.as_str()),
        ))
        .await;
    assert_eq!(r.status, 429);
    assert!(r.json()["error"].as_str().unwrap().starts_with("Too many"));
    // Another computer still gets its tries.
    let r = env.send(code_form("198.51.100.8", &code)).await;
    assert_eq!(r.status, 303);
    assert!(env.db.get_user("admin").unwrap().is_none());
}

/// A flood of wrong codes from many addresses (and forged X-Forwarded-For headers from
/// an untrusted peer) never locks setup: localhost still works, the code is replaced
/// rather than locked, and a forged header buys no fresh tries.
#[tokio::test]
async fn an_attack_never_locks_the_owner_out() {
    let env = Env::new();
    env.deps.setup.issue_code();
    let attacker = "198.51.100.66";
    // Forged forwarding headers from a peer that is no trusted proxy: one bucket.
    for i in 0..10 {
        let r = env
            .send(
                from_peer("POST", "/setup/code", attacker)
                    .header("x-forwarded-for", format!("10.9.{i}.1"))
                    .header("x-real-ip", format!("10.8.{i}.1"))
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(axum::body::Body::from("code=ZZZZZ-ZZZZZ"))
                    .unwrap(),
            )
            .await;
        assert_eq!(
            r.status,
            if i < ATTEMPTS_PER_IP as usize {
                403
            } else {
                429
            },
            "{i}"
        );
    }
    // Many addresses: past the rotation count, the code is replaced, never locked.
    for i in 0..(ROTATE_AFTER * 2) {
        let r = env
            .send(code_form(
                &format!("203.0.{}.{}", i / 250, i % 250 + 1),
                "ZZZZZ-ZZZZZ",
            ))
            .await;
        assert_eq!(r.status, 403, "attempt {i} is refused, not limited");
    }
    // Localhost always works.
    let r = env
        .send(empty(from_peer("GET", "/setup/api/status", "127.0.0.1")))
        .await;
    assert_eq!((r.status, r.json()), (200, json!({"needs_setup": true})));
    let r = complete(&env, from_peer("POST", "/setup/api/complete", "127.0.0.1")).await;
    assert_eq!(r.status, 201, "{}", r.text());
}

#[tokio::test]
async fn the_right_code_opens_the_wizard_once() {
    let env = Env::new();
    let code = env.deps.setup.issue_code();
    // Typed loosely: lower case, spaces instead of the dash.
    let typed = code.to_lowercase().replace('-', " ");
    let r = env.send(code_form(DOCKER, &typed)).await;
    assert_eq!((r.status, r.header("location")), (303, Some("/setup")));
    let cookie = r
        .header("set-cookie")
        .expect("a session cookie")
        .to_string();
    assert!(
        cookie.starts_with(&format!("{SESSION_COOKIE}=")),
        "{cookie}"
    );
    for attr in ["Path=/setup", "HttpOnly", "SameSite=Strict", "Max-Age=3600"] {
        assert!(cookie.contains(attr), "{cookie}");
    }
    assert!(!cookie.contains("Secure"), "plain http");
    assert!(
        !cookie.contains(&normalize_code(&code)) && !cookie.contains(&code),
        "the cookie is a session id, not the code"
    );
    let jar = jar_of(&cookie);

    // The wizard and its API calls work with the cookie.
    let r = env
        .send(empty(
            from_peer("GET", "/setup", DOCKER).header("cookie", jar.as_str()),
        ))
        .await;
    assert_eq!(r.status, 200);
    assert!(r.text().contains("setup.js"), "the wizard page");
    let r = env
        .send(empty(
            from_peer("GET", "/setup/setup.js", DOCKER).header("cookie", jar.as_str()),
        ))
        .await;
    assert_eq!(r.status, 200);
    let r = env
        .send(empty(
            from_peer("GET", "/setup/api/status", DOCKER).header("cookie", jar.as_str()),
        ))
        .await;
    assert_eq!((r.status, r.json()), (200, json!({"needs_setup": true})));
    // A forged session does not.
    let r = env
        .send(empty(from_peer("GET", "/setup/api/status", DOCKER).header(
            "cookie",
            format!("{SESSION_COOKIE}=forged").as_str(),
        )))
        .await;
    assert_eq!(r.status, 403);

    let r = complete(
        &env,
        from_peer("POST", "/setup/api/complete", DOCKER).header("cookie", jar.as_str()),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(env.db.get_user("admin").unwrap().unwrap().role, Role::Admin);

    // Dead now: the code, the session, the header.
    assert!(!env.deps.setup.has_code());
    let r = complete(
        &env,
        from_peer("POST", "/setup/api/complete", DOCKER).header("cookie", jar.as_str()),
    )
    .await;
    assert_eq!(r.status, 403, "the spent session opens nothing");
    let r = complete(
        &env,
        from_peer("POST", "/setup/api/complete", DOCKER).header("x-setup-code", code.as_str()),
    )
    .await;
    assert_eq!(r.status, 403, "nor the spent code");
    let r = env.send(code_form(DOCKER, &code)).await;
    assert_eq!(
        (r.status, r.header("location"), r.header("set-cookie")),
        (303, Some("/setup"), None),
        "setup is done: the (inert) page, no session"
    );
}

#[tokio::test]
async fn the_code_header_serves_scripts() {
    let env = Env::new();
    let code = env.deps.setup.issue_code();
    let r = complete(
        &env,
        from_peer("POST", "/setup/api/complete", "10.0.0.9")
            .header("x-setup-code", normalize_code(&code).as_str()),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert!(!r.text().contains(&normalize_code(&code)));
}

#[tokio::test]
async fn an_admin_made_elsewhere_ends_the_code() {
    let env = Env::new();
    let code = env.deps.setup.issue_code();
    // `admin add-user` (another process) writes the database directly.
    env.user("root", "password123", Role::Admin);
    let r = env.send(code_form(DOCKER, &code)).await;
    assert_eq!((r.status, r.header("set-cookie")), (303, None));
    assert!(!env.deps.setup.has_code());
}

#[tokio::test]
async fn https_sessions_are_secure_cookies() {
    let env = Env::with_config(|c| c.ssl.enabled = true);
    let code = env.deps.setup.issue_code();
    let r = env.send(code_form(DOCKER, &code)).await;
    assert!(r.header("set-cookie").unwrap().contains("; Secure"));
}

#[test]
fn the_betas_token_file_is_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bunko_core::StorageLayout::new(tmp.path());
    std::fs::write(tmp.path().join(LEGACY_TOKEN_FILE), "old").unwrap();
    remove_legacy_token(&layout);
    assert!(!tmp.path().join(LEGACY_TOKEN_FILE).exists());
    remove_legacy_token(&layout); // absent: fine
}

#[test]
fn the_banner_names_the_address_and_the_code_once() {
    let b = bunko_server::app::setup_banner("0.0.0.0", 8080, false, "ABCDE-12345", true, false);
    let text = b.join("\n");
    assert_eq!(text.matches("ABCDE-12345").count(), 1, "{text}");
    assert!(text.contains(
        "First run: create the admin account at http://<this server's address>:8080/setup (setup code: ABCDE-12345)"
    ));
    assert!(
        text.contains("host port mapped to the container's port 8080"),
        "{text}"
    );
    let b = bunko_server::app::setup_banner("192.168.1.5", 8443, true, "X", false, false);
    assert!(b[1].contains("https://192.168.1.5:8443/setup"), "{b:?}");
    assert!(!b.join("\n").contains("Docker"));
    let b = bunko_server::app::setup_banner("127.0.0.1", 8080, false, "X", false, false);
    assert!(b.join("\n").contains("this computer only"));
    let b = bunko_server::app::setup_banner("0.0.0.0", 8081, false, "X", true, true);
    assert!(b[1].contains("<web UI port>"), "behind nginx: {b:?}");
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

/// Regression (review finding, CSRF): the setup completion takes JSON only.
#[tokio::test]
async fn complete_needs_a_json_body() {
    let env = Env::new();
    let body = complete_body().to_string();
    let r = env
        .send(
            req("POST", "/setup/api/complete")
                .header("content-type", "text/plain")
                .header("content-length", body.len().to_string())
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await;
    assert_eq!(
        (r.status, r.json()),
        (
            415,
            json!({"error": "Content-Type must be application/json"})
        )
    );
    assert!(env.db.get_user("admin").unwrap().is_none());
}
