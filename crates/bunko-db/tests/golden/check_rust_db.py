"""Rust -> Python 0.5.2: check a database written by bunko-db with the Python code.

Run by `tests/golden_rust_to_py.rs` (or by hand):

    ~/.cache/mokuro-bunko-demo/ref052/bin/python check_rust_db.py <mokuro.db> <manifest.json> <out.json>

`manifest.json` is what the Rust test wrote and expects Python to see. The script
- compares the schema text with a fresh Python database's (byte equality);
- opens the file with Python's `Database` (running its migrations over it);
- checks passwords, tokens, invites, audit details (ASCII JSON spelling), audit pages
  and cursors, ownership, OCR rows, identities and the series tables, including that
  every JSON column is spelled exactly as Python's `json.dumps` would spell it;
- then writes as a rollback would (a user, an invite, an audit event, a token) and
  records them in `out.json` for the Rust test to read back.
Exits non-zero with a message on the first mismatch.
"""

from __future__ import annotations

import json
import sqlite3
import sys
import tempfile
from pathlib import Path

from mokuro_bunko.database import Database
from mokuro_bunko.registration.invites import InviteManager


def schema_rows(path: Path) -> list[list]:
    conn = sqlite3.connect(path)
    try:
        return [
            list(r)
            for r in conn.execute(
                "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY rowid"
            )
        ]
    finally:
        conn.close()


def check(cond: bool, what: str) -> None:
    if not cond:
        raise SystemExit(f"MISMATCH: {what}")


def main() -> None:
    db_path, manifest_path, out_path = (Path(a) for a in sys.argv[1:4])
    m = json.loads(manifest_path.read_text())

    rust_schema = schema_rows(db_path)
    with tempfile.TemporaryDirectory() as tmp:
        fresh = Path(tmp) / "fresh.db"
        Database(fresh)._conn.close()
        python_schema = schema_rows(fresh)
    check(rust_schema == python_schema, "schema text differs from a fresh Python database")

    db = Database(db_path)
    check(schema_rows(db_path) == python_schema, "Python's open changed the schema")
    version = [tuple(r) for r in db._conn.execute("SELECT version FROM schema_version")]
    check(version == [(6,)], f"schema_version {version}")

    for u in m["users"]:
        check(db.get_user(u["username"]) == u["expected"], f"user {u['username']}")
    for name, pw in m["authenticates"]:
        got = db.authenticate_user(name, pw)
        check(got is not None and got["username"] == name, f"{name} must authenticate")
    for name, pw in m["refuses"]:
        check(db.authenticate_user(name, pw) is None, f"{name}/{pw!r} must be refused")
    for name, stamp in m["processor_stamps"].items():
        check(db.processor_account_stamp(name) == stamp, f"stamp {name}")

    for t in m["tokens"]:
        got = db.resolve_auth_token(t["token"])
        check((got["username"] if got else None) == t["resolves_to"], f"token {t}")

    mgr = InviteManager(db)
    for inv in m["invites"]:
        check((db.validate_invite(inv["code"]) is not None) == inv["valid"], f"invite {inv['code']}")
        check(mgr.get_info(inv["code"]) == inv["info"], f"invite info {inv['code']}")

    with db._connection() as conn:
        details = dict(conn.execute("SELECT id, details FROM audit_logs"))
    for ev in m["detail_events"]:
        want = (
            json.dumps(dict(ev["pairs"]), separators=(",", ":"), ensure_ascii=True)
            if ev["pairs"] is not None
            else None
        )
        check(details[ev["id"]] == want, f"audit details {ev['id']}: {details[ev['id']]!r} != {want!r}")
    for case in m["audit_queries"]:
        q = dict(case["query"])
        cursor = None
        for want in case["pages"]:
            page = db.query_audit_events(**q, cursor=cursor)
            got = {
                "ids": [e["id"] for e in page["events"]],
                "next_cursor": page["next_cursor"],
                "total": page["total"],
            }
            check(got == want, f"audit page {case['query']}: {got} != {want}")
            cursor = page["next_cursor"]
    check(db.audit_facets() == m["audit_facets"], "audit facets")

    for p, owner in m["owners"].items():
        check(db.get_volume_owner(p) == owner, f"owner {p}")
    for u, p, want in m["can_delete"]:
        check(db.can_user_delete_library_path(u, p) == want, f"can_delete {u} {p}")
    for u, s, want in m["can_edit"]:
        check(db.can_user_edit_series(u, s) == want, f"can_edit {u} {s}")
    for u, want in m["owned_series"].items():
        check(db.list_series_owned_by(u) == want, f"owned_series {u}")

    check(db.list_ocr_sidecars() == m["ocr_sidecars"], "ocr_sidecars")
    check([list(t) for t in db.ocr_sidecar_producers()] == m["ocr_producers"], "ocr producers")
    for p, uuid in m["identities"].items():
        check(db.remembered_volume_uuid(p) == uuid, f"identity {p}")

    for want in m["series_facts"]:
        check(db.get_series_facts(want["series_key"]) == want, f"series_facts {want['series_key']}")
    with db._connection() as conn:
        for row in conn.execute(
            "SELECT external_ids, titles, synonyms, volume_offsets FROM series_facts"
        ):
            for raw in row:
                again = json.dumps(json.loads(raw), ensure_ascii=False)
                check(raw == again, f"series_facts JSON spelling {raw!r} != {again!r}")
        for (raw,) in conn.execute("SELECT entry_json FROM series_entry_cache"):
            again = json.dumps(json.loads(raw), ensure_ascii=False)
            check(raw == again, f"entry_json spelling {raw!r} != {again!r}")
        for row in conn.execute("SELECT tags, genres FROM community_details"):
            for raw in row:
                check(raw == json.dumps(json.loads(raw)), f"community JSON spelling {raw!r}")
    for e in m["entry_cache"]:
        got = db.get_cached_volume_entry(e["volume_key"], e["cbz_size"], e["cbz_mtime"], e["sidecar_key"])
        check(got == e["entry"], f"entry cache {e['volume_key']}")
    check(db.list_catalog_series() == m["catalog_series"], "catalog_series")
    check(db.list_community_details() == m["community_details"], "community_details")

    # Write as a rolled-back 0.5.2 server would; the Rust test reads these back.
    db.create_user("pyuser", "py-password1", "uploader", notes="from python")
    code = db.create_invite("editor", "2w", invited_by="pyuser")
    token, _ = db.create_auth_token("pyuser", "reader", label="py")
    event_id = db.log_audit_event(
        "upload", actor_username="pyuser", target_type="library",
        target_path="/mokuro-reader/P/V1.cbz", details={"existed_before": False, "n": "é"},
    )
    db.put_series_facts({
        "series_key": "py", "series_title": "Py", "external_ids": {"anilist": 1},
        "titles": {"ja": "パイ"}, "synonyms": ["p"], "tag": None, "unit": None,
        "facts_updated_at": "2026-01-01T00:00:00.000Z", "spine_offset": -40,
        "volume_offsets": {"V1.cbz": 1.5}, "updated_by": "pyuser", "updated_at": "",
    })
    db._conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    db._conn.close()
    out_path.write_text(json.dumps({
        "invite_code": code, "token": token, "event_id": event_id,
        "event_details": json.dumps({"existed_before": False, "n": "é"},
                                    separators=(",", ":"), ensure_ascii=True),
    }))
    print("python 0.5.2 accepted the Rust database")


if __name__ == "__main__":
    main()
