//! `GET /_admin/api/audit`: cursor paging, totals and facets on the first page only,
//! filters, search and argument errors.

mod admin_support;

use admin_support::Harness;
use bunko_db::{AuditDetails, NewAuditEvent};
use serde_json::{Value, json};
use std::collections::HashSet;

fn seed(h: &Harness) {
    for i in 0..120 {
        let path = format!("/mokuro-reader/S/V{i}.cbz");
        h.db.log_audit_event(
            &NewAuditEvent::new("upload")
                .actor(Some(if i % 2 == 0 { "alice" } else { "bob" }))
                .target_type("library")
                .target_path(&path),
        )
        .unwrap();
    }
    for _ in 0..30 {
        h.db.log_audit_event(
            &NewAuditEvent::new("edit")
                .actor(Some("alice"))
                .target_type("progress")
                .target_path("/mokuro-reader/volume-data.json"),
        )
        .unwrap();
    }
    h.db.log_audit_event(
        &NewAuditEvent::new("ocr_sidecar_written")
            .target_type("sidecar")
            .target_path("/x.mokuro")
            .details(AuditDetails::new().with("engine", "日本")),
    )
    .unwrap();
}

#[tokio::test]
async fn pages_follow_the_cursor_to_the_end() {
    let h = Harness::new();
    let admin = h.admin();
    seed(&h);
    let first = h.call("GET", "/_admin/api/audit", Some(&admin), None).await;
    assert_eq!(first.status, 200);
    let body = first.json();
    let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["events", "next_cursor", "total", "facets"]);
    // Progress rows are hidden by default: 120 uploads + 1 sidecar.
    assert_eq!(body["total"], 121);
    assert_eq!(body["events"].as_array().unwrap().len(), 50);
    assert_eq!(body["facets"]["actors"], json!(["alice", "bob"]));
    assert_eq!(
        body["facets"]["target_types"],
        json!(["library", "progress", "sidecar"])
    );
    let ev_keys: Vec<&String> = body["events"][0].as_object().unwrap().keys().collect();
    assert_eq!(
        ev_keys,
        [
            "id",
            "actor_username",
            "action",
            "target_type",
            "target_path",
            "target_username",
            "details",
            "created_at"
        ]
    );

    let mut seen: HashSet<i64> = HashSet::new();
    let mut last_id = i64::MAX;
    let mut page = body;
    loop {
        for e in page["events"].as_array().unwrap() {
            let id = e["id"].as_i64().unwrap();
            assert!(id < last_id, "newest first");
            last_id = id;
            assert!(seen.insert(id));
        }
        let Some(cursor) = page["next_cursor"].as_str().map(str::to_string) else {
            break;
        };
        let r = h
            .call(
                "GET",
                &format!("/_admin/api/audit?cursor={cursor}"),
                Some(&admin),
                None,
            )
            .await;
        assert_eq!(r.status, 200);
        page = r.json();
        assert_eq!(page["total"], Value::Null, "total only on a first page");
        assert!(page.get("facets").is_none(), "facets only on a first page");
    }
    assert_eq!(seen.len(), 121);
}

#[tokio::test]
async fn filters_search_and_errors() {
    let h = Harness::new();
    let admin = h.admin();
    seed(&h);
    let get = |q: &'static str| {
        let h = &h;
        let admin = admin.clone();
        async move {
            h.call("GET", &format!("/_admin/api/audit?{q}"), Some(&admin), None)
                .await
        }
    };
    let r = get("actor=bob&limit=500").await.json();
    assert_eq!(r["total"], 60);
    assert_eq!(r["events"].as_array().unwrap().len(), 60);
    let r = get("include_progress=TRUE&limit=1").await.json();
    assert_eq!(r["total"], 151);
    assert_eq!(r["events"].as_array().unwrap().len(), 1);
    let r = get("target_type=progress").await.json();
    assert_eq!(r["total"], 30);
    let r = get("action=edit,+ocr_sidecar_written&include_progress=1")
        .await
        .json();
    assert_eq!(r["total"], 31);
    let r = get("action=edit&action=upload&include_progress=1&limit=0")
        .await
        .json();
    assert_eq!(r["total"], 150);
    assert_eq!(
        r["events"].as_array().unwrap().len(),
        1,
        "limit clamps to 1"
    );
    // Non-ASCII search finds the ASCII-escaped details.
    let r = get("q=%E6%97%A5%E6%9C%AC").await.json();
    assert_eq!(r["total"], 1);
    let r = get("q=V11%25").await.json();
    assert_eq!(r["total"], 0, "wildcards are literal");
    let r = get("q=v119").await.json();
    assert_eq!(r["total"], 1, "case-insensitive");

    let r = get("limit=abc").await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (400, json!({"error": "limit is not a number"}))
    );
    let r = get("cursor=garbage").await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (
            400,
            json!({"error": "cursor is not one this server gave out"})
        )
    );
    let r = get("since=yesterday").await;
    assert_eq!(r.status, 400);
    assert!(r.json()["error"].as_str().unwrap().contains("since"));
    let r = get("since=2000-01-01&until=2100-01-01T00:00:00Z")
        .await
        .json();
    assert_eq!(r["total"], 121);
    // Blank values are dropped (parse_qs).
    let r = get("actor=&limit=").await.json();
    assert_eq!(r["total"], 121);
}
