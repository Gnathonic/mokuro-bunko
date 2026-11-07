# The reader contract

What Bunko sends Mokuro Reader and what the reader sends Bunko, as golden files (spec
`docs/design/interop-testing.md` §5). Both sides test against these files, so a change
to the contract shows up as a fixture diff in the pull request that makes it.

## Layout

- `seed.json`: the users and the library every fixture is made from. The passwords are
  test values for a throwaway in-process server. `reader` is an uploader (may write
  progress and add files), `viewer` a processor account (the only signed-in role with
  neither right), `admin` an admin, `progress` a `registered` account (progress and
  reading history only; the `history-*` fixtures are its). Volume 01 has OCR, a layer sidecar and a cover;
  Volume 02 has no OCR; one series and its volume have Japanese names. A series'
  `owner` is its recorded series folder owner (0.8): `reader` owns "Contract Series",
  so its writes there are an owner's; the Japanese series has none (shared).
- `bunko/<name>.json`: what Bunko answers, written by
  `crates/bunko-server/tests/contract_export.rs` from a seeded full-stack server. The
  reader's unit tests parse these instead of hand-written JSON.
- `reader/<name>.json`: the requests the reader sends, written by the reader's
  `src/lib/interop/reader-requests.test.ts`, each with the response fields and headers
  its code reads. `crates/bunko-server/tests/contract_replay.rs` sends each one to a
  seeded server and checks them.
- `reader/legacy/*.json`: hand-written records for requests older readers sent.
- `seal/<name>.json`: the content seal on an installed `.mokuro` (`ocr_engine.seal`),
  written by `crates/bunko-layout/tests/seal_vectors.rs`. See "Seal vectors" below.

- `sidecar-names.json`: sidecar names rule 2, what a name listed beside volume
  archives is. Written by hand (it is a rule, not a recording); Bunko's
  `bunko_core::sidecar_names` tests and the reader's `classifyMokuroSidecar` run it.
  See below.
- `fold.json` and `fold-table.json`: fold v2, the one name fold rule 2 decides on, and
  its tables. Generated, never edited: `python3 -I crates/bunko-core/scripts/gen_fold.py`
  writes them with `crates/bunko-core/src/fold_table.rs` (`--check` fails on any
  difference; `--ucd-dir` caches the pinned UCD files). The reader vendors both
  verbatim. See below.

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
  `nullable_fields` (dot paths that must exist, null allowed), `body_values` (`{path:
  value}`, exact), `headers` (names that must be present, and exposed to the reader's
  page) and `header_prefixes` (`{name: prefix}`, used by the legacy records). The replay
  sends each record with the reader's `Origin`, after a preflight when it is not simple. A top-level `after` names the record whose answer's ETag
  replaces the `"etag"` sentinel in this one's request; the two run on one server. A
  `Content-Digest` is recorded as `<digest>`, and the replay sends the digest of the
  body it sends.

## Sentinels

Anything that changes from run to run is replaced by a value of the same JSON type:

| Kind | Sentinel |
|---|---|
| ISO-8601 or SQLite time | `"2000-01-01T00:00:00Z"`, in the same format (`.000`, `+00:00`, `2000-01-01 00:00:00`) |
| HTTP date | `"Sat, 01 Jan 2000 00:00:00 GMT"` |
| epoch seconds / milliseconds, under `*_at`, `*_modified`, `at_ms` keys | `946684800` / `946684800000` |
| ETag (header, XML `getetag`, `If-Match`, `If-None-Match`) | `"\"etag\""` (`W/` kept) |
| `Content-Digest` (reader records) | `"<digest>"` |
| token | `"<token>"` |
| random ids (`instance_id`, `boot_id`, `took_over_from`) | `00000000-0000-4000-8000-00000000000N` for a UUID, else `"<id-N>"`, numbered by first appearance |
| Bunko version (`bunko_version`, `server_version`, and the top-level `version` of a `/login/api/me` body only) / machine name | `"0.0.0"` / `"contract-host"` |
| a wait counted down (`Retry-After`, `Retry in Ns`) | rounded up to whole minutes |
| the lease claim's `about N min left` | `about <n> min left` |

`version` is replaced in `/login/api/me` bodies alone: everywhere else (a `.mokuro`, the
queue file, the marker) it names a file format and is contract. The sentinel is below
0.8.0, so a reader test that derives its history gate from a recorded identity sees it
off: such a test sets the gate itself.

Every other id (a `.mokuro`'s uuids, the uuid Bunko derives for a volume without one) is
contract and stays as written, and objects keep Bunko's key order. Archives and the cover
are generated byte for byte the same, so sizes and SHA-256 digests stay real.
The reader's loader maps the sentinel times onto a `now` of its choosing, so freshness
checks (a lease marker, a queue file) behave as they would on a live server.

## Seal vectors

`ocr_engine.seal` is `"v1:"` + the lowercase hex sha256 of the RFC 8785 (JCS) text of
`{"ocr_engine": <the block without "seal" and "content_sha256">, "pages": <pages>}`:
keys sorted by UTF-16 code units, numbers as JavaScript's `String(n)`, strings as
`JSON.stringify`, no whitespace. So `JSON.stringify` over key-sorted objects matches it.
No `ocr_engine` object, no `pages` array, or a NaN/Infinity in either: no seal. That
includes a number past the double range (`1e400`, a 400-digit integer): `JSON.parse`
reads it as Infinity and `JSON.stringify` would write it as `null`, so a reader checks
for non-finite numbers after parsing and gives no seal (the `overflow` vector). A
document is sealed when its block's `seal` matches `^v1:[0-9a-f]{64}$` and equals the
seal its content gives (a leading UTF-8 BOM is ignored).

Each vector is

```json
{
  "description": "what it shows",
  "document": "<the sidecar's exact text, as a string>",
  "seal_input": "<the JCS text hashed> | null",
  "seal": "v1:<64 hex> | null",
  "is_sealed": true
}
```

`document` is a string so Python's float spellings (`1e-05`, `24.0`, `-0.0`), integers
past 2^53 and the key order reach the other side unchanged. `seal` is what sealing the
document would set, also when its own seal no longer checks (`edited-line`).

## Accepting a change

```sh
BUNKO_BLESS=1 cargo test -p bunko-server --test contract_export   # Bunko's answers
BUNKO_BLESS=1 cargo test -p bunko-layout --test seal_vectors       # the seal vectors
BUNKO_REPO=<this checkout> READER_BLESS=1 npx vitest run src/lib/interop   # in the reader
```

Then read the diff: it is the contract change, and the other side's tests show whether
it breaks them. `READER_BLESS=1` writes only into the checkout `BUNKO_REPO` names.

A change to a request the reader sends lands in two steps: first the re-blessed
`reader/*.json` records are committed here (where `contract_replay` checks them), then the
reader change, whose CI reads this repository at `BUNKO_REF` and would otherwise compare
against the old records. `BUNKO_CONTRACT_DIR=<copy of this folder>` points Bunko's
contract tests at a scratch copy (to check that a deliberate change fails).

## Fold v2 and sidecar names rule 2

### The fold (`fold.json`, `fold-table.json`)

F, fold v2, pinned to UCD 17.0.0, decides every name question on both sides: which
names are one entry, and (rule 2) which files are a volume's.

- An ASCII name folds to its ASCII lowercase.
- Any other name: drop every Default_Ignorable_Code_Point, NFD, map each code point `c`
  to `M(c) = casefold(full_upper(casefold(c)))`, NFC, drop the Default_Ignorable code
  points again. `casefold` is CaseFolding.txt status C and F; `full_upper` is
  UnicodeData's simple uppercase overridden by SpecialCasing.txt's unconditional
  entries (context-free, as Rust's `str::to_uppercase`).

F is idempotent and context-free, and no code point folds to or from `.` or `/`, so the
k-th dot of a name is the k-th dot of its fold: every slice of a name (stem, suffix,
layer id) is cut at the same dot as its fold's.

`fold-table.json` is normative for the reader, which bundles its own NFD/NFC from it
(never `String.prototype.normalize`, whose Unicode version is the engine's):

```json
{"fold": 2, "ucd": "17.0.0", "map": "...", "ignorable": "...", "decomp": "...",
 "ccc": "...", "exclusions": "..."}
```

Every field is a `;`-separated list of entries in lowercase hex, delta-encoded:

| Field | Entry | Meaning |
|---|---|---|
| `map` | `<d>:<o1>[,<o2>...]` | `M(c)` for every scalar `c` it changes. `c` = previous `c` (0 at first) + `d`; each output code point is `c` + `o`, `o` signed (`-20`). |
| `ignorable` | `<g>:<n>` | Default_Ignorable_Code_Point ranges: the range starts `g` after the previous range's end (+1; 0 at first) and is `n` code points long. |
| `decomp` | `<d>:<t1>[,<t2>]` | Canonical decompositions as UnicodeData lists them (one level; apply recursively), Hangul syllables excluded (algorithmic). `c` delta-encoded as in `map`; targets absolute. |
| `ccc` | `<g>:<n>:<k>` | Runs of `n` code points with the non-zero canonical combining class `k`, gaps as in `ignorable`. Every other code point is class 0. |
| `exclusions` | `<g>:<n>` | Full_Composition_Exclusion (CompositionExclusions.txt, singletons and non-starter decompositions), as ranges. The composition pairs are the two-code-point `decomp` entries whose source is not excluded, plus Hangul. |

`fold.json`:

- `fold`, `ucd`; `table_sha256`, the SHA-256 of `fold-table.json`'s bytes.
- `digest`: for every Unicode scalar value `c` in order (U+0000 to U+10FFFF, surrogates
  skipped), F(c), F(`a` + c) and F(c + U+0301), each UTF-8 and LF-terminated, hashed
  together with SHA-256 (`sha256`). Both sides reproduce it.
- `vectors` (`{in, out, why}`), `same` and `different` (`{a, b, why}`: one entry or two),
  and `series_key` (`{in, out, why}`): F, then trimmed, then every run of whitespace one
  space, the whitespace being U+0009-000D, U+001C-001F, U+0020, U+0085, U+00A0,
  U+1680, U+2000-200A, U+2028, U+2029, U+202F, U+205F and U+3000 (Python's
  `str.isspace()` united with JavaScript's `\s`).

A change of the fold or of its UCD is a new rule (rule 3), landed in both repositories
at once.

### Sidecar names (`sidecar-names.json`)

Rule 2 is the old rule v1 asked of F of every listed name. Let f = F(name):

- R1: two names are one entry iff they fold alike.
- R2: the suffix is the first of `.cbz`, `.mokuro.gz`, `.mokuro`, `.webp`, `.nocover`
  that f ends with. Only `.cbz` archives define volume stems (the folds of the listed
  `.cbz` stems); `.cbr`, `.zip` and `.rar` still keep a sweep off their files.
- R3: a `.mokuro[.gz]` whose folded base is a volume stem is that stem's primary.
- R4: a primary reading wins over a layer reading.
- R5: otherwise, when the folded base is `<s>.<id>` (split on its last dot), `<s>` a
  volume stem and `<id>` matching `^[a-z0-9-]{1,32}$`, it is layer `<id>` of `<s>`. The id
  is the folded segment and is reported folded (`V.EDIT.mokuro` is layer `edit`, as
  Mokuro Reader 1.10.0 already reads it); all-digit ids are layers. Otherwise it is an
  orphan. Layer classes are read on folded ids.
- R6: of the listed archives whose folded stem is `<s>`, the one spelled exactly like the
  file's stem, else the only one. In a fold group (two spellings of one fold listed):
  to read or show, the code-point-smallest spelling (never UTF-16 order); to sweep,
  delete or rename, exact spellings only.
- R7: a leading UTF-8 BOM is tolerated on read, never written; hashes cover the raw
  bytes.
- R8: fixed names (`series.json`, `catalog.json`, `volume-data.json`,
  `.mokuro-queue.json`, `.mokuro-bunko.json`, per-user root names) match by F.
- R9: a writer writes `<listed archive stem>.<id>.mokuro`, minted ids lowercase ASCII
  with a letter, onto the listed entry of the same fold instead of a new spelling.

The file:

- `rule`: the rule version, `2`.
- `classify`: `{name, folder, expect, why}`. `folder` is every name listed beside it;
  `expect` is `{"kind": "primary", "stem", "gz"}`, `{"kind": "layer", "stem", "layer",
  "gz"}` or `{"kind": "orphan"}`. `stem` is spelled as in `name`, cut at the same dot as
  its fold; `layer` is folded.
- `layer_class`: `{id, class}`, read on the folded id: `personal` (`edit`, `edit-<n>`),
  `curated` (`curated`), `reserved` (no client writes it on Bunko: `curated-<n>`),
  `other` (everything else).
- `sweep`: `{archive, folder, expect, why}`: the names that go when `archive` is deleted
  (sorted; compare as a set). Besides R6, a sweep never takes a name whose base is the
  stem of any archive of `.cbz`, `.cbr`, `.zip` or `.rar`, so it can leave a file
  `classify` calls a layer.
- `owner`: `{name, folder, display, sweep, why}`: R6 for one volume file. `display` is
  the archive it is read and shown with, `sweep` the archive whose sweep takes it
  (`null`: none).
- `equivalent`: `{crafted, canonical, why}`: two listings whose names are pairwise one
  entry. Each name classifies alike in its own listing (stems compared folded).

## Beyond the fixtures: the rig

These files are layer 1: each side checks them alone, in its own unit tests. Layer 2,
the interop rig (`cargo xtask interop rig`, specs in `interop/e2e/rig/`,
`docs/design/interop-testing.md` §7.1), runs the real reader against a real server: the
reader commit `interop/reader.ref` pins (a pushed commit; a local run passes
`--reader-dir`), built with `--mode interop`. A field rename fails the fixtures first;
a behaviour the fixtures cannot show (a sync, a merge, a lease) fails a rig spec. Both
run on every Bunko PR (§9). A Bunko change the pinned reader cannot pass yet lists the
reader capability it waits for in `interop/pending-reader-caps.txt` until a PR bumps
`reader.ref`.
