//! The Bunko side of the reader contract (spec interop-testing §5): every endpoint and
//! file the reader reads, driven on a seeded full-stack server, normalized and compared
//! with `interop/contract/bunko/*.json` (`BUNKO_BLESS=1` rewrites them). The reader's
//! unit tests parse those files, so a change here shows as a fixture diff in the PR and
//! fails the reader's tests if it breaks them.

// Fixtures are read and written with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod contract_support;

use std::time::Duration;

use contract_support::{
    Contract, ETAG_SENTINEL, ROOT, Req, Seed, Server, contract_dir, dav_path, file_fixture,
    fixture, mokuro_doc,
};
use serde_json::{Value, json};

/// The body `checkWritePermissions` sends (reader `webdav-provider.ts`).
const PRIVILEGES_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:current-user-privilege-set/>
  </D:prop>
</D:propfind>"#;

const LOCK_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype><D:owner>contract</D:owner></D:lockinfo>"#;

/// The reader's origin: a browser sends it on every request the reader makes.
const READER_ORIGIN: &str = "https://reader.mokuro.app";

/// The `webdav` library's listing request (`getDirectoryContents`).
fn listing(path: &str, depth: &str) -> Req {
    Req::new("PROPFIND", path, "basic:reader")
        .header("accept", "text/plain,application/xml")
        .header("depth", depth)
}

fn token_body(user: &str, password: &str) -> Value {
    // `requestBunkoToken` (reader `bunko-token.ts`).
    json!({"username": user, "password": password, "kind": "reader", "label": "mokuro-reader"})
}

fn manifest(series: &str, volume: &str) -> String {
    format!(
        "/catalog/api/manifest?series={}&volume={}",
        contract_support::encode(series),
        contract_support::encode(volume)
    )
}

async fn export(c: &mut Contract, server: &Server, name: &str, req: Req) -> contract_support::Resp {
    let resp = server.send(&req).await;
    c.golden(name, &fixture(&req, &resp));
    resp
}

/// Ten wrong passwords for `user` on WebDAV (its own limiter): the next check is refused.
async fn exhaust_dav(server: &Server, user: &str) {
    let bad = format!(
        "raw:{}",
        contract_support::basic_value(user, "wrong-password")
    );
    for _ in 0..10 {
        let r = server
            .send(&Req::new("PROPFIND", &format!("/{ROOT}/"), &bad).header("depth", "0"))
            .await;
        assert_eq!(r.status, 401, "{}", r.text());
    }
}

/// Wrong passwords for `user` on the login API until the limiter refuses (ten failures,
/// counting any made before).
async fn exhaust(server: &Server, user: &str) {
    for _ in 0..10 {
        let r = server
            .send(
                &Req::new("POST", "/login/api/token", "none")
                    .header("content-type", "application/json")
                    .json(token_body(user, "wrong-password")),
            )
            .await;
        if r.status == 429 {
            return;
        }
        assert_eq!(r.status, 401, "{}", r.text());
    }
}

async fn identity(c: &mut Contract, seed: &Seed) {
    let server = Server::local(seed).await;
    let me = "/login/api/me";
    let r = export(
        c,
        &server,
        "login-me-basic",
        Req::new("GET", me, "basic:reader"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    export(
        c,
        &server,
        "login-me-bearer",
        Req::new("GET", me, "bearer:reader"),
    )
    .await;
    export(
        c,
        &server,
        "login-me-anonymous",
        Req::new("GET", me, "none"),
    )
    .await;
    let bad = format!(
        "raw:{}",
        contract_support::basic_value("reader", "not-the-password")
    );
    let r = export(
        c,
        &server,
        "login-me-bad-password",
        Req::new("GET", me, &bad),
    )
    .await;
    assert_eq!(r.status, 401);
    export(
        c,
        &server,
        "login-me-viewer",
        Req::new("GET", me, "basic:viewer"),
    )
    .await;
    let r = export(
        c,
        &server,
        "login-me-bearer-invalid",
        Req::new("GET", me, "raw:Bearer not-a-real-token"),
    )
    .await;
    assert_eq!(r.status, 401, "{}", r.text());
    // Last: the limiter stays hot for `reader` on this server.
    exhaust(&server, "reader").await;
    let r = export(
        c,
        &server,
        "login-me-rate-limited",
        Req::new("GET", me, "basic:reader"),
    )
    .await;
    assert_eq!(r.status, 429, "{}", r.text());
    server.stop().await;
}

async fn tokens(c: &mut Contract, seed: &Seed) {
    let server = Server::local(seed).await;
    let issue = Req::new("POST", "/login/api/token", "none")
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .json(token_body("reader", "<password:reader>"));
    let r = export(c, &server, "login-token-issue", issue.clone()).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = export(
        c,
        &server,
        "login-token-revoke",
        Req::new("DELETE", "/login/api/token", "bearer:reader"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    exhaust(&server, "reader").await;
    let r = export(c, &server, "login-token-rate-limited", issue).await;
    assert_eq!(r.status, 429, "{}", r.text());
    server.stop().await;
}

async fn manifests(c: &mut Contract, seed: &Seed, server: &Server) {
    let _ = seed;
    for (name, series, volume) in [
        ("manifest-with-ocr", "Contract Series", "Volume 01"),
        ("manifest-without-ocr", "Contract Series", "Volume 02"),
        ("manifest-japanese", "日本語シリーズ", "第1巻"),
        ("manifest-unknown-volume", "Contract Series", "Volume 99"),
    ] {
        export(
            c,
            server,
            name,
            Req::new("GET", &manifest(series, volume), "basic:reader"),
        )
        .await;
    }
}

async fn queue(c: &mut Contract, server: &Server) {
    let path = format!("/{ROOT}/.mokuro-queue.json");
    let r = export(
        c,
        server,
        "queue-file-http",
        Req::new("GET", &path, "basic:reader"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let etag = r
        .header("etag")
        .expect("the queue file has an ETag")
        .to_string();
    let doc = r.json().expect("the queue file is JSON");
    // Sent with the real ETag; recorded with the sentinel.
    let req = Req::new("GET", &path, "basic:reader").header("if-none-match", &etag);
    let resp = server.send(&req).await;
    assert_eq!(resp.status, 304, "{}", resp.text());
    c.golden("queue-file-http-not-modified", &fixture(&req, &resp));

    // The cloud copy (`publish_cloud_queue`): the HTTP document, with `done[]`.
    let generated = doc["generated_at"].as_str().expect("generated_at");
    let now_ms =
        time::OffsetDateTime::parse(generated, &time::format_description::well_known::Rfc3339)
            .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
            .expect("generated_at is ISO");
    let done = vec![
        bunko_remote::status::DoneEntry {
            series: "Contract Series".into(),
            volume: "Volume 01".into(),
            at_ms: now_ms - 60_000,
            ocr: Some("Contract Series/Volume 01.mokuro".into()),
            layers: vec![],
        },
        bunko_remote::status::DoneEntry {
            series: "日本語シリーズ".into(),
            volume: "第1巻".into(),
            at_ms: now_ms - 120_000,
            ocr: Some("日本語シリーズ/第1巻.mokuro".into()),
            layers: vec![bunko_remote::status::DoneLayer {
                id: "hayai".into(),
                path: "日本語シリーズ/第1巻.hayai.mokuro".into(),
            }],
        },
    ];
    let full = bunko_remote::status::cloud_queue_doc(&doc, "contract-instance", &done, 1 << 20);
    assert_eq!(full["done"].as_array().map(Vec::len), Some(2), "{full}");
    c.golden(
        "queue-file-cloud",
        &file_fixture(".mokuro-queue.json", &full),
    );
    let size = serde_json::to_vec(&full).unwrap().len();
    let cut = bunko_remote::status::cloud_queue_doc(&doc, "contract-instance", &done, size - 1);
    assert_eq!(cut["truncated"], true, "{cut}");
    c.golden(
        "queue-file-cloud-truncated",
        &file_fixture(".mokuro-queue.json", &cut),
    );
}

async fn compiled(c: &mut Contract, server: &Server) {
    let lib = server.library();
    for (name, rel) in [
        ("series-json", "Contract Series/series.json"),
        ("series-json-japanese", "日本語シリーズ/series.json"),
        ("catalog-json", "catalog.json"),
    ] {
        let raw = std::fs::read(lib.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        let v: Value = serde_json::from_slice(&raw).unwrap();
        c.golden(name, &file_fixture(rel, &v));
    }
}

async fn webdav(c: &mut Contract, server: &Server) {
    let root = format!("/{ROOT}/");
    let r = export(
        c,
        server,
        "propfind-root-depth0-privileges",
        Req::new("PROPFIND", &root, "basic:reader")
            .header("depth", "0")
            .text(PRIVILEGES_BODY),
    )
    .await;
    assert_eq!(r.status, 207, "{}", r.text());
    export(c, server, "propfind-root-depth1", listing(&root, "1")).await;
    // The reader's sign-in: a listing of the server root (its connection test), then
    // the library folder's `stat` (`exists`).
    export(c, server, "propfind-server-root-depth1", listing("/", "1")).await;
    export(
        c,
        server,
        "propfind-library-depth0",
        listing(&format!("/{ROOT}"), "0"),
    )
    .await;
    let series = format!("{}/", dav_path(&["Contract Series"]));
    export(c, server, "propfind-series-depth1", listing(&series, "1")).await;
    export(
        c,
        server,
        "propfind-root-depth-infinity",
        listing(&root, "infinity"),
    )
    .await;
    // The reader's own OPTIONS on the library (its write-permission fallback reads
    // `Allow`, its probe `X-Mokuro-Put`), as a browser sends it after the preflight.
    let r = export(
        c,
        server,
        "options-library",
        Req::new("OPTIONS", &root, "basic:reader").header("origin", READER_ORIGIN),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    // The reader's control-file read (`readControlFile`) with the ETag it holds.
    let path = format!("/{ROOT}/.mokuro-queue.json");
    let first = server.send(&Req::new("GET", &path, "basic:reader")).await;
    let etag = first.header("etag").unwrap().to_string();
    let req = Req::new("GET", &path, "basic:reader").header("if-none-match", &etag);
    let resp = server.send(&req).await;
    c.golden("get-control-file-not-modified", &fixture(&req, &resp));
}

async fn puts(c: &mut Contract, seed: &Seed) {
    let server = Server::local(seed).await;
    let r = export(
        c,
        &server,
        "put-volume",
        Req::new(
            "PUT",
            &dav_path(&["Contract Series", "Volume 03.cbz"]),
            "basic:reader",
        )
        .header("content-type", "application/octet-stream")
        .cbz(2),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = export(
        c,
        &server,
        "put-sidecar",
        Req::new(
            "PUT",
            &dav_path(&["Contract Series", "Volume 02.mokuro"]),
            "basic:reader",
        )
        // The reader PUTs every file as an octet stream (`uploadFileWithClient`).
        .header("content-type", "application/octet-stream")
        .json(mokuro_doc("Contract Series", "Volume 02", 2)),
    )
    .await;
    assert!(matches!(r.status, 201 | 204), "{}", r.text());
    let r = export(
        c,
        &server,
        "put-refused-viewer",
        Req::new(
            "PUT",
            &dav_path(&["Contract Series", "Volume 04.cbz"]),
            "basic:viewer",
        )
        .header("content-type", "application/octet-stream")
        .cbz(1),
    )
    .await;
    assert_eq!(r.status, 403, "{}", r.text());
    let r = server
        .send(
            &Req::new(
                "PUT",
                &dav_path(&["Contract Series", "Volume 03.cbz"]),
                "basic:reader",
            )
            .header("if-match", "\"stale-etag\"")
            .cbz(3),
        )
        .await;
    assert_eq!(r.status, 412, "{}", r.text());
    c.golden(
        "put-precondition-failed",
        &fixture(
            &Req::new(
                "PUT",
                &dav_path(&["Contract Series", "Volume 03.cbz"]),
                "basic:reader",
            )
            .header("if-match", ETAG_SENTINEL)
            .cbz(3),
            &r,
        ),
    );
    server.stop().await;
}

async fn errors(c: &mut Contract, seed: &Seed) {
    let server = Server::local(seed).await;
    let root = format!("/{ROOT}/");
    let r = export(
        c,
        &server,
        "dav-401-basic-missing",
        Req::new(
            "PUT",
            &dav_path(&["Contract Series", "Volume 05.cbz"]),
            "none",
        )
        .cbz(1),
    )
    .await;
    assert_eq!(r.status, 401, "{}", r.text());
    let r = export(
        c,
        &server,
        "dav-401-bearer-invalid",
        Req::new("PROPFIND", &root, "raw:Bearer not-a-real-token").header("depth", "0"),
    )
    .await;
    assert_eq!(r.status, 401, "{}", r.text());
    // `btoa()` of `reader:` + the Latin-1 bytes of the password's first part, as an old
    // reader built the header.
    let mut latin1 = b"reader:contract-p".to_vec();
    latin1.extend_from_slice(b"\xe4ssw\xf6rd");
    let raw = {
        use base64::Engine as _;
        format!(
            "raw:Basic {}",
            base64::engine::general_purpose::STANDARD.encode(&latin1)
        )
    };
    let r = export(
        c,
        &server,
        "dav-401-basic-latin1",
        Req::new("PROPFIND", &root, &raw).header("depth", "0"),
    )
    .await;
    assert_eq!(r.status, 401, "{}", r.text());
    let r = export(
        c,
        &server,
        "dav-409",
        Req::new(
            "PUT",
            &dav_path(&["No Such Series", "Deeper", "Volume 01.cbz"]),
            "basic:reader",
        )
        .cbz(1),
    )
    .await;
    assert_eq!(r.status, 409, "{}", r.text());
    // A LOCK held by another client: a write without its token is refused.
    let target = dav_path(&["Contract Series", "Volume 01.cbz"]);
    let lock = server
        .send(
            &Req::new("LOCK", &target, "basic:admin")
                .header("content-type", "application/xml")
                .header("timeout", "Second-600")
                .text(LOCK_BODY),
        )
        .await;
    assert_eq!(lock.status, 200, "{}", lock.text());
    let r = export(
        c,
        &server,
        "dav-423",
        Req::new("PUT", &target, "basic:admin").cbz(2),
    )
    .await;
    assert_eq!(r.status, 423, "{}", r.text());
    server.stop().await;

    let server = Server::local(seed).await;
    exhaust_dav(&server, "reader").await;
    let r = export(
        c,
        &server,
        "dav-429",
        Req::new("PROPFIND", &root, "basic:reader").header("depth", "0"),
    )
    .await;
    assert_eq!(r.status, 429, "{}", r.text());
    server.stop().await;
}

/// No lease (writes may leave at once).
fn no_lease(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = false;
    c.queue_floor = Some(Duration::from_millis(200));
    c.retry_base = Duration::from_millis(50);
}

/// The lease runs with a short settle wait: this Bunko holds it at once.
fn quick_claim(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = true;
    c.lease_settle = Some(Duration::from_millis(100));
    c.retry_base = Duration::from_millis(50);
}

/// The lease runs, and its first claim waits an hour before it may hold the lease.
fn slow_claim(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = true;
    c.lease_settle = Some(Duration::from_secs(3600));
    c.retry_base = Duration::from_millis(50);
}

async fn eventually(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let until = std::time::Instant::now() + timeout;
    while !f() {
        assert!(
            std::time::Instant::now() < until,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn cloud_json(server: &Server, rel: &str) -> Option<Value> {
    let fake = server.fake.as_ref()?;
    let n = fake.node_at(&format!("{ROOT}/{rel}"))?;
    serde_json::from_slice(&fake.content(&n.pid)?).ok()
}

async fn markers(c: &mut Contract, seed: &Seed) {
    let server = Server::cloud(seed, quick_claim).await;
    let store = server.store();
    eventually(
        "this Bunko holds the lease",
        Duration::from_secs(30),
        || {
            matches!(
                store.lease().map(|l| l.event()),
                Some(bunko_remote::status::LeaseEvent::Acquired)
            ) && cloud_json(&server, ".mokuro-bunko.json").is_some_and(|m| m["state"] == "active")
        },
    )
    .await;
    let active = cloud_json(&server, ".mokuro-bunko.json").unwrap();
    c.golden(
        "marker-active",
        &file_fixture(".mokuro-bunko.json", &active),
    );
    // A marker is `released` only by a disconnect (§7 step 2); a shutdown writes
    // nothing (§9.2 step 5).
    let s = store.clone();
    let released = tokio::task::spawn_blocking(move || s.release_lease(Duration::from_secs(10)))
        .await
        .unwrap()
        .unwrap();
    assert!(released);
    let marker = cloud_json(&server, ".mokuro-bunko.json").unwrap();
    assert_eq!(marker["state"], "released", "{marker}");
    c.golden(
        "marker-released",
        &file_fixture(".mokuro-bunko.json", &marker),
    );
    drop(store);
    server.stop().await;
}

async fn cloud_errors(c: &mut Contract, seed: &Seed) {
    let server = Server::cloud(seed, slow_claim).await;
    let store = server.store();
    eventually("the settle wait", Duration::from_secs(20), || {
        store.lease().and_then(|l| l.claiming_left()).is_some()
    })
    .await;
    let r = export(
        c,
        &server,
        "dav-503-lease-claiming",
        Req::new(
            "PUT",
            &dav_path(&["Contract Series", "Volume 03.cbz"]),
            "basic:reader",
        )
        .cbz(2),
    )
    .await;
    assert_eq!(r.status, 503, "{}", r.text());
    drop(store);
    server.stop().await;

    let server = Server::cloud(seed, no_lease).await;
    server.fake.as_ref().unwrap().set_quota(Some(1));
    let r = export(
        c,
        &server,
        "dav-507",
        Req::new(
            "PUT",
            &dav_path(&["Contract Series", "Volume 03.cbz"]),
            "basic:reader",
        )
        .cbz(2),
    )
    .await;
    assert_eq!(r.status, 507, "{}", r.text());
    server.stop().await;
}

/// Every fixture, in order.
async fn export_all(c: &mut Contract, seed: &Seed) {
    identity(c, seed).await;
    tokens(c, seed).await;
    {
        // Read-only fixtures share one server.
        let server = Server::local(seed).await;
        manifests(c, seed, &server).await;
        queue(c, &server).await;
        compiled(c, &server).await;
        webdav(c, &server).await;
        server.stop().await;
    }
    puts(c, seed).await;
    errors(c, seed).await;
    markers(c, seed).await;
    cloud_errors(c, seed).await;
}

/// The export runs twice, on fresh servers, and must give the same bytes both times
/// (then it is compared with the committed files).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_the_reader_contract() {
    let seed = Seed::load();
    let mut c = Contract::new(contract_dir().join("bunko"));
    export_all(&mut c, &seed).await;
    let mut again = Contract::new(contract_dir().join("bunko"));
    export_all(&mut again, &seed).await;
    let names = |c: &Contract| {
        c.outputs()
            .iter()
            .map(|(n, _)| n.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(&c),
        names(&again),
        "the two exports made different fixtures"
    );
    for ((name, first), (_, second)) in c.outputs().iter().zip(again.outputs()) {
        assert!(
            first == second,
            "{name}.json differs between two exports:\n{}",
            contract_support::diff(first, second)
        );
    }
    c.finish();
}
