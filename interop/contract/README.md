# The reader contract

What Bunko sends Mokuro Reader and what the reader sends Bunko, as golden files (spec
`docs/design/interop-testing.md` §5). Both sides test against these files, so a change
to the contract shows up as a fixture diff in the pull request that makes it.

## Layout

- `seed.json`: the users and the library every fixture is made from. The passwords are
  test values for a throwaway in-process server. `reader` is an uploader (may write
  progress and add files), `viewer` a processor account (the only signed-in role with
  neither right), `admin` an admin. One series and its volume have Japanese names.
- `bunko/<name>.json`: what Bunko answers, written by
  `crates/bunko-server/tests/contract_export.rs` from a seeded full-stack server. The
  reader's unit tests parse these instead of hand-written JSON.
- `reader/<name>.json`: the requests the reader sends, written by the reader's
  `src/lib/interop/reader-requests.test.ts`, each with the response fields and headers
  its code reads. `crates/bunko-server/tests/contract_replay.rs` sends each one to a
  seeded server and checks them.
- `reader/legacy/*.json`: hand-written records for requests older readers sent.

## Fixture format

```json
{
  "request": {"method": "GET", "path": "/login/api/me", "auth": "basic:reader", "headers": {}},
  "status": 200,
  "headers": {"content-type": "application/json"},
  "body": {}
}
```

- `auth` is `none`, `basic:<user>`, `bearer:<user>` (a fresh token for that user) or
  `raw:<header value>`. A request body is `body` (JSON; the string `<password:USER>` is
  that seed password), `body_text`, or `body_cbz` (`{"pages": n}`, a generated archive).
- The response body is `body` (JSON), `body_text` (XML or text, `\n` line endings) or
  absent. Only the headers the reader reads, plus `WWW-Authenticate`, `Retry-After`,
  `Vary` and the CORS headers, are kept.
- Files Bunko writes into a library (the cloud queue copy, the lease marker,
  `series.json`, `catalog.json`) use the same shape with `"request": {"file": "<path>"}`.
- Reader records carry `expect` instead of a response: `status` (a list), `body_fields`
  (dot paths that must exist and be non-null, e.g. `permissions.canWriteProgress`),
  `headers` (names that must be present), and optionally `after` (the record whose
  answer's ETag replaces the `"etag"` sentinel in this one's request).

## Sentinels

Anything that changes from run to run is replaced by a value of the same JSON type:

| Kind | Sentinel |
|---|---|
| ISO-8601 time | `"2000-01-01T00:00:00Z"` |
| HTTP date | `"Sat, 01 Jan 2000 00:00:00 GMT"` |
| epoch seconds / milliseconds | `946684800` / `946684800000` |
| ETag (header, XML `getetag`, `If-Match`, `If-None-Match`) | `"\"etag\""` |
| token | `"<token>"` |
| UUID-shaped id | `00000000-0000-4000-8000-00000000000N`, numbered by first appearance in the file |
| other ids (`instance_id`, `boot_id`, ...) | `"<id-N>"` |
| Bunko version / machine name | `"0.0.0"` / `"contract-host"` |
| a wait counted down (`Retry-After`, `Retry in Ns`) | rounded up to whole minutes |

Archives are generated with fixed timestamps, so sizes and SHA-256 digests stay real.
The reader's loader maps the sentinel times onto a `now` of its choosing, so freshness
checks (a lease marker, a queue file) behave as they would on a live server.

## Accepting a change

```sh
BUNKO_BLESS=1 cargo test -p bunko-server --test contract_export   # Bunko's answers
BUNKO_REPO=<this checkout> READER_BLESS=1 npx vitest run src/lib/interop   # in the reader
```

Then read the diff: it is the contract change, and the other side's tests show whether
it breaks them.
