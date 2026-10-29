# OCR sidecar provenance

The one normative description of what a `.mokuro` says about where its text came from,
shared by Mokuro Bunko and Mokuro Reader. Bunko's code: `bunko_layout::seal`,
`bunko_layout::sidecar`, `bunko-server/src/ocr/upgrade.rs`. Executable form: the
vectors in `interop/contract/seal/*.json`, which both sides run. File names (which file
is a volume's main sidecar, which is a layer) follow sidecar names rule v1
(`interop/contract/sidecar-names.json`, `bunko_core::sidecar_names`).

The rule this serves (decision D4, `0.8-remote-storage.md`): the main `<Vol>.mokuro` is
model output. A person's corrections live in layers whose content says they are not
model output, and nothing Bunko does replaces a person's text without keeping it.

## Blocks

A sidecar may carry, at top level, at most one of `edited` and `ocr_engine`, and
`preserved` only on a keepsake layer. Readers and tools that do not know a block keep it
as they found it; mokuro itself ignores all three.

**`ocr_engine`** — GENERATED. Written by the engine runner and completed by Bunko at
install.

| Key | Type | Meaning |
|---|---|---|
| `id` | string | The engine (`hayai-nova`, `paddle-manga`, `ppocr-manga`; `mokuro` for Bunko 0.5's served mokuro). Required for a recipe. |
| `revision` | integer ≥ 1 | The engine's output revision. Absent reads as 1 (every 0.7 file). |
| `created` | ISO 8601 UTC | When this library installed the file. |
| `detector`, `patch_budget`, `precision`, … | | The recipe details the runner records. |
| `generator` | string | `mokuro-bunko <version>` (layers). |
| `generation` | string | The generation row that wrote it (layers, and the edited-main layer below). |
| `seal` | `"v1:"` + 64 lowercase hex | The content seal (below). |

`content_sha256` (an earlier byte seal that never shipped) is not a seal; sealing
removes it.

**`edited`** — EDITED. Written by a reader that saved a person's corrections:
`{"by": "mokuro-reader", "at": <ISO>, "base_sha256": <hex>|null, "base_seal": <seal>|null}`,
the base being the file the edit started from. A file with `edited` carries no
`ocr_engine`; Bunko still reads one left in as part of the edited file, and ignores it.

**`preserved`** — PRESERVED. Bunko's keepsake of a main sidecar it replaced, as the layer
`<Vol>.pre-upgrade.mokuro` (`-2`, `-3`… when taken):
`{"by": "mokuro-bunko", "reason": "generation-upgrade", "replaced_at": <ISO>, "sha256": <the old file's bytes>, "replaced_by": <new ocr_engine.id>}`,
and no `ocr_engine`. Only Bunko writes it.

## Precedence

Decided in this order, first match wins:

1. an `edited` object: **EDITED** (a person's text);
2. a `preserved` object: **PRESERVED** (nobody's recipe; never read as one);
3. an `ocr_engine` object with a string `id`: **GENERATED**(id, revision, sealed or not);
4. a top-level non-empty `version` string: **LEGACY** (stock mokuro output, family
   `mokuro-legacy`);
5. anything else: **UNKNOWN** (never upgraded).

An `ocr_engine` stamp only names a recipe. It proves nothing about who wrote the file:
anyone who can write the file can add one, and a tool that re-saves an edited file can
keep one. The proof is the seal.

## Seal v1

`ocr_engine.seal` = `"v1:"` + hex(sha256(JCS(`{"ocr_engine": B, "pages": P}`))), where
`B` is the `ocr_engine` object without `seal` and `content_sha256`, `P` is the `pages`
array, and JCS is RFC 8785 (keys sorted by UTF-16 code units, ECMAScript number
spelling, so `24.0` and `24` are one number, no whitespace). A file is sealed when its
`seal` is well formed and equal to the one its content gives. A leading UTF-8 BOM is not
content. A file with no `ocr_engine` object, no `pages` array, or a NaN or infinity in
either has no seal.

Outside the seal: `title`, `volume`, `title_uuid`, `volume_uuid`, `chars`,
`spine_width`, `version`, an `edited` block, key order and spacing. So a rename, a
re-serialisation by any JSON library, a move to another machine and a promotion of a
layer to the main sidecar keep it; an edit of any page or of the block breaks it.

Bunko seals every result it installs, after normalisation has stamped `generation` and
`created` into the block, and seals again whenever it changes the block. The threat
model is accidents, chiefly a person's corrections being taken for model output, not
forgery: the algorithm is public, and whoever can rewrite a file can replace it anyway.

Worked example. Bunko installs:

```json
{"version":"0.2.5","title":"S","volume":"Vol 1","volume_uuid":"vu","ocr_engine":{"id":"hayai-nova","revision":1,"detector":"ppocr-manga","patch_budget":512,"created":"2026-10-29T00:00:00Z"},"pages":[{"img_path":"001.jpg","img_width":1200,"img_height":1700,"blocks":[{"box":[812,104,880,402],"vertical":true,"font_size":31.5,"lines":["橋を渡ろう"]}]}]}
```

The seal input is

```json
{"ocr_engine":{"created":"2026-10-29T00:00:00Z","detector":"ppocr-manga","id":"hayai-nova","patch_budget":512,"revision":1},"pages":[{"blocks":[{"box":[812,104,880,402],"font_size":31.5,"lines":["橋を渡ろう"],"vertical":true}],"img_height":1700,"img_path":"001.jpg","img_width":1200}]}
```

and the block gains `"seal":"v1:562a1da2b4921aad4f7660526536c7b303cb057edd096356e74d704a47f54bd5"`.
A reader renaming the volume to `Vol 1 (new)` and saving the file with two-space
indentation leaves that seal valid; changing `橋を渡ろう` breaks it.

## Revisions

`bunko_layout::sidecar::ENGINE_REVISIONS` holds each engine's current revision (all 1
today). Raise an engine's number only when its output changes for the same input: new
weights or a new export, a detector or layout change that moves boxes or text. Do not
raise it for a speed-up, a packaging change or a runner fix that leaves output
identical. A raise means, with upgrades on, every older sealed file of that engine is
OCR'd again, and processors announcing an older revision (`Catalog.engine_revisions`;
a 0.7 processor counts as 1) are offered none of that engine's work.

## What Bunko does with it

All of this applies only with `ocr.upgrade.enabled` (default off), except the `_n` sweep.

- **Outdated main.** A GENERATED or LEGACY main is outdated when its recipe is the
  primary's own at a lower revision, or its family is listed in `ocr.upgrade.replace`.
  A sealed one is replaced with no copy; any other is first kept as `pre-upgrade`.
  UNKNOWN and PRESERVED are never outdated.
- **EDITED main.** Never replaced, preserved or swapped. When the recipe beneath the edit
  is outdated, the new OCR is written beside it as the layer
  `<Vol>.<primary generation name>.mokuro` (`ocr_engine.generation` = that name,
  sealed), while that name is free or holds Bunko's own sealed output of an older
  revision; a file of anyone else's at that name stays and nothing is owed. Once the
  `edited` block is gone, the ordinary census applies, and that layer is what it
  swaps in. A reader's EDITED file carries no `ocr_engine` (see above) and its PUT
  forgets the provenance row, so the recipe beneath reads as LEGACY from its `version`
  (UNKNOWN without one), whatever the edit started from: under the default `replace`
  list every EDITED main is owed one run of the primary, once.
- **Layers.** A layer is swapped in as the main only when it is sealed, has no `edited`
  block and its own block names the current recipe. A generation layer Bunko wrote at
  an older revision is OCR'd again and replaced in place.
- **Occupied destination.** Bunko never writes `<stem>_<n>.mokuro`, on any store. A file
  at a result's destination stays and the result is discarded, unless it is Bunko's own
  sealed output of the same engine at an older revision, which is replaced. Numbered
  files an older Bunko wrote are swept after each OCR walk when they are sealed or
  recorded as Bunko's AND belong to that volume: their `volume` is the archive's stem
  (the collector wrote it; a reader renaming a volume to `Vol_2` uploads a main naming
  `Vol_2`, sealed again, before it moves the archive), and their row, if any, is keyed
  by that archive. They are renamed to the canonical name when it is free, removed when
  it is taken (on a cloud library, to the provider's trash unless the store's ledger
  proves the version Bunko's, and only while the remote index is steady). Any other
  numbered file stays, is counted (`orphans_unproven` in the admin's OCR status), and
  is not read again until its stat changes.
- **An older reader's rewrite.** A WebDAV PUT of a main sidecar with no `ocr_engine`
  and no `edited` whose pages (as JCS) equal a sealed main's — the one at that path, or
  the main of the same `volume_uuid` in the folder — gets that main's block back and is
  sealed again before it is stored. Mokuro Reader 1.10.0 regenerates the main without
  the block when it renames a volume; this keeps the provenance of those renames.

Accepted limits: a page edit by Mokuro Reader 1.10.0 still lands in the main sidecar
(diverting it would break that reader's sync); an upgrade keeps it as `pre-upgrade`
before replacing the main. A 1.10.0 rename into another series folder finds no sealed
main beside it and stays unsealed (kept as `pre-upgrade` before an upgrade). A rename
written straight to a cloud folder, not through Bunko's WebDAV, is not re-sealed.

## Reader carry rule

A reader never mints an `ocr_engine` block. It stores each installed file's block
verbatim together with a v1 digest of that file's pages (`VolumeMetadata.ocr_provenance`,
at most 4 entries), and writes the block back onto a main it regenerates only when the
digest of the pages it writes matches. A file whose pages a person changed is written
with an `edited` block instead.
