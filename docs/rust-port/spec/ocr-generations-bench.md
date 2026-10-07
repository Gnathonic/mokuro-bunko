# Spec: OCR generations, provenance, precision, devices, bench, engine registry, installer, admin OCR API

Source of truth: mokuro-bunko 0.5.2 (Python), tree `src/mokuro_bunko`. Citations are `file:line` relative to that tree (`ocr/...` = `src/mokuro_bunko/ocr/...`). Target: Rust rewrite with in-process ONNX Runtime (`ort`), no Python venvs, no torch.

Owner decision applied throughout: surviving engines are **hayai-nova**, **paddle-manga**, **ppocr-manga**, with the **ppocr-manga** (PP-OCRv6) detector as the only detector. Everything for `mokuro` (manga-ocr), `ctd`, `animetext`, `rtdetr` is marked **DROP**. Where dropping something changes behaviour (e.g. the default generation), it is called out under "Consequences of the DROPs".

Legend: **KEEP** = reproduce the behaviour/wire format exactly. **DROP** = do not port. **ADAPT** = port with a changed mechanism. **DECIDE** = needs an owner call (also listed in Open questions).

---

## 0. Consequences of the DROPs (read first)

- `DEFAULT_GENERATION` is a `mokuro` primary row (`ocr/generations.py:394-400`). With mokuro dropped the unset-config default must be a different row. **DECIDE** (OQ-1). Everything below that says "mokuro alone, primary" is the 0.5.2 behaviour.
- Roads (`ocr/engine_runner.py:2493-2500`, `STAGE_GRAPHS` 2549-2620): of the four roads only two survive.
  - `line` (engine `ppocr-manga` alone): stages `detect` ("detect + CTC read"), `layout` ("layout + dump"). Both CPU pooled.
  - `reconciled` (hayai-nova / paddle-manga reading the ppocr-manga detector's lines): stages `detect` (CPU pooled), `engine` (GPU device-bound), `post` (CPU pooled).
  - `adapter` (ctd/animetext via subprocess adapters): **DROP**. `served` (mokuro serve process; stages `feed`, `mokuro`, `post`): **DROP**. The monolithic "mokuro CLI" pseudo-road (single stage `mokuro`): **DROP**.
- `page_road(engine, detector)` (`engine_runner.py:2880-2893`) therefore reduces to: `ppocr-manga` -> `line`; hayai-nova/paddle-manga -> `reconciled`.
- `MODEL_STAGES = (detect, engine, mokuro)` (`engine_runner.py:2683`) reduces to `(detect, engine)`. `STAGE_MOKURO`, `STAGE_FEED`, `ROAD_SERVED`, `SERVED_ENGINES`, `SERVED_FP16_FLAG`, `MOKURO_SERVE_MODULE`, `ENGINE_FORMATS` (the mokuro fp16/fp32-only restriction) all go.
- Because only one detector remains, the generation `detector` field is vestigial: it can only ever be `ppocr-manga`. Keep accepting/emitting it for config and wire compatibility (see 1.3), but it carries no choice.

---

## 1. Generation recipes (`ocr/generations.py`)

### 1.1 What a generation is

One row of `ocr.generations`: a named recipe `{id, name, primary, enabled, engine, detector?, patch_budget, precision?, pools}` (`generations.py:176-204`). The queue's unit of work is `(volume, generation)`, never `(volume, engine)` (module doc `:1-9`). Two rows may share an engine and differ only in detector; they never share a file name, log, failure record or round-robin turn. With one detector left, two rows sharing an engine will differ only in other fields (patch_budget, precision, pools, name) - still legal.

Config shape (YAML/JSON stored form = `GenerationSpec.to_dict`, `:373-391`), key order as emitted:

```json
{
  "id": "g-1",
  "name": "hayai-nova-ppocr-manga",
  "primary": true,
  "enabled": true,
  "engine": "hayai-nova",
  "detector": "ppocr-manga",
  "patch_budget": 512,
  "precision": "auto-balanced",
  "pools": {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}},
  "precision_pick": "bf16",
  "precision_why": "benchmark: ..."
}
```

Emission rules (`to_dict`):
- `id,name,primary,enabled,engine` always.
- `detector` only when not None (None for engines with their own detector, see 1.3).
- `patch_budget` always (int), even for engines that ignore it.
- `precision` only when it differs from the default `auto-accuracy`.
- `pools` always, with all three sub-maps present, keys sorted, values coerced to int/int/str (`GenerationPools.to_dict` `:167-173`).
- `precision_pick` + `precision_why` only on the transient per-machine copy (never stored in config, `:199-204`). Parse accepts them only when `precision_pick` is one of `bf16|fp16|fp32` (`:738-739`). **KEEP** but low priority: only needed for remote processors (`_remote_row_spec`).

### 1.2 Names, ids, sidecar file naming

- **Sidecar naming (KEEP, load-bearing for reader compat):** primary -> `<Volume>.mokuro`; every other -> `<Volume>.<name>.mokuro` (`sidecar_suffix` `:282-287`). `sidecar_paths(cbz)` returns `(plain, plain + ".gz")` where `plain = cbz.with_suffix('') + suffix` (`:343-350`). The `.gz` variant counts for "work done" for every row even though only the mokuro CLI wrote it (**KEEP the read side: treat `.mokuro.gz` as present**; **DROP the write side**).
- **Name grammar (KEEP):** `^[a-z0-9][a-z0-9-]{0,31}$` matched with *fullmatch* (`GENERATION_NAME_RE` `:97`, `name_rejection` `:411-435`). Max 32 (`MAX_GENERATION_NAME`). The admin UI builds a JS RegExp from the pattern string and so the pattern string is published verbatim in the catalog (`name_pattern`, `admin/api.py:2257`). The name is an on-disk postfix the *reader* parses; the reader's own grammar is `LAYER_ID_RE = ^[a-z0-9-]{1,32}\Z` (`:81`, mirrors mokuro-reader `syncable-file.ts:108`). Server grammar is a subset (no leading hyphen).
- **Reserved (KEEP, `:106-107`):** names `original`, `gcv`, `updated-ocr`; prefix `tr-`.
- Rejection messages (shown verbatim by admin API and printed on server start; keep wording):
  - empty: `a name is required (it is this row's label and its file-name postfix)`
  - grammar: `name {name!r} cannot be a file-name postfix: use lowercase letters, digits and hyphens only, 32 characters at most, starting with a letter or a digit (a name outside this is invisible to every reader)`
  - reserved: `name {name!r} is reserved by the reader for its own layer of that name`
  - prefix: `name {name!r} is reserved: the reader files any layer starting with 'tr-' as a translation, whatever produced it`
  - duplicate (checked after rejection, `:690-697`): `ocr.generations[{i}]: name {name!r} is already generation {j}'s; every generation writes a file named after it, so names must be unique`
- **Name seeding (KEEP, `seed_generation_name` `:438-464`):** only when a row has no name. Evaluated once at row creation, then stored (never recomputed, or a moving name would re-OCR the library). Stem = engine id if the engine brings its own detector (or is monolithic) or no detector, else `"{engine}-{detector}"`. Trim to 32 and `rstrip("-")`. On collision with taken names or a rejection: counter starts at 2, tail `-N`, stem truncated to `32 - len(tail)` then trimmed, loop until free. Examples after the DROPs: `ppocr-manga` (engine has own detector -> engine id), `hayai-nova-ppocr-manga` (22 chars), `paddle-manga-ppocr-manga` (24 chars). (A 24-char cap once truncated `hayai-nova-animetext-attn` silently; hence 32; `:25-31`.)
- **Id (KEEP):** minted once as `g-<n>` = highest numeric `g-N` seen + 1, then probe for a free one (`mint_generation_id` `:472-483`). Hand-written ids allowed if `^[A-Za-z0-9][A-Za-z0-9_-]{0,31}\Z` and unique (`GENERATION_ID_RE` `:112`; used as a directory name for workspaces/detector dumps). `_assign_ids` (`:593-627`): first pass validates/dedups existing ids (errors with `field:"id"`: `... id {v!r} is not usable - ids are letters, digits, '-' and '_' only (they name this row's working directories); leave it out and the server mints one` / `... id {v!r} is already used by an earlier generation; ids identify a row for its whole life and must be unique`), second pass mints for blanks (in list order, accounting for ids minted earlier in the same pass). The id is never shown in the UI; it keys internal state (in-flight set, round-robin cursor, workspace dirs, congestion history, bench results, processor profiles).
- **Rename semantics:** a rename moves only the sidecar file name. What is "pending" is decided by which sidecar files exist, so a rename re-OCRs under the new name; nothing on disk is renamed (docs/ocr-internals.md "Generations").

### 1.3 Fields, defaults, validation (`parse_generation_list`, `_parse_row` `:489-741`)

Input forms accepted by `parse_generation_list(value, devices=None)`: parsed list of mappings; a JSON string (this is what `MOKURO_OCR_GENERATIONS` and `config set ocr.generations` carry); a single mapping (-> one row); a single `GenerationSpec`; list containing `GenerationSpec`s (round-tripped via `to_dict`). `None`, empty string/whitespace, or empty list -> `default_generations()` (`:509-511`, `_coerce_rows` `:567-590`). Bad JSON -> `ocr.generations must be a list of generations, or the JSON text of one; could not read it as JSON ({msg})`. Non-sequence -> `ocr.generations must be a list of generations, got {type}`. A non-mapping entry -> `ocr.generations[{i}]: each generation must be a mapping of fields, got {type}`.

All errors are `GenerationConfigError(message, row=index|None, field=str|None)`; the message always starts `ocr.generations[{i}]: ` for row errors. **Every rule is enforced at config load, env var, CLI `config set`, and admin API alike** (hand-edited YAML bypasses the API otherwise).

Per row, in this order:

1. **engine** (required): trimmed string; blank -> `ocr.generations[{i}]: engine is required (one of {ENGINE_IDS})`, `field:"engine"`. Unknown -> `Unknown OCR engine '{id}' (known: ...)` with `field:"engine"`. After the DROPs `ENGINE_IDS = (hayai-nova, paddle-manga, ppocr-manga)` (0.5.2 order was mokuro, hayai-nova, paddle-manga, ppocr-manga, `engines.py:68-118`). A config naming `mokuro` becomes a hard error at load. **DECIDE** whether to special-case a migration message (OQ-1).
2. **detector**: only parsed for engines that do **not** bring their own detector (`ppocr-manga` engine does; hayai-nova and paddle-manga do not). Blank/absent -> `DEFAULT_DETECTOR = "ppocr-manga"`. Unknown id -> `Unknown OCR detector '{id}' (known: ...)`, `field:"detector"`. A detector in `DISABLED_DETECTORS` is refused with its sentence, `... -- set this row's detector to one of {OFFERED}, or delete the row` (`:664-679`). For an engine with its own detector, any supplied `detector` is **ignored silently** (stored as None) (`:652-653`). After the DROPs, the registry holds one detector, so the only accepted explicit value is `ppocr-manga`. **KEEP** the "refuse (do not drop/swap) removed detectors" behaviour: a stored `ctd`/`animetext` value must error, not silently map (rationale `:664-671`: a sidecar must not misstate what read it).
3. **name**: blank/absent -> seeded (1.2); then rejection checks; then duplicate check against earlier rows (names map filled as rows parse, so the duplicate error names the *earlier* row index).
4. **`char_map` key present (any value) -> error** `ocr.generations[{i}]: char_map was removed with the character-map system (no per-character placement mode produced output worth using; readers lay characters on a uniform grid) -- delete the key`, `field:"char_map"` (`:699-710`). **KEEP** (the key must keep failing; no feature behind it).
5. **patch_budget**: `null`/absent -> 512 (`DEFAULT_PATCH_BUDGET`); else `int(str(v).strip())`, must be in `{256, 384, 512}` (`engines.py:237-250`). Messages `Invalid OCR patch budget {v!r}` / `Unknown OCR patch budget '{v}' (known: 256, 384, 512)`, `field:"patch_budget"`. Always validated, stored and emitted even for engines that ignore it; only `hayai-nova` uses it (`EngineSpec.patch_budget`, `PATCH_BUDGET_ENGINES = {hayai-nova}`). 512 is the default and 384 must never become default (measured rationale `engines.py:230-236`).
6. **primary / enabled**: `bool(row.get(...))`; `primary` default False, `enabled` default True. (Truthiness coercion, so the string `"false"` would be True; the admin UI sends real booleans. **KEEP semantics or tighten - DECIDE** minor.)
7. **precision** (`_parse_precision` `:834-862`): value = `precision` if non-empty else legacy `pools.precision`. Normalize: `None`/empty/`"auto"` -> `auto-accuracy`; lowercased; must be one of the six modes else `precision is {v!r}; it must be one of auto-accuracy, auto-balanced, auto-speed, fp32, bf16, fp16`, `field:"precision"`. If the engine is not in `PRECISION_ENGINES` (see section 3) the stored mode is **forced to the default** regardless of input (no error). If a forced mode is not in `engine_formats(engine)` -> `precision is {mode!r}, but {engine} does not run {mode} (it runs ...)`. After the DROPs `engine_formats` is always `(bf16, fp16, fp32)` so this branch never fires. Legacy `pools.precision` is accepted on read and discarded on write (**KEEP read-compat**).
8. **pools** (`_parse_pools` `:744-831`): see 1.4.

Whole-list rule `_validate_primary` (`:930-955`): among **enabled** rows, if there is at least one enabled row, exactly one must be `primary`. Zero enabled rows -> valid (no check). None primary -> `ocr.generations: no enabled generation is the primary one - exactly one must be, because it writes the bare <Volume>.mokuro every reader counts characters and inherits volume ids from`, `field:"primary"`, `row=None`. More than one -> `ocr.generations: 'a', 'b' are all marked primary - exactly one enabled generation may write the bare <Volume>.mokuro`, `row` = index of the *second* primary, `field:"primary"`. A disabled row may carry `primary:true` freely. (Note: a disabled primary plus an enabled non-primary row fails: "no enabled generation is the primary".)

Why exactly one primary: the bare `.mokuro` is the only source of `mokuro_version`, character counts, page character counts and the `volume_uuid` every other layer inherits (`:931-936`, docs "Why exactly one primary").

`parse_bench_spec(value, devices=None)` (`:538-564`): validate an *unsaved* spec with the same `_parse_row` rules. Strips `name/primary/enabled/id`, uses id `"bench-spec"`, empty `names`. Non-mapping -> `spec must be a mapping of engine, detector, patch_budget and pools, got {type|null}`. Row errors are re-raised with the `ocr.generations[0]: ` prefix replaced by `spec: `, `row=None`, `field` preserved. Used by bench, `derive`, and per-processor pools PUT.

### 1.4 Pools

`pools = {stage_workers, queue_capacity, stage_device}`, each a map stage-key -> value; an empty map means "derive it" (`GenerationPools` `:138-173`). Pools never change what is written, so a pools-only edit never cancels a running job.

Validation (per row, against the stage keys of **that row's road**):
- `pools` must be a mapping (or None) else `pools must be a mapping with 'stage_workers', 'queue_capacity', 'stage_device', 'precision'`. Unknown key -> `pools has no {key!r} setting (the settings are ...)` (`POOL_KEYS` includes `precision` only for legacy read).
- `stage_workers`/`queue_capacity` must be mappings of stage -> int-coercible. Stage keys: `stage_workers` accepts `pool_stage_keys`, `queue_capacity` accepts `stage_keys`; after the DROPs these are identical (road's stage keys; `line`: `detect,layout`; `reconciled`: `detect,engine,post`). Unknown stage -> `pools.{table} names stage {key!r}, but {engine} with {detector} runs {keys}`. Non-integer -> `pools.{table}.{key} must be a whole number, got {n!r}`. Range: workers `0..64` (`MAX_STAGE_WORKERS`), capacity `1..256` (`MAX_QUEUE_CAPACITY`) -> `pools.{table}.{key} is {n}; it must be between {floor} and {limit}`. All errors use `field:"pools"`. (Workers `0` is legal = serial width; capacity floor 1.) The runner separately clamps a width to the stage's structural ceiling; this only stops typos.
- The monolithic special case ("engine runs behind its own command line...", `:789-797`) is **DROP**.
- `stage_device` (`_parse_stage_device` `:865-927`): mapping of stage -> device; keys must be in `device_stage_keys` = the road's model-bearing stages = `detect`, `engine` that exist on the road (`model_stages(road)`: `line` -> `detect`; `reconciled` -> `detect, engine`). Else `pools.stage_device names stage {key!r}, but only a stage holding a model takes a device; this row's are {keys}`. `null` value skipped (treated as absent). Value normalised by `parse_device`: `auto|cpu|gpu:<n>` plus `gpu` and `cuda` -> `gpu:0`, `cuda:<n>`/`gpu:<n>` with `n` digits and `<= 15` (`MAX_GPU_INDEX`, `engine_runner.py:2362`, `parse_device` 2396-2418). Errors: `a device is required (auto, cpu or gpu:<n>)`, `{v!r} does not name a device: a card is gpu:<n>, n between 0 and 15`, `{v!r} does not name a device (auto, cpu or gpu:<n>)`. Must be known to the catalog (`DeviceCatalog.knows`, section 4): `device {id!r} is not on {where}, which reports {N GPU(s)}`. A CPU-locked stage may only be `auto` or `cpu`: `pools.stage_device.{key} is {device!r}, but {lock reason}; leave it on cpu`.

Stored `stage_device` values are canonical ids (`auto`, `cpu`, `gpu:N`); absent key == `auto`.

`output_affecting()` (`:352-371`) = tuple `(engine, effective_detector, patch_budget if engine uses it else None)`. This is the "recipe": a change to it cancels a running job (recorded as cancellation, not failure) and invalidates per-processor profile entries (section 6.5). Rename, `primary` flip, pools, precision are **not** output-affecting at the row level (note: precision changes stale per-machine *bench* separately, section 6.6). **KEEP exactly** (the recipe is serialised as a JSON list into profiles: `["hayai-nova","ppocr-manga",512]`; keep byte-compatible if profiles are to survive an upgrade, else DECIDE a profile reset, OQ-9).

Frozen-row rule: a claimed job carries the row it was claimed with (`GenerationSpec` is frozen), so an edit landing mid-run cannot move the file the finished volume is collected under (`:178-182`).

### 1.5 Derived properties (what the Rust type must expose)

After DROPs:
- `road`: `ppocr-manga` -> `"line"`, else `"reconciled"` (`:289-303`).
- `stage_keys`, `pool_stage_keys` = road's stage keys; `device_stage_keys` = road's model stages.
- `effective_detector`: engine's own (`ppocr-manga` for engine `ppocr-manga`) else row's `detector` or default (`:244-256`).
- `reported_detector`: always `effective_detector` (the mokuro/served exception that returned None is DROP, `:258-270`).
- `detector_locked` = engine has own detector (`ppocr-manga` engine). (The `uses_mokuro_env` clause DROP.)
- `patch_budget_applies` = engine is `hayai-nova`.
- `precision_applies` = engine in `PRECISION_ENGINES` = `{hayai-nova, paddle-manga}` after the DROPs.
- `monolithic`, `served`, `mokuro_env`: **DROP** (always False).

### 1.6 Collection helpers (KEEP unless noted)

- `enabled_generations(specs)` = enabled rows in **list order**; list order *is* queue order and OS-priority order (`:961-968`).
- `primary_generation`, `generation_by_id` (`:971-984`).
- `required_detectors(specs)`: union of `effective_detector` over enabled non-mokuro rows (`:987-1001`). After the DROPs it is `("ppocr-manga",)` when any row is enabled. Only used by the installer and runtime status; drop if there is no per-detector install.
- `required_engines(specs)`: engine ids needed by enabled rows, de-duplicated, row order (`:1033-1039`). Used by runtime status/`install-ocr`.
- `local_environment_problem(problems, row)` (`:1015-1030`, keys `mokuro`, `engines`, `detector:<id>`): **ADAPT**. With no venvs, "this server cannot run this row" reduces to "model files missing / ORT provider unavailable / model failed to load". Keep a per-row "cannot run here" reason string (surfaces in bench errors and the queue as `environment_problem`).

### 1.7 Sidecar sibling sweep (delete cascade) - KEEP

`sidecar_siblings(cbz)` (`:1042-1069`): the files that go when an archive is deleted: `<stem>.mokuro`, `<stem>.mokuro.gz`, `<stem>.webp`, `<stem>.nocover` (always listed, existing or not), plus every directory entry whose layer id (below) belongs to `stem`. Listed from the **directory**, not from configured names, so renamed/disabled/deleted rows' files and reader-pushed layers go too.
- `split_layer_sidecar(name)`: strip a trailing `.gz`; must end `.mokuro`; `middle = name[:-7]`; cut at the **last** dot (must be `> 0`, i.e. a dot at index 0 -> not a layer); `layer = middle[cut+1:]` must match `LAYER_ID_RE` (`^[a-z0-9-]{1,32}\Z`); returns `(middle[:cut], layer)`. Bare `<stem>.mokuro` -> None.
- `layer_id_of_sidecar(name, stem, other_volumes)`: split must exist and `split.stem == stem`, and `f"{stem}.{layer}"` must **not** be the stem of another archive in the same directory (`Volume 01.5.mokuro` is the primary OCR of volume `Volume 01.5`, not layer `5` of `Volume 01`).
- `volume_stems(names)`: stems of names ending (case-insensitive) in `.cbz .cbr .zip .rar` (`VOLUME_ARCHIVE_EXTENSIONS` `:1074`; a test pins it to `processor.SUPPORTED_EXTENSIONS`).
- Note: the reader lowercases the layer id; the server's `LAYER_ID_RE` is lowercase-only, so a file with an uppercase postfix is *not* swept.

### 1.8 What the admin can edit

- Via `PUT /api/ocr/generations` (admin role only): the whole list (order, add, delete, rename, enabled, primary, engine, detector, patch_budget, precision, pools). Full replacement only (section 8.3).
- Via `PUT /api/ocr/generations/<id>/pools`: one processor's pools override, never the config (8.5).
- Via `PUT /api/settings/ocr`: `poll_interval` only (8.2). `backend` is launch-only (rejected); `engines`/`detector`/`patch_budget` are rejected as moved; `char_map` rejected.
- Not editable via admin: `ocr.concurrency`, `ocr.sessions`, `ocr.local_processing`, `ocr.autobench` ("at startup" settings; config/CLI/env only).
- Config surface: `ocr.generations` is also settable via `config set ocr.generations '<json>'`, env `MOKURO_OCR_GENERATIONS` (JSON text), and `serve --generations '<json>'` (`config.py:597-601`, `646-647`; `docs/configuration.md:614-616`). Retired keys `ocr.engines`, `ocr.detector`, `ocr.patch_budget` and their `MOKURO_OCR_*` env equivalents, and `ocr.char_map` / `MOKURO_OCR_CHAR_MAP`, are refused by name at config load (`config.py:290-330`, `670-690`). **KEEP** these refusals (a config using them must not silently half-load). Other `OcrConfig` fields: `backend` (`auto|cuda|rocm|cpu|skip`; **ADAPT**: with `ort` the backends are execution providers - DECIDE OQ-6), `poll_interval` (default 30, >=1), `concurrency` (default 1), `sessions` (default true), `local_processing` (default true), `autobench` (default true) (`config.py:332-376`).

---

## 2. Engine registry (`ocr/engines.py`)

Pure data; no engine imports. After DROPs:

| id | label | recognizer repo (informational, written to sidecar) | own detector | patch_budget | cpu_only_reason |
|---|---|---|---|---|---|
| `hayai-nova` | `hayai-ocr v2.5 Nova` | `JustANormalTinkerer/hayai-ocr-v2.5-nova` | none | yes | "" (any device) |
| `paddle-manga` | `PaddleOCR-VL 1.6 manga LoRA` | `sorryhyun/paddleocr-vl-1.6-manga-lora` | none | no | "" |
| `ppocr-manga` | `PP-OCRv6 manga (CTC, CPU)` | `Kellenok/PP-OCRv6_manga` | `ppocr-manga` | no | `PP-OCRv6's CTC recognizer runs on the CPU (onnxruntime)` |

(`engines.py:75-118`). **DO NOT rename the `hayai-nova` id**: a generation named after it writes `Volume.hayai-nova.mokuro`, which readers already imported as a layer (`:92-94`). `mokuro` (`label "mokuro (manga-ocr)"`, recognizer `kha-white/manga-ocr-base`, served via `mokuro.serve`) **DROP**.

`EngineSpec` fields to keep: `id, label, recognizer, detector (own), patch_budget, cpu_only_reason` (+ derived `cpu_only`). Fields `uses_mokuro_env, road, serve_module` **DROP** (road is derived from engine/detector; section 1.5).

Detector registry (`engines.py:123-205`): keep **only** `ppocr-manga`: label `PP-OCRv6 manga line detector (Kellenok)`, license `Apache-2.0`, `line_level: true`, `cpu_only_reason: "the PP-OCRv6 detector runs on the CPU (onnxruntime)"`. `ctd` (GPL-3.0, comic-text-detector via mokuro), `animetext` (GPL-3.0, AnimeText YOLO12-x; was already in `DISABLED_DETECTORS`) and `rtdetr` are **DROP**, along with `script`, `extra_packages`, `probe_import`, `isolated` (the adapter/license-boundary machinery) and `DISABLED_DETECTORS`/`OFFERED_DETECTOR_IDS` (equal to `DETECTOR_IDS` once one remains; keep the "known:" list helper). `DEFAULT_DETECTOR = "ppocr-manga"` (`:187`). `rtdetr` does not appear in 0.5.2's registry at all; it is mentioned only in the owner decision.

Other constants (`engines.py`): `PATCH_BUDGETS = (256, 384, 512)`, `DEFAULT_PATCH_BUDGET = 512`, `get_patch_budget`, `get_engine` (raises `Unknown OCR engine '{id}' (known: a, b, c)`), `get_detector` (`Unknown OCR detector '{id}' (known: ...)`). `GPU_BACKENDS = {cuda, rocm, mps}` and `backend_is_gpu(backend)`: True for those, False for `cpu`, None for `auto|skip|unknown` (`:276-290`) - **ADAPT** with the EP model. `GpuUse` dataclass (`:293-307`) reports which environments run torch on a GPU: **DROP**.

---

## 3. Precision (`ocr/precision.py` + `engine_runner.py:282-600`)

### 3.1 Modes (KEEP names and semantics)

Six modes, in order (`PRECISION_MODES` `engine_runner.py:337-339`): `auto-accuracy` (default), `auto-balanced`, `auto-speed`, `fp32`, `bf16`, `fp16`. Formats: `PRECISIONS = (bf16, fp16, fp32)`. Legacy spelling `auto` normalises to the default; normalisation lowercases/strips; `None`/blank -> default (`normalize_precision_mode` `:392-403`). `FORCED_PRECISION_MODES = {fp32,bf16,fp16}`, `AUTO_PRECISION_MODES = {auto-*}`, `BENCHED_PRECISION_MODES = {auto-balanced, auto-speed}` (modes a per-machine benchmark decides). `PRECISION_TIE = 0.05`.

### 3.2 The policy table (`PRECISION_POLICY` `:351-373`)

Engine -> mode -> candidate formats, order = preference:

| engine | auto-accuracy | auto-balanced | auto-speed |
|---|---|---|---|
| hayai-nova | bf16, fp32 | bf16, fp32 | bf16, fp16, fp32 |
| paddle-manga | fp32 | bf16, fp32 | bf16, fp16, fp32 |
| mokuro | fp32 | fp32 | fp16, fp32 | **DROP** |
| ppocr-manga | (not in table: engine fixes its own precision; the mode is ignored, stored as default) |

`PRECISION_ENGINES` = keys of the table = `{hayai-nova, paddle-manga}` after DROP. The numbers behind the table (measured on 1,330 B/W pages) are in the comment `:309-324`: hayai-nova bf16 was *more* accurate than fp32 (fp16 the confirmable loser); paddle-manga most accurate in fp32, bf16 close and much faster. `TORCH_PRECISION_ENGINES = {hayai-nova, paddle-manga}` (precision set in-process).

`mode_candidates(engine, mode)`: not a precision engine -> `()`; forced mode -> `(mode,)` if the engine runs that format else `()`; else the policy list. `engine_modes(engine)`: `()` for non-precision engines else all modes (every forced mode allowed once mokuro is gone). `engine_precision_modes(engine)` (precision.py:511-513) returns `list(engine_modes)` or `[]`.

### 3.3 Support is probed, never listed (`supported_formats` `:507-522`)

fp32 always; on a GPU also fp16 unconditionally; bf16 when `torch.cuda.is_bf16_supported()` is true with that device current. CPU (or any non-cuda device) -> `{fp32}` only. A probe failure -> bf16 omitted. No architecture lists. A card that emulates bf16 slowly (RX 6000) reports bf16 supported; only a benchmark shows it slow. **ADAPT (central design question, OQ-2):** under ORT the dtype is largely baked into the exported graph, and "bf16 supported" is not an `ort` query. The Rust port must decide what a "format" is (a choice among pre-exported fp32/fp16/bf16 model files? an EP-level fp16 flag?) and how `supported` is probed.

### 3.4 Resolution (`resolve_mode` `:442-481`, KEEP logic)

`ModeResolution(precision: str|None, eligible: bool, why: str, usable: tuple)`.
- Engine not a precision engine -> `(None, True, "fixed by the engine")`.
- Forced mode: engine must run the format (`"{engine} does not run {mode}"`, ineligible); `supported=None` is treated as `{fp32}`. Supported -> `(mode, True, mode, (mode,))`; unsupported -> `(None, False, "{mode} not supported")`, or `"card not reported"` when `supported is None`.
- Auto mode with `supported=None` (card never reported) -> `(None, True, "decided at start (card not reported)")`.
- Auto mode: `usable` = candidates present in `supported | {fp32}` (fp32 is always added). If the mode is not benchmarked (accuracy) or only one candidate is usable -> `(usable[0], True, mode, usable)`. If a benchmark `pick` is in `usable` -> `(pick, True, pick_why or "benchmark", usable)`. Else `(usable[0], True, "not benchmarked yet: first supported candidate", usable)`.

`pick_precision(trials, usable)` (`:483-504`): `rates = {fmt: rate}` for usable fmts with rate > 0; none -> None. `fastest = max(rates)`. Chosen = the **first** fmt in `usable` order whose rate >= `fastest * (1 - 0.05)`. `others` = other measured usable fmts. `why = "benchmark: {chosen} {rate:.2f} p/s {verb} {f1 rate p/s, f2 rate p/s}"` where verb = `beat` if chosen's rate is strictly greater than all others else `tied with`; with no others `"benchmark: {chosen} {rate:.2f} p/s"`.

`resolve_precision(engine, requested, supported, pick, pick_why)` -> `(precision, why)` where `why = mode` if the resolution reason equals the mode else `"{mode}; {reason}"`; ineligible -> raises `PrecisionUnavailable` with message `precision not available here: {engine} is asked for {mode}, and this device cannot run it ({why})`. The marker `PRECISION_REFUSAL = "precision not available here"` makes the library give the volume back **unrecorded (never a failure)** and back the row off on that machine. The runner logs `[runner] {engine} precision: {precision} ({why})`.

### 3.5 Server-side policy (`precision.py`)

- `PRECISION_MODE_LABELS` (UI select text, KEEP verbatim): `auto-accuracy` "Auto: accuracy", `auto-balanced` "Auto: balanced", `auto-speed` "Auto: speed", `fp32` "fp32 only", `bf16` "bf16 only (cards that support it)", `fp16` "fp16 only (GPUs)" (`:359-366`).
- `hold_reason(mode)` = `No connected machine can run {mode}` (`:369-371`); a row nobody can run is "held" (`OCRWorker.precision_holds`, surfaced as `precision_hold` on the generations payload and as `held` in `.mokuro-queue.json`).
- `model_device(row, stage_device)`: the placement key of the row's model: `engine` stage for composed rows (the `mokuro` stage case DROP), value from the given `stage_device` map or the row's pools, default `"auto"` (`:378-382`).
- `bench_pick(bench, mode)` (`:385-401`): only for benched modes and only when `bench.precision_mode == mode`; requires non-empty `precision_trials` (list of `{precision, pages_per_second}` with a valid format and a numeric non-bool rate) and `bench.precision` among the trial formats; returns `(bench.precision, bench.precision_why or pick_precision(...) reason or "benchmark")`.
- `row_resolution(row, catalog, stage_device, mode=None, bench=None)`: `resolve_mode(row.engine, mode or row.precision, catalog.supported_for(model_device(row, stage_device)), pick, why)`.
- `row_refusal(row, catalog, stage_device)`: None if `!precision_applies` or eligible, else `it cannot run {row.precision} ({why})`. Used by bench enqueue (`bench.py:1038-1044`) and by the scheduler (`catalog_can_run`).
- `resolution_entry(resolved, bench, mode)` -> `{"precision", "eligible", "why"[, "trials": [{precision, pages_per_second}]]}` (trials only for benched modes whose bench was taken for that mode with trials).
- `precision_on(row, machines, unpicked=None)` -> `{machine: {mode: entry}}` for **every** mode in `engine_modes(row.engine)`; for benched modes with more than one usable candidate adds `entry["bench"]` = `"done"` if the stored pick is among `usable`, else `unpicked(machine)` (`"pending"` default; admin passes `"off"` when `ocr.autobench` is false, `"failed"` when `(machine, id)` is in the autobench-failed set, else `"pending"`) (`:468-498`, `admin/api.py:1638-1642`).
- `precision_catalog()` -> `{"precision_modes": [{id,label}...], "precision_default": "auto-accuracy"}`.

### 3.6 Where the precision is recorded

Resolved precision is stamped into `ocr_engine.precision` (section 5) and the `ocr_sidecars.precision` column. It is never stored in a machine's `pools` (legacy pins are read, ignored, logged once per (machine,row)); it is one mode per row for all machines.

---

## 4. Devices (`ocr/devices.py`)

### 4.1 Device ids

`auto | cpu | gpu:<n>` (n 0..15), vendor-neutral. `auto` = card 0 where there is one, else CPU. An absent `stage_device` key means `auto`. (`engine_runner.py:2358-2418`.)

### 4.2 Catalog and GPU detection

`DeviceCatalog{gpus: [GpuDevice], cpu_label="CPU", vendor="", probed=False, ort_gpu_providers: tuple|None, where="this server"}` (`:166-186`). `GpuDevice{index, name, memory_bytes|None, formats: frozenset|None}`; `id = "gpu:{index}"`; `supported()` = `{fp32} | formats` or None if never reported (`:129-163`).

- `ids()` = `("auto", "cpu", gpu ids...)`. `entries()` (UI list, `:295-303`) = `[{"id":"auto","label":"Auto - GPU {first.index} when available"|"Auto - CPU"}, {"id":"cpu","label":cpu_label}, {"id":"gpu:N","label": gpu.label}...]` (the dash in the source is an em dash `—`; keep the exact label text from the source, including `—`).
- `GpuDevice.label`: blank name -> `GPU {i}`; a name already starting `GPU {i} ` returned as is; with memory -> `GPU {i} — {name} ({bytes/1e9:.0f} GB)`; else `GPU {i} — {name}` (`:150-163`).
- `label_for(id)`: `auto` -> `Auto`; `cpu` -> `cpu_label` stripped or `CPU`; known gpu -> its label; unknown `gpu:N` -> `GPU N`; else the id (`:254-271`).
- `knows(id)`: parse failure -> False; `auto`/`cpu` True; **unprobed catalog accepts any `gpu:N`** (a server that cannot look may not tell a user their card does not exist); once probed, only listed indexes (`:236-246`). `refusal(id)` = `device {id!r} is not on {where}, which reports {1 GPU|N GPUs}` (`:248-252`).
- `supported_for(device_id)`: `cpu` -> `{fp32}`; `auto`/blank -> card 0's `supported()` if any GPU, else `{fp32}` if probed else None (unknown); explicit `gpu:N` -> that card's `supported()` or None (`:202-220`).
- `gpu_facts()` -> `[{"index", "formats": {"bf16": bool, "fp16": bool}}]` for cards with known formats (the `gpus` key of a processor's registration catalog, `GPU_FACTS_KEY`, `:222-234`).
- `for_machine(name, host)`: re-label for one named machine - CPU label from `host.cpu` if the catalog's was blank/`CPU`; card 0 gets `host.gpu` as name if it had none; `where=name` (`:273-293`).
- `ort_gpu`: `None` if `ort_gpu_providers` is None (unknown), else `bool(providers)` (`:192-197`). Today only used for the animetext/ORT-GPU detector case, which is **DROP**, so `ORT_GPU_DETECTORS`, `stage_needs_ort_gpu`, `ORT_GPU_PROVIDERS` handling and the "`device_locked_reason` because onnxruntime has no GPU provider" branch (`stage_lock_reason` `:539-548`) are DROP. **ADAPT-DECIDE (OQ-3):** if ppocr stages are to run on a GPU EP via `ort` (CUDA/ROCm/MIGraphX/DirectML...), the "ort_gpu" concept is back and the CPU-lock on the PP-OCR stages (`cpu_only_reason`) changes. 0.5.2 locks every ppocr stage to CPU.
- `merge_catalogs(catalogs)` (`:432-465`): union of cards by index (first seen wins); `probed = all probed` (one unprobed keeps merge unprobed); `cpu_label`/`vendor` from the first; ORT providers merged (any non-empty -> union; else None if any None; else `()`). Used for "every device any machine could place a model on" when validating a saved row (`AdminAPI._settable_devices`).
- `catalog_from_entries`/`catalog_from_processor`: rebuild a catalog from a remote processor's registration (`devices:[{id,label}]`, optional `gpus` formats, optional `onnxruntime_gpu_providers`); an empty/non-mapping catalog -> unprobed `DeviceCatalog()`. Remote-processor protocol detail belongs to the processor spec; keep the catalog wire keys: `devices`, `gpus`, `onnxruntime_gpu_providers`.
- Process-wide cache: `cached_catalog()` (fallback `DeviceCatalog()` before any probe) / `set_cached_catalog(c|None)` under a lock (`:468-482`). `POST /api/ocr/devices/refresh` clears and re-probes.

**Probe (DROP mechanism, ADAPT contract).** 0.5.2 probes by running `PROBE_SOURCE` (a torch one-liner) in the engines venv with a 120 s timeout and parses one JSON object on stdout (`:85-106`, `bench.py:245-257`):

```json
{"vendor": "rocm|cuda|", "gpus": [{"index": 0, "name": "...", "memory_bytes": 17163091968, "formats": {"bf16": true, "fp16": true}}], "onnxruntime_gpu_providers": ["CUDAExecutionProvider"] | [] | null}
```

`vendor` is `rocm` if `torch.version.hip` else `cuda` if any device else `""`. `parse_probe(payload, cpu_label)`: empty/invalid/non-object -> unprobed fallback; skips non-dict entries and entries without an int-coercible `index`; `memory_bytes` kept only if a non-zero number; cards sorted by index; `formats` -> frozenset of those of `bf16`,`fp16` whose value `is True` (None if the key is absent/not a mapping). In Rust, enumerate adapters via the chosen EP / NVML / ROCm-SMI and emit the same catalog; keep `gpus[].formats` semantic as decided in OQ-2.

`describe_host(backend, engines_python)` (`bench.py:233-258`) -> `{"cpu": "<model> (<N> cores)", "gpu": <name|None>, "backend": <backend|None>}`: CPU model from `/proc/cpuinfo` `model name` (fallback `platform.processor()`...), physical cores = distinct (physical id, core id) pairs (fallback `os.cpu_count()`); GPU from torch device 0 name, else `nvidia-smi --query-gpu=name --format=csv,noheader` (first line), else `rocm-smi --showproductname --csv` ("Card Series" of first row). **KEEP** the output shape and the "(N cores)" suffix: the admin parses it with `\((\d+) cores?\)` to size the host worker budget (`admin/api.py:~127, 309-322`).

### 4.3 Stage placement rules (`stage_lock_reason`, `stage_devices_allowed`)

- `stage_takes_a_device(road, key)`: key is a model stage of the road.
- `stage_devices_allowed(road, key, engine, detector, catalog)`: `[]` for a model-less stage (UI Device cell is a label); `["auto","cpu"]` when CPU-locked; else `catalog.ids()` (`:485-504`).
- `stage_lock_reason`: None unless the key is a model stage; CPU-only stage (`stage_is_cpu_only`: the ppocr pair on `line` `detect`, `reconciled` `detect`; the `engine` stage when the engine has `cpu_only_reason`) -> for `engine` stage the engine's reason else `this recognizer runs on the CPU`; for `detect` with an engine-owned detector the detector's reason, then the engine's, else `this detector runs on the CPU`; else the detector's reason or `this detector runs on the CPU` (`:549-559`). Reasons are surfaced as `device_locked_reason`.
- After DROPs: `ppocr-manga` engine -> `detect` locked CPU (reason "the PP-OCRv6 detector runs on the CPU (onnxruntime)" via the detector spec); `hayai-nova`/`paddle-manga` -> `detect` locked CPU (the ppocr detector), `engine` free (any device).
- Placement consequences (docs/ocr-internals.md "Devices"): a stage on a card is one model/one worker; the same stage on CPU is a pool sized by the host budget. With `ocr.concurrency > 1`, a slot opening a session prefers a row whose engine device has no session on it yet. The runner reports where each model really came up (`stage_device` in its ready event).

---

## 5. Provenance (`ocr/provenance.py`, `ocr/processor.py:_stamp_ocr_engine`, `engine_runner.py:build_volume`)

### 5.1 What a sidecar records about its producer

Top-level mokuro keys written for every sidecar (`build_volume` `engine_runner.py:1082-1107`, `_normalize_mokuro_metadata` `processor.py:899-960`): `version` ("0.2.5" = `MOKURO_FORMAT_VERSION` `engine_runner.py:80`), `title`, `title_uuid`, `volume`, `volume_uuid`, then `ocr_engine` (if any), then `pages`. After the runner writes, the server rewrites the file: `title = series name`, `volume = archive stem`, `title_uuid = uuid5(NAMESPACE_DNS, series_name)`, `volume_uuid = volume_uuid_for(cbz, generation)` (same for every sidecar of a volume). The rewrite is `json.dump(..., ensure_ascii=False, separators=(",", ":"))` (compact), UTF-8, gzip-aware.

`ocr_engine` block for composed engines (runner, `engine_runner.py:7924-7948`). Key order and presence:

```json
"ocr_engine": {
  "id": "hayai-nova",
  "recognizer": "JustANormalTinkerer/hayai-ocr-v2.5-nova",
  "detector": "ppocr-manga",
  "generator": "mokuro-bunko 0.5.2",
  "patch_budget": 512,
  "weights": {
    "JustANormalTinkerer/hayai-ocr-v2.5-nova": "e46d79138499600564f810d44ab6bdea7230dee1",
    "google/siglip2-base-patch16-naflex": "b53b807d3a2d5e2b3911292f2d69e5341cdc064c",
    "Kellenok/PP-OCRv6_manga": "ba1d479e8a61a20e8318c9758c73fbbbd290b98d"
  },
  "precision": "bf16",
  "generation": "hayai-nova-ppocr-manga"
}
```

- `id` = engine id; `recognizer` = `RECOGNIZER_REPOS[engine]` (`hayai-nova`, `paddle-manga`, `ppocr-manga` repos, table in section 2); `detector` = the pipeline's detector id (`ppocr-manga` for all three after DROPs); `generator` = `config.generator or "mokuro-bunko"` (the server passes `mokuro-bunko <__version__>`; 0.5.0 appears in docs, use the version string of the Rust build); `patch_budget` **only** for `PATCH_BUDGET_ENGINES` (`hayai-nova`) - never claim a resolution the recognizer did not read at; `weights` only if non-empty; `precision` only if the recognizer resolved one (`pipe.precision()`, which is None for ppocr-manga and while loading).
- `weights`: repo -> resolved commit **for every model this run actually loaded** (collected from loaded objects, never a static table: `OpenPipeline.weights` `engine_runner.py:7228`). hayai-nova -> `{recognizer repo, HAYAI_VISION_REPO}` pinned; paddle-manga -> `{PADDLE_BASE_REPO, LoRA repo}` pinned (`:1566`, `1836`); ppocr detector/engine -> `{Kellenok/PP-OCRv6_manga: REPO_REVISION}` **only when every file came from the pinned download** - models found in a `MOKURO_PPOCR_MODELS` directory are *not* claimed (`pinned=False`, `ppocr.py:360-378`, `engine_runner.py:4655-4665`). A reconciled read names both recognizer and detector repos. The mokuro served-engine form (`{"id","precision"}` only, no weights) is **DROP**.
- Server stamping (`_stamp_ocr_engine` `processor.py:962-984`): for every **non-primary** sidecar the server `setdefault`s `id` = engine id, `generator` = `"mokuro-bunko {version}"`, and **always sets** `generation` = generation name (overwriting). Existing runner-written keys are kept. The **primary** `<Volume>.mokuro` is left in pure upstream-mokuro shape except that a composed engine running as primary keeps the runner's own `ocr_engine` block **without** a `generation` key (the server does not stamp primary). KEEP: this is how a reader tells server OCR from a hand-edited layer (`ocr_engine` object with string `id` = server OCR; absent = a person's edit). Note the asymmetry: `generation` appears only on non-primary layers.
- The `review.json` listing (`{"format":"ocr-review/1","engine","detector","pages":[...]}`) is written beside the per-page dumps on the reconciled road, never inside the sidecar (`engine_runner.py:7955-7966`). Out of scope here (runner spec) but note it is not provenance.
- `_weights.json` adapter file (`DETECTOR_WEIGHTS_FILE`) is the adapter-process contract: **DROP** (no adapters).

### 5.2 Pinned model revisions (KEEP the pins; the policy is the point)

`REPO_REVISIONS` (`engine_runner.py:116-121`):

| repo | commit |
|---|---|
| `JustANormalTinkerer/hayai-ocr-v2.5-nova` | `e46d79138499600564f810d44ab6bdea7230dee1` |
| `google/siglip2-base-patch16-naflex` (`HAYAI_VISION_REPO`, hayai's image processor) | `b53b807d3a2d5e2b3911292f2d69e5341cdc064c` |
| `sorryhyun/paddleocr-vl-1.6-manga-lora` | `26292839d1469c14212a12a1e01b5b1fe01bff15` |
| `PaddlePaddle/PaddleOCR-VL-1.6` (`PADDLE_BASE_REPO`) | `c5630abae1d940eafe0697512a0325494b02ab42` |
| `Kellenok/PP-OCRv6_manga` (`ppocr.py:79-83`, `REPO_ID`/`REPO_REVISION`; v0.2, 2026-09-28) | `ba1d479e8a61a20e8318c9758c73fbbbd290b98d` |

Policy: pin to commit SHAs only, never tags/branches (they move). Rationale: hayai-nova and PaddleOCR-VL use `trust_remote_code=True` (the repo's Python runs on the host), and the second reason is that sidecars can name the exact weights. Bump procedure (docs/ocr-internals.md): fetch at new commit into scratch cache, read the diff (for remote-code repos the `.py` diff), re-run a bench volume and compare sidecars, change the SHA, note in CHANGELOG. **ADAPT:** the Rust port loads ONNX exports, not these torch repos. Unless the ONNX files are published as artifacts in the *same* HF repos at those commits, the `weights` map / pins must be re-derived for whatever repos+revisions the ONNX models come from (OQ-4). The `recognizer` string written to the sidecar is informational but readers may display it; keep the repo ids stable unless the owner decides otherwise.

### 5.3 The `ocr_sidecars` table and audit events

Table (`database.py:658-678`), one row per sidecar file **on disk now** (replaced by a re-run, deleted when the file leaves the library); the audit log is the history:

```sql
CREATE TABLE IF NOT EXISTS ocr_sidecars (
  sidecar_path TEXT PRIMARY KEY, volume_key TEXT NOT NULL,
  generation_id TEXT NOT NULL, generation_name TEXT NOT NULL,
  machine TEXT NOT NULL, account TEXT,
  engine TEXT, detector TEXT, precision TEXT, runner_build TEXT,
  pages INTEGER, failed_pages INTEGER,
  archive_size INTEGER, archive_mtime_ns INTEGER,
  written_at TEXT NOT NULL DEFAULT (datetime('now')));
CREATE INDEX IF NOT EXISTS idx_ocr_sidecars_volume ON ocr_sidecars(volume_key);
```

`record_ocr_sidecar(row)` = `INSERT OR REPLACE ... written_at = datetime('now')` over the 14 data columns (`database.py:1958-1972`). Also `get_ocr_sidecar(path)` (path `.strip("/")`), `list_ocr_sidecars()` (`ORDER BY written_at, rowid`), `ocr_sidecar_producers()` -> `(generation_id, volume_key, machine)` oldest first, `forget_ocr_sidecar(path)`, `forget_ocr_sidecars_of_volume(volume_key)` (`database.py:1983-2012`).

`ProvenanceRecorder.written(...)` (`provenance.py:180-241`): fields = `sidecar_path` (library-relative posix path or None), `volume_key` (relative posix path of the cbz), `generation_id`, `generation_name`, `machine` (`"local"` or processor name), `account` (processor account or None), `engine` (= `generation.engine`), `detector` = runner block's `detector` string else `generation.reported_detector`, `precision` = runner block's `precision` string else (`generation.precision` mode string if `precision_applies`) else None - note this can store a *mode* name (e.g. `auto-accuracy`) when the runner reported no resolved format, `runner_build`, `pages` (given, else sidecar's `pages` length), `failed_pages`, `archive_size`/`archive_mtime_ns` (the archive's stamp at write). The row is written only if both relative paths resolve inside the library. Then an audit event `action="ocr_sidecar_written"`, `actor_username=account`, `target_type="sidecar"`, `target_path="/mokuro-reader/<relative>"` (or the raw path if outside), `details` = `{"generation": name, "generation_id", "machine", "engine", "detector", "precision", "pages", "failed_pages", "runner_build"}`. `rejected(...)`: audit `ocr_sidecar_rejected`, same actor/target (computed from `generation.sidecar_paths(cbz)[0]`), details `{"generation","generation_id","machine","engine","reason": reason[:500]}`, **no table row**. `forget(sidecar)` drops the row. **Nothing here may fail an OCR result**: any DB error is logged (`logger.exception`) and swallowed.

`SidecarFacts{pages, engine_block, format_version}` from `read_sidecar_facts(path)` (plain or `.gz`; None on any I/O/JSON error or non-object; `pages = len(data["pages"])` if a list; `engine_block` = a copy of `data["ocr_engine"]` if a dict else `{}`; `format_version` = top-level `version` if a non-empty string). Read **before** the server's normalisation so the server's own stamp is not mistaken for the runner's. `failed_pages_from_log(log)`: last `failed_pages=(\d+)` in the final 256 KiB of the volume log, else None (the log line is `[runner] wrote <path> pages=N failed_pages=M elapsed=Xs`; the Rust runner must keep emitting it or provenance gets None).

`runner_build(bunko_version, runner_digest, uses_mokuro, facts)` = comma-joined `"mokuro-bunko {version}"`, `"runner {digest}"` (the staged runner's content hash, `ocr/staging.py`), and `"mokuro {format_version}"` only when `uses_mokuro` (**DROP** that part); None if all empty. **ADAPT:** there is no staged Python runner in Rust; `runner_digest` becomes a build id (git sha) or is omitted - DECIDE (OQ-5). A processor reports its own build string when it registers.

`attribute_volumes(records, present)` (`:139-161`): from records oldest-first, the newest record per `(generation_id, volume_key)` wins; counts per `{generation_id: {machine: n}}` only for volumes whose sidecar is currently present for that row (`present[generation_id]` = set of volume keys). A sidecar with no record counts for nobody; a machine's counts sum to <= the row's done count. Feeds `volumes_by_machine` in the admin payload.

`volume_uploads`/volume_identities interplay (a re-OCR keeps the volume id; `volume_uuid_for`) is in the processor spec; the sidecar rule is "every sidecar is stamped, in order, with the primary's uuid on disk, else the remembered one, else a layer's, else `deterministic_uuid("<Series>/<Volume>")`".

---

## 6. Benchmarks (`ocr/bench.py`, runner `--bench` in `engine_runner.py:8380-9300`)

### 6.1 What a benchmark is for

Answers "should I commit to this row, on this machine?" for the row **as currently edited** (a spec, saved or not). It measures pages/second of the real pipeline on pages sampled from the operator's own library, tunes pool widths (and for balanced/speed modes picks a precision), and stores the result. Every speed number shown must be measured on this machine/library, not inferred.

Whether the **tuning search** is ported is a scope decision (OQ-7); at minimum the data model below (`.ocr-bench.json`, endpoints, per-machine profile bench, staleness, auto-bench, precision trials) is what the admin UI and the ETA/scheduler read.

### 6.2 Sample

`build_sample(storage_path, pages)` (`bench.py:405-476`): library = `<storage>/library`; no archives -> `BenchError(400, "there are no volumes in the library to benchmark with - upload one first, then the numbers are measured on your own pages")` (em dash in source `—`); no readable pages -> `400 "the library's archives have no readable pages to benchmark with"`; nothing extractable -> `400 "no pages could be read out of the library's archives to benchmark with"`. Parameters: `DEFAULT_SAMPLE_PAGES=32`, `MIN=4`, `MAX=512` (request clamps `max(4, min(512, int(pages)))` here but `enqueue` *rejects* out-of-range, below). Archives = every `*.cbz` under `library` (rglob, sorted), grouped by series (`parent.relative_to(library).as_posix()`), each series sorted by natural key (digits compared as ints, text lowercased), then **round-robin across series** (index 0 of every series in sorted series order, then index 1, ...). Pages of an archive = members that are not directories, with suffix in `{.jpg,.jpeg,.png,.webp,.bmp,.gif,.tif,.tiff}`, excluding the thumbnail `<stem>.webp`, natural-sorted. Per visited archive take `per_archive = max(4, ceil(wanted / n_archives))` (`SAMPLE_PAGES_PER_VOLUME = 4`) bounded by what is still needed, chosen by `_spread(count, total)`: `count >= total` -> all; else skip `edge = min(2, (total-count)//2)` pages at each end (`SAMPLE_EDGE_PAGES=2`; covers/inserts/ads are 2.6-3.4x faster than story pages and skew results), `span = total - 2*edge`, `step = span / count`, indices `edge + min(span-1, int((i+0.5)*step))` (set, sorted). Extract each to `<storage>/.processing/bench-sample-*/` as `{extracted:04d}_{safe_stem40}{suffix}` (`safe` = stem with `[^A-Za-z0-9._-]+` -> `_`, first 40 chars). Result `BenchSample{pages, volumes}` -> `{"pages": N, "volumes": M}`. Scratch dir removed afterward.

### 6.3 Machine handling, queue, concurrency

- "Machine" = `"local"` (this server's own hardware, `LOCAL_BENCH`) or a connected processor by name. **One line (FIFO) per machine**; different machines' lines run concurrently; a machine is held and its running OCR volumes pre-empted **once** when its line starts (`OCRWorker.preempt_for_bench(timeout=3600, processor=machine)`), then released when its line empties. Preempted volumes are cancelled without a failure or backoff and run again later. Other machines keep working. Timeouts: `QUEUE_HOLD_TIMEOUT=3600 s`, runner budget `BENCH_BUDGET_SECONDS=900`, `BENCH_SLACK_SECONDS=1800` (server gives up on a silent runner after budget+slack = 2700 s), `BENCH_MAX_TRIALS=8`.
- `enqueue(key, spec, pages, processor="local", autobench=False, on_done=None, precision_only=False)` validation order (`:~770-880`), each a `BenchError(status, message, row?, field?)`:
  1. processor != local: must be a connected, non-local, non-dropped registry entry with that name, else `400 "no processor called {name!r} is connected"`; its catalog (`catalog_from_processor`) is the device catalog for validation (else `cached_catalog()`).
  2. key = saved generation id or a draft key `^draft-[a-z0-9-]{1,24}\Z`; a non-draft key not in config -> `400 "there is no generation {key!r} to benchmark"`.
  3. If `spec` given: `parse_bench_spec(spec, devices=catalog)` else `400` with the parse message, `row:null`, `field`. If no spec, a saved row is measured as saved; a draft with no spec -> `400 "there is no generation {key!r} to benchmark - send a spec to measure one that is not saved yet"`.
  4. Worker absent/thumbnails-only -> `400 "OCR is disabled in this server process"`.
  5. Remote: `catalog_can_run(entry.catalog, measured)` reason -> `400 "{processor} cannot run this row: {reason}"`; monolithic rows on a processor (DROP).
  6. Local: `worker.local_processing` false -> `400 "this server runs no OCR of its own (ocr.local_processing is off); choose a connected processor to benchmark on"`; row's environment missing -> `400 <environment_problem>` (admin: `the OCR engines environment {name} needs ({engine}) is not installed yet; it installs on restart`; **ADAPT** to "model files not available"); `row_refusal(measured, devices)` -> `400 "this server cannot run this row: {refusal}"`.
  7. `pages` if given must be an int (not bool) in `[4, 512]` else `400 "pages must be a whole number between 4 and 512"`.
  8. Empty library -> `400 "there are no volumes in the library to benchmark with - ..."`.
  9. Same key already queued/running **on the same machine** -> `409 "a benchmark of {name} is already queued or running [on {processor} ]- re-posting the same row is a no-op"`.
  Everything that can be said at once is said before anything is queued or the OCR queue touched. Returns `get(key, processor)`.
- `get(key, processor=None)` precedence: live queued/running run (with `position` = index in **its machine's** line, 0 = running/about to) -> last result of `(key, machine)` in process memory (`_recent`: any terminal state incl. failed/cancelled; draft results live only here, capped at `MAX_DRAFT_RESULTS=32`, oldest evicted) -> for non-draft local keys, the persisted result from `.ocr-bench.json` -> `{"state":"idle","generation":key,"key":key,"queue":{...}}`. A processor's finished benches live in its profile, never `.ocr-bench.json`. All responses carry `"queue": {"running": <first key in global FIFO|null>, "queued": [rest]}`; non-live ones also `"position": null`.
- `cancel(key, processor=None)`: no queued/running run -> `400 "there is no benchmark of this generation queued or running to cancel"`. A queued non-head run is removed immediately (state `cancelled`); the running one is killed (session/process kill) and the call waits up to 60 s for it to settle then returns `get(...)`. A kill that surfaces as `failed` is rewritten to `cancelled` with `error:null` (`_finish` `:1210-1217`).
- `paused_for_benchmark()` -> `{"key","generation": row name or draft key,"queued": len(queue)-1,"processor": head.processor}` or None; `configuring()` -> `{machine: {"key","generation","auto": bool}}` (head of each machine's line) - both feed the Queue page.

### 6.4 The result object (wire format, KEEP)

`GET /api/ocr/generations/<key>/bench` returns (initial values, `_BenchRun.data` `:663-692`):

```json
{
  "state": "queued|running|done|failed|cancelled|idle",
  "key": "g-1", "generation": "g-1",
  "processor": "local", "autobench": false, "precision_only": false,
  "spec": {"engine": "...", "detector": "...", "patch_budget": 512, "precision": "auto-balanced", "pools": {"stage_workers":{},"queue_capacity":{},"stage_device":{}}},
  "started_at": "2026-10-01T12:00:00Z", "finished_at": null,
  "waiting_for_queue": true,
  "sample": {"pages": 32, "volumes": 9},
  "host": {"cpu": "...", "gpu": "...", "backend": "...", "devices": {"detect": "cpu", "engine": "gpu:0"}},
  "tunable": true,
  "progress": {"trial": 2, "max_trials": 8, "pages_done": 10, "pages": 32, "stage_workers": {}, "pages_per_second": 4.2, "pass_index": 1, "window_seconds": 6.0, "pages_measured": 20},
  "startup_seconds": 12.3,
  "trials": [ { "n": 1, "note": "auto", "stage_workers": {}, "queue_capacity": {}, "stage_device": {}, "seconds": 22.1, "pages_per_second": 4.1, "window_seconds": 20.3, "pages_measured": 80, "passes": 3, "short_window": false, "first_emission_at": 0.0, "last_emission_at": 0.0, "accepted": true, "verdict": null, "bottleneck": "engine", "stages": [], "queues": [], "precision": "bf16", "gpu_busy_pct": 71.0, "cpu_busy_pct": 33.0 } ],
  "baseline": {"pages_per_second": 4.1, "seconds_per_page": 0.2439, "window_seconds": 20.3, "pages_measured": 80, "passes": 3, "short_window": false, "first_emission_at": 0.0, "last_emission_at": 0.0},
  "best": {"trial": 3, "stage_workers": {}, "queue_capacity": {}, "stage_device": {}, "pages_per_second": 5.0, "seconds_per_page": 0.2, "speedup": 1.22, "window_seconds": 20.1, "pages_measured": 90, "passes": 3, "short_window": false, "first_emission_at": 0.0, "last_emission_at": 0.0, "same_as_spec": false, "gpu_busy_pct": 70.0, "cpu_busy_pct": 35.0},
  "precision": "bf16", "precision_mode": "auto-balanced",
  "precision_trials": [{"precision": "bf16", "pages_per_second": 5.0, "chosen": true}],
  "precision_why": "benchmark: bf16 5.00 p/s beat fp32 3.10 p/s",
  "peak_rss_mb": 4100, "peak_vram_mb": 1900,
  "estimates": {"volume_200_pages_seconds": 40, "remaining_pages": 1200, "remaining_seconds": 240},
  "preempted": [],
  "error": null,
  "position": 0, "queue": {"running": "g-1", "queued": []}
}
```

Facts: `state` transitions `queued -> running -> done|failed|cancelled`; `snapshot()` nulls `progress` unless `running`. `started_at`/`finished_at` are UTC ISO-8601 with seconds and `Z` (`_now_iso` `:128-129`). `tunable = !measured.monolithic` initially, then overwritten by the runner's `bench_ready.tunable` (False when nothing on the road can be widened or the run is `precision_only`; then `max_trials` is 1 + precision phase). `host` = `describe_host` for local; the processor's registered host for remote; plus `devices` = where each model actually came up (runner `bench_ready.stage_device`). `spec` = `_spec_payload(measured)`: `engine`, `detector` only if not None, `patch_budget`, `precision` only if `precision_applies` and not default, `pools` (never name/primary/enabled/id). `estimates`: `volume_200_pages_seconds = round(200 / pages_per_second)` (`BENCH_VOLUME_PAGES=200`, mirrored in `admin.js`), `remaining_pages` = pages still owing this row a sidecar (from the library index + metadata page-count cache, no archive is opened; `None` if unknown), `remaining_seconds = round(remaining / pps)`; both seconds are None when `pps` is missing/<=0. **Startup/model-load time is never folded into a rate or estimate**; it is reported once as `startup_seconds` ("first page after X s").

`best.same_as_spec` = True when applying `best` would change nothing: compares `{best.stage_workers}`, `{best.queue_capacity}`, `{best.stage_device}` (after the "apply" completions below) against the **measured spec's** pools (int-coerced except the literal `"auto"`).

"Apply" completions on `bench_done` (`:1265-1330`): the runner's `best` holds only differences from the derivation; the server completes them into whole tables so applying is `pools = best`: `stage_workers`/`queue_capacity`: start from the runner's table; for each width/capacity the **spec pins** but the search left alone, use the pin if the winning trial actually ran at that value, else the literal `"auto"` (`POOL_AUTO`) - never persist a value that was not measured. `stage_device`: spec pins first (`ran` value replaces a pin the runner did not honour: `explicit = pin not in ("", "auto")`, `ran = host.devices[key]`), then the runner's moved stages. The runner is **told** the spec's `stage_device` (`--stage-device`) but not its widths/capacities (`open_bench`: it measures the machine, not the hand-tuning).

Precision fields: only set when `measured.precision_applies` and the runner's `precision` is one of `bf16|fp16|fp32`; `precision_mode` = the runner's mode; `precision_trials` (list of `{precision, pages_per_second, ...}`) + `precision_why` only for benched modes. A precision is never part of `best`/pools: stale keys `precision`, `precision_auto`, `precision_ran`, `card_family` are popped from `best`.

Utilization: local trials get `gpu_busy_pct`/`cpu_busy_pct` from a once-a-second sampler whose means are taken over **that trial's window only** (`first_emission_at`/`last_emission_at`, which the runner reports in seconds since its own start; the server adds the spawn instant taken before the spawn); a remote trial already carries its machine's numbers (only fills missing, `trial.get(k) is None`). `best` takes the winning trial's busy numbers (`setdefault`) - never another trial's.

### 6.5 Persistence

`<storage>/.ocr-bench.json` (`BENCH_FILE`): one JSON object `{generation_id: <result object as above, state "done">}`, `indent=2`, `ensure_ascii=False`, written to `<name>.tmp` then `os.replace` (atomic). Written **before** the in-memory state turns `done` so a poller never sees `done` without the file (`_finish` `:1205-1230`). Saved only when state is `done`, not a draft, not remote, and not `precision_only` (a precision-only run's pick lives only in the machine profile). The saved object is the snapshot plus `state:"done"`, `finished_at`, `progress:null`, `waiting_for_queue:false`. Loading ignores non-dict top level and non-dict values; any I/O/JSON error -> `{}`. Every `_save` also prunes to the **currently configured row ids** (+ the id being saved); `prune(known_ids)` drops other rows and **deletes the file** when nothing remains. `PUT /api/ocr/generations` calls `prune` when the list changed. `saved_summaries(rows)` (the generations table): `{id: result minus "trials", with "progress": null}` (trials are fetched per row).

Per-machine profile (`<storage>/processors/<file>.json`, `ocr/remote/profiles.py`): for **remote** benches and for the **local** auto-bench (`profile=LOCAL_PROFILE`, file `@local.json`, key `" local"` which no processor name can equal), `_persist_remote` stores into `rows[<id>].bench` (never `.ocr-bench.json`; drafts never stored):

```json
{"pages_per_second": 5.0, "window_seconds": 20.1, "gpu_busy_pct": 70.0, "cpu_busy_pct": 35.0,
 "startup_seconds": 12.3, "host": {...}, "at": "2026-10-01T12:00:00Z",
 "precision": "bf16", "precision_mode": "auto-balanced",
 "precision_trials": [...], "precision_why": "..."}
```
(precision keys only when present; trials+why only with trials). If `autobench` and not `precision_only` and `best.same_as_spec` is false, `best` is also written as that machine's **pools** via `set_pools(..., recipe, keep_existing=True, autobench=True)`: only into a pair that has no pools yet (`holds_pools`: any table non-empty), recorded by an empty `pools_autobench: {}` marker (a person's later save clears it); precision never goes into pools.

Profile file layout (`profiles.py`): `{"name": <machine>, "account": <owner>, "host": {...}, "catalog": {...}, "rows": {"<gen id>": {"recipe": [engine, effective_detector, patch_budget|null], "pools": {...}, "pools_autobench": {}, "bench": {...}, "runs": {"volumes","pages","seconds","pages_per_second","recent":[{pages,seconds,at}],"last_at","contended","contended_last_at","congestion":[...]}}}}`; written `indent=2` via tmp + `os.replace`, under one process-wide lock (`_WRITE_LOCK`); file name = the name itself if `[a-z0-9][a-z0-9._-]*` and <=64 chars, else `<readable<=40 of [^a-zA-Z0-9._-]+ -> "_", stripped of "._", or "processor">~<sha256(name utf-8 surrogatepass)[:16]>.json`; `@local.json` for this server. `RUNS_KEPT=5` congestion entries. Machine profile prune on row deletion (`prune`). The `account` field: a machine name belongs to the processor account that first registered it (`claim`). (Registry/protocol detail is the processor spec's; recorded here because the bench writes into it.)

### 6.6 Staleness

- **Recipe staleness (per-machine rows, KEEP):** `ProcessorProfiles.row(name, id, recipe=...)` returns None (absent) when a recipe is supplied, stored is not None and `stored != recipe_key(recipe)`; the next write replaces the whole entry (`_row` resets it). The recipe is `output_affecting()` = `[engine, effective_detector, patch_budget-or-null]` - so changing engine/detector or hayai's patch budget wipes that row's pools, bench and runs on every machine.
- **Precision staleness** (`stale_bench_reason(engine, bench, mode, supported)` `profiles.py:254-323`), applied inside `row(...)` for engines in `PRECISION_ENGINES`; a stale bench reads as `None` + `stale_bench=True` (the pair is "unmeasured", re-benchmarked where autobench is on, dropped from the file at its next save keyed by the bench's `at`; logged once per `(machine,row,at)` at INFO). Rules in order:
  1. `ran = bench.precision` (or the trial marked `chosen` in `precision_trials`); none and not mokuro -> stale `"it records no precision (measured before the precision modes)"` (mokuro special-case DROP).
  2. `bench.precision_mode` present, valid, and `!= row mode` -> `"it was measured for {X}; the row asks {mode} now"`.
  3. `supported` unknown (machine never reported): judged only when every device kind (`{fp32}`, `{fp32,fp16}`, `{fp32,fp16,bf16}`) that is eligible gives the same single answer and the mode is not benchmarked: stale if `ran != that answer`; otherwise kept.
  4. Else `resolved = resolve_mode(engine, mode, supported)`: ineligible -> kept; benched mode with >1 usable candidate -> stale unless `precision_mode == mode` and the set of trial formats equals `set(usable)` (`"its precision trials were {sorted tried}; this machine's candidates for {mode} are {usable}"`); fixed/unique result -> stale if `ran != resolved.precision` (`"it ran at {ran}; this machine runs {p} now"`).
  The device formats used are the row's model device's `supported_for` on that machine's catalog (admin passes it), or `recorded_formats` (if the bench's `host.devices.engine == "cpu"` -> `{fp32}`, else the profile's stored `catalog`'s `auto` support).
- **Row-level `.ocr-bench.json` has no staleness check**: it is only pruned by row id. An engine/detector/patch_budget edit does **not** clear it (the UI shows its `spec` echo). Only per-machine profile entries are recipe-keyed. (Possible latent inconsistency; port as-is unless the owner wants it fixed - OQ-8.)
- ETA/throughput models do not read bench staleness; they use `RateModel` evidence (other spec).

### 6.7 The runner `--bench` search (server only consumes events; algorithm here for the port decision)

Events (`engine_runner.py:9258-9330`, `8959`, `8762`) consumed by `_read_composed`: `bench_ready{startup_seconds, model_load_seconds, min_window_seconds, pages, tunable, max_trials, stage_keys, stage_device}`, `bench_progress{trial, pages_done, pages, stage_workers, pages_per_second, pass_index, window_seconds, pages_measured}` (>= 1 s apart), `bench_trial{...BenchTrial.as_dict}`, `bench_done{baseline, best, precision, precision_mode, precision_trials, precision_why, peak_rss_mb, peak_vram_mb}`, `fatal{error}`, `exit{returncode}`. Failure texts: runner will not start -> its first event's `error` or `"the runner would not start"`; budget exceeded -> `"the benchmark ran past its time budget and was stopped"`; ended without `bench_done` -> `"the {name} benchmark ended[ with status N][: {fatal|stderr tail}| before it produced a result]"`.

Algorithm constants: warm-up `BENCH_WARMUP_PAGES=4` discarded; fill = first `min(8, N//4)` emissions of the first pass dropped; **rate = `(M-1) / (t_last - t_first)` over the remaining M emissions** (page-emission timestamps, never a stopwatch around a process; nanosecond mtimes on the monolithic path); a trial re-feeds the sample as one continuous run until its measured window reaches `BENCH_MIN_WINDOW_SECONDS=20` or `BENCH_MAX_PASSES=8` feeds (`BENCH_MAX_REPEAT=64` repeats per feed; a feed's window counts as steady-state only if it spans >= `BENCH_WINDOW_IS_THE_FEED=0.25` of the feed); a window under `BENCH_SHORT_WINDOW_SECONDS=10` is reported with `short_window:true` and **never decided on**; search: widen the stage the pipeline's own verdict names, within structural ceiling and host budget, keep a step only if pages/s improves by >= `BENCH_GAIN=3%`; then narrow, keeping a narrower width if throughput holds within `BENCH_HOLD=1%`; at most 8 trials, 900 s; `best.stage_workers` holds only differences from derived widths; `best.queue_capacity` is **always `{}`** (capacities derive from widths). The precision phase (benched modes with >1 usable candidate on a torch recognizer on a card) runs one trial per candidate on the same sample, re-casting from an fp32 master between trials, extends `max_trials` by the candidate count (does not eat the width search's budget), and `pick_precision` (5% tie -> earlier/more accurate) decides; `precision_only` skips width search (`tunable:false`). The server-side copy of the rate arithmetic is `emission_window` (`bench.py:497-521`) returning `{pages_per_second, window_seconds, pages_measured, passes, short_window, first_emission_at, last_emission_at}`.

### 6.8 Auto-bench

`ocr.autobench` (default true; startup setting): a `(row, machine)` pair never measured (or whose bench is stale) is benchmarked before that machine is offered the row's volumes, widths found are stored in that machine's profile (not config). On this server's own hardware auto-bench of **width** happens only for a row whose `pools` table is empty (hand-set pools are used as written); a machine with hand-set pools gets a `precision_only` bench for balanced/speed modes (`OCRWorker.autobench_kind`). A failed bench -> pair runs untuned/first supported candidate; surfaced as `bench: "failed"` in `precision_on`. `autobench` field is also echoed in `GET /api/ocr/generations`.

---

## 7. Installer (`ocr/installer.py`, `ocr/hf_cache.py`) - mostly DROP

The whole subsystem builds Python venvs (`OCRInstaller` for the mokuro venv; `EnginesInstaller` for the engines venv with torch + transformers) and picks torch wheels by hardware. **DROP entirely** in the Rust port: `OCRBackend` enum, `HardwareInfo`/`detect_cuda|rocm|mps|hardware`, `get_backend_unavailable_reasons`, `get_supported_backends`, `get_recommended_backend`, `_ROCM_WHEEL_CHANNELS` (7.1/7.2 -> `rocm7.1`, 6.3, 6.4; nightly index for Python >= 3.13), `get_torch_install_command`, `MOKURO_PACKAGE_SPEC` (`mokuro @ git+https://github.com/Gnathonic/mokuro.git@feat/serve-mode`, override `MOKURO_BUNKO_MOKURO_SPEC`), `MOKURO_INSTALL_PACKAGES`, the venv creation/verification snippets, `install_detector`, `install-ocr` CLI, `rocm_gfx.py` HSA override, `OCR_CLI_HINT`/`OCR_DRIVER_HINT`/`OCR_NO_LOCAL_HINT` (these three texts are shown by the admin runtime status; replace with Rust-appropriate text if the field stays).

What is **not** dropped, because it is behaviour the port still needs:

### 7.1 Model files to obtain

After the DROPs the artifacts a server needs are:

| engine | needs |
|---|---|
| ppocr-manga (engine and the detector for the other two) | HF repo `Kellenok/PP-OCRv6_manga` at `ba1d479e8a61a20e8318c9758c73fbbbd290b98d`: files `det/manga_det_v0.2.onnx`, `rec/manga_rec_v0.2.onnx` (fp32, default; 23 MB total) and `ppocrv6_dict.txt`; optional fp16 files `det/manga_det_v0.2_fp16.onnx`, `rec/manga_rec_v0.2_fp16.onnx` (11 MB; **slower** on CPU - fp16 files take fp32 input and cast per op, 0.34 vs 0.30 s/page on v0.1, and read 19/322 lines differently; default stays `fp32`) |
| hayai-nova | recognizer `JustANormalTinkerer/hayai-ocr-v2.5-nova` + image processor config from `google/siglip2-base-patch16-naflex` (hayai's repo ships no preprocessor config) - **torch/transformers form today; ONNX export needed (OQ-4)** |
| paddle-manga | `sorryhyun/paddleocr-vl-1.6-manga-lora` on base `PaddlePaddle/PaddleOCR-VL-1.6` - **torch/peft today; ONNX export needed (OQ-4)** |

PP-OCR model resolution (`ppocr.py:290-390`, KEEP): dict `ppocrv6_dict.txt`; `MODEL_FILES = {fp32:(det,rec), fp16:(det,rec)}`; precision from arg, else env `MOKURO_PPOCR_PRECISION`, else `fp32` (unknown -> `unknown precision '{p}' (known: fp32, fp16)`); models dir from arg else env `MOKURO_PPOCR_MODELS`; search order per file: `<dir>/<repo-relative path>` then `<dir>/<basename>` (flat); anything found locally makes `pinned=False` (sidecar then **must not claim the commit**); anything missing is downloaded (unless disabled by arg/env `MOKURO_PPOCR_DOWNLOAD` in `{0,false,no,off}`) via `hf_hub_download(repo, filename, revision=<pin>[, local_dir])` into the dir if configured else the shared HF cache; with download off and a missing file -> `FileNotFoundError("PP-OCR manga model file '{rel}' not found in {where} and downloading is disabled; copy it from https://huggingface.co/Kellenok/PP-OCRv6_manga")` (`where` = the dir or `$MOKURO_PPOCR_MODELS (unset)`). Env `MOKURO_PPOCR_THREADS` sets the ORT intra-op threads. CTC vocab: one symbol per line (strip only `\r`, drop one trailing empty from the final `\n`), classes = `["", *symbols, " "]` (index 0 blank, last a space). **KEEP all of this** - it is the offline/air-gapped story: an offline server fails with a message naming the files instead of hanging on the network.

### 7.2 Offline cache decision (`hf_cache.py`) - ADAPT

0.5.2: if every repo a row loads is fully present in the Hugging Face hub cache, the runner env gets `HF_HUB_OFFLINE=1` and `TRANSFORMERS_OFFLINE=1` (only when neither is already set, `processor.py:1898-1919`) so a session start makes no Hub round trips (saves ~0.8-0.9 s/start and a network dependency). Cache dir resolution (`hub_cache_dir`): `HF_HUB_CACHE` or `HUGGINGFACE_HUB_CACHE`, else `$HF_HOME/hub`, else `$XDG_CACHE_HOME or ~/.cache` + `/huggingface/hub`. `repo_cached(repo, revision, cache)`: folder `models--<org>--<name>`; revision None reads `refs/main`; requires `snapshots/<revision>` to be non-empty AND no `*.incomplete` blob in `blobs/` (an interrupted download). `row_repos(engine, detector)` = recognizer + secondary repo at pinned revisions (hayai: vision repo; paddle: base repo) + the detector repo (ppocr: `Kellenok/PP-OCRv6_manga` at its pin); None for unknown engines (stay online). In Rust, with no `huggingface_hub`, decide the model store (OQ-4): at minimum keep (a) pinned-revision download with SHA verification, (b) never re-hit the network when files are present, (c) a documented offline directory override equivalent to `MOKURO_PPOCR_MODELS`.

### 7.3 Startup behaviour to preserve semantically

Server installs "what the configured generations need at startup"; admin `PUT generations` returns `installing`/`restart_required` (section 8.3) when a newly needed environment is missing. With no environments, this becomes "model files needed by enabled rows are present, else downloading in background" - keep the response fields (`applied`, `installing`, `restart_required`, `reason`) so the admin UI keeps working (OQ-6). `OcrControl.apply(generations, poll_interval)` returns exactly those four fields; "environment missing here" is `applied: true` AND `restart_required: true` with a reason (the library's own hardware is just another processor entry, so rows stay queued for processors that can run them); OCR disabled in the process -> `restart_required: true, reason: "OCR is disabled in this server process"`.

---

## 8. Admin API: OCR endpoints (`admin/api.py`)

All paths are relative to the admin mount `config.path` (prefix stripped, then `/api/...`). **All are admin-role only** (`_can_access_api` `:467-472`: only `/api/invites*` admits `inviter`); non-admin -> `403 {"error": "Admin access required"}`. Bodies are JSON, max 64 KiB (`MAX_JSON_BODY_BYTES`); invalid JSON/over-size -> `400 {"error": <message>}` (message from `_parse_json_body`). `500 {"error": "Config not available"}` when the server has no full config. Responses are `application/json`. Any route not matched -> `404 {"error": "API endpoint not found"}`.

Routing (`:536-562`, in this order of match): `PUT /api/settings/ocr`; `GET /api/ocr/generations/stats`; `GET|PUT /api/ocr/generations`; `POST /api/ocr/generations/derive`; `POST /api/ocr/devices/refresh`; `PUT /api/ocr/generations/<id>/pools`; `* /api/ocr/generations/<key>/bench` (POST/GET/DELETE). Also OCR-related: `GET /api/settings` (config with `ocr_runtime` and masked dyndns token), `GET /api/processors`, `GET /api/status`.

### 8.1 `GET /api/settings` (OCR part)

Full config dict (`Config.to_dict`; OCR section: `{"backend","poll_interval","concurrency","sessions"?,"local_processing"?,"autobench"?,"generations":[row.to_dict()...]}` - `config.py:470-480`), plus `ocr_runtime` = cached `build_ocr_runtime_status`:
`{"available": bool, "launch_only": true, "configured_backend", "installed", "installed_backend", "env_path", "generations": [...], "detectors": [...], "engines_env_path", "engines_installed", "detector_ready", "supported_backends": [...], "unavailable_backends": {...}, "cli_hint", "driver_hint"[, "active_engines": [...]]}` (`:333-374`, `active_engines` added by `_refresh_ocr_runtime_cache`). **ADAPT/mostly DROP**: venv/backend fields have no meaning. Keep `available`, `generations`, and decide which of the rest the UI still needs (OQ-6).

### 8.2 `PUT /api/settings/ocr`

Body keys handled: `poll_interval` (int >= 1; else `400 {"error": "poll_interval must be a positive integer"}`). Rejected: `backend` -> `400 {"error": "OCR backend is launch-only. Use CLI flags/config file to change it."}`; `char_map` -> `400 {"error": "char_map was removed with the character-map system (no per-character placement mode produced output worth using; readers lay characters on a uniform grid) -- delete the key"}`; any of `engines`, `detector`, `patch_budget` -> `400 {"error": "<comma list> moved into the generations list; PUT them to /api/ocr/generations"}`. Checked in that order, under a config lock. Saves config; if `poll_interval` changed and a control handle exists, `ocr_control.apply(generations, poll_interval=float)`. Response `200`: `{"success": true, "ocr": {"backend","poll_interval","concurrency"}, "applied","installing","restart_required","reason", "ocr_runtime": {...}}` (outcome defaults `{"applied": false, "installing": false, "restart_required": <changed>, "reason": ""}`).

### 8.3 `GET /api/ocr/generations` and `PUT /api/ocr/generations`

**GET** -> `200` `_generations_payload`:

```json
{
  "stats_pending": true,
  "generations": [ <entry>... ],
  "catalog": { ... },
  "processors": [ <ProcessorEntry.to_dict> ... non-local ],
  "local_processing": true,
  "autobench": true
}
```

Each entry (`_generation_entry` `:2005-2033` + payload additions `:1672-1760`) = `row.to_dict()` (with `detector` defaulted to `null` if absent) plus:
`sidecar` (`"<Volume>.mokuro"` or `"<Volume>.<name>.mokuro"` - the literal text `<Volume>`), `effective_detector` (= `reported_detector`), `detector_locked`, `patch_budget_applies`, `precision_applies`, `road` (`"line"|"reconciled"`; null for the DROPped monolithic case), `stages` (see below), `volumes_done`/`volumes_total` (null while stats pending), `congestion` (averaged recent runs, `average_runs`), `processor_stages` (`{machine: stages[]}` for each connected non-local processor whose pools still parse), `volumes_skipped`, `volumes_by_machine` (`{machine: n}`), `bench` (saved summary without trials, or null), `precision` + `precision_on` (only when `precision_applies`), `precision_hold` (reason string or null), `configured` (`!row.pools.is_empty()`), `local_pools`, `local_bench`, `local_runs` (from `@local.json`; null/absent when none), `processor_pools`, `processor_bench`, `processor_runs`, `processor_congestion` (each `{machine: ...}` only for machines with data).

`stages[]` (`_stage_rows` `:176-306`; the monolithic one-stage form DROP): one entry per stage of the road, in order:
`{"key","name","device","max_workers","derived_workers","derived_capacity","devices_allowed","device_options":[{"id","label"}],"device_locked_reason","workers_means"}`. `key`/`name`/`device`/`max_workers` come from the runner's resolved `road_specs(road, detector, engine, gpu, devices=row.pools.stage_device, ort_gpu)` (the declared graph with this row's device pins applied); `derived_workers`/`derived_capacity` = what `stage_widths`/`stage_capacities` would use with this row's pools and the host worker budget (`host_worker_budget(jobs=ocr.concurrency)`); `device_options` labels: `auto` -> `Auto → {CPU|GPU n}` (arrow `→`, what auto resolves to with the stage left on auto), others -> `catalog.label_for(id)`; `workers_means`: `"copies"` for the `engine` stage when its device is a GPU, else `"pool"` (`"engine"` was for the served road: DROP). Stage names (KEEP verbatim from `STAGE_GRAPHS`): `line`: `detect`="detect + CTC read", `layout`="layout + dump"; `reconciled`: `detect`="detect + CTC read", `engine`="engine read + reconcile", `post`="layout + dump".
Declared stage costs (seconds/page, used by width derivation; `engine_runner.py:2549-2620`): `line.detect 0.19`, `line.layout 0.011`; `reconciled.detect 0.225`, `engine 0.30` (device-bound), `post 0.011`; per-engine overrides in `ENGINE_STAGE_SECONDS` (paddle-manga engine 0.915 s/page, more on novel prose). Width derivation, `host_worker_budget`, `CPU_WORKERS_MAX=4`, `MAX_ENGINE_COPIES=8`, `STAGE_WIDTH_HEADROOM=2.0` are runner-spec material; the admin only echoes their result.

`catalog` (`_generations_catalog` `:2217-2262`):

```json
{
  "engines": [{"id","label","monolithic":false,"served":false,"own_environment":false,"own_detector":null|"ppocr-manga","patch_budget":bool,"precision":bool,"precision_modes":[...],"devices":["cpu"]|"any"}],
  "detectors": [{"id","label","devices":["cpu"]|"any"}],
  "devices": [{"id","label"}],
  "patch_budgets": [256,384,512],
  "precision_modes": [{"id","label"}], "precision_default": "auto-accuracy",
  "name_pattern": "^[a-z0-9][a-z0-9-]{0,31}$",
  "reserved_names": ["original","gcv","updated-ocr"],
  "reserved_prefixes": ["tr-"]
}
```
(`monolithic`/`served`/`own_environment` fields: after the DROPs always false; **keep emitting** them as false or coordinate removal with the frontend - OQ-10.) `devices:["cpu"]` for engines/detectors with `cpu_only_reason` (and onnxruntime-GPU-less hosts for ORT detectors: DROP). `catalog.devices` is the merged catalog of every machine (`_settable_devices`), not just this server.

Stats are computed in a background thread, single-flight, cached `GEN_STATS_TTL_SECONDS=60`; the list waits at most `GEN_STATS_LIST_WAIT_SECONDS=0.5` for a first computation, the stats endpoint `GEN_STATS_WAIT_SECONDS=5`. Cache key = `[(id,name,primary,enabled)...]`. Counts: `done` per row = volumes with the row's sidecar (primary: `has_mokuro or has_mokuro_gz`; else `row.name in volume.sidecars`) out of `total` = volumes with a `.cbz`; `skipped` for non-primary rows = volumes known short of pages (`cached_missing_pages > 0`, only checked in series the metadata pass flagged `missing_pages>0 or damaged_volumes>0` or has not compiled) lacking the row's sidecar; primary is never skipped; `by_machine` = `attribute_volumes(...)` (5.3).

**`GET /api/ocr/generations/stats`** -> `200 {"stats_pending": true}` if none yet; else `{"stats_pending": false, "computed_at": "<UTC %Y-%m-%dT%H:%M:%SZ>", "generations": {"<id>": {"volumes_done","volumes_total","volumes_skipped","volumes_by_machine"}}}`. `503 {"error": "Config unavailable"}` without config.

**PUT /api/ocr/generations** body `{"generations": [...]}` (full replacement; ORDER is the setting, so no patches). Errors always `{"error", "row", "field"}`:
- invalid body -> `400 {"error": <msg>, "row": null, "field": null}`.
- missing key -> `400 {"error": "generations is required (a list of rows, in run order)", "row": null, "field": null}`.
- a row with an `id` that matches no existing row -> `400` `ocr.generations[{i}]: id {id!r} is not a generation this server knows; leave id out for a new row`, `row:i`, `field:"id"` (it would silently detach history and a running job). Rows without id are new (id minted).
- `parse_generation_list(rows, devices=_settable_devices())` - validated against **every** machine's cards: this server's (when `ocr.local_processing` and backend != `skip`), each connected processor's, and each remembered processor profile's (`catalog` in `processors/*.json`); unprobed catalogs contribute nothing. `GenerationConfigError` -> `400 {"error","row","field"}`; other `ValueError` -> `400 {...,"row":null,"field":null}`.
- On success: `changed = [to_dict...] != [current to_dict...]`; replace and **save config file**; `ocr_control.apply(parsed, poll_interval)` if changed; if changed, `bench.prune(ids)` and `ProcessorProfiles.prune(ids)`; respond `200 {"success": true, ...<GET payload>..., "applied","installing","restart_required","reason", "ocr_runtime": {...}}` (the apply outcome defaults to `{applied:false, installing:false, restart_required:changed, reason:""}` without a control handle).
- All under `_config_lock`.

### 8.4 `POST /api/ocr/generations/derive`

Body `{"spec": {...}, "processor": "<name>|null"}`. Returns `200 {"road": spec.road, "stages": [...]}` for a spec that is not saved - same `stages[]` as GET, for the row as currently edited. Read-only; touches no config. With a `processor` (not `"local"`): `_machine(name)` = that machine's devices, host budget, GPU flag; unknown -> `400 {"error": "no processor called {name!r} is known"}`. A machine's catalog comes from the connected registry entry (connected first), else the one remembered in `processors/<name>.json`; backend from `host.backend`, else GPU iff it has cards; budget from its host line `(N cores)` and `max_sessions` (`host_worker_budget(cpus, jobs=sessions)`). `parse_bench_spec` errors -> `400 {"error","row":null,"field"}`. Local: `devices = _settable_devices()` (labels from this server's catalog), budget `host_worker_budget(jobs=ocr.concurrency)`, `gpu = backend_is_gpu(selected_backend)`.

### 8.5 `PUT /api/ocr/generations/<id>/pools`

Body `{"processor": "<name>", "pools": {...}}`. Writes **one machine's** pools override into its profile; never touches config; takes effect at that processor's next session (pools are not output-affecting). Errors `400 {"error"}`: processor missing/blank/`"local"` -> `"processor must name a processor"`; unknown generation -> `"there is no generation {id!r}"`; unknown machine -> `"no processor called {name!r} is known"`; `pools` not an object -> `{"error": "pools must be an object", "field": "pools"}`; validation error -> `{"error","row":null,"field"}`. `"auto"` (`POOL_AUTO`) in `stage_workers`/`queue_capacity` is accepted ("derived on this machine, where the row pins one"): validated as width 1 (so unknown stages are still refused) then stored as the literal `"auto"`. Validated against **that machine's** catalog via `parse_bench_spec({**row.to_dict(), "pools": checked}, devices=machine.devices)`. Stored via `set_pools(name, id, pools, recipe=row.output_affecting())`. Response `200 {"success": true, "pools": {"stage_workers","queue_capacity","stage_device"}}` (no precision, ever). `machine_pools(stored, own)` composes the effective table **table by table**: a non-empty stored table replaces the row's wholesale (missing key = derived there); an empty stored table says nothing, so the row's own table applies. `runner_pools` strips `"auto"` widths/capacities (absent key = derive; stored `0` is a serial width, not derived).

### 8.6 `POST /api/ocr/devices/refresh`

Clears the cached catalog and re-probes (engines env in 0.5.2). `200 {"success": true, "devices": catalog.entries()}`. Probe never raises: a missing environment gives the fallback `auto`+`cpu`, `probed:false`.

### 8.7 `POST|GET|DELETE /api/ocr/generations/<key>/bench`

`key` = saved id or draft key `^draft-[a-z0-9-]{1,24}\Z`. (Any path ending `/bench` under `/api/ocr/generations/` matches; `<key>` is everything between.) No config -> `500 {"error": "Config not available"}`.
- **POST** body `{"spec"?: {...}, "pages"?: int, "processor"?: "<name>"}`; a body that fails to parse is treated as `{}`. Calls `enqueue(key, spec, pages, processor=str(processor or "local"))`; returns **`202`** + the bench object (section 6.4).
- **GET** `?processor=<name>` (default `local`) -> `200` bench object (live, recent, saved, or idle).
- **DELETE** `?processor=<name>` -> `200` bench object after cancel.
- `BenchError` -> `e.status` (400 or 409) with `{"error": message, "row": e.row|null, "field": e.field|null}` (row always null for spec errors).
- Other method -> `404 {"error": "API endpoint not found"}`.

### 8.8 `GET /api/processors` (OCR-adjacent; detail in the processor spec)

`200 {"processors": [entry.to_dict() + "host" (local: this server's probed `{cpu, gpu}`, background-probed once) + "cannot_start" (backoffs of rows whose runner will not start there)], "speed": ..., "failed_logins": [{username, reason, at}], "last_disconnect": {"name","at"}|null, "local_processing": bool, "processing_hold": ...}`. Entry shape: `{processor_id, name, label, username, host, catalog, max_sessions, sessions, connected_since, last_seen, installing, local, public_name, transfer:{volumes, mb_per_s, resumed, restarted, repaired, damaged, returned, returned_by_class, last_returned, held_until, held_error}}` (`remote/registry.py:132-149, 316-335`). `_hardware(host)` reduces a host dict to `{"cpu": str|None, "gpu": str|None}` (None if both empty). Listed here only because it carries the device/host facts above; the registry/protocol is another spec.

---

## 9. Behaviours with no obvious home that a port must not lose

- **List order = queue order = OS-priority order**; reordering via PUT changes who runs first.
- **Exactly-one-primary** invariant is validated at every entry point (config load, env, CLI set, admin PUT).
- **Never silently map a removed thing**: `char_map`, retired `ocr.engines/detector/patch_budget`, removed detectors, unknown engine ids are hard errors naming the offender. A server that refuses to start prints the message verbatim.
- **Name immutability of seeding**: seed once, store forever.
- **A rename or `primary` flip is not output-affecting**; pools/precision edits never cancel a job; only engine/effective-detector/patch_budget (hayai) do.
- **Bench never persists what it did not measure** (pins replaced by what ran; `"auto"` sentinel for unmeasured pinned widths/capacities).
- **Provenance never fails a result**; sidecar claims only weights actually loaded and only pinned downloads.
- Atomic JSON writes (`tmp` + `os.replace`) for `.ocr-bench.json` and profiles; UTF-8; `ensure_ascii=False`; `indent=2`. Sidecar JSON itself is compact (`separators=(",", ":")`).
- Admin error shape: generations/bench errors carry `row` and `field`; others only `error`. The UI highlights `ocr.generations[{row}]` / `field`.
- Em dash (`—`) and arrow (`→`) characters appear in user-facing strings (device labels, a few errors); preserve them byte-for-byte if any tests/UI compare text.

---

## Open questions

1. **Default generation / mokuro migration.** `DEFAULT_GENERATION` is mokuro-primary and an empty `ocr.generations` means that. With mokuro dropped: which row is the unset default (`ppocr-manga` primary? `hayai-nova` primary with `ppocr-manga` detector?) and what is the error/migration message for an existing config or live `Volume.mokuro` provenance naming `mokuro`? Existing libraries have `<Volume>.mokuro` written by mokuro; the primary row's engine only changes future writes, but the primary row's identity (`g-1` name `mokuro`) persists in `.ocr-bench.json`, profiles, congestion history, failure records.
2. **Precision model under ORT.** `bf16/fp16/fp32` policy and `supported_formats` are torch-specific. What does a "format" mean for ONNX exports (separate model files per dtype? ORT session options? EP-specific fp16)? How is "bf16 supported" probed with `ort`? Does the policy table (accuracy-measured on torch) carry over, or must it be re-measured? Does `gpus[].formats` stay on the processor wire?
3. **GPU for ONNX stages.** 0.5.2 locks all PP-OCR stages to CPU. Should the Rust port allow GPU execution providers (CUDA/ROCm/MIGraphX/DirectML) for the PP-OCR detector/recognizer? That revives `ort_gpu_providers`, the "no GPU provider on this host" lock reason, and changes `stage_lock_reason`/`device_locked_reason` and `engine` stage placement rules.
4. **Models and pins.** hayai-nova and paddle-manga (+ LoRA via peft) are torch/transformers `trust_remote_code` models today; the port needs ONNX exports. Where do they come from, how are they pinned (HF repo+commit for the ONNX artifacts? sha256 of files?), and what do `ocr_engine.recognizer` and `ocr_engine.weights` say in new sidecars (keep the old repo ids and SHAs, or name the ONNX artifact)? Does the SigLIP2 preprocessing (`HAYAI_VISION_REPO`) get re-implemented in Rust? Offline cache layout/override (equivalent of `MOKURO_PPOCR_MODELS`, `HF_HUB_*`) and whether to keep reading the HF hub cache layout.
5. **`runner_build` / `runner_digest`.** There is no staged Python runner; what replaces the content-hash digest (git sha, cargo pkg version, binary hash)? Format stays `"mokuro-bunko <v>, runner <digest>"`? (`generator` in `ocr_engine` is `"mokuro-bunko <version>"` - bump to the Rust version string or keep emitting the Python-compatible string?)
6. **Backend / environment concept.** `ocr.backend` (`auto|cuda|rocm|cpu|skip`), `ocr_runtime` (installed/venv/env_path/supported_backends/driver_hint), `installing`/`restart_required` semantics, `--ocr` CLI flag, `install-ocr`, `OCR_*_HINT` texts: which survive as EP selection and "models downloaded" status, and what does the admin UI expect for these fields? `restart_required` after adding a row that needs an unavailable model.
7. **Bench scope.** Port the full runner `--bench` search (width widening/narrowing, precision trials, 20 s windows) or only the measurement + persisted result shape? Autobench and the profile writes depend on it. Is `tunable` ever false except `precision_only`? Are remote processors in scope for benches at all (`RemoteBench`, `/_processor/<id>/bench/<bid>/sample`)?
8. **Row-level `.ocr-bench.json` staleness.** It is never invalidated by recipe/precision-mode edits (only pruned by id). Port as-is, or invalidate on `output_affecting` change like the profiles (a behavioural fix)?
9. **Profile/recipe compatibility.** Keep `recipe = [engine, effective_detector, patch_budget|null]` and the `processors/*.json`, `.ocr-bench.json`, `.mokuro-queue.json`, `.ocr-failures.json` layouts byte-compatible so an in-place upgrade keeps benches, pools and history? Or accept a one-time reset?
10. **Wire compatibility with the existing admin UI.** The catalog/payload carry `monolithic`, `served`, `own_environment`, `own_detector`, `road` (including null), `workers_means` (`"engine"`), the `mokuro` and `feed` stages, `detectors` list with GPL labels. Is the Rust server expected to serve the *unchanged* admin JS (so these must still be emitted, with `monolithic:false`, etc.), or is the frontend changing? Also the `detector` field: keep accepting `detector: "ppocr-manga"` on hayai/paddle rows when only one detector exists, and what to do with stored rows naming `ctd`/`animetext` (hard error per 0.5.2 philosophy vs migrate)?
11. **`primary`/`enabled` coercion.** `bool(row.get(...))` accepts any truthy value (the string `"false"` is True). Tighten in Rust (reject non-bools) or copy exactly?
12. **precision column semantics.** `ocr_sidecars.precision` can hold a mode name (`auto-accuracy`) rather than a resolved format when the runner reported none (`provenance.py:198-199`). Intentional? Keep or store only resolved formats (for ppocr-manga it is NULL since the mode never applies)?
13. **Host probe fallbacks.** `describe_host` shells out to `nvidia-smi`/`rocm-smi` and reads `/proc/cpuinfo`; the `(N cores)` suffix is parsed by the admin with a regex. Keep the shell-outs, or use native enumeration (NVML/sysfs) while preserving the string format?
14. **GPU utilization sampler** (`ocr/utilization.py`, `sampler_for(device)`; not specced here) feeds `gpu_busy_pct`/`cpu_busy_pct` in trials; confirm whether it is in scope with the bench.
