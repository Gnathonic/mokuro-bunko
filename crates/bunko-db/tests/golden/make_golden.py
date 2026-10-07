"""Generate the Python golden fixtures for bunko-db's cross-compat tests.

What it writes depends on the `mokuro_bunko` it imports:

    ~/.cache/mokuro-bunko-demo/ref052/bin/python crates/bunko-db/tests/golden/make_golden.py
    ~/.cache/mokuro-bunko-demo/ref053/bin/python crates/bunko-db/tests/golden/make_golden.py

Under 0.5.2, next to this script:
- py052.db / py052.json: a database populated through Python's `Database` class, and a
  manifest of what Rust must find in it (plaintext passwords, tokens and invite codes,
  expected rows, audit query results with cursors, the schema text).
- legacy.db / legacy.json: a hand-built pre-0.3 database (no `notes`/`invited_by`/
  damage columns, no tokens/OCR/identity tables, `writer` roles, schema_version 2), and
  what Python's `Database()` makes of a copy of it (schema + migrated rows).

Under 0.5.3 (the version bunko-db now tracks: the catalog is `catalog_folders`, keyed by
folder, and `catalog_series` is neither created nor touched):
- py053.db / py053.json: the same population through 0.5.3, plus data shaped like prod
  after the 0.5.3 cutover (case-variant `Kingdom`/`kingdom` catalog rows sharing a series
  key, rows prefix-renamed from `kingdom/` to `Kingdom/`, the 0.5.2 `catalog_series`
  table left behind with its last row) and the schema of a FRESH 0.5.3 database.
- upgrade053.json: what 0.5.3's `Database()` makes of copies of the checked-in py052.db
  and legacy.db (schema + rows), which is what Rust must make of them too.

The fixtures are checked in; regenerate only when the Python side changes. Secrets in
them are random test values.
"""

from __future__ import annotations

import json
import shutil
import sqlite3
import sys
import tempfile
from datetime import datetime, timedelta
from pathlib import Path

import bcrypt

from mokuro_bunko import __version__ as VERSION
from mokuro_bunko.database import Database
from mokuro_bunko.registration.invites import InviteManager

HERE = Path(__file__).resolve().parent

USERS = [
    # username, password, role, status, notes
    ("root", "password123", "admin", "active", ""),
    ("alice", "pässwörd-日本語", "uploader", "active", "ノート"),
    ("longpw", "é" * 50, "registered", "active", ""),  # 100 bytes: bcrypt keeps 72
    ("nulpw", "abc\x00defghij", "editor", "active", ""),
    ("proc1", "processor-pass", "processor", "active", ""),
    ("inv1", "inviter-pass", "inviter", "active", ""),
    ("pend", "pending-pass", "registered", "pending", ""),
    ("dis", "disabled-pass", "registered", "disabled", ""),
    ("gone", "deleted-pass", "registered", "active", ""),  # deleted below
]

# Passwords that must NOT authenticate (and the 72-byte prefix that must).
WRONG = [
    ("root", "password124"),
    ("longpw", "é" * 35),
    ("nulpw", "abc"),
    ("pend", "pending-pass"),
    ("dis", "disabled-pass"),
    ("gone", "deleted-pass"),
    ("nobody", "password123"),
]
RIGHT_EXTRA = [("longpw", "é" * 36)]


def schema_rows(path: Path) -> list[list]:
    conn = sqlite3.connect(path)
    try:
        rows = conn.execute(
            "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY rowid"
        ).fetchall()
    finally:
        conn.close()
    return [list(r) for r in rows]


def checkpoint(db: Database) -> None:
    """Fold the WAL into the main file so the fixture is one self-contained file."""
    db._conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    db._conn.close()


def page_json(page: dict) -> dict:
    return {
        "ids": [e["id"] for e in page["events"]],
        "next_cursor": page["next_cursor"],
        "total": page["total"],
    }


def make_python_db(tag: str) -> None:
    path = HERE / f"py{tag}.db"
    for suffix in ("", "-wal", "-shm"):
        Path(str(path) + suffix).unlink(missing_ok=True)
    db = Database(path)
    manifest: dict = {"generator": f"mokuro-bunko {VERSION}", "bcrypt": bcrypt.__version__}

    # -- users ---------------------------------------------------------------
    for username, password, role, status, notes in USERS:
        db.create_user(username, password, role, status=status, notes=notes)
    db.delete_user("gone")
    # A `$2y$` spelling of a `$2b$` hash (PHP-made hashes look like this).
    y_hash = bcrypt.hashpw(b"ypassword1", bcrypt.gensalt()).decode().replace("$2b$", "$2y$", 1)
    with db._connection() as conn:
        conn.execute(
            "INSERT INTO users (username, password_hash, role) VALUES ('yuser', ?, 'registered')",
            (y_hash,),
        )
    manifest["users"] = [
        {"username": u, "password": p, "expected": db.get_user(u)}
        for (u, p, *_rest) in USERS
    ] + [{"username": "yuser", "password": "ypassword1", "expected": db.get_user("yuser")}]
    manifest["authenticates"] = [
        [u, p] for (u, p, _r, st, _n) in USERS if st == "active" and u != "gone"
    ] + [list(x) for x in RIGHT_EXTRA] + [["yuser", "ypassword1"]]
    for u, p in manifest["authenticates"]:
        assert db.authenticate_user(u, p) is not None, u
    manifest["refuses"] = [list(x) for x in WRONG]
    for u, p in WRONG:
        assert db.authenticate_user(u, p) is None, u
    manifest["processor_stamps"] = {
        u: db.processor_account_stamp(u) for u in ("proc1", "root", "nobody")
    }
    manifest["user_list"] = [u["username"] for u in db.list_users()]

    # -- tokens --------------------------------------------------------------
    tokens = []
    for username, kind, label, lifetime, resolves in [
        ("root", "web", "", None, "root"),
        ("alice", "reader", "Mokuro Reader 日本", None, "alice"),
        ("proc1", "processor", "gpu-box", None, "proc1"),
        ("alice", "web", "", -10.0, None),
        ("dis", "web", "", None, None),
    ]:
        token, expires_at = db.create_auth_token(
            username, kind, label=label, lifetime_seconds=lifetime
        )
        tokens.append(
            {"token": token, "username": username, "kind": kind, "label": label,
             "expires_at": expires_at, "resolves_to": resolves}
        )
        got = db.resolve_auth_token(token)
        assert (got["username"] if got else None) == resolves
    manifest["tokens"] = tokens

    # -- invites -------------------------------------------------------------
    mgr = InviteManager(db)
    valid = db.create_invite("uploader", "520w", invited_by="inv1")
    used = db.create_invite("registered", "520w", invited_by="root")
    expired = db.create_invite("editor", "1h", invited_by=None)
    assert db.use_invite(used, "alice")
    past = (datetime.now() - timedelta(days=3)).isoformat()
    with db._connection() as conn:
        conn.execute("UPDATE invites SET expires_at = ? WHERE code = ?", (past, expired))
    manifest["invites"] = [
        {"code": c, "valid": db.validate_invite(c) is not None, "info": mgr.get_info(c)}
        for c in (valid, used, expired)
    ]

    # -- audit ---------------------------------------------------------------
    detail_vectors = [
        [["existed_before", False]],
        [["name", "漢字 \U0001f600"], ["n", 3], ["f", 1e-05], ["big", 1e16],
         ["ok", True], ["none", None], ["q", 'a"b\\c\x7f\n\t']],
        [["destination", "/mokuro-reader/S/V2.cbz"], ["path", "100%_sure"]],
        [["generation", "Hayai"], ["generation_id", "g1"], ["machine", "local"],
         ["pages", 12], ["failed_pages", 0], ["seconds", 12.5]],
    ]
    manifest["detail_vectors"] = [
        {"pairs": pairs, "json": json.dumps(dict(pairs), separators=(",", ":"), ensure_ascii=True)}
        for pairs in detail_vectors
    ]
    events = [
        ("upload", "alice", "library", "/mokuro-reader/S/V1.cbz", None, detail_vectors[0]),
        ("edit", "bob", "progress", "/mokuro-reader/volume-data.json", None, None),
        ("delete", "alice", "library", "/mokuro-reader/S/V2.cbz", None, detail_vectors[1]),
        ("move", "root", "library_folder", "/mokuro-reader/S", None, detail_vectors[2]),
        ("ocr_sidecar_written", "proc1", "sidecar", "/mokuro-reader/S/V1.mokuro", None,
         detail_vectors[3]),
        ("admin_create_user", "root", "user", None, "alice", [["role", "uploader"]]),
        ("mkdir", None, None, "/x", None, None),
        ("edit", "alice", "progress", "/mokuro-reader/profiles.json", None, None),
    ]
    for action, actor, ttype, tpath, tuser, pairs in events:
        db.log_audit_event(
            action, actor_username=actor, target_type=ttype, target_path=tpath,
            target_username=tuser, details=dict(pairs) if pairs is not None else None,
        )
    # Fixed timestamps (two pairs share a second) so the paging is deterministic. The
    # Rust test never logs into this copy, so the 30-day prune never sees them.
    with db._connection() as conn:
        ids = [r[0] for r in conn.execute("SELECT id FROM audit_logs ORDER BY id")]
        for n, event_id in enumerate(ids):
            stamp = f"2026-09-{1 + n // 2:02d} {10 + n % 3:02d}:00:00"
            conn.execute("UPDATE audit_logs SET created_at = ? WHERE id = ?", (stamp, event_id))
    manifest["audit_rows"] = db.list_audit_events(1000)
    queries = [
        {},
        {"include_progress": True},
        {"actor": "alice"},
        {"actions": ["upload", "mkdir", ""]},
        {"target_types": ["progress"]},
        {"since": "2026-09-02", "until": "2026-09-04T10:00:00Z"},
        {"until": "2026-09-01T15:00:01+05:00", "include_progress": True},
        {"search": "ALICE"},
        {"search": "漢字"},
        {"search": "100%_"},
        {"search": "invite"},
        {"limit": 3},
        {"limit": 3, "include_progress": True, "search": "e"},
    ]
    results = []
    for q in queries:
        page = db.query_audit_events(**q)
        chain = [page_json(page)]
        while page["next_cursor"]:
            page = db.query_audit_events(**q, cursor=page["next_cursor"])
            chain.append(page_json(page))
        results.append({"query": q, "pages": chain})
    manifest["audit_queries"] = results
    errors = []
    for q in ({"since": "yesterday"}, {"until": "x" * 50}, {"cursor": "!!!"}):
        try:
            db.query_audit_events(**q)
        except ValueError as e:
            errors.append({"query": q, "error": str(e)})
    manifest["audit_errors"] = errors
    manifest["audit_facets"] = db.audit_facets()

    # -- ownership -----------------------------------------------------------
    for rel, user in [
        ("Dr Stone/V1.cbz", "alice"), ("Dr Stone/V2.cbz", "alice"),
        ("Mixed/V1.cbz", "alice"), ("Mixed/V2.cbz", "root"),
        ("Café/V1.cbz", "alice"), ("loose.cbz", "alice"),
        ("Dr Stone/V1.mokuro", "root"), ("Untracked/V1.mokuro", "root"),
        ("Old/V1.cbz", "root"),
    ]:
        db.record_volume_upload(rel, user)
    db.rename_volume_upload("Old/V1.cbz", "Old/Vol 1.cbz")
    owner_paths = ["Dr Stone/V1.cbz", "Dr Stone/V1.webp", "Untracked/V1.cbz", "Old/V1.cbz",
                   "Old/Vol 1.cbz", "Mixed/V2.mokuro.gz", "readme.txt"]
    manifest["owners"] = {p: db.get_volume_owner(p) for p in owner_paths}
    delete_checks = [
        ("alice", "/mokuro-reader/Dr Stone/V1.cbz"),
        ("alice", "/mokuro-reader/Dr Stone/V1.hayai-nova.mokuro.gz"),
        ("root", "/mokuro-reader/Dr Stone/V1.cbz"),
        ("alice", "/mokuro-reader/Dr Stone"),
        ("alice", "/mokuro-reader/Dr Stone/V01.5.mokuro"),
    ]
    manifest["can_delete"] = [[u, p, db.can_user_delete_library_path(u, p)] for u, p in delete_checks]
    edit_checks = [("alice", "dr  STONE"), ("alice", "Mixed"), ("root", "Mixed"),
                   ("alice", "Café"), ("alice", "Untracked"), ("alice", "Dr_Stone")]
    manifest["can_edit"] = [[u, s, db.can_user_edit_series(u, s)] for u, s in edit_checks]
    manifest["owned_series"] = {u: db.list_series_owned_by(u) for u in ("alice", "root", "x")}

    # -- OCR provenance / identities ------------------------------------------
    for row in [
        {"sidecar_path": "Dr Stone/V1.mokuro", "volume_key": "Dr Stone/V1.cbz",
         "generation_id": "g1", "generation_name": "Hayai", "machine": "local",
         "engine": "hayai-nova", "detector": "ctd", "precision": "fp16", "pages": 20,
         "failed_pages": 1, "archive_size": 123456789, "archive_mtime_ns": 1700000000123456789},
        {"sidecar_path": "Dr Stone/V1.paddle.mokuro.gz", "volume_key": "Dr Stone/V1.cbz",
         "generation_id": "g2", "generation_name": "Paddle", "machine": "gpu-box",
         "account": "proc1", "runner_build": "b1"},
    ]:
        db.record_ocr_sidecar(row)
    manifest["ocr_sidecars"] = db.list_ocr_sidecars()
    manifest["ocr_producers"] = [list(t) for t in db.ocr_sidecar_producers()]
    db.remember_volume_uuid("Dr Stone/V1.mokuro", "uuid-v1")

    # -- series facts / entry cache / catalog / community ----------------------
    # Keys in sorted order, so serde_json's (sorted) maps re-serialize byte-identically.
    facts = [
        {"series_key": "dr stone", "series_title": "Dr. STONE ドクター",
         "external_ids": {"anilist": 98416, "mal": 104}, "synonyms": ["DS", "\U0001f600"],
         "titles": {"en": 'Dr. "Stone"', "ja": "ドクター・ストーン"},
         "tag": "shounen", "unit": "volume", "facts_updated_at": "2026-09-01T00:00:00.000Z",
         "spine_offset": -40, "volume_offsets": {"V1.cbz": 2.5, "V2.cbz": -3},
         "updated_by": "alice", "updated_at": ""},
        {"series_key": "mixed", "series_title": "Mixed", "external_ids": {}, "titles": {},
         "synonyms": [], "tag": None, "unit": None,
         "facts_updated_at": "1970-01-01T00:00:00.000Z", "spine_offset": 2.5,
         "volume_offsets": {}, "updated_by": None, "updated_at": ""},
        {"series_key": "none", "series_title": "None", "external_ids": {}, "titles": {},
         "synonyms": [], "tag": None, "unit": None,
         "facts_updated_at": "1970-01-01T00:00:00.000Z", "spine_offset": None,
         "volume_offsets": {}, "updated_by": None, "updated_at": ""},
    ]
    for row in facts:
        db.put_series_facts(row)
    manifest["series_facts"] = [db.get_series_facts(r["series_key"]) for r in facts]
    with db._connection() as conn:
        manifest["series_facts_raw"] = [
            list(r) for r in conn.execute(
                "SELECT series_key, external_ids, titles, synonyms, volume_offsets, "
                "typeof(spine_offset) FROM series_facts ORDER BY series_key"
            )
        ]
    entries = [
        ("Dr Stone/V2.cbz", "dr stone",
         {"mokuro_sha256": "ab" * 32, "title": "V2 é", "volume_uuid": "uuid-v2"},
         1048576, 1700000000.123456, "V2.mokuro:123:456"),
        ("Dr Stone/V3.cbz", "dr stone",
         {"mokuro_size": 10, "mokuro_version": "0.2.1", "volume_uuid": "uuid-v3"},
         5, 1.0, ""),
        ("Mixed/V1.cbz", "mixed", {"pages": 3, "volume_uuid": "derived-uuid"}, 7, 2.5, "k"),
    ]
    for key, series, entry, size, mtime, sidecar in entries:
        db.put_cached_volume_entry(key, series, entry, size, mtime, sidecar)
    manifest["entry_cache"] = [
        {"volume_key": k, "cbz_size": s, "cbz_mtime": m, "sidecar_key": sk, "entry": e}
        for k, _series, e, s, m, sk in entries
    ]
    with db._connection() as conn:
        manifest["entry_cache_raw"] = {
            r[0]: r[1] for r in conn.execute("SELECT volume_key, entry_json FROM series_entry_cache")
        }
    for row in [
        {"series_key": "dr stone", "folder_name": "Dr Stone", "cover_path": "Dr Stone/V1.webp",
         "volume_count": 3, "latest_volume_modified": 1700000000.5, "total_pages": 600,
         "total_chars": 123456, "missing_pages": 2, "damaged_volumes": 1},
        {"series_key": "mixed", "folder_name": "Mixed", "cover_path": None, "volume_count": 2,
         "latest_volume_modified": 0, "total_pages": 0, "total_chars": 0, "missing_pages": 0,
         "damaged_volumes": 0},
    ]:
        db.upsert_catalog_series(row)
    manifest["catalog_series"] = db.list_catalog_series()
    db.upsert_community_details(
        {"series_key": "dr stone", "score": 8.25, "tags": ["Time Skip", "日本"],
         "genres": ["Sci-Fi"], "source": "anilist", "fetched_at": "2026-09-01T00:00:00Z"}
    )
    manifest["community_details"] = db.list_community_details()
    with db._connection() as conn:
        manifest["community_raw"] = list(
            conn.execute("SELECT tags, genres FROM community_details").fetchone()
        )
    manifest["identities"] = {
        p: db.remembered_volume_uuid(p)
        for p in ("Dr Stone/V1.cbz", "Dr Stone/V2.cbz", "Dr Stone/V3.cbz", "Mixed/V1.cbz")
    }

    if tag == "053":
        add_prod_shape_053(db, manifest)

    checkpoint(db)
    manifest["schema"] = schema_rows(path)
    if tag == "053":
        with tempfile.TemporaryDirectory() as tmp:
            fresh = Path(tmp) / "fresh.db"
            checkpoint(Database(fresh))
            manifest["fresh_schema"] = schema_rows(fresh)
    (HERE / f"py{tag}.json").write_text(json.dumps(manifest, indent=1, ensure_ascii=False) + "\n")


# 0.5.2's `catalog_series` DDL, byte for byte as 0.5.2 left it in prod's `sqlite_master`.
CATALOG_SERIES_052_DDL = """
                CREATE TABLE IF NOT EXISTS catalog_series (
                    series_key TEXT PRIMARY KEY,
                    folder_name TEXT NOT NULL,
                    cover_path TEXT,
                    volume_count INTEGER NOT NULL,
                    latest_volume_modified REAL NOT NULL DEFAULT 0,
                    total_pages INTEGER NOT NULL DEFAULT 0,
                    total_chars INTEGER NOT NULL DEFAULT 0,
                    missing_pages INTEGER NOT NULL DEFAULT 0,
                    damaged_volumes INTEGER NOT NULL DEFAULT 0,
                    scanned_at TEXT NOT NULL DEFAULT (datetime('now'))
                )
            """


def add_prod_shape_053(db: Database, manifest: dict) -> None:
    """Prod's database after the 0.5.3 cutover (2026-10-06).

    0.5.2 had kept ONE `catalog_series` row for `Kingdom/` + the stray `kingdom/` (the
    one-volume folder its pass reached last); 0.5.3 leaves that table alone and keeps one
    `catalog_folders` row per folder. The cutover merged `kingdom/` into `Kingdom/` and
    prefix-renamed the rows keyed by library path, as `rename_*` do.
    """
    with db._connection() as conn:
        conn.execute(CATALOG_SERIES_052_DDL)
        conn.execute(
            "INSERT INTO catalog_series (series_key, folder_name, volume_count, total_pages) "
            "VALUES ('kingdom', 'kingdom', 1, 200)"
        )
    for row in [
        {"series_key": "kingdom", "folder_name": "Kingdom", "cover_path": "Kingdom/第01巻.webp",
         "volume_count": 79, "latest_volume_modified": 1790000000.25, "total_pages": 15800,
         "total_chars": 1234567, "missing_pages": 0, "damaged_volumes": 0},
        {"series_key": "kingdom", "folder_name": "kingdom", "cover_path": None,
         "volume_count": 1, "latest_volume_modified": 1790100000.5, "total_pages": 200,
         "total_chars": 15000, "missing_pages": 0, "damaged_volumes": 0},
    ]:
        db.upsert_catalog_series(row)
    manifest["catalog_series"] = db.list_catalog_series()

    db.record_volume_upload("Kingdom/第01巻.cbz", "root")
    db.record_volume_upload("kingdom/第80巻.cbz", "alice")
    db.record_ocr_sidecar(
        {"sidecar_path": "kingdom/第80巻.mokuro", "volume_key": "kingdom/第80巻.cbz",
         "generation_id": "g1", "generation_name": "Hayai", "machine": "local",
         "engine": "hayai-nova", "pages": 200, "failed_pages": 0}
    )
    db.remember_volume_uuid("kingdom/第80巻.mokuro", "0aebfb59-0000-4000-8000-000000000080")
    db.remember_volume_uuid("Kingdom/第01巻.mokuro", "uuid-kingdom-01")
    # The cutover's merge of `kingdom/` into `Kingdom/`.
    db.rename_volume_upload("kingdom/第80巻.cbz", "Kingdom/第80巻.cbz")
    manifest["kingdom_renamed"] = {
        "ocr_sidecars": db.rename_ocr_sidecars_under_prefix("kingdom", "Kingdom"),
        "volume_identities": db.rename_volume_uuids_under_prefix("kingdom", "Kingdom"),
    }

    paths = ["Kingdom/第80巻.cbz", "kingdom/第80巻.cbz", "Kingdom/第80巻.mokuro",
             "Kingdom/第01巻.cbz", "KINGDOM/第01巻.cbz"]
    manifest["kingdom_owners"] = {p: db.get_volume_owner(p) for p in paths}
    manifest["kingdom_identities"] = {p: db.remembered_volume_uuid(p) for p in paths}
    manifest["kingdom_can_delete"] = [
        [u, p, db.can_user_delete_library_path(u, p)]
        for u, p in [
            ("alice", "/mokuro-reader/Kingdom/第80巻.cbz"),
            ("alice", "/mokuro-reader/Kingdom/第80巻.mokuro"),
            ("alice", "/mokuro-reader/kingdom/第80巻.cbz"),
            ("alice", "/mokuro-reader/Kingdom/第01巻.cbz"),
            ("root", "/mokuro-reader/Kingdom/第01巻.cbz"),
        ]
    ]
    manifest["kingdom_can_edit"] = [
        [u, s, db.can_user_edit_series(u, s)]
        for u, s in [("alice", "Kingdom"), ("alice", "kingdom"), ("root", "Kingdom")]
    ]
    manifest["ocr_sidecars"] = db.list_ocr_sidecars()
    manifest["ocr_producers"] = [list(t) for t in db.ocr_sidecar_producers()]
    manifest["owned_series"] = {u: db.list_series_owned_by(u) for u in ("alice", "root", "x")}
    with db._connection() as conn:
        manifest["legacy_catalog_series"] = [
            list(r) for r in conn.execute(
                "SELECT series_key, folder_name, volume_count, total_pages "
                "FROM catalog_series ORDER BY series_key"
            )
        ]


LEGACY_DDL = [
    "CREATE TABLE schema_version (version INTEGER PRIMARY KEY)",
    """CREATE TABLE users (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        username TEXT UNIQUE NOT NULL,
        password_hash TEXT NOT NULL,
        role TEXT NOT NULL DEFAULT 'registered',
        status TEXT NOT NULL DEFAULT 'active',
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    )""",
    """CREATE TABLE invites (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        code TEXT UNIQUE NOT NULL,
        role TEXT NOT NULL DEFAULT 'registered',
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        expires_at TEXT NOT NULL,
        used_by TEXT,
        used_at TEXT
    )""",
    """CREATE TABLE audit_logs (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        actor_username TEXT,
        action TEXT NOT NULL,
        target_type TEXT,
        target_path TEXT,
        target_username TEXT,
        details TEXT,
        created_at TEXT NOT NULL DEFAULT (datetime('now'))
    )""",
    """CREATE TABLE series_entry_cache (
        volume_key TEXT PRIMARY KEY,
        series_key TEXT NOT NULL,
        entry_json TEXT NOT NULL,
        cbz_size INTEGER NOT NULL,
        cbz_mtime REAL NOT NULL,
        sidecar_key TEXT NOT NULL DEFAULT '',
        computed_at TEXT NOT NULL DEFAULT (datetime('now'))
    )""",
    """CREATE TABLE catalog_series (
        series_key TEXT PRIMARY KEY,
        folder_name TEXT NOT NULL,
        cover_path TEXT,
        volume_count INTEGER NOT NULL,
        latest_volume_modified REAL NOT NULL DEFAULT 0,
        total_pages INTEGER NOT NULL DEFAULT 0,
        total_chars INTEGER NOT NULL DEFAULT 0,
        scanned_at TEXT NOT NULL DEFAULT (datetime('now'))
    )""",
    "CREATE INDEX idx_users_username ON users(username)",
]


def migrated_contents(path: Path) -> dict:
    conn = sqlite3.connect(path)
    try:
        q = lambda sql: [list(r) for r in conn.execute(sql)]  # noqa: E731
        # 0.5.2 gave the legacy `catalog_series` its damage columns; 0.5.3 leaves it as is.
        damage = ", missing_pages, damaged_volumes" if VERSION == "0.5.2" else ""
        return {
            "schema_version": q("SELECT version FROM schema_version"),
            "users": q("SELECT id, username, role, status, notes FROM users ORDER BY id"),
            "invites": q("SELECT code, role, invited_by, expires_at FROM invites ORDER BY id"),
            "catalog_series": q(
                f"SELECT series_key, folder_name, volume_count{damage} "
                "FROM catalog_series ORDER BY series_key"
            ),
            "catalog_folders": q(
                "SELECT series_key, folder_name, volume_count FROM catalog_folders "
                "ORDER BY folder_name"
            ) if VERSION != "0.5.2" else [],
            "volume_identities": q(
                "SELECT volume_key, volume_uuid FROM volume_identities ORDER BY volume_key"
            ),
        }
    finally:
        conn.close()


def make_legacy() -> None:
    path = HERE / "legacy.db"
    path.unlink(missing_ok=True)
    conn = sqlite3.connect(path)
    for ddl in LEGACY_DDL:
        conn.execute(ddl)
    hashed = bcrypt.hashpw(b"writer-pass1", bcrypt.gensalt()).decode()
    conn.execute(
        "INSERT INTO users (username, password_hash, role) VALUES ('scribe', ?, 'writer')",
        (hashed,),
    )
    conn.execute(
        "INSERT INTO users (username, password_hash, role, status) VALUES ('boss', ?, 'admin', 'active')",
        (hashed,),
    )
    conn.execute(
        "INSERT INTO invites (code, role, expires_at) VALUES ('legacyCode1', 'writer', ?)",
        ((datetime.now() + timedelta(weeks=520)).isoformat(),),
    )
    for key, entry in [
        ("S/V1.cbz", '{"volume_uuid": "legacy-u1", "mokuro_sha256": "aa"}'),
        ("S/V2.cbz", '{"volume_uuid": "legacy-u2", "mokuro_size": 0, "mokuro_version": "0.1"}'),
        ("S/V3.cbz", '{"volume_uuid": "derived"}'),
        ("S/V4.cbz", "{not json"),
        ("S/V5.cbz", "[1, 2]"),
    ]:
        conn.execute(
            "INSERT INTO series_entry_cache (volume_key, series_key, entry_json, cbz_size, cbz_mtime) "
            "VALUES (?, 's', ?, 1, 1.0)",
            (key, entry),
        )
    conn.execute(
        "INSERT INTO catalog_series (series_key, folder_name, volume_count) VALUES ('s', 'S', 5)"
    )
    conn.execute("INSERT INTO schema_version (version) VALUES (2)")
    conn.commit()
    conn.close()

    with tempfile.TemporaryDirectory() as tmp:
        copy = Path(tmp) / "legacy.db"
        shutil.copy(path, copy)
        db = Database(copy)
        assert db.authenticate_user("scribe", "writer-pass1")["role"] == "uploader"
        checkpoint(db)
        out = {
            "password": "writer-pass1",
            "schema": schema_rows(copy),
            "contents": migrated_contents(copy),
        }
    (HERE / "legacy.json").write_text(json.dumps(out, indent=1, ensure_ascii=False) + "\n")


def make_upgrades_053() -> None:
    """What 0.5.3 makes of the checked-in 0.5.2-era fixtures (they are not regenerated)."""
    out = {}
    with tempfile.TemporaryDirectory() as tmp:
        for name in ("py052", "legacy"):
            copy = Path(tmp) / f"{name}.db"
            shutil.copy(HERE / f"{name}.db", copy)
            db = Database(copy)
            catalog = db.list_catalog_series()
            checkpoint(db)
            out[name] = {
                "schema": schema_rows(copy),
                "contents": migrated_contents(copy),
                "catalog": catalog,
            }
    (HERE / "upgrade053.json").write_text(json.dumps(out, indent=1, ensure_ascii=False) + "\n")


if __name__ == "__main__":
    if VERSION == "0.5.2":
        make_python_db("052")
        make_legacy()
    elif VERSION == "0.5.3":
        make_python_db("053")
        make_upgrades_053()
    else:
        raise SystemExit(f"no fixtures defined for mokuro-bunko {VERSION}")
    print("wrote", *(p.name for p in sorted(HERE.glob("*.db"))), file=sys.stderr)
