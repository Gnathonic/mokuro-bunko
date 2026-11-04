//! Rough geography (design §9): the country of a client IP from DB-IP "IP to Country
//! Lite" (CC BY 4.0), downloaded on demand when `analytics.geo.enabled` (opt-in).
//!
//! - **Lookups** ([`Geo::country`], once per request, WebDAV and web alike) are in
//!   memory: loopback, private, link-local, CGNAT and ULA addresses are [`Country::LAN`]
//!   without a lookup; the rest go through a bounded two-generation cache in front of
//!   the `maxminddb` reader (ISC). An address the database does not place is
//!   [`Country::UNKNOWN`] (`ZZ`, as DB-IP itself spells it). Off, or before a database
//!   is loaded, every answer is `None`.
//! - **The database** lives at `<storage base>/analytics/country.mmdb`. A background
//!   task (never on the startup path) loads it, and downloads `analytics.geo.url` with
//!   `{year}`/`{month}` filled in when it is missing or not this month's: the current
//!   month first, the previous one when that is not out yet (404). The download is
//!   capped at [`MAX_BYTES`] compressed and uncompressed, gunzipped, opened and fully
//!   verified before it atomically replaces the file; a failed or corrupt download
//!   keeps the database there was. It refreshes monthly, on `POST .../geo/refresh`, and
//!   when the settings turn it on or change its URL. Failures are logged and shown in
//!   the admin view; nothing is retried faster than [`RETRY_AFTER_ERROR`].

use crate::analytics::Analytics;
use crate::core::Core;
use bunko_core::config::AnalyticsConfig;
use maxminddb::{PathElement, Reader};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::io::Read;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// An ISO 3166-1 alpha-2 code, or `LAN` (loopback, private, link-local). `Copy`, so the
/// per-request path never allocates for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Country {
    code: [u8; 3],
    len: u8,
}

impl Country {
    pub const LAN: Country = Country {
        code: *b"LAN",
        len: 3,
    };

    /// An address the loaded database does not place (DB-IP's own `ZZ`).
    pub const UNKNOWN: Country = Country {
        code: *b"ZZ\0",
        len: 2,
    };

    /// 2 or 3 ASCII letters, upper-cased; anything else is `None`.
    pub fn new(code: &str) -> Option<Country> {
        let b = code.trim().as_bytes();
        if !(2..=3).contains(&b.len()) || !b.iter().all(u8::is_ascii_alphabetic) {
            return None;
        }
        let mut out = [0u8; 3];
        for (o, c) in out.iter_mut().zip(b) {
            *o = c.to_ascii_uppercase();
        }
        Some(Country {
            code: out,
            len: b.len() as u8,
        })
    }

    pub fn as_str(&self) -> &str {
        // Built from ASCII letters only (`new`, `LAN`).
        std::str::from_utf8(&self.code[..usize::from(self.len)]).unwrap_or("")
    }
}

impl std::fmt::Display for Country {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The database file, under `<storage base>/analytics/`.
pub const DB_FILE: &str = "country.mmdb";
/// Largest download accepted, compressed and uncompressed (the real file is a few MB).
pub const MAX_BYTES: u64 = 64 << 20;
/// Addresses per cache generation (two generations: the design's 16 384 in all).
pub const CACHE_PER_GENERATION: usize = 8192;
/// After a failed download.
pub const RETRY_AFTER_ERROR: i64 = 6 * 3600;
/// While only the previous month's database is out.
const RETRY_PREVIOUS_MONTH: i64 = 24 * 3600;
/// After the 1st of a month, how long before looking for its database.
const MONTH_GRACE: i64 = 3600;
/// A URL without `{year}`/`{month}` is fetched again after this long.
const UNDATED_REFRESH: i64 = 30 * 86400;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// What the licence asks the UI to show.
pub const ATTRIBUTION: &str = "IP geolocation by DB-IP";
pub const ATTRIBUTION_URL: &str = "https://db-ip.com";
pub const LICENSE: &str = "CC BY 4.0";

/// `meta` keys of the analytics database.
mod keys {
    /// The edition (`YYYY-MM` from the URL, or the build date) of the stored file.
    pub const VERSION: &str = "geo_version";
    /// Epoch seconds the stored file was downloaded.
    pub const DOWNLOADED_AT: &str = "geo_downloaded_at";
}

/// A loaded database and what the admin view says about it.
struct Loaded {
    reader: Reader<Vec<u8>>,
    info: DbInfo,
    /// Bumped per load, so a lookup that raced a reload never caches the old answer.
    generation: u64,
}

/// A database read and verified, and what to show about it.
type Opened = (Reader<Vec<u8>>, DbInfo);

/// The loaded database, as the admin view shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbInfo {
    pub version: String,
    pub downloaded_at: Option<i64>,
    pub built_at: i64,
    pub database_type: String,
    pub size: u64,
}

#[derive(Default)]
struct Cache {
    generation: u64,
    current: HashMap<IpAddr, Country>,
    previous: HashMap<IpAddr, Country>,
}

impl Cache {
    fn get(&mut self, ip: &IpAddr) -> Option<Country> {
        if let Some(c) = self.current.get(ip) {
            return Some(*c);
        }
        let c = *self.previous.get(ip)?;
        self.put(*ip, c);
        Some(c)
    }

    fn put(&mut self, ip: IpAddr, c: Country) {
        if self.current.len() >= CACHE_PER_GENERATION {
            self.previous = std::mem::take(&mut self.current);
        }
        self.current.insert(ip, c);
    }

    fn reset(&mut self, generation: u64) {
        self.generation = generation;
        self.current = HashMap::new();
        self.previous = HashMap::new();
    }
}

/// The downloader's state, as the admin view shows it.
#[derive(Debug, Clone, Default)]
struct State {
    /// The last attempt's failure (cleared by a success).
    error: Option<String>,
    checked_at: Option<i64>,
    /// When the background task looks again.
    next_at: Option<i64>,
}

struct Shared {
    enabled: AtomicBool,
    url: Mutex<String>,
    db: RwLock<Option<Arc<Loaded>>>,
    generation: AtomicU64,
    cache: Mutex<Cache>,
    state: Mutex<State>,
    /// A download is running.
    busy: AtomicBool,
    /// The next pass downloads even when the stored file is current (refresh, URL
    /// change).
    force: AtomicBool,
    wake: Notify,
    client: reqwest::Client,
}

/// The lookup and its downloader.
pub struct Geo {
    core: Core,
    shared: Arc<Shared>,
}

/// What `GET .../geo` reports.
#[derive(Debug, Clone)]
pub struct GeoStatus {
    pub enabled: bool,
    /// `off`, `downloading`, `ready` or `error`.
    pub status: &'static str,
    pub error: Option<String>,
    pub db: Option<DbInfo>,
    pub checked_at: Option<i64>,
    pub next_check_at: Option<i64>,
}

impl Geo {
    pub fn new(core: Core) -> Geo {
        let geo = core.config.read().analytics.geo.clone();
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(DOWNLOAD_TIMEOUT)
            .user_agent(concat!(
                "mokuro-bunko/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/Gnathonic/mokuro-bunko)"
            ))
            .build()
            .unwrap_or_default();
        Geo {
            core,
            shared: Arc::new(Shared {
                enabled: AtomicBool::new(geo.enabled),
                url: Mutex::new(geo.url),
                db: RwLock::new(None),
                generation: AtomicU64::new(0),
                cache: Mutex::new(Cache::default()),
                state: Mutex::new(State::default()),
                busy: AtomicBool::new(false),
                force: AtomicBool::new(false),
                wake: Notify::new(),
                client,
            }),
        }
    }

    /// `<storage base>/analytics/country.mmdb`.
    pub fn db_path(&self) -> PathBuf {
        db_path(&self.core)
    }

    /// Start the downloader / monthly refresh (when enabled). Called once at start;
    /// returns at once (the stored file is loaded by the task).
    pub fn start(&self, a: &Analytics, stop: CancellationToken) {
        let shared = self.shared.clone();
        let a = a.clone();
        tokio::spawn(run(shared, a, stop));
    }

    /// Synchronous and cheap (an in-memory LRU in front of the mmdb): the country of
    /// `ip`, `None` when off, not loaded or unknown.
    pub fn country(&self, ip: &IpAddr) -> Option<Country> {
        if !self.shared.enabled.load(Ordering::Relaxed) {
            return None;
        }
        let ip = canonical(*ip);
        if is_lan(&ip) {
            return Some(Country::LAN);
        }
        let db = self.shared.db.read().clone()?;
        {
            let mut cache = self.shared.cache.lock();
            if cache.generation == db.generation
                && let Some(c) = cache.get(&ip)
            {
                return Some(c);
            }
        }
        let c = lookup(&db.reader, ip)?;
        let mut cache = self.shared.cache.lock();
        if cache.generation == db.generation {
            cache.put(ip, c);
        }
        Some(c)
    }

    /// Download the database again now (`POST .../geo/refresh`). `false` when the
    /// lookup is off (nothing is downloaded then).
    pub fn refresh(&self) -> bool {
        if !self.shared.enabled.load(Ordering::Relaxed) {
            return false;
        }
        self.shared.force.store(true, Ordering::Relaxed);
        self.shared.wake.notify_one();
        true
    }

    /// `analytics.geo` changed (enabled toggled, URL): start or stop accordingly.
    pub fn settings_changed(&self, cfg: &AnalyticsConfig) {
        let was = self.shared.enabled.swap(cfg.geo.enabled, Ordering::Relaxed);
        let url_changed = {
            let mut url = self.shared.url.lock();
            let changed = *url != cfg.geo.url;
            if changed {
                url.clone_from(&cfg.geo.url);
            }
            changed
        };
        if cfg.geo.enabled && url_changed {
            self.shared.force.store(true, Ordering::Relaxed);
        }
        if was != cfg.geo.enabled || url_changed {
            self.shared.wake.notify_one();
        }
    }

    /// The state the admin view shows.
    pub fn status(&self) -> GeoStatus {
        let enabled = self.shared.enabled.load(Ordering::Relaxed);
        let db = self.shared.db.read().as_ref().map(|l| l.info.clone());
        let st = self.shared.state.lock().clone();
        let status = if !enabled {
            "off"
        } else if self.shared.busy.load(Ordering::Relaxed) {
            "downloading"
        } else if st.error.is_some() {
            "error"
        } else if db.is_some() {
            "ready"
        } else {
            // On, nothing loaded and nothing failed yet: the first download is queued.
            "downloading"
        };
        GeoStatus {
            enabled,
            status,
            error: st.error,
            db,
            checked_at: st.checked_at,
            next_check_at: st.next_at.filter(|_| enabled),
        }
    }

    /// Load the stored file, then download one if it is missing or not current (or
    /// always, with `force`). The background task's one pass; tests call it to run a
    /// pass in line. `Err` is the failure the admin view shows.
    pub async fn update_now(&self, a: &Analytics, force: bool) -> Result<(), String> {
        pass(&self.shared, a, force).await
    }
}

fn db_path(core: &Core) -> PathBuf {
    core.layout.base.join("analytics").join(DB_FILE)
}

/// An IPv4-mapped IPv6 address as the IPv4 address it is.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// Loopback, private, link-local, CGNAT (100.64/10), unspecified; IPv6 loopback,
/// unique-local (fc00::/7) and link-local (fe80::/10).
pub fn is_lan(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            let s0 = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (s0 & 0xfe00) == 0xfc00
                || (s0 & 0xffc0) == 0xfe80
        }
    }
}

const COUNTRY_ISO: [PathElement<'static>; 2] =
    [PathElement::Key("country"), PathElement::Key("iso_code")];
const REGISTERED_ISO: [PathElement<'static>; 2] = [
    PathElement::Key("registered_country"),
    PathElement::Key("iso_code"),
];

/// `country.iso_code` (else `registered_country.iso_code`) of `ip`; an address with
/// no record is [`Country::UNKNOWN`]; `None` when the database cannot answer (an IPv6
/// address in an IPv4-only database, a damaged record).
fn lookup<S: AsRef<[u8]>>(reader: &Reader<S>, ip: IpAddr) -> Option<Country> {
    let found = reader.lookup(ip).ok()?;
    if !found.has_data() {
        return Some(Country::UNKNOWN);
    }
    let code = found
        .decode_path::<&str>(&COUNTRY_ISO)
        .ok()
        .flatten()
        .or_else(|| found.decode_path::<&str>(&REGISTERED_ISO).ok().flatten());
    Some(code.and_then(Country::new).unwrap_or(Country::UNKNOWN))
}

/// `(year, month)` of epoch seconds `t`, UTC.
fn year_month(t: i64) -> (i32, u8) {
    match time::OffsetDateTime::from_unix_timestamp(t) {
        Ok(d) => (d.year(), u8::from(d.month())),
        Err(_) => (1970, 1),
    }
}

fn previous_month((y, m): (i32, u8)) -> (i32, u8) {
    if m <= 1 { (y - 1, 12) } else { (y, m - 1) }
}

/// Epoch seconds of the 1st of the month after `(y, m)`, 00:00 UTC.
fn next_month_start((y, m): (i32, u8)) -> i64 {
    let (y, m) = if m >= 12 { (y + 1, 1) } else { (y, m + 1) };
    time::Month::try_from(m)
        .ok()
        .and_then(|month| time::Date::from_calendar_date(y, month, 1).ok())
        .map_or(i64::MAX, |d| d.midnight().assume_utc().unix_timestamp())
}

fn tag((y, m): (i32, u8)) -> String {
    format!("{y:04}-{m:02}")
}

/// Is the URL dated (`{year}`/`{month}`)?
fn dated(template: &str) -> bool {
    template.contains("{year}") || template.contains("{month}")
}

fn fill(template: &str, (y, m): (i32, u8)) -> String {
    template
        .replace("{year}", &format!("{y:04}"))
        .replace("{month}", &format!("{m:02}"))
}

/// When to look for a newer database while holding edition `version` (downloaded at
/// `downloaded_at`), last looked for at `checked_at` (`None`: not in this run).
fn due_at(
    template: &str,
    version: &str,
    downloaded_at: Option<i64>,
    checked_at: Option<i64>,
    now: i64,
) -> i64 {
    if !dated(template) {
        return downloaded_at.unwrap_or(0) + UNDATED_REFRESH;
    }
    let current = year_month(now);
    if version == tag(current) {
        next_month_start(current) + MONTH_GRACE
    } else {
        // This month's is not held: look now, then daily while it is not out.
        checked_at.map_or(now, |c| c + RETRY_PREVIOUS_MONTH)
    }
}

enum Fetch {
    Body(Vec<u8>),
    NotFound,
}

/// GET `url` into memory, at most [`MAX_BYTES`].
async fn fetch(client: &reqwest::Client, url: &str) -> Result<Fetch, String> {
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("could not reach the download ({e})"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(Fetch::NotFound);
    }
    if !status.is_success() {
        return Err(format!("the download answered HTTP {}", status.as_u16()));
    }
    if resp.content_length().is_some_and(|n| n > MAX_BYTES) {
        return Err(format!(
            "the download is larger than {} MiB",
            MAX_BYTES >> 20
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("the download broke off ({e})"))?
    {
        if body.len() as u64 + chunk.len() as u64 > MAX_BYTES {
            return Err(format!(
                "the download is larger than {} MiB",
                MAX_BYTES >> 20
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Fetch::Body(body))
}

/// The mmdb in `body`: gunzipped when it is gzip, at most [`MAX_BYTES`] either way.
fn decompress(body: Vec<u8>) -> Result<Vec<u8>, String> {
    if !body.starts_with(&[0x1f, 0x8b]) {
        return Ok(body);
    }
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(&body[..])
        .take(MAX_BYTES + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("the download is not a valid gzip file ({e})"))?;
    if out.len() as u64 > MAX_BYTES {
        return Err(format!(
            "the database is larger than {} MiB uncompressed",
            MAX_BYTES >> 20
        ));
    }
    Ok(out)
}

/// Open and fully verify `raw` as a country database.
fn open_verified(raw: Vec<u8>) -> Result<Reader<Vec<u8>>, String> {
    let reader =
        Reader::from_source(raw).map_err(|e| format!("not a MaxMind DB database ({e})"))?;
    reader
        .verify()
        .map_err(|e| format!("the database is damaged ({e})"))?;
    Ok(reader)
}

fn info_of(
    reader: &Reader<Vec<u8>>,
    version: Option<String>,
    downloaded_at: Option<i64>,
    size: u64,
) -> DbInfo {
    let m = reader.metadata();
    let built_at = i64::try_from(m.build_epoch).unwrap_or(0);
    DbInfo {
        version: version.unwrap_or_else(|| {
            time::OffsetDateTime::from_unix_timestamp(built_at)
                .map(|d| d.date().to_string())
                .unwrap_or_default()
        }),
        downloaded_at,
        built_at,
        database_type: m.database_type.clone(),
        size,
    }
}

fn install(shared: &Shared, reader: Reader<Vec<u8>>, info: DbInfo) {
    let generation = shared.generation.fetch_add(1, Ordering::Relaxed) + 1;
    *shared.db.write() = Some(Arc::new(Loaded {
        reader,
        info,
        generation,
    }));
    shared.cache.lock().reset(generation);
}

fn unload(shared: &Shared) {
    *shared.db.write() = None;
    let generation = shared.generation.fetch_add(1, Ordering::Relaxed) + 1;
    shared.cache.lock().reset(generation);
}

/// Load the stored file (when nothing is loaded yet). A file that does not open is
/// left for the next download to replace.
async fn load_stored(shared: &Arc<Shared>, a: &Analytics) {
    if shared.db.read().is_some() {
        return;
    }
    let path = db_path(a.core());
    let adb = a.db().clone();
    let loaded = tokio::task::spawn_blocking(move || -> Result<Option<Opened>, String> {
        if !bunko_store::fs::is_file(&path) {
            return Ok(None);
        }
        let meta = bunko_store::fs::metadata(&path).map_err(|e| e.to_string())?;
        if meta.len() > MAX_BYTES {
            return Err(format!(
                "{} is larger than {} MiB",
                path.display(),
                MAX_BYTES >> 20
            ));
        }
        let raw = bunko_store::fs::read(&path).map_err(|e| e.to_string())?;
        let size = raw.len() as u64;
        let reader = open_verified(raw)?;
        let version = adb.meta_get(keys::VERSION).ok().flatten();
        let downloaded_at = adb
            .meta_i64(keys::DOWNLOADED_AT)
            .ok()
            .flatten()
            .or_else(|| meta.unix_mtime().map(|(s, _)| s));
        let info = info_of(&reader, version, downloaded_at, size);
        Ok(Some((reader, info)))
    })
    .await;
    match loaded {
        Ok(Ok(Some((reader, info)))) => {
            tracing::info!(
                "[ANALYTICS] geo: loaded the country database {} ({})",
                info.version,
                info.database_type
            );
            install(shared, reader, info);
        }
        Ok(Ok(None)) => {}
        Ok(Err(e)) => tracing::warn!(
            "[ANALYTICS] geo: the stored country database cannot be used ({e}); downloading a new one"
        ),
        Err(e) => tracing::warn!("[ANALYTICS] geo: loading the country database failed: {e}"),
    }
}

/// One pass: load what is stored, then download when due (or `force`d). Records the
/// outcome for the admin view and when to look next.
async fn pass(shared: &Arc<Shared>, a: &Analytics, force: bool) -> Result<(), String> {
    if !a.config().geo.enabled {
        return Ok(());
    }
    load_stored(shared, a).await;
    let now = super::now_s();
    let template = a.config().geo.url;
    let held = shared.db.read().as_ref().map(|l| l.info.clone());
    let (next_at, checked_at) = {
        let st = shared.state.lock();
        (st.next_at, st.checked_at)
    };
    let due = force
        || match (next_at, &held) {
            (Some(t), _) => t <= now,
            (None, None) => true,
            (None, Some(i)) => {
                due_at(&template, &i.version, i.downloaded_at, checked_at, now) <= now
            }
        };
    if !due {
        if next_at.is_none()
            && let Some(i) = &held
        {
            shared.state.lock().next_at = Some(due_at(
                &template,
                &i.version,
                i.downloaded_at,
                checked_at,
                now,
            ));
        }
        return Ok(());
    }
    if shared.busy.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    let outcome = download(shared, a, &template, held.as_ref(), force, now).await;
    shared.busy.store(false, Ordering::Release);
    let mut st = shared.state.lock();
    st.checked_at = Some(now);
    match outcome {
        Ok(info) => {
            st.error = None;
            st.next_at = Some(due_at(
                &template,
                &info.version,
                info.downloaded_at,
                Some(now),
                now,
            ));
            Ok(())
        }
        Err(e) => {
            tracing::warn!("[ANALYTICS] geo: country database update failed: {e}");
            st.error = Some(e.clone());
            st.next_at = Some(now + RETRY_AFTER_ERROR);
            Err(e)
        }
    }
}

/// Fetch the current month's database (the previous month's on a 404), verify it,
/// write it in place atomically and load it. Returns what is loaded afterwards (the
/// held edition when the only one out is the one held).
async fn download(
    shared: &Arc<Shared>,
    a: &Analytics,
    template: &str,
    held: Option<&DbInfo>,
    force: bool,
    now: i64,
) -> Result<DbInfo, String> {
    let current = year_month(now);
    let candidates: Vec<(String, Option<String>)> = if dated(template) {
        let prev = previous_month(current);
        vec![
            (fill(template, current), Some(tag(current))),
            (fill(template, prev), Some(tag(prev))),
        ]
    } else {
        vec![(template.to_string(), None)]
    };
    let mut got = None;
    for (i, (url, version)) in candidates.iter().enumerate() {
        if !force
            && let (Some(h), Some(v)) = (held, version)
            && h.version == *v
        {
            // The newest one out is the one held.
            return Ok(h.clone());
        }
        match fetch(&shared.client, url).await? {
            Fetch::Body(b) => {
                got = Some((b, version.clone()));
                break;
            }
            Fetch::NotFound if i + 1 < candidates.len() => {
                tracing::info!(
                    "[ANALYTICS] geo: {url} is not out yet (404), trying the previous month"
                );
            }
            Fetch::NotFound => return Err(format!("the download answered HTTP 404 ({url})")),
        }
    }
    let Some((body, version)) = got else {
        return Err("no database to download".into());
    };
    let path = db_path(a.core());
    let adb = a.db().clone();
    let (reader, info) = tokio::task::spawn_blocking(move || -> Result<Opened, String> {
        let raw = decompress(body)?;
        // Verified before anything is written: a bad download keeps the old file.
        let check = Reader::from_source(&raw[..])
            .map_err(|e| format!("not a MaxMind DB database ({e})"))?;
        check
            .verify()
            .map_err(|e| format!("the database is damaged ({e})"))?;
        drop(check);
        let size = raw.len() as u64;
        if let Some(dir) = path.parent() {
            bunko_store::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {} ({e})", dir.display()))?;
        }
        let staged = path.with_extension("mmdb.download");
        bunko_store::fs::write(&staged, &raw)
            .map_err(|e| format!("cannot write {} ({e})", staged.display()))?;
        if let Err(e) = bunko_store::fs::rename(&staged, &path) {
            let _ = bunko_store::fs::remove_file(&staged);
            return Err(format!("cannot replace {} ({e})", path.display()));
        }
        let reader =
            Reader::from_source(raw).map_err(|e| format!("not a MaxMind DB database ({e})"))?;
        let downloaded_at = super::now_s();
        let info = info_of(&reader, version, Some(downloaded_at), size);
        // Bookkeeping only: the file is what counts.
        if let Err(e) = adb.write(|tx| {
            tx.meta_set(keys::VERSION, &info.version)?;
            tx.meta_set(keys::DOWNLOADED_AT, &downloaded_at.to_string())
        }) {
            tracing::warn!("[ANALYTICS] geo: could not record the database version: {e}");
        }
        Ok((reader, info))
    })
    .await
    .map_err(|e| format!("the update task failed ({e})"))??;
    tracing::info!(
        "[ANALYTICS] geo: installed the country database {} ({}, {} bytes)",
        info.version,
        info.database_type,
        info.size
    );
    install(shared, reader, info.clone());
    Ok(info)
}

/// The background task: a pass at start, then whenever it is due, refreshed or the
/// settings change; off, the database is dropped from memory.
async fn run(shared: Arc<Shared>, a: Analytics, stop: CancellationToken) {
    loop {
        let enabled = a.config().geo.enabled;
        shared.enabled.store(enabled, Ordering::Relaxed);
        let wait = if enabled {
            let force = shared.force.swap(false, Ordering::Relaxed);
            let _ = pass(&shared, &a, force).await;
            let next = shared.state.lock().next_at;
            next.map(|t| {
                Duration::from_secs(u64::try_from(t - super::now_s()).unwrap_or(0).max(60))
            })
        } else {
            if shared.db.read().is_some() {
                unload(&shared);
            }
            None
        };
        let sleep = async {
            match wait {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            () = stop.cancelled() => return,
            () = shared.wake.notified() => {}
            () = sleep => {}
        }
    }
}

pub mod api {
    use super::{ATTRIBUTION, ATTRIBUTION_URL, LICENSE};
    use crate::analytics::Analytics;
    use crate::analytics::api::{ApiQuery, answer, query};
    use axum::response::Response;
    use serde_json::{Value, json};

    /// `GET /_admin/api/analytics/geo` (design §10).
    pub async fn geo(a: &Analytics, q: &ApiQuery) -> Response {
        let st = a.geo().status();
        let db = a.db().clone();
        let (d0, d1) = q.range.days();
        let rows = query("analytics geo", move || db.countries(d0, d1)).await;
        let rows = match rows {
            Ok(r) => r,
            Err(r) => return r,
        };
        let countries: Vec<Value> = rows
            .into_iter()
            .map(|(country, requests, bytes_out)| {
                json!({"country": country, "requests": requests, "bytes_out": bytes_out})
            })
            .collect();
        let db_info = st.db.as_ref().map(|d| {
            json!({
                "version": d.version,
                "downloaded_at": d.downloaded_at,
                "built_at": d.built_at,
                "database_type": d.database_type,
                "size": d.size,
                "attribution": ATTRIBUTION,
                "attribution_url": ATTRIBUTION_URL,
                "license": LICENSE,
            })
        });
        answer(
            a,
            q,
            json!({
                "analytics_enabled": a.config().enabled,
                "enabled": st.enabled,
                "status": st.status,
                "error": st.error,
                "db": db_info,
                "attribution": {"text": ATTRIBUTION, "url": ATTRIBUTION_URL, "license": LICENSE},
                "checked_at": st.checked_at,
                "next_check_at": st.next_check_at,
                "countries": countries,
                "series_granularity": "day",
            }),
        )
    }

    /// `POST /_admin/api/analytics/geo/refresh`: `{ok: true, started}` (`started` is
    /// false when the lookup is off).
    pub async fn refresh(a: &Analytics) -> Response {
        let started = a.geo().refresh();
        let mut body = json!({"ok": true, "started": started});
        if !started {
            body["reason"] = json!("off");
        }
        crate::admin::ok(body)
    }
}

/// A tiny MaxMind DB writer for tests: an IPv6 tree (IPv4 under `::/96`, as DB-IP's
/// files are) mapping networks to `{"country": {"iso_code": ..}}`. Built in code, so
/// no third-party database is committed and no test touches the network.
#[cfg(any(test, feature = "testing"))]
pub mod test_mmdb {
    use std::net::IpAddr;

    fn ctrl(out: &mut Vec<u8>, kind: u8, size: usize) {
        assert!(size < 29, "test writer: short values only");
        if kind <= 7 {
            out.push((kind << 5) | size as u8);
        } else {
            out.push(size as u8);
            out.push(kind - 7);
        }
    }

    fn string(out: &mut Vec<u8>, s: &str) {
        ctrl(out, 2, s.len());
        out.extend_from_slice(s.as_bytes());
    }

    fn uint(out: &mut Vec<u8>, kind: u8, v: u64) {
        let bytes = v.to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        ctrl(out, kind, 8 - skip);
        out.extend_from_slice(&bytes[skip..]);
    }

    fn bits(ip: IpAddr) -> u128 {
        match ip {
            IpAddr::V4(v4) => u128::from(u32::from(v4)),
            IpAddr::V6(v6) => u128::from(v6),
        }
    }

    #[derive(Default)]
    struct Node {
        child: [Option<usize>; 2],
        leaf: [Option<usize>; 2],
    }

    /// `(network, prefix length, country code)`; an IPv4 prefix length counts IPv4
    /// bits. `build_epoch` is the build time recorded in the metadata.
    pub fn build(networks: &[(IpAddr, u8, &str)], build_epoch: u64) -> Vec<u8> {
        // Data section: one record per distinct country.
        let mut data = Vec::new();
        let mut offsets: Vec<(String, usize)> = Vec::new();
        for (_, _, cc) in networks {
            if offsets.iter().any(|(c, _)| c == cc) {
                continue;
            }
            offsets.push((cc.to_string(), data.len()));
            ctrl(&mut data, 7, 1);
            string(&mut data, "country");
            ctrl(&mut data, 7, 1);
            string(&mut data, "iso_code");
            string(&mut data, cc);
        }
        let mut nodes = vec![Node::default()];
        for (net, len, cc) in networks {
            let (value, len) = match net {
                IpAddr::V4(_) => (bits(*net), u32::from(*len) + 96),
                IpAddr::V6(_) => (bits(*net), u32::from(*len)),
            };
            let data_at = offsets
                .iter()
                .find(|(c, _)| c == cc)
                .map(|(_, o)| *o)
                .unwrap();
            let mut n = 0;
            for depth in 0..len {
                let bit = ((value >> (127 - depth)) & 1) as usize;
                if depth + 1 == len {
                    nodes[n].leaf[bit] = Some(data_at);
                } else {
                    n = match nodes[n].child[bit] {
                        Some(c) => c,
                        None => {
                            nodes.push(Node::default());
                            let c = nodes.len() - 1;
                            nodes[n].child[bit] = Some(c);
                            c
                        }
                    };
                }
            }
        }
        let count = nodes.len();
        let mut out = Vec::new();
        for node in &nodes {
            for side in 0..2 {
                let record = match (node.child[side], node.leaf[side]) {
                    (Some(c), _) => c,
                    (None, Some(off)) => count + 16 + off,
                    (None, None) => count,
                };
                out.extend_from_slice(&(record as u32).to_be_bytes()[1..]);
            }
        }
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(&data);
        out.extend_from_slice(b"\xAB\xCD\xEFMaxMind.com");
        ctrl(&mut out, 7, 9);
        string(&mut out, "binary_format_major_version");
        uint(&mut out, 5, 2);
        string(&mut out, "binary_format_minor_version");
        uint(&mut out, 5, 0);
        string(&mut out, "build_epoch");
        uint(&mut out, 9, build_epoch);
        string(&mut out, "database_type");
        string(&mut out, "Test-Country-Lite");
        string(&mut out, "description");
        ctrl(&mut out, 7, 1);
        string(&mut out, "en");
        string(&mut out, "test database");
        string(&mut out, "ip_version");
        uint(&mut out, 5, 6);
        string(&mut out, "languages");
        ctrl(&mut out, 11, 1);
        string(&mut out, "en");
        string(&mut out, "node_count");
        uint(&mut out, 6, count as u64);
        string(&mut out, "record_size");
        uint(&mut out, 5, 24);
        out
    }

    /// [`build`], gzipped (what the DB-IP URL serves).
    pub fn build_gz(networks: &[(IpAddr, u8, &str)], build_epoch: u64) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(&build(networks, build_epoch)).unwrap();
        e.finish().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn country_codes() {
        assert_eq!(Country::new("jp").unwrap().as_str(), "JP");
        assert_eq!(Country::LAN.as_str(), "LAN");
        assert_eq!(Country::UNKNOWN.as_str(), "ZZ");
        assert!(Country::new("J").is_none());
        assert!(Country::new("J1").is_none());
        assert!(Country::new("ABCD").is_none());
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn lan_addresses() {
        for a in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.9",
            "192.168.1.5",
            "169.254.10.10",
            "100.64.0.1",
            "100.127.255.254",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
        ] {
            assert!(is_lan(&canonical(ip(a))), "{a}");
        }
        assert!(is_lan(&canonical(ip("::ffff:192.168.1.5"))));
        for a in ["203.0.113.5", "2001:db8::1", "198.51.100.1"] {
            assert!(!is_lan(&canonical(ip(a))), "{a}");
        }
    }

    #[test]
    fn test_db_lookups() {
        let raw = test_mmdb::build(
            &[
                (ip("203.0.113.0"), 24, "JP"),
                (ip("198.51.100.0"), 25, "DE"),
                (ip("2001:db8::"), 32, "FR"),
            ],
            1_700_000_000,
        );
        let reader = open_verified(raw).unwrap();
        assert_eq!(reader.metadata().database_type, "Test-Country-Lite");
        let at = |s: &str| lookup(&reader, ip(s)).map(|c| c.as_str().to_string());
        assert_eq!(at("203.0.113.77").as_deref(), Some("JP"));
        assert_eq!(at("198.51.100.1").as_deref(), Some("DE"));
        assert_eq!(at("198.51.100.200").as_deref(), Some("ZZ"));
        assert_eq!(at("2001:db8:1::5").as_deref(), Some("FR"));
        assert_eq!(at("192.0.2.1").as_deref(), Some("ZZ"));
    }

    #[test]
    fn corrupt_and_oversized_downloads_are_refused() {
        assert!(open_verified(b"not a database".to_vec()).is_err());
        let mut raw = test_mmdb::build(&[(ip("203.0.113.0"), 24, "JP")], 1);
        // Damage a tree record so it points past the data section.
        raw[0] = 0xff;
        assert!(open_verified(raw).is_err());
        // A gzip bomb: 65 MiB of zeros, a few KB compressed.
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        let zeros = vec![0u8; 1 << 20];
        for _ in 0..65 {
            e.write_all(&zeros).unwrap();
        }
        let bomb = e.finish().unwrap();
        assert!(bomb.len() < 1 << 20);
        let err = decompress(bomb).unwrap_err();
        assert!(err.contains("larger than 64 MiB"), "{err}");
        // A plain (not gzipped) file is taken as it is.
        assert_eq!(decompress(b"plain".to_vec()).unwrap(), b"plain");
    }

    #[test]
    fn months_and_schedule() {
        // 2026-01-15 and 2026-12-31 UTC.
        let jan = 1_768_435_200;
        let dec = 1_798_675_200;
        assert_eq!(year_month(jan), (2026, 1));
        assert_eq!(previous_month((2026, 1)), (2025, 12));
        assert_eq!(year_month(dec), (2026, 12));
        assert_eq!(next_month_start((2026, 12)), 1_798_761_600); // 2027-01-01
        let t = "https://example.com/db-{year}-{month}.mmdb.gz";
        assert_eq!(fill(t, (2026, 1)), "https://example.com/db-2026-01.mmdb.gz");
        // Holding this month's: look again after the 1st of the next.
        assert_eq!(
            due_at(t, "2026-01", Some(jan), None, jan),
            next_month_start((2026, 1)) + MONTH_GRACE
        );
        // Holding an older one: now, then daily after each look.
        assert_eq!(due_at(t, "2025-12", Some(jan - 86400 * 40), None, jan), jan);
        assert_eq!(
            due_at(t, "2025-12", Some(jan), Some(jan), jan),
            jan + RETRY_PREVIOUS_MONTH
        );
        // An undated URL: 30 days after the download.
        assert_eq!(
            due_at("https://example.com/db.mmdb", "x", Some(jan), None, jan),
            jan + UNDATED_REFRESH
        );
    }

    /// `cargo test -p bunko-server --lib geo::tests::lookup_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement, run on demand"]
    fn lookup_cost() {
        // A tree about as deep as the real one: 4096 /24s and 4096 IPv6 /48s.
        let mut nets = Vec::new();
        for i in 0..4096u32 {
            let v4 = std::net::Ipv4Addr::from((1u32 << 24) | (i << 8));
            nets.push((IpAddr::V4(v4), 24u8, if i % 2 == 0 { "JP" } else { "DE" }));
            let v6 = std::net::Ipv6Addr::new(0x2001, 0x0db8, i as u16, 0, 0, 0, 0, 0);
            nets.push((IpAddr::V6(v6), 48u8, "FR"));
        }
        let reader = open_verified(test_mmdb::build(&nets, 1)).unwrap();
        let ips: Vec<IpAddr> = (0..4096u32)
            .map(|i| IpAddr::V4(std::net::Ipv4Addr::from((1u32 << 24) | (i << 8) | 7)))
            .collect();
        let n = 200_000;
        let t = std::time::Instant::now();
        for i in 0..n {
            std::hint::black_box(lookup(&reader, std::hint::black_box(ips[i % ips.len()])));
        }
        let raw = t.elapsed().as_nanos() as f64 / n as f64;
        let mut cache = Cache::default();
        for ip in &ips {
            cache.put(*ip, Country::LAN);
        }
        let lock = Mutex::new(cache);
        let t = std::time::Instant::now();
        for i in 0..n {
            std::hint::black_box(lock.lock().get(std::hint::black_box(&ips[i % ips.len()])));
        }
        let hit = t.elapsed().as_nanos() as f64 / n as f64;
        println!("mmdb lookup {raw:.0} ns/call (uncached), cache hit {hit:.0} ns/call");
    }

    #[test]
    fn cache_is_bounded_and_generational() {
        let mut c = Cache::default();
        for i in 0..(CACHE_PER_GENERATION * 3) {
            c.put(IpAddr::from([10, 0, (i >> 8) as u8, i as u8]), Country::LAN);
        }
        assert!(c.current.len() <= CACHE_PER_GENERATION);
        assert!(c.previous.len() <= CACHE_PER_GENERATION);
        c.reset(7);
        assert_eq!(c.generation, 7);
        assert!(c.get(&ip("10.0.0.1")).is_none());
    }
}
