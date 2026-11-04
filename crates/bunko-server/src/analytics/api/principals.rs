//! `GET /_admin/api/analytics/principals?limit=50`: who the traffic was for (accounts,
//! anonymous visitors by pseudonymous id, processors), heaviest first, with the IPs an
//! anonymous id was seen from while the IP retention keeps them.

use super::{ApiQuery, answer, query};
use crate::analytics::Analytics;
use crate::analytics::principal::{PrincipalKind, label};
use axum::response::Response;
use serde_json::{Map, Value, json};

const LIMIT_DEFAULT: usize = 50;
const LIMIT_MAX: usize = 500;
/// IPs listed per anonymous principal.
const IPS_MAX: usize = 10;

pub(super) async fn get(a: &Analytics, q: &ApiQuery) -> Response {
    let limit = q
        .param("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(LIMIT_DEFAULT)
        .clamp(1, LIMIT_MAX);
    let ip_days = i64::from(a.config().retention.ip_days);
    let db = a.db().clone();
    let range = q.range;
    let today = q.now.div_euclid(86400);
    let body = query("analytics principals", move || {
        let (d0, d1) = range.days();
        let mut rows = Vec::new();
        for p in db.principals(d0, d1, limit)? {
            let kind = PrincipalKind::of(&p.principal);
            let ips = if kind == PrincipalKind::Anon {
                db.anon_ips_of(&p.principal, today - ip_days, IPS_MAX)?
            } else {
                Vec::new()
            };
            let by_category: Map<String, Value> = p
                .by_category
                .iter()
                .map(|(c, n)| (c.clone(), json!(n.bytes_out)))
                .collect();
            rows.push(json!({
                "principal": p.principal,
                "label": label(&p.principal),
                "kind": kind.as_str(),
                "requests": p.totals.requests,
                "bytes_out": p.totals.bytes_out,
                "bytes_in": p.totals.bytes_in,
                "by_category": by_category,
                "last_seen": p.last_seen,
                "ips": ips,
            }));
        }
        let (users, anon, processor) = db.principal_kinds(d0, d1)?;
        Ok(json!({
            "rows": rows,
            "totals": {"users": users, "anon": anon, "processor": processor},
        }))
    })
    .await;
    match body {
        Ok(body) => answer(a, q, body),
        Err(r) => r,
    }
}
