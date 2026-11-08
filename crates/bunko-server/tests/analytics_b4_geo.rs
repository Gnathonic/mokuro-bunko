//! Geography through the whole server (docs/design/analytics.md §9): the country
//! database is downloaded from a local HTTPS test server (never the real DB-IP host;
//! a self-signed certificate the download is told to trust), with
//! the previous-month fallback, size caps and verification; WebDAV and web requests
//! get a country; `GET/POST /_admin/api/analytics/geo` answer. The test database is
//! built in code (`bunko_server::analytics::geo::test_mmdb`), so no third-party file
//! is committed.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::analytics::geo::{Country, test_mmdb};
use bunko_server::app::{ServeOptions, Services, assemble};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const HOST: &str = "bunko.example.net";
const RCLONE: &str = "rclone/v1.66.0";
const FIREFOX: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0";

/// What the test download server answers for a path.
#[derive(Clone)]
enum Answer {
    Bytes(Vec<u8>),
    /// A chunked body of this many zero bytes (no Content-Length).
    Stream(usize),
    /// A `302 Found` to this URL.
    Redirect(String),
}

#[derive(Clone, Default)]
struct Files {
    paths: Arc<Mutex<HashMap<String, Answer>>>,
    hits: Arc<AtomicUsize>,
}

async fn serve_file(State(f): State<Files>, req: Request<Body>) -> http::Response<Body> {
    f.hits.fetch_add(1, Ordering::SeqCst);
    let answer = f.paths.lock().get(req.uri().path()).cloned();
    match answer {
        Some(Answer::Bytes(b)) => http::Response::new(Body::from(b)),
        Some(Answer::Stream(n)) => {
            let chunk = bytes::Bytes::from(vec![0u8; 1 << 20]);
            let chunks =
                (0..n.div_ceil(1 << 20)).map(move |_| Ok::<_, std::io::Error>(chunk.clone()));
            http::Response::new(Body::from_stream(futures_util::stream::iter(chunks)))
        }
        Some(Answer::Redirect(to)) => http::Response::builder()
            .status(StatusCode::FOUND)
            .header(http::header::LOCATION, to)
            .body(Body::empty())
            .unwrap(),
        None => http::Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap(),
    }
}

/// The test server's TLS settings: a self-signed certificate for 127.0.0.1, which
/// every download client made afterwards trusts (made once per test binary).
fn test_tls() -> Arc<rustls::ServerConfig> {
    static TLS: std::sync::OnceLock<Arc<rustls::ServerConfig>> = std::sync::OnceLock::new();
    TLS.get_or_init(|| {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        bunko_server::analytics::geo::trust_for_tests(cert.der().to_vec());
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into());
        let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)
        .unwrap();
        Arc::new(cfg)
    })
    .clone()
}

/// A download server on 127.0.0.1 (the only network the tests use), over HTTPS
/// unless `plain`.
async fn serve_files(plain: bool) -> (Files, String) {
    let files = Files::default();
    let app = Router::new().fallback(serve_file).with_state(files.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tls = (!plain).then(test_tls);
    tokio::spawn(bunko_server::serve::serve(
        listener,
        app,
        tls,
        CancellationToken::new(),
        Duration::from_secs(1),
    ));
    let scheme = if plain { "http" } else { "https" };
    (files, format!("{scheme}://{addr}"))
}

async fn file_server() -> (Files, String) {
    serve_files(false).await
}

fn ym(t: i64) -> (i32, u8) {
    let d = time::OffsetDateTime::from_unix_timestamp(t).unwrap();
    (d.year(), u8::from(d.month()))
}

/// This month and the previous one, as `YYYY-MM`.
fn months() -> (String, String) {
    let (y, m) = ym(bunko_server::analytics::now_s());
    let (py, pm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
    (format!("{y:04}-{m:02}"), format!("{py:04}-{pm:02}"))
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

fn db_v1() -> Vec<u8> {
    test_mmdb::build_gz(
        &[
            (ip("203.0.113.0"), 24, "JP"),
            (ip("198.51.100.0"), 24, "DE"),
            (ip("2001:db8::"), 32, "FR"),
        ],
        1_700_000_000,
    )
}

fn db_v2() -> Vec<u8> {
    test_mmdb::build_gz(&[(ip("203.0.113.0"), 24, "NL")], 1_760_000_000)
}

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

impl Env {
    fn new(geo_url: Option<&str>) -> Env {
        let dir = tempfile::tempdir().unwrap();
        Self::at(dir, geo_url)
    }

    fn at(dir: tempfile::TempDir, geo_url: Option<&str>) -> Env {
        Self::build(dir, geo_url, true)
    }

    fn build(dir: tempfile::TempDir, geo_url: Option<&str>, analytics_on: bool) -> Env {
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(storage.join("library/series")).unwrap();
        std::fs::write(storage.join("library/series/vol1.cbz"), vec![7u8; 2048]).unwrap();
        let mut config = Config::default();
        config.storage.base_path = storage.clone();
        config.analytics.enabled = analytics_on;
        if let Some(url) = geo_url {
            config.analytics.geo.enabled = true;
            config.analytics.geo.url = url.to_string();
        }
        let opts = ServeOptions::default();
        let services = Services::new(config, Some(dir.path().join("config.yaml")), &opts).unwrap();
        if services.db.get_user("boss").ok().flatten().is_none() {
            services
                .db
                .create_user("boss", "pass1234", Role::Admin, UserStatus::Active, "")
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

    async fn call(
        &self,
        user: Option<&str>,
        method: &str,
        path: &str,
        ua: &str,
        body: &[u8],
        ip: &str,
    ) -> (StatusCode, Vec<u8>) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("host", HOST)
            .header("user-agent", ua);
        if let Some(u) = user {
            b = b.header("authorization", basic(u));
        }
        if method == "PROPFIND" {
            b = b.header("depth", "1");
        }
        if !body.is_empty() {
            b = b
                .header("content-type", "application/json")
                .header("content-length", body.len().to_string());
        }
        let mut req = b.body(Body::from(body.to_vec())).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(ip.parse().unwrap(), 40000)));
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, bytes)
    }

    async fn admin(&self, method: &str, path: &str, body: &[u8]) -> Value {
        let (status, body) = self
            .call(Some("boss"), method, path, FIREFOX, body, "127.0.0.1")
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }

    fn country(&self, s: &str) -> Option<String> {
        self.services
            .analytics
            .geo()
            .country(&ip(s))
            .map(|c| c.as_str().to_string())
    }

    fn mmdb(&self) -> PathBuf {
        self.storage.join("analytics/country.mmdb")
    }

    fn sql_rows(&self, sql: &str) -> Vec<(String, i64)> {
        let c = rusqlite::Connection::open(self.storage.join("analytics.sqlite")).unwrap();
        let mut st = c.prepare(sql).unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }
}

#[tokio::test]
async fn previous_month_fallback_then_webdav_and_web_traffic_get_countries() {
    let (files, base) = file_server().await;
    let (cur, prev) = months();
    // Only last month's is out.
    files.paths.lock().insert(
        format!("/dbip-country-lite-{prev}.mmdb.gz"),
        Answer::Bytes(db_v1()),
    );
    let env = Env::new(Some(&format!(
        "{base}/dbip-country-lite-{{year}}-{{month}}.mmdb.gz"
    )));
    // Enabled but nothing loaded: nothing is known, the LAN included.
    assert_eq!(env.country("203.0.113.10"), None);
    assert_eq!(env.country("192.168.1.5"), None);
    let g = env.services.analytics.geo();
    g.update_now(&env.services.analytics, false).await.unwrap();
    // This month's answered 404, then last month's was fetched.
    assert_eq!(files.hits.load(Ordering::SeqCst), 2);
    let st = g.status();
    assert_eq!(st.status, "ready");
    assert_eq!(st.db.as_ref().unwrap().version, prev);
    assert!(env.mmdb().is_file());
    assert_ne!(prev, cur);
    // Not due again until tomorrow: a second pass does nothing.
    g.update_now(&env.services.analytics, false).await.unwrap();
    assert_eq!(files.hits.load(Ordering::SeqCst), 2);

    assert_eq!(env.country("203.0.113.10").as_deref(), Some("JP"));
    assert_eq!(env.country("::ffff:198.51.100.1").as_deref(), Some("DE"));
    assert_eq!(env.country("2001:db8::77").as_deref(), Some("FR"));
    assert_eq!(env.country("192.0.2.1").as_deref(), Some("ZZ"));
    assert_eq!(env.country("fd12::1").as_deref(), Some("LAN"));
    assert_eq!(env.country("100.64.3.4").as_deref(), Some("LAN"));

    // WebDAV (rclone) and web requests, each from a different place.
    env.call(
        None,
        "PROPFIND",
        "/mokuro-reader/",
        RCLONE,
        b"",
        "203.0.113.10",
    )
    .await;
    env.call(
        None,
        "GET",
        "/mokuro-reader/series/vol1.cbz",
        RCLONE,
        b"",
        "203.0.113.10",
    )
    .await;
    env.call(
        None,
        "PROPFIND",
        "/mokuro-reader/series/",
        RCLONE,
        b"",
        "198.51.100.23",
    )
    .await;
    env.call(None, "GET", "/", FIREFOX, b"", "2001:db8::5")
        .await;
    env.call(None, "GET", "/", FIREFOX, b"", "192.0.2.9").await;
    env.services.analytics.flush_now().await.unwrap();

    let geo = env.sql_rows("SELECT country, requests FROM geo_daily ORDER BY country");
    assert_eq!(
        geo,
        [
            ("DE".to_string(), 1),
            ("FR".to_string(), 1),
            ("JP".to_string(), 2),
            ("ZZ".to_string(), 1)
        ]
    );
    let ips = env.sql_rows("SELECT ip || '=' || country, requests FROM ip_daily ORDER BY ip");
    assert!(ips.contains(&("203.0.113.10=JP".to_string(), 2)), "{ips:?}");

    let v = env
        .admin("GET", "/_admin/api/analytics/geo?range=7d", b"")
        .await;
    assert_eq!(v["geo_enabled"], true);
    assert_eq!(v["enabled"], true);
    assert_eq!(v["status"], "ready");
    assert_eq!(v["db"]["version"], prev.as_str());
    assert_eq!(v["db"]["attribution"], "IP geolocation by DB-IP");
    assert_eq!(v["db"]["license"], "CC BY 4.0");
    assert_eq!(v["db"]["database_type"], "Test-Country-Lite");
    assert!(v["db"]["downloaded_at"].as_i64().is_some());
    assert_eq!(v["countries"][0]["country"], "JP");
    assert_eq!(v["countries"][0]["requests"], 2);
    assert!(v["countries"][0]["bytes_out"].as_u64().unwrap() >= 2048);
}

#[tokio::test]
async fn refresh_installs_a_new_edition_and_bad_downloads_keep_the_old_one() {
    let (files, base) = file_server().await;
    let (cur, _) = months();
    let path = format!("/db-{cur}.mmdb.gz");
    files
        .paths
        .lock()
        .insert(path.clone(), Answer::Bytes(db_v1()));
    let env = Env::new(Some(&format!("{base}/db-{{year}}-{{month}}.mmdb.gz")));
    let a = &env.services.analytics;
    // The background task: never blocks startup, downloads on its own.
    let stop = CancellationToken::new();
    a.start(stop.clone());
    wait_for(|| env.country("203.0.113.1").as_deref() == Some("JP")).await;
    assert_eq!(a.geo().status().db.unwrap().version, cur);
    let good = std::fs::read(env.mmdb()).unwrap();

    // A new edition, fetched on POST refresh.
    files
        .paths
        .lock()
        .insert(path.clone(), Answer::Bytes(db_v2()));
    let r = env
        .admin("POST", "/_admin/api/analytics/geo/refresh", b"{}")
        .await;
    assert_eq!(r, json!({"ok": true, "started": true}));
    wait_for(|| env.country("203.0.113.1").as_deref() == Some("NL")).await;
    let v2 = std::fs::read(env.mmdb()).unwrap();
    assert_ne!(v2, good);

    // A corrupt download: refused, the old database stays (in memory and on disk).
    files.paths.lock().insert(
        path.clone(),
        Answer::Bytes(b"\x1f\x8b not really gzip".to_vec()),
    );
    let err = a.geo().update_now(a, true).await.unwrap_err();
    assert!(err.contains("gzip"), "{err}");
    let mut damaged = test_mmdb::build(&[(ip("203.0.113.0"), 24, "US")], 1);
    damaged[1] = 0xff;
    files
        .paths
        .lock()
        .insert(path.clone(), Answer::Bytes(damaged));
    let err = a.geo().update_now(a, true).await.unwrap_err();
    assert!(err.contains("damaged") || err.contains("MaxMind"), "{err}");
    // Too big uncompressed (a gzip bomb), and too big on the wire (no length).
    let bomb = {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        let zeros = vec![0u8; 1 << 20];
        for _ in 0..65 {
            e.write_all(&zeros).unwrap();
        }
        e.finish().unwrap()
    };
    files.paths.lock().insert(path.clone(), Answer::Bytes(bomb));
    let err = a.geo().update_now(a, true).await.unwrap_err();
    assert!(err.contains("larger than 64 MiB uncompressed"), "{err}");
    files
        .paths
        .lock()
        .insert(path.clone(), Answer::Stream(65 << 20));
    let err = a.geo().update_now(a, true).await.unwrap_err();
    assert!(err.contains("larger than 64 MiB"), "{err}");

    assert_eq!(env.country("203.0.113.1").as_deref(), Some("NL"));
    assert_eq!(std::fs::read(env.mmdb()).unwrap(), v2);
    let v = env.admin("GET", "/_admin/api/analytics/geo", b"").await;
    assert_eq!(v["status"], "error");
    assert!(v["error"].as_str().unwrap().contains("64 MiB"), "{v}");
    assert_eq!(v["db"]["version"], cur.as_str());
    // A good download clears the error.
    files
        .paths
        .lock()
        .insert(path.clone(), Answer::Bytes(db_v1()));
    a.geo().update_now(a, true).await.unwrap();
    assert_eq!(a.geo().status().status, "ready");
    assert_eq!(env.country("203.0.113.1").as_deref(), Some("JP"));
    stop.cancel();
}

#[tokio::test]
async fn the_stored_database_is_loaded_at_start_without_the_network() {
    let (files, base) = file_server().await;
    let (cur, _) = months();
    files
        .paths
        .lock()
        .insert(format!("/db-{cur}.mmdb.gz"), Answer::Bytes(db_v1()));
    let url = format!("{base}/db-{{year}}-{{month}}.mmdb.gz");
    let env = Env::new(Some(&url));
    let a = &env.services.analytics;
    a.geo().update_now(a, false).await.unwrap();
    let Env { dir, services, .. } = env;
    drop(services);
    // A new server on the same storage, with the download gone: the stored file is
    // loaded and nothing is fetched (it is this month's).
    files.paths.lock().clear();
    let hits = files.hits.load(Ordering::SeqCst);
    let env = Env::at(dir, Some(&url));
    let a = &env.services.analytics;
    a.geo().update_now(a, false).await.unwrap();
    assert_eq!(files.hits.load(Ordering::SeqCst), hits);
    assert_eq!(env.country("198.51.100.200").as_deref(), Some("DE"));
    let st = a.geo().status();
    assert_eq!((st.status, st.db.unwrap().version), ("ready", cur));
}

#[tokio::test]
async fn off_by_default_nothing_is_downloaded_and_settings_turn_it_on() {
    let (files, base) = file_server().await;
    let (cur, _) = months();
    files
        .paths
        .lock()
        .insert(format!("/db-{cur}.mmdb.gz"), Answer::Bytes(db_v1()));
    let env = Env::new(None);
    let a = &env.services.analytics;
    assert!(!a.config().geo.enabled);
    let stop = CancellationToken::new();
    a.start(stop.clone());
    env.call(
        None,
        "PROPFIND",
        "/mokuro-reader/",
        RCLONE,
        b"",
        "203.0.113.10",
    )
    .await;
    env.call(None, "GET", "/", FIREFOX, b"", "127.0.0.1").await;
    a.flush_now().await.unwrap();
    assert_eq!(env.country("127.0.0.1"), None);
    assert!(
        env.sql_rows("SELECT country, requests FROM geo_daily")
            .is_empty()
    );
    let v = env.admin("GET", "/_admin/api/analytics/geo", b"").await;
    // `enabled` is the common analytics flag every endpoint sends (the global banner
    // reads it); geography's own is `geo_enabled`.
    assert_eq!(
        (
            v["enabled"].clone(),
            v["geo_enabled"].clone(),
            v["status"].clone()
        ),
        (json!(true), json!(false), json!("off"))
    );
    assert_eq!(v["db"], Value::Null);
    let r = env
        .admin("POST", "/_admin/api/analytics/geo/refresh", b"{}")
        .await;
    assert_eq!(r["started"], false);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(files.hits.load(Ordering::SeqCst), 0);
    assert!(!env.mmdb().exists());

    // The settings editor turns it on, with the test URL: the task downloads.
    let body =
        json!({"geo": {"enabled": true, "url": format!("{base}/db-{{year}}-{{month}}.mmdb.gz")}});
    let saved = env
        .admin(
            "PUT",
            "/_admin/api/settings/analytics",
            body.to_string().as_bytes(),
        )
        .await;
    assert_eq!(saved["analytics"]["geo"]["enabled"], true);
    wait_for(|| env.country("203.0.113.1").as_deref() == Some("JP")).await;
    assert_eq!(env.country("::1").as_deref(), Some("LAN"));
    // And off again: no answers, the database leaves memory.
    let body = json!({"geo": {"enabled": false}});
    env.admin(
        "PUT",
        "/_admin/api/settings/analytics",
        body.to_string().as_bytes(),
    )
    .await;
    assert_eq!(env.country("203.0.113.1"), None);
    wait_for(|| a.geo().status().db.is_none()).await;
    assert_eq!(a.geo().status().status, "off");
    stop.cancel();
    assert_eq!(Country::new("jp"), Country::new("JP"));
}

/// Between turning geo on and the first database that loads (here: every download
/// fails), nothing is counted: not public addresses, and not LAN ones either, so the
/// card never shows a day of "Local network" only.
#[tokio::test]
async fn nothing_is_counted_before_a_database_loads_lan_included() {
    let (files, base) = file_server().await;
    let (cur, _) = months();
    let env = Env::new(Some(&format!("{base}/db-{{year}}-{{month}}.mmdb.gz")));
    let a = &env.services.analytics;
    let err = a.geo().update_now(a, false).await.unwrap_err();
    assert!(err.contains("404"), "{err}");
    for ip in ["192.168.1.5", "127.0.0.1", "203.0.113.10", "2001:db8::5"] {
        env.call(None, "PROPFIND", "/mokuro-reader/", RCLONE, b"", ip)
            .await;
        env.call(None, "GET", "/", FIREFOX, b"", ip).await;
    }
    a.flush_now().await.unwrap();
    assert!(
        env.sql_rows("SELECT country, requests FROM geo_daily")
            .is_empty()
    );
    let v = env.admin("GET", "/_admin/api/analytics/geo", b"").await;
    assert_eq!(v["status"], "error");
    assert_eq!(v["countries"], json!([]));

    // Once one loads, LAN and public addresses both count.
    files
        .paths
        .lock()
        .insert(format!("/db-{cur}.mmdb.gz"), Answer::Bytes(db_v1()));
    a.geo().update_now(a, true).await.unwrap();
    env.call(
        None,
        "PROPFIND",
        "/mokuro-reader/",
        RCLONE,
        b"",
        "192.168.1.5",
    )
    .await;
    env.call(None, "GET", "/", FIREFOX, b"", "203.0.113.10")
        .await;
    a.flush_now().await.unwrap();
    assert_eq!(
        env.sql_rows("SELECT country, requests FROM geo_daily ORDER BY country"),
        [("JP".to_string(), 1), ("LAN".to_string(), 1)]
    );
}

/// `analytics.enabled: false` records nothing, so geo downloads nothing either, even
/// with `analytics.geo.enabled: true`; turning analytics on starts it.
#[tokio::test]
async fn analytics_off_keeps_geo_off_and_offline() {
    let (files, base) = file_server().await;
    let (cur, _) = months();
    files
        .paths
        .lock()
        .insert(format!("/db-{cur}.mmdb.gz"), Answer::Bytes(db_v1()));
    let url = format!("{base}/db-{{year}}-{{month}}.mmdb.gz");
    let env = Env::build(tempfile::tempdir().unwrap(), Some(&url), false);
    let a = &env.services.analytics;
    assert!(a.config().geo.enabled && !a.config().enabled);
    let stop = CancellationToken::new();
    a.start(stop.clone());
    a.geo().update_now(a, true).await.unwrap();
    let r = env
        .admin("POST", "/_admin/api/analytics/geo/refresh", b"{}")
        .await;
    assert_eq!(r["started"], false);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(files.hits.load(Ordering::SeqCst), 0);
    assert!(!env.mmdb().exists());
    assert_eq!(env.country("203.0.113.1"), None);
    let v = env.admin("GET", "/_admin/api/analytics/geo", b"").await;
    assert_eq!(
        (
            v["enabled"].clone(),
            v["geo_enabled"].clone(),
            v["status"].clone()
        ),
        (json!(false), json!(false), json!("off"))
    );

    // Analytics on (geo was already on): the task downloads.
    env.admin(
        "PUT",
        "/_admin/api/settings/analytics",
        json!({"enabled": true}).to_string().as_bytes(),
    )
    .await;
    wait_for(|| env.country("203.0.113.1").as_deref() == Some("JP")).await;
    assert!(files.hits.load(Ordering::SeqCst) > 0);
    // And off again: the database leaves memory, lookups answer nothing.
    env.admin(
        "PUT",
        "/_admin/api/settings/analytics",
        json!({"enabled": false}).to_string().as_bytes(),
    )
    .await;
    assert_eq!(env.country("203.0.113.1"), None);
    wait_for(|| a.geo().status().db.is_none()).await;
    stop.cancel();
}

async fn wait_for(mut ok: impl FnMut() -> bool) {
    for _ in 0..200 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("condition not met within 5 s");
}

/// The database comes over HTTPS only: a plain `http://` URL is never fetched, and a
/// redirect away from HTTPS is refused before anything is sent there; a redirect that
/// stays on HTTPS is followed.
#[tokio::test]
async fn the_database_is_downloaded_over_https_only() {
    let (cur, _) = months();
    let (plain, plain_base) = serve_files(true).await;
    plain
        .paths
        .lock()
        .insert(format!("/db-{cur}.mmdb.gz"), Answer::Bytes(db_v1()));
    // Set directly (the settings refuse it; see the next step), still not fetched.
    let env = Env::new(Some(&format!("{plain_base}/db-{{year}}-{{month}}.mmdb.gz")));
    let a = &env.services.analytics;
    let err = a.geo().update_now(a, true).await.unwrap_err();
    assert!(err.contains("https"), "{err}");
    assert_eq!(plain.hits.load(Ordering::SeqCst), 0);
    assert!(!env.mmdb().exists());
    // The settings refuse a plain URL.
    let body = json!({"geo": {"url": format!("{plain_base}/db.mmdb.gz")}}).to_string();
    let (status, answer) = env
        .call(
            Some("boss"),
            "PUT",
            "/_admin/api/settings/analytics",
            FIREFOX,
            body.as_bytes(),
            "127.0.0.1",
        )
        .await;
    assert!(
        status.is_client_error(),
        "{status} {}",
        String::from_utf8_lossy(&answer)
    );
    assert!(String::from_utf8_lossy(&answer).contains("https"));

    // An HTTPS URL redirecting to the plain server: refused, nothing sent there.
    let (files, base) = file_server().await;
    files.paths.lock().insert(
        format!("/db-{cur}.mmdb.gz"),
        Answer::Redirect(format!("{plain_base}/db-{cur}.mmdb.gz")),
    );
    let env = Env::new(Some(&format!("{base}/db-{{year}}-{{month}}.mmdb.gz")));
    let a = &env.services.analytics;
    let err = a.geo().update_now(a, true).await.unwrap_err();
    assert!(err.contains("HTTPS"), "{err}");
    assert_eq!(plain.hits.load(Ordering::SeqCst), 0);
    assert!(!env.mmdb().exists());

    // A redirect that stays on HTTPS is followed.
    files.paths.lock().insert(
        format!("/db-{cur}.mmdb.gz"),
        Answer::Redirect(format!("{base}/mirror/db-{cur}.mmdb.gz")),
    );
    files
        .paths
        .lock()
        .insert(format!("/mirror/db-{cur}.mmdb.gz"), Answer::Bytes(db_v1()));
    a.geo().update_now(a, true).await.unwrap();
    assert!(env.mmdb().exists());
    assert_eq!(env.country("203.0.113.10").as_deref(), Some("JP"));
}
