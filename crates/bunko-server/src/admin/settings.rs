//! `GET /api/settings` and the `PUT /api/settings/*` editors (spec db-auth-admin §18.6,
//! ocr-generations-bench §8.1-8.2).
//!
//! Each editor mutates the LIVE config (`Core.config`, read per request by the modules
//! that use it, so registration, CORS, catalog and queue changes apply at once) and saves
//! it, under one lock. As in 0.5.2 a validation error part-way through a body returns
//! at once, leaving the keys already applied in memory but unsaved.

use super::{AdminState, ApiRequest, blocking, error, json_response, ok, save_config};
use axum::response::Response;
use bunko_core::config::{
    DEFAULT_ROLES, DYNDNS_PROVIDERS, QUEUE_DISPLAY_LEVELS, REGISTRATION_MODES,
};
use bunko_db::pyfmt::truthy;
use serde_json::{Map, Value, json};

const TOKEN_MASK: &str = "****";

/// Python `int(value)` of a decoded JSON value (floats truncate, numeric strings parse).
pub(crate) fn py_int_value(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(*b as i64),
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite())
                .map(|f| f.trunc() as i64)
        }),
        Value::String(s) => {
            let t = bunko_db::pyfmt::strip(s).replace('_', "");
            t.parse().ok()
        }
        _ => None,
    }
}

/// The Python list repr 0.5.2 prints in "Must be one of" messages.
fn py_list(items: &[&str]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|i| format!("'{i}'"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

pub(super) async fn get(s: &AdminState) -> Response {
    let cfg = s.core().config.read().clone();
    let ocr = s.ocr();
    let runtime_cfg = cfg.clone();
    let runtime = match blocking(move || ocr.runtime_status(&runtime_cfg)).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let mut data = cfg.to_value();
    if let Some(Value::Object(d)) = data.get_mut("dyndns")
        && d.get("token").is_some_and(truthy)
    {
        d.insert("token".into(), json!(TOKEN_MASK));
    }
    if let Value::Object(m) = &mut data {
        m.entry("update").or_insert_with(|| {
            json!({"check": cfg.update.check, "channel": cfg.update.channel, "manifest_url": cfg.update.manifest_url})
        });
        m.insert("ocr_runtime".into(), runtime);
        // New in 0.7: migration notes (removed engines/detectors), for the admin to read.
        m.insert("config_warnings".into(), json!(cfg.warnings));
    }
    ok(data)
}

pub(super) async fn registration(s: &AdminState, req: &ApiRequest) -> Response {
    let data = match req.json() {
        Ok(d) => d.clone(),
        Err(r) => return r,
    };
    let s2 = s.clone();
    blocking(move || {
        let _guard = s2.config_lock.lock();
        {
            let mut cfg = s2.core().config.write();
            let reg = &mut cfg.registration;
            if let Some(v) = data.get("mode") {
                match v.as_str().filter(|m| REGISTRATION_MODES.contains(m)) {
                    Some(m) => reg.mode = m.to_string(),
                    None => {
                        return error(
                            400,
                            format!(
                                "Invalid mode. Must be one of: {}",
                                py_list(REGISTRATION_MODES)
                            ),
                        );
                    }
                }
            }
            if let Some(v) = data.get("default_role") {
                match v.as_str().filter(|r| DEFAULT_ROLES.contains(r)) {
                    Some(r) => reg.default_role = r.to_string(),
                    None => {
                        return error(
                            400,
                            format!(
                                "Invalid default_role. Must be one of: {}",
                                py_list(DEFAULT_ROLES)
                            ),
                        );
                    }
                }
            }
            if let Some(v) = data.get("allow_anonymous_browse") {
                reg.allow_anonymous_browse = truthy(v);
            }
            if let Some(v) = data.get("allow_anonymous_download") {
                reg.allow_anonymous_download = truthy(v);
            }
            // Older admin clients: applied last, so it wins.
            if let Some(v) = data.get("require_login") {
                let require = truthy(v);
                reg.allow_anonymous_browse = !require;
                reg.allow_anonymous_download = !require;
            }
            reg.require_login = !reg.allow_anonymous_browse && !reg.allow_anonymous_download;
        }
        if let Err(r) = save_config(&s2) {
            return r;
        }
        let cfg = s2.core().config.read();
        let reg = &cfg.registration;
        ok(json!({
            "success": true,
            "registration": {
                "mode": reg.mode,
                "default_role": reg.default_role,
                "allow_anonymous_browse": reg.allow_anonymous_browse,
                "allow_anonymous_download": reg.allow_anonymous_download,
                "require_login": !reg.allow_anonymous_browse && !reg.allow_anonymous_download,
            }
        }))
    })
    .await
    .unwrap_or_else(|r| r)
}

pub(super) async fn cors(s: &AdminState, req: &ApiRequest) -> Response {
    let data = match req.json() {
        Ok(d) => d.clone(),
        Err(r) => return r,
    };
    let s2 = s.clone();
    blocking(move || {
        let _guard = s2.config_lock.lock();
        {
            let mut cfg = s2.core().config.write();
            if let Some(v) = data.get("enabled") {
                cfg.cors.enabled = truthy(v);
            }
            if let Some(v) = data.get("allowed_origins") {
                // 0.5.2 left the elements unchecked; a non-string origin can never match,
                // and the config holds strings, so it is refused like a non-list.
                let list: Option<Vec<String>> =
                    v.as_array().and_then(|a| a.iter().map(|o| o.as_str().map(str::to_string)).collect());
                match list {
                    Some(l) => cfg.cors.allowed_origins = l,
                    None => return error(400, "allowed_origins must be a list"),
                }
            }
        }
        if let Err(r) = save_config(&s2) {
            return r;
        }
        let cfg = s2.core().config.read();
        ok(json!({"success": true, "cors": {"enabled": cfg.cors.enabled, "allowed_origins": cfg.cors.allowed_origins}}))
    })
    .await
    .unwrap_or_else(|r| r)
}

pub(super) async fn catalog(s: &AdminState, req: &ApiRequest) -> Response {
    let data = match req.json() {
        Ok(d) => d.clone(),
        Err(r) => return r,
    };
    let s2 = s.clone();
    blocking(move || {
        let _guard = s2.config_lock.lock();
        {
            let mut cfg = s2.core().config.write();
            if let Some(v) = data.get("enabled") {
                cfg.catalog.enabled = truthy(v);
            }
            if let Some(v) = data.get("reader_url") {
                let Some(text) = v.as_str() else {
                    // 0.5.2: AttributeError → 500; spec §18.6: 400.
                    return error(400, "reader_url must be a string");
                };
                let url = bunko_db::pyfmt::strip(text).trim_end_matches('/');
                if !url.is_empty() {
                    cfg.catalog.reader_url = url.to_string();
                }
            }
            if let Some(v) = data.get("use_as_homepage") {
                cfg.catalog.use_as_homepage = truthy(v);
            }
        }
        if let Err(r) = save_config(&s2) {
            return r;
        }
        let cfg = s2.core().config.read();
        ok(json!({"success": true, "catalog": {
            "enabled": cfg.catalog.enabled,
            "reader_url": cfg.catalog.reader_url,
            "use_as_homepage": cfg.catalog.use_as_homepage,
        }}))
    })
    .await
    .unwrap_or_else(|r| r)
}

pub(super) async fn queue(s: &AdminState, req: &ApiRequest) -> Response {
    let data = match req.json() {
        Ok(d) => d.clone(),
        Err(r) => return r,
    };
    let display = match data.get("display") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_str().filter(|d| QUEUE_DISPLAY_LEVELS.contains(d)) {
            Some(d) => Some(d.to_string()),
            None => {
                return json_response(
                    400,
                    &json!({"error": format!("display must be one of: {}", QUEUE_DISPLAY_LEVELS.join(", ")), "field": "display"}),
                );
            }
        },
    };
    let s2 = s.clone();
    blocking(move || {
        {
            let _guard = s2.config_lock.lock();
            {
                let mut cfg = s2.core().config.write();
                if let Some(v) = data.get("show_in_nav") {
                    cfg.queue.show_in_nav = truthy(v);
                }
                if let Some(v) = data.get("public_access") {
                    cfg.queue.public_access = truthy(v);
                }
                if let Some(d) = display {
                    cfg.queue.display = d;
                }
            }
            if let Err(r) = save_config(&s2) {
                return r;
            }
        }
        // The queue page's fingerprint must move so open pages re-render at the new level.
        s2.ocr().queue_changed();
        let cfg = s2.core().config.read();
        ok(json!({"success": true, "queue": {
            "show_in_nav": cfg.queue.show_in_nav,
            "public_access": cfg.queue.public_access,
            "display": cfg.queue.display,
        }}))
    })
    .await
    .unwrap_or_else(|r| r)
}

/// The apply outcome when nothing (or no OCR control) applied the change.
pub(crate) fn default_outcome(changed: bool) -> Value {
    json!({"applied": false, "installing": false, "restart_required": changed, "reason": ""})
}

pub(super) async fn ocr(s: &AdminState, req: &ApiRequest) -> Response {
    let data = match req.json() {
        Ok(d) => d.clone(),
        Err(r) => return r,
    };
    let s2 = s.clone();
    blocking(move || {
        let ocr = s2.ocr();
        let (snapshot, outcome) = {
            let _guard = s2.config_lock.lock();
            if data.contains_key("backend") {
                return error(400, "OCR backend is launch-only. Use CLI flags/config file to change it.");
            }
            if data.contains_key("char_map") {
                return error(
                    400,
                    "char_map was removed with the character-map system (no per-character placement mode produced \
                     output worth using; readers lay characters on a uniform grid) -- delete the key",
                );
            }
            let moved: Vec<&str> = ["engines", "detector", "patch_budget"].into_iter().filter(|k| data.contains_key(*k)).collect();
            if !moved.is_empty() {
                return error(400, format!("{} moved into the generations list; PUT them to /api/ocr/generations", moved.join(", ")));
            }
            let mut changed = false;
            if let Some(v) = data.get("poll_interval") {
                match py_int_value(v).filter(|n| *n >= 1 && *n <= u32::MAX as i64) {
                    Some(n) => {
                        let mut cfg = s2.core().config.write();
                        changed = n as u32 != cfg.ocr.poll_interval;
                        cfg.ocr.poll_interval = n as u32;
                    }
                    None => return error(400, "poll_interval must be a positive integer"),
                }
            }
            if let Err(r) = save_config(&s2) {
                return r;
            }
            let snapshot = s2.core().config.read().clone();
            let outcome = if changed { ocr.apply(&snapshot).unwrap_or_else(|| default_outcome(true)) } else { default_outcome(false) };
            (snapshot, outcome)
        };
        let mut body = Map::new();
        body.insert("success".into(), json!(true));
        body.insert(
            "ocr".into(),
            json!({"backend": snapshot.ocr.backend, "poll_interval": snapshot.ocr.poll_interval, "concurrency": snapshot.ocr.concurrency}),
        );
        merge(&mut body, outcome);
        body.insert("ocr_runtime".into(), ocr.runtime_status(&snapshot));
        ok(Value::Object(body))
    })
    .await
    .unwrap_or_else(|r| r)
}

/// Copy an object's keys into `into` (Python `{**a, **b}`).
pub(crate) fn merge(into: &mut Map<String, Value>, from: Value) {
    if let Value::Object(m) = from {
        for (k, v) in m {
            into.insert(k, v);
        }
    }
}

pub(super) async fn dyndns(s: &AdminState, req: &ApiRequest) -> Response {
    let data = match req.json() {
        Ok(d) => d.clone(),
        Err(r) => return r,
    };
    let s2 = s.clone();
    let saved = blocking(
        move || -> Result<bunko_core::config::DynDnsConfig, Response> {
            let _guard = s2.config_lock.lock();
            {
                let mut cfg = s2.core().config.write();
                let d = &mut cfg.dyndns;
                if let Some(v) = data.get("enabled") {
                    d.enabled = truthy(v);
                }
                if let Some(v) = data.get("provider") {
                    match v.as_str().filter(|p| DYNDNS_PROVIDERS.contains(p)) {
                        Some(p) => d.provider = p.to_string(),
                        None => return Err(error(400, "Invalid provider")),
                    }
                }
                let text = |key: &str| -> Result<Option<String>, Response> {
                    match data.get(key) {
                        None => Ok(None),
                        Some(Value::Null) => Ok(Some(String::new())),
                        Some(Value::String(t)) => Ok(Some(t.clone())),
                        Some(_) => Err(error(400, format!("{key} must be a string"))),
                    }
                };
                // The masked value the page was given never overwrites the real token.
                if let Some(token) = text("token")?
                    && token != TOKEN_MASK
                {
                    d.token = token;
                }
                if let Some(domain) = text("domain")? {
                    d.domain = domain;
                }
                if let Some(url) = text("update_url")? {
                    d.update_url = url;
                }
                if let Some(v) = data.get("interval") {
                    match py_int_value(v).filter(|n| *n >= 30 && *n <= u32::MAX as i64) {
                        Some(n) => d.interval = n as u32,
                        None => return Err(error(400, "interval must be at least 30")),
                    }
                }
            }
            save_config(&s2)?;
            Ok(s2.core().config.read().dyndns.clone())
        },
    )
    .await;
    let d = match saved {
        Ok(Ok(d)) => d,
        Ok(Err(r)) | Err(r) => return r,
    };
    if let Some(service) = &s.deps.dyndns {
        service.configure(d.clone());
    }
    ok(json!({"success": true, "dyndns": {
        "enabled": d.enabled,
        "provider": d.provider,
        "domain": d.domain,
        "update_url": d.update_url,
        "interval": d.interval,
        "token": if d.token.is_empty() { "" } else { TOKEN_MASK },
    }}))
}
