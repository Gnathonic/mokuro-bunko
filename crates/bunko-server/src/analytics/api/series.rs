//! `GET /_admin/api/analytics/series?sort=downloads|downloaders|readers|reads|minutes|
//! pages|bytes&limit=50&q=`: the popular series, with the previous period of the same
//! length for trending.

use super::overview::round1;
use super::{ApiQuery, answer, query};
use crate::analytics::Analytics;
use axum::response::Response;
use bunko_db::analytics::AnalyticsDb;
use serde_json::{Value, json};
use std::collections::HashMap;

const LIMIT_DEFAULT: usize = 50;
const LIMIT_MAX: usize = 500;

#[derive(Debug, Clone, Default)]
struct Row {
    title: String,
    downloads: u64,
    downloaders: u64,
    readers: u64,
    reads: u64,
    pages: u64,
    minutes: f64,
    bytes: u64,
    volumes: u64,
}

fn rows(db: &AnalyticsDb, d0: i64, d1: i64) -> bunko_db::Result<HashMap<String, Row>> {
    let mut out: HashMap<String, Row> = HashMap::new();
    for s in db.series_downloads(d0, d1)? {
        let r = out.entry(s.series_key).or_default();
        r.title = s.title;
        r.downloads = s.downloads;
        r.downloaders = s.downloaders;
        r.bytes = s.bytes;
        r.volumes = s.volumes;
    }
    for s in db.series_reading(d0, d1)? {
        let r = out.entry(s.series_key).or_default();
        if r.title.is_empty() {
            r.title = s.title;
        }
        r.readers = s.readers;
        r.reads = s.reads;
        r.pages = s.pages;
        r.minutes = s.minutes;
    }
    Ok(out)
}

pub(super) async fn get(a: &Analytics, q: &ApiQuery) -> Response {
    let sort = q.param("sort").unwrap_or_else(|| "downloads".into());
    if !matches!(
        sort.as_str(),
        "downloads" | "downloaders" | "readers" | "reads" | "minutes" | "pages" | "bytes"
    ) {
        return crate::admin::error(
            400,
            format!(
                "sort must be one of downloads, downloaders, readers, reads, minutes, pages, bytes (got '{sort}')"
            ),
        );
    }
    let limit = q
        .param("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(LIMIT_DEFAULT)
        .clamp(1, LIMIT_MAX);
    let needle = q.param("q").map(|s| bunko_db::fold_series_title_key(&s));
    let db = a.db().clone();
    let range = q.range;
    let body = query("analytics series", move || {
        let (d0, d1) = range.days();
        let current = rows(&db, d0, d1)?;
        let previous = match range.previous() {
            Some(p) => {
                let (p0, p1) = p.days();
                rows(&db, p0, p1)?
            }
            None => HashMap::new(),
        };
        let mut list: Vec<(String, Row)> = current
            .into_iter()
            .filter(|(key, r)| {
                needle.as_ref().is_none_or(|n| {
                    key.contains(n.as_str())
                        || bunko_db::fold_series_title_key(&r.title).contains(n.as_str())
                })
            })
            .collect();
        let metric = |r: &Row| -> f64 {
            match sort.as_str() {
                "downloaders" => r.downloaders as f64,
                "readers" => r.readers as f64,
                "reads" => r.reads as f64,
                "minutes" => r.minutes,
                "pages" => r.pages as f64,
                "bytes" => r.bytes as f64,
                _ => r.downloads as f64,
            }
        };
        list.sort_by(|a, b| {
            metric(&b.1)
                .total_cmp(&metric(&a.1))
                .then_with(|| a.0.cmp(&b.0))
        });
        let rows: Vec<Value> = list
            .into_iter()
            .take(limit)
            .map(|(key, r)| {
                let prev = previous.get(&key).cloned().unwrap_or_default();
                json!({
                    "series_key": key,
                    "title": r.title,
                    "downloads": r.downloads,
                    "downloaders": r.downloaders,
                    "readers": r.readers,
                    "reads": r.reads,
                    "pages": r.pages,
                    "minutes": round1(r.minutes),
                    "bytes_out": r.bytes,
                    "prev": {"downloads": prev.downloads, "readers": prev.readers},
                    "volumes": r.volumes,
                })
            })
            .collect();
        Ok(json!({"rows": rows, "sort": sort}))
    })
    .await;
    match body {
        Ok(body) => answer(a, q, body),
        Err(r) => r,
    }
}
