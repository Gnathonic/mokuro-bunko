//! Crawl detection (design §8): per client IP, the distinct series touched by volume
//! downloads and by per-series metadata over `analytics.crawl.window_hours`; an
//! episode opens past `min_series` and `min_coverage` of the catalog. Detected and
//! reported only: nothing is blocked or slowed.
//!
//! **Signals.** *Content*: a `GET` of a library archive answered 200 or 206 (WebDAV or
//! the catalog's download link alike). *Metadata*: a request addressing ONE series
//! and answered 2xx or 304: a WebDAV `PROPFIND` of a series folder with `Depth: 1` or
//! `infinity` (what rclone, davfs2, gvfs and the Windows redirector send to walk a
//! tree), `/catalog/api/series?name=` or a manifest. Deliberately not signals, because
//! Mokuro Reader's normal sync sends them: the one depth-infinity `PROPFIND` of
//! `/mokuro-reader` (the whole library in one cached listing, attributed to no series),
//! `series.json` GETs (after a listing the reader fetches every one whose size or date
//! changed: all of them on its first sync, after a parser bump, or after the server
//! rewrote them), covers (fetched for every card the reader scrolls past), `HEAD`s
//! (sent before every download), sidecars, a `Depth: 0` stat, and anything refused.
//! A walk that lists folders is still caught by its `Depth: 1` listings, and a mirror
//! by its downloads. Every request of a tracked IP still counts toward its episode's
//! requests and bytes.
//!
//! **Known false positive.** When the reader's depth-infinity listing fails with an
//! error it does not recognise as "unsupported" (a timeout or a 5xx on a large
//! library), it falls back to one `Depth: 1` listing per folder for that sync, and
//! that walk is a metadata episode like davfs2's. Telling the two apart needs the
//! client (Mokuro Reader) or the failed listing just before; neither is used yet.
//! Once the catalog's series are known ([`Detector::refresh_catalog_from`], each minute
//! from the host sampler) only a touch of one of them counts: listing a folder that is
//! no series (the server's own `thumbnails/`, a made-up path) does not.
//!
//! **Window.** `window_hours` is approximated by two rotating half-window buckets: a
//! touch is stamped with its half (`at / (window / 2)`) and counts while its half is
//! the current or the previous one, so an IP is judged on between half a window and a
//! whole window of activity.
//!
//! **Episodes.** Judged at every flush ([`Detector::begin_flush`], so the catalog size
//! and thresholds are read off the request path): an episode opens when the series
//! touched in the window reach `min_series` and `min_coverage` of the catalog
//! (`LibraryCounts::total_series`). While open, every touch is kept, so its counters
//! are cumulative (`series`, `volumes`, `coverage`, `requests`, `bytes_out`; the
//! principals, user agent, client and country seen last), and they are rewritten each
//! flush. It closes after a whole window without a request from that IP. Kind:
//! `download` when the downloads alone touched enough series (a mirror lists folders
//! to find files, so listings beside them do not make it `mixed`), `metadata` when
//! nothing was downloaded, `mixed` otherwise. Processors are never observed (the
//! collector skips them); admins are (the UI names the principal).
//!
//! **Bounds** (design §4.5): at most [`MAX_IPS`] trackers, evicted least recently used
//! in batches (an IP with an open episode last); at most [`MAX_ENTRIES`] series and
//! volume touches over all trackers (about 6 MiB at worst); per tracker at most the
//! catalog's series count (plus slack) and [`MAX_VOLUMES`] volumes. Only an IP that
//! sends a signal gets a tracker; the hot path is one short lock and a few hash
//! lookups, no I/O and no allocation for an IP already tracked.
//!
//! A restart forgets the trackers: the first flush closes the episodes a previous run
//! left open, and a crawl that goes on opens a new one.

use crate::accounts::LibraryCounts;
use crate::analytics::clients::ClientInfo;
use crate::analytics::collector::FxHasher;
use crate::analytics::geo::Country;
use crate::core::Core;
use bunko_core::config::CrawlConfig;
use bunko_db::analytics::AnalyticsTx;
use bunko_db::analytics::crawl::{EpisodeRow, STATUS_CLOSED, STATUS_OPEN};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

/// Most client IPs tracked at once.
pub const MAX_IPS: usize = 4096;
/// Trackers dropped at once when [`MAX_IPS`] or [`MAX_ENTRIES`] is reached (so the
/// least-recently-used search runs once per this many new IPs).
const EVICT_BATCH: usize = 512;
/// Most series and volume touches kept over every tracker. An entry is a `u32` hash
/// and two `u16` half stamps (or one, for a volume): at hashbrown's worst load about
/// 21 bytes, so about 6 MiB.
pub const MAX_ENTRIES: usize = 300_000;
/// Most distinct volumes kept per tracker.
pub const MAX_VOLUMES: usize = 65_536;
/// Series a tracker keeps beyond the catalog's count (folders not in the index yet).
const SERIES_SLACK: usize = 256;
/// Most principals named per episode.
pub const PRINCIPALS_MAX: usize = 8;
/// How long a catalog series count is trusted before it is read again.
const CATALOG_TTL_S: i64 = 60;

/// One finished request, as the detector sees it.
#[derive(Debug, Clone, Copy)]
pub struct Observation<'a> {
    /// Epoch seconds the request started.
    pub at: i64,
    /// The client address (canonical IP text, or a trusted proxy's raw text).
    pub ip: &'a str,
    /// `u:<name>` or `a:<hash>` (processors are never observed).
    pub principal: &'a str,
    pub protocol: &'static str,
    pub method: &'static str,
    pub category: &'static str,
    pub detail: &'static str,
    pub status: u16,
    /// The series the request is attributed to (design §4.3), folded.
    pub series_key: Option<&'a str>,
    pub volume_key: Option<&'a str>,
    /// Counted plus estimated bytes.
    pub bytes_out: u64,
    pub user_agent: Option<&'a str>,
    pub client: &'a ClientInfo,
    pub country: Option<Country>,
}

/// What a request says about the series it touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// A volume download.
    Content,
    /// Per-series metadata (a folder listing, the catalog's series, a manifest).
    Metadata,
}

/// The signal `o` carries, if any (see the module docs).
pub fn signal_of(o: &Observation<'_>) -> Option<Signal> {
    o.series_key?;
    match o.category {
        "volume" if o.method == "GET" && matches!(o.status, 200 | 206) => Some(Signal::Content),
        "metadata"
            if ((200..300).contains(&o.status) || o.status == 304)
                && matches!(
                    o.detail,
                    "propfind_1" | "propfind_inf" | "catalog_api" | "manifest"
                ) =>
        {
            Some(Signal::Metadata)
        }
        _ => None,
    }
}

/// The live `analytics.crawl` thresholds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    pub window_hours: u32,
    pub min_series: u32,
    pub min_coverage: f64,
}

impl Thresholds {
    pub fn of(c: &CrawlConfig) -> Thresholds {
        Thresholds {
            window_hours: c.window_hours,
            min_series: c.min_series,
            min_coverage: c.min_coverage,
        }
    }

    pub fn window_s(&self) -> i64 {
        i64::from(self.window_hours.max(1)) * 3600
    }

    fn half_s(&self) -> i64 {
        (self.window_s() / 2).max(1)
    }

    /// `series` distinct series of a catalog of `catalog` (0: unknown, then the count
    /// alone decides) are enough for an episode.
    pub fn met(&self, series: u64, catalog: u64) -> bool {
        series >= u64::from(self.min_series.max(1))
            && (catalog == 0 || series as f64 / catalog as f64 >= self.min_coverage)
    }
}

/// A half window's stamp: its number mod 65535, plus one (0 = never).
fn stamp(half: i64) -> u16 {
    (half.rem_euclid(65_535) + 1) as u16
}

fn hash32(s: &str) -> u32 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish() as u32
}

type Fx = BuildHasherDefault<FxHasher>;

/// When a series was last touched, per signal (half stamps, 0 = never).
#[derive(Debug, Clone, Copy, Default)]
struct Touch {
    content: u16,
    meta: u16,
}

/// Requests and bytes of one half window.
#[derive(Debug, Clone, Copy, Default)]
struct HalfStats {
    half: i64,
    requests: u64,
    bytes: u64,
    first_at: i64,
}

#[derive(Debug, Clone)]
struct Episode {
    /// Its `crawl_episodes` row (0: not written yet).
    id: i64,
    /// Tells this episode from a later one of the same IP when its id comes back.
    serial: u64,
    started_at: i64,
    last_at: i64,
    requests: u64,
    bytes_out: u64,
    /// The kind and catalog size it was last written with.
    kind: &'static str,
    catalog: u64,
    dirty: bool,
}

#[derive(Debug, Default)]
struct Tracker {
    /// The detector's clock at the last request (least recently used goes first).
    used: u64,
    series: HashMap<u32, Touch, Fx>,
    volumes: HashMap<u32, u16, Fx>,
    /// `[previous, current]` half.
    halves: [HalfStats; 2],
    last_at: i64,
    principals: Vec<Arc<str>>,
    user_agent: Option<Box<str>>,
    client: &'static str,
    country: Option<Country>,
    episode: Option<Episode>,
}

impl Tracker {
    fn entries(&self) -> usize {
        self.series.len() + self.volumes.len()
    }

    fn note(&mut self, o: &Observation<'_>, half: i64) {
        let cur = self.halves[1].half;
        if half > cur {
            self.halves[0] = if half == cur + 1 {
                self.halves[1]
            } else {
                HalfStats::default()
            };
            self.halves[1] = HalfStats {
                half,
                first_at: o.at,
                ..HalfStats::default()
            };
        }
        let h = &mut self.halves[1];
        h.requests += 1;
        h.bytes += o.bytes_out;
        h.first_at = h.first_at.min(o.at);
        self.last_at = self.last_at.max(o.at);
        if !self.principals.iter().any(|p| &**p == o.principal)
            && self.principals.len() < PRINCIPALS_MAX
        {
            self.principals.push(Arc::from(o.principal));
        }
        if let Some(ua) = o.user_agent
            && self.user_agent.as_deref() != Some(ua)
        {
            self.user_agent = Some(ua.into());
        }
        self.client = o.client.client;
        if o.country.is_some() {
            self.country = o.country;
        }
        if let Some(e) = &mut self.episode {
            e.requests += 1;
            e.bytes_out += o.bytes_out;
            e.last_at = e.last_at.max(o.at);
            e.dirty = true;
        }
    }

    /// Requests, bytes and the first request of the halves `cur` and `cur - 1`.
    fn window_stats(&self, cur: i64) -> (u64, u64, Option<i64>) {
        let mut out = (0, 0, None::<i64>);
        for h in self
            .halves
            .iter()
            .filter(|h| h.half >= cur - 1 && h.requests > 0)
        {
            out.0 += h.requests;
            out.1 += h.bytes;
            out.2 = Some(out.2.map_or(h.first_at, |f| f.min(h.first_at)));
        }
        out
    }
}

/// Counts of one tracker's touches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    content: u64,
    meta: u64,
    any: u64,
}

fn count(series: &HashMap<u32, Touch, Fx>, live: impl Fn(u16) -> bool) -> Counts {
    let mut c = Counts::default();
    for t in series.values() {
        let (ct, mt) = (live(t.content), live(t.meta));
        c.content += u64::from(ct);
        c.meta += u64::from(mt);
        c.any += u64::from(ct || mt);
    }
    c
}

fn kind_of(c: Counts, th: &Thresholds, catalog: u64) -> &'static str {
    if th.met(c.content, catalog) {
        "download"
    } else if c.content == 0 {
        "metadata"
    } else {
        "mixed"
    }
}

/// An episode row to write, and whose it is.
#[derive(Debug, Clone)]
pub(crate) struct Pending {
    ip: Arc<str>,
    serial: u64,
    row: EpisodeRow,
}

/// The trackers (behind the detector's lock).
#[derive(Debug, Default)]
pub(crate) struct State {
    trackers: HashMap<Arc<str>, Tracker>,
    /// Series and volume touches over every tracker.
    entries: usize,
    clock: u64,
    serials: u64,
    /// The half-window length in force (seconds).
    half_s: i64,
    /// Rows of episodes whose trackers went (evicted), still to be closed.
    orphans: Vec<Pending>,
    /// The catalog's series (hashes of their folded keys), when known: a touch of
    /// anything else (a folder that is no series, such as `thumbnails/`, or a path the
    /// index does not have) counts toward requests and bytes only.
    catalog_keys: Option<HashSet<u32, Fx>>,
    /// Touches not kept for lack of room (all time).
    pub dropped: u64,
    /// Trackers evicted (all time).
    pub evicted: u64,
}

impl State {
    pub(crate) fn new(th: &Thresholds) -> State {
        State {
            half_s: th.half_s(),
            ..State::default()
        }
    }

    pub(crate) fn tracked(&self) -> usize {
        self.trackers.len()
    }

    pub(crate) fn entries(&self) -> usize {
        self.entries
    }

    /// The heap the trackers hold, estimated from the tables' capacities (hashbrown:
    /// a slot plus a control byte each) and the text they own.
    pub(crate) fn approx_bytes(&self) -> usize {
        fn table<K, V, S>(m: &HashMap<K, V, S>) -> usize {
            m.capacity() * 8 / 7 * (std::mem::size_of::<(K, V)>() + 1)
        }
        table(&self.trackers)
            + self
                .trackers
                .iter()
                .map(|(ip, t)| {
                    ip.len()
                        + 16
                        + table(&t.series)
                        + table(&t.volumes)
                        + t.principals.capacity() * std::mem::size_of::<Arc<str>>()
                        + t.principals.iter().map(|p| p.len() + 16).sum::<usize>()
                        + t.user_agent.as_deref().map_or(0, str::len)
                })
                .sum::<usize>()
    }

    #[cfg(test)]
    pub(crate) fn is_tracked(&self, ip: &str) -> bool {
        self.trackers.contains_key(ip)
    }

    /// Fold one request in. `catalog`: the catalog's series count (0: unknown).
    pub(crate) fn observe(&mut self, o: &Observation<'_>, catalog: u64) {
        if o.ip.is_empty() {
            return;
        }
        // The series, when it is one of the catalog's (or the catalog is not known yet).
        let series = o.series_key.map(hash32).filter(|k| {
            self.catalog_keys
                .as_ref()
                .is_none_or(|keys| keys.contains(k))
        });
        let signal = signal_of(o).filter(|_| series.is_some());
        if signal.is_none() && !self.trackers.contains_key(o.ip) {
            return;
        }
        self.clock += 1;
        let clock = self.clock;
        let half = o.at.div_euclid(self.half_s);
        if !self.trackers.contains_key(o.ip) {
            if self.trackers.len() >= MAX_IPS {
                self.evict(None);
            }
            self.trackers.insert(Arc::from(o.ip), Tracker::default());
        }
        let series_cap = usize::try_from(catalog)
            .unwrap_or(usize::MAX)
            .max(1)
            .saturating_add(SERIES_SLACK);
        let mut need_room = false;
        {
            let Some(t) = self.trackers.get_mut(o.ip) else {
                return;
            };
            t.used = clock;
            t.note(o, half);
            let Some(signal) = signal else {
                return;
            };
            // A late request (started in an earlier half) counts in the newest one.
            let s = stamp(t.halves[1].half);
            if let Some(k) = series {
                let known = t.series.contains_key(&k);
                if known || (t.series.len() < series_cap && self.entries < MAX_ENTRIES) {
                    let touch = t.series.entry(k).or_default();
                    match signal {
                        Signal::Content => touch.content = s,
                        Signal::Metadata => touch.meta = s,
                    }
                    if !known {
                        self.entries += 1;
                    }
                } else {
                    self.dropped += 1;
                    need_room = self.entries >= MAX_ENTRIES;
                }
            }
            if signal == Signal::Content
                && let Some(vk) = o.volume_key
            {
                let k = hash32(vk);
                if let Some(v) = t.volumes.get_mut(&k) {
                    *v = s;
                } else if t.volumes.len() < MAX_VOLUMES && self.entries < MAX_ENTRIES {
                    t.volumes.insert(k, s);
                    self.entries += 1;
                } else {
                    self.dropped += 1;
                    need_room |= self.entries >= MAX_ENTRIES;
                }
            }
        }
        if need_room {
            // Make room for later touches (this one is lost): the least recently used
            // trackers other than this one go.
            self.evict(Some(o.ip));
        }
    }

    /// Drop the [`EVICT_BATCH`] least recently used trackers (never `keep`), those
    /// without an open episode first; an evicted open episode is closed at the next
    /// flush.
    fn evict(&mut self, keep: Option<&str>) {
        let mut candidates: Vec<(bool, u64, Arc<str>)> = self
            .trackers
            .iter()
            .filter(|(ip, _)| Some(&***ip) != keep)
            .map(|(ip, t)| (t.episode.is_some(), t.used, ip.clone()))
            .collect();
        let n = EVICT_BATCH.min(candidates.len());
        if n == 0 {
            return;
        }
        if n < candidates.len() {
            candidates.select_nth_unstable(n - 1);
        }
        for (_, _, ip) in candidates.into_iter().take(n) {
            if let Some(t) = self.trackers.remove(&ip) {
                self.entries = self.entries.saturating_sub(t.entries());
                self.evicted += 1;
                if let Some(e) = &t.episode
                    && e.id > 0
                {
                    // Written before: it ends here (a new episode opens if it goes on).
                    let c = count(&t.series, |s| s != 0);
                    let mut row = row_of(&ip, &t, e, c, e.catalog, e.kind);
                    row.status = STATUS_CLOSED.into();
                    self.orphans.push(Pending {
                        ip,
                        serial: e.serial,
                        row,
                    });
                }
            }
        }
    }

    /// Judge every tracker as of `now`: open, update and close episodes, forget what
    /// fell out of the window. Returns the rows to write (in the flush transaction,
    /// outside the lock); once it commits [`State::committed`] takes their ids, and if
    /// it rolls back [`State::rolled_back`] queues them again.
    pub(crate) fn evaluate(&mut self, now: i64, th: &Thresholds, catalog: u64) -> Vec<Pending> {
        let mut out = std::mem::take(&mut self.orphans);
        if th.half_s() != self.half_s {
            // The window changed: the stamps mean something else now. Close what is
            // open (at its current numbers) and start over.
            for (ip, t) in &self.trackers {
                if let Some(e) = &t.episode {
                    let c = count(&t.series, |s| s != 0);
                    let mut row = row_of(ip, t, e, c, catalog, kind_of(c, th, catalog));
                    row.status = STATUS_CLOSED.into();
                    out.push(Pending {
                        ip: ip.clone(),
                        serial: e.serial,
                        row,
                    });
                }
            }
            self.trackers.clear();
            self.entries = 0;
            self.half_s = th.half_s();
            return out;
        }
        let cur = now.div_euclid(self.half_s);
        let live = |s: u16| s != 0 && (s == stamp(cur) || s == stamp(cur - 1));
        let window = th.window_s();
        let mut gone: Vec<Arc<str>> = Vec::new();
        for (ip, t) in self.trackers.iter_mut() {
            let before = t.entries();
            if let Some(e) = &t.episode
                && now - e.last_at.max(t.last_at) >= window
            {
                // A whole window without a request: the episode ends.
                let c = count(&t.series, |s| s != 0);
                let mut row = row_of(ip, t, e, c, catalog, kind_of(c, th, catalog));
                row.status = STATUS_CLOSED.into();
                out.push(Pending {
                    ip: ip.clone(),
                    serial: e.serial,
                    row,
                });
                t.episode = None;
            }
            if t.episode.is_none() {
                // Only the window is kept outside an episode.
                t.series.retain(|_, touch| {
                    if !live(touch.content) {
                        touch.content = 0;
                    }
                    if !live(touch.meta) {
                        touch.meta = 0;
                    }
                    touch.content != 0 || touch.meta != 0
                });
                t.volumes.retain(|_, s| live(*s));
            }
            self.entries = self.entries.saturating_sub(before - t.entries());
            let in_window = count(&t.series, live);
            if t.episode.is_none() && th.met(in_window.any, catalog) {
                let (requests, bytes_out, first) = t.window_stats(cur);
                self.serials += 1;
                t.episode = Some(Episode {
                    id: 0,
                    serial: self.serials,
                    started_at: first.unwrap_or(t.last_at).min(t.last_at),
                    last_at: t.last_at,
                    requests,
                    bytes_out,
                    kind: "mixed",
                    catalog,
                    dirty: true,
                });
                tracing::info!(
                    series = in_window.any,
                    catalog,
                    "[ANALYTICS] a crawl episode opened (an IP touched {} series in {} h; see Analytics, Crawlers)",
                    in_window.any,
                    th.window_hours
                );
            }
            if t.episode.as_ref().is_some_and(|e| e.dirty) {
                let c = count(&t.series, |s| s != 0);
                let kind = kind_of(c, th, catalog);
                if let Some(e) = &mut t.episode {
                    e.dirty = false;
                    e.kind = kind;
                    e.catalog = catalog;
                }
                if let Some(e) = &t.episode {
                    out.push(Pending {
                        ip: ip.clone(),
                        serial: e.serial,
                        row: row_of(ip, t, e, c, catalog, kind),
                    });
                }
            } else if t.episode.is_none() && t.series.is_empty() && now - t.last_at >= window {
                gone.push(ip.clone());
            }
        }
        for ip in gone {
            if let Some(t) = self.trackers.remove(&ip) {
                self.entries = self.entries.saturating_sub(t.entries());
            }
        }
        out
    }

    /// The flush that wrote `rows` (each with the id it was written as) committed: the
    /// episodes still open take their ids. One that left memory meanwhile (evicted while
    /// its row was being written) is closed at the next flush, under that id.
    pub(crate) fn committed(&mut self, rows: Vec<(Pending, i64)>) {
        for (mut p, id) in rows {
            if p.row.status != STATUS_OPEN {
                continue;
            }
            match self
                .trackers
                .get_mut(&p.ip)
                .and_then(|t| t.episode.as_mut())
                .filter(|e| e.serial == p.serial)
            {
                Some(e) => e.id = id,
                // Unless its eviction already queued that (newer) closing row.
                None if self.orphans.iter().any(|o| o.row.id == id) => {}
                None => {
                    p.row.id = id;
                    p.row.status = STATUS_CLOSED.into();
                    self.orphans.push(p);
                }
            }
        }
    }

    /// The flush that was to write `rows` rolled back: nothing of it happened. Closed
    /// rows are written at the next flush; open episodes are written again (with the
    /// ids they had, so a new one is inserted again).
    pub(crate) fn rolled_back(&mut self, rows: Vec<Pending>) {
        for p in rows {
            if p.row.status != STATUS_OPEN {
                self.orphans.push(p);
            } else if let Some(e) = self
                .trackers
                .get_mut(&p.ip)
                .and_then(|t| t.episode.as_mut())
                .filter(|e| e.serial == p.serial)
            {
                e.dirty = true;
            }
            // Else it was evicted meanwhile: with an id, eviction queued its closing row;
            // without one, nothing of it was ever committed.
        }
    }

    /// The ids of the episodes open in memory (already written).
    fn open_ids(&self) -> Vec<i64> {
        self.trackers
            .values()
            .filter_map(|t| t.episode.as_ref().map(|e| e.id))
            .filter(|id| *id > 0)
            .collect()
    }
}

fn row_of(
    ip: &str,
    t: &Tracker,
    e: &Episode,
    c: Counts,
    catalog: u64,
    kind: &'static str,
) -> EpisodeRow {
    EpisodeRow {
        id: e.id,
        ip: ip.to_string(),
        country: t.country.map(|c| c.as_str().to_string()),
        principals: t.principals.iter().map(|p| p.to_string()).collect(),
        client: Some(t.client.to_string()).filter(|c| !c.is_empty()),
        user_agent: t.user_agent.as_deref().map(str::to_string),
        kind: kind.to_string(),
        started_at: e.started_at,
        last_at: e.last_at.max(t.last_at),
        series: c.any,
        volumes: t.volumes.len() as u64,
        catalog_series: catalog,
        coverage: if catalog > 0 {
            (c.any as f64 / catalog as f64).min(1.0)
        } else {
            0.0
        },
        bytes_out: e.bytes_out,
        requests: e.requests,
        status: STATUS_OPEN.into(),
    }
}

/// The detector: bounded per-IP trackers (4 096 IPs, LRU).
pub struct Detector {
    core: Core,
    /// The catalog's series count (`LibraryCounts::total_series`).
    counts: Option<Arc<dyn LibraryCounts>>,
    /// The last count read (0: unknown) and when (epoch seconds; 0: never).
    catalog: AtomicU64,
    catalog_at: AtomicI64,
    state: Mutex<State>,
    /// The previous run's open episodes were closed (the first flush does it).
    resumed: AtomicBool,
    /// Held by the flush in progress ([`Detector::begin_flush`]).
    flushing: Mutex<()>,
}

impl Detector {
    pub fn new(core: Core, counts: Option<Arc<dyn LibraryCounts>>) -> Detector {
        let th = Thresholds::of(&core.config.read().analytics.crawl);
        Detector {
            core,
            counts,
            catalog: AtomicU64::new(0),
            catalog_at: AtomicI64::new(0),
            state: Mutex::new(State::new(&th)),
            resumed: AtomicBool::new(false),
            flushing: Mutex::new(()),
        }
    }

    /// The live thresholds.
    pub fn thresholds(&self) -> Thresholds {
        Thresholds::of(&self.core.config.read().analytics.crawl)
    }

    /// The catalog's series count as last read (`None`: never read, or no index).
    pub fn catalog_series(&self) -> Option<u64> {
        (self.catalog_at.load(Ordering::Relaxed) > 0).then(|| self.catalog.load(Ordering::Relaxed))
    }

    /// Read the catalog's series count again (blocking: the index may walk the library
    /// the first time). The host sampler calls it every minute, off the request path.
    pub fn refresh_catalog(&self, now: i64) {
        let Some(counts) = &self.counts else {
            return;
        };
        match counts.total_series() {
            Ok(n) => {
                self.catalog.store(n, Ordering::Relaxed);
                self.catalog_at.store(now.max(1), Ordering::Relaxed);
            }
            Err(e) => tracing::debug!("[ANALYTICS] crawl detector: no catalog count ({e})"),
        }
    }

    /// Take the catalog's series from the library index (blocking: the first read may
    /// walk the library): their count, and which series keys count as touches. The host
    /// sampler calls it every minute, off the request path.
    pub fn refresh_catalog_from(&self, index: &bunko_library::LibraryIndexCache, now: i64) {
        let snapshot = index.get_snapshot();
        let keys: HashSet<u32, Fx> = snapshot
            .series
            .iter()
            .map(|s| hash32(&bunko_db::fold_series_title_key(&s.name)))
            .collect();
        self.catalog
            .store(snapshot.series.len() as u64, Ordering::Relaxed);
        self.catalog_at.store(now.max(1), Ordering::Relaxed);
        self.state.lock().catalog_keys = Some(keys);
    }

    /// IPs tracked now and touches kept (the bounds' gauges).
    pub fn tracked(&self) -> (usize, usize) {
        let s = self.state.lock();
        (s.tracked(), s.entries())
    }

    /// The trackers' heap, estimated (bytes).
    pub fn approx_bytes(&self) -> usize {
        self.state.lock().approx_bytes()
    }

    /// Cheap, synchronous and bounded: called once per non-processor request.
    pub fn observe(&self, o: &Observation<'_>) {
        let catalog = self.catalog.load(Ordering::Relaxed);
        self.state.lock().observe(o, catalog);
    }

    /// Start writing the episodes (`crawl_episodes`) as of `now` (epoch seconds), in
    /// the collector's flush transaction: [`CrawlFlush::write`] inside it, then
    /// [`CrawlFlush::finish`] with whether it committed. What the detector learns from
    /// the write (new rows' ids, closed and evicted episodes gone) is applied only once
    /// it committed; a rolled-back flush leaves everything to the next one. Flushes take
    /// turns (the returned value holds the detector's flush lock; take it before the
    /// database's writer).
    pub fn begin_flush(&self, now: i64) -> CrawlFlush<'_> {
        let gate = self.flushing.lock();
        if now - self.catalog_at.load(Ordering::Relaxed) >= CATALOG_TTL_S {
            // Normally the sampler keeps it fresh; this covers a server without one.
            self.refresh_catalog(now);
        }
        CrawlFlush {
            detector: self,
            _gate: gate,
            now,
            resumed: false,
            written: Vec::new(),
            unwritten: Vec::new(),
            settled: false,
        }
    }
}

/// One flush of the crawl episodes ([`Detector::begin_flush`]).
pub struct CrawlFlush<'a> {
    detector: &'a Detector,
    _gate: parking_lot::MutexGuard<'a, ()>,
    now: i64,
    /// This flush closed the episodes a previous run left open.
    resumed: bool,
    /// Rows written, with their ids.
    written: Vec<(Pending, i64)>,
    /// Rows judged but not written (an error stopped the write).
    unwritten: Vec<Pending>,
    settled: bool,
}

impl CrawlFlush<'_> {
    /// Judge the trackers and write the episodes' rows, inside the flush transaction:
    /// open ones updated, idle ones closed.
    pub fn write(&mut self, tx: &AnalyticsTx<'_>) -> bunko_db::Result<()> {
        let d = self.detector;
        let th = d.thresholds();
        let catalog = d.catalog.load(Ordering::Relaxed);
        let (pending, open) = {
            let mut s = d.state.lock();
            (s.evaluate(self.now, &th, catalog), s.open_ids())
        };
        if !d.resumed.swap(true, Ordering::Relaxed) {
            self.resumed = true;
            match tx.close_crawl_episodes_except(&open) {
                Ok(0) => {}
                Ok(closed) => tracing::debug!(
                    "[ANALYTICS] closed {closed} crawl episode(s) a previous run left open"
                ),
                Err(e) => {
                    self.unwritten = pending;
                    return Err(e);
                }
            }
        }
        let mut rows = pending.into_iter();
        for p in rows.by_ref() {
            match tx.put_crawl_episode(&p.row) {
                Ok(id) => self.written.push((p, id)),
                Err(e) => {
                    self.unwritten.push(p);
                    self.unwritten.extend(rows);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// The flush transaction `committed`, or rolled back.
    pub fn finish(mut self, committed: bool) {
        self.settle(committed);
    }

    fn settle(&mut self, committed: bool) {
        if std::mem::replace(&mut self.settled, true) {
            return;
        }
        let d = self.detector;
        let mut s = d.state.lock();
        if committed {
            s.committed(std::mem::take(&mut self.written));
        } else {
            let mut rows: Vec<Pending> = std::mem::take(&mut self.written)
                .into_iter()
                .map(|(p, _)| p)
                .collect();
            rows.append(&mut self.unwritten);
            s.rolled_back(rows);
            if self.resumed {
                d.resumed.store(false, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for CrawlFlush<'_> {
    /// Never finished (a panic in the flush): nothing is known to have committed.
    fn drop(&mut self) {
        self.settle(false);
    }
}

pub mod api {
    use super::Thresholds;
    use crate::analytics::Analytics;
    use crate::analytics::api::{ApiQuery, answer, query};
    use axum::response::Response;
    use serde_json::{Value, json};

    /// Most episodes listed.
    const EPISODES_MAX: u32 = 500;

    /// `GET /_admin/api/analytics/crawlers` (design §10): the thresholds, the catalog
    /// size, the episodes active in the range (newest first), per IP, and episodes
    /// started per UTC day (`episodes_per_day`: one point per day the range touches,
    /// hourly ranges included).
    pub async fn crawlers(a: &Analytics, q: &ApiQuery) -> Response {
        let th: Thresholds = a.crawl().thresholds();
        let catalog = a.crawl().catalog_series();
        let (tracked, entries) = a.crawl().tracked();
        let approx_bytes = a.crawl().approx_bytes();
        let db = a.db().clone();
        let (from, to) = (q.range.from, q.range.to + 1);
        let (d0, d1) = q.range.days();
        let got = query("analytics crawlers", move || {
            Ok((
                db.crawl_episodes(from, to, EPISODES_MAX)?,
                db.crawl_episodes_by_ip(from, to)?,
                db.crawl_episodes_per_day(d0, d1)?,
            ))
        })
        .await;
        let (episodes, by_ip, per_day) = match got {
            Ok(v) => v,
            Err(r) => return r,
        };
        let episodes: Vec<Value> = episodes
            .into_iter()
            .map(|e| {
                json!({
                    "id": e.id,
                    "ip": e.ip,
                    "country": e.country,
                    "principals": e.principals,
                    "client": e.client,
                    "user_agent": e.user_agent,
                    "kind": e.kind,
                    "started_at": e.started_at,
                    "last_at": e.last_at,
                    "series": e.series,
                    "volumes": e.volumes,
                    "catalog_series": e.catalog_series,
                    "coverage": (e.coverage * 1000.0).round() / 1000.0,
                    "bytes_out": e.bytes_out,
                    "requests": e.requests,
                    "status": e.status,
                    "active": e.status == bunko_db::analytics::crawl::STATUS_OPEN,
                })
            })
            .collect();
        let by_ip: Vec<Value> = by_ip
            .into_iter()
            .map(|r| {
                json!({
                    "ip": r.ip,
                    "country": r.country,
                    "episodes": r.episodes,
                    "last_at": r.last_at,
                    "bytes_out": r.bytes_out,
                })
            })
            .collect();
        let days: Vec<i64> = (d0..=d1).collect();
        let values: Vec<u64> = days
            .iter()
            .map(|d| {
                per_day
                    .iter()
                    .find(|(pd, _)| pd == d)
                    .map_or(0, |(_, n)| *n)
            })
            .collect();
        answer(
            a,
            q,
            json!({
                "thresholds": {
                    "window_hours": th.window_hours,
                    "min_series": th.min_series,
                    "min_coverage": th.min_coverage,
                },
                "catalog_series": catalog,
                "episodes": episodes,
                "by_ip": by_ip,
                "episodes_per_day": {
                    "t": days.iter().map(|d| d * 86400).collect::<Vec<_>>(),
                    "granularity": "day",
                    "series": [{"key": "episodes", "label": "Episodes", "values": values}],
                },
                "tracking": {
                    "ips": tracked,
                    "max_ips": super::MAX_IPS,
                    "entries": entries,
                    "max_entries": super::MAX_ENTRIES,
                    "approx_bytes": approx_bytes,
                },
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TH: Thresholds = Thresholds {
        window_hours: 6,
        min_series: 20,
        min_coverage: 0.25,
    };
    const T0: i64 = 1_800_000_000;
    const RCLONE: ClientInfo = ClientInfo {
        app: "webdav-client",
        client: "rclone",
        browser: "-",
        os: "-",
    };

    struct Req {
        at: i64,
        ip: &'static str,
        principal: &'static str,
        method: &'static str,
        category: &'static str,
        detail: &'static str,
        status: u16,
        series: Option<String>,
        volume: Option<String>,
        bytes: u64,
    }

    fn req(
        at: i64,
        ip: &'static str,
        method: &'static str,
        category: &'static str,
        detail: &'static str,
    ) -> Req {
        Req {
            at,
            ip,
            principal: "a:0123456789ab",
            method,
            category,
            detail,
            status: 200,
            series: None,
            volume: None,
            bytes: 100,
        }
    }

    impl Req {
        fn series(mut self, s: &str) -> Req {
            self.series = Some(s.to_string());
            self
        }
        fn volume(mut self, v: &str) -> Req {
            self.volume = Some(v.to_string());
            self
        }
        fn status(mut self, s: u16) -> Req {
            self.status = s;
            self
        }
        fn by(mut self, p: &'static str) -> Req {
            self.principal = p;
            self
        }
        fn feed(&self, s: &mut State, catalog: u64) {
            s.observe(
                &Observation {
                    at: self.at,
                    ip: self.ip,
                    principal: self.principal,
                    protocol: "dav",
                    method: self.method,
                    category: self.category,
                    detail: self.detail,
                    status: self.status,
                    series_key: self.series.as_deref(),
                    volume_key: self.volume.as_deref(),
                    bytes_out: self.bytes,
                    user_agent: Some("rclone/v1.68.1"),
                    client: &RCLONE,
                    country: Country::new("LAN"),
                },
                catalog,
            );
        }
    }

    fn download(s: &mut State, at: i64, ip: &'static str, series: usize, catalog: u64) {
        let name = format!("series {series}");
        req(at, ip, "GET", "volume", "full")
            .series(&name)
            .volume(&format!("{name}/v1.cbz"))
            .feed(s, catalog);
    }

    fn list(s: &mut State, at: i64, ip: &'static str, series: usize, catalog: u64) {
        req(at, ip, "PROPFIND", "metadata", "propfind_1")
            .status(207)
            .series(&format!("series {series}"))
            .feed(s, catalog);
    }

    #[test]
    fn signals() {
        let s = |r: Req| {
            signal_of(&Observation {
                at: r.at,
                ip: r.ip,
                principal: r.principal,
                protocol: "dav",
                method: r.method,
                category: r.category,
                detail: r.detail,
                status: r.status,
                series_key: r.series.as_deref(),
                volume_key: r.volume.as_deref(),
                bytes_out: 0,
                user_agent: None,
                client: &ClientInfo::UNKNOWN,
                country: None,
            })
        };
        let ip = "192.0.2.1";
        assert_eq!(
            s(req(0, ip, "GET", "volume", "full").series("a")),
            Some(Signal::Content)
        );
        assert_eq!(
            s(req(0, ip, "GET", "volume", "range").series("a").status(206)),
            Some(Signal::Content)
        );
        // HEAD before every download, refused and not-found GETs: no signal.
        assert_eq!(s(req(0, ip, "HEAD", "volume", "full").series("a")), None);
        assert_eq!(
            s(req(0, ip, "GET", "volume", "full").series("a").status(404)),
            None
        );
        for d in ["propfind_1", "propfind_inf", "catalog_api", "manifest"] {
            assert_eq!(
                s(req(0, ip, "GET", "metadata", d).series("a").status(207)),
                Some(Signal::Metadata),
                "{d}"
            );
        }
        assert_eq!(
            s(req(0, ip, "GET", "metadata", "manifest")
                .series("a")
                .status(304)),
            Some(Signal::Metadata)
        );
        // series.json: the reader refreshes every one on its first sync.
        for status in [200, 304] {
            assert_eq!(
                s(req(0, ip, "GET", "metadata", "series_json")
                    .series("a")
                    .status(status)),
                None
            );
        }
        // A stat, the library-wide listing (no series), covers, sidecars: no signal.
        assert_eq!(
            s(req(0, ip, "PROPFIND", "metadata", "propfind_0").series("a")),
            None
        );
        assert_eq!(s(req(0, ip, "PROPFIND", "metadata", "propfind_inf")), None);
        assert_eq!(s(req(0, ip, "GET", "cover", "").series("a")), None);
        assert_eq!(s(req(0, ip, "GET", "sidecar", "primary").series("a")), None);
        assert_eq!(
            s(req(0, ip, "PROPFIND", "metadata", "propfind_1")
                .series("a")
                .status(403)),
            None
        );
    }

    #[test]
    fn thresholds_need_both_count_and_coverage() {
        assert!(!TH.met(19, 40));
        assert!(TH.met(20, 40));
        // 20 of 100 is 20 %, under a quarter.
        assert!(!TH.met(20, 100));
        assert!(TH.met(25, 100));
        // Unknown catalog: the count alone.
        assert!(TH.met(20, 0));
    }

    #[test]
    fn a_full_mirror_opens_a_download_episode_and_updates_then_closes_it() {
        let mut s = State::new(&TH);
        let ip = "203.0.113.9";
        for i in 0..30 {
            list(&mut s, T0 + i as i64, ip, i, 40);
            download(&mut s, T0 + i as i64, ip, i, 40);
        }
        let rows = s.evaluate(T0 + 60, &TH, 40);
        assert_eq!(rows.len(), 1);
        let r = &rows[0].row;
        assert_eq!(r.kind, "download");
        assert_eq!((r.series, r.volumes, r.catalog_series), (30, 30, 40));
        assert_eq!(r.coverage, 0.75);
        assert_eq!(r.requests, 60);
        assert_eq!(r.bytes_out, 6000);
        assert_eq!(r.started_at, T0);
        assert_eq!(r.status, STATUS_OPEN);
        assert_eq!(r.client.as_deref(), Some("rclone"));
        assert_eq!(r.country.as_deref(), Some("LAN"));
        s.committed(vec![(rows[0].clone(), 7)]);
        // Nothing new: nothing to write.
        assert!(s.evaluate(T0 + 120, &TH, 40).is_empty());
        // It goes on: the same row, updated.
        for i in 30..40 {
            download(&mut s, T0 + 200, ip, i, 40);
        }
        let rows = s.evaluate(T0 + 240, &TH, 40);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].row.id, 7);
        assert_eq!((rows[0].row.series, rows[0].row.coverage), (40, 1.0));
        assert_eq!(rows[0].row.requests, 70);
        // A whole window without a request closes it, and the tracker is forgotten.
        let quiet = T0 + 200 + TH.window_s();
        let rows = s.evaluate(quiet, &TH, 40);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].row.id, rows[0].row.status.as_str()),
            (7, STATUS_CLOSED)
        );
        assert!(s.evaluate(quiet + TH.window_s(), &TH, 40).is_empty());
        assert_eq!(s.tracked(), 0);
        assert_eq!(s.entries(), 0);
    }

    #[test]
    fn a_metadata_walk_is_metadata_and_a_little_download_makes_it_mixed() {
        let mut s = State::new(&TH);
        for i in 0..25 {
            list(&mut s, T0, "198.51.100.7", i, 40);
            list(&mut s, T0, "198.51.100.8", i, 40);
        }
        download(&mut s, T0, "198.51.100.8", 3, 40);
        let mut rows = s.evaluate(T0 + 60, &TH, 40);
        rows.sort_by(|a, b| a.row.ip.cmp(&b.row.ip));
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].row.kind.as_str(), rows[0].row.volumes),
            ("metadata", 0)
        );
        assert_eq!(
            (rows[1].row.kind.as_str(), rows[1].row.volumes),
            ("mixed", 1)
        );
    }

    #[test]
    fn reading_a_few_series_and_the_whole_library_listing_never_open_one() {
        let mut s = State::new(&TH);
        let ip = "192.0.2.10";
        // The reader's sync: one depth-infinity listing of the reader root (no series),
        // covers of everything it shows, its own files; then a few series read.
        req(T0, ip, "PROPFIND", "metadata", "propfind_inf")
            .status(207)
            .feed(&mut s, 40);
        for i in 0..40 {
            // Its first sync refreshes every series.json.
            req(T0, ip, "GET", "metadata", "series_json")
                .series(&format!("series {i}"))
                .feed(&mut s, 40);
            req(T0, ip, "GET", "cover", "")
                .series(&format!("series {i}"))
                .feed(&mut s, 40);
            req(T0, ip, "HEAD", "volume", "full")
                .series(&format!("series {i}"))
                .feed(&mut s, 40);
        }
        for i in 0..5 {
            download(&mut s, T0 + 10, ip, i, 40);
        }
        assert!(s.evaluate(T0 + 60, &TH, 40).is_empty());
        // A day of reading in 3-hour sessions of a few new series each: never 20 in
        // one window.
        for day_half in 0..8 {
            let at = T0 + day_half * TH.half_s();
            for i in 0..6 {
                download(&mut s, at, ip, (day_half as usize) * 6 + i, 100);
            }
            assert!(s.evaluate(at + 60, &TH, 100).is_empty(), "half {day_half}");
        }
    }

    #[test]
    fn touches_older_than_the_window_do_not_add_up() {
        let mut s = State::new(&TH);
        let ip = "192.0.2.11";
        for i in 0..15 {
            download(&mut s, T0, ip, i, 40);
        }
        assert!(s.evaluate(T0 + 60, &TH, 40).is_empty());
        // Two halves later the first 15 are gone; 15 more is still 15.
        let later = T0 + 2 * TH.half_s() + 10;
        for i in 15..30 {
            download(&mut s, later, ip, i, 40);
        }
        assert!(s.evaluate(later + 60, &TH, 40).is_empty());
        // Within one half of each other they count together.
        let mut s = State::new(&TH);
        for i in 0..15 {
            download(&mut s, T0, ip, i, 40);
        }
        for i in 15..30 {
            download(&mut s, T0 + TH.half_s(), ip, i, 40);
        }
        assert_eq!(s.evaluate(T0 + TH.half_s() + 60, &TH, 40).len(), 1);
    }

    #[test]
    fn an_episode_names_every_principal_up_to_eight() {
        let mut s = State::new(&TH);
        let ip = "192.0.2.12";
        let users = [
            "u:alice",
            "u:bob",
            "a:000000000001",
            "a:000000000002",
            "a:000000000003",
            "a:000000000004",
            "a:000000000005",
            "a:000000000006",
            "a:000000000007",
        ];
        for (i, u) in users.iter().cycle().take(30).enumerate() {
            let name = format!("series {i}");
            req(T0, ip, "GET", "volume", "full")
                .series(&name)
                .volume(&name)
                .by(u)
                .feed(&mut s, 40);
        }
        let rows = s.evaluate(T0 + 60, &TH, 40);
        assert_eq!(rows[0].row.principals.len(), PRINCIPALS_MAX);
        assert_eq!(rows[0].row.principals[0], "u:alice");
    }

    #[test]
    fn ten_thousand_ips_stay_within_the_bounds_and_an_open_episode_survives() {
        let mut s = State::new(&TH);
        let crawler = "203.0.113.200";
        for i in 0..30 {
            download(&mut s, T0, crawler, i, 40);
        }
        let rows = s.evaluate(T0 + 1, &TH, 40);
        s.committed(vec![(rows[0].clone(), 1)]);
        let ips: Vec<&'static str> = (0..10_000)
            .map(|i| &*Box::leak(format!("2001:db8::{i:x}").into_boxed_str()))
            .collect();
        for (i, ip) in ips.iter().enumerate() {
            download(&mut s, T0 + 2, ip, i % 40, 40);
            assert!(s.tracked() <= MAX_IPS);
            if i % 1000 == 0 {
                // The crawler goes on, so it is recently used too.
                download(&mut s, T0 + 2, crawler, 0, 40);
            }
        }
        assert!(s.tracked() <= MAX_IPS);
        assert!(s.is_tracked(crawler));
        assert!(s.is_tracked(ips[9_999]), "the newest IP is tracked");
        assert!(!s.is_tracked(ips[0]), "the oldest went");
        assert!(s.evicted >= 10_000 - MAX_IPS as u64);
        assert!(s.entries() <= MAX_ENTRIES);
        // Each tracker holds one series and one volume.
        assert_eq!(s.entries(), (s.tracked() - 1) * 2 + 60);
    }

    #[test]
    fn the_touch_budget_holds_and_crawlers_keep_their_trackers() {
        let mut s = State::new(&TH);
        // Many IPs each touching 200 distinct series of a huge catalog.
        let catalog = 1_000_000;
        let ips: Vec<&'static str> = (0..3000)
            .map(|i| &*Box::leak(format!("2001:db8:1::{i:x}").into_boxed_str()))
            .collect();
        for (n, ip) in ips.iter().enumerate() {
            for i in 0..200 {
                download(&mut s, T0, ip, n * 1000 + i, catalog);
            }
            assert!(s.entries() <= MAX_ENTRIES);
        }
        assert!(s.dropped > 0 || s.evicted > 0);
        assert!(s.is_tracked(ips[2999]));
    }

    #[test]
    fn an_evicted_open_episode_is_closed_and_a_window_change_starts_over() {
        let mut s = State::new(&TH);
        for i in 0..30 {
            download(&mut s, T0, "203.0.113.1", i, 40);
        }
        let rows = s.evaluate(T0 + 1, &TH, 40);
        s.committed(vec![(rows[0].clone(), 11)]);
        // Force it out: every other tracker has an episode too, and it is the oldest.
        s.evict(None);
        assert!(!s.is_tracked("203.0.113.1"));
        let rows = s.evaluate(T0 + 2, &TH, 40);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].row.id, rows[0].row.status.as_str()),
            (11, STATUS_CLOSED)
        );
        // A new window length: open episodes close and every tracker goes.
        for i in 0..30 {
            download(&mut s, T0 + 3, "203.0.113.2", i, 40);
        }
        let rows = s.evaluate(T0 + 4, &TH, 40);
        s.committed(vec![(rows[0].clone(), 12)]);
        let wider = Thresholds {
            window_hours: 12,
            ..TH
        };
        let rows = s.evaluate(T0 + 5, &wider, 40);
        assert_eq!(
            (rows[0].row.id, rows[0].row.status.as_str()),
            (12, STATUS_CLOSED)
        );
        assert_eq!((s.tracked(), s.entries()), (0, 0));
    }

    /// The worst a flood can make the trackers hold: every IP slot taken and the touch
    /// budget full, with long IPs, user agents and eight principals each.
    #[test]
    fn the_worst_case_memory_is_bounded() {
        let mut s = State::new(&TH);
        let catalog = 1_000_000;
        let ua = "x".repeat(256);
        let mut n = 0usize;
        for i in 0..MAX_IPS * 2 {
            let ip: &'static str = Box::leak(
                format!(
                    "2001:db8:ffff:ffff:ffff:ffff:{:x}:{:x}",
                    i >> 16,
                    i & 0xffff
                )
                .into_boxed_str(),
            );
            for k in 0..(MAX_ENTRIES / MAX_IPS + 8) {
                n += 1;
                let name = format!("s{n}");
                let principal: &'static str =
                    Box::leak(format!("a:{:012x}", k % 8).into_boxed_str());
                s.observe(
                    &Observation {
                        at: T0,
                        ip,
                        principal,
                        protocol: "dav",
                        method: "GET",
                        category: "volume",
                        detail: "full",
                        status: 200,
                        series_key: Some(&name),
                        volume_key: Some(&name),
                        bytes_out: 1,
                        user_agent: Some(&ua),
                        client: &RCLONE,
                        country: None,
                    },
                    catalog,
                );
            }
        }
        let bytes = s.approx_bytes();
        eprintln!(
            "crawl worst case: {} IPs, {} touches, ~{:.2} MiB",
            s.tracked(),
            s.entries(),
            bytes as f64 / (1 << 20) as f64
        );
        assert!(s.tracked() <= MAX_IPS);
        assert!(s.entries() <= MAX_ENTRIES);
        assert!(bytes < 16 << 20, "{bytes}");
    }

    /// `cargo test -p bunko-server --lib analytics::crawl::tests::observe_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement, not a check"]
    fn observe_cost() {
        use std::time::Instant;
        let mut s = State::new(&TH);
        for i in 0..MAX_IPS {
            let ip: &'static str = Box::leak(format!("2001:db8::{i:x}").into_boxed_str());
            download(&mut s, T0, ip, i % 40, 40);
        }
        let names: Vec<String> = (0..2000).map(|i| format!("series {i}")).collect();
        let n = 200_000;
        let bench = |s: &mut State, label: &str, f: &dyn Fn(usize) -> Req| {
            let reqs: Vec<Req> = (0..n).map(f).collect();
            let t = Instant::now();
            for r in &reqs {
                r.feed(s, 2000);
            }
            eprintln!(
                "{label}: {:.0} ns/observe",
                t.elapsed().as_nanos() as f64 / n as f64
            );
        };
        bench(&mut s, "untracked IP, no signal (most requests)", &|_| {
            req(T0, "192.0.2.99", "GET", "web", "")
        });
        bench(&mut s, "tracked IP, no signal", &|_| {
            req(T0, "2001:db8::5", "PUT", "progress_sync", "positions")
        });
        bench(
            &mut s,
            "tracked IP, volume GET (series + volume touch)",
            &|i| {
                req(T0, "2001:db8::6", "GET", "volume", "full")
                    .series(&names[i % 2000])
                    .volume(&names[i % 2000])
            },
        );
        let t = Instant::now();
        let rows = s.evaluate(T0 + 60, &TH, 2000);
        eprintln!(
            "evaluate over {} trackers / {} touches: {:?} ({} rows)",
            s.tracked(),
            s.entries(),
            t.elapsed(),
            rows.len()
        );
    }

    #[test]
    fn only_the_catalogs_series_count_once_it_is_known() {
        let mut s = State::new(&TH);
        s.catalog_keys = Some((0..40).map(|i| hash32(&format!("series {i}"))).collect());
        let ip = "192.0.2.60";
        // Listing folders that are no series (the server's thumbnails/, made-up paths).
        for name in ["thumbnails", "not a series", "series 999"] {
            req(T0, ip, "PROPFIND", "metadata", "propfind_1")
                .status(207)
                .series(name)
                .feed(&mut s, 40);
        }
        assert_eq!(s.entries(), 0);
        for i in 0..20 {
            download(&mut s, T0, ip, i, 40);
        }
        let rows = s.evaluate(T0 + 60, &TH, 40);
        assert_eq!((rows[0].row.series, rows[0].row.volumes), (20, 20));
        // The listings came first: no tracker yet, so not counted.
        assert_eq!(rows[0].row.requests, 20);
    }

    #[test]
    fn a_rolled_back_flush_leaves_everything_to_the_next_one() {
        let mut s = State::new(&TH);
        for i in 0..30 {
            download(&mut s, T0, "203.0.113.1", i, 40);
        }
        // Opened, but the flush rolled back: written again, still as a new row.
        let rows = s.evaluate(T0 + 1, &TH, 40);
        assert_eq!((rows.len(), rows[0].row.id), (1, 0));
        s.rolled_back(rows);
        let rows = s.evaluate(T0 + 2, &TH, 40);
        assert_eq!((rows.len(), rows[0].row.id), (1, 0));
        assert_eq!(rows[0].row.status, STATUS_OPEN);
        // Committed: the id is kept.
        s.committed(vec![(rows[0].clone(), 5)]);
        assert!(s.evaluate(T0 + 3, &TH, 40).is_empty());
        // Closed after a quiet window, rolled back: the closing row comes again.
        let quiet = T0 + TH.window_s();
        let rows = s.evaluate(quiet, &TH, 40);
        assert_eq!(
            (rows[0].row.id, rows[0].row.status.as_str()),
            (5, STATUS_CLOSED)
        );
        s.rolled_back(rows);
        let rows = s.evaluate(quiet + 1, &TH, 40);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].row.id, rows[0].row.status.as_str()),
            (5, STATUS_CLOSED)
        );
        s.committed(vec![(rows[0].clone(), 5)]);
        assert!(s.evaluate(quiet + 2, &TH, 40).is_empty());
    }

    #[test]
    fn an_episode_evicted_while_its_row_is_written_is_closed_under_its_new_id() {
        let mut s = State::new(&TH);
        for i in 0..30 {
            download(&mut s, T0, "203.0.113.1", i, 40);
        }
        let rows = s.evaluate(T0 + 1, &TH, 40);
        // Evicted before the flush committed (it had no id then: no closing row).
        s.evict(None);
        assert!(s.evaluate(T0 + 2, &TH, 40).is_empty());
        s.committed(vec![(rows[0].clone(), 9)]);
        let rows = s.evaluate(T0 + 3, &TH, 40);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].row.id, rows[0].row.status.as_str()),
            (9, STATUS_CLOSED)
        );
    }

    #[test]
    fn only_signals_create_trackers() {
        let mut s = State::new(&TH);
        req(T0, "192.0.2.50", "GET", "web", "").feed(&mut s, 40);
        req(T0, "192.0.2.50", "PROPFIND", "metadata", "propfind_inf")
            .status(207)
            .feed(&mut s, 40);
        assert_eq!(s.tracked(), 0);
        req(T0, "", "GET", "volume", "full")
            .series("a")
            .feed(&mut s, 40);
        assert_eq!(s.tracked(), 0);
    }
}
