//! `Depth: infinity` PROPFIND cache (spec §10).
//!
//! Observable behaviour kept from 0.5.2: only `Depth: infinity` (exact header) is cached,
//! keyed by the normalised path; the shared entry is generated anonymously and always
//! answers `allprop`; an authenticated caller of `/` or `/mokuro-reader` gets their own
//! progress files appended; `gzip` is served when `Accept-Encoding` mentions it; entries
//! are fresh for `ttl`, then served stale while one background refresh runs, until
//! `stale_ttl`.
//!
//! Changed: a DAV write invalidates the affected entries AFTER it commits (0.5.2 refreshed
//! before the write and could cache the pre-write listing as fresh), a generation that
//! raced an invalidation is not stored, and the cache is bounded by bytes (LRU), not by
//! entry count alone. Entries are stored gzip-compressed: the multistatus prefix is
//! deflated once with a sync flush, so a per-user tail can be appended as a fresh deflate
//! stream and the gzip trailer computed with a CRC combine — no recompression per hit.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use bytes::Bytes;
use flate2::Compression;
use flate2::write::DeflateEncoder;
use futures_util::stream;
use http::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, VARY};
use http::{Response, StatusCode};
use parking_lot::Mutex;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::propfind::{self, Mode, PropCtx};
use crate::resource::Roots;
use crate::xml::{MULTISTATUS_CLOSE, MULTISTATUS_OPEN};

#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Total compressed bytes held (LRU beyond it).
    pub budget_bytes: usize,
    pub ttl: Duration,
    pub stale_ttl: Duration,
    pub max_entries: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            budget_bytes: 16 * 1024 * 1024,
            ttl: Duration::from_secs(120),
            stale_ttl: Duration::from_secs(86_400),
            max_entries: 32,
        }
    }
}

/// One compressed listing: gzip header + non-final deflate blocks of everything before
/// `</D:multistatus>`.
#[derive(Clone)]
struct Entry {
    gz_prefix: Bytes,
    crc: crc32fast::Hasher,
    raw_len: u64,
    generated: Instant,
    last_used: u64,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    bytes: usize,
    tick: u64,
    epoch: u64,
    refreshing: HashSet<String>,
    stopped: bool,
    debounce: Option<JoinHandle<()>>,
}

pub struct PropfindCache {
    cfg: CacheConfig,
    roots: Roots,
    state: Mutex<State>,
    flights: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    runtime: Option<Handle>,
}

const GZIP_HEADER: [u8; 10] = [0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];

/// A cache answer, before per-user injection.
pub(crate) struct Hit {
    entry: Entry,
}

impl PropfindCache {
    pub(crate) fn new(cfg: CacheConfig, roots: Roots) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            roots,
            state: Mutex::new(State::default()),
            flights: Mutex::new(HashMap::new()),
            runtime: Handle::try_current().ok(),
        })
    }

    /// Generate the anonymous allprop listing of `key`, compressed. Blocking. `None` when
    /// the path is not a resource (not cached; the live path answers it).
    fn generate(roots: &Roots, key: &str) -> Option<Entry> {
        let res = roots.lookup(key, None)?;
        let mut crc = crc32fast::Hasher::new();
        let mut raw_len = 0u64;
        let mut enc = DeflateEncoder::new(Vec::with_capacity(64 * 1024), Compression::new(6));
        let mut failed = false;
        let mut feed = |chunk: &str| {
            crc.update(chunk.as_bytes());
            raw_len += chunk.len() as u64;
            if enc.write_all(chunk.as_bytes()).is_err() {
                failed = true;
            }
        };
        feed(MULTISTATUS_OPEN);
        let ctx = PropCtx {
            username: None,
            locks: None,
            dead: None,
        };
        propfind::walk(roots, &res, None, &Mode::AllProp, &ctx, &mut feed);
        // Sync flush: byte-aligned, no final block, so another deflate stream may follow.
        if failed || enc.flush().is_err() {
            return None;
        }
        let deflated = std::mem::take(enc.get_mut());
        let mut gz = Vec::with_capacity(GZIP_HEADER.len() + deflated.len());
        gz.extend_from_slice(&GZIP_HEADER);
        gz.extend_from_slice(&deflated);
        Some(Entry {
            gz_prefix: Bytes::from(gz),
            crc,
            raw_len,
            generated: Instant::now(),
            last_used: 0,
        })
    }

    fn insert(&self, key: &str, mut entry: Entry, epoch: u64) {
        let mut st = self.state.lock();
        if st.stopped || st.epoch != epoch {
            return; // an invalidation raced this generation: it may predate the write
        }
        let size = entry.gz_prefix.len();
        if size > self.cfg.budget_bytes {
            return;
        }
        if let Some(old) = st.entries.remove(key) {
            st.bytes -= old.gz_prefix.len();
        }
        while (st.bytes + size > self.cfg.budget_bytes || st.entries.len() >= self.cfg.max_entries)
            && !st.entries.is_empty()
        {
            let Some(victim) = st
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(old) = st.entries.remove(&victim) {
                st.bytes -= old.gz_prefix.len();
            }
        }
        st.tick += 1;
        entry.last_used = st.tick;
        st.bytes += size;
        st.entries.insert(key.to_string(), entry);
    }

    /// Look the key up, generating it when absent or past `stale_ttl`; `None` when the
    /// path is not a resource.
    pub(crate) async fn get(self: &Arc<Self>, key: &str) -> Option<Hit> {
        if let Some(hit) = self.lookup_fresh(key) {
            return Some(hit);
        }
        let flight = self
            .flights
            .lock()
            .entry(key.to_string())
            .or_default()
            .clone();
        let _guard = flight.lock().await;
        if let Some(hit) = self.lookup_fresh(key) {
            return Some(hit);
        }
        let epoch = self.state.lock().epoch;
        let roots = self.roots.clone();
        let k = key.to_string();
        let entry = tokio::task::spawn_blocking(move || Self::generate(&roots, &k))
            .await
            .ok()
            .flatten();
        if let Some(e) = &entry {
            self.insert(key, e.clone(), epoch);
        }
        self.flights.lock().remove(key);
        entry.map(|entry| Hit { entry })
    }

    /// A fresh or stale-but-servable entry (starting one background refresh when stale).
    fn lookup_fresh(self: &Arc<Self>, key: &str) -> Option<Hit> {
        let mut st = self.state.lock();
        st.tick += 1;
        let tick = st.tick;
        let entry = st.entries.get_mut(key)?;
        let age = entry.generated.elapsed();
        if age >= self.cfg.stale_ttl {
            return None;
        }
        entry.last_used = tick;
        let entry = entry.clone();
        if age >= self.cfg.ttl && !st.refreshing.contains(key) {
            st.refreshing.insert(key.to_string());
            drop(st);
            self.spawn_refresh(key.to_string());
        }
        Some(Hit { entry })
    }

    fn spawn_refresh(self: &Arc<Self>, key: String) {
        let Some(rt) = self.runtime.clone() else {
            self.state.lock().refreshing.remove(&key);
            return;
        };
        let this = Arc::clone(self);
        rt.spawn(async move {
            let epoch = this.state.lock().epoch;
            let roots = this.roots.clone();
            let k = key.clone();
            if let Ok(Some(entry)) =
                tokio::task::spawn_blocking(move || Self::generate(&roots, &k)).await
            {
                this.insert(&key, entry, epoch);
            }
            this.state.lock().refreshing.remove(&key);
        });
    }

    /// Drop every entry whose listing contains (or is contained in) one of `paths`
    /// (virtual, normalised). Called after a DAV write commits.
    pub fn invalidate_paths(&self, paths: &[String]) {
        let mut st = self.state.lock();
        st.epoch += 1;
        let doomed: Vec<String> = st
            .entries
            .keys()
            .filter(|k| {
                paths.iter().any(|p| {
                    crate::paths::is_equal_or_child(k, p) || crate::paths::is_equal_or_child(p, k)
                })
            })
            .cloned()
            .collect();
        for k in doomed {
            if let Some(e) = st.entries.remove(&k) {
                st.bytes -= e.gz_prefix.len();
            }
        }
    }

    /// Clear everything.
    pub fn invalidate(&self) {
        let mut st = self.state.lock();
        st.epoch += 1;
        st.entries.clear();
        st.bytes = 0;
    }

    /// Background-refresh every cached key (stale entries stay servable meanwhile).
    pub fn refresh_all(self: &Arc<Self>) {
        let keys: Vec<String> = {
            let mut st = self.state.lock();
            let keys: Vec<String> = st
                .entries
                .keys()
                .filter(|k| !st.refreshing.contains(*k))
                .cloned()
                .collect();
            for k in &keys {
                st.refreshing.insert(k.clone());
            }
            keys
        };
        for k in keys {
            self.spawn_refresh(k);
        }
    }

    /// Debounced [`refresh_all`](Self::refresh_all): fires after `delay` of quiet (the
    /// filesystem watcher and the metadata publisher call this). A no-op after
    /// [`stop`](Self::stop). Without a tokio runtime it simply clears the cache.
    pub fn schedule_refresh(self: &Arc<Self>, delay: Duration) {
        let mut st = self.state.lock();
        if st.stopped {
            return;
        }
        if let Some(h) = st.debounce.take() {
            h.abort();
        }
        let Some(rt) = self.runtime.clone() else {
            drop(st);
            self.invalidate();
            return;
        };
        let this = Arc::clone(self);
        st.debounce = Some(rt.spawn(async move {
            tokio::time::sleep(delay).await;
            tracing::debug!("PROPFIND cache: debounced refresh");
            this.refresh_all();
        }));
    }

    /// Generate `path`'s entry in the background (server start-up).
    pub fn warm(self: &Arc<Self>, path: &str) {
        let key = crate::paths::normalize(path);
        let Some(rt) = self.runtime.clone() else {
            return;
        };
        let this = Arc::clone(self);
        rt.spawn(async move {
            let started = Instant::now();
            if let Some(hit) = this.get(&key).await {
                tracing::info!(
                    path = %key,
                    raw_mb = hit.entry.raw_len as f64 / 1048576.0,
                    gzip_kb = hit.entry.gz_prefix.len() as f64 / 1024.0,
                    secs = started.elapsed().as_secs_f64(),
                    "PROPFIND cache warmed"
                );
            }
        });
    }

    /// Cancel the debounce timer and refuse future ones (shutdown).
    pub fn stop(&self) {
        let mut st = self.state.lock();
        st.stopped = true;
        if let Some(h) = st.debounce.take() {
            h.abort();
        }
    }

    /// `(entries, compressed bytes)` held.
    pub fn usage(&self) -> (usize, usize) {
        let st = self.state.lock();
        (st.entries.len(), st.bytes)
    }
}

/// Build the 207 answer of a hit with `tail_xml` (injected responses) appended.
pub(crate) fn respond(hit: Hit, tail_xml: &str, gzip: bool) -> Response<Body> {
    let mut tail = String::with_capacity(tail_xml.len() + MULTISTATUS_CLOSE.len());
    tail.push_str(tail_xml);
    tail.push_str(MULTISTATUS_CLOSE);
    let total = hit.entry.raw_len + tail.len() as u64;
    let mut resp = if gzip {
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::new(6));
        let tail_deflated = enc
            .write_all(tail.as_bytes())
            .and_then(|_| enc.finish())
            .unwrap_or_default();
        let mut crc = hit.entry.crc.clone();
        crc.update(tail.as_bytes());
        let mut trailer = Vec::with_capacity(8);
        trailer.extend_from_slice(&crc.finalize().to_le_bytes());
        trailer.extend_from_slice(&(total as u32).to_le_bytes());
        let len = hit.entry.gz_prefix.len() + tail_deflated.len() + trailer.len();
        let parts: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(hit.entry.gz_prefix.clone()),
            Ok(Bytes::from(tail_deflated)),
            Ok(Bytes::from(trailer)),
        ];
        let mut r = Response::new(Body::from_stream(stream::iter(parts)));
        r.headers_mut()
            .insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        r.headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from(len));
        r
    } else {
        let body = inflate_stream(
            hit.entry.gz_prefix.slice(GZIP_HEADER.len()..),
            Bytes::from(tail),
        );
        let mut r = Response::new(body);
        r.headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from(total));
        r
    };
    *resp.status_mut() = StatusCode::MULTI_STATUS;
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/xml; charset=utf-8"),
    );
    resp.headers_mut()
        .insert(VARY, HeaderValue::from_static("Accept-Encoding"));
    resp
}

/// Stream the inflated prefix in 64 KiB chunks, then the tail (identity clients).
fn inflate_stream(deflated: Bytes, tail: Bytes) -> Body {
    struct St {
        input: Bytes,
        pos: usize,
        dec: flate2::Decompress,
        tail: Option<Bytes>,
        done: bool,
    }
    let st = St {
        input: deflated,
        pos: 0,
        dec: flate2::Decompress::new(false),
        tail: Some(tail),
        done: false,
    };
    let s = stream::unfold(st, |mut st| async move {
        if !st.done {
            let mut out = Vec::with_capacity(64 * 1024);
            while out.len() < out.capacity() && st.pos < st.input.len() {
                let (before_in, before_out) = (st.dec.total_in(), out.len());
                let res = st.dec.decompress_vec(
                    &st.input[st.pos..],
                    &mut out,
                    flate2::FlushDecompress::Sync,
                );
                st.pos += (st.dec.total_in() - before_in) as usize;
                match res {
                    Err(e) => {
                        return Some((
                            Err(std::io::Error::other(e)),
                            St {
                                done: true,
                                tail: None,
                                ..st
                            },
                        ));
                    }
                    // No progress either way: the input is exhausted for this stream.
                    Ok(_) if st.dec.total_in() == before_in && out.len() == before_out => {
                        st.pos = st.input.len();
                        break;
                    }
                    Ok(_) => {}
                }
            }
            if st.pos >= st.input.len() {
                st.done = true;
            }
            if !out.is_empty() {
                return Some((Ok(Bytes::from(out)), st));
            }
        }
        let tail = st.tail.take()?;
        Some((Ok(tail), st))
    });
    Body::from_stream(s)
}
