//! Connection and in-flight gauges (design §8): atomics the listener and the
//! accounting layer bump, the host sampler reads (and resets the peaks of).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct Gauges {
    connections: AtomicU64,
    connections_peak: AtomicU64,
    inflight: AtomicU64,
    inflight_peak: AtomicU64,
}

/// A snapshot of [`Gauges`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GaugeValues {
    pub connections: u64,
    pub connections_peak: u64,
    pub inflight: u64,
    pub inflight_peak: u64,
}

impl Gauges {
    /// One accepted connection, open until the guard drops (`serve.rs`).
    pub fn connection(self: &Arc<Self>) -> ConnectionGuard {
        let now = self.connections.fetch_add(1, Ordering::Relaxed) + 1;
        self.connections_peak.fetch_max(now, Ordering::Relaxed);
        ConnectionGuard(self.clone())
    }

    /// One request in flight, until its response body ends or is dropped.
    pub(crate) fn request(self: &Arc<Self>) -> InflightGuard {
        let now = self.inflight.fetch_add(1, Ordering::Relaxed) + 1;
        self.inflight_peak.fetch_max(now, Ordering::Relaxed);
        InflightGuard(self.clone())
    }

    pub fn values(&self) -> GaugeValues {
        GaugeValues {
            connections: self.connections.load(Ordering::Relaxed),
            connections_peak: self.connections_peak.load(Ordering::Relaxed),
            inflight: self.inflight.load(Ordering::Relaxed),
            inflight_peak: self.inflight_peak.load(Ordering::Relaxed),
        }
    }

    /// The values, then the peaks reset to the current levels (the sampler's interval
    /// starts over).
    pub fn take_peaks(&self) -> GaugeValues {
        let connections = self.connections.load(Ordering::Relaxed);
        let inflight = self.inflight.load(Ordering::Relaxed);
        GaugeValues {
            connections,
            connections_peak: self
                .connections_peak
                .swap(connections, Ordering::Relaxed)
                .max(connections),
            inflight,
            inflight_peak: self
                .inflight_peak
                .swap(inflight, Ordering::Relaxed)
                .max(inflight),
        }
    }
}

pub struct ConnectionGuard(Arc<Gauges>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) struct InflightGuard(Arc<Gauges>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_follow_and_reset() {
        let g = Arc::new(Gauges::default());
        let a = g.connection();
        let b = g.connection();
        let r = g.request();
        drop(b);
        let v = g.take_peaks();
        assert_eq!(
            (
                v.connections,
                v.connections_peak,
                v.inflight,
                v.inflight_peak
            ),
            (1, 2, 1, 1)
        );
        let v = g.values();
        assert_eq!((v.connections_peak, v.inflight_peak), (1, 1));
        drop((a, r));
        assert_eq!(g.values().connections, 0);
        assert_eq!(g.values().inflight, 0);
    }
}
