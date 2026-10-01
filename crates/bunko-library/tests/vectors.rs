//! Golden vectors computed by Python 0.5.2 (`tests/golden/gen_vectors.py`):
//! every pure helper must agree with the reference implementation exactly.

use std::path::Path;

use bunko_library::archive::natsort::natsorted;
use bunko_library::compat;
use bunko_library::pyjson::{self, DumpOptions, JsonNum, JsonObject, JsonValue};
use bunko_library::pyunicode;
use bunko_library::schema::{SeriesFacts, SeriesIndexData};
use bunko_library::sidecar::split_layer_sidecar;
use bunko_library::update::{StoredSeries, merge_series_update, parse_series_update};

const COMPACT_NAN: DumpOptions = DumpOptions {
    ensure_ascii: false,
    item_separator: ",",
    key_separator: ":",
    allow_nan: true,
};

fn vectors() -> JsonObject {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/vectors.json");
    let text = std::fs::read_to_string(path).expect("vectors.json");
    match pyjson::parse(&text).expect("valid vectors") {
        JsonValue::Object(object) => object,
        _ => panic!("vectors.json is not an object"),
    }
}

fn get<'a>(object: &'a JsonValue, key: &str) -> &'a JsonValue {
    object
        .as_object()
        .and_then(|o| o.get(key))
        .unwrap_or_else(|| panic!("missing {key}"))
}

fn s(value: &JsonValue) -> &str {
    value.as_str().expect("string")
}

fn list(value: &JsonValue) -> &[JsonValue] {
    value.as_array().expect("array")
}

fn compact(value: &JsonValue) -> String {
    pyjson::dumps(value, COMPACT_NAN).unwrap()
}

fn now(v: &JsonObject) -> f64 {
    v.get("now")
        .and_then(JsonValue::as_num)
        .map(JsonNum::as_f64)
        .unwrap()
}

#[test]
fn natural_sort_matches_python() {
    let v = vectors();
    let case = v.get("natural_sort").unwrap();
    let mut titles: Vec<&str> = list(get(case, "input")).iter().map(s).collect();
    titles.sort_by(|a, b| compat::natural_title_cmp(a, b));
    let expected: Vec<&str> = list(get(case, "sorted")).iter().map(s).collect();
    assert_eq!(titles, expected);
}

#[test]
fn keys_uuids_and_case_tables_match_python() {
    let v = vectors();
    for case in list(v.get("keys").unwrap()) {
        let text = s(get(case, "text"));
        assert_eq!(
            compat::normalize_series_key(text),
            s(get(case, "series_key")),
            "series key of {text:?}"
        );
        assert_eq!(
            compat::normalize_volume_title_key(text),
            s(get(case, "volume_key")),
            "volume key of {text:?}"
        );
        assert_eq!(
            compat::deterministic_uuid(text),
            s(get(case, "uuid")),
            "uuid of {text:?}"
        );
        assert_eq!(
            pyunicode::lower(text),
            s(get(case, "lower")),
            "lower of {text:?}"
        );
        assert_eq!(
            pyunicode::casefold(text),
            s(get(case, "casefold")),
            "casefold of {text:?}"
        );
    }
}

#[test]
fn float_repr_matches_python() {
    let v = vectors();
    for case in list(v.get("floats").unwrap()) {
        let bits = match get(case, "bits") {
            JsonValue::Num(JsonNum::Int(bits)) => *bits as u64,
            JsonValue::Num(JsonNum::BigInt(text)) => text.parse::<u64>().unwrap(),
            other => panic!("bits {other:?}"),
        };
        let value = f64::from_bits(bits);
        assert_eq!(
            pyjson::float_repr(value),
            s(get(case, "repr")),
            "bits {bits:#x}"
        );
    }
}

#[test]
fn normalize_updated_at_matches_python() {
    let v = vectors();
    let now = now(&v);
    let mut checked = 0;
    for case in list(v.get("updated_at").unwrap()) {
        let input = s(get(case, "input"));
        let expected = match get(case, "output") {
            JsonValue::Str(text) => Some(text.as_str()),
            _ => None, // null (rejected) or {"raised": ...} (0.5.2 crashed; the port rejects)
        };
        let actual = compat::normalize_updated_at(Some(&JsonValue::from(input)), now);
        assert_eq!(
            actual.as_deref(),
            expected,
            "normalize_updated_at({input:?})"
        );
        checked += 1;
    }
    assert!(checked > 5000);
}

#[test]
fn iso_stamp_matches_python() {
    let v = vectors();
    for case in list(v.get("iso_stamp").unwrap()) {
        let seconds = get(case, "seconds").as_num().unwrap().as_f64();
        assert_eq!(
            compat::iso_stamp(seconds).as_deref(),
            get(case, "output").as_str(),
            "iso_stamp({seconds})"
        );
    }
}

#[test]
fn json_reading_and_writing_match_python() {
    let v = vectors();
    for case in list(v.get("json").unwrap()) {
        let doc = s(get(case, "doc"));
        let ok = matches!(get(case, "ok"), JsonValue::Bool(true));
        let parsed = pyjson::parse(doc);
        assert_eq!(parsed.is_ok(), ok, "json.loads({doc:?})");
        if let Ok(value) = parsed {
            assert_eq!(
                compact(&value),
                s(get(case, "compact")),
                "compact dumps of {doc:?}"
            );
            assert_eq!(
                pyjson::dumps(&value, DumpOptions::DEFAULT).unwrap(),
                s(get(case, "default")),
                "default dumps of {doc:?}"
            );
        }
    }
}

#[test]
fn count_chars_matches_python() {
    let v = vectors();
    for case in list(v.get("count_chars").unwrap()) {
        let text = s(get(case, "text"));
        let expected = get(case, "count").as_num().unwrap().as_f64() as u64;
        assert_eq!(compat::count_chars(text), expected, "count_chars({text:?})");
    }
}

#[test]
fn matched_pages_match_python() {
    let v = vectors();
    for case in list(v.get("matched_pages").unwrap()) {
        let pages: Vec<Option<String>> = list(get(case, "pages"))
            .iter()
            .map(|p| p.as_str().map(str::to_owned))
            .collect();
        let files: Vec<String> = list(get(case, "files"))
            .iter()
            .map(|f| s(f).to_owned())
            .collect();
        let expected = get(case, "matched").as_num().unwrap().as_f64() as u64;
        assert_eq!(
            compat::count_matched_pages(&pages, &files),
            expected,
            "{pages:?} vs {files:?}"
        );
    }
}

#[test]
fn system_files_and_extensions_match_python() {
    let v = vectors();
    for case in list(v.get("system_files").unwrap()) {
        let path = s(get(case, "path"));
        assert_eq!(
            compat::is_system_file(path),
            matches!(get(case, "system"), JsonValue::Bool(true)),
            "{path:?}"
        );
        assert_eq!(
            compat::trailing_extension(path),
            s(get(case, "ext")),
            "{path:?}"
        );
        let ext = compat::trailing_extension(path);
        assert_eq!(
            compat::is_image_extension(&ext),
            matches!(get(case, "image"), JsonValue::Bool(true)),
            "{path:?}"
        );
    }
}

#[test]
fn layer_split_matches_python() {
    let v = vectors();
    for case in list(v.get("layers").unwrap()) {
        let name = s(get(case, "name"));
        let expected = get(case, "split")
            .as_array()
            .map(|pair| (s(&pair[0]).to_owned(), s(&pair[1]).to_owned()));
        let actual = split_layer_sidecar(name).map(|(a, b)| (a.to_owned(), b.to_owned()));
        assert_eq!(actual, expected, "{name:?}");
    }
}

#[test]
fn natsort_page_order_matches_python() {
    let v = vectors();
    for case in list(v.get("natsort").unwrap()) {
        let mut items: Vec<&str> = list(get(case, "input")).iter().map(s).collect();
        natsorted(&mut items, |item| item);
        let expected: Vec<&str> = list(get(case, "sorted")).iter().map(s).collect();
        assert_eq!(items, expected);
    }
}

fn facts_json(facts: &SeriesFacts) -> String {
    let mut object = JsonObject::new();
    object.insert(
        "external_ids",
        JsonValue::Object(facts.external_ids.clone()),
    );
    object.insert("titles", JsonValue::Object(facts.titles.clone()));
    object.insert("synonyms", JsonValue::Array(facts.synonyms.clone()));
    object.insert("tag", JsonValue::from(facts.tag.clone()));
    object.insert("unit", JsonValue::from(facts.unit.clone()));
    object.insert("updated_at", JsonValue::from(facts.updated_at.as_str()));
    object.insert("has_facts", JsonValue::Bool(facts.has_facts()));
    compact(&JsonValue::Object(object))
}

#[test]
fn update_validation_matches_python() {
    let v = vectors();
    let now = now(&v);
    for case in list(v.get("updates").unwrap()) {
        let payload = s(get(case, "payload"));
        let actual = parse_series_update(payload.as_bytes(), now);
        let expected = get(case, "result");
        match (actual, expected) {
            (None, JsonValue::Null) => {}
            (Some(update), JsonValue::Object(_)) => {
                assert_eq!(
                    facts_json(&update.facts),
                    compact(get(expected, "facts")),
                    "facts of {payload}"
                );
                assert_eq!(
                    compact(&JsonValue::from(update.spine_offset.clone())),
                    compact(get(expected, "spine_offset")),
                    "{payload}"
                );
                assert_eq!(
                    update.spine_offset_present,
                    matches!(get(expected, "spine_offset_present"), JsonValue::Bool(true))
                );
                assert_eq!(
                    compact(&JsonValue::Object(update.volume_offsets.clone())),
                    compact(get(expected, "volume_offsets")),
                    "{payload}"
                );
                let mut listed = update.listed_uuids.clone();
                listed.sort();
                let expected_listed: Vec<String> = list(get(expected, "listed"))
                    .iter()
                    .map(|x| s(x).to_owned())
                    .collect();
                assert_eq!(listed, expected_listed);
            }
            (actual, expected) => panic!("{payload}: rust {actual:?} vs python {expected:?}"),
        }
    }
}

fn object(pairs: &[(&str, JsonValue)]) -> JsonObject {
    let mut object = JsonObject::new();
    for (key, value) in pairs {
        object.insert(*key, value.clone());
    }
    object
}

#[test]
fn merges_match_python() {
    let v = vectors();
    let now = now(&v);
    let stored = [
        None,
        Some(StoredSeries {
            facts: SeriesFacts {
                tag: Some("x".into()),
                updated_at: "2026-01-01T00:00:00.000Z".into(),
                ..Default::default()
            },
            index: SeriesIndexData {
                spine_offset: Some(JsonNum::Int(12)),
                volume_offsets: object(&[("u", JsonValue::from(3)), ("v", JsonValue::from(4))]),
            },
        }),
        Some(StoredSeries {
            facts: SeriesFacts {
                updated_at: "2026-01-01T00:00:00.000Z".into(),
                ..Default::default()
            },
            index: SeriesIndexData::default(),
        }),
        Some(StoredSeries {
            facts: SeriesFacts {
                external_ids: object(&[("anilist", JsonValue::from(5))]),
                updated_at: "2026-06-01T00:00:00.000Z".into(),
                ..Default::default()
            },
            index: SeriesIndexData {
                spine_offset: Some(JsonNum::Float(12.0)),
                volume_offsets: JsonObject::new(),
            },
        }),
    ];
    for case in list(v.get("merges").unwrap()) {
        let index = get(case, "stored").as_num().unwrap().as_f64() as usize;
        let payload = s(get(case, "payload"));
        let update = parse_series_update(payload.as_bytes(), now).unwrap();
        let result = merge_series_update(stored[index].as_ref(), &update);
        let label = format!("stored #{index} + {payload}");
        assert_eq!(
            facts_json(&result.facts),
            compact(get(case, "facts")),
            "{label}"
        );
        assert_eq!(
            compact(&JsonValue::from(result.index.spine_offset.clone())),
            compact(get(case, "spine_offset")),
            "{label}"
        );
        assert_eq!(
            compact(&JsonValue::Object(result.index.volume_offsets.clone())),
            compact(get(case, "volume_offsets")),
            "{label}"
        );
        assert_eq!(
            result.facts_changed,
            matches!(get(case, "facts_changed"), JsonValue::Bool(true)),
            "{label}"
        );
        assert_eq!(
            result.index_changed,
            matches!(get(case, "index_changed"), JsonValue::Bool(true)),
            "{label}"
        );
    }
}

#[test]
fn metadata_paths_and_watcher_routing_match_python() {
    use bunko_library::paths;
    let v = vectors();
    let cases = v.get("paths").unwrap();
    for case in list(get(cases, "virtual")) {
        let path = s(get(case, "path"));
        assert_eq!(
            paths::is_catalog_file_path(path),
            matches!(get(case, "catalog"), JsonValue::Bool(true)),
            "{path}"
        );
        assert_eq!(
            paths::series_title_from_series_file_path(path).as_deref(),
            get(case, "series").as_str(),
            "{path}"
        );
        assert_eq!(
            paths::is_compiled_metadata_path(path),
            matches!(get(case, "compiled"), JsonValue::Bool(true)),
            "{path}"
        );
    }
    for case in list(get(cases, "changes")) {
        let path = s(get(case, "path"));
        let actual = paths::classify_change(Path::new("/lib"), Path::new(path));
        let expected = match s(get(case, "kind")) {
            "series" => paths::LibraryChange::Series(s(get(case, "name")).to_owned()),
            "library" => paths::LibraryChange::Library,
            "ignore" => paths::LibraryChange::Ignore,
            other => panic!("kind {other}"),
        };
        assert_eq!(actual, expected, "{path}");
    }
    for case in list(get(cases, "relevant")) {
        let path = s(get(case, "path"));
        let is_dir = matches!(get(case, "dir"), JsonValue::Bool(true));
        assert_eq!(
            paths::is_relevant_change(Path::new(path), is_dir),
            matches!(get(case, "relevant"), JsonValue::Bool(true)),
            "{path}"
        );
    }
}
