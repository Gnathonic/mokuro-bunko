//! The host (design §8): process memory, CPU, connections and requests in flight, per
//! minute and per hour. The foundation creates the tables; the sampler (bunko-server
//! `analytics::host`) adds its statements here.

pub(crate) const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS host_minute (
        minute INTEGER PRIMARY KEY, rss_avg INTEGER, rss_max INTEGER, cpu_pct REAL,
        connections_max INTEGER NOT NULL DEFAULT 0, inflight_max INTEGER NOT NULL DEFAULT 0
    )",
    "CREATE TABLE IF NOT EXISTS host_hourly (
        hour INTEGER PRIMARY KEY, rss_avg INTEGER, rss_max INTEGER, cpu_avg REAL,
        cpu_max REAL, connections_max INTEGER NOT NULL DEFAULT 0,
        inflight_max INTEGER NOT NULL DEFAULT 0
    )",
];
