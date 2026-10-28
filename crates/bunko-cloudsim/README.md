# bunko-cloudsim

One HTTP server that plays Google Drive v3, Microsoft Graph (OneDrive) and MEGA for
tests, with a control API for tokens, faults, seeding and accounting. Bunko's driver
tests run on its library form; the reader's provider suite and the interop rig run
against the binary. Nothing here talks to a real cloud. Design: §6 of
`docs/design/interop-testing.md`.

## Running it

```sh
cargo run -p bunko-cloudsim -- --port 0 --seed-dir ./fixture --seed-account default
```

The first line on stdout is `listening on http://HOST:PORT`; after that it logs one line
per request to stderr. It stops on SIGINT or SIGTERM (Ctrl-C on Windows).

| Option | Meaning |
|---|---|
| `--port N` | Listen on port N; `0` (the default) picks a free one. |
| `--host ADDR` | Listen address (default `127.0.0.1`). |
| `--base-url URL` | The address clients reach it at, used in the URLs it hands out (default `http://HOST:PORT`). |
| `--seed-dir DIR` | Seed every provider's account with DIR's files, and create the MEGA account `NAME@cloudsim.test` with password `cloudsim`. |
| `--seed-account NAME` | The account `--seed-dir` fills (default `default`). |

In Rust tests: `let sim = bunko_cloudsim::start().await;`, then `sim.drive_api_base()`,
`sim.graph_api_base()`, `sim.mega_origin()` and `sim.token_url(provider)` for the driver
configs, `sim.drive(account)` / `sim.onedrive(account)` / `sim.mega(account)` for the
state and knobs. Dropping the `CloudSim` fails the test if it recorded a violation.

## Prefixes

| What | Where |
|---|---|
| Drive v3 (`GDriveConfig.api_base`) | `{base}/google` (uploads under `{base}/google/upload/drive/v3/...`) |
| Graph v1.0 (`OneDriveConfig.api_base`) | `{base}/graph/v1.0` |
| OneDrive download, upload-session and copy-monitor URLs | `{base}/graph/{preauth,upload,monitor}/{account hex}/{id}` (no token), on the other loopback name (`localhost` for `127.0.0.1` and back) so a client following the download redirect drops its token as it would for Microsoft's storage hosts |
| MEGA gateway (`ClientConfig.api_origin`, megajs `gateway`) | `{base}/mega/` |
| OAuth refresh (`OAuthClient.token_url`) | `{base}/oauth/google/token`, `{base}/oauth/microsoft/token` |
| Control API | `{base}/_sim/` |

State is per (provider, account). A bearer token picks the account; MEGA picks it by
session id, or by the email of a login. Every response carries permissive CORS.

## Control API

JSON in and out. `account` defaults to `default`; `provider` is `google`, `microsoft` or
`mega`. A bad control call is `400 {"error": ...}`.

| Call | Does |
|---|---|
| `GET /_sim/health` | `{"ok":true,"providers":["google","microsoft","mega"]}` |
| `POST /_sim/token {provider, account}` | A new grant: `{access_token, refresh_token, expires_in}` (Google and Microsoft) |
| `POST /_sim/token/revoke {token}` | Kill the grant: API calls 401, refresh answers `invalid_grant` |
| `POST /_sim/token/expire {token}` | The access token 401s; its refresh token still works |
| `POST /_sim/mega/account {account, email, password}` | Enable password login for a MEGA account |
| `POST /_sim/seed {provider, account, files: [{path, content_base64}]}` | Create folders along each path and put the file (replacing one already there) |
| `GET /_sim/files?provider=&account=` | `[{path, size, sha256}]` of every live file |
| `POST /_sim/fault {provider, account, method?, path_has, status, reason, retry_after?, times?}` | The next `times` (default 1) matching Drive or Graph requests get `status` with the provider's error body for `reason` |
| `POST /_sim/knobs {provider, account, <knob>: value, ...}` | Drive: `page_size`, `reverse_listing`, `sha256`, `keep_revisions`, `revisions_forbidden`, `interrupt_chunk`, `down_after_interrupt`, `session_down`, `quota_limit`. OneDrive: `page_size`, `interrupt_fragment`, `down_after_interrupt`, `session_down`, `send_ctag`, `storage_ctag`, `ignore_range`, `permanent_delete`, `quota_total`, `lost_below`. MEGA: `eagain`, `overquota`, `bandwidth_509`, `garbage_download`, `require_hashcash`, `drop_upload_at`, `lose_cursor`, `kill_session`, `fail_command {command, code}` |
| `GET /_sim/log`, `DELETE /_sim/log` | `[{seq, provider, account, method, path, class, units, bytes_in, bytes_out, status}]`; `class` is `list`, `metadata`, `download`, `upload`, `write`, `delete`, `changes` or `auth`; `units` is 1 per Drive request |
| `GET /_sim/violations`, `DELETE /_sim/violations` | Requests the simulator could not accept (it answered them 4xx); `DELETE` clears them |
| `POST /_sim/reset` | Forget every account, token, log entry and violation |

## Example

```sh
$ bunko-cloudsim --port 0 &
listening on http://127.0.0.1:41234
$ S=http://127.0.0.1:41234
$ curl -s -XPOST $S/_sim/seed -H 'content-type: application/json' \
    -d '{"provider":"google","files":[{"path":"mokuro-reader/A/v1.cbz","content_base64":"aGVsbG8="}]}'
{"seeded":1}
$ T=$(curl -s -XPOST $S/_sim/token -H 'content-type: application/json' \
    -d '{"provider":"google"}' | jq -r .access_token)
$ curl -s -H "authorization: Bearer $T" \
    "$S/google/drive/v3/files?q=name%3D'v1.cbz'" | jq '.files[0].name'
"v1.cbz"
$ curl -s $S/_sim/log | jq '.[0] | {class, units}'
{"class":"list","units":1}
```
