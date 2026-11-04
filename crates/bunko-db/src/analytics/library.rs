//! The library's shape (design §7): the latest file-type scan, a daily growth
//! snapshot, and the page formats inside archives. The foundation creates the tables;
//! the scanner (bunko-server `analytics::library`) adds its statements here.

pub(crate) const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS library_scan (
        id INTEGER PRIMARY KEY CHECK (id = 1), scanned_at INTEGER NOT NULL, json TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS library_daily (
        day INTEGER PRIMARY KEY, series INTEGER NOT NULL DEFAULT 0,
        volumes INTEGER NOT NULL DEFAULT 0, bytes INTEGER NOT NULL DEFAULT 0, json TEXT
    )",
    // `formats`: JSON `{ext: count}`.
    "CREATE TABLE IF NOT EXISTS archive_pages (
        volume_key TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
        pages INTEGER NOT NULL DEFAULT 0, formats TEXT NOT NULL DEFAULT '{}',
        scanned_at INTEGER NOT NULL
    )",
];
