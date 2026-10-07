//! The public catalog (0.5.2 `catalog/api.py`, spec metadata-catalog §9,
//! web-frontend-contract §5.6).
//!
//! `/catalog*` sits OUTSIDE the WebDAV auth gate and is read live against
//! `catalog.enabled`: while the catalog is off every such request falls through to the
//! rest of the app, which is why it is a middleware ([`catalog_middleware`]) and not a
//! router. The volume manifest is the exception: it is always served, gated like a
//! `GET` of the volume's `.cbz` ([`router`]).

use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use bunko_library::compat::normalize_volume_title_key;
use bunko_library::manifest::{build_volume_manifest, damage_by_volume_title, with_ocr_sha256};
use bunko_library::pyjson::{self, DumpOptions, JsonValue};
use bunko_library::pyunicode;
use http::{HeaderMap, HeaderValue, Method, header};
use serde_json::{Map, Value, json};

use super::LibraryDeps;
use super::store::to_py;
use super::util::{self, is_within, json, json_bytes, json_error, query_param, resolve, unquote};
use crate::auth;
use crate::core::RequestCtx;

pub const MANIFEST_PATH: &str = "/catalog/api/manifest";

/// `GET /catalog/api/manifest`, served whether or not the catalog page is on.
pub fn router(deps: LibraryDeps) -> Router {
    Router::new()
        .route(MANIFEST_PATH, get(manifest))
        .with_state(deps)
}

/// Everything else under `/catalog`, while `catalog.enabled` (read per request).
pub async fn catalog_middleware(
    State(deps): State<LibraryDeps>,
    req: Request,
    next: Next,
) -> Response {
    // PATH_INFO: the percent-decoded path.
    let path = unquote(req.uri().path());
    let method = req.method().clone();
    if !path.starts_with("/catalog") || (path == MANIFEST_PATH && method == Method::GET) {
        return next.run(req).await;
    }
    if !deps.core.config.read().catalog.enabled {
        return next.run(req).await;
    }
    if path == "/catalog" || path == "/catalog/" {
        return serve_static("index.html");
    }
    if let Some(rest) = path.strip_prefix("/catalog/api/") {
        let rest = rest.to_owned();
        return handle_api(
            deps,
            &format!("/catalog/api/{rest}"),
            &method,
            req.uri().query(),
            req.headers(),
        )
        .await;
    }
    if let Some(file) = path.strip_prefix("/catalog/") {
        return serve_static(file);
    }
    next.run(req).await
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

fn internal_error() -> Response {
    util::text(500, "Error")
}

async fn handle_api(
    deps: LibraryDeps,
    path: &str,
    method: &Method,
    query: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    if *method != Method::GET {
        return json_error(404, "Not found");
    }
    match path {
        "/catalog/api/library" => {
            let headers = headers.clone();
            blocking(move || {
                let body = list_library(&deps);
                json_bytes(200, body.into_bytes(), Some(&headers), &[])
            })
            .await
            .unwrap_or_else(internal_error)
        }
        "/catalog/api/config" => {
            let reader_url = deps.core.config.read().catalog.reader_url.clone();
            json(200, &json!({ "reader_url": reader_url }))
        }
        "/catalog/api/ocr-status" => blocking(move || match active_progress(&deps) {
            Some(progress) => json(200, &Value::Object(progress)),
            None => json(200, &json!({ "active": false })),
        })
        .await
        .unwrap_or_else(internal_error),
        "/catalog/api/series" => {
            let name = query_param(query, "name");
            if name.is_empty() {
                return json_error(400, "Missing series name");
            }
            blocking(move || get_series(&deps, &name))
                .await
                .unwrap_or_else(internal_error)
        }
        "/catalog/api/cover" => {
            let cover = query_param(query, "path");
            if cover.is_empty() {
                return util::text(400, "Missing cover path");
            }
            serve_cover(&deps, &cover).await
        }
        _ => {
            if let Some(name) = path.strip_prefix("/catalog/api/series/") {
                // 0.5.2 unquoted the already-decoded PATH_INFO once more.
                let name = unquote(name);
                return blocking(move || get_series(&deps, &name))
                    .await
                    .unwrap_or_else(internal_error);
            }
            if let Some(cover) = path.strip_prefix("/catalog/api/cover/") {
                return serve_cover(&deps, &unquote(cover)).await;
            }
            json_error(404, "Not found")
        }
    }
}

// ---------------------------------------------------------------------------
// /catalog/api/library
// ---------------------------------------------------------------------------

/// `_display_facts_by_series_key`: `titles` (non-empty object) and `tag` (non-blank)
/// per series key.
fn display_facts(deps: &LibraryDeps) -> std::collections::HashMap<String, Map<String, Value>> {
    let rows = match deps.db.list_series_facts() {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "catalog: series facts unavailable");
            return Default::default();
        }
    };
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let mut entry = Map::new();
        if !row.titles.is_empty() {
            entry.insert("titles".into(), Value::Object(row.titles));
        }
        if let Some(tag) = row.tag.filter(|t| !bunko_db::pyfmt::strip(t).is_empty()) {
            entry.insert("tag".into(), Value::String(tag));
        }
        if !entry.is_empty() {
            out.insert(row.series_key, entry);
        }
    }
    out
}

fn community_by_key(deps: &LibraryDeps) -> std::collections::HashMap<String, Value> {
    match deps.db.list_community_details() {
        Ok(rows) => rows
            .into_iter()
            .map(|row| {
                let score = row.score.map_or(Value::Null, Value::from);
                (row.series_key, json!({"score": score, "tags": row.tags, "genres": row.genres, "source": row.source}))
            })
            .collect(),
        Err(_) => Default::default(),
    }
}

/// `_list_library`: the root view's one slim payload (Python `json.dumps` text).
fn list_library(deps: &LibraryDeps) -> String {
    let facts = display_facts(deps);
    let rows = deps.db.list_catalog_series().unwrap_or_else(|error| {
        tracing::warn!(%error, "catalog: catalog_folders unavailable; listing from the index");
        Vec::new()
    });
    let mut series: Vec<Value> = Vec::new();
    if !rows.is_empty() {
        let community = community_by_key(deps);
        for row in rows {
            let mut info = Map::new();
            info.insert("name".into(), Value::String(row.folder_name.clone()));
            info.insert("path".into(), Value::String(row.folder_name));
            info.insert(
                "cover".into(),
                row.cover_path.map_or(Value::Null, Value::String),
            );
            info.insert("volume_count".into(), Value::from(row.volume_count));
            info.insert(
                "latest_volume_modified".into(),
                Value::from(row.latest_volume_modified),
            );
            info.insert("total_pages".into(), Value::from(row.total_pages));
            info.insert("total_chars".into(), Value::from(row.total_chars));
            info.insert("missing_pages".into(), Value::from(row.missing_pages));
            info.insert("damaged_volumes".into(), Value::from(row.damaged_volumes));
            if let Some(extra) = facts.get(&row.series_key) {
                info.extend(extra.clone());
            }
            if let Some(details) = community.get(&row.series_key) {
                info.insert("community".into(), details.clone());
            }
            series.push(Value::Object(info));
        }
    } else {
        // First boot, before the startup pass filled the table: never blank.
        let snapshot = deps.runtime.index().get_snapshot();
        for entry in &snapshot.series {
            let mut info = Map::new();
            info.insert("name".into(), Value::String(entry.name.clone()));
            info.insert("path".into(), Value::String(entry.name.clone()));
            info.insert(
                "cover".into(),
                entry.cover.clone().map_or(Value::Null, Value::String),
            );
            info.insert("volume_count".into(), Value::from(entry.volumes.len()));
            if let Some(extra) = facts.get(&normalize_volume_title_key(&entry.name)) {
                info.extend(extra.clone());
            }
            series.push(Value::Object(info));
        }
    }
    bunko_db::pyfmt::dumps(
        &json!({ "series": series }),
        bunko_db::pyfmt::JsonStyle::DEFAULT_ASCII,
    )
}

// ---------------------------------------------------------------------------
// /catalog/api/series, /ocr-status
// ---------------------------------------------------------------------------

/// The OCR progress document when it says `active` (0.5.2 `_read_ocr_progress`).
fn active_progress(deps: &LibraryDeps) -> Option<Map<String, Value>> {
    let progress = deps.ocr_status.as_ref()?.progress()?;
    progress
        .get("active")
        .is_some_and(bunko_db::pyfmt::truthy)
        .then_some(progress)
}

/// `_active_ocr_job`: the running job whose `relative_cbz` is this volume (casefolded).
fn active_job<'a>(
    progress: Option<&'a Map<String, Value>>,
    series: &str,
    volume: &str,
) -> Option<&'a Map<String, Value>> {
    let progress = progress?;
    let expected = pyunicode::casefold(&format!("{series}/{volume}.cbz"));
    let candidates: Vec<&Map<String, Value>> = match progress.get("jobs").and_then(Value::as_array)
    {
        Some(jobs) if !jobs.is_empty() => jobs.iter().filter_map(Value::as_object).collect(),
        _ => vec![progress],
    };
    candidates.into_iter().find(|entry| {
        entry
            .get("relative_cbz")
            .and_then(Value::as_str)
            .is_some_and(|rel| pyunicode::casefold(rel) == expected)
    })
}

fn volume_progress(job: &Map<String, Value>) -> Value {
    let field = |name: &str| job.get(name).cloned().unwrap_or(Value::Null);
    json!({
        "percent": field("percent"),
        "eta_seconds": field("eta_seconds"),
        "status": field("status"),
        "processed_pages": field("processed_pages"),
        "total_pages": field("total_pages"),
    })
}

fn get_series(deps: &LibraryDeps, name: &str) -> Response {
    let library = deps.runtime.library_path();
    let base = resolve(library).unwrap_or_else(|| library.to_path_buf());
    let Some(series_dir) = resolve(&library.join(name)) else {
        return json_error(403, "Forbidden");
    };
    if !is_within(&series_dir, &base) {
        return json_error(403, "Forbidden");
    }
    let snapshot = deps.runtime.index().get_snapshot();
    let Some(series) = snapshot.series_by_name(name) else {
        return json_error(404, "Series not found");
    };
    let progress = active_progress(deps);
    let damage = damage_by_volume_title(&series_dir);
    let mut volumes: Vec<Value> = Vec::with_capacity(series.volumes.len());
    let mut series_cover = Value::Null;
    for volume in &series.volumes {
        let mut info = Map::new();
        let cover = volume.cover.clone().map_or(Value::Null, Value::String);
        if series_cover.is_null() && !cover.is_null() {
            series_cover = cover.clone();
        }
        info.insert("name".into(), Value::String(volume.name.clone()));
        info.insert("cover".into(), cover);
        let active = active_job(progress.as_ref(), name, &volume.name);
        info.insert(
            "ocr_pending".into(),
            Value::Bool(volume.has_cbz && !volume.has_mokuro && !volume.has_mokuro_gz),
        );
        info.insert("ocr_active".into(), Value::Bool(active.is_some()));
        if let Some(job) = active {
            info.insert("ocr_pending".into(), Value::Bool(false));
            info.insert("ocr_progress".into(), volume_progress(job));
        }
        if let Some((pages, missing)) = damage.get(&volume.name) {
            info.insert("page_count".into(), Value::from(*pages));
            info.insert("missing_pages".into(), Value::from(*missing));
        }
        volumes.push(Value::Object(info));
    }
    json(
        200,
        &json!({ "name": name, "cover": series_cover, "volumes": volumes }),
    )
}

// ---------------------------------------------------------------------------
// /catalog/api/cover
// ---------------------------------------------------------------------------

fn image_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        ".webp" => "image/webp",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".png" => "image/png",
        _ => return None,
    })
}

/// `_serve_cover`: a library image, streamed (never buffered whole).
async fn serve_cover(deps: &LibraryDeps, cover: &str) -> Response {
    let library = deps.runtime.library_path().to_path_buf();
    let cover = cover.to_owned();
    let checked = blocking(move || -> Result<(PathBuf, u64, &'static str), Response> {
        let base = resolve(&library).unwrap_or_else(|| library.clone());
        let file = resolve(&library.join(&cover)).ok_or_else(|| util::text(400, "Invalid path"))?;
        if !is_within(&file, &base) {
            return Err(util::text(403, "Forbidden"));
        }
        let meta = std::fs::metadata(&file)
            .ok()
            .filter(|m| m.is_file())
            .ok_or_else(|| util::text(404, "Not found"))?;
        let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let ext = pyunicode::lower(bunko_library::sidecar::py_suffix(name));
        let content_type = image_type(&ext).ok_or_else(|| util::text(403, "Forbidden"))?;
        Ok((file, meta.len(), content_type))
    })
    .await;
    let (file, size, content_type) = match checked {
        Some(Ok(found)) => found,
        Some(Err(resp)) => return resp,
        None => return internal_error(),
    };
    let Ok(handle) = tokio::fs::File::open(&file).await else {
        return util::text(500, "Error");
    };
    let mut resp = Response::new(Body::from_stream(tokio_util::io::ReaderStream::new(handle)));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    resp
}

// ---------------------------------------------------------------------------
// Static files
// ---------------------------------------------------------------------------

fn static_type(name: &str) -> &'static str {
    match pyunicode::lower(bunko_library::sidecar::py_suffix(name)).as_str() {
        ".html" => "text/html; charset=utf-8",
        ".js" => "application/javascript; charset=utf-8",
        ".css" => "text/css; charset=utf-8",
        ".json" => "application/json",
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

/// `_serve_static`: `web/catalog/<file>`, unknown files fall back to `index.html` (the
/// SPA), a path escaping the directory is 403.
fn serve_static(file: &str) -> Response {
    let file = if file.is_empty() || file == "/" {
        "index.html"
    } else {
        file
    };
    if file.starts_with('/') {
        return util::text(403, "Forbidden");
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in file.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return util::text(403, "Forbidden");
                }
            }
            other => parts.push(other),
        }
    }
    let wanted = parts.join("/");
    let (name, asset) = match crate::http::static_files::asset("catalog", &wanted) {
        Some(asset) => (wanted.as_str(), asset),
        None => match crate::http::static_files::asset("catalog", "index.html") {
            Some(asset) => ("index.html", asset),
            None => return util::text(404, "Not found"),
        },
    };
    let body = asset.data.into_owned();
    let length = body.len();
    let mut resp = Response::new(Body::from(body));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(static_type(name)),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}

// ---------------------------------------------------------------------------
// /catalog/api/manifest
// ---------------------------------------------------------------------------

/// `_serve_manifest`: answered as `GET /mokuro-reader/<series>/<volume>.cbz` would be
/// (authentication, rate limiter, anonymous-download switch, 401 challenge), then 403
/// for a path escaping the library and 404 for a volume without its archive.
async fn manifest(State(deps): State<LibraryDeps>, req: Request) -> Response {
    let query = req.uri().query().map(str::to_owned);
    let series = query_param(query.as_deref(), "series");
    let volume = query_param(query.as_deref(), "volume");
    if series.is_empty() || volume.is_empty() {
        return json_error(400, "Missing series or volume");
    }
    let (parts, _body) = req.into_parts();
    let headers = parts.headers.clone();
    let ctx = match parts.extensions.get::<RequestCtx>() {
        Some(ctx) => ctx.clone(),
        None => {
            let core = deps.core.clone();
            match blocking(move || RequestCtx::resolve(&core, &parts)).await {
                Some(ctx) => ctx,
                None => return internal_error(),
            }
        }
    };
    let archive_path = format!(
        "/{}/{series}/{volume}.cbz",
        bunko_library::paths::READER_ROOT
    );
    if let Err(denied) = auth::authorize(
        &Method::GET,
        &archive_path,
        None,
        &ctx.identity,
        deps.core.anonymous_access(),
        deps.core.backend.as_ref(),
    ) {
        return denied.into_response();
    }
    blocking(move || build_manifest(&deps, &series, &volume, &headers))
        .await
        .unwrap_or_else(internal_error)
}

fn build_manifest(deps: &LibraryDeps, series: &str, volume: &str, headers: &HeaderMap) -> Response {
    let library = deps.runtime.library_path();
    let base = resolve(library).unwrap_or_else(|| library.to_path_buf());
    let resolved = resolve(&library.join(series)).and_then(|dir| {
        let archive = resolve(&dir.join(format!("{volume}.cbz")))?;
        Some((dir, archive))
    });
    let Some((series_dir, archive)) = resolved else {
        return json_error(400, "Invalid path");
    };
    if !is_within(&series_dir, &base) || !is_within(&archive, &base) {
        return json_error(403, "Forbidden");
    }
    if series_dir == base || volume.contains('/') || volume.contains('\\') {
        return json_error(404, "Volume not found");
    }
    let order = deps.layer_order.as_ref().map(|f| f()).unwrap_or_default();
    let Some(mut manifest) = build_volume_manifest(&series_dir, series, volume, &order) else {
        return json_error(404, "Volume not found");
    };
    if !matches!(manifest.get("ocr"), Some(JsonValue::Null) | None)
        && let Some(digest) = deps.runtime.cached_mokuro_sha256(&base, &archive)
    {
        with_ocr_sha256(&mut manifest, &digest);
    }
    let outlook = deps
        .outlook
        .as_ref()
        .map(|o| o.volume_outlook(&archive, series, volume))
        .unwrap_or_default();
    manifest.insert(
        "pending",
        JsonValue::Array(outlook.pending.iter().map(to_py).collect()),
    );
    manifest.insert(
        "recheck_after",
        outlook
            .recheck_after
            .as_ref()
            .map_or(JsonValue::Null, to_py),
    );
    let body =
        pyjson::dumps(&JsonValue::Object(manifest), DumpOptions::DEFAULT).unwrap_or_default();
    json_bytes(
        200,
        body.into_bytes(),
        Some(headers),
        &[("cache-control", "no-store")],
    )
}
