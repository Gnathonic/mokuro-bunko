//! `GET /_admin/api/analytics/traffic?by=category|protocol|method|detail&metric=
//! bytes_out|bytes_in|requests&category=`: a time series by `by` (one category's rows
//! with `category=`), the totals, what progress and profile sync cost, the navigation
//! and metadata split, and WebDAV by method.

use super::overview::out;
use super::{
    ApiQuery, answer, category_label, detail_label, protocol_label, query, time_series,
    top_with_other,
};
use crate::analytics::Analytics;
use crate::analytics::classify::{CATEGORIES, METHODS, PROTOCOLS, category};
use axum::response::Response;
use bunko_db::analytics::AnalyticsDb;
use bunko_db::analytics::traffic::{Span, TrafficAgg, TrafficCounts, TrafficDim, TrafficFilter};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Series drawn on their own; the rest are summed as `other`.
const SERIES_TOP: usize = 12;

#[derive(Clone, Copy)]
enum Metric {
    BytesOut,
    BytesIn,
    Requests,
}

impl Metric {
    fn of(&self, c: &TrafficCounts) -> u64 {
        match self {
            Metric::BytesOut => out(c),
            Metric::BytesIn => c.bytes_in,
            Metric::Requests => c.requests,
        }
    }
}

fn label_of(by: TrafficDim, key: &str) -> String {
    match by {
        TrafficDim::Category => category_label(key),
        TrafficDim::Protocol => protocol_label(key),
        TrafficDim::Detail => detail_label(key),
        _ => key.to_string(),
    }
}

pub(super) async fn get(a: &Analytics, q: &ApiQuery) -> Response {
    let by = match q.param("by").as_deref() {
        None | Some("category") => TrafficDim::Category,
        Some("protocol") => TrafficDim::Protocol,
        Some("method") => TrafficDim::Method,
        Some("detail") => TrafficDim::Detail,
        Some(other) => {
            return crate::admin::error(
                400,
                format!("by must be one of category, protocol, method, detail (got '{other}')"),
            );
        }
    };
    let metric = match q.param("metric").as_deref() {
        None | Some("bytes_out") => Metric::BytesOut,
        Some("bytes_in") => Metric::BytesIn,
        Some("requests") => Metric::Requests,
        Some(other) => {
            return crate::admin::error(
                400,
                format!("metric must be one of bytes_out, bytes_in, requests (got '{other}')"),
            );
        }
    };
    let filter = TrafficFilter {
        category: q.param("category"),
        ..Default::default()
    };
    if let Some(c) = &filter.category
        && !CATEGORIES.contains(&c.as_str())
    {
        return crate::admin::error(400, format!("unknown category '{c}'"));
    }
    let db = a.db().clone();
    let range = q.range;
    let body = query("analytics traffic", move || {
        let rows = db.traffic(&range.span, Some(by), true, &filter)?;
        let mut per_key: HashMap<String, u64> = HashMap::new();
        for r in &rows {
            *per_key.entry(r.key.clone()).or_default() += metric.of(&r.counts);
        }
        let (top, more) = top_with_other(per_key.into_iter().collect(), SERIES_TOP);
        let other_key = "(other)";
        let mut order: Vec<(String, String)> =
            top.iter().map(|k| (k.clone(), label_of(by, k))).collect();
        if more {
            order.push((other_key.into(), "Other".into()));
        }
        let points = rows.into_iter().filter_map(|r| {
            let key = if top.contains(&r.key) {
                r.key
            } else {
                other_key.to_string()
            };
            Some((r.bucket?, key, metric.of(&r.counts)))
        });
        let mut body = time_series(&range, &order, points);
        let mut totals = db.traffic(&range.span, Some(by), false, &filter)?;
        totals.sort_by(|a, b| {
            out(&b.counts)
                .cmp(&out(&a.counts))
                .then_with(|| a.key.cmp(&b.key))
        });
        let totals: Vec<Value> = totals
            .iter()
            .map(|t| {
                json!({
                    "key": t.key,
                    "label": label_of(by, &t.key),
                    "requests": t.counts.requests,
                    "bytes_out": t.counts.bytes_out,
                    "bytes_in": t.counts.bytes_in,
                    "est_bytes_out": t.counts.est_bytes_out,
                })
            })
            .collect();
        let sync = json!({
            "progress": sync_block(&db, &range.span, category::PROGRESS_SYNC)?,
            "profile": sync_block(&db, &range.span, category::PROFILE_SYNC)?,
        });
        let meta = details(&db, &range.span, category::METADATA)?;
        let meta_total = sum(&meta);
        let metadata = json!({
            "bytes_out": out(&meta_total),
            "bytes_in": meta_total.bytes_in,
            "requests": meta_total.requests,
            "by_detail": meta.iter().map(|d| json!({
                "key": d.key,
                "label": detail_label(&d.key),
                "requests": d.counts.requests,
                "bytes_out": out(&d.counts),
            })).collect::<Vec<_>>(),
        });
        let mut dav = db.traffic(
            &range.span,
            Some(TrafficDim::Method),
            false,
            &TrafficFilter {
                protocol: Some(PROTOCOLS[0].into()),
                ..Default::default()
            },
        )?;
        dav.sort_by_key(|m| {
            METHODS
                .iter()
                .position(|k| *k == m.key)
                .unwrap_or(METHODS.len())
        });
        let dav_methods: Vec<Value> = dav
            .iter()
            .map(|m| {
                json!({
                    "method": m.key,
                    "requests": m.counts.requests,
                    "bytes_out": out(&m.counts),
                    "bytes_in": m.counts.bytes_in,
                })
            })
            .collect();
        if let Value::Object(m) = &mut body {
            m.insert("by".into(), json!(by_name(by)));
            m.insert(
                "metric".into(),
                json!(match metric {
                    Metric::BytesOut => "bytes_out",
                    Metric::BytesIn => "bytes_in",
                    Metric::Requests => "requests",
                }),
            );
            m.insert("totals".into(), Value::Array(totals));
            m.insert("sync".into(), sync);
            m.insert("metadata".into(), metadata);
            m.insert("dav_methods".into(), Value::Array(dav_methods));
        }
        Ok(body)
    })
    .await;
    match body {
        Ok(body) => answer(a, q, body),
        Err(r) => r,
    }
}

fn by_name(by: TrafficDim) -> &'static str {
    match by {
        TrafficDim::Category => "category",
        TrafficDim::Protocol => "protocol",
        TrafficDim::Method => "method",
        TrafficDim::Detail => "detail",
        TrafficDim::StatusClass => "status_class",
    }
}

fn details(db: &AnalyticsDb, span: &Span, cat: &str) -> bunko_db::Result<Vec<TrafficAgg>> {
    let mut rows = db.traffic(
        span,
        Some(TrafficDim::Detail),
        false,
        &TrafficFilter {
            category: Some(cat.into()),
            ..Default::default()
        },
    )?;
    rows.sort_by(|a, b| {
        out(&b.counts)
            .cmp(&out(&a.counts))
            .then_with(|| a.key.cmp(&b.key))
    });
    Ok(rows)
}

fn sum(rows: &[TrafficAgg]) -> TrafficCounts {
    let mut t = TrafficCounts::default();
    for r in rows {
        t.add(&r.counts);
    }
    t
}

fn sync_block(db: &AnalyticsDb, span: &Span, cat: &str) -> bunko_db::Result<Value> {
    let rows = details(db, span, cat)?;
    let t = sum(&rows);
    Ok(json!({
        "bytes_out": out(&t),
        "bytes_in": t.bytes_in,
        "requests": t.requests,
        "by_detail": rows.iter().map(|d| json!({
            "key": d.key,
            "label": detail_label(&d.key),
            "requests": d.counts.requests,
            "bytes_out": out(&d.counts),
            "bytes_in": d.counts.bytes_in,
        })).collect::<Vec<_>>(),
    }))
}
