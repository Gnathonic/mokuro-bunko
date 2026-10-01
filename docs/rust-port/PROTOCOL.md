# Processor protocol v3 (0.7)

Types: `crates/bunko-proto/src/lib.rs`. Semantics carried over from 0.5.2:
`spec/remote-processors.md` and `spec/ocr-scheduling.md` §9, §11, §16, §17.

## Why it changed from v2

v2 used an NDJSON assignment stream (a long GET) plus one length-framed chunked POST per
session for events, with the sidecar bytes inline. Those were workarounds for a threaded
WSGI server: cheroot's socket timeouts forced 3 s pings, and every open session cost a
server thread. v3 has:

- **one WebSocket per processor** (`GET /_processor/{pid}/socket`). It carries every op
  and every event of every session, multiplexed by `sid`. WebSocket ping/pong frames do
  the liveness work.
- **result uploads as plain `PUT`s** (`/_processor/{pid}/results/{sid}/{claim}`). They
  stream straight to a temp file, so a 30 MB sidecar never sits in memory or blocks
  control traffic.

Proxies: Caddy forwards WebSockets as-is. The bundled nginx template sets
`Upgrade`/`Connection` and `proxy_read_timeout 3600s` on `/_processor/`.

## Lifecycle

1. The processor trades its password for a bearer token:
   `POST /login/api/token {"kind":"processor","label":name}` (Basic auth). Every later
   request uses `Authorization: Bearer`. On a 401 it gets a new token and retries once; a
   second refusal means the account is gone, and it exits 1.
2. `POST /_processor/register` sends a `RegisterRequest`. A protocol mismatch gets 400
   `ProtocolMismatch` with `protocols: [3]` and `version`; `protocol: 0` is a probe.
   Name rules, reserved names, the 4-entries-per-account limit, the `max_sessions` clamp,
   409 for a name owned by another account, and 413 for an oversized identity are all as
   in v2.
3. The processor opens the socket. A second socket for the same `pid` drops the entry
   (409, register again).
4. The library sends `open_session{sid, generation}`. The processor loads the models and
   answers `ready{sid, …}`, or `fatal` and then `exit`.
5. The library sends `volume{…}` ops, at most 2 outstanding per session. For each one the
   processor fetches the archive (`GET` with `Range`/`If-Range` against the strong
   `ETag`), sending `fetch{state:"downloading"…}` progress and then
   `fetch{state:"ready"}` once the verified archive is in the pipeline. After that it
   sends `volume_started`, `page`…, and `stats`. When the volume is finished it `PUT`s
   the sidecar (headers `x-mokuro-sidecar-name`, `x-mokuro-sha256`) and then sends
   `volume_done{…, sidecar_sha256}`. If the upload fails, it sends `volume_failed` with
   "the finished sidecar could not be sent from the processor".
6. A claim that never reached the pipeline is sent back as `volume_returned{class,…}`
   and never counts as a failure of the volume (see the v2 class table).
7. `cancel{sid, claim}` records nothing for that claim. `close_session{sid}` finishes
   accepted volumes and then sends `exit`. Every session ends with exactly one `exit`.
8. The processor is gone when any of these happens: the socket closes, nothing has
   arrived for `SILENCE_SECONDS`, the account check fails (every 15 s), or it
   re-registers under the same name. All its claims then go back to the queue
   unrecorded, and no row is struck. If the socket closes while a session is still open,
   that is a disconnect, never a runner crash. This keeps v2's `_ended_without_exit`
   rule.

## Local processing

The full build runs the same `bunko-processor` code in-process. Ops and events travel over
tokio channels. The archive path in `volume` is a local filesystem path, and the
"upload" is the processor writing the sidecar into the volume's workspace. The scheduler
treats the local processor exactly like a remote one named `local`.
