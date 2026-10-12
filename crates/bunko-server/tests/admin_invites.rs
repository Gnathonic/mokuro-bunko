//! Admin invite endpoints (admin or inviter).

mod admin_support;

use admin_support::Harness;
use bunko_core::Role;
use serde_json::json;

#[tokio::test]
async fn inviter_creates_lists_and_deletes_invites() {
    let h = Harness::new();
    let ivy = h.login("ivy", Role::Inviter);
    let r = h
        .call(
            "POST",
            "/_admin/api/invites",
            Some(&ivy),
            Some(json!({"role": "editor", "expires": "1d"})),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let body = r.json();
    assert_eq!(body["success"], true);
    let invite = &body["invite"];
    let code = invite["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 22);
    assert_eq!(invite["role"], "editor");
    assert_eq!(invite["status"], "valid");
    assert_eq!(invite["invited_by"], "ivy");
    assert_eq!(invite["used_by"], serde_json::Value::Null);
    let keys: Vec<&String> = invite.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "code",
            "role",
            "status",
            "created_at",
            "expires_at",
            "used_by",
            "invited_by"
        ]
    );
    let ev = &h.audit()[0];
    assert_eq!(
        (ev.action.as_str(), ev.target_path.as_deref()),
        ("invite_created", Some(code.as_str()))
    );

    // Defaults: registered, 7 days.
    let r = h
        .call("POST", "/_admin/api/invites", Some(&ivy), Some(json!({})))
        .await;
    assert_eq!(r.json()["invite"]["role"], "registered");

    let r = h
        .call(
            "POST",
            "/_admin/api/invites",
            Some(&ivy),
            Some(json!({"role": "writer"})),
        )
        .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (
            400,
            json!({"error": "Invalid role. Must be one of: ['editor', 'inviter', 'registered', 'uploader']"})
        )
    );
    let r = h
        .call(
            "POST",
            "/_admin/api/invites",
            Some(&ivy),
            Some(json!({"role": "admin"})),
        )
        .await;
    assert_eq!(r.status, 400);
    let r = h
        .call(
            "POST",
            "/_admin/api/invites",
            Some(&ivy),
            Some(json!({"expires": "5x"})),
        )
        .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (400, json!({"error": "Unknown duration unit: x"}))
    );
    let r = h
        .call(
            "POST",
            "/_admin/api/invites",
            Some(&ivy),
            Some(json!({"expires": 7})),
        )
        .await;
    assert_eq!(r.status, 400);

    let r = h.call("GET", "/_admin/api/invites", Some(&ivy), None).await;
    let invites = r.json()["invites"].as_array().unwrap().clone();
    assert_eq!(invites.len(), 2);
    assert!(invites.iter().any(|i| i["code"] == code.as_str()));

    let r = h
        .call(
            "DELETE",
            &format!("/_admin/api/invites/{code}"),
            Some(&ivy),
            None,
        )
        .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (200, json!({"success": true, "message": "Invite deleted"}))
    );
    let ev = &h.audit()[0];
    assert_eq!(
        (
            ev.action.as_str(),
            ev.target_type.as_deref(),
            ev.target_path.as_deref()
        ),
        ("invite_deleted", Some("invite"), Some(code.as_str()))
    );
    assert_eq!(ev.actor_username.as_deref(), Some("ivy"));
    let r = h
        .call(
            "DELETE",
            &format!("/_admin/api/invites/{code}"),
            Some(&ivy),
            None,
        )
        .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (404, json!({"error": "Invite not found"}))
    );
}

#[tokio::test]
async fn used_invites_are_listed_with_their_status() {
    let h = Harness::new();
    let admin = h.admin();
    let code = h.db.create_invite(Role::Uploader, "1h", None).unwrap();
    h.db.create_user(
        "joiner",
        "password123",
        Role::Uploader,
        bunko_db::UserStatus::Active,
        "",
    )
    .unwrap();
    assert!(h.db.use_invite(&code, "joiner").unwrap());
    let r = h
        .call("GET", "/_admin/api/invites", Some(&admin), None)
        .await;
    let inv = &r.json()["invites"][0];
    assert_eq!(inv["status"], "used");
    assert_eq!(inv["used_by"], "joiner");
    assert_eq!(inv["invited_by"], serde_json::Value::Null);
}
