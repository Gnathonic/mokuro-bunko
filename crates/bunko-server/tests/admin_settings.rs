//! `GET /api/settings` and the settings editors: live config, saved config, round trips;
//! `GET /api/status`; DynDNS/tunnel wrappers.

mod admin_support;

use admin_support::Harness;
use serde_json::json;

#[tokio::test]
async fn settings_get_masks_the_token_and_reports_no_ocr() {
    let h = Harness::new();
    let admin = h.admin();
    let r = h
        .call("GET", "/_admin/api/settings", Some(&admin), None)
        .await;
    assert_eq!(r.status, 200);
    let s = r.json();
    assert_eq!(s["dyndns"]["token"], "****");
    assert_eq!(s["registration"]["mode"], "self");
    assert_eq!(s["ocr_runtime"]["available"], false);
    assert_eq!(s["ocr_runtime"]["local_processing"], false);
    assert_eq!(s["ocr"]["poll_interval"], 30);
    assert_eq!(s["update"]["check"], true);
    assert!(s["config_warnings"].is_array());
}

#[tokio::test]
async fn registration_cors_catalog_queue_round_trip() {
    let h = Harness::new();
    let admin = h.admin();
    let r = h
        .call("PUT", "/_admin/api/settings/registration", Some(&admin), Some(json!({"mode": "invite", "default_role": "uploader", "allow_anonymous_browse": 0, "allow_anonymous_download": "yes"})))
        .await;
    assert_eq!(
        r.json(),
        json!({"success": true, "registration": {"mode": "invite", "default_role": "uploader", "allow_anonymous_browse": false, "allow_anonymous_download": true, "require_login": false}})
    );
    assert_eq!(
        h.core.config.read().registration.mode,
        "invite",
        "applied live"
    );
    assert_eq!(
        h.saved_config().registration.default_role,
        "uploader",
        "saved"
    );
    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/registration",
            Some(&admin),
            Some(json!({"require_login": true})),
        )
        .await;
    assert_eq!(r.json()["registration"]["require_login"], true);
    assert!(!h.core.anonymous_access().download);
    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/registration",
            Some(&admin),
            Some(json!({"mode": "open"})),
        )
        .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (
            400,
            json!({"error": "Invalid mode. Must be one of: ['disabled', 'self', 'invite', 'approval']"})
        )
    );
    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/registration",
            Some(&admin),
            Some(json!({"default_role": "writer"})),
        )
        .await;
    assert_eq!(
        r.json(),
        json!({"error": "Invalid default_role. Must be one of: ['registered', 'uploader', 'inviter', 'editor']"})
    );

    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/cors",
            Some(&admin),
            Some(json!({"allowed_origins": "https://a"})),
        )
        .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (400, json!({"error": "allowed_origins must be a list"}))
    );
    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/cors",
            Some(&admin),
            Some(json!({"enabled": true, "allowed_origins": ["https://a.example"]})),
        )
        .await;
    assert_eq!(
        r.json(),
        json!({"success": true, "cors": {"enabled": true, "allowed_origins": ["https://a.example"]}})
    );
    assert!(
        h.core
            .config
            .read()
            .cors
            .is_origin_allowed("https://a.example")
    );

    let r = h.call("PUT", "/_admin/api/settings/catalog", Some(&admin), Some(json!({"enabled": true, "reader_url": "  https://r.example/// ", "use_as_homepage": true}))).await;
    assert_eq!(
        r.json(),
        json!({"success": true, "catalog": {"enabled": true, "reader_url": "https://r.example", "use_as_homepage": true}})
    );
    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/catalog",
            Some(&admin),
            Some(json!({"reader_url": "  "})),
        )
        .await;
    assert_eq!(
        r.json()["catalog"]["reader_url"],
        "https://r.example",
        "blank is ignored"
    );

    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/queue",
            Some(&admin),
            Some(json!({"display": "loud"})),
        )
        .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (
            400,
            json!({"error": "display must be one of: minimal, normal, detailed", "field": "display"})
        )
    );
    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/queue",
            Some(&admin),
            Some(json!({"display": "detailed", "show_in_nav": true, "public_access": false})),
        )
        .await;
    assert_eq!(
        r.json(),
        json!({"success": true, "queue": {"show_in_nav": true, "public_access": false, "display": "detailed"}})
    );

    // GET reflects every change, and so does the file.
    let s = h
        .call("GET", "/_admin/api/settings", Some(&admin), None)
        .await
        .json();
    assert_eq!(s["registration"]["mode"], "invite");
    assert_eq!(s["cors"]["allowed_origins"], json!(["https://a.example"]));
    assert_eq!(s["catalog"]["reader_url"], "https://r.example");
    assert_eq!(s["queue"]["display"], "detailed");
    let saved = h.saved_config();
    assert_eq!(saved.queue.display, "detailed");
    assert!(saved.catalog.use_as_homepage);
}

#[tokio::test]
async fn ocr_settings_refuse_moved_keys_and_save_the_interval() {
    let h = Harness::new();
    let admin = h.admin();
    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/ocr",
            Some(&admin),
            Some(json!({"backend": "cuda"})),
        )
        .await;
    assert_eq!(
        r.json(),
        json!({"error": "OCR backend is launch-only. Use CLI flags/config file to change it."})
    );
    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/ocr",
            Some(&admin),
            Some(json!({"patch_budget": 1, "engines": []})),
        )
        .await;
    assert_eq!(
        r.json(),
        json!({"error": "engines, patch_budget moved into the generations list; PUT them to /api/ocr/generations"})
    );
    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/ocr",
            Some(&admin),
            Some(json!({"char_map": true})),
        )
        .await;
    assert!(
        r.json()["error"]
            .as_str()
            .unwrap()
            .starts_with("char_map was removed")
    );
    for bad in [json!(0), json!("x"), json!(null)] {
        let r = h
            .call(
                "PUT",
                "/_admin/api/settings/ocr",
                Some(&admin),
                Some(json!({"poll_interval": bad})),
            )
            .await;
        assert_eq!(
            r.json(),
            json!({"error": "poll_interval must be a positive integer"})
        );
    }
    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/ocr",
            Some(&admin),
            Some(json!({"poll_interval": "45"})),
        )
        .await;
    let body = r.json();
    assert_eq!(body["success"], true);
    assert_eq!(
        body["ocr"],
        json!({"backend": "auto", "poll_interval": 45, "concurrency": 1})
    );
    assert_eq!(body["restart_required"], false);
    assert_eq!(body["applied"], false);
    assert!(body["reason"].is_string());
    assert_eq!(body["ocr_runtime"]["available"], false);
    assert_eq!(h.saved_config().ocr.poll_interval, 45);
}

#[tokio::test]
async fn dyndns_settings_keep_the_masked_token_and_reconfigure_the_service() {
    let h = Harness::new();
    let admin = h.admin();
    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/dyndns",
            Some(&admin),
            Some(json!({"provider": "noip"})),
        )
        .await;
    assert_eq!(r.json(), json!({"error": "Invalid provider"}));
    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/dyndns",
            Some(&admin),
            Some(json!({"interval": 10})),
        )
        .await;
    assert_eq!(r.json(), json!({"error": "interval must be at least 30"}));
    let r = h
        .call("PUT", "/_admin/api/settings/dyndns", Some(&admin), Some(json!({"provider": "generic", "token": "****", "domain": "me.example", "update_url": "https://u/{ip}", "interval": "60"})))
        .await;
    assert_eq!(
        r.json(),
        json!({"success": true, "dyndns": {"enabled": false, "provider": "generic", "domain": "me.example", "update_url": "https://u/{ip}", "interval": 60, "token": "****"}})
    );
    assert_eq!(
        h.saved_config().dyndns.token,
        "secret-token",
        "the mask never overwrites the token"
    );
    let st = h
        .call("GET", "/_admin/api/dyndns/status", Some(&admin), None)
        .await
        .json();
    assert_eq!(st["provider"], "generic");
    assert_eq!(st["domain"], "me.example");
    assert_eq!(st["running"], false);
    let r = h
        .call(
            "PUT",
            "/_admin/api/settings/dyndns",
            Some(&admin),
            Some(json!({"token": ""})),
        )
        .await;
    assert_eq!(r.json()["dyndns"]["token"], "");
}

#[tokio::test]
async fn status_and_absent_services() {
    let h = Harness::new();
    let admin = h.admin();
    h.login("gone", bunko_core::Role::Registered);
    h.db.delete_user("gone").unwrap();
    let lib = h.core.config.read().storage.library_path();
    std::fs::create_dir_all(lib.join("Series A")).unwrap();
    std::fs::create_dir_all(lib.join("Series B/inner")).unwrap();
    std::fs::write(lib.join("loose.cbz"), b"x").unwrap();
    let r = h
        .call("GET", "/_admin/api/status", Some(&admin), None)
        .await;
    let s = r.json();
    let keys: Vec<&String> = s.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "uptime",
            "host",
            "port",
            "storage_path",
            "disk_total",
            "disk_used",
            "disk_free",
            "user_count",
            "volume_count",
            "stats",
            "version"
        ]
    );
    assert_eq!(s["user_count"], 1);
    // Every immediate directory, `thumbnails/` included (0.5.2 counts it too).
    assert_eq!(s["volume_count"], 3);
    assert_eq!(s["port"], 8080);
    assert!(s["disk_total"].as_u64().unwrap() > 0);
    assert!(s["uptime"].as_f64().unwrap() >= 0.0);

    let r = h
        .call("GET", "/_admin/api/tunnel/status", Some(&admin), None)
        .await;
    assert_eq!(
        r.json(),
        json!({"running": false, "url": null, "available": false})
    );
    let r = h
        .call(
            "POST",
            "/_admin/api/tunnel/start",
            Some(&admin),
            Some(json!({})),
        )
        .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (500, json!({"error": "Tunnel service not available"}))
    );
    let r = h
        .call(
            "POST",
            "/_admin/api/dyndns/stop",
            Some(&admin),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.json()["success"], true);
}
