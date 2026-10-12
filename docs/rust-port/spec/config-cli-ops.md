# Spec: config, CLI, setup wizard, SSL, tunnel, DynDNS, doctor, logging, packaging

Subsystem of mokuro-bunko 0.5.2 (Python) to be re-implemented in Rust as a drop-in over existing `config.yaml` files.
All paths are relative to `src/mokuro_bunko/` unless prefixed (`deploy/`, `scripts/`, `docs/`, `tests/` are repo root).
Citations are `file:line`. Tags: **KEEP** (port), **DROP** (Python/mokuro/venv-only, remove), **REPLACE** (needs a Rust-native equivalent), **QUIRK** (observed behaviour that may be a bug; decide per Open Questions).

---------------------------------------------------------------------------

## 1. Config file: location, loading, precedence

### 1.1 Default paths (config.py:28-43, ssl.py:20-32)
- Config file default: Linux/macOS `$XDG_CONFIG_HOME/mokuro-bunko/config.yaml` (fallback `~/.config/...`); Windows `%LOCALAPPDATA%\mokuro-bunko\config.yaml` (fallback `~\AppData\Local`). (config.py:37-43)
- Storage default `storage.base_path`: Linux/macOS `$XDG_DATA_HOME/mokuro-bunko` (fallback `~/.local/share/mokuro-bunko`); Windows `%LOCALAPPDATA%\mokuro-bunko`. (config.py:28-34)
- Auto-cert directory: same data base + `mokuro-bunko/certs/{cert.pem,key.pem}` (ssl.py:31-32). NOTE: this is NOT under `storage.base_path`. A drop-in Rust port must keep this location (or read both).
- Config path selection: global `-c/--config PATH` flag, else env `MOKURO_CONFIG`, else default (__main__.py:25-31). Sub-commands resolve `ctx.obj["config_path"] or get_default_config_path()`; those that pass `None` to `load_config` (`serve`, `ssl status`, `config show`, `doctor`, `admin *`) get the same default through `load_config` itself (config.py:518-527).

### 1.2 Load algorithm (`load_config`, config.py:518-543)
1. `path` None -> default path.
2. If the file does not exist -> `Config()` all defaults (NOT an error, and nothing is written).
3. Else `yaml.safe_load`; empty file/`null` -> `{}`. Then `Config.from_dict` (config.py:487-515).
4. Then `_apply_env_overrides(config)` (config.py:668-710), in all cases (even when the file is missing).

Precedence (lowest to highest): built-in defaults < YAML file < canonical env `MOKURO_<SECTION>_<KEY>` < alias env `MOKURO_HOST/PORT/STORAGE` < CLI flags of `serve` (see 4.1).

### 1.3 `from_dict` behaviour (config.py:487-515)
- Top-level sections read: `server storage registration cors ssl admin catalog queue database ocr dyndns`. Missing section -> `{}` -> defaults. Unknown TOP-LEVEL keys are silently ignored.
- Each section is built via `Section(**dict)`: an UNKNOWN KEY INSIDE A SECTION raises `TypeError` (Python "unexpected keyword argument"), not `ValueError`. A section whose value is YAML `null` (e.g. `server:` with nothing) also raises `TypeError` (`**None`). A top-level YAML list/scalar raises `AttributeError`. **QUIRK**: `config *` commands only catch `ValueError`/`YAMLError` (config_cli.py:34-37) so these show a Python traceback; `serve`, `ssl`, `dyndns`, `tunnel`, `admin` also traceback. Q1.
- Legacy migrations in `from_dict`:
  - `registration.require_login` is read only if NEITHER `allow_anonymous_browse` NOR `allow_anonymous_download` is present: both are then set to `not require_login` (config.py:490-499). If either explicit key is present, `require_login` is ignored (but still accepted as a key, since `RegistrationConfig` has the field).
  - `registration.default_role: writer` -> `uploader` (config.py:126-127).
- Retired/removed `ocr.*` keys are refused with `ValueError` (see 3.10).

### 1.4 No hot reload
There is no file watcher, SIGHUP, or reload of `config.yaml`. The `Config` object is built once at start and mutated in place by the admin API (see 1.7). Most settings "take effect at startup" (ocr.concurrency, sessions, local_processing, autobench, backend). `queue.*`, `registration.*`, `catalog.*` changes through the admin API are read live from the same shared object. DynDNS settings via admin API call `DynDNSService.configure()` live (admin/api.py:2316-2318). Editing the file by hand requires a restart.

### 1.5 Save algorithm (`save_config`, config.py:546-562)
- `path.parent.mkdir(parents=True, exist_ok=True)`, then `yaml.safe_dump(config.to_dict(), f, default_flow_style=False)`.
- Consequences for the Rust writer (must be semantically identical; byte-identical only if Q3 says so):
  - FULL rewrite of the file from the in-memory model: all comments and unknown keys are lost; every section/key is emitted, including defaults.
  - PyYAML default `sort_keys=True`: keys emitted alphabetically within each mapping (top level: `admin, catalog, cors, database, dyndns, ocr, queue, registration, server, ssl, storage`).
  - Block style; lists as `- item`; non-ASCII escaped (default `allow_unicode=False`).
  - Not atomic (direct open/truncate/write, no temp+rename). Not permission-restricted (the file holds `dyndns.token` in clear text).
  - `registration.require_login` is ALWAYS written, derived as `(not allow_anonymous_browse) and (not allow_anonymous_download)` (config.py:431-434).
- **QUIRK (important)**: `load_config` applies env overrides before returning, and every writer (`config set`, `cors-add/-remove`, `ssl enable/disable`, `dyndns *`, tunnel CORS prompt, admin API `_save_config` admin/api.py:2671-2674, setup wizard setup/api.py:161-162) saves the loaded-and-env-overridden object. So any `MOKURO_*` env var in force at that time is baked into the YAML on the next save (e.g. Docker `MOKURO_STORAGE=/data` is persisted as `storage.base_path`; with nginx accel the backend `MOKURO_PORT=8081` is persisted as `server.port`). Q2.

### 1.6 `to_dict()` (config.py:414-485): canonical serialised shape
See section 3 for every key. Notable: `storage.base_path` is written as `str(path)` after `expanduser`; `ocr.generations` is written via `GenerationSpec.to_dict()` (3.10); `cors.allowed_origins` as given. NOT written: nothing omitted.

### 1.7 Runtime config mutation by the admin API (cross-reference; HTTP surface is another agent's)
Admin API endpoints that mutate + save config (admin/api.py): `PUT /api/settings/registration` (mode, default_role, allow_anonymous_browse, allow_anonymous_download, legacy require_login; 400 on bad mode/default_role) :960-1015; `/cors` (enabled, allowed_origins must be a list, entries NOT validated) :1030-1052; `/catalog` (enabled, reader_url stripped and `rstrip("/")`, empty ignored, use_as_homepage) :1060-1090; `/queue` (show_in_nav, public_access, display in minimal|normal|detailed else 400 `{"error":"display must be one of: ...","field":"display"}`) :1105-1143; `/ocr` (poll_interval int>=1 else 400 "poll_interval must be a positive integer"; `backend` refused 400 "OCR backend is launch-only..."; `char_map`, `engines`, `detector`, `patch_budget` refused) :1160-1215; `PUT /api/ocr/generations` replaces `ocr.generations` :1321-1324; `/dyndns` (token replaced only if not literally `"****"`; interval>=30 else 400 "interval must be at least 30"; provider in duckdns|generic else 400 "Invalid provider") :2284-2325. `GET /api/settings` returns `to_dict()` with `dyndns.token` masked as `"****"` plus `ocr_runtime` (:946-952). Writes are serialised by `self._config_lock`. `ssl`, `server`, `storage`, `database`, `admin`, `ocr.concurrency|sessions|local_processing|autobench` are NOT editable through the API.

---------------------------------------------------------------------------

## 2. Environment variable overrides

### 2.1 Canonical rule (config.py:668-698)
For every key present in `_CONFIG_TYPES` (config.py:565-607), env var = `MOKURO_` + dotted key with `.` -> `_`, upper-cased. If set (even to the empty string; the test is `is not None`), `set_by_dotted_key(config, key, value)` is applied (string -> typed per 2.3).

Keys that HAVE an env var (36): `server.{host,port,trusted_proxies}`, `storage.base_path`, `registration.{mode,default_role,allow_anonymous_browse,allow_anonymous_download,require_login}`, `cors.{enabled,allow_credentials}`, `ssl.{enabled,auto_cert,cert_file,key_file}`, `admin.{enabled,path}`, `catalog.{enabled,reader_url,use_as_homepage,enrich_community}`, `queue.{show_in_nav,public_access,display}`, `ocr.{backend,poll_interval,concurrency,sessions,local_processing,autobench,generations}`, `dyndns.{enabled,provider,token,domain,update_url,interval}`.

Keys with NO env var and NOT settable via `config set`: `cors.allowed_origins` (use `config cors-add/-remove`), the entire `database.*` section (`busy_timeout_ms`, `lock_retries`, `retry_initial_delay_seconds`). (`config set` of anything not in `_CONFIG_TYPES` -> `KeyError "Unknown config key: k"`, config.py:635-637.)

### 2.2 Aliases (config.py:700-710), applied AFTER canonical vars (so they win)
`MOKURO_HOST` -> `server.host`; `MOKURO_PORT` -> `server.port`; `MOKURO_STORAGE` -> `storage.base_path`. The Docker/Unraid entrypoints rewrite `MOKURO_HOST/PORT` to point python at the backend (127.0.0.1:8081) when nginx accel is on (deploy/docker-entrypoint.sh:25-27, docker-entrypoint.unraid.sh:115-118), so the aliases MUST keep overriding even a `MOKURO_SERVER_PORT`.

### 2.3 Type coercion from env / `config set` (`set_by_dotted_key`, config.py:610-665)
- Key must be `section.field` (exactly 2 dot-parts) else `KeyError("Invalid key: ... Expected format: section.field")`; then `Unknown config section: X`, `Unknown field 'f' in section 's'`, `Unknown config key: k` in that order.
- bool: case-insensitive `true|1|yes` -> True; `false|0|no` -> False; else `ValueError("Invalid boolean value: X")`.
- int (`server.port`, `ocr.poll_interval`, `ocr.concurrency`, `dyndns.interval`): `int(value)`; failure -> `ValueError`.
- Path (`storage.base_path`): `Path(value)` with NO `expanduser` (QUIRK: `~` from env/`config set` is not expanded, unlike a YAML value, which goes through `StorageConfig.__post_init__`, config.py:82-86).
- list (`server.trusted_proxies`): comma-split, each stripped, empties dropped. Validated by constructing `ServerConfig(trusted_proxies=...)` (each element must parse as `ipaddress.ip_network(x, strict=False)`: CIDR or single address, v4/v6; else `ValueError "server.trusted_proxies: 'x' is not a network or address"`).
- `queue.display`: not in `minimal|normal|detailed` -> `ValueError("Invalid queue display level: ... (expected one of: ...)")` (NB from a FILE an invalid display only logs a warning and falls back to `normal`; from env/`config set` it is a hard error).
- `ocr.generations`: value is JSON text -> `parse_generation_list` (3.10); `GenerationConfigError` is a `ValueError`.
- str: as-is.
- **QUIRK (validation gap)**: env/`config set` values go through `setattr`, so section `__post_init__` validators do NOT re-run. Not enforced from env: `server.port` range, `registration.mode`, `registration.default_role` (and the `writer` alias), SSL enabled-needs-certs, `ocr.backend` enum, `ocr.poll_interval>=1`, `ocr.concurrency` 1..8, `dyndns.provider`, `dyndns.interval>=30`. A bad `MOKURO_REGISTRATION_MODE=foo` loads fine and misbehaves later. Rust may validate strictly. Q4.

### 2.4 Retired-key refusal in the environment (config.py:668-696)
If `MOKURO_OCR_CHAR_MAP` is set -> `ValueError` (removed key). If any of `MOKURO_OCR_ENGINES`, `MOKURO_OCR_DETECTOR`, `MOKURO_OCR_PATCH_BUDGET` is set -> `ValueError` naming `MOKURO_OCR_GENERATIONS` as the replacement. These checks run on every load, before the canonical loop. Message substrings asserted by tests/unit/test_config.py:527-610: the env var name, `MOKURO_OCR_GENERATIONS`, "removed".

### 2.5 Non-config env vars read by the server
| Var | Where | Meaning |
|---|---|---|
| `MOKURO_CONFIG` | __main__.py:29 | config path (click `envvar`) |
| `MOKURO_THREADS` | server.py:525 | request thread count, default 50 (cheroot `numthreads`). Docs: each connected processor holds one thread, plus one per open session. |
| `MOKURO_NGINX_ACCEL` | server.py:239 | exactly `"1"` flags requests so library file GETs emit `X-Accel-Redirect: /internal-library/<rel>`. (The Unraid entrypoint also accepts `true` and normalises to `1` before exec: docker-entrypoint.unraid.sh:76-79.) |
| `MOKURO_DEBUG` | middleware/request_log.py:17-18 | non-empty and not `0`/`false` -> request logging middleware |
| `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `LOCALAPPDATA` | config.py:30-42, ssl.py:27-29 | default paths |
| `MOKURO_BUNKO_OCR_ENV`, `MOKURO_BUNKO_OCR_ENGINES_ENV`, `MOKURO_BUNKO_MOKURO_SPEC`, `HF_HOME`, `PIP_CACHE_DIR`, `TMPDIR` | ocr/installer.py:48,407,808,971; processor/service.py:26 | **DROP/REPLACE**: Python venv management (section 12) |
| `MOKURO_OCR_STAGE_WORKERS/CPU_WORKERS/QUEUE_CAPACITY/STAGE_DEVICE/DETECT_TIMEOUT/PIPELINE_STATS/JOBS`, `MOKURO_PPOCR_*`, `MOKURO_EFT_TRACE`, `MOKURO_PROCESSOR_RUNNER/ENGINES_PYTHON`, `ANIMETEXT_VARIANT` | ocr/* , processor/* | other agents' scope; listed for completeness |
| `PUID PGID UMASK TAKE_OWNERSHIP OCR_AUTO_INSTALL MOKURO_BACKEND_PORT MOKURO_LIBRARY` | deploy/*.sh only | container entrypoint inputs, not read by Python |

---------------------------------------------------------------------------

## 3. Every config key

"env" = canonical `MOKURO_SECTION_KEY` exists (2.1). File-load validation = dataclass `__post_init__`; env/`config set` skip it (2.3).

### 3.1 `server` (ServerConfig, config.py:46-69)
| key | type | default | validation | env |
|---|---|---|---|---|
| host | str | `"0.0.0.0"` | none | yes + alias `MOKURO_HOST` |
| port | int | `8080` | `0 <= port < 65536` else `ValueError("Invalid port: N")` (0 = ephemeral, valid) | yes + alias `MOKURO_PORT` |
| trusted_proxies | list[str] | `[]` | each element a valid network/address (see 2.3), else `ValueError("server.trusted_proxies: 'x' is not a network or address")` | yes (comma list) |

Meaning: networks of reverse proxies whose `X-Real-IP`/`X-Forwarded-For` are believed; loopback is always trusted (security.py:31-80). Installed at app creation via `set_trusted_proxies` (server.py:188). Client-IP rule (`get_client_ip`, security.py:59-80): if the peer (`REMOTE_ADDR`) is not loopback/trusted -> the peer address; else `X-Real-IP` if non-empty; else the RIGHTMOST `X-Forwarded-For` entry; else the peer. Private (RFC1918) peers are NOT trusted by default. The `/setup` local-only check uses this (section 9).

### 3.2 `storage` (StorageConfig, config.py:71-103)
| key | type | default | validation | env |
|---|---|---|---|---|
| base_path | path str | platform default (1.1) | `expanduser()` on YAML values (str -> Path) | yes + alias `MOKURO_STORAGE` (no expanduser, 2.3) |

Derived directories (NOT configurable): `library = base/library`, `inbox = base/inbox`, `users = base/users`. `ensure_directories()` (config.py:98-103) creates `library`, `inbox`, `users` (all `parents=True, exist_ok=True`) and `library/thumbnails`.
Other fixed names under `base_path` (grep of src; other specs own contents): `mokuro.db` (SQLite; server.py:194), `logs/server.log`, `logs/ocr/` (logging_setup.py:23-30), `.ocr-failures.json`, `.ocr-progress.json`, `.ocr-heartbeat`, `.ocr-bench.json`, `.ocr-congestion.json`, `.processing/` (OCR staging/samples), `processors/` (per-machine profiles), `bench/` (processor-side), `.tmp`/`.pip-cache` (Unraid entrypoint only). Probe files created+deleted by checks: `.mokuro-doctor-probe`, `.mokuro-write-test`.

### 3.3 `registration` (RegistrationConfig, config.py:106-135)
| key | type | default | validation |
|---|---|---|---|
| mode | enum | `self` | in `disabled|self|invite|approval` else `ValueError("Invalid registration mode: X")` |
| default_role | enum | `registered` | `writer` -> `uploader` first; must be in `registered|uploader|inviter|editor` else `ValueError("Invalid default role: X. Must be one of: (...)")` (admin/processor/anonymous refused) |
| allow_anonymous_browse | bool | true | none |
| allow_anonymous_download | bool | true | none |
| require_login | bool | false | legacy; see 1.3 and 1.5 |
Env: all five. YAML values are not type-coerced (Q5).

### 3.4 `cors` (CorsConfig, config.py:137-176)
| key | type | default |
|---|---|---|
| enabled | bool | true |
| allowed_origins | list[str] | `["https://reader.mokuro.app","http://localhost:5173","http://localhost:*","http://127.0.0.1:*"]` |
| allow_credentials | bool | true |
Matching (config.py:146-176): if `not enabled` -> False. For each pattern: no `*` -> exact string equality with the Origin. With `*`: only patterns ENDING in `:*` are meaningful; origin must start with `pattern[:-2]` and the remainder must be `:` followed by one or more ASCII digits (`str.isdigit`). Any other wildcard form never matches. Env: `enabled`, `allow_credentials` only. The middleware applying it is another agent's.

### 3.5 `ssl` (SslConfig, config.py:178-194)
| key | type | default |
|---|---|---|
| enabled | bool | false |
| auto_cert | bool | false |
| cert_file | str | `""` |
| key_file | str | `""` |
File-load validation: `enabled and not auto_cert and (not cert_file or not key_file)` -> `ValueError("SSL enabled but cert_file and key_file not provided. Either provide cert paths or set auto_cert: true")`. Env path does not re-validate (startup check 5 catches it later; with empty cert_file the message is `SSL certificate file not found: .`). Env: all four.

### 3.6 `admin` (config.py:196-201): `enabled` bool true; `path` str `"/_admin"`. No validation. Env: both.

### 3.7 `catalog` (config.py:204-212): `enabled` false; `reader_url` `"https://reader.mokuro.app"`; `use_as_homepage` false; `enrich_community` true (starts the AniList/MAL `CommunityFetcher` only if `catalog.enabled and enrich_community`, server.py:458). No validation. Env: all four.

### 3.8 `queue` (config.py:215-243): `show_in_nav` false; `public_access` true; `display` str default `normal`. Invalid `display` from FILE: `logging.warning("queue.display %r is not one of %s; using 'normal'")` and coerced to `normal` (never fails startup). From env/`config set`: hard error (2.3). Env: all three.

### 3.9 `database` (config.py:245-262): `busy_timeout_ms` int 5000 (`>=100` else `ValueError("Database busy timeout must be at least 100 ms")`); `lock_retries` int 5 (`>=1` else `"Database lock retries must be at least 1"`); `retry_initial_delay_seconds` float 0.05 (`>0` else `"Database retry initial delay must be positive"`; doubles on each retry). FILE-ONLY (no env, no `config set`). Applied to both Database handles (server.py:196-199, 947-951). The admin CLI's `Database(path)` ignores them (admin/cli.py:30).

### 3.10 `ocr` (OcrConfig, config.py:330-377; generations in ocr/generations.py)
| key | type | default | validation | env |
|---|---|---|---|---|
| backend | enum | `auto` | `auto|cuda|rocm|cpu|skip` else `ValueError("Invalid OCR backend: X")` | yes |
| poll_interval | int | 30 | `>=1` else `ValueError("Invalid poll interval: N")` | yes |
| concurrency | int | 1 | whole number 1..8 (`MAX_OCR_CONCURRENCY=8`, config.py:271); parsed `int(str(v).strip())`; messages `Invalid OCR concurrency 'x'`, `Invalid OCR concurrency: N (must be at least 1)`, `Invalid OCR concurrency: N (at most 8; ...)` (config.py:274-290) | yes |
| sessions | bool | true | | yes |
| local_processing | bool | true | | yes |
| autobench | bool | true | | yes |
| generations | list[GenerationSpec] | one row `{id:g-1,name:mokuro,engine:mokuro,primary:true,enabled:true}` (generations.py:394-408) | `parse_generation_list` (below) | yes (JSON text) |

Derived: `processes_locally(config) = local_processing and backend != "skip"` (server.py:154-162).

**Retired keys** (config.py:294-327), checked on the `ocr` mapping before dataclass construction: `char_map` -> `ValueError("ocr.char_map: the character-map system was removed (...) — delete the key")`; `engines`, `detector`, `patch_budget` -> `ValueError("ocr.engines, ocr.detector was replaced by ocr.generations, a list of named OCR recipes (...) — rewrite the ocr section as generations, e.g. generations: [{name: mokuro, engine: mokuro, primary: true}]")` (all present retired keys named). Removed key check runs first.

**`ocr.generations` schema** (ocr/generations.py:489-960; owned jointly with the OCR spec; reproduced because a bad row stops startup and the file round-trips on every save):
- Accepts: YAML list of mappings; JSON string (env/`config set`/`serve --generations`; empty/whitespace/None -> default one-row list); a single mapping (wrapped); `[]`/absent -> default list. A non-mapping row -> `GenerationConfigError("ocr.generations[i]: each generation must be a mapping of fields, got X")`. JSON syntax error -> `"ocr.generations must be a list of generations, or the JSON text of one; could not read it as JSON (...)"`. Row index in errors is 0-based.
- Row fields: `id` (optional; `^[A-Za-z0-9][A-Za-z0-9_-]{0,31}$`, unique; missing -> minted `g-<max+1>`), `name`, `engine` (required; one of `mokuro|hayai-nova|paddle-manga|ppocr-manga`), `primary` (bool, default false), `enabled` (bool, default true), `detector` (only meaningful for engines without a built-in detector: `ppocr-manga` default | `ctd`; `animetext` refused as disabled; omitted for mokuro/ppocr-manga engines), `patch_budget` (256|384|512, default 512; null = default), `pools` (mapping, only `stage_workers`, `queue_capacity`, `stage_device`, and legacy `precision`), `precision` (`auto-accuracy` default | `auto-balanced` | `auto-speed` | `fp32|bf16|fp16`; forced format must be one the engine runs, mokuro: fp16/fp32 only; engines outside the precision policy always normalise to default), `precision_pick`/`precision_why` (benchmark outputs; not stored normally), `char_map` -> refused.
- `name`: `^[a-z0-9][a-z0-9-]{0,31}$` (full match), not in reserved `{original,gcv,updated-ocr}`, no `tr-` prefix, unique across ALL rows (enabled or not). A missing name is seeded: the engine id, or `<engine>-<detector>` when the engine has no built-in detector; cut to 32 chars, trailing `-` stripped, collisions get `-2`,`-3`... (generations.py:438-470). The name is the sidecar postfix `<Volume>.<name>.mokuro` (bare `<Volume>.mokuro` for the primary row).
- Exactly one ENABLED row must be `primary` (none -> "no enabled generation is the primary one"; two -> "... are all marked primary"). If no row is enabled, no check.
- `pools.stage_workers` per stage 0..64; `pools.queue_capacity` per stage 1..256; stage keys must belong to the row's road: `line` (detect, layout), `reconciled` (detect, engine, post), `adapter` (detect, engine, post), `served` (feed, mokuro, post), monolithic mokuro row (`mokuro` only). `pools.stage_device` keys limited to model-bearing stages (`detect`, `engine`, or `mokuro`), values `auto|cpu|gpu:<n>` validated against a device catalog (before any probe: `auto`, `cpu`, any well-formed `gpu:<n>`); CPU-only stages refuse GPUs.
- `to_dict()` key order (generations.py:373-391): `id, name, primary, enabled, engine, [detector], patch_budget, [precision if != auto-accuracy], pools{stage_workers, queue_capacity, stage_device (each key-sorted)}, [precision_pick, precision_why]`.
- Engine/detector classification: `mokuro` (mokuro env, road served, no patch budget) is **DROP-or-replace** per scope; `hayai-nova` (patch budget applies), `paddle-manga`, `ppocr-manga` (built-in detector, CPU only). Detectors: `ppocr-manga` (Apache), `ctd` (GPL, **DROP**), `animetext` (**DROP**, already refused).
- A deployed config.yaml nearly always contains the default `mokuro` row (config.example.yaml:174-183; Unraid default). A drop-in Rust server MUST still parse such a file. Q6.

### 3.11 `dyndns` (DynDNSConfig, config.py:379-396)
| key | type | default | validation |
|---|---|---|---|
| enabled | bool | false | |
| provider | enum | `duckdns` | `duckdns|generic` else `ValueError("Invalid DynDNS provider: X")` |
| token | str | `""` | |
| domain | str | `""` | |
| update_url | str | `""` | generic provider template |
| interval | int | 300 | `>=30` else `ValueError("DynDNS interval must be at least 30 seconds")` |
Env: all six. Token is stored in clear text; API/CLI mask it as `****`.

---------------------------------------------------------------------------

## 4. Root CLI (`__main__.py`)

Framework: click group `cli` (`invoke_without_command=True`). Entry point `mokuro-bunko = mokuro_bunko.__main__:main` (pyproject.toml `[project.scripts]`).

Global options (__main__.py:24-39): `-c/--config PATH` (envvar `MOKURO_CONFIG`; need not exist); `-v/--verbose` flag (only `serve` consumes it); `--version` prints `mokuro-bunko, version 0.5.2`. No subcommand -> prints help, exit 0. Global flags must precede the subcommand. `processor *` subcommands have their own `--config`.

`main()` calls `tolerant_console_streams(sys.stdout, sys.stderr)` (__main__.py:259-284): non-UTF-8 streams are reconfigured `errors="replace"`. Rust: write UTF-8 lossily; not needed on Unix.

Exit codes: click usage error 2; `click.ClickException` 1 (`Error: ...` on stderr); explicit `sys.exit(1)` where listed; `doctor` 1 on any FAIL; startup validation failure in `serve` 2 (server.py:603-607).

### 4.1 `serve` (__main__.py:45-113)
Options: `--host TEXT` (default `0.0.0.0`), `--port INT` (default 8080), `--ocr [auto|cuda|rocm|cpu|skip]` (default `auto`), `--generations JSON`.
Algorithm: `load_config(config_path)` (incl. env) -> `if host != "0.0.0.0": server.host = host`; `if port != 8080: server.port = port`; `if ocr != "auto": ocr.backend = ocr`; `if generations: ocr.generations = parse_generation_list(generations)` (ValueError -> ClickException). **QUIRK**: an option equal to its default is indistinguishable from "not passed", so `--port 8080` cannot override `MOKURO_SERVER_PORT=9000` or a YAML port (Q7). With `-v` echoes `Verbose mode enabled` and `Storage path: X`. Then `run_server(config, config_path, verbose)`. `load_config` errors (ValueError/TypeError/YAMLError) are NOT wrapped: traceback.

`run_server` sequence (server.py:592-1037), config-relevant parts:
1. `_validate_startup_environment` (section 5): failure prints `Startup validation failed: <msg>` and exits 2.
2. `setup_logging(storage, verbose)` (section 8).
3. Logs `Starting mokuro-bunko server on <http|https>://host:port`, `Storage path: ...`, `Server log: ...`, and `SSL: ...` when enabled.
4. OCR environment selection/auto-install (**DROP/REPLACE**, server.py:625-893): when local processing is on and backend != skip it detects hardware and installs the mokuro / engines venvs if missing.
5. `create_ssl_server` -> `create_app` and a cheroot server with `numthreads = MOKURO_THREADS or 50`; SSL adapter if `ssl.enabled` (6.3). `create_app` also: `ensure_directories()`; DB at `base/mokuro.db` with the `database.*` knobs; `TunnelService` + `DynDNSService` instantiated; `dyndns_service.start()` iff `dyndns.enabled` (server.py:207-208); `SetupWizardAPI` always in the chain (server.py:403); library watcher; metadata rescan every 6 h (server.py:~455); `CommunityFetcher` iff catalog.enabled and enrich_community.
6. `ThreadPoolWatchdog` (**DROP**: cheroot-specific, 5 s checks, cheroot_watchdog.py:30) and `_start_server_resilient` (**DROP**: clears cheroot's interrupt flag, server.py:545-589).
7. OCR worker start (other spec); prints `Press Ctrl+C to stop`.
8. On KeyboardInterrupt (SIGINT; Python has no explicit SIGTERM handler): prints `Shutting down...`, stops watchdog, OCR worker, `remote.drop_all("the library server is shutting down")`, `shutdown_app` (library watcher, community fetcher, metadata service, propfind cache, DynDNS; server.py:470-495), `server.stop()`. Rust: handle SIGINT and SIGTERM (Docker/systemd send SIGTERM).

### 4.2 `install-ocr` (__main__.py:116-245) - **DROP / REPLACE**
Options: `--force`, `--backend [auto|cuda|rocm|cpu]` (default auto), `--list-backends`, `--engines TEXT` (comma list of `mokuro,hayai-nova,paddle-manga,ppocr-manga`; default `mokuro`), `--detector [ppocr-manga|ctd]` (default `ppocr-manga`). Behaviour: detects hardware, lists/validates the backend (`Supported OCR backends:` / `Unavailable backends:` with reasons), creates/rebuilds the mokuro venv (pip install of the mokuro fork from GitHub + torch wheels, with CPU fallback) and a second "engines" venv, smoke-tests both. No venv in Rust. If the Rust OCR needs model downloads, add a differently shaped command; keep `install-ocr` as a documented no-op/alias only if scripts must not break (deploy/docker-entrypoint.unraid.sh:95-97 and scripts/setup-windows.ps1 call it with `--backend X`). Q8.

### 4.3 `setup` (setup_cli.py:28-173) - interactive wizard, KEEP
Option `--skip-if-exists`. Path = global config path or default. Flow (prompts in order; defaults in brackets):
1. Exists + `--skip-if-exists`: echo `Config file already exists at {path}, skipping setup.`, return 0. Exists otherwise: confirm `Config file exists at {path}. Overwrite?` (no -> return).
2. Echo `=== mokuro-bunko setup ===`.
3. `Storage path` [default storage path]; `Server port` [8080, int].
4. `Enable SSL?` [n]; if yes `  Generate a self-signed certificate?` [y] -> `SslConfig(enabled=True, auto_cert=True)`, else prompts `  Path to certificate file`, `  Path to private key file`.
5. `Create an admin user?` [y]: `  Admin username` [admin], `  Admin password` (hidden, confirmed). NO `validate_username/validate_password` here (QUIRK vs web wizard; the DB layer may validate).
6. `Registration mode` Choice(disabled|self|invite|approval) [self].
7. `Access method` Choice(lan|cloudflare|dyndns|reverse-proxy) [lan]: dyndns -> `  DynDNS provider` (duckdns|generic) [duckdns], `  Domain`, `  API token` (hidden), generic: `  Update URL`; builds an enabled DynDNSConfig (interval 300). cloudflare / reverse-proxy -> only echo a hint.
8. `Add custom CORS origins?` [n] -> loop `  Origin (empty to finish)`.
9. Builds `Config(server=ServerConfig(host="0.0.0.0", port), storage, registration=RegistrationConfig(mode), cors, ssl, dyndns)`; prints `=== Configuration Summary ===` + YAML; confirm `Save this configuration?` [y], else `Setup cancelled.`
10. `save_config`; echo `Config saved to {path}`. Admin: `ensure_directories()`, `Database(base/mokuro.db).create_user(user, pw, "admin")` -> `Admin user '{u}' created`; any exception -> stderr `Warning: Could not create admin user: {e}` (continues).
11. If auto-cert: generate the default pair when `cert.pem` is absent -> `SSL certificate generated at {cert_path}`.
12. Echo `Setup complete! Run 'mokuro-bunko serve' to start the server.`
The wizard builds a fresh Config (env overrides NOT applied, other sections default).

### 4.4 `doctor` (doctor_cli.py:209-254): see section 10.

### 4.5 `admin` group (admin/cli.py) - details belong to the DB/auth spec
Every subcommand: `db = Database(load_config(config_path).storage.base_path / "mokuro.db")` (ctor creates the parent dir, database.py:448). `ValueError` -> stderr `Error: {e}`, exit 1. Role choices everywhere: `registered|uploader|inviter|editor|admin|processor`, normalised by `normalize_role` (legacy alias `writer` -> `uploader`).
| command | args/flags | output / exit |
|---|---|---|
| `add-user USERNAME` | `--role` (default registered); `--password` (prompt, hidden, confirm) | `User '{u}' created with role '{role}'` |
| `delete-user USERNAME` | `-y/--yes` skips `confirm("Delete user 'x'?", abort=True)` | `User 'x' deleted` / stderr `User 'x' not found` exit 1 |
| `list-users` | `--status active|pending|disabled|deleted` | empty -> `No users found` / `No {status} users found`; else header `{Username:<20} {Role:<12} {Status:<10} {Created:<20}`, 64 dashes, rows with `created_at[:19]` |
| `change-role USERNAME ROLE` | | `User 'x' role changed to 'r'` / not found exit 1 |
| `generate-invite` | `--role` (choices sorted(registered,uploader,inviter,editor), default registered), `--expires` (default `7d`; `1h`,`7d`,`30d`...) | `Invite code: {code}`, `Role: ..`, `Expires in: ..` |
| `list-invites` | `--all` includes used/expired | `No invites found`; else table `Code(24) Role(12) Expires(20) [Used By(15)]`, rule width 58 / 73 |
| `delete-invite CODE` | `ignore_unknown_options` (codes may start with `-`) | `Invite 'c' deleted` / not found exit 1 |
| `restore-user USERNAME` | `--role` (optional), `--password` (prompt) | `User 'x' restored` / `Error: 'x' is not a deleted account` exit 1 |
| `approve-user USERNAME` | | `User 'x' approved` / `User 'x' not found or not pending` exit 1 |
| `disable-user USERNAME` | | `User 'x' disabled` / not found exit 1 |
| `set-password USERNAME` | `--password` (prompt) | `Password updated for 'x'` / not found exit 1 |

### 4.6 `config` group (config_cli.py) - KEEP
`_load` wraps `load_config` errors `ValueError|YAMLError` as ClickException (exit 1, `Error: <msg>`) (config_cli.py:26-37).
- `config show` -> `yaml.safe_dump(config.to_dict(), stdout, default_flow_style=False)` of the effective config (env included, token NOT masked) (:40-46).
- `config set KEY VALUE` -> load (incl. env), `set_by_dotted_key`, `save_config`, echo `Set {key} = {value}` (:49-71). Errors `Error: {e}` on stderr, exit 1 (KeyError/ValueError). File is created if missing. Settable keys = the 36 in 2.1.
- `config path` -> `Config file: {path}` and `Storage dir: {base_path}` (via load incl. env), or `Storage dir: unknown -- the config file cannot be read ({e})` if load raises `OSError|ValueError|YAMLError` (:74-90).
- `config init [--force]` -> exists and no `--force`: stderr `Error: Config file already exists at {path}` + `Use --force to overwrite`, exit 1; else `save_config(Config())` (pure defaults, no env), echo `Created config file at {path}` (:93-108).
- `config cors-add ORIGIN` -> present: echo `Origin already allowed: {o}` (exit 0, no write); else append + save + `Added CORS origin: {o}` (:111-125). No validation of the origin string.
- `config cors-remove ORIGIN` -> absent: stderr `Error: Origin not found: {o}` exit 1; else remove + save + `Removed CORS origin: {o}` (:128-142).

### 4.7 `ssl` group (ssl_cli.py)
- `ssl enable [--auto-cert] [--cert PATH(must exist)] [--key PATH(must exist)]` (:20-65): neither auto nor both cert+key -> stderr `Error: Provide --auto-cert or both --cert and --key` exit 1; only one of cert/key -> `Error: Both --cert and --key are required` exit 1. Sets `enabled=True`. Auto: `auto_cert=True`, cert/key strings cleared, generates the default pair if `cert.pem` missing (echo `Generating self-signed certificate...`, `Certificate: {p}`, `Key: {p}`). Else `auto_cert=False`, `cert_file/key_file` = given paths as typed. Save; echo `SSL enabled`. (`--auto-cert` wins if combined with cert/key.)
- `ssl disable` (:68-78): `enabled=False`, `auto_cert=False` (cert paths kept), save, `SSL disabled`.
- `ssl status` (:81-130): `SSL: disabled`, or `SSL: enabled` + `Mode: auto-cert|custom certificate` + `Certificate: {path}`; missing file -> `Certificate file not found (will be generated on server start)` (also printed for custom mode - QUIRK, wrong for custom); else `Subject: {rfc4514}`, `Not before: {datetime}`, `Not after: {datetime}` (Python datetime str, e.g. `2026-01-01 00:00:00+00:00`), `SANs: a, b` (DNS names only). Parse error -> stderr `Could not read certificate: {e}`.
- `ssl generate [--hostname H=localhost] [--days N=365]` (:133-147): existing cert -> confirm `Certificate already exists at {p}. Overwrite?` (no -> return); echo `Generating self-signed certificate for '{host}'...`, `Certificate: ..`, `Key: ..`. Always the DEFAULT paths regardless of config.

### 4.8 `tunnel` group (tunnel_cli.py:15-108) - KEEP; shells out to external `cloudflared`
- `tunnel status`: `which cloudflared`; absent -> `cloudflared: not installed` + `Install from: https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/` (exit 0). Present -> `cloudflared: {path}`, runs `cloudflared version` (10 s timeout), prints `Version: {stdout or stderr}`; failure `Could not get version: {e}` on stderr.
- `tunnel cloudflare [--port N]`: absent binary -> stderr error + URL, exit 1. Port default = `server.port`; scheme `https` if `ssl.enabled` else `http`; target `{scheme}://localhost:{port}`. Echoes `Starting Cloudflare tunnel for {url}...` and `Press Ctrl+C to stop`; runs `cloudflared tunnel --url {url}`, echoing stderr line by line; first match of regex `(https://[a-z0-9-]+\.trycloudflare\.com)` -> prints `Tunnel URL: {u}` and prompts `Add tunnel URL to CORS allowed origins?` [y]; yes and not already present -> append to `cors.allowed_origins`, `save_config` (env-baking quirk applies), echo `Added {u} to CORS origins`. On Ctrl-C: `Stopping tunnel...`, terminate, wait 5 s. Other exception: `Error: {e}`, exit 1.
- Runtime `TunnelService` (tunnel/service.py:15-97; admin API `/api/tunnel/{status,start,stop}`): `start(port=None)` no-op if running; `RuntimeError("cloudflared is not installed")` if missing; same command and scheme/port derivation; stdout discarded, stderr read by a daemon thread to capture the URL (same regex, first match only, kept in memory, NOT added to CORS). `stop()` terminates, waits 5 s, then kills; `atexit` registered. `status` = `{running: bool, url: str|None (only while running), available: bool}`. Not started at boot.

### 4.9 `dyndns` group (dyndns_cli.py)
- `dyndns setup` (:24-64): prompts `Provider` (duckdns|generic) [duckdns], `Domain` [existing], `API Token` (hidden; ALWAYS re-asked), generic only `Update URL (use {ip}, {domain}, {token} as placeholders)` [existing], `Update interval (seconds)` [300 - not the current value; `<30` raises an uncaught ValueError], `Enable DynDNS?` [y]. Saves; echo `DynDNS configuration saved to {path}`; if enabled `DynDNS will start automatically when the server runs.`
- `dyndns status` (:70-83): lines `Enabled:   {True|False}`, `Provider:  ..`, `Domain:    {d or (not set)}`, `Token:     {**** or (not set)}`, `Interval:  {n}s`, and for generic `URL:       {u or (not set)}`.
- `dyndns update` (:87-105): no token or no domain -> stderr `Error: DynDNS not configured. Run 'mokuro-bunko dyndns setup' first.` (exit 0, QUIRK); else `Updating DNS for {domain}...`; success `Success! IP: {ip}`; failure stderr `Failed: {error}` (exit 0).
- `dyndns enable|disable` (:110-129): set flag, save, echo `DynDNS enabled. Restart the server for changes to take effect.` / `DynDNS disabled. Restart ...`.

### 4.10 `processor` group (processor/cli.py) - listing only; another agent covers internals
`processor serve --config PATH(required, env MOKURO_PROCESSOR_CONFIG) [-v]`; `processor install --config PATH [--force] [--engines CSV] [--detector]` (**DROP/REPLACE**: installs Python OCR envs); `processor service --config PATH [--install]` (writes a systemd user unit `mokuro-bunko-processor.service` under `~/.config/systemd/user`, or on Windows a Startup `.cmd`); `processor setup [--config processor.yaml] [--url] [--username] [--password-stdin] [--name] [--backend] [--tls-verify true|false|PATH] [-y] [--no-install] [--no-service] [--force]` (wizard writing `processor.yaml`); `processor status --config PATH`. `processor.yaml` is a separate file (docs/processor.example.yaml; loader processor/config.py:97-180).

---------------------------------------------------------------------------

## 5. Startup environment validation (server.py:50-102) - KEEP
`_validate_startup_environment(config)` raises `ValueError` (printed `Startup validation failed: ...`, exit 2):
1. `ensure_directories()`; for each of `base_path, library, inbox, users` the path must exist (`Required directory does not exist ({label}): {path}`), be a directory (`Required path is not a directory ...`), and be writable by create+delete of `.mokuro-write-test` containing `ok` (`Directory is not writable ({label}): {path}`). Labels: `storage.base_path`, `storage.library_path`, `storage.inbox_path`, `storage.users_path`.
2. If `ssl.enabled`:
   - `auto_cert`: create the parents of the default cert/key paths and require both writable (labels `ssl auto-cert directory`, `ssl auto-key directory`).
   - else: `cert=Path(cert_file).expanduser()`, `key=Path(key_file).expanduser()`; each must be a file (`SSL certificate file not found: {p}` / `SSL private key file not found: {p}`); then `validate_certificate_pair`, the FIRST error raised.
   QUIRK: later `create_ssl_server` uses the UNexpanded `config.ssl.cert_file` strings (server.py:537-540), so `~/x.pem` passes validation and then fails at bind.

## 6. SSL (ssl.py)
### 6.1 `generate_self_signed_cert(cert_path, key_path, hostname="localhost", validity_days=365)` (ssl.py:35-124)
RSA 2048 (e=65537); subject = issuer = `CN=<hostname>, O=mokuro-bunko`; random serial; not_before = now UTC (no backdating); not_after = now + days; SHA-256 signature; extensions: SubjectAltName (non-critical) = DNS `localhost`, DNS `<hostname>`, IP `127.0.0.1`, plus DNS `<socket.gethostname()>` when different from `hostname` (best-effort; when hostname is `localhost` the first two are duplicates); BasicConstraints CA=false (critical). No KeyUsage/EKU. Parent dirs created; cert PEM, then key as TraditionalOpenSSL PEM (`-----BEGIN RSA PRIVATE KEY-----`), unencrypted, default file mode (NO chmod 600). Rust may emit PKCS#8 and tighten perms but must still READ PKCS#1 keys from existing installs.
### 6.2 `ensure_ssl_context` (ssl.py:140-172): not used by the server path (dead code) but documents the semantic: disabled -> None; auto -> generate if cert OR key missing; custom -> `FileNotFoundError("Certificate file not found: p")` / `"Key file not found: p"`.
### 6.3 Server bind (server.py:529-540): auto: generate (hostname `localhost`) when cert or key missing; cheroot `BuiltinSSLAdapter(cert, key)` (Python `ssl` defaults). Rust: rustls with the same PEM inputs (accept PKCS#1, PKCS#8, SEC1).
### 6.4 `get_ssl_info` (ssl.py:175-191) strings logged at startup: `SSL disabled`, `SSL enabled (auto-cert: {path})`, `SSL enabled (cert: {cert_file})`.
### 6.5 `validate_certificate_pair(cert, key, expiry_warning_days=30) -> (errors, warnings)` (ssl.py:194-246)
In order, returning early on the first error: (a) cert+key must load together (catches mismatch/bad files) -> `SSL certificate/key validation failed: {exc}`; (b) parse PEM cert -> `Failed to parse certificate file: {exc}`; (c) `not_after <= now` -> `SSL certificate has expired: {iso}`; (d) `not_before > now` -> `SSL certificate is not valid yet: {iso}`; (e) remaining `<= 30 days` -> WARNING `SSL certificate expires soon ({iso}, {N} days remaining)`. `run_server` uses only `errors[0]` and drops warnings. Tests: tests/unit/test_ssl_validation.py, test_ssl.py.

---------------------------------------------------------------------------

## 7. DynDNS service (dyndns/service.py) - KEEP
- Started only when `dyndns.enabled` at app creation (server.py:207-208); also controllable via admin API (`/api/dyndns/{status,start,stop,test}`); `configure(new_cfg)` stops (if running), swaps, restarts if it was running and the new cfg is enabled (service.py:58-65).
- Loop (`_run`, :67-72): performs an update immediately on start, then waits `interval` seconds (interruptible by the stop event), repeats. No jitter, no backoff, no "IP changed" short-circuit (updates every interval). `stop()` joins the thread with a 5 s timeout. Daemon thread.
- One update (`_do_update`, :74-90): (1) public IP via GET `https://api.ipify.org` (10 s timeout, body stripped); (2a) `duckdns`: a trailing `.duckdns.org` is stripped from the domain; GET `https://www.duckdns.org/update?domains={domain}&token={token}&ip={ip}` (10 s; values NOT URL-encoded); body must equal `OK` else `RuntimeError("DuckDNS update failed: {body}")`; (2b) `generic`: empty `update_url` -> `RuntimeError("No update_url configured for generic provider")`; replaces literal `{ip}`, `{domain}`, `{token}` in the URL; GET (10 s); any successful HTTP response counts as success and its body is returned (urllib raises on HTTP errors).
- Result dict: success `{"success": true, "ip": ip, "response": body}`; failure `{"success": false, "error": str}`. State: `last_update` (UTC `%Y-%m-%dT%H:%M:%SZ`), `last_ip` (set even if the provider call then fails), `last_error` (cleared on success).
- `status()` dict: `{enabled, running, provider, domain, last_update, last_ip, last_error}`.
- Min interval 30 s is enforced by config validation only.

---------------------------------------------------------------------------

## 8. Logging (logging_setup.py) - KEEP
- Configured once from `run_server` after storage validation (server.py:609-611); other CLI commands only print via click.
- Root logger level DEBUG. Handlers (tagged so reconfiguration replaces them, :50-54): console `StreamHandler(stdout)` level INFO (DEBUG with `-v`), format `%(levelname)s [%(name)s] %(message)s`; file handler `RotatingFileHandler(<storage>/logs/server.log, maxBytes=2*1024*1024, backupCount=5, encoding="utf-8", delay=True)` (files `server.log`, `server.log.1`..`.5`), level INFO, format `%(asctime)s %(levelname)s [%(name)s] %(message)s` (asctime default `YYYY-MM-DD HH:MM:SS,mmm`). File creation failure (`OSError`) -> console warning `Could not create log file under {storage}: {e} (console logging only)` (:78-83).
- Third-party loggers `wsgidav`, `cheroot`, `urllib3` pinned to WARNING (Python-only).
- Per-volume OCR logs: `<storage>/logs/ocr/<series>_<volume>[.<generation>].log` (OCR spec owns writing; docs/configuration.md:939-948). Logger names: `mokuro_bunko.server`, `mokuro_bunko.ocr`. Request logging when `MOKURO_DEBUG` (middleware/request_log.py). Plain `print()` still used for: `Generating self-signed certificate at ...` (ssl.py:157), `Press Ctrl+C to stop`, `Shutting down...`, `Startup validation failed: ...`, `[WATCHDOG] ...` (stderr).

---------------------------------------------------------------------------

## 9. Web setup wizard (`setup/`, first run) - KEEP
Class `SetupWizardAPI` (setup/api.py:33-257), a WSGI wrapper at layer 13 of the chain (server.py:403): inside CORS/security headers/static, outside registration/account/login. "Setup needed" = no user with role `admin` exists (`db.list_users()`); cached True forever once an admin is seen, re-queried per request while none (setup/api.py:49-59).

Routes (setup/api.py:61-117):
| Method + path | Behaviour |
|---|---|
| `GET /setup/api/status` | needed AND non-local -> `403 {"error":"Setup is only allowed from localhost"}`; else `200 {"needs_setup": bool}` (non-local callers get an answer only after an admin exists) |
| `POST /setup/api/complete` | non-local -> same 403 (even after setup); not needed -> `400 {"error":"Setup already completed"}`; else the handler below |
| `GET /setup`, `GET /setup/` | needed && non-local -> 403 JSON; else serves `web/index.html` |
| `GET /setup/<file>` (not starting `api/`) | same gate; serves `setup/web/<file>` (assets: index.html, setup.css, setup.js; Rust: embed these) |
| `GET /` with `Accept` containing `text/html` and setup needed | `302 Location: /setup` (empty body) |
| anything else | passed to the wrapped app |
"Local" (`_is_local_request`, setup/api.py:172-183): `REMOTE_ADDR` must be a loopback IP; and the effective client IP (`get_client_ip`, 3.1), if different from `REMOTE_ADDR`, must also be loopback. A localhost reverse proxy forwarding a public client's `X-Real-IP` is therefore rejected; Docker bridge networking (non-loopback peer) makes setup unreachable from the host browser unless using localhost inside the container.

`POST /setup/api/complete` (setup/api.py:119-170): Content-Length 0/missing -> `400 {"error":"Empty body"}`; > 65536 -> `413 {"error":"Request body too large"}`; invalid JSON/UTF-8 -> `400 {"error":"Invalid JSON"}`. Body: `{"admin":{"username","password"},"registration":{"mode"}}`. `username` is stripped; `validate_username` (validation.py:12-24): `Username is required` / `Username must be 3-32 characters and contain only letters, numbers, underscores, and hyphens` (regex `^[a-zA-Z0-9_-]{3,32}$`; Python `$` lets a trailing `\n` slip through - Rust should full-match); `validate_password` (validation.py:27-38): `Password is required` / `Password must be at least 8 characters` / `Password must be at most 128 characters`. Each -> `400 {"error": msg}`. `db.create_user(username, password, "admin")` ValueError -> `409 {"error": str(e)}`. Then if `registration.mode` in `disabled|self|invite|approval` it is set on the live config (an invalid mode is silently ignored); `save_config(config, config_path)` (env-baking quirk applies); success `201 {"success": true, "message": "Setup completed successfully"}`. JSON responses: `Content-Type: application/json`, `Content-Length`; static: `Cache-Control: no-cache`, MIME map setup/api.py:20-28, traversal guarded (`..`, leading `/`, resolved path outside web dir -> 403/404).
The page then POSTs `/login/api/token` with `{username,password,kind:"web",label:"web page"}` (login API, other agent) to sign the new admin in (setup/web/setup.js:97-120). Tests: tests/unit/test_setup_api.py.

---------------------------------------------------------------------------

## 10. `doctor` checks (doctor_cli.py) - port the generic ones, DROP the Python ones
Output: header `mokuro-bunko {version} - environment diagnostics` + blank line; then one row per result ` {STATUS:<4}  {label}: {detail}` (PASS green / WARN yellow / FAIL red, bold) and, when status != PASS and a hint exists, `        -> {hint}`; blank line; summary. Exit 1 if any FAIL (`N problem(s) found - see FAIL lines above.`); else with warnings `OK with N warning(s) - see WARN lines above.` (exit 0); else `All checks passed.` Order:
1. **Python** (`_check_python`, :42-52) **DROP**: WARN when interpreter >= 3.13 (no CUDA wheels), else PASS.
2. **Config** (`_check_config`, :55-94): load config (any exception -> `FAIL Config: {path}: {err}`, hint `Fix or delete the config file, then re-run 'mokuro-bunko setup'.`; the storage-dependent checks 5-7 are then skipped). Then `ensure_directories()` + write/unlink probe `.mokuro-doctor-probe` (`ok`); `OSError` -> `FAIL Storage: {base} is not writable: {e}`, hint `Point storage.base_path at a writable directory.` (config still returned so checks 5-7 run). Success: `PASS Config: {path}{ (not found; using defaults)} - storage: {base}`.
3. **NVIDIA driver** (`_check_nvidia`, :97-115): `nvidia-smi --query-gpu=name,driver_version --format=csv,noheader` (10 s); first output line -> PASS; missing/failed/timeout -> WARN `nvidia-smi not found - GPU OCR unavailable, CPU backend will be used`, hint `Install the NVIDIA driver if this machine has an NVIDIA GPU.` (keep only if Rust OCR uses CUDA; else replace with an execution-provider probe).
4. **OCR environment** (`_check_ocr_env`, :118-146) **DROP/REPLACE**: no mokuro venv -> WARN `OCR environment: not installed (expected at {env_path})`, hint `Run: mokuro-bunko install-ocr   (or start the server once; it installs on launch)`; else PASS `OCR environment: {path}` and `OCR stack` PASS/FAIL from `verify_installation()` (hint `Run: mokuro-bunko install-ocr --force`).
5. **Disk space** (`_check_disk`, :149-163): `disk_usage(storage)`; `OSError` -> WARN `could not check: {e}`; free `< 10 GiB` -> WARN `{x:.1f} GB free at {path}` hint `A CUDA OCR environment plus models needs ~8-10 GB.`; else PASS (threshold/hint tied to the Python env size; retune for Rust).
6. **Port** (`_check_port`, :166-179): bind probe `AF_INET`/`SOCK_STREAM` on `127.0.0.1` (when host is `0.0.0.0`) else `host`; failure -> WARN `{port} is in use on {host} - is the server already running?`, hint `Stop the other process or change server.port in the config.`; else PASS `{port} available on {host}`.
7. **Failed volumes** (`_check_failures`, :182-198): reads `<storage>/.ocr-failures.json`; unreadable / invalid / non-dict / empty -> PASS `none recorded`; else WARN `{n} volume(s) failing OCR (see the Queue page)`, hint `Full per-volume logs: {storage}/logs/ocr`.
Tests: tests/unit/test_doctor_cli.py.

---------------------------------------------------------------------------

## 11. Packaging and deployment currently in the repo (what a Rust release must replace)

### 11.1 Python package (pyproject.toml)
`mokuro-bunko` 0.5.2, hatchling wheel, Python >=3.11, license MPL-2.0 (Dockerfile labels say MIT: inconsistent, Q9). Runtime deps: wsgidav, cheroot, pyyaml, bcrypt, watchdog, Pillow, click, cryptography. Console script `mokuro-bunko`. Dev: pytest, httpx, playwright, ruff, mypy, shiv. -> **REPLACE** with a single static `mokuro-bunko` binary per target (linux x86_64/aarch64, macOS arm64/x64, windows x64) published as release assets.

### 11.2 Docker (deploy/)
- `deploy/Dockerfile` (97 lines): `python:3.12-slim` two-stage (wheel -> venv); runtime adds `nginx gettext-base git`; non-root user `mokuro`; copies `deploy/nginx-internal.conf.template` to `/etc/nginx/` and `deploy/docker-entrypoint.sh`; ENV `MOKURO_HOST=0.0.0.0 MOKURO_PORT=8080 MOKURO_BACKEND_PORT=8081 MOKURO_STORAGE=/data MOKURO_CONFIG=/data/config.yaml MOKURO_BUNKO_OCR_ENV=/data/.ocr-env MOKURO_BUNKO_OCR_ENGINES_ENV=/data/.ocr-engines-env MOKURO_REGISTRATION_MODE=disabled MOKURO_SSL_ENABLED=false MOKURO_NGINX_ACCEL=1`; `EXPOSE 8080`; healthcheck `python -c "import httpx; httpx.get('http://localhost:${MOKURO_PORT}/')"` (30s/10s/5s/3); `ENTRYPOINT docker-entrypoint.sh`, `CMD ["serve"]`. `git` exists only for the OCR installer -> DROP. A Rust healthcheck needs a binary subcommand or `curl`/`wget` (no Python/httpx).
- `deploy/docker-entrypoint.sh` (29 lines): exports `MOKURO_LIBRARY=$MOKURO_STORAGE/library`; mkdirs nginx temp dirs; `envsubst '${MOKURO_PORT} ${MOKURO_BACKEND_PORT} ${MOKURO_LIBRARY}'` over the template into `/tmp/nginx.conf`; `nginx -c /tmp/nginx.conf &`; then `MOKURO_HOST=127.0.0.1 MOKURO_PORT=$MOKURO_BACKEND_PORT MOKURO_NGINX_ACCEL=1 exec mokuro-bunko "$@"`.
- `deploy/Dockerfile.unraid` (89 lines): `nvidia/cuda:12.8.1-runtime-ubuntu24.04` two-stage; runtime adds `python3 python3-venv python3-pip ca-certificates git tini gosu nginx gettext-base p7zip-full libxcb1 libgl1 libglib2.0-0`; ENV `PYTHONUNBUFFERED=1 MOKURO_HOST=0.0.0.0 MOKURO_PORT=8080 MOKURO_BACKEND_PORT=8081 MOKURO_STORAGE=/data MOKURO_CONFIG=/config/config.yaml MOKURO_OCR_BACKEND=auto MOKURO_BUNKO_OCR_ENV=/opt/ocr-env MOKURO_BUNKO_OCR_ENGINES_ENV=/opt/ocr-engines-env NVIDIA_VISIBLE_DEVICES=all NVIDIA_DRIVER_CAPABILITIES=compute,utility`; `VOLUME ["/data","/config"]`; healthcheck urllib GET `http://127.0.0.1:8080/` (30s/10s/20s/3); `ENTRYPOINT tini -- docker-entrypoint.sh`, `CMD ["serve"]`. `MOKURO_OCR_BACKEND` is the canonical env var for `ocr.backend`. `p7zip-full`, `libxcb1/libgl1/libglib2.0-0` support Python OCR/archives (OCR/archives spec decides what survives); `gosu`, `tini`, `nginx`, `gettext-base` are deploy-level.
- `deploy/docker-entrypoint.unraid.sh` (122 lines): PUID/PGID (default 99/100), UMASK (002), TAKE_OWNERSHIP, OCR_AUTO_INSTALL; defaults then exports `MOKURO_HOST/PORT/STORAGE/OCR_BACKEND/CONFIG/BUNKO_OCR_ENV/BUNKO_OCR_ENGINES_ENV`; redirects `TMPDIR`/`PIP_CACHE_DIR` into the data volume; creates group/user with `groupadd -o`/`useradd -o`; optional recursive chown (`TAKE_OWNERSHIP=true`); best-effort `chown` of storage, `/config`, env dirs; `umask`; optional `gosu ... mokuro-bunko install-ocr --backend $MOKURO_OCR_BACKEND || true` (`OCR_AUTO_INSTALL=true` and backend != skip); nginx X-Accel mode when `MOKURO_NGINX_ACCEL` is `1|true`: mkdir `/tmp/nginx-{client-body,proxy,fastcgi,uwsgi,scgi}` + library dir, render template, start nginx master as root with workers `user ${nginx_user} ${nginx_group}` (`nginx -c /tmp/nginx.conf -g "user ...;"`), then rebind python to `127.0.0.1:$MOKURO_BACKEND_PORT` via `MOKURO_HOST`/`MOKURO_PORT`; finally `exec gosu PUID:PGID mokuro-bunko "$@"` (default arg `serve`). Keep PUID/PGID/UMASK semantics; drop pip cache/TMPDIR/OCR_AUTO_INSTALL/git. If nginx is dropped, `MOKURO_NGINX_ACCEL=1` must still be tolerated (Q10).
- Compose files: `docker-compose.yml` (generic build, volume `mokuro-data:/data`, env `MOKURO_HOST/PORT/STORAGE/CONFIG=/data/config.yaml/REGISTRATION_MODE=self`, python+httpx healthcheck); `docker-compose.unraid-cuda.yml` (build Dockerfile.unraid, `/mnt/user/appdata/mokuro-bunko/{data,config}`, `gpus: all`, env incl. `MOKURO_BUNKO_OCR_ENV=/data/.ocr-env`, `OCR_AUTO_INSTALL=false`, commented `MOKURO_OCR_GENERATIONS` and `MOKURO_OCR_LOCAL_PROCESSING=false` examples); `docker-compose.unraid-lan.yml` (port 8081->8080, `/mnt/user/mokuro-bunko-library:/data`, `MOKURO_OCR_BACKEND=cuda`, `OCR_AUTO_INSTALL=true`, nvidia device reservation); `docker-compose.cloudflared.yml` (python service on an internal network without published ports, `cloudflare/cloudflared:latest` with `TUNNEL_TOKEN=${CLOUDFLARE_TUNNEL_TOKEN}`, `MOKURO_REGISTRATION_MODE=invite`).
- Unraid template `deploy/unraid/mokuro-bunko.xml`: repository `mokuro-bunko:unraid-cuda` (locally built; none published), `--runtime=nvidia`, WebUI port 8080, paths `/data` and `/config`, variables PUID PGID UMASK TAKE_OWNERSHIP MOKURO_CONFIG MOKURO_HOST MOKURO_PORT MOKURO_STORAGE MOKURO_OCR_BACKEND OCR_AUTO_INSTALL MOKURO_BUNKO_OCR_ENV MOKURO_OCR_GENERATIONS MOKURO_OCR_LOCAL_PROCESSING MOKURO_BUNKO_OCR_ENGINES_ENV NVIDIA_*. Keep every unchanged variable name so existing user templates/compose files work; `*_OCR_ENV*` may be accepted and ignored.
- Deployments can run the image with `MOKURO_NGINX_ACCEL=1` (nginx X-Accel throughput). The Rust server must keep an equivalent or better download path and keep anonymous `/mokuro-reader/` URLs unchanged.

### 11.3 nginx / Caddy / systemd
- `deploy/nginx-internal.conf.template` (203 lines, envsubst `${MOKURO_PORT} ${MOKURO_BACKEND_PORT} ${MOKURO_LIBRARY}`): `worker_processes auto`; stderr error log; pid `/tmp/nginx.pid`; sendfile/tcp_nopush/tcp_nodelay; `server_tokens off`; `$forwarded_proto` map preserving the edge `X-Forwarded-Proto` (wsgidav's MOVE/COPY Destination scheme check); gzip for `application/json` (min 1024, level 5, `gzip_proxied any`, `gzip_vary`); temp paths under `/tmp/nginx-*`; `set_real_ip_from 10.0.0.0/8,172.16.0.0/12,192.168.0.0/16`, `real_ip_header X-Forwarded-For`, recursive; `$cors_origin` map for nginx-generated error pages only; `client_max_body_size 2048M`; upstream `127.0.0.1:${MOKURO_BACKEND_PORT}` keepalive 16; `location /` proxy with `Host`, `X-Real-IP $remote_addr`, `X-Forwarded-For $proxy_add_x_forwarded_for`, `X-Forwarded-Proto $forwarded_proto`, timeouts 10s/300s/300s, `proxy_request_buffering off`, `proxy_buffering off`; `location /_processor/` (no body cap, unbuffered, same timeouts) for processor chunked streams; `location /internal-library/ { internal; alias ${MOKURO_LIBRARY}/; add_header Access-Control-Allow-Origin $cors_origin always; ...Allow-Credentials true; Vary Origin }`; `@cors_error` returns 503 `mokuro-bunko: backend temporarily unavailable`. Tested by tests/unit/test_nginx_config.py. If Rust serves files itself none of this is required; the X-Accel header, `trusted_proxies` interplay and per-error CORS vanish.
- `deploy/nginx.conf.example` (104 lines): public TLS reverse-proxy example (certbot, 80->443 redirect, 500M body, WebDAV methods, upstream 127.0.0.1:8080). `deploy/caddy.example` (62 lines): `YOUR_DOMAIN` site; `@processor path /_processor/*` handle with `flush_interval -1` and no body cap; main handle `request_body max_size 500MB`, health check `/` every 30s; forwards `Host`, `X-Real-IP {remote_host}`, `X-Forwarded-For {remote_host}`, `X-Forwarded-Proto {scheme}`; log roll 10mb/5 files/720h. KEEP as docs; the Rust server must keep the `X-Real-IP` + `server.trusted_proxies` semantics (3.1).
- `deploy/mokuro-bunko.service` (39 lines): User/Group `mokuro`, `WorkingDirectory=/var/lib/mokuro-bunko`, Env `MOKURO_HOST=127.0.0.1 MOKURO_PORT=8080 MOKURO_STORAGE=/var/lib/mokuro-bunko/storage`, `ExecStart=/usr/local/bin/mokuro-bunko serve`, `Restart=on-failure RestartSec=5s`, hardening (NoNewPrivileges, ProtectSystem=strict, ProtectHome, ReadWritePaths=/var/lib/mokuro-bunko, PrivateTmp, ProtectKernelTunables/Modules, ProtectControlGroups), `LimitNOFILE=65536`, `MemoryMax=2G`. KEEP (binary path only). Note: with `ProtectHome=yes` and no `MOKURO_CONFIG`, the default config path is unreadable, so defaults apply (existing behaviour).
- `deploy/mokuro-bunko-processor.service` (34 lines): `ExecStart=/usr/local/bin/mokuro-bunko processor serve --config /etc/mokuro-bunko/processor.yaml`, `Restart=on-failure`, `RestartSec=10s`, `TimeoutStopSec=60s` (SIGTERM = clean stop), `ProtectSystem=full`, `PrivateTmp`. Processor scope.

### 11.4 scripts/
- `scripts/build-binary.sh` (136 lines): builds a wheel then a `shiv` zipapp `dist/mokuro-bunko-<platform>[.exe]` (`--entry-point mokuro_bunko.__main__:main`; deps wsgidav cheroot pyyaml bcrypt watchdog cryptography click Pillow); `--platform linux-x64|linux-arm64|macos-x64|macos-arm64|windows-x64` (default detected). Requires a system Python at runtime. **REPLACE** by a `cargo build --release --target` matrix.
- `scripts/build-portable.ps1` (93 lines; `-OutDir dist`, `-UvVersion 0.11.28`): stages `dist/mokuro-bunko-portable-windows-x64.zip` = `app/` (robocopy of the source tree excluding .git/.venv/.ocr-env/dist/build/tests/...) + `bin/uv.exe` (downloaded from the astral release) + `LICENSE-uv.txt` + `run.bat doctor.bat _env.cmd README.txt` from `scripts/portable/`. **REPLACE**: a zip with `mokuro-bunko.exe` and launchers.
  - `scripts/portable/_env.cmd`: sets `MB_ROOT`; `UV_PYTHON_INSTALL_DIR`, `UV_CACHE_DIR`, `UV_PROJECT_ENVIRONMENT`, `PIP_CACHE_DIR`, `HF_HOME` under `runtime\` (DROP); KEEP `MOKURO_CONFIG=%MB_ROOT%data\config.yaml`, `MOKURO_STORAGE=%MB_ROOT%data`, mkdir `data`; `MOKURO_BUNKO_OCR_ENV=%MB_ROOT%runtime\ocr-env` DROP/REPLACE. Nothing is written to AppData/registry (portable guarantee to keep).
  - `run.bat`: banner, background PowerShell loop polling `http://127.0.0.1:8080` for up to 900 s then opening the browser, `uv run --directory app mokuro-bunko serve`, `pause`. `doctor.bat`: `uv run ... mokuro-bunko doctor`. `README.txt`: layout (`data\library`, `data\config.yaml`, `data\logs\server.log`, `data\logs\ocr\<volume>.log`), "first run downloads ~2 GB" text (DROP/rewrite), queue page `/queue`, licence note (MPL-2.0; uv MIT/Apache).
- `scripts/setup-windows.ps1` (265 lines; `-InstallDir %LOCALAPPDATA%\Programs\mokuro-bunko`, `-Ref main`, `-Backend auto|cuda|rocm|cpu`, `-SkipOcr`, `-NoShortcut`, `-NoStart`, `-NonInteractive` (implies NoStart+NoShortcut)): checks Git (needed for the OCR git install, unless `MOKURO_BUNKO_MOKURO_SPEC` is a non-git spec), downloads the source zip from codeload.github.com (branch, then tag), installs `uv`, `uv sync`, `uv run mokuro-bunko install-ocr --backend`, `uv run mokuro-bunko doctor` (aborts on non-zero), writes `start-mokuro-bunko.cmd` + Desktop shortcut, starts the server and polls `http://127.0.0.1:8080` for 60 s then opens the browser, transcript at `%TEMP%\mokuro-bunko-setup.log`. **REPLACE** with a download-the-release-binary script (same flags minus `-Ref/-Backend/-SkipOcr`), then `doctor`, shortcut, start. docs/setup-windows-nvidia-ocr.md describes the Python/CUDA flow (rewrite).

---------------------------------------------------------------------------

## 12. DROP / REPLACE summary
- DROP (Python env management): `install-ocr` as defined, `processor install`, doctor's Python and OCR-env checks, `MOKURO_BUNKO_OCR_ENV`, `MOKURO_BUNKO_OCR_ENGINES_ENV`, `MOKURO_BUNKO_MOKURO_SPEC`, `PIP_CACHE_DIR`/`TMPDIR` redirection, `HF_HOME` carry-over, `OCR_AUTO_INSTALL`, Git prerequisite, `uv.exe` bundling, shiv zipapp, `ThreadPoolWatchdog` and `_start_server_resilient` (cheroot), `tolerant_console_streams`, wsgidav/cheroot log-level pinning, `MOKURO_THREADS` thread semantics (map to async limits), `python+httpx` healthchecks.
- DROP (engines): `mokuro` engine row (manga-ocr via the mokuro fork), `ctd` detector (GPL), `animetext` detector (already refused), `MOKURO_PPOCR_*` Python-adapter vars if the Rust OCR is native.
- REPLACE: release packaging (cargo matrix), portable zip, Windows setup script, container healthcheck, optional model-fetch command, doctor OCR-runtime check, generation-row semantics for rows the Rust build cannot run (Q6).
- KEEP verbatim: config file format and precedence (1-3), env var names (2), CLI surface (4) except `install-ocr`, setup wizard JSON (9), SSL generation/validation (6), DynDNS URLs and timings (7), tunnel command and regex (4.8), log layout and rotation (8), startup validation and exit code 2 (5), systemd unit, nginx/caddy examples, container env-variable contract (11.2), PUID/PGID/UMASK semantics.

---------------------------------------------------------------------------

## 13. Open questions

1. Unknown keys inside a section raise `TypeError` today (config.py:507-515); unknown top-level keys are ignored; `null` sections crash. Should Rust refuse unknown section keys (matching intent, with a clean message), ignore with a warning, or preserve them on save? A drop-in must at least accept every key in 3.x, including `registration.require_login`. Refusing is safe for any config Python already loads.
2. Env overrides are baked into the file on every save (1.5 QUIRK). Preserve, or save only file-derived values (needs a separate file model vs effective model)? Fixing changes observable file contents but is almost certainly desired (e.g. nginx-accel backend `MOKURO_PORT=8081` lands in `server.port`).
3. Required fidelity of the saved YAML: semantic equivalence only, or also alphabetical key order/format so existing diffs stay stable? Comments are lost unless a comment-preserving writer is used.
4. Strict validation of env-supplied values (enum/range): Python lets bad values through (2.3). Rust should probably reject; confirm no deployment relies on one.
5. YAML dialect: PyYAML treats `yes/no/on/off` as booleans (YAML 1.1); `serde_yaml` does not. Hand-written configs may use them. Also dataclasses do not coerce types (`port: "8080"` crashes). Which behaviour for Rust?
6. Rust behaviour for `mokuro` / `ctd` generation rows: the default config contains the `mokuro` primary row and its sidecar name (`<Volume>.mokuro`) is what readers consume. Does the Rust OCR take over the primary slot, keep a `mokuro` row as an alias for the Rust engine, or require config migration? Meaning of `ocr.backend` cuda/rocm/cpu for a non-torch runtime?
7. `serve --host/--port/--ocr` default-value detection (4.1): preserve, or switch to "explicitly given" semantics (a safe improvement)?
8. Fate of `install-ocr`, `processor install` and the install step of `processor setup` (no-op alias vs removal vs model-download command); the Unraid entrypoint and Windows script call `install-ocr --backend`.
9. Licence inconsistency: pyproject MPL-2.0 vs Dockerfile labels MIT vs the user's licence stance (Apache/MIT for on-device parts). Choose for the Rust crate and fix image labels.
10. Is nginx (X-Accel) kept in the Rust container image? If the Rust server serves downloads efficiently, nginx and `MOKURO_NGINX_ACCEL` go, but the variable must remain accepted (existing deployments set it), and the `/_processor/` unbuffered-stream requirement for front proxies (Caddy/nginx examples) stays documented.
11. Auto-cert location stays outside `storage.base_path` (1.1, 6): keep for compat (existing certs live there; containers lose them unless `XDG_DATA_HOME` is mounted)?
12. CLI `setup` and `admin add-user` prompts skip `validate_username`/`validate_password` (only the web wizard validates, DB layer may): align in Rust?
13. Hot reload: none exists. Is a SIGHUP/`reload` desirable (not needed for drop-in)?
14. Exit codes/messages that are probably bugs: `dyndns update` exits 0 on failure and when unconfigured; `ssl status` prints "will be generated on server start" for a missing custom cert; `dyndns setup` interval default 300 and uncaught ValueError. Preserve exact exit codes for scripts, or correct?
15. `database.*` knobs have no env/CLI path; keep file-only? Should the admin CLI honour them (it does not today, admin/cli.py:30)?
16. `MOKURO_THREADS` (default 50; processors hold a thread per stream): does the Rust async server need an equivalent concurrency limiter for processor streams, or accept-and-ignore?
17. Container healthcheck without Python: add a `mokuro-bunko healthcheck` subcommand (new surface) or use `curl`/`wget` in the image?
