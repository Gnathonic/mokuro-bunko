//! The host (design §8): process RSS and peak, CPU, connections and requests in
//! flight, sampled every 15 s into `host_minute` (and rolled into `host_hourly`).
//!
//! FOUNDATION STUB (area B3 fills it in): [`Host::start`] samples nothing and
//! [`Host::now`] reports only the gauges. The listener and the accounting layer already
//! keep the [`Gauges`] current.

use crate::analytics::{Analytics, Gauges};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The host right now (`GET .../host` `now`, the overview's KPIs). `None`: this platform
/// has no reader for it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostNow {
    pub rss: Option<u64>,
    pub peak_rss: Option<u64>,
    /// Percent of one core over the last interval.
    pub cpu_pct: Option<f64>,
    pub connections: u64,
    pub inflight: u64,
    pub host_total: Option<u64>,
    pub host_available: Option<u64>,
}

pub struct Host {
    gauges: Arc<Gauges>,
}

impl Host {
    pub fn new(gauges: Arc<Gauges>) -> Host {
        Host { gauges }
    }

    /// Start the 15 s sampler (it writes `host_minute` itself and resets the gauges'
    /// peaks with [`Gauges::take_peaks`]).
    pub fn start(&self, _a: &Analytics, _stop: CancellationToken) {}

    pub fn now(&self) -> HostNow {
        let g = self.gauges.values();
        HostNow {
            connections: g.connections,
            inflight: g.inflight,
            ..HostNow::default()
        }
    }
}

pub mod api {
    use crate::analytics::Analytics;
    use crate::analytics::api::{ApiQuery, not_collected};
    use axum::response::Response;

    /// `GET /_admin/api/analytics/host` (design §10).
    pub async fn host(a: &Analytics, q: &ApiQuery) -> Response {
        not_collected(a, q).await
    }
}
