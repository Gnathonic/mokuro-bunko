//! `GET /api/status` and the tunnel / DynDNS wrappers (spec db-auth-admin §18.7, §18.9).

use super::{AdminState, blocking, error, internal, ok};
use axum::response::Response;
use bunko_db::UserStatus;
use serde_json::{Map, Value, json};

/// `shutil.disk_usage`: (total, used, free); zeros when the path cannot be asked.
fn disk_usage(path: &std::path::Path) -> (u64, u64, u64) {
    match fs4::statvfs(path) {
        Ok(st) => {
            let total = st.total_space();
            (
                total,
                total.saturating_sub(st.free_space()),
                st.available_space(),
            )
        }
        Err(_) => (0, 0, 0),
    }
}

pub(super) async fn status(s: &AdminState) -> Response {
    let uptime = s.core().started_at.elapsed().as_secs_f64();
    let (host, port, base, library) = {
        let c = s.core().config.read();
        (
            c.server.host.clone(),
            c.server.port,
            c.storage.base_path.clone(),
            c.storage.library_path(),
        )
    };
    let db = s.db();
    let result = blocking(move || -> Result<Value, bunko_db::DbError> {
        let users = db.list_users(None)?;
        let user_count = users
            .iter()
            .filter(|u| u.status != UserStatus::Deleted)
            .count();
        let storage_path = std::path::absolute(&base).unwrap_or(base);
        let (disk_total, disk_used, disk_free) = disk_usage(&storage_path);
        // Series folders: the immediate directories of the library (0.5.2 counts these
        // as "volumes").
        let volume_count = std::fs::read_dir(&library)
            .map(|it| {
                it.filter_map(Result::ok)
                    .filter(|e| e.path().is_dir())
                    .count()
            })
            .unwrap_or(0);
        Ok(json!({
            "uptime": uptime,
            "host": host,
            "port": port,
            "storage_path": storage_path.to_string_lossy(),
            "disk_total": disk_total,
            "disk_used": disk_used,
            "disk_free": disk_free,
            "user_count": user_count,
            "volume_count": volume_count,
            "stats": {},
        }))
    })
    .await;
    match result {
        Ok(Ok(body)) => ok(body),
        Ok(Err(e)) => internal("database error", e),
        Err(r) => r,
    }
}

/// `{"success": true, **status}`.
fn success_with(status: Value) -> Value {
    let mut m = Map::new();
    m.insert("success".into(), json!(true));
    if let Value::Object(st) = status {
        m.extend(st);
    }
    Value::Object(m)
}

pub(super) fn tunnel_status(s: &AdminState) -> Response {
    match &s.deps.tunnel {
        None => ok(json!({"running": false, "url": null, "available": false})),
        Some(t) => ok(t.status()),
    }
}

pub(super) fn tunnel_start(s: &AdminState) -> Response {
    let Some(t) = &s.deps.tunnel else {
        return error(500, "Tunnel service not available");
    };
    let (port, https) = {
        let c = s.core().config.read();
        (c.server.port, c.ssl.enabled)
    };
    match t.start(port, https) {
        Ok(()) => ok(success_with(t.status())),
        Err(e) => error(400, e),
    }
}

pub(super) async fn tunnel_stop(s: &AdminState) -> Response {
    let Some(t) = &s.deps.tunnel else {
        return error(500, "Tunnel service not available");
    };
    t.stop().await;
    ok(success_with(t.status()))
}

pub(super) fn dyndns_status(s: &AdminState) -> Response {
    match &s.deps.dyndns {
        None => ok(json!({"enabled": false, "running": false})),
        Some(d) => ok(d.status()),
    }
}

pub(super) fn dyndns_start(s: &AdminState) -> Response {
    let Some(d) = &s.deps.dyndns else {
        return error(500, "DynDNS service not available");
    };
    d.start();
    ok(success_with(d.status()))
}

pub(super) fn dyndns_stop(s: &AdminState) -> Response {
    let Some(d) = &s.deps.dyndns else {
        return error(500, "DynDNS service not available");
    };
    d.stop();
    ok(success_with(d.status()))
}

pub(super) async fn dyndns_test(s: &AdminState) -> Response {
    let Some(d) = &s.deps.dyndns else {
        return error(500, "DynDNS service not available");
    };
    ok(d.update_now().await)
}
