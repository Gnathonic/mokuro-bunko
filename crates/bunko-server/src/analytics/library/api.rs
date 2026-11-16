//! `GET /_admin/api/analytics/library` and `POST .../library/rescan` (design §10).
//!
//! The file-type part is the latest scan as it was stored (`library_scan`); page
//! formats are summed over `archive_pages`; growth is one point per UTC day (the day's
//! last snapshot, carried forward over days without a scan); uploads come from
//! `uploads_daily` for the days analytics ran and from the main database's
//! `volume_uploads` before that; dormant series are measured against the latest scan's
//! series list.

use super::pages::FORMATS;
use super::{ScanStatus, cloud_library};
use crate::analytics::Analytics;
use crate::analytics::api::{ApiQuery, answer, query};
use axum::response::Response;
use bunko_db::Database;
use bunko_db::analytics::AnalyticsDb;
use bunko_db::analytics::library::PAGES_PENDING_KEY;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// "Dormant": no download and no read in this many days.
pub const DORMANT_DAYS: i64 = 90;
/// Dormant series listed by name.
const DORMANT_LIST: usize = 200;
/// Uploaders drawn as their own series; the rest are summed.
const UPLOADER_SERIES: usize = 8;
/// Rows of `by_uploader`.
const UPLOADER_ROWS: usize = 100;
/// Rows of `uploads.recent`.
const RECENT_UPLOADS: usize = 20;
/// The longest daily axis an `all` range extends back to for the uploads recorded
/// before analytics existed.
const MAX_UPLOAD_DAYS: i64 = 3660;

/// What the handler needs besides the databases.
struct Inputs {
    days: (i64, i64),
    all: bool,
    today: i64,
    tracking_since: Option<i64>,
    cloud: bool,
    page_formats: bool,
    interval_hours: u32,
    status: ScanStatus,
}

fn axis(d0: i64, d1: i64) -> Vec<i64> {
    (d0..=d1).map(|d| d * 86400).collect()
}

/// The parent folder of a library-relative path.
fn parent_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(p, _)| p)
}

/// Growth: every day of the range gets the latest snapshot on or before it. The axis
/// starts at the first snapshot ever when that is inside the range: before it the
/// library's size is unknown, not zero (a chart from zero would read as growth).
fn growth(db: &AnalyticsDb, d0: i64, d1: i64) -> bunko_db::Result<Value> {
    let rows = db.library_days(d0, d1)?;
    let d0 = match rows.first() {
        Some(first) if first.day > d0 => first.day.min(d1),
        _ => d0,
    };
    let n = (d1 - d0 + 1).max(0) as usize;
    let (mut series, mut volumes, mut bytes) = (vec![0u64; n], vec![0u64; n], vec![0u64; n]);
    let mut it = rows.iter().peekable();
    let mut current: Option<&bunko_db::analytics::library::LibraryDay> = None;
    for (i, day) in (d0..=d1).enumerate() {
        while let Some(r) = it.peek() {
            if r.day > day {
                break;
            }
            current = Some(r);
            it.next();
        }
        if let Some(c) = current {
            series[i] = c.series;
            volumes[i] = c.volumes;
            bytes[i] = c.bytes;
        }
    }
    Ok(json!({
        "t": axis(d0, d1),
        "series": [
            {"key": "series", "label": "Series", "values": series},
            {"key": "volumes", "label": "Volumes", "values": volumes},
            {"key": "bytes", "label": "Size", "values": bytes},
        ],
    }))
}

/// Uploads per day per uploader. Series keys are positional (`uploader-0`, ...) and
/// the account name is the label, so no name can be mistaken for a unit.
fn uploads(db: &AnalyticsDb, main: &Database, i: &Inputs) -> bunko_db::Result<Value> {
    let (mut d0, d1) = i.days;
    if i.all
        && let Some(first) = main.first_volume_upload_day()?
    {
        d0 = d0.min(first).max(d1 - MAX_UPLOAD_DAYS);
    }
    // Days analytics ran come from the collector; earlier days from `volume_uploads`.
    let tracked_from = i.tracking_since.map(|t| t.div_euclid(86400));
    // (day, user) -> (volumes, bytes)
    let mut cells: BTreeMap<(i64, String), (u64, u64)> = BTreeMap::new();
    let before_end = tracked_from.map_or(d1, |t| (t - 1).min(d1));
    if before_end >= d0 {
        for (day, user, n) in main.volume_uploads_per_day(d0, before_end)? {
            cells.entry((day, user)).or_default().0 += n;
        }
    }
    if let Some(t) = tracked_from {
        for u in db.uploads_between(d0.max(t), d1)? {
            let c = cells.entry((u.day, u.username)).or_default();
            c.0 += u.volumes;
            c.1 += u.bytes;
        }
    }
    let mut per_user: HashMap<String, (u64, u64)> = HashMap::new();
    for ((_, user), (v, b)) in &cells {
        let e = per_user.entry(user.clone()).or_default();
        e.0 += v;
        e.1 += b;
    }
    let mut ranked: Vec<(String, (u64, u64))> = per_user.into_iter().collect();
    ranked.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
    let drawn: HashMap<&str, usize> = ranked
        .iter()
        .take(UPLOADER_SERIES)
        .enumerate()
        .map(|(n, (u, _))| (u.as_str(), n))
        .collect();
    let n = (d1 - d0 + 1).max(0) as usize;
    let mut values = vec![vec![0u64; n]; drawn.len()];
    let mut others = vec![0u64; n];
    for ((day, user), (v, _)) in &cells {
        let at = (day - d0) as usize;
        if at >= n {
            continue;
        }
        match drawn.get(user.as_str()) {
            Some(&k) => values[k][at] += v,
            None => others[at] += v,
        }
    }
    let mut series: Vec<Value> = ranked
        .iter()
        .take(UPLOADER_SERIES)
        .zip(values)
        .enumerate()
        .map(|(k, ((user, _), vals))| {
            json!({"key": format!("uploader-{k}"), "label": user, "values": vals})
        })
        .collect();
    if ranked.len() > UPLOADER_SERIES {
        series.push(json!({"key": "uploader-other", "label": "Other uploaders", "values": others}));
    }
    let by_uploader: Vec<Value> = ranked
        .iter()
        .take(UPLOADER_ROWS)
        .map(|(user, (v, b))| json!({"username": user, "volumes": v, "bytes": b}))
        .collect();
    let recent: Vec<Value> = main
        .recent_volume_uploads(RECENT_UPLOADS)?
        .into_iter()
        .map(|u| {
            json!({
                "volume_key": u.volume_key,
                "series_title": parent_of(&u.volume_key),
                "username": u.username,
                "uploaded_at": u.uploaded_at,
            })
        })
        .collect();
    Ok(json!({
        "t": axis(d0, d1),
        "series": series,
        "by_uploader": by_uploader,
        "recent": recent,
    }))
}

fn page_formats(db: &AnalyticsDb, i: &Inputs) -> bunko_db::Result<Value> {
    if i.cloud {
        return Ok(json!({"available": false, "reason": "cloud"}));
    }
    if !i.page_formats {
        return Ok(json!({"available": false, "reason": "disabled"}));
    }
    let t = db.page_format_totals()?;
    let pending = match i.status.pages_pending {
        Some(p) => p,
        None => db
            .meta_get(PAGES_PENDING_KEY)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    };
    let mut formats = t.formats;
    // Known formats first by pages; anything unexpected in the table is kept as is.
    formats.sort_by(|a, b| {
        b.1.cmp(&a.1).then_with(|| {
            let pos = |k: &str| {
                FORMATS
                    .iter()
                    .position(|f| *f == k)
                    .unwrap_or(FORMATS.len())
            };
            pos(&a.0).cmp(&pos(&b.0))
        })
    });
    Ok(json!({
        "available": true,
        "reason": null,
        "scanned": t.scanned,
        "pending": pending,
        "unreadable": t.unreadable,
        "formats": formats
            .into_iter()
            .map(|(ext, pages, volumes)| json!({"ext": ext, "pages": pages, "volumes": volumes}))
            .collect::<Vec<_>>(),
    }))
}

fn body(db: &AnalyticsDb, main: &Database, i: &Inputs) -> bunko_db::Result<Value> {
    let scan = db.library_scan()?;
    let (scanned_at, stored) = match &scan {
        Some((at, text)) => (Some(*at), serde_json::from_str::<Value>(text).ok()),
        None => (None, None),
    };
    let stored = stored.unwrap_or_else(|| json!({}));
    let field = |k: &str, empty: Value| stored.get(k).cloned().unwrap_or(empty);
    let dormant = if scan.is_some() {
        let (count, rows) = db.dormant_series(i.today - (DORMANT_DAYS - 1), DORMANT_LIST)?;
        json!({
            "days": DORMANT_DAYS,
            "count": count,
            "tracking_since": i.tracking_since,
            "series": rows
                .into_iter()
                .map(|s| json!({"series_key": s.series_key, "title": s.title, "volumes": s.volumes}))
                .collect::<Vec<_>>(),
        })
    } else {
        Value::Null
    };
    let st = &i.status;
    Ok(json!({
        "scanned_at": scanned_at,
        "cloud": i.cloud,
        "scanning": st.running || st.queued,
        "scan": {
            "phase": st.phase,
            "started_at": st.started_at.or_else(|| stored.get("started_at").and_then(Value::as_i64)),
            "duration_ms": stored.get("duration_ms"),
            "pages_duration_ms": stored.get("pages_duration_ms"),
            "interval_hours": i.interval_hours,
            "waiting": st.waiting,
            "error": st.error,
        },
        "totals": field("totals", json!({"series": 0, "volumes": 0, "bytes": 0, "files": 0})),
        "by_kind": field("by_kind", json!([])),
        "by_ext": field("by_ext", json!([])),
        "archive_mix": field("archive_mix", json!([])),
        "no_sidecar": field("no_sidecar", json!({"count": 0, "series": []})),
        "loose_archives": field("loose_archives", json!(0)),
        "page_formats": page_formats(db, i)?,
        "growth": growth(db, i.days.0, i.days.1)?,
        "uploads": uploads(db, main, i)?,
        "dormant": dormant,
    }))
}

pub async fn library(a: &Analytics, q: &ApiQuery) -> Response {
    let cfg = a.config();
    let inputs = Inputs {
        days: q.range.days(),
        all: q.range.key == "all",
        today: q.now.div_euclid(86400),
        tracking_since: q.tracking_since,
        cloud: cloud_library(a),
        page_formats: cfg.library_scan.page_formats,
        interval_hours: cfg.library_scan.interval_hours,
        status: a.library_scanner().status(),
    };
    let db = a.db().clone();
    let main: Arc<Database> = a.main_db().clone();
    match query("analytics library", move || body(&db, &main, &inputs)).await {
        Ok(v) => answer(a, q, v),
        Err(r) => r,
    }
}

pub async fn rescan(a: &Analytics) -> Response {
    if !a.enabled() {
        return crate::admin::error(
            409,
            "Analytics are off (analytics.enabled): turn them on to scan the library",
        );
    }
    a.library_scanner().rescan();
    crate::admin::ok(json!({"ok": true, "scanning": true}))
}
