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
        json!({"ok": true, "providers": ["google", "microsoft", "mega"],
               "preauth": sim.preauth_base})
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

async fn mega_sid(sim: &bunko_cloudsim::CloudSim, email: &str) -> String {
    let cfg = bunko_mega::ClientConfig {
        api_origin: sim.mega_origin(),
        min_cs_interval: std::time::Duration::ZERO,
        ..bunko_mega::ClientConfig::default()
    };
    let client = bunko_mega::Client::login(cfg, email, "pw", None)
        .await
        .unwrap();
    client.session().unwrap().sid().to_string()
}

/// A MEGA session saved before a reset opens nothing after it, not whichever account
/// is created next in the same slot.
#[tokio::test]
async fn a_mega_session_from_before_a_reset_is_refused_after_it() {
    let sim = bunko_cloudsim::start().await;
    sim.mega_account("m", "m@cloudsim.test", "pw");
    let old = mega_sid(&sim, "m@cloudsim.test").await;
    let cs = |sid: String| {
        let url = format!("{}cs?id=1&sid={sid}", sim.mega_origin());
        async move {
            http()
                .post(url)
                .body(r#"[{"a":"uq"}]"#)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    assert!(cs(old.clone()).await[0]["mstrg"].is_number());
    let (s, _) = post(&sim.base, "reset", json!({})).await;
    assert_eq!(s, 204);
    // The same account name again, and another one.
    sim.mega_account("m", "m@cloudsim.test", "pw");
    sim.mega_account("n", "n@cloudsim.test", "pw");
    assert_eq!(cs(old.clone()).await, json!([-15]));
    let new = mega_sid(&sim, "m@cloudsim.test").await;
    assert_ne!(new, old);
    assert_ne!(new, mega_sid(&sim, "n@cloudsim.test").await);
    // A second simulator never hands out the first one's session ids either.
    let other = bunko_cloudsim::start().await;
    other.mega_account("m", "m@cloudsim.test", "pw");
    assert_ne!(mega_sid(&other, "m@cloudsim.test").await, new);
}

// ---- /_sim/node and /_sim/tree --------------------------------------------------------

/// Create a node; `(status, body)`.
async fn node(base: &str, body: Value) -> (u16, Value) {
    post(base, "node", body).await
}

async fn node_id(base: &str, body: Value) -> String {
    let (s, v) = node(base, body.clone()).await;
    assert_eq!(s, 200, "{body} -> {v}");
    v["id"].as_str().unwrap().to_string()
}

fn epoch(rfc3339: &str) -> i64 {
    time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339)
        .unwrap()
        .unix_timestamp()
}

/// The `/_sim/tree` row with id `id`.
fn row<'a>(tree: &'a Value, id: &str) -> &'a Value {
    tree.as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no row {id} in {tree}"))
}

fn ids(tree: &Value) -> Vec<String> {
    tree.as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect()
}

/// `cs` `f` (fetch every node) on a MEGA session.
async fn mega_f(sim: &bunko_cloudsim::CloudSim, sid: &str) -> Vec<Value> {
    let v: Value = http()
        .post(format!("{}cs?id=1&sid={sid}", sim.mega_origin()))
        .body(r#"[{"a":"f","c":1}]"#)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v[0]["f"].as_array().unwrap().clone()
}

const T1: &str = "2026-01-02T03:04:05Z";
const T2: &str = "2026-02-03T04:05:06Z";
const T3: &str = "2026-03-04T05:06:07Z";

/// Two same-named folders under one parent, as a second client's race leaves them: path
/// seeding cannot make them, `/_sim/node` can, and each provider's own listing shows
/// both.
#[tokio::test]
async fn same_named_folders_sit_side_by_side_on_google_and_mega() {
    let sim = bunko_cloudsim::start().await;
    for p in ["google", "mega"] {
        let a = node_id(
            &sim.base,
            json!({"provider": p, "account": "a", "name": "mokuro-reader", "folder": true,
                   "created": T1, "modified": T2}),
        )
        .await;
        let b = node_id(
            &sim.base,
            json!({"provider": p, "account": "a", "name": "mokuro-reader", "folder": true,
                   "modified": T3}),
        )
        .await;
        assert_ne!(a, b, "{p}");
        let file = node_id(
            &sim.base,
            json!({"provider": p, "account": "a", "parent_id": b, "name": "volume-data.json",
                   "content_base64": b64(b"{}")}),
        )
        .await;
        let tree = get(&sim.base, &format!("tree?provider={p}&account=a")).await;
        assert_eq!(tree.as_array().unwrap().len(), 3, "{p}: {tree}");
        let ra = row(&tree, &a);
        assert_eq!(ra["name"], "mokuro-reader", "{p}");
        assert_eq!(ra["folder"], true, "{p}");
        assert_eq!(ra["trashed"], false, "{p}");
        assert_eq!(ra["size"], 0, "{p}");
        assert_eq!(ra["sha256"], Value::Null, "{p}");
        assert_eq!(epoch(ra["modified"].as_str().unwrap()), epoch(T2), "{p}");
        let rf = row(&tree, &file);
        assert_eq!(rf["parent_id"], b.as_str(), "{p}");
        assert_eq!(rf["folder"], false, "{p}");
        assert_eq!(rf["size"], 2, "{p}");
        assert_eq!(
            rf["sha256"], "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
            "{p}"
        );
        assert_eq!(ra["parent_id"], row(&tree, &b)["parent_id"], "{p}");
        // `/_sim/files` names it by path, whichever folder holds it.
        assert_eq!(
            get(&sim.base, &format!("files?provider={p}&account=a")).await[0]["path"],
            "mokuro-reader/volume-data.json",
            "{p}"
        );
    }

    // Drive's own `q` lists both, with the times given.
    let t = token(&sim.base, "google", "a").await;
    let v: Value = http()
        .get(format!("{}/drive/v3/files", sim.drive_api_base()))
        .bearer_auth(&t)
        .query(&[(
            "q",
            "'root' in parents and name = 'mokuro-reader' and trashed = false",
        )])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "{v}");
    let tree = get(&sim.base, "tree?provider=google&account=a").await;
    for f in files {
        let r = row(&tree, f["id"].as_str().unwrap());
        assert_eq!(f["modifiedTime"], r["modified"]);
        assert_eq!(f["createdTime"], r["created"]);
        assert_eq!(f["mimeType"], "application/vnd.google-apps.folder");
    }
    let first = files.iter().find(|f| created_at(&tree, f, T1)).unwrap();
    assert_eq!(epoch(first["createdTime"].as_str().unwrap()), epoch(T1));
    assert_eq!(epoch(first["modifiedTime"].as_str().unwrap()), epoch(T2));

    // MEGA's `f` lists both under the cloud root, `ts` as given.
    sim.mega_account("a", "a@cloudsim.test", "pw");
    let sid = mega_sid(&sim, "a@cloudsim.test").await;
    let tree = get(&sim.base, "tree?provider=mega&account=a").await;
    let folders: Vec<&Value> = tree
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["folder"] == true)
        .collect();
    let nodes = mega_f(&sim, &sid).await;
    for r in &folders {
        let n = nodes
            .iter()
            .find(|n| n["h"] == r["id"])
            .expect("listed by f");
        assert_eq!(n["p"], bunko_cloudsim::mega::ROOT);
        assert_eq!(n["t"], 1);
        assert_eq!(n["ts"], epoch(r["modified"].as_str().unwrap()));
        assert_eq!(
            sim.mega("a").name_of(n["h"].as_str().unwrap()).unwrap().0,
            "mokuro-reader"
        );
    }
    let mut ts: Vec<i64> = folders
        .iter()
        .map(|r| epoch(r["modified"].as_str().unwrap()))
        .collect();
    ts.sort();
    assert_eq!(ts, [epoch(T2), epoch(T3)]);
}

/// Whether the tree row of Drive file `f` was created at `at`.
fn created_at(tree: &Value, f: &Value, at: &str) -> bool {
    epoch(
        row(tree, f["id"].as_str().unwrap())["created"]
            .as_str()
            .unwrap(),
    ) == epoch(at)
}

/// `modified` and `created` are what each provider's API reports for the node.
#[tokio::test]
async fn modified_and_created_are_honoured() {
    let sim = bunko_cloudsim::start().await;
    // Only one given: the other takes it.
    let id = node_id(
        &sim.base,
        json!({"provider": "google", "name": "v.cbz", "content_base64": b64(b"x"),
               "modified": T2}),
    )
    .await;
    let t = token(&sim.base, "google", "default").await;
    let f: Value = http()
        .get(format!("{}/drive/v3/files/{id}", sim.drive_api_base()))
        .bearer_auth(&t)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(epoch(f["modifiedTime"].as_str().unwrap()), epoch(T2));
    assert_eq!(epoch(f["createdTime"].as_str().unwrap()), epoch(T2));
    // A later write is later than the seeded time, not before it.
    sim.drive("default")
        .lock()
        .rewrite(&id, b"y".to_vec().into());
    let tree = get(&sim.base, "tree?provider=google").await;
    assert!(epoch(row(&tree, &id)["modified"].as_str().unwrap()) > epoch(T2));

    let id = node_id(
        &sim.base,
        json!({"provider": "microsoft", "name": "v.cbz", "content_base64": b64(b"x"),
               "created": T1, "modified": T3}),
    )
    .await;
    let t = token(&sim.base, "microsoft", "default").await;
    let f: Value = http()
        .get(format!("{}/me/drive/items/{id}", sim.graph_api_base()))
        .bearer_auth(&t)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        epoch(f["lastModifiedDateTime"].as_str().unwrap()),
        epoch(T3)
    );
    assert_eq!(epoch(f["createdDateTime"].as_str().unwrap()), epoch(T1));
    let tree = get(&sim.base, "tree?provider=microsoft").await;
    assert_eq!(
        epoch(row(&tree, &id)["created"].as_str().unwrap()),
        epoch(T1)
    );
    assert_eq!(row(&tree, &id)["parent_id"], "ROOT");

    // MEGA keeps one time per node (`ts`): the modified one.
    let id = node_id(
        &sim.base,
        json!({"provider": "mega", "name": "v.cbz", "content_base64": b64(b"x"),
               "created": T1, "modified": T3}),
    )
    .await;
    assert_eq!(sim.mega("default").node(&id).unwrap().ts, epoch(T3));
    let tree = get(&sim.base, "tree?provider=mega").await;
    assert_eq!(
        epoch(row(&tree, &id)["modified"].as_str().unwrap()),
        epoch(T3)
    );
    assert_eq!(
        epoch(row(&tree, &id)["created"].as_str().unwrap()),
        epoch(T3)
    );
    // A file's content round-trips through MEGA's crypto.
    assert_eq!(
        sim.mega("default").content(&id).unwrap(),
        (b"x".to_vec(), true)
    );
}

/// A node in the trash, or under a trashed folder, is in `/_sim/tree` only with
/// `include_trashed=1` (marked `trashed`), and never in `/_sim/files`.
#[tokio::test]
async fn trashed_nodes_show_only_in_the_full_tree() {
    let sim = bunko_cloudsim::start().await;
    for p in ["google", "microsoft", "mega"] {
        let series = node_id(
            &sim.base,
            json!({"provider": p, "parent_path": "mokuro-reader", "name": "S", "folder": true}),
        )
        .await;
        let vol = node_id(
            &sim.base,
            json!({"provider": p, "parent_path": "/mokuro-reader/S/", "name": "v.cbz",
                   "content_base64": b64(b"v")}),
        )
        .await;
        let tree = get(&sim.base, &format!("tree?provider={p}")).await;
        assert_eq!(row(&tree, &vol)["parent_id"], series.as_str(), "{p}");
        let root = row(&tree, &series)["parent_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(row(&tree, &root)["name"], "mokuro-reader", "{p}");
        // Trash the series folder the way the provider's own clients do.
        match p {
            "google" => {
                let t = token(&sim.base, p, "default").await;
                let r = http()
                    .patch(format!("{}/drive/v3/files/{series}", sim.drive_api_base()))
                    .bearer_auth(&t)
                    .json(&json!({"trashed": true}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), 200);
            }
            "microsoft" => {
                let t = token(&sim.base, p, "default").await;
                let r = http()
                    .delete(format!("{}/me/drive/items/{series}", sim.graph_api_base()))
                    .bearer_auth(&t)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), 204);
            }
            _ => sim
                .mega("default")
                .move_to(&series, bunko_cloudsim::mega::RUBBISH),
        }
        assert_eq!(
            get(&sim.base, &format!("files?provider={p}")).await,
            json!([]),
            "{p}"
        );
        let live = get(&sim.base, &format!("tree?provider={p}")).await;
        assert_eq!(ids(&live), std::slice::from_ref(&root), "{p}");
        let full = get(&sim.base, &format!("tree?provider={p}&include_trashed=1")).await;
        assert_eq!(full.as_array().unwrap().len(), 3, "{p}: {full}");
        assert_eq!(row(&full, &root)["trashed"], false, "{p}");
        assert_eq!(row(&full, &series)["trashed"], true, "{p}");
        assert_eq!(row(&full, &vol)["trashed"], true, "{p}");
    }
}

/// Graph refuses a second child of the same (case-folded) name, as the real one does.
#[tokio::test]
async fn microsoft_refuses_a_duplicate_name() {
    let sim = bunko_cloudsim::start().await;
    node_id(
        &sim.base,
        json!({"provider": "microsoft", "name": "mokuro-reader", "folder": true}),
    )
    .await;
    for name in ["mokuro-reader", "Mokuro-Reader"] {
        let (s, v) = node(
            &sim.base,
            json!({"provider": "microsoft", "name": name, "folder": true}),
        )
        .await;
        assert_eq!(s, 409, "{name}: {v}");
        assert_eq!(v["error"]["code"], "nameAlreadyExists", "{name}");
    }
    assert_eq!(
        get(&sim.base, "tree?provider=microsoft")
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

/// Mistakes are the test's (400, not violations), and neither call is logged.
#[tokio::test]
async fn node_mistakes_are_refused_and_nothing_is_logged() {
    let sim = bunko_cloudsim::start().await;
    let folder = node_id(
        &sim.base,
        json!({"provider": "google", "name": "d", "folder": true}),
    )
    .await;
    let file = node_id(
        &sim.base,
        json!({"provider": "google", "parent_id": folder, "name": "f", "content_base64": b64(b"f")}),
    )
    .await;
    for (body, says) in [
        (
            json!({"provider": "google", "parent_id": "nope", "name": "x"}),
            "nope",
        ),
        (
            json!({"provider": "google", "parent_id": file, "name": "x"}),
            "not a folder",
        ),
        (
            json!({"provider": "google", "parent_id": folder, "parent_path": "d", "name": "x"}),
            "parent_id or parent_path",
        ),
        (json!({"provider": "google", "folder": true}), "name"),
        (
            json!({"provider": "google", "name": "x", "folder": true, "content_base64": b64(b"x")}),
            "folder",
        ),
        (
            json!({"provider": "google", "name": "x", "modified": "yesterday"}),
            "modified",
        ),
        (
            json!({"provider": "mega", "parent_id": "N9999999", "name": "x"}),
            "N9999999",
        ),
        (
            json!({"provider": "microsoft", "parent_id": "F404", "name": "x"}),
            "F404",
        ),
        (json!({"provider": "dropbox", "name": "x"}), "dropbox"),
    ] {
        let (s, v) = node(&sim.base, body.clone()).await;
        assert_eq!(s, 400, "{body}: {v}");
        assert!(v["error"].as_str().unwrap().contains(says), "{body}: {v}");
    }
    let r = http()
        .get(format!("{}/_sim/tree?provider=dropbox", sim.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(get(&sim.base, "log").await, json!([]));
    assert_eq!(get(&sim.base, "violations").await, json!([]));
}
