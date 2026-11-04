//! `GET /_admin/api/analytics/{reading,users,funnel}` (design §10).

use crate::analytics::Analytics;
use crate::analytics::api::{ApiQuery, not_collected};
use axum::response::Response;

pub async fn reading(a: &Analytics, q: &ApiQuery) -> Response {
    not_collected(a, q).await
}

pub async fn users(a: &Analytics, q: &ApiQuery) -> Response {
    not_collected(a, q).await
}

/// `?series=<series_key>`.
pub async fn funnel(a: &Analytics, q: &ApiQuery) -> Response {
    not_collected(a, q).await
}
