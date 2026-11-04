//! `GET /_admin/api/analytics/overview`: the KPIs, bandwidth by category (top 6 and
//! `other`), the top series and the heaviest principals.

use super::{ApiQuery, answer, category_label, query, time_series, top_with_other};
use crate::analytics::Analytics;
use crate::analytics::principal::{PrincipalKind, label};
use axum::response::Response;
use bunko_db::analytics::keys;
use bunko_db::analytics::traffic::{TrafficCounts, TrafficDim, TrafficFilter};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Categories drawn on their own in the bandwidth chart.
const BANDWIDTH_TOP: usize = 6;

pub(super) async fn get(a: &Analytics, q: &ApiQuery) -> Response {
    let db = a.db().clone();
    let range = q.range;
    let now = q.now;
    let host = a.host().now();
    let uptime = a.core().started_at.elapsed().as_secs();
    let body = query("analytics overview", move || {
        let none = TrafficFilter::default();
        let totals = db
            .traffic(&range.span, None, false, &none)?
            .into_iter()
            .next()
            .map(|r| r.counts)
            .unwrap_or_default();
        let errors_5xx: u64 = db
            .traffic(&range.span, Some(TrafficDim::StatusClass), false, &none)?
            .into_iter()
            .filter(|r| r.key == "5")
            .map(|r| r.counts.requests)
            .sum();
        let (d0, d1) = range.days();
        let (h0, h1) = range.hours();
        let (downloads, downloaders) = db.download_totals(d0, d1)?;
        let reading = db.reading_totals(d0, d1)?;
        let today = now.div_euclid(86400);
        let dau = db.active_principals(today, today)?;
        let wau = db.active_principals(today - 6, today)?;
        let mau = db.active_principals(today - 29, today)?;
        let crawl_episodes = db.crawl_episodes_between(range.from, range.to + 1)?;
        let auth_failures = db.auth_failure_total(h0, h1)?;
        let dropped = db.meta_i64(keys::DROPPED_TOTAL)?.unwrap_or(0);

        let by_cat = db.traffic(&range.span, Some(TrafficDim::Category), true, &none)?;
        let mut cat_totals: HashMap<String, u64> = HashMap::new();
        for r in &by_cat {
            *cat_totals.entry(r.key.clone()).or_default() += out(&r.counts);
        }
        let (top, more) = top_with_other(cat_totals.into_iter().collect(), BANDWIDTH_TOP);
        let mut order: Vec<(String, String)> =
            top.iter().map(|k| (k.clone(), category_label(k))).collect();
        let other_needed = more && !top.iter().any(|k| k == "other");
        if other_needed {
            order.push(("other".into(), category_label("other")));
        }
        let points = by_cat.into_iter().filter_map(|r| {
            let key = if top.contains(&r.key) {
                r.key
            } else {
                "other".to_string()
            };
            Some((r.bucket?, key, out(&r.counts)))
        });
        let bandwidth = time_series(&range, &order, points);

        let mut series: Vec<(String, String, u64, u64, f64)> = Vec::new();
        for s in db.series_downloads(d0, d1)? {
            series.push((s.series_key, s.title, s.downloads, 0, 0.0));
        }
        for r in db.series_reading(d0, d1)? {
            match series.iter_mut().find(|s| s.0 == r.series_key) {
                Some(s) => {
                    s.3 = r.readers;
                    s.4 = r.minutes;
                    if s.1.is_empty() {
                        s.1 = r.title;
                    }
                }
                None => series.push((r.series_key, r.title, 0, r.readers, r.minutes)),
            }
        }
        series.sort_by(|a, b| {
            b.2.cmp(&a.2)
                .then(b.3.cmp(&a.3))
                .then(b.4.total_cmp(&a.4))
                .then_with(|| a.0.cmp(&b.0))
        });
        let top_series: Vec<Value> = series
            .into_iter()
            .take(5)
            .map(|(key, title, downloads, readers, minutes)| {
                json!({"series_key": key, "title": title, "downloads": downloads, "readers": readers, "minutes": round1(minutes)})
            })
            .collect();
        let top_principals: Vec<Value> = db
            .principals(d0, d1, 5)?
            .into_iter()
            .map(|p| {
                json!({
                    "principal": p.principal,
                    "label": label(&p.principal),
                    "kind": PrincipalKind::of(&p.principal).as_str(),
                    "bytes_out": p.totals.bytes_out,
                })
            })
            .collect();
        Ok(json!({
            "kpis": {
                "bytes_out": totals.bytes_out,
                "bytes_in": totals.bytes_in,
                "est_bytes_out": totals.est_bytes_out,
                "requests": totals.requests,
                "downloads": downloads,
                "downloaders": downloaders,
                "reads": reading.reads,
                "readers": reading.readers,
                "minutes_read": round1(reading.minutes),
                "pages_read": reading.pages,
                "dau": dau,
                "wau": wau,
                "mau": mau,
                "crawl_episodes": crawl_episodes,
                "errors_5xx": errors_5xx,
                "auth_failures": auth_failures,
                "rss_bytes": host.rss,
                "rss_peak_bytes": host.peak_rss,
                "cpu_pct": host.cpu_pct,
                "uptime_s": uptime,
                "dropped": dropped,
            },
            "bandwidth": bandwidth,
            "top_series": top_series,
            "top_principals": top_principals,
        }))
    })
    .await;
    match body {
        Ok(body) => answer(a, q, body),
        Err(r) => r,
    }
}

/// Bytes out as the UI totals them: counted plus estimated (nginx offload).
pub(super) fn out(c: &TrafficCounts) -> u64 {
    c.bytes_out + c.est_bytes_out
}

pub(super) fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}
