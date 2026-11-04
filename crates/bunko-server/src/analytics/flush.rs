//! Flushing and upkeep (design §4.6): every `analytics.flush_seconds` the window is
//! swapped out and written in ONE transaction of upserts (the crawl detector writes its
//! episodes in the same one); once an hour, and at start, complete days are rolled up
//! and old rows pruned. A crash loses at most one window.

use super::Analytics;
use super::collector::Window;
use bunko_db::analytics::clients::ClientDims;
use bunko_db::analytics::keys;
use bunko_db::analytics::traffic::{DownloadRow, IpCounts, KindDims, SlowRequest, TrafficDims};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// How often the rollup and pruning run.
pub const MAINTAIN_EVERY: Duration = Duration::from_secs(3600);

/// The flush task: until `stop` (whose final flush is [`Analytics::stop`]'s).
pub(crate) async fn run(a: Analytics, stop: CancellationToken) {
    maintain(&a).await;
    let mut maintained = Instant::now();
    loop {
        let every = Duration::from_secs(u64::from(a.config().flush_seconds.max(1)));
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(every) => {}
        }
        if let Err(e) = a.flush_now().await {
            tracing::warn!("[ANALYTICS] flush failed (this window is lost): {e}");
        }
        if maintained.elapsed() >= MAINTAIN_EVERY {
            maintain(&a).await;
            maintained = Instant::now();
        }
    }
}

async fn maintain(a: &Analytics) {
    match a.maintain_at(super::now_s()).await {
        Ok(stats) => tracing::debug!(
            days = stats.days_rolled,
            pruned = stats.pruned,
            "[ANALYTICS] rollup and pruning done"
        ),
        Err(e) => tracing::warn!("[ANALYTICS] rollup and pruning failed: {e}"),
    }
}

/// Write one window (blocking). `now`: epoch seconds, for the crawl episodes.
pub(crate) fn write_window(a: &Analytics, w: Window, now: i64) -> bunko_db::Result<()> {
    let dropped = w.dropped_total();
    if dropped > 0 {
        tracing::warn!(
            "[ANALYTICS] {dropped} record(s) did not fit this flush window ({}); they are counted as dropped",
            w.dropped
                .iter()
                .map(|(m, n)| format!("{m}: {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    a.db().write(|tx| {
        for (k, c) in &w.traffic {
            let d = TrafficDims {
                protocol: k.protocol,
                method: k.method,
                category: k.category,
                detail: k.detail,
                status_class: k.status_class,
            };
            tx.add_traffic(k.hour, &d, c)?;
        }
        for (k, (h, t)) in &w.latency {
            let d = KindDims {
                protocol: k.protocol,
                method: k.method,
                category: k.category,
            };
            tx.add_latency(k.hour, &d, h, t)?;
        }
        for ((k, status), n) in &w.errors {
            let d = KindDims {
                protocol: k.protocol,
                method: k.method,
                category: k.category,
            };
            tx.add_errors(k.hour, &d, *status, *n)?;
        }
        for ((day, p, cat), c) in &w.principals {
            tx.add_principal(*day, p, cat, c)?;
        }
        for (p, (first, last)) in &w.seen {
            tx.see_principal(p, *first, *last)?;
        }
        for ((day, p, ip), n) in &w.anon_ips {
            tx.add_anon_ip(*day, p, ip, *n)?;
        }
        for ((day, ip), agg) in &w.ips {
            let c = IpCounts {
                requests: agg.requests,
                bytes_out: agg.bytes_out,
                errors: agg.errors,
                auth_failures: agg.auth_failures,
                principals: agg.principals.iter().map(|p| p.to_string()).collect(),
                user_agent: agg.user_agent.clone(),
                country: agg.country.map(|c| c.as_str().to_string()),
            };
            tx.add_ip(*day, ip, &c)?;
        }
        for ((day, p, vk), d) in &w.downloads {
            tx.add_download(&DownloadRow {
                day: *day,
                principal: p,
                volume_key: vk,
                series_key: &d.series_key,
                series_title: &d.series_title,
                bytes: d.bytes,
                size: d.size,
            })?;
        }
        for ((hour, ip, user), n) in &w.auth_failures {
            tx.add_auth_failures(*hour, ip, user, *n)?;
        }
        let slow: Vec<SlowRequest> = w.slow.values().flatten().cloned().collect();
        if !slow.is_empty() {
            tx.add_slow(&slow)?;
        }
        for ((day, ci, protocol), (requests, bytes)) in &w.clients {
            let d = ClientDims {
                app: ci.app,
                client: ci.client,
                browser: ci.browser,
                os: ci.os,
                protocol,
            };
            tx.add_client(*day, &d, *requests, *bytes)?;
        }
        for (day, ci, protocol, p) in w.client_principals.keys() {
            tx.add_client_principal(*day, ci.app, p)?;
            let d = ClientDims {
                app: ci.app,
                client: ci.client,
                browser: ci.browser,
                os: ci.os,
                protocol,
            };
            tx.add_client_mix_principal(*day, &d, p)?;
        }
        for ((day, origin), n) in &w.origins {
            tx.add_origin(*day, origin, *n)?;
        }
        for ((day, user), (volumes, bytes)) in &w.uploads {
            tx.add_uploads(*day, user, *volumes, *bytes)?;
        }
        for ((day, country), (requests, bytes)) in &w.geo {
            tx.add_geo(*day, country.as_str(), *requests, *bytes)?;
        }
        if let Some(first) = w.first_at {
            tx.meta_set_once(keys::TRACKING_SINCE, &first.to_string())?;
        }
        if dropped > 0 {
            tx.meta_add(
                keys::DROPPED_TOTAL,
                i64::try_from(dropped).unwrap_or(i64::MAX),
            )?;
        }
        a.crawl().flush_into(tx, now)
    })
}
