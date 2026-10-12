//! `GET /api/audit` (spec db-auth-admin §18.5): one page of the audit log, newest first,
//! with `total` and `facets` on a first page only.

use super::{AdminState, ApiRequest, blocking, error, internal, ok, parse_qs, query_one};
use axum::response::Response;
use bunko_db::{AUDIT_PAGE_SIZE, AuditQuery, DbError};
use serde_json::{Map, Value};

/// Repeated and/or comma-separated values, stripped, empties dropped (0.5.2 `many`).
fn many(q: &[(String, String)], name: &str) -> Vec<String> {
    q.iter()
        .filter(|(k, _)| k == name)
        .flat_map(|(_, v)| v.split(','))
        .map(|p| bunko_db::pyfmt::strip(p).to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Python `int()` of an already-stripped string: optional sign, digits, `_` separators.
fn py_int(text: &str) -> Option<i64> {
    let t = text.trim();
    let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return None;
    }
    t.replace('_', "").parse().ok()
}

pub(super) async fn list(s: &AdminState, req: &ApiRequest) -> Response {
    let q = parse_qs(&req.query);
    let limit = match query_one(&q, "limit") {
        None => AUDIT_PAGE_SIZE,
        Some(text) => match py_int(&text) {
            Some(n) => n,
            None => return error(400, "limit is not a number"),
        },
    };
    let cursor = query_one(&q, "cursor");
    let first_page = cursor.is_none();
    let query = AuditQuery {
        actor: query_one(&q, "actor"),
        actions: many(&q, "action"),
        target_types: many(&q, "target_type"),
        since: query_one(&q, "since"),
        until: query_one(&q, "until"),
        search: query_one(&q, "q"),
        include_progress: query_one(&q, "include_progress")
            .is_some_and(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes")),
        cursor,
        limit,
    };
    let db = s.db();
    let result = blocking(move || -> Result<Value, DbError> {
        let page = db.query_audit_events(&query)?;
        let mut body = Map::new();
        body.insert(
            "events".into(),
            serde_json::to_value(&page.events).unwrap_or_default(),
        );
        body.insert(
            "next_cursor".into(),
            serde_json::to_value(&page.next_cursor).unwrap_or_default(),
        );
        body.insert(
            "total".into(),
            serde_json::to_value(page.total).unwrap_or_default(),
        );
        if first_page {
            body.insert(
                "facets".into(),
                serde_json::to_value(db.audit_facets()?).unwrap_or_default(),
            );
        }
        Ok(Value::Object(body))
    })
    .await;
    match result {
        Ok(Ok(body)) => ok(body),
        Ok(Err(DbError::AuditQuery(msg))) => error(400, msg),
        Ok(Err(e)) => internal("database error", e),
        Err(r) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::py_int;

    #[test]
    fn python_int() {
        assert_eq!(py_int("50"), Some(50));
        assert_eq!(py_int("+5"), Some(5));
        assert_eq!(py_int("-3"), Some(-3));
        assert_eq!(py_int("1_000"), Some(1000));
        assert_eq!(py_int("1.5"), None);
        assert_eq!(py_int("abc"), None);
        assert_eq!(py_int("_1"), None);
    }
}
