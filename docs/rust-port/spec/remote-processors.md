# Remote OCR processors: behavioural spec (mokuro-bunko 0.5.2 -> Rust)

Source tree: `src/mokuro_bunko/`. Every `file:line` below is relative to that directory unless it starts with `docs/` or `tests/`.
Abbreviations: **lib** = the library server, **proc** = a processor (a separate process/machine), **claim** = one volume handed to a processor, **sid** = session id, **bid** = benchmark id.

Tags: **KEEP** (port as is), **CHANGE** (port the semantics, wire form may be cleaned up), **DROP** (do not port).

DROP, globally (not specified further except where it touches the wire):
- Everything about the `mokuro` engine / mokuro environment / `serves_mokuro` / `uses_mokuro_env` (`ocr/remote/scheduler.py:68-74`, `processor/cli.py:101-166`, `processor/cli.py:363-398`).
- The `ctd`, `animetext`, `rtdetr`, `ppocr-manga`, `paddle`, `hayai` detector/engine identities as such. The *concept* "catalog lists installed engines and detectors, library only offers rows the catalog can run" stays (see 5.2 and 7.2); the identifiers belong to the Rust OCR spec.
- Python venv / torch installation: `processor install`, `processor setup`'s install step, `--backend`, `ocr.backend`, `EnginesInstaller`/`OCRInstaller`, `detect_hardware` (`processor/cli.py:335-398`, `processor/setup.py:352-374`, `processor/config.py:20,73-76`), `CARRIED_ENV` (`processor/service.py:26`), `MOKURO_PROCESSOR_ENGINES_PYTHON`, `MOKURO_PROCESSOR_RUNNER` test override.
- `ocr.staging.pin_runner` / runner build staging / `runner_build` / runner drift warning (`processor/bridge.py:165-169, 311-327`). The Rust processor is one binary; keep only a `version` string in `host` for provenance (see 5.2).
- The subprocess-pipe runner (`OcrSession`, `engine_runner.py --serve`, `SessionVolume.workspace/cache_dir/detect_dir`, `/proc/<pid>/fd/<n>` hand-off). In Rust the OCR pipeline is in-process or a library call; the wire only needs the *events* it produces (6.2).
- cheroot-specific workarounds (`ocr/remote/protocol.py:1-17`): the frame format was length-prefixed only because of a cheroot `ChunkedRFile.readline` bug. A Rust server need not copy that reasoning, but see 3.2 (the framing is still a fine choice).
- `X-Accel-Redirect` / `MOKURO_NGINX_ACCEL` mismatch detection in the processor (`processor/archives.py:1214-1220`) is DROP unless the Rust server keeps nginx offload (open question Q7).

Everything else in `ocr/remote/*.py` and `processor/*.py` is in scope and KEEP/CHANGE.

---

## 1. Architecture in one paragraph

A processor is an ordinary authenticated *client* of the library (role `processor`). It dials OUT; the library opens no connection to it. After a one-shot `POST /_processor/register` it holds **one long-lived chunked GET** (the *assignment stream*, library->proc NDJSON ops + heartbeats), and for every OCR session the library asks it to open it holds **one long-lived chunked POST** (the *events body*, proc->library length-framed events, including finished sidecar bytes). Archives are pulled with ordinary authenticated `GET`s of the library's normal file URLs. The library never writes to a processor except through the assignment stream; the processor never writes library files except by sending the sidecar bytes in the events body, which the *library* validates and installs. The registry is in memory only; a processor exists exactly while its registration entry lives, and a disconnect returns all its claims to the queue unrecorded.

Connection budget per processor: 1 stream + 1 per open session + 1 while benchmarking (`docs/ocr-internals.md:624-628`; `MOKURO_THREADS` default 50 in the Python server). A Rust async server has no thread problem, but long-lived bodies must not be subject to normal request timeouts/body limits.

---

## 2. Authentication and authorization (via processor role)

### 2.1 Accounts and role
- Role `processor` is a normal user row with `role == "processor"`; created only by an admin (`admin add-user --role processor`), never via invite (`database.py:40-48`, `INVITABLE_ROLES`).
- Permission matrix: `processor` = `{READ, PROCESS}` only (`middleware/auth.py:77-87`). No WRITE_PROGRESS, ADD_FILES, MODIFY_DELETE, MANAGE_INVITES, ADMIN. So a processor can `GET` any library file (including when anonymous download is off) and use `/_processor/*`, nothing else.
- `/_processor` and `/_processor/*` are authorized by `PROCESS` alone, independent of method (`middleware/auth.py:162-164, 543-560`): no/invalid credentials -> 401 `Authentication required`; authenticated but not processor -> 403 `Processor access required`.
- `ProcessorAPI` re-checks `role == "processor"` and a non-empty username (second lock) and answers 403 `{"error":"Processor access required"}` (`ocr/remote/library_api.py:112-120`).
- A failed login (401 or 429) on any `/_processor/*` request with an attempted username fires `registry.record_failed_login(username, "invalid credentials from <ip>")` (`middleware/auth.py:417-431`, `server.py:321-326`). Same for a failed `POST /login/api/token` with `kind=="processor"` (`login/api.py:190-201, 213-219`, `server.py:381-385`). The registry keeps the last 20 refusals, newest first, username truncated to 64 chars (`ocr/remote/registry.py:39-45, 563-582`). Shown on the admin Processors card.

### 2.2 Credentials on the wire
- Processor first trades its password for a **bearer token**: `POST {root}/login/api/token`, body `{"kind":"processor","label":<processor.name>}`, header `Authorization: Basic base64(user:pass)` (`processor/client.py:447-487`). Token response (`login/api.py:149-211`): `{"token","token_type":"Bearer","kind","expires_at","user":{"username","role"}}`. Processor token lifetime 30 days (`database.py:53-60`). Password attempts are rate limited per `ip:username` (`login/api.py:182-193`); 429 `{"error":"Too many failed attempts. Retry in Ns"}`.
- Statuses 404/405 from the token endpoint -> processor falls back to sending Basic on every request (`client.py:474-477`). **CHANGE:** the Rust pair is one release; drop the fallback and always use a token. Keep "re-issue token on 401".
- Every later request carries `Authorization: Bearer <token>` (`client.py:435-439`). A bearer lookup is one indexed read, no rate limiter (`middleware/auth.py:296-304`); invalid/expired -> 401 with `WWW-Authenticate: Bearer`.
- On `register` a 401/403 with a held token triggers one fresh token + one retry; a second refusal propagates (`client.py:489-506`). A refused login is **final**: the processor exits 1 (`cli.py:236-240`). 429 or anything else is a retryable `LibraryError` (backoff).

### 2.3 Continuous re-authentication ("cut off within a heartbeat")
Authentication happens once per HTTP request but stream/events bodies live for hours, so the lib re-checks the *account* itself:
- At registration it stores `entry.account_stamp = db.processor_account_stamp(username)` (`library_api.py:236`, `database.py:903-925`). Stamp = first 32 hex of `sha256(role \0 status \0 password_hash)`, or None unless the user exists, `status=="active"` and role is `processor`.
- `_account_revoked(entry)`: stamp None -> revoked ("the account X is no longer an active processor"); stamp != stored -> revoked ("changed since it registered"); lookup exceptions revoke nobody (`library_api.py:277-300`).
- Checked: (a) on the assignment stream whenever >=15 s elapsed since the last check, evaluated each time the stream loop wakes for an op or heartbeat (`library_api.py:355-374`); (b) once at every events-body open (`library_api.py:431-434`). Revocation -> `registry.drop(pid, reason)` (claims return, stream ends). The events open then answers 403 `code:"dropped"`.
- `registry.drop_account(username, reason)` is called by the admin API when it disables/deletes/re-roles a user (`registry.py:508-523`); `drop_all` on server shutdown, after the worker has stopped (`registry.py:525-537`, `server.py:1034-1037`).
- Consequence for the processor: stream ends -> it re-registers -> login refused -> exits non-zero.

---

## 3. Transport and framing

### 3.1 Downstream: assignment stream (NDJSON)
- `GET {root}/_processor/{pid}/stream`; `200`, `Content-Type: application/x-ndjson`, `Cache-Control: no-store`, `X-Accel-Buffering: no`, chunked, unbounded (`library_api.py:333-343`).
- One JSON object per line, `ensure_ascii` (`protocol.py:151-153`). Receivers skip blank/garbage lines and non-object values without failing (`protocol.py:156-170`).
- The library emits `{"op":"heartbeat"}` whenever the per-processor op queue has been empty for 15 s (`library_api.py:357-359`). Heartbeats are *not* sent while ops are flowing.
- Python note (headers only sent with the first chunk under cheroot) explains the client's +5 s grace (`client.py:37-49`). **CHANGE:** a Rust server should flush headers immediately; keep the client socket-read timeout at `2*15+5 = 35 s` so a slow first byte can never mimic death.
- The lib does NOT stamp `last_seen` on writes (`library_api.py:375-380`): only frames the processor really sent do.
- Stream end conditions: sentinel `None` on the ops queue (drop) -> response ends; socket write failure; account revoked.

### 3.2 Upstream: events body (length-framed)
- `POST {root}/_processor/{pid}/sessions/{sid}/events`, `Transfer-Encoding: chunked`, `Content-Type: application/x-mokuro-events`, `Authorization: Bearer ...` (`client.py:641-658`). The request body is a sequence of **frames** and lives for the whole session; the library's single JSON reply arrives when the body ends (or when it refuses early).
- Frame (`protocol.py:173-186, 222-262`): `8 lowercase-hex digits = byte length H of the head` + `H bytes of UTF-8 JSON object (ensure_ascii)` + `payload bytes`. If the head has an integer `"payload": N` (N>=0, not bool), exactly N raw bytes follow. `payload` is a reserved head key: the encoder strips any caller-supplied one and sets it only when there is a tail.
- Limits: head `1 <= H <= 1 MiB`; payload `<= 128 MiB` (`protocol.py:132-134`). Violations raise ProtocolError.
- Clean EOF is only valid *before* the first header byte; any later short read is `ProtocolError("stream ended mid-frame")` (`protocol.py:222-262`). ProtocolError -> library answers 400 `bad_frame` and ends the session.
- **Keep this framing in Rust** (simple, binary-safe for sidecars, no newline ambiguity). The client wraps each frame as an HTTP/1.1 chunk (`b"%x\r\n" + frame + b"\r\n"`, `client.py:214`), terminated by `0\r\n\r\n` on close (`client.py:295`). If the Rust pair moves to HTTP/2 or a WebSocket, keep the frame layout inside the stream.
- The events body must be treated as a stream by the server: no `Content-Length`, no body-size cap, no read timeout shorter than a few ping intervals (see Section 4). The Python server's cheroot 10 s socket timeout forced the 3 s ping; the requirement is "server idle-read timeout > 3 s, and a silent body for 30 s is a dead processor".

### 3.3 Registration / token: ordinary JSON requests
`Content-Length` required, body cap 256 KiB (`library_api.py:693-718`).

### 3.4 Archive downloads
Plain `GET {root}{archive path}` using the lib's normal file serving and the same bearer token (see 8.3 for the exact HTTP behaviour the processor depends on: 200/206, `Content-Length`, `Range`, `If-Range`, strong `ETag`, 404/410, 401/403/407, 5xx handling).

---

## 4. Constants (all of them)

| Constant | Value | Where |
|---|---|---|
| `PROTOCOL_VERSION` | 2, **strict equality** at register | `protocol.py:32`, `library_api.py:156-165` |
| `ARCHIVES_ROOT` | `/mokuro-reader/` (normalised to leading+trailing `/`; `//manga//`->`/manga/`) | `protocol.py:39-54` |
| `PROCESSOR_ROOT` | `/_processor` | `library_api.py:41` |
| `HEARTBEAT_SECONDS` | 15 | `protocol.py:104` |
| `MISSED_HEARTBEATS` | 2 | `protocol.py:105` |
| client `STREAM_GRACE_SECONDS` / `STREAM_TIMEOUT` | 5 / 35 s (socket read timeout on the stream) | `client.py:48-49` |
| `EVENTS_PING_SECONDS` | 3 | `protocol.py:111` |
| `EVENTS_OPEN_SECONDS` | 30 (library waits this long after `open_session` for the body to be opened) | `protocol.py:121` |
| `EVENTS_SILENCE_SECONDS` | 30 (no frame at all => processor gone, not busy) | `protocol.py:128` |
| `EVENTS_REPLY_TIMEOUT` (client) | 10 s | `client.py:54` |
| `EVENTS_REFUSAL_PROBE` (client) | 0.5 s | `client.py:59` |
| `EVENTS_OPEN_ATTEMPTS`, back-off | 3 attempts, 1 s then 2 s, only for `body_open` | `client.py:60, 622-639` |
| `FRAME_HEAD_BYTES` / `MAX_HEAD_BYTES` / `MAX_PAYLOAD_BYTES` | 8 / 1 MiB / 128 MiB | `protocol.py:132-134` |
| `MAX_REGISTER_BODY_BYTES` / `MAX_IDENTITY_BYTES` | 256 KiB / 16 KiB (json of `{host,catalog}`) | `library_api.py:42-45` |
| `MAX_PROCESSOR_NAME` | 64 chars | `registry.py:72` |
| `MAX_SESSIONS_PER_PROCESSOR` | 16 (clamped, never refused; min 1) | `registry.py:76`, `registry.py:430` |
| `MAX_ENTRIES_PER_ACCOUNT` / `STALE_REGISTRATION_SECONDS` | 4 / 300 | `registry.py:67-68` |
| `ENDED_SESSION_TTL_SECONDS` / `ENDED_SESSIONS_KEPT` | 300 / 32 | `registry.py:57-58` |
| `FAILED_LOGIN_MEMORY` / `MAX_FAILED_LOGIN_USERNAME` | 20 / 64 | `registry.py:43-45` |
| `TRANSFER_MEMORY` / `MAX_RETURN_CLASSES` | 20 / 16 | `registry.py:80, 91` |
| `MAX_OUTSTANDING_VOLUMES` (per session, wire) / `SESSION_LOOKAHEAD` (scheduler) | 2 / 2 | `session.py:44`, `watcher.py:106` |
| `SESSION_WEDGE_SECONDS` | 600 (no runner event at all) | `watcher.py:115` |
| `SESSION_CRASH_LIMIT` | 2 sessions in a row ending with no volume finished -> stop row on that machine for the scan | `watcher.py:147` |
| `EFT_MARGIN` / `EFT_MARGIN_CAP_SECONDS` / `EFT_SLACK_SECONDS` / `EFT_LOOKAHEAD` / `EFT_CLAIM_GRACE_SECONDS` | 0.10 / 5 s / 2 s / 256 / 15 s | `eta.py:1310-1325`, `watcher.py:97` |
| `SAMPLE_CHUNK` | 256 KiB | `library_api.py:62` |
| `DOWNLOAD_BREAKER_RETURNS` / `_HOLD` / `_MAX_HOLD` | 3 / 600 s / 3600 s | `watcher.py:176-178` |
| `DOWNLOAD_RETURN_LIMIT` | 3 counted returns of one job -> recorded "download failed" | `watcher.py:183` |
| `OWN_COPY_READ_SECONDS` | 60 | `watcher.py:186` |
| processor serve backoff | start 5 s, x2, max 300 s; `REREGISTER_FLOOR` 1 s | `cli.py:31-36` |
| fetch `FetchTiming` | connect 15 s; read 30 s; retry delays 1,2,4,8,15,30 s then 30 s; stall 120 s; max_restarts 3; retry_after_cap 30 s; progress_after 3 s, progress_every 0.5 s | `archives.py:674-699` |
| spool | RAM budget 2048 MB default, `SHM_MARGIN` 64 MiB, `MEMORY_MARGIN` 1 GiB, `DISK_MARGIN` 256 MiB, `READ_CHUNK` 1 MiB | `archives.py:62-80`, `config.py:24` |

---

## 5. Endpoints (library side, `ocr/remote/library_api.py`)

All under `/_processor`, role `processor` required (Section 2). Unknown path/method -> 404 `{"error":"No such processor endpoint"}` (`library_api.py:142`). Mounted unconditionally, even with the admin panel off (`server.py:285-308`). JSON replies are `Content-Type: application/json` with `Content-Length`.

### 5.1 Endpoint table

| Method + path | Purpose | Reply |
|---|---|---|
| `POST /_processor/register` | handshake, identity, capability report | 200 reply below; 400/409/413 |
| `GET /_processor/{pid}/stream` | assignment stream (ops down) | 200 NDJSON stream; 403/404/409 |
| `POST /_processor/{pid}/sessions/{sid}/events` | events body (events + sidecars up); `{sid}` is a session id *or a `bench-...` id* | 200 `{"received":n}` at end; 4xx with `code` |
| `GET\|HEAD /_processor/{pid}/bench/{bid}/sample` | the packed benchmark sample (optional feature, 5.5) | 200/206 cbz; 404/409/416 |
| `POST /login/api/token` (outside the prefix) | token issue | see 2.2 |
| `GET /login/api/me` (outside the prefix) | used by `processor setup` to learn role | `{authenticated, role, username}` |

### 5.2 `POST /_processor/register` (`library_api.py:146-263`)
Request JSON (object; `Content-Length` required, 1..256 KiB; else 400 with `error` in: `Content-Length is required`, `invalid Content-Length`, `a registration needs a body`, `registration body too large`, `registration body is not readable JSON`, `registration body is not an object`):

```
{ "protocol": 2,
  "name": "tower",                 // optional; null/absent -> account username; non-string -> 400 "name must be text"
  "public_name": "the big box",    // optional, text or 400 "public_name must be text"
  "host":   { "cpu": "...", "gpu": "RTX 4090"|null, "backend": ..., "version": "0.5.2", "runner_build": "..." },  // non-dict -> {}
  "catalog":{ "engines": [..], "detectors": [..], "devices": [{"id","label"}..],
              "serves_mokuro": bool,                    // DROP
              "onnxruntime_gpu_providers": [..],        // DROP/optional
              "gpus": [{"index":0,"formats":["fp32","fp16",..]}] },   // optional, precision
  "max_sessions": 1 }
```
`host` is built by `describe_host` (`ocr/bench.py:260-281`: `{cpu, gpu|null, backend}`) plus `version` and `runner_build` (`cli.py:169-183`); `catalog` by `_catalog` (`cli.py:101-166`). Catalog parsing on the library: `ocr/devices.py:395-429` (`devices` rows `{id,label}`; `gpus` `[{index, formats}]`).

Processing order and responses:
1. `protocol != 2` (exact) -> **400** `{"error":"this server speaks protocol 2, not <x>","protocols":[2],"version":"<lib version>"}`. The setup wizard deliberately sends protocol `0` to read this answer without registering (`setup.py:84-87, 218-285`). **KEEP this probe contract** (any non-matching protocol yields 400 with `protocols` list and `version`).
2. `name` -> `clean_processor_name`: `raw.strip()[:64].strip()`, falling back to the username similarly (`registry.py:152-161`). If `registry.is_reserved_name(name)` (casefold equals `"local"` or equals this server's local row name, which is `"this server"` when local processing is on, `server.py:294-296`) -> 400 `'<name>' is a reserved name`.
3. `public_name`: must be str; cleaned with fallback `""` -> None when empty; reserved -> 400.
4. `catalog` null -> `{}`; non-dict -> 400 `catalog must be an object`; if `engines`/`detectors`/`devices` present and not lists -> 400 `catalog.<key> must be a list`. **Empty/absent lists are legal** (a processor still installing; the lib shows "installing").
5. `max_sessions`: `int(body.get("max_sessions") or 1)`, non-numeric -> 400 `max_sessions is not a whole number`; later clamped to `[1,16]`.
6. `len(json.dumps({host,catalog})) > 16 KiB` -> **413** `host and catalog take more than 16384 bytes`.
7. Name ownership: if another account's entry already registered under this exact name, or the on-disk profile claims the name for a different account (`profiles.claim`, persistent across restarts) -> **409** `the name '<n>' belongs to another processor account; give this machine its own name`. A profile with no owner is claimed by the first later registrant.
8. `registry.register(...)` (7.1) then `profiles.set_identity(name, host, catalog)` (swallow errors), `account_stamp` stored.
9. **200**:
```
{ "protocol": 2, "processor_id": "<16 hex>",
  "session_stream": "/_processor/<pid>/stream",
  "events": "/_processor/<pid>/sessions/{sid}/events",   // literal "{sid}" placeholder
  "archives": "/mokuro-reader/" }
```
The processor stores `processor_id`, `stream`, `events` (substitutes `{sid}`), `archives` (default `/mokuro-reader/` if missing); an empty id or stream is a `LibraryError` (`client.py:541-549`). **CHANGE (permitted):** channel paths may be fixed constants; keep `archives` advertised because the volume op's archive path is built from the same root. The processor does not otherwise use `channels["archives"]` (ops carry full URL paths already prefixed with the root); it is informational.

### 5.3 `GET /_processor/{pid}/stream` (`library_api.py:304-394`)
- Entry must exist, not be the local entry, and belong to the caller's account (`_owned`, `library_api.py:686-691`). Else: if the id exists at all -> **403** `{"error":"Not your processor"}`; unknown -> **404** `{"error":"No such processor"}` (no `code` field on these two).
- If `entry.stream_open` already: the first stream is a ghost. **Drop the entry** (reason `a second stream was opened`, claims return) and answer **409** `{"error":"Stream already open; register again"}`. The entry is NOT resumed; the processor must re-register (gets a new id).
- Otherwise 200 and stream: set `stream_open` (under the entry lock), loop: wait up to 15 s on the entry's op queue; timeout -> yield heartbeat; `None` sentinel -> end; every >=15 s run the account check (2.3); yield each op as one NDJSON line.
- On exit (any reason): clear `stream_open`; **if the entry was not already dropped, `registry.drop(pid, "stream closed")`** (a stream that ends on its own is a disconnect). If it was already dropped (re-register, revocation, admin) the earlier reason stands.

### 5.4 `POST /_processor/{pid}/sessions/{sid}/events` (`library_api.py:398-578`)
Pre-body checks, in order, each a JSON reply with `{"error": <prose>, "code": <code>}` (`EVENTS_REFUSALS`, `library_api.py:51-59`):

| Condition | Status | `code` | Client action |
|---|---|---|---|
| entry exists but is another account's / local | 403 | `not_owner` | re-register |
| no such entry | 404 | `unknown` | re-register |
| account revoked/changed (also drops the entry) | 403 | `dropped` | re-register (then refused, exit) |
| `sid` not in `entry.sessions`, but ended within the last 300 s | 409 | `session_ended` | carry on with other sessions |
| `sid` unknown otherwise | 404 | `unknown` | re-register |
| another body already holds this session | 409 | `body_open` | back off and retry (<=3 tries) |
| unreadable frame | 400 | `bad_frame` | do not retry |
| processor dropped mid-body | 409 | `dropped` (+`received`) | re-register |
| session ended by the library mid-body | 409 | `session_ended` (+`received`) | move on |

Body loop: `read_frame` repeatedly. For **every** frame: if `entry.dropped` -> `session.end(reason)`, 409 `dropped`; if `entry.sessions[sid] is not session` -> `session.end`, 409 `session_ended`; else stamp `entry.last_seen = now` (any frame, ping included), `session.feed(head, payload)`, `received += 1`.

Ending the body:
- Clean EOF -> `_ended_without_exit` then `200 {"received": n}`.
- `ProtocolError` -> `session.end`, 400 `bad_frame` with `"unreadable event frame: <msg[:200]>"`.
- `TimeoutError`/`OSError` (socket) -> `_ended_without_exit`, then 200 `{"received":n}` (reply is moot).
- Any other exception (own bug, malformed chunking) -> `_ended_without_exit`, 400 `bad_frame` with `"the events body failed: <ExceptionClass>"` (class only, never the message).
- Always `session.release_events()` in `finally`.

`_ended_without_exit` (`library_api.py:549-578`) is a **core semantic**: if the session is still alive when its body ends (no runner `exit` was fed), the *processor* went away; so `registry.drop(pid, "the events body of session <sid> ended without its runner's exit (<reason>)")` happens FIRST (claims return, nothing recorded, no row struck), and only then is the session's synthetic `exit` queued. Reversing the order would blame the oldest volume for a machine being switched off. A processor that is leaving says nothing further on any body (8.6) precisely so this fires.

`{sid}` may also name a benchmark (`bench-` prefix); the sink resolves either in `entry.sessions` (`session.py:446-463`).

### 5.5 `GET|HEAD /_processor/{pid}/bench/{bid}/sample` (`library_api.py:582-658`) (benchmark feature)
Benchmarks (admin "Benchmark & tune" and auto-bench) are an optional subsystem. If the Rust port keeps them, the contract is:
- entry owned by caller else 404 `No such processor`; `bid` must start with `bench-` and be in `entry.sessions`; else if recently ended -> 409 `session_ended`; else 404 `No such sample`.
- File `<storage>/.processing/<sanitised bid>.cbz` (`bench_sample_filename`: keep `[A-Za-z0-9_-]`, max 80, default `sample`, + `.cbz`). Not in the served library tree. The sample URL sent in the `bench` op is `/_processor/<pid>/bench/<bid>/sample` (`ocr/bench.py:1513`).
- `Range: bytes=a-b | bytes=a- | bytes=-n` supported (single range), 206 with `Content-Range`; invalid -> 400 `bad Range header`; unsatisfiable -> 416. Headers: `Content-Type: application/vnd.comicbook+zip`, `Content-Length`, `Accept-Ranges: bytes`, `Cache-Control: no-store`. No ETag. HEAD returns headers only. Streams in 256 KiB chunks.

---

## 6. Messages

### 6.1 Ops, library -> processor (`protocol.py:60-62`; sent via `ProcessorEntry.send`, only names in `OPS` accepted, else logged and dropped `registry.py:293-314`)

All ids matching `[A-Za-z0-9_-]{1,64}` or the processor drops the whole op with an error log (`bridge.py:139-144, 195-198`; they become file names). Library ids: processor id `token_hex(8)`, sid `token_hex(6)` (`registry.py:625`), claim `v<seq>` (`watcher.py:_next_session_job_id`), bid `bench-<key>`.

1. **`open_session`** `{"op","sid","generation":<row spec>}` (`session.py:129-131`). `generation` is the OCR "recipe" row as *that machine should run it*: `generation.to_dict()` (`id, name, primary, enabled, engine, detector?, patch_budget, precision?, pools{stage_workers, queue_capacity, stage_device}, precision_pick?, precision_why?`; `ocr/generations.py:373-391`) with `pools` replaced by that processor's per-machine pools (`watcher.py:3480-3600`) and precision pick. **CHANGE/Rust:** this object is "the recipe", defined by the OCR spec; the processor treats it as opaque-but-parseable. Processor forces `primary=True, enabled=True`, default `name="remote"` (`bridge.py:329-342`). The sidecar *name* never comes from the recipe, only from the volume op.
2. **`volume`** (`session.py:267-283`):
```
{ "op":"volume","sid","claim":"v12",
  "archive":"/mokuro-reader/<series>/<vol>.cbz",   // archives_root + "/".join(path parts relative to <storage>/library); NOT percent-encoded
  "sidecar_name":"<stem>.mokuro" | "<stem>.<row name>.mokuro",   // volume.output.name: stem + generation.sidecar_suffix (generations.py:282-287)
  "title":"<series dir name>", "volume_title":"<stem>",
  "title_uuid": "<uuid5(NAMESPACE_DNS, series name)>", "volume_uuid": "<...>"|null,
  "size": <bytes>        // optional: library's stat().st_size at claim time
}
```
   (`ocr/processor.py:1811-1850` builds the `SessionVolume`.) The library refuses to send (returns False, logged) when: volume has no archive (only an extracted dir), archive is outside `<storage>/library` (resolved), or the session already has 2 outstanding claims (`session.py:240-266`). The send registers `_volumes[claim]` and appends to `_order` BEFORE queuing; a failed send un-registers it. It then appends one line to the volume log: `processed remotely on <label>; the runner's own log is on that host` (`session.py:301-311`).
3. **`cancel`**: `{"op":"cancel","sid","claim"}` for a volume (one per outstanding claim) or `{"op":"cancel","bid"}` for a benchmark. Semantics: nothing is recorded for a cancelled claim; processor kills the whole session runner (no per-volume un-submit).
4. **`close_session`** `{"sid"}`: finish everything the runner already accepted; abandon claims still downloading with no event; then exit.
5. **`bench`** `{"op":"bench","bid","spec":<recipe>,"sample":"/_processor/<pid>/bench/<bid>/sample","pages":<int>,["precision_only":true]}` (`session.py:505-515`). Optional feature (5.5).
6. **`heartbeat`** `{"op":"heartbeat"}`; ignored by the processor except as liveness.

### 6.2 Events, processor -> library (`protocol.py:80-100`; frames on the events body)
Allowed names: `ready, volume_started, page, volume_done, volume_failed, stats, fatal, bench_ready, bench_progress, bench_trial, bench_done, sidecar, ping, exit, spawn_failed, fetch, volume_returned`. Anything else is dropped with a (truncated to 40 chars) warning (`session.py:316-325`). Frames carry the head as-is (payload key removed before queuing, `session.py:332`).

Runner events (forwarded verbatim; defined by the OCR runner, `ocr/session.py:68-80`, `ocr/engine_runner.py:7978-7990, 8168-8198, 8285-8296`):
- `ready` `{startup_seconds, weights, stage_workers, queue_capacity, stage_device, pipeline}`
- `volume_started` `{id, pages}`
- `page` `{id, done, total}`
- `stats` `{pipeline:<snapshot>, ...}` every ~2 s
- `volume_done` `{id, pages, failed_pages, seconds, stats, [cpu_pressure], [other_cpu]}`
- `volume_failed` `{id, error}`
- `fatal` `{error}`; session-level `spawn_failed {error}` and `exit {returncode}`.
The Rust OCR spec owns these; this spec requires only: `id` = the claim; `volume_done.pages/seconds/failed_pages` are used for rate learning and provenance; `seconds` is wall time between page emissions (excludes model load); `ready.startup_seconds` is recorded as that machine's per-session start cost.

Protocol-added events:
- **`ping`** `{event:"ping"}` every 3 s when idle (`client.py:259-271`). Ignored by `feed` but stamps `last_seen`.
- **`sidecar`** `{event:"sidecar", id:<claim>, name:<file name>, payload:N}` + N bytes: the finished sidecar. Sent **immediately before** the `volume_done` that announces it (`bridge.py:731-740, 827-841`). (6.3)
- **`fetch`** `{event:"fetch", id:<claim>, state:"downloading"|"retrying"|"restarting"|"ready", ...}` (`archives.py:807-821, 1097-1099, 1165-1172, 1364-1369`, `bridge.py:615-617, 654-656`):
  - `downloading`: `{bytes,total,requests}`; `retrying`: `{bytes,total,requests,error,retry_in}`; `restarting`: `{error,restarts}`. Rate-limited to >=0.5 s apart and nothing for the first 3 s; offered as "latest per key" and sent on the 3 s ping tick *in place of* a ping (so they double as keep-alives and are never blocked behind a sidecar upload; `client.py:233-271`). Pure liveness for the library: arrival resets the wedge timer; payload otherwise ignored.
  - `ready` (exactly one per delivered claim): `{bytes, seconds, mb_per_s, requests, restarts, repairs, verify_seconds, placement:"memory"|"disk", crc32:"<8 hex>", members, [verdict:"damaged at the library", damaged:[names], structural]}` (`archives.py:750-769`). Sent only after the verified archive has been handed to the runner. Library: marks claim **delivered**, closes that processor's download breaker, forgets the job's download-return count, feeds `TransferStats` (`session.py:347-348`, `watcher.py:5081-5130, 5207-5255`).
- **`volume_returned`** `{event, id, class, error[:300], bytes, total, requests, [status]}`: a claim that never reached the runner (9.2). Terminal for the claim; never a failure of the volume.

Library-side feed rules (`session.py:315-349`): `ping` swallowed; `sidecar` -> install (6.3); `exit` -> single claimed exit (see below); `fatal`/`spawn_failed` record `_fatal = error[:300]`; `volume_done|volume_failed|volume_returned` free that claim's outstanding slot immediately (so the 2-deep lookahead tops up while the watcher judges it); `volume_returned` additionally `note_returned` (error truncated to 300); `fetch ready` additionally `note_transfer`; everything else (except sidecar/ping/exit) is queued to the watcher.

Exactly-one-exit rule: whichever of {processor's own `exit`, `end(reason)` from body close/ghost/kill} arrives first queues THE exit; `_ended` is set after the put; the session removes itself from `entry.sessions` and records a 300 s / 32-entry tombstone (`session.py:400-443`, `registry.py:237-253`). The watcher therefore never sees two exits.

### 6.3 Result upload (sidecar) semantics (`session.py:351-398`)
On `sidecar{id,name}+payload`:
1. Look up `_volumes[id]`. Unknown claim -> warn and ignore (**not** fatal: a cancelled claim may still be answered).
2. `name` must equal `volume.output.name` (the name the library chose). Mismatch (or empty) -> log error, call `on_rejected(volume, "its sidecar arrived as <name[:80]!r>, not <expected!r>")` (audit entry), and **end the session** (`end("a sidecar arrived as ...")`). Unconditional: a processor that cannot name its own output is untrusted.
3. Write payload to `<output>.tmp` then atomic replace to `volume.output` (a library-side temp workspace path, not the final library path), creating parent dirs. On OSError log + end the session. Always unlink the `.tmp`.
4. The following `volume_done` goes through the normal local collection path (`watcher.py:5476-5560`, `ocr/processor.py:1861-1887`): archive-still-current check (deleted/replaced archive => discard result, release job, audit "rejected"); claim must still be held by the same slot (a disconnected processor's late result is ignored: "a file written now would land beside the one the next owner writes"); JSON validity (plain or `.mokuro.gz`); metadata normalisation (stamps volume uuid etc.); move to the destination beside the archive, `<stem><suffix>` or a numbered unique name if that exists; provenance row (who wrote it: machine name, processor *account*, build from `host.version`+`host.runner_build`, facts, pages, failed_pages, archive stamp). Any failure -> record rejected (audit) and settle as failure of the volume.
5. The library writes **every** sidecar itself; a processor has no write permission anywhere.

Processor side (`bridge.py:705-750, 827-841`): on `volume_done` it reads `volume.output` and sends `sidecar` first; if the read or send fails the event is **downgraded to `volume_failed`** with error `the finished sidecar could not be sent from the processor` (a `volume_done` without its file would read as a mystery on the library). A cancelled claim's terminal event is swallowed (nothing recorded). A `volume_failed` for a claim whose archive was "damaged at the library" gets `" (the library's copy: the same bytes on two downloads); <describe_damaged>"` appended (`bridge.py:70-86`).

---

## 7. Library-side lifecycle

### 7.1 Registry (`ocr/remote/registry.py`)
In-memory `ProcessorRegistry`: `_entries{pid -> ProcessorEntry}`, `_failures` ring, `_last_disconnect (name, ts)`, `public_names` alias allocator, optional `_local` entry (`id "local"`, `name "this server"`, only when local processing is on; never addressable by a processor; `send()` always False for it).

`ProcessorEntry` fields: `processor_id, name, username, host, catalog, max_sessions, connected_since, last_seen, ops (unbounded queue; None = end-of-stream sentinel), sessions{id->RemoteSession|RemoteBench}, stream_open, local, dropped, public_name, account_stamp, transfer (TransferStats), ended_sessions (tombstones)`. `label()` = `"<name> (<host.gpu>)"` or name. `installing` = remote and `catalog.engines` empty. `open_sessions` counts non-`bench-` sessions only.

Locking contract to preserve (or replace with an equivalent single-writer model): registry lock -> entry lock, never inverse; `send()` and `drop()` flag-flip are one critical section so no op can be queued behind the sentinel.

`register(...)` (`registry.py:389-465`), atomic under the registry lock, for **this account only**:
1. name cleaned; new entry with `processor_id = token_hex(8)`; `max_sessions = max(1, min(16, int(max_sessions or 1)))`.
2. doomed entries: same name ("re-registered" - the reconnect case, replaces rather than rejects so the old claims come back before the new entry is offered anything); entries with no stream that have been silent for >300 s ("registered but never opened a stream"); then if still >= 4 entries for the account, evict oldest *without a stream first* (`keep.sort(key=(stream_open, connected_since))`), reason "too many registrations from this account". Newest registration always wins.
3. Each doomed entry: removed, `dropped=True`, `stream_open=False`, `_last_disconnect` updated; **after** the lock: `ops.put(None)` and `on_drop(entry, reason)` (the worker returns claims).
4. `public_names.assign(name, public_name)`: numbered alias "machine N" in order of first registration this process, or the processor's own `public_name`.

`drop(pid, reason)` is the single disconnect path (stream closed, events body ended without exit, account revoked, ghost stream, silent processor, admin/account ops, shutdown). It never fails if already gone. `drop_account`, `drop_all` wrap it.

`on_drop` -> `OCRWorker.processor_disconnected` (`watcher.py:4300-4345`): under the worker lock, for every claim owned by a slot of that registration (excluding claims whose sidecar is mid-install) -> settled as released, removed from attempted set; breaker state for that registration discarded; sessions of that entry get `end(reason)`; queue generation bumped, waiters notified; log `"<label> disconnected (<reason>); N volume(s) back in the queue"`. Late `volume_done` from before the drop is ignored by `_take_for_settling` (a log line, no blame, no release of someone else's claim).

### 7.2 What a connected processor becomes in scheduling
- Each connected registration contributes `max(1, max_sessions)` **slots/lanes** to the worker while a scan runs (`watcher.py:3446-3478`); new registrations are given slots in the running scan within `SLOT_SUPERVISE_SECONDS=1` (`watcher.py:84-90`).
- A slot offers a row to the processor only if `catalog_can_run` is None (`ocr/remote/scheduler.py:50-92`): engine in `catalog.engines`; the row's detector in `catalog.detectors` (checked even if locked); every stage pinned to a device id the catalog reports (`auto`, `cpu`, `""` are universal; `scheduler.py:24-47`); the row's precision mode runnable on the reported card formats (`row_refusal`, `ocr/precision.py:121-132`). Gate uses the placement the machine would actually run (`row_spec_for(entry, row).pools.stage_device`), not the row's own table (`registry.py:639-652`). A catalog with no engines (installing) matches nothing.
- No remote ever runs the per-volume (non-session) path: every remote row is a session (`watcher.py:4657-4666`, `registry.py:632-637`).
- Rows with no benchmark/profile on that machine are benchmarked first when autobench is on ("measured on THIS machine first"); skip if bench feature dropped.
- A processor's `host`/`catalog` are copied into its on-disk profile at every registration so benchmarks and estimates still read "on tower (RTX 4090)" after it disconnects.

### 7.3 `RemoteSession` (library's view of one session), `ocr/remote/session.py:47-443`
- Created per claim-owning slot by `RemoteOCRProcessor.open_session` with a fresh `sid`; `start()` registers in `entry.sessions` and sends `open_session`; if the send fails (processor already dropped) it queues `spawn_failed {error:"<name> disconnected"}` + `exit {returncode:null}` and returns False. `start()` after `kill`/ended returns False.
- `submit` (6.1.2) enforces <=2 outstanding claims. `claims()` lists outstanding claims.
- `close()` -> one `close_session` op (idempotent). `kill()` (once) -> one `cancel {sid, claim}` per outstanding claim, then `close_session`, then `end("killed")`. Remote kill = cancel+close; nothing is recorded for cancelled claims (used by settings change, bench pre-empt). `RemoteOCRProcessor.cancel_active` kills only a still-live session.
- `claim_events()` grants the single events body (`_events_open`), sets `_events_seen`; `release_events()` clears `_events_open` only (so a new body can be opened later, `body_open` cleared).
- `events_overdue(seconds)`: `open_session` sent > `seconds` ago, no body ever seen, still alive. Used with `EVENTS_OPEN_SECONDS`: the processor opens the body FIRST, before spawning anything, so this means "processor not there" (suspended/unplugged), not "slow model load".
- `is_alive()` = not ended and not `entry.dropped`.
- `wait()` returns 0 when ended; `stderr_tail()` returns `_fatal`.

### 7.4 The watcher's session loop for a remote session (what the wire must support) (`watcher.py:4640-5020`)
- Lookahead: keeps up to 2 claims in flight; tops up when a claim terminates; stops accepting when the queue is held, the row is stopped for this machine for this scan, the download breaker is open, the row was disabled, or the session is dead (`_session_drain_reason`, `watcher.py:4990-5020`).
- No event for 600 s = wedged -> `_remote_session_lost(session, wedged)` (`watcher.py:4845-4890`):
  * events body never opened within 30 s of `open_session` -> `registry.drop(pid, "it never opened the events body of session <sid> (30s)")`;
  * wedged AND `now - entry.last_seen > 30 s` -> drop ("it has sent nothing for Ns");
  * otherwise (processor still pinging) a **wedged runner**: `session.kill()` and blame like a local runner.
  Drops blame nothing and strike no row.
- Session end accounting (`watcher.py:5612-5740`): if the processor left -> nothing blamed; if the runner never became `ready` (remote) -> environment failure, nothing blamed, start-failure backoff recorded per (row, machine), strike; else the **oldest delivered** in-flight claim (first of `order`, only if `delivered`) gets a failure record, all others are released with `retry_this_scan`; a session ending with zero completed volumes counts a strike for (row, machine); `SESSION_CRASH_LIMIT=2` strikes stops that row on that machine for the rest of the scan (not on other machines, resets next scan); completing >=1 volume clears strikes. Only a claim the runner actually received (`fetch ready`/`volume_started`) can be blamed; a `fatal`/runner crash while the oldest claim was still downloading blames nobody.
- Session start failure on a processor (`spawn_failed`, no session) -> volume released unrecorded, retried this scan, strike on that machine; start-failure signatures space attempts across scans (`watcher.py:4900-4940`).

---

## 8. Processor-side lifecycle

### 8.1 `processor serve` (`processor/cli.py:186-332`)
Startup order:
1. Load + validate `processor.yaml` (10.2). Errors -> `click.ClickException`.
2. **Storage lock** (`<storage>/.processing/serve.lock`, exclusive non-blocking `flock`; none on Windows): a second processor on the same storage prints `another processor is running on <storage>; give each processor its own storage` and exits 1 (`cli.py:64-78, 306-316`). Keep (use an OS file lock; consider `LockFileEx` on Windows, Q15).
3. SIGTERM handler raises KeyboardInterrupt (clean stop).
4. Build the archive spool (8.3), sweep `<storage>/.processing/archives/` leftovers (safe only because of the storage lock).
5. Loop forever (`_serve_with`):
   ```
   backoff = 5
   loop:
     client = new LibraryClient
     catalog, host = probe(); register(catalog, host+{version, runner_build})
       LibraryLoginRefused -> status "refused", print "Login refused: ...", EXIT 1
       other LibraryError   -> status "unreachable"; sleep backoff; backoff = min(2*backoff, 300); continue
     bridge = RunnerBridge(client, ...); backoff = 5; status "connected" (+name)
     print "Connected to <url> as <name> (<n> engine(s), <max_sessions> session slot(s))"
     try: for op in client.ops(): bridge.handle(op)
       ReregisterNeeded -> at_once = true
       LibraryError    -> print
     finally: bridge.shutdown(); client.close()
     status "disconnected"
     if at_once: sleep 1 (REREGISTER_FLOOR); continue
     print "Disconnected; reconnecting in <backoff>s"; sleep backoff; backoff = min(2*backoff,300)
   ```
   On KeyboardInterrupt: status "stopped", print "Processor stopped", release lock, spool closed.
6. The catalog and host are **re-probed at every registration**, so an install that finishes while the processor is running becomes visible on the next reconnect.

Reconnect semantics: the registration id is never reused. Any stream end (silent for 35 s, socket error, EOF, 409) leads to a fresh `register`, which the library treats as "re-registered" for the same name (drops the stale entry, returns claims). **Everything in flight when the stream ends is shut down** (`bridge.shutdown()`): runners killed, spool released, **nothing more reported** (so the library sees bodies end without an `exit` = processor left). Work in flight is lost, nothing recorded as failure.

`LibraryClient.ops()` (`client.py:569-618`): `GET` stream with socket timeout 35 s; non-200: 409 -> `ReregisterNeeded`, else `LibraryError` (401/403 -> `LibraryLoginRefused`; a body with `protocols` adds "(this library speaks protocol [...])"); lines are decoded with `decode_line`, blank/garbage skipped; `TimeoutError` (35 s of silence) and socket errors simply end the generator (caller reconnects). `close()` shuts the socket down (not close) so a blocked read on another thread returns cleanly; it is called from any thread (e.g. a re-register request from a session event).

### 8.2 `RunnerBridge.handle` (`processor/bridge.py:185-290, 344-418`)
- `heartbeat` ignored. Id-shaped fields validated. An exception in handling one op is logged and costs only that op.
- **`open_session`**: if `_leaving` -> ignore. **Open the events body first** (`client.open_events(sid)`, retries `body_open` x3). An `OSError` opening it: log, `client.close()` (forces re-register; the library cannot tell "could not reach" from "gone" and treats a never-opened body as the processor leaving), return. If the body was refused at open (`sink.ended`): act on the refusal (`_react`) and do **not** spawn a runner. Then: build recipe + OCR driver, open the session (log `<storage>/logs/session.<sid>.log`); on `FileNotFoundError/OSError/ValueError` send `spawn_failed {error}` + `exit {returncode:null}` and close the body; start the runner (failure: `spawn_failed` + `exit`, finish); start a *feeder* thread and a *pump* thread; finally attach `sink.on_end(...)`.
- **`volume`**: if session unknown, warn+drop; else enqueue on that session's FIFO `work` queue.
- **`cancel`**: with `bid` -> set that benchmark's stop flag + abort its fetch; with `sid`+`claim` -> mark claim cancelled, set session `stopping`, abort fetches, kill the session runner (whole session ends; the pump swallows terminal events of cancelled claims).
- **`close_session`**: `stopping` set, in-flight downloads aborted (abandoned silently), runner told to close (finishes accepted volumes in order).
- **`bench`**: own thread, see 8.7.

### 8.3 Archive download, caching, verification (`processor/archives.py`)
There is **no persistent cache**: an archive lives for the lifetime of its claim and is released at the claim's terminal event. Per session: the volume in the runner + the one on deck (library's 2-deep lookahead) at most; across sessions the spool budget is shared.

`ArchiveSpool` (`archives.py:349-502`) decides where bytes live: RAM if (budget `archive_memory_mb` not exceeded, tmpfs `/dev/shm` free minus other reservations >= size+64 MiB, machine/cgroup headroom `min(MemAvailable, cgroup limit-usage)` >= size+1 GiB, and `/proc` fd hand-off usable (Linux)), as an **unnamed file** (`O_TMPFILE`); else disk under `<storage>/.processing/archives/` (unnamed `O_TMPFILE` where supported else a private `<uuid>.cbz`), requiring size+256 MiB free; else `TransferFault(no_room)`. A write ENOSPC in RAM re-places on disk and restarts the copy from 0. Reservation is the whole size up front. `Placement.release()` is idempotent and returns the reservation. **DROP/CHANGE for Rust:** the `/proc/<pid>/fd/<n>` runner hand-off is Python-subprocess machinery; a Rust in-process pipeline can read the downloaded bytes directly. Keep the *policy*: bounded RAM budget (default 2048 MB, 0 = always disk), disk fallback, headroom margins, no-room => return the claim (`no_room`), sweep leftovers at start. On Windows there is no `/dev/shm`: always disk.

`ArchiveFetcher.fetch(url_path, size, cancel, progress, label)` (`archives.py:935-1020`):
1. URL = `client.root + percent-quote(path bytes, safe="/")` (non-UTF-8 names do not raise; lib answers 404 if wrong).
2. `_download`: loop `_attempt` until a complete copy. Each `_attempt` is one new connection (connect timeout 15 s, read timeout 30 s) `GET` with `Authorization`; when resuming: `Range: bytes=<received>-` and `If-Range: <strong ETag>` if one was seen.
3. Response handling (`archives.py:1213-1310`):
   - `X-Accel-Redirect` present -> return class `mismatch` (DROP unless offload kept).
   - 401/403/407 -> `LibraryTransportError` (whole processor steps away, 9.1).
   - 404/410 -> return `missing`.
   - 412/416 -> restart from 0 (counts as a restart).
   - 408/429/>=500 -> retryable `_Transport`; honour `Retry-After` (cap 30 s); `server_error` (>=500 except 502/503/504) twice in a row -> return `stalled` with status 500.
   - other not-200/206 -> return `rejected`.
   - 206: needs Content-Range, `start == received`, total/ETag consistent with earlier; else restart. 200 while resuming = file changed or Range ignored -> counted restart (`no_range` if ETag unchanged/absent), start over from 0.
   - If op carried `size` and the response length/total != `size` -> return `mismatch`.
   - ETag used only if **strong** (quoted, not `W/`).
   - Short read/closed early -> retry (resume); more than expected -> restart.
4. Back-off between failed requests: 1,2,4,8,15,30 s, then 30 s; reset to first step after any attempt that received new bytes. **Stall budget is silence, not slowness**: 120 s with no new byte since the last progress (across all attempts of all copies) -> return `stalled`.
5. `max_restarts` = 3; exceeding it -> `no_range` (if every restart was a Range ignored) else `changed`.
6. **Verify** (`verify_archive`, `archives.py:602-668`): open the zip; for every distinct member name (the last entry of a name wins, as the reader would resolve it), skipping directories, read to EOF in 1 MiB steps checking CRC-32/inflate (`zipfile` semantics); unsupported methods/encryption -> "skipped" (not damage); structural failure or damaged members -> not ok. Check cancel after every MiB. (No `InflateLimit` on the processor path; that limit is for uploads, other spec.) Rust: use any zip reader that verifies CRC-32 per member against the central directory.
7. If not ok: download a **second full copy** (diagnostic). If its ETag/total differ from the first: the file changed -> second becomes the copy, counted restart, re-verify (loop). Else verify second: ok -> "corrupted in transit", first discarded, `repairs += 1`. Both bad: identical `(received, crc32)` -> **damaged at the library**: deliver the first copy as is with `verdict="damaged at the library"` and `damaged` names (the runner then decides exactly as it would locally; a runner failure gets the damaged-note appended); different bytes -> return `differs`.
8. Return value `FetchedArchive{placement,size,crc32,requests,restarts,repairs,seconds,verify_seconds,members,damaged,structural,verdict,shadowed}`; `summary()` is the `fetch ready` event body (6.2).

`TransferFault` classes (`RETURN_CLASSES`, `archives.py:137-140`): `stalled, differs, changed, no_range, mismatch, missing, rejected, no_room, local`. `local` = any other local exception.

### 8.4 Feeder (per session): fetch then submit (`bridge.py:550-675`)
One feeder thread per session processes `volume` ops **strictly in arrival order**, so the claims delivered to the runner are always a prefix of the library's `order` (this is what makes "only a delivered claim can be blamed" sound).
For each op: derive `hint = basename(volume_title)`, `stem = basename(stem of archive path)` (thumbnail rule keyed on the archive stem), `sidecar_name = basename(op.sidecar_name or "<volume_title>.mokuro")` (every wire string that becomes a path is reduced to a basename); `fetch(...)` with progress offers keyed `fetch:<claim>`; build workspace; hand the verified archive + metadata (`title`, `volume`, `title_uuid`, `volume_uuid`) to the runner; if the session is stopping drop the claim silently; if the runner refuses the volume, drop (the session is over); on success `withdraw` the progress offer and emit `fetch{state:"ready", ...summary}`. Failure mapping: `FetchCancelled` -> silent; `LibraryTransportError` -> `_lost_library` (8.6); `TransferFault` -> `volume_returned{class=kind, error[:300], counters}` unless the session is stopping/leaving; any other exception -> `volume_returned class "local"`. The feeder never dies. Volumes still queued when a session ends are left alone (the library returns them).
Release discipline: an archive is the feeder's until handed over; then the pump's, released **before** forwarding the claim's terminal event (so the library's top-up can never find 3 archives held), workspace removed after.

### 8.5 Pump (per session): runner events -> upload (`bridge.py:705-750`)
Reads runner events (1 s poll); for terminal `volume_done|volume_failed`: release archive; if the claim was cancelled swallow; for `volume_done` upload the sidecar frame first (6.3 fallback to `volume_failed`); append damaged note; forward the event; drop claim state. Stops after forwarding `exit`; `_finish` closes the events body (sends final chunk, reads the library's reply).

### 8.6 Leaving / losing the library
- `shutdown()` (stream ended, SIGTERM): `_leaving` set; every session `leaving`+`stopping` set **before** any runner is touched so no event of the kill (a signal `exit`, a `fatal` from a cut feed) reaches the library; fetches aborted; runners killed (5 s wait); bodies closed. Library-visible effect: body closes without `exit` => processor left => claims return unrecorded (7.1).
- `_lost_library(reason)` (account refused for a download, 401/403/407): sets leaving, stops everything, aborts fetches, `client.close()` (ends the stream => serve loop re-registers; a revoked account is then refused and the process exits non-zero).
- `_react(sid, sink)` per events-body refusal action: `reregister` -> log + `client.close()`; `retry` handled by `open_events`; `move_on`/`stop`/none -> log only.
- `_on_sink_end` (library ended this body, found by whichever thread touches it first, ping thread included): react, mark stopping, abort fetches, kill the runner. The library sends no op for a session it ended itself, so without this an idle runner would keep models loaded on a card the library already counts free.

`EventSink` (`client.py:113-340`) semantics to preserve: one body per session; `send` returns False once closed/ended; before every write it peeks non-blockingly for an *early reply* (any application byte means the library has stopped reading and is answering; reading it intact preserves the `code`); first failed send (or early reply) marks the body "ended by library", collects the reply (<=10 s), and fires `on_end` exactly once from whichever thread found it; `close()` sends the terminating chunk and reads the single reply; the reply status/`code`/`error` -> `action` (`dropped|not_owner|unknown`->`reregister`, `body_open`->`retry`, `session_ended`->`move_on`, `bad_frame`->`stop`; a 401/403 with *no* code (auth layer) -> `reregister`). A TLS 1.3 pitfall in the Python (session tickets making the socket readable) is handled in `_early_reply`; with a Rust client this is a non-issue if the HTTP stack gives access to the response while the request body is still streaming (HTTP/1.1 full-duplex or HTTP/2). **Requirement:** the client must be able to observe an early response while still streaming the request body.

### 8.7 Benchmarks (optional feature)
`bench` op -> own thread (key = bid; duplicate bid ignored): open events body first; fetch the sample via `GET sample` (size unknown, no ETag; Range resume works via the same fetcher), unzip pages, run the benchmark runner, forward `bench_ready|bench_progress|bench_trial|bench_done|fatal` (stamping GPU-busy samples onto `bench_trial` from where the work happens), always finish with `exit{returncode:null}` + body close - except when the processor is leaving (then silent). `cancel{bid}` sets the stop flag and aborts the fetch. Library side `RemoteBench` (`session.py:446-615`): registered in `entry.sessions["bench-..."]`; start sends `bench`; no body within 30 s -> synthetic `fatal` + exit; `kill` sends `cancel{bid}`; `bench_done`/`fatal` makes it terminal so a body that closes after them is not a processor leaving; `exit` from far end ends it; `ping`/`sidecar` ignored.

---

## 9. Failure handling matrix

### 9.1 What the processor does
| Event | Reaction |
|---|---|
| Register: login refused (401/403) | print, status `refused`, exit 1 |
| Register: unreachable / 5xx / 429 / 400 protocol / 409 name | status `unreachable`, retry after backoff 5,10,...,300 s |
| Stream: silent 35 s, socket error, EOF | shutdown bridge, backoff, re-register |
| Stream: 409 | `ReregisterNeeded`: re-register after 1 s floor, no backoff |
| Stream: other non-200 | `LibraryError` -> backoff |
| Events body refused: `dropped`/`not_owner`/`unknown` or 401/403 w/o code | `client.close()` (-> re-register) |
| Events body refused: `body_open` | retry up to 3 total attempts with 1 s, 2 s |
| Events body refused/cut: `session_ended` | drop that session only |
| Events body: `bad_frame` | stop that session |
| Download 401/403/407 | whole processor steps away and re-registers (nothing reported, claims return) |
| Download 404/410, stalled, mismatch, no_range, changed, rejected, no_room, differs, local | `volume_returned{class}` (never a volume failure) |
| Runner fails to start | `spawn_failed` + `exit`, session over |
| Sidecar cannot be read/sent | event downgraded to `volume_failed` |
| Cancel | session runner killed, nothing reported for cancelled claims |

### 9.2 What the library does with `volume_returned` (`watcher.py:5257-5400`)
Judged from the library's own file, in order:
1. worker stopping -> released unrecorded.
2. archive gone (`stat` FileNotFound) -> released ("the archive is gone"), nothing counted.
3. archive size != `volume.archive_size` sent -> released with `retry_this_scan` ("changed after it was sent"), nothing counted.
4. class `stalled` or `differs`: library reads its **own copy sequentially** (<=60 s, `read_own_copy`, `watcher.py:189-218`); if that fails (bad sector/500) -> **recorded as a failure** with `"the library cannot read its own copy of this archive: <os error> at byte N"`.
5. Otherwise unrecorded: no attempt, no backoff, no strike, but counted:
   * `_note_download_return`: per processor registration `consecutive += 1` (not for class `changed`); 3 in a row opens the **breaker**: that processor's slots are held `hold` seconds (600, doubling per re-open to max 3600) (`watcher.py:5207-5255, 5340-5380`), shown on the card (`note_breaker`). The job counts only when this registration has *delivered an archive before* (`breaker.proven`) and it had no return pending (evidence about the job, not the path).
   * Per job, `DOWNLOAD_RETURN_LIMIT=3` counted returns (not `no_room`) -> recorded `"download failed on N tries (<machines>): <class>: <error>"`. A different file stamp (size/mtime) resets the count.
   * `_returned_by[job] += processor_id`: that processor will not be offered the job again this scan.
   * released with `retry_this_scan`.
A `fetch ready` closes the breaker, marks the registration proven, clears job returns, and logs `"<label> fetched an archive again; no longer held"` if it had been open. A new registration starts with no breaker.

### 9.3 Disconnect of the processor (library perspective)
`drop()` -> every claim returned unrecorded **at once** and re-offered in the same scan; sessions ended; a late `volume_done` ignored; claims being installed are left to finish. No failure record, no backoff, no strike. Shutdown of the library: `drop_all` after the worker stops, so `cancel`/`close_session` ops are queued ahead of the sentinel.

---

## 10. Processor CLI and configuration

### 10.1 Commands (`processor/cli.py`)
The CLI group is `mokuro-bunko processor <cmd>`; in Rust either keep the group or ship a separate binary with the same subcommands (Q1). Common option: `--config PATH` (required, env `MOKURO_PROCESSOR_CONFIG`) for serve/install/service/status.

- **`serve`** `--config`, `-v/--verbose`: 8.1. Verbose = debug logging and logs every op. KEEP.
- **`install`** `--config`, `--force`, `--engines a,b`, `--detector <id>`: installs Python OCR environments. **DROP** (Rust binary ships the OCR; if models need fetching, that is an OCR-spec "model download" command).
- **`service`** `--config`, `--install`: without `--install` prints the unit (Linux) or Startup file (Windows, prefixed by a `# <path>` comment line); with it, installs and starts. KEEP (10.4).
- **`setup`** `--config` (default `processor.yaml`), `--url`, `--username`, `--password-stdin`, `--name`, `--backend` (DROP), `--tls-verify`, `-y/--yes`, `--no-install` (DROP/rename), `--no-service`, `--force`: wizard, 10.3. KEEP minus install/backends. There is intentionally no `--password` flag (shell history).
- **`status`** `--config`: prints `describe(read_status(storage))`. KEEP.

Status file `<storage>/processor-status.json` (atomic write via `.tmp`, errors swallowed): `{updated_at, state: "connected"|"unreachable"|"refused"|"disconnected"|"stopped", library, [name], [error], [sessions]}`. `describe`: empty -> `never connected`; else `"<state> to <library>"` + `" - N session(s)"` if `sessions`. (`processor/status.py`). Note: `sessions` is never actually written by serve in 0.5.2 (only the reader handles it).

### 10.2 `processor.yaml` (`processor/config.py`, `docs/processor.example.yaml`)
Sections `library`, `processor`, `ocr` only; unknown section/key -> error `"<key>: no such section"` / `"<section>.<key>: no such setting (expected one of ...)"`.
- `library.url` (required, trailing `/` stripped; a path prefix is kept and prepended to every request path: `root = urlsplit(url).path.rstrip("/")`), `library.username`, `library.password` (required, non-blank), `library.tls_verify` (`true` | `false` | path to a CA/cert file; else error).
- `processor.name` (default hostname; used as profile key, 64-char clean), `processor.public_name` (str, <=64 after strip, empty -> None), `processor.max_sessions` (int >= 1, default 1), `processor.storage` (default `$XDG_DATA_HOME|~/.local/share` + `/mokuro-bunko-processor`; Windows `%LOCALAPPDATA%` (fallback `~/AppData/Local`) + `\mokuro-bunko-processor`; `~` expanded), `processor.archive_memory_mb` (int >= 0, not bool, default 2048; 0 = disk only).
- `ocr.backend` (`auto|cuda|rocm|cpu`) -> **DROP** (Rust: maybe `ocr.device`; owned by OCR spec).
Passwords live in this file: mode 600 (Windows: `icacls /inheritance:r /grant:r <user>:F`), written atomically (`setup.py:455-500`).

### 10.3 `processor setup` wizard (`processor/setup.py:520-644`)
1. If config exists and no `--force`: with `--yes` error `<path> already exists; pass --force to overwrite it`, interactive asks (default no -> "Nothing written.").
2. Gather URL/username/password: from flags or prompts (`--yes` with a missing one -> `<flag> is required with --yes`; password only via `--password-stdin` first stdin line, else hidden prompt). `normalize_url`: bare host -> `http://` + note "No scheme given, so using <url> (type https://... if the library uses TLS)"; scheme must be http/https, valid port, host present, no query/fragment, trailing slash stripped.
3. `verify_account` (`setup.py:218-285`) **never registers** (a registration would add a phantom to the admin panel):
   - Build the same client; `GET /login/api/me`: 3xx -> error "redirects to <target>: run setup again with that URL"; 401 -> bad username/password message (+ `admin set-password` hint); 429 -> refusing logins for now; non-200 or JSON without `authenticated` -> "does not look like a mokuro-bunko library"; `authenticated: false` -> "did not see the login ... proxy may be dropping Authorization"; `role != "processor"` -> "<acct> is a <role> account, not a processor account ... admin change-role <acct> processor".
   - `POST /_processor/register {"protocol":0}`: 404 -> "has no remote processors ... update the library"; expect 400 with `protocols` list and optional `version`; if our protocol not in `protocols` -> hard error naming which side to update (all-newer => update this machine, else update the library; "both must run the same release"); if lib version differs from ours -> a *note* (works, update when possible); any other answer -> note "Could not check the library's processor protocol".
   - TLS: `SSLCertVerificationError` -> message about `--tls-verify <cert.pem>`/`false`; other SSL error -> http:// hint; connect/HTTP errors -> "Could not reach the library at <url>: ...". Timeout 15 s.
4. Detect hardware (DROP/informational), write config (only non-default settings: `library` always; `tls_verify` if not true; `processor.name` if != hostname; `ocr.backend` if != auto), with header comment, mode 600, loaded back through `load_processor_config` before the atomic `replace`. Temp file `.<name>.XXXX.tmp` beside the target, fsynced.
5. Install environments (DROP) and then the service step (10.4), each with a Y/n prompt (`--yes` = yes), failures are collected and shown (`failed` => final `SetupError`, exit non-zero), summary block `Config/Installed/Running/Logs`; when no service is started it prints the `serve` command (and on Windows the `service --install` command) and the `status` command.

### 10.4 Service installation (`processor/service.py`)
- **Linux**: *systemd user unit* `~/.config/systemd/user/mokuro-bunko-processor.service` (`$XDG_CONFIG_HOME` respected). Unit text: `[Unit] Description=Mokuro Bunko OCR processor; Documentation=https://github.com/Gnathonic/mokuro-bunko/blob/main/docs/deployment.md#remote-ocr-processors; Wants/After=network-online.target`, `[Service] Type=simple; Environment=...(carried vars - DROP); ExecStart=<abs entry point> processor serve --config <abs config>; Restart=on-failure; RestartSec=10s; TimeoutStopSec=60s`, `[Install] WantedBy=default.target`. Quoting rules: `%`->`%%`, `$`->`$$`, line breaks refused (`ServiceError`), words with whitespace/quotes double-quoted with backslash escapes. Install: write file, `systemctl --user daemon-reload`, `systemctl --user enable --now <unit>`; nonzero -> `ServiceError("`<cmd>` failed: <stderr>")`. Then `loginctl show-user <user> -p Linger` to tell the user whether the service survives logout; message `loginctl enable-linger <user>` if not. Prints `journalctl --user -u mokuro-bunko-processor.service -f`. Supported by the wizard only when `sys.platform` is linux + `systemctl` on PATH + `/run/systemd/system` is a dir + entry point found (`setup.py` `user_service_supported`). SIGTERM stops it cleanly; claims go back unrecorded.
- **Windows**: no service; a `mokuro-bunko-processor.cmd` in `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup` (fallback `~\AppData\Roaming\...`), CRLF, `@echo off` / `rem` comment / `start "mokuro-bunko processor" /min "<exe>" processor serve --config "<abs config>"`; `%`->`%%`, `"`->`""`; written as bytes and started immediately as a detached process (`CREATE_NEW_PROCESS_GROUP|DETACHED_PROCESS`, `cmd /c <file>`). No admin needed. Stop: delete the file / Ctrl+C in its window. Message: "Added <file>: the processor starts, minimized, at every logon. Started it now in its own window."
- Also documented but outside the Python package: `deploy/mokuro-bunko-processor.service`, a system unit for a dedicated `mokuro` user at boot (`docs/deployment.md:530+`). The same ExecStart semantic (`processor serve --config`).
- macOS: not supported by `service`; `serve` works anywhere.
- Reverse-proxy requirements for `/_processor/` (docs/deployment.md:112-124, 163-170): no request buffering, no response buffering, no body limit, read/send timeout >= 300 s (nginx `proxy_request_buffering off; proxy_buffering off; client_max_body_size 0`; Caddy `flush_interval -1`). The Rust server must document the same.

---

## 11. "Each volume goes to whichever machine will finish it first" (earliest-finish scheduling)
Not in `ocr/remote/` proper but it is the reason the remote protocol exists; documented in `docs/ocr-internals.md:669-710`, implemented in `ocr/eta.py:1310-1425` and `ocr/watcher.py:2715-3080`.

- **Lanes**: every slot of every machine (local `ocr.concurrency` + each connected processor's `max_sessions`) is a lane `EftLane{key, machine, free_in, warm_row, rows}`; `free_in` = sum of ETAs of its in-flight claims (from the live progress card, else page-count/rate); `warm` = the row of its open (not closing) session; `rows` = rows it may be GIVEN (catalog gate, benchmark done, no start backoff, not stopped this scan, no open breaker, not held). Lanes with a held machine, gone processor, stopped loop or open breaker are excluded. (`watcher.py:2980-3060`.) Fewer than two lanes -> no walk.
- **Walk**: when a lane asks, take the pending list in normal queue order (<=256 volumes) and assign each volume in turn to the lane with the earliest *finish* = `start + volume_seconds(pages)` where `start = free_in` (already including volumes the walk gave it earlier) `+ startup_seconds` unless warm on that row. Update that lane's `(free_in, warm)` after each assignment (list scheduling). The asking lane **keeps** a volume if `mine_at <= best_other + min(best_other * 0.10, 5 s) + 2 s`. The walk stops at the first volume given to the asking lane, which takes it.
- **Pages**: the volume's known page count (from the metadata cache); unknown -> median of the known ones; if none known the walk cannot be priced.
- **Pricing**: per (row, machine) `RateEstimate` (pages/s + latency) from: current-session volumes (EWMA, newest = half), recent runs of earlier sessions, saved benchmark; a volume in flight blends in as it proves itself. Rates are pages over the time between page *emissions* (never model load / pipeline fill); startup is charged separately once per session (from `ready.startup_seconds`). A volume read while the host was busy (CPU pressure >= 60%, or other processes >= 50% CPU) is not learned ("host busy"; `watcher.py:5400-5425`). Rate keys: `<row>` for local, `<row>@<processor name>` for a processor (so a processor's numbers never move another machine's, `watcher.py:3595-3608`).
- **If any lane that may take a row has no rate for it, or no page count is known at all: no walk (return None) -> plain first-come**: the first free lane takes the next volume.
- **Left volumes**: the volumes before the asking lane's own that it could run but the walk gave to another lane are *skipped by this lane* until a deadline = `now + max(starts, 0) + 15 s` for the target lane: if that lane is busy first, the deadline *follows the current prediction* (monotone max); if it is idle now, the deadline is fixed when first set (min with later predictions, never pushed back). Past the deadline anyone may take it ("a volume never waits on a machine that is not coming"). Idle lanes are woken (`notify_all`) only the first time a volume is left. Deadlines for jobs no longer pending are pruned. `MOKURO_EFT_TRACE=1` logs each decision.
- **Warm-session rule**: a session tops itself up only with its own row (`claim_for_session(slot, generation_id)`); it still participates in the walk (priced with no startup) so a slow card's lookahead never holds a volume a fast idle card would finish earlier; when the walk gives it another row's volume it drains and switches rows. A slot starting a *new* session is offered rows in plain queue order (not preferring a row another slot has open) (`ocr/remote/scheduler.py:7-14`).
- **Pre-emption**: a session closes (drains) when an earlier-ranked row gains work that *this slot could run* (`offered(row)`); otherwise a processor that cannot run the earlier row would reopen in a loop.
- Claim exclusivity, queue order, round robin within a row, missing-pages skip are the existing queue's (outside this spec). A claim is `_attempted` in the scan; processors that returned a job are excluded from re-claiming it this scan.
The queue page's finishing times use the same model simulated over every lane (`eta.py`).

---

## 12. Profile store (`ocr/remote/profiles.py`)
`<storage>/processors/<file>.json` per stored processor name; `@local.json` for this server's own hardware. Used by scheduling (pools, benchmark validity, rate evidence) and name ownership.
- File name: if name matches `[a-z0-9][a-z0-9._-]*` and <=64 chars -> `<name>.json`; else `<sanitised readable part (<=40, [^a-zA-Z0-9._-]+ -> _, strip "._", default "processor")>~<first 16 hex of sha256(name utf-8)>.json` (`profile_filename`, `profiles.py:107-133`). `LOCAL_PROFILE = " local"` (leading space cannot occur in a stored name) -> `@local.json`. **KEEP exactly if you want to read existing profiles; CHANGE freely if the Rust server migrates** (Q5).
- Content: `{"name", "account", "host", "catalog", "rows": {<generation id>: {"recipe":[output-affecting fields], "pools":{stage_workers,queue_capacity,stage_device}, "pools_autobench":{}, "bench":{...,"at","precision","precision_mode","precision_trials","host":{devices:{engine}}}, "runs":{volumes,pages,seconds,pages_per_second,recent:[{pages,seconds,at}] (<=RECENT_VOLUMES),last_at,congestion:[<=5],contended,contended_last_at}}}}`.
- Writes: one **process-wide** lock for every instance; read-modify-write; stamped with `name`; stale benchmarks pending removal are dropped on the next save; atomic via `<file>.tmp` + replace; failures logged not raised.
- `claim(name, account)`: owner = first registrant account; a different account claiming -> False (409 at register). No owner yet -> claimed. Single locked read-and-write.
- `set_identity(name, host, catalog)` at every register. `set_pools` (a person's save clears `pools_autobench`; `keep_existing` + `holds_pools` rule: an entry whose three tables are all empty is "no opinion"; pools never carry a precision), `set_bench`, `record_run` (contended volumes only counted, not folded in; pages<=0 or seconds<=0 ignored; maintains cumulative `pages_per_second`, last `RECENT_VOLUMES` volumes), `prune(generation_ids)` drops rows for deleted generations in every profile and local.
- `row(name, gen_id, recipe, mode, supported)` returns None if the stored recipe differs from the current one (a changed engine/detector/budget = a different row, measured afresh); a stored benchmark that no longer describes the row's precision mode on this machine's formats reads as absent/`stale_bench` and is dropped at next save (precision-mode staleness rules in `stale_bench_reason`, `profiles.py:218-290`: **DROP if the Rust port has no precision modes**).
- `machine_pools(stored, own)`: table-by-table; a stored non-empty table replaces the row's own table wholesale (missing key = derived there); an empty stored table says nothing. `runner_pools`: strips `auto` widths/capacities (absent = derived). A stored pin to a device the processor no longer reports is ignored for the row's own table (logged once). Pin to an ONNX GPU provider where none exists is rewritten to cpu (**DROP ort part**).
- `names()` reads each file's `name`, listing only files whose digest filename matches that name.

---

## 13. Admin-visible surface (just so nothing is lost)
`ProcessorEntry.to_dict()` (`registry.py:316-335`) is what the admin API/Processors card reads: `processor_id, name, label, username, host, catalog, max_sessions, sessions, connected_since, last_seen, installing, local, public_name, transfer`. `TransferStats.to_dict()` (`registry.py:132-149`): `volumes` (last 20 deliveries), `mb_per_s` (sum bytes/sum seconds, 1 dp), `resumed` (requests>1), `restarted`, `repaired`, `damaged`, `returned`, `returned_by_class` (<=16 distinct classes, overflow bucket `"other"`), `last_returned{class,error[:300],at}`, `held_until`, `held_error`. Also `registry.entries()` (local first, then connected sorted by lowercase name), `connected()`, `failures()`, `last_disconnect()`. The queue page shows visitors `public_name` / "machine N" and never the name (`queue/shape.py`; other spec). `processing_hold()` (`watcher.py:4350-4375`): no local processing and no connected processor -> `{"reason":"no-processor","since":<last disconnect time or worker start>, "last":{name,disconnected_at}}`.

---

## 14. Suggested wire cleanups for Rust (semantics unchanged)
1. Keep HTTP/1.1 chunked long-poll *or* move both channels onto one HTTP/2 or WebSocket connection per processor. Semantic requirement either way: (a) library->proc ordered ops with heartbeats, (b) proc->library ordered frames per session with a reply/refusal code, (c) the sender can learn of an early refusal while still streaming, (d) liveness detectable in <=35 s (proc) / <=30 s (lib).
2. Fixed endpoint paths; drop the `{sid}` template and the advertised `session_stream`/`events` URLs. Keep `processor_id`, `protocol`, `archives`.
3. Keep the `code` enum (`not_owner, unknown, dropped, body_open, session_ended, bad_frame`) with prose; add a typed `action` hint so clients need not hard-code the code->action table.
4. `volume.archive` should be percent-encoded by the lib (today the processor re-encodes the raw path with `quote(safe="/")`).
5. `ping` and `fetch` progress can collapse into one `keepalive{progress?}` frame; keep "latest-wins per claim" coalescing so progress never delays a sidecar and vice versa.
6. Token: always a bearer token; drop the Basic fallback.
7. Replace the 10 s cheroot idle-timeout/3 s ping reasoning with a server-side per-connection idle timeout of 30 s (EVENTS_SILENCE) and keep `ping` every 3 s (cheap, detects half-open sockets in one tick).
8. Version probe: keep `protocol` as an integer; either keep the setup wizard's `protocol: 0` register probe or add `GET /_processor/info` returning `{protocols, version}` (the lib's bad-protocol reply already contains both).
9. Keep the strict equality of `PROTOCOL_VERSION`; the Rust pair starts at its own number (not 2) so an old Python processor is refused at register.

---

## 15. Tests that pin this behaviour (port as acceptance tests)
`tests/unit` and `tests/integration` (listed by name, not read line by line for this spec; behaviours above come from the source): `test_remote_protocol.py`, `test_remote_registry.py`, `test_remote_stream.py`, `test_remote_events.py`, `test_remote_returns.py`, `test_remote_revocation.py`, `test_remote_profiles.py`, `test_remote_pools.py`, `test_remote_scheduler.py`, `test_remote_machines.py`, `test_remote_bench.py`, `test_remote_processor.py`, `test_remote_processor_mount.py`, `test_proxy_processor_channels.py`, `test_run_server_processors.py`, `test_eft_assign.py`, `test_eft_claim.py`, `test_processor_{archives,bridge,client,close,docs,name_ownership,op_ids,pinning,role,service,setup,speed,token,windows_start}.py`.

---

## 16. Open questions

Q1. Single binary vs two (`mokuro-bunko processor ...` subcommands vs a separate `bunko-processor`)? Affects `service --install` entry-point discovery (`service.entry_point()` looks beside `sys.executable`) and the unit's `ExecStart`.
Q2. Rust catalog schema: which fields? Proposal: `engines[]`, `detectors[]`, `devices[{id,label}]`, optional `gpus[{index,formats}]`. `serves_mokuro`, `onnxruntime_gpu_providers` are DROP. The row-eligibility rules (`catalog_can_run`, precision `row_refusal`) must be re-derived from whatever the Rust OCR spec calls an engine/recipe; the *structure* (engine installed, detector installed, pinned devices reported, precision runnable) is KEEP.
Q3. Is the "recipe/row spec" sent in `open_session` still pools+precision-pick shaped (per-machine tuning, benchmarks, autobench), or does the Rust OCR have no pools? That decides whether 7.2 (bench gate), 8.7 (benchmarks), 12 (profile `rows.*.pools/bench`) and the `/bench/.../sample` endpoint survive. If dropped, profiles shrink to `{name, account, host, catalog, rows.*.runs}` and `record_run` still feeds EFT rates.
Q4. Rate persistence: EFT uses `RateModel` (`ocr/eta.py`, `ocr/throughput.py`) plus profile `runs.recent`/`bench`. I did not read `RateModel` in depth; the ETA/queue spec must define what survives a library restart, since the profile docstring says "nothing schedules on that mean".
Q5. Name/profile migration: keep reading `<storage>/processors/*.json` in the existing format (so an upgraded library keeps name ownership and benches)? If yes, `profile_filename` and the `account` owner field are a compatibility contract.
Q6. Archive serving contract: processors need, on the normal file GET path, `Content-Length`, `Accept-Ranges`, `Range` (single `bytes=N-`), `If-Range` with a **strong** `ETag` (else the fetcher restarts rather than splices), 206 `Content-Range`, 404 for missing, no `X-Accel-Redirect`. I did not verify the ETag format the Python wsgidav path emits, nor `If-Range` support; the Rust file server (other spec) must emit strong ETags and honour `If-Range`, otherwise every resume degrades to a restart (up to 3) then `no_range`.
Q7. Nginx offload (`MOKURO_NGINX_ACCEL`) kept in Rust? If yes the processor's `X-Accel-Redirect` -> `mismatch` return class stays; if no, DROP it.
Q8. Archive-in-RAM design: Linux `/dev/shm` unnamed files exist to hand a path to the child runner via `/proc/<pid>/fd/<n>`. With an in-process Rust pipeline, is the "spool" still wanted (bounded RAM budget + disk fallback + `no_room` class), or just a buffer/temp file with the same budget semantics? Config key `archive_memory_mb` is user-visible.
Q9. Processor `status` file's `sessions` field is read but never written in 0.5.2. Keep as dead, drop, or populate in Rust?
Q10. `processor_account_stamp` is "checked at every heartbeat" but in code only when the stream loop wakes with >=15 s since the last check, so a constantly busy stream is checked at the first op after 15 s. Equivalent bound; a timer-based check in Rust is simpler. Confirm.
Q11. Keep `MAX_ENTRIES_PER_ACCOUNT=4` and `STALE_REGISTRATION_SECONDS=300` exact? (They exist for reconnect storms from one account.)
Q12. Registry/sessions/tombstones are not persisted; confirm the Rust server keeps them in memory (restart => every processor re-registers; claims were in memory anyway).
Q13. TLS: `tls_verify` accepting a bare certificate file path (Python `cafile=`). Rust equivalents: CA bundle or pinned cert? `false` disables verification and hostname checks. Keep both.
Q14. `volume_returned` with `class = local` covers any exception in the feeder (including bugs); the library treats it as the transfer's fault (unrecorded, counted toward breaker). Keep that catch-all in Rust (panic in the fetch task -> `local`)?
Q15. Windows: `flock` is skipped on Windows (no storage lock), so two processors on one storage are not detected there. Fix in Rust?
Q16. Library clamps `max_sessions` to 16 silently; the processor only validates `>=1`. Surface the clamp in the register reply?
Q17. The `exit` synthetic event carries `returncode: null` for remote sessions; the watcher's `_session_exit_error` path for remote exits was not read in detail (`watcher.py:~5560-5610`). Confirm what error text is recorded when a remote runner's own `exit` carries a nonzero code (the processor currently forwards the runner's `exit` verbatim, including its returncode).
