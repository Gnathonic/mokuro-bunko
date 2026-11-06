//! Request accounting through the whole server (docs/design/analytics.md §4): WebDAV
//! and web requests are counted by the outermost layers, the window is flushed into
//! `analytics.sqlite`, and the admin API answers the shapes of §10.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// `series/vol1.cbz`: big enough for several 64 KiB body frames.
const SIZE: usize = 300_000;
const VOL: &str = "/mokuro-reader/series/vol1.cbz";
const IP_A: &str = "203.0.113.10";
const IP_B: &str = "198.51.100.23";

struct Env {
    dir: tempfile::TempDir,
    storage: PathBuf,
    services: Services,
    app: Router,
}

fn basic(user: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:pass1234"))
    )
}

fn volume_bytes() -> Vec<u8> {
    (0..SIZE).map(|i| (i % 251) as u8).collect()
}

impl Env {
    fn new() -> Env {
        Self::with(|_| {})
    }

    fn with(edit: impl FnOnce(&mut Config)) -> Env {
        Self::prepared(edit, |_| {})
    }

    /// `prepare` runs on the storage folder before the server opens it.
    fn prepared(edit: impl FnOnce(&mut Config), prepare: impl FnOnce(&std::path::Path)) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(storage.join("library/series")).unwrap();
        prepare(&storage);
        std::fs::write(storage.join("library/series/vol1.cbz"), volume_bytes()).unwrap();
        std::fs::write(storage.join("library/series/small.cbz"), b"small volume").unwrap();
        let mut config = Config::default();
        config.storage.base_path = storage.clone();
        edit(&mut config);
        // Settings edits save the config: into the temp dir, never the user's own.
        let config_path = dir.path().join("config.yaml");
        let opts = ServeOptions::default();
        let services = Services::new(config, Some(config_path), &opts).unwrap();
        for (name, role) in [
            ("reader", Role::Registered),
            ("uploader", Role::Uploader),
            ("boss", Role::Admin),
            ("ocr", Role::Processor),
        ] {
            services
                .db
                .create_user(name, "pass1234", role, UserStatus::Active, "")
                .unwrap();
        }
        let app = assemble(&services, &opts);
        Env {
            dir,
            storage,
            services,
            app,
        }
    }

    fn request(
        &self,
        user: Option<&str>,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        ip: &str,
    ) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost:8080");
        if let Some(u) = user {
            b = b.header("authorization", basic(u));
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        if !body.is_empty() {
            b = b.header("content-length", body.len().to_string());
        }
        let mut req = b.body(Body::from(body.to_vec())).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(ip.parse().unwrap(), 40000)));
        req
    }

    async fn send(&self, req: Request<Body>) -> Response<Body> {
        self.app.clone().oneshot(req).await.unwrap()
    }

    async fn call(
        &self,
        user: Option<&str>,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        ip: &str,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let resp = self
            .send(self.request(user, method, path, headers, body, ip))
            .await;
        let status = resp.status();
        let (parts, body) = resp.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes().to_vec();
        (status, parts.headers, bytes)
    }

    async fn admin(&self, path: &str) -> Value {
        let (status, _, body) = self.call(Some("boss"), "GET", path, &[], b"", IP_A).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path}: {}",
            String::from_utf8_lossy(&body)
        );
        serde_json::from_slice(&body).unwrap()
    }

    async fn flush(&self) {
        self.services.analytics.flush_now().await.unwrap();
    }

    fn sql(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.storage.join("analytics.sqlite")).unwrap()
    }

    fn int(&self, sql: &str) -> i64 {
        self.sql()
            .query_row(sql, [], |r| r.get::<_, Option<i64>>(0))
            .unwrap()
            .unwrap_or(0)
    }

    fn text_rows(&self, sql: &str) -> Vec<String> {
        let c = self.sql();
        let mut s = c.prepare(sql).unwrap();
        s.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }
}

/// `(method, path, headers, body)`.
type Step<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)], &'a [u8]);
/// `(user, method, path, headers)`.
type Call<'a> = (Option<&'a str>, &'a str, &'a str, &'a [(&'a str, &'a str)]);

/// `(requests, bytes_out, bytes_in)` of the traffic rows matching `filter`.
fn traffic(env: &Env, filter: &str) -> (i64, i64, i64) {
    env.sql()
        .query_row(
            &format!(
                "SELECT COALESCE(SUM(requests),0), COALESCE(SUM(bytes_out),0), COALESCE(SUM(bytes_in),0) \
                 FROM traffic_hourly WHERE {filter}"
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

#[tokio::test]
async fn a_full_get_is_one_download_of_every_byte() {
    let env = Env::new();
    let (status, _, body) = env.call(None, "GET", VOL, &[], b"", IP_A).await;
    assert_eq!((status, body.len()), (StatusCode::OK, SIZE));
    env.flush().await;
    let (n, out, _) = traffic(
        &env,
        "protocol='dav' AND method='GET' AND category='volume' AND detail='full' AND status_class=2",
    );
    assert_eq!((n, out), (1, SIZE as i64));
    let (principal, bytes, size, completed, series_key, series_title, volume_key): (
        String,
        i64,
        i64,
        i64,
        String,
        String,
        String,
    ) = env
        .sql()
        .query_row(
            "SELECT principal, bytes, size, completed, series_key, series_title, volume_key FROM volume_downloads",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )
        .unwrap();
    assert!(principal.starts_with("a:"), "{principal}");
    assert_eq!((bytes, size, completed), (SIZE as i64, SIZE as i64, 1));
    assert_eq!(
        (
            series_key.as_str(),
            series_title.as_str(),
            volume_key.as_str()
        ),
        ("series", "series", "series/vol1.cbz")
    );
    // Latency was measured for it, and a tracking start recorded.
    assert_eq!(
        env.int("SELECT COUNT(*) FROM latency_hourly WHERE category='volume'"),
        1
    );
    assert!(
        env.services
            .analytics
            .db()
            .tracking_since()
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_range_get_counts_its_bytes_and_a_resume() {
    let env = Env::new();
    let (status, _, body) = env
        .call(
            Some("reader"),
            "GET",
            VOL,
            &[("range", "bytes=100-1099")],
            b"",
            IP_A,
        )
        .await;
    assert_eq!((status, body.len()), (StatusCode::PARTIAL_CONTENT, 1000));
    env.flush().await;
    let (n, out, _) = traffic(&env, "category='volume' AND detail='range'");
    assert_eq!((n, out), (1, 1000));
    assert_eq!(env.int("SELECT SUM(resumed) FROM traffic_hourly"), 1);
    let (p, bytes, completed): (String, i64, i64) = env
        .sql()
        .query_row(
            "SELECT principal, bytes, completed FROM volume_downloads",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((p.as_str(), bytes, completed), ("u:reader", 1000, 0));
}

#[tokio::test]
async fn head_and_not_modified_send_no_bytes() {
    let env = Env::new();
    let (status, headers, body) = env.call(None, "HEAD", VOL, &[], b"", IP_A).await;
    assert_eq!((status, body.len()), (StatusCode::OK, 0));
    assert_eq!(headers["content-length"], SIZE.to_string().as_str());
    let etag = headers["etag"].to_str().unwrap().to_string();
    let (status, _, _) = env
        .call(None, "GET", VOL, &[("if-none-match", &etag)], b"", IP_A)
        .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    env.flush().await;
    let (n, out, _) = traffic(&env, "method='HEAD'");
    assert_eq!((n, out), (1, 0));
    let (n, out, _) = traffic(&env, "status_class=3");
    assert_eq!((n, out), (1, 0));
    assert_eq!(env.int("SELECT SUM(not_modified) FROM traffic_hourly"), 1);
    assert_eq!(
        env.int("SELECT SUM(saved_bytes) FROM traffic_hourly"),
        SIZE as i64
    );
    assert_eq!(
        env.int("SELECT SUM(aborted) FROM traffic_hourly"),
        0,
        "a HEAD is not cut short"
    );
    assert_eq!(
        env.int("SELECT COUNT(*) FROM volume_downloads"),
        0,
        "neither is a download"
    );
}

#[tokio::test]
async fn a_body_dropped_part_way_is_an_abort_with_the_bytes_sent() {
    let env = Env::new();
    let resp = env
        .send(env.request(None, "GET", VOL, &[], b"", IP_A))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body();
    let mut got = 0usize;
    while got < 70_000 {
        let frame = body.frame().await.unwrap().unwrap();
        got += frame.data_ref().map_or(0, |d| d.len());
    }
    assert!(got < SIZE);
    drop(body);
    env.flush().await;
    let (aborted, aborted_bytes, out): (i64, i64, i64) = env
        .sql()
        .query_row(
            "SELECT SUM(aborted), SUM(aborted_bytes), SUM(bytes_out) FROM traffic_hourly",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((aborted, aborted_bytes, out), (1, got as i64, got as i64));
    let (bytes, completed): (i64, i64) = env
        .sql()
        .query_row("SELECT bytes, completed FROM volume_downloads", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((bytes, completed), (got as i64, 0));
}

#[tokio::test]
async fn propfind_listings_are_metadata_by_depth() {
    let env = Env::new();
    for (path, depth) in [
        ("/mokuro-reader/series", "1"),
        ("/mokuro-reader/series/vol1.cbz", "0"),
    ] {
        let (status, _, _) = env
            .call(None, "PROPFIND", path, &[("depth", depth)], b"", IP_A)
            .await;
        assert_eq!(status, StatusCode::MULTI_STATUS);
    }
    let (status, headers, gz) = env
        .call(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader/",
            &[("depth", "infinity"), ("accept-encoding", "gzip")],
            b"",
            IP_A,
        )
        .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert_eq!(headers["content-encoding"], "gzip");
    env.flush().await;
    let details = env.text_rows(
        "SELECT detail FROM traffic_hourly WHERE protocol='dav' AND method='PROPFIND' AND category='metadata' ORDER BY detail",
    );
    assert_eq!(details, ["propfind_0", "propfind_1", "propfind_inf"]);
    let (raw, sent): (i64, i64) = env
        .sql()
        .query_row(
            "SELECT gzip_raw_bytes, gzip_sent_bytes FROM traffic_hourly WHERE detail='propfind_inf'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(sent, gz.len() as i64);
    assert!(raw > sent, "{raw} > {sent}");
    // The series folder's listing is attributed to the series (a crawl signal).
    let (_, out1, _) = traffic(&env, "detail='propfind_1'");
    assert!(out1 > 0);
}

#[tokio::test]
async fn a_positions_upload_counts_its_bytes_in_and_reaches_the_reading_worker() {
    let env = Env::new();
    let body = br#"{"version":1,"positions":{"uuid-1":[1762000000000,12,3400,"dev-1"]}}"#;
    let (status, _, _) = env
        .call(
            Some("reader"),
            "PUT",
            "/mokuro-reader/positions.json",
            &[],
            body,
            IP_A,
        )
        .await;
    assert!(status.is_success(), "{status}");
    env.flush().await;
    let (n, _, bytes_in) = traffic(
        &env,
        "protocol='dav' AND method='PUT' AND category='progress_sync' AND detail='positions'",
    );
    assert_eq!((n, bytes_in), (1, body.len() as i64));
    let mut rx = env.services.analytics.reading().take_receiver().unwrap();
    let ev = rx.try_recv().unwrap();
    assert_eq!(
        (ev.username.as_str(), ev.rel.as_str(), ev.method, ev.target),
        (
            "reader",
            "positions.json",
            "PUT",
            bunko_dav::DavTarget::Progress
        )
    );
    assert!(rx.try_recv().is_err(), "one event per write");
    // A refused write (anonymous) is no event.
    let (status, _, _) = env
        .call(
            None,
            "PUT",
            "/mokuro-reader/positions.json",
            &[],
            body,
            IP_A,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    env.flush().await;
    assert!(rx.try_recv().is_err());
    assert_eq!(env.services.analytics.reading().queued(), 1);
}

#[tokio::test]
async fn an_anonymous_principal_is_a_hash_and_its_ip_lives_only_in_ip_rows() {
    let env = Env::new();
    let (status, _, _) = env.call(None, "GET", VOL, &[], b"", IP_B).await;
    assert_eq!(status, StatusCode::OK);
    env.flush().await;
    let salt = env.services.analytics.db().salt();
    let want = bunko_server::analytics::principal::anon_id(&salt, IP_B);
    let principals = env.text_rows("SELECT DISTINCT principal FROM principal_daily");
    assert_eq!(principals, std::slice::from_ref(&want));
    assert_eq!(want.len(), 14);
    // Every text cell of every table that holds the raw address.
    let c = env.sql();
    let tables: Vec<String> = {
        let mut s = c
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap();
        s.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    let mut holding = Vec::new();
    for t in &tables {
        let cols: Vec<String> = {
            let mut s = c.prepare(&format!("PRAGMA table_info({t})")).unwrap();
            s.query_map([], |r| r.get(1))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        for col in cols {
            let n: i64 = c
                .query_row(
                    &format!("SELECT COUNT(*) FROM {t} WHERE CAST({col} AS TEXT) LIKE ?1"),
                    [format!("%{IP_B}%")],
                    |r| r.get(0),
                )
                .unwrap();
            if n > 0 {
                holding.push(t.clone());
            }
        }
    }
    holding.sort();
    holding.dedup();
    assert_eq!(holding, ["anon_ips", "ip_daily", "slow_requests"]);
    let (p, ip): (String, String) = c
        .query_row("SELECT principal, ip FROM anon_ips", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((p, ip.as_str()), (want, IP_B));
}

#[tokio::test]
async fn processors_are_not_downloads_but_are_bandwidth() {
    let env = Env::new();
    let (status, _, body) = env.call(Some("ocr"), "GET", VOL, &[], b"", IP_A).await;
    assert_eq!((status, body.len()), (StatusCode::OK, SIZE));
    env.flush().await;
    assert_eq!(env.int("SELECT COUNT(*) FROM volume_downloads"), 0);
    let (n, out, _) = traffic(&env, "protocol='processor' AND category='processor'");
    assert_eq!((n, out), (1, SIZE as i64));
    assert_eq!(
        env.text_rows("SELECT principal FROM principal_daily"),
        ["p:ocr"]
    );
}

/// A one-page archive, as a reader uploads one.
fn tiny_cbz() -> Vec<u8> {
    use std::io::Write;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("000.jpg", opts).unwrap();
        zw.write_all(b"fake image bytes").unwrap();
        zw.finish().unwrap();
    }
    buf.into_inner()
}

#[tokio::test]
async fn webdav_methods_are_each_counted() {
    let env = Env::new();
    let cbz = &tiny_cbz();
    let steps: &[Step] = &[
        ("OPTIONS", "/mokuro-reader/", &[], b""),
        ("MKCOL", "/mokuro-reader/newseries", &[], b""),
        ("PUT", "/mokuro-reader/newseries/v1.cbz", &[], cbz),
        ("PROPPATCH", "/mokuro-reader/newseries/v1.cbz", &[], b""),
        (
            "COPY",
            "/mokuro-reader/newseries/v1.cbz",
            &[("destination", "/mokuro-reader/newseries/v2.cbz")],
            b"",
        ),
        (
            "MOVE",
            "/mokuro-reader/newseries/v2.cbz",
            &[("destination", "/mokuro-reader/newseries/v3.cbz")],
            b"",
        ),
        ("LOCK", "/mokuro-reader/newseries/v3.cbz", &[], b""),
        (
            "UNLOCK",
            "/mokuro-reader/newseries/v3.cbz",
            &[("lock-token", "<opaquelocktoken:nope>")],
            b"",
        ),
        ("DELETE", "/mokuro-reader/newseries/v3.cbz", &[], b""),
        (
            "PROPFIND",
            "/mokuro-reader/newseries",
            &[("depth", "1")],
            b"",
        ),
        ("GET", "/mokuro-reader/newseries/v1.cbz", &[], b""),
        ("HEAD", "/mokuro-reader/newseries/v1.cbz", &[], b""),
    ];
    for (method, path, headers, body) in steps {
        env.call(Some("uploader"), method, path, headers, body, IP_A)
            .await;
    }
    env.flush().await;
    let methods = env.text_rows(
        "SELECT DISTINCT method FROM traffic_hourly WHERE protocol='dav' ORDER BY method",
    );
    for m in [
        "COPY",
        "DELETE",
        "GET",
        "HEAD",
        "LOCK",
        "MKCOL",
        "MOVE",
        "OPTIONS",
        "PROPFIND",
        "PROPPATCH",
        "PUT",
        "UNLOCK",
    ] {
        assert!(
            methods.iter().any(|x| x == m),
            "{m} missing from {methods:?}"
        );
    }
    assert_eq!(
        env.text_rows("SELECT detail FROM traffic_hourly WHERE method='MKCOL'"),
        ["folder"]
    );
    let (n, _, bytes_in) = traffic(
        &env,
        "method='PUT' AND category='upload' AND detail='archive'",
    );
    assert_eq!((n, bytes_in), (1, cbz.len() as i64));
    let (volumes, bytes): (i64, i64) = env
        .sql()
        .query_row(
            "SELECT volumes, bytes FROM uploads_daily WHERE username='uploader'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((volumes, bytes), (1, cbz.len() as i64));
    // The admin API breaks WebDAV down by method.
    let t = env
        .admin("/_admin/api/analytics/traffic?range=24h&by=method")
        .await;
    let dav: Vec<&str> = t["dav_methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["method"].as_str().unwrap())
        .collect();
    assert!(
        dav.contains(&"PROPPATCH") && dav.contains(&"MOVE"),
        "{dav:?}"
    );
    assert_eq!(dav[0], "GET", "listed in method order");
}

/// A refused credential may name an account of any length (a Basic header can carry
/// hundreds of KB): only its first 64 characters are kept, in memory and on disk.
#[tokio::test]
async fn over_long_attempted_usernames_are_cut() {
    let env = Env::new();
    let long_dav = "d".repeat(30_000);
    let mut req = env.request(
        None,
        "PROPFIND",
        "/mokuro-reader/",
        &[("depth", "0")],
        b"",
        IP_B,
    );
    req.headers_mut().insert(
        "authorization",
        {
            use base64::Engine;
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{long_dav}:wrong"))
            )
        }
        .parse()
        .unwrap(),
    );
    let resp = env.send(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    drop(resp);
    let long_login = "\u{e9}".repeat(1_000);
    let (status, _, _) = env
        .call(
            None,
            "POST",
            "/login/api/check",
            &[("content-type", "application/json")],
            format!(r#"{{"username": "{long_login}", "password": "nope"}}"#).as_bytes(),
            IP_B,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    env.flush().await;
    let rows =
        env.text_rows("SELECT attempted_username FROM auth_failures ORDER BY attempted_username");
    assert_eq!(rows, ["d".repeat(64), "\u{e9}".repeat(64)]);
}

#[tokio::test]
async fn failed_sign_ins_are_counted_per_ip_and_username() {
    let env = Env::new();
    let mut req = env.request(None, "GET", VOL, &[], b"", IP_B);
    req.headers_mut().insert(
        "authorization",
        {
            use base64::Engine;
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode("mallory:wrong")
            )
        }
        .parse()
        .unwrap(),
    );
    let resp = env.send(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    drop(resp);
    let (status, _, _) = env
        .call(
            None,
            "POST",
            "/login/api/token",
            &[("content-type", "application/json")],
            br#"{"username": "reader", "password": "nope"}"#,
            IP_B,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    env.flush().await;
    let rows = env.text_rows(
        "SELECT ip || ' ' || attempted_username || ' ' || count FROM auth_failures ORDER BY attempted_username",
    );
    assert_eq!(
        rows,
        [format!("{IP_B} mallory 1"), format!("{IP_B} reader 1")]
    );
    assert_eq!(env.int("SELECT auth_failures FROM ip_daily"), 2);
    let h = env.admin("/_admin/api/analytics/health?range=24h").await;
    assert_eq!(h["auth_failures"]["top_ips"][0]["ip"], IP_B);
    assert_eq!(h["auth_failures"]["top_ips"][0]["count"], 2);
    assert_eq!(
        h["auth_failures"]["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .sum::<u64>(),
        2
    );
}

#[tokio::test]
async fn early_answers_are_counted_too() {
    let env = Env::with(|c| c.cors.allowed_origins = vec!["https://reader.example.com".into()]);
    // path_guard (outside everything else) and a CORS preflight.
    let (status, _, _) = env
        .call(None, "GET", "/mokuro-reader/%2e%2e/x", &[], b"", IP_A)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = env
        .call(
            None,
            "OPTIONS",
            "/mokuro-reader/",
            &[
                ("origin", "https://reader.example.com"),
                ("access-control-request-method", "PROPFIND"),
            ],
            b"",
            IP_A,
        )
        .await;
    assert!(status.is_success(), "{status}");
    env.flush().await;
    assert_eq!(traffic(&env, "status_class=4").0, 1);
    assert_eq!(
        traffic(
            &env,
            "method='OPTIONS' AND category='metadata' AND detail='options'"
        )
        .0,
        1
    );
    assert_eq!(
        env.text_rows("SELECT origin FROM origins_daily"),
        ["https://reader.example.com"]
    );
}

/// A CORS preflight a signed-in hosted reader's browser sends before its requests
/// carries no credentials: it is traffic (requests, bytes, the `options` detail, the
/// address's row) but nobody's, so it makes no anonymous visitor of the reader's IP.
#[tokio::test]
async fn a_preflight_is_traffic_but_no_visitor() {
    let env = Env::with(|c| c.cors.allowed_origins = vec!["https://reader.example.com".into()]);
    let ua = "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";
    let origin = ("origin", "https://reader.example.com");
    for _ in 0..3 {
        let (status, _, _) = env
            .call(
                None,
                "OPTIONS",
                "/mokuro-reader/",
                &[
                    origin,
                    ("user-agent", ua),
                    ("access-control-request-method", "PROPFIND"),
                    ("access-control-request-headers", "authorization,depth"),
                ],
                b"",
                IP_B,
            )
            .await;
        assert!(status.is_success(), "{status}");
        let (status, _, _) = env
            .call(
                Some("reader"),
                "PROPFIND",
                "/mokuro-reader/",
                &[origin, ("user-agent", ua), ("depth", "1")],
                b"",
                IP_B,
            )
            .await;
        assert_eq!(status.as_u16(), 207);
    }
    env.flush().await;
    // Traffic as before.
    assert_eq!(
        traffic(
            &env,
            "method='OPTIONS' AND category='metadata' AND detail='options'"
        )
        .0,
        3
    );
    assert_eq!(
        env.int(&format!(
            "SELECT requests FROM ip_daily WHERE ip = '{IP_B}'"
        )),
        6
    );
    // Only the account, wherever principals are kept.
    assert_eq!(
        env.text_rows(&format!(
            "SELECT principals FROM ip_daily WHERE ip = '{IP_B}'"
        )),
        [r#"["u:reader"]"#]
    );
    assert_eq!(
        env.text_rows("SELECT DISTINCT principal FROM principal_daily ORDER BY 1"),
        ["u:reader"]
    );
    assert_eq!(
        env.text_rows("SELECT principal FROM principal_seen ORDER BY 1"),
        ["u:reader"]
    );
    assert_eq!(env.int("SELECT COUNT(*) FROM anon_ips"), 0);
    assert_eq!(
        env.text_rows("SELECT DISTINCT principal FROM client_principals ORDER BY 1"),
        ["u:reader"]
    );
    assert_eq!(
        env.text_rows("SELECT DISTINCT principal FROM client_mix_principals ORDER BY 1"),
        ["u:reader"]
    );
    let o = env.admin("/_admin/api/analytics/overview?range=24h").await;
    assert_eq!(o["kpis"]["dau"], 1, "{}", o["kpis"]);
    let u = env.admin("/_admin/api/analytics/users?range=24h").await;
    // Visitors are accounts and anonymous ids together: the reader alone.
    assert_eq!(u["visitors"]["dau"], 1, "{u}");
}

/// A sign-in carries its password in the body, so it resolves no identity of its own:
/// a successful one counts for the account that signed in, never for an anonymous
/// visitor at its IP (a failed one stays anonymous).
#[tokio::test]
async fn a_successful_sign_in_counts_for_its_account() {
    let env = Env::new();
    for path in ["/login/api/token", "/login/api/check"] {
        let (status, _, _) = env
            .call(
                None,
                "POST",
                path,
                &[("content-type", "application/json")],
                br#"{"username": "reader", "password": "pass1234"}"#,
                IP_B,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
    env.flush().await;
    assert_eq!(
        env.text_rows("SELECT DISTINCT principal FROM principal_daily ORDER BY 1"),
        ["u:reader"]
    );
    assert_eq!(
        env.text_rows("SELECT principal FROM principal_seen ORDER BY 1"),
        ["u:reader"]
    );
    assert_eq!(
        env.int("SELECT requests FROM principal_daily WHERE principal = 'u:reader' AND category = 'auth'"),
        2
    );
    assert_eq!(env.int("SELECT COUNT(*) FROM anon_ips"), 0);
    assert_eq!(env.int("SELECT COUNT(*) FROM auth_failures"), 0);
    assert_eq!(
        env.text_rows(&format!(
            "SELECT principals FROM ip_daily WHERE ip = '{IP_B}'"
        )),
        [r#"["u:reader"]"#]
    );
}

/// The same requests with analytics on and off answer byte for byte the same, and off
/// writes nothing.
#[tokio::test]
async fn disabled_analytics_change_nothing_and_write_nothing() {
    let env = Env::new();
    let requests: &[Call] = &[
        (None, "GET", VOL, &[]),
        (None, "GET", VOL, &[("range", "bytes=5-9")]),
        (None, "HEAD", VOL, &[]),
        (None, "PROPFIND", "/mokuro-reader/series", &[("depth", "1")]),
        (
            None,
            "PROPFIND",
            "/mokuro-reader/",
            &[("accept-encoding", "gzip")],
        ),
        (Some("reader"), "GET", "/login/api/me", &[]),
        (None, "GET", "/robots.txt", &[]),
        (None, "GET", "/mokuro-reader/%2e%2e/x", &[]),
        (None, "OPTIONS", "/mokuro-reader/", &[]),
    ];
    let mut on = Vec::new();
    for (user, method, path, headers) in requests {
        on.push(env.call(*user, method, path, headers, b"", IP_A).await);
    }
    env.flush().await;
    let rows = env.int("SELECT SUM(requests) FROM traffic_hourly");
    assert_eq!(rows, requests.len() as i64);
    env.services.core.config.write().analytics.enabled = false;
    env.services.analytics.settings_changed();
    assert!(!env.services.analytics.enabled());
    for ((user, method, path, headers), want) in requests.iter().zip(&on) {
        let got = env.call(*user, method, path, headers, b"", IP_A).await;
        assert_eq!(got.0, want.0, "{method} {path}");
        assert_eq!(got.1, want.1, "{method} {path}");
        assert_eq!(got.2, want.2, "{method} {path}");
    }
    env.flush().await;
    assert_eq!(env.int("SELECT SUM(requests) FROM traffic_hourly"), rows);

    // Off from the start: nothing at all is written.
    let off = Env::with(|c| c.analytics.enabled = false);
    for (user, method, path, headers) in requests {
        off.call(*user, method, path, headers, b"", IP_A).await;
    }
    off.flush().await;
    for t in [
        "traffic_hourly",
        "principal_daily",
        "ip_daily",
        "volume_downloads",
        "slow_requests",
    ] {
        assert_eq!(off.int(&format!("SELECT COUNT(*) FROM {t}")), 0, "{t}");
    }
    assert!(
        off.services
            .analytics
            .db()
            .tracking_since()
            .unwrap()
            .is_none()
    );
}

fn has_keys(v: &Value, keys: &[&str], what: &str) {
    for k in keys {
        assert!(v.get(*k).is_some(), "{what}: no `{k}` in {v}");
    }
}

fn common(v: &Value, key: &str, granularity: &str) {
    assert_eq!(v["range"]["key"], key, "{v}");
    assert_eq!(v["range"]["granularity"], granularity);
    assert!(v["range"]["from"].as_i64().unwrap() <= v["range"]["to"].as_i64().unwrap());
    assert!(v["tracking_since"].is_i64() || v["tracking_since"].is_null());
    assert_eq!(v["enabled"], true);
}

fn timeseries(v: &Value, what: &str) {
    let t = v["t"]
        .as_array()
        .unwrap_or_else(|| panic!("{what}: no t in {v}"));
    for s in v["series"].as_array().unwrap() {
        has_keys(s, &["key", "label", "values"], what);
        assert_eq!(s["values"].as_array().unwrap().len(), t.len(), "{what}");
    }
}

#[tokio::test]
async fn the_admin_api_answers_the_design_shapes() {
    let env = Env::new();
    env.call(Some("reader"), "GET", VOL, &[], b"", IP_A).await;
    env.call(None, "GET", VOL, &[], b"", IP_B).await;
    env.call(
        None,
        "PROPFIND",
        "/mokuro-reader/series",
        &[("depth", "1")],
        b"",
        IP_B,
    )
    .await;
    env.call(
        Some("ocr"),
        "GET",
        "/mokuro-reader/series/small.cbz",
        &[],
        b"",
        IP_A,
    )
    .await;
    env.call(
        Some("reader"),
        "GET",
        "/mokuro-reader/missing.cbz",
        &[],
        b"",
        IP_A,
    )
    .await;
    env.flush().await;

    let o = env.admin("/_admin/api/analytics/overview?range=24h").await;
    common(&o, "24h", "hour");
    has_keys(
        &o["kpis"],
        &[
            "bytes_out",
            "bytes_in",
            "est_bytes_out",
            "requests",
            "downloads",
            "downloaders",
            "reads",
            "readers",
            "minutes_read",
            "pages_read",
            "dau",
            "wau",
            "mau",
            "crawl_episodes",
            "errors_5xx",
            "auth_failures",
            "rss_bytes",
            "rss_peak_bytes",
            "cpu_pct",
            "uptime_s",
            "dropped",
        ],
        "overview.kpis",
    );
    assert_eq!(o["kpis"]["downloads"], 2);
    assert_eq!(o["kpis"]["downloaders"], 2);
    assert_eq!(
        o["kpis"]["dau"], 2,
        "the account and the anonymous visitor, not the processor"
    );
    assert!(o["kpis"]["bytes_out"].as_u64().unwrap() >= 2 * SIZE as u64);
    timeseries(&o["bandwidth"], "overview.bandwidth");
    assert_eq!(o["bandwidth"]["t"].as_array().unwrap().len(), 24);
    let ts = &o["top_series"][0];
    has_keys(
        ts,
        &["series_key", "title", "downloads", "readers", "minutes"],
        "top_series",
    );
    assert_eq!(
        (ts["series_key"].as_str(), ts["downloads"].as_u64()),
        (Some("series"), Some(2))
    );
    let tp = &o["top_principals"][0];
    has_keys(
        tp,
        &["principal", "label", "kind", "bytes_out"],
        "top_principals",
    );

    let t = env
        .admin("/_admin/api/analytics/traffic?range=7d&by=protocol&metric=requests")
        .await;
    common(&t, "7d", "hour");
    timeseries(&t, "traffic");
    for row in t["totals"].as_array().unwrap() {
        has_keys(
            row,
            &[
                "key",
                "label",
                "requests",
                "bytes_out",
                "bytes_in",
                "est_bytes_out",
            ],
            "traffic.totals",
        );
    }
    assert!(
        t["totals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["key"] == "dav" && r["label"] == "WebDAV")
    );
    for s in ["progress", "profile"] {
        has_keys(
            &t["sync"][s],
            &["bytes_out", "bytes_in", "requests", "by_detail"],
            "traffic.sync",
        );
    }
    has_keys(
        &t["metadata"],
        &["bytes_out", "bytes_in", "requests", "by_detail"],
        "traffic.metadata",
    );
    let md = &t["metadata"]["by_detail"][0];
    has_keys(
        md,
        &["key", "label", "requests", "bytes_out"],
        "metadata.by_detail",
    );
    assert_eq!(md["key"], "propfind_1");
    let m = &t["dav_methods"][0];
    has_keys(
        m,
        &["method", "requests", "bytes_out", "bytes_in"],
        "dav_methods",
    );
    let bad = env
        .call(
            Some("boss"),
            "GET",
            "/_admin/api/analytics/traffic?by=nope",
            &[],
            b"",
            IP_A,
        )
        .await;
    assert_eq!(bad.0, StatusCode::BAD_REQUEST);
    let bad = env
        .call(
            Some("boss"),
            "GET",
            "/_admin/api/analytics/overview?range=1y",
            &[],
            b"",
            IP_A,
        )
        .await;
    assert_eq!(bad.0, StatusCode::BAD_REQUEST);

    let p = env.admin("/_admin/api/analytics/principals").await;
    common(&p, "30d", "day");
    let rows = p["rows"].as_array().unwrap();
    for r in rows {
        has_keys(
            r,
            &[
                "principal",
                "label",
                "kind",
                "requests",
                "bytes_out",
                "bytes_in",
                "by_category",
                "last_seen",
                "ips",
            ],
            "principals.rows",
        );
    }
    // The anonymous visitors are one row by default; `anon=only` lists each.
    assert_eq!(p["anon"], "group");
    let group = rows.iter().find(|r| r["kind"] == "anon_group").unwrap();
    assert_eq!(
        (&group["principal"], &group["visitors"]),
        (&json!("a:*"), &json!(1))
    );
    assert!(group["by_category"]["volume"].as_u64().unwrap() >= SIZE as u64);
    assert!(rows.iter().all(|r| r["kind"] != "anon"));
    let each = env
        .admin("/_admin/api/analytics/principals?anon=only")
        .await;
    let anon = &each["rows"][0];
    assert_eq!(anon["kind"], "anon");
    assert!(anon["ips"].as_array().unwrap().iter().any(|i| i == IP_B));
    assert!(anon["by_category"]["volume"].as_u64().unwrap() >= SIZE as u64);
    assert!(
        rows.iter()
            .any(|r| r["kind"] == "processor" && r["label"] == "ocr")
    );
    assert_eq!(
        p["totals"],
        json!({"users": 1, "anon": 1, "processor": 1}),
        "{}",
        p["totals"]
    );

    let s = env
        .admin("/_admin/api/analytics/series?sort=downloads&limit=10")
        .await;
    common(&s, "30d", "day");
    let row = &s["rows"][0];
    has_keys(
        row,
        &[
            "series_key",
            "title",
            "downloads",
            "downloaders",
            "readers",
            "reads",
            "pages",
            "minutes",
            "bytes_out",
            "prev",
            "volumes",
        ],
        "series.rows",
    );
    has_keys(&row["prev"], &["downloads", "readers"], "series.prev");
    assert_eq!(row["downloads"], 2);
    assert_eq!(row["bytes_out"], 2 * SIZE as u64);
    let none = env
        .admin("/_admin/api/analytics/series?q=nothing-like-it")
        .await;
    assert!(none["rows"].as_array().unwrap().is_empty());

    let h = env.admin("/_admin/api/analytics/health?range=24h").await;
    common(&h, "24h", "hour");
    timeseries(&h["status"], "health.status");
    let keys: Vec<&str> = h["status"]["series"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["2xx", "3xx", "4xx", "5xx"]);
    let e = &h["errors"][0];
    has_keys(
        e,
        &["status", "protocol", "method", "category", "count"],
        "health.errors",
    );
    assert_eq!(
        (e["status"].as_u64(), e["protocol"].as_str()),
        (Some(404), Some("dav"))
    );
    has_keys(
        &h["auth_failures"],
        &["t", "values", "top_ips", "top_usernames"],
        "health.auth_failures",
    );
    let l = &h["latency"][0];
    has_keys(
        l,
        &[
            "protocol",
            "method",
            "category",
            "requests",
            "p50_ms",
            "p95_ms",
            "p99_ms",
            "ttfb_p50_ms",
            "ttfb_p95_ms",
        ],
        "health.latency",
    );
    let slow = &h["slow"][0];
    has_keys(
        slow,
        &["at", "method", "path", "status", "ms", "principal"],
        "health.slow",
    );
    assert!(slow.get("ip").is_none(), "no raw IP in the slow list");
    has_keys(
        &h["efficiency"],
        &[
            "not_modified",
            "saved_bytes",
            "gzip_raw_bytes",
            "gzip_sent_bytes",
            "aborted",
            "aborted_bytes",
            "resumed",
            "est_bytes_out",
        ],
        "health.efficiency",
    );

    // Admins only, like the rest of the panel.
    let (status, _, _) = env
        .call(
            Some("reader"),
            "GET",
            "/_admin/api/analytics/overview",
            &[],
            b"",
            IP_A,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = env
        .call(
            Some("boss"),
            "GET",
            "/_admin/api/analytics/nope",
            &[],
            b"",
            IP_A,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_settings_editor_validates_saves_and_applies() {
    let env = Env::new();
    let s = env.admin("/_admin/api/settings").await;
    assert_eq!(s["analytics"]["enabled"], true);
    assert_eq!(s["analytics"]["retention"]["ip_days"], 30);
    let put = |body: Value| {
        let env = &env;
        async move {
            env.call(
                Some("boss"),
                "PUT",
                "/_admin/api/settings/analytics",
                &[("content-type", "application/json")],
                body.to_string().as_bytes(),
                IP_A,
            )
            .await
        }
    };
    let (status, _, body) = put(
        json!({"retention": {"ip_days": 14}, "crawl": {"min_coverage": 0.5}, "flush_seconds": 30}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["success"], true);
    assert_eq!(v["analytics"]["retention"]["ip_days"], 14);
    assert_eq!(v["analytics"]["crawl"]["min_coverage"], 0.5);
    let saved = std::fs::read_to_string(env.dir.path().join("config.yaml")).unwrap();
    assert!(saved.contains("ip_days: 14"), "{saved}");
    // A bad value refuses the whole change.
    let (status, _, body) =
        put(json!({"flush_seconds": 10, "retention": {"hourly_days": 0}})).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(env.services.core.config.read().analytics.flush_seconds, 30);
    // Turning it off applies at once: the next request is not counted.
    let (status, _, _) = put(json!({"enabled": false})).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!env.services.analytics.enabled());
    env.flush().await;
    let before = env.int("SELECT COALESCE(SUM(requests), 0) FROM traffic_hourly");
    env.call(None, "GET", VOL, &[], b"", IP_A).await;
    env.flush().await;
    assert_eq!(
        env.int("SELECT COALESCE(SUM(requests), 0) FROM traffic_hourly"),
        before
    );
    // Stored data stays viewable.
    let o = env.admin("/_admin/api/analytics/overview").await;
    assert_eq!(o["enabled"], false);
    // CSRF: a cross-origin write is refused, as for every other editor.
    let (status, _, _) = env
        .call(
            Some("boss"),
            "PUT",
            "/_admin/api/settings/analytics",
            &[
                ("content-type", "application/json"),
                ("origin", "https://evil.example.net"),
            ],
            br#"{"enabled": true}"#,
            IP_A,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!env.services.core.config.read().analytics.enabled);
}

#[tokio::test]
async fn upkeep_rolls_days_up_and_prunes_by_retention() {
    let env = Env::new();
    env.call(None, "GET", VOL, &[], b"", IP_B).await;
    env.flush().await;
    let now = bunko_server::analytics::now_s();
    // Two days on, the request's day is complete: rolled up, nothing pruned yet.
    let stats = env
        .services
        .analytics
        .maintain_at(now + 2 * 86400)
        .await
        .unwrap();
    assert!(stats.days_rolled >= 1, "{stats:?}");
    assert_eq!(env.int("SELECT SUM(requests) FROM traffic_daily"), 1);
    assert_eq!(env.int("SELECT COUNT(*) FROM traffic_hourly"), 1);
    assert_eq!(env.int("SELECT COUNT(*) FROM ip_daily"), 1);
    // 40 days on: the IP rows (30 days) go, the hourly rows (90) stay.
    env.services
        .analytics
        .maintain_at(now + 40 * 86400)
        .await
        .unwrap();
    for t in ["ip_daily", "anon_ips", "slow_requests"] {
        assert_eq!(env.int(&format!("SELECT COUNT(*) FROM {t}")), 0, "{t}");
    }
    assert_eq!(env.int("SELECT COUNT(*) FROM traffic_hourly"), 1);
    // 100 days on: hourly rows go too; daily rows, principals and downloads stay.
    env.services
        .analytics
        .maintain_at(now + 100 * 86400)
        .await
        .unwrap();
    assert_eq!(env.int("SELECT COUNT(*) FROM traffic_hourly"), 0);
    assert_eq!(env.int("SELECT COUNT(*) FROM latency_hourly"), 0);
    assert_eq!(env.int("SELECT SUM(requests) FROM traffic_daily"), 1);
    assert_eq!(env.int("SELECT COUNT(*) FROM latency_daily"), 1);
    assert_eq!(env.int("SELECT COUNT(*) FROM principal_daily"), 1);
    assert_eq!(env.int("SELECT COUNT(*) FROM volume_downloads"), 1);
    // The daily rows answer a daily range as the hourly ones did.
    let o = env.admin("/_admin/api/analytics/overview?range=30d").await;
    assert!(o["kpis"]["requests"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn catalog_covers_are_counted_with_their_304_savings() {
    let env = Env::with(|c| c.catalog.enabled = true);
    let webp = b"RIFF\x10\0\0\0WEBPVP8 fake cover bytes";
    std::fs::write(env.storage.join("library/series/vol1.webp"), webp).unwrap();
    let path = "/catalog/api/cover?path=series/vol1.webp";
    let (status, headers, body) = env.call(None, "GET", path, &[], b"", IP_A).await;
    assert_eq!((status, body.len()), (StatusCode::OK, webp.len()));
    let etag = headers["etag"].to_str().unwrap().to_string();
    let (status, _, _) = env
        .call(None, "GET", path, &[("if-none-match", &etag)], b"", IP_A)
        .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    env.flush().await;
    let (n, out, _) = traffic(&env, "protocol='api' AND category='cover'");
    assert_eq!((n, out), (2, webp.len() as i64));
    assert_eq!(
        env.int("SELECT SUM(saved_bytes) FROM traffic_hourly"),
        webp.len() as i64
    );
}

/// `//` and `/.` segments reach the same file; the accounting keys it by its one
/// canonical path, so they can neither split a download nor make up a series.
#[tokio::test]
async fn non_canonical_paths_are_one_download_of_one_series() {
    let env = Env::new();
    for path in [
        "/mokuro-reader/series//vol1.cbz",
        "/mokuro-reader/series/./vol1.cbz",
        "/mokuro-reader/./series/vol1.cbz",
        "/mokuro-reader//series/././vol1.cbz",
        VOL,
    ] {
        let (status, _, body) = env.call(Some("reader"), "GET", path, &[], b"", IP_A).await;
        assert_eq!((status, body.len()), (StatusCode::OK, SIZE), "{path}");
    }
    env.flush().await;
    let rows: Vec<String> = env.text_rows(
        "SELECT principal || '|' || volume_key || '|' || series_key || '|' || series_title \
         FROM volume_downloads",
    );
    assert_eq!(rows, ["u:reader|series/vol1.cbz|series|series"]);
}

/// Anonymous ids: every address of one IPv6 /64 is one visitor, and a day keeps rows
/// of their own for at most its cap of anonymous visitors; the rest still count, as
/// `a:other`, with no row (nor address-to-id link) each.
#[tokio::test]
async fn anonymous_visitors_are_per_slash_64_and_capped_per_day() {
    let env = Env::new();
    env.services.analytics.set_anon_per_day(3);
    for i in 0..40 {
        let ip = format!("2001:db8:aa:1::{:x}", i + 1);
        let (status, _, _) = env.call(None, "GET", "/robots.txt", &[], b"", &ip).await;
        assert!(status.is_client_error() || status.is_success(), "{status}");
    }
    env.flush().await;
    let anon = |env: &Env| {
        env.int("SELECT COUNT(DISTINCT principal) FROM principal_daily WHERE principal LIKE 'a:%'")
    };
    assert_eq!(anon(&env), 1, "one /64, one visitor");
    // Two more visitors fit the cap of 3; the five after them are `a:other`.
    for ip in [
        "192.0.2.1",
        "192.0.2.2",
        "192.0.2.3",
        "192.0.2.4",
        "2001:db8:bb::1",
    ] {
        env.call(None, "GET", "/robots.txt", &[], b"", ip).await;
    }
    env.call(None, "GET", "/robots.txt", &[], b"", "2001:db8:cc::1")
        .await;
    env.call(None, "GET", "/robots.txt", &[], b"", "2001:db8:aa:1::99")
        .await;
    env.flush().await;
    let ids = env.text_rows(
        "SELECT DISTINCT principal FROM principal_daily WHERE principal LIKE 'a:%' ORDER BY 1",
    );
    assert_eq!(ids.len(), 4, "{ids:?}");
    assert!(ids.iter().any(|p| p == "a:other"), "{ids:?}");
    // Every request still counts.
    assert_eq!(
        env.int("SELECT SUM(requests) FROM principal_daily WHERE principal LIKE 'a:%'"),
        47
    );
    assert_eq!(
        env.int("SELECT SUM(requests) FROM principal_daily WHERE principal = 'a:other'"),
        4
    );
    // No seen row, no address link for the folded ones.
    assert_eq!(
        env.int("SELECT COUNT(*) FROM principal_seen WHERE principal LIKE 'a:%'"),
        3
    );
    assert_eq!(
        env.int("SELECT COUNT(*) FROM anon_ips WHERE principal = 'a:other'"),
        0
    );
}

/// A one-byte `Range` probe is no download: popularity counts completed downloads.
#[tokio::test]
async fn a_one_byte_range_probe_is_not_a_download() {
    let env = Env::new();
    for i in 0..20 {
        let ip = format!("2001:db8:{:x}::1", i + 1);
        let (status, _, body) = env
            .call(None, "GET", VOL, &[("range", "bytes=0-0")], b"", &ip)
            .await;
        assert_eq!((status, body.len()), (StatusCode::PARTIAL_CONTENT, 1));
    }
    env.flush().await;
    let v = env.admin("/_admin/api/analytics/overview?range=24h").await;
    assert_eq!(v["kpis"]["downloads"], 0, "{}", v["kpis"]);
    assert_eq!(v["kpis"]["downloaders"], 0);
    let top = v["top_series"].as_array().unwrap();
    assert!(
        top.is_empty(),
        "a probed series is not on the list: {top:?}"
    );
    // One whole download counts.
    env.call(Some("reader"), "GET", VOL, &[], b"", IP_A).await;
    env.flush().await;
    let v = env.admin("/_admin/api/analytics/overview?range=24h").await;
    assert_eq!(v["kpis"]["downloads"], 1);
    assert_eq!(v["top_series"][0]["downloads"], 1);
    let s = env.admin("/_admin/api/analytics/series?range=24h").await;
    assert_eq!(s["rows"][0]["downloads"], 1, "{}", s["rows"]);
    assert_eq!(s["rows"][0]["downloaders"], 1);
}

fn files_named(dir: &std::path::Path, prefix: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(prefix))
        .collect();
    v.sort();
    v
}

/// A healthy file that cannot be opened now (here: read-only) keeps its place and
/// its history; the run counts in memory. It used to be renamed `.broken-*` and the
/// history dropped out of view, its `-shm` left behind to break the new file too.
/// Unix only: a read-only mode bit is how the test makes the file unopenable.
#[cfg(unix)]
#[tokio::test]
async fn a_healthy_file_that_cannot_be_opened_is_left_in_place() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::prepared(
        |_| {},
        |storage| {
            let path = storage.join("analytics.sqlite");
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch("CREATE TABLE keep (x); INSERT INTO keep VALUES (42);")
                .unwrap();
            drop(c);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        },
    );
    assert!(env.services.analytics.db().path().is_none(), "in memory");
    assert_eq!(
        files_named(&env.storage, "analytics.sqlite"),
        ["analytics.sqlite"],
        "nothing moved aside"
    );
    assert_eq!(env.int("SELECT x FROM keep"), 42);
    // It still counts, in memory.
    env.call(None, "GET", VOL, &[], b"", IP_A).await;
    env.flush().await;
    let v = env.admin("/_admin/api/analytics/overview?range=24h").await;
    assert!(v["kpis"]["requests"].as_u64().unwrap() >= 1);
}

/// A damaged file (here: a table page overwritten, which the open itself does not
/// notice: `quick_check` does) is moved aside under a name no earlier one has, and a
/// new file takes its place.
#[tokio::test]
async fn a_damaged_file_is_moved_aside_and_a_new_one_made() {
    let mut taken = String::new();
    let env = Env::prepared(
        |_| {},
        |storage| {
            let path = storage.join("analytics.sqlite");
            {
                let db = bunko_db::analytics::AnalyticsDb::open(&path).unwrap();
                db.meta_set("tracking_since", "1").unwrap();
            }
            // Overwrite the root page of traffic_hourly.
            let c = rusqlite::Connection::open(&path).unwrap();
            let root: i64 = c
                .query_row(
                    "SELECT rootpage FROM sqlite_master WHERE name = 'traffic_hourly'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let page: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap();
            drop(c);
            let mut bytes = std::fs::read(&path).unwrap();
            let at = ((root - 1) * page) as usize;
            for (i, b) in bytes[at..at + page as usize].iter_mut().enumerate() {
                *b = (i * 7 + 3) as u8;
            }
            std::fs::write(&path, bytes).unwrap();
            std::fs::write(storage.join("analytics.sqlite-shm"), b"stale").unwrap();
            // An earlier one moved aside this very second.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            for s in [now, now + 1] {
                std::fs::write(storage.join(format!("analytics.sqlite.broken-{s}")), b"old")
                    .unwrap();
            }
            taken = format!("analytics.sqlite.broken-{now}");
        },
    );
    assert!(env.services.analytics.db().path().is_some(), "a new file");
    let names = files_named(&env.storage, "analytics.sqlite");
    let moved: Vec<&String> = names
        .iter()
        .filter(|n| n.contains(".broken-") && !n.ends_with("-shm") && n.as_str() != taken)
        .filter(|n| std::fs::read(env.storage.join(n)).unwrap() != b"old")
        .collect();
    assert_eq!(moved.len(), 1, "{names:?}");
    // The stale -shm did not carry over to the new file (SQLite drops it when the
    // damaged file's connection closes; what it leaves would move aside with it).
    assert_ne!(
        std::fs::read(env.storage.join("analytics.sqlite-shm"))
            .ok()
            .as_deref(),
        Some(&b"stale"[..]),
        "{names:?}"
    );
    assert_eq!(
        std::fs::read(env.storage.join(&taken)).unwrap(),
        b"old",
        "an earlier one is never overwritten"
    );
    // The new file works.
    env.call(None, "GET", VOL, &[], b"", IP_A).await;
    env.flush().await;
    assert_eq!(
        traffic(&env, "category='volume'").0,
        1,
        "the new file records"
    );
}

/// A settings change whose save fails changes nothing: the config, the settings view
/// and what the layer does agree (it used to say off while still counting).
/// Unix only: a read-only directory (mode bits) is how the test makes the save fail;
/// Windows' read-only attribute does not stop files being created in a directory.
#[cfg(unix)]
#[tokio::test]
async fn a_settings_change_that_cannot_be_saved_changes_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let dir = env.dir.path().to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let (status, _, body) = env
        .call(
            Some("boss"),
            "PUT",
            "/_admin/api/settings/analytics",
            &[("content-type", "application/json")],
            json!({"enabled": false}).to_string().as_bytes(),
            IP_A,
        )
        .await;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let s = env.admin("/_admin/api/settings").await;
    assert_eq!(s["analytics"]["enabled"], true);
    assert!(env.services.analytics.enabled());
    let v = env.admin("/_admin/api/analytics/overview?range=24h").await;
    assert_eq!(v["enabled"], true);
}

/// A shorter `flush_seconds` applies to the wait already running: from 600 s to 5 s,
/// the next flush is 5 s after the last one, not ten minutes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_flush_interval_applies_at_once() {
    let env = Env::with(|c| c.analytics.flush_seconds = 600);
    let stop = tokio_util::sync::CancellationToken::new();
    env.services.analytics.start(stop.clone());
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    env.call(None, "GET", VOL, &[], b"", IP_A).await;
    let (status, _, body) = env
        .call(
            Some("boss"),
            "PUT",
            "/_admin/api/settings/analytics",
            &[("content-type", "application/json")],
            json!({"flush_seconds": 5}).to_string().as_bytes(),
            IP_A,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    tokio::time::sleep(std::time::Duration::from_millis(6000)).await;
    let n = traffic(&env, "category='volume'").0;
    stop.cancel();
    assert_eq!(n, 1, "flushed by the new interval");
}
