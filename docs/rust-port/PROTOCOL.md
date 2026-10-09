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
   `volume_done{…, sidecar_sha256}`. The name (percent-encoded) must be exactly the
   op's `sidecar_name`, which is the volume's own and may hold characters Windows
   refuses (`? : * " < > |`) or be a device name (`CON.mokuro`). A name with a path
   separator or NUL, or not ending `.mokuro`, is refused with 400; any other name that
   is not the op's, with 409. Neither side uses it as a path: the processor holds the
   file as `result.mokuro` in its claim folder, and the library stores the upload the
   same way. If the upload fails, it sends `volume_failed` with
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
9. **Owner pause** (0.7, additive; docs/rust-port/GUI.md §3). A processor whose owner
   paused it sends `availability{paused, until, reason}` (wire:
   `{"event":"availability","paused":true,"until":"<RFC 3339>|null","reason":"user"|"schedule"|null}`),
   and `availability{paused:false}` when it resumes. `RegisterRequest.availability`
   (optional) carries the same object, so a processor that restarts paused is offered
   nothing from its first moment. The library stops offering a paused machine work
   (no lanes, no benchmarks: queueing one is 409) and shows the pause in
   `/_admin/api/processors` (`pause`). `released{claims:[…]}` hands claims back: with
   `after_volume` the claims not yet in the pipeline, with `now` all of them (the
   processor also abandons its sessions and sends their `exit`). The library requeues
   released claims at once and records nothing for them; a session that ends while its
   machine is paused is not blamed (no strike, no start-failure backoff). While paused a
   processor answers `volume` with `released`, `open_session` with `exit`, and a bench
   with `fatal` + `exit`. Older libraries ignore both events (unknown events are logged
   and dropped), so the claims time out as before.

10. **Version mismatch** (0.7, additive). The library compares the
   version the processor registers with (`host.version`, which every processor
   already sends) with its own, as semver precedence, and when they differ adds
   `version_mismatch` to the `RegisterReply`:
   `{"library_version":"0.7.1","processor_version":"0.7.0","relation":"library_newer"}`
   (`relation`: `library_newer`, `library_older`, or `unknown` when a version does
   not parse). Equal versions: the field is absent. A field in the reply rather than
   an op on the socket because the versions only change across a reconnect (a
   library that updates restarts, so every processor registers again) and an older
   processor ignores an unknown reply field, where an unknown op would be an
   unreadable frame. It is the trigger of a processor's opt-in automatic update
   (`processor.auto_update`): with `library_newer` the processor drains (an
   `availability{paused:true, reason:"update"}`, after-volume semantics, so the
   claims not yet in the pipeline come back as `released` and the running volumes
   finish and upload), installs exactly `library_version`, closes the socket and
   restarts; it re-registers on the new version. With `library_older` it never
   downgrades and only reports. The processor reports progress with the event
   `update_status` (wire: `{"event":"update_status","state":"waiting","version":"0.7.1",
   "message":"…","action":"…"}`; `state`: `waiting`, `installing`, `restarting`,
   `failed`, `blocked`, `off`, `idle`), sent only to a library that sent
   `version_mismatch`. The library keeps the last one per registration and shows it,
   with the mismatch, in `/_admin/api/processors` (`version_mismatch`, `update`).
   Older libraries send no field (nothing happens: a processor compares only to
   report) and log and drop the event.

## Local processing

The full build runs the same `bunko-processor` code in-process. Ops and events travel over
tokio channels. The archive path in `volume` is a local filesystem path, and the
"upload" is the processor writing the sidecar into the volume's workspace. The scheduler
treats the local processor exactly like a remote one named `local`.
