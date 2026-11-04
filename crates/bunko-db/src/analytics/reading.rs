//! Reading (design §6): positions, per-day reading, history cursors, the reader's own
//! progress records and devices. The foundation creates the tables; the reading
//! worker (bunko-server `analytics::reading`) adds its statements here as
//! `impl AnalyticsTx` / `impl AnalyticsDb` blocks.

pub(crate) const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS read_positions (
        username TEXT NOT NULL, volume_uuid TEXT NOT NULL, t_ms INTEGER NOT NULL,
        page INTEGER NOT NULL DEFAULT 0, chars INTEGER NOT NULL DEFAULT 0, device TEXT,
        PRIMARY KEY (username, volume_uuid)
    ) WITHOUT ROWID",
    // `sources`: bit 1 positions.json, bit 2 history.
    "CREATE TABLE IF NOT EXISTS reading_daily (
        day INTEGER NOT NULL, username TEXT NOT NULL, volume_uuid TEXT NOT NULL,
        volume_key TEXT, series_key TEXT NOT NULL DEFAULT '', series_title TEXT,
        volume_title TEXT, opens INTEGER NOT NULL DEFAULT 0, pages INTEGER NOT NULL DEFAULT 0,
        chars INTEGER NOT NULL DEFAULT 0, dwell_ms INTEGER NOT NULL DEFAULT 0,
        sources INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (day, username, volume_uuid)
    ) WITHOUT ROWID",
    "CREATE INDEX IF NOT EXISTS idx_reading_daily_series ON reading_daily (series_key, day)",
    "CREATE TABLE IF NOT EXISTS history_cursor (
        username TEXT NOT NULL, device TEXT NOT NULL, segment TEXT NOT NULL,
        last_seq INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (username, device, segment)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS volume_progress (
        username TEXT NOT NULL, volume_uuid TEXT NOT NULL, series_uuid TEXT,
        series_key TEXT NOT NULL DEFAULT '', series_title TEXT, volume_title TEXT,
        progress INTEGER NOT NULL DEFAULT 0, chars INTEGER NOT NULL DEFAULT 0,
        completed INTEGER NOT NULL DEFAULT 0, completed_at INTEGER,
        time_read_min REAL NOT NULL DEFAULT 0, last_progress_update INTEGER,
        updated_at INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (username, volume_uuid)
    ) WITHOUT ROWID",
    "CREATE INDEX IF NOT EXISTS idx_volume_progress_series ON volume_progress (series_key)",
    "CREATE TABLE IF NOT EXISTS devices (
        username TEXT NOT NULL, device_id TEXT NOT NULL, label TEXT,
        first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL, source TEXT NOT NULL,
        PRIMARY KEY (username, device_id)
    ) WITHOUT ROWID",
];
