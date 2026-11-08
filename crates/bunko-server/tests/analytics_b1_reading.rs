//! Reading analytics through the whole server (docs/design/analytics.md §6): the
//! reader's per-user files arrive as WebDAV PUTs (or MOVEs into place), the worker
//! reads them back from the user's tree and the admin API's reading, users and funnel
//! views answer from what it stored. A generic WebDAV client writing the same files
//! counts the same; a DELETE-then-PUT, a re-upload or another device's merged copy
//! never counts twice.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const IP_READER: &str = "203.0.113.10";
const IP_RCLONE: &str = "198.51.100.23";
const IP_ADMIN: &str = "192.0.2.5";
const READER_ORIGIN: &str = "https://reader.example.com";
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";

/// Volume ids, as the reader's catalog knows them (made up).
const U1: &str = "00000000-0000-4000-8000-0000000000a1";
const U2: &str = "00000000-0000-4000-8000-0000000000a2";
const U3: &str = "00000000-0000-4000-8000-0000000000a3";
const U_LOCAL: &str = "00000000-0000-4000-8000-0000000000ff";
const DEVICE: &str = "0b6f1c2e-3d4a-4b5c-8d9e-0f1a2b3c4d5e";

/// The reader's own fixtures (`src/analytics/reading/testdata`, made by its encoder).
const FIXTURE_2026_10: &[u8] =
    include_bytes!("../src/analytics/reading/testdata/device-a-2026-10.events");
const FIXTURE_LEGACY: &[u8] =
    include_bytes!("../src/analytics/reading/testdata/device-a-legacy.events");
/// Another converting device's `legacy.events`: the same converted page events.
const FIXTURE_LEGACY_B: &[u8] =
    include_bytes!("../src/analytics/reading/testdata/device-b-legacy.events");

/// `(user, uuid, opens, pages, chars, dwell_ms, sources, series_key)`.
type Read = (String, String, i64, i64, i64, i64, i64, String);

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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// "Now" for the uploads: now, or 23:00 UTC yesterday in the first 20 minutes of a UTC
/// day, so that the reading of a test (minutes back from it) lands on one day and in
/// one month, and never in the future.
fn base_ms() -> i64 {
    let now = now_ms();
    let into_day = now.rem_euclid(86_400_000);
    if into_day < 20 * 60_000 {
        now - into_day - 3_600_000
    } else {
        now
    }
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let lib = storage.join("library");
        // Two spellings of one series (case and whitespace: one series, NTFS
        // semantics; they differ in whitespace too, so they are two folders on every
        // filesystem) and another series.
        for (folder, file) in [
            ("The Kingdom", "Kingdom 01.cbz"),
            ("The Kingdom", "Kingdom 02.cbz"),
            ("the  kingdom", "Kingdom 10.cbz"),
            ("Mushishi", "Mushishi 01.cbz"),
        ] {
            std::fs::create_dir_all(lib.join(folder)).unwrap();
            std::fs::write(lib.join(folder).join(file), vec![7u8; 4096]).unwrap();
        }
        let mut config = Config::default();
        config.storage.base_path = storage.clone();
        let config_path = dir.path().join("config.yaml");
        let opts = ServeOptions::default();
        let services = Services::new(config, Some(config_path), &opts).unwrap();
        for (name, role) in [
            ("reader", Role::Registered),
            ("rclone", Role::Registered),
            ("boss", Role::Admin),
        ] {
            services
                .db
                .create_user(name, "pass1234", role, UserStatus::Active, "")
                .unwrap();
        }
        // The primary sidecars' ids (what `volume_identities` keeps).
        services
            .db
            .remember_volume_uuid("The Kingdom/Kingdom 01.cbz", U1)
            .unwrap();
        services
            .db
            .remember_volume_uuid("the  kingdom/Kingdom 10.cbz", U2)
            .unwrap();
        services
            .db
            .remember_volume_uuid("Mushishi/Mushishi 01.cbz", U3)
            .unwrap();
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

    /// A Mokuro Reader WebDAV write: cross-origin from the hosted reader, a browser UA.
    async fn reader_put(&self, user: &str, rel: &str, body: &[u8]) -> StatusCode {
        let path = format!("/mokuro-reader/{rel}");
        let (status, _) = self
            .call(
                Some(user),
                "PUT",
                &path,
                &[
                    ("origin", READER_ORIGIN),
                    ("user-agent", BROWSER_UA),
                    ("content-type", "application/octet-stream"),
                ],
                body,
                IP_READER,
            )
            .await;
        status
    }

    /// A generic WebDAV client's write (rclone-style: no Origin, its own UA).
    async fn rclone(
        &self,
        method: &str,
        rel: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> StatusCode {
        let path = format!("/mokuro-reader/{rel}");
        let mut h = vec![("user-agent", "rclone/v1.71.0")];
        h.extend_from_slice(headers);
        let (status, _) = self
            .call(Some("rclone"), method, &path, &h, body, IP_RCLONE)
            .await;
        status
    }

    async fn mkcol_history(&self, user: &str, device: &str) {
        for path in [
            "/mokuro-reader/history/".to_string(),
            format!("/mokuro-reader/history/{device}/"),
        ] {
            let (status, _) = self
                .call(
                    Some(user),
                    "MKCOL",
                    &path,
                    &[("origin", READER_ORIGIN)],
                    b"",
                    IP_READER,
                )
                .await;
            assert!(
                status == StatusCode::CREATED || status == StatusCode::METHOD_NOT_ALLOWED,
                "{path}: {status}"
            );
        }
    }

    /// Read every file the worker holds, now (no debounce).
    async fn settle(&self) -> usize {
        self.services
            .analytics
            .reading()
            .settle(&self.services.analytics)
            .await
    }

    async fn flush(&self) {
        self.services.analytics.flush_now().await.unwrap();
    }

    async fn admin(&self, path: &str) -> Value {
        let (status, body) = self
            .call(Some("boss"), "GET", path, &[], b"", IP_ADMIN)
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path}: {}",
            String::from_utf8_lossy(&body)
        );
        serde_json::from_slice(&body).unwrap()
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

    /// Every read, by day, user and volume.
    fn reads(&self) -> Vec<Read> {
        let c = self.sql();
        let mut s = c
            .prepare(
                "SELECT username, volume_uuid, opens, pages, chars, dwell_ms, sources, series_key \
                 FROM reading_daily ORDER BY day, username, volume_uuid",
            )
            .unwrap();
        s.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    }
}

fn positions(entries: &[(&str, i64, i64, i64, &str)]) -> Vec<u8> {
    let mut map = serde_json::Map::new();
    for (uuid, t, page, chars, device) in entries {
        map.insert(uuid.to_string(), json!([t, page, chars, device]));
    }
    serde_json::to_vec(&json!({"version": 1, "positions": map})).unwrap()
}

/// A format-1 segment of `device` for `month`, encoded as the reader's
/// `encodeSegment` lays it out (rows: `[kind, dSeq, dT, vol, ...]`).
fn segment(device: &str, month: &str, volumes: &[&str], events: &[(i64, i64, Value)]) -> Vec<u8> {
    let (mut prev_seq, mut prev_t) = (0i64, 0i64);
    let rows: Vec<Value> = events
        .iter()
        .map(|(seq, t, row)| {
            let mut cells = vec![row[0].clone(), json!(seq - prev_seq), json!(t - prev_t)];
            cells.extend(row.as_array().unwrap()[1..].iter().cloned());
            prev_seq = *seq;
            prev_t = *t;
            Value::Array(cells)
        })
        .collect();
    let doc = json!({
        "format": 1, "device": device, "month": month,
        "first_seq": events.first().map_or(0, |e| e.0),
        "last_seq": events.last().map_or(0, |e| e.0),
        "count": events.len(), "volumes": volumes, "rows": rows,
    });
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(doc.to_string().as_bytes()).unwrap();
    e.finish().unwrap()
}

/// A page view row (after `[0, dSeq, dT]`): `[0, vol, first, last, page_chars, chars_before, dwell, layout, orientation, w, h]`.
fn page(vol: usize, first: i64, last: i64, chars: &[i64], dwell: i64) -> Value {
    json!([0, vol, first, last, chars, 0, dwell, 1, 1, 1920, 1080])
}

fn month_of(t_ms: i64) -> String {
    let d = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(t_ms) * 1_000_000).unwrap();
    format!("{:04}-{:02}", d.year(), u8::from(d.month()))
}

fn series<'a>(v: &'a Value, key: &str) -> &'a Vec<Value> {
    v["series"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == key)
        .unwrap_or_else(|| panic!("no series {key}: {v}"))["values"]
        .as_array()
        .unwrap()
}

fn sum(values: &[Value]) -> f64 {
    values.iter().map(|v| v.as_f64().unwrap_or(0.0)).sum()
}

#[tokio::test]
async fn reader_sync_files_over_webdav_become_reads_devices_and_progress() {
    let env = Env::new();
    let now = base_ms();
    // The reader's first positions.json: one volume read just now, one 10 days ago.
    let first = positions(&[
        (U1, now - 600_000, 12, 3400, DEVICE),
        (U3, now - 10 * 86_400_000, 50, 9000, DEVICE),
    ]);
    assert!(
        env.reader_put("reader", "positions.json", &first)
            .await
            .is_success()
    );
    assert_eq!(env.settle().await, 1);
    let r = env.reads();
    assert_eq!(
        r.len(),
        1,
        "only the recent first sighting is a read: {r:?}"
    );
    assert_eq!((r[0].1.as_str(), r[0].2, r[0].3), (U1, 1, 0));
    assert_eq!(r[0].7, "the kingdom", "through volume_identities");
    // Reading on: a new t on U1, and U3's entry is unchanged (another device's copy).
    let next = positions(&[
        (U1, now - 60_000, 30, 7000, DEVICE),
        (U3, now - 10 * 86_400_000, 50, 9000, DEVICE),
    ]);
    assert!(
        env.reader_put("reader", "positions.json", &next)
            .await
            .is_success()
    );
    env.settle().await;
    let r = env.reads();
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].2, r[0].3, r[0].4), (1, 18, 3600));

    // The reading history of this month, as the reader uploads it (MKCOL, then PUT
    // with its query string): it replaces the positions' estimate for that day.
    env.mkcol_history("reader", DEVICE).await;
    let t = now - 300_000;
    let seg = segment(
        DEVICE,
        &month_of(t),
        &[U1],
        &[
            (1, t, page(0, 13, 14, &[100, 120], 40_000)),
            (2, t + 40_000, page(0, 15, 15, &[90], 2_000_000)),
            (3, t + 60_000, json!([1, 0, -10_000, 5])),
        ],
    );
    let rel = format!(
        "history/{DEVICE}/{}.events?Content-Type=application/octet-stream",
        month_of(t)
    );
    assert!(env.reader_put("reader", &rel, &seg).await.is_success());
    // device.json: the device's own facts.
    let facts = json!({"device": DEVICE, "class": "tablet", "os": "iOS", "browser": "Safari",
        "first_seen": "2026-10-01T00:00:00.000Z", "last_seen": "2026-11-01T00:00:00.000Z"});
    let rel_facts = format!("history/{DEVICE}/device.json");
    assert!(
        env.reader_put("reader", &rel_facts, facts.to_string().as_bytes())
            .await
            .is_success()
    );
    assert_eq!(env.settle().await, 2);
    let r = env.reads();
    assert_eq!(r.len(), 1, "{r:?}");
    // 3 pages, 310 + 5 chars, 40 s + 15 min (capped) - 10 s; both sources.
    assert_eq!(
        (r[0].3, r[0].4, r[0].5, r[0].6),
        (3, 315, 40_000 + 900_000 - 10_000, 3)
    );
    // The same segment again (the reader re-uploads the month): nothing more.
    assert!(env.reader_put("reader", &rel, &seg).await.is_success());
    env.settle().await;
    assert_eq!(env.reads()[0].3, 3);

    // volume-data.json: progress for the funnel; a volume the library does not hold
    // goes to the series the reader names; a tombstone is left out.
    let vd = json!({
        "tracking": {"devices": {DEVICE: {"name": "Reading tablet", "lastUpdated": "x"}}},
        U1: {"progress": 30, "chars": 7000, "lastProgressUpdate": "2026-11-04T10:00:00.000Z",
             "series_title": "The Kingdom", "volume_title": "Kingdom 01",
             "recentPageTurns": [[1, 2, 3]], "sessions": [{"x": 1}]},
        U2: {"progress": 200, "completed": true, "series_title": "the kingdom", "volume_title": "Kingdom 10"},
        U_LOCAL: {"progress": 5, "series_title": "Local Only", "volume_title": "Local 1"},
        U3: {"progress": 9, "deletedOn": "2026-11-01T00:00:00Z"},
    });
    assert!(
        env.reader_put("reader", "volume-data.json", vd.to_string().as_bytes())
            .await
            .is_success()
    );
    env.settle().await;
    assert_eq!(
        env.int("SELECT COUNT(*) FROM volume_progress WHERE username='reader'"),
        3
    );
    assert_eq!(
        env.int("SELECT COUNT(*) FROM volume_progress WHERE series_key='local only'"),
        1
    );
    let label: String = env
        .sql()
        .query_row(
            "SELECT label FROM devices WHERE device_id = ?1",
            [DEVICE],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(label, "Reading tablet", "the user's own name wins");

    // A download of U2's archive, for the funnel.
    let (status, _) = env
        .call(
            Some("reader"),
            "GET",
            "/mokuro-reader/the%20%20kingdom/Kingdom%2010.cbz",
            &[("origin", READER_ORIGIN)],
            b"",
            IP_READER,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    env.flush().await;

    // The admin API.
    let v = env.admin("/_admin/api/analytics/reading?range=7d").await;
    // The common fields every answer carries (design §10).
    assert_eq!(v["range"]["key"], "7d");
    assert_eq!(v["range"]["granularity"], "hour");
    assert_eq!(v["enabled"], true);
    assert!(v["tracking_since"].as_i64().is_some(), "{v}");
    assert!(v.get("available").is_none());
    let t_axis = v["t"].as_array().unwrap();
    assert!(t_axis.len() >= 7, "daily points over the days 7d touches");
    assert_eq!(
        t_axis[1].as_i64().unwrap() - t_axis[0].as_i64().unwrap(),
        86400
    );
    assert_eq!(sum(series(&v, "pages")), 3.0);
    assert_eq!(sum(series(&v, "readers")), 1.0);
    assert_eq!(sum(series(&v, "minutes")), 15.5);
    assert_eq!(v["top_readers"][0]["username"], "reader");
    assert_eq!(v["top_readers"][0]["minutes"], 15.5);
    assert_eq!(
        v["top_volumes"][0]["volume_key"],
        "The Kingdom/Kingdom 01.cbz"
    );
    assert_eq!(v["top_volumes"][0]["series_title"], "The Kingdom");
    assert_eq!(v["sources"], json!({"positions": 0, "history": 1}));
    assert_eq!(v["worker"]["segments"], 2);

    let u = env.admin("/_admin/api/analytics/users?range=30d").await;
    assert_eq!(u["wau"], 1);
    assert_eq!(u["mau"], 1);
    assert_eq!(sum(series(&u, "dau")), 1.0);
    let row = u["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["username"] == "reader")
        .unwrap();
    assert!(row["requests"].as_u64().unwrap() >= 7);
    assert!(row["bytes_out"].as_u64().unwrap() >= 4096);
    assert_eq!(row["devices"][0]["id"], DEVICE);
    assert_eq!(row["devices"][0]["label"], "Reading tablet");
    assert_eq!(row["volumes_completed"], 1);
    assert_eq!(row["pages"], 3);
    assert!(row["last_seen"].as_i64().unwrap() > 0);
    // The count is every account of the range, not the listed rows.
    assert_eq!(u["accounts"], u["rows"].as_array().unwrap().len());
    assert_eq!(u["rows_limit"], 200);

    let f = env
        .admin("/_admin/api/analytics/funnel?series=the%20kingdom")
        .await;
    let title = f["title"].as_str().unwrap();
    assert!(title == "The Kingdom" || title == "the  kingdom", "{title}");
    let vols = f["volumes"].as_array().unwrap();
    let titles: Vec<&str> = vols.iter().map(|v| v["title"].as_str().unwrap()).collect();
    assert_eq!(
        titles,
        ["Kingdom 01", "Kingdom 02", "Kingdom 10"],
        "both spellings, library order"
    );
    assert_eq!(
        (vols[0]["started"].as_u64(), vols[0]["finished"].as_u64()),
        (Some(1), Some(0))
    );
    assert_eq!(
        (vols[2]["started"].as_u64(), vols[2]["finished"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(vols[2]["downloads"], 1);
    assert_eq!(
        (
            f["readers_started"].as_u64(),
            f["readers_finished"].as_u64()
        ),
        (Some(1), Some(1))
    );
    // Asked by a spelling, not the key: the same series.
    let again = env
        .admin("/_admin/api/analytics/funnel?series=THE%20%20Kingdom")
        .await;
    assert_eq!(again["volumes"], f["volumes"]);
    // The library-wide funnel.
    let all = env.admin("/_admin/api/analytics/funnel").await;
    assert_eq!(all["series_key"], Value::Null);
    assert_eq!(all["readers_started"], 1);
    let keys: Vec<&str> = all["series"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["series_key"].as_str().unwrap())
        .collect();
    assert!(
        keys.contains(&"the kingdom") && keys.contains(&"local only"),
        "{keys:?}"
    );
    // A series nobody read or downloaded answers, with its volumes at zero.
    let m = env
        .admin("/_admin/api/analytics/funnel?series=mushishi")
        .await;
    assert_eq!(m["volumes"][0]["started"], 0);
    let none = env
        .admin("/_admin/api/analytics/funnel?series=nothing%20here")
        .await;
    assert_eq!(none["volumes"], json!([]));
    assert_eq!(none["in_library"], false);
    // The popular table (A1's series endpoint) now has readers.
    let s = env.admin("/_admin/api/analytics/series?sort=readers").await;
    assert_eq!(s["rows"][0]["series_key"], "the kingdom");
    assert_eq!(s["rows"][0]["readers"], 1);
}

/// Every library volume of a series shows its page count in the funnel, matched by
/// its folded identity: one the reader has no id for (not in the resolver's uuid map)
/// used to have none, and so did one whose only other key was a (folded) download.
#[tokio::test]
async fn every_library_volume_shows_its_page_count_in_the_series_funnel() {
    let env = Env::new();
    // The compiled entries, keyed as the library spells them; Kingdom 02 has no
    // volume id anywhere (no sidecar id, none in its entry).
    for (folder, file, pages, uuid) in [
        ("The Kingdom", "Kingdom 01", 40, Some(U1)),
        ("The Kingdom", "Kingdom 02", 12, None),
        ("the  kingdom", "Kingdom 10", 30, Some(U2)),
    ] {
        let key = bunko_library::compiler::volume_key_for(folder, file);
        let series_key = bunko_library::compat::normalize_volume_title_key(folder);
        let mut entry = serde_json::Map::new();
        entry.insert("page_count".into(), json!(pages));
        if let Some(u) = uuid {
            entry.insert("volume_uuid".into(), json!(u));
        }
        env.services
            .db
            .put_cached_volume_entry(&key, &series_key, &entry, 4096, 1.0, "")
            .unwrap();
    }
    let page_counts = |f: &Value| -> Vec<(String, Value)> {
        f["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                (
                    v["title"].as_str().unwrap().to_string(),
                    v["page_count"].clone(),
                )
            })
            .collect()
    };
    let expected = vec![
        ("Kingdom 01".to_string(), json!(40)),
        ("Kingdom 02".to_string(), json!(12)),
        ("Kingdom 10".to_string(), json!(30)),
    ];
    let f = env
        .admin("/_admin/api/analytics/funnel?series=the%20kingdom")
        .await;
    assert_eq!(page_counts(&f), expected, "{f}");
    // A download of it, by another spelling: its key is the folded identity.
    let (status, _) = env
        .call(
            Some("reader"),
            "GET",
            "/mokuro-reader/THE%20KINGDOM/kingdom%2002.cbz",
            &[("origin", READER_ORIGIN)],
            b"",
            IP_READER,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    env.flush().await;
    let f = env
        .admin("/_admin/api/analytics/funnel?series=the%20kingdom")
        .await;
    assert_eq!(f["volumes"][1]["downloads"], 1, "{f}");
    assert_eq!(page_counts(&f), expected, "{f}");
}

#[tokio::test]
async fn a_generic_webdav_client_counts_the_same_as_the_reader() {
    let env = Env::new();
    let now = base_ms();
    let body = positions(&[(U1, now - 60_000, 7, 700, DEVICE)]);
    // Mokuro Reader's PUT for one account, rclone's for another.
    assert!(
        env.reader_put("reader", "positions.json", &body)
            .await
            .is_success()
    );
    assert!(
        env.rclone("PUT", "positions.json", &[], &body)
            .await
            .is_success()
    );
    env.settle().await;
    let r = env.reads();
    assert_eq!(r.len(), 2);
    assert_eq!((r[0].0.as_str(), r[0].2), ("rclone", 1));
    assert_eq!((r[1].0.as_str(), r[1].2), ("reader", 1));
    assert_eq!(r[0].7, r[1].7);

    // A history segment over plain WebDAV (MKCOL, PUT).
    for p in ["history/", &format!("history/{DEVICE}/")] {
        let s = env.rclone("MKCOL", p, &[], b"").await;
        assert!(
            s == StatusCode::CREATED || s == StatusCode::METHOD_NOT_ALLOWED,
            "{s}"
        );
    }
    let t = now - 120_000;
    let seg = segment(
        DEVICE,
        &month_of(t),
        &[U1],
        &[(1, t, page(0, 8, 9, &[50, 50], 30_000))],
    );
    let rel = format!("history/{DEVICE}/{}.events", month_of(t));
    assert!(env.rclone("PUT", &rel, &[], &seg).await.is_success());
    env.settle().await;
    let rclone_row = env.reads().into_iter().find(|r| r.0 == "rclone").unwrap();
    assert_eq!((rclone_row.3, rclone_row.4, rclone_row.5), (2, 100, 30_000));

    // A MOVE into place counts its destination: goals.json moved over positions.json.
    let moved = positions(&[(U1, now - 30_000, 9, 900, DEVICE)]);
    assert!(
        env.rclone("PUT", "goals.json", &[], &moved)
            .await
            .is_success()
    );
    let s = env
        .rclone(
            "MOVE",
            "goals.json",
            &[
                (
                    "destination",
                    "http://localhost:8080/mokuro-reader/positions.json",
                ),
                ("overwrite", "T"),
            ],
            b"",
        )
        .await;
    assert!(s.is_success(), "MOVE: {s}");
    env.settle().await;
    let row = env.reads().into_iter().find(|r| r.0 == "rclone").unwrap();
    assert_eq!(row.2, 1, "still one read that day");
    let t_ms = env.int("SELECT t_ms FROM read_positions WHERE username='rclone'");
    assert_eq!(t_ms, now - 30_000, "the moved file was read");
}

#[tokio::test]
async fn delete_then_put_and_merged_copies_never_count_twice() {
    let env = Env::new();
    let now = base_ms();
    let v1 = positions(&[(U1, now - 120_000, 10, 1000, DEVICE)]);
    let v2 = positions(&[(U1, now - 60_000, 20, 2000, DEVICE)]);
    assert!(
        env.reader_put("reader", "positions.json", &v1)
            .await
            .is_success()
    );
    env.settle().await;
    // The reader's non-verified upload: DELETE, then PUT; coalesced by the debounce.
    let (s, _) = env
        .call(
            Some("reader"),
            "DELETE",
            "/mokuro-reader/positions.json",
            &[],
            b"",
            IP_READER,
        )
        .await;
    assert!(s.is_success(), "{s}");
    assert!(
        env.reader_put("reader", "positions.json", &v2)
            .await
            .is_success()
    );
    assert_eq!(env.settle().await, 1, "one read of the file");
    let r = env.reads();
    assert_eq!((r.len(), r[0].2, r[0].3), (1, 1, 10));
    // The same, read in between (no coalescing): the DELETE reads nothing.
    let v3 = positions(&[(U1, now - 30_000, 25, 2500, DEVICE)]);
    let (s, _) = env
        .call(
            Some("reader"),
            "DELETE",
            "/mokuro-reader/positions.json",
            &[],
            b"",
            IP_READER,
        )
        .await;
    assert!(s.is_success());
    env.settle().await;
    assert_eq!(env.reads()[0].3, 10);
    assert!(
        env.reader_put("reader", "positions.json", &v3)
            .await
            .is_success()
    );
    env.settle().await;
    assert_eq!(env.reads()[0].3, 15);
    // Another device PUTs the same merged copy: nothing new.
    assert!(
        env.reader_put("reader", "positions.json", &v3)
            .await
            .is_success()
    );
    env.settle().await;
    assert_eq!(env.reads()[0].3, 15);

    // volume-data.json DELETE-then-PUT keeps the progress; a DELETE alone clears it.
    let vd = json!({U1: {"progress": 25, "series_title": "The Kingdom"}}).to_string();
    assert!(
        env.reader_put("reader", "volume-data.json", vd.as_bytes())
            .await
            .is_success()
    );
    env.settle().await;
    let (s, _) = env
        .call(
            Some("reader"),
            "DELETE",
            "/mokuro-reader/volume-data.json",
            &[],
            b"",
            IP_READER,
        )
        .await;
    assert!(s.is_success());
    assert!(
        env.reader_put("reader", "volume-data.json", vd.as_bytes())
            .await
            .is_success()
    );
    env.settle().await;
    assert_eq!(env.int("SELECT COUNT(*) FROM volume_progress"), 1);
    let (s, _) = env
        .call(
            Some("reader"),
            "DELETE",
            "/mokuro-reader/volume-data.json",
            &[],
            b"",
            IP_READER,
        )
        .await;
    assert!(s.is_success());
    env.settle().await;
    assert_eq!(env.int("SELECT COUNT(*) FROM volume_progress"), 0);

    // History: a DELETE then a PUT of the same segment again counts nothing new.
    env.mkcol_history("reader", DEVICE).await;
    let t = now - 90_000;
    let seg = segment(
        DEVICE,
        &month_of(t),
        &[U3],
        &[(1, t, page(0, 1, 4, &[1, 2, 3, 4], 60_000))],
    );
    let rel = format!("history/{DEVICE}/{}.events", month_of(t));
    assert!(env.reader_put("reader", &rel, &seg).await.is_success());
    env.settle().await;
    let (s, _) = env
        .call(
            Some("reader"),
            "DELETE",
            &format!("/mokuro-reader/{rel}"),
            &[],
            b"",
            IP_READER,
        )
        .await;
    assert!(s.is_success(), "{s}");
    env.settle().await;
    assert!(env.reader_put("reader", &rel, &seg).await.is_success());
    env.settle().await;
    let mushishi: Vec<_> = env.reads().into_iter().filter(|r| r.1 == U3).collect();
    assert_eq!((mushishi.len(), mushishi[0].3), (1, 4));
    assert_eq!(env.services.analytics.reading().stats().missing, 3);
}

#[tokio::test]
async fn the_readers_own_segments_and_unknown_formats() {
    let env = Env::new();
    // The reader's encoder's output (October 2026, device-a), on its own paths.
    env.mkcol_history("reader", "device-a").await;
    env.mkcol_history("reader", "device-b").await;
    let month = "history/device-a/2026-10.events";
    let legacy = "history/device-a/legacy.events";
    // The converted turns first, as the reader uploads them (they are older): a page
    // is credited to the first view that arrives with it, and files are read in the
    // order they were written.
    assert!(
        env.reader_put("reader", legacy, FIXTURE_LEGACY)
            .await
            .is_success()
    );
    assert!(
        env.reader_put("reader", month, FIXTURE_2026_10)
            .await
            .is_success()
    );
    // A file on another device's path, and a newer format: refused, counted, no error.
    assert!(
        env.reader_put("reader", "history/device-b/2026-10.events", FIXTURE_2026_10)
            .await
            .is_success()
    );
    assert!(
        env.reader_put(
            "reader",
            "history/device-a/2026-09.events",
            include_bytes!("../src/analytics/reading/testdata/format-2.events")
        )
        .await
        .is_success()
    );
    env.settle().await;
    let stats = env.services.analytics.reading().stats();
    assert_eq!(
        (stats.segments, stats.malformed, stats.unknown_format),
        (2, 1, 1),
        "{stats:?}"
    );
    let rows = || -> Vec<(i64, String, i64, i64, i64)> {
        let c = env.sql();
        let mut s = c
            .prepare(
                "SELECT day, volume_uuid, pages, chars, dwell_ms FROM reading_daily \
                 ORDER BY day, volume_uuid",
            )
            .unwrap();
        s.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    };
    // Legacy (2026-10-02, 20728), V1: pages 4 and 5, 30 s. 2026-10-03 (day 20729), V1:
    // views of pages 3-4, 5 and 6, but 4 and 5 were credited the day before (one pass:
    // the restart is V2's), so pages 3 and 6, 120 + 51 (50.5) - 10 chars, 41 s + 15 min
    // (an hour, capped) - 60 s; 2026-10-04 (20730), V2: 2 pages, 30 + 5 chars, 90 s +
    // 30 s. The forget, position and resolve events add nothing.
    let expected = [
        (20728, U1.to_string(), 2, 0, 30_000),
        (20729, U1.to_string(), 2, 161, 41_000 + 900_000 - 60_000),
        (20730, U2.to_string(), 2, 35, 120_000),
    ];
    assert_eq!(rows(), expected);
    // One cursor per legacy volume (every converting device uploads the same events
    // under `legacy:<volume>`).
    assert_eq!(
        env.int("SELECT COUNT(*) FROM history_cursor WHERE segment='legacy'"),
        2
    );
    // Uploaded again: nothing more.
    assert!(
        env.reader_put("reader", month, FIXTURE_2026_10)
            .await
            .is_success()
    );
    assert!(
        env.reader_put("reader", legacy, FIXTURE_LEGACY)
            .await
            .is_success()
    );
    env.settle().await;
    assert_eq!(rows(), expected);
    // A second converting device uploads the same legacy events on its own path:
    // read (its header names its own device), and counted once.
    let segments = env.services.analytics.reading().stats().segments;
    assert!(
        env.reader_put("reader", "history/device-b/legacy.events", FIXTURE_LEGACY_B)
            .await
            .is_success()
    );
    env.settle().await;
    let stats = env.services.analytics.reading().stats();
    assert_eq!(stats.segments, segments + 1, "{stats:?}");
    assert_eq!(stats.malformed, 1, "{stats:?}");
    assert_eq!(rows(), expected);
    assert_eq!(
        env.int("SELECT COUNT(*) FROM history_cursor WHERE segment='legacy'"),
        2
    );
}

/// Mokuro Reader's own segments of two devices reading in two-page spreads, turning a
/// page at a time (views 1, 2-3, 3-4, ...; `testdata/spreads-*.events`, written by the
/// reader in an end-to-end run): its stats engine credits each page once, 12 + 5 + 4
/// pages and 96 + 40 + 32 characters, not once per view that showed it.
const SPREADS_DESKTOP: (&str, &[u8]) = (
    "9276f2e6-d344-4b94-8e3d-5f7c85759675",
    include_bytes!("../src/analytics/reading/testdata/spreads-desktop-2026-11.events"),
);
const SPREADS_SECOND: (&str, &[u8]) = (
    "001f324d-7d82-4abe-891c-aaca13584b41",
    include_bytes!("../src/analytics/reading/testdata/spreads-second-2026-11.events"),
);

/// `(volume_uuid, pages, chars, dwell_ms)` of every read.
fn page_rows(env: &Env) -> Vec<(String, i64, i64, i64)> {
    env.reads()
        .into_iter()
        .map(|r| (r.1, r.3, r.4, r.5))
        .collect()
}

#[tokio::test]
async fn overlapping_views_credit_each_page_once_per_pass() {
    let env = Env::new();
    for (device, bytes) in [SPREADS_DESKTOP, SPREADS_SECOND] {
        env.mkcol_history("reader", device).await;
        let rel = format!("history/{device}/2026-11.events");
        assert!(env.reader_put("reader", &rel, bytes).await.is_success());
    }
    env.settle().await;
    let vol = |n: u8| format!("00000000-0000-4000-8000-00000000000{n}");
    // Minutes as before: every view's dwell.
    assert_eq!(
        page_rows(&env),
        [
            (vol(0), 12, 96, 60_111),
            (vol(1), 5, 40, 20_023),
            (vol(3), 4, 32, 15_023),
        ]
    );

    // Through the API, over the reader's own paths: a skip, another device, a restart,
    // a later upload of the same month.
    let env = Env::new();
    let now = base_ms();
    let t = now - 900_000;
    let month = month_of(t);
    let other = "1c2d3e4f-5a6b-4c7d-8e9f-0a1b2c3d4e5f";
    env.mkcol_history("reader", DEVICE).await;
    env.mkcol_history("reader", other).await;
    let first = segment(
        DEVICE,
        &month,
        &[U1],
        &[
            (1, t, page(0, 1, 1, &[10], 5_000)),
            (2, t + 5_000, page(0, 2, 3, &[10, 10], 5_000)),
            (3, t + 10_000, page(0, 3, 4, &[10, 10], 5_000)),
            // A step back.
            (4, t + 15_000, page(0, 2, 3, &[10, 10], 5_000)),
            // Flicked through: 2000 characters in a second is a skip.
            (5, t + 20_000, page(0, 5, 6, &[1000, 1000], 1_000)),
        ],
    );
    let rel = format!("history/{DEVICE}/{month}.events");
    assert!(env.reader_put("reader", &rel, &first).await.is_success());
    env.settle().await;
    assert_eq!(page_rows(&env), [(U1.to_string(), 4, 40, 21_000)]);
    // The second device: page 4 again, 5 read now.
    let second = segment(
        other,
        &month,
        &[U1],
        &[(1, t + 30_000, page(0, 4, 5, &[10, 10], 5_000))],
    );
    let rel_other = format!("history/{other}/{month}.events");
    assert!(
        env.reader_put("reader", &rel_other, &second)
            .await
            .is_success()
    );
    env.settle().await;
    assert_eq!(page_rows(&env), [(U1.to_string(), 5, 50, 26_000)]);
    // The first device's file again, with a restart and page 1 after it: a new pass.
    let mut events = vec![
        (1, t, page(0, 1, 1, &[10], 5_000)),
        (2, t + 5_000, page(0, 2, 3, &[10, 10], 5_000)),
        (3, t + 10_000, page(0, 3, 4, &[10, 10], 5_000)),
        (4, t + 15_000, page(0, 2, 3, &[10, 10], 5_000)),
        (5, t + 20_000, page(0, 5, 6, &[1000, 1000], 1_000)),
    ];
    events.push((6, t + 40_000, json!([2, 0])));
    events.push((7, t + 45_000, page(0, 1, 2, &[10, 10], 5_000)));
    let again = segment(DEVICE, &month, &[U1], &events);
    assert!(env.reader_put("reader", &rel, &again).await.is_success());
    env.settle().await;
    assert_eq!(page_rows(&env), [(U1.to_string(), 7, 70, 31_000)]);
    env.flush().await;
    let v = env.admin("/_admin/api/analytics/reading?range=24h").await;
    assert_eq!(sum(series(&v, "pages")), 7.0);
    assert_eq!(v["top_volumes"][0]["pages"], 7, "{v}");
}

/// The server view's `reading_queue` is how many written files wait for the worker
/// now; `reading_queued_total` counts them since start.
#[tokio::test]
async fn the_reading_queue_is_its_depth_now() {
    let env = Env::new();
    let now = base_ms();
    for i in 0..3 {
        let body = positions(&[(U1, now - 60_000 + i, 5 + i, 100, DEVICE)]);
        assert!(
            env.reader_put("reader", "positions.json", &body)
                .await
                .is_success()
        );
    }
    let v = env.admin("/_admin/api/analytics/host?range=24h").await;
    assert_eq!(v["now"]["reading_queue"], 3, "{}", v["now"]);
    env.settle().await;
    let after = env.admin("/_admin/api/analytics/host?range=24h").await;
    assert_eq!(after["now"]["reading_queue"], 0, "{}", after["now"]);
    assert_eq!(v["now"]["reading_queued_total"], 3);
    assert_eq!(after["now"]["reading_queued_total"], 3);
}

/// More accounts than the users view lists: `accounts` still counts them all, and the
/// rows are the ones with the most traffic.
#[tokio::test]
async fn the_users_view_counts_accounts_past_its_row_cap() {
    let env = Env::new();
    let today = now_ms().div_euclid(86_400_000);
    {
        let c = env.sql();
        for i in 0..230 {
            c.execute(
                "INSERT INTO principal_daily (day, principal, category, requests, bytes_out) \
                 VALUES (?1, ?2, 'volume', 1, ?3)",
                rusqlite::params![today, format!("u:user{i:03}"), 1000 + i],
            )
            .unwrap();
        }
    }
    let u = env.admin("/_admin/api/analytics/users?range=7d").await;
    let rows = u["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 200);
    assert_eq!(u["rows_limit"], 200);
    assert!(u["accounts"].as_u64().unwrap() >= 230, "{}", u["accounts"]);
    assert_eq!(rows[0]["username"], "user229");
    assert!(rows.iter().all(|r| r["username"] != "user000"));
}

/// The worker itself, started as the server starts it: a write is read once its
/// debounce has passed, not before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_started_worker_reads_a_file_after_its_debounce() {
    let env = Env::new();
    let stop = tokio_util::sync::CancellationToken::new();
    env.services.analytics.start(stop.clone());
    let now = base_ms();
    let body = positions(&[(U1, now - 60_000, 7, 700, DEVICE)]);
    let started = Instant::now();
    assert!(
        env.reader_put("reader", "positions.json", &body)
            .await
            .is_success()
    );
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(env.reads().is_empty(), "not before the debounce");
    loop {
        if !env.reads().is_empty() {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(30), "never read");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        started.elapsed() >= Duration::from_secs(9),
        "{:?}",
        started.elapsed()
    );
    stop.cancel();
    env.services.analytics.stop().await;
    assert_eq!(env.reads().len(), 1);
}

/// The worker keeps taking events while it reads a batch of files, so the queue's
/// 1024 slots are a burst size, not a budget per batch; files it never reads (goals,
/// profiles) take no slot at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_queue_drains_while_files_are_read_and_ignored_files_take_no_slot() {
    use bunko_dav::DavTarget;
    use bunko_server::analytics::reading::{FileWritten, QUEUE};
    let env = Env::new();
    let a = &env.services.analytics;
    a.reading().set_debounce(Duration::from_millis(20));
    let stop = tokio_util::sync::CancellationToken::new();
    a.start(stop.clone());
    let ev = |user: String, rel: &str| FileWritten {
        username: user,
        target: DavTarget::Progress,
        rel: rel.to_string(),
        method: "PUT",
        at_ms: 0,
    };
    // A batch the worker is stuck reading (a slow disk).
    let slow = a.reading().hold_work().await;
    a.reading()
        .on_file_written(ev("u0".into(), "positions.json"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let n = QUEUE * 3;
    for i in 0..n {
        a.reading()
            .on_file_written(ev(format!("u{}", i + 1), "positions.json"));
        a.reading()
            .on_file_written(ev(format!("u{}", i + 1), "goals.json"));
        if i % 64 == 0 {
            // Requests arrive over time, not all in one instant.
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (queued, dropped) = (a.reading().queued(), a.reading().dropped());
    drop(slow);
    stop.cancel();
    assert_eq!(dropped, 0, "queued {queued}");
    assert_eq!(queued, 1 + n as u64, "goals.json never takes a slot");
}

/// A positions.json written just before a shutdown (an update restart) is read by the
/// shutdown itself, not lost with the debounce queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shutdown_reads_the_files_still_waiting_their_debounce() {
    let env = Env::new();
    let a = &env.services.analytics;
    a.reading().set_debounce(Duration::from_secs(600));
    a.start(tokio_util::sync::CancellationToken::new());
    let now = base_ms();
    let file = positions(&[(U1, now - 600_000, 12, 3400, DEVICE)]);
    assert!(
        env.reader_put("reader", "positions.json", &file)
            .await
            .is_success()
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(env.reads().is_empty(), "still waiting its debounce");
    a.stop().await;
    let r = env.reads();
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!((r[0].1.as_str(), r[0].2), (U1, 1));
}
