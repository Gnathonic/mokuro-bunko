//! Reading (design §6): positions, per-day reading, history cursors, the reader's own
//! progress records and devices. The reading worker (bunko-server `analytics::reading`)
//! parses the per-user files the reader uploads and hands the results to the
//! statements here, one write transaction per file; the admin API reads them back.
//!
//! - A *read* is one `(day, username, volume_uuid)` row of `reading_daily`; *readers*
//!   are distinct users; *minutes* are `dwell_ms / 60000`.
//! - `positions.json` sets `opens` and, while no history covers that row, adds the
//!   page and character deltas between two positions ([`AnalyticsTx::ingest_positions`]).
//! - A history segment's new events (`seq` past the cursor) add pages, characters and
//!   dwell; the first page events for a row replace what the positions estimated
//!   there, since the history is the exact log ([`AnalyticsTx::ingest_history`]).
//! - `volume-data.json` replaces the user's `volume_progress` rows
//!   ([`AnalyticsTx::replace_volume_progress`]), which the completion funnel reads.
//!
//! `series_key` is the fold of the library series folder (`fold_series_title_key`), or
//! of the reader's own `series_title` for a volume the library does not know; `''` is a
//! volume of no known series (left out of the per-series tables).

use super::{AnalyticsDb, AnalyticsTx, int, uint};
use crate::Database;
use crate::error::Result;
use rusqlite::{OptionalExtension, params};
use std::collections::{BTreeMap, HashMap, HashSet};

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
    // A user's rows of one volume, whatever the day: a later volume-data.json names the
    // series of reads the positions could not place.
    "CREATE INDEX IF NOT EXISTS idx_reading_daily_user ON reading_daily (username, volume_uuid)",
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

/// `reading_daily.sources` bits.
pub const SOURCE_POSITIONS: i64 = 1;
pub const SOURCE_HISTORY: i64 = 2;

/// A position seen for the first time counts as a read only this close to now: the
/// first upload of `positions.json` carries up to 30 days of older positions.
pub const FIRST_SEEN_WINDOW_MS: i64 = 48 * 3600 * 1000;
/// `read_positions` rows the file no longer lists are dropped once this old: the
/// reader keeps 30 days in the file, and an entry seen again after that is older than
/// [`FIRST_SEEN_WINDOW_MS`], so it never counts twice.
pub const POSITIONS_KEEP_MS: i64 = 35 * 86_400_000;
/// One history page view counts for at most this long (a book left open).
pub const DWELL_CAP_MS: i64 = 15 * 60 * 1000;
/// Devices kept per account (the reader's ids come from files the account writes).
pub const DEVICES_PER_USER: i64 = 64;

const MS_PER_DAY: i64 = 86_400_000;

/// Where a volume's reading is attributed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attribution {
    /// The library archive (`Series/Vol 1.cbz`, on-disk spelling).
    pub volume_key: Option<String>,
    /// `fold_series_title_key` of the series folder; `''` when unknown.
    pub series_key: String,
    pub series_title: Option<String>,
    pub volume_title: Option<String>,
}

/// One `positions.json` entry: `"<volume_uuid>": [t_ms, page, chars, "<device>"]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionEntry {
    pub volume_uuid: String,
    pub t_ms: i64,
    pub page: i64,
    pub chars: i64,
    pub device: String,
}

/// What one `positions.json` did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PositionsOutcome {
    /// Entries that counted as reading (a new `t`).
    pub observations: u64,
    /// Entries seen for the first time and stored as a baseline only.
    pub baselines: u64,
    /// Entries with the `t` already stored (other devices' merged copies).
    pub unchanged: u64,
    /// Pages the deltas added (rows with no history only).
    pub pages: u64,
}

/// What a decoded history event adds (bunko-server's codec maps the reader's kinds).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HistoryKind {
    /// A page view: `pages` = `last - first + 1`, `chars` = `sum(page_chars)`.
    Page {
        pages: i64,
        chars: i64,
        dwell_ms: Option<i64>,
    },
    /// A manual stat edit.
    Adjust {
        time_delta_ms: i64,
        chars_delta: i64,
    },
    /// restart, forget, position, resolve: nothing to add.
    Other,
}

/// One event of a history segment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HistoryEvent<'a> {
    /// The cursor's device: the segment's own device, or `legacy:<volume>` for a
    /// legacy segment (every converting device uploads the same events under it).
    pub cursor_device: &'a str,
    pub seq: i64,
    pub t_ms: i64,
    pub volume_uuid: &'a str,
    pub kind: HistoryKind,
}

/// What one history segment did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HistoryOutcome {
    /// Events past the cursor.
    pub new_events: u64,
    /// Events at or before the cursor (an upload seen before).
    pub seen_events: u64,
    /// `(day, volume)` rows written.
    pub rows: u64,
    /// Adjust-only days with no read to adjust.
    pub adjust_skipped: u64,
}

/// One `volume-data.json` record, as the reader wrote it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProgressRecord {
    pub volume_uuid: String,
    pub series_uuid: Option<String>,
    pub attribution: Attribution,
    /// The page reached.
    pub progress: i64,
    pub chars: i64,
    pub completed: bool,
    /// Epoch seconds.
    pub completed_at: Option<i64>,
    pub time_read_min: f64,
    /// Epoch seconds.
    pub last_progress_update: Option<i64>,
}

/// A device seen in a user's files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSeen<'a> {
    pub device_id: &'a str,
    /// A generated label (from `device.json`): it only fills a device that has none
    /// (the user's own name, [`AnalyticsTx::name_device`], wins).
    pub label: Option<&'a str>,
    /// Epoch seconds.
    pub first_seen: i64,
    pub last_seen: i64,
    /// `positions` or `history`.
    pub source: &'a str,
}

fn day_of_ms(t_ms: i64) -> i64 {
    t_ms.div_euclid(MS_PER_DAY)
}

/// What a `reading_daily` row holds before a write.
struct DayRow {
    sources: i64,
    pages: i64,
    chars: i64,
    dwell_ms: i64,
}

/// One `(day, volume)` of a segment's new events.
#[derive(Default)]
struct DayAgg {
    pages: i64,
    chars: i64,
    dwell_ms: i64,
    page_events: bool,
    adjust_dwell_ms: i64,
    adjust_chars: i64,
}

/// The counters of a `reading_daily` write.
struct DayCounts {
    opens: i64,
    pages: i64,
    chars: i64,
    dwell_ms: i64,
    sources: i64,
}

impl AnalyticsTx<'_> {
    fn day_row(&self, day: i64, username: &str, uuid: &str) -> Result<Option<DayRow>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT sources, pages, chars, dwell_ms FROM reading_daily \
                 WHERE day = ?1 AND username = ?2 AND volume_uuid = ?3",
            )?
            .query_row(params![day, username, uuid], |r| {
                Ok(DayRow {
                    sources: r.get(0)?,
                    pages: r.get(1)?,
                    chars: r.get(2)?,
                    dwell_ms: r.get(3)?,
                })
            })
            .optional()?)
    }

    /// The attribution of `uuid` for `username`: the library's (`known`), else the
    /// series the user's own `volume-data.json` named, else none.
    fn attribution_of(
        &self,
        username: &str,
        uuid: &str,
        known: &HashMap<String, Attribution>,
    ) -> Result<Attribution> {
        if let Some(a) = known.get(uuid) {
            return Ok(a.clone());
        }
        let own = self
            .conn
            .prepare_cached(
                "SELECT series_key, series_title, volume_title FROM volume_progress \
                 WHERE username = ?1 AND volume_uuid = ?2",
            )?
            .query_row(params![username, uuid], |r| {
                Ok(Attribution {
                    volume_key: None,
                    series_key: r.get(0)?,
                    series_title: r.get(1)?,
                    volume_title: r.get(2)?,
                })
            })
            .optional()?;
        Ok(own.unwrap_or_default())
    }

    /// Write a `reading_daily` row: the counters as given (the caller computed them from
    /// the stored row), `opens` kept at its highest, `sources` OR-ed, the attribution
    /// filled where the row had none.
    fn put_day_row(
        &self,
        (day, username, uuid): (i64, &str, &str),
        a: &Attribution,
        n: &DayCounts,
    ) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO reading_daily (day, username, volume_uuid, volume_key, series_key, \
                 series_title, volume_title, opens, pages, chars, dwell_ms, sources) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
                 ON CONFLICT (day, username, volume_uuid) DO UPDATE SET \
                 volume_key = COALESCE(reading_daily.volume_key, excluded.volume_key), \
                 series_title = CASE WHEN reading_daily.series_key = '' \
                   THEN excluded.series_title ELSE reading_daily.series_title END, \
                 series_key = CASE WHEN reading_daily.series_key = '' \
                   THEN excluded.series_key ELSE reading_daily.series_key END, \
                 volume_title = COALESCE(reading_daily.volume_title, excluded.volume_title), \
                 opens = MAX(reading_daily.opens, excluded.opens), \
                 pages = excluded.pages, chars = excluded.chars, dwell_ms = excluded.dwell_ms, \
                 sources = reading_daily.sources | excluded.sources",
            )?
            .execute(params![
                day,
                username,
                uuid,
                a.volume_key,
                a.series_key,
                a.series_title,
                a.volume_title,
                n.opens,
                n.pages,
                n.chars,
                n.dwell_ms,
                n.sources
            ])?;
        Ok(())
    }

    /// Diff one `positions.json` against `read_positions` (design §6): a new `t` is a
    /// read on `day(t)` (`opens` = 1; the page and character deltas while no history
    /// covers that row); a first-seen entry is a baseline, and a read only within
    /// [`FIRST_SEEN_WINDOW_MS`] of `now_ms`; an unchanged `t` is another device's merged
    /// copy. A page-0 position is a reset (un-read, restart), never a read. Rows the
    /// file dropped are forgotten once [`POSITIONS_KEEP_MS`] old.
    pub fn ingest_positions(
        &self,
        username: &str,
        entries: &[PositionEntry],
        known: &HashMap<String, Attribution>,
        now_ms: i64,
    ) -> Result<PositionsOutcome> {
        let mut stored: HashMap<String, (i64, i64, i64)> = HashMap::new();
        {
            let mut stmt = self.conn.prepare_cached(
                "SELECT volume_uuid, t_ms, page, chars FROM read_positions WHERE username = ?1",
            )?;
            let rows = stmt.query_map([username], |r| {
                Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?)))
            })?;
            for row in rows {
                let (uuid, v) = row?;
                stored.insert(uuid, v);
            }
        }
        let mut out = PositionsOutcome::default();
        let mut listed: HashSet<&str> = HashSet::with_capacity(entries.len());
        for e in entries {
            listed.insert(e.volume_uuid.as_str());
            let delta = match stored.get(&e.volume_uuid) {
                Some(&(t, page, chars)) => {
                    if e.t_ms <= t {
                        out.unchanged += 1;
                        continue;
                    }
                    Some(((e.page - page).max(0), (e.chars - chars).max(0)))
                }
                None => {
                    out.baselines += 1;
                    (now_ms - e.t_ms <= FIRST_SEEN_WINDOW_MS).then_some((0, 0))
                }
            };
            self.conn
                .prepare_cached(
                    "INSERT INTO read_positions (username, volume_uuid, t_ms, page, chars, device) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                     ON CONFLICT (username, volume_uuid) DO UPDATE SET t_ms = excluded.t_ms, \
                     page = excluded.page, chars = excluded.chars, device = excluded.device",
                )?
                .execute(params![
                    username,
                    e.volume_uuid,
                    e.t_ms,
                    e.page,
                    e.chars,
                    e.device
                ])?;
            let Some((pages, chars)) = delta else {
                continue;
            };
            if e.page <= 0 {
                continue;
            }
            let day = day_of_ms(e.t_ms);
            let a = self.attribution_of(username, &e.volume_uuid, known)?;
            let row = self.day_row(day, username, &e.volume_uuid)?;
            let (old_pages, old_chars, old_dwell, history) = row.map_or((0, 0, 0, false), |r| {
                (
                    r.pages,
                    r.chars,
                    r.dwell_ms,
                    r.sources & SOURCE_HISTORY != 0,
                )
            });
            let (add_pages, add_chars) = if history { (0, 0) } else { (pages, chars) };
            self.put_day_row(
                (day, username, &e.volume_uuid),
                &a,
                &DayCounts {
                    opens: 1,
                    pages: old_pages.saturating_add(add_pages),
                    chars: old_chars.saturating_add(add_chars),
                    dwell_ms: old_dwell,
                    sources: SOURCE_POSITIONS,
                },
            )?;
            out.observations += 1;
            out.pages += uint(add_pages);
        }
        let cutoff = now_ms - POSITIONS_KEEP_MS;
        for (uuid, (t, _, _)) in &stored {
            if *t < cutoff && !listed.contains(uuid.as_str()) {
                self.conn
                    .prepare_cached(
                        "DELETE FROM read_positions WHERE username = ?1 AND volume_uuid = ?2",
                    )?
                    .execute(params![username, uuid])?;
            }
        }
        Ok(out)
    }

    /// Add one segment's events past each cursor (`history_cursor` per (user, device,
    /// segment)), per `(day(t), volume)`. A day's first page events replace what the
    /// positions estimated for that row (the history is the exact log); adjust deltas
    /// apply to a row that holds a read (clamped at zero), never make one.
    pub fn ingest_history(
        &self,
        username: &str,
        segment: &str,
        events: &[HistoryEvent<'_>],
        known: &HashMap<String, Attribution>,
    ) -> Result<HistoryOutcome> {
        let mut out = HistoryOutcome::default();
        // device → (stored last_seq, highest seq ingested now)
        let mut cursors: HashMap<&str, (i64, i64)> = HashMap::new();
        let mut days: BTreeMap<(i64, &str), DayAgg> = BTreeMap::new();
        for e in events {
            let cursor = match cursors.get_mut(e.cursor_device) {
                Some(c) => c,
                None => {
                    let last: i64 = self
                        .conn
                        .prepare_cached(
                            "SELECT last_seq FROM history_cursor \
                             WHERE username = ?1 AND device = ?2 AND segment = ?3",
                        )?
                        .query_row(params![username, e.cursor_device, segment], |r| r.get(0))
                        .optional()?
                        .unwrap_or(0);
                    cursors.entry(e.cursor_device).or_insert((last, last))
                }
            };
            if e.seq <= cursor.0 {
                out.seen_events += 1;
                continue;
            }
            cursor.1 = cursor.1.max(e.seq);
            out.new_events += 1;
            let agg = days.entry((day_of_ms(e.t_ms), e.volume_uuid)).or_default();
            match e.kind {
                HistoryKind::Page {
                    pages,
                    chars,
                    dwell_ms,
                } => {
                    agg.page_events = true;
                    agg.pages = agg.pages.saturating_add(pages.max(0));
                    agg.chars = agg.chars.saturating_add(chars.max(0));
                    if let Some(d) = dwell_ms.filter(|d| *d >= 0) {
                        agg.dwell_ms = agg.dwell_ms.saturating_add(d.min(DWELL_CAP_MS));
                    }
                }
                HistoryKind::Adjust {
                    time_delta_ms,
                    chars_delta,
                } => {
                    agg.adjust_dwell_ms = agg.adjust_dwell_ms.saturating_add(time_delta_ms);
                    agg.adjust_chars = agg.adjust_chars.saturating_add(chars_delta);
                }
                HistoryKind::Other => {}
            }
        }
        for ((day, uuid), agg) in &days {
            if !agg.page_events && agg.adjust_dwell_ms == 0 && agg.adjust_chars == 0 {
                continue;
            }
            let row = self.day_row(*day, username, uuid)?;
            let Some(base) = row.or_else(|| {
                agg.page_events.then_some(DayRow {
                    sources: 0,
                    pages: 0,
                    chars: 0,
                    dwell_ms: 0,
                })
            }) else {
                out.adjust_skipped += 1;
                continue;
            };
            let history = base.sources & SOURCE_HISTORY != 0;
            // Positions-only counts give way to the history's first page events.
            let (pages, chars) = if agg.page_events && !history {
                (0, 0)
            } else {
                (base.pages, base.chars)
            };
            let a = self.attribution_of(username, uuid, known)?;
            self.put_day_row(
                (*day, username, uuid),
                &a,
                &DayCounts {
                    opens: 0,
                    pages: pages.saturating_add(agg.pages),
                    chars: chars
                        .saturating_add(agg.chars)
                        .saturating_add(agg.adjust_chars)
                        .max(0),
                    dwell_ms: base
                        .dwell_ms
                        .saturating_add(agg.dwell_ms)
                        .saturating_add(agg.adjust_dwell_ms)
                        .max(0),
                    sources: if agg.page_events { SOURCE_HISTORY } else { 0 },
                },
            )?;
            out.rows += 1;
        }
        for (device, (last, new)) in &cursors {
            if new > last {
                self.conn
                    .prepare_cached(
                        "INSERT INTO history_cursor (username, device, segment, last_seq) \
                         VALUES (?1, ?2, ?3, ?4) ON CONFLICT (username, device, segment) \
                         DO UPDATE SET last_seq = MAX(last_seq, excluded.last_seq)",
                    )?
                    .execute(params![username, device, segment, new])?;
            }
        }
        Ok(out)
    }

    /// Replace `username`'s `volume_progress` with `records` (records absent from the
    /// file, or tombstoned by the reader, are the caller's to leave out). Reads that
    /// had no series yet take the one a record names. Returns the rows written.
    pub fn replace_volume_progress(
        &self,
        username: &str,
        records: &[ProgressRecord],
        now_s: i64,
    ) -> Result<u64> {
        let existing: HashSet<String> = {
            let mut stmt = self
                .conn
                .prepare_cached("SELECT volume_uuid FROM volume_progress WHERE username = ?1")?;
            stmt.query_map([username], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut keep: HashSet<&str> = HashSet::with_capacity(records.len());
        let mut written = 0u64;
        for p in records {
            keep.insert(p.volume_uuid.as_str());
            let a = &p.attribution;
            written += self
                .conn
                .prepare_cached(
                    "INSERT INTO volume_progress (username, volume_uuid, series_uuid, series_key, \
                     series_title, volume_title, progress, chars, completed, completed_at, \
                     time_read_min, last_progress_update, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
                     ON CONFLICT (username, volume_uuid) DO UPDATE SET \
                     series_uuid = excluded.series_uuid, series_key = excluded.series_key, \
                     series_title = excluded.series_title, volume_title = excluded.volume_title, \
                     progress = excluded.progress, chars = excluded.chars, \
                     completed = excluded.completed, completed_at = excluded.completed_at, \
                     time_read_min = excluded.time_read_min, \
                     last_progress_update = excluded.last_progress_update, \
                     updated_at = excluded.updated_at \
                     WHERE series_uuid IS NOT excluded.series_uuid \
                     OR series_key IS NOT excluded.series_key \
                     OR series_title IS NOT excluded.series_title \
                     OR volume_title IS NOT excluded.volume_title \
                     OR progress IS NOT excluded.progress OR chars IS NOT excluded.chars \
                     OR completed IS NOT excluded.completed \
                     OR completed_at IS NOT excluded.completed_at \
                     OR time_read_min IS NOT excluded.time_read_min \
                     OR last_progress_update IS NOT excluded.last_progress_update",
                )?
                .execute(params![
                    username,
                    p.volume_uuid,
                    p.series_uuid,
                    a.series_key,
                    a.series_title,
                    a.volume_title,
                    p.progress,
                    p.chars,
                    p.completed,
                    p.completed_at,
                    p.time_read_min,
                    p.last_progress_update,
                    now_s
                ])? as u64;
            if !a.series_key.is_empty() {
                self.conn
                    .prepare_cached(
                        "UPDATE reading_daily SET series_key = ?3, series_title = ?4, \
                         volume_key = COALESCE(volume_key, ?5), \
                         volume_title = COALESCE(volume_title, ?6) \
                         WHERE username = ?1 AND volume_uuid = ?2 AND series_key = ''",
                    )?
                    .execute(params![
                        username,
                        p.volume_uuid,
                        a.series_key,
                        a.series_title,
                        a.volume_key,
                        a.volume_title
                    ])?;
            }
        }
        for uuid in &existing {
            if !keep.contains(uuid.as_str()) {
                self.conn
                    .prepare_cached(
                        "DELETE FROM volume_progress WHERE username = ?1 AND volume_uuid = ?2",
                    )?
                    .execute(params![username, uuid])?;
                written += 1;
            }
        }
        Ok(written)
    }

    /// The user's `volume-data.json` is gone: so is their progress.
    pub fn clear_volume_progress(&self, username: &str) -> Result<u64> {
        Ok(self
            .conn
            .prepare_cached("DELETE FROM volume_progress WHERE username = ?1")?
            .execute([username])? as u64)
    }

    /// Widen a device's `[first_seen, last_seen]` and fill its label if it has none. A
    /// user past [`DEVICES_PER_USER`] gets no new devices; `false` when this one was
    /// left out.
    pub fn see_device(&self, username: &str, d: &DeviceSeen<'_>) -> Result<bool> {
        let known: bool = self
            .conn
            .prepare_cached("SELECT 1 FROM devices WHERE username = ?1 AND device_id = ?2")?
            .query_row(params![username, d.device_id], |_| Ok(true))
            .optional()?
            .unwrap_or(false);
        if !known {
            let n: i64 = self
                .conn
                .prepare_cached("SELECT COUNT(*) FROM devices WHERE username = ?1")?
                .query_row([username], |r| r.get(0))?;
            if n >= DEVICES_PER_USER {
                return Ok(false);
            }
        }
        self.conn
            .prepare_cached(
                "INSERT INTO devices (username, device_id, label, first_seen, last_seen, source) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (username, device_id) DO UPDATE SET \
                 label = COALESCE(devices.label, excluded.label), \
                 first_seen = MIN(devices.first_seen, excluded.first_seen), \
                 last_seen = MAX(devices.last_seen, excluded.last_seen)",
            )?
            .execute(params![
                username,
                d.device_id,
                d.label,
                d.first_seen,
                d.last_seen,
                d.source
            ])?;
        Ok(true)
    }

    /// The user's own name for a device they have (`volume-data.json`'s
    /// `tracking.devices`): it replaces the generated label. A device not seen yet is
    /// left alone (nothing says when it was).
    pub fn name_device(&self, username: &str, device_id: &str, name: &str) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached(
                "UPDATE devices SET label = ?3 WHERE username = ?1 AND device_id = ?2 \
                 AND label IS NOT ?3",
            )?
            .execute(params![username, device_id, name])?
            > 0)
    }
}

/// Reading per day.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadingDay {
    pub day: i64,
    pub readers: u64,
    pub reads: u64,
    pub opens: u64,
    pub pages: u64,
    pub chars: u64,
    pub dwell_ms: u64,
}

/// One reader over a span of days.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReaderTotals {
    pub username: String,
    pub reads: u64,
    pub days: u64,
    pub volumes: u64,
    pub pages: u64,
    pub chars: u64,
    pub dwell_ms: u64,
}

/// One volume over a span of days.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VolumeReading {
    pub volume_uuid: String,
    pub volume_key: Option<String>,
    pub volume_title: Option<String>,
    pub series_key: String,
    pub series_title: Option<String>,
    pub readers: u64,
    pub reads: u64,
    pub pages: u64,
    pub dwell_ms: u64,
}

/// An account's traffic over a span of days (`principal_daily`, `u:` rows).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountTraffic {
    pub username: String,
    pub requests: u64,
    pub bytes_out: u64,
    pub bytes_in: u64,
    pub days_active: u64,
    /// Epoch seconds (`principal_seen`).
    pub last_seen: Option<i64>,
}

/// A device row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceRow {
    pub device_id: String,
    pub label: Option<String>,
    pub first_seen: i64,
    pub last_seen: i64,
    pub source: String,
}

/// One `volume_progress` row, as the funnel reads it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProgressRow {
    pub username: String,
    pub volume_uuid: String,
    pub series_key: String,
    pub series_title: Option<String>,
    pub volume_title: Option<String>,
    pub progress: i64,
    pub completed: bool,
}

/// An account's `volume_progress` counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProgressCounts {
    pub started: u64,
    pub completed: u64,
}

impl AnalyticsDb {
    /// Reading per day of `from..=to` (days with any).
    pub fn reading_days(&self, from_day: i64, to_day: i64) -> Result<Vec<ReadingDay>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT day, COUNT(DISTINCT username), COUNT(*), SUM(opens), SUM(pages), \
                 SUM(chars), SUM(dwell_ms) FROM reading_daily WHERE day >= ?1 AND day <= ?2 \
                 GROUP BY day ORDER BY day",
            )?;
            let rows = stmt
                .query_map(params![from_day, to_day], |r| {
                    Ok(ReadingDay {
                        day: r.get(0)?,
                        readers: uint(r.get(1)?),
                        reads: uint(r.get(2)?),
                        opens: uint(r.get(3)?),
                        pages: uint(r.get(4)?),
                        chars: uint(r.get(5)?),
                        dwell_ms: uint(r.get(6)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Readers of `from..=to`, most time first (then pages), at most `limit`.
    pub fn top_readers(
        &self,
        from_day: i64,
        to_day: i64,
        limit: usize,
    ) -> Result<Vec<ReaderTotals>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT username, COUNT(*), COUNT(DISTINCT day), COUNT(DISTINCT volume_uuid), \
                 SUM(pages) AS p, SUM(chars), SUM(dwell_ms) AS d FROM reading_daily \
                 WHERE day >= ?1 AND day <= ?2 GROUP BY username \
                 ORDER BY d DESC, p DESC, username ASC LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(params![from_day, to_day, int(limit as u64)], |r| {
                    Ok(ReaderTotals {
                        username: r.get(0)?,
                        reads: uint(r.get(1)?),
                        days: uint(r.get(2)?),
                        volumes: uint(r.get(3)?),
                        pages: uint(r.get(4)?),
                        chars: uint(r.get(5)?),
                        dwell_ms: uint(r.get(6)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Volumes read in `from..=to`, most readers first (then time), at most `limit`.
    pub fn top_volumes(
        &self,
        from_day: i64,
        to_day: i64,
        limit: usize,
    ) -> Result<Vec<VolumeReading>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT volume_uuid, MAX(volume_key), MAX(volume_title), MAX(series_key), \
                 MAX(series_title), COUNT(DISTINCT username) AS r, COUNT(*), SUM(pages), \
                 SUM(dwell_ms) AS d FROM reading_daily WHERE day >= ?1 AND day <= ?2 \
                 GROUP BY volume_uuid ORDER BY r DESC, d DESC, volume_uuid ASC LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(params![from_day, to_day, int(limit as u64)], |r| {
                    Ok(VolumeReading {
                        volume_uuid: r.get(0)?,
                        volume_key: r.get(1)?,
                        volume_title: r.get(2)?,
                        series_key: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        series_title: r.get(4)?,
                        readers: uint(r.get(5)?),
                        reads: uint(r.get(6)?),
                        pages: uint(r.get(7)?),
                        dwell_ms: uint(r.get(8)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Reads of `from..=to` by where they came from: `(positions only, with history)`.
    pub fn reading_sources(&self, from_day: i64, to_day: i64) -> Result<(u64, u64)> {
        self.read(|c| {
            Ok(c.prepare_cached(
                "SELECT COALESCE(SUM(CASE WHEN sources & 2 = 0 AND sources & 1 != 0 THEN 1 ELSE 0 END), 0), \
                 COALESCE(SUM(CASE WHEN sources & 2 != 0 THEN 1 ELSE 0 END), 0) \
                 FROM reading_daily WHERE day >= ?1 AND day <= ?2",
            )?
            .query_row(params![from_day, to_day], |r| {
                Ok((uint(r.get(0)?), uint(r.get(1)?)))
            })?)
        })
    }

    /// Accounts (`u:` principals) active in `from..=to`, most bytes first.
    pub fn account_traffic(
        &self,
        from_day: i64,
        to_day: i64,
        limit: usize,
    ) -> Result<Vec<AccountTraffic>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT d.principal, SUM(d.requests), SUM(d.bytes_out) AS b, SUM(d.bytes_in), \
                 COUNT(DISTINCT d.day), s.last_seen FROM principal_daily d \
                 LEFT JOIN principal_seen s ON s.principal = d.principal \
                 WHERE d.day >= ?1 AND d.day <= ?2 AND substr(d.principal, 1, 2) = 'u:' \
                 GROUP BY d.principal ORDER BY b DESC, d.principal ASC LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(params![from_day, to_day, int(limit as u64)], |r| {
                    let principal: String = r.get(0)?;
                    Ok(AccountTraffic {
                        username: principal.get(2..).unwrap_or_default().to_string(),
                        requests: uint(r.get(1)?),
                        bytes_out: uint(r.get(2)?),
                        bytes_in: uint(r.get(3)?),
                        days_active: uint(r.get(4)?),
                        last_seen: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Distinct accounts active per day of `from..=to` (days with any).
    pub fn accounts_per_day(&self, from_day: i64, to_day: i64) -> Result<Vec<(i64, u64)>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT day, COUNT(DISTINCT principal) FROM principal_daily \
                 WHERE day >= ?1 AND day <= ?2 AND substr(principal, 1, 2) = 'u:' \
                 GROUP BY day ORDER BY day",
            )?;
            let rows = stmt
                .query_map(params![from_day, to_day], |r| {
                    Ok((r.get(0)?, uint(r.get(1)?)))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Distinct accounts active in `from..=to`.
    pub fn active_accounts(&self, from_day: i64, to_day: i64) -> Result<u64> {
        self.read(|c| {
            Ok(uint(
                c.prepare_cached(
                    "SELECT COUNT(DISTINCT principal) FROM principal_daily \
                     WHERE day >= ?1 AND day <= ?2 AND substr(principal, 1, 2) = 'u:'",
                )?
                .query_row(params![from_day, to_day], |r| r.get(0))?,
            ))
        })
    }

    /// A user's devices, most recently seen first.
    pub fn devices_of(&self, username: &str) -> Result<Vec<DeviceRow>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT device_id, label, first_seen, last_seen, source FROM devices \
                 WHERE username = ?1 ORDER BY last_seen DESC, device_id ASC",
            )?;
            let rows = stmt
                .query_map([username], |r| {
                    Ok(DeviceRow {
                        device_id: r.get(0)?,
                        label: r.get(1)?,
                        first_seen: r.get(2)?,
                        last_seen: r.get(3)?,
                        source: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// The client apps `principal` used in `from..=to` (`client_principals`, kept for
    /// the IP retention only), most days first.
    pub fn apps_of(&self, principal: &str, from_day: i64, to_day: i64) -> Result<Vec<String>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT app, COUNT(*) AS n FROM client_principals \
                 WHERE principal = ?1 AND day >= ?2 AND day <= ?3 \
                 GROUP BY app ORDER BY n DESC, app ASC",
            )?;
            let rows = stmt
                .query_map(params![principal, from_day, to_day], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(rows)
        })
    }

    /// A user's volumes started (progress or completed) and completed.
    pub fn progress_counts(&self, username: &str) -> Result<ProgressCounts> {
        self.read(|c| {
            Ok(c.prepare_cached(
                "SELECT COALESCE(SUM(progress > 0 OR completed != 0), 0), \
                 COALESCE(SUM(completed != 0), 0) FROM volume_progress WHERE username = ?1",
            )?
            .query_row([username], |r| {
                Ok(ProgressCounts {
                    started: uint(r.get(0)?),
                    completed: uint(r.get(1)?),
                })
            })?)
        })
    }

    /// `volume_progress` rows of one series (`None`: every row, of no known series too).
    pub fn progress_rows(&self, series_key: Option<&str>) -> Result<Vec<ProgressRow>> {
        self.read(|c| {
            let map = |r: &rusqlite::Row<'_>| {
                Ok(ProgressRow {
                    username: r.get(0)?,
                    volume_uuid: r.get(1)?,
                    series_key: r.get(2)?,
                    series_title: r.get(3)?,
                    volume_title: r.get(4)?,
                    progress: r.get(5)?,
                    completed: r.get::<_, i64>(6)? != 0,
                })
            };
            let rows = match series_key {
                Some(key) => c
                    .prepare_cached(
                        "SELECT username, volume_uuid, series_key, series_title, volume_title, \
                         progress, completed FROM volume_progress WHERE series_key = ?1",
                    )?
                    .query_map([key], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                None => c
                    .prepare_cached(
                        "SELECT username, volume_uuid, series_key, series_title, volume_title, \
                         progress, completed FROM volume_progress",
                    )?
                    .query_map([], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            };
            Ok(rows)
        })
    }

    /// Distinct downloaders per volume of one series, all time (`volume_downloads`,
    /// completed downloads).
    pub fn downloaders_by_volume(&self, series_key: &str) -> Result<Vec<(String, u64)>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT volume_key, COUNT(DISTINCT principal) FROM volume_downloads \
                 WHERE series_key = ?1 AND completed GROUP BY volume_key",
            )?;
            let rows = stmt
                .query_map([series_key], |r| Ok((r.get(0)?, uint(r.get(1)?))))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Distinct downloaders per series, all time (completed downloads):
    /// `(series_key, title, downloaders)`.
    pub fn downloaders_by_series(&self) -> Result<Vec<(String, String, u64)>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT series_key, MAX(COALESCE(series_title, '')), COUNT(DISTINCT principal) \
                 FROM volume_downloads WHERE series_key != '' AND completed GROUP BY series_key",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, uint(r.get(2)?))))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// The most recent spelling seen for a series key (downloads, reading, progress).
    pub fn series_title_of(&self, series_key: &str) -> Result<Option<String>> {
        self.read(|c| {
            for sql in [
                "SELECT series_title FROM volume_downloads WHERE series_key = ?1 \
                 AND series_title IS NOT NULL ORDER BY day DESC LIMIT 1",
                "SELECT series_title FROM reading_daily WHERE series_key = ?1 \
                 AND series_title IS NOT NULL ORDER BY day DESC LIMIT 1",
                "SELECT series_title FROM volume_progress WHERE series_key = ?1 \
                 AND series_title IS NOT NULL LIMIT 1",
            ] {
                let title: Option<String> = c
                    .prepare_cached(sql)?
                    .query_row([series_key], |r| r.get(0))
                    .optional()?;
                if title.is_some() {
                    return Ok(title);
                }
            }
            Ok(None)
        })
    }
}

/// The main database's side of the uuid → library volume lookup (design §6).
impl Database {
    /// Every library archive's reader volume id, `(volume_key, volume_uuid)`, at most
    /// `limit`: the primary sidecars' ids (`volume_identities`), then the ids the
    /// compiled series entries publish (`series_entry_cache`; a volume without a sidecar
    /// has the reader's deterministic id there).
    pub fn analytics_volume_uuids(&self, limit: usize) -> Result<Vec<(String, String)>> {
        let limit = int(limit as u64);
        self.read(|c| {
            let mut out: Vec<(String, String)> = c
                .prepare_cached(
                    "SELECT volume_key, volume_uuid FROM volume_identities \
                     ORDER BY volume_key LIMIT ?1",
                )?
                .query_map([limit], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let room = (limit - out.len() as i64).max(0);
            let compiled = c
                .prepare_cached(
                    "SELECT e.volume_key, json_extract(e.entry_json, '$.volume_uuid') \
                     FROM series_entry_cache e WHERE json_valid(e.entry_json) \
                     AND json_type(e.entry_json, '$.volume_uuid') = 'text' \
                     AND NOT EXISTS (SELECT 1 FROM volume_identities i \
                       WHERE i.volume_key = e.volume_key) \
                     ORDER BY e.volume_key LIMIT ?1",
                )?
                .query_map([room], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
            out.extend(compiled);
            Ok(out)
        })
    }

    /// The archive whose primary sidecar names `volume_uuid` (the first by key when
    /// copies share it).
    pub fn analytics_volume_key_of_uuid(&self, volume_uuid: &str) -> Result<Option<String>> {
        self.read(|c| {
            Ok(c.prepare_cached(
                "SELECT volume_key FROM volume_identities WHERE volume_uuid = ?1 \
                 ORDER BY volume_key LIMIT 1",
            )?
            .query_row([volume_uuid], |r| r.get(0))
            .optional()?)
        })
    }

    /// Page counts of the compiled entries of `volume_keys` (`None`: every entry).
    pub fn analytics_page_counts(
        &self,
        volume_keys: Option<&[String]>,
    ) -> Result<HashMap<String, i64>> {
        self.read(|c| {
            let mut out = HashMap::new();
            match volume_keys {
                Some(keys) => {
                    let mut stmt = c.prepare_cached(
                        "SELECT json_extract(entry_json, '$.page_count') FROM series_entry_cache \
                         WHERE volume_key = ?1 AND json_valid(entry_json) \
                         AND json_type(entry_json, '$.page_count') = 'integer'",
                    )?;
                    for k in keys {
                        if let Some(n) = stmt.query_row([k], |r| r.get::<_, i64>(0)).optional()? {
                            out.insert(k.clone(), n);
                        }
                    }
                }
                None => {
                    let mut stmt = c.prepare_cached(
                        "SELECT volume_key, json_extract(entry_json, '$.page_count') \
                         FROM series_entry_cache WHERE json_valid(entry_json) \
                         AND json_type(entry_json, '$.page_count') = 'integer'",
                    )?;
                    let rows =
                        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                    for row in rows {
                        let (k, n) = row?;
                        out.insert(k, n);
                    }
                }
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::testutil::temp_db;
    use crate::analytics::traffic::PrincipalCounts;

    const NOW: i64 = 1_800_000_000_000;
    const DAY: i64 = MS_PER_DAY;

    fn pos(uuid: &str, t: i64, page: i64, chars: i64, device: &str) -> PositionEntry {
        PositionEntry {
            volume_uuid: uuid.into(),
            t_ms: t,
            page,
            chars,
            device: device.into(),
        }
    }

    fn known(uuid: &str, series: &str) -> HashMap<String, Attribution> {
        HashMap::from([(
            uuid.to_string(),
            Attribution {
                volume_key: Some(format!("{series}/v1.cbz")),
                series_key: crate::fold_series_title_key(series),
                series_title: Some(series.to_string()),
                volume_title: Some("v1".into()),
            },
        )])
    }

    /// `(day, user, uuid, opens, pages, chars, dwell, sources, series_key)`.
    type Row = (i64, String, String, i64, i64, i64, i64, i64, String);

    fn rows(db: &AnalyticsDb) -> Vec<Row> {
        db.read(|c| {
            let mut s = c.prepare(
                "SELECT day, username, volume_uuid, opens, pages, chars, dwell_ms, sources, \
                 series_key FROM reading_daily ORDER BY day, username, volume_uuid",
            )?;
            let r = s
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(r)
        })
        .unwrap()
    }

    #[test]
    fn positions_count_new_times_and_recent_first_sightings_only() {
        let (_d, db) = temp_db();
        let k = known("u1", "Kingdom");
        // First upload: one recent entry (a read), one 10 days old (a baseline only).
        let first = [
            pos("u1", NOW - 3_600_000, 10, 1000, "dev-a"),
            pos("u2", NOW - 10 * DAY, 50, 9000, "dev-b"),
        ];
        let o = db
            .write(|tx| tx.ingest_positions("alice", &first, &k, NOW))
            .unwrap();
        assert_eq!((o.observations, o.baselines, o.pages), (1, 2, 0));
        let r = rows(&db);
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].3, r[0].4, r[0].7), (1, 0, SOURCE_POSITIONS));
        assert_eq!(r[0].8, "kingdom");
        // The same file again (another device's PUT of the merged copy): nothing new.
        let o = db
            .write(|tx| tx.ingest_positions("alice", &first, &k, NOW))
            .unwrap();
        assert_eq!((o.observations, o.unchanged), (0, 2));
        // A new t on u1: page and character deltas; opens stays 1 on the same day.
        let next = [
            pos("u1", NOW - 1_800_000, 25, 2500, "dev-a"),
            pos("u2", NOW - 10 * DAY, 50, 9000, "dev-b"),
        ];
        let o = db
            .write(|tx| tx.ingest_positions("alice", &next, &k, NOW))
            .unwrap();
        assert_eq!((o.observations, o.pages), (1, 15));
        let r = rows(&db);
        assert_eq!((r[0].3, r[0].4, r[0].5), (1, 15, 1500));
        // Going back counts no pages (but is a read); page 0 is a reset, never a read.
        let back = [
            pos("u1", NOW - 60_000, 5, 100, "dev-a"),
            pos("u3", NOW, 0, 0, "dev-a"),
        ];
        let o = db
            .write(|tx| tx.ingest_positions("alice", &back, &k, NOW))
            .unwrap();
        assert_eq!((o.observations, o.pages), (1, 0));
        assert_eq!(rows(&db).len(), 1);
        // Rows the file dropped go once old: u2 (40 days by then), not u1 and u3 (30).
        let later = NOW + 30 * DAY;
        db.write(|tx| tx.ingest_positions("alice", &[], &k, later))
            .unwrap();
        let left: i64 = db
            .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM read_positions", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(left, 2);
        db.write(|tx| tx.ingest_positions("alice", &[], &k, NOW + 40 * DAY))
            .unwrap();
        let left: i64 = db
            .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM read_positions", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(left, 0);
    }

    fn page(seq: i64, t: i64, pages: i64, chars: i64, dwell: Option<i64>) -> HistoryEvent<'static> {
        HistoryEvent {
            cursor_device: "dev-a",
            seq,
            t_ms: t,
            volume_uuid: "u1",
            kind: HistoryKind::Page {
                pages,
                chars,
                dwell_ms: dwell,
            },
        }
    }

    fn adjust(
        seq: i64,
        t: i64,
        uuid: &'static str,
        dwell: i64,
        chars: i64,
    ) -> HistoryEvent<'static> {
        HistoryEvent {
            cursor_device: "dev-a",
            seq,
            t_ms: t,
            volume_uuid: uuid,
            kind: HistoryKind::Adjust {
                time_delta_ms: dwell,
                chars_delta: chars,
            },
        }
    }

    #[test]
    fn history_counts_new_seqs_once_and_replaces_position_estimates() {
        let (_d, db) = temp_db();
        let k = known("u1", "Kingdom");
        let t = NOW - 3_600_000;
        db.write(|tx| tx.ingest_positions("alice", &[pos("u1", t - 60_000, 3, 0, "d")], &k, NOW))
            .unwrap();
        db.write(|tx| tx.ingest_positions("alice", &[pos("u1", t, 9, 900, "d")], &k, NOW))
            .unwrap();
        assert_eq!(rows(&db)[0].4, 6, "the positions' estimate: 6 pages");
        let events = [
            page(1, t, 2, 120, Some(41_000)),
            page(2, t + 41_000, 1, 88, None),
            page(3, t + 60_000, 1, 50, Some(3_600_000)),
            adjust(4, t + 70_000, "u1", -60_000, -10),
            adjust(5, t + 80_000, "u2", 60_000, 0),
        ];
        let o = db
            .write(|tx| tx.ingest_history("alice", "2027-01", &events, &k))
            .unwrap();
        assert_eq!((o.new_events, o.rows, o.adjust_skipped), (5, 1, 1));
        let r = rows(&db);
        assert_eq!(r.len(), 1, "an adjust alone makes no read");
        // 4 pages, 258 - 10 chars, 41 s + the 15 min cap - 60 s.
        assert_eq!(
            (r[0].4, r[0].5, r[0].6),
            (4, 248, 41_000 + DWELL_CAP_MS - 60_000)
        );
        assert_eq!(r[0].7, SOURCE_POSITIONS | SOURCE_HISTORY);
        assert_eq!(r[0].3, 1, "opens kept");
        // The same segment again (the reader re-uploads the month): nothing added.
        let o = db
            .write(|tx| tx.ingest_history("alice", "2027-01", &events, &k))
            .unwrap();
        assert_eq!((o.new_events, o.seen_events), (0, 5));
        assert_eq!(rows(&db)[0].4, 4);
        // Positions after the history: no more page estimates for that row.
        db.write(|tx| {
            tx.ingest_positions("alice", &[pos("u1", t + 100_000, 30, 5000, "d")], &k, NOW)
        })
        .unwrap();
        assert_eq!(rows(&db)[0].4, 4);
        // More events in the same month: only the new ones.
        let more = [
            page(4, t + 200_000, 3, 30, Some(1000)),
            page(6, t + 300_000, 1, 0, Some(-1)),
        ];
        let o = db
            .write(|tx| tx.ingest_history("alice", "2027-01", &more, &k))
            .unwrap();
        assert_eq!((o.new_events, o.seen_events), (1, 1));
        assert_eq!(rows(&db)[0].4, 5);
        // Another month's segment has its own cursor.
        let o = db
            .write(|tx| tx.ingest_history("alice", "2027-02", &more, &k))
            .unwrap();
        assert_eq!(o.new_events, 2);
        // Clamped: a big negative adjust never makes totals negative.
        let neg = [adjust(9, t, "u1", -100 * 60_000, -1_000_000)];
        db.write(|tx| tx.ingest_history("alice", "2027-01", &neg, &k))
            .unwrap();
        let r = rows(&db);
        assert_eq!((r[0].5, r[0].6), (0, 0));
    }

    #[test]
    fn volume_progress_replaces_and_names_unplaced_reads() {
        let (_d, db) = temp_db();
        let none = HashMap::new();
        db.write(|tx| tx.ingest_positions("bob", &[pos("u9", NOW, 4, 0, "d")], &none, NOW))
            .unwrap();
        assert_eq!(rows(&db)[0].8, "", "no series known yet");
        let rec = |uuid: &str, series: &str, progress, completed| ProgressRecord {
            volume_uuid: uuid.into(),
            series_uuid: None,
            attribution: Attribution {
                volume_key: None,
                series_key: crate::fold_series_title_key(series),
                series_title: Some(series.into()),
                volume_title: Some(format!("{series} 1")),
            },
            progress,
            chars: 0,
            completed,
            completed_at: None,
            time_read_min: 0.0,
            last_progress_update: None,
        };
        let n = db
            .write(|tx| {
                tx.replace_volume_progress(
                    "bob",
                    &[
                        rec("u9", "Mushishi", 4, false),
                        rec("u8", "Mushishi", 200, true),
                    ],
                    1,
                )
            })
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(rows(&db)[0].8, "mushishi");
        assert_eq!(
            db.progress_counts("bob").unwrap(),
            ProgressCounts {
                started: 2,
                completed: 1
            }
        );
        // Unchanged rows are not rewritten; a vanished one is deleted.
        let n = db
            .write(|tx| tx.replace_volume_progress("bob", &[rec("u9", "Mushishi", 4, false)], 2))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(db.progress_rows(Some("mushishi")).unwrap().len(), 1);
        assert_eq!(db.progress_rows(None).unwrap().len(), 1);
        assert!(db.progress_rows(Some("kingdom")).unwrap().is_empty());
        assert_eq!(db.write(|tx| tx.clear_volume_progress("bob")).unwrap(), 1);
    }

    #[test]
    fn devices_keep_the_users_names_and_stop_at_the_cap() {
        let (_d, db) = temp_db();
        let seen = |id: &'static str, label: Option<&'static str>, at| DeviceSeen {
            device_id: id,
            label,
            first_seen: at,
            last_seen: at,
            source: "history",
        };
        assert!(
            !db.write(|tx| tx.name_device("u", "d1", "My PC")).unwrap(),
            "not seen yet"
        );
        db.write(|tx| tx.see_device("u", &seen("d1", Some("Desktop · Linux"), 10)))
            .unwrap();
        assert!(db.write(|tx| tx.name_device("u", "d1", "My PC")).unwrap());
        db.write(|tx| tx.see_device("u", &seen("d1", Some("Desktop · Linux"), 5)))
            .unwrap();
        db.write(|tx| tx.see_device("u", &seen("d1", None, 30)))
            .unwrap();
        let d = db.devices_of("u").unwrap();
        assert_eq!(d[0].label.as_deref(), Some("My PC"));
        assert_eq!((d[0].first_seen, d[0].last_seen), (5, 30));
        let ids: Vec<String> = (0..DEVICES_PER_USER + 5).map(|i| format!("x{i}")).collect();
        for id in &ids {
            db.write(|tx| {
                tx.see_device(
                    "u",
                    &DeviceSeen {
                        device_id: id,
                        label: None,
                        first_seen: 1,
                        last_seen: 1,
                        source: "positions",
                    },
                )
            })
            .unwrap();
        }
        assert_eq!(db.devices_of("u").unwrap().len() as i64, DEVICES_PER_USER);
    }

    #[test]
    fn queries_read_back_what_was_written() {
        let (_d, db) = temp_db();
        let k = known("u1", "Kingdom");
        let day = NOW / DAY;
        db.write(|tx| {
            tx.ingest_positions("alice", &[pos("u1", NOW, 5, 0, "d")], &k, NOW)?;
            tx.ingest_positions("bob", &[pos("u1", NOW, 5, 0, "d")], &k, NOW)?;
            let c = |requests, bytes_out, bytes_in| PrincipalCounts {
                requests,
                bytes_out,
                bytes_in,
            };
            tx.add_principal(day, "u:alice", "progress_sync", &c(3, 10, 20))?;
            tx.add_principal(day, "a:0123456789ab", "volume", &c(1, 1, 0))
        })
        .unwrap();
        let days = db.reading_days(day, day).unwrap();
        assert_eq!((days[0].readers, days[0].reads, days[0].opens), (2, 2, 2));
        assert_eq!(db.top_readers(day, day, 10).unwrap().len(), 2);
        let v = db.top_volumes(day, day, 10).unwrap();
        assert_eq!((v[0].readers, v[0].series_key.as_str()), (2, "kingdom"));
        assert_eq!(db.reading_sources(day, day).unwrap(), (2, 0));
        let accounts = db.account_traffic(day, day, 10).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(
            (accounts[0].username.as_str(), accounts[0].requests),
            ("alice", 3)
        );
        assert_eq!(db.accounts_per_day(day, day).unwrap(), vec![(day, 1)]);
        assert_eq!(db.active_accounts(day - 6, day).unwrap(), 1);
        assert_eq!(
            db.series_title_of("kingdom").unwrap().as_deref(),
            Some("Kingdom")
        );
        assert_eq!(db.series_title_of("nothing").unwrap(), None);
    }

    #[test]
    fn main_db_uuid_lookups() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("mokuro.db")).unwrap();
        db.remember_volume_uuid("S/v1.cbz", "uuid-1").unwrap();
        db.write(|c| {
            c.execute(
                "INSERT INTO series_entry_cache (volume_key, series_key, entry_json, cbz_size, cbz_mtime) \
                 VALUES ('S/v2.cbz', 's', '{\"volume_uuid\":\"det-2\",\"page_count\":120}', 1, 1), \
                 ('S/v1.cbz', 's', '{\"volume_uuid\":\"uuid-1\",\"page_count\":80}', 1, 1), \
                 ('S/bad.cbz', 's', 'not json', 1, 1)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let mut all = db.analytics_volume_uuids(10).unwrap();
        all.sort();
        assert_eq!(
            all,
            [
                ("S/v1.cbz".to_string(), "uuid-1".to_string()),
                ("S/v2.cbz".to_string(), "det-2".to_string())
            ]
        );
        assert_eq!(db.analytics_volume_uuids(1).unwrap().len(), 1);
        assert_eq!(
            db.analytics_volume_key_of_uuid("uuid-1")
                .unwrap()
                .as_deref(),
            Some("S/v1.cbz")
        );
        assert_eq!(db.analytics_volume_key_of_uuid("nope").unwrap(), None);
        let pages = db.analytics_page_counts(None).unwrap();
        assert_eq!((pages["S/v1.cbz"], pages["S/v2.cbz"]), (80, 120));
        let some = db
            .analytics_page_counts(Some(&["S/v2.cbz".to_string()]))
            .unwrap();
        assert_eq!(some.len(), 1);
    }
}
