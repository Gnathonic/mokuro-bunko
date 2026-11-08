//! The client mix through the whole server (docs/design/analytics.md §9, §10): real
//! user agents and origins of WebDAV clients, the reader and Bunko's own pages are
//! classified, flushed into `clients_daily` / `client_mix_principals`, and answered by
//! `GET /_admin/api/analytics/clients`.

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
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

const HOST: &str = "bunko.example.net";
const READER: &str = "https://reader.example.com";
const DEV_READER: &str = "http://localhost:5173";
const VOL: &str = "/mokuro-reader/series/vol1.cbz";

const RCLONE: &str = "rclone/v1.66.0";
const DAVFS2: &str = "davfs2/1.7.0 neon/0.33.0";
const MINIREDIR: &str = "Microsoft-WebDAV-MiniRedir/10.0.19045";
const CHROME: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const FIREFOX: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0";

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

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(storage.join("library/series")).unwrap();
        std::fs::write(storage.join("library/series/vol1.cbz"), vec![7u8; 4096]).unwrap();
        let mut config = Config::default();
        config.storage.base_path = storage.clone();
        config.catalog.reader_url = format!("{READER}/");
        config.cors.allowed_origins.push(READER.into());
        let opts = ServeOptions::default();
        let services = Services::new(config, Some(dir.path().join("config.yaml")), &opts).unwrap();
        for (name, role) in [("reader", Role::Registered), ("boss", Role::Admin)] {
            services
                .db
                .create_user(name, "pass1234", role, UserStatus::Active, "")
                .unwrap();
        }
        let app = assemble(&services, &opts);
        Env {
            _dir: dir,
            storage,
            services,
            app,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn call(
        &self,
        user: Option<&str>,
        method: &str,
        path: &str,
        ua: Option<&str>,
        origin: Option<&str>,
        extra: &[(&str, &str)],
        body: &[u8],
        ip: &str,
    ) -> (StatusCode, Vec<u8>) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("host", HOST);
        if let Some(u) = user {
            b = b.header("authorization", basic(u));
        }
        if let Some(ua) = ua {
            b = b.header("user-agent", ua);
        }
        if let Some(o) = origin {
            b = b.header("origin", o);
        }
        for (k, v) in extra {
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

    async fn admin(&self, path: &str) -> Value {
        let (status, body) = self
            .call(
                Some("boss"),
                "GET",
                path,
                Some(FIREFOX),
                None,
                &[],
                b"",
                "192.0.2.1",
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }

    fn int(&self, sql: &str) -> i64 {
        rusqlite::Connection::open(self.storage.join("analytics.sqlite"))
            .unwrap()
            .query_row(sql, [], |r| r.get::<_, Option<i64>>(0))
            .unwrap()
            .unwrap_or(0)
    }
}

fn row<'a>(rows: &'a Value, key: &str) -> &'a Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == key)
        .unwrap_or_else(|| panic!("no {key} in {rows}"))
}

/// One of everything: three WebDAV clients, the hosted reader, a dev reader and
/// Bunko's own pages, from documentation addresses.
async fn traffic(env: &Env) {
    let depth1 = [("depth", "1")];
    let depth0 = [("depth", "0")];
    // rclone lists and downloads, anonymously, from one address.
    for _ in 0..2 {
        let (s, _) = env
            .call(
                None,
                "PROPFIND",
                "/mokuro-reader/",
                Some(RCLONE),
                None,
                &depth1,
                b"",
                "203.0.113.10",
            )
            .await;
        assert_eq!(s.as_u16(), 207);
    }
    let (s, body) = env
        .call(
            None,
            "GET",
            VOL,
            Some(RCLONE),
            None,
            &[],
            b"",
            "203.0.113.10",
        )
        .await;
    assert_eq!((s, body.len()), (StatusCode::OK, 4096));
    // davfs2 and the Windows mini-redirector, each from its own address.
    let (s, _) = env
        .call(
            None,
            "GET",
            VOL,
            Some(DAVFS2),
            None,
            &[],
            b"",
            "198.51.100.23",
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = env
        .call(
            None,
            "PROPFIND",
            "/mokuro-reader/series/",
            Some(MINIREDIR),
            None,
            &depth0,
            b"",
            "198.51.100.24",
        )
        .await;
    assert_eq!(s.as_u16(), 207);
    // The hosted reader (Chrome, cross-origin): its listing, a download and a
    // positions.json sync as an account.
    let (s, _) = env
        .call(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader/",
            Some(CHROME),
            Some(READER),
            &depth1,
            b"",
            "192.0.2.50",
        )
        .await;
    assert_eq!(s.as_u16(), 207);
    let (s, _) = env
        .call(
            Some("reader"),
            "GET",
            VOL,
            Some(CHROME),
            Some(READER),
            &[],
            b"",
            "192.0.2.50",
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let positions = br#"{"version":1,"positions":{}}"#;
    let (s, _) = env
        .call(
            Some("reader"),
            "PUT",
            "/mokuro-reader/positions.json",
            Some(CHROME),
            Some(READER),
            &[("content-type", "application/json")],
            positions,
            "192.0.2.50",
        )
        .await;
    assert!(s.is_success(), "{s}");
    let (s, _) = env
        .call(
            Some("reader"),
            "GET",
            "/login/api/me",
            Some(CHROME),
            Some(READER),
            &[],
            b"",
            "192.0.2.50",
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    // A reader build on a dev server.
    let (s, _) = env
        .call(
            None,
            "PROPFIND",
            "/mokuro-reader/",
            Some(FIREFOX),
            Some(DEV_READER),
            &depth1,
            b"",
            "192.0.2.60",
        )
        .await;
    assert_eq!(s.as_u16(), 207);
    // Bunko's own pages in Firefox, same origin.
    env.call(
        None,
        "GET",
        "/",
        Some(FIREFOX),
        None,
        &[],
        b"",
        "192.0.2.70",
    )
    .await;
    let own = format!("http://{HOST}");
    env.call(
        None,
        "GET",
        "/login/api/me",
        Some(FIREFOX),
        Some(&own),
        &[],
        b"",
        "192.0.2.70",
    )
    .await;
}

#[tokio::test]
async fn webdav_clients_the_reader_and_the_web_ui_are_told_apart() {
    let env = Env::new();
    traffic(&env).await;
    env.services.analytics.flush_now().await.unwrap();

    // The rows themselves: WebDAV clients are protocol dav rows.
    assert_eq!(
        env.int(
            "SELECT SUM(requests) FROM clients_daily WHERE app='webdav-client' AND client='rclone' AND protocol='dav'"
        ),
        3
    );
    assert_eq!(
        env.int(
            "SELECT SUM(requests) FROM clients_daily WHERE app='webdav-client' AND client='windows-webdav' AND os='windows' AND protocol='dav'"
        ),
        1
    );
    assert_eq!(
        env.int(
            "SELECT SUM(requests) FROM clients_daily WHERE app='reader-hosted' AND browser='chrome' AND os='windows'"
        ),
        4
    );
    assert_eq!(
        env.int("SELECT COUNT(*) FROM client_mix_principals WHERE client='rclone'"),
        1
    );

    let v = env.admin("/_admin/api/analytics/clients?range=24h").await;
    assert_eq!(v["range"]["key"], "24h");
    let apps = &v["apps"];
    let hosted = row(apps, "reader-hosted");
    assert_eq!(hosted["requests"], 4);
    assert_eq!(hosted["principals"], 1);
    assert_eq!(hosted["label"], "Mokuro Reader (hosted)");
    assert!(hosted["bytes_out"].as_u64().unwrap() >= 4096);
    assert_eq!(row(apps, "reader-other")["requests"], 1);
    let dav = row(apps, "webdav-client");
    assert_eq!(dav["requests"], 5);
    // rclone, davfs2 and the mini-redirector came from three addresses.
    assert_eq!(dav["principals"], 3);
    assert_eq!(row(apps, "bunko-web")["requests"], 2);

    let clients = &v["clients"];
    assert_eq!(row(clients, "rclone")["requests"], 3);
    assert!(row(clients, "rclone")["bytes_out"].as_u64().unwrap() >= 4096);
    assert_eq!(row(clients, "davfs2")["principals"], 1);
    assert_eq!(row(clients, "windows-webdav")["requests"], 1);
    assert_eq!(row(clients, "browser")["requests"], 7);
    assert_eq!(row(clients, "browser")["principals"], 3);
    assert_eq!(row(&v["browsers"], "chrome")["requests"], 4);
    assert_eq!(row(&v["browsers"], "firefox")["requests"], 3);
    assert_eq!(row(&v["browsers"], "-")["requests"], 5);
    assert_eq!(row(&v["os"], "windows")["requests"], 5);
    assert_eq!(row(&v["os"], "linux")["requests"], 4);
    let protocols = &v["protocols"];
    assert_eq!(row(protocols, "dav")["requests"], 9);
    assert_eq!(row(protocols, "dav")["label"], "WebDAV");

    // The WebDAV breakdown on its own: the reader's DAV requests and the generic
    // clients side by side.
    let dc = &v["dav_clients"];
    assert_eq!(row(dc, "rclone")["requests"], 3);
    assert_eq!(row(dc, "browser")["requests"], 4);
    assert_eq!(row(&v["dav_apps"], "reader-hosted")["requests"], 3);

    // Cross-origin callers only; the page's own origin is not one.
    let origins: Vec<&str> = v["origins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["origin"].as_str().unwrap())
        .collect();
    assert_eq!(origins, [READER, DEV_READER], "{}", v["origins"]);
    assert_eq!(v["origins"][0]["requests"], 4);
    assert_eq!(v["origins"][0]["hosted_reader"], true);
    assert_eq!(v["origins"][1]["hosted_reader"], false);

    // Requests by app per UTC day (one or two days for a 24h range).
    let t = v["by_app"]["t"].as_array().unwrap();
    assert!((1..=2).contains(&t.len()), "{}", v["by_app"]);
    assert_eq!(t.last().unwrap().as_i64().unwrap() % 86400, 0);
    let series = v["by_app"]["series"].as_array().unwrap();
    let hosted_series = series.iter().find(|s| s["key"] == "reader-hosted").unwrap();
    assert_eq!(
        hosted_series["values"].as_array().unwrap().last().unwrap(),
        4
    );
    assert_eq!(v["totals"]["requests"], 12);
    assert_eq!(v["principals_days"], 30);
}

#[tokio::test]
async fn distinct_visitors_follow_the_ip_retention() {
    let env = Env::new();
    traffic(&env).await;
    env.services.analytics.flush_now().await.unwrap();
    assert!(env.int("SELECT COUNT(*) FROM client_mix_principals") > 0);
    let now = bunko_server::analytics::now_s();
    env.services
        .analytics
        .maintain_at(now + 40 * 86400)
        .await
        .unwrap();
    assert_eq!(env.int("SELECT COUNT(*) FROM client_mix_principals"), 0);
    // The daily totals are kept forever.
    assert_eq!(env.int("SELECT SUM(requests) FROM clients_daily"), 12);
}

/// Bunko's own sign-in and account calls behind the reverse proxies the docs describe
/// are Bunko's pages, never a reader on another origin: the CSRF rule decides what
/// "same origin" means (a port-dropping `Host $host`, an `X-Forwarded-Host`), and a
/// proxy that hides both still leaves a browser on a CORS-disallowed origin, which only
/// Bunko's own page can be.
#[tokio::test]
async fn bunko_sign_in_behind_a_proxy_is_bunko_web() {
    let env = Env::new();
    let send = |host: &str, xfh: Option<&str>, origin: &str, method: &str, path: &str| {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("host", host)
            .header("origin", origin)
            .header("user-agent", FIREFOX);
        if let Some(x) = xfh {
            b = b.header("x-forwarded-host", x);
        }
        let body: &[u8] = if method == "POST" {
            br#"{"username":"reader","password":"pass1234"}"#
        } else {
            b""
        };
        if !body.is_empty() {
            b = b
                .header("content-type", "application/json")
                .header("content-length", body.len().to_string());
        }
        let mut req = b.body(Body::from(body.to_vec())).unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            "192.0.2.80".parse().unwrap(),
            40000,
        )));
        let app = env.app.clone();
        async move { app.oneshot(req).await.unwrap().status() }
    };
    // nginx's `proxy_set_header Host $host` drops a non-default port.
    let own_port = "https://bunko.example.net:8443";
    let s = send(HOST, None, own_port, "POST", "/login/api/token").await;
    assert_eq!(
        s,
        StatusCode::OK,
        "the CSRF rule lets Bunko's own sign-in in"
    );
    send(HOST, None, own_port, "GET", "/login/api/me").await;
    // A proxy that rewrites Host and says where the page is in X-Forwarded-Host.
    let fwd = Some("bunko.example.com");
    let s = send(
        "127.0.0.1:8080",
        fwd,
        "https://bunko.example.com",
        "POST",
        "/login/api/token",
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // One that hides both: a browser on an origin CORS refuses.
    send(
        "127.0.0.1:8080",
        None,
        "https://bunko.example.com",
        "GET",
        "/login/api/me",
    )
    .await;
    env.services.analytics.flush_now().await.unwrap();

    assert_eq!(
        env.int("SELECT SUM(requests) FROM clients_daily WHERE app='bunko-web'"),
        4
    );
    assert_eq!(
        env.int("SELECT COUNT(*) FROM clients_daily WHERE app='reader-other'"),
        0
    );
    // Bunko's own origin is not a cross-origin caller; only the hidden one is listed.
    let v = env.admin("/_admin/api/analytics/clients?range=24h").await;
    let origins: Vec<&str> = v["origins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["origin"].as_str().unwrap())
        .collect();
    assert_eq!(origins, ["https://bunko.example.com"], "{}", v["origins"]);
}
