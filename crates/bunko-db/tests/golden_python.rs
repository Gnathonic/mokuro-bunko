//! Python -> Rust: open databases made by the Python `Database` class
//! (`tests/golden/make_golden.py`) and check that every value, password, token, invite,
//! audit page (cursors included) and JSON spelling reads back as Python saw it.
//!
//! Two generations of fixture: `py052` (made by 0.5.2, before the catalog was keyed by
//! folder) and `py053` (made by 0.5.3, the version bunko-db tracks, with data shaped like
//! a library that went through 0.5.3's case merge). Every content check runs over both; what opening a
//! 0.5.2-era file does to it is pinned against what 0.5.3 does (`upgrade053.json`).

use bunko_db::{AuditDetails, AuditQuery, CommunityDetails, Database, DbOptions, SeriesFacts};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

fn manifest(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(golden(name)).unwrap()).unwrap()
}

/// Copy a fixture into a temp dir: opening runs migrations, which must never touch the
/// checked-in file.
fn copy_fixture(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mokuro.db");
    std::fs::copy(golden(name), &path).unwrap();
    (dir, path)
}

fn schema_rows(path: &Path) -> Value {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY rowid")
        .unwrap();
    let rows: Vec<Value> = stmt
        .query_map([], |r| {
            Ok(json!([
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?
            ]))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    Value::Array(rows)
}

fn open(path: &Path) -> Database {
    Database::open_with(path, &DbOptions::default()).unwrap()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

/// Every row of `sql` as JSON arrays (as Python's `[list(r) for r in conn.execute(sql)]`).
fn query(conn: &rusqlite::Connection, sql: &str) -> Value {
    let mut stmt = conn.prepare(sql).unwrap();
    let n = stmt.column_count();
    let rows: Vec<Value> = stmt
        .query_map([], |r| {
            let mut cols = Vec::with_capacity(n);
            for i in 0..n {
                cols.push(match r.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(v) => json!(v),
                    rusqlite::types::ValueRef::Real(v) => json!(v),
                    rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                    rusqlite::types::ValueRef::Blob(_) => Value::Null,
                });
            }
            Ok(Value::Array(cols))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    Value::Array(rows)
}

/// One `#[test]` per fixture generation for each content check.
macro_rules! per_fixture {
    ($($check:ident),* $(,)?) => {
        mod py052 {
            $(#[test] fn $check() { super::$check("py052") })*
        }
        mod py053 {
            $(#[test] fn $check() { super::$check("py053") })*
        }
    };
}

per_fixture!(
    users_and_passwords,
    tokens,
    invites,
    audit_log,
    ownership,
    ocr_and_identities,
    series_tables_read_back_and_rewrite_byte_identically,
);

#[test]
fn a_fresh_rust_schema_is_byte_identical_to_python() {
    let m = manifest("py053.json");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mokuro.db");
    drop(open(&path));
    assert_eq!(schema_rows(&path), m["fresh_schema"]);
}

/// A 0.5.3 database opens unchanged; a 0.5.2 one gains exactly what 0.5.3 gives it
/// (`catalog_folders`, nothing else: its `catalog_series` is left as it was).
#[test]
fn a_python_database_opens_as_python_053_opens_it() {
    for (tag, want) in [
        ("py053", manifest("py053.json")["schema"].clone()),
        (
            "py052",
            manifest("upgrade053.json")["py052"]["schema"].clone(),
        ),
    ] {
        let (_dir, path) = copy_fixture(&format!("{tag}.db"));
        let before = schema_rows(&path);
        drop(open(&path));
        assert_eq!(schema_rows(&path), want, "{tag}");
        assert_legacy_table_untouched(&before, &schema_rows(&path), tag);
        let version: i64 = rusqlite::Connection::open(&path)
            .unwrap()
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 6);
    }
}

/// The 0.5.2 `catalog_series` DDL, where a file has one, is byte-identical after an open.
fn assert_legacy_table_untouched(before: &Value, after: &Value, tag: &str) {
    let legacy = |rows: &Value| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r[1] == "catalog_series")
            .cloned()
    };
    assert_eq!(legacy(before), legacy(after), "{tag}: catalog_series DDL");
}

fn users_and_passwords(tag: &str) {
    let m = manifest(&format!("{tag}.json"));
    let (_dir, path) = copy_fixture(&format!("{tag}.db"));
    let db = open(&path);
    for u in m["users"].as_array().unwrap() {
        let user = db.get_user(s(&u["username"])).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&user).unwrap(),
            u["expected"],
            "{}",
            u["username"]
        );
    }
    for pair in m["authenticates"].as_array().unwrap() {
        let (name, pw) = (s(&pair[0]), s(&pair[1]));
        let user = db.authenticate_user(name, pw).unwrap();
        assert_eq!(
            user.map(|u| u.username).as_deref(),
            Some(name),
            "{name} must authenticate"
        );
    }
    for pair in m["refuses"].as_array().unwrap() {
        let (name, pw) = (s(&pair[0]), s(&pair[1]));
        assert!(
            db.authenticate_user(name, pw).unwrap().is_none(),
            "{name} / {pw:?} must fail"
        );
    }
    for (name, stamp) in m["processor_stamps"].as_object().unwrap() {
        let ours = db.processor_account_stamp(name).unwrap();
        assert_eq!(ours.as_deref(), stamp.as_str(), "{name}");
    }
    let mut ours: Vec<String> = db
        .list_users(None)
        .unwrap()
        .into_iter()
        .map(|u| u.username)
        .collect();
    let mut theirs: Vec<String> = m["user_list"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| s(v).to_string())
        .collect();
    ours.sort();
    theirs.sort();
    assert_eq!(ours, theirs);
}

fn tokens(tag: &str) {
    let m = manifest(&format!("{tag}.json"));
    let (_dir, path) = copy_fixture(&format!("{tag}.db"));
    let db = open(&path);
    for t in m["tokens"].as_array().unwrap() {
        let user = db.resolve_auth_token(s(&t["token"])).unwrap();
        assert_eq!(
            user.map(|u| u.username).as_deref(),
            t["resolves_to"].as_str(),
            "{t}"
        );
    }
    let first = s(&m["tokens"][0]["token"]);
    assert!(db.revoke_auth_token(first).unwrap());
    assert!(db.resolve_auth_token(first).unwrap().is_none());
    assert_eq!(
        db.prune_expired_auth_tokens().unwrap(),
        1,
        "the one expired token"
    );
}

fn invites(tag: &str) {
    let m = manifest(&format!("{tag}.json"));
    let (_dir, path) = copy_fixture(&format!("{tag}.db"));
    let db = open(&path);
    for inv in m["invites"].as_array().unwrap() {
        let code = s(&inv["code"]);
        assert_eq!(
            db.validate_invite(code).unwrap().is_some(),
            inv["valid"].as_bool().unwrap(),
            "{code}"
        );
        let info = db.invite_info(code).unwrap().unwrap();
        assert_eq!(serde_json::to_value(&info).unwrap(), inv["info"], "{code}");
    }
}

fn query_from(v: &Value) -> AuditQuery {
    let strs = |k: &str| -> Vec<String> {
        v.get(k)
            .and_then(Value::as_array)
            .map_or_else(Vec::new, |a| {
                a.iter().map(|x| x.as_str().unwrap().to_string()).collect()
            })
    };
    let opt = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    AuditQuery {
        actor: opt("actor"),
        actions: strs("actions"),
        target_types: strs("target_types"),
        since: opt("since"),
        until: opt("until"),
        search: opt("search"),
        include_progress: v
            .get("include_progress")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        cursor: opt("cursor"),
        limit: v.get("limit").and_then(Value::as_i64).unwrap_or(50),
    }
}

fn audit_log(tag: &str) {
    let m = manifest(&format!("{tag}.json"));
    let (_dir, path) = copy_fixture(&format!("{tag}.db"));
    let db = open(&path);
    for vector in m["detail_vectors"].as_array().unwrap() {
        let details: AuditDetails = vector["pairs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (s(&p[0]).to_string(), p[1].clone()))
            .collect();
        assert_eq!(details.to_json(), s(&vector["json"]));
    }
    let rows = db.list_audit_events(1000, None).unwrap();
    assert_eq!(serde_json::to_value(&rows).unwrap(), m["audit_rows"]);
    for case in m["audit_queries"].as_array().unwrap() {
        let mut q = query_from(&case["query"]);
        for want in case["pages"].as_array().unwrap() {
            let page = db.query_audit_events(&q).unwrap();
            let ids: Vec<i64> = page.events.iter().map(|e| e.id).collect();
            assert_eq!(json!(ids), want["ids"], "{}", case["query"]);
            assert_eq!(json!(page.total), want["total"], "{}", case["query"]);
            assert_eq!(
                json!(page.next_cursor),
                want["next_cursor"],
                "{}",
                case["query"]
            );
            q.cursor = page.next_cursor;
        }
    }
    for case in m["audit_errors"].as_array().unwrap() {
        let err = db
            .query_audit_events(&query_from(&case["query"]))
            .unwrap_err();
        assert_eq!(err.to_string(), s(&case["error"]));
    }
    assert_eq!(
        serde_json::to_value(db.audit_facets().unwrap()).unwrap(),
        m["audit_facets"]
    );
}

fn ownership(tag: &str) {
    let m = manifest(&format!("{tag}.json"));
    let (_dir, path) = copy_fixture(&format!("{tag}.db"));
    let db = open(&path);
    for (p, owner) in m["owners"].as_object().unwrap() {
        assert_eq!(
            db.get_volume_owner(p).unwrap().as_deref(),
            owner.as_str(),
            "{p}"
        );
    }
    for c in m["can_delete"].as_array().unwrap() {
        let got = db.can_user_delete_library_path(s(&c[0]), s(&c[1])).unwrap();
        assert_eq!(got, c[2].as_bool().unwrap(), "{c}");
    }
    for c in m["can_edit"].as_array().unwrap() {
        let got = db.can_user_edit_series(s(&c[0]), s(&c[1])).unwrap();
        assert_eq!(got, c[2].as_bool().unwrap(), "{c}");
    }
    for (user, series) in m["owned_series"].as_object().unwrap() {
        assert_eq!(
            json!(db.list_series_owned_by(user).unwrap()),
            *series,
            "{user}"
        );
    }
}

fn ocr_and_identities(tag: &str) {
    let m = manifest(&format!("{tag}.json"));
    let (_dir, path) = copy_fixture(&format!("{tag}.db"));
    let db = open(&path);
    assert_eq!(
        serde_json::to_value(db.list_ocr_sidecars().unwrap()).unwrap(),
        m["ocr_sidecars"]
    );
    assert_eq!(
        json!(db.ocr_sidecar_producers().unwrap()),
        m["ocr_producers"]
    );
    for (p, uuid) in m["identities"].as_object().unwrap() {
        assert_eq!(
            db.remembered_volume_uuid(p).unwrap().as_deref(),
            uuid.as_str(),
            "{p}"
        );
    }
}

fn series_tables_read_back_and_rewrite_byte_identically(tag: &str) {
    let m = manifest(&format!("{tag}.json"));
    let (_dir, path) = copy_fixture(&format!("{tag}.db"));
    let db = open(&path);
    for want in m["series_facts"].as_array().unwrap() {
        let got = db
            .get_series_facts(s(&want["series_key"]))
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::to_value(&got).unwrap(), *want);
        // Write what we read back: the stored JSON text must not change.
        let row: SeriesFacts = got.clone();
        db.put_series_facts(&row).unwrap();
    }
    let conn = rusqlite::Connection::open(&path).unwrap();
    let raw: Vec<Value> = conn
        .prepare(
            "SELECT series_key, external_ids, titles, synonyms, volume_offsets, \
             typeof(spine_offset) FROM series_facts ORDER BY series_key",
        )
        .unwrap()
        .query_map([], |r| {
            Ok(json!([
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?
            ]))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(Value::Array(raw), m["series_facts_raw"]);

    for e in m["entry_cache"].as_array().unwrap() {
        let key = s(&e["volume_key"]);
        let size = e["cbz_size"].as_i64().unwrap();
        let mtime = e["cbz_mtime"].as_f64().unwrap();
        let got = db
            .get_cached_volume_entry(key, size, mtime, s(&e["sidecar_key"]))
            .unwrap()
            .unwrap();
        assert_eq!(Value::Object(got.clone()), e["entry"]);
        assert!(
            db.get_cached_volume_entry(key, size, mtime + 1e-6, s(&e["sidecar_key"]))
                .unwrap()
                .is_none()
        );
        db.put_cached_volume_entry(key, "x", &got, size, mtime, s(&e["sidecar_key"]))
            .unwrap();
        let raw: String = conn
            .query_row(
                "SELECT entry_json FROM series_entry_cache WHERE volume_key = ?",
                [key],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, s(&m["entry_cache_raw"][key]), "{key}");
    }

    // The catalog: 0.5.3 reads `catalog_folders` only. A 0.5.2 file's rows are in
    // `catalog_series`, which is left exactly as it was (the first pass rebuilds the folder
    // rows; the listing falls back to the scanning index until then).
    let (catalog, legacy) = if tag == "py052" {
        let rows = |v: &Value| -> Value {
            Value::Array(
                v.as_array()
                    .unwrap()
                    .iter()
                    .map(|r| {
                        json!([
                            r["series_key"],
                            r["folder_name"],
                            r["volume_count"],
                            r["total_pages"]
                        ])
                    })
                    .collect(),
            )
        };
        (
            manifest("upgrade053.json")["py052"]["catalog"].clone(),
            rows(&m["catalog_series"]),
        )
    } else {
        (
            m["catalog_series"].clone(),
            m["legacy_catalog_series"].clone(),
        )
    };
    assert_eq!(
        serde_json::to_value(db.list_catalog_series().unwrap()).unwrap(),
        catalog
    );
    let legacy_sql = "SELECT series_key, folder_name, volume_count, total_pages \
                      FROM catalog_series ORDER BY series_key";
    assert_eq!(query(&conn, legacy_sql), legacy);
    let community = db.list_community_details().unwrap();
    assert_eq!(
        serde_json::to_value(&community).unwrap(),
        m["community_details"]
    );
    let row: CommunityDetails = community[0].clone();
    db.upsert_community_details(&row).unwrap();
    let raw: (String, String) = conn
        .query_row("SELECT tags, genres FROM community_details", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(json!([raw.0, raw.1]), m["community_raw"]);
}

#[test]
fn a_legacy_database_is_migrated_as_python_053_migrates_it() {
    let m = manifest("upgrade053.json")["legacy"].clone();
    let password = manifest("legacy.json")["password"].clone();
    let (_dir, path) = copy_fixture("legacy.db");
    let db = open(&path);
    let scribe = db
        .authenticate_user("scribe", s(&password))
        .unwrap()
        .unwrap();
    assert_eq!(scribe.role, bunko_core::Role::Uploader);
    assert_eq!(
        db.get_invite("legacyCode1").unwrap().unwrap().role,
        bunko_core::Role::Uploader
    );
    drop(db);
    assert_eq!(schema_rows(&path), m["schema"]);

    let conn = rusqlite::Connection::open(&path).unwrap();
    let q = |sql: &str| query(&conn, sql);
    let c = &m["contents"];
    assert_eq!(q("SELECT version FROM schema_version"), c["schema_version"]);
    assert_eq!(
        q("SELECT id, username, role, status, notes FROM users ORDER BY id"),
        c["users"]
    );
    assert_eq!(
        q("SELECT code, role, invited_by, expires_at FROM invites ORDER BY id"),
        c["invites"]
    );
    // 0.5.2 gave the legacy table its damage columns; 0.5.3 leaves it as it was.
    assert_eq!(
        q("SELECT series_key, folder_name, volume_count FROM catalog_series ORDER BY series_key"),
        c["catalog_series"]
    );
    assert_eq!(
        q("SELECT series_key, folder_name, volume_count FROM catalog_folders ORDER BY folder_name"),
        c["catalog_folders"]
    );
    assert_eq!(
        q("SELECT volume_key, volume_uuid FROM volume_identities ORDER BY volume_key"),
        c["volume_identities"]
    );
    drop(conn);

    // Idempotent: opening again changes nothing.
    drop(open(&path));
    assert_eq!(schema_rows(&path), m["schema"]);
}

/// A `Kingdom/` after 0.5.3's case merge: `kingdom/` merged into `Kingdom/`, the rows
/// keyed by library path prefix-renamed (volume 13 keeps its uploader and its uuid), and
/// both case-variant folders' catalog rows still there until the next pass prunes the
/// folder that is gone.
#[test]
fn a_case_merged_library_reads_back_as_python_053_left_it() {
    let m = manifest("py053.json");
    let (_dir, path) = copy_fixture("py053.db");
    let db = open(&path);
    for (p, owner) in m["kingdom_owners"].as_object().unwrap() {
        assert_eq!(
            db.get_volume_owner(p).unwrap().as_deref(),
            owner.as_str(),
            "{p}"
        );
    }
    for (p, uuid) in m["kingdom_identities"].as_object().unwrap() {
        assert_eq!(
            db.remembered_volume_uuid(p).unwrap().as_deref(),
            uuid.as_str(),
            "{p}"
        );
    }
    for c in m["kingdom_can_delete"].as_array().unwrap() {
        let got = db.can_user_delete_library_path(s(&c[0]), s(&c[1])).unwrap();
        assert_eq!(got, c[2].as_bool().unwrap(), "{c}");
    }
    for c in m["kingdom_can_edit"].as_array().unwrap() {
        let got = db.can_user_edit_series(s(&c[0]), s(&c[1])).unwrap();
        assert_eq!(got, c[2].as_bool().unwrap(), "{c}");
    }
    assert_eq!(
        db.get_volume_owner("Kingdom/第13巻.cbz")
            .unwrap()
            .as_deref(),
        Some("alice")
    );
    let kingdom: Vec<(String, i64)> = db
        .list_catalog_series()
        .unwrap()
        .into_iter()
        .filter(|r| r.series_key == "kingdom")
        .map(|r| (r.folder_name, r.volume_count))
        .collect();
    assert_eq!(
        kingdom,
        [("Kingdom".to_string(), 12), ("kingdom".to_string(), 1)]
    );

    // The next pass finds only `Kingdom/` (13 volumes now) and prunes by folder name.
    db.upsert_catalog_series(&bunko_db::CatalogSeries {
        series_key: "kingdom".into(),
        folder_name: "Kingdom".into(),
        volume_count: 13,
        ..Default::default()
    })
    .unwrap();
    let keep: std::collections::HashSet<String> = ["Dr Stone", "Kingdom", "Mixed"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(db.prune_catalog_series(&keep).unwrap(), 1);
    let kingdom: Vec<(String, i64)> = db
        .list_catalog_series()
        .unwrap()
        .into_iter()
        .filter(|r| r.series_key == "kingdom")
        .map(|r| (r.folder_name, r.volume_count))
        .collect();
    assert_eq!(kingdom, [("Kingdom".to_string(), 13)]);
    // 0.5.2's table still holds what 0.5.2 last wrote, for a rollback.
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        query(
            &conn,
            "SELECT series_key, folder_name, volume_count, total_pages FROM catalog_series"
        ),
        m["legacy_catalog_series"]
    );
}
