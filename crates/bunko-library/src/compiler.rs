//! Turn a series folder on disk into the reader's volume index
//! (`metadata/compiler.py`).
//!
//! One entry per `.cbz` (a sidecar without an archive is not a volume). The
//! series title is the FOLDER name, the volume title the archive's stem. Each
//! compiled entry is cached against the stat of the files it came from, so a
//! regeneration that changes nothing is a stat walk; a miss parses the
//! sidecar and reads only the archive's central directory.

use std::fs;
use std::path::{Path, PathBuf};

use crate::archive;
use crate::compat::{
    count_matched_pages, count_page_chars, deterministic_uuid, natural_sort_key,
    normalize_volume_title_key,
};
use crate::fsutil::{self, py_mtime, py_mtime_int, size_of};
use crate::pyjson::{self, DumpOptions, JsonNum, JsonObject, JsonValue};
use crate::pyunicode;
use crate::schema::{VolumeEntry, missing_page_count};
use crate::sidecar::{self, py_stem};
use crate::store::{CachedEntryWrite, MetadataStore, StoreResult, identity_from_entry};

/// A top-level library folder that holds at least one archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesFolder {
    pub title: String,
    pub path: PathBuf,
}

/// Library-relative key of a volume's archive — the entry cache's key.
pub fn volume_key_for(series_title: &str, volume_title: &str) -> String {
    format!("{series_title}/{volume_title}.cbz")
}

fn is_cbz_name(name: &str) -> bool {
    pyunicode::lower(name).ends_with(".cbz")
}

/// `iter_series_folders`: top-level, non-hidden directories (symlinks
/// followed) directly holding at least one `.cbz` file, sorted by name. A
/// missing/unreadable library is empty.
pub fn iter_series_folders(library_path: &Path) -> Vec<SeriesFolder> {
    let Ok(mut entries) = fsutil::list_dir(library_path) else {
        return Vec::new();
    };
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
        .into_iter()
        .filter(|(name, path)| !name.starts_with('.') && fsutil::is_dir(path) && has_archive(path))
        .map(|(title, path)| SeriesFolder { title, path })
        .collect()
}

fn has_archive(folder: &Path) -> bool {
    fsutil::list_dir(folder)
        .map(|entries| {
            entries
                .iter()
                .any(|(name, path)| is_cbz_name(name) && fsutil::is_file(path))
        })
        .unwrap_or(false)
}

/// The `.cbz` file names directly in `folder`, sorted.
pub fn archive_names(folder: &Path) -> Vec<String> {
    let Ok(entries) = fsutil::list_dir(folder) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .into_iter()
        .filter(|(name, path)| is_cbz_name(name) && fsutil::is_file(path))
        .map(|(n, _)| n)
        .collect();
    names.sort();
    names
}

/// `_stat_key`: `"<sidecar name>:<st_size>:<repr(st_mtime)>"`, `""` for none.
pub fn sidecar_stat_key(sidecar: Option<&Path>, meta: Option<&fs::Metadata>) -> String {
    match (sidecar, meta) {
        (Some(path), Some(meta)) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            format!(
                "{name}:{}:{}",
                meta.len(),
                pyjson::float_repr(py_mtime(meta))
            )
        }
        _ => String::new(),
    }
}

/// `_page_paths`: each page's `img_path`, or `None` when no page names one.
fn page_paths(pages: Option<&JsonValue>) -> Option<Vec<Option<String>>> {
    let JsonValue::Array(pages) = pages? else {
        return None;
    };
    let mut any = false;
    let paths: Vec<Option<String>> = pages
        .iter()
        .map(
            |page| match page.as_object().and_then(|p| p.get("img_path")) {
                Some(JsonValue::Str(path)) if !path.is_empty() => {
                    any = true;
                    Some(path.clone())
                }
                _ => None,
            },
        )
        .collect();
    any.then_some(paths)
}

/// `_compile_volume`: one compiled entry, and whether it is safe to cache
/// (not when the archive could not be opened this time).
pub fn compile_volume(
    series_title: &str,
    cbz_path: &Path,
    sidecar: Option<&Path>,
    sidecar_meta: Option<&fs::Metadata>,
) -> (VolumeEntry, bool) {
    let volume_title = cbz_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(py_stem)
        .unwrap_or_default()
        .to_owned();
    let loaded = sidecar.map(sidecar::load_sidecar);
    let archive_size = fsutil::stat(cbz_path).map_or(0, |meta| size_of(&meta));
    let archive_size = (archive_size != 0).then_some(JsonNum::Int(archive_size));
    let mokuro_size = sidecar_meta.map(|meta| JsonValue::from(size_of(meta)));
    let mokuro_modified = sidecar_meta.map(|meta| JsonValue::from(py_mtime_int(meta)));

    let image_names = archive::reader_image_names(cbz_path);
    let cacheable = image_names.is_some();
    let archive_pages = image_names.as_ref().map_or(0, Vec::len) as i64;

    let (data, sha256) = match loaded {
        Some(loaded) => (loaded.data, loaded.sha256),
        None => (None, None),
    };
    let Some(data) = data else {
        return (
            VolumeEntry {
                volume_uuid: deterministic_uuid(&format!("{series_title}/{volume_title}")),
                volume_title,
                page_count: archive_pages,
                matched_page_count: cacheable.then_some(archive_pages),
                character_count: JsonNum::Int(0),
                mokuro_version: String::new(),
                spine_width: None,
                archive_size,
                mokuro_size,
                mokuro_modified,
                mokuro_sha256: None,
                cover_size: None,
                cover_modified: None,
            },
            cacheable,
        );
    };

    let pages = data.get("pages");
    let volume_uuid = match data.get("volume_uuid") {
        Some(JsonValue::Str(uuid)) if !pyunicode::strip(uuid).is_empty() => uuid.clone(),
        _ => deterministic_uuid(&format!("{series_title}/{volume_title}")),
    };
    let mokuro_version = data
        .get("version")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .to_owned();
    let character_count = match data.get("chars") {
        Some(JsonValue::Num(num)) if num.is_int() && num.is_positive() => num.clone(),
        _ => JsonNum::Int(i64::try_from(count_page_chars(pages)).unwrap_or(i64::MAX)),
    };
    let (page_count, matched_page_count) = match page_paths(pages) {
        None => {
            let count = match pages {
                Some(JsonValue::Array(items)) => items.len() as i64,
                _ => archive_pages,
            };
            (count, None)
        }
        Some(paths) => {
            let matched = image_names
                .as_ref()
                .map(|names| i64::try_from(count_matched_pages(&paths, names)).unwrap_or(i64::MAX));
            (paths.len() as i64, matched)
        }
    };
    let spine_width = match data.get("spine_width") {
        Some(JsonValue::Num(num)) if num.is_positive() => Some(num.clone()),
        _ => None,
    };
    (
        VolumeEntry {
            volume_uuid,
            volume_title,
            page_count,
            matched_page_count,
            character_count,
            mokuro_version,
            spine_width,
            archive_size,
            mokuro_size,
            mokuro_modified,
            mokuro_sha256: sha256,
            cover_size: None,
            cover_modified: None,
        },
        cacheable,
    )
}

/// `_entry_to_dict`: the cached JSON object, in 0.5.2's key order.
pub fn entry_to_object(entry: &VolumeEntry) -> JsonObject {
    let mut object = JsonObject::new();
    object.insert("volume_uuid", JsonValue::from(entry.volume_uuid.as_str()));
    object.insert("volume_title", JsonValue::from(entry.volume_title.as_str()));
    object.insert("page_count", JsonValue::from(entry.page_count));
    object.insert(
        "matched_page_count",
        JsonValue::from(entry.matched_page_count),
    );
    object.insert(
        "character_count",
        JsonValue::Num(entry.character_count.clone()),
    );
    object.insert(
        "mokuro_version",
        JsonValue::from(entry.mokuro_version.as_str()),
    );
    object.insert("spine_width", JsonValue::from(entry.spine_width.clone()));
    object.insert("archive_size", JsonValue::from(entry.archive_size.clone()));
    object.insert(
        "mokuro_size",
        entry.mokuro_size.clone().unwrap_or(JsonValue::Null),
    );
    object.insert(
        "mokuro_modified",
        entry.mokuro_modified.clone().unwrap_or(JsonValue::Null),
    );
    object.insert(
        "mokuro_sha256",
        JsonValue::from(entry.mokuro_sha256.clone()),
    );
    object
}

/// `json.dumps(_entry_to_dict(entry), ensure_ascii=False)`.
pub fn entry_json(entry: &VolumeEntry) -> String {
    // allow_nan is on for this format, so this cannot fail.
    pyjson::dumps(
        &JsonValue::Object(entry_to_object(entry)),
        DumpOptions::DEFAULT_UTF8,
    )
    .unwrap_or_default()
}

/// Python `str(value)` for the scalar values a cache row can hold.
fn py_str(value: &JsonValue) -> Option<String> {
    Some(match value {
        JsonValue::Str(text) => text.clone(),
        JsonValue::Null => "None".to_owned(),
        JsonValue::Bool(true) => "True".to_owned(),
        JsonValue::Bool(false) => "False".to_owned(),
        JsonValue::Num(JsonNum::Int(value)) => value.to_string(),
        JsonValue::Num(JsonNum::BigInt(text)) => text.to_string(),
        JsonValue::Num(JsonNum::Float(value)) if value.is_finite() => pyjson::float_repr(*value),
        _ => return None,
    })
}

/// Python `int(value)` for the scalar values a cache row can hold (`None`
/// where Python raises).
fn py_int(value: &JsonValue) -> Option<i64> {
    match value {
        JsonValue::Num(JsonNum::Int(value)) => Some(*value),
        JsonValue::Num(JsonNum::Float(value)) if value.is_finite() => Some(value.trunc() as i64),
        JsonValue::Bool(flag) => Some(i64::from(*flag)),
        JsonValue::Str(text) => {
            let cleaned: String = pyunicode::strip(text)
                .chars()
                .filter(|&c| c != '_')
                .collect();
            let (negative, digits) = match cleaned.strip_prefix('-') {
                Some(rest) => (true, rest.to_owned()),
                None => (
                    false,
                    cleaned.strip_prefix('+').unwrap_or(&cleaned).to_owned(),
                ),
            };
            if digits.is_empty() || !digits.chars().all(pyunicode::is_decimal) {
                return None;
            }
            let value: i64 = pyunicode::decimal_run_value(&digits).parse().ok()?;
            Some(if negative { -value } else { value })
        }
        _ => None,
    }
}

fn non_null(value: &JsonValue) -> Option<JsonValue> {
    (!matches!(value, JsonValue::Null)).then(|| value.clone())
}

/// `_entry_from_dict`: a cached entry, or `None` (a miss) when the row lacks
/// a required key or holds values Python could not convert.
pub fn entry_from_object(raw: &JsonObject) -> Option<VolumeEntry> {
    let matched = raw.get("matched_page_count")?;
    let mokuro_size = raw.get("mokuro_size")?;
    let mokuro_modified = raw.get("mokuro_modified")?;
    let number = |key: &str| raw.get(key).and_then(JsonValue::as_num).cloned();
    Some(VolumeEntry {
        volume_uuid: py_str(raw.get("volume_uuid")?)?,
        volume_title: py_str(raw.get("volume_title")?)?,
        page_count: py_int(raw.get("page_count")?)?,
        matched_page_count: match matched {
            JsonValue::Null => None,
            other => Some(py_int(other)?),
        },
        character_count: match raw.get("character_count")? {
            JsonValue::Num(JsonNum::BigInt(text)) => JsonNum::BigInt(text.clone()),
            other => JsonNum::Int(py_int(other)?),
        },
        mokuro_version: py_str(raw.get("mokuro_version")?)?,
        spine_width: number("spine_width"),
        archive_size: number("archive_size"),
        mokuro_size: non_null(mokuro_size),
        mokuro_modified: non_null(mokuro_modified),
        mokuro_sha256: raw
            .get("mokuro_sha256")
            .and_then(JsonValue::as_str)
            .map(str::to_owned),
        cover_size: None,
        cover_modified: None,
    })
}

/// The stats a cache lookup is keyed on.
struct VolumeStats {
    cbz_size: i64,
    cbz_mtime: f64,
    sidecar: Option<PathBuf>,
    sidecar_meta: Option<fs::Metadata>,
    sidecar_key: String,
}

fn volume_stats(cbz_path: &Path) -> Option<VolumeStats> {
    let sidecar = sidecar::primary_sidecar(cbz_path);
    let sidecar_meta = sidecar.as_deref().and_then(fsutil::stat);
    let sidecar_key = sidecar_stat_key(sidecar.as_deref(), sidecar_meta.as_ref());
    let cbz_meta = fsutil::stat(cbz_path)?;
    Some(VolumeStats {
        cbz_size: size_of(&cbz_meta),
        cbz_mtime: py_mtime(&cbz_meta),
        sidecar,
        sidecar_meta,
        sidecar_key,
    })
}

/// `get_cached_volume_entry`: the cached object when the row's stats match
/// exactly (`int(cbz_size) ==`, `float(cbz_mtime) ==`, `sidecar_key ==`) and
/// it holds a non-empty object.
fn cached_object(
    store: &dyn MetadataStore,
    key: &str,
    stats: &VolumeStats,
) -> StoreResult<Option<JsonObject>> {
    let Some(row) = store.cached_volume_entry(key)? else {
        return Ok(None);
    };
    if row.cbz_size != stats.cbz_size
        || row.cbz_mtime != stats.cbz_mtime
        || row.sidecar_key != stats.sidecar_key
    {
        return Ok(None);
    }
    let object = pyjson::load_json_object(Some(&row.entry_json));
    Ok((!object.is_empty()).then_some(object))
}

fn put_entry(
    store: &dyn MetadataStore,
    key: &str,
    series_key: &str,
    entry: &VolumeEntry,
    stats: &VolumeStats,
) -> StoreResult<()> {
    let object = entry_to_object(entry);
    let text = pyjson::dumps(
        &JsonValue::Object(object.clone()),
        DumpOptions::DEFAULT_UTF8,
    )
    .unwrap_or_default();
    store.put_cached_volume_entry(&CachedEntryWrite {
        volume_key: key,
        series_key,
        entry_json: &text,
        cbz_size: stats.cbz_size,
        cbz_mtime: stats.cbz_mtime,
        sidecar_key: &stats.sidecar_key,
        identity: identity_from_entry(&object),
    })
}

/// `compile_series_volumes`: every volume of one series, in natural title
/// order. `fill_hashes` completes cache rows written before `mokuro_sha256`
/// existed (background passes only; request paths pass `false`).
pub fn compile_series_volumes(
    series: &SeriesFolder,
    store: Option<&dyn MetadataStore>,
    fill_hashes: bool,
) -> StoreResult<Vec<VolumeEntry>> {
    let series_key = normalize_volume_title_key(&series.title);
    let mut entries: Vec<VolumeEntry> = Vec::new();
    for name in archive_names(&series.path) {
        let cbz_path = series.path.join(&name);
        let volume_title = py_stem(&name).to_owned();
        let Some(stats) = volume_stats(&cbz_path) else {
            continue;
        };
        let key = volume_key_for(&series.title, &volume_title);
        let mut entry: Option<VolumeEntry> = None;
        if let Some(store) = store
            && let Some(cached) = cached_object(store, &key, &stats)?
        {
            entry = entry_from_object(&cached);
            if let Some(found) = entry.as_mut()
                && fill_hashes
                && !cached.contains_key("mokuro_sha256")
                && let Some(sidecar) = &stats.sidecar
                && let Some(digest) = sidecar::sidecar_sha256(sidecar)
            {
                found.mokuro_sha256 = Some(digest);
                put_entry(store, &key, &series_key, found, &stats)?;
            }
        }
        let mut entry = match entry {
            Some(entry) => entry,
            None => {
                let (entry, cacheable) = compile_volume(
                    &series.title,
                    &cbz_path,
                    stats.sidecar.as_deref(),
                    stats.sidecar_meta.as_ref(),
                );
                if let Some(store) = store
                    && cacheable
                {
                    put_entry(store, &key, &series_key, &entry, &stats)?;
                }
                entry
            }
        };
        // The cover stat is never cached: thumbnails appear on their own schedule.
        let cover = fsutil::stat(&sidecar::cover_path(&cbz_path));
        entry.cover_size = cover.as_ref().map(size_of);
        entry.cover_modified = cover.as_ref().map(py_mtime_int);
        entries.push(entry);
    }
    let mut keyed: Vec<_> = entries
        .into_iter()
        .map(|entry| (natural_sort_key(&entry.volume_title), entry))
        .collect();
    keyed.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.volume_title.cmp(&b.1.volume_title))
    });
    Ok(keyed.into_iter().map(|(_, entry)| entry).collect())
}

// --- cache-only lookups (OCR queue, manifest) --------------------------------

fn cached_entry(
    store: &dyn MetadataStore,
    library_path: &Path,
    cbz_path: &Path,
) -> StoreResult<Option<VolumeEntry>> {
    let Ok(relative) = cbz_path.strip_prefix(library_path) else {
        return Ok(None);
    };
    let parts: Vec<_> = relative.components().collect();
    if parts.len() != 2 {
        return Ok(None);
    }
    let Some(series_title) = parts[0].as_os_str().to_str() else {
        return Ok(None);
    };
    let Some(volume_title) = cbz_path.file_name().and_then(|n| n.to_str()).map(py_stem) else {
        return Ok(None);
    };
    let Some(stats) = volume_stats(cbz_path) else {
        return Ok(None);
    };
    Ok(
        cached_object(store, &volume_key_for(series_title, volume_title), &stats)?
            .and_then(|o| entry_from_object(&o)),
    )
}

/// `cached_missing_pages`: pages short, from the CACHE only (0 when stale/absent).
pub fn cached_missing_pages(
    store: &dyn MetadataStore,
    library_path: &Path,
    cbz_path: &Path,
) -> StoreResult<i64> {
    Ok(cached_entry(store, library_path, cbz_path)?.map_or(0, |e| {
        missing_page_count(e.page_count, e.matched_page_count)
    }))
}

/// `cached_page_count`: the sidecar's page count from the cache, else `None`.
pub fn cached_page_count(
    store: &dyn MetadataStore,
    library_path: &Path,
    cbz_path: &Path,
) -> StoreResult<Option<i64>> {
    Ok(cached_entry(store, library_path, cbz_path)?
        .map(|e| e.page_count)
        .filter(|&pages| pages > 0))
}

/// `cached_mokuro_sha256`: the primary sidecar's hash from the cache, else `None`.
pub fn cached_mokuro_sha256(
    store: &dyn MetadataStore,
    library_path: &Path,
    cbz_path: &Path,
) -> StoreResult<Option<String>> {
    Ok(cached_entry(store, library_path, cbz_path)?.and_then(|e| e.mokuro_sha256))
}

/// `missing_pages_now`: [`cached_missing_pages`], compiled (and cached) right
/// now when nothing current is cached and the volume has a sidecar.
pub fn missing_pages_now(
    store: &dyn MetadataStore,
    library_path: &Path,
    cbz_path: &Path,
) -> StoreResult<i64> {
    let Ok(relative) = cbz_path.strip_prefix(library_path) else {
        return Ok(0);
    };
    let parts: Vec<_> = relative.components().collect();
    if parts.len() != 2 {
        return Ok(0);
    }
    let Some(sidecar) = sidecar::primary_sidecar(cbz_path) else {
        return Ok(0);
    };
    if let Some(entry) = cached_entry(store, library_path, cbz_path)? {
        return Ok(missing_page_count(
            entry.page_count,
            entry.matched_page_count,
        ));
    }
    let Some(series_title) = parts[0].as_os_str().to_str() else {
        return Ok(0);
    };
    let Some(stats) = volume_stats(cbz_path) else {
        return Ok(0);
    };
    let sidecar_meta = fsutil::stat(&sidecar);
    let (entry, cacheable) = compile_volume(
        series_title,
        cbz_path,
        Some(&sidecar),
        sidecar_meta.as_ref(),
    );
    if cacheable {
        let key = volume_key_for(series_title, &entry.volume_title);
        put_entry(
            store,
            &key,
            &normalize_volume_title_key(series_title),
            &entry,
            &stats,
        )?;
    }
    Ok(missing_page_count(
        entry.page_count,
        entry.matched_page_count,
    ))
}
