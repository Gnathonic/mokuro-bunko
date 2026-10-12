//! The per-volume manifest a reader deep link points at
//! (`catalog/manifest.py`): every file the reader should fetch for one volume
//! (archive, primary OCR, OCR layers, cover, series file) with size and
//! modification time, built from ONE directory listing.
//!
//! A file belongs to the LONGEST archive stem it starts with plus `.`:
//! `Vol 1.5.mokuro` is the primary OCR of `Vol 1.5` when `Vol 1.5.cbz` is there.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::fsutil::{self, py_mtime, size_of};
use crate::isodate;
use crate::pyjson::{JsonObject, JsonValue};
use crate::pyunicode;
use crate::sidecar::split_layer_sidecar;

pub const MANIFEST_VERSION: i64 = 1;

/// `urllib.parse.quote(value.encode("utf-8"), safe="!*'()")` — JS
/// `encodeURIComponent`-compatible.
pub fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.-~!*'()".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `/mokuro-reader/<series>/<file>`, each segment escaped as the catalog does.
pub fn reader_file_url(series: &str, file_name: &str) -> String {
    format!(
        "/{}/{}/{}",
        crate::paths::READER_ROOT,
        encode_component(series),
        encode_component(file_name)
    )
}

/// The URL of a volume's manifest, as the catalog's link builds it.
pub fn manifest_url(series: &str, volume: &str) -> String {
    format!(
        "/catalog/api/manifest?series={}&volume={}",
        encode_component(series),
        encode_component(volume)
    )
}

fn file_entry(series: &str, name: &str, meta: &fs::Metadata) -> JsonObject {
    let mut entry = JsonObject::new();
    entry.insert("url", JsonValue::from(reader_file_url(series, name)));
    entry.insert("size", JsonValue::from(size_of(meta)));
    entry.insert(
        "modified",
        JsonValue::from(isodate::iso_seconds_stamp(py_mtime(meta)).unwrap_or_default()),
    );
    entry
}

/// A regular file (symlinks followed), with its stat.
fn file_meta(path: &Path) -> Option<fs::Metadata> {
    fsutil::stat(path).filter(|meta| meta.is_file())
}

fn with_meta<'a>(
    entries: &HashMap<String, PathBuf>,
    name: Option<&'a str>,
) -> Option<(&'a str, fs::Metadata)> {
    let name = name?;
    Some((name, file_meta(entries.get(name)?)?))
}

/// `build_volume_manifest`: the manifest for `<series_dir>/<volume>.cbz`, or
/// `None` when there is no such archive. `series` is the name URLs are built
/// with; `layer_order` the configured generation order (layers it names come
/// first, the rest alphabetically). The caller has already gated the read and
/// checked `series_dir` is inside the library; it adds `ocr.sha256`
/// (see [`with_ocr_sha256`]), `pending` and `recheck_after`.
pub fn build_volume_manifest(
    series_dir: &Path,
    series: &str,
    volume: &str,
    layer_order: &[String],
) -> Option<JsonObject> {
    let entries: HashMap<String, PathBuf> =
        fsutil::list_dir(series_dir).ok()?.into_iter().collect();
    let archive_name = format!("{volume}.cbz");
    let archive_meta = file_meta(entries.get(&archive_name)?)?;
    let prefix = format!("{volume}.");
    let longer: Vec<&str> = entries
        .keys()
        .filter(|name| {
            pyunicode::casefold(name).ends_with(".cbz")
                && name.starts_with(&prefix)
                && **name != archive_name
        })
        .filter_map(|name| name.get(..name.len() - 4))
        .collect();

    let mut plain_ocr: Option<&str> = None;
    let mut gz_ocr: Option<&str> = None;
    let mut cover: Option<&str> = None;
    let mut layers: HashMap<&str, &str> = HashMap::new();
    for name in entries.keys() {
        if !name.starts_with(&prefix) || *name == archive_name {
            continue;
        }
        if longer
            .iter()
            .any(|stem| name.starts_with(&format!("{stem}.")))
        {
            continue;
        }
        match &name[prefix.len()..] {
            "mokuro" => plain_ocr = Some(name),
            "mokuro.gz" => gz_ocr = Some(name),
            "webp" => cover = Some(name),
            _ => {
                let Some((owner, layer_id)) = split_layer_sidecar(name) else {
                    continue;
                };
                if owner != volume {
                    continue;
                }
                if !layers.contains_key(layer_id) || name.ends_with(".mokuro") {
                    layers.insert(layer_id, name);
                }
            }
        }
    }
    let layer_files: HashMap<&str, (&str, fs::Metadata)> = layers
        .into_iter()
        .filter_map(|(id, name)| Some((id, (name, file_meta(&entries[name])?))))
        .collect();
    let mut ordered: Vec<&str> = Vec::new();
    for id in layer_order {
        if layer_files.contains_key(id.as_str()) && !ordered.contains(&id.as_str()) {
            ordered.push(id.as_str());
        }
    }
    let mut rest: Vec<&str> = layer_files
        .keys()
        .copied()
        .filter(|id| !ordered.contains(id))
        .collect();
    rest.sort_unstable();
    ordered.extend(rest);

    let ocr = with_meta(&entries, plain_ocr).or_else(|| with_meta(&entries, gz_ocr));
    let cover = with_meta(&entries, cover);
    let series_file = entries
        .get(crate::paths::SERIES_FILE_NAME)
        .and_then(|path| file_meta(path))
        .map(|meta| (crate::paths::SERIES_FILE_NAME, meta));

    let entry_or_null = |item: Option<(&str, fs::Metadata)>| match item {
        Some((name, meta)) => JsonValue::Object(file_entry(series, name, &meta)),
        None => JsonValue::Null,
    };
    let mut manifest = JsonObject::new();
    manifest.insert("version", JsonValue::from(MANIFEST_VERSION));
    manifest.insert("series", JsonValue::from(series));
    manifest.insert("volume", JsonValue::from(volume));
    manifest.insert(
        "archive",
        JsonValue::Object(file_entry(series, &archive_name, &archive_meta)),
    );
    manifest.insert("ocr", entry_or_null(ocr));
    let layers_json: Vec<JsonValue> = ordered
        .into_iter()
        .map(|id| {
            let (name, meta) = &layer_files[id];
            let mut layer = JsonObject::new();
            layer.insert("id", JsonValue::from(id));
            for (key, value) in file_entry(series, name, meta).0 {
                layer.insert(key, value);
            }
            JsonValue::Object(layer)
        })
        .collect();
    manifest.insert("layers", JsonValue::Array(layers_json));
    manifest.insert("cover", entry_or_null(cover));
    manifest.insert("series_file", entry_or_null(series_file));
    Some(manifest)
}

/// Add the cached `sha256` to the manifest's `ocr` entry (only when the
/// metadata pass's hash of this very sidecar is current; never computed on a
/// request).
pub fn with_ocr_sha256(manifest: &mut JsonObject, digest: &str) {
    for (key, value) in &mut manifest.0 {
        if key == "ocr"
            && let JsonValue::Object(ocr) = value
        {
            ocr.insert("sha256", JsonValue::from(digest));
        }
    }
}

/// `{volume_title: (page_count, missing_pages)}` read back out of a compiled
/// `<Series>/series.json` (the catalog's per-volume damage badges; catalog
/// `_damage_by_volume_title`). Missing or corrupt file: empty.
pub fn damage_by_volume_title(series_dir: &Path) -> HashMap<String, (i64, i64)> {
    let mut damage = HashMap::new();
    let Ok(text) = fs::read_to_string(series_dir.join(crate::paths::SERIES_FILE_NAME)) else {
        return damage;
    };
    let Ok(JsonValue::Object(raw)) = crate::pyjson::parse(&text) else {
        return damage;
    };
    let Some(JsonValue::Array(volumes)) = raw.get("volumes") else {
        return damage;
    };
    for entry in volumes {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let (Some(JsonValue::Str(title)), Some(JsonValue::Num(crate::pyjson::JsonNum::Int(pages)))) =
            (entry.get("volume_title"), entry.get("page_count"))
        else {
            continue;
        };
        let matched = match entry.get("matched_page_count") {
            Some(JsonValue::Num(crate::pyjson::JsonNum::Int(matched))) => Some(*matched),
            _ => None,
        };
        damage.insert(
            title.clone(),
            (*pages, crate::schema::missing_page_count(*pages, matched)),
        );
    }
    damage
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_encode_uri_component() {
        assert_eq!(encode_component("Dr Stone 01"), "Dr%20Stone%2001");
        assert_eq!(encode_component("a/b!*'()~"), "a%2Fb!*'()~");
        assert_eq!(encode_component("日"), "%E6%97%A5");
        assert_eq!(
            manifest_url("A B", "v&1"),
            "/catalog/api/manifest?series=A%20B&volume=v%261"
        );
    }
}
