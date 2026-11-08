//! `GET /_admin/api/analytics/{reading,users,funnel}` (design §10).
//!
//! Reading and accounts come from day-bucketed tables (`reading_daily`,
//! `principal_daily`), so their time series have one point per UTC day whatever the
//! range's granularity: a `24h` or `7d` range covers every day it touches.

use super::resolve::{series_of_key, volume_fold, volume_stem};
use crate::analytics::Analytics;
use crate::analytics::api::{ApiQuery, answer, query};
use axum::response::Response;
use bunko_db::analytics::reading::{ProgressRow, ReaderTotals};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

/// Rows in the reading view's top lists.
const TOP: usize = 20;
/// Accounts listed in the users view (most bytes first).
const ACCOUNTS: usize = 200;
/// Series listed by the library-wide funnel.
const FUNNEL_SERIES: usize = 50;
/// How a volume with no known series is shown.
const UNKNOWN_SERIES: &str = "(unknown)";

fn minutes(dwell_ms: u64) -> f64 {
    (dwell_ms as f64 / 6000.0).round() / 10.0
}

/// The UTC days `from..=to`, as epoch seconds (the `t` of a daily series).
fn day_axis(from_day: i64, to_day: i64) -> Vec<i64> {
    (from_day..=to_day).map(|d| d * 86400).collect()
}

/// One value per day of `from..=to` from `(day, value)` points (zeros filled).
fn per_day(
    from_day: i64,
    to_day: i64,
    points: impl IntoIterator<Item = (i64, Value)>,
) -> Vec<Value> {
    let n = usize::try_from(to_day - from_day + 1).unwrap_or(0);
    let mut out = vec![json!(0); n];
    for (day, v) in points {
        if let Some(slot) = usize::try_from(day - from_day)
            .ok()
            .and_then(|i| out.get_mut(i))
        {
            *slot = v;
        }
    }
    out
}

pub async fn reading(a: &Analytics, q: &ApiQuery) -> Response {
    let db = a.db().clone();
    let (d0, d1) = q.range.days();
    let stats = a.reading().stats();
    let body = query("analytics reading", move || {
        let days = db.reading_days(d0, d1)?;
        let totals = db.reading_totals(d0, d1)?;
        let chars: u64 = days.iter().map(|d| d.chars).sum();
        let opens: u64 = days.iter().map(|d| d.opens).sum();
        let top_readers: Vec<Value> = db
            .top_readers(d0, d1, TOP)?
            .into_iter()
            .map(|r| {
                json!({
                    "username": r.username,
                    "minutes": minutes(r.dwell_ms),
                    "pages": r.pages,
                    "volumes": r.volumes,
                    "reads": r.reads,
                    "days": r.days,
                    "chars": r.chars,
                })
            })
            .collect();
        let top_volumes: Vec<Value> = db
            .top_volumes(d0, d1, TOP)?
            .into_iter()
            .map(|v| {
                let title = v
                    .volume_title
                    .clone()
                    .or_else(|| v.volume_key.as_deref().map(|k| volume_stem(k).to_string()))
                    .unwrap_or_else(|| v.volume_uuid.clone());
                let series_title = if v.series_key.is_empty() {
                    UNKNOWN_SERIES.to_string()
                } else {
                    v.series_title
                        .clone()
                        .unwrap_or_else(|| v.series_key.clone())
                };
                json!({
                    "volume_key": v.volume_key,
                    "volume_uuid": v.volume_uuid,
                    "title": title,
                    "series_key": v.series_key,
                    "series_title": series_title,
                    "readers": v.readers,
                    "reads": v.reads,
                    "pages": v.pages,
                    "minutes": minutes(v.dwell_ms),
                })
            })
            .collect();
        let (positions, history) = db.reading_sources(d0, d1)?;
        let series = vec![
            json!({"key": "minutes", "label": "Minutes read", "values": per_day(d0, d1,
                days.iter().map(|d| (d.day, json!(minutes(d.dwell_ms)))))}),
            json!({"key": "pages", "label": "Pages read", "values": per_day(d0, d1,
                days.iter().map(|d| (d.day, json!(d.pages))))}),
            json!({"key": "readers", "label": "Readers", "values": per_day(d0, d1,
                days.iter().map(|d| (d.day, json!(d.readers))))}),
        ];
        Ok(json!({
            "t": day_axis(d0, d1),
            "series_granularity": "day",
            "series": series,
            "totals": {
                "reads": totals.reads,
                "readers": totals.readers,
                "minutes": (totals.minutes * 10.0).round() / 10.0,
                "pages": totals.pages,
                "chars": chars,
                "opens": opens,
            },
            "top_readers": top_readers,
            "top_volumes": top_volumes,
            "sources": {"positions": positions, "history": history},
            "worker": stats,
        }))
    })
    .await;
    match body {
        Ok(body) => answer(a, q, body),
        Err(r) => r,
    }
}

pub async fn users(a: &Analytics, q: &ApiQuery) -> Response {
    let db = a.db().clone();
    let (d0, d1) = q.range.days();
    let today = q.now.div_euclid(86400);
    let body = query("analytics users", move || {
        let dau = db.accounts_per_day(d0, d1)?;
        let reading: HashMap<String, ReaderTotals> = db
            .top_readers(d0, d1, usize::MAX >> 1)?
            .into_iter()
            .map(|r| (r.username.clone(), r))
            .collect();
        let mut rows = Vec::new();
        for r in db.account_traffic(d0, d1, ACCOUNTS)? {
            let devices: Vec<Value> = db
                .devices_of(&r.username)?
                .into_iter()
                .map(|d| {
                    json!({
                        "id": d.device_id,
                        "label": d.label,
                        "first_seen": d.first_seen,
                        "last_seen": d.last_seen,
                        "source": d.source,
                    })
                })
                .collect();
            let clients = db.apps_of(&format!("u:{}", r.username), d0, d1)?;
            let progress = db.progress_counts(&r.username)?;
            let read = reading.get(&r.username);
            rows.push(json!({
                "username": r.username,
                "last_seen": r.last_seen,
                "days_active": r.days_active,
                "requests": r.requests,
                "bytes_out": r.bytes_out,
                "bytes_in": r.bytes_in,
                "devices": devices,
                "clients": clients,
                "minutes": read.map_or(0.0, |x| minutes(x.dwell_ms)),
                "pages": read.map_or(0, |x| x.pages),
                "volumes_read": read.map_or(0, |x| x.volumes),
                "reads": read.map_or(0, |x| x.reads),
                "volumes_started": progress.started,
                "volumes_completed": progress.completed,
            }));
        }
        let visitors = json!({
            "dau": db.active_principals(today, today)?,
            "wau": db.active_principals(today - 6, today)?,
            "mau": db.active_principals(today - 29, today)?,
        });
        Ok(json!({
            "t": day_axis(d0, d1),
            "series_granularity": "day",
            "series": [{"key": "dau", "label": "Active accounts", "values": per_day(d0, d1,
                dau.into_iter().map(|(d, n)| (d, json!(n))))}],
            "dau": db.active_accounts(today, today)?,
            "wau": db.active_accounts(today - 6, today)?,
            "mau": db.active_accounts(today - 29, today)?,
            "visitors": visitors,
            // Every account active in the range: `rows` lists at most `rows_limit` of
            // them (the most bytes first), so the UI counts from this.
            "accounts": db.active_accounts(d0, d1)?,
            "rows_limit": ACCOUNTS,
            "rows": rows,
        }))
    })
    .await;
    match body {
        Ok(body) => answer(a, q, body),
        Err(r) => r,
    }
}

/// `?series=<series_key>`: that series' volumes in library order with the people who
/// downloaded, started and finished each (all time); without it, every series.
pub async fn funnel(a: &Analytics, q: &ApiQuery) -> Response {
    let a2 = a.clone();
    let series = q.param("series");
    let body = query("analytics funnel", move || match series {
        Some(s) => series_funnel(&a2, &crate::analytics::classify::series_key(&s)),
        None => library_funnel(&a2),
    })
    .await;
    match body {
        Ok(body) => answer(a, q, body),
        Err(r) => r,
    }
}

/// Reads (user, volume) by how far they got: started, a quarter, half, three
/// quarters, finished. A finished volume counts at every stage; one whose page count
/// the library does not know counts as started (or finished) only.
#[derive(Default)]
struct Stages([u64; 5]);

impl Stages {
    fn add(&mut self, p: &ProgressRow, page_count: Option<i64>) {
        let started = p.progress > 0 || p.completed;
        if !started {
            return;
        }
        let fraction = match (p.completed, page_count) {
            (true, _) => 1.0,
            (false, Some(n)) if n > 0 => p.progress as f64 / n as f64,
            _ => 0.0,
        };
        self.0[0] += 1;
        for (i, at) in [0.25, 0.5, 0.75].into_iter().enumerate() {
            if fraction >= at {
                self.0[i + 1] += 1;
            }
        }
        if p.completed {
            self.0[4] += 1;
        }
    }

    fn json(&self) -> Value {
        let labels = [
            ("started", "Started"),
            ("p25", "Read a quarter"),
            ("p50", "Read half"),
            ("p75", "Read three quarters"),
            ("completed", "Finished"),
        ];
        Value::Array(
            labels
                .iter()
                .zip(self.0)
                .map(|((key, label), n)| json!({"key": key, "label": label, "reads": n}))
                .collect(),
        )
    }
}

/// One row of a series' funnel.
#[derive(Default)]
struct FunnelVolume {
    title: String,
    volume_key: Option<String>,
    in_library: bool,
    started: HashSet<String>,
    finished: HashSet<String>,
    downloads: u64,
}

fn series_funnel(a: &Analytics, key: &str) -> bunko_db::Result<Value> {
    let db = a.db();
    let main = a.main_db();
    // The library's volumes of every folder spelling of the series, in library order.
    let mut folder: Option<String> = None;
    let mut folders: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut vols: HashMap<String, FunnelVolume> = HashMap::new();
    if let Some(rt) = a.library_runtime() {
        let snapshot = rt.index().get_snapshot();
        let mut lib: Vec<(String, String, Option<String>)> = Vec::new();
        for s in snapshot
            .series
            .iter()
            .filter(|s| crate::analytics::classify::series_key(&s.name) == key)
        {
            folder.get_or_insert_with(|| s.name.clone());
            folders.insert(s.name.clone());
            for v in &s.volumes {
                let fold = volume_fold(&s.name, &v.name);
                if lib.iter().any(|(f, _, _)| *f == fold) {
                    continue;
                }
                let volume_key = v.has_cbz.then(|| format!("{}/{}.cbz", s.name, v.name));
                lib.push((fold, v.name.clone(), volume_key));
            }
        }
        lib.sort_by(|x, y| bunko_library::compat::natural_title_cmp(&x.1, &y.1));
        for (fold, title, volume_key) in lib {
            order.push(fold.clone());
            vols.insert(
                fold,
                FunnelVolume {
                    title,
                    volume_key,
                    in_library: true,
                    ..FunnelVolume::default()
                },
            );
        }
    }
    let resolver = a.reading().resolver();
    let mut uuid_fold: HashMap<String, String> = HashMap::new();
    let mut fold_key: HashMap<String, String> = HashMap::new();
    for (uuid, volume_key) in resolver.volumes_of_series(main, key) {
        if let Some((_, title)) = series_of_key(&volume_key) {
            let fold = volume_fold(&title, volume_stem(&volume_key));
            fold_key.entry(fold.clone()).or_insert(volume_key);
            uuid_fold.insert(uuid, fold);
        }
    }
    let progress = db.progress_rows(Some(key))?;
    let series_title = folder
        .clone()
        .or_else(|| progress.iter().find_map(|p| p.series_title.clone()));
    let fold_of = |p: &ProgressRow| -> String {
        if let Some(f) = uuid_fold.get(&p.volume_uuid) {
            return f.clone();
        }
        // A volume the library has no id for: matched by the reader's title.
        if let (Some(t), Some(s)) = (p.volume_title.as_deref(), series_title.as_deref()) {
            let f = volume_fold(s, t);
            if vols.get(&f).is_some_and(|v| v.in_library) {
                return f;
            }
        }
        format!("uuid:{}", p.volume_uuid)
    };
    let folds: Vec<String> = progress.iter().map(fold_of).collect();
    let mut readers_started: HashSet<&str> = HashSet::new();
    let mut readers_finished: HashSet<&str> = HashSet::new();
    for (p, fold) in progress.iter().zip(&folds) {
        let v = vols.entry(fold.clone()).or_insert_with(|| FunnelVolume {
            title: p
                .volume_title
                .clone()
                .unwrap_or_else(|| p.volume_uuid.clone()),
            ..FunnelVolume::default()
        });
        if p.progress > 0 || p.completed {
            v.started.insert(p.username.clone());
            readers_started.insert(&p.username);
        }
        if p.completed {
            v.finished.insert(p.username.clone());
            readers_finished.insert(&p.username);
        }
    }
    for (volume_key, n) in db.downloaders_by_volume(key)? {
        let fold = series_of_key(&volume_key)
            .map(|(_, title)| volume_fold(&title, volume_stem(&volume_key)))
            .unwrap_or_else(|| volume_key.clone());
        let v = vols.entry(fold.clone()).or_insert_with(|| FunnelVolume {
            title: volume_stem(&volume_key).to_string(),
            ..FunnelVolume::default()
        });
        v.downloads += n;
        if v.volume_key.is_none() {
            v.volume_key = Some(volume_key.clone());
        }
        fold_key.entry(fold).or_insert(volume_key);
    }
    // Page counts by the volumes' fold, never their spelling: the compiled entries of
    // every folder spelling of the series (each library volume, whether the reader has
    // an id for it or not, and whatever spelling a download keyed it by), then the
    // entries of the exact keys the views know.
    folders.extend(
        fold_key
            .values()
            .filter_map(|k| series_of_key(k).map(|(_, title)| title)),
    );
    let catalog_keys: Vec<String> = folders
        .iter()
        .map(|f| bunko_library::compat::normalize_volume_title_key(f))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut fold_pages: HashMap<String, i64> = HashMap::new();
    for (volume_key, n) in main.analytics_page_counts_of_series(&catalog_keys)? {
        if let Some((series, title)) = series_of_key(&volume_key)
            && series == key
        {
            fold_pages
                .entry(volume_fold(&title, volume_stem(&volume_key)))
                .or_insert(n);
        }
    }
    let keys: Vec<String> = fold_key.values().cloned().collect();
    let page_counts = main.analytics_page_counts(Some(&keys))?;
    for (fold, volume_key) in &fold_key {
        if let Some(n) = page_counts.get(volume_key) {
            fold_pages.entry(fold.clone()).or_insert(*n);
        }
    }
    let pages_of = |fold: &str| fold_pages.get(fold).copied();
    let mut stages = Stages::default();
    for (p, fold) in progress.iter().zip(&folds) {
        stages.add(p, pages_of(fold));
    }
    // Volumes not in the library (any more) after the library's, by title.
    let mut extra: Vec<&String> = vols
        .iter()
        .filter(|(_, v)| !v.in_library)
        .map(|(f, _)| f)
        .collect();
    extra
        .sort_by(|x, y| bunko_library::compat::natural_title_cmp(&vols[*x].title, &vols[*y].title));
    let volumes: Vec<Value> = order
        .iter()
        .chain(extra)
        .filter_map(|fold| {
            let v = vols.get(fold)?;
            Some(json!({
                "volume_key": v.volume_key.clone().or_else(|| fold_key.get(fold).cloned()),
                "title": v.title,
                "started": v.started.len(),
                "finished": v.finished.len(),
                "downloads": v.downloads,
                "in_library": v.in_library,
                "page_count": pages_of(fold),
            }))
        })
        .collect();
    let title = match series_title {
        Some(t) => t,
        None => db.series_title_of(key)?.unwrap_or_else(|| key.to_string()),
    };
    Ok(json!({
        "series_key": key,
        "title": title,
        "in_library": !order.is_empty(),
        "volumes": volumes,
        "readers_started": readers_started.len(),
        "readers_finished": readers_finished.len(),
        "stages": stages.json(),
    }))
}

fn library_funnel(a: &Analytics) -> bunko_db::Result<Value> {
    #[derive(Default)]
    struct Row {
        title: String,
        started: HashSet<String>,
        finished: HashSet<String>,
        volumes_started: u64,
        downloads: u64,
    }
    let db = a.db();
    let main = a.main_db();
    let progress = db.progress_rows(None)?;
    let uuid_key = a.reading().resolver().all_volumes(main);
    let page_counts = main.analytics_page_counts(None)?;
    let mut series: HashMap<String, Row> = HashMap::new();
    let mut stages = Stages::default();
    let mut readers_started: HashSet<&str> = HashSet::new();
    let mut readers_finished: HashSet<&str> = HashSet::new();
    for p in &progress {
        let pages = uuid_key
            .get(&p.volume_uuid)
            .and_then(|k| page_counts.get(k))
            .copied();
        stages.add(p, pages);
        let started = p.progress > 0 || p.completed;
        if started {
            readers_started.insert(&p.username);
        }
        if p.completed {
            readers_finished.insert(&p.username);
        }
        if p.series_key.is_empty() {
            continue;
        }
        let row = series.entry(p.series_key.clone()).or_default();
        if row.title.is_empty()
            && let Some(t) = &p.series_title
        {
            row.title = t.clone();
        }
        if started {
            row.started.insert(p.username.clone());
            row.volumes_started += 1;
        }
        if p.completed {
            row.finished.insert(p.username.clone());
        }
    }
    for (key, title, n) in db.downloaders_by_series()? {
        let row = series.entry(key).or_default();
        row.downloads = n;
        if row.title.is_empty() {
            row.title = title;
        }
    }
    let mut rows: Vec<(String, Row)> = series.into_iter().collect();
    rows.sort_by(|(ka, a), (kb, b)| {
        b.started
            .len()
            .cmp(&a.started.len())
            .then(b.downloads.cmp(&a.downloads))
            .then_with(|| ka.cmp(kb))
    });
    let rows: Vec<Value> = rows
        .into_iter()
        .take(FUNNEL_SERIES)
        .map(|(key, r)| {
            json!({
                "title": if r.title.is_empty() { key.clone() } else { r.title },
                "series_key": key,
                "readers_started": r.started.len(),
                "readers_finished": r.finished.len(),
                "volumes_started": r.volumes_started,
                "downloads": r.downloads,
            })
        })
        .collect();
    Ok(json!({
        "series_key": Value::Null,
        "title": "All series",
        "in_library": true,
        "volumes": [],
        "series": rows,
        "readers_started": readers_started.len(),
        "readers_finished": readers_finished.len(),
        "stages": stages.json(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_axes_fill_zeros() {
        assert_eq!(day_axis(10, 12), [864_000, 950_400, 1_036_800]);
        assert!(day_axis(10, 9).is_empty());
        let v = per_day(10, 12, [(11, json!(5)), (9, json!(1)), (13, json!(1))]);
        assert_eq!(v, [json!(0), json!(5), json!(0)]);
        assert_eq!(minutes(90_000), 1.5);
    }

    #[test]
    fn stages_are_monotonic() {
        let row = |progress, completed| ProgressRow {
            progress,
            completed,
            ..ProgressRow::default()
        };
        let mut s = Stages::default();
        s.add(&row(0, false), Some(100));
        s.add(&row(30, false), Some(100));
        s.add(&row(80, false), Some(100));
        s.add(&row(10, false), None);
        s.add(&row(0, true), None);
        assert_eq!(s.0, [4, 3, 2, 2, 1]);
    }
}
