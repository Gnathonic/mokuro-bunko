# OCR internals

How mokuro-bunko's OCR works underneath: generations, queue order, the
processor model, scheduling, benchmarks and the precision policy. For what
you can set and when, see the [OCR section of the configuration
reference](configuration.md#ocr); this page explains why those settings do
what they do. The design documents it summarises are in
[`docs/rust-port/`](rust-port/) (`ARCHITECTURE.md`, `OCR-WORKER.md`,
`PROTOCOL.md`, `MODELS.md`).

## The moving parts

| Part | Where it runs | What it does |
|---|---|---|
| Scheduler | the server process (all builds) | One actor that owns every piece of OCR state: it keeps an index of what is owed, claims `(volume, generation)` jobs for each lane, collects sidecars, records failures. Its state is mutated one message at a time, with no locks around scheduling. |
| Processor | the server process (full build, "local") or another machine (`mokuro-bunko processor serve`) | Receives open-session and volume operations, runs the OCR pipeline, answers with events and the finished sidecar. The local processor is the same code as a remote one, wired over in-process channels, and the scheduler treats it as a machine named `local`. |
| Engines | inside a processor (full build only) | `hayai-nova` and `paddle-manga` on libtorch (the backend pack `install-ocr` installs), `ppocr-manga` and the PP-OCR detector on ONNX Runtime on the CPU, plus the layout step that turns reads into the sidecar. |

The lite build has no engines: `ocr.local_processing` is forced off, the
queue holds with "no processor" until a remote processor connects, and the
server never links ONNX Runtime or loads libtorch. OCR inference runs on dedicated OS threads,
never on the async server's workers.

## Generations

A generation is `(engine, detector, patch budget, pools)` plus a name. The
unit of queued work is `(volume, generation)`, never `(volume, engine)`: two
rows may share an engine and must never share a file name, a failure record
or a turn in the queue.

Three things are **derived** from a row and never stored, so a rename moves
them together: the sidecar file name (`<Volume>.mokuro` for the primary row,
`<Volume>.<name>.mokuro` otherwise), the row's road through the pipeline (and
therefore which stage keys its `pools` may use), and the detector actually
used (an engine that brings its own always uses it).

The `id` (`g-1`, `g-2`, …) is minted once and keys every piece of internal
state: the in-flight set, the round-robin cursor, the workspace directories,
the congestion history, benchmark results and per-processor profile entries.
A rename therefore costs only the file name. What is *pending* is decided by
which sidecar files exist, which is why a rename re-OCRs under the new name:
nothing is renamed on disk.

**Why names follow the reader's grammar.** The name is a file-name postfix
that readers parse (`^[a-z0-9-]{1,32}$` after the last dot, lowercased). A
postfix outside that grammar would be treated as an orphan by every reader,
silently. The server enforces the same grammar at config load and in the
admin API, so such a name is refused instead.

**Why exactly one primary.** The bare `<Volume>.mokuro` is the only source of
a downloaded volume's character counts and page character counts, and of the
`volume_uuid` every other layer inherits. Without it every volume reads as
image-only and every layer gets a fresh random uuid that detaches it from the
volume the reader knows. That is also why a volume missing its primary
sidecar is offered only the primary row, and why two jobs never run on the
same volume at once (they could stamp different uuids).

**A re-OCR keeps the volume's id.** Readers key progress and stats by
`volume_uuid` and keep their local id when they take a volume's new OCR, so a
primary the server makes again must name the volume as before. The id is
remembered per archive in the database (`volume_identities`): written whenever
the metadata pass compiles a volume from its `.mokuro`, and when a primary is
deleted or moved away over WebDAV. Every sidecar is stamped, in order, with
the primary's id on disk, else the remembered one, else a layer's, else a
deterministic uuid of `"<Series>/<Volume>"`. The memory goes with the
archive's sidecars: deleting the `.cbz` forgets it, so a new upload under
that name starts fresh; a PUT over the `.cbz` leaves them, so the id
survives; a folder move carries it along.

**What cancels a job.** Each job carries the frozen row it was claimed with,
so a rename or a `primary` flip only changes where the *next* job's file
lands. A job is cancelled only when its row is removed or disabled, or when a
field that reaches its pipeline changes (engine, effective detector, patch
budget where the engine has one). Cancellation is recorded as a
cancellation, never as a failure of the volume. Pool and device changes never
cancel: they do not change what is written.

**Failures** are recorded per `(volume, generation)` in `.ocr-failures.json`
(the primary row keyed by the bare relative path, other rows suffixed with
their name) and retried after `poll_interval × 4^(attempts−1)`, capped at one
hour. Replacing the archive (a newer mtime) clears the record. A generation
that will not start on a machine (a missing model, a backend pack that does not load) is
backed off the same way per `(row, machine)`, without blaming any volume.

**Incomplete archives.** The metadata compiler cross-checks every `.mokuro`
against its archive (`matched_page_count` in `series.json`). A volume known
to be short of pages gets its primary sidecar and no further layers: every
layer would have the same holes. It is not a failure (no record, no backoff),
and a replaced, complete archive is picked up normally.

**Old and removed engines.** A row on the removed `mokuro` engine is
*retired*: it never runs but keeps its name reserved, and when it was the
primary a `hayai-nova` primary takes over. Bare `<Volume>.mokuro` files that
were not made by a composed engine have no `ocr_engine` block and are the
`mokuro-legacy` recipe; they count as complete, and are replaced only by the
optional generation upgrade (below).

### The generation upgrade

With `ocr.upgrade.enabled`, the scheduler compares each volume's primary
sidecar's **recipe** (engine, effective detector, patch budget and, where
known, the model revisions) with the primary row's. When they differ and the
sidecar's family is in `ocr.upgrade.replace`, the volume is either swapped
directly (another layer already holds the target recipe made from the
current archive) or gets an upgrade job queued behind every ordinary job. A
volume with missing pages, or whose sidecar a person edited, is skipped. The
swap runs under the per-path write lock shared with WebDAV: the old primary
is first copied to a layer named after its generation, then the new bytes
replace the bare file atomically, so there is never a moment without a
primary. The row in `ocr_sidecars` moves with the file and an
`ocr_sidecar_upgraded` audit event is written; `volume_uuid` is preserved.

## Queue order

The queue runs the enabled rows **in list order**: every volume gets the
first row's sidecar before any volume gets the second row's, so the layer
readers are waiting for comes first. There is no per-engine speed ranking:
rearranging the rows is how priority is expressed. The primary row also runs
on an inbox upload before the archive moves into the library.

Within a row the queue is a **round-robin across series**: each series' first
pending volume, then each series' second, and so on, so one long series
cannot hold up the rest. Each row keeps a cursor on the series it served last
and continues from the next series in name order (the cursor is in memory
only and starts over after a restart). Volumes within a series run in
natural order: numbers compare as numbers (full-width digits included,
`第2.5巻` between `第2巻` and `第3巻`), and kanji numerals count as numbers
where a number stands (after `第`, before `巻`/`話`/`章`/`集`, or on their own)
but not inside a word (`一番くじ`). A volume waiting out a failure backoff
holds no turn. Archive dates play no part.

The queue page lists pending work in exactly this order: it shows the
scheduler's own queue rather than sorting the library a second time. The
scheduler keeps an in-memory index of what is owed: a full re-walk of the
library every `poll_interval`, kept current in between by WebDAV
arrival/removal hooks and the library watcher, and re-checked at every claim.

## The page pipeline

Every engine reads a page through **stages**, each with a pool of its own
size and a bounded queue between every pair. One volume, one pipeline; this
is not the same thing as `ocr.concurrency`, which runs several volumes at
once.

| Road | Rows | Stages |
|------|------|--------|
| `reconciled` | `hayai-nova` / `paddle-manga` (read the `ppocr-manga` detector's lines) | `detect` (CPU) → `engine` (card or CPU; paddle-manga: card only) → `post` (CPU) |
| `line` | the `ppocr-manga` engine | `detect` (CPU) → `layout` (CPU) |

The stage holding the recognizer holds only the recognizer: boxing, the
CTC read and cropping are in front of it, layout and the raw dump behind it,
each with its own pool, so a card never waits on a thread doing image work.
Pages come out in page order, and a failure in any stage surfaces at that
page. A row's `pools` (`stage_workers`, `queue_capacity`, `stage_device`) set
the widths, depths and placement; leaving them empty derives everything from
the host. A stage on a card is one model and one worker (several recognizer
copies are possible by asking for them by name); the same stage on the CPU
is a pool sized from the host. Pool and device changes only change speed,
never the sidecar.

A queue that sits **empty** means the stage after it is starved (widen the
stage that fills it); one that sits **full** means the stage that fills it is
blocked (widen the stage that drains it). The *detailed* queue page and the
admin panel's Congestion column show that reading, as a one-line verdict per
row from the average of the row's last five completed runs
(`.ocr-congestion.json`, keyed by the row's `id`).

## Devices

A device id is `cpu` or `gpu:<n>`, the same spelling whatever the vendor
(CUDA or ROCm, through the installed backend pack, is behind it). An
absent key is `auto`: card 0 when there is one, else the CPU. A processor
reports the devices it really has when it registers, and the admin panel
offers only those. Only a stage holding a model takes a device, and the
PP-OCRv6 pair accepts only `cpu`. With `ocr.concurrency` above one, a slot
opening a session prefers a row whose engine device has no session on it
yet, so two rows on two cards run side by side.

## Sessions

A processor opens one **session** per row and slot: it loads the models once
and then reads volume after volume through them, so the pipeline never drains
at a volume boundary. The library keeps at most two volumes submitted but
unfinished per session. When an earlier row gains work, the session stops
taking volumes, finishes what it accepted and closes. A settings change
stops only the sessions whose recipe changed, and their volumes go back
unrecorded. When a session dies, the oldest unfinished volume is recorded as
a failure and the rest go back untouched. (`ocr.sessions` is accepted for old
configs and ignored.)

## Precision modes

A row carries ONE precision mode (`precision` on the row), the same for every
machine: `auto-accuracy` (the default), `auto-balanced`, `auto-speed`, or a
forced `fp32` / `bf16` / `fp16`. `ppocr-manga` fixes its own and ignores it.

- **Support is probed, never listed.** A device reports the formats it
  computes in (the backend pack's report): fp32 always; a GPU adds fp16 and
  bf16; the CPU adds bf16 only with AVX512-BF16. A format also needs a
  compiled package for the device (packages exist in fp32, bf16 and fp16 for
  the GPUs, no bf16 for Turing, and in fp32, plus bf16 for hayai-nova, for the CPU); one without
  is not supported there. The automatic modes take bf16 only where the
  hardware runs it natively (NVIDIA Ampere and newer, AMD RDNA3/4), so RDNA2
  and the CPU run fp32 unless a row forces bf16.
- `auto-accuracy` takes the first candidate the machine supports (fixed, no
  benchmark). `auto-balanced` and `auto-speed` take the machine's benchmark
  pick: each machine benchmarks the row automatically before its first
  volume, and again when the mode or its candidates change, and keeps the
  fastest; within 5% of the fastest the earlier (more accurate) candidate
  wins.
- A forced mode runs only on machines that support it; every other machine is
  *not eligible* and is never offered the row. A row that no connected
  machine can run is **held**, and the admin card, the queue page (admins)
  and `.mokuro-queue.json` (jobs in state `held`) say so.
- Each sidecar records the precision it was read at in `ocr_engine.precision`.

## Benchmarks

A benchmark answers "should I commit to this row, on this machine?". It
samples real pages from the library (32 by default, spread across series and
volumes), **pre-empts only the machine it is for** (every job there is stopped
through the same cancel-without-failure path a removed row uses, so the
interrupted volumes simply run again later) and runs the row as currently
edited.

The model loads once, one warm-up pass is discarded, then **trials** run
over the same pages with only the pools rebuilt between them. The search
follows the pipeline's own verdict rather than sweeping a grid: widen the
stage the numbers name, within its structural ceiling and the host budget;
keep a step that improves pages/s by at least 3% and revert one that does
not; then a narrowing pass keeps a narrower width while throughput holds
within 1%. A benchmark refuses hand-set widths as a starting point: it would
be measuring the hand.

**Timing is by page emissions only**: every page leaving the pipeline is
stamped, the first pages of the first pass are dropped as fill, and the rate
is pages over the time between emissions. Model load and pipeline fill are in
no rate; they are reported once as "first page after X s". A volume estimate
is reading time only (`200 / pages-per-second`), because a session loads once
and then reads every queued volume of that generation.

Benchmarks **queue**: each request is accepted with its position in its
machine's line and they run one at a time per machine. A row that has never
been saved is benchmarked under a `draft-*` key whose result lives in memory
only. The last result of each saved row is kept in `.ocr-bench.json`;
applying it is `pools = best`, and `{}` means the derivation already wins.

**Autobench.** With `ocr.autobench` on, a `(row, machine)` pair that has
never been measured is benchmarked before that machine is offered the row's
volumes, and the widths found are stored in that machine's profile
(`processors/<name>.json`, or `processors/@local.json` for the library's own
hardware), never in `config.yaml`. On the library's own hardware this
happens only for a row whose pools table is empty. A pair whose benchmark
cannot be had runs untuned rather than never.

## Remote processors

A processor is a client of the library, not a user: the `processor` role can
read files and use `/_processor/`, nothing else. It registers with its
hardware and its **catalog** (the engines and devices it can run, and what
each card computes in, see [Precision modes](#precision-modes)) and is only
ever offered rows its catalog can run.

**Protocol v3** (`docs/rust-port/PROTOCOL.md`). Everything is opened by the
processor, so it works behind NAT:

1. It trades its password for a bearer token (`POST /login/api/token`, kind
   `processor`) and registers (`POST /_processor/register`); a protocol
   mismatch is refused with the supported versions.
2. It opens **one WebSocket** (`GET /_processor/<id>/socket`) that carries
   every operation and event of every session, multiplexed by session id.
   WebSocket ping/pong does the liveness work; nothing arriving for the
   silence limit drops the processor.
3. The library sends `open_session`, then `volume` operations (at most two
   outstanding per session). For each one the processor fetches the archive
   with an ordinary `GET` (`Range` + `If-Range` against the strong `ETag`
   resumes a broken download), reports `fetch` progress, then
   `volume_started`, `page`s and `stats`.
4. A finished sidecar is sent as a plain `PUT`
   (`/_processor/<id>/results/<sid>/<claim>`, with its sha256) *before*
   `volume_done`; the upload streams to a temporary file, is verified against
   the sha256, and is collected like a local result.

A proxy must pass the WebSocket upgrade and not buffer or cap the uploads;
see [deployment](deployment.md#remote-ocr-processors-behind-a-proxy).

**Whole archives.** A processor downloads the whole `.cbz` before its OCR
reads a page of it: the volume being read and the one on deck, held in RAM
(`processor.archive_memory_mb`, in-memory files on Linux that the kernel
frees even after a crash) or, when it does not fit, in the processor's
storage. Every member's CRC-32 is checked against the archive's own directory
before OCR starts: a copy damaged in transit is fetched again, and a copy
damaged at the library (the same bad bytes twice) is handled exactly as the
library would handle it locally. A volume the processor cannot deliver goes
back to the queue with a reason class and **never counts as a failure of the
volume**. `MOKURO_NGINX_ACCEL=1` without nginx in front turns every download
into an empty answer; processors detect that and give those volumes back.

When a processor gives a volume back undelivered, the library judges it from
its own file: a file that is gone or has changed size is simply offered
again; a file the library cannot read either is recorded as a failure of the
volume; anything else is the transfer's fault and costs the volume nothing.
Several such returns in a row hold that processor for a while, doubling up to
an hour. The library writes every sidecar itself, behind the same per-path
write lock WebDAV uses, after validating it, normalising it and recording its
provenance.

**Profiles.** Each processor has a profile on the library,
`<storage>/processors/<name>.json`: its host, its catalog and, per
generation, its pools, its last benchmark and the evidence of its runs. An
entry is tied to the row's recipe (engine, detector, patch budget); change
any of them and every machine measures the row afresh. A processor's numbers
never move another machine's rates or congestion history.

**Failure handling.** A processor that drops mid-volume returns its claims
unrecorded and reconnects with a backoff from 5 seconds to 5 minutes. A
pipeline that will not start on it stops that generation on that machine
only, with a backoff, and blames no volume. A socket that closes with a
session still open is a disconnect, never a crash. Disabling, deleting,
re-roling or re-passwording its account cuts it off within about 15 seconds
(the account is re-checked on that cadence); a refused login is logged,
listed in the Processors card and ends the processor with a non-zero exit.

The library and processor must speak the same protocol version; a mismatch is
refused at registration.

## Earliest-finish scheduling

Every slot on every machine is a **lane**: `ocr.concurrency` of the library's
own (when it processes locally) plus each connected processor's
`max_sessions`. When a lane asks for work, the scheduler walks the queue in
its normal order (up to 256 volumes) and assigns each volume, in turn, to the
lane that would **finish it first**: that lane's current work, plus the row's
startup unless the lane already has a session open on that row, plus the
volume's pages at that machine's measured speed. Each lane's finish time
includes what the walk has already given it, so this is list scheduling, not
"is someone else faster": a slow machine is still fed further down a long
queue, while a lone volume (or the last few of a queue) goes to the machine
that finishes it first. The asking lane keeps a volume it would finish within
10% (at most 5 s) plus 2 s of the best other lane. The walk stops at the first
volume given to the asking lane, which it takes.

Volumes the walk gave to another lane are left for it, with a deadline: the
predicted start plus a 15-second grace. A prediction is not a promise: past
the deadline anyone may take the volume. When a volume has no sidecar yet,
its page count comes from its archive; when a lane has no speed for the row
at all, the walk cannot be priced and the plain first-come rule applies.

**Speeds.** A machine's speed on a row comes from the best evidence it has,
newest first: volumes finished in its current session (an exponentially
weighted average in which the newest volume is half the answer), then its
recent runs from earlier sessions, then its saved benchmark; a volume in
flight is blended in as it proves itself. A rate is always pages over the time
between page emissions; startup is charged separately, once per session. A
volume read while the host was busy with something else (CPU pressure at or
above 60% on Linux, or other processes using at least half the host's CPU) is
not learned as the machine's speed, and its queue card says "host busy".

The queue page's finishing times come from the same model, simulated over
every lane.

## The `ppocr-manga` engine: lines, not bubbles

Every other pipeline detects a bubble and reads it. `ppocr-manga` (a PP-OCRv6
DBNet line detector and SVTR-CTC recognizer fine-tuned on manga, running in
ONNX Runtime on the CPU) detects and reads **lines**: each is a rotated
rectangle with its text and a confidence. A geometry-only layout step then:

- removes furigana (small kana lines hugging a kanji column) and emphasis
  dots;
- groups lines into bubbles on manga and, where a page has a text body of
  long aligned columns, into one block per **paragraph**;
- orders blocks for reading (tier by tier on manga, right to left in a novel
  body, page numbers and running titles outside the flow);
- leaves out lone low-confidence reads, which on illustration pages are
  hatching and logos.

Because readers lay a line's characters on a uniform grid, a skipped cell
would shift every later character off its glyph, so the decoder gives
skipped cells back: a blank cell after `！`/`？`, the second cell of a `――`,
and a cell full of ink that decoded as nothing (a glyph the model cannot
name) written as the geta mark `〓` so the line keeps its length.

Repairs that need the page image run between recognition and layout: pieces
of one printed column that the detector cut apart are read again as one
line; each line's ends are probed for a bracket or stop the tight box left
outside; and kanji the recognizer doubted are put to a vote against two
slightly wider crops. A last text pass undoes systematic slips of a
recognizer without a language model (katakana read as look-alike kanji or
hiragana, a stray small `ュ`, a dash read as long-vowel marks).

`lines_coords` are the rotated quads, corners in the line's own upright frame
(top-left, top-right, bottom-right, bottom-left), so a reader can render a
slanted line slanted. Known limits: rare kanji can be misread confidently;
text tilted more than 45° is taken for the other orientation; a table of
contents set as widely spaced short columns may read as rows.

## Two reads per line: engines on the `ppocr-manga` detector

A row of `hayai-nova` or `paddle-manga` reads the page first exactly as the
`ppocr-manga` engine does (detection, the CTC read, split columns joined,
clipped brackets recovered, furigana separated). The row's engine then reads
a deskewed crop of every non-furigana line, and the two reads are merged per
line by sequence alignment:

- the engine's kana and kanji win;
- the CTC read restores what a sentence-writing decoder drops or folds:
  opening and closing brackets and stops, full-width `！` `？` `……` and
  digits, blank cells, the second cell of a `――`, word gaps in Latin text;
- glyphs the engine skipped come back where the CTC read has every one of
  them at high confidence; kanji are never taken from the CTC read this way;
- lines the two reads still differ on are read once more from a wider crop,
  and two reads out of three settle each difference;
- an engine runaway or an empty read falls back to the CTC line, and the
  engine's token budget is tied to the quad's glyph room;
- on one- and two-glyph lines, which give a language model no context, a
  confident CTC kanji read stands;
- a line only the engine could read is written when both of its reads agree
  and it is display lettering (large glyphs: hand-lettered sound effects);
  otherwise it keeps the CTC text.

Nothing extra is written into the sidecar. This pairing is the one to use for
scanned novels: the line detector finds every column and the engine reads the
rare kanji the small recognizer gets wrong.

## Models, provenance and pins

The recognizers are compiled libtorch packages of Apache-2.0 weights, one
per engine, precision and device type, produced by `tools/torch_export/` (a
Python development tool that never runs on a user's machine) and published
as the `torch-models-v1` release assets with a manifest of sha256 hashes and
the source model revisions; the PP-OCR files and the recognizers' host files
are in the `models-v1` release. A full build downloads what it needs into
`<storage>/models/`, verifies it and unpacks the packages there (see
[configuration](configuration.md#ocr-models) and
[`rust-port/TORCH-BACKEND.md`](rust-port/TORCH-BACKEND.md)). Every source
repo is pinned to a commit; a changed export is a new release, never a
re-upload under the same tag.

Every sidecar a composed engine writes carries a top-level `ocr_engine`
block, and the server stamps one onto every non-primary layer that lacks it;
this is how a reader tells server OCR from a layer a person edited:

```json
"ocr_engine": {
  "id": "hayai-nova",
  "recognizer": "JustANormalTinkerer/hayai-ocr-v2.5-nova",
  "detector": "ppocr-manga",
  "generator": "mokuro-bunko 0.7.0",
  "patch_budget": 512,
  "precision": "fp16",
  "weights": { "JustANormalTinkerer/hayai-ocr-v2.5-nova": "<commit sha>", "…": "…" }
}
```

### Who wrote each sidecar

The file says what read it; the database says which machine. Every sidecar
the scheduler installs, from a local run or a processor, gets a row in the
`ocr_sidecars` table: the sidecar's library path, its volume, the generation
(id and name), the machine (`local` or the processor's name) and account, the
engine, detector and precision, the build that wrote it, pages and failed
pages, and the archive's size and mtime at the time.

There is one row per file on disk. A re-run replaces it. It is deleted when
the file goes: its volume is deleted or moved away over WebDAV, the sidecar
itself is deleted or overwritten over WebDAV, or the corrupt-sidecar sweep
removes it. A series folder moved over WebDAV takes its rows with it. A
sidecar with no row has an unknown producer (it predates the table, or
arrived some other way) and is never attributed.

The admin card's History line counts from these rows. A machine's share of a
generation is its sidecars of that row on disk now, so every machine's share
adds up to at most the total. Every write and every refused result is also in
the audit log (see [configuration](configuration.md#audit-log)).

## `hayai-nova`'s patch budget

`patch_budget` is `max_num_patches` for hayai-nova's SigLIP2-NaFlex vision
tower. NaFlex fits the crop to the 16×16 patch grid that packs closest to the
budget, keeping its aspect ratio, then pads to exactly the budget, so the
budget *is* the resolution a line is read at: on a long vertical column it
buys 4, 5 or 6 patch rows across the glyph at 256, 384 or 512. Cost is linear
in the budget and independent of crop shape, and the memory difference across
the whole range is small next to the model itself, so lowering it saves time,
not memory. 512 is the default rather than the model card's 384 because on the
model author's own benchmark 384 reads worse than the previous hayai model
and only 512 beats it; dense vertical lettering and novel columns are exactly
where the difference shows.
