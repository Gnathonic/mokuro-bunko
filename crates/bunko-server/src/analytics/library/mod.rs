//! The library's shape (design §7): file types, page formats inside archives, growth,
//! uploads per uploader and dormant series, from a background scan.
//!
//! FOUNDATION STUB (area B2 fills it in): the scanner does nothing and the API says
//! "not collected yet". `uploads_daily` is already written by the collector.

pub mod api;

use crate::analytics::Analytics;
use tokio_util::sync::CancellationToken;

pub struct Scanner {}

impl Scanner {
    pub fn new() -> Scanner {
        Scanner {}
    }

    /// Start the periodic scan (2 min after start, then every
    /// `analytics.library_scan.interval_hours`).
    pub fn start(&self, _a: &Analytics, _stop: CancellationToken) {}

    /// Scan again now (`POST .../library/rescan`).
    pub fn rescan(&self) {}
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}
