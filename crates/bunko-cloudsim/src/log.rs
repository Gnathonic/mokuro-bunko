//! The request log behind `GET /_sim/log`: one entry per provider request, with its
//! class, Drive quota units, and bytes in and out. API budgets read this rather than a
//! provider's dashboard. Handlers tag their responses with a [`LogTag`] (MEGA's with
//! `bunko_mega::mock::MegaTag`); a middleware turns tagged responses into entries.

use axum::body::HttpBody;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use bunko_mega::mock::MegaTag;
use parking_lot::Mutex;
use serde::Serialize;

use crate::SimState;
use crate::accounts::Provider;

/// What a provider handler says about the request it answered.
#[derive(Debug, Clone)]
pub struct LogTag {
    pub provider: Provider,
    /// "" when no account matched (a refused token).
    pub account: String,
    pub class: &'static str,
    pub bytes_in: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub seq: u64,
    pub provider: Provider,
    pub account: String,
    pub method: String,
    pub path: String,
    /// `list`, `metadata`, `download`, `upload`, `write`, `delete`, `changes` or `auth`.
    pub class: &'static str,
    /// Drive's per-user query quota counts requests: 1 per Drive request, 0 elsewhere.
    pub units: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub status: u16,
}

#[derive(Default)]
pub struct Log {
    entries: Mutex<Vec<LogEntry>>,
}

impl Log {
    pub fn list(&self) -> Vec<LogEntry> {
        self.entries.lock().clone()
    }

    pub fn clear(&self) {
        self.entries.lock().clear();
    }

    fn push(&self, mut e: LogEntry) {
        let mut v = self.entries.lock();
        e.seq = v.len() as u64 + 1;
        v.push(e);
    }
}

/// Response extension → log entry.
pub async fn record(State(sim): State<SimState>, req: Request, next: Next) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let resp = next.run(req).await;
    let tag = resp.extensions().get::<LogTag>().cloned().or_else(|| {
        resp.extensions().get::<MegaTag>().map(|m| LogTag {
            provider: Provider::Mega,
            account: m.account.clone(),
            class: m.class,
            bytes_in: m.bytes_in,
        })
    });
    if let Some(t) = tag {
        let hint = resp.body().size_hint();
        sim.log().push(LogEntry {
            seq: 0,
            provider: t.provider,
            account: t.account,
            method,
            path,
            class: t.class,
            units: u64::from(t.provider == Provider::Google && t.class != "auth"),
            bytes_in: t.bytes_in,
            bytes_out: hint.exact().unwrap_or(hint.lower()),
            status: resp.status().as_u16(),
        });
    }
    resp
}
