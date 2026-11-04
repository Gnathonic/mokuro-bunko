//! The in-memory window (design §4.4, §4.5): every finished request folds into a
//! handful of maps under one `parking_lot` lock, no I/O; [`super::flush`] swaps the
//! window out and writes it. Each map holds at most [`MAP_CAP`] keys per window; past
//! that a record's share of the map is dropped and counted.

use super::classify::{Class, category};
use super::clients::{ClientInfo, ClientRequest};
use super::crawl::Observation;
use super::geo::Country;
use super::principal::{Principal, PrincipalKind};
use super::reading::FileWritten;
use bunko_dav::{DavTarget, ListingInfo, Served};
use bunko_db::analytics::traffic::{
    Histogram, IP_PRINCIPALS_MAX, PrincipalCounts, SLOW_PER_HOUR, SlowRequest, TrafficCounts,
    latency_bucket,
};
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::net::IpAddr;
use std::sync::Arc;

/// Most keys one map keeps per flush window.
pub const MAP_CAP: usize = 100_000;
/// Longest path kept for a slow request.
pub const SLOW_PATH_MAX: usize = 256;

/// FxHash (rustc's hasher), for the maps keyed by fixed names and numbers only (no
/// client-chosen text, so SipHash's flood resistance buys nothing there): the hot path
/// hashes several such keys per request.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct FxHasher(u64);

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(FX_SEED);
    }
}

impl Hasher for FxHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            self.add(u64::from_le_bytes(*c));
        }
        if !rest.is_empty() {
            let mut w = [0u8; 8];
            w[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(w));
        }
    }

    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    fn write_u16(&mut self, i: u16) {
        self.add(u64::from(i));
    }

    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }

    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
}

type Fx = BuildHasherDefault<FxHasher>;

/// One finished request (design §4.1).
#[derive(Debug, Clone)]
pub(crate) struct RequestRecord {
    /// Epoch milliseconds it started.
    pub at_ms: i64,
    pub method: &'static str,
    pub status: u16,
    pub class: Class,
    /// Body bytes handed to hyper.
    pub bytes_out: u64,
    /// Bytes nginx sends for an `X-Accel-Redirect` answer.
    pub est_bytes_out: u64,
    pub bytes_in: u64,
    pub headers_ms: u64,
    pub total_ms: u64,
    /// The body was dropped before its end.
    pub aborted: bool,
    /// The request had a `Range` header.
    pub range: bool,
    pub principal: Principal,
    /// The client address (canonical; a trusted proxy's text as sent, cut to 64).
    pub ip: Arc<str>,
    pub user_agent: Option<String>,
    pub origin: Option<String>,
    pub host: Option<String>,
    /// The request path, percent-decoded.
    pub path: String,
    pub served: Option<Served>,
    pub listing: Option<ListingInfo>,
    /// A refused credential or login: the username it named (`""` unknown).
    pub auth_failure: Option<String>,
    /// A MOVE's destination in the caller's own tree.
    pub destination_rel: Option<String>,
}

impl RequestRecord {
    pub fn at(&self) -> i64 {
        self.at_ms.div_euclid(1000)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TrafficKey {
    pub hour: i64,
    pub protocol: &'static str,
    pub method: &'static str,
    pub category: &'static str,
    pub detail: &'static str,
    pub status_class: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct KindKey {
    pub hour: i64,
    pub protocol: &'static str,
    pub method: &'static str,
    pub category: &'static str,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct IpAgg {
    pub requests: u64,
    pub bytes_out: u64,
    pub errors: u64,
    pub auth_failures: u64,
    pub principals: Vec<Arc<str>>,
    pub user_agent: Option<String>,
    pub country: Option<Country>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DownloadAgg {
    pub series_key: String,
    pub series_title: String,
    pub bytes: u64,
    pub size: u64,
}

/// One flush window's worth of everything.
#[derive(Debug, Default)]
pub(crate) struct Window {
    /// The earliest request start (epoch seconds) folded in.
    pub first_at: Option<i64>,
    pub traffic: HashMap<TrafficKey, TrafficCounts, Fx>,
    pub latency: HashMap<KindKey, (Histogram, Histogram), Fx>,
    pub errors: HashMap<(KindKey, u16), u64, Fx>,
    pub principals: HashMap<(i64, Arc<str>, &'static str), PrincipalCounts>,
    /// `(first, last)` request start per principal.
    pub seen: HashMap<Arc<str>, (i64, i64)>,
    pub anon_ips: HashMap<(i64, Arc<str>, Arc<str>), u64>,
    pub ips: HashMap<(i64, Arc<str>), IpAgg>,
    pub downloads: HashMap<(i64, Arc<str>, String), DownloadAgg>,
    pub auth_failures: HashMap<(i64, Arc<str>, String), u64>,
    /// The slowest requests (time to headers) per hour, at most [`SLOW_PER_HOUR`].
    pub slow: HashMap<i64, Vec<SlowRequest>>,
    pub clients: HashMap<(i64, ClientInfo, &'static str), (u64, u64), Fx>,
    pub client_principals: HashSet<(i64, &'static str, Arc<str>)>,
    pub origins: HashMap<(i64, String), u64>,
    pub uploads: HashMap<(i64, Arc<str>), (u64, u64)>,
    pub geo: HashMap<(i64, Country), (u64, u64), Fx>,
    /// Records a full map could not keep, per map.
    pub dropped: BTreeMap<&'static str, u64>,
}

/// The value for `key`, unless the map is full and `key` new (then counted dropped).
fn slot<'m, K: Hash + Eq, V: Default, S: std::hash::BuildHasher>(
    map: &'m mut HashMap<K, V, S>,
    key: K,
    name: &'static str,
    dropped: &mut BTreeMap<&'static str, u64>,
) -> Option<&'m mut V> {
    if map.len() >= MAP_CAP && !map.contains_key(&key) {
        *dropped.entry(name).or_default() += 1;
        return None;
    }
    Some(map.entry(key).or_default())
}

impl Window {
    pub fn dropped_total(&self) -> u64 {
        self.dropped.values().sum()
    }

    pub fn fold(&mut self, r: &RequestRecord, client: ClientInfo, country: Option<Country>) {
        let at = r.at();
        let hour = at.div_euclid(3600);
        let day = at.div_euclid(86400);
        let c = &r.class;
        let out_total = r.bytes_out + r.est_bytes_out;
        let error = r.status >= 400;
        self.first_at = Some(self.first_at.map_or(at, |f| f.min(at)));
        let drops = &mut self.dropped;

        let tk = TrafficKey {
            hour,
            protocol: c.protocol,
            method: r.method,
            category: c.category,
            detail: c.detail,
            status_class: r.status / 100,
        };
        if let Some(t) = slot(&mut self.traffic, tk, "traffic", drops) {
            t.requests += 1;
            t.bytes_out += r.bytes_out;
            t.est_bytes_out += r.est_bytes_out;
            t.bytes_in += r.bytes_in;
            if r.aborted {
                t.aborted += 1;
                t.aborted_bytes += r.bytes_out;
            }
            if r.range && c.category == category::VOLUME {
                t.resumed += 1;
            }
            if r.status == 304 {
                t.not_modified += 1;
                t.saved_bytes += r.served.map_or(0, |s| s.size);
            }
            if let Some(l) = r.listing.filter(|l| l.gzip) {
                t.gzip_raw_bytes += l.raw_len;
                t.gzip_sent_bytes += r.bytes_out;
            }
        }
        let kk = KindKey {
            hour,
            protocol: c.protocol,
            method: r.method,
            category: c.category,
        };
        if let Some((h, t)) = slot(&mut self.latency, kk, "latency", drops) {
            h[latency_bucket(r.headers_ms)] += 1;
            t[latency_bucket(r.total_ms)] += 1;
        }
        if error && let Some(n) = slot(&mut self.errors, (kk, r.status), "errors", drops) {
            *n += 1;
        }
        let p = &r.principal.id;
        let pk = (day, p.clone(), c.category);
        if let Some(pc) = slot(&mut self.principals, pk, "principals", drops) {
            pc.requests += 1;
            pc.bytes_out += out_total;
            pc.bytes_in += r.bytes_in;
        }
        if let Some((first, last)) = slot(&mut self.seen, p.clone(), "principals", drops) {
            *first = if *first == 0 { at } else { (*first).min(at) };
            *last = (*last).max(at);
        }
        if r.principal.kind == PrincipalKind::Anon
            && let Some(n) = slot(
                &mut self.anon_ips,
                (day, p.clone(), r.ip.clone()),
                "anon_ips",
                drops,
            )
        {
            *n += 1;
        }
        if let Some(ip) = slot(&mut self.ips, (day, r.ip.clone()), "ips", drops) {
            ip.requests += 1;
            ip.bytes_out += out_total;
            ip.errors += u64::from(error);
            ip.auth_failures += u64::from(r.auth_failure.is_some());
            if ip.principals.len() < IP_PRINCIPALS_MAX && !ip.principals.contains(p) {
                ip.principals.push(p.clone());
            }
            if r.user_agent.is_some() {
                ip.user_agent.clone_from(&r.user_agent);
            }
            if country.is_some() {
                ip.country = country;
            }
        }
        let downloads = c.category == category::VOLUME
            && r.method == "GET"
            && matches!(r.status, 200 | 206)
            && matches!(r.principal.kind, PrincipalKind::User | PrincipalKind::Anon);
        if downloads
            && let Some(vk) = &c.volume_key
            && let Some(d) = slot(
                &mut self.downloads,
                (day, p.clone(), vk.clone()),
                "downloads",
                drops,
            )
        {
            if let Some(s) = &c.series {
                d.series_key.clone_from(&s.key);
                d.series_title.clone_from(&s.title);
            }
            d.bytes += out_total;
            d.size = d.size.max(r.served.map_or(0, |s| s.size));
        }
        if let Some(user) = &r.auth_failure
            && let Some(n) = slot(
                &mut self.auth_failures,
                (hour, r.ip.clone(), user.clone()),
                "auth_failures",
                drops,
            )
        {
            *n += 1;
        }
        note_slow(&mut self.slow, r, hour);
        if let Some(n) = slot(
            &mut self.clients,
            (day, client, c.protocol),
            "clients",
            drops,
        ) {
            n.0 += 1;
            n.1 += out_total;
        }
        if self.client_principals.len() < MAP_CAP {
            self.client_principals.insert((day, client.app, p.clone()));
        } else if !self
            .client_principals
            .contains(&(day, client.app, p.clone()))
        {
            *drops.entry("client_principals").or_default() += 1;
        }
        if let Some(origin) = &r.origin
            && let Some(n) = slot(&mut self.origins, (day, origin.clone()), "origins", drops)
        {
            *n += 1;
        }
        let archive_upload = r.method == "PUT"
            && c.category == category::UPLOAD
            && c.detail == "archive"
            && matches!(r.status, 201 | 204);
        if archive_upload
            && r.principal.kind == PrincipalKind::User
            && let Some(user) = &r.principal.username
            && let Some(u) = slot(&mut self.uploads, (day, user.clone()), "uploads", drops)
        {
            u.0 += 1;
            u.1 += r.bytes_in;
        }
        if let Some(country) = country
            && let Some(g) = slot(&mut self.geo, (day, country), "geo", drops)
        {
            g.0 += 1;
            g.1 += out_total;
        }
    }
}

/// Keep `r` if it is among the hour's [`SLOW_PER_HOUR`] slowest (time to headers: the
/// server's own work, not the client's bandwidth).
fn note_slow(slow: &mut HashMap<i64, Vec<SlowRequest>>, r: &RequestRecord, hour: i64) {
    {
        if slow.len() >= MAP_CAP && !slow.contains_key(&hour) {
            return;
        }
        let list = slow.entry(hour).or_default();
        if list.len() >= SLOW_PER_HOUR {
            let Some((i, min)) = list.iter().enumerate().min_by_key(|(_, s)| s.ms) else {
                return;
            };
            if min.ms >= r.headers_ms {
                return;
            }
            list.swap_remove(i);
        }
        list.push(SlowRequest {
            at: r.at(),
            method: r.method.to_string(),
            path: r.path.chars().take(SLOW_PATH_MAX).collect(),
            status: r.status,
            ms: r.headers_ms,
            principal: r.principal.id.to_string(),
            ip: r.ip.to_string(),
        });
    }
}

/// The window behind its lock.
#[derive(Default)]
pub(crate) struct Collector {
    window: Mutex<Window>,
}

impl Collector {
    pub fn fold(&self, r: &RequestRecord, client: ClientInfo, country: Option<Country>) {
        self.window.lock().fold(r, client, country);
    }

    /// The window so far, leaving an empty one.
    pub fn take(&self) -> Window {
        std::mem::take(&mut *self.window.lock())
    }
}

impl super::Inner {
    /// A request finished (its body ended or was dropped): classify its client, look up
    /// its country, fold it in, then tell the crawl detector and the reading worker,
    /// all outside the window's lock except the fold itself.
    pub(crate) fn commit(&self, r: RequestRecord) {
        let client = self.clients.classify(&ClientRequest {
            user_agent: r.user_agent.as_deref(),
            origin: r.origin.as_deref(),
            host: r.host.as_deref(),
            protocol: r.class.protocol,
            path: &r.path,
        });
        let country =
            r.ip.parse::<IpAddr>()
                .ok()
                .and_then(|ip| self.geo.country(&ip));
        self.collector.fold(&r, client, country);
        if r.principal.kind != PrincipalKind::Processor {
            self.crawl.observe(&Observation {
                at: r.at(),
                ip: &r.ip,
                principal: &r.principal.id,
                protocol: r.class.protocol,
                method: r.method,
                category: r.class.category,
                detail: r.class.detail,
                status: r.status,
                series_key: r.class.series.as_ref().map(|s| s.key.as_str()),
                volume_key: r.class.volume_key.as_deref(),
                bytes_out: r.bytes_out + r.est_bytes_out,
                user_agent: r.user_agent.as_deref(),
                client: &client,
                country,
            });
        }
        self.reading_hook(&r);
    }

    /// A 2xx PUT, DELETE or MOVE of a per-user file goes to the reading worker (a MOVE
    /// reports where it went, when that is the caller's own tree too).
    fn reading_hook(&self, r: &RequestRecord) {
        if !(200..300).contains(&r.status) || !matches!(r.method, "PUT" | "DELETE" | "MOVE") {
            return;
        }
        let Some(user) = &r.principal.username else {
            return;
        };
        let send = |rel: &str| {
            self.reading.on_file_written(FileWritten {
                username: user.to_string(),
                target: DavTarget::Progress,
                rel: rel.to_string(),
                method: r.method,
                at_ms: r.at_ms,
            });
        };
        let per_user = matches!(
            r.class.category,
            category::PROGRESS_SYNC | category::PROFILE_SYNC
        );
        if per_user
            && r.class.target == DavTarget::Progress
            && let Some(rel) = &r.class.rel
        {
            send(rel);
        }
        if r.method == "MOVE"
            && let Some(dest) = &r.destination_rel
        {
            send(dest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::classify::{self, Input};

    fn record(method: &'static str, path: &str, status: u16, principal: &str) -> RequestRecord {
        let class = classify::classify(&Input {
            method,
            path,
            query: None,
            dav: None,
            depth: None,
            processor: principal.starts_with("p:"),
            status,
            range: false,
            accel: false,
        });
        RequestRecord {
            at_ms: 1_800_000_000_000,
            method,
            status,
            class,
            bytes_out: 100,
            est_bytes_out: 0,
            bytes_in: 0,
            headers_ms: 3,
            total_ms: 9,
            aborted: false,
            range: false,
            principal: Principal {
                id: principal.into(),
                kind: PrincipalKind::of(principal),
                username: principal
                    .get(2..)
                    .filter(|_| !principal.starts_with("a:"))
                    .map(Into::into),
            },
            ip: "203.0.113.7".into(),
            user_agent: Some("ua".into()),
            origin: None,
            host: None,
            path: path.into(),
            served: Some(Served {
                size: 100,
                len: 100,
                accel: false,
            }),
            listing: None,
            auth_failure: None,
            destination_rel: None,
        }
    }

    #[test]
    fn folds_downloads_but_not_processors_or_heads() {
        let mut w = Window::default();
        w.fold(
            &record("GET", "/mokuro-reader/S/v.cbz", 200, "u:alice"),
            ClientInfo::UNKNOWN,
            None,
        );
        w.fold(
            &record("GET", "/mokuro-reader/S/v.cbz", 200, "a:0123456789ab"),
            ClientInfo::UNKNOWN,
            None,
        );
        w.fold(
            &record("GET", "/mokuro-reader/S/v.cbz", 200, "p:ocr"),
            ClientInfo::UNKNOWN,
            None,
        );
        w.fold(
            &record("HEAD", "/mokuro-reader/S/v.cbz", 200, "u:alice"),
            ClientInfo::UNKNOWN,
            None,
        );
        w.fold(
            &record("GET", "/mokuro-reader/S/v.cbz", 404, "u:alice"),
            ClientInfo::UNKNOWN,
            None,
        );
        assert_eq!(w.downloads.len(), 2);
        assert!(w.downloads.keys().all(|(_, p, _)| !p.starts_with("p:")));
        assert_eq!(w.traffic.values().map(|t| t.requests).sum::<u64>(), 5);
        assert_eq!(w.errors.values().sum::<u64>(), 1);
        assert_eq!(w.anon_ips.len(), 1);
        assert_eq!(w.ips.len(), 1);
        let ip = w.ips.values().next().unwrap();
        assert_eq!(ip.principals.len(), 3);
        assert_eq!(ip.errors, 1);
        // Processor traffic is kept in the bandwidth totals.
        assert!(
            w.principals
                .keys()
                .any(|(_, p, c)| &**p == "p:ocr" && *c == "processor")
        );
    }

    #[test]
    fn maps_stop_at_the_cap_and_count_drops() {
        let mut w = Window::default();
        let mut r = record("GET", "/mokuro-reader/S/v.cbz", 200, "u:alice");
        for i in 0..(MAP_CAP + 5) {
            r.ip = format!("ip-{i}").into();
            w.ips
                .entry((r.at().div_euclid(86400), r.ip.clone()))
                .or_default();
        }
        w.fold(&r, ClientInfo::UNKNOWN, None);
        assert_eq!(w.ips.len(), MAP_CAP + 5, "nothing evicted");
        assert_eq!(w.dropped.get("ips"), None, "an existing key still counts");
        r.ip = "brand-new".into();
        w.fold(&r, ClientInfo::UNKNOWN, None);
        assert_eq!(w.dropped.get("ips"), Some(&1));
        assert_eq!(w.dropped_total(), 1);
    }

    #[test]
    fn slow_keeps_the_twenty_slowest_per_hour() {
        let mut w = Window::default();
        for ms in 0..50u64 {
            let mut r = record("GET", "/x", 200, "u:a");
            r.headers_ms = ms;
            w.fold(&r, ClientInfo::UNKNOWN, None);
        }
        let list = w.slow.values().next().unwrap();
        assert_eq!(list.len(), SLOW_PER_HOUR);
        assert_eq!(list.iter().map(|s| s.ms).min(), Some(30));
    }
}
