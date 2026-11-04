//! Crawl episodes (design §8): one row per episode of a client IP touching a large part
//! of the catalog. The foundation creates the table (pruned by `retention.crawl_days`
//! on `last_at`); the detector (bunko-server `analytics::crawl`) adds its statements
//! here, writing inside the collector's flush transaction (`Detector::flush_into`).

pub(crate) const DDL: &[&str] = &[
    // `principals`: JSON array; `kind`: download | metadata | mixed; `status`: open | closed.
    "CREATE TABLE IF NOT EXISTS crawl_episodes (
        id INTEGER PRIMARY KEY, ip TEXT NOT NULL, country TEXT, principals TEXT NOT NULL DEFAULT '[]',
        client TEXT, user_agent TEXT, kind TEXT NOT NULL, started_at INTEGER NOT NULL,
        last_at INTEGER NOT NULL, series INTEGER NOT NULL DEFAULT 0,
        volumes INTEGER NOT NULL DEFAULT 0, catalog_series INTEGER NOT NULL DEFAULT 0,
        coverage REAL NOT NULL DEFAULT 0, bytes_out INTEGER NOT NULL DEFAULT 0,
        requests INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL DEFAULT 'open'
    )",
    "CREATE INDEX IF NOT EXISTS idx_crawl_episodes_ip ON crawl_episodes (ip)",
    "CREATE INDEX IF NOT EXISTS idx_crawl_episodes_last_at ON crawl_episodes (last_at)",
];
