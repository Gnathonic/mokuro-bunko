//! Admin users endpoints, the role gate, the static SPA and body parsing.

mod admin_support;

use admin_support::Harness;
use bunko_core::Role;
use serde_json::json;

#[tokio::test]
async fn static_spa_is_ungated_with_fallback_and_no_cache() {
    let h = Harness::new();
    for path in ["/_admin", "/_admin/", "/_admin/index.html", "/_admin/users/whatever"] {
        let r = h.call("GET", path, None, None).await;
        assert_eq!(r.status, 200, "{path}");
        assert_eq!(r.headers["content-type"], "text/html; charset=utf-8", "{path}");
        assert_eq!(r.headers["cache-control"], "no-cache");
        assert!(r.text().contains("<html"), "{path}");
    }
    let js = h.call("GET", "/_admin/admin.js", None, None).await;
    assert_eq!(js.status, 200);
    assert_eq!(js.headers["content-type"], "application/javascript; charset=utf-8");
    assert!(js.text().contains("API_BASE"));
    let bad = h.call("GET", "/_admin/..%2f..%2fCargo.toml", None, None).await;
    assert_eq!(bad.status, 403);
    assert_eq!(bad.text(), "Forbidden");
}

#[tokio::test]
async fn role_gate_runs_before_routing() {
    let h = Harness::new();
    let r = h.call("GET", "/_admin/api/users", None, None).await;
    assert_eq!((r.status.as_u16(), r.json()), (403, json!({"error": "Admin access required"})));
    let inviter = h.login("ivy", Role::Inviter);
    let r = h.call("GET", "/_admin/api/users", Some(&inviter), None).await;
    assert_eq!(r.status, 403);
    let r = h.call("GET", "/_admin/api/nonsense", Some(&inviter), None).await;
    assert_eq!(r.json()["error"], "Admin access required");
    let r = h.call("GET", "/_admin/api/invites", Some(&inviter), None).await;
    assert_eq!(r.status, 200);
    let editor = h.login("eddy", Role::Editor);
    let r = h.call("GET", "/_admin/api/invites", Some(&editor), None).await;
    assert_eq!((r.status.as_u16(), r.json()), (403, json!({"error": "Admin or inviter access required"})));

    let admin = h.admin();
    let r = h.call("GET", "/_admin/api/nonsense", Some(&admin), None).await;
    assert_eq!((r.status.as_u16(), r.json()), (404, json!({"error": "API endpoint not found"})));
    // A wrong method on a known path is the same 404.
    let r = h.call("PATCH", "/_admin/api/users", Some(&admin), None).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.headers["content-type"], "application/json");
}

#[tokio::test]
async fn bodies_are_parsed_like_0_5_2() {
    let h = Harness::new();
    let admin = h.admin();
    let r = h.raw("POST", "/_admin/api/users", Some(&admin), b"{nope".to_vec()).await;
    assert_eq!((r.status.as_u16(), r.json()), (400, json!({"error": "Invalid JSON body"})));
    let r = h.raw("POST", "/_admin/api/users", Some(&admin), b"[1,2]".to_vec()).await;
    assert_eq!(r.json(), json!({"error": "Invalid JSON body"}));
    let big = format!("{{\"notes\": \"{}\"}}", "x".repeat(70_000)).into_bytes();
    let r = h.raw("POST", "/_admin/api/users", Some(&admin), big).await;
    assert_eq!((r.status.as_u16(), r.json()), (400, json!({"error": "Request body too large"})));
    // An empty body is `{}`.
    let r = h.raw("POST", "/_admin/api/users", Some(&admin), vec![]).await;
    assert_eq!(r.json(), json!({"error": "Username is required"}));
}

#[tokio::test]
async fn create_list_and_validate_users() {
    let h = Harness::new();
    let admin = h.admin();
    let r = h.call("POST", "/_admin/api/users", Some(&admin), Some(json!({"username": "  alice ", "password": "password123", "role": "uploader", "notes": "hi"}))).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let body = r.json();
    assert_eq!(body["success"], true);
    assert_eq!(body["user"]["username"], "alice");
    assert_eq!(body["user"]["role"], "uploader");
    assert_eq!(body["user"]["status"], "active");
    assert_eq!(body["user"]["notes"], "hi");
    let keys: Vec<&String> = body["user"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["id", "username", "role", "status", "notes", "created_at"]);

    let ev = &h.audit()[0];
    assert_eq!(ev.action, "admin_create_user");
    assert_eq!(ev.actor_username.as_deref(), Some("boss"));
    assert_eq!(ev.target_username.as_deref(), Some("alice"));
    assert_eq!(ev.details.as_deref(), Some(r#"{"role":"uploader"}"#));

    let dup = h.call("POST", "/_admin/api/users", Some(&admin), Some(json!({"username": "alice", "password": "password123"}))).await;
    assert_eq!((dup.status.as_u16(), dup.json()), (409, json!({"error": "Username 'alice' already exists"})));
    let r = h.call("POST", "/_admin/api/users", Some(&admin), Some(json!({"username": "bob"}))).await;
    assert_eq!(r.json(), json!({"error": "Password is required"}));
    let r = h.call("POST", "/_admin/api/users", Some(&admin), Some(json!({"username": "bad name!", "password": "password123"}))).await;
    assert_eq!(r.status, 400);
    let r = h.call("POST", "/_admin/api/users", Some(&admin), Some(json!({"username": "bob", "password": "password123", "role": "wizard"}))).await;
    assert_eq!(
        r.json(),
        json!({"error": "Invalid role: wizard. Must be one of: ['admin', 'anonymous', 'editor', 'inviter', 'processor', 'registered', 'uploader']"})
    );
    // The legacy spelling is accepted on create.
    let r = h.call("POST", "/_admin/api/users", Some(&admin), Some(json!({"username": "wally", "password": "password123", "role": "writer"}))).await;
    assert_eq!(r.json()["user"]["role"], "uploader");

    let r = h.call("GET", "/_admin/api/users", Some(&admin), None).await;
    let names: Vec<String> = r.json()["users"].as_array().unwrap().iter().map(|u| u["username"].as_str().unwrap().to_string()).collect();
    assert!(names.contains(&"alice".to_string()) && names.contains(&"boss".to_string()) && names.contains(&"wally".to_string()));
}

#[tokio::test]
async fn role_notes_approve_disable_delete() {
    let h = Harness::new();
    let admin = h.admin();
    h.login("proc1", Role::Processor);

    let r = h.call("PUT", "/_admin/api/users/proc1/role", Some(&admin), Some(json!({}))).await;
    assert_eq!(r.json(), json!({"error": "Role is required"}));
    let r = h.call("PUT", "/_admin/api/users/proc1/role", Some(&admin), Some(json!({"role": "writer"}))).await;
    assert_eq!(
        r.json(),
        json!({"error": "Invalid role. Must be one of: ['registered', 'uploader', 'inviter', 'editor', 'admin', 'processor']"})
    );
    let r = h.call("PUT", "/_admin/api/users/ghost/role", Some(&admin), Some(json!({"role": "editor"}))).await;
    assert_eq!((r.status.as_u16(), r.json()), (404, json!({"error": "User 'ghost' not found"})));
    let r = h.call("PUT", "/_admin/api/users/proc1/role", Some(&admin), Some(json!({"role": "processor"}))).await;
    assert_eq!(r.status, 200);
    assert!(h.dropped.lock().is_empty(), "a processor kept as processor is not dropped");
    let r = h.call("PUT", "/_admin/api/users/proc1/role", Some(&admin), Some(json!({"role": "editor"}))).await;
    assert_eq!(r.json()["user"]["role"], "editor");
    assert_eq!(h.dropped.lock().last().unwrap(), &("proc1".to_string(), "its account's role was changed to editor".to_string()));
    assert_eq!(h.audit()[0].details.as_deref(), Some(r#"{"role":"editor"}"#));

    let r = h.call("PUT", "/_admin/api/users/proc1/notes", Some(&admin), Some(json!({"notes": 5}))).await;
    assert_eq!(r.json(), json!({"error": "notes must be a string"}));
    let r = h.call("PUT", "/_admin/api/users/proc1/notes", Some(&admin), Some(json!({"notes": "night shift"}))).await;
    assert_eq!(r.json()["user"]["notes"], "night shift");
    assert_eq!(h.audit()[0].action, "admin_update_notes");
    assert_eq!(h.audit()[0].details, None);

    let r = h.call("POST", "/_admin/api/users/proc1/approve", Some(&admin), Some(json!({}))).await;
    assert_eq!((r.status.as_u16(), r.json()), (404, json!({"error": "User 'proc1' not found or not pending"})));
    h.db.create_user("newbie", "password123", Role::Registered, bunko_db::UserStatus::Pending, "").unwrap();
    let r = h.call("POST", "/_admin/api/users/newbie/approve", Some(&admin), None).await;
    assert_eq!(r.json()["user"]["status"], "active");

    let r = h.call("POST", "/_admin/api/users/newbie/disable", Some(&admin), Some(json!({}))).await;
    assert_eq!(r.json()["user"]["status"], "disabled");
    assert_eq!(h.dropped.lock().last().unwrap().1, "its account was disabled");

    // Percent-encoded names are decoded once.
    h.login("dash-name", Role::Registered);
    let r = h.call("DELETE", "/_admin/api/users/dash%2Dname", Some(&admin), None).await;
    assert_eq!((r.status.as_u16(), r.json()), (200, json!({"success": true, "message": "User 'dash-name' deleted"})));
    assert_eq!(h.dropped.lock().last().unwrap(), &("dash-name".to_string(), "its account was deleted".to_string()));
    assert_eq!(h.audit()[0].action, "admin_delete_user");
    let r = h.call("DELETE", "/_admin/api/users/dash-name", Some(&admin), None).await;
    assert_eq!((r.status.as_u16(), r.json()), (404, json!({"error": "User 'dash-name' not found"})));
}
