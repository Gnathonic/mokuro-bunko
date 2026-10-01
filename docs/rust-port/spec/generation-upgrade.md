# mokuro/ctd deprecation and generation upgrade — design

**Date:** 2026-09-29 · **Status:** draft for review · **Part 2 of** `2026-09-29-0.6-roadmap.md`

## The problem

Most OCR in a real library is the bare `<Volume>.mokuro` that mokuro wrote.
That is manga-ocr plus comic-text-detector (ctd). ctd is GPL-3.0, and both
read worse than the engines 0.5 added. Changing the primary generation today
does nothing for those volumes. A job is "pending" only when its file is
missing, completed sidecars are never overwritten (`_get_unique_path` picks
`_1` instead), and so the old OCR stays the reader's default forever. The only
way to re-OCR a volume is to delete its sidecar by hand.

0.6 does two things:

1. Deprecates mokuro and ctd.
2. Adds a **generation upgrade**. The volume's bare `.mokuro` is brought up to
   the current primary recipe. There are two ways to do it:
   - **Direct replace:** when a matching file is already on disk, it is swapped
     in.
   - **Generate then swap:** otherwise the file is generated, then swapped in.

   Either way, the upgrade runs only when the archive holds every page the old
   file names.

## 1. Deprecation (soft; removal in 0.7)

**Registry.** The `mokuro` engine and the `ctd` detector each gain
`deprecated: "<one-line reason>, removed in 0.7"` in `ocr/engines.py`.

**Admin panel.**
- They are left out of the "add row" catalog (`_generations_catalog`).
- An existing row shows a *Deprecated — removed in 0.7* badge, plus a link to
  the upgrade section.
- Editing an existing row still works.

**Configuration.**
- A config that names them still loads.
- The server logs one warning per deprecated row at start.
- `doctor` lists each deprecated row as a warning, not an error.

**New installs** no longer default to mokuro. `config.example.yaml`, the
`setup` wizard, the Unraid template and the docs all default to one primary
row:

```yaml
ocr:
  generations:
    - {name: hayai-nova, engine: hayai-nova, detector: ppocr-manga, primary: true}
```

Both models are Apache-2.0. The row runs on CPU, and after part 1 lands it is
2× faster on CPU than torch.

**A config with no `ocr.generations` key keeps today's implicit mokuro
primary in 0.6.** Changing that fallback would silently change what an
upgraded server does. Instead, it logs the deprecation along with the exact
line to add. In 0.7 the fallback becomes the row above.

**The mokuro environment is built only while an enabled row needs it.** It
stays on torch. Its dependencies cannot run free-threaded (see part 1), so it
is created from a regular, non-free-threaded CPython 3.14.

## 2. What a file's recipe is

An upgrade compares recipes. A sidecar's **recipe** is:

- engine id
- effective detector
- `patch_budget`, if the engine uses one
- the model weight pins

The first three are what `GenerationSpec.output_affecting()` already returns.
Precision and pools are left out, as they are there.

| The file | Its recipe comes from |
|---|---|
| Written by this server (0.5+) | its `ocr_sidecars` row: engine, detector, `runner_build`. Weight pins come from its `ocr_engine.weights` block. |
| A composed engine's file with no row | the `ocr_engine` block: `recognizer`/`id`, `detector`, `weights`. |
| No `ocr_engine` block, but mokuro's top-level `version` | **`mokuro-legacy`**, with detector `ctd`. This covers served mokuro in 0.5 (`{"id": "mokuro"}` only) and every pre-0.5 file. |
| Anything else | **unknown**, never upgraded automatically. |

The **target recipe** is the enabled primary row's `output_affecting()`, plus
the weight pins the runner reports for it (`REPO_REVISIONS` and the detector
pins).

## 3. When a volume is upgraded

A volume is an **upgrade candidate** when all of the following hold:

1. **Policy.** `ocr.upgrade.enabled` is true, and the bare file's recipe
   *family* is in `ocr.upgrade.replace`. The family is the engine id, or
   `mokuro-legacy`. Listing the primary's own engine means "re-run when the
   pins change".
2. **Different recipe.** The bare file's recipe differs from the target.
3. **Whole archive.** `pages_short(cbz) == 0`. This is the existing
   missing-pages cross-check, so the old file names no page the archive lacks.
   Regenerating from a short archive would lose OCR for those pages, so such
   volumes are **skipped (missing pages)** until a whole archive replaces
   them. This matches the gate 0.5 already applies to extra layers.
4. **Not edited.** Nothing shows a person edited the bare file. The evidence
   is either of:
   - an audit-log WebDAV write to that sidecar path, by a user account;
   - its `ocr_sidecars` row predating the file's current mtime by more than 2 s.

   Edited files are **skipped (edited)**, and listed for the admin, who can
   force one volume (§6).

```yaml
ocr:
  upgrade:
    enabled: false          # default off: an upgrade rewrites what readers see
    replace: [mokuro-legacy, mokuro]
```

Settings apply live through `OcrControl.apply()`, like the rest of `ocr.*`.

## 4. Direct replace, or generate then swap

For each candidate, in this order:

1. **Direct replace.** The first existing sidecar of that volume is taken if
   it meets all three conditions:
   - its recipe equals the target recipe;
   - it was produced from the current archive, meaning either:
     - its `ocr_sidecars` row's `archive_size`/`archive_mtime_ns` match the
       archive now, or
     - with no row, its page list matches the archive: the same count, and
       every `img_path` present;
   - it is not edited (same test as §3).

   Its bytes become the new bare file. This is the usual case when the new
   engine already ran as a layer: `<Volume>.hayai-nova.mokuro` exists and the
   hayai-nova row was just made primary.
2. **Generate then swap.** Otherwise the volume gets an **upgrade job**, which
   is `(cbz, primary generation id, upgrade=True)` in the existing scheduler.
   Nothing new is stored: like every other job, it is recomputed from the
   filesystem on each scan.
   - **Order.** Upgrade jobs come after every ordinary job of every generation,
     round-robin by series like the rest, and niced like a non-first row. A
     volume with no OCR always beats one with old OCR.
   - **Where it runs.** Any machine that can run the primary row can run it,
     including remote processors. The processor runs a normal job. The
     library decides at install time that this one ends in a swap, from its
     own claim record, so the processor protocol does not change.
   - **What happens to the output.** The runner writes to the workspace as
     always. `install_session_sidecar` routes an upgrade result through the
     swap (§5) instead of `session_sidecar_destination`.
   - **If the archive changed mid-run,** `publish_guard` drops the result, as
     it already does.

## 5. The swap

With the volume's per-path write lock held, and with the new content already
validated and normalized in the workspace:

1. **Keep the old file as a layer.** Copy the old bare file to
   `<Volume>.<old-name>.mokuro` through a temp file and `os.replace`.
   - `<old-name>` is the `generation_name` on its `ocr_sidecars` row, or
     `mokuro` for `mokuro-legacy`. It must pass the layer-name grammar and
     avoid the reserved names.
   - If that path already holds a *different* file, `-prev`, `-prev2`, and so
     on are appended.
   - If the old file has no `ocr_engine` block, it is stamped with one:
     `{"id": <engine>, "generation": <old-name>, "generator": "mokuro-bunko X", "upgraded_from_primary": true}`.
     The reader treats an unstamped layer as a person's edit.
   - An edited file that reaches here through a forced upgrade stays
     unstamped, so the reader shows it as the edit it is.
2. **Install the new file.** `os.replace(new, bare)`, which is atomic.
   - If the new bytes came from a direct replace and that layer's row is
     **disabled or gone**, the layer file is then removed, so it is not kept
     twice.
   - If the row is **enabled**, the layer file stays. Deleting it would make
     that row pending again. The admin panel flags a primary whose recipe
     equals an enabled layer's recipe as redundant.

**Crash safety.**
- A crash between steps 1 and 2 leaves the old bare file and a copy of it.
  There is never a moment with no bare file, so the primary never looks
  pending.
- The next scan still sees an old recipe and redoes the swap. Step 1 is
  idempotent because the content is the same.

**Bookkeeping.**
- `ocr_sidecars`: the old row moves to the layer path, and the new bare file
  gets a row.
- The audit log records `ocr_sidecar_upgraded` with `{from_recipe, to_recipe, kept_as, mode: direct|generated}`.
- The metadata recompile runs from the existing filesystem callback.
- `volume_uuid` is preserved, because `_normalize_mokuro_metadata` stamps the
  volume's own uuid, so reading progress is untouched.
- The manifest's `modified` changes, which is what tells readers to refetch.

## 6. Admin surface

**Census.** On the Generations card, the primary row gets an **Upgrade**
section showing:

- one line per recipe family among the library's bare files, with its count
  (e.g. *mokuro-legacy 1,234 · hayai-nova 40 · unknown 3*);
- checkboxes that write `ocr.upgrade.replace`, and the enable toggle;
- counts for:
  - **ready to swap** — direct replace available;
  - **needs OCR**;
  - **skipped (missing pages)**;
  - **skipped (edited)** — with the volume list.

The census is computed on the watcher's scan and cached against each bare
file's stat, so it does not re-read unchanged files.

**Single-volume actions:**
- `POST /api/ocr/upgrade/<volume>` with `force` overrides the edited check
  only. The missing-pages check is never overridden.
- `POST /api/ocr/upgrade/<volume>/revert` swaps the kept layer back. This is
  the same swap run the other way.

Both are audited.

**Queue page.** Upgrade jobs show as their own kind, and a volume's ETA
counts them.

## 7. Error handling

- An unreadable bare file is recipe **unknown**, and never upgraded.
- A direct-replace candidate that fails validation is ignored, and the volume
  falls back to *generate then swap*.
- A failed upgrade job goes through the normal failure and backoff record,
  keyed `rel@upgrade`. The old file is untouched.
- A lock the swap cannot get, because a WebDAV write is in flight, means the
  volume is retried on the next scan, not waited for.

## 8. Testing

- **Unit:**
  - recipe classification for each of the four sources in §2;
  - the four candidate conditions, each alone;
  - `-prev` naming and reserved-name avoidance;
  - stamping and not-stamping;
  - the redundant-layer rule.
- **Swap:** injected crash points after step 1, and after step 2 before the
  bookkeeping. Each rerun must converge.
- **Watcher:**
  - upgrade jobs order after ordinary ones;
  - a remote processor's result is swapped, not `_1`-suffixed;
  - `publish_guard` still drops a result whose archive changed.
- **End to end:** the ~/Downloads samples with their shipped `.mokuro` files.
  Enable the upgrade, check both the direct and the generated paths, then
  check that revert restores byte-identical files.
- **Deprecation:**
  - the catalog omits mokuro and ctd;
  - the badge shows;
  - the start warning is logged;
  - the implicit fallback still resolves to mokuro;
  - the mokuro env is built only when a row needs it.

## Out of scope

- Upgrading non-primary layers. Disabling the old row already retires a layer.
- Deleting kept layers. Keeping them is what makes an upgrade reversible.
  Space is left to the admin.
- Deciding which recipe reads better. The admin decides, through the primary
  row and `replace`.
