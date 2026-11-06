//! Anonymous visitors as ONE row of the per-principal usage lists (docs/design/analytics.md
//! §10, `principals` and the overview's `top_principals`): a display grouping the
//! server does for those ranked lists only. Every count stays per visitor (the active
//! visitors, the client mix, crawl detection, failed sign-ins per IP, the IP lists, the
//! stored rows): `every_other_count_is_unchanged` holds them to a snapshot taken
//! before the grouping existed (`BUNKO_BLESS=1` rewrites it).

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Map, Value, json};
use tower::ServiceExt;

/// A fixed salt, so the anonymous ids are the same on every run (the snapshot).
const SALT: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

const READER_IP: &str = "192.0.2.10";
const UPLOADER_IP: &str = "192.0.2.11";
const OCR_IP: &str = "192.0.2.12";
const ADMIN_IP: &str = "192.0.2.200";
/// The anonymous visitors: four networks (the two IPv6 addresses share a /64), and
/// the refused sign-in's.
const ANON_HEAVY: &str = "203.0.113.10";
const ANON_CRAWLER: &str = "203.0.113.11";
const ANON_CATALOG: &str = "198.51.100.23";
const ANON_V6_A: &str = "2001:db8:1::5";
const ANON_V6_B: &str = "2001:db8:1::6";
const SIGN_IN_IP: &str = "198.51.100.50";

const SERIES: [(&str, &[(&str, usize)]); 4] = [
    ("Alpha Saga", &[("Vol 1", 200_000), ("Vol 2", 100_000)]),
    ("Beta Days", &[("Vol 1", 50_000)]),
    ("Gamma Run", &[("Vol 1", 20_000)]),
    ("Delta Line", &[("Vol 1", 10_000)]),
];

struct Env {
    _dir: tempfile::TempDir,
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

fn seed_library(storage: &Path) {
    for (series, volumes) in SERIES {
        let dir = storage.join("library").join(series);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, size) in volumes {
            let bytes: Vec<u8> = (0..*size).map(|i| (i % 251) as u8).collect();
            std::fs::write(dir.join(format!("{name}.cbz")), bytes).unwrap();
            std::fs::write(dir.join(format!("{name}.webp")), b"RIFF0000WEBP").unwrap();
        }
    }
}

/// `analytics.sqlite` with the fixed salt of the current period, before the server
/// opens it (it keeps a stored salt).
fn seed_salt(storage: &Path) {
    let c = rusqlite::Connection::open(storage.join("analytics.sqlite")).unwrap();
    c.execute(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        [],
    )
    .unwrap();
    let epoch = bunko_server::analytics::now_s().div_euclid(86400) / 30;
    c.execute(
        "INSERT INTO meta (key, value) VALUES ('salt', ?1), ('salt_epoch', ?2)",
        rusqlite::params![SALT, epoch.to_string()],
    )
    .unwrap();
}

impl Env {
    async fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        seed_library(&storage);
        seed_salt(&storage);
        let mut config = Config::default();
        config.storage.base_path = storage.clone();
        config.catalog.enabled = true;
        // Four series: a walk of three of them is a crawl, two are not.
        config.analytics.crawl.min_series = 3;
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
        services.library.recompile_all_now().await;
        services
            .analytics
            .crawl()
            .refresh_catalog_from(services.library.index(), bunko_server::analytics::now_s());
        let app = assemble(&services, &opts);
        Env {
            _dir: dir,
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
        headers: &[(&str, &str)],
        body: &[u8],
        ip: &str,
    ) -> (StatusCode, Vec<u8>) {
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

    async fn get(&self, user: Option<&str>, path: &str, ip: &str) -> StatusCode {
        self.call(user, "GET", path, &[], b"", ip).await.0
    }

    async fn admin(&self, path: &str) -> Value {
        let (status, body) = self
            .call(Some("boss"), "GET", path, &[], b"", ADMIN_IP)
            .await;
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
}

fn vol(series: &str, volume: &str) -> String {
    format!(
        "/mokuro-reader/{}/{}.cbz",
        series.replace(' ', "%20"),
        volume.replace(' ', "%20")
    )
}

/// The same traffic on every run: two accounts, a processor, four anonymous visitors
/// (one walking three series with rclone: a crawl), a refused sign-in, a cross-origin
/// reader.
async fn seed_traffic(env: &Env) {
    let reader_origin = [("origin", "https://reader.example.com")];
    // Accounts.
    assert_eq!(
        env.call(
            Some("reader"),
            "GET",
            &vol("Alpha Saga", "Vol 1"),
            &reader_origin,
            b"",
            READER_IP
        )
        .await
        .0,
        StatusCode::OK
    );
    for v in [vol("Beta Days", "Vol 1"), vol("Delta Line", "Vol 1")] {
        assert_eq!(
            env.get(Some("uploader"), &v, UPLOADER_IP).await,
            StatusCode::OK
        );
    }
    // A processor: bandwidth, never a download.
    assert_eq!(
        env.get(Some("ocr"), &vol("Alpha Saga", "Vol 2"), OCR_IP)
            .await,
        StatusCode::OK
    );
    // Anonymous visitors.
    for v in [vol("Alpha Saga", "Vol 1"), vol("Alpha Saga", "Vol 2")] {
        assert_eq!(env.get(None, &v, ANON_HEAVY).await, StatusCode::OK);
    }
    let rclone = [("user-agent", "rclone/v1.68.1"), ("depth", "1")];
    for (series, _) in SERIES.iter().take(3) {
        let path = format!("/mokuro-reader/{}/", series.replace(' ', "%20"));
        let (s, _) = env
            .call(None, "PROPFIND", &path, &rclone, b"", ANON_CRAWLER)
            .await;
        assert_eq!(s, StatusCode::MULTI_STATUS, "{path}");
    }
    assert_eq!(
        env.call(
            None,
            "GET",
            &vol("Gamma Run", "Vol 1"),
            &[("user-agent", "rclone/v1.68.1")],
            b"",
            ANON_CRAWLER
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        // (The library listing's size depends on the files' mtimes: not this one.)
        env.get(None, "/catalog/api/config", ANON_CATALOG).await,
        StatusCode::OK
    );
    assert_eq!(
        env.get(
            None,
            "/catalog/api/cover?path=Beta%20Days/Vol%201.webp",
            ANON_CATALOG
        )
        .await,
        StatusCode::OK
    );
    for ip in [ANON_V6_A, ANON_V6_B] {
        assert_eq!(
            env.get(None, &vol("Beta Days", "Vol 1"), ip).await,
            StatusCode::OK
        );
    }
    // A refused sign-in.
    let (s, _) = env
        .call(
            None,
            "POST",
            "/login/api/token",
            &[("content-type", "application/json")],
            br#"{"username":"reader","password":"wrong-one"}"#,
            SIGN_IN_IP,
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    env.flush().await;
}

// ---------------------------------------------------------------------------
// The grouped row
// ---------------------------------------------------------------------------

fn rows(v: &Value) -> &Vec<Value> {
    v["rows"].as_array().unwrap()
}

fn kind(r: &Value) -> &str {
    r["kind"].as_str().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anonymous_visitors_are_one_row_that_sums_the_split_rows() {
    let env = Env::new().await;
    seed_traffic(&env).await;

    let grouped = env.admin("/_admin/api/analytics/principals").await;
    assert_eq!(grouped["anon"], "group", "the default mode is echoed");
    let split = env
        .admin("/_admin/api/analytics/principals?anon=split")
        .await;
    assert_eq!(split["anon"], "split");
    let only = env
        .admin("/_admin/api/analytics/principals?anon=only&limit=100")
        .await;
    assert_eq!(only["anon"], "only");

    // One anonymous row in the grouped list, none of the individual ones.
    let g = rows(&grouped);
    let anon: Vec<&Value> = g.iter().filter(|r| kind(r) == "anon_group").collect();
    assert_eq!(anon.len(), 1, "{grouped:#}");
    assert!(g.iter().all(|r| kind(r) != "anon"), "{grouped:#}");
    let group = anon[0];
    assert_eq!(group["principal"], "a:*");
    assert_eq!(group["label"], "Anonymous visitors");
    assert_eq!(
        group["visitors"], 5,
        "five networks (the refused sign-in too): {group:#}"
    );
    assert_eq!(group["ips"], json!([]));

    // `only` lists the anonymous visitors alone, heaviest first, with their IPs.
    let o = rows(&only);
    assert_eq!(o.len(), 5, "{only:#}");
    assert!(o.iter().all(|r| kind(r) == "anon"));
    let bytes: Vec<u64> = o.iter().map(|r| r["bytes_out"].as_u64().unwrap()).collect();
    assert!(bytes.windows(2).all(|w| w[0] >= w[1]), "{bytes:?}");
    let v6 = o
        .iter()
        .find(|r| r["ips"].as_array().unwrap().len() == 2)
        .expect("the /64 visitor lists both of its addresses");
    let mut ips: Vec<&str> = v6["ips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    ips.sort();
    assert_eq!(ips, [ANON_V6_A, ANON_V6_B]);

    // The grouped row is the sum of the split anonymous rows, per field and category.
    let split_anon: Vec<&Value> = rows(&split).iter().filter(|r| kind(r) == "anon").collect();
    assert_eq!(split_anon.len(), 5);
    for field in ["requests", "bytes_out", "bytes_in"] {
        let sum: u64 = split_anon.iter().map(|r| r[field].as_u64().unwrap()).sum();
        assert_eq!(group[field].as_u64().unwrap(), sum, "{field}");
    }
    let mut cats: Map<String, Value> = Map::new();
    for r in &split_anon {
        for (c, n) in r["by_category"].as_object().unwrap() {
            let prev = cats.get(c).and_then(Value::as_u64).unwrap_or(0);
            cats.insert(c.clone(), json!(prev + n.as_u64().unwrap()));
        }
    }
    let mut got: Vec<(String, u64)> = group["by_category"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_u64().unwrap()))
        .collect();
    let mut want: Vec<(String, u64)> = cats
        .into_iter()
        .map(|(k, v)| (k, v.as_u64().unwrap()))
        .collect();
    got.sort();
    want.sort();
    assert_eq!(got, want);
    let last = split_anon
        .iter()
        .filter_map(|r| r["last_seen"].as_i64())
        .max();
    assert_eq!(group["last_seen"].as_i64(), last);

    // The accounts and the processor keep their own rows, as split lists them, and
    // the group stands where its bytes put it.
    let named = |list: &Vec<Value>| -> Vec<Value> {
        list.iter()
            .filter(|r| kind(r) == "user" || kind(r) == "processor")
            .cloned()
            .collect()
    };
    assert_eq!(named(g), named(rows(&split)));
    let order: Vec<u64> = g.iter().map(|r| r["bytes_out"].as_u64().unwrap()).collect();
    assert!(order.windows(2).all(|w| w[0] >= w[1]), "{order:?}");
    // The totals count visitors as before.
    assert_eq!(grouped["totals"], split["totals"]);
    assert_eq!(grouped["totals"]["anon"], 5);

    // The overview's top list groups them the same way.
    let overview = env.admin("/_admin/api/analytics/overview").await;
    assert_eq!(overview["anon"], "group");
    let top = overview["top_principals"].as_array().unwrap();
    let tg: Vec<&Value> = top.iter().filter(|r| kind(r) == "anon_group").collect();
    assert_eq!(tg.len(), 1, "{top:#?}");
    assert_eq!(tg[0]["bytes_out"], group["bytes_out"]);
    assert_eq!(tg[0]["visitors"], 5);
    assert!(top.iter().all(|r| kind(r) != "anon"));
    let split_top = env.admin("/_admin/api/analytics/overview?anon=split").await;
    assert!(
        split_top["top_principals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| kind(r) == "anon")
    );
    // A bad mode is a 400.
    let (s, _) = env
        .call(
            Some("boss"),
            "GET",
            "/_admin/api/analytics/principals?anon=merge",
            &[],
            b"",
            ADMIN_IP,
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// Many anonymous visitors, each heavier than an account, never push the accounts out
/// of a short list: they take one row together.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_anonymous_visitors_take_one_place_in_a_short_list() {
    let env = Env::new().await;
    assert_eq!(
        env.get(Some("reader"), &vol("Delta Line", "Vol 1"), READER_IP)
            .await,
        StatusCode::OK
    );
    assert_eq!(
        env.get(Some("uploader"), &vol("Gamma Run", "Vol 1"), UPLOADER_IP)
            .await,
        StatusCode::OK
    );
    for i in 1..=12 {
        let ip = format!("203.0.113.{}", 100 + i);
        assert_eq!(
            env.get(None, &vol("Beta Days", "Vol 1"), &ip).await,
            StatusCode::OK
        );
    }
    env.flush().await;
    let split = env
        .admin("/_admin/api/analytics/principals?anon=split&limit=5")
        .await;
    assert!(
        rows(&split).iter().all(|r| kind(r) == "anon"),
        "split: the anonymous visitors fill the list: {split:#}"
    );
    let grouped = env.admin("/_admin/api/analytics/principals?limit=3").await;
    let names: Vec<&str> = rows(&grouped)
        .iter()
        .map(|r| r["principal"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a:*", "u:uploader", "u:reader"]);
    assert_eq!(rows(&grouped)[0]["visitors"], 12);
    assert_eq!(rows(&grouped)[0]["bytes_out"], 12 * 50_000);
    let overview = env.admin("/_admin/api/analytics/overview").await;
    let top: Vec<&str> = overview["top_principals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["principal"].as_str().unwrap())
        .collect();
    assert_eq!(top, ["a:*", "u:uploader", "u:reader"]);
    // Expanding lists them, capped by `limit`, heaviest first.
    let only = env
        .admin("/_admin/api/analytics/principals?anon=only&limit=10")
        .await;
    assert_eq!(rows(&only).len(), 10);
    assert!(rows(&only).iter().all(|r| kind(r) == "anon"));
    assert_eq!(only["totals"]["anon"], 12);
}

/// No anonymous traffic: no group row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_anonymous_traffic_no_group_row() {
    let env = Env::new().await;
    assert_eq!(
        env.get(Some("reader"), &vol("Delta Line", "Vol 1"), READER_IP)
            .await,
        StatusCode::OK
    );
    env.flush().await;
    let grouped = env.admin("/_admin/api/analytics/principals").await;
    let names: Vec<&str> = rows(&grouped)
        .iter()
        .map(|r| r["principal"].as_str().unwrap())
        .collect();
    // The admin's own API calls are hers; nobody anonymous.
    assert!(names.iter().all(|p| p.starts_with("u:")), "{names:?}");
}

// ---------------------------------------------------------------------------
// Everything else: unchanged
// ---------------------------------------------------------------------------

/// Keys whose values depend on when the test runs (or on timing), dropped from the
/// snapshot: the axes, clocks, latencies, host readings.
const VOLATILE: &[&str] = &[
    "t",
    "range",
    "tracking_since",
    "last_seen",
    "first_seen",
    "started_at",
    "last_at",
    "at",
    "checked_at",
    "next_check_at",
    "uptime_s",
    "rss_bytes",
    "rss_peak_bytes",
    "cpu_pct",
    "latency",
    "slow",
    "ms",
];

fn strip(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for k in VOLATILE {
                // In place: the snapshot keeps the answers' own key order.
                m.shift_remove(*k);
            }
            for x in m.values_mut() {
                strip(x);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(strip),
        _ => {}
    }
}

fn pick(v: &Value, keys: &[&str]) -> Value {
    let mut m = Map::new();
    for k in keys {
        m.insert((*k).to_string(), v[*k].clone());
    }
    Value::Object(m)
}

/// The stored rows of the per-visitor tables, days relative to today.
fn stored(env: &Env) -> Value {
    let today = bunko_server::analytics::now_s().div_euclid(86400);
    let c = env.sql();
    let dump = |sql: &str| -> Value {
        let mut s = c.prepare(sql).unwrap();
        let n = s.column_count();
        let rows: Vec<Value> = s
            .query_map([today], |r| {
                let mut row = Vec::with_capacity(n);
                for i in 0..n {
                    let v: rusqlite::types::Value = r.get(i)?;
                    row.push(match v {
                        rusqlite::types::Value::Null => Value::Null,
                        rusqlite::types::Value::Integer(i) => json!(i),
                        rusqlite::types::Value::Real(f) => json!(f),
                        rusqlite::types::Value::Text(t) => json!(t),
                        rusqlite::types::Value::Blob(b) => json!(b.len()),
                    });
                }
                Ok(Value::Array(row))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        Value::Array(rows)
    };
    json!({
        "principal_daily": dump("SELECT day - ?1, principal, category, requests, bytes_out, bytes_in FROM principal_daily ORDER BY 1, 2, 3"),
        "principal_seen": dump("SELECT principal FROM principal_seen WHERE ?1 IS NOT NULL ORDER BY 1"),
        "anon_ips": dump("SELECT day - ?1, principal, ip, requests FROM anon_ips ORDER BY 1, 2, 3"),
        "ip_daily": dump("SELECT day - ?1, ip, requests, bytes_out, errors, auth_failures, principals, user_agent, country FROM ip_daily ORDER BY 1, 2"),
        "volume_downloads": dump("SELECT day - ?1, principal, volume_key, series_key, series_title, bytes, size, completed FROM volume_downloads ORDER BY 1, 2, 3"),
        "auth_failures": dump("SELECT ip, attempted_username, count FROM auth_failures WHERE ?1 IS NOT NULL ORDER BY 1, 2"),
        "client_principals": dump("SELECT day - ?1, app, principal FROM client_principals ORDER BY 1, 2, 3"),
        "crawl_episodes": dump("SELECT ip, principals, client, kind, series, volumes, catalog_series, coverage, requests, status FROM crawl_episodes WHERE ?1 IS NOT NULL ORDER BY id"),
    })
}

async fn unchanged_answers(env: &Env) -> Value {
    let api = |p: &str| format!("/_admin/api/analytics/{p}");
    let overview = env.admin(&api("overview")).await;
    let mut principals = env.admin(&api("principals?anon=split")).await;
    // The mode it echoes is new; the rest is the list as it was.
    if let Some(m) = principals.as_object_mut() {
        m.shift_remove("anon");
    }
    let mut out = json!({
        // The overview's figures; its top_principals list is the grouped one now.
        "overview": pick(&overview, &["enabled", "kpis", "bandwidth", "top_series"]),
        // Every principal on its own, as the list was before the grouping.
        "principals_split": principals,
        "users": env.admin(&api("users")).await,
        "clients": env.admin(&api("clients")).await,
        "crawlers": env.admin(&api("crawlers")).await,
        "geo": pick(&env.admin(&api("geo")).await, &["enabled", "geo_enabled", "status", "countries"]),
        "health": env.admin(&api("health")).await,
        "series": env.admin(&api("series")).await,
        "traffic": env.admin(&api("traffic")).await,
        "traffic_dav": env.admin(&api("traffic?by=method&metric=requests")).await,
        "reading": env.admin(&api("reading")).await,
        "funnel": env.admin(&api("funnel")).await,
        "stored": stored(env),
    });
    strip(&mut out);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_other_count_is_unchanged() {
    let env = Env::new().await;
    seed_traffic(&env).await;
    let got = unchanged_answers(&env).await;
    // The snapshot's own sanity: what it pins is what the seed made.
    assert_eq!(got["overview"]["kpis"]["dau"], 7, "{:#}", got["overview"]);
    assert_eq!(got["users"]["visitors"]["dau"], 7);
    assert_eq!(got["principals_split"]["totals"]["anon"], 5);
    assert_eq!(
        got["crawlers"]["episodes"].as_array().unwrap().len(),
        1,
        "{:#}",
        got["crawlers"]
    );
    let text = serde_json::to_string_pretty(&got).unwrap() + "\n";
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/analytics_support/anon-rows-unchanged.json");
    if std::env::var_os("BUNKO_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).expect("the snapshot (BUNKO_BLESS=1 writes it)");
    assert!(
        want == text,
        "the per-visitor answers changed; diff {} against:\n{text}",
        path.display()
    );
}
