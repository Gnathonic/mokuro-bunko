//! `GET /_admin/api/analytics/health`: status classes over time, the commonest errors,
//! failed logins (counted over the whole range; by IP and by username within the IP
//! retention only, `ip_days`), latency per kind of
//! request, the slowest requests, and transfer efficiency.

use super::{ApiQuery, answer, query, time_series};
use crate::analytics::Analytics;
use axum::response::Response;
use bunko_db::analytics::traffic::{TrafficCounts, TrafficDim, TrafficFilter, histogram_quantile};
use serde_json::{Value, json};

const ERRORS_MAX: usize = 50;
const TOP_IPS: usize = 10;
const TOP_USERNAMES: usize = 10;
const SLOW_MAX: usize = 50;

fn ms(v: Option<f64>) -> Value {
    v.map_or(Value::Null, |m| json!((m * 10.0).round() / 10.0))
}

pub(super) async fn get(a: &Analytics, q: &ApiQuery) -> Response {
    let db = a.db().clone();
    let ip_days = a.config().retention.ip_days;
    let range = q.range;
    let body = query("analytics health", move || {
        let none = TrafficFilter::default();
        let (h0, h1) = range.hours();
        let by_status = db.traffic(&range.span, Some(TrafficDim::StatusClass), true, &none)?;
        let order: Vec<(String, String)> = ["2", "3", "4", "5"]
            .iter()
            .map(|c| (c.to_string(), format!("{c}xx")))
            .collect();
        let mut status = time_series(
            &range,
            &order,
            by_status
                .into_iter()
                .filter_map(|r| Some((r.bucket?, r.key, r.counts.requests))),
        );
        // Keys as the UI names them.
        if let Some(Value::Array(series)) = status.get_mut("series") {
            for s in series {
                if let Some(label) = s.get("label").cloned() {
                    s["key"] = label;
                }
            }
        }
        let errors: Vec<Value> = db
            .errors(&range.span, ERRORS_MAX)?
            .into_iter()
            .map(|e| {
                json!({"status": e.status, "protocol": e.protocol, "method": e.method, "category": e.category, "count": e.count})
            })
            .collect();
        let failures = time_series(
            &range,
            &[("auth_failures".into(), "Failed sign-ins".into())],
            db.auth_failure_series(&range.span)?
                .into_iter()
                .map(|(b, n)| (b, "auth_failures".to_string(), n)),
        );
        let values = failures["series"][0]["values"].clone();
        let top_ips: Vec<Value> = db
            .auth_failure_top_ips(h0, h1, TOP_IPS)?
            .into_iter()
            .map(|f| json!({"ip": f.ip, "country": f.country, "count": f.count, "usernames": f.usernames}))
            .collect();
        let top_usernames: Vec<Value> = db
            .auth_failure_top_usernames(h0, h1, TOP_USERNAMES)?
            .into_iter()
            .map(|(u, n)| json!({"username": u, "count": n}))
            .collect();
        let mut latency = db.latency(&range.span)?;
        latency.sort_by(|a, b| {
            b.total
                .iter()
                .sum::<u64>()
                .cmp(&a.total.iter().sum::<u64>())
                .then_with(|| (&a.protocol, &a.method, &a.category).cmp(&(&b.protocol, &b.method, &b.category)))
        });
        let latency: Vec<Value> = latency
            .iter()
            .map(|l| {
                json!({
                    "protocol": l.protocol,
                    "method": l.method,
                    "category": l.category,
                    "requests": l.total.iter().sum::<u64>(),
                    "p50_ms": ms(histogram_quantile(&l.total, 0.50)),
                    "p95_ms": ms(histogram_quantile(&l.total, 0.95)),
                    "p99_ms": ms(histogram_quantile(&l.total, 0.99)),
                    "ttfb_p50_ms": ms(histogram_quantile(&l.headers, 0.50)),
                    "ttfb_p95_ms": ms(histogram_quantile(&l.headers, 0.95)),
                })
            })
            .collect();
        let slow: Vec<Value> = db
            .slow_requests(range.from, range.to + 1, SLOW_MAX)?
            .into_iter()
            .map(|s| json!({"at": s.at, "method": s.method, "path": s.path, "status": s.status, "ms": s.ms, "principal": s.principal}))
            .collect();
        let t: TrafficCounts = db
            .traffic(&range.span, None, false, &none)?
            .into_iter()
            .next()
            .map(|r| r.counts)
            .unwrap_or_default();
        Ok(json!({
            "status": status,
            "errors": errors,
            "auth_failures": {
                "t": failures["t"],
                "values": values,
                "top_ips": top_ips,
                "top_usernames": top_usernames,
                // The IPs and usernames are kept this long; the counts forever.
                "ip_days": ip_days,
            },
            "latency": latency,
            "slow": slow,
            "efficiency": {
                "not_modified": t.not_modified,
                "saved_bytes": t.saved_bytes,
                "gzip_raw_bytes": t.gzip_raw_bytes,
                "gzip_sent_bytes": t.gzip_sent_bytes,
                "aborted": t.aborted,
                "aborted_bytes": t.aborted_bytes,
                "resumed": t.resumed,
                "est_bytes_out": t.est_bytes_out,
            },
        }))
    })
    .await;
    match body {
        Ok(body) => answer(a, q, body),
        Err(r) => r,
    }
}
