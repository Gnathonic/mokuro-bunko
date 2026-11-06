//! `GET /_admin/api/analytics/principals?limit=50&anon=group|split|only`: who the
//! traffic was for (accounts, anonymous visitors by pseudonymous id, processors),
//! heaviest first, with the IPs an anonymous id was seen from while the IP retention
//! keeps them.
//!
//! `anon` is a display grouping only (nothing stored or counted changes): `group` (the
//! default) lists every anonymous visitor as ONE row (`principal: "a:*"`, `kind:
//! "anon_group"`, their sums, `visitors`: how many it stands for), ranked by its bytes
//! among the accounts, so a crowd of visitors never pushes an account off the list;
//! `split` lists each on its own, as before; `only` lists the anonymous visitors
//! alone (what the grouped row expands to).

use super::{ApiQuery, answer, query};
use crate::analytics::Analytics;
use crate::analytics::principal::{PrincipalKind, label};
use axum::response::Response;
use bunko_db::analytics::AnalyticsDb;
use bunko_db::analytics::traffic::{ANON_GROUP, AnonRows, PrincipalAgg};
use serde_json::{Map, Value, json};

const LIMIT_DEFAULT: usize = 50;
const LIMIT_MAX: usize = 500;
/// IPs listed per anonymous principal.
const IPS_MAX: usize = 10;

/// The `anon` parameter (`None`: `group`); `Err` names the values allowed.
pub(super) fn anon_mode(q: &ApiQuery) -> Result<(AnonRows, &'static str), Response> {
    match q.param("anon").as_deref() {
        None | Some("group") => Ok((AnonRows::Grouped, "group")),
        Some("split") => Ok((AnonRows::Each, "split")),
        Some("only") => Ok((AnonRows::Only, "only")),
        Some(other) => Err(crate::admin::error(
            400,
            format!("anon must be one of group, split, only (got '{other}')"),
        )),
    }
}

/// A row's `kind`: `anon_group` for the grouped row.
pub(super) fn kind_of(principal: &str) -> &'static str {
    if principal == ANON_GROUP {
        "anon_group"
    } else {
        PrincipalKind::of(principal).as_str()
    }
}

/// `principal`, `label`, `kind`, and for the grouped row `visitors`.
pub(super) fn head(p: &PrincipalAgg, anon_visitors: u64) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("principal".into(), json!(p.principal));
    m.insert("label".into(), json!(label(&p.principal)));
    m.insert("kind".into(), json!(kind_of(&p.principal)));
    if p.principal == ANON_GROUP {
        m.insert("visitors".into(), json!(anon_visitors));
    }
    m
}

pub(super) async fn get(a: &Analytics, q: &ApiQuery) -> Response {
    let limit = q
        .param("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(LIMIT_DEFAULT)
        .clamp(1, LIMIT_MAX);
    let (mode, mode_name) = match anon_mode(q) {
        Ok(m) => m,
        Err(r) => return r,
    };
    let ip_days = i64::from(a.config().retention.ip_days);
    let db = a.db().clone();
    let range = q.range;
    let today = q.now.div_euclid(86400);
    let body = query("analytics principals", move || {
        let (d0, d1) = range.days();
        let (users, anon, processor) = db.principal_kinds(d0, d1)?;
        let mut rows = Vec::new();
        for p in db.principals_with(d0, d1, limit, mode)? {
            rows.push(row(&db, &p, anon, today - ip_days)?);
        }
        Ok(json!({
            "anon": mode_name,
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

fn row(db: &AnalyticsDb, p: &PrincipalAgg, anon: u64, ips_from: i64) -> bunko_db::Result<Value> {
    let ips = if PrincipalKind::of(&p.principal) == PrincipalKind::Anon && p.principal != ANON_GROUP
    {
        db.anon_ips_of(&p.principal, ips_from, IPS_MAX)?
    } else {
        Vec::new()
    };
    let by_category: Map<String, Value> = p
        .by_category
        .iter()
        .map(|(c, n)| (c.clone(), json!(n.bytes_out)))
        .collect();
    let mut m = head(p, anon);
    m.insert("requests".into(), json!(p.totals.requests));
    m.insert("bytes_out".into(), json!(p.totals.bytes_out));
    m.insert("bytes_in".into(), json!(p.totals.bytes_in));
    m.insert("by_category".into(), Value::Object(by_category));
    m.insert("last_seen".into(), json!(p.last_seen));
    m.insert("ips".into(), json!(ips));
    Ok(Value::Object(m))
}
