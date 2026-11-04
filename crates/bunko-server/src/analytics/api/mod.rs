//! `/_admin/api/analytics/*` (design §10): admin only (the admin panel's gate), every
//! mutating call through its CSRF check. `range=24h|7d|30d|90d|365d|all` (default
//! `30d`): hourly buckets up to 7 days, daily beyond. Every answer carries `range`,
//! `tracking_since` and `enabled`; a time series is `{"t": [...], "series": [{"key",
//! "label", "values"}]}` with a value for every `t` (zeros filled).

mod health;
mod overview;
mod principals;
mod series;
mod traffic;

use super::Analytics;
use super::classify::category;
use crate::admin::{internal, ok, parse_qs, query_one};
use axum::response::Response;
use bunko_db::analytics::traffic::{Granularity, Span};
use http::Method;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// A parsed `range` (design §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeSpec {
    pub key: &'static str,
    /// Its buckets (hour or day numbers).
    pub span: Span,
    /// Epoch seconds: the first bucket's start, and now.
    pub from: i64,
    pub to: i64,
}

impl RangeSpec {
    /// `value` (`None`: `30d`) as of `now`; `all` starts at `tracking_since`.
    pub fn parse(
        value: Option<&str>,
        now: i64,
        tracking_since: Option<i64>,
    ) -> Result<RangeSpec, String> {
        let (key, granularity, buckets) = match value.unwrap_or("30d") {
            "24h" => ("24h", Granularity::Hour, 24),
            "7d" => ("7d", Granularity::Hour, 7 * 24),
            "30d" => ("30d", Granularity::Day, 30),
            "90d" => ("90d", Granularity::Day, 90),
            "365d" => ("365d", Granularity::Day, 365),
            "all" => ("all", Granularity::Day, 0),
            other => {
                return Err(format!(
                    "range must be one of 24h, 7d, 30d, 90d, 365d, all (got '{other}')"
                ));
            }
        };
        let secs = granularity.seconds();
        let to_b = now.div_euclid(secs);
        let from_b = if key == "all" {
            tracking_since
                .map_or(to_b, |t| t.div_euclid(secs))
                .min(to_b)
        } else {
            to_b - buckets + 1
        };
        Ok(RangeSpec {
            key,
            span: Span {
                granularity,
                from: from_b,
                to: to_b,
            },
            from: from_b * secs,
            to: now,
        })
    }

    pub fn granularity_name(&self) -> &'static str {
        match self.span.granularity {
            Granularity::Hour => "hour",
            Granularity::Day => "day",
        }
    }

    /// The same number of buckets just before this range (`None` for `all`).
    pub fn previous(&self) -> Option<Span> {
        if self.key == "all" {
            return None;
        }
        let n = self.span.to - self.span.from + 1;
        Some(Span {
            granularity: self.span.granularity,
            from: self.span.from - n,
            to: self.span.from - 1,
        })
    }

    /// The bucket starts, epoch seconds (the `t` of every time series).
    pub fn axis(&self) -> Vec<i64> {
        let secs = self.span.granularity.seconds();
        (self.span.from..=self.span.to).map(|b| b * secs).collect()
    }

    /// The days this range touches (day-bucketed tables), `from..=to`.
    pub fn days(&self) -> (i64, i64) {
        self.span.days()
    }

    /// The hours it covers, `from..=to`.
    pub fn hours(&self) -> (i64, i64) {
        self.span.hours()
    }
}

/// An analytics API request.
#[derive(Debug, Clone)]
pub struct ApiQuery {
    pub range: RangeSpec,
    /// Epoch seconds of the first record (`None`: nothing collected yet).
    pub tracking_since: Option<i64>,
    /// The query string's pairs (`urllib.parse.parse_qs`).
    pub params: Vec<(String, String)>,
    /// Epoch seconds the request is answered as of.
    pub now: i64,
}

impl ApiQuery {
    /// First value of `name`, stripped (`None` when missing or blank).
    pub fn param(&self, name: &str) -> Option<String> {
        query_one(&self.params, name)
    }
}

/// `range`, `tracking_since`, `enabled`: what every answer carries.
pub fn common(a: &Analytics, q: &ApiQuery) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "range".into(),
        json!({
            "key": q.range.key,
            "from": q.range.from,
            "to": q.range.to,
            "granularity": q.range.granularity_name(),
        }),
    );
    m.insert("tracking_since".into(), json!(q.tracking_since));
    m.insert("enabled".into(), json!(a.config().enabled));
    m
}

/// A body with the common fields added.
pub fn answer(a: &Analytics, q: &ApiQuery, body: Value) -> Response {
    let mut m = common(a, q);
    if let Value::Object(b) = body {
        m.extend(b);
    }
    ok(Value::Object(m))
}

/// Run a query off the async workers; a database error is a 500.
pub(crate) async fn query<T: Send + 'static>(
    what: &str,
    f: impl FnOnce() -> bunko_db::Result<T> + Send + 'static,
) -> Result<T, Response> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(internal(what, e)),
        Err(e) => Err(internal("worker failed", e)),
    }
}

/// `{"t": axis, "series": [...]}` from `(bucket, key, value)` points; series in
/// `order`, each with one value per bucket.
pub fn time_series(
    range: &RangeSpec,
    order: &[(String, String)],
    points: impl IntoIterator<Item = (i64, String, u64)>,
) -> Value {
    let n = (range.span.to - range.span.from + 1).max(0) as usize;
    let mut values: HashMap<String, Vec<u64>> = order
        .iter()
        .map(|(k, _)| (k.clone(), vec![0u64; n]))
        .collect();
    for (bucket, key, v) in points {
        let i = bucket - range.span.from;
        if i < 0 || i as usize >= n {
            continue;
        }
        if let Some(slot) = values.get_mut(&key) {
            slot[i as usize] += v;
        }
    }
    let series: Vec<Value> = order
        .iter()
        .map(|(k, label)| json!({"key": k, "label": label, "values": values.remove(k).unwrap_or_default()}))
        .collect();
    json!({"t": range.axis(), "series": series})
}

/// Top `n` keys by `total`, the rest summed into `other` (whose label is "Other").
pub fn top_with_other(mut totals: Vec<(String, u64)>, n: usize) -> (Vec<String>, bool) {
    totals.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let more = totals.len() > n;
    (totals.into_iter().take(n).map(|(k, _)| k).collect(), more)
}

pub fn category_label(key: &str) -> String {
    match key {
        category::VOLUME => "Volumes",
        category::SIDECAR => "OCR sidecars",
        category::COVER => "Covers",
        category::METADATA => "Navigation & metadata",
        category::PROGRESS_SYNC => "Progress sync",
        category::PROFILE_SYNC => "Profile sync",
        category::PERSONAL => "Personal corrections",
        category::UPLOAD => "Uploads",
        category::AUTH => "Sign-in",
        category::ADMIN => "Admin",
        category::WEB => "Web pages",
        category::PROCESSOR => "OCR processors",
        category::OTHER => "Other",
        other => return other.to_string(),
    }
    .to_string()
}

pub fn protocol_label(key: &str) -> String {
    match key {
        "dav" => "WebDAV",
        "api" => "JSON API",
        "web" => "Web",
        "processor" => "Processor",
        other => return other.to_string(),
    }
    .to_string()
}

pub fn detail_label(key: &str) -> String {
    match key {
        "" => "(none)",
        "full" => "Full download",
        "range" => "Range / resumed",
        "primary" => "Primary sidecar",
        "layer" => "OCR layer",
        "propfind_0" => "PROPFIND depth 0",
        "propfind_1" => "PROPFIND depth 1",
        "propfind_inf" => "PROPFIND depth infinity",
        "options" => "OPTIONS",
        "series_json" => "series.json",
        "catalog_json" => "catalog.json",
        "catalog_api" => "Catalog API",
        "manifest" => "Volume manifest",
        "login_me" => "/login/api/me",
        "queue_poll" => "OCR queue polling",
        "volume_data" => "volume-data.json",
        "positions" => "positions.json",
        "history" => "Reading history",
        "profiles" => "profiles.json",
        "goals" => "goals.json",
        "archive" => "Archive",
        "sidecar" => "Sidecar",
        "folder" => "Folder",
        "other" => "Other",
        other => return other.to_string(),
    }
    .to_string()
}

/// Dispatch `/_admin/api/analytics/<seg...>` (the admin panel's one arm).
pub async fn dispatch(
    a: &Analytics,
    seg: &[&str],
    method: &Method,
    query_string: &str,
) -> Response {
    let get = *method == Method::GET;
    let post = *method == Method::POST;
    let params = parse_qs(query_string);
    let db = a.db().clone();
    let tracking_since = match query("analytics: tracking_since", move || db.tracking_since()).await
    {
        Ok(v) => v,
        Err(r) => return r,
    };
    let now = super::now_s();
    let range = match RangeSpec::parse(query_one(&params, "range").as_deref(), now, tracking_since)
    {
        Ok(r) => r,
        Err(msg) => return crate::admin::error(400, msg),
    };
    let q = ApiQuery {
        range,
        tracking_since,
        params,
        now,
    };
    match seg {
        ["overview"] if get => overview::get(a, &q).await,
        ["traffic"] if get => traffic::get(a, &q).await,
        ["principals"] if get => principals::get(a, &q).await,
        ["series"] if get => series::get(a, &q).await,
        ["health"] if get => health::get(a, &q).await,
        ["funnel"] if get => super::reading::api::funnel(a, &q).await,
        ["reading"] if get => super::reading::api::reading(a, &q).await,
        ["users"] if get => super::reading::api::users(a, &q).await,
        ["library"] if get => super::library::api::library(a, &q).await,
        ["library", "rescan"] if post => super::library::api::rescan(a).await,
        ["host"] if get => super::host::api::host(a, &q).await,
        ["crawlers"] if get => super::crawl::api::crawlers(a, &q).await,
        ["clients"] if get => super::clients::api::clients(a, &q).await,
        ["geo"] if get => super::geo::api::geo(a, &q).await,
        ["geo", "refresh"] if post => super::geo::api::refresh(a).await,
        _ => crate::admin::not_found(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let now = 20_000 * 86400 + 5 * 3600 + 7;
        let r = RangeSpec::parse(None, now, None).unwrap();
        assert_eq!((r.key, r.granularity_name()), ("30d", "day"));
        assert_eq!((r.span.from, r.span.to), (19_971, 20_000));
        assert_eq!(r.axis().len(), 30);
        assert_eq!(r.from, 19_971 * 86400);
        assert_eq!(r.to, now);
        assert_eq!(
            r.previous().unwrap(),
            Span {
                granularity: Granularity::Day,
                from: 19_941,
                to: 19_970
            }
        );
        let h = RangeSpec::parse(Some("24h"), now, None).unwrap();
        assert_eq!(h.granularity_name(), "hour");
        assert_eq!(h.axis().len(), 24);
        assert_eq!(h.span.to, now / 3600);
        assert_eq!(
            RangeSpec::parse(Some("7d"), now, None)
                .unwrap()
                .axis()
                .len(),
            168
        );
        let all = RangeSpec::parse(Some("all"), now, Some(19_990 * 86400 + 10)).unwrap();
        assert_eq!(all.span.from, 19_990);
        assert!(all.previous().is_none());
        assert_eq!(
            RangeSpec::parse(Some("all"), now, None)
                .unwrap()
                .axis()
                .len(),
            1
        );
        assert!(RangeSpec::parse(Some("1y"), now, None).is_err());
    }

    #[test]
    fn series_fill_zeros() {
        let r = RangeSpec::parse(Some("24h"), 100 * 3600, None).unwrap();
        let v = time_series(
            &r,
            &[("a".into(), "A".into()), ("b".into(), "B".into())],
            [
                (100, "a".to_string(), 5),
                (99, "a".to_string(), 1),
                (50, "a".to_string(), 9),
                (100, "z".to_string(), 1),
            ],
        );
        let a = &v["series"][0]["values"];
        assert_eq!(a.as_array().unwrap().len(), 24);
        assert_eq!(a[23], 5);
        assert_eq!(a[22], 1);
        assert_eq!(v["series"][1]["values"][23], 0);
        assert_eq!(v["t"][23], 100 * 3600);
        let (top, more) =
            top_with_other(vec![("x".into(), 1), ("y".into(), 5), ("z".into(), 3)], 2);
        assert_eq!(top, ["y", "z"]);
        assert!(more);
    }
}
