# Spec: database, authentication, login, registration, account, admin API/CLI, audit log

Target: mokuro-bunko 0.5.2 (Python). The Rust rewrite must be a DROP-IN over existing 0.5.2 storage:
the same `mokuro.db` SQLite file must keep working in both directions (a Python 0.5.2 rollback must
still open a file the Rust server has touched).

Source root: `src/mokuro_bunko/` (abbreviated `S/` below). Tests: `tests/`. Citations are `file:line`.

Legend
- **DROP** = mokuro / ctd / animetext / rtdetr only. Do not port the behaviour. Where it touches a
  DB column or JSON field, the column/field must still exist and round-trip as opaque data.
- **KEEP** = must be reproduced.
- **QUIRK** = Python behaviour that looks accidental. Each one says whether to reproduce or fix; the
  ones that are decisions for the owner are repeated in "Open questions" at the end.

Contents
1. Storage: file, pragmas, connection model, retry
2. Schema (every table, column, index) and migration/versioning
3. Time and ID formats stored in the DB
4. Password hashing
5. Users: methods and SQL
6. Bearer tokens (the only session mechanism)
7. Invites
8. Audit log (writer, reader, pagination, search, facets, all emitted actions)
9. Volume ownership (`volume_uploads`) and series ownership
10. OCR sidecar provenance and volume identity tables
11. Series/catalog/community tables (DB layer only)
12. Rate limiting and client IP
13. Authentication (`middleware/auth.py`): parsing, results, errors
14. Authorization matrix (roles, permissions, per-method/path rules)
15. Login API (`/login/api/*`)
16. Registration API
17. Account API
18. Admin HTTP API (every endpoint)
19. Admin CLI
20. First-run setup (only the DB-touching part)
21. Queue page auth cache (consumer of `users_version`)
22. Static assets
23. Tests that pin behaviour
24. Open questions

---------------------------------------------------------------------------------------------------

## 1. Storage: file, pragmas, connection model, retry

### 1.1 File location
- DB file is `<storage.base_path>/mokuro.db` (`S/server.py:194`, `S/admin/cli.py:30`,
  `S/server.py:947`). Parent dirs are created (`database.py:448`).
- Three independent `Database` handles can be open on one file at once: the app's, the OCR worker's
  (`server.py:947`), and the admin CLI's, plus an external process (CLI against a live server). The
  Rust implementation must tolerate other processes writing concurrently (WAL + busy retry below).

### 1.2 Connection (database.py:433-473)
- One persistent connection per `Database`, `check_same_thread=False`, guarded by one process-wide
  mutex (`self._lock`): ALL access (reads included) is serialised through `_connection()`.
- `sqlite3.connect(path, timeout=30)` then `PRAGMA journal_mode=WAL` then
  `PRAGMA busy_timeout=<busy_timeout_ms>` (default 5000). The later pragma replaces the 30 s connect
  timeout, so the effective per-statement lock wait is `busy_timeout_ms`.
- Row access by column name (`row_factory = sqlite3.Row`).
- No `PRAGMA synchronous`, no `foreign_keys` (there are no FKs), no other pragmas. WAL mode is
  persistent in the file header: a DB the Rust server opens will be in WAL mode and the `-wal`/`-shm`
  side files will exist next to it. Rust must `PRAGMA journal_mode=WAL` as well.
- Python's default (legacy) transaction control: DML statements open an implicit transaction;
  `_connection()` ends each block with `commit()` (or `rollback()` on exception). DDL in
  `_init_schema` autocommits statement by statement. Rust: one transaction per `_connection()` block
  is the equivalent; a block that raises must roll back everything it did.
- Because every method takes the mutex for its whole body, read-modify-write sequences inside one
  method are atomic w.r.t. other threads of the same process (not other processes).

### 1.3 Tuning (`configure_connection`, database.py:458-473; config.py:246-262)
Config section `database` (`DatabaseConfig`): `busy_timeout_ms` (5000; must be >= 100),
`lock_retries` (5; >= 1), `retry_initial_delay_seconds` (0.05; > 0). `create_app` applies them to the
app's DB and to the OCR worker's DB (`server.py:196`, `server.py:948`). The CLI does NOT apply them
(it uses the defaults 5000 / 5 / 0.05). `configure_connection` clamps: busy timeout `max(100, v)`,
retries `max(1, v)`, delay `max(0.001, v)`; a changed busy timeout is re-issued as
`PRAGMA busy_timeout=`.

### 1.4 Lock-retry behaviour (database.py:378-408, 489-508)
- EVERY statement run through the proxy (`conn.execute`) is retried up to `lock_retries` attempts when
  `sqlite3.OperationalError` text contains `"database is locked"` (case-insensitive). Between attempts
  sleep `delay`, then `delay *= 2` (default sleeps 0.05, 0.1, 0.2, 0.4 s; 5 attempts total). The last
  failure (or any other error) is re-raised. Rust: retry on `SQLITE_BUSY` (not `SQLITE_LOCKED`
  "database table is locked", which Python does not match).
- `COMMIT` is retried with the same schedule (`_commit_with_retry`).
- `executemany`/`executescript`/direct `conn.*` other than `execute` are NOT retried (only used in tests).
- On any exception out of the block: `rollback()`, re-raise.
- Retrying is only safe because a statement that fails with BUSY took no lock and had no effect.

---------------------------------------------------------------------------------------------------

## 2. Schema and migrations

### 2.1 Version tracking (database.py:430, 515-770)
- `Database.SCHEMA_VERSION = 6`. Table `schema_version(version INTEGER PRIMARY KEY)` holds at most one
  row.
- **The version is bookkeeping only; nothing is gated on it.** `_init_schema` runs on EVERY open and
  is fully idempotent (`CREATE TABLE/INDEX IF NOT EXISTS`, column-presence probes for `ALTER TABLE`).
  At the end: if no `schema_version` row, `INSERT INTO schema_version (version) VALUES (6)`, else
  `UPDATE schema_version SET version = 6` (unconditionally, i.e. a DB marked 7 by something newer would
  be rewritten to 6). The Rust port must run the same idempotent steps and write 6.
- Comment labels in source: v3 = `series_facts`, `series_entry_cache` (+ `community_details`,
  `catalog_series`), v4 = `auth_tokens`/`ocr_sidecars`, v6 = `volume_identities`. (v5 is not
  referenced; auth_tokens is the likely v5.) Do not invent behaviour keyed on these numbers.

### 2.2 Tables (exact DDL; Rust must create them identically if absent)

```sql
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);

CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT UNIQUE NOT NULL,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'registered',
    status TEXT NOT NULL DEFAULT 'active',
    notes TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);                                                              -- database.py:524-535

CREATE TABLE IF NOT EXISTS invites (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    code TEXT UNIQUE NOT NULL,
    role TEXT NOT NULL DEFAULT 'registered',
    invited_by TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at TEXT NOT NULL,
    used_by TEXT,
    used_at TEXT
);                                                              -- :537-548

CREATE TABLE IF NOT EXISTS audit_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_username TEXT,
    action TEXT NOT NULL,
    target_type TEXT,
    target_path TEXT,
    target_username TEXT,
    details TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);                                                              -- :550-561

CREATE TABLE IF NOT EXISTS volume_uploads (
    volume_key TEXT PRIMARY KEY,
    uploader_username TEXT NOT NULL,
    uploaded_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_modified_by TEXT,
    last_modified_at TEXT NOT NULL DEFAULT (datetime('now'))
);                                                              -- :563-571

CREATE TABLE IF NOT EXISTS series_facts (
    series_key TEXT PRIMARY KEY,
    series_title TEXT NOT NULL,
    external_ids TEXT NOT NULL DEFAULT '{}',
    titles TEXT NOT NULL DEFAULT '{}',
    synonyms TEXT NOT NULL DEFAULT '[]',
    tag TEXT,
    unit TEXT,
    facts_updated_at TEXT NOT NULL,
    spine_offset NUMERIC,           -- NUMERIC on purpose: keeps -40 an integer (:577-582)
    volume_offsets TEXT NOT NULL DEFAULT '{}',
    updated_by TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);                                                              -- :583-598

CREATE TABLE IF NOT EXISTS series_entry_cache (
    volume_key TEXT PRIMARY KEY,
    series_key TEXT NOT NULL,
    entry_json TEXT NOT NULL,
    cbz_size INTEGER NOT NULL,
    cbz_mtime REAL NOT NULL,
    sidecar_key TEXT NOT NULL DEFAULT '',
    computed_at TEXT NOT NULL DEFAULT (datetime('now'))
);                                                              -- :600-610

CREATE TABLE IF NOT EXISTS community_details (
    series_key TEXT PRIMARY KEY,
    score REAL,
    tags TEXT NOT NULL DEFAULT '[]',
    genres TEXT NOT NULL DEFAULT '[]',
    source TEXT NOT NULL,
    fetched_at TEXT NOT NULL
);                                                              -- :612-621

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
);                                                              -- :623-636

CREATE TABLE IF NOT EXISTS auth_tokens (
    token_hash TEXT PRIMARY KEY,
    username TEXT NOT NULL,
    kind TEXT NOT NULL,
    label TEXT NOT NULL DEFAULT '',
    created_at REAL NOT NULL,
    expires_at REAL NOT NULL,
    last_used_at REAL NOT NULL
);                                                              -- :642-652
CREATE INDEX IF NOT EXISTS idx_auth_tokens_username ON auth_tokens(username);   -- :653

CREATE TABLE IF NOT EXISTS ocr_sidecars (
    sidecar_path TEXT PRIMARY KEY,
    volume_key TEXT NOT NULL,
    generation_id TEXT NOT NULL,
    generation_name TEXT NOT NULL,
    machine TEXT NOT NULL,
    account TEXT,
    engine TEXT,
    detector TEXT,
    precision TEXT,
    runner_build TEXT,
    pages INTEGER,
    failed_pages INTEGER,
    archive_size INTEGER,
    archive_mtime_ns INTEGER,
    written_at TEXT NOT NULL DEFAULT (datetime('now'))
);                                                              -- :657-675
CREATE INDEX IF NOT EXISTS idx_ocr_sidecars_volume ON ocr_sidecars(volume_key);  -- :676

CREATE TABLE IF NOT EXISTS volume_identities (
    volume_key TEXT PRIMARY KEY,
    volume_uuid TEXT NOT NULL,
    recorded_at TEXT NOT NULL DEFAULT (datetime('now'))
);                                                              -- :690-696
```

`users.status` and `users.role` have NO CHECK constraint; allowed values are enforced in code only:
status in `active|pending|disabled|deleted`; role in
`anonymous|registered|uploader|inviter|editor|admin|processor` (+ legacy `writer`, see 2.4).
`AUTOINCREMENT` means a `sqlite_sequence` table exists; ids are never reused.

### 2.3 Indexes (database.py:719-757)
```sql
CREATE INDEX IF NOT EXISTS idx_users_username            ON users(username);
CREATE INDEX IF NOT EXISTS idx_invites_code              ON invites(code);
CREATE INDEX IF NOT EXISTS idx_audit_created_at          ON audit_logs(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_audit_actor               ON audit_logs(actor_username);
CREATE INDEX IF NOT EXISTS idx_audit_type_created        ON audit_logs(target_type, created_at);
CREATE INDEX IF NOT EXISTS idx_audit_action_created      ON audit_logs(action, created_at);
CREATE INDEX IF NOT EXISTS idx_audit_actor_created       ON audit_logs(actor_username, created_at);
CREATE INDEX IF NOT EXISTS idx_volume_uploads_uploader   ON volume_uploads(uploader_username);
CREATE INDEX IF NOT EXISTS idx_series_entry_cache_series ON series_entry_cache(series_key);
```
(plus `idx_auth_tokens_username`, `idx_ocr_sidecars_volume` above). `test_audit_search.py:192-215` pins
that the three audit indexes exist and are re-created on an existing DB where they were dropped.
`idx_audit_created_at` is referenced by name in an `INDEXED BY` hint (8.4), so it MUST exist under
that exact name (or the Rust query must drop the hint).

### 2.4 Migration steps, in execution order (all run every open)
1. Create tables in the order of 2.2 up to and including `ocr_sidecars` + its index.
2. **volume_identities with one-time backfill** (database.py:681-698). Read
   `SELECT version FROM schema_version` (row or none) and test whether table `volume_identities`
   exists in `sqlite_master` BEFORE creating it. Then `CREATE TABLE IF NOT EXISTS volume_identities`.
   Backfill runs only if a `schema_version` row existed (i.e. not a brand-new DB) AND the table did not
   exist: for every `series_entry_cache` row, parse `entry_json` (object; invalid JSON => `{}`), and
   if `_identity_from_entry` (10.4) yields a uuid, `INSERT OR IGNORE INTO volume_identities
   (volume_key, volume_uuid) VALUES (?, ?)` (recorded_at = default).
3. **catalog_series columns** (:706-711): for `missing_pages`, `damaged_volumes`: if
   `PRAGMA table_info(catalog_series)` lacks the column,
   `ALTER TABLE catalog_series ADD COLUMN <c> INTEGER NOT NULL DEFAULT 0`.
4. **users.notes** (:713): if absent, `ALTER TABLE users ADD COLUMN notes TEXT NOT NULL DEFAULT ''`.
5. **invites.invited_by** (:716): if absent, `ALTER TABLE invites ADD COLUMN invited_by TEXT`.
6. Create the indexes of 2.3.
7. **Role rename** (:760-761): `UPDATE users SET role='uploader' WHERE role='writer'` and
   `UPDATE invites SET role='uploader' WHERE role='writer'` (runs on every open; harmless once done).
8. Write schema version 6 (2.1).

Old DBs may therefore lack: `users.notes`, `invites.invited_by`, the two `catalog_series` columns,
`auth_tokens`, `ocr_sidecars`, `volume_identities`, several indexes. The Rust port must tolerate (and
upgrade) all of them; a DB created by the Rust port must contain all of the above so Python can open it.

Reads also defensively normalize: any role read from `users`/`invites` passes through
`normalize_role` (`writer` -> `uploader`; anything not in VALID_ROLES raises `ValueError`, which in
Python surfaces as an unhandled 500 from `get_user`/`list_users`/`authenticate_user`). QUIRK: a bad role
value in a row bricks that user's login. Rust may degrade more gracefully; it must never panic.

---------------------------------------------------------------------------------------------------

## 3. Time and ID formats stored in the DB

| Column | Format | Producer |
|---|---|---|
| `users.created_at/updated_at`, `invites.created_at`, `invites.used_at`, `audit_logs.created_at`, `volume_uploads.*_at`, `ocr_sidecars.written_at`, `volume_identities.recorded_at`, `series_*.updated_at/computed_at`, `catalog_series.scanned_at` | `YYYY-MM-DD HH:MM:SS` UTC, space separator, no zone (SQLite `datetime('now')`) | SQL default / `datetime('now')` |
| `invites.expires_at` | Python `datetime.now().isoformat()` = **local-naive**, `T` separator, microseconds present unless exactly 0: `2026-10-08T14:03:22.123456` | `create_invite` (database.py:1244, 1252) |
| `auth_tokens.created_at/expires_at/last_used_at` | REAL, Unix epoch seconds (float, `time.time()`) | `create_auth_token` |
| `series_facts.facts_updated_at`, `community_details.fetched_at` | text as supplied by caller (metadata subsystem) | |

`invites.expires_at` is the one trap: it is the SERVER'S LOCAL time (process TZ), not UTC, and not the
format of `created_at`. Rust must produce `chrono::Local::now() + duration` formatted
`%Y-%m-%dT%H:%M:%S%.6f` (omit the fraction if microsecond == 0, as Python does) so Python can parse it,
and must parse stored values the way `datetime.fromisoformat` does (Python >= 3.11: accepts `T` or
space, optional fraction, optional offset; a tz-aware value compared to naive `now()` raises
`TypeError` in Python -> 500 in `validate_invite`). Minimum: accept `YYYY-MM-DD[T ]HH:MM:SS[.ffffff]`;
unparseable: `cleanup_expired_invites` skips the row (7.6), `validate_invite`/`get_status` raise
(Python 500). Rust should treat unparseable as "invalid/expired" instead of 500 (safe divergence).

API JSON exposes these strings verbatim (users `created_at`, invites `created_at`/`expires_at`).

---------------------------------------------------------------------------------------------------

## 4. Password hashing

- **bcrypt** via the `bcrypt` PyPI package (pyproject: `bcrypt>=4.0`; the exact installed major is
  unknown - see open question 1).
- Hash: `bcrypt.hashpw(password.encode() /*UTF-8*/, bcrypt.gensalt()).decode()` (database.py:772-774).
  `gensalt()` defaults: **cost 12, prefix `$2b$`**. Stored as the 60-char modular-crypt string in
  `users.password_hash`. Test pins only `startswith("$2")` and that two identical passwords hash
  differently (`test_database.py:611-646`).
- Verify: `bcrypt.checkpw(password.encode(), password_hash.encode())` (database.py:776-778). Must accept
  hashes with `$2a$`, `$2b$`, `$2y$` prefixes (Rust `bcrypt` crate verifies `2a/2b/2x/2y`). There are
  **no legacy non-bcrypt formats** and no rehash-on-login/upgrade path. A malformed stored hash makes
  `checkpw` raise `ValueError` -> unhandled 500 in Python; Rust should return "invalid credentials".
- bcrypt only uses the first 72 BYTES of the password. Validation allows up to 128 CHARACTERS
  (`validation.py`), so longer passwords are possible. bcrypt 4.x truncates silently (hash created from
  the first 72 bytes); bcrypt >= 5.0 raises `ValueError` for > 72 bytes. Existing installs' hashes were made
  with whichever version ran; Rust must verify against the first 72 bytes only (truncate before verify)
  to be compatible with 4.x-made hashes, and should hash the truncated 72 bytes too.
- Timing: `authenticate_user` skips bcrypt entirely for an unknown user, a non-`active` user, so those
  return faster (username-enumeration timing). Rust may add a dummy verify; not required.
- Passwords are compared as UTF-8 bytes of the Unicode string; HTTP Basic credentials are decoded as
  UTF-8 first (13.1).

Validation (S/validation.py; used by registration, admin create, account change, CLI via `Database`):
- `USERNAME_PATTERN = ^[a-zA-Z0-9_-]{3,32}$` applied with `re.match` (no `\Z`). QUIRK: Python's `$` also
  matches before a trailing `\n`, so `"abc\n"` passes `validate_username`. Reachable only via CLI/direct
  `Database` use (HTTP paths `.strip()` first). Rust should reject (`\z`); recorded as a safe divergence.
- Empty username -> `"Username is required"`; pattern mismatch ->
  `"Username must be 3-32 characters and contain only letters, numbers, underscores, and hyphens"`.
- Password: empty -> `"Password is required"`; `len(password) < 8` -> `"Password must be at least 8
  characters"`; `len > 128` -> `"Password must be at most 128 characters"`. Length counts Unicode code
  points (Python `len`), not bytes.

---------------------------------------------------------------------------------------------------

## 5. Users: methods and SQL

Types: `UserDict` = `{id:int, username:str, role:str, status:str, notes:str, created_at:str}`
(database.py:64-72) - this exact key set is what every API returns for a user. `password_hash` and
`updated_at` are never exposed.

`users_version` (database.py:414-424, 447): an in-process integer incremented (in a `finally`, i.e.
even when the method raises) by `create_user`, `update_user_role`, `update_user_password`,
`approve_user`, `disable_user`, `delete_user`, `restore_user`. NOT bumped by `update_user_notes`,
invite or token operations. It is per-`Database` instance and per process: a CLI change does not bump the
server's counter (consumer: 21).

| Method | Behaviour / SQL |
|---|---|
| `create_user(username,password,role='registered',status='active',notes='')` :782-840 | (1) empty/whitespace username -> `ValueError("Username is required")`; (2) `validate_username`; (3) `validate_password`; (4) `normalize_role(role)` (accepts ANY of the 7 roles incl. `anonymous`, plus `writer`); (5) bcrypt hash; (6) `INSERT INTO users (username, password_hash, role, status, notes) VALUES (?,?,?,?,?)`, returns lastrowid. `status` is not validated. On `IntegrityError`: look up `SELECT status FROM users WHERE username = ?`; if `deleted` -> `ValueError("Username '<u>' belongs to a deleted account; bring it back with: mokuro-bunko admin restore-user <u>")`, else `ValueError("Username '<u>' already exists")`. Username uniqueness is case-SENSITIVE (default BINARY collation). |
| `get_user(username)` :842 | `SELECT id, username, role, status, notes, created_at FROM users WHERE username = ?` -> UserDict or None. Returns deleted/pending/disabled users too. |
| `authenticate_user(username,password)` :871 | `SELECT id, username, password_hash, role, status, notes, created_at FROM users WHERE username = ?`; only if `status == 'active'` AND bcrypt verifies -> UserDict, else None. pending/disabled/deleted can never authenticate. |
| `processor_account_stamp(username)` :903 | `SELECT role, status, password_hash FROM users WHERE username=?`. None unless row exists, `role == 'processor'`, `status == 'active'`. Else `sha256(f"{role}\0{status}\0{password_hash}".encode()).hexdigest()[:32]`. (Consumer: processor registry; used to cut off revoked processors.) |
| `list_users(status=None)` :928 | `SELECT id, username, role, status, notes, created_at FROM users [WHERE status = ?] ORDER BY created_at DESC`. Ties (same second) are in unspecified order; admin.js shows the list as given. Rust: add `, id DESC` as tiebreak is a harmless improvement. Includes deleted users unless filtered. |
| `update_user_role(u, role)` :966 | `normalize_role`; `UPDATE users SET role = ?, updated_at = datetime('now') WHERE username = ?`; True iff rowcount > 0. Does NOT touch tokens. Works on deleted users too. |
| `update_user_notes(u, notes)` :988 | `UPDATE users SET notes = ?, updated_at = datetime('now') WHERE username = ?`. No version bump. |
| `update_user_password(u, pw)` :1000 | `validate_password` (ValueError); hash; `UPDATE users SET password_hash = ?, updated_at = datetime('now') WHERE username = ?`; if rowcount > 0: `DELETE FROM auth_tokens WHERE username = ?` (a new password signs out every token). Works on any status. |
| `approve_user(u)` :1029 | `UPDATE users SET status='active', updated_at=datetime('now') WHERE username=? AND status='pending'`; True iff changed. |
| `disable_user(u)` :1049 | `UPDATE users SET status='disabled', updated_at=datetime('now') WHERE username=?` (any current status, including `deleted` -> becomes `disabled`). Tokens are NOT deleted but `resolve_auth_token` refuses non-active users. QUIRK: there is no way to re-enable a disabled user (approve only works on `pending`, restore only on `deleted`); recovery is `delete-user` then `restore-user`. |
| `delete_user(u)` :1069 | Soft delete: `UPDATE users SET status='deleted', updated_at=datetime('now') WHERE username=? AND status != 'deleted'`, then ALWAYS `DELETE FROM auth_tokens WHERE username=?`; True iff the UPDATE changed a row. The row (and so the name) survives forever; `volume_uploads` and audit rows are kept. |
| `restore_user(u, pw, role=None)` :1179 | `validate_password`; hash; `normalize_role(role)` if given; `UPDATE users SET status='active', password_hash=?, role=COALESCE(?, role), updated_at=datetime('now') WHERE username=? AND status='deleted'`; True iff changed. Old password never returns. |

Not present: no hard delete of users, no rename, no email.

---------------------------------------------------------------------------------------------------

## 6. Bearer tokens - the only session mechanism

There are **no cookies and no server-side web sessions**. A signed-in browser tab holds a bearer token
in `sessionStorage` (`S/static/nav.js:14-63`: keys `mokuro_token`, `mokuro_user`) and sends
`Authorization: Bearer <token>`. The reader and processors send Basic or Bearer. Basic remains accepted
everywhere.

Constants (database.py:55-61):
```
TOKEN_KINDS = {"web": 7*86400.0, "reader": 90*86400.0, "processor": 30*86400.0}   # seconds
TOKEN_TOUCH_SECONDS = 60.0
```

- `create_auth_token(username, kind, *, label="", lifetime_seconds=None) -> (token, expires_at)`
  (:1097): `kind` not in TOKEN_KINDS -> `ValueError("unknown token kind 'x'")`.
  `token = secrets.token_urlsafe(32)` (32 random bytes, base64url without padding = **43 chars**
  `[A-Za-z0-9_-]`). `now = time.time()`; `expires_at = now + lifetime` (default per kind; an explicit
  negative lifetime is allowed, used by tests). Stored:
  `INSERT INTO auth_tokens (token_hash, username, kind, label, created_at, expires_at, last_used_at)
  VALUES (sha256_hex(token_utf8), username, kind, label[:200], now, now+lifetime, now)`.
  `label[:200]` = first 200 code points. Only the SHA-256 hex (64 lowercase chars) is stored; the
  token is shown once. It does NOT check that the user exists or is active (callers did).
- `resolve_auth_token(token) -> UserDict|None` (:1126): empty token -> None. Single query
  `SELECT t.expires_at, t.last_used_at, u.id, u.username, u.role, u.status, u.notes, u.created_at
  FROM auth_tokens t JOIN users u ON u.username = t.username WHERE t.token_hash = ?`. None if no row,
  `expires_at <= now`, or `status != 'active'`. If `now - last_used_at >= 60`:
  `UPDATE auth_tokens SET last_used_at = ? WHERE token_hash = ?` (so a request is a write at most once a
  minute). Role/status are read from the user row on EVERY call: role changes and disabling take effect
  on the next request. Expired rows are not deleted here.
- `revoke_auth_token(token) -> bool` (:1159): `DELETE FROM auth_tokens WHERE token_hash = ?`.
- `revoke_user_auth_tokens(username) -> int` (:1167): `DELETE ... WHERE username = ?`. (No production
  caller; the delete/password paths inline the same SQL.)
- `prune_expired_auth_tokens() -> int` (:1173): `DELETE FROM auth_tokens WHERE expires_at <= ?`
  (time.time()). Called ONLY on each successful `POST /login/api/token` (login/api.py:203). There is no
  periodic sweeper.
- A token of any `kind` is equally valid for any request; `kind` only sets the lifetime (and is not
  consulted on use). The token carries the user's CURRENT role.
- Invalidation events: logout (`DELETE /login/api/token`), password change (`update_user_password`),
  `delete_user`. Disable and role change do not delete rows (disable still blocks via status check).

Tests: `tests/unit/test_auth_tokens.py`, `tests/unit/test_auth_token_requests.py`.

---------------------------------------------------------------------------------------------------

## 7. Invites

Constants: `INVITABLE_ROLES = {"registered","uploader","inviter","editor"}` (database.py:50). `admin` and
`processor` can never be minted by an invite (enforced in `create_invite`, not only in menus).

`InviteDict` (database.py:75-84): `{id, code, role, created_at, expires_at, used_by, invited_by}`.

| Method | Behaviour |
|---|---|
| `create_invite(role='registered', expires='7d', invited_by=None) -> code` :1210 | `normalize_role(role)` (so `writer` -> `uploader`, accepted); if not in INVITABLE_ROLES -> `ValueError("Role cannot be granted by invite: <role>. Must be one of: ['editor', 'inviter', 'registered', 'uploader']")`. Code = `secrets.token_urlsafe(16)` (**22 chars**), regenerated while it starts with `-` or `_` (Click arg parsing). `parse_duration(expires)` (below). `expires_at = datetime.now() + duration` stored `.isoformat()` (local naive, see 3). `INSERT INTO invites (code, role, invited_by, expires_at) VALUES (?,?,?,?)`. Then audit event `invite_created` (actor = invited_by, target_type `invite`, **target_path = the code**, details `{"role": <normalized>, "expires": <the raw expires string>}`). The audit row therefore contains the plaintext invite code. |
| `get_invite(code)` :1264 | `SELECT id, code, role, created_at, expires_at, used_by, invited_by FROM invites WHERE code = ?`. |
| `validate_invite(code)` :1294 | None if not found, or `used_by` truthy, or `datetime.now() > fromisoformat(expires_at)` (strictly greater: valid at the exact instant). Else the InviteDict. |
| `use_invite(code, username)` :1316 | `validate_invite` first (False if None); then `UPDATE invites SET used_by = ?, used_at = datetime('now') WHERE code = ? AND used_by IS NULL`; True iff rowcount > 0 (a second concurrent consumer loses here). If changed: audit `invite_used` (actor = username, target_type `invite`, target_path = code, target_username = username, details `{"invited_by": <inviter or null>, "role": <invite role>}`). |
| `list_invites(include_used=False)` :1352 | `include_used`: `SELECT <cols> FROM invites ORDER BY created_at DESC`. Else `... WHERE used_by IS NULL AND expires_at > datetime('now') ORDER BY created_at DESC`. QUIRK: `expires_at` is local-naive `T`-format text compared lexicographically to UTC `datetime('now')` (space format): `'T' (0x54) > ' ' (0x20)`, so any invite whose expiry DATE equals today's UTC date is listed even if already expired, and zone offsets shift the boundary. Reproduce by running the identical SQL; used by the CLI `list-invites` (no `--all`) and `InviteManager.list_valid` (no HTTP caller). |
| `delete_invite(code)` :1391 | `DELETE FROM invites WHERE code = ?`; True iff rowcount > 0 (works on used and expired invites). |
| `cleanup_expired_invites()` :1407 | `SELECT id, expires_at FROM invites WHERE used_by IS NULL`; in-process compare `fromisoformat(expires_at) < datetime.now()` (local naive); unparseable rows skipped; `DELETE FROM invites WHERE id IN (...)` (parameterised). Returns count. **Never called by the server or CLI** (only `InviteManager.cleanup_expired`, tested). Port the method; do not schedule it. |

`parse_duration(s)` (database.py:250-282): empty -> `ValueError("Duration cannot be empty")`; unit =
last char lowercased (`h`/`d`/`w`); `int(s[:-1])` failing -> `"Invalid duration format: <s>"`; `<= 0` ->
`"Duration must be positive: <s>"`; other unit -> `"Unknown duration unit: <u>"`. `1h`, `7d`, `2w`,
case-insensitive unit. (No minutes/seconds/months.) Python `int()` accepts whitespace/underscores/sign,
e.g. `" 7d"`? (`int(" 7")` is 7). Rust: accept optional ASCII sign/whitespace only if cheap; not pinned.

`InviteManager` (S/registration/invites.py) is a thin wrapper plus status:
`get_status(invite)`: `used_by` truthy -> `"used"`; else `now > expires_at` -> `"expired"`; else `"valid"`.
`InviteInfo` JSON object: `{code, role, status, created_at, expires_at, used_by, invited_by}` (NO `id`).
`list_all()` = `list_invites(include_used=True)` mapped with computed status; `get_info(code)` likewise
(None if missing).

---------------------------------------------------------------------------------------------------

## 8. Audit log

### 8.1 Writer (database.py:1445-1484)
`log_audit_event(action, actor_username=None, target_type=None, target_path=None, target_username=None,
details=None) -> int (rowid)`.
- `details` (a dict) is serialised `json.dumps(details, separators=(",",":"), ensure_ascii=True)`:
  compact, insertion-order keys, every non-ASCII char `\uXXXX` (lowercase hex; astral chars as surrogate
  pairs), `"` and `\` escaped. Stored as text in `details`. `None` -> SQL NULL. The 8.5 search relies on
  this ASCII-escaping; Rust MUST produce byte-compatible escaped JSON (serde_json by default does NOT
  escape non-ASCII: needs a custom formatter) or searches on old/new rows diverge.
- `INSERT INTO audit_logs (actor_username, action, target_type, target_path, target_username, details)
  VALUES (?,?,?,?,?,?)` (`created_at` = default UTC `datetime('now')`).
- **Pruning**: at most once per hour per process, and on the FIRST event after process start:
  `should_prune = (monotonic_now - last_prune >= 3600) or last_prune == 0.0`. When true, in the same
  transaction: `DELETE FROM audit_logs WHERE created_at < datetime('now', '-30 days')` and set
  `last_prune = monotonic_now` (set before commit). Retention = **30 days** (`AUDIT_RETENTION_DAYS`).
  Pinned by `tests/unit/test_database_resilience.py:110-146`.
- Callers swallow audit failures in some places (metadata middleware, provenance) and not in others
  (admin handlers, account delete: an audit failure there would 500).
- Audit rows are never deleted by user deletion.

### 8.2 `AuditEventDict` (:102-112) - the row exposed everywhere
`{id:int, actor_username:str|null, action:str, target_type:str|null, target_path:str|null,
target_username:str|null, details:str|null /* JSON TEXT, not an object */, created_at:str}`.

### 8.3 Legacy `list_audit_events(limit=200, *, actor=None)` (:1486)
`limit` clamped `max(1, min(int(limit), 1000))`; `SELECT id, actor_username, action, target_type,
target_path, target_username, details, created_at FROM audit_logs [WHERE actor_username = ?]
ORDER BY created_at DESC, id DESC LIMIT ?`. Not used by any HTTP route (tests only). Port optional.

### 8.4 `query_audit_events(...)` (:1552-1660)
Keyword args: `actor`, `actions[]`, `target_types[]`, `since`, `until`, `search`, `include_progress`,
`cursor`, `limit` (default 50 = `AUDIT_PAGE_SIZE`, clamped to 1..200 = `AUDIT_PAGE_MAX`).
Clauses, ANDed, in this order, with bound params:
1. `actor` truthy: `actor_username = ?`.
2. non-empty `actions` (empty strings dropped): `action IN (?,..)`.
3. non-empty `target_types`: `target_type IN (?,..)`; **else if not include_progress**:
   `(target_type IS NULL OR target_type <> ?)` with `'progress'` (so asking for the progress type by name
   re-includes it; `include_progress` only matters when no type is named).
4. `since`: `created_at >= ?` (inclusive); `until`: `created_at < ?` (exclusive); values converted by
   `_audit_instant` (8.6).
5. `search` (after `.strip()`, non-empty): see 8.5.
6. cursor (only after the count filters are snapshotted): `(created_at, id) < (?, ?)` (row value).
Query: `SELECT id, actor_username, action, target_type, target_path, target_username, details, created_at
FROM audit_logs [INDEXED BY idx_audit_created_at] [WHERE ...] ORDER BY created_at DESC, id DESC
LIMIT size+1`. The `INDEXED BY idx_audit_created_at ` hint is emitted ONLY when `actor` is falsy AND no
actions AND no target_types were requested (planner otherwise picks a bad skip-scan; comment at
:1629-1638). It is a performance hint, not semantics.
Result `AuditPage` = `{events: first `size` rows, next_cursor, total}`:
- `next_cursor`: if more than `size` rows came back, `_audit_cursor(events[-1].created_at, events[-1].id)`
  else null.
- `total`: only when no `cursor` was given: `SELECT COUNT(*) FROM audit_logs [WHERE <filters without
  cursor>]`; with a cursor it is null.
Cursor encoding (:1531-1545): `base64.urlsafe_b64encode(json.dumps([created_at, id],
separators=(",",":")).encode()).decode().rstrip("=")`. Decode: re-pad with `=`, urlsafe-b64-decode,
UTF-8, JSON-parse, must be a 2-element list `[str, int]` (a JSON bool counts as int; wrong shape/any
decode error) else `AuditQueryError("cursor is not one this server gave out")`. The cursor embeds
`created_at` in the DB text format, so Rust cursors are interchangeable with Python's.

### 8.5 Search semantics (:1604-1618, :1547-1550)
- term = `search.strip()`. `_like_pattern(t)` = `"%" + t with \ -> \\, % -> \%, _ -> \_ + "%"`.
- patterns = `[like(term)]`; `ascii_form = json.dumps(term, ensure_ascii=True)[1:-1]` (the term as it
  would appear inside an ASCII-escaped JSON string); if `ascii_form != term`, append `like(ascii_form)`.
- Clause: `(actor_username LIKE ? ESCAPE '\' OR action LIKE ? ESCAPE '\' OR target_path LIKE ? ESCAPE '\'
  OR details LIKE ? ESCAPE '\' [OR details LIKE ? ESCAPE '\' ...])`. Params: `pattern0` four times,
  then the extra patterns (for `details` only).
- Searched columns: actor, action, target_path, details. NOT `target_type`, NOT `target_username`.
- SQLite `LIKE` is case-insensitive for ASCII only (default); wildcards in the term are literal.
Pinned: `test_audit_search.py` (search finds non-ASCII stored escaped; wildcards literal;
case-insensitive).

### 8.6 `_audit_instant(value, name)` (:1517-1529)
`text = value.strip()`; if it matches `^\d{4}-\d{2}-\d{2}$` -> `"<text> 00:00:00"`. Else
`datetime.fromisoformat(text.replace("Z","+00:00").replace("z","+00:00"))` (every `Z`/`z` replaced);
failure -> `AuditQueryError(f"{name} is not a date: {value[:40]!r}")` (Python `repr` of the first 40
chars, single-quoted); tz-aware -> converted to UTC and made naive; result formatted
`%Y-%m-%d %H:%M:%S` (fraction dropped). The 400 body is `{"error": "<that message>"}`; test requires the
word `since` present.

### 8.7 `audit_facets()` (:1662-1687)
For each of `("actors","actor_username")`, `("actions","action")`, `("target_types","target_type")`,
distinct non-null values, ascending (BINARY), via
```sql
WITH RECURSIVE seen(value) AS (
  SELECT MIN(<col>) FROM audit_logs
  UNION ALL
  SELECT (SELECT MIN(<col>) FROM audit_logs WHERE <col> > seen.value) FROM seen WHERE seen.value IS NOT NULL
) SELECT value FROM seen WHERE value IS NOT NULL
```
Returns `{"actors":[...], "actions":[...], "target_types":[...]}` of strings. (`SELECT DISTINCT ... ORDER
BY` is equivalent; the loose-index-scan CTE is a perf choice for 100k-row logs.)
Performance expectation in tests: a page well under 50 ms at 100k rows (`test_audit_search.py:247-290`).

### 8.8 Every audit action emitted (action / target_type / when)
Written through `log_audit_event`; the `Details` column is compact ASCII JSON.

| action | target_type | target_path | target_username | details | where |
|---|---|---|---|---|---|
| `invite_created` | `invite` | the invite code | null | `{"role","expires"}` | database.py:1255 (actor = inviter or null for CLI) |
| `invite_used` | `invite` | code | username | `{"invited_by","role"}` | database.py:1342 |
| `invite_deleted` | `invite` | code | null | none | admin/api.py:876 |
| `admin_create_user` | `user` | null | username | `{"role": <role string as the client sent it>}` | admin/api.py:674 |
| `admin_delete_user` | `user` | null | username | none | :695 |
| `admin_change_role` | `user` | null | username | `{"role"}` | :734 |
| `admin_approve_user` | `user` | null | username | none | :753 |
| `admin_disable_user` | `user` | null | username | none | :774 |
| `admin_update_notes` | `user` | null | username | none (notes text NOT logged) | :817 |
| `self_delete_account` | `user` | null | username | none | account/api.py:195 (logged BEFORE the delete) |
| `metadata_update` / `metadata_rejected` | `library` | request path | null | `{"accepted": bool}` | metadata/middleware.py:137 |
| `ocr_sidecar_written` / `ocr_sidecar_rejected` | `sidecar` | sidecar path | null | OCR details | ocr/provenance.py:36-38,223,255 (generation/machine/engine/detector/precision/pages/... : OCR subsystem) |
| `upload`, `edit`, `delete`, `move`, `copy`, `mkdir`, `lock_conflict` | `library`, `progress`, `webdav`, `library_folder`, `webdav_folder` | `/mokuro-reader/<rel>` or the DAV path | null | e.g. `{"existed_before":..}`, `{"destination":..}`, `{"path":..}`, `{"operation":..}` | webdav/resources.py:606-620, 1072-1090 (WebDAV subsystem) |

Not audited: registration, password change (account), settings changes, token issue/revoke, all CLI
actions except the invite ones (actor null). Reading-progress writes are `target_type = 'progress'` and
dominate the table; hence they are hidden by default in queries.

---------------------------------------------------------------------------------------------------

## 9. Volume ownership (`volume_uploads`) and series ownership

(DB layer; the HTTP semantic is in the authz section 14.)

`normalize_volume_key_from_library_relative(path)` (database.py:285-302): `cleaned = path.strip("/")`
(both ends); empty -> None; case-insensitive suffix test in order `.cbz` (key = cleaned unchanged, even
if suffix is `.CBZ`), `.mokuro.gz`, `.mokuro`, `.webp`, `.nocover` (suffix replaced by lowercase
`.cbz`); anything else -> None. Keys are case-preserving for the stem and byte-exact.

| Method | Behaviour |
|---|---|
| `record_volume_upload(rel, uploader, existed_before=False)` :1704 | key None -> no-op. `is_archive = rel.strip("/").lower().endswith(".cbz")`. **Non-archive** (sidecar): `UPDATE volume_uploads SET last_modified_by=?, last_modified_at=datetime('now') WHERE volume_key=?` only (never inserts: a sidecar must not capture ownership). **Archive** (either value of `existed_before`; both branches are identical): `INSERT INTO volume_uploads (volume_key, uploader_username, last_modified_by, last_modified_at) VALUES (?,?,?,datetime('now')) ON CONFLICT(volume_key) DO UPDATE SET last_modified_by = excluded.last_modified_by, last_modified_at = datetime('now')`. The original `uploader_username`/`uploaded_at` are preserved on conflict. |
| `get_volume_owner(rel)` :1760 | key None -> None; `SELECT uploader_username FROM volume_uploads WHERE volume_key = ?`. |
| `can_user_delete_library_path(username, virtual_path)` :1776 | `prefix="/mokuro-reader/"`; path must start with it else False; `relative = rest.strip("/")`; False if `not relative or ("/" not in relative and "." not in relative)` (root and top-level dirs never); `owner = get_volume_owner(relative)`; if None and the path is an OCR layer file `<stem>.<layer>.mokuro[.gz]` (`_layer_sidecar_volume_path`: strip optional `.gz`, require `.mokuro`, split at the last `.` in the stem, layer matches `^(?=[a-z0-9-]*[a-z])[a-z0-9-]{1,32}\Z` (at least one letter, so `Vol 01.5.mokuro` is not a layer), result `<stem-before-dot>.cbz`) then owner of that parent; return `owner == username`. BYTE-EXACT matching (deliberately not Unicode-folded). |
| `_volume_upload_folder_owners()` :1807 | `SELECT DISTINCT uploader_username, volume_key FROM volume_uploads`; keep rows whose key has a `/`; pair = (first path segment, uploader). |
| `series_owners(series_title)` :1832 | `prefix = series_title.strip("/")`; empty -> empty set; owners = uploaders whose folder, folded by `_fold_series_title_key`, equals the folded `prefix`. Fold = NFC-normalize, `strip()`, collapse whitespace runs (`\s+`, Unicode whitespace) to one space, `.lower()` (NOT casefold: `ß` != `ss`). Identical to `metadata.reader_compat.normalize_volume_title_key` (pinned by `test_database.py:584-609`). |
| `can_user_edit_series(username, series_title)` :1867 | `owners = series_owners(..)`; `bool(owners) and owners == {username}` (every tracked volume owned by exactly this user; untracked folder -> False). |
| `list_series_owned_by(username)` :1880 | One pass: group by folded folder key -> set of owners and set of raw folder spellings; for each key with `owners == {username}` collect ALL raw spellings; return sorted list (Python string sort: code point order). Feeds `/login/api/me` `permissions.metadata.ownedSeries`. |
| `forget_volume_upload(rel)` :1906 | `DELETE FROM volume_uploads WHERE volume_key = ?`. |
| `forget_volume_uploads_under_prefix(prefix)` :1914 | prefix `.strip("/")`; empty -> 0; `DELETE FROM volume_uploads WHERE volume_key LIKE ?` with `"<prefix>/%"`. QUIRK: LIKE is unescaped (`_`/`%` in a folder name are wildcards) and ASCII-case-insensitive, so it can over-delete rows of another folder (`Dr_Stone/` also deletes `Dr Stone/`, `dr stone/` rows). Contrast the OCR tables (10) which use exact `substr`. Recommend Rust use the exact `substr(volume_key,1,?) = ?` form (see open question 4). |
| `rename_volume_upload(old_rel, new_rel)` :1926 | keys via normalize; any None or equal -> no-op; `SELECT uploader_username, uploaded_at, last_modified_by FROM volume_uploads WHERE volume_key = ?` (old); none -> return; `INSERT INTO volume_uploads (volume_key, uploader_username, uploaded_at, last_modified_by, last_modified_at) VALUES (?,?,?,?,datetime('now')) ON CONFLICT(volume_key) DO UPDATE SET uploader_username=excluded.uploader_username, uploaded_at=excluded.uploaded_at, last_modified_by=excluded.last_modified_by, last_modified_at=datetime('now')`; then `DELETE ... WHERE volume_key = old`. |

Known data caveat (memory note): `volume_uploads.uploader_username` is wrong for rows created before
2026-02-25 (a sidecar edit used to create a row crediting the editor). Not fixable in code; do not "fix"
by migration.

---------------------------------------------------------------------------------------------------

## 10. OCR sidecar provenance and volume identity tables

Column set / dict shape `OcrSidecarRow` (database.py:115-146): `sidecar_path, volume_key, generation_id,
generation_name, machine, account, engine, detector, precision, runner_build, pages, failed_pages,
archive_size, archive_mtime_ns, written_at`. `engine`/`detector`/`precision` values are opaque strings
(`detector` may hold `ctd`, `rtdetr`, `animetext` etc. in existing rows - **DROP** the detectors but keep
the column and never fail on unknown values).

| Method | SQL |
|---|---|
| `record_ocr_sidecar(row)` :1959 | `INSERT OR REPLACE INTO ocr_sidecars (<14 cols>, written_at) VALUES (?x14, datetime('now'))` (missing keys bind NULL). |
| `get_ocr_sidecar(path)` :1979 | `SELECT * FROM ocr_sidecars WHERE sidecar_path = ?` (path `.strip("/")`). Returns every column incl. `written_at`. |
| `list_ocr_sidecars()` :1987 | `SELECT * FROM ocr_sidecars ORDER BY written_at, rowid`. |
| `ocr_sidecar_producers()` :1993 | `SELECT generation_id, volume_key, machine FROM ocr_sidecars ORDER BY written_at, rowid` -> list of 3-tuples of str. |
| `forget_ocr_sidecar(path)` :2002 | `DELETE ... WHERE sidecar_path = ?` -> rowcount. |
| `forget_ocr_sidecars_of_volume(volume_key)` :2010 | `DELETE ... WHERE volume_key = ?` (key `.strip("/")`). |
| `forget_ocr_sidecars_under_prefix(prefix)` :2018 | prefix stripped; empty -> 0; head = prefix + "/"; `DELETE FROM ocr_sidecars WHERE substr(sidecar_path, 1, ?) = ?` (len(head), head). |
| `rename_ocr_sidecars_under_prefix(old, new)` :2031 | both stripped, either empty or equal -> 0; `UPDATE OR REPLACE ocr_sidecars SET sidecar_path = ? || substr(sidecar_path, ?), volume_key = ? || substr(volume_key, ?) WHERE substr(sidecar_path, 1, ?) = ?` with `(new_head, len(old_head)+1, new_head, len(old_head)+1, len(old_head), old_head)`. `OR REPLACE` deletes any destination row that collides. |

### 10.4 `volume_identities` (database.py:2051-2160)
- `_identity_from_entry(entry)`: `volume_uuid` must be a non-blank string AND (`entry["mokuro_sha256"]`
  truthy OR (`mokuro_size is not None` AND `mokuro_version` truthy)); else None. (`mokuro_*` here are the
  field names of the compiled `.mokuro` sidecar FILE FORMAT; keep them - the `.mokuro` format itself is
  not a DROP item, only the mokuro OCR engine/environment is.)
- `remember_volume_uuid(rel, uuid)`: key via normalize; None or blank uuid -> no-op; upsert (below).
- `_upsert_volume_identity`: `INSERT INTO volume_identities (volume_key, volume_uuid, recorded_at)
  VALUES (?,?,datetime('now')) ON CONFLICT(volume_key) DO UPDATE SET volume_uuid = excluded.volume_uuid,
  recorded_at = excluded.recorded_at WHERE volume_uuid != excluded.volume_uuid` (no write when unchanged).
- `remembered_volume_uuid(rel)`: `SELECT volume_uuid FROM volume_identities WHERE volume_key = ?`.
- `forget_volume_uuid(rel)`: `DELETE ... WHERE volume_key = ?`.
- `forget_volume_uuids_under_prefix(prefix)`: `DELETE ... WHERE substr(volume_key, 1, ?) = ?` (exact).
- `rename_volume_uuids_under_prefix(old,new)`: `UPDATE OR REPLACE volume_identities SET volume_key = ? ||
  substr(volume_key, ?) WHERE substr(volume_key, 1, ?) = ?` with `(new_head, len(old_head)+1,
  len(old_head), old_head)`.

---------------------------------------------------------------------------------------------------

## 11. Series / catalog / community tables (DB layer only)

Consumed by the metadata/catalog subsystems; stated here because they live in `mokuro.db` and must
round-trip with Python.

- `_load_json_object(raw, fallback)` (:2165): non-string -> fallback; JSON parse error -> fallback;
  decoded value must be an instance of `type(fallback)` (`dict` or `list`) else fallback. Corrupt JSON
  in these columns is never fatal.
- `get_series_facts(series_key)`: `SELECT * FROM series_facts WHERE series_key = ?`;
  `list_series_facts()`: `SELECT * FROM series_facts`. Row -> `SeriesFactsRow`: `series_key,
  series_title, external_ids (dict, JSON col), titles (dict), synonyms (list), tag, unit,
  facts_updated_at, spine_offset (NUMERIC: int stays int, float stays float), volume_offsets (dict),
  updated_by, updated_at`.
- `put_series_facts(row)`: `INSERT INTO series_facts (series_key, series_title, external_ids, titles,
  synonyms, tag, unit, facts_updated_at, spine_offset, volume_offsets, updated_by, updated_at) VALUES
  (?x11, datetime('now')) ON CONFLICT(series_key) DO UPDATE SET series_title, external_ids, titles,
  synonyms, tag, unit, facts_updated_at, spine_offset, volume_offsets, updated_by = excluded.*,
  updated_at = datetime('now')`. JSON columns written with Python `json.dumps(x, ensure_ascii=False)`
  (default separators `", "`/`": "`; Rust may write compact but must READ both). `spine_offset` bound
  through `_bindable_offset`: bool/non-number -> NULL; int outside i64 -> NULL; non-finite float -> NULL.
- `get_cached_volume_entry(volume_key, cbz_size, cbz_mtime, sidecar_key)`: `SELECT * FROM
  series_entry_cache WHERE volume_key = ?`; hit only if `int(cbz_size)==`, `float(cbz_mtime) ==`
  (exact float equality), `sidecar_key ==`; entry = parsed `entry_json` object; `{}`/invalid -> None.
- `put_cached_volume_entry(...)`: in one transaction, if `_identity_from_entry(entry)` -> upsert
  `volume_identities` (10.4), then `INSERT INTO series_entry_cache (volume_key, series_key, entry_json,
  cbz_size, cbz_mtime, sidecar_key, computed_at) VALUES (?,?,?,?,?,?,datetime('now')) ON CONFLICT(volume_key)
  DO UPDATE SET <all but key> = excluded.*, computed_at = datetime('now')` (`entry_json` =
  `json.dumps(entry, ensure_ascii=False)`).
- `prune_series_entry_cache(keep_volume_keys)`: `SELECT volume_key FROM series_entry_cache`; delete each
  not in keep with `DELETE FROM series_entry_cache WHERE volume_key = ?`, all in one block; returns count.
- `upsert_catalog_series(row)`: `INSERT INTO catalog_series (series_key, folder_name, cover_path,
  volume_count, latest_volume_modified, total_pages, total_chars, missing_pages, damaged_volumes,
  scanned_at) VALUES (?x9, datetime('now')) ON CONFLICT(series_key) DO UPDATE SET <all> = excluded.*`
  (`scanned_at = excluded.scanned_at` i.e. now). `list_catalog_series()`: `SELECT * FROM catalog_series
  ORDER BY folder_name` -> `CatalogSeriesRow` (the 9 non-`scanned_at` fields, typed int/float).
  `prune_catalog_series(keep_keys)`: same scan-then-delete shape.
- `upsert_community_details(row)`: `INSERT INTO community_details (series_key, score, tags, genres,
  source, fetched_at) VALUES (?,?,?,?,?,?) ON CONFLICT(series_key) DO UPDATE SET score, tags, genres,
  source, fetched_at = excluded.*`; `tags`/`genres` = `json.dumps(list)` (default: ASCII-escaped,
  `", "` separators). `list_community_details()`: `SELECT * FROM community_details`, `json.loads` of
  `tags`/`genres` WITHOUT a safety fallback (a corrupt row raises in Python).

---------------------------------------------------------------------------------------------------

## 12. Rate limiting and client IP

### 12.1 `AuthAttemptLimiter` (S/security.py:91-139)
In-memory only, per key, thread-safe, uses a monotonic clock, NOT persisted, never garbage-collected.
Defaults: `max_failures=10`, `window_seconds=300`, `block_seconds=900`.
- `allow_attempt(key) -> (allowed, retry_after)`: if `blocked_until[key] > now` -> `(False,
  int(blocked_until - now) + 1)`. Else drop failures older than `now - 300`; if `len(failures) >= 10`:
  set `blocked_until = now + 900`, clear the failure list, return `(False, 900)`; else `(True, 0)`.
  So the 11th attempt inside 5 minutes is the first refused one; the block lasts 15 minutes; after the
  block expires the key starts clean.
- `record_failure(key)`: append `now`. `record_success(key)`: drop failures and block for the key.
- Key everywhere: `f"{get_client_ip(environ)}:{username}"` (username as typed, case-sensitive).
- **Two separate limiter instances exist**: `middleware/auth.py:AUTH_RATE_LIMITER` (used by
  `AuthMiddleware.authenticate` and the queue page auth, `queue/api.py`) and `login/api.py:
  AUTH_RATE_LIMITER` (used by `/login/api/check`, `/login/api/token`, `/login/api/me`). They do not
  share counts. Tests monkeypatch both to one instance. Rust may share one (stricter) - see open
  question 7. The account API and `authenticate_basic_header` have NO limiter.
- Bearer tokens are never rate limited (32 random bytes).
- Refusal message: `"Too many failed attempts. Retry in {retry_after}s"`; HTTP 429.

### 12.2 `get_client_ip(environ)` (security.py:59-80)
`remote = REMOTE_ADDR.strip()`. If `remote` is NOT a trusted proxy (loopback, or inside a configured
`server.trusted_proxies` network): return `remote` (proxy headers ignored entirely). Otherwise:
`X-Real-IP` if non-empty; else the RIGHTMOST entry of `X-Forwarded-For` (comma split, strip) if
non-empty; else `remote`. Private LAN addresses are NOT trusted by default. `set_trusted_proxies`
(config `server.trusted_proxies`, list of IPs/CIDRs; malformed -> config error) is installed at
`create_app`. `is_loopback_ip(v)`. Pinned by `tests/unit/test_client_ip.py`.

---------------------------------------------------------------------------------------------------

## 13. Authentication (`S/middleware/auth.py`)

### 13.1 Header parsing
- `parse_basic_auth_checked(header) -> (creds, error)` (:236-267):
  - header absent/empty -> `(None, None)` (anonymous);
  - header does not start with the literal, case-sensitive `"Basic "` (this includes `Bearer`, `Negotiate`,
    lowercase `basic`) -> `(None, None)` (anonymous - deliberate, for rclone/Windows first contact);
  - otherwise `base64.b64decode(header[6:])` (Python default = NON-strict: characters outside the base64
    alphabet are silently discarded, then the remainder must have valid padding, else error) followed by
    strict UTF-8 decode; any `ValueError`/`binascii.Error`/`UnicodeDecodeError` -> `(None, "Invalid
    authorization header")`; no `:` in the decoded text -> same error. Split at the FIRST `:` ->
    `(username, password)`; empty username or password is allowed through (rejected later).
  - Latin-1 payloads (e.g. `dXNlcjpw5HNz`) are errors (UTF-8 only). `"Basic !!!notb64!!!"` is an error
    (discards `!`, leaves `notb64` = 6 chars, bad padding). `"Basic "` (empty payload) -> error.
- `bearer_token(header)` (:289): header must start with the literal `"Bearer "`; returns `header[7:].strip()`
  (possibly the empty string, which still counts as a Bearer attempt and fails as an invalid token).
  Otherwise None.
- `authenticate_bearer(db, token)` (:296): `db.resolve_auth_token(token)`; None -> `AuthResult(False,
  role="anonymous", error="Invalid or expired token")`; else authenticated with `role = user.role`.
- Constant `INVALID_TOKEN_ERROR = "Invalid or expired token"`; Basic failures use `"Invalid
  credentials"`; malformed -> `"Invalid authorization header"`.
- `authenticate_basic_header(db, header)` (:308): Bearer or Basic as above WITHOUT rate limiting;
  used by the Account API.

### 13.2 `AuthResult` (:210-226)
`{authenticated: bool, user: UserDict|None, role: str = "anonymous", error: str|None,
attempted_username: str|None}`. `attempted_username` is set only for a failed/limited Basic attempt.

### 13.3 `AuthMiddleware.authenticate(environ)` (:470-515)
1. Bearer present -> `authenticate_bearer` (no limiter, no password check).
2. Basic parse error -> `AuthResult(False, "anonymous", error="Invalid authorization header")`, no limiter
   interaction.
3. No creds -> anonymous (`authenticated=False`, no error).
4. creds: key = `"{ip}:{username}"`; `allow_attempt`; if refused -> `AuthResult(False, error="Too many
   failed attempts. Retry in Ns", attempted_username=username)` (NO password check performed).
5. `db.authenticate_user(username, password)` (exactly ONE bcrypt check per request, tested). Success ->
   `record_success`; failure -> `record_failure`, `AuthResult(False, error="Invalid credentials",
   attempted_username=username)`.
No caching of authentication results inside `AuthMiddleware` (bcrypt cost is paid on every Basic
request). The only auth-result cache is in the queue page (21).

### 13.4 Per-request environment (`__call__`, :402-439)
Sets `environ["mokuro.auth"|"mokuro.user"|"mokuro.role"|"mokuro.username"|"mokuro.db"]` for downstream
layers (`mokuro.role` is `anonymous` when unauthenticated). Then `authorize`. On denial: if an
`on_processor_login_refused` callback is set AND status in (401, 429) AND path is a processor path AND
`attempted_username` is set -> callback(username, client_ip) (errors swallowed; used by the processor
registry's failed-login list, 18 `/api/processors`). Then the error response (13.5).

### 13.5 Error response (`_error_response`, :923-958)
Plain text body = the message (UTF-8), `Content-Type: text/plain; charset=utf-8`, no Content-Length.
Status line texts: 401 Unauthorized, 403 Forbidden, 404 Not Found, 405 Method Not Allowed, 429 Too Many
Requests. `WWW-Authenticate` added only on 401:
- if the request carried a failed BEARER token (`auth_result.error == "Invalid or expired token"`):
  `Bearer realm="mokuro-bunko", error="invalid_token"` (never a Basic challenge: it would pop a browser
  password dialog over a token-based page);
- otherwise `Basic realm="mokuro-bunko", charset="UTF-8"`. (`realm` is a constructor arg, default
  `mokuro-bunko`, always passed as `mokuro-bunko`.)
403 and 429 have no `WWW-Authenticate`.

### 13.6 `gate_read(environ, start_response, path)` (:441-468)
Used by the catalog volume manifest and the virtual queue file: re-runs authenticate+authorize on a COPY
of environ with `REQUEST_METHOD=GET` and `PATH_INFO=<path>` and returns None (allowed) or a refusal
(same `_error_response`, same Bearer challenge rule). Rust equivalent: a function
`gate_read(request, path) -> Option<Response>`.

---------------------------------------------------------------------------------------------------

## 14. Authorization matrix

### 14.1 Roles and permissions (auth.py:30-101)
Permissions: `READ, WRITE_PROGRESS, ADD_FILES, MODIFY_DELETE, MANAGE_INVITES, ADMIN, PROCESS`.

| role | READ | WRITE_PROGRESS | ADD_FILES | MODIFY_DELETE | MANAGE_INVITES | ADMIN | PROCESS |
|---|---|---|---|---|---|---|---|
| anonymous | Y | | | | | | |
| registered | Y | Y | | | | | |
| uploader | Y | Y | Y | | | | |
| inviter | Y | Y | Y | Y | Y | | |
| editor | Y | Y | Y | Y | | | |
| admin | Y | Y | Y | Y | Y | Y | |
| processor | Y | | | | | | Y |

Unknown role -> no permissions. Legacy `writer` is normalized to `uploader` at the DB boundary and is
accepted anywhere a role is accepted EXCEPT the admin change-role endpoint and the CLI choices (18, 19).
`processor` is a machine account: reads the library and the `/_processor/*` API only; it has no
WRITE_PROGRESS (cannot save progress), no ADD_FILES.

`METHOD_PERMISSIONS` (:82-96) exists but `authorize` does not consult it; the explicit branches below
are the truth.

### 14.2 Path predicates (auth.py:114-170; PathMapper.READER_ROOT = `mokuro-reader`,
`PER_USER_FILES = {volume-data.json, profiles.json, goals.json}` (webdav/resources.py:343-349))
Let `p = "/" + path.strip("/")` for the first three.
- `is_progress_file(path)`: `p` starts with `/mokuro-reader/` and the remainder is exactly one of the three
  per-user file names (single segment; `/mokuro-reader/S/profiles.json` is NOT progress).
- `is_library_path(path)`: `p` starts with `/mokuro-reader/` and remainder non-empty and not a per-user file
  name. (`/mokuro-reader` itself and `/mokuro-reader/` are NOT library paths.)
- `is_admin_path(path)`: `path.startswith("/_admin")` (raw, so `/_adminfoo` matches; the admin app's
  configurable `admin.path` is NOT consulted here, only in `AdminAPI`).
- `is_processor_path(path)`: `path == "/_processor"` or starts with `"/_processor/"` (`/_processors` is not).
- `is_invites_admin_api_path(path)`: `path == "/_admin/api/invites"` or starts with `"/_admin/api/invites/"`.
- `is_inbox_path`: `/inbox` or `/inbox/...` (defined, not used by `authorize`).
- Compiled metadata paths (`metadata/paths.py`): `is_catalog_file_path` (root `catalog.json` under the
  reader root; defined in the metadata spec) and `is_series_file_path` = exactly
  `/mokuro-reader/<Series>/series.json` (one folder level, filename compared case-insensitively to
  `series.json`, non-blank folder). `series_title_from_series_file_path` returns `<Series>`.
- `PATH_INFO` is first passed through `re_encode_wsgi_path` (latin-1 -> UTF-8 round trip). In Rust the
  path is already real UTF-8 (percent-decoded once); this is a no-op there. Invalid-UTF-8 request paths
  behave as "unchanged" in Python.

### 14.3 Decision procedure `authorize(environ, auth_result)` (auth.py:517-762), in order
Let `role = auth_result.role`, `authed = auth_result.authenticated`.
1. `OPTIONS` -> allow (CORS preflight; CORS middleware is outside anyway).
2. `not authed and error` (a failed/limited/malformed credential): 429 if `"Too many failed attempts"`
   in the error else 401, body = error message. (So a bad password to ANY URL, including public ones,
   is 401, never silent anonymous.)
3. Processor path: need `PROCESS` -> else 401 `"Authentication required"` (not authed) or 403
   `"Processor access required"`. If allowed -> authorized.
4. Admin path (`/_admin*`): if method is GET/HEAD and `"/api/"` not in the path -> authorized (static admin
   UI; no auth). Else required permission = `MANAGE_INVITES` for the invites API paths else `ADMIN`;
   lacking it -> 401 `"Authentication required"` (not authed) or 403 (`"Invite management access
   required"` for invites else `"Admin access required"`).
5. Methods `DELETE, MOVE, COPY, PROPPATCH, MKCOL, LOCK, UNLOCK` on a compiled metadata path ->
   `_compiled_metadata_denied`: 401 `"Authentication required"` if not authed else 403
   `"Permission denied: this file is compiled by the server"` (any role).
6. `MOVE`/`COPY`: parse `Destination` header: `urlparse(unquote(header), allow_fragments=False).path`
   (absolute URI or bare path, percent-decoded; `ValueError` -> None); if the destination is a compiled
   metadata path -> same denial as step 5.
7. `GET`, `HEAD`, `PROPFIND` -> authorized, EXCEPT unauthenticated:
   - `PROPFIND`: 401 `"Authentication required"` if `not allow_anonymous_browse`.
   - `GET`/`HEAD`: library path and `not allow_anonymous_download` -> 401; not a library path and `not
     allow_anonymous_browse` and path in (`"/"`, `"/mokuro-reader"`) -> 401 (other non-library GET paths
     pass).
   Authenticated users of ANY role (including `processor`) can GET/PROPFIND anything this layer sees.
8. `PUT` -> `_authorize_put` (14.4).
9. `MKCOL`: library path -> need `ADD_FILES` (401 `"Authentication required"` / 403 `"Permission denied:
   cannot create directories"`); non-library path -> 403 `"Permission denied: unsupported target path"`
   (even for admin).
10. `DELETE`: progress file -> `_authorize_progress_write`; else if `role == "uploader"` and authed and
    library path and `db.can_user_delete_library_path(username, path)` -> allow; else need
    `MODIFY_DELETE`: 401 `"Authentication required"` / 403 `"Permission denied: cannot modify or delete
    files"`. (Any path, including non-library, passes for MODIFY_DELETE holders; the DAV layer then
    decides.)
11. `MOVE`/`COPY`: progress file -> `_authorize_progress_write`; else need `MODIFY_DELETE` (same 401/403
    messages as DELETE). Uploaders can NOT move/copy (even their own).
12. `LOCK`/`UNLOCK`: progress file -> progress-write rule; else need `MODIFY_DELETE` (403 message
    `"Permission denied"`).
13. `PROPPATCH`: need `MODIFY_DELETE` (403 `"Permission denied"`).
14. Anything else -> allow.

`allow_anonymous_browse` / `allow_anonymous_download` properties (:385-400): read the LIVE
`RegistrationConfig` fields (`allow_anonymous_browse`, `allow_anonymous_download`, both default true;
the admin API mutates them at runtime without restart). `require_login` is only a legacy admin-API
input that sets both to `not require_login`.

### 14.4 `_authorize_put(path, auth_result)` (:789-868)
1. Progress file -> `_authorize_progress_write`.
2. Series file (`/mokuro-reader/<Series>/series.json`): not authed -> 401 `"Authentication required"`;
   `MODIFY_DELETE` holder -> allow; `role == "uploader"` and `db.can_user_edit_series(username,
   series_title)` -> allow; else 403 `"Permission denied: cannot submit metadata updates for this series"`
   (registered -> always this 403).
3. Any other compiled metadata path (root `catalog.json`) -> `_compiled_metadata_denied`.
4. Library path: lacks `ADD_FILES` -> 401 (not authed) or 403 `"Permission denied: cannot add files"`.
   Then, for roles WITHOUT `MODIFY_DELETE` (i.e. uploader): if `_replaces_unowned` -> 403 `"Permission
   denied: cannot replace a file another account uploaded"`. `_replaces_unowned`: only when the auth
   middleware was built with `storage_base_path`; maps the virtual path to a physical file via
   `PathMapper.virtual_to_physical(path, username)`; if the target does not exist (or unmappable) -> False
   (a new file is fine); if it exists: True unless `username` and `can_user_delete_library_path(username,
   path)` (the same ownership a DELETE checks, layer files included). Untracked legacy files belong to no
   one, so an uploader cannot overwrite them.
5. Else (non-library, non-progress, e.g. `/inbox/...`, `/`) -> 403 `"Permission denied: unsupported
   target path"`.

### 14.5 `_authorize_progress_write` (:889-921)
Not authed -> 401 `"Authentication required to save progress"`; lacks `WRITE_PROGRESS` (anonymous,
processor) -> 403 `"Permission denied: cannot save progress"`; else (the URL is always mapped to the
caller's private dir) allow. The `username and is_user_progress_path` tail always succeeds for
authenticated users.

### 14.6 `AdminAPI` second gate
`AdminAPI` independently checks `environ["mokuro.role"]` (set by this middleware): `role == "admin"` for
every `/api/*` endpoint; `role in ("admin","inviter")` for `/api/invites` and `/api/invites/...`.
Failure -> 403 JSON `{"error": "Admin access required"}` (or `"Admin or inviter access required"` for
invites) with NO 401 variant (anonymous would already have gotten 401 from step 4 above).

---------------------------------------------------------------------------------------------------

## 15. Login API (`S/login/api.py`)

Stack position: Registration, Login and Account are wrapped OUTSIDE `AuthMiddleware` (server.py:370-390; request flow is outermost first: RequestLog, SecurityHeaders, CORS, Static, Setup, Home, Account, Login, Registration, Queue, Catalog, QueueFile, Upload, Auth, Metadata, Processor, Admin, DAV). They therefore authenticate themselves and never see `mokuro.role`.

Common: JSON bodies <= 64 KiB (`MAX_JSON_BODY_BYTES = 65536`); responses `json.dumps(data)` (default
separators, ASCII-escaped), headers `Content-Type: application/json`, `Content-Length`. Status texts: 200
OK, 400 Bad Request, 401 Unauthorized, 429 Too Many Requests, 413 Payload Too Large, 500 Internal Server
Error. No `WWW-Authenticate` is ever sent (fetch()-consumed).
If the DB is missing: 500 `{"error":"Database not configured"}` (not reachable in practice).

Routes (matched on exact `PATH_INFO`):

| Method | Path | Notes |
|---|---|---|
| POST | `/login/api/check` | Legacy credential check. |
| POST | `/login/api/token` | Issue a bearer token. |
| DELETE | `/login/api/token` | Revoke the presented bearer token. |
| GET | `/login/api/me` | Identity + permissions. |
| GET | `/api/nav/config` | Nav feature flags. |
| GET | `/login`, `/login/`, `/login/<file>` | Static login page (22). Other methods fall through. |

### 15.1 `POST /login/api/check` (:95-139)
Body JSON `{"username","password"}`. `Content-Length == 0` -> 400 `{"error":"Missing credentials"}`;
> 64 KiB -> 413 `{"error":"Request body too large"}`; either field empty/falsy -> 400 `{"error":"Missing
credentials"}`; rate limit (login limiter, key `ip:username`) refused -> 429 `{"error":"Too many failed
attempts. Retry in Ns"}`; `authenticate_user` ok -> `record_success`, 200
`{"success": true, "user": {"username", "role"}}`; else `record_failure`, 401 `{"error":"Invalid
credentials"}`. JSON decode / UnicodeDecode (is a ValueError) -> 400 `{"error":"Invalid request"}`.
QUIRK: a JSON body that is not an object (list/number) -> `AttributeError` -> unhandled 500; Rust should
answer 400 `{"error":"Invalid request"}`.

### 15.2 `POST /login/api/token` (:141-211)
Body optional JSON object; fields: `username`, `password`, `kind` (default `"web"`), `label` (string,
else `""`). Order of evaluation:
1. `Content-Length` unparsable -> treated as 0; `> 64 KiB` -> 413 `{"error":"Request body too large"}`.
2. Body (if length > 0): invalid JSON/UTF-8 -> 400 `{"error":"Invalid request"}`; JSON non-object -> 400
   same.
3. If BOTH `username` and `password` falsy: fall back to the `Authorization` header: Basic parse error ->
   400 `{"error":"Invalid credentials"}`; creds present -> use them (a Bearer header yields no creds).
4. username/password must both be non-empty strings else 400 `{"error":"Missing credentials"}`.
5. `kind = data.get("kind") or "web"`; not in (`web`,`reader`,`processor`) -> 400
   `{"error":"kind must be one of web, reader, processor"}`.
6. Rate limit (login limiter, key `ip:username`): refused -> (if kind == `processor` and an
   on_processor_login_refused callback is set: report `(username, ip)`) 429 `{"error":"Too many failed
   attempts. Retry in Ns"}`.
7. `authenticate_user` failed -> `record_failure`; if kind processor report refusal; 401
   `{"error":"Invalid credentials"}`.
8. Success: `record_success`; `db.prune_expired_auth_tokens()`; `create_auth_token(username, kind,
   label=label)` (note: `kind` is NOT checked against the account's role: any account may request a
   `processor` token, but the token only grants that account's own role); 200:
```json
{"token":"<43 chars>","token_type":"Bearer","kind":"web","expires_at":1760000000.123,
 "user":{"username":"alice","role":"admin"}}
```
`expires_at` is epoch seconds (float). The `user` object has only `username` and `role`.

### 15.3 `DELETE /login/api/token` (:221-234)
`Authorization: Bearer <t>` required: missing/non-Bearer/empty token -> 400 `{"error":"No bearer token"}`;
else `revoke_auth_token(t)` -> 200 `{"revoked": true|false}` (false if unknown/already gone; no 401).

### 15.4 `GET /login/api/me` (:274-359) - "the authenticated boolean is load-bearing in EVERY response"
- Bearer header: valid -> 200 `{"authenticated":true,"username","role","created_at","permissions":P}`;
  invalid/expired/revoked -> 401 `{"authenticated":false,"error":"Invalid or expired token"}`. (No
  limiter.)
- Basic header present but malformed -> 401 `{"authenticated":false,"error":"Invalid credentials"}` (no
  limiter interaction).
- No header, or any other scheme -> 200 `{"authenticated":false,"role":"anonymous","permissions":P_anon}`
  (note: no `username`/`created_at` keys).
- Basic creds: rate limiter refused -> 429 `{"authenticated":false,"error":"Too many failed attempts.
  Retry in Ns"}`; success -> `record_success`, 200 as the Bearer success body; failure -> `record_failure`,
  401 `{"authenticated":false,"error":"Invalid credentials"}`.
`P` = `{"canWriteProgress": WRITE_PROGRESS, "canAddFiles": ADD_FILES, "canModifyDelete": MODIFY_DELETE,
"metadata": M}` evaluated for the role, where `M`:
- role has `MODIFY_DELETE` (inviter, editor, admin) -> `{"scope":"all"}`;
- role `uploader` with a username -> `{"scope":"owned","ownedSeries": db.list_series_owned_by(username)}`
  (sorted list of folder names, possibly empty);
- else (registered, anonymous, processor) -> `{"scope":"none"}`.
`metadata` is nested INSIDE `permissions` (reader's `identity.ts` reads `permissions.metadata`). Key
order in the JSON object: canWriteProgress, canAddFiles, canModifyDelete, metadata. `created_at` is the
raw DB string. Pinned by `tests/integration/test_login_me.py`.

### 15.5 `GET /api/nav/config` (:361-383) - no auth
Response 200: `{"home_enabled": bool, "catalog_enabled": bool, "queue_show_in_nav": bool,
"queue_public_access": bool, "registration_enabled": bool}` where (with a Config present):
`catalog_enabled = config.catalog.enabled`; `home_enabled = not (catalog.enabled and
catalog.use_as_homepage)`; `queue_show_in_nav = config.queue.show_in_nav`; `queue_public_access =
config.queue.public_access`; `registration_enabled = config.registration.mode != "disabled"`. (Without a
config object: home true, catalog true, queue_show_in_nav false, queue_public_access true,
registration true - not reachable in the real app.) Reads the LIVE config (admin edits apply at once).

---------------------------------------------------------------------------------------------------

## 16. Registration API (`S/registration/api.py`)

Config (`RegistrationConfig`, config.py:107-134): `mode` in `disabled|self|invite|approval` (default
`self`), `default_role` in `registered|uploader|inviter|editor` (default `registered`; `writer` ->
`uploader`), `allow_anonymous_browse`/`allow_anonymous_download` (default true), `require_login`
(legacy, default false). Invalid mode/default_role at load -> `ValueError`. The admin API mutates the
live object.

Routes (exact `PATH_INFO`), JSON responses as in 15 (status texts add 201 Created, 204 No Content, 403
Forbidden, 404 Not Found, 405 Method Not Allowed, 409 Conflict):

| Method | Path | Behaviour |
|---|---|---|
| POST | `/api/register` | Register. |
| GET | `/api/register`, `/api/register/config` | Mode info. |
| OPTIONS | both | 204, header `Allow: GET, POST, OPTIONS`, empty body. |
| other | `/api/register`, `/api/register/config` | 405 `{"error":"Method not allowed"}`. (`/api/register/config` is GET/OPTIONS only; POST there is 405.) |
| GET | `/register`, `/register/` | `register.html`. |
| GET | `/register/<file>` | static file from `registration/web` (404 JSON `{"error":"File not found"}` when `..` in name, leading `/`, missing; MIME via `mimetypes.guess_type`, default `application/octet-stream`; no cache header). |

### 16.1 `GET /api/register` info (:329-343)
200 `{"mode": <mode>, "enabled": mode != "disabled"}` plus `"requires_invite": true` when mode is `invite`.

### 16.2 `POST /api/register` (:112-184)
Order:
1. mode `disabled` -> 403 `{"error":"Registration is disabled"}`.
2. Body: `Content-Length` > 64 KiB -> 413 `{"error":"Request body too large"}`; JSON/UTF-8 error ->
   400 `{"error":"Invalid JSON body"}`. (Empty body -> JSON error -> 400.) A JSON non-object, or
   `username: null`, -> `AttributeError` -> 500 in Python; Rust should 400.
3. `username = data.get("username","").strip()` (Python `str.strip`, Unicode whitespace);
   `validate_username` error -> 400 `{"error": <message>}`.
4. `password = data.get("password","")` (NOT stripped); `validate_password` error -> 400.
5. Mode dispatch (live read of the config):
   - **self**: `db.get_user(username)` exists (any status, including `deleted`) -> 409 `{"error":"Username
     already exists"}`; `create_user(role=config.default_role, status="active")`; `ValueError` -> 400
     `{"error": str(e)}` (e.g. race on UNIQUE -> `already exists` message, still 400 here); success 201
     `{"success":true,"message":"Registration successful","username":u,"status":"active"}`.
   - **invite**: `invite_code = str(data.get("invite_code",""))` (a JSON null becomes the string
     `"None"`); empty -> 400 `{"error":"Invite code is required"}`; `validate_invite` None -> 400
     `{"error":"Invalid or expired invite code"}`; username exists -> 409 as above; `create_user(role =
     invite.role, status="active")`; `ValueError` -> 400; THEN `use_invite(code, username)` (result
     ignored; emits `invite_used` audit); success 201 same body as self (`"Registration successful"`,
     `status` active). The user is created BEFORE the invite is consumed: a concurrent second
     registration with the same code can also succeed (only the first `UPDATE ... WHERE used_by IS NULL`
     wins; the loser's account still exists). Rust may close this race with a transaction (safe
     tightening).
   - **approval**: exists -> 409; `create_user(role = default_role, status="pending")`; 201
     `{"success":true,"message":"Registration submitted for approval","username":u,"status":"pending"}`.
     Pending accounts cannot authenticate until an admin approves them.
Registration is NOT rate limited, NOT audited (except `invite_used`), and has no CAPTCHA.
`default_role` is applied as configured at request time.

---------------------------------------------------------------------------------------------------

## 17. Account API (`S/account/api.py`)

Authenticates with `authenticate_basic_header` (Bearer or Basic, NO rate limiter, so password-guessing
via these endpoints is unthrottled - QUIRK; decision in open question 7).
JSON conventions as in 15; status texts add 403/404.

| Method | Path | Behaviour |
|---|---|---|
| GET | `/api/account/stats` | Not authed -> 401 `{"error":"Authentication required"}`. Else 200 of an all-zero `PersonalStats`: `{"volumes":0,"pages_read":0,"characters_read":0,"reading_time_seconds":0,"reading_time_formatted":"0s"}` (placeholder; never computed from data). `_format_time`: `<60s` -> `"{s}s"`; `<60m` -> `"{m}m"`; `<24h` -> `"{h}h {m}m"`; else `"{d}d {h}h"`. |
| POST | `/api/account/password` | Body JSON `{"current_password","new_password"}`. Order: auth (401 `Authentication required`); body: `Content-Length == 0` -> 400 `{"error":"Missing request body"}`; > 64 KiB -> 413; JSON error -> 400 `{"error":"Invalid request"}`; either field falsy -> 400 `{"error":"Missing required fields"}`; `authenticate_user(username, current_password)` fails -> 401 `{"error":"Current password is incorrect"}`; `validate_password(new)` error -> 400 `{"error": msg}`; `update_user_password` (also revokes ALL the user's tokens, including the one used for this very request); 200 `{"success":true}`. Not audited. |
| POST | `/api/account/delete` | Body `{"password"}`. 401 / 400 `Missing request body` / 413 / 400 `Invalid request` as above; empty password -> 400 `{"error":"Password confirmation required"}`; wrong -> 401 `{"error":"Password is incorrect"}`; then audit `self_delete_account` (actor=username, target_type `user`, target_username=username), `delete_user(username)` (soft delete + token wipe), then if a storage path is configured: `users_root = <base>/users` resolved, `user_dir = users_root/username` resolved; not under `users_root` -> 400 `{"error":"Invalid username path"}` (after the soft delete already happened); else `shutil.rmtree(user_dir, ignore_errors=True)` if it exists (per-user progress data is permanently deleted); 200 `{"success":true}`. |
| OPTIONS | `/api/account/*` | 204, `Allow: GET, POST, OPTIONS`. |
| GET | `/account`, `/account/`, `/account/<file>` | static account page (22). Non-GET non-API falls through. |

Any authenticated role may use these (a `processor` account can change its password / delete itself).
The deleted account's volume ownership rows and audit rows remain.

---------------------------------------------------------------------------------------------------

## 18. Admin HTTP API (`S/admin/api.py`)

### 18.1 Mounting, gating, routing
- Mounted only when `config.admin.enabled` (default true); innermost API layer (inside `AuthMiddleware`).
  `AdminConfig`: `enabled=True`, `path="/_admin"` (config.py:197). `AuthMiddleware` hard-codes `/_admin`
  (14.2) while `AdminAPI` uses `config.path.rstrip("/")`; a custom path only works safely because
  `AdminAPI` re-checks the role (14.6). Recommend the Rust port keep the default and not support
  configurable paths unless the config spec requires it.
- `path.startswith(admin_path)` selects admin requests (so `/_adminXYZ` is "admin" too); `sub_path =
  path[len(admin_path):]` (or `/`).
- Non-`/api/` sub paths: serve static (22) for ANY method (and AuthMiddleware already required auth for
  non-GET/HEAD).
- `/api/*`: role gate (14.6) then `_handle_api` dispatch below. Unmatched -> 404 `{"error":"API endpoint
  not found"}` (also for a wrong method on a known path).
- Request body: `_parse_json_body`: `Content-Length` 0/absent -> `{}`; `> 65536` -> 400 `{"error":"Request
  body too large"}`; invalid JSON/UTF-8 -> 400 `{"error":"Invalid JSON body"}`. A non-object JSON body is
  not rejected and crashes later handlers (500); Rust should 400 `{"error":"Invalid JSON body"}`.
- Responses: JSON (`json.dumps`, `Content-Type: application/json`, `Content-Length`); status texts known:
  200, 201, 400, 403, 404, 409, 500 (others like 202/503 print as `"<code> Unknown"` - QUIRK: status line
  for 202 is `202 Unknown`, 503 `503 Unknown`; HTTP clients only use the code).
- Dispatch quirk (:493-512): user sub-routes are matched by SUFFIX, not anchored to `/api/users/`:
  `PUT *…/notes`, `PUT *…/role`, `POST *…/approve`, `POST *…/disable`, evaluated after `DELETE
  /api/users/<name>`. The username is `path[len("/api/users/"):-len(suffix)]`. Rust should anchor to
  `/api/users/<name>/<action>` (safe tightening) and URL-decode `<name>` once; names are `[A-Za-z0-9_-]`.
- Actor for audit: `environ["mokuro.user"]["username"]` else `environ["mokuro.username"]` else null.

### 18.2 Endpoint table

Permission: **A** = admin only; **A/I** = admin or inviter.

| # | Method | Path (after `/_admin`) | Perm | Group |
|---|---|---|---|---|
| 1 | GET | `/api/users` | A | users |
| 2 | POST | `/api/users` | A | users |
| 3 | DELETE | `/api/users/<u>` | A | users |
| 4 | PUT | `/api/users/<u>/notes` | A | users |
| 5 | PUT | `/api/users/<u>/role` | A | users |
| 6 | POST | `/api/users/<u>/approve` | A | users |
| 7 | POST | `/api/users/<u>/disable` | A | users |
| 8 | GET | `/api/invites` | A/I | invites |
| 9 | POST | `/api/invites` | A/I | invites |
| 10 | DELETE | `/api/invites/<code>` | A/I | invites |
| 11 | GET | `/api/audit` | A | audit |
| 12 | GET | `/api/settings` | A | settings |
| 13 | PUT | `/api/settings/registration` | A | settings |
| 14 | PUT | `/api/settings/cors` | A | settings |
| 15 | PUT | `/api/settings/catalog` | A | settings |
| 16 | PUT | `/api/settings/queue` | A | settings |
| 17 | PUT | `/api/settings/ocr` | A | settings (OCR) |
| 18 | PUT | `/api/settings/dyndns` | A | settings |
| 19 | GET | `/api/status` | A | status |
| 20 | GET | `/api/processors` | A | OCR/processors |
| 21 | GET | `/api/ocr/generations` | A | OCR |
| 22 | PUT | `/api/ocr/generations` | A | OCR |
| 23 | GET | `/api/ocr/generations/stats` | A | OCR |
| 24 | POST | `/api/ocr/generations/derive` | A | OCR |
| 25 | PUT | `/api/ocr/generations/<id>/pools` | A | OCR |
| 26 | POST/GET/DELETE | `/api/ocr/generations/<key>/bench` | A | OCR |
| 27 | POST | `/api/ocr/devices/refresh` | A | OCR |
| 28 | GET | `/api/tunnel/status` | A | tunnel |
| 29 | POST | `/api/tunnel/start` | A | tunnel |
| 30 | POST | `/api/tunnel/stop` | A | tunnel |
| 31 | GET | `/api/dyndns/status` | A | dyndns |
| 32 | POST | `/api/dyndns/start` | A | dyndns |
| 33 | POST | `/api/dyndns/stop` | A | dyndns |
| 34 | POST | `/api/dyndns/test` | A | dyndns |

Route-order trap: in `_handle_api`, `/api/ocr/generations/stats`, `/api/ocr/generations`,
`/derive`, `/devices/refresh` are matched before the `…/pools` and `…/bench` suffix routes; the `bench`
route matches ANY method (handler returns 404 for methods other than POST/GET/DELETE).

### 18.3 Users

1. `GET /api/users` -> 200 `{"users":[UserDict,...]}` (all statuses, `created_at DESC`).
2. `POST /api/users` body `{"username","password","role"="registered","notes"=""}`:
   `username = data.get("username","").strip()`; empty -> 400 `{"error":"Username is required"}`;
   `validate_username` -> 400; empty password -> 400 `{"error":"Password is required"}`; `validate_password`
   -> 400. `db.create_user(username, password, role, notes=notes)` (status `active`; `role` normalized
   by `normalize_role`, so `anonymous`/`writer` are accepted, invalid -> `ValueError("Invalid role: x. Must
   be one of: ['admin', 'anonymous', 'editor', 'inviter', 'processor', 'registered', 'uploader']")`);
   `user = db.get_user(username)`; audit `admin_create_user` (details `{"role": <raw role from body>}`);
   201 `{"success":true,"user":UserDict}`. `ValueError` -> 409 if the lowercased message contains
   `"already exists"` else 400, body `{"error": msg}`. The deleted-account message does NOT contain "already
   exists" -> 400. `notes` of non-string type -> SQLite bind error -> 500 (Rust: 400).
3. `DELETE /api/users/<u>`: `delete_user`; true -> drop any connected processor of that account
   (`registry.drop_account(u, "its account was deleted")`; errors swallowed), audit `admin_delete_user`,
   200 `{"success":true,"message":"User '<u>' deleted"}`; false (unknown OR already deleted) -> 404
   `{"error":"User '<u>' not found"}`. No self-delete or last-admin protection.
4. `PUT /api/users/<u>/notes` body `{"notes": str}`: `notes = data.get("notes","")`; non-string -> 400
   `{"error":"notes must be a string"}`; `update_user_notes` true -> audit `admin_update_notes`, 200
   `{"success":true,"user":UserDict}`; else 404 `{"error":"User '<u>' not found"}`.
5. `PUT /api/users/<u>/role` body `{"role"}`: falsy -> 400 `{"error":"Role is required"}`; not in
   `["registered","uploader","inviter","editor","admin","processor"]` -> 400 `{"error":"Invalid role. Must
   be one of: ['registered', 'uploader', 'inviter', 'editor', 'admin', 'processor']"}` (Python list repr);
   `update_user_role` true -> if role != `processor` drop the user's connected processors (`"its account's
   role was changed to <role>"`); audit `admin_change_role` (`{"role"}`); 200
   `{"success":true,"user":UserDict}`; else 404 `{"error":"User '<u>' not found"}`. `writer` and
   `anonymous` are rejected here.
6. `POST /api/users/<u>/approve`: `approve_user` true -> audit, 200 `{"success":true,"user":UserDict}`; else
   404 `{"error":"User '<u>' not found or not pending"}`.
7. `POST /api/users/<u>/disable`: `disable_user` true -> drop processors (`"its account was disabled"`),
   audit `admin_disable_user`, 200 `{"success":true,"user":UserDict}`; else 404 `{"error":"User '<u>' not
   found"}`.
`drop_account` is the processor registry (OCR subsystem); in a Rust port without that subsystem it is a
hook invoked after DB success.

### 18.4 Invites
8. `GET /api/invites` -> 200 `{"invites":[InviteInfo,...]}` ALL invites (used + expired), newest first, each
   `{"code","role","status":"valid|expired|used","created_at","expires_at","used_by","invited_by"}`.
9. `POST /api/invites` body `{"role"="registered","expires"="7d"}`: `role` must be in
   `sorted(INVITABLE_ROLES)` = `['editor','inviter','registered','uploader']` else 400 `{"error":"Invalid
   role. Must be one of: ['editor', 'inviter', 'registered', 'uploader']"}` (so `writer` is refused here
   though `Database.create_invite` accepts it); `create_invite(role, expires, invited_by = actor)`;
   `ValueError` (bad duration) -> 400 `{"error": msg}`; success 201 `{"success":true,"invite":InviteInfo}`.
   Non-string `expires` -> `TypeError` -> 500 (Rust 400). QUIRK: an `inviter` may mint invites of ANY
   invitable role including `editor` (no cap by the inviter's own role). Audit `invite_created` is written
   inside `create_invite`.
10. `DELETE /api/invites/<code>`: `delete_invite` true -> audit `invite_deleted` (target_path = code), 200
    `{"success":true,"message":"Invite deleted"}`; else 404 `{"error":"Invite not found"}`.

### 18.5 Audit
11. `GET /api/audit` query (parsed with `parse_qs`: percent-decoding, `+` = space, BLANK values dropped):
    `actor`; `action` and `target_type` (repeatable and/or comma-separated; parts stripped; empties
    dropped); `since`, `until`; `q`; `include_progress` (`1|true|yes`, case-insensitive); `cursor`; `limit`
    (default 50). `one(name)` = first value stripped, empty -> none. `limit` not an int -> 400
    `{"error":"limit is not a number"}` (note `limit=0`/negative clamp to 1; > 200 clamp to 200).
    `AuditQueryError` -> 400 `{"error": msg}`. Success 200:
```json
{"events":[{"id":1,"actor_username":"alice","action":"upload","target_type":"library",
            "target_path":"/mokuro-reader/S/V1.cbz","target_username":null,
            "details":"{\"existed_before\":false}","created_at":"2026-10-01 11:02:03"}],
 "next_cursor":"<base64url or null>","total":123,
 "facets":{"actors":[...],"actions":[...],"target_types":[...]}}
```
`total` and `facets` are present only on a first page (no `cursor`); on later pages `total` is `null` and
`facets` is absent. `details` is a JSON STRING. Pinned by `test_admin_api.py:1400-1478`.

### 18.6 Settings (mutate the LIVE config object and persist via `save_config(config, config_path)`;
guarded by one `_config_lock`; all return 500 `{"error":"Config not available"}` if there is no full
config; body-parse errors 400)

12. `GET /api/settings` -> 200 `config.to_dict()` (the full config tree; defined by the config spec) with
    `dyndns.token` replaced by `"****"` when non-empty, plus the top-level key `ocr_runtime` (cached OCR
    runtime status; **DROP** the mokuro-env fields, see 18.8; keep `{"available":false}` shape when there
    is no OCR).
13. `PUT /api/settings/registration`: optional keys `mode` (must be in `disabled|self|invite|approval`
    else 400 `{"error":"Invalid mode. Must be one of: ['disabled', 'self', 'invite', 'approval']"}`),
    `default_role` (must be in `registered|uploader|inviter|editor` else 400 `{"error":"Invalid default_role.
    Must be one of: ['registered', 'uploader', 'inviter', 'editor']"}`; `writer` refused here),
    `allow_anonymous_browse`, `allow_anonymous_download` (coerced with Python `bool()`), legacy
    `require_login` (sets both anonymous flags to `not bool(v)`, applied last so it wins). Validation errors
    return immediately and leave previously applied keys of the same request applied but UNSAVED (QUIRK:
    partial in-memory mutation); saved on success. 200:
    `{"success":true,"registration":{"mode","default_role","allow_anonymous_browse","allow_anonymous_download",
    "require_login":(not browse and not download)}}`.
14. `PUT /api/settings/cors`: `enabled` (bool()), `allowed_origins` (must be a list else 400
    `{"error":"allowed_origins must be a list"}`; elements unchecked). 200 `{"success":true,"cors":
    {"enabled","allowed_origins"}}`. (Takes effect on the CORS middleware only if it reads the live config;
    the middleware is only mounted when CORS was enabled at startup.)
15. `PUT /api/settings/catalog`: `enabled`, `reader_url` (`.strip().rstrip("/")`, applied only if non-empty;
    non-string -> `AttributeError` 500; Rust 400), `use_as_homepage`. 200 `{"success":true,"catalog":
    {"enabled","reader_url","use_as_homepage"}}`.
16. `PUT /api/settings/queue`: `display` (must be one of `QUEUE_DISPLAY_LEVELS = ("minimal","normal",
    "detailed")` if present and non-null, else 400 `{"error":"display must be one of: minimal, normal,
    detailed","field":"display"}`), `show_in_nav`, `public_access` (bool()). After saving, bumps the OCR
    queue state (`control.queue_state.bump()`) if an OCR control exists. 200 `{"success":true,"queue":
    {"show_in_nav","public_access","display"}}`.
18. `PUT /api/settings/dyndns`: `enabled` (bool); `provider` must be `duckdns` or `generic` else 400
    `{"error":"Invalid provider"}`; `token` set unless it equals the mask `"****"`; `domain`; `update_url`;
    `interval` int `>= 30` else 400 `{"error":"interval must be at least 30"}` (any `int()` failure too).
    Saves, then reconfigures the running DynDNS service if present. 200 `{"success":true,"dyndns":
    {"enabled","provider","domain","update_url","interval","token":"****" if set else ""}}`.

### 18.7 Status
19. `GET /api/status` -> 200:
```json
{"uptime": <seconds float since AdminAPI construction>, "host": "<server.host>", "port": <server.port>,
 "storage_path": "<abs path>", "disk_total": N, "disk_used": N, "disk_free": N,
 "user_count": <users with status != 'deleted'>, "volume_count": <count of immediate subdirectories of storage.library_path>,
 "stats": {}}
```
Disk numbers from `shutil.disk_usage(base_path)` (0 on OSError); with no full config: host "" port 0
storage_path "" and zeros. `volume_count` counts SERIES FOLDERS (immediate dirs), not volumes.
20. `GET /api/processors` (OCR/remote-processor contract; **KEEP the endpoint shape, implementation belongs to
    the processor subsystem**) -> 200 `{"processors":[...], "speed":[...], "failed_logins":[{"username",
    "reason","at"}...], "last_disconnect": {"name","at"}|null, "local_processing": bool, "processing_hold":
    ...}`. When no registry: `{"processors":[],"failed_logins":[],"last_disconnect":null,
    "local_processing":<bool>,"processing_hold":<hold>,"speed":[...]}`. `failed_logins` is fed by the
    `on_processor_login_refused` hooks (13.4 and 15.2) with reason `"invalid credentials from <ip>"`.

### 18.8 OCR endpoints - contracts only (internals belong to the OCR spec)
- 17 `PUT /api/settings/ocr`: body keys. Refusals (400 `{"error":...}`): any `backend` -> `"OCR backend is
  launch-only. Use CLI flags/config file to change it."`; `char_map` -> removed message; any of `engines`,
  `detector`, `patch_budget` -> `"<keys> moved into the generations list; PUT them to /api/ocr/generations"`.
  `poll_interval` positive int else 400 `{"error":"poll_interval must be a positive integer"}`; saved; if it
  changed and an OCR control exists it is applied live. 200 `{"success":true,"ocr":{"backend","poll_interval",
  "concurrency"}, "applied":bool,"installing":bool,"restart_required":bool,"reason":str,"ocr_runtime":{...}}`.
- 21 `GET /api/ocr/generations` -> 200 `{"stats_pending":bool,"generations":[row...],"catalog":{...},
  "processors":[...],"local_processing":bool,"autobench":bool}`; 500 `{"error":"Config not available"}` if no
  config. Row = the generation spec dict + derived fields (`sidecar`, `effective_detector`, `detector_locked`,
  `patch_budget_applies`, `precision_applies`, `road`, `stages[]`, `volumes_done`, `volumes_total`,
  `volumes_skipped`, `volumes_by_machine`, `congestion`, `bench`, `configured`, `local_pools`, `local_bench`,
  `local_runs`, `processor_pools`, `processor_bench`, `processor_runs`, `processor_congestion`,
  `processor_stages`, optional `precision`/`precision_on`, `precision_hold`). Catalog `{engines[],
  detectors[], devices, patch_budgets, ..., name_pattern, reserved_names, reserved_prefixes}`.
  **DROP** `engines` entries that need the mokuro environment (`own_environment`/`monolithic` mokuro engine)
  and the detectors `ctd`, `animetext`, `rtdetr` (and any `DISABLED_DETECTORS`); `admin.js` (203 KB) is the
  consumer and defines which fields are really read.
- 22 `PUT /api/ocr/generations` body `{"generations":[rows]}` full replacement; errors 400 `{"error","row":
  index|null,"field":str|null}` (unknown `id` -> row index + field `id`); 200 `{"success":true, ...GET payload,
  "applied","installing","restart_required","reason","ocr_runtime"}`; prunes bench/profile data of removed rows.
- 23 `GET /api/ocr/generations/stats` -> 503 `{"error":"Config unavailable"}` or 200 `{"stats_pending":true}`
  or `{"stats_pending":false,"computed_at":"YYYY-MM-DDTHH:MM:SSZ","generations":{<id>:{"volumes_done",
  "volumes_total","volumes_skipped","volumes_by_machine"}}}`. Library-derived counts, cached 60 s in the
  background; reads `ocr_sidecars` via `ocr_sidecar_producers()`, `catalog_series` via
  `list_catalog_series()`, `series_entry_cache`/metadata cache through `cached_page_count`/
  `cached_missing_pages`.
- 24 `POST /api/ocr/generations/derive` body `{"spec": {...}, "processor"?: name}` -> 200 `{"road","stages":[...]}`;
  400 `{"error","row","field"}` or `{"error":"no processor called '<n>' is known"}`.
- 25 `PUT /api/ocr/generations/<id>/pools` body `{"processor": name, "pools": {...}}` -> 200
  `{"success":true,"pools":{...}}`; 400 `{"error":...}` variants (`processor must name a processor`,
  `there is no generation '<id>'`, `no processor called ...`, `pools must be an object` with `field`).
- 26 `/api/ocr/generations/<key>/bench`: POST body `{"spec"?,"pages"?,"processor"?}` -> 202 bench object;
  GET (`?processor=`) -> 200 bench object; DELETE (`?processor=`) -> 200 cancel result; `BenchError` ->
  `e.status` `{"error","row","field"}`; no config -> 500 `{"error":"Config not available"}`. Key is a generation
  id or `^draft-[a-z0-9-]{1,24}$`.
- 27 `POST /api/ocr/devices/refresh` -> 200 `{"success":true,"devices":[...]}` (re-probes GPUs in the engines
  env).
Whole OCR admin surface depends on the OCR design the Rust port ends up with; if a sub-feature is DROPped,
return the same response shape with empty/neutral values rather than removing the route (admin.js expects
the keys), or confirm admin.js is being replaced (open question 9).

### 18.9 Tunnel and DynDNS (thin wrappers; contract only)
28 `GET /api/tunnel/status`: no service -> 200 `{"running":false,"url":null,"available":false}`; else the
service's `status` dict. 29 `POST /api/tunnel/start`: no service -> 500 `{"error":"Tunnel service not
available"}`; `RuntimeError` -> 400 `{"error": msg}`; else 200 `{"success":true, ...status}`. 30 `POST
/api/tunnel/stop`: 500 if no service else 200 `{"success":true, ...status}`. 31 `GET /api/dyndns/status`: no
service -> 200 `{"enabled":false,"running":false}` else `status()` dict. 32/33 start/stop: 500 `{"error":"DynDNS
service not available"}` if none else 200 `{"success":true, ...status()}`. 34 `POST /api/dyndns/test`: 500 if
none else 200 `update_now()` result dict.

---------------------------------------------------------------------------------------------------

## 19. Admin CLI (`S/admin/cli.py`)

Click group `admin` (`mokuro-bunko admin <cmd>`); config path from `ctx.obj["config_path"]`; every command
loads config, opens `Database(<base_path>/mokuro.db)` WITHOUT `configure_connection` (defaults 5000 ms /
5 retries / 0.05 s). Role choices for add-user/change-role/restore-user:
`registered, uploader, inviter, editor, admin, processor` (no `writer`, no `anonymous`); invite role choices
`sorted(INVITABLE_ROLES)`. The CLI writes NO audit rows except the invite ones from the DB layer (actor
null). Failures print to stderr and `exit(1)`. Password options use `prompt=True, hide_input=True,
confirmation_prompt=True`.

| Command | Behaviour / exact output |
|---|---|
| `add-user USERNAME [--role R=registered] [--password]` | `create_user(username, password, normalize_role(role))` (status active). OK: `User '<u>' created with role '<r>'`. `ValueError` -> stderr `Error: <msg>`, exit 1. |
| `delete-user USERNAME [--yes/-y]` | without `-y`: confirm `Delete user '<u>'?` (abort -> exit 1). `delete_user`: `User '<u>' deleted` else stderr `User '<u>' not found`, exit 1. |
| `list-users [--status active|pending|disabled|deleted]` | empty -> `No users found` / `No <status> users found`. Else header `f"{'Username':<20} {'Role':<12} {'Status':<10} {'Created':<20}"`, a line of 64 `-`, then per user the same widths with `created_at[:19]`. |
| `change-role USERNAME ROLE` | `update_user_role`; `User '<u>' role changed to '<r>'` else stderr `User '<u>' not found`, exit 1. |
| `generate-invite [--role R=registered] [--expires 7d]` | `create_invite(normalize_role(role), expires)` (invited_by null); prints `Invite code: <code>`, `Role: <r>`, `Expires in: <expires>`. `ValueError` -> `Error: <msg>` exit 1. |
| `list-invites [--all]` | none -> `No invites found`. `--all`: header `Code(24) Role(12) Expires(20) Used By(15)`, 73 dashes, rows with `expires_at[:19]` and `used_by or "-"`. Default (unused & not expired by the quirky SQL of 7): header `Code(24) Role(12) Expires(20)`, 58 dashes. |
| `delete-invite CODE` (`ignore_unknown_options`, so a code starting with `-` works) | `Invite '<c>' deleted` else stderr `Invite '<c>' not found`, exit 1. |
| `restore-user USERNAME [--role R] [--password]` | `restore_user(u, pw, normalize_role(role) if role else None)`; `User '<u>' restored`; false -> stderr `Error: '<u>' is not a deleted account`, exit 1; `ValueError` -> `Error: <msg>` exit 1. The default role is "the role it had". |
| `approve-user USERNAME` | `User '<u>' approved` else stderr `User '<u>' not found or not pending`, exit 1. |
| `disable-user USERNAME` | `User '<u>' disabled` else stderr `User '<u>' not found`, exit 1. |
| `set-password USERNAME [--password]` | `update_user_password` (revokes tokens): `Password updated for '<u>'`; false -> stderr `User '<u>' not found` exit 1; `ValueError` -> `Error: <msg>` exit 1. |

Notes: the CLI process's `users_version` bump is invisible to a running server (21 TTL applies). Changes
become visible to the server's next request because every request reads the DB. The `setup_cli`
(`setup_cli.py:158`) also opens the DB to create the first admin (not specified here).
Tests: `tests/integration/test_admin_cli.py`.

---------------------------------------------------------------------------------------------------

## 20. First-run setup (DB-touching part only; `S/setup/api.py`)

`needs_setup` = no user with `role == "admin"` exists (`list_users()`; any status counts, including
deleted/disabled admins). Cached as True-complete once seen. `POST /setup/api/complete` (localhost only;
`get_client_ip`-based) creates the admin: `data["admin"]["username"/"password"]` validated with
`validate_username`/`validate_password`, `db.create_user(username, password, "admin")`; `ValueError` -> 409
`{"error": msg}`; optional `registration.mode` applied; config saved; 201 `{"success":true,"message":"Setup
completed successfully"}`. Full contract belongs to the setup/home spec.

---------------------------------------------------------------------------------------------------

## 21. Queue page auth cache (consumer of `users_version`; `S/queue/api.py:36-250`)

The public queue page authenticates a viewer itself (outside `AuthMiddleware`) with a result cache so a
page polling once a second does not pay bcrypt each time:
- Bearer: always checked directly (`authenticate_bearer`), never cached; invalid -> viewer `failed`.
- Basic: cache key = HMAC-SHA256 of the whole `Authorization` header under a per-process random key;
  value `(expiry, users_version, role|None)`. Success TTL `AUTH_CACHE_SECONDS = 30`, failure TTL
  `AUTH_FAIL_CACHE_SECONDS = 60`, max `AUTH_CACHE_SIZE = 256` entries (on overflow drop expired; if still
  full, clear all). A hit is honoured only if `cached.users_version == db.users_version` and not expired.
- Cache miss: parse (garbage/non-Basic -> failed, cached as failure); limiter (the MIDDLEWARE's
  `AUTH_RATE_LIMITER`, key `ip:username`) refused -> viewer `limited` (not cached); `authenticate_user`
  failure -> `record_failure`, cached failure; success -> `record_success`, cached role.
- `users_version` is per-process: a CLI password change is not seen until the TTL expires (<= 60 s). The
  Rust port needs an in-process counter bumped by the same 7 mutators if it keeps this cache.

---------------------------------------------------------------------------------------------------

## 22. Static assets

Served from the package's `web/` directories; the Rust build must embed/serve the same files with the same
routing: `login/web/{index.html,login.js,styles.css}`, `registration/web/{register.html,register.js,
styles.css}`, `account/web/{index.html,account.js,styles.css}`, `admin/web/{index.html,admin.js,
styles.css}`, plus `S/static/nav.js` (shared nav/auth helper, served by StaticMiddleware at `/_static/`).
- Login/account: `GET <prefix>/` or bare prefix -> `index.html`; `GET <prefix>/<file>` -> file; path
  traversal outside the web dir -> 403 text `Forbidden`; missing -> 404 text `Not found`; read error -> 500
  text `Error`. MIME: `.html` `text/html; charset=utf-8`, `.js` `application/javascript; charset=utf-8`,
  `.css` `text/css; charset=utf-8`, else `application/octet-stream`; header `Cache-Control: no-cache`.
- Admin (no auth needed to fetch): `/` -> `index.html`; traversal -> 403 `Forbidden`; a MISSING file falls
  back to `index.html` (SPA); MIME map adds `.json .png .jpg .jpeg .webp .ico .svg`; `Cache-Control:
  no-cache`. Pinned: `test_admin_api.py:433-460`.
- Registration: see 16 (no `Cache-Control`, MIME via `mimetypes`).

---------------------------------------------------------------------------------------------------

## 23. Tests that pin behaviour (read; all must keep passing in spirit)

- `tests/unit/test_database.py` - duration parsing, user CRUD semantics, invite CRUD, ownership,
  series-fold identity (`test_database.py:584-609`), bcrypt `$2` prefix, unique salts.
- `tests/unit/test_database_resilience.py` - commit lock retry (external lock released -> success; retries
  exhausted -> raises), `configure_connection` clamps (`busy_timeout` 250 round trip), audit prune
  throttle, expired-invite cleanup with local-naive isoformat, server wiring of `database.*` config.
- `tests/unit/test_auth_tokens.py`, `test_auth_token_requests.py` - section 6 and 15, Bearer challenge.
- `tests/unit/test_audit_search.py`, `tests/integration/test_admin_api.py` (`TestAuditAPI`) - section 8/18.5.
- `tests/unit/test_permissions.py` - role matrix, path predicates, Basic parsing vectors (incl. Latin-1 =
  error, empty username/password pass-through of the parser, colon in password).
- `tests/unit/test_processor_role.py`, `test_remote_revocation.py` - `processor` role/permission, account
  stamp, refused-login hook.
- `tests/integration/test_auth.py` - full authorization matrix incl. UTF-8 auth, exactly one bcrypt per
  request, limiter counts one failure per request, garbage header = 401 with no limiter failure,
  `WWW-Authenticate` charset.
- `tests/integration/test_login_me.py` - `/login/api/me` contract.
- `tests/integration/test_registration.py`, `test_admin_cli.py`, `tests/unit/test_invites.py`,
  `test_non_ascii_ownership.py`, `test_client_ip.py`.

---------------------------------------------------------------------------------------------------

## 24. Open questions

1. **bcrypt version / >72-byte passwords.** Passwords may be 8-128 chars (up to 512 bytes UTF-8); bcrypt
   uses 72 bytes. bcrypt 4.x truncates silently, 5.x raises. Which bcrypt version produced existing installs' hashes? Proposed:
   Rust truncates to 72 bytes on verify and hash (compatible with 4.x). Also: NUL bytes in a password
   (the Rust bcrypt crate errors; Python <5 accepts).
2. **Schema-version downgrade.** Python unconditionally writes 6. If a future/other build writes >6, should
   Rust refuse to open (safer) or also overwrite? Also should Rust bump the version when it adds nothing?
   (Proposal: keep 6, run identical idempotent steps, never go lower than an existing larger value.)
3. **Invite `expires_at` timezone.** Local-naive ISO text is a bug-prone format (TZ change/DST shifts
   validity, `list_invites(include_used=False)` compares it to UTC text). Reproduce exactly (needed for
   Python interop) or convert to UTC on write? Proposal: reproduce on write (Python must read it), parse
   leniently, and run the identical SQL for `list_invites`.
4. **`forget_volume_uploads_under_prefix` LIKE over-deletion.** Reproduce (unescaped, case-insensitive
   LIKE) or use exact `substr` as the OCR tables do? Proposal: exact prefix (a bug fix, no storage change).
5. **Username `$`/trailing-newline quirk, non-object JSON bodies, `null` fields, non-string notes/
   expires/reader_url.** Python 500s; proposal: Rust returns 400 with the nearest existing error message.
6. **`disable-user` is irreversible.** There is no re-enable path (approve needs `pending`). Add one
   (e.g. approve also revives `disabled`) or keep? Out of drop-in scope; flagged because admin.js may offer
   a button.
7. **Rate limiter sharing and coverage.** Python has two independent limiter instances and none on the
   account endpoints. Proposal: ONE shared limiter keyed `ip:username` applied to login, token, me,
   AuthMiddleware Basic checks, queue, and account endpoints. Is stricter acceptable (it could lock out a
   user who failed 6 times on each of two surfaces)?
8. **`users_version` across processes.** Keep the in-process counter (queue cache up to 60 s stale after a
   CLI change) or key the cache on something DB-derived (e.g. `PRAGMA data_version`, or max `updated_at`)?
9. **admin.js compatibility.** Is the existing 203 KB `admin.js` reused unchanged? If so every field listed
   in 18.8 (and anything it reads from `/api/settings` `config.to_dict()`) is a hard contract; if the
   OCR panel is trimmed for DROPped engines, confirm whether `/api/ocr/*` returns neutral shapes or 404s.
10. **Configurable `admin.path`.** Python half-supports it (middleware hard-codes `/_admin`). Support it or
    fix the path to `/_admin`?
11. **Idle features to port or drop:** `cleanup_expired_invites` (never scheduled), `revoke_user_auth_tokens`
    (no caller), `list_audit_events` (tests only), `InviteManager.list_valid`, `METHOD_PERMISSIONS`,
    `PersonalStats` (always zeros), `is_inbox_path`, `parse_basic_auth` compat wrapper. Proposal: port
    `cleanup_expired_invites`/`revoke_user_auth_tokens` as library functions, skip the rest.
12. **Expired token and audit-row hygiene.** Tokens are pruned only on token issuance; a server where nobody
    logs in again keeps expired rows indefinitely (harmless). Add a periodic prune? (Not required for
    compatibility.)
13. **`volume_uploads.existed_before` argument.** Both branches are identical in Python; Rust can ignore
    the parameter. Confirm the WebDAV spec does not need a different behaviour for overwritten untracked
    archives (the auth layer already blocks uploaders from replacing unowned files, 14.4).
14. **Audit `invite_created` stores the plaintext invite code in `target_path`** and it is readable by
    admins through `/api/audit` for 30 days; kept for compatibility (admin.js/audit page may link codes).
15. **Default TZ for `datetime.now()`** inside containers is UTC unless `TZ` is set; Rust's local-time
    source must follow `TZ` the same way (chrono `Local`), otherwise invite expiries shift on mixed
    deployments.
