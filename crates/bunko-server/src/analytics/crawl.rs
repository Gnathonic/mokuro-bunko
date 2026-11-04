//! Crawl detection (design §8): per client IP, the distinct series touched by volume
//! downloads and by per-series metadata over `analytics.crawl.window_hours`; an
//! episode opens past `min_series` and `min_coverage` of the catalog. Detected and
//! reported only: nothing is blocked or slowed.
//!
//! FOUNDATION STUB (area B3 fills it in): [`Detector::observe`] ignores what it is
//! given and [`Detector::flush_into`] writes nothing. The collector already calls
//! `observe` for every non-processor request (outside its lock) and `flush_into` inside
//! every flush transaction (so open episodes' counters are updated with the window).

use crate::accounts::LibraryCounts;
use crate::analytics::clients::ClientInfo;
use crate::analytics::geo::Country;
use crate::core::Core;
use bunko_db::analytics::AnalyticsTx;
use std::sync::Arc;

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

/// The detector: bounded per-IP trackers (4 096 IPs, LRU).
pub struct Detector {
    #[allow(dead_code)] // B3: thresholds from the live `analytics.crawl` settings.
    core: Core,
    /// The catalog's series count (`LibraryCounts::total_series`).
    #[allow(dead_code)]
    counts: Option<Arc<dyn LibraryCounts>>,
}

impl Detector {
    pub fn new(core: Core, counts: Option<Arc<dyn LibraryCounts>>) -> Detector {
        Detector { core, counts }
    }

    /// Cheap, synchronous and bounded: called once per non-processor request.
    pub fn observe(&self, _o: &Observation<'_>) {}

    /// Write open episodes' rows (`crawl_episodes`) inside the flush transaction, as of
    /// `now` (epoch seconds); close the ones idle for a window.
    pub fn flush_into(&self, _tx: &AnalyticsTx<'_>, _now: i64) -> bunko_db::Result<()> {
        Ok(())
    }
}

pub mod api {
    use crate::analytics::Analytics;
    use crate::analytics::api::{ApiQuery, not_collected};
    use axum::response::Response;

    /// `GET /_admin/api/analytics/crawlers` (design §10).
    pub async fn crawlers(a: &Analytics, q: &ApiQuery) -> Response {
        not_collected(a, q).await
    }
}
