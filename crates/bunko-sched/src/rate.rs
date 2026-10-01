//! How fast a generation reads pages (`ocr/eta.py`, the `RateModel` half).
//!
//! A rate is ALWAYS pages over the time between page *emissions*: model load
//! and pipeline fill never enter one; startup is charged separately, once per
//! session. Evidence, newest first: this process's session volumes (EWMA,
//! or a least-squares cost line when two lengths are known), the congestion
//! history, the saved benchmark, and the volume in flight blended in once it
//! has emitted [`MIN_INFLIGHT_PAGES`]. Blends are done in seconds per page.
//!
//! The clock and the two file-backed priors are injected ([`Clock`],
//! [`PriorSource`]) so the server can share one cached reader and tests can
//! run on a fake clock.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::py::{as_float, round_to, sum};
use crate::pyjson::read_object;
use crate::throughput::{RECENT_VOLUMES, Sample, Throughput, throughput_of};

/// Weight of the newest completed volume in the session EWMA.
pub const SESSION_ALPHA: f64 = 0.5;
/// Pages a volume must have emitted before its own rate counts.
pub const MIN_INFLIGHT_PAGES: i64 = 4;
/// What the prior is worth, in pages of in-flight evidence.
pub const BLEND_PRIOR_PAGES: f64 = 16.0;
/// Pages a session must have finished before it outweighs older evidence.
pub const SESSION_PRIOR_PAGES: f64 = 64.0;
/// A session start nothing has measured (marked rough wherever shown).
pub const DEFAULT_STARTUP_SECONDS: f64 = 20.0;
/// How long a file-backed source is reused before being read again.
pub const CACHE_TTL_SECONDS: f64 = 5.0;
/// The most a volume's fixed cost may be believed to be.
pub const MAX_LATENCY_SECONDS: f64 = 60.0;
/// Recent (pages, seconds) pairs the latency fit keeps per key.
pub const LATENCY_SAMPLES: usize = 8;

pub const CONGESTION_FILE: &str = ".ocr-congestion.json";
pub const BENCH_FILE: &str = ".ocr-bench.json";

pub const SOURCE_SESSION: &str = "session";
pub const SOURCE_SESSION_OPENING: &str = "session (opening volume)";
pub const SOURCE_HISTORY: &str = "history";
pub const SOURCE_BENCH: &str = "bench";
pub const SOURCE_VOLUME: &str = "volume";
pub const FIT_SUFFIX: &str = " (fit)";

pub const STARTUP_SESSION: &str = "session";
pub const STARTUP_BENCH: &str = "bench";
pub const STARTUP_DEFAULT: &str = "default";

/// The machine key of this server (`LOCAL_SLOT`).
pub const LOCAL_MACHINE: &str = "local";

/// `_rate_key(gen, hardware)`: the row id for this server, `<id>@<name>`
/// for a processor.
pub fn rate_key(generation_id: &str, machine: &str) -> String {
    if machine == LOCAL_MACHINE {
        generation_id.to_owned()
    } else {
        format!("{generation_id}@{machine}")
    }
}

/// `emission_rate(M, t)`: `(M - 1) / t` for `M >= 2, t > 0`, else None.
pub fn emission_rate(pages_done: i64, seconds_since_first: f64) -> Option<f64> {
    if pages_done < 2 || seconds_since_first <= 0.0 {
        return None;
    }
    let rate = (pages_done - 1) as f64 / seconds_since_first;
    if rate > 0.0 { Some(rate) } else { None }
}

/// A volume's cost line: `seconds = latency + pages * seconds_per_page`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VolumeCostFit {
    pub latency_seconds: f64,
    pub seconds_per_page: f64,
}

impl VolumeCostFit {
    pub fn pages_per_second(&self) -> f64 {
        1.0 / self.seconds_per_page
    }
}

/// `fit_volume_cost(samples, alpha)`: weighted least squares over
/// `(pages, seconds)`, newest weighted 1 and each older one by `(1-alpha)`
/// more. None with fewer than two distinct page counts or a slope ≤ 0; the
/// intercept is clamped to `[0, 60]`.
pub fn fit_volume_cost(samples: &[(f64, f64)], alpha: f64) -> Option<VolumeCostFit> {
    let pairs: Vec<(f64, f64)> = samples
        .iter()
        .copied()
        .filter(|(p, s)| *p > 0.0 && *s > 0.0)
        .collect();
    let mut distinct: Vec<f64> = Vec::new();
    for (p, _) in &pairs {
        // Python's set: 1 and 1.0 are one value; -0.0 cannot occur (p > 0).
        if !distinct.contains(p) {
            distinct.push(*p);
        }
        if distinct.len() >= 2 {
            break;
        }
    }
    if distinct.len() < 2 {
        return None;
    }
    let last = pairs.len() - 1;
    let (mut total, mut sum_x, mut sum_y, mut sum_xx, mut sum_xy) =
        (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for (index, (x, y)) in pairs.iter().enumerate() {
        let weight = (1.0 - alpha).powf((last - index) as f64);
        total += weight;
        sum_x += weight * x;
        sum_y += weight * y;
        sum_xx += weight * x * x;
        sum_xy += weight * x * y;
    }
    let denominator = total * sum_xx - sum_x * sum_x;
    if denominator <= 0.0 {
        return None;
    }
    let slope = (total * sum_xy - sum_x * sum_y) / denominator;
    if slope <= 0.0 {
        return None;
    }
    let intercept = (sum_y - slope * sum_x) / total;
    Some(VolumeCostFit {
        latency_seconds: MAX_LATENCY_SECONDS.min(0.0f64.max(intercept)),
        seconds_per_page: slope,
    })
}

/// `fit_latency`: just the fixed part of [`fit_volume_cost`].
pub fn fit_latency(samples: &[(f64, f64)], alpha: f64) -> Option<f64> {
    fit_volume_cost(samples, alpha).map(|f| f.latency_seconds)
}

/// One row's speed, its fixed per-volume cost, and where both came from.
#[derive(Clone, Debug, PartialEq)]
pub struct RateEstimate {
    pub pages_per_second: f64,
    pub source: String,
    pub volumes_observed: i64,
    pub latency_seconds: f64,
}

impl RateEstimate {
    pub fn new(
        pages_per_second: f64,
        source: impl Into<String>,
        volumes_observed: i64,
        latency_seconds: f64,
    ) -> Self {
        RateEstimate {
            pages_per_second,
            source: source.into(),
            volumes_observed,
            latency_seconds,
        }
    }

    /// A benchmark prior: a page rate and no fixed cost.
    pub fn bench(pages_per_second: f64) -> Self {
        RateEstimate::new(pages_per_second, SOURCE_BENCH, 0, 0.0)
    }

    /// How long `pages` take to read (no fixed cost).
    pub fn seconds_for(&self, pages: f64) -> f64 {
        pages / self.pages_per_second
    }

    /// A whole volume of `pages`, fill and drain included.
    pub fn volume_seconds(&self, pages: f64) -> f64 {
        self.latency_seconds + self.seconds_for(pages)
    }

    pub fn as_dict(&self) -> Value {
        json!({
            "pages_per_second": round_to(self.pages_per_second, 4),
            "latency_seconds": round_to(self.latency_seconds, 2),
            "source": self.source,
            "volumes_observed": self.volumes_observed,
        })
    }
}

/// What opening a session for one row costs, and where that came from.
#[derive(Clone, Debug, PartialEq)]
pub struct StartupEstimate {
    pub seconds: f64,
    pub source: String,
    pub rough: bool,
}

impl StartupEstimate {
    pub fn new(seconds: f64, source: impl Into<String>) -> Self {
        StartupEstimate {
            seconds,
            source: source.into(),
            rough: false,
        }
    }

    /// `DEFAULT_STARTUP`: 20 s, rough.
    pub fn default_estimate() -> Self {
        StartupEstimate {
            seconds: DEFAULT_STARTUP_SECONDS,
            source: STARTUP_DEFAULT.to_owned(),
            rough: true,
        }
    }

    pub fn as_dict(&self) -> Value {
        json!({"seconds": round_to(self.seconds, 2), "source": self.source, "rough": self.rough})
    }
}

/// Wall and monotonic clocks, injectable for tests.
pub trait Clock: Send + Sync {
    /// `time.time()`: seconds since the epoch.
    fn time(&self) -> f64;
    /// `time.monotonic()`.
    fn monotonic(&self) -> f64;
}

/// The real clocks.
#[derive(Debug)]
pub struct SystemClock {
    start: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        SystemClock {
            start: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn time(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }

    fn monotonic(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
}

/// A clock that only moves when told to (tests, replays).
#[derive(Debug, Default)]
pub struct ManualClock {
    now: Mutex<f64>,
}

impl ManualClock {
    pub fn new(now: f64) -> Self {
        ManualClock {
            now: Mutex::new(now),
        }
    }

    pub fn set(&self, now: f64) {
        *lock(&self.now) = now;
    }
}

impl Clock for ManualClock {
    fn time(&self) -> f64 {
        *lock(&self.now)
    }

    fn monotonic(&self) -> f64 {
        *lock(&self.now)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock only means another thread panicked mid-update of plain
    // numbers; the data is still usable for an estimate.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The two file-backed priors: `.ocr-congestion.json` (history) and
/// `.ocr-bench.json` (saved benchmarks). Either may be empty.
pub trait PriorSource: Send + Sync {
    fn congestion(&self) -> Arc<Map<String, Value>>;
    fn bench(&self) -> Arc<Map<String, Value>>;
}

/// In-memory priors (tests, or a server that keeps them in memory).
#[derive(Debug, Default)]
pub struct StaticPriors {
    congestion: Mutex<Arc<Map<String, Value>>>,
    bench: Mutex<Arc<Map<String, Value>>>,
}

impl StaticPriors {
    pub fn new(congestion: Map<String, Value>, bench: Map<String, Value>) -> Self {
        StaticPriors {
            congestion: Mutex::new(Arc::new(congestion)),
            bench: Mutex::new(Arc::new(bench)),
        }
    }

    pub fn set_congestion(&self, data: Map<String, Value>) {
        *lock(&self.congestion) = Arc::new(data);
    }

    pub fn set_bench(&self, data: Map<String, Value>) {
        *lock(&self.bench) = Arc::new(data);
    }
}

impl PriorSource for StaticPriors {
    fn congestion(&self) -> Arc<Map<String, Value>> {
        lock(&self.congestion).clone()
    }

    fn bench(&self) -> Arc<Map<String, Value>> {
        lock(&self.bench).clone()
    }
}

/// One cached JSON object and when (monotonic) it was read.
type CachedObject = Mutex<Option<(f64, Arc<Map<String, Value>>)>>;

/// The two JSON files under the storage root, each re-read at most once per
/// TTL (monotonic). A missing, half-written or corrupt file is "nothing".
pub struct FilePriors {
    storage: Option<PathBuf>,
    ttl: f64,
    clock: Arc<dyn Clock>,
    congestion: CachedObject,
    bench: CachedObject,
}

impl FilePriors {
    pub fn new(storage: Option<&Path>, ttl: f64, clock: Arc<dyn Clock>) -> Self {
        FilePriors {
            storage: storage.map(Path::to_path_buf),
            ttl,
            clock,
            congestion: Mutex::new(None),
            bench: Mutex::new(None),
        }
    }

    fn cached(&self, slot: &CachedObject, file: &str) -> Arc<Map<String, Value>> {
        let now = self.clock.monotonic();
        if let Some((at, data)) = lock(slot).as_ref()
            && now - at <= self.ttl
        {
            return data.clone();
        }
        let data = Arc::new(
            self.storage
                .as_ref()
                .and_then(|s| read_object(&s.join(file)))
                .unwrap_or_default(),
        );
        *lock(slot) = Some((now, data.clone()));
        data
    }
}

impl PriorSource for FilePriors {
    fn congestion(&self) -> Arc<Map<String, Value>> {
        self.cached(&self.congestion, CONGESTION_FILE)
    }

    fn bench(&self) -> Arc<Map<String, Value>> {
        self.cached(&self.bench, BENCH_FILE)
    }
}

/// `_SessionRate`: one key's completed volumes this process.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionRate {
    /// EWMA over steady-state volumes only.
    pub pages_per_second: f64,
    pub steady: i64,
    /// Every completed volume, opening ones included.
    pub volumes: i64,
    pub pages: f64,
    /// The opening volumes' own rate (EWMA), used only while nothing else is.
    pub opening_rate: f64,
    /// Steady (pages, seconds) pairs for the fit, newest last (≤ 8).
    pub samples: Vec<(f64, f64)>,
    /// Wall clock of the newest volume folded in.
    pub last_at: f64,
    /// Every completed volume's (pages, seconds), newest last (≤ 20).
    pub recent: Vec<(f64, f64)>,
}

impl SessionRate {
    fn add(&mut self, pages: f64, seconds: f64, alpha: f64, steady: bool, now: f64) {
        let rate = pages / seconds;
        self.last_at = now;
        self.volumes += 1;
        self.pages += pages;
        self.recent.push((pages, seconds));
        trim_front(&mut self.recent, RECENT_VOLUMES);
        if !steady {
            self.opening_rate = if self.opening_rate <= 0.0 {
                rate
            } else {
                alpha * rate + (1.0 - alpha) * self.opening_rate
            };
            return;
        }
        if self.steady == 0 {
            self.pages_per_second = rate;
        } else {
            self.pages_per_second = alpha * rate + (1.0 - alpha) * self.pages_per_second;
        }
        self.steady += 1;
        self.samples.push((pages, seconds));
        trim_front(&mut self.samples, LATENCY_SAMPLES);
    }
}

fn trim_front<T>(v: &mut Vec<T>, keep: usize) {
    if v.len() > keep {
        let drop = v.len() - keep;
        v.drain(..drop);
    }
}

#[derive(Default)]
struct State {
    session: HashMap<String, SessionRate>,
    /// Insertion order of `session` keys (Python dict order).
    order: Vec<String>,
    startup: HashMap<String, f64>,
}

/// Pages per second and session startup, per generation id / rate key.
/// Thread-safe; every method takes `&self`.
pub struct RateModel {
    alpha: f64,
    clock: Arc<dyn Clock>,
    priors: Arc<dyn PriorSource>,
    state: Mutex<State>,
}

impl RateModel {
    /// A model over `<storage>/.ocr-congestion.json` and `.ocr-bench.json`
    /// (None: both priors empty), on the system clock.
    pub fn with_storage(storage: Option<&Path>) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::default());
        let priors = Arc::new(FilePriors::new(storage, CACHE_TTL_SECONDS, clock.clone()));
        RateModel::new(clock, priors, SESSION_ALPHA)
    }

    pub fn new(clock: Arc<dyn Clock>, priors: Arc<dyn PriorSource>, alpha: f64) -> Self {
        RateModel {
            alpha,
            clock,
            priors,
            state: Mutex::new(State::default()),
        }
    }

    // --- recording ---------------------------------------------------------

    /// Fold one finished volume into `key`'s session rate. A pair that cannot
    /// make a rate (≤ 0) is ignored. `first_of_session`: the volume a session
    /// opened with (kept out of the EWMA and the fit; its pages still count).
    pub fn record_volume(&self, key: &str, pages: f64, seconds: f64, first_of_session: bool) {
        if pages <= 0.0 || seconds <= 0.0 || pages.is_nan() || seconds.is_nan() {
            return;
        }
        let now = self.clock.time();
        let mut st = lock(&self.state);
        if !st.session.contains_key(key) {
            st.order.push(key.to_owned());
        }
        st.session.entry(key.to_owned()).or_default().add(
            pages,
            seconds,
            self.alpha,
            !first_of_session,
            now,
        );
    }

    /// [`Self::record_volume`] from raw event values (`_as_float` rules).
    pub fn record_volume_values(
        &self,
        key: &str,
        pages: Option<&Value>,
        seconds: Option<&Value>,
        first_of_session: bool,
    ) {
        if let (Some(p), Some(s)) = (as_float(pages), as_float(seconds)) {
            self.record_volume(key, p, s, first_of_session);
        }
    }

    /// What `key`'s last session cost to open (a `ready` event). Negative ignored.
    pub fn record_startup(&self, key: &str, seconds: f64) {
        if seconds < 0.0 || seconds.is_nan() {
            return;
        }
        lock(&self.state).startup.insert(key.to_owned(), seconds);
    }

    /// Drop a row's in-process measurements on every machine (`id`, `id@*`).
    pub fn forget(&self, generation_id: &str) {
        let prefix = format!("{generation_id}@");
        let mut st = lock(&self.state);
        let gone = |k: &String| k == generation_id || k.starts_with(&prefix);
        st.session.retain(|k, _| !gone(k));
        st.order.retain(|k| !gone(k));
        st.startup.retain(|k, _| !gone(k));
    }

    /// A snapshot of one key's session evidence.
    pub fn session(&self, key: &str) -> Option<SessionRate> {
        lock(&self.state).session.get(key).cloned()
    }

    // --- asking ------------------------------------------------------------

    /// Machines that finished a volume of this row in the last `within`
    /// seconds: `"local"` for the row's own key, the name for `<id>@<name>`.
    /// Newest first.
    pub fn machines_with_evidence(&self, generation_id: &str, within: f64) -> Vec<String> {
        let cutoff = self.clock.time() - within;
        let prefix = format!("{generation_id}@");
        let st = lock(&self.state);
        let mut found: Vec<(f64, String)> = Vec::new();
        for key in &st.order {
            let Some(entry) = st.session.get(key) else {
                continue;
            };
            if entry.volumes <= 0 || entry.last_at < cutoff {
                continue;
            }
            if key == generation_id {
                found.push((entry.last_at, LOCAL_MACHINE.to_owned()));
            } else if let Some(name) = key.strip_prefix(&prefix) {
                found.push((entry.last_at, name.to_owned()));
            }
        }
        found.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        found.into_iter().map(|(_, name)| name).collect()
    }

    /// What the machine filed under `key` really delivered this process.
    pub fn throughput(&self, key: &str) -> Option<Throughput> {
        let (samples, last_at) = {
            let st = lock(&self.state);
            let entry = st.session.get(key)?;
            if entry.recent.is_empty() {
                return None;
            }
            let last = if entry.last_at != 0.0 {
                Some(entry.last_at)
            } else {
                None
            };
            (
                entry
                    .recent
                    .iter()
                    .map(|(p, s)| Sample::new(*p, *s))
                    .collect::<Vec<_>>(),
                last,
            )
        };
        throughput_of(&samples, last_at)
    }

    /// `rate(gen, observed…)`: session over (history or bench), with the
    /// volume in flight blended in.
    pub fn rate(
        &self,
        generation_id: &str,
        observed_pages: i64,
        observed_seconds: f64,
    ) -> Option<RateEstimate> {
        self.with_inflight(
            self.base_rate(generation_id),
            observed_pages,
            observed_seconds,
        )
    }

    /// `rate_on(gen, machine_key, machine_prior, observed…)`: the row on ONE
    /// machine (`machine_key` None or the row id = this server).
    pub fn rate_on(
        &self,
        generation_id: &str,
        machine_key: Option<&str>,
        machine_prior: Option<&RateEstimate>,
        observed_pages: i64,
        observed_seconds: f64,
    ) -> Option<RateEstimate> {
        self.with_inflight(
            self.machine_base(generation_id, machine_key, machine_prior),
            observed_pages,
            observed_seconds,
        )
    }

    fn with_inflight(
        &self,
        base: Option<RateEstimate>,
        observed_pages: i64,
        observed_seconds: f64,
    ) -> Option<RateEstimate> {
        let live = emission_rate(observed_pages, observed_seconds);
        let live = match live {
            Some(l) if observed_pages >= MIN_INFLIGHT_PAGES => l,
            _ => return base,
        };
        let Some(base) = base else {
            return Some(RateEstimate::new(live, SOURCE_VOLUME, 0, 0.0));
        };
        let n = observed_pages as f64;
        let weight = n / (n + BLEND_PRIOR_PAGES);
        let blended_spp = weight * (1.0 / live) + (1.0 - weight) * (1.0 / base.pages_per_second);
        Some(RateEstimate::new(
            1.0 / blended_spp,
            format!("{}+{}", base.source, SOURCE_VOLUME),
            base.volumes_observed,
            base.latency_seconds,
        ))
    }

    /// This row's fixed per-volume cost; 0.0 when unfittable.
    pub fn latency(&self, generation_id: &str) -> f64 {
        self.base_rate(generation_id)
            .map_or(0.0, |b| b.latency_seconds)
    }

    /// What opening a session for this row costs (never None).
    pub fn startup(&self, generation_id: &str) -> StartupEstimate {
        if let Some(measured) = lock(&self.state).startup.get(generation_id).copied() {
            return StartupEstimate::new(measured, STARTUP_SESSION);
        }
        let bench = self.priors.bench();
        let saved = bench
            .get(generation_id)
            .and_then(Value::as_object)
            .and_then(|row| as_float(row.get("startup_seconds")));
        if let Some(saved) = saved
            && saved > 0.0
        {
            return StartupEstimate::new(saved, STARTUP_BENCH);
        }
        StartupEstimate::default_estimate()
    }

    /// What opening a session for this row costs on ONE machine.
    pub fn startup_on(
        &self,
        generation_id: &str,
        machine_key: Option<&str>,
        machine_prior: Option<f64>,
    ) -> StartupEstimate {
        let prior = machine_prior.filter(|p| *p > 0.0);
        match machine_key {
            None => self.startup_local(generation_id, prior),
            Some(k) if k == generation_id => self.startup_local(generation_id, prior),
            Some(key) => {
                if let Some(measured) = lock(&self.state).startup.get(key).copied() {
                    return StartupEstimate::new(measured, STARTUP_SESSION);
                }
                if let Some(p) = prior {
                    return StartupEstimate::new(p, STARTUP_BENCH);
                }
                self.startup(generation_id)
            }
        }
    }

    fn startup_local(&self, generation_id: &str, prior: Option<f64>) -> StartupEstimate {
        let found = self.startup(generation_id);
        match prior {
            Some(p) if found.source == STARTUP_DEFAULT => StartupEstimate::new(p, STARTUP_BENCH),
            _ => found,
        }
    }

    /// `{rate: {…}|null, startup: {…}}`.
    pub fn report(&self, generation_id: &str) -> Value {
        json!({
            "rate": self.rate(generation_id, 0, 0.0).map(|r| r.as_dict()),
            "startup": self.startup(generation_id).as_dict(),
        })
    }

    // --- sources -----------------------------------------------------------

    fn base_rate(&self, generation_id: &str) -> Option<RateEstimate> {
        let prior = self
            .history_rate(generation_id)
            .or_else(|| self.bench_rate(generation_id));
        self.over_prior(generation_id, prior)
    }

    fn machine_base(
        &self,
        generation_id: &str,
        machine_key: Option<&str>,
        machine_prior: Option<&RateEstimate>,
    ) -> Option<RateEstimate> {
        let key = match machine_key {
            Some(k) if k != generation_id => k,
            _ => {
                let prior = self
                    .history_rate(generation_id)
                    .or_else(|| self.bench_rate(generation_id))
                    .or_else(|| machine_prior.cloned());
                return self.over_prior(generation_id, prior);
            }
        };
        if let Some(own) = self.over_prior(key, machine_prior.cloned()) {
            return Some(own);
        }
        if let Some(here) = self.over_prior(generation_id, self.history_rate(generation_id)) {
            return Some(here);
        }
        if let Some(other) = self.other_machines(generation_id, key) {
            return Some(other);
        }
        self.bench_rate(generation_id)
    }

    fn other_machines(&self, generation_id: &str, exclude: &str) -> Option<RateEstimate> {
        let prefix = format!("{generation_id}@");
        let mut candidates: Vec<(f64, String)> = {
            let st = lock(&self.state);
            st.order
                .iter()
                .filter_map(|k| st.session.get(k).map(|e| (k, e)))
                .filter(|(k, e)| k.starts_with(&prefix) && k.as_str() != exclude && e.volumes > 0)
                .map(|(k, e)| (e.pages, k.clone()))
                .collect()
        };
        candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        candidates
            .into_iter()
            .find_map(|(_, key)| self.over_prior(&key, None))
    }

    fn over_prior(&self, key: &str, prior: Option<RateEstimate>) -> Option<RateEstimate> {
        let (session, session_pages) = {
            let st = lock(&self.state);
            let entry = st.session.get(key);
            (self.session_rate(entry), entry.map_or(0.0, |e| e.pages))
        };
        let Some(session) = session else { return prior };
        let Some(prior) = prior else {
            return Some(session);
        };
        let weight = session_pages / (session_pages + SESSION_PRIOR_PAGES);
        let blended = weight * (1.0 / session.pages_per_second)
            + (1.0 - weight) * (1.0 / prior.pages_per_second);
        let latency = weight * session.latency_seconds + (1.0 - weight) * prior.latency_seconds;
        let source = if weight >= 0.5 {
            session.source.clone()
        } else {
            format!("{}+{}", session.source, prior.source)
        };
        Some(RateEstimate::new(
            1.0 / blended,
            source,
            session.volumes_observed,
            latency,
        ))
    }

    fn session_rate(&self, entry: Option<&SessionRate>) -> Option<RateEstimate> {
        let entry = entry?;
        if entry.volumes == 0 {
            return None;
        }
        if let Some(fit) = fit_volume_cost(&entry.samples, self.alpha) {
            return Some(RateEstimate::new(
                fit.pages_per_second(),
                format!("{SOURCE_SESSION}{FIT_SUFFIX}"),
                entry.volumes,
                fit.latency_seconds,
            ));
        }
        if entry.steady > 0 && entry.pages_per_second > 0.0 {
            return Some(RateEstimate::new(
                entry.pages_per_second,
                SOURCE_SESSION,
                entry.volumes,
                0.0,
            ));
        }
        if entry.opening_rate > 0.0 {
            return Some(RateEstimate::new(
                entry.opening_rate,
                SOURCE_SESSION_OPENING,
                entry.volumes,
                0.0,
            ));
        }
        None
    }

    fn history_pairs(&self, generation_id: &str, steady_only: bool) -> Vec<(f64, f64)> {
        let congestion = self.priors.congestion();
        let Some(Value::Array(runs)) = congestion.get(generation_id) else {
            return Vec::new();
        };
        let truthy_num = |v: Option<&Value>| as_float(v).filter(|f| *f != 0.0);
        let mut pairs = Vec::new();
        for run in runs {
            let Some(run) = run.as_object() else { continue };
            if steady_only && crate::py::truthy(run.get("volume_first")) {
                continue;
            }
            let pages = truthy_num(run.get("volume_pages")).or_else(|| as_float(run.get("pages")));
            let seconds =
                truthy_num(run.get("volume_seconds")).or_else(|| as_float(run.get("elapsed")));
            if let (Some(p), Some(s)) = (pages, seconds)
                && p != 0.0
                && s != 0.0
                && p > 0.0
                && s > 0.0
            {
                pairs.push((p, s));
            }
        }
        pairs
    }

    /// The history prior of a row: its recorded runs' fit, else pooled.
    pub fn history_rate(&self, generation_id: &str) -> Option<RateEstimate> {
        let mut pairs = self.history_pairs(generation_id, true);
        if pairs.is_empty() {
            pairs = self.history_pairs(generation_id, false);
        }
        if pairs.is_empty() {
            return None;
        }
        if let Some(fit) = fit_volume_cost(&pairs, self.alpha) {
            return Some(RateEstimate::new(
                fit.pages_per_second(),
                format!("{SOURCE_HISTORY}{FIT_SUFFIX}"),
                pairs.len() as i64,
                fit.latency_seconds,
            ));
        }
        let pages = sum(pairs.iter().map(|p| p.0));
        let seconds = sum(pairs.iter().map(|p| p.1));
        if seconds <= 0.0 {
            return None;
        }
        Some(RateEstimate::new(
            pages / seconds,
            SOURCE_HISTORY,
            pairs.len() as i64,
            0.0,
        ))
    }

    /// The saved-benchmark prior of a row (`best`, else `baseline`).
    pub fn bench_rate(&self, generation_id: &str) -> Option<RateEstimate> {
        let bench = self.priors.bench();
        let row = bench.get(generation_id)?.as_object()?;
        for block in [row.get("best"), row.get("baseline")] {
            if let Some(Value::Object(block)) = block
                && let Some(rate) = as_float(block.get("pages_per_second"))
                && rate > 0.0
            {
                return Some(RateEstimate::bench(rate));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> (Arc<ManualClock>, RateModel) {
        let clock = Arc::new(ManualClock::new(1000.0));
        let priors = Arc::new(StaticPriors::default());
        (clock.clone(), RateModel::new(clock, priors, SESSION_ALPHA))
    }

    #[test]
    fn fit_reproduces_a_line() {
        let fit = fit_volume_cost(&[(4.0, 2.0), (12.0, 3.0), (24.0, 4.0)], 0.5).unwrap();
        assert!(fit.latency_seconds > 1.0 && fit.latency_seconds < 2.0);
        assert!(fit_volume_cost(&[(4.0, 2.0), (4.0, 3.0)], 0.5).is_none());
        assert!(fit_volume_cost(&[(4.0, 3.0), (12.0, 2.0)], 0.5).is_none());
    }

    #[test]
    fn opening_volume_then_steady() {
        let (_c, m) = model();
        m.record_volume("g", 10.0, 5.0, true);
        let r = m.rate("g", 0, 0.0).unwrap();
        assert_eq!(r.source, SOURCE_SESSION_OPENING);
        assert_eq!(r.pages_per_second, 2.0);
        m.record_volume("g", 20.0, 5.0, false);
        let r = m.rate("g", 0, 0.0).unwrap();
        assert_eq!(r.source, SOURCE_SESSION);
        assert_eq!(r.pages_per_second, 4.0);
        assert_eq!(r.volumes_observed, 2);
    }

    #[test]
    fn startup_chain() {
        let (_c, m) = model();
        assert_eq!(m.startup("g"), StartupEstimate::default_estimate());
        assert_eq!(m.startup_on("g", None, Some(7.0)).seconds, 7.0);
        m.record_startup("g@box", 3.0);
        assert_eq!(m.startup_on("g", Some("g@box"), Some(7.0)).seconds, 3.0);
        assert_eq!(
            m.startup_on("g", Some("g@other"), Some(7.0)).source,
            STARTUP_BENCH
        );
        m.forget("g");
        assert_eq!(
            m.startup_on("g", Some("g@box"), None),
            StartupEstimate::default_estimate()
        );
    }

    #[test]
    fn file_priors_read_and_cache() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(BENCH_FILE),
            r#"{"g": {"best": {"pages_per_second": 4.5}, "startup_seconds": 9}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join(CONGESTION_FILE), "{not json").unwrap();
        let clock = Arc::new(ManualClock::new(0.0));
        let priors = Arc::new(FilePriors::new(
            Some(dir.path()),
            CACHE_TTL_SECONDS,
            clock.clone(),
        ));
        let m = RateModel::new(clock.clone(), priors, SESSION_ALPHA);
        assert_eq!(m.rate("g", 0, 0.0), Some(RateEstimate::bench(4.5)));
        assert_eq!(m.startup("g"), StartupEstimate::new(9.0, STARTUP_BENCH));
        // Within the TTL the cached copy answers; after it the file is re-read.
        std::fs::remove_file(dir.path().join(BENCH_FILE)).unwrap();
        clock.set(4.0);
        assert!(m.rate("g", 0, 0.0).is_some());
        clock.set(10.0);
        assert!(m.rate("g", 0, 0.0).is_none());
    }
}
