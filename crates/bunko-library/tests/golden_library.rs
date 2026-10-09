//! End-to-end golden test against Python 0.5.3 (`tests/golden/gen_library.py`):
//! the same fixture library, the same full passes and client PUTs, and every
//! compiled file must come out byte-identical; the database rows, the library
//! index, the manifests and the archive page lists must match too.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bunko_library::archive::{self, Volume};
use bunko_library::index::scan_library;
use bunko_library::manifest::build_volume_manifest;
use bunko_library::pyjson::{self, DumpOptions, JsonObject, JsonValue};
use bunko_library::service::{DebouncePolicy, MetadataService, NoHooks, NoPathLocks};
use bunko_library::store::MetadataStore;
use common::{SqliteStore, dump_table, expected_table, fixture_library, load_json, obj};
use sha2::{Digest, Sha256};

#[rustfmt::skip]
const TABLES: &[(&str, &str, &[&str])] = &[
    (
        "series_facts",
        "series_key",
        &[
            "series_key", "series_title", "external_ids", "titles", "synonyms", "tag", "unit", "facts_updated_at",
            "spine_offset", "volume_offsets", "updated_by",
        ],
    ),
    ("series_entry_cache", "volume_key", &["volume_key", "series_key", "entry_json", "cbz_size", "cbz_mtime", "sidecar_key"]),
    (
        "catalog_folders",
        "folder_name",
        &[
            "series_key", "folder_name", "cover_path", "volume_count", "latest_volume_modified", "total_pages",
            "total_chars", "missing_pages", "damaged_volumes",
        ],
    ),
    ("volume_identities", "volume_key", &["volume_key", "volume_uuid"]),
];

fn schema(db: &JsonValue) -> Vec<String> {
    obj(db)
        .get("schema")
        .and_then(JsonValue::as_array)
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_owned())
        .collect()
}

/// Every `*.json` under the library, relative path -> text.
fn compiled_files(root: &Path) -> BTreeMap<String, String> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|ext| ext == "json") {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn expected_files(step: &JsonValue) -> BTreeMap<String, String> {
    obj(obj(step).get("files").unwrap())
        .iter()
        .map(|(k, v)| (k.to_owned(), v.as_str().unwrap().to_owned()))
        .collect()
}

fn assert_files(root: &Path, step: &JsonValue, label: &str) {
    let actual = compiled_files(root);
    let expected = expected_files(step);
    assert_eq!(
        actual.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "{label}: compiled file set"
    );
    for (path, text) in &expected {
        assert_eq!(
            &actual[path], text,
            "{label}: {path} differs from Python 0.5.3"
        );
    }
}

fn mtimes(root: &Path) -> BTreeMap<String, std::time::SystemTime> {
    compiled_files(root)
        .keys()
        .map(|rel| {
            (
                rel.clone(),
                std::fs::metadata(root.join(rel))
                    .unwrap()
                    .modified()
                    .unwrap(),
            )
        })
        .collect()
}

fn service(root: &Path, store: Arc<SqliteStore>) -> MetadataService {
    MetadataService::new(
        root,
        store,
        Arc::new(NoHooks),
        Arc::new(NoPathLocks),
        DebouncePolicy::default(),
    )
}

#[test]
fn full_passes_and_puts_are_byte_identical_to_python() {
    let steps = load_json("expected/steps.json");
    let steps = steps.as_array().unwrap();
    let db = load_json("expected/db.json");
    let (_temp, root) = fixture_library();
    let store = Arc::new(SqliteStore::new(&schema(&db)));
    let service = service(&root, Arc::clone(&store));

    let changed = service.recompile_all().unwrap();
    assert_eq!(
        changed as f64,
        obj(&steps[0])
            .get("changed")
            .unwrap()
            .as_num()
            .unwrap()
            .as_f64()
    );
    assert_files(&root, &steps[0], "initial full pass");

    let mut previous = expected_files(&steps[0]);
    for step in &steps[1..steps.len() - 1] {
        let step_obj = obj(step);
        let title = step_obj.get("title").and_then(JsonValue::as_str).unwrap();
        let payload = step_obj.get("payload").and_then(JsonValue::as_str).unwrap();
        let actor = step_obj.get("actor").and_then(JsonValue::as_str);
        let label = step_obj.get("step").and_then(JsonValue::as_str).unwrap();
        let before = mtimes(&root);
        // Distinct mtimes for anything rewritten during this step.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let accepted = service
            .apply_series_update(title, payload.as_bytes(), actor)
            .unwrap();
        assert_eq!(
            accepted,
            matches!(step_obj.get("accepted"), Some(JsonValue::Bool(true))),
            "{label}: accepted"
        );
        assert_files(&root, step, label);
        // A file whose bytes did not change must not be rewritten (clients key on size/mtime).
        let current = expected_files(step);
        let after = mtimes(&root);
        for (path, text) in &current {
            if previous.get(path) == Some(text) {
                assert_eq!(
                    before.get(path),
                    after.get(path),
                    "{label}: unchanged {path} was rewritten"
                );
            } else {
                assert_ne!(
                    before.get(path),
                    after.get(path),
                    "{label}: changed {path} kept its mtime"
                );
            }
        }
        previous = current;
    }

    let last = steps.last().unwrap();
    let changed = service.recompile_all().unwrap();
    assert_eq!(
        changed as f64,
        obj(last).get("changed").unwrap().as_num().unwrap().as_f64()
    );
    assert_files(&root, last, "final full pass");

    for (table, key, columns) in TABLES {
        assert_eq!(
            dump_table(&store, table, key, columns),
            expected_table(&db, table),
            "table {table}"
        );
    }
}

#[test]
fn a_warm_python_cache_is_read_back_without_recompiling() {
    let steps = load_json("expected/steps.json");
    let steps = steps.as_array().unwrap();
    let db = load_json("expected/db.json");
    let (_temp, root) = fixture_library();
    let store = Arc::new(SqliteStore::new(&schema(&db)));
    // Facts and cache rows exactly as Python left them.
    {
        let conn = store.conn.lock();
        for (table, _, columns) in TABLES {
            for row in obj(&db).get(table).unwrap().as_array().unwrap() {
                let values: Vec<rusqlite::types::Value> = row
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|cell| {
                        let cell = obj(cell);
                        match (
                            cell.get("type").and_then(JsonValue::as_str).unwrap(),
                            cell.get("value").unwrap(),
                        ) {
                            ("NoneType", _) => rusqlite::types::Value::Null,
                            ("int", JsonValue::Num(n)) => {
                                rusqlite::types::Value::Integer(n.as_f64() as i64)
                            }
                            ("float", JsonValue::Str(text)) => {
                                rusqlite::types::Value::Real(text.parse().unwrap())
                            }
                            (_, JsonValue::Str(text)) => rusqlite::types::Value::Text(text.clone()),
                            other => panic!("unexpected cell {other:?}"),
                        }
                    })
                    .collect();
                let placeholders = vec!["?"; columns.len()].join(", ");
                conn.execute(
                    &format!(
                        "INSERT INTO {table} ({}) VALUES ({placeholders})",
                        columns.join(", ")
                    ),
                    rusqlite::params_from_iter(values),
                )
                .unwrap();
            }
        }
        // A legacy row: written before `mokuro_sha256` existed.
        let legacy: String = conn
            .query_row("SELECT entry_json FROM series_entry_cache WHERE volume_key = 'Dr Stone/Dr Stone 01.cbz'", [], |r| r.get(0))
            .unwrap();
        let mut entry = pyjson::load_json_object(Some(&legacy));
        assert!(entry.remove("mokuro_sha256").is_some());
        let stripped = pyjson::dumps(&JsonValue::Object(entry), DumpOptions::DEFAULT_UTF8).unwrap();
        conn.execute("UPDATE series_entry_cache SET entry_json = ? WHERE volume_key = 'Dr Stone/Dr Stone 01.cbz'", [&stripped]).unwrap();
    }
    let service = service(&root, Arc::clone(&store));
    service.recompile_all().unwrap();
    // Only the legacy row was written back (hash filled); everything else hit.
    assert_eq!(store.cache_writes.load(Ordering::SeqCst), 1);
    assert_files(&root, steps.last().unwrap(), "warm full pass");
    for (table, key, columns) in TABLES {
        assert_eq!(
            dump_table(&store, table, key, columns),
            expected_table(&db, table),
            "table {table}"
        );
    }
    // Cache-only lookups answer from those rows.
    let library = root.as_path();
    let damaged = root.join("Dr Stone/Dr Stone 02.cbz");
    assert_eq!(
        bunko_library::compiler::cached_missing_pages(store.as_ref(), library, &damaged).unwrap(),
        1
    );
    assert!(
        bunko_library::compiler::cached_mokuro_sha256(store.as_ref(), library, &damaged)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        bunko_library::compiler::cached_page_count(store.as_ref(), library, &damaged).unwrap(),
        Some(10)
    );
    assert_eq!(
        bunko_library::compiler::missing_pages_now(store.as_ref(), library, &damaged).unwrap(),
        1
    );
}

fn snapshot_json(root: &Path) -> JsonValue {
    let snapshot = scan_library(root);
    let mut out = JsonObject::new();
    let series = snapshot
        .series
        .iter()
        .map(|s| {
            let mut entry = JsonObject::new();
            entry.insert("name", JsonValue::from(s.name.as_str()));
            entry.insert("cover", JsonValue::from(s.cover.clone()));
            let volumes = s
                .volumes
                .iter()
                .map(|v| {
                    let mut volume = JsonObject::new();
                    volume.insert("name", JsonValue::from(v.name.as_str()));
                    volume.insert("has_cbz", JsonValue::Bool(v.has_cbz));
                    volume.insert("has_mokuro", JsonValue::Bool(v.has_mokuro));
                    volume.insert("has_mokuro_gz", JsonValue::Bool(v.has_mokuro_gz));
                    volume.insert("cover", JsonValue::from(v.cover.clone()));
                    volume.insert(
                        "sidecars",
                        JsonValue::Array(
                            v.sidecars
                                .iter()
                                .map(|s| JsonValue::from(s.as_str()))
                                .collect(),
                        ),
                    );
                    JsonValue::Object(volume)
                })
                .collect();
            entry.insert("volumes", JsonValue::Array(volumes));
            JsonValue::Object(entry)
        })
        .collect();
    out.insert("series", JsonValue::Array(series));
    out.insert(
        "pending_ocr",
        JsonValue::Array(
            snapshot
                .pending_ocr
                .iter()
                .map(|(a, b)| {
                    JsonValue::Array(vec![
                        JsonValue::from(a.as_str()),
                        JsonValue::from(b.as_str()),
                    ])
                })
                .collect(),
        ),
    );
    out.insert(
        "pending_thumbnails",
        JsonValue::from(snapshot.pending_thumbnails as i64),
    );
    JsonValue::Object(out)
}

fn canonical(value: &JsonValue) -> String {
    pyjson::dumps(value, DumpOptions::DEFAULT).unwrap()
}

/// 0.5.3's index with 0.7's one deliberate difference: `Dr Stone 03.CBZ` is an archive
/// (`has_cbz`; 0.5.3 asked for exactly `.cbz`), so it is also waiting for a cover.
fn expected_index_07() -> String {
    let python = canonical(&load_json("expected/index.json"));
    let (from, to) = (
        r#"{"name": "Dr Stone 03", "has_cbz": false,"#,
        r#"{"name": "Dr Stone 03", "has_cbz": true,"#,
    );
    assert_eq!(python.matches(from).count(), 1, "{python}");
    let (thumbs_from, thumbs_to) = (r#""pending_thumbnails": 10"#, r#""pending_thumbnails": 11"#);
    assert_eq!(python.matches(thumbs_from).count(), 1, "{python}");
    python.replace(from, to).replace(thumbs_from, thumbs_to)
}

#[test]
fn library_index_matches_python() {
    let (_temp, root) = fixture_library();
    assert_eq!(canonical(&snapshot_json(&root)), expected_index_07());
}

#[test]
fn manifests_match_python() {
    let (_temp, root) = fixture_library();
    // The compiled files Python had written, with the mtime it pinned them to.
    let steps = load_json("expected/steps.json");
    for (rel, text) in expected_files(steps.as_array().unwrap().last().unwrap()) {
        let path = root.join(&rel);
        std::fs::write(&path, text).unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(
            std::time::UNIX_EPOCH + std::time::Duration::new(1_800_000_000, 500_000_000),
        )
        .unwrap();
    }
    let expected = load_json("expected/manifests.json");
    let order = vec!["ppocr-manga".to_owned(), "hayai-nova".to_owned()];
    let mut checked = 0;
    for (key, manifest) in obj(&expected).iter() {
        let (series, volume) = key.rsplit_once('/').unwrap();
        let actual = build_volume_manifest(&root.join(series), series, volume, &order);
        let actual = actual.map_or(JsonValue::Null, JsonValue::Object);
        assert_eq!(canonical(&actual), canonical(manifest), "manifest of {key}");
        checked += 1;
    }
    assert!(checked > 10);
}

#[test]
fn archive_listings_and_pages_match_python() {
    let (_temp, root) = fixture_library();
    let expected = load_json("expected/archives.json");
    for (rel, case) in obj(&expected).iter() {
        let path = root.join(rel);
        let case = obj(case);
        let names = archive::reader_image_names(&path);
        let expected_names = case
            .get("reader_image_names")
            .unwrap()
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| item.as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            });
        assert_eq!(names, expected_names, "reader image names of {rel}");

        let siblings: Vec<String> = bunko_library::sidecar::sidecar_siblings(&path)
            .iter()
            .map(|p| {
                // Python recorded POSIX paths.
                let parts: Vec<String> = p
                    .strip_prefix(&root)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                parts.join("/")
            })
            .collect();
        let expected_siblings: Vec<String> = case
            .get("siblings")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(siblings, expected_siblings, "sidecar siblings of {rel}");

        let expected_pages = case.get("pages").unwrap().as_array();
        match (Volume::open(&path), expected_pages) {
            (Ok(volume), Some(pages)) => {
                let actual: Vec<(String, String)> = (0..volume.pages().len())
                    .map(|i| {
                        let bytes = volume
                            .read_page(i)
                            .unwrap_or_else(|e| panic!("{rel} page {i}: {e}"));
                        (
                            volume.pages()[i].path.clone(),
                            hex::encode(Sha256::digest(&bytes)),
                        )
                    })
                    .collect();
                let wanted: Vec<(String, String)> = pages
                    .iter()
                    .map(|page| {
                        let page = obj(page);
                        (
                            page.get("path").unwrap().as_str().unwrap().to_owned(),
                            page.get("sha256").unwrap().as_str().unwrap().to_owned(),
                        )
                    })
                    .collect();
                assert_eq!(actual, wanted, "pages of {rel}");
            }
            (Err(error), None) => assert!(
                matches!(error, archive::ArchiveError::Damaged(_)),
                "{rel}: {error}"
            ),
            (actual, expected) => panic!(
                "{rel}: rust {:?} vs python {expected:?}",
                actual.map(|v| v.pages().len())
            ),
        }
    }
}

#[test]
fn store_trait_is_object_safe() {
    fn takes(_: &dyn MetadataStore) {}
    let db = load_json("expected/db.json");
    takes(&SqliteStore::new(&schema(&db)));
}

#[test]
fn directory_volumes_list_pages_like_python() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("pagedir");
    common::copy_tree(&common::golden_dir().join("pagedir"), &dir);
    // Not committed (git cannot check it out on Windows), but part of the reference run:
    // a trailing dot is not an image extension. Windows strips it from new names.
    #[cfg(not(windows))]
    std::fs::write(dir.join("e.jpg."), b"IMG:e.jpg.e.jpg.e.jpg.").unwrap();
    let volume = Volume::open(&dir).unwrap();
    let pages: Vec<&str> = volume
        .pages()
        .iter()
        .map(|page| page.path.as_str())
        .collect();
    let expected = load_json("expected/pagedir.json");
    let expected: Vec<&str> = expected
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert_eq!(pages, expected);
    // Pages stream straight from the files.
    let first = volume.read_page(0).unwrap();
    assert_eq!(first, std::fs::read(dir.join("9.jpg")).unwrap());
}

#[test]
fn damage_is_read_back_from_the_compiled_series_file() {
    let (_temp, root) = fixture_library();
    let steps = load_json("expected/steps.json");
    for (rel, text) in expected_files(steps.as_array().unwrap().last().unwrap()) {
        std::fs::write(root.join(rel), text).unwrap();
    }
    let damage = bunko_library::manifest::damage_by_volume_title(&root.join("Dr Stone"));
    assert_eq!(damage.get("Dr Stone 02"), Some(&(10, 1)));
    assert_eq!(damage.get("Dr Stone 01"), Some(&(5, 0)));
    assert!(bunko_library::manifest::damage_by_volume_title(&root.join("Nested")).is_empty());
}
