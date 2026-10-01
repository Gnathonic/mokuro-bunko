//! Account page API (spec db-auth-admin §17; 0.5.2 had no direct tests for it).

mod accounts_support;

use accounts_support::*;
use bunko_core::Role;
use bunko_db::UserStatus;
use parking_lot::Mutex;
use serde_json::json;
use std::sync::Arc;

const PW: &str = "carol-password-1";

fn env() -> Env {
    let env = Env::new();
    env.user("carol", PW, Role::Uploader);
    env
}

#[tokio::test]
async fn stats_needs_a_login_and_reports_zeros() {
    let env = env();
    let r = env.send(empty(req("GET", "/api/account/stats"))).await;
    assert_eq!(
        (r.status, r.json()),
        (401, json!({"error": "Authentication required"}))
    );
    assert!(r.header("www-authenticate").is_none());
    let r = env
        .send(empty(
            req("GET", "/api/account/stats").header("authorization", "Bearer dead"),
        ))
        .await;
    assert_eq!(r.status, 401);
    let t = token(&env, "carol", PW).await;
    for auth in [bearer(&t), basic("carol", PW)] {
        let r = env
            .send(empty(
                req("GET", "/api/account/stats").header("authorization", auth),
            ))
            .await;
        assert_eq!(r.status, 200);
        assert_eq!(
            r.text(),
            r#"{"volumes":0,"pages_read":0,"characters_read":0,"reading_time_seconds":0,"reading_time_formatted":"0s"}"#
        );
    }
}

async fn change(env: &Env, auth: &str, body: serde_json::Value) -> Resp {
    env.send(json_body(
        req("POST", "/api/account/password").header("authorization", auth),
        body,
    ))
    .await
}

#[tokio::test]
async fn password_change_revokes_every_token() {
    let env = env();
    let dropped: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let mut deps = env.deps.clone();
    let sink = dropped.clone();
    deps.hooks.drop_processor_account = Some(Arc::new(move |u: &str, why: &str| {
        sink.lock().push((u.into(), why.into()))
    }));
    let app = app(deps);

    let t1 = token(&env, "carol", PW).await;
    let t2 = token(&env, "carol", PW).await;
    let body = json!({"current_password": PW, "new_password": "brand-new-pass"});
    let r = send(
        app.clone(),
        json_body(
            req("POST", "/api/account/password").header("authorization", bearer(&t1)),
            body,
        ),
    )
    .await;
    assert_eq!((r.status, r.json()), (200, json!({"success": true})));
    // Both tokens (the caller's included) are dead; the new password signs in.
    for t in [&t1, &t2] {
        let r = env
            .send(empty(
                req("GET", "/login/api/me").header("authorization", bearer(t)),
            ))
            .await;
        assert_eq!(r.status, 401);
    }
    assert!(env.db.authenticate_user("carol", PW).unwrap().is_none());
    token(&env, "carol", "brand-new-pass").await;
    assert_eq!(
        dropped.lock().as_slice(),
        &[("carol".to_string(), "password changed".to_string())]
    );
}

#[tokio::test]
async fn password_change_errors() {
    let env = env();
    let auth = basic("carol", PW);
    let r = env
        .send(json_body(
            req("POST", "/api/account/password"),
            json!({"current_password": PW, "new_password": "x"}),
        ))
        .await;
    assert_eq!(
        (r.status, r.json()),
        (401, json!({"error": "Authentication required"}))
    );
    let r = env
        .send(empty(
            req("POST", "/api/account/password").header("authorization", auth.as_str()),
        ))
        .await;
    assert_eq!(
        (r.status, r.json()),
        (400, json!({"error": "Missing request body"}))
    );
    let r = env
        .send(raw(
            req("POST", "/api/account/password").header("authorization", auth.as_str()),
            b"{bad",
        ))
        .await;
    assert_eq!(
        (r.status, r.json()),
        (400, json!({"error": "Invalid request"}))
    );
    let r = change(&env, &auth, json!({"current_password": PW})).await;
    assert_eq!(
        (r.status, r.json()),
        (400, json!({"error": "Missing required fields"}))
    );
    let r = change(
        &env,
        &auth,
        json!({"current_password": "not-it-at-all", "new_password": "brand-new-pass"}),
    )
    .await;
    assert_eq!(
        (r.status, r.json()),
        (401, json!({"error": "Current password is incorrect"}))
    );
    let r = change(
        &env,
        &auth,
        json!({"current_password": PW, "new_password": "short"}),
    )
    .await;
    assert_eq!(
        (r.status, r.json()),
        (
            400,
            json!({"error": "Password must be at least 8 characters"})
        )
    );
    let r = change(
        &env,
        &auth,
        json!({"current_password": PW, "new_password": "x".repeat(129)}),
    )
    .await;
    assert_eq!(
        (r.status, r.json()),
        (
            400,
            json!({"error": "Password must be at most 128 characters"})
        )
    );
    assert!(
        env.db.authenticate_user("carol", PW).unwrap().is_some(),
        "nothing changed"
    );
}

#[tokio::test]
async fn wrong_current_passwords_are_rate_limited() {
    let env = env();
    let t = token(&env, "carol", PW).await;
    let mut last = 0;
    for n in 0..11 {
        let body =
            json!({"current_password": format!("guess-{n}-xx"), "new_password": "brand-new-pass"});
        last = change(&env, &bearer(&t), body).await.status;
    }
    assert_eq!(last, 429);
}

async fn delete(env: &Env, auth: &str, body: serde_json::Value) -> Resp {
    env.send(json_body(
        req("POST", "/api/account/delete").header("authorization", auth),
        body,
    ))
    .await
}

#[tokio::test]
async fn self_delete_removes_the_account_and_its_progress() {
    let env = env();
    let progress = env.deps.core.layout.users().join("carol");
    std::fs::create_dir_all(&progress).unwrap();
    std::fs::write(progress.join("volume-data.json"), b"{}").unwrap();
    let t = token(&env, "carol", PW).await;

    let r = delete(&env, &bearer(&t), json!({})).await;
    assert_eq!(
        (r.status, r.json()),
        (400, json!({"error": "Password confirmation required"}))
    );
    let r = delete(&env, &bearer(&t), json!({"password": "not-the-one"})).await;
    assert_eq!(
        (r.status, r.json()),
        (401, json!({"error": "Password is incorrect"}))
    );
    assert!(progress.exists());

    let r = delete(&env, &bearer(&t), json!({"password": PW})).await;
    assert_eq!((r.status, r.json()), (200, json!({"success": true})));
    assert!(!progress.exists());
    let user = env.db.get_user("carol").unwrap().unwrap();
    assert_eq!(user.status, UserStatus::Deleted);
    let r = env
        .send(empty(
            req("GET", "/login/api/me").header("authorization", bearer(&t)),
        ))
        .await;
    assert_eq!(r.status, 401, "tokens are wiped");
    let events = env.db.list_audit_events(10, Some("carol")).unwrap();
    let ev = events
        .iter()
        .find(|e| e.action == "self_delete_account")
        .expect("audited");
    assert_eq!(ev.target_type.as_deref(), Some("user"));
    assert_eq!(ev.target_username.as_deref(), Some("carol"));
}

#[tokio::test]
async fn options_and_pages() {
    let env = env();
    for path in [
        "/api/account/stats",
        "/api/account/password",
        "/api/account/delete",
        "/api/account/whatever",
    ] {
        let r = env.send(empty(req("OPTIONS", path))).await;
        assert_eq!(r.status, 204, "{path}");
        assert_eq!(r.header("allow"), Some("GET, POST, OPTIONS"));
    }
    for path in ["/account", "/account/"] {
        let r = env.send(empty(req("GET", path))).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.header("cache-control"), Some("no-cache"));
        assert!(r.text().contains("account.js"));
    }
    let r = env.send(empty(req("GET", "/account/styles.css"))).await;
    assert_eq!(r.header("content-type"), Some("text/css; charset=utf-8"));
    let r = env.send(empty(req("GET", "/account/nope.js"))).await;
    assert_eq!((r.status, r.text().as_str()), (404, "Not found"));
}

/// Regression (review finding, CSRF): a body that is not declared JSON (what a cross-site
/// form can send) is refused before it is read.
#[tokio::test]
async fn account_calls_need_a_json_body() {
    let env = env();
    let auth = basic("carol", PW);
    for ct in ["text/plain", "application/x-www-form-urlencoded"] {
        let body = json!({"current_password": PW, "new_password": "brand-new-pass-1"}).to_string();
        let r = env
            .send(
                req("POST", "/api/account/password")
                    .header("authorization", auth.as_str())
                    .header("content-type", ct)
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
            ),
            "{ct}"
        );
    }
    assert!(
        env.db.authenticate_user("carol", PW).unwrap().is_some(),
        "the password did not change"
    );
}
