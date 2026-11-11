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
    listing_as(path, depth, "basic:reader")
}

fn listing_as(path: &str, depth: &str, auth: &str) -> Req {
    Req::new("PROPFIND", path, auth)
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
    // `registered`: progress and reading history only.
    let r = export(
        c,
        &server,
        "login-me-registered",
        Req::new("GET", me, "basic:progress"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
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
    // Sent with the real ETag; recorded with the sentinel.
    let req = Req::new("GET", &path, "basic:reader").header("if-none-match", &etag);
    let resp = server.send(&req).await;
    assert_eq!(resp.status, 304, "{}", resp.text());
    c.golden("queue-file-http-not-modified", &fixture(&req, &resp));
}

/// A queue file v2 answer made independent of when it ran: every time (`at`,
/// `clock_at`, the clock's knots, the jobs' times, `done`) moved by one offset so the
/// clock's fit time lands on the epoch sentinel, and `rev` (which starts from the
/// time) set to 1. Knots stay strictly increasing, and every time keeps its distance
/// to the others.
fn steady_series_queue(resp: &contract_support::Resp) -> contract_support::Resp {
    let v = steady_queue_body(resp.json().expect("the series queue file is JSON"));
    contract_support::Resp {
        status: resp.status,
        headers: resp.headers.clone(),
        body: serde_json::to_vec(&v).unwrap(),
    }
}

/// [`steady_series_queue`] on a body.
fn steady_queue_body(mut v: Value) -> Value {
    let fit = v["live"]["clock_at"]
        .as_i64()
        .or_else(|| v["clock_at"].as_i64())
        .expect("a clock_at");
    let off = fit - contract_support::EPOCH_S;
    let shift = |x: &mut Value| {
        if let Some(n) = x.as_i64() {
            *x = json!(n - off);
        }
    };
    shift(&mut v["at"]);
    shift(&mut v["clock_at"]);
    v["rev"] = json!(1);
    if let Some(live) = v.get_mut("live").filter(|l| l.is_object()) {
        shift(&mut live["clock_at"]);
        for knot in live["clock"].as_array_mut().into_iter().flatten() {
            for x in knot.as_array_mut().into_iter().flatten() {
                shift(x);
            }
        }
    }
    for jobs in v["vols"]
        .as_object_mut()
        .into_iter()
        .flat_map(|m| m.values_mut())
    {
        for job in jobs.as_array_mut().into_iter().flatten() {
            for x in job.as_array_mut().into_iter().flatten().skip(2) {
                shift(x);
            }
        }
    }
    for x in v["done"]
        .as_object_mut()
        .into_iter()
        .flat_map(|m| m.values_mut())
    {
        shift(x);
    }
    v
}

/// The marker's `series_queue` block made independent of when it ran: its clock moved
/// so its fit time lands on the epoch sentinel.
fn steady_marker(mut m: Value) -> Value {
    let Some(fit) = m["series_queue"]["clock_at"].as_i64() else {
        return m;
    };
    let off = fit - contract_support::EPOCH_S;
    let block = &mut m["series_queue"];
    block["clock_at"] = json!(fit - off);
    for knot in block["clock"].as_array_mut().into_iter().flatten() {
        for x in knot.as_array_mut().into_iter().flatten() {
            if let Some(n) = x.as_i64() {
                *x = json!(n - off);
            }
        }
    }
    m
}

/// The queue file v2 over HTTP (`docs/design/queue-file-v2.md` §7, §12.2): a series
/// that owes OCR (Volume 02, held: no processor), the same with its ETag, a series
/// that owes nothing, a write refused; and one file with every job state, rendered by
/// the code that renders the real ones.
async fn series_queue(c: &mut Contract, server: &Server) {
    let path = format!(
        "{}/.mokuro-series-queue.json",
        dav_path(&["Contract Series"])
    );
    let req = Req::new("GET", &path, "basic:reader");
    let r = server.send(&req).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let doc = r.json().unwrap();
    assert_eq!(doc["vols"], json!({"Volume 02": [[0, "q", null]]}), "{doc}");
    c.golden(
        "series-queue-http",
        &fixture(&req, &steady_series_queue(&r)),
    );
    let etag = r.header("etag").expect("an ETag").to_string();
    // Sent with the real ETag; recorded with the sentinel.
    let req = Req::new("GET", &path, "basic:reader").header("if-none-match", &etag);
    let resp = server.send(&req).await;
    assert_eq!(resp.status, 304, "{}", resp.text());
    c.golden("series-queue-http-not-modified", &fixture(&req, &resp));
    let empty = format!(
        "{}/.mokuro-series-queue.json",
        dav_path(&["日本語シリーズ"])
    );
    let req = Req::new("GET", &empty, "basic:reader");
    let r = server.send(&req).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json().unwrap()["vols"], json!({}));
    c.golden(
        "series-queue-http-empty",
        &fixture(&req, &steady_series_queue(&r)),
    );
    let r = export(
        c,
        server,
        "series-queue-put-refused",
        Req::new("PUT", &path, "basic:reader")
            .header("content-type", "application/json")
            .text("{}"),
    )
    .await;
    assert_eq!(r.status, 405, "{}", r.text());
    c.golden(
        "series-queue-shapes",
        &file_fixture(
            "Example Saga/.mokuro-series-queue.json",
            &series_queue_shapes(),
        ),
    );
}

/// A made-up series with every job state: running (and finishing), claimed, waiting
/// with a work position, untimed, held by its row, and a finished volume; with the
/// `live` block of the HTTP copy. Times are on the epoch sentinel.
fn series_queue_shapes() -> Value {
    use bunko_sched::series_queue::{
        Clock, LiveJob, LiveState, RowDesc, SeriesLive, Sink, Snapshot, live_block,
    };
    let t0 = contract_support::EPOCH_S as f64;
    let rows = vec![
        RowDesc {
            kind: "ocr".into(),
            id: "hayai-nova".into(),
            parts: json!([{"role": "recognizer", "name": "Hayai Nova", "version": "2.5"},
                          {"role": "detector", "name": "PP-OCR Manga", "version": "0.3"}]),
        },
        RowDesc {
            kind: "layer".into(),
            id: "paddle".into(),
            parts: json!([{"role": "recognizer", "name": "PaddleOCR-VL", "version": "0.3"}]),
        },
    ];
    let job = |row: &str, state: LiveState| LiveJob {
        row: row.into(),
        state,
    };
    let snapshot = |now: f64, vols: Vec<(&str, Vec<LiveJob>)>| Snapshot {
        now,
        rows: rows.clone(),
        held: None,
        held_rows: Default::default(),
        series: [(
            "example saga".to_string(),
            SeriesLive {
                folder: "Example Saga".into(),
                vols: vols.into_iter().map(|(s, j)| (s.to_string(), j)).collect(),
            },
        )]
        .into_iter()
        .collect(),
    };
    let mut sink = Sink::new(0, None);
    sink.clock = Some(Clock::identity(t0 as i64));
    let first = snapshot(
        t0,
        vec![
            (
                "Example Saga 01",
                vec![job(
                    "hayai-nova",
                    LiveState::Running {
                        t0: t0 - 300.0,
                        e: Some(t0 + 60.0),
                    },
                )],
            ),
            (
                "Example Saga 02",
                vec![job(
                    "hayai-nova",
                    LiveState::Running {
                        t0: t0 - 120.0,
                        e: Some(t0 + 600.0),
                    },
                )],
            ),
            (
                "Example Saga 03",
                vec![
                    job(
                        "hayai-nova",
                        LiveState::Queued {
                            e: Some(t0 + 1800.0),
                        },
                    ),
                    job(
                        "paddle",
                        LiveState::Queued {
                            e: Some(t0 + 9000.0),
                        },
                    ),
                ],
            ),
            (
                "Example Saga 04",
                vec![job("hayai-nova", LiveState::Claimed)],
            ),
            (
                "Example Saga 05",
                vec![job("hayai-nova", LiveState::Queued { e: None })],
            ),
        ],
    );
    sink.refresh(&first, &|_| true);
    // Volume 01 finishes and its series settles: it leaves, with a done entry. The
    // layer row is held from now: its job keeps the position it was published with.
    let mut second = first.clone();
    second.now = t0 + 120.0;
    second.held_rows.insert("paddle".into());
    second
        .series
        .get_mut("example saga")
        .unwrap()
        .vols
        .remove("Example Saga 01");
    sink.refresh(&second, &|_| true);
    // Volume 02 finishes while the series' metadata is still to be recompiled: it
    // stays, shown running, until the series settles.
    let mut third = second.clone();
    third.now = t0 + 900.0;
    third
        .series
        .get_mut("example saga")
        .unwrap()
        .vols
        .remove("Example Saga 02");
    sink.refresh(&third, &|_| false);
    let published = &sink.series["example saga"];
    let mut body = published.body("example-instance");
    body.insert(
        "live".into(),
        live_block(&third, sink.clock.as_ref().unwrap(), 5),
    );
    Value::Object(body)
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
        // As the reader sends it to a server that verifies PUTs (the digest of the
        // body actually sent replaces the placeholder).
        .header("content-digest", contract_support::DIGEST_SENTINEL)
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
        .header("content-digest", contract_support::DIGEST_SENTINEL)
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

/// Series owners (0.8): a series the admin created is closed to `reader`, whose `/me`
/// lists it (never its owner) and whose PUT into it is refused, saying why. The volume
/// `reader` added to the shared (ownerless) `日本語シリーズ` is in its `/me` as one it
/// may publish the curated layer of (D1).
async fn series_owners(c: &mut Contract, seed: &Seed) {
    let server = Server::local(seed).await;
    let r = server
        .send(
            &Req::new(
                "PUT",
                &dav_path(&["日本語シリーズ", "第2巻.cbz"]),
                "basic:reader",
            )
            .header("content-type", "application/octet-stream")
            .cbz(1),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = server
        .send(&Req::new(
            "MKCOL",
            &dav_path(&["Admin Series"]),
            "basic:admin",
        ))
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = server
        .send(
            &Req::new(
                "PUT",
                &dav_path(&["Admin Series", "Volume 01.cbz"]),
                "basic:admin",
            )
            .header("content-type", "application/octet-stream")
            .cbz(1),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = export(
        c,
        &server,
        "login-me-closed-series",
        Req::new("GET", "/login/api/me", "basic:reader"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        r.json().unwrap()["permissions"]["addFiles"],
        json!({"scope": "open", "closedSeries": ["Admin Series"]})
    );
    assert_eq!(
        r.json().unwrap()["permissions"]["curated"],
        json!({"scope": "owned", "volumes": [{"series": "日本語シリーズ", "volume": "第2巻"}]})
    );
    let r = export(
        c,
        &server,
        "put-refused-series-owned",
        Req::new(
            "PUT",
            &dav_path(&["Admin Series", "Volume 09.cbz"]),
            "basic:reader",
        )
        .header("content-type", "application/octet-stream")
        .cbz(1),
    )
    .await;
    assert_eq!(r.status, 403, "{}", r.text());
    server.stop().await;
}

/// Reading history (0.8): `registered` keeps its own `history/<device>/<file>` tree
/// under the reader root, and nobody else lists it.
async fn history(c: &mut Contract, seed: &Seed) {
    let server = Server::local(seed).await;
    let history = format!("/{ROOT}/history");
    let device = format!("{history}/0f6c-dev");
    let r = export(
        c,
        &server,
        "history-mkcol",
        Req::new("MKCOL", &history, "basic:progress"),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = export(
        c,
        &server,
        "history-mkcol-exists",
        Req::new("MKCOL", &history, "basic:progress"),
    )
    .await;
    assert_eq!(r.status, 405, "{}", r.text());
    let r = server
        .send(&Req::new("MKCOL", &device, "basic:progress"))
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = export(
        c,
        &server,
        "history-put",
        // As the reader sends every file to a server that verifies PUTs.
        Req::new("PUT", &format!("{device}/2026-10.events"), "basic:progress")
            .header("content-type", "application/octet-stream")
            .header("content-digest", contract_support::DIGEST_SENTINEL)
            .text("contract reading history\n"),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.text());
    export(
        c,
        &server,
        "history-propfind-depth1",
        listing_as(&format!("{device}/"), "1", "basic:progress"),
    )
    .await;
    let root = format!("/{ROOT}/");
    let r = export(
        c,
        &server,
        "history-propfind-reader-root-infinity",
        listing_as(&root, "infinity", "basic:progress"),
    )
    .await;
    assert!(r.text().contains("2026-10.events"), "{}", r.text());
    let r = export(
        c,
        &server,
        "history-propfind-other-user",
        listing(&root, "infinity"),
    )
    .await;
    assert!(!r.text().contains("history"), "{}", r.text());
    server.stop().await;
}

/// Corrections (D27): `<Vol>.edit.mokuro` is each account's own (`registered`
/// included), nobody else sees it; the shared `curated` layer is not `registered`'s to
/// publish.
async fn personal(c: &mut Contract, seed: &Seed) {
    let server = Server::local(seed).await;
    let edit = dav_path(&["Contract Series", "Volume 01.edit.mokuro"]);
    // The reader's edit layer: the volume's pages, an `edited` block, no `ocr_engine`.
    let mut layer = mokuro_doc("Contract Series", "Volume 01", 2);
    layer["edited"] = json!({"by": "mokuro-reader", "at": "2000-01-01T00:00:00.000Z",
        "base_sha256": null, "base_seal": null});
    let r = export(
        c,
        &server,
        "personal-put",
        // As the reader sends every file to a server that verifies PUTs.
        Req::new("PUT", &edit, "basic:progress")
            .header("content-type", "application/octet-stream")
            .header("content-digest", contract_support::DIGEST_SENTINEL)
            .json(layer.clone()),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = export(
        c,
        &server,
        "personal-get-other-user",
        Req::new("GET", &edit, "basic:reader"),
    )
    .await;
    assert_eq!(r.status, 404, "{}", r.text());
    let r = export(
        c,
        &server,
        "curated-put-refused",
        Req::new(
            "PUT",
            &dav_path(&["Contract Series", "Volume 01.curated.mokuro"]),
            "basic:progress",
        )
        .header("content-type", "application/octet-stream")
        .header("content-digest", contract_support::DIGEST_SENTINEL)
        .json(layer),
    )
    .await;
    assert_eq!(r.status, 403, "{}", r.text());
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

/// The lease runs with a short settle wait: this Bunko holds it at once. A change of
/// the queue's hold reaches the marker within a second.
fn quick_claim(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = true;
    c.lease_settle = Some(Duration::from_millis(100));
    c.retry_base = Duration::from_millis(50);
    c.series_queue_timing = bunko_remote::SeriesQueueTiming {
        hold_debounce: Duration::from_millis(100),
        unhold_debounce: Duration::from_millis(100),
        early_gap: Duration::from_millis(300),
        refit_every: Duration::from_secs(3600),
    };
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
    // The queue rides in the marker and in each series' file (queue file v2): the
    // marker's block shows the queue's hold (no processor) once the scheduler's first
    // walk is over, and the series that owes OCR has its file.
    let series_file = "Contract Series/.mokuro-series-queue.json";
    eventually(
        "the queue in the marker and the series' file",
        Duration::from_secs(30),
        || {
            cloud_json(&server, ".mokuro-bunko.json")
                .is_some_and(|m| m["series_queue"]["held"]["reason"] == "no-processor")
                && cloud_json(&server, series_file).is_some()
        },
    )
    .await;
    let active = cloud_json(&server, ".mokuro-bunko.json").unwrap();
    assert_eq!(active["queue"], Value::Null, "{active}");
    c.golden(
        "marker-active",
        &file_fixture(".mokuro-bunko.json", &steady_marker(active)),
    );
    let stored = cloud_json(&server, series_file).unwrap();
    assert_eq!(
        stored["vols"],
        json!({"Volume 02": [[0, "q", null]]}),
        "{stored}"
    );
    assert!(
        stored.get("live").is_none(),
        "a stored copy carries no live block"
    );
    let mut stored = steady_queue_body(stored);
    // When it was written is no part of the shape.
    stored["at"] = json!(contract_support::EPOCH_S);
    c.golden("series-queue-stored", &file_fixture(series_file, &stored));
    // A listing of the series through the bridge shows no queue file, the stored copy
    // included.
    let r = export(
        c,
        &server,
        "propfind-series-depth1-cloud",
        listing(&dav_path(&["Contract Series"]), "1"),
    )
    .await;
    assert_eq!(r.status, 207, "{}", r.text());
    assert!(
        !r.text().to_lowercase().contains("series-queue"),
        "{}",
        r.text()
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
        &file_fixture(".mokuro-bunko.json", &steady_marker(marker)),
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
        series_queue(c, &server).await;
        compiled(c, &server).await;
        webdav(c, &server).await;
        server.stop().await;
    }
    puts(c, seed).await;
    series_owners(c, seed).await;
    history(c, seed).await;
    personal(c, seed).await;
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
