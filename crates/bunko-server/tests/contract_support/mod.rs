//! The reader contract's shared harness (`interop/contract/`, spec interop-testing §5):
//! the seed, a full-stack in-process server built from it (`Services` + `assemble`, the
//! only setup with the CORS, queue-file and WebDAV layers), requests in the fixture
//! format, the normalizer that turns volatile values into typed sentinels, and the
//! golden-file compare.
#![allow(dead_code)]
// Fixtures are written and compared with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use base64::Engine as _;
use bunko_core::config::LibraryBackend;
use bunko_core::{Config, Role};
use bunko_db::{TokenKind, UserStatus};
use bunko_remote::fake::FakeBackend;
use bunko_server::app::{ServeOptions, Services, assemble, start_library_services};
use http::{HeaderMap, HeaderValue, Request};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

// ---- sentinels ----------------------------------------------------------------------

pub const ISO_SENTINEL: &str = "2000-01-01T00:00:00Z";
pub const HTTP_DATE_SENTINEL: &str = "Sat, 01 Jan 2000 00:00:00 GMT";
pub const EPOCH_S: i64 = 946_684_800;
pub const EPOCH_MS: i64 = 946_684_800_000;
pub const ETAG_SENTINEL: &str = "\"etag\"";
pub const TOKEN_SENTINEL: &str = "<token>";
pub const VERSION_SENTINEL: &str = "0.0.0";

/// The library's root folder as WebDAV serves it.
pub const ROOT: &str = "mokuro-reader";

// ---- paths --------------------------------------------------------------------------

/// `interop/contract/` in this checkout.
pub fn contract_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/contract")
}

// ---- the seed -----------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SeedUser {
    pub username: String,
    pub password: String,
    pub role: Role,
}

#[derive(Debug, Clone)]
pub struct SeedVolume {
    pub name: String,
    pub pages: usize,
    pub mokuro: bool,
}

#[derive(Debug, Clone)]
pub struct SeedSeries {
    pub series: String,
    pub volumes: Vec<SeedVolume>,
}

#[derive(Debug, Clone)]
pub struct Seed {
    pub users: Vec<SeedUser>,
    pub library: Vec<SeedSeries>,
}

impl Seed {
    pub fn load() -> Seed {
        let path = contract_dir().join("seed.json");
        let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let v: Value = serde_json::from_slice(&raw).expect("seed.json is JSON");
        let s = |v: &Value, k: &str| {
            v[k].as_str()
                .unwrap_or_else(|| panic!("seed: {k}"))
                .to_string()
        };
        let users = v["users"]
            .as_array()
            .expect("seed users")
            .iter()
            .map(|u| SeedUser {
                username: s(u, "username"),
                password: s(u, "password"),
                role: s(u, "role").parse().expect("seed role"),
            })
            .collect();
        let library = v["library"]
            .as_array()
            .expect("seed library")
            .iter()
            .map(|series| SeedSeries {
                series: s(series, "series"),
                volumes: series["volumes"]
                    .as_array()
                    .expect("seed volumes")
                    .iter()
                    .map(|vol| SeedVolume {
                        name: s(vol, "name"),
                        pages: vol["pages"].as_u64().expect("seed pages") as usize,
                        mokuro: vol["mokuro"].as_bool().unwrap_or(false),
                    })
                    .collect(),
            })
            .collect();
        Seed { users, library }
    }

    pub fn user(&self, name: &str) -> &SeedUser {
        self.users
            .iter()
            .find(|u| u.username == name)
            .unwrap_or_else(|| panic!("no seed user {name}"))
    }

    pub fn password(&self, name: &str) -> &str {
        &self.user(name).password
    }

    /// Every file of the library, relative to its root, with its bytes.
    pub fn files(&self) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        for s in &self.library {
            for v in &s.volumes {
                out.push((format!("{}/{}.cbz", s.series, v.name), cbz(v.pages)));
                if v.mokuro {
                    out.push((
                        format!("{}/{}.mokuro", s.series, v.name),
                        mokuro(&s.series, &v.name, v.pages),
                    ));
                }
            }
        }
        out
    }
}

/// A `.cbz` of `pages` tiny "images" (`001.jpg`...), byte-identical on every run: fixed
/// timestamps, stored entries.
pub fn cbz(pages: usize) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(zip::DateTime::default());
    for i in 1..=pages {
        z.start_file(format!("{i:03}.jpg"), opts).unwrap();
        z.write_all(b"\xff\xd8\xff\xd9").unwrap();
    }
    z.finish().unwrap().into_inner()
}

/// The primary `.mokuro` of a seed volume (fixed uuids, one text block a page).
pub fn mokuro_doc(series: &str, volume: &str, pages: usize) -> Value {
    let pages: Vec<Value> = (1..=pages)
        .map(|i| {
            json!({"img_path": format!("{i:03}.jpg"), "img_width": 100, "img_height": 150,
                "blocks": [{"box": [1, 1, 50, 50], "vertical": true, "font_size": 12,
                    "lines": ["日本語です"], "lines_coords": [[[1, 1], [50, 1], [50, 50], [1, 50]]]}]})
        })
        .collect();
    json!({
        "version": "0.2.2", "title": series, "title_uuid": "11111111-1111-4111-8111-111111111111",
        "volume": volume, "volume_uuid": "22222222-2222-4222-8222-222222222222", "pages": pages,
    })
}

pub fn mokuro(series: &str, volume: &str, pages: usize) -> Vec<u8> {
    serde_json::to_vec(&mokuro_doc(series, volume, pages)).unwrap()
}

/// A path segment as the reader encodes it (`encodeURIComponent`).
pub fn encode(segment: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'!')
        .remove(b'~')
        .remove(b'*')
        .remove(b'\'')
        .remove(b'(')
        .remove(b')');
    percent_encoding::utf8_percent_encode(segment, SET).to_string()
}

/// `/mokuro-reader/<series>/<file>`, each segment encoded.
pub fn dav_path(parts: &[&str]) -> String {
    let mut p = format!("/{ROOT}");
    for s in parts {
        p.push('/');
        p.push_str(&encode(s));
    }
    p
}

// ---- requests -----------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ReqBody {
    None,
    /// JSON; a string `<password:USER>` anywhere in it is sent as that seed password.
    Json(Value),
    Text(String),
    /// A generated `.cbz` of this many pages ([`cbz`]).
    Cbz(usize),
}

/// A request in the fixture format.
#[derive(Debug, Clone, PartialEq)]
pub struct Req {
    pub method: String,
    pub path: String,
    /// `none`, `basic:<user>`, `bearer:<user>` or `raw:<header value>`.
    pub auth: String,
    /// Lower-case names.
    pub headers: Vec<(String, String)>,
    pub body: ReqBody,
}

impl Req {
    pub fn new(method: &str, path: &str, auth: &str) -> Req {
        Req {
            method: method.into(),
            path: path.into(),
            auth: auth.into(),
            headers: Vec::new(),
            body: ReqBody::None,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Req {
        self.headers
            .push((name.to_ascii_lowercase(), value.to_string()));
        self
    }

    pub fn json(mut self, body: Value) -> Req {
        self.body = ReqBody::Json(body);
        self
    }

    pub fn text(mut self, body: &str) -> Req {
        self.body = ReqBody::Text(body.into());
        self
    }

    pub fn cbz(mut self, pages: usize) -> Req {
        self.body = ReqBody::Cbz(pages);
        self
    }

    pub fn to_json(&self) -> Value {
        let mut headers = Map::new();
        let mut sorted = self.headers.clone();
        sorted.sort();
        for (k, v) in sorted {
            headers.insert(k, Value::String(v));
        }
        let mut m = Map::new();
        m.insert("method".into(), json!(self.method));
        m.insert("path".into(), json!(self.path));
        m.insert("auth".into(), json!(self.auth));
        m.insert("headers".into(), Value::Object(headers));
        match &self.body {
            ReqBody::None => {}
            ReqBody::Json(v) => {
                m.insert("body".into(), v.clone());
            }
            ReqBody::Text(t) => {
                m.insert("body_text".into(), json!(normalize_eol(t)));
            }
            ReqBody::Cbz(pages) => {
                m.insert("body_cbz".into(), json!({ "pages": pages }));
            }
        }
        Value::Object(m)
    }

    /// The fixture format back into a request (the replay's records).
    pub fn from_json(v: &Value) -> Result<Req, String> {
        let s = |k: &str| {
            v[k].as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("request.{k} missing"))
        };
        let headers = match &v["headers"] {
            Value::Null => Vec::new(),
            Value::Object(m) => m
                .iter()
                .map(|(k, v)| {
                    v.as_str()
                        .map(|s| (k.to_ascii_lowercase(), s.to_string()))
                        .ok_or_else(|| format!("header {k} is not a string"))
                })
                .collect::<Result<_, _>>()?,
            _ => return Err("request.headers is not an object".into()),
        };
        let body = if let Some(b) = v.get("body") {
            ReqBody::Json(b.clone())
        } else if let Some(t) = v.get("body_text").and_then(Value::as_str) {
            ReqBody::Text(t.to_string())
        } else if let Some(p) = v.get("body_cbz") {
            ReqBody::Cbz(p["pages"].as_u64().unwrap_or(1) as usize)
        } else {
            ReqBody::None
        };
        Ok(Req {
            method: s("method")?,
            path: s("path")?,
            auth: v["auth"].as_str().unwrap_or("none").to_string(),
            headers,
            body,
        })
    }
}

/// A response as the test saw it.
pub struct Resp {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Replace every `<password:USER>` string in a JSON body with that seed password.
fn with_passwords(v: &Value, seed: &Seed) -> Value {
    match v {
        Value::String(s) => match s
            .strip_prefix("<password:")
            .and_then(|r| r.strip_suffix('>'))
        {
            Some(user) => Value::String(seed.password(user).to_string()),
            None => v.clone(),
        },
        Value::Array(a) => Value::Array(a.iter().map(|x| with_passwords(x, seed)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), with_passwords(x, seed)))
                .collect(),
        ),
        other => other.clone(),
    }
}

pub fn basic_value(user: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
    )
}

// ---- the server ---------------------------------------------------------------------

/// A seeded server: a disk library (or a `FakeBackend` cloud library) generated from the
/// seed, its users, and the full router.
pub struct Server {
    _dir: tempfile::TempDir,
    pub services: Services,
    pub app: Router,
    pub seed: Seed,
    pub fake: Option<Arc<FakeBackend>>,
    stop: CancellationToken,
    running: Option<tokio::task::JoinHandle<()>>,
}

fn base_config(base: &Path) -> Config {
    let mut c = Config::default();
    c.storage.base_path = base.to_path_buf();
    c.server.serve_readers = true;
    c.catalog.enabled = true;
    c.catalog.enrich_community = false;
    c.cors.enabled = true;
    c.ocr.autobench = false;
    c.ocr.local_processing = false;
    c
}

fn add_users(services: &Services, seed: &Seed) {
    for u in &seed.users {
        services
            .db
            .create_user(&u.username, &u.password, u.role, UserStatus::Active, "")
            .unwrap();
    }
}

/// [`Server::local`]: the server every contract fixture but the cloud ones runs on.
pub async fn build_server(seed: &Seed) -> Server {
    Server::local(seed).await
}

impl Server {
    /// A disk library from the seed, compiled (`series.json`, `catalog.json` exist), the
    /// OCR scheduler running without a processor (Volume 02 waits in the queue).
    pub async fn local(seed: &Seed) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("storage");
        let library = base.join("library");
        for (rel, bytes) in seed.files() {
            let path = library.join(&rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let opts = ServeOptions::default();
        let services = Services::new(base_config(&base), None, &opts).unwrap();
        add_users(&services, seed);
        let changed = services.library.recompile_all_now().await;
        assert!(changed > 0, "the seed library compiles");
        let stop = CancellationToken::new();
        services.ocr.start(stop.child_token());
        let app = assemble(&services, &opts);
        let server = Server {
            _dir: dir,
            services,
            app,
            seed: seed.clone(),
            fake: None,
            stop,
            running: None,
        };
        server.wait_queue_settled().await;
        server
    }

    /// A cloud library on the in-memory `FakeBackend`, filled from the seed, after its
    /// first sync, with the library services running.
    pub async fn cloud(seed: &Seed, tune: bunko_server::storage::testing::Tune) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("storage");
        let fake = Arc::new(FakeBackend::drive_like());
        for (rel, bytes) in seed.files() {
            fake.reader_put(&format!("{ROOT}/{rel}"), &bytes);
        }
        bunko_server::storage::testing::register_tuned(&base, fake.clone(), Some(tune));
        let mut config = base_config(&base);
        config.storage.library.backend = LibraryBackend::GDrive;
        let opts = ServeOptions::default();
        let services = Services::new(config, None, &opts).unwrap();
        add_users(&services, seed);
        let storage = services.storage.clone().expect("a cloud library");
        let steady =
            tokio::task::spawn_blocking(move || storage.wait_steady(Duration::from_secs(20)))
                .await
                .unwrap();
        assert!(steady, "the first sync finishes");
        let stop = CancellationToken::new();
        let running = start_library_services(&services, stop.clone());
        let app = assemble(&services, &opts);
        Server {
            _dir: dir,
            services,
            app,
            seed: seed.clone(),
            fake: Some(fake),
            stop,
            running: Some(running),
        }
    }

    pub fn store(&self) -> Arc<bunko_remote::RemoteStore<FakeBackend>> {
        self.services
            .storage
            .as_ref()
            .and_then(|s| s.fake_store())
            .expect("a cloud library")
            .clone()
    }

    pub fn library(&self) -> PathBuf {
        self.services.core.layout.library()
    }

    /// The queue file stops changing (the scheduler has looked at the library).
    async fn wait_queue_settled(&self) {
        let until = Instant::now() + Duration::from_secs(20);
        let req = Req::new("GET", &format!("/{ROOT}/.mokuro-queue.json"), "none");
        let mut last: Option<Value> = None;
        loop {
            let r = self.send(&req).await;
            let doc = r.json().map(|mut d| {
                if let Some(m) = d.as_object_mut() {
                    m.remove("generated_at");
                }
                d
            });
            let waiting = doc
                .as_ref()
                .and_then(|d| d["pending_volumes"].as_u64())
                .unwrap_or(0);
            if waiting > 0 && doc == last {
                return;
            }
            assert!(
                Instant::now() < until,
                "the queue file never listed the volume without OCR: {}",
                r.text()
            );
            last = doc;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The `Authorization` header value for a fixture `auth`.
    pub fn authorization(&self, auth: &str) -> Option<HeaderValue> {
        if auth == "none" || auth.is_empty() {
            return None;
        }
        if let Some(user) = auth.strip_prefix("basic:") {
            return Some(basic_value(user, self.seed.password(user)).parse().unwrap());
        }
        if let Some(user) = auth.strip_prefix("bearer:") {
            let token = self.token(user);
            return Some(format!("Bearer {token}").parse().unwrap());
        }
        if let Some(raw) = auth.strip_prefix("raw:") {
            return Some(HeaderValue::from_bytes(raw.as_bytes()).unwrap());
        }
        panic!("unknown auth {auth}");
    }

    /// A fresh reader token for `user`.
    pub fn token(&self, user: &str) -> String {
        self.services
            .db
            .create_auth_token(user, TokenKind::Reader, "contract", None)
            .unwrap()
            .0
    }

    pub async fn send(&self, req: &Req) -> Resp {
        self.send_with(req, self.authorization(&req.auth)).await
    }

    pub async fn send_with(&self, req: &Req, auth: Option<HeaderValue>) -> Resp {
        let mut b = Request::builder()
            .method(req.method.as_str())
            .uri(req.path.as_str())
            .header("host", "localhost:8080");
        if let Some(a) = auth {
            b = b.header("authorization", a);
        }
        for (k, v) in &req.headers {
            b = b.header(k.as_str(), v.as_str());
        }
        let bytes = match &req.body {
            ReqBody::None => Vec::new(),
            ReqBody::Json(v) => serde_json::to_vec(&with_passwords(v, &self.seed)).unwrap(),
            ReqBody::Text(t) => t.clone().into_bytes(),
            ReqBody::Cbz(pages) => cbz(*pages),
        };
        if !bytes.is_empty() {
            b = b.header("content-length", bytes.len().to_string());
        }
        let resp = self
            .app
            .clone()
            .oneshot(b.body(Body::from(bytes)).unwrap())
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Resp {
            status,
            headers,
            body,
        }
    }

    pub async fn stop(self) {
        self.stop.cancel();
        if let Some(r) = self.running {
            let _ = tokio::time::timeout(Duration::from_secs(10), r).await;
        }
        self.services.ocr.stop().await;
        self.services.library.stop().await;
    }
}

// ---- fixtures -----------------------------------------------------------------------

/// Response headers a fixture keeps: what the reader reads, and what the spec names.
pub const KEPT_HEADERS: [&str; 10] = [
    "content-type",
    "etag",
    "www-authenticate",
    "retry-after",
    "vary",
    "dav",
    "allow",
    "location",
    "cache-control",
    "last-modified",
];

fn kept(name: &str) -> bool {
    KEPT_HEADERS.contains(&name)
        || name.starts_with("x-mokuro-")
        || name.starts_with("access-control-")
}

/// A header value with its volatile part replaced.
fn normalize_header(name: &str, value: &str) -> String {
    match name {
        "etag" | "if-none-match" | "if-match" if value != "*" => ETAG_SENTINEL.into(),
        "last-modified" | "date" | "if-modified-since" => HTTP_DATE_SENTINEL.into(),
        // A wait counted down from a fixed length: whole minutes, rounded up.
        "retry-after" => match value.parse::<u64>() {
            Ok(n) if n > 60 => n.div_ceil(60).saturating_mul(60).to_string(),
            _ => value.into(),
        },
        _ => value.into(),
    }
}

/// The fixture a request and its answer make (normalized, keys sorted).
pub fn fixture(req: &Req, resp: &Resp) -> Value {
    let mut request = req.to_json();
    if let Some(Value::Object(h)) = request.get_mut("headers") {
        for (k, v) in h.iter_mut() {
            if let Some(s) = v.as_str() {
                *v = Value::String(normalize_header(k, s));
            }
        }
    }
    let mut headers = Map::new();
    let mut names: Vec<&str> = resp
        .headers
        .keys()
        .map(|k| k.as_str())
        .filter(|k| kept(k))
        .collect();
    names.sort();
    names.dedup();
    for name in names {
        let joined = resp
            .headers
            .get_all(name)
            .iter()
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .collect::<Vec<_>>()
            .join(", ");
        headers.insert(name.to_string(), json!(normalize_header(name, &joined)));
    }
    let mut m = Map::new();
    m.insert("request".into(), request);
    m.insert("status".into(), json!(resp.status));
    m.insert("headers".into(), Value::Object(headers));
    if !resp.body.is_empty() {
        let is_json = resp
            .header("content-type")
            .is_some_and(|c| c.contains("json"));
        match (is_json, resp.json()) {
            (true, Some(v)) => {
                m.insert("body".into(), v);
            }
            _ => {
                m.insert(
                    "body_text".into(),
                    json!(normalize_eol(&String::from_utf8_lossy(&resp.body))),
                );
            }
        }
    }
    normalize(Value::Object(m))
}

/// A file Bunko writes into a library, in the fixture shape.
pub fn file_fixture(path: &str, content: &Value) -> Value {
    normalize(json!({"request": {"file": path}, "body": content}))
}

// ---- the normalizer -----------------------------------------------------------------

/// Keys whose string values are opaque ids that change from run to run.
const ID_KEYS: [&str; 4] = ["instance_id", "boot_id", "took_over_from", "lock_token"];
/// Keys whose string values are a Bunko version.
const VERSION_KEYS: [&str; 2] = ["bunko_version", "server_version"];
/// Keys whose string values name the machine the test ran on.
const HOST_KEYS: [&str; 1] = ["instance_name"];
pub const HOST_SENTINEL: &str = "contract-host";

#[derive(Default)]
struct Normalizer {
    uuids: HashMap<String, usize>,
    ids: HashMap<String, usize>,
}

/// Replace volatile values with typed sentinels (see the README), sort object keys.
/// Idempotent: normalizing a normalized value changes nothing.
pub fn normalize(v: Value) -> Value {
    Normalizer::default().value(v, None)
}

/// [`normalize`] for text (XML, plain): line endings, times, ETags, UUIDs.
pub fn normalize_text(s: &str) -> String {
    Normalizer::default().text(&normalize_eol(s))
}

pub fn normalize_eol(s: &str) -> String {
    s.replace("\r\n", "\n")
}

impl Normalizer {
    fn value(&mut self, v: Value, key: Option<&str>) -> Value {
        match v {
            Value::Object(m) => {
                let mut entries: Vec<(String, Value)> = m.into_iter().collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                let mut out = Map::new();
                for (k, v) in entries {
                    let nv = self.value(v, Some(&k));
                    out.insert(k, nv);
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.into_iter().map(|x| self.value(x, key)).collect()),
            Value::Number(n) => {
                let f = n.as_f64().unwrap_or(0.0);
                if (1e9..1e10).contains(&f) {
                    json!(EPOCH_S)
                } else if (1e12..1e13).contains(&f) {
                    json!(EPOCH_MS)
                } else {
                    Value::Number(n)
                }
            }
            Value::String(s) => Value::String(self.string(&s, key)),
            other => other,
        }
    }

    fn string(&mut self, s: &str, key: Option<&str>) -> String {
        match key {
            Some("token") if !s.is_empty() => return TOKEN_SENTINEL.into(),
            Some("etag") if !s.is_empty() => return ETAG_SENTINEL.into(),
            Some(k) if HOST_KEYS.contains(&k) && !s.is_empty() => return HOST_SENTINEL.into(),
            Some(k) if VERSION_KEYS.contains(&k) && !s.is_empty() => {
                return VERSION_SENTINEL.into();
            }
            Some(k) if ID_KEYS.contains(&k) && !s.is_empty() && !is_uuid(s) => {
                return self.id(s);
            }
            _ => {}
        }
        self.text(s)
    }

    fn id(&mut self, s: &str) -> String {
        if s.starts_with("<id-") {
            return s.to_string();
        }
        let next = self.ids.len() + 1;
        let n = *self.ids.entry(s.to_string()).or_insert(next);
        format!("<id-{n}>")
    }

    fn uuid(&mut self, s: &str) -> String {
        let lower = s.to_ascii_lowercase();
        if let Some(n) = lower
            .strip_prefix("00000000-0000-4000-8000-")
            .and_then(|t| t.parse::<usize>().ok())
        {
            // Already a sentinel: keep its number taken.
            self.uuids.entry(lower.clone()).or_insert(n);
            return lower;
        }
        let next = self.uuids.len() + 1;
        let n = *self.uuids.entry(lower).or_insert(next);
        format!("00000000-0000-4000-8000-{n:012}")
    }

    /// Substring replacement over text: ISO times, HTTP dates, UUIDs, `getetag` values.
    fn text(&mut self, s: &str) -> String {
        let b = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        let mut last = 0;
        while i < b.len() {
            let matched: Option<(usize, String)> = if let Some(n) = http_date_at(b, i) {
                Some((n, HTTP_DATE_SENTINEL.into()))
            } else if let Some(n) = iso_at(b, i) {
                Some((n, ISO_SENTINEL.into()))
            } else if let Some(n) = (i == 0 || !b[i - 1].is_ascii_hexdigit())
                .then(|| uuid_at(b, i))
                .flatten()
            {
                let u = self.uuid(&s[i..i + n]);
                Some((n, u))
            } else if let Some((n, secs)) = retry_in_at(b, i) {
                Some((n, format!("Retry in {secs}s")))
            } else {
                etag_element_at(b, i).map(|n| (n, format!("getetag>{ETAG_SENTINEL}<")))
            };
            match matched {
                Some((n, rep)) => {
                    out.push_str(&s[last..i]);
                    out.push_str(&rep);
                    i += n;
                    last = i;
                }
                None => i += 1,
            }
        }
        out.push_str(&s[last..]);
        out
    }
}

/// The limiter's `Retry in <n>s`, counted down from its block: whole minutes, rounded
/// up (as the `Retry-After` header is). Its length and the rounded seconds.
fn retry_in_at(b: &[u8], i: usize) -> Option<(usize, u64)> {
    const LEAD: &[u8] = b"Retry in ";
    if !b[i..].starts_with(LEAD) {
        return None;
    }
    let start = i + LEAD.len();
    let n = b[start..].iter().take_while(|c| c.is_ascii_digit()).count();
    if n == 0 || b.get(start + n) != Some(&b's') {
        return None;
    }
    let secs: u64 = std::str::from_utf8(&b[start..start + n])
        .ok()?
        .parse()
        .ok()?;
    let rounded = if secs > 60 {
        secs.div_ceil(60) * 60
    } else {
        secs
    };
    Some((LEAD.len() + n + 1, rounded))
}

fn digits(b: &[u8], at: usize, n: usize) -> bool {
    b.len() >= at + n && b[at..at + n].iter().all(u8::is_ascii_digit)
}

/// `YYYY-MM-DD[T ]HH:MM:SS[.fff][Z|+HH:MM]` at `i`: its length.
fn iso_at(b: &[u8], i: usize) -> Option<usize> {
    let ok = digits(b, i, 4)
        && b.get(i + 4) == Some(&b'-')
        && digits(b, i + 5, 2)
        && b.get(i + 7) == Some(&b'-')
        && digits(b, i + 8, 2)
        && matches!(b.get(i + 10), Some(b'T') | Some(b' '))
        && digits(b, i + 11, 2)
        && b.get(i + 13) == Some(&b':')
        && digits(b, i + 14, 2)
        && b.get(i + 16) == Some(&b':')
        && digits(b, i + 17, 2);
    if !ok || (i > 0 && b[i - 1].is_ascii_digit()) {
        return None;
    }
    let mut j = i + 19;
    if b.get(j) == Some(&b'.') {
        j += 1;
        while b.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
    }
    if b.get(j) == Some(&b'Z') {
        j += 1;
    } else if matches!(b.get(j), Some(b'+') | Some(b'-'))
        && digits(b, j + 1, 2)
        && b.get(j + 3) == Some(&b':')
        && digits(b, j + 4, 2)
    {
        j += 6;
    }
    Some(j - i)
}

/// `Sat, 01 Jan 2000 00:00:00 GMT` at `i`.
fn http_date_at(b: &[u8], i: usize) -> Option<usize> {
    const DAYS: [&[u8]; 7] = [b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat", b"Sun"];
    const MONTHS: [&[u8]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];
    if b.len() < i + 29 {
        return None;
    }
    let w = &b[i..i + 29];
    let ok = DAYS.contains(&&w[0..3])
        && &w[3..5] == b", "
        && digits(w, 5, 2)
        && w[7] == b' '
        && MONTHS.contains(&&w[8..11])
        && w[11] == b' '
        && digits(w, 12, 4)
        && w[16] == b' '
        && digits(w, 17, 2)
        && w[19] == b':'
        && digits(w, 20, 2)
        && w[22] == b':'
        && digits(w, 23, 2)
        && &w[25..29] == b" GMT";
    ok.then_some(29)
}

/// A UUID at `i` (`8-4-4-4-12` hex), or a generated id of its shape with a shorter last
/// group (`8-4-4-4-8`, as Bunko makes for a volume without a `.mokuro`): its length.
fn uuid_at(b: &[u8], i: usize) -> Option<usize> {
    if b.len() < i + 32 {
        return None;
    }
    let head = b[i..i + 24].iter().enumerate().all(|(k, c)| match k {
        8 | 13 | 18 | 23 => *c == b'-',
        _ => c.is_ascii_hexdigit(),
    });
    if !head {
        return None;
    }
    let tail = b[i + 24..]
        .iter()
        .take(13)
        .take_while(|c| c.is_ascii_hexdigit())
        .count();
    (8..=12).contains(&tail).then_some(24 + tail)
}

fn is_uuid(s: &str) -> bool {
    uuid_at(s.as_bytes(), 0) == Some(s.len())
}

/// `getetag>…<` (any namespace prefix before it): the length through the `<`.
fn etag_element_at(b: &[u8], i: usize) -> Option<usize> {
    const OPEN: &[u8] = b"getetag>";
    if !b[i..].starts_with(OPEN) {
        return None;
    }
    // An opening tag only: the `<` before the name is not followed by `/`.
    let lt = b[..i].iter().rposition(|c| *c == b'<')?;
    if b.get(lt + 1) == Some(&b'/') {
        return None;
    }
    let start = i + OPEN.len();
    let end = b[start..].iter().position(|c| *c == b'<')?;
    Some(OPEN.len() + end + 1)
}

// ---- golden files -------------------------------------------------------------------

/// The fixture as committed: pretty JSON, `\n` line endings, a final newline.
pub fn render(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap();
    s.push('\n');
    s
}

/// Golden compare for one fixture directory: collects every mismatch, and with
/// `BUNKO_BLESS=1` rewrites the files instead (and removes the ones no longer made).
pub struct Contract {
    dir: PathBuf,
    bless: bool,
    seen: Vec<String>,
    failures: Vec<String>,
}

pub const BLESS_HINT: &str =
    "run BUNKO_BLESS=1 cargo test -p bunko-server --test contract_export to accept";

impl Contract {
    pub fn new(dir: PathBuf) -> Contract {
        let bless = std::env::var("BUNKO_BLESS").is_ok_and(|v| v == "1");
        if bless {
            std::fs::create_dir_all(&dir).unwrap();
        }
        Contract {
            dir,
            bless,
            seen: Vec::new(),
            failures: Vec::new(),
        }
    }

    pub fn golden(&mut self, name: &str, fixture: &Value) {
        assert!(
            !self.seen.iter().any(|n| n == name),
            "fixture {name} exported twice"
        );
        self.seen.push(name.to_string());
        let path = self.dir.join(format!("{name}.json"));
        let want = render(fixture);
        if self.bless {
            std::fs::write(&path, &want).unwrap();
            return;
        }
        match std::fs::read(&path) {
            Ok(have) => {
                let have = normalize_eol(&String::from_utf8_lossy(&have));
                if have != want {
                    self.failures
                        .push(format!("{name}.json differs:\n{}", diff(&have, &want)));
                }
            }
            Err(_) => self
                .failures
                .push(format!("{name}.json is missing; it would be:\n{want}")),
        }
    }

    /// Fail with every mismatch at once; check no committed fixture went unmade.
    pub fn finish(mut self) {
        let mut stale = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(stem) = name.strip_suffix(".json")
                    && !self.seen.iter().any(|n| n == stem)
                {
                    stale.push((e.path(), name));
                }
            }
        }
        for (path, name) in stale {
            if self.bless {
                std::fs::remove_file(path).unwrap();
            } else {
                self.failures
                    .push(format!("{name} is committed but no longer exported"));
            }
        }
        if !self.failures.is_empty() {
            panic!(
                "{} contract fixture(s) differ:\n\n{}\n\n{BLESS_HINT}",
                self.failures.len(),
                self.failures.join("\n\n")
            );
        }
    }
}

/// A unified-style line diff (`-` committed, `+` now), with a little context.
pub fn diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let (n, m) = (a.len(), b.len());
    // Longest common subsequence table, from the end.
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            ops.push(('-', a[i]));
            i += 1;
        } else {
            ops.push(('+', b[j]));
            j += 1;
        }
    }
    const CONTEXT: usize = 2;
    let near: Vec<bool> = (0..ops.len())
        .map(|k| {
            let lo = k.saturating_sub(CONTEXT);
            let hi = (k + CONTEXT + 1).min(ops.len());
            ops[lo..hi].iter().any(|(c, _)| *c != ' ')
        })
        .collect();
    let mut out = String::from("--- committed\n+++ exported\n");
    let mut skipped = false;
    for (k, (c, line)) in ops.iter().enumerate() {
        if near[k] {
            if skipped {
                out.push_str("@@\n");
                skipped = false;
            }
            out.push_str(&format!("{c}{line}\n"));
        } else {
            skipped = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_replaces_each_volatile_kind_and_keeps_types() {
        let v = json!({
            "z": 1,
            "a": {"created_at": "2026-10-28T12:34:56.789Z", "updated": "2026-10-28 12:34:56"},
            "expires_at": 1_793_000_000.25,
            "at_ms": 1_793_000_000_123i64,
            "size": 1234,
            "token": "abcdef0123",
            "etag": "\"5f-1a2b\"",
            "bunko_version": "0.8.0",
            "instance_id": "inst-8c1f",
            "ids": ["3f2b8c4e-1d2a-4b7c-9e0f-a1b2c3d4e5f6", "9a9a9a9a-0000-4111-8222-333344445555",
                "3F2B8C4E-1D2A-4B7C-9E0F-A1B2C3D4E5F6"],
            "detail": "Modified Wed, 28 Oct 2026 10:00:00 GMT by x",
            "error": "Too many failed attempts. Retry in 899s",
            "volume_uuid": "1a04d289-f618-43eb-ac1c-6162101d",
            "instance_name": "some-desktop",
            "xml": "<D:getetag>\"abc\"</D:getetag>",
            "sha": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        });
        let n = normalize(v.clone());
        let want = json!({
            "a": {"created_at": ISO_SENTINEL, "updated": ISO_SENTINEL},
            "at_ms": EPOCH_MS,
            "bunko_version": VERSION_SENTINEL,
            "detail": format!("Modified {HTTP_DATE_SENTINEL} by x"),
            "error": "Too many failed attempts. Retry in 900s",
            "volume_uuid": "00000000-0000-4000-8000-000000000003",
            "instance_name": HOST_SENTINEL,
            "etag": ETAG_SENTINEL,
            "expires_at": EPOCH_S,
            "ids": ["00000000-0000-4000-8000-000000000001", "00000000-0000-4000-8000-000000000002",
                "00000000-0000-4000-8000-000000000001"],
            "instance_id": "<id-1>",
            "sha": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "size": 1234,
            "token": TOKEN_SENTINEL,
            "xml": format!("<D:getetag>{ETAG_SENTINEL}</D:getetag>"),
            "z": 1,
        });
        assert_eq!(n, want);
        // Keys come out sorted, whatever order they went in.
        let keys: Vec<&String> = n.as_object().unwrap().keys().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        // Twice is once.
        assert_eq!(normalize(n.clone()), n);
        assert_eq!(render(&normalize(v)), render(&n));
    }

    #[test]
    fn normalize_text_handles_xml_and_line_endings() {
        let xml = "<a>\r\n<D:creationdate>2026-10-28T01:02:03Z</D:creationdate>\r\n\
<D:getlastmodified>Wed, 28 Oct 2026 01:02:03 GMT</D:getlastmodified>\r\n\
<D:getetag>\"x-1\"</D:getetag><D:locktoken>opaquelocktoken:3f2b8c4e-1d2a-4b7c-9e0f-a1b2c3d4e5f6</D:locktoken></a>";
        let n = normalize_text(xml);
        assert_eq!(
            n,
            "<a>\n<D:creationdate>2000-01-01T00:00:00Z</D:creationdate>\n\
<D:getlastmodified>Sat, 01 Jan 2000 00:00:00 GMT</D:getlastmodified>\n\
<D:getetag>\"etag\"</D:getetag><D:locktoken>opaquelocktoken:00000000-0000-4000-8000-000000000001</D:locktoken></a>"
        );
        assert_eq!(normalize_text(&n), n);
    }

    #[test]
    fn the_seed_archives_are_byte_identical() {
        assert_eq!(cbz(2), cbz(2));
        assert_eq!(encode("第1巻"), "%E7%AC%AC1%E5%B7%BB");
        assert_eq!(encode("Volume 01"), "Volume%2001");
    }

    #[test]
    fn diff_marks_changed_lines() {
        let d = diff("a\nb\nc\n", "a\nB\nc\n");
        assert!(d.contains("-b\n+B\n"), "{d}");
    }
}
