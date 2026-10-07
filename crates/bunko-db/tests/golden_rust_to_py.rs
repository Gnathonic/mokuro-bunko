//! Rust -> Python: build a database with bunko-db, then let the Python `Database` class
//! open and check it (`tests/golden/check_rust_db.py`), as a rollback would; then read
//! back what Python wrote into it.
//!
//! Two rollbacks: to 0.5.3 (what prod ran before 0.7; the schema must be byte-identical)
//! and on to 0.5.2 (0.5.3's own rollback: 0.5.2 adds its `catalog_series` table and must
//! read everything else).
//!
//! Needs the reference interpreters: `$BUNKO_REF_PYTHON` (0.5.3), else
//! `~/.cache/mokuro-bunko-demo/ref053/bin/python`; `$BUNKO_REF052_PYTHON`, else
//! `~/.cache/mokuro-bunko-demo/ref052/bin/python`. Without one its test is skipped with a
//! message, unless `BUNKO_REQUIRE_REF_PYTHON=1` (then it fails).

use bunko_core::Role;
use bunko_db::{
    AuditDetails, AuditQuery, CatalogSeries, CommunityDetails, Database, DbOptions, NewAuditEvent,
    OcrSidecar, SeriesFacts, TokenKind, UserStatus,
};
use serde_json::{Map, Number, Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

fn ref_python(var: &str, env: &str) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(var) {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    let p = Path::new(&home).join(format!(".cache/mokuro-bunko-demo/{env}/bin/python"));
    p.exists().then_some(p)
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => panic!("not an object"),
    }
}

const USERS: &[(&str, &str, Role, UserStatus, &str)] = &[
    ("root", "password123", Role::Admin, UserStatus::Active, ""),
    (
        "alice",
        "p\u{e4}ssw\u{f6}rd-\u{65e5}\u{672c}\u{8a9e}",
        Role::Uploader,
        UserStatus::Active,
        "\u{30ce}\u{30fc}\u{30c8}",
    ),
    (
        "longpw",
        "\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}",
        Role::Registered,
        UserStatus::Active,
        "",
    ),
    (
        "nulpw",
        "abc\0defghij",
        Role::Editor,
        UserStatus::Active,
        "",
    ),
    (
        "proc1",
        "processor-pass",
        Role::Processor,
        UserStatus::Active,
        "",
    ),
    (
        "pend",
        "pending-pass",
        Role::Registered,
        UserStatus::Pending,
        "",
    ),
    (
        "dis",
        "disabled-pass",
        Role::Registered,
        UserStatus::Disabled,
        "",
    ),
    (
        "gone",
        "deleted-pass",
        Role::Registered,
        UserStatus::Active,
        "",
    ),
];

#[test]
fn python_053_reads_and_writes_a_rust_database() {
    reads_and_writes("BUNKO_REF_PYTHON", "ref053", "0.5.3");
}

#[test]
fn python_052_reads_and_writes_a_rust_database() {
    reads_and_writes("BUNKO_REF052_PYTHON", "ref052", "0.5.2");
}

fn reads_and_writes(var: &str, env: &str, version: &str) {
    let Some(python) = ref_python(var, env) else {
        assert!(
            std::env::var_os("BUNKO_REQUIRE_REF_PYTHON").is_none(),
            "BUNKO_REQUIRE_REF_PYTHON is set but no {version} reference interpreter was found"
        );
        eprintln!("SKIPPED: no Python {version} reference interpreter (set {var})");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mokuro.db");
    let db = Database::open_with(&path, &DbOptions::default()).unwrap();
    let mut m = Map::new();

    // -- users / tokens / invites ----------------------------------------------
    let mut users = Vec::new();
    for (name, pw, role, status, notes) in USERS {
        db.create_user(name, pw, *role, *status, notes).unwrap();
    }
    db.delete_user("gone").unwrap();
    for (name, pw, ..) in USERS {
        let user = db.get_user(name).unwrap().unwrap();
        users.push(json!({"username": name, "password": pw, "expected": user}));
    }
    m.insert("users".into(), Value::Array(users));
    let long36 = "\u{e9}".repeat(36);
    m.insert(
        "authenticates".into(),
        json!([
            ["root", USERS[0].1],
            ["alice", USERS[1].1],
            ["longpw", USERS[2].1],
            ["longpw", long36],
            ["nulpw", USERS[3].1],
            ["proc1", USERS[4].1]
        ]),
    );
    m.insert(
        "refuses".into(),
        json!([
            ["root", "password124"],
            ["longpw", "\u{e9}".repeat(35)],
            ["nulpw", "abc"],
            ["pend", "pending-pass"],
            ["dis", "disabled-pass"],
            ["gone", "deleted-pass"]
        ]),
    );
    let mut stamps = Map::new();
    for name in ["proc1", "root"] {
        stamps.insert(
            name.into(),
            json!(db.processor_account_stamp(name).unwrap()),
        );
    }
    m.insert("processor_stamps".into(), Value::Object(stamps));

    let mut tokens = Vec::new();
    for (name, kind, life, resolves) in [
        ("root", TokenKind::Web, None, Some("root")),
        ("alice", TokenKind::Reader, None, Some("alice")),
        ("proc1", TokenKind::Processor, None, Some("proc1")),
        ("alice", TokenKind::Web, Some(-10.0), None),
        ("dis", TokenKind::Web, None, None),
    ] {
        let (token, _) = db
            .create_auth_token(name, kind, "label \u{65e5}", life)
            .unwrap();
        tokens.push(json!({"token": token, "resolves_to": resolves}));
    }
    m.insert("tokens".into(), Value::Array(tokens));

    let valid = db
        .create_invite(Role::Uploader, "520w", Some("root"))
        .unwrap();
    let used = db.create_invite(Role::Registered, "1d", None).unwrap();
    assert!(db.use_invite(&used, "alice").unwrap());
    let invites: Vec<Value> = [&valid, &used]
        .iter()
        .map(|c| {
            json!({"code": c, "valid": db.validate_invite(c).unwrap().is_some(),
                   "info": db.invite_info(c).unwrap().unwrap()})
        })
        .collect();
    m.insert("invites".into(), Value::Array(invites));

    // -- audit -------------------------------------------------------------------
    let mut detail_events = Vec::new();
    for (id, details) in db
        .list_audit_events(100, None)
        .unwrap()
        .iter()
        .map(|e| (e.id, e.details.clone()))
    {
        // invite rows: their details were written by Rust; Python re-derives the text.
        let pairs: Option<Value> = details.map(|d| {
            let v: Value = serde_json::from_str(&d).unwrap();
            // serde_json maps are sorted; recover Rust's insertion order from the text.
            let order: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            let mut keys: Vec<(usize, String)> = order
                .into_iter()
                .map(|k| (d.find(&format!("\"{k}\":")).unwrap(), k))
                .collect();
            keys.sort();
            Value::Array(keys.into_iter().map(|(_, k)| json!([k, v[&k]])).collect())
        });
        detail_events.push(json!({"id": id, "pairs": pairs}));
    }
    let vectors: Vec<Vec<(&str, Value)>> = vec![
        vec![("existed_before", json!(false))],
        vec![
            ("name", json!("\u{6f22}\u{5b57} \u{1F600}")),
            ("n", json!(3)),
            ("f", json!(1e-5)),
            ("big", json!(1e16)),
            ("ok", json!(true)),
            ("none", Value::Null),
            ("q", json!("a\"b\\c\u{7f}\n\t\u{1}")),
        ],
        vec![
            ("destination", json!("/mokuro-reader/S/V2.cbz")),
            ("path", json!("100%_sure")),
        ],
        vec![
            ("zeta", json!(1)),
            ("alpha", json!(2.5)),
            ("mid", json!([1, "\u{e9}"])),
        ],
    ];
    let specs = [
        (
            "upload",
            Some("alice"),
            Some("library"),
            Some("/mokuro-reader/S/V1.cbz"),
            Some(0),
        ),
        (
            "edit",
            Some("bob"),
            Some("progress"),
            Some("/mokuro-reader/volume-data.json"),
            None,
        ),
        (
            "delete",
            Some("alice"),
            Some("library"),
            Some("/mokuro-reader/S/V2.cbz"),
            Some(1),
        ),
        (
            "move",
            Some("root"),
            Some("library_folder"),
            Some("/mokuro-reader/S"),
            Some(2),
        ),
        (
            "ocr_sidecar_written",
            Some("proc1"),
            Some("sidecar"),
            None,
            Some(3),
        ),
        ("mkdir", None, None, Some("/x"), None),
    ];
    for (action, actor, ttype, tpath, vector) in specs {
        let mut ev = NewAuditEvent::new(action).actor(actor);
        ev.target_type = ttype;
        ev.target_path = tpath;
        let pairs = vector.map(|i| vectors[i].clone());
        if let Some(pairs) = &pairs {
            ev = ev.details(pairs.iter().cloned().collect::<AuditDetails>());
        }
        let id = db.log_audit_event(&ev).unwrap();
        detail_events.push(json!({"id": id, "pairs": pairs.map(|p| p.into_iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>())}));
    }
    m.insert("detail_events".into(), Value::Array(detail_events));
    let mut queries = Vec::new();
    for q in [
        json!({}),
        json!({"include_progress": true}),
        json!({"actor": "alice"}),
        json!({"search": "\u{6f22}\u{5b57}"}),
        json!({"search": "100%_"}),
        json!({"limit": 2}),
        json!({"limit": 3, "include_progress": true, "search": "e"}),
    ] {
        let mut query = AuditQuery {
            actor: q.get("actor").and_then(Value::as_str).map(str::to_string),
            search: q.get("search").and_then(Value::as_str).map(str::to_string),
            include_progress: q
                .get("include_progress")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            limit: q.get("limit").and_then(Value::as_i64).unwrap_or(50),
            ..AuditQuery::default()
        };
        let mut pages = Vec::new();
        loop {
            let page = db.query_audit_events(&query).unwrap();
            let ids: Vec<i64> = page.events.iter().map(|e| e.id).collect();
            pages.push(json!({"ids": ids, "next_cursor": page.next_cursor, "total": page.total}));
            match page.next_cursor {
                Some(c) => query.cursor = Some(c),
                None => break,
            }
        }
        queries.push(json!({"query": q, "pages": pages}));
    }
    m.insert("audit_queries".into(), Value::Array(queries));
    m.insert(
        "audit_facets".into(),
        serde_json::to_value(db.audit_facets().unwrap()).unwrap(),
    );

    // -- ownership / OCR / identities ---------------------------------------------
    for (rel, user) in [
        ("Dr Stone/V1.cbz", "alice"),
        ("Mixed/V1.cbz", "alice"),
        ("Mixed/V2.cbz", "root"),
        ("Cafe\u{301}/V1.cbz", "alice"),
        ("Dr Stone/V1.mokuro", "root"),
        ("Old/V1.cbz", "root"),
    ] {
        db.record_volume_upload(rel, user).unwrap();
    }
    db.rename_volume_upload("Old/V1.cbz", "Old/Vol 1.cbz")
        .unwrap();
    let mut owners = Map::new();
    for p in [
        "Dr Stone/V1.webp",
        "Old/V1.cbz",
        "Old/Vol 1.cbz",
        "Untracked/V1.cbz",
    ] {
        owners.insert(p.into(), json!(db.get_volume_owner(p).unwrap()));
    }
    m.insert("owners".into(), Value::Object(owners));
    let deletes: Vec<Value> = [
        ("alice", "/mokuro-reader/Dr Stone/V1.cbz"),
        ("alice", "/mokuro-reader/Dr Stone/V1.hayai-nova.mokuro.gz"),
        ("root", "/mokuro-reader/Dr Stone/V1.cbz"),
        ("alice", "/mokuro-reader/Dr Stone"),
    ]
    .iter()
    .map(|(u, p)| json!([u, p, db.can_user_delete_library_path(u, p).unwrap()]))
    .collect();
    m.insert("can_delete".into(), Value::Array(deletes));
    let edits: Vec<Value> = [
        ("alice", "dr  STONE"),
        ("alice", "Mixed"),
        ("alice", "Caf\u{e9}"),
        ("alice", "Dr_Stone"),
    ]
    .iter()
    .map(|(u, s)| json!([u, s, db.can_user_edit_series(u, s).unwrap()]))
    .collect();
    m.insert("can_edit".into(), Value::Array(edits));
    let mut owned = Map::new();
    for u in ["alice", "root"] {
        owned.insert(u.into(), json!(db.list_series_owned_by(u).unwrap()));
    }
    m.insert("owned_series".into(), Value::Object(owned));

    db.record_ocr_sidecar(&OcrSidecar {
        sidecar_path: "Dr Stone/V1.mokuro".into(),
        volume_key: "Dr Stone/V1.cbz".into(),
        generation_id: "g1".into(),
        generation_name: "Hayai".into(),
        machine: "local".into(),
        engine: Some("hayai-nova".into()),
        detector: Some("ctd".into()),
        pages: Some(20),
        archive_mtime_ns: Some(1_700_000_000_123_456_789),
        ..Default::default()
    })
    .unwrap();
    m.insert(
        "ocr_sidecars".into(),
        serde_json::to_value(db.list_ocr_sidecars().unwrap()).unwrap(),
    );
    m.insert(
        "ocr_producers".into(),
        json!(db.ocr_sidecar_producers().unwrap()),
    );
    db.remember_volume_uuid("Dr Stone/V1.mokuro", "uuid-v1")
        .unwrap();

    // -- series tables ------------------------------------------------------------
    let facts = [
        SeriesFacts {
            series_key: "dr stone".into(),
            series_title: "Dr. STONE \u{30c9}\u{30af}".into(),
            external_ids: obj(json!({"mal": 104, "anilist": 98416})),
            titles: obj(json!({"ja": "\u{30c9}\u{30af}", "en": "Dr. \"Stone\""})),
            synonyms: vec![json!("DS"), json!("\u{1F600}")],
            tag: Some("shounen".into()),
            unit: None,
            facts_updated_at: "2026-09-01T00:00:00.000Z".into(),
            spine_offset: Some(Number::from(-40)),
            volume_offsets: obj(json!({"V1.cbz": 2.5, "V2.cbz": -3, "V3.cbz": 1e-7})),
            updated_by: Some("alice".into()),
            updated_at: String::new(),
        },
        SeriesFacts {
            series_key: "float".into(),
            series_title: "Float".into(),
            facts_updated_at: "1970-01-01T00:00:00.000Z".into(),
            spine_offset: Number::from_f64(2.5),
            ..Default::default()
        },
    ];
    let mut facts_out = Vec::new();
    for f in &facts {
        db.put_series_facts(f).unwrap();
        facts_out.push(
            serde_json::to_value(db.get_series_facts(&f.series_key).unwrap().unwrap()).unwrap(),
        );
    }
    m.insert("series_facts".into(), Value::Array(facts_out));
    let entries = [
        (
            "Dr Stone/V2.cbz",
            json!({"volume_uuid": "uuid-v2", "mokuro_sha256": "ab", "title": "V2 \u{e9}", "pages": [1, 2.5, null]}),
            1_048_576_i64,
            1_700_000_000.123456_f64,
            "V2.mokuro:1:2",
        ),
        (
            "Mixed/V1.cbz",
            json!({"volume_uuid": "derived"}),
            7,
            2.5,
            "",
        ),
    ];
    let mut entries_out = Vec::new();
    for (key, entry, size, mtime, sidecar) in &entries {
        db.put_cached_volume_entry(key, "s", &obj(entry.clone()), *size, *mtime, sidecar)
            .unwrap();
        entries_out.push(json!({"volume_key": key, "cbz_size": size, "cbz_mtime": mtime, "sidecar_key": sidecar, "entry": entry}));
    }
    m.insert("entry_cache".into(), Value::Array(entries_out));
    let mut identities = Map::new();
    for p in ["Dr Stone/V1.cbz", "Dr Stone/V2.cbz", "Mixed/V1.cbz"] {
        identities.insert(p.into(), json!(db.remembered_volume_uuid(p).unwrap()));
    }
    m.insert("identities".into(), Value::Object(identities));
    db.upsert_catalog_series(&CatalogSeries {
        series_key: "dr stone".into(),
        folder_name: "Dr Stone".into(),
        cover_path: Some("Dr Stone/V1.webp".into()),
        volume_count: 3,
        latest_volume_modified: 1_700_000_000.5,
        total_pages: 600,
        total_chars: 123_456,
        missing_pages: 2,
        damaged_volumes: 1,
    })
    .unwrap();
    // Case-variant folders sharing a series key: one row each (0.5.3).
    for (folder, volumes) in [("Kingdom", 79), ("kingdom", 1)] {
        db.upsert_catalog_series(&CatalogSeries {
            series_key: "kingdom".into(),
            folder_name: folder.into(),
            volume_count: volumes,
            ..Default::default()
        })
        .unwrap();
    }
    m.insert(
        "catalog_series".into(),
        serde_json::to_value(db.list_catalog_series().unwrap()).unwrap(),
    );
    db.upsert_community_details(&CommunityDetails {
        series_key: "dr stone".into(),
        score: Some(8.25),
        tags: vec![json!("Time Skip"), json!("\u{65e5}\u{672c}")],
        genres: vec![json!("Sci-Fi")],
        source: "anilist".into(),
        fetched_at: "2026-09-01T00:00:00Z".into(),
    })
    .unwrap();
    m.insert(
        "community_details".into(),
        serde_json::to_value(db.list_community_details().unwrap()).unwrap(),
    );
    drop(db);

    // -- hand over to Python ---------------------------------------------------------
    let manifest = dir.path().join("manifest.json");
    std::fs::write(
        &manifest,
        serde_json::to_string_pretty(&Value::Object(m)).unwrap(),
    )
    .unwrap();
    let out = dir.path().join("out.json");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/check_rust_db.py");
    let result = Command::new(&python)
        .arg(&script)
        .arg(&path)
        .arg(&manifest)
        .arg(&out)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        result.status.success(),
        "python check failed:\n{stdout}\n{stderr}"
    );
    eprintln!("{stdout}");

    // -- and back: what the rolled-back server wrote ------------------------------------
    let back: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let db = Database::open_with(&path, &DbOptions::default()).unwrap();
    let py = db
        .authenticate_user("pyuser", "py-password1")
        .unwrap()
        .unwrap();
    assert_eq!(py.role, Role::Uploader);
    assert_eq!(py.notes, "from python");
    let inv = db
        .validate_invite(back["invite_code"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(inv.role, Role::Editor);
    assert_eq!(inv.invited_by.as_deref(), Some("pyuser"));
    assert_eq!(
        db.resolve_auth_token(back["token"].as_str().unwrap())
            .unwrap()
            .unwrap()
            .username,
        "pyuser"
    );
    let ev = db
        .list_audit_events(1000, None)
        .unwrap()
        .into_iter()
        .find(|e| e.id == back["event_id"].as_i64().unwrap())
        .unwrap();
    assert_eq!(ev.details.as_deref(), back["event_details"].as_str());
    let facts = db.get_series_facts("py").unwrap().unwrap();
    assert_eq!(facts.spine_offset, Some(Number::from(-40)));
    assert_eq!(facts.titles["ja"], "\u{30d1}\u{30a4}");
}
