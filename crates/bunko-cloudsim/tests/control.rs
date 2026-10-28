//! The control API over HTTP, as the reader and the rig drive it: seeding and listing,
//! faults, knobs, the request log, reset, and CORS.

use base64::Engine;
use serde_json::{Value, json};

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn post(base: &str, path: &str, body: Value) -> (u16, Value) {
    let r = http()
        .post(format!("{base}/_sim/{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

async fn get(base: &str, path: &str) -> Value {
    http()
        .get(format!("{base}/_sim/{path}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

async fn token(base: &str, provider: &str, account: &str) -> String {
    let (s, v) = post(
        base,
        "token",
        json!({"provider": provider, "account": account}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    v["access_token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn health_names_the_providers() {
    let sim = bunko_cloudsim::start().await;
    assert_eq!(
        get(&sim.base, "health").await,
        json!({"ok": true, "providers": ["google", "microsoft", "mega"]})
    );
}

#[tokio::test]
async fn seeded_files_are_listed_on_every_provider() {
    let sim = bunko_cloudsim::start().await;
    let files = json!([
        {"path": "mokuro-reader/Series A/Vol 1.cbz", "content_base64": b64(b"cbz bytes")},
        {"path": "mokuro-reader/Series A/Vol 1.mokuro", "content_base64": b64(b"{}")},
    ]);
    for p in ["google", "microsoft", "mega"] {
        let (s, v) = post(
            &sim.base,
            "seed",
            json!({"provider": p, "account": "alice", "files": files}),
        )
        .await;
        assert_eq!((s, v), (200, json!({"seeded": 2})), "{p}");
        let listed = get(&sim.base, &format!("files?provider={p}&account=alice")).await;
        assert_eq!(
            listed,
            json!([
                {"path": "mokuro-reader/Series A/Vol 1.cbz", "size": 9,
                 "sha256": "0dc2f81a560197a880619a8d3d023ee545836571d26115356beacc0627db95bc"},
                {"path": "mokuro-reader/Series A/Vol 1.mokuro", "size": 2,
                 "sha256": "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"},
            ]),
            "{p}"
        );
        // Another account sees nothing.
        let other = get(&sim.base, &format!("files?provider={p}&account=bob")).await;
        assert_eq!(other, json!([]), "{p}");
    }
    // Seeding the same path again replaces the file.
    post(
        &sim.base,
        "seed",
        json!({"provider": "google", "account": "alice",
               "files": [{"path": "mokuro-reader/Series A/Vol 1.mokuro", "content_base64": b64(b"{\"v\":2}")}]}),
    )
    .await;
    let listed = get(&sim.base, "files?provider=google&account=alice").await;
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert_eq!(listed[1]["size"], 7);
}

#[tokio::test]
async fn a_fault_fires_its_times_then_stops() {
    let sim = bunko_cloudsim::start().await;
    let t = token(&sim.base, "google", "default").await;
    let (s, _) = post(
        &sim.base,
        "fault",
        json!({"provider": "google", "method": "GET", "path_has": "/drive/v3/about",
               "status": 429, "reason": "rateLimitExceeded", "retry_after": 1, "times": 2}),
    )
    .await;
    assert_eq!(s, 200);
    let about = || {
        http()
            .get(format!("{}/drive/v3/about", sim.drive_api_base()))
            .bearer_auth(&t)
            .send()
    };
    for _ in 0..2 {
        let r = about().await.unwrap();
        assert_eq!(r.status(), 429);
        assert_eq!(r.headers()["retry-after"], "1");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["error"]["errors"][0]["reason"], "rateLimitExceeded");
    }
    assert_eq!(about().await.unwrap().status(), 200);

    // OneDrive the same way.
    let mt = token(&sim.base, "microsoft", "default").await;
    post(
        &sim.base,
        "fault",
        json!({"provider": "microsoft", "path_has": "/me/drive", "status": 503,
               "reason": "serviceNotAvailable"}),
    )
    .await;
    let r = || {
        http()
            .get(format!("{}/me/drive", sim.graph_api_base()))
            .bearer_auth(&mt)
            .send()
    };
    assert_eq!(r().await.unwrap().status(), 503);
    assert_eq!(r().await.unwrap().status(), 200);
}

#[tokio::test]
async fn knobs_change_behaviour_and_unknown_ones_are_refused() {
    let sim = bunko_cloudsim::start().await;
    let t = token(&sim.base, "google", "default").await;
    for n in 0..3 {
        sim.drive("default").put("root", &format!("f{n}"), b"x");
    }
    let (s, _) = post(
        &sim.base,
        "knobs",
        json!({"provider": "google", "page_size": 2, "sha256": false}),
    )
    .await;
    assert_eq!(s, 200);
    let v: Value = http()
        .get(format!("{}/drive/v3/files", sim.drive_api_base()))
        .bearer_auth(&t)
        .query(&[("q", "'me' in owners")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["files"].as_array().unwrap().len(), 2);
    assert!(v["nextPageToken"].is_string());
    assert!(v["files"][0].get("sha256Checksum").is_none());
    let (s, v) = post(
        &sim.base,
        "knobs",
        json!({"provider": "google", "no_such_knob": 1}),
    )
    .await;
    assert_eq!(s, 400);
    assert!(v["error"].as_str().unwrap().contains("no_such_knob"));
    let (s, _) = post(
        &sim.base,
        "knobs",
        json!({"provider": "mega", "account": "m", "eagain": 2, "overquota": true}),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = post(
        &sim.base,
        "knobs",
        json!({"provider": "microsoft", "ignore_range": true, "interrupt_fragment": null}),
    )
    .await;
    assert_eq!(s, 200);
    assert!(sim.onedrive("default").lock().ignore_range);
}

#[tokio::test]
async fn the_log_records_classes_units_and_bytes() {
    let sim = bunko_cloudsim::start().await;
    let t = token(&sim.base, "google", "default").await;
    let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
    let id = sim.drive("default").put("root", "a.cbz", &data);
    let api = sim.drive_api_base();
    let r = http()
        .get(format!("{api}/drive/v3/files/{id}"))
        .query(&[("alt", "media")])
        .bearer_auth(&t)
        .send()
        .await
        .unwrap();
    assert_eq!(r.bytes().await.unwrap().len(), 1000);
    let body = format!(
        "--b\r\nContent-Type: application/json\r\n\r\n{}\r\n--b\r\nContent-Type: application/octet-stream\r\n\r\n{}\r\n--b--",
        json!({"name": "up.bin", "parents": ["root"]}),
        "u".repeat(500)
    );
    let up_len = body.len() as u64;
    let r = http()
        .post(format!("{api}/upload/drive/v3/files"))
        .query(&[("uploadType", "multipart")])
        .bearer_auth(&t)
        .header("Content-Type", "multipart/related; boundary=b")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = http()
        .get(format!("{api}/drive/v3/about"))
        .bearer_auth("wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    let log = get(&sim.base, "log").await;
    let log = log.as_array().unwrap();
    assert_eq!(log.len(), 3, "{log:?}");
    assert_eq!(log[0]["seq"], 1);
    assert_eq!(log[0]["provider"], "google");
    assert_eq!(log[0]["account"], "default");
    assert_eq!(log[0]["class"], "download");
    assert_eq!(log[0]["method"], "GET");
    assert_eq!(log[0]["path"], format!("/google/drive/v3/files/{id}"));
    assert_eq!(log[0]["bytes_out"], 1000);
    assert_eq!(log[0]["units"], 1);
    assert_eq!(log[0]["status"], 200);
    assert_eq!(log[1]["class"], "upload");
    assert_eq!(log[1]["bytes_in"], up_len);
    assert_eq!(log[2]["status"], 401);
    assert_eq!(log[2]["account"], "");
    assert_eq!(log[2]["units"], 1, "a refused request still costs a unit");
    assert_eq!(
        sim.files(bunko_cloudsim::Provider::Google, "default").len(),
        2
    );

    let r = http()
        .delete(format!("{}/_sim/log", sim.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert_eq!(get(&sim.base, "log").await, json!([]));
}

#[tokio::test]
async fn reset_empties_everything() {
    let sim = bunko_cloudsim::start().await;
    let t = token(&sim.base, "google", "alice").await;
    post(
        &sim.base,
        "seed",
        json!({"provider": "google", "account": "alice",
               "files": [{"path": "a/b.cbz", "content_base64": b64(b"b")}]}),
    )
    .await;
    // A violation, to see it cleared.
    http()
        .get(format!("{}/drive/v3/files", sim.drive_api_base()))
        .bearer_auth(&t)
        .query(&[("q", "fullText contains 'x'")])
        .send()
        .await
        .unwrap();
    assert_eq!(
        get(&sim.base, "violations").await.as_array().unwrap().len(),
        1
    );
    assert!(!get(&sim.base, "log").await.as_array().unwrap().is_empty());
    let (s, _) = post(&sim.base, "reset", json!({})).await;
    assert_eq!(s, 204);
    assert_eq!(
        get(&sim.base, "files?provider=google&account=alice").await,
        json!([])
    );
    assert_eq!(get(&sim.base, "log").await, json!([]));
    assert_eq!(get(&sim.base, "violations").await, json!([]));
    let r = http()
        .get(format!("{}/drive/v3/about", sim.drive_api_base()))
        .bearer_auth(&t)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "tokens are gone too");
    // The 401 was logged after the reset.
    assert_eq!(get(&sim.base, "log").await.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn cors_on_preflights_and_answers() {
    let sim = bunko_cloudsim::start().await;
    let r = http()
        .request(
            reqwest::Method::OPTIONS,
            format!("{}/drive/v3/files", sim.drive_api_base()),
        )
        .header("Origin", "http://localhost:5173")
        .header("Access-Control-Request-Method", "PATCH")
        .header(
            "Access-Control-Request-Headers",
            "authorization,content-type",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let h = r.headers();
    assert_eq!(h["access-control-allow-origin"], "http://localhost:5173");
    assert_eq!(h["access-control-allow-methods"], "PATCH");
    assert_eq!(
        h["access-control-allow-headers"],
        "authorization,content-type"
    );

    let t = token(&sim.base, "google", "default").await;
    let r = http()
        .get(format!("{}/drive/v3/about", sim.drive_api_base()))
        .header("Origin", "http://localhost:5173")
        .bearer_auth(&t)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["access-control-allow-origin"],
        "http://localhost:5173"
    );
    let expose = r.headers()["access-control-expose-headers"]
        .to_str()
        .unwrap();
    for h in ["Location", "Content-Range", "Retry-After", "X-Hashcash"] {
        assert!(expose.contains(h), "{expose}");
    }
    // No Origin: `*`, on the control API and MEGA too.
    let r = http()
        .get(format!("{}/_sim/health", sim.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["access-control-allow-origin"], "*");
    let r = http()
        .post(format!("{}cs?id=1", sim.mega_origin()))
        .body("[{\"a\":\"us0\",\"user\":\"nobody@example.com\"}]")
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["access-control-allow-origin"], "*");
}

#[tokio::test]
async fn a_mega_account_logs_in_by_password_and_is_logged() {
    let sim = bunko_cloudsim::start().await;
    let (s, _) = post(
        &sim.base,
        "mega/account",
        json!({"account": "m", "email": "M@cloudsim.test", "password": "cloudsim"}),
    )
    .await;
    assert_eq!(s, 200);
    // The prelogin answers the account's salt (v2) for its email, any case.
    let v: Value = http()
        .post(format!("{}cs?id=1", sim.mega_origin()))
        .body("[{\"a\":\"us0\",\"user\":\"m@cloudsim.test\"}]")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v[0]["v"], 2, "{v}");
    let log = get(&sim.base, "log").await;
    assert_eq!(log[0]["provider"], "mega");
    assert_eq!(log[0]["account"], "m");
    assert_eq!(log[0]["class"], "auth");
    assert_eq!(log[0]["units"], 0);
    let (s, v) = post(&sim.base, "token", json!({"provider": "mega"})).await;
    assert_eq!(s, 400, "{v}");
}
