//! `GET /_admin/api/analytics/library` and `POST .../library/rescan` (design §10).

use crate::analytics::Analytics;
use crate::analytics::api::{ApiQuery, not_collected};
use axum::response::Response;

pub async fn library(a: &Analytics, q: &ApiQuery) -> Response {
    not_collected(a, q).await
}

pub async fn rescan(a: &Analytics) -> Response {
    a.library_scanner().rescan();
    crate::admin::ok(serde_json::json!({"ok": false, "available": false}))
}
