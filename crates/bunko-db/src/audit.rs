//! The audit log (spec §8): writer, pruning, filtered keyset-paged reader, facets.
//!
//! Reproduced exactly:
//! - `details` is `json.dumps(d, separators=(",",":"), ensure_ascii=True)` — compact,
//!   INSERTION-ordered keys ([`AuditDetails`] keeps its own order), non-ASCII as lowercase
//!   `\uXXXX` (surrogate pairs above the BMP), floats as Python `repr`;
//! - pruning of rows older than 30 days, in the writing transaction, on the first event
//!   of the process and then at most once an hour;
//! - every query clause, its order, the `'progress'` default exclusion, LIKE escaping,
//!   the extra ASCII-escaped search pattern, the `INDEXED BY idx_audit_created_at` hint,
//!   `total` only on a first page, the base64url cursor `[created_at, id]` (Rust and
//!   Python cursors are interchangeable), and the error messages (`since is not a date:
//!   '...'`, `cursor is not one this server gave out`).
//!
//! Choices: a cursor id beyond 64 bits is refused as a bad cursor (Python failed with an
//! unhandled `OverflowError` at the bind); a date given with non-ASCII digits is refused
//! (Python's `\d` accepted it and compared garbage).

use crate::database::Database;
use crate::error::{DbError, Result};
use crate::pyfmt::{self, JsonStyle};
use crate::pytime::fromisoformat;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::{Connection, Row, ToSql, params_from_iter};
use serde::Serialize;
use serde_json::Value;
use std::time::{Duration, Instant};

/// Rows older than this are pruned.
pub const AUDIT_RETENTION_DAYS: u32 = 30;
/// Pruning runs at most this often (and on a process's first event).
pub const AUDIT_PRUNE_INTERVAL: Duration = Duration::from_secs(3600);
/// Default page size of [`Database::query_audit_events`].
pub const AUDIT_PAGE_SIZE: i64 = 50;
/// Largest page size.
pub const AUDIT_PAGE_MAX: i64 = 200;
/// Reading-progress sync: most of the log, hidden unless asked for.
pub const AUDIT_PROGRESS_TYPE: &str = "progress";

const AUDIT_COLUMNS: &str =
    "id, actor_username, action, target_type, target_path, target_username, details, created_at";

/// Audit details in insertion order, as a Python dict literal keeps them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuditDetails(Vec<(String, Value)>);

impl AuditDetails {
    pub fn new() -> Self {
        AuditDetails(Vec::new())
    }

    /// Append (or replace in place, as re-assigning a dict key does) one entry.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.insert(key, value);
        self
    }

    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        let key = key.into();
        let value = value.into();
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key, value)),
        }
    }

    pub fn entries(&self) -> &[(String, Value)] {
        &self.0
    }

    /// The `details` column text: compact ASCII-escaped JSON.
    pub fn to_json(&self) -> String {
        let style = JsonStyle::COMPACT_ASCII;
        let mut out = String::from("{");
        for (i, (key, value)) in self.0.iter().enumerate() {
            if i > 0 {
                out.push_str(style.item_separator);
            }
            pyfmt::write_str(&mut out, key, true);
            out.push_str(style.key_separator);
            pyfmt::write_value(&mut out, value, style);
        }
        out.push('}');
        out
    }
}

impl From<serde_json::Map<String, Value>> for AuditDetails {
    /// Keeps the map's iteration order.
    fn from(map: serde_json::Map<String, Value>) -> Self {
        AuditDetails(map.into_iter().collect())
    }
}

impl<K: Into<String>, V: Into<Value>> FromIterator<(K, V)> for AuditDetails {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut d = AuditDetails::new();
        for (k, v) in iter {
            d.insert(k, v);
        }
        d
    }
}

/// One event to append (`log_audit_event`'s arguments).
#[derive(Debug, Clone, Default)]
pub struct NewAuditEvent<'a> {
    pub action: &'a str,
    pub actor_username: Option<&'a str>,
    pub target_type: Option<&'a str>,
    pub target_path: Option<&'a str>,
    pub target_username: Option<&'a str>,
    pub details: Option<AuditDetails>,
}

impl<'a> NewAuditEvent<'a> {
    pub fn new(action: &'a str) -> Self {
        NewAuditEvent {
            action,
            ..Default::default()
        }
    }
    pub fn actor(mut self, actor: Option<&'a str>) -> Self {
        self.actor_username = actor;
        self
    }
    pub fn target_type(mut self, t: &'a str) -> Self {
        self.target_type = Some(t);
        self
    }
    pub fn target_path(mut self, p: &'a str) -> Self {
        self.target_path = Some(p);
        self
    }
    pub fn target_username(mut self, u: &'a str) -> Self {
        self.target_username = Some(u);
        self
    }
    pub fn details(mut self, d: AuditDetails) -> Self {
        self.details = Some(d);
        self
    }
}

/// 0.5.2 `AuditEventDict`; `details` is the JSON TEXT, not an object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditEvent {
    pub id: i64,
    pub actor_username: Option<String>,
    pub action: String,
    pub target_type: Option<String>,
    pub target_path: Option<String>,
    pub target_username: Option<String>,
    pub details: Option<String>,
    pub created_at: String,
}

impl AuditEvent {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<AuditEvent> {
        Ok(AuditEvent {
            id: row.get(0)?,
            actor_username: row.get(1)?,
            action: row.get(2)?,
            target_type: row.get(3)?,
            target_path: row.get(4)?,
            target_username: row.get(5)?,
            details: row.get(6)?,
            created_at: row.get(7)?,
        })
    }
}

/// One page of [`Database::query_audit_events`], newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditPage {
    pub events: Vec<AuditEvent>,
    /// Pass back as `cursor` for the next (older) page; `None` on the last.
    pub next_cursor: Option<String>,
    /// Every matching event, on a first page (no cursor) only.
    pub total: Option<i64>,
}

/// Filters of [`Database::query_audit_events`]; all optional, ANDed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditQuery {
    pub actor: Option<String>,
    pub actions: Vec<String>,
    pub target_types: Vec<String>,
    /// Inclusive; a date (its midnight) or an ISO date-time (zoned -> UTC).
    pub since: Option<String>,
    /// Exclusive.
    pub until: Option<String>,
    /// Case-insensitive (ASCII) substring of actor, action, target path or details.
    pub search: Option<String>,
    pub include_progress: bool,
    pub cursor: Option<String>,
    /// Clamped to `1..=200`.
    pub limit: i64,
}

impl Default for AuditQuery {
    fn default() -> Self {
        AuditQuery {
            actor: None,
            actions: Vec::new(),
            target_types: Vec::new(),
            since: None,
            until: None,
            search: None,
            include_progress: false,
            cursor: None,
            limit: AUDIT_PAGE_SIZE,
        }
    }
}

/// Distinct values of the three facet columns, ascending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditFacets {
    pub actors: Vec<String>,
    pub actions: Vec<String>,
    pub target_types: Vec<String>,
}

/// `_audit_instant`: a date or ISO date-time as the log stores it (UTC, to the second).
pub fn audit_instant(value: &str, name: &str) -> Result<String> {
    let text = pyfmt::strip(value);
    let b = text.as_bytes();
    if b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
    {
        return Ok(format!("{text} 00:00:00"));
    }
    let replaced = text.replace(['Z', 'z'], "+00:00");
    match fromisoformat(&replaced) {
        Some(parsed) => Ok(parsed
            .to_utc_naive()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()),
        None => {
            let head: String = value.chars().take(40).collect();
            Err(DbError::AuditQuery(format!(
                "{name} is not a date: {}",
                pyfmt::repr_str(&head)
            )))
        }
    }
}

/// `_audit_cursor`: base64url (unpadded) of `json.dumps([created_at, id])` compact.
pub fn audit_cursor(created_at: &str, id: i64) -> String {
    let raw = format!("[{},{id}]", pyfmt::dumps_str(created_at, true));
    URL_SAFE_NO_PAD.encode(raw.as_bytes())
}

/// `_audit_uncursor`: the `(created_at, id)` a cursor carries.
pub fn audit_uncursor(cursor: &str) -> Result<(String, i64)> {
    let bad = || DbError::AuditQuery("cursor is not one this server gave out".into());
    if !cursor.is_ascii() {
        return Err(bad());
    }
    // Python's non-strict b64decode: urlsafe alphabet mapped, other characters dropped.
    let cleaned: String = cursor
        .chars()
        .filter_map(|c| match c {
            '-' | '+' => Some('-'),
            '_' | '/' => Some('_'),
            c if c.is_ascii_alphanumeric() => Some(c),
            _ => None,
        })
        .collect();
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::URL_SAFE,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    let bytes = engine.decode(cleaned.as_bytes()).map_err(|_| bad())?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    let Value::Array(items) = value else {
        return Err(bad());
    };
    let [created_at, id] = items.as_slice() else {
        return Err(bad());
    };
    let Value::String(created_at) = created_at else {
        return Err(bad());
    };
    let id = match id {
        Value::Number(n) if !n.is_f64() => n.as_i64().ok_or_else(bad)?,
        Value::Bool(b) => i64::from(*b),
        _ => return Err(bad()),
    };
    Ok((created_at.clone(), id))
}

/// `_like_pattern`: `%term%` with `\`, `%`, `_` escaped for `ESCAPE '\'`.
pub fn like_pattern(term: &str) -> String {
    let escaped = term
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

impl Database {
    /// Append an audit event (and, at most hourly, prune old ones); returns its id.
    pub fn log_audit_event(&self, event: &NewAuditEvent<'_>) -> Result<i64> {
        self.write(|conn| self.log_audit_in(conn, event))
    }

    /// `log_audit_event` inside an open write transaction.
    pub(crate) fn log_audit_in(&self, conn: &Connection, event: &NewAuditEvent<'_>) -> Result<i64> {
        let details = event.details.as_ref().map(AuditDetails::to_json);
        let now = Instant::now();
        let should_prune = match *self.last_audit_prune.lock() {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= AUDIT_PRUNE_INTERVAL,
        };
        conn.prepare_cached(
            "INSERT INTO audit_logs (actor_username, action, target_type, target_path, \
             target_username, details) VALUES (?, ?, ?, ?, ?, ?)",
        )?
        .execute(rusqlite::params![
            event.actor_username,
            event.action,
            event.target_type,
            event.target_path,
            event.target_username,
            details
        ])?;
        let id = conn.last_insert_rowid();
        if should_prune {
            conn.execute(
                "DELETE FROM audit_logs WHERE created_at < datetime('now', ?)",
                [format!("-{AUDIT_RETENTION_DAYS} days")],
            )?;
            *self.last_audit_prune.lock() = Some(now);
        }
        Ok(id)
    }

    /// Legacy reader: newest first, optionally one actor's; `limit` clamped to 1..=1000.
    pub fn list_audit_events(&self, limit: i64, actor: Option<&str>) -> Result<Vec<AuditEvent>> {
        let limit = limit.clamp(1, 1000);
        self.read(|conn| {
            let events = match actor.filter(|a| !a.is_empty()) {
                Some(actor) => conn
                    .prepare_cached(&format!(
                        "SELECT {AUDIT_COLUMNS} FROM audit_logs WHERE actor_username = ? \
                         ORDER BY created_at DESC, id DESC LIMIT ?"
                    ))?
                    .query_map(rusqlite::params![actor, limit], AuditEvent::from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                None => conn
                    .prepare_cached(&format!(
                        "SELECT {AUDIT_COLUMNS} FROM audit_logs \
                         ORDER BY created_at DESC, id DESC LIMIT ?"
                    ))?
                    .query_map([limit], AuditEvent::from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            };
            Ok(events)
        })
    }

    /// One page of audit events, newest first, every filter applied in SQL. Keyset paging
    /// by `(created_at, id)`: events logged between page loads never shift a later page.
    pub fn query_audit_events(&self, q: &AuditQuery) -> Result<AuditPage> {
        let size = q.limit.clamp(1, AUDIT_PAGE_MAX);
        let mut clauses: Vec<String> = Vec::new();
        let mut params: Vec<Box<dyn ToSql>> = Vec::new();
        let actor = q.actor.as_deref().filter(|a| !a.is_empty());
        if let Some(actor) = actor {
            clauses.push("actor_username = ?".into());
            params.push(Box::new(actor.to_string()));
        }
        let actions: Vec<&String> = q.actions.iter().filter(|a| !a.is_empty()).collect();
        if !actions.is_empty() {
            clauses.push(format!("action IN ({})", placeholders(actions.len())));
            params.extend(
                actions
                    .iter()
                    .map(|a| Box::new((*a).clone()) as Box<dyn ToSql>),
            );
        }
        let types: Vec<&String> = q.target_types.iter().filter(|t| !t.is_empty()).collect();
        if !types.is_empty() {
            clauses.push(format!("target_type IN ({})", placeholders(types.len())));
            params.extend(
                types
                    .iter()
                    .map(|t| Box::new((*t).clone()) as Box<dyn ToSql>),
            );
        } else if !q.include_progress {
            clauses.push("(target_type IS NULL OR target_type <> ?)".into());
            params.push(Box::new(AUDIT_PROGRESS_TYPE));
        }
        if let Some(since) = q.since.as_deref().filter(|s| !s.is_empty()) {
            clauses.push("created_at >= ?".into());
            params.push(Box::new(audit_instant(since, "since")?));
        }
        if let Some(until) = q.until.as_deref().filter(|s| !s.is_empty()) {
            clauses.push("created_at < ?".into());
            params.push(Box::new(audit_instant(until, "until")?));
        }
        let term = pyfmt::strip(q.search.as_deref().unwrap_or(""));
        if !term.is_empty() {
            let mut patterns = vec![like_pattern(term)];
            // Details are stored ASCII-escaped, so search that spelling too.
            let quoted = pyfmt::dumps_str(term, true);
            let ascii_form = &quoted[1..quoted.len() - 1];
            if ascii_form != term {
                patterns.push(like_pattern(ascii_form));
            }
            let mut ors: Vec<String> = ["actor_username", "action", "target_path", "details"]
                .iter()
                .map(|c| format!("{c} LIKE ? ESCAPE '\\'"))
                .collect();
            ors.extend(std::iter::repeat_n(
                "details LIKE ? ESCAPE '\\'".to_string(),
                patterns.len() - 1,
            ));
            clauses.push(format!("({})", ors.join(" OR ")));
            for _ in 0..4 {
                params.push(Box::new(patterns[0].clone()));
            }
            params.extend(
                patterns[1..]
                    .iter()
                    .map(|p| Box::new(p.clone()) as Box<dyn ToSql>),
            );
        }
        let filter_count = clauses.len();
        let filter_param_count = params.len();
        let has_cursor = q.cursor.as_deref().is_some_and(|c| !c.is_empty());
        if let Some(cursor) = q.cursor.as_deref().filter(|c| !c.is_empty()) {
            let (created_at, id) = audit_uncursor(cursor)?;
            clauses.push("(created_at, id) < (?, ?)".into());
            params.push(Box::new(created_at));
            params.push(Box::new(id));
        }
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        // With no actor/action/type filter the page is a walk down the created_at index;
        // the planner would otherwise pick a skip-scan plus a sort (0.5.2, measured).
        let index = if actor.is_some() || !actions.is_empty() || !types.is_empty() {
            ""
        } else {
            "INDEXED BY idx_audit_created_at "
        };
        let page_sql = format!(
            "SELECT {AUDIT_COLUMNS} FROM audit_logs {index}{where_sql} \
             ORDER BY created_at DESC, id DESC LIMIT ?"
        );
        let count_sql = if filter_count == 0 {
            "SELECT COUNT(*) FROM audit_logs ".to_string()
        } else {
            format!(
                "SELECT COUNT(*) FROM audit_logs WHERE {}",
                clauses[..filter_count].join(" AND ")
            )
        };
        let limit = size + 1;
        let (mut rows, total) = self.read(|conn| {
            let mut bound: Vec<&dyn ToSql> = params.iter().map(|p| p.as_ref()).collect();
            bound.push(&limit);
            let rows = conn
                .prepare_cached(&page_sql)?
                .query_map(params_from_iter(bound), AuditEvent::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let total = if has_cursor {
                None
            } else {
                let bound = params[..filter_param_count].iter().map(|p| p.as_ref());
                Some(
                    conn.prepare_cached(&count_sql)?
                        .query_row(params_from_iter(bound), |r| r.get(0))?,
                )
            };
            Ok((rows, total))
        })?;
        let more = rows.len() as i64 > size;
        rows.truncate(size as usize);
        let next_cursor = match rows.last() {
            Some(last) if more => Some(audit_cursor(&last.created_at, last.id)),
            _ => None,
        };
        Ok(AuditPage {
            events: rows,
            next_cursor,
            total,
        })
    }

    /// Distinct actors, actions and target types in the log, ascending (BINARY), read by
    /// a loose index scan (one seek per distinct value).
    pub fn audit_facets(&self) -> Result<AuditFacets> {
        self.read(|conn| {
            let column_values = |column: &str| -> Result<Vec<String>> {
                let sql = format!(
                    "WITH RECURSIVE seen(value) AS (\
                     SELECT MIN({column}) FROM audit_logs \
                     UNION ALL \
                     SELECT (SELECT MIN({column}) FROM audit_logs WHERE {column} > seen.value) \
                     FROM seen WHERE seen.value IS NOT NULL\
                     ) SELECT value FROM seen WHERE value IS NOT NULL"
                );
                let values = conn
                    .prepare_cached(&sql)?
                    .query_map([], |r| crate::series::opt_text(r, 0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(values.into_iter().flatten().collect())
            };
            Ok(AuditFacets {
                actors: column_values("actor_username")?,
                actions: column_values("action")?,
                target_types: column_values("target_type")?,
            })
        })
    }

    /// Test hook: pretend the last prune happened `ago` (or never, with `None`).
    #[doc(hidden)]
    pub fn set_last_audit_prune_ago(&self, ago: Option<Duration>) {
        *self.last_audit_prune.lock() =
            ago.map(|d| Instant::now().checked_sub(d).unwrap_or_else(Instant::now));
    }
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_db;
    use serde_json::json;

    type SeedRow<'a> = (
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        &'a str,
    );

    fn seed(db: &Database) {
        let rows: &[SeedRow] = &[
            (
                "upload",
                Some("alice"),
                Some("library"),
                Some("/mokuro-reader/S/V1.cbz"),
                Some(r#"{"existed_before":false}"#),
                "2026-09-01 10:00:00",
            ),
            (
                "edit",
                Some("bob"),
                Some("progress"),
                Some("/mokuro-reader/volume-data.json"),
                None,
                "2026-09-02 10:00:00",
            ),
            (
                "delete",
                Some("alice"),
                Some("library"),
                Some("/mokuro-reader/S/V2.cbz"),
                Some(r#"{"name":"\u6f22\u5b57"}"#),
                "2026-09-03 10:00:00",
            ),
            (
                "admin_create_user",
                Some("root"),
                Some("user"),
                None,
                Some(r#"{"role":"100%_sure"}"#),
                "2026-09-04 10:00:00",
            ),
            ("mkdir", None, None, Some("/x"), None, "2026-09-05 10:00:00"),
        ];
        db.with_writer_connection(|c| {
            for (action, actor, tt, tp, details, at) in rows {
                c.execute(
                    "INSERT INTO audit_logs (action, actor_username, target_type, target_path, details, created_at) VALUES (?,?,?,?,?,?)",
                    rusqlite::params![action, actor, tt, tp, details, at],
                )
                .unwrap();
            }
        });
    }

    fn ids(page: &AuditPage) -> Vec<i64> {
        page.events.iter().map(|e| e.id).collect()
    }

    fn q() -> AuditQuery {
        AuditQuery::default()
    }

    #[test]
    fn details_are_python_compact_ascii_in_insertion_order() {
        let d = AuditDetails::new()
            .with("role", "uploader")
            .with("expires", "7d")
            .with("name", "\u{6f22}\u{5b57} \u{1F600}")
            .with("f", 1e-5)
            .with("ok", true)
            .with("none", Value::Null)
            .with("q", "a\"b\\c\u{7f}");
        assert_eq!(
            d.to_json(),
            r#"{"role":"uploader","expires":"7d","name":"\u6f22\u5b57 \ud83d\ude00","f":1e-05,"ok":true,"none":null,"q":"a\"b\\c\u007f"}"#
        );
    }

    #[test]
    fn filters() {
        let (_dir, db) = temp_db();
        seed(&db);
        assert_eq!(
            ids(&db.query_audit_events(&q()).unwrap()),
            [5, 4, 3, 1],
            "progress hidden"
        );
        let p = db
            .query_audit_events(&AuditQuery {
                include_progress: true,
                ..q()
            })
            .unwrap();
        assert_eq!(ids(&p), [5, 4, 3, 2, 1]);
        assert_eq!(p.total, Some(5));
        let p = db
            .query_audit_events(&AuditQuery {
                target_types: vec!["progress".into()],
                ..q()
            })
            .unwrap();
        assert_eq!(ids(&p), [2]);
        let p = db
            .query_audit_events(&AuditQuery {
                actor: Some("alice".into()),
                ..q()
            })
            .unwrap();
        assert_eq!(ids(&p), [3, 1]);
        let p = db
            .query_audit_events(&AuditQuery {
                actions: vec!["upload".into(), "".into(), "mkdir".into()],
                ..q()
            })
            .unwrap();
        assert_eq!(ids(&p), [5, 1]);
        let p = db
            .query_audit_events(&AuditQuery {
                since: Some("2026-09-02".into()),
                until: Some("2026-09-04T10:00:00Z".into()),
                ..q()
            })
            .unwrap();
        assert_eq!(ids(&p), [3]);
        let p = db
            .query_audit_events(&AuditQuery {
                until: Some("2026-09-01T16:00:00+05:00".into()),
                ..q()
            })
            .unwrap();
        assert_eq!(ids(&p), [1], "zoned instants are converted to UTC");
    }

    #[test]
    fn search() {
        let (_dir, db) = temp_db();
        seed(&db);
        let s = |t: &str| {
            ids(&db
                .query_audit_events(&AuditQuery {
                    search: Some(t.into()),
                    ..q()
                })
                .unwrap())
        };
        assert_eq!(s("ALICE"), [3, 1]);
        assert_eq!(s("v2.CBZ"), [3]);
        assert_eq!(s("existed_BEFORE"), [1]);
        assert_eq!(
            s("\u{6f22}\u{5b57}"),
            [3],
            "non-ASCII is found in its escaped spelling"
        );
        assert_eq!(s("100%_"), [4]);
        assert_eq!(s("%"), [4], "wildcards are literal");
        assert_eq!(s("_"), [4, 1]);
        assert_eq!(s("  mkdir "), [5]);
        assert_eq!(s("user"), [4], "target_type is not searched, action is");
    }

    #[test]
    fn bad_dates_and_cursors() {
        let (_dir, db) = temp_db();
        let err = db
            .query_audit_events(&AuditQuery {
                since: Some("yesterday".into()),
                ..q()
            })
            .unwrap_err();
        assert!(matches!(err, DbError::AuditQuery(_)));
        assert_eq!(err.to_string(), "since is not a date: 'yesterday'");
        let err = db
            .query_audit_events(&AuditQuery {
                cursor: Some("!!!".into()),
                ..q()
            })
            .unwrap_err();
        assert_eq!(err.to_string(), "cursor is not one this server gave out");
        for bad in [
            r#"["a"]"#,
            r#"[1,2]"#,
            r#"["a",1.0]"#,
            r#"{"a":1,"b":2}"#,
            r#""ab""#,
        ] {
            let c = URL_SAFE_NO_PAD.encode(bad);
            assert!(audit_uncursor(&c).is_err(), "{bad}");
        }
        assert_eq!(
            audit_uncursor(&URL_SAFE_NO_PAD.encode(r#"["a",true]"#)).unwrap(),
            ("a".into(), 1)
        );
    }

    #[test]
    fn cursor_matches_python() {
        // Python: Database._audit_cursor("2026-10-01 11:02:03", 42)
        assert_eq!(
            audit_cursor("2026-10-01 11:02:03", 42),
            "WyIyMDI2LTEwLTAxIDExOjAyOjAzIiw0Ml0"
        );
        assert_eq!(
            audit_uncursor("WyIyMDI2LTEwLTAxIDExOjAyOjAzIiw0Ml0").unwrap(),
            ("2026-10-01 11:02:03".to_string(), 42)
        );
        assert_eq!(
            audit_uncursor("WyIyMDI2LTEwLTAxIDExOjAyOjAzIiw0Ml0=")
                .unwrap()
                .1,
            42,
            "padding tolerated"
        );
    }

    #[test]
    fn keyset_pages_have_no_gaps_or_duplicates_under_inserts() {
        let (_dir, db) = temp_db();
        for i in 0..25 {
            db.log_audit_event(&NewAuditEvent::new("upload").target_path(&format!("/p{i}")))
                .unwrap();
        }
        let mut seen = Vec::new();
        let mut cursor = None;
        let mut pages = 0;
        loop {
            let page = db
                .query_audit_events(&AuditQuery {
                    cursor: cursor.clone(),
                    limit: 10,
                    ..q()
                })
                .unwrap();
            assert_eq!(page.total.is_some(), cursor.is_none());
            seen.extend(ids(&page));
            pages += 1;
            // Inserted between page loads: newer than every cursor.
            db.log_audit_event(&NewAuditEvent::new("upload")).unwrap();
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        assert_eq!(pages, 3);
        assert_eq!(seen, (1..=25).rev().collect::<Vec<i64>>());
        let p = db
            .query_audit_events(&AuditQuery { limit: 0, ..q() })
            .unwrap();
        assert_eq!(p.events.len(), 1);
        let p = db
            .query_audit_events(&AuditQuery {
                limit: 10_000,
                ..q()
            })
            .unwrap();
        assert_eq!(p.events.len(), 28);
    }

    #[test]
    fn facets() {
        let (_dir, db) = temp_db();
        seed(&db);
        let f = db.audit_facets().unwrap();
        assert_eq!(f.actors, ["alice", "bob", "root"]);
        assert_eq!(
            f.actions,
            ["admin_create_user", "delete", "edit", "mkdir", "upload"]
        );
        assert_eq!(f.target_types, ["library", "progress", "user"]);
        assert_eq!(
            serde_json::to_value(&f).unwrap(),
            json!({"actors": ["alice","bob","root"], "actions": ["admin_create_user","delete","edit","mkdir","upload"], "target_types": ["library","progress","user"]})
        );
    }

    #[test]
    fn pruning_is_throttled() {
        let (_dir, db) = temp_db();
        let old = |db: &Database| {
            db.with_writer_connection(|c| {
                c.execute("INSERT INTO audit_logs (action, created_at) VALUES ('old', datetime('now', '-31 days'))", [])
                    .unwrap();
            })
        };
        let count = |db: &Database| -> i64 {
            db.with_writer_connection(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM audit_logs WHERE action='old'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap()
        };
        db.set_last_audit_prune_ago(None);
        old(&db);
        db.log_audit_event(&NewAuditEvent::new("x")).unwrap();
        assert_eq!(count(&db), 0, "the first event prunes");
        old(&db);
        db.log_audit_event(&NewAuditEvent::new("x")).unwrap();
        assert_eq!(count(&db), 1, "throttled within the hour");
        db.set_last_audit_prune_ago(Some(Duration::from_secs(3601)));
        db.log_audit_event(&NewAuditEvent::new("x")).unwrap();
        assert_eq!(count(&db), 0, "runs again after the interval");
    }

    #[test]
    fn legacy_list() {
        let (_dir, db) = temp_db();
        seed(&db);
        let all = db.list_audit_events(200, None).unwrap();
        assert_eq!(
            all.iter().map(|e| e.id).collect::<Vec<_>>(),
            [5, 4, 3, 2, 1]
        );
        assert_eq!(db.list_audit_events(0, Some("alice")).unwrap().len(), 1);
    }

    #[test]
    fn instants() {
        assert_eq!(
            audit_instant(" 2026-10-01 ", "since").unwrap(),
            "2026-10-01 00:00:00"
        );
        assert_eq!(
            audit_instant("2026-10-01T10:11:12.9", "since").unwrap(),
            "2026-10-01 10:11:12"
        );
        assert_eq!(
            audit_instant("2026-10-01T00:30:00+01:00", "x").unwrap(),
            "2026-09-30 23:30:00"
        );
        assert_eq!(
            audit_instant("2026-10-01t10:00z", "x").unwrap(),
            "2026-10-01 10:00:00"
        );
        assert_eq!(
            audit_instant(&"x".repeat(50), "until")
                .unwrap_err()
                .to_string(),
            format!("until is not a date: '{}'", "x".repeat(40))
        );
    }
}
