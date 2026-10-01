# OCR internals

How mokuro-bunko's OCR works underneath: the page pipeline, sessions,
benchmarks, remote processors, scheduling and the precision policy. For what
you can set and when, see the [OCR section of the configuration
reference](configuration.md#ocr); this page explains why those settings do
what they do.

## The moving parts

| Part | Where it runs | What it does |
|---|---|---|
| OCR worker | the server process | Scans the library every `poll_interval`, keeps the queue, claims `(volume, generation)` jobs for each slot, collects sidecars, records failures. |
| Runner (`ocr/engine_runner.py`) | a subprocess in the engines environment | Reads pages through a staged pipeline for every engine the server composes itself, and drives the served mokuro process for mokuro rows. |
| mokuro serve process | a subprocess in the mokuro environment | The mokuro fork's `python -m mokuro.serve`: one model held open, pages in on stdin, page JSON out on stdout. |
| Detector adapters (`ocr/detectors/`) | a subprocess each, in the engines environment | One detector each, behind a process boundary (which also keeps the GPL `ctd` option out of the server and the runner). |
| Processor (`mokuro-bunko processor serve`) | another machine | Logs in to the library, receives work over HTTP, runs the same runner locally, sends sidecars back. |

There are two Python environments because the mokuro stack needs
transformers 4 and the newer recognizers need transformers 5. Everything that
turns a page into a crop in the engines environment (transformers, OpenCV,
Pillow) is pinned to an exact version, like the model weights, so a silent
upgrade cannot change a sidecar.

## Generations

A generation is `(engine, detector, patch budget, pools)` plus a name. The
unit of queued work is `(volume, generation)`, never `(volume, engine)`: two
rows may share an engine and must never share a file name, a log, a failure
record or a turn in the queue.

Three things are **derived** from a row and never stored, so a rename moves
them together: the sidecar file name (`<Volume>.mokuro` for the primary row,
`<Volume>.<name>.mokuro` otherwise), the row's road through the runner (and
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
a downloaded volume's `mokuro_version`, character counts and page character
counts, and of the `volume_uuid` every other layer inherits. Without it every
volume reads as image-only and every layer gets a fresh random uuid that
detaches it from the volume the reader knows. That is also why a volume
missing its primary sidecar is offered only the primary row, and why two jobs
never run on the same volume at once (they could stamp different uuids).

**A re-OCR keeps the volume's id.** Readers key progress and stats by
`volume_uuid` and keep their local id when they take a volume's new OCR, so a
primary the server makes again must name the volume as before. The server
only makes a primary that is missing (deleted over WebDAV to re-OCR the
volume, or removed on disk), so the id is remembered per archive in the
database (`volume_identities`): written whenever the metadata pass compiles a
volume from its `.mokuro`, and when a primary is deleted or moved away over
WebDAV. Every sidecar is stamped, in order, with the primary's id on disk,
else the remembered one, else a layer's, else
`deterministic_uuid("<Series>/<Volume>")`. The memory goes with the archive's
sidecars: deleting the `.cbz` (which deletes them) forgets it, so a new upload
under that name starts fresh; a PUT over the `.cbz` leaves them, so the id
survives; a folder move carries it along. An archive removed outside WebDAV
leaves its id behind, and a new archive at exactly that path inherits it.

**What cancels a job.** Each job carries the frozen row it was claimed with,
so a rename or a `primary` flip only changes where the *next* job's file
lands. A job is cancelled only when its row is removed or disabled, or when a
field that reaches its runner changes (engine, effective detector, patch
budget where the engine has one). Cancellation is recorded as a
cancellation, never as a failure of the volume. Pool and device changes never
cancel: they provably do not change what is written.

**Failures** are recorded per `(volume, generation)` in `.ocr-failures.json`
(the primary row keyed by the bare relative path, other rows suffixed with
their name) and retried after `poll_interval × 4^(attempts−1)`, capped at one
hour. Replacing the archive (a newer mtime) clears the record. A generation
whose runner will not start on a machine (a broken environment, a missing
model) is backed off the same way per `(row, machine)`, without blaming any
volume.

**Incomplete archives.** The metadata compiler already cross-checks every
`.mokuro` against its archive (`matched_page_count` in `series.json`). A
volume known to be short of pages gets its primary sidecar and no further
layers: every layer would have the same holes. It is not a failure — no
record, no backoff — and a replaced, complete archive is picked up normally.

## Queue order

The queue runs the enabled rows **in list order**: every volume gets the
first row's sidecar before any volume gets the second row's. The same order
decides OS priority — the head of the list runs at normal priority, every row
below it is niced (`nice` on Linux/macOS, below-normal on Windows) — so the
layer readers are waiting for is never the one yielding CPU. There is no
per-engine speed ranking: rearranging the rows is how priority is expressed.
The primary row also runs on an inbox upload before the archive moves into
the library.

Within a row the queue is a **round-robin across series**: each series' first
pending volume, then each series' second, and so on, so one long series
cannot hold up the rest. Each row keeps a cursor on the series it served last
and continues from the next series in name order (the cursor is in memory
only and starts over after a restart); without it the worker, which
recomputes the queue after every job, would serve the alphabetically first
series again and again. Volumes within a series run in natural order:
numbers compare as numbers (full-width digits included, `第2.5巻` between
`第2巻` and `第3巻`), and kanji numerals count as numbers where a number
stands (after `第`, before `巻`/`話`/`章`/`集`, or on their own) but not
inside a word (`一番くじ`). A volume waiting out a failure backoff holds no
turn. Archive dates play no part.

The queue page lists pending work in exactly this order: it shows the
worker's own queue rather than sorting the library a second time.

## The staged page pipeline

Every engine reads a page through **stages**, each with a **pool of its own
size** and a **bounded queue between every pair**. One volume, one pipeline:
this is not the same thing as `ocr.concurrency`, which runs several volumes
at once.

A page is declared as an ordered stage graph (`STAGE_GRAPHS` in
`ocr/engine_runner.py`); each stage says which device it occupies and whether
it is a pool or bound to one model on one device. One scheduler runs whatever
graph it is handed, so a new engine or detector is a line in that table.

| Road | Rows | Stages |
|------|------|--------|
| `served` | `mokuro` | `feed` (cpu) → `mokuro` (serve process) → `post` (cpu) |
| `adapter` | `hayai-nova` / `paddle-manga` behind `ctd` | `detect` (card or cpu) → `engine` (card) → `post` (cpu) |
| `reconciled` | `hayai-nova` / `paddle-manga` behind the `ppocr-manga` detector | `detect` (cpu) → `engine` (card) → `post` (cpu) |
| `line` | the `ppocr-manga` engine | `detect` (cpu) → `layout` (cpu) |

The stage holding the recognizer holds **only** the recognizer: boxing, the
CTC read and cropping are in front of it, layout and the raw dump behind it,
each with its own pool, so the card never waits on a thread doing numpy. It
is deliberately never the first or last stage: a stage at the end has no
output queue, so it could neither be *blocked* (the signal that what follows
is too narrow) nor park when it has run ahead.

### The served road

The mokuro fork's serve mode takes a page path on stdin and answers page JSON
on stdout, so mokuro joins the runner like any other road and a session keeps
one mokuro process — one model — open for volume after volume.

- `feed` reads a page out of the `.cbz` and writes it where the process can
  open it; the copy is deleted as soon as its answer arrives.
- `mokuro` is the process. Its width is one, because one process holds one
  model; its **Workers** cell is the engine's own `--num_workers` (its
  CPU-side pipeline), which is why it is editable although the stage is
  device-bound.
- `post` keeps the page the engine returned. The sidecar is byte-for-byte what
  the mokuro command line would have written, so nothing on an existing
  library regenerates.
- The queue in front of `mokuro` is sized to exactly the `window` the process
  reports on its ready line. More is forbidden by the protocol; less can
  strand the engine, whose last batch waits for the end of the volume. A
  `queue_capacity` for `feed` is therefore ignored, with a line in the session
  log saying so.
- The device choice reaches the process as its own command line: `cpu` starts
  it with `--force_cpu`, `gpu:<n>` sets `CUDA_VISIBLE_DEVICES` and
  `HIP_VISIBLE_DEVICES` (mokuro has no device index of its own, so the card is
  chosen by hiding the others), `auto` leaves the choice to the fork.
- The process is started with `OMP_NUM_THREADS=1` unless its device is the
  CPU or you set the variable yourself: its CPU side is small, and a full
  thread pool stalls the card whenever something else holds the cores. On
  `auto`, a process that reports it loaded on the CPU is restarted without
  the cap before its first page.
- **Fallback.** The server probes once per start whether the mokuro
  environment's package has `mokuro.serve`. If it does not (for example a
  `MOKURO_BUNKO_MOKURO_SPEC` override pointing at PyPI mokuro), mokuro rows
  run as one command per volume, extracted up front, and the log says which
  path was chosen. The device setting translates the same way on either
  path.

### The detector is a stage, not a pass

On the adapter road the detector runs in its own process — a licence
boundary, since `ctd` combines with GPL-3.0 code and nothing in the server or
the runner may import it. The adapters have a **serve mode**: load the model
once, then one request a line on stdin (the image to read and the JSON to
write) and one reply a line on stdout. A `detect` worker owns one such
process and asks it for the page it is holding, so detection streams like
every other stage and the ordinary bounded queues are its backpressure. A
detector that dies or wedges costs its page only: end-of-file fails that
page, each page has a timeout (`MOKURO_OCR_DETECT_TIMEOUT`, 300 s; the first
answer may take longer because it includes loading the model), and the pool
replaces a dead process at most twice before the remaining pages fail fast.
The adapters' batch mode runs the same per-page code, so a page's JSON cannot
depend on how the adapter was started.

A detector on a card derives to **one** process, because a second is a
second GPU context and a second copy of the model in memory. Asking for more
by name (`stage_workers: {detect: 2}`) still works, which is what a CPU
detector that cannot keep the recognizer fed needs.

### The recognizer loads while the pipeline runs

Loading a vision-language recognizer (imports, weights, LoRA merge) takes
seconds. It happens on a thread of its own from the start of the run: the
pipeline starts at once, the detector processes load their models at the same
time, and the first page to reach the `engine` stage is the one that waits.
The queue in front of that stage gets four slots rather than one so the
overlap is used. A load that fails ends the run at the first page with the
real error rather than writing a volume of blank pages.

### Several recognizer copies on one card

`stage_workers: {engine: N}` with the engine on a card loads N copies of a
`hayai-nova` / `paddle-manga` recognizer (at most 8), each in an engine
process of its own; each engine worker hands its page's crops to a free
copy. The copies load one after another while the pipeline runs. A copy whose
process dies is dropped, and the last one ends the run with its error. Only a
width asked for by name does this: a derived width never adds a copy, and on
the CPU the setting is ignored. More copies only pay when one copy leaves
the card idle and the detector can feed them, so widen `detect` with it.

### torch threads

A `hayai-nova` / `paddle-manga` recognizer on a card holds torch to four CPU
threads in every thread that calls it. The CPU side of a GPU read is small,
and a pool of one thread per core stalls the card whenever another program
holds those cores. `OMP_NUM_THREADS` / `MKL_NUM_THREADS` set by you are kept;
a recognizer on the CPU keeps torch's default pool.

### What does not change

Pages come out in page order (the pipeline reassembles it at the sink from a
sequence number), each page's lines are reconciled only against its own
detection, the review file is appended in page order, and a failure in any
stage surfaces at that page under the same blank-page guard. The sidecar is
**byte-identical** to the single-threaded path (`--cpu-workers 0`) at every
width and on every road — checked with real pages and real models, including
the per-page cache, detector JSON and raw dumps. That is why pool and device
edits never cancel a job.

## Sessions

With `ocr.sessions` on, a slot opens ONE runner (`engine_runner.py --serve`)
for the first row with claimable work and keeps it fed: models load once a
session, and while the sink assembles volume N's sidecar the `detect` stage
is already on volume N+1's pages, so the pipeline never drains at a volume
boundary.

stdin carries one JSON object a line:

```json
{"op":"volume","id":"<job id>","archive":"<abs .cbz>","workspace":"<abs scratch dir>",
 "output":"<abs sidecar>","cache_dir":"<abs>","detect_dir":"<abs>","log":"<abs per-volume log>",
 "title":"…","volume":"…","title_uuid":"…|null","volume_uuid":"…|null"}
{"op":"close"}
```

`"input": "<abs dir of page images>"` in place of `archive` is equally valid
(the single-volume command line and the benchmark sample use it). Volumes are
read in arrival order; `close`, or stdin ending, finishes every accepted
volume and exits 0.

stdout carries one JSON object a line and **nothing else**:

| event | when |
|---|---|
| `ready` | models loaded; carries `startup_seconds`, `weights`, `stage_workers`, `queue_capacity`, `stage_device` (where each model really is), `pipeline` |
| `volume_started` | the volume's pages are known; carries `pages` |
| `page` | a page left the pipeline; carries `done` / `total` |
| `volume_done` | the sidecar at `output` is complete (written beside and renamed before this line); carries `pages`, `failed_pages`, `seconds`, `stats` |
| `volume_failed` | that volume is over; the session carries on |
| `stats` | every 2 s, the live cumulative pipeline snapshot |
| `fatal` | the models would not load; a non-zero exit follows |

`volume_done.stats` is that volume's **share** of the pipeline counters (the
difference between the snapshots at the previous volume's end and this one's),
so the windows tile the session exactly and the congestion history reads a
session volume the same way as a single-volume run.

Human-readable output goes to files, never to stdout. The session log gets
everything; each volume's own log gets every line attributable to it, down to
the `Processed successfully: 1/1` summary the server parses. Attribution is
per thread, and file descriptor 1 is taken away from everything but the
protocol, so a library's own `printf` cannot land inside a JSON line.

**The archive is the source.** Nothing is extracted: the feeder reads
members out of the `.cbz` just ahead of the pipeline and rolls from one
archive into the next, with exactly the page order, `img_path`s and
exclusions that extracting would give (including skipping an embedded
`<stem>.webp` thumbnail). Where the detector runs in-process the bytes go
from the archive to the decoder in memory; where it is a subprocess the page
is written under `workspace` and removed once it leaves the sink. A corrupt
member fails that page; an unreadable archive fails that volume; neither ends
the session.

The server keeps two volumes submitted-but-unfinished per session, so the
feeder always has the next archive to roll into; more buys nothing and makes
pre-emption dearer. When an earlier row gains work, the session stops taking
volumes, finishes what it accepted and closes. A settings change kills only
the sessions whose recipe changed, and their volumes go back unrecorded. When
a runner dies, the oldest unfinished volume is recorded as a failure and the
rest go back untouched; two sessions of one row dying without finishing
anything stops that row until the next scan. Ten minutes with no event at all
is a wedge, handled the same way.

What a session writes is byte-identical to what the single-volume command
line writes for the same volume — it is the same code; the command line is a
session with one volume in it.

## Backpressure, and reading it

Every queue has a capacity. A worker that cannot put into a full queue
**blocks** on a condition variable (it does not spin), so a stage that has
run ahead is asleep and a card-side producer gives its time back. That makes
the queues answer "where is the time going?" by themselves:

- a queue that sits **empty** ⇒ the stage after it is **starved** ⇒ widen the
  stage that fills it;
- a queue that sits **full** ⇒ the stage that fills it is **blocked** ⇒ widen
  the stage that drains it.

Both are counted in seconds per queue, next to time-weighted mean depth and
the high-water mark, and printed at the end of every volume:

```
[runner] pipeline over 12 page(s) in 1.8s (0.15s a page)
[runner]   stage detect  cpu x3 busy  5.3s ( 96.8% of its pool) starved  0.0s blocked  0.0s [detect + CTC read]
[runner]   stage layout  cpu x1 busy  0.0s (  1.9% of its pool) starved  1.8s blocked  0.0s [layout + dump]
[runner]   queue in->detect      cap 5 mean depth 3.46 (69.1% full) max 5 producers blocked 0.8s / consumers starved 0.0s
[runner]   queue detect->layout  cap 3 mean depth 0.00 ( 0.1% full) max 1 producers blocked 0.0s / consumers starved 1.8s
[runner]   bottleneck: detect (97% of a pool of 3) -- widen it to go faster, or narrow the stages waiting on it to free cores
```

Seconds are summed over the pool: two workers each waiting a second read as
two seconds against a wall clock of one.

The **detailed** queue page shows the same reading live, one row a stage:
name, device and width (`fused` where a stage rides the one before it); a bar
splitting that pool's time into busy, blocked and starved (shares of pool
time, so a two-wide and a four-wide stage compare directly); and the mean
depth of the queue it fills against its capacity. The highlighted row is the
bottleneck. Above the rows, when the numbers support one, is a one-line
verdict, *what is waiting → what to widen*.

Waiting **propagates**, so the biggest number is usually not the culprit: a
slow detector starves the engine, and the engine then starves `post` harder
than it is itself starved. Starvation is therefore read from the **source**
end (the first starved stage whose own feeder is not also waiting) and
blocking from the **sink** end. A stage that cannot be widened — one model on
one card — is never named as the fix: on a GPU-bound run the verdict says the
card sets the pace instead. The verdict says nothing rather than guess until
eight pages have come out (a pipeline's fill is all starvation), with
`--cpu-workers 0`, when every stage waits on the one before it (the archive
is being read slower than it is OCRed), and when the pipeline is balanced. A
blank verdict with the rows shown means there is nothing worth widening.

The same numbers are published as `pipeline.json` beside the detector dumps
while a volume runs (every two seconds), or where `--stats-file` /
`MOKURO_OCR_PIPELINE_STATS` says. Never under `--cache-dir`: the server counts
the JSON files there as finished pages. The chain to the page is
`pipeline.json` → the worker's progress poll → `GET /queue/api/status`, so
the rows lag the run by a few seconds, and the figures are cumulative from
the start of the volume.

### The Congestion column

The live readout describes the job running now. The admin panel's
generations table shows the **average of the last five completed runs** of
each row, kept in `<storage>/.ocr-congestion.json` keyed by the row's `id`
(never its name or postfix, which a rename would erase). Rows that no longer
exist are pruned on the next write; cancelled and failed runs are not
recorded. The same verdict function reads the averages, so the table and the
log say the same thing. A row that has fallen back to one mokuro command per
volume has no stages and no data, permanently.

## Sizing

Each stage declares what a page costs it and is sized **relative to the
stage that sets the pace**: `ceil(stage cost / pace)`, where the pace is the
device-bound stage's cost when there is one, and otherwise the most expensive
pooled stage at its own ceiling. The host budget
(`(cores − 1) / concurrent jobs / 4 threads a session`) and a measured
plateau are per-stage **ceilings**, not a pot the stages share.

On top of the ratio sit two rules:

- **Headroom for page-to-page variance.** A pool sized on the mean page
  stalls on every page above the mean, so the width is derived from a high
  percentile, expressed as a multiple of the mean (`STAGE_WIDTH_HEADROOM`,
  2.0).
- **Never a second model on a device.** A stage on a card derives to one
  worker whatever it costs; only naming it widens it.

Queue capacities default to one slot per worker of the stage that fills the
queue, or four in front of a device-bound stage (whose model may still be
loading). Capacity is memory — a waiting page can hold a decoded image of
~14 MB — so the defaults keep a long volume flat rather than comfortable.
Every page also takes a **ticket** at the entrance, returned when it is
handed back, with as many tickets as the pipeline has queue slots and
workers: the queues alone do not bound the reassembly buffer, and one page
stuck in the last stage would otherwise leave every page behind it held in
memory. In practice resident memory is dominated by the number of sessions
and model copies, not by queue capacity.

A runner cannot see its siblings, so the server tells each one how many jobs
share the host (`MOKURO_OCR_JOBS`, from `ocr.concurrency`) and it budgets for
its share. The runner's first log line prints the graph, every width, every
capacity and anything set by hand.

### Knobs

An explicit setting always beats the derivation; only a stage's structural
limit (one model on one device) still holds. A row's `pools` reaches the
runner as the flags below; the environment variables apply to every runner
not given the flag, which is mainly useful when running the runner by hand.

| Knob | Runner flag | Environment | Meaning |
|------|-------------|-------------|---------|
| per-stage width | `--stage-workers detect=4,post=2` | `MOKURO_OCR_STAGE_WORKERS` | Pool width of named stages; a bare number sets all of them. `engine=N` on a card loads N recognizer copies. |
| every CPU stage | `--cpu-workers N` | `MOKURO_OCR_CPU_WORKERS` | One width for every CPU stage. `0` is the serial fallback (no threads, no queues); `1` still pipelines. |
| per-queue capacity | `--queue-capacity engine=4` | `MOKURO_OCR_QUEUE_CAPACITY` | Pages allowed to wait in the queue a named stage fills; a bare number sets all of them. |
| stage device | `--stage-device detect=cpu,engine=gpu:0` | `MOKURO_OCR_STAGE_DEVICE` | Where each model-bearing stage runs. |
| precision | `--precision auto-speed` (`--precision-pick fp16`) | — | The row's precision mode, and this machine's benchmarked pick for a balanced/speed mode. |
| detector page timeout | — | `MOKURO_OCR_DETECT_TIMEOUT` | Seconds one page may take in a detector process (default 300). |
| live stats file | `--stats-file PATH` | `MOKURO_OCR_PIPELINE_STATS` | Where `pipeline.json` is written. |

Per-stage settings take `name=N` pairs or a bare number and refuse an unknown
stage name.

## Devices

A device id is `cpu` or `gpu:<n>` — the same spelling whatever the vendor,
mapped to torch's `cuda:<n>` inside the runner (ROCm builds use the same
name). An absent key is `auto`: card 0 when there is one, else the CPU. The
admin panel offers the ids a machine really has, probed once in its engines
environment (`POST /api/ocr/devices/refresh` probes again; a processor
reports its own).

Only a stage holding a model takes a device. A model that cannot leave the
CPU (the PP-OCRv6 pair is onnxruntime) accepts only `cpu`, and an onnxruntime
detector goes on a card only where onnxruntime can reach one. Placement
decides what a width can be: a stage on a card is one model and one worker;
the same stage on the CPU is a pool the host budget sizes. With
`ocr.concurrency` above one, a slot opening a session prefers a row whose
engine device has no session on it yet, so two rows on two cards run side by
side. The runner reports what it actually resolved in its `ready` event's
`stage_device`.

## Precision modes

A row carries ONE precision mode (`precision` on the row), the same for every
machine: `auto-accuracy` (the default), `auto-balanced`, `auto-speed`, or a
forced `fp32` / `bf16` / `fp16`. It reaches `mokuro` (the serve process's
`--fp16`), `paddle-manga` (the model's dtype) and `hayai-nova` (the autocast
dtype); `ppocr-manga` fixes its own and ignores it.

**The policy** is one table, `PRECISION_POLICY` in `ocr/engine_runner.py`:
engine → mode → candidate formats in order of preference.

| Engine | `auto-accuracy` | `auto-balanced` | `auto-speed` |
|---|---|---|---|
| `hayai-nova` | bf16, fp32 | bf16, fp32 | bf16, fp16, fp32 |
| `paddle-manga` | fp32 | bf16, fp32 | bf16, fp16, fp32 |
| `mokuro` | fp32 | fp32 | fp16, fp32 |
| anything else | fp32 | fp32 | fp32 |

The lists come from a line-by-line review of every line that read
differently between formats on a black-and-white sample of about 1,300 pages
(fp32 read identically on every card tested): `hayai-nova`'s bf16 was more
accurate than its fp32 and fp16 the confirmable loser; `paddle-manga` is most
accurate in fp32 and close, and much faster, in bf16; `mokuro`'s manga-ocr in
bf16 was much worse than in fp16 (2.89% against 0.48% CER over 60k lines), so
`mokuro` never runs bf16 -- the fork has no switch for it either.

**Support is probed, never listed.** What a device can compute in is asked
of torch on that machine (`supported_formats`, and the device probe in
`ocr/devices.py`): fp32 always, fp16 on any GPU, bf16 where
`torch.cuda.is_bf16_supported()` says so with that device current. The CPU
supports fp32 only. No architecture or compute-capability list decides
anything; a card that emulates bf16 slowly (an RX 6000) reports support, and
only a benchmark shows it slow.

**Resolution on one machine** (`resolve_mode`), from the candidates its device
supports:

- `auto-accuracy`: the first supported candidate. Fixed; no benchmark.
- `auto-balanced`, `auto-speed`: the machine's benchmark pick. Each machine
  benchmarks the row automatically before its first volume, and again when
  the mode changes (or its candidates do), and keeps the fastest: the
  benchmark loads the recognizer once (in fp32, for an in-process torch
  recognizer) and runs ONE trial per supported candidate on the same sample,
  re-casting the weights from an fp32 master copy between trials -- the
  served mokuro process is restarted with or without `--fp16` instead.
  Within `PRECISION_TIE` (5%) of the fastest, the earlier candidate (the more
  accurate) wins. The pick, every trial's pages a second and the reason are
  kept with that machine's benchmark of the row (`precision`,
  `precision_mode`, `precision_trials`, `precision_why`); the library sends
  it to that machine's runner as `--precision-pick`.
  Hand-set pools switch width tuning off, never the pick: a machine whose
  pools a person set (this server's own table, or pools an admin saved for a
  processor) gets a **precision-only** benchmark instead
  (`OCRWorker.autobench_kind`, the runner's `--bench-precision-only`) -- the
  candidate trials at its pools exactly as configured, storing the pick, the
  trials and the why, and never writing a pool.
  Only with automatic benchmarks off (`ocr.autobench: false`), or when a
  machine's benchmark failed, does it run the first supported candidate; the
  admin card says which.
- A forced mode: that format, or the machine is **not eligible**.

**Eligibility.** A processor registers what its probe found per card, in the
optional `gpus` field of its protocol-2 registration catalog:
`[{"index": 0, "formats": {"bf16": true, "fp16": true}}]`. This server
answers from its own probe. `scheduler.catalog_can_run` (a processor) and
`OCRProcessor.can_run` (this server) refuse a row whose forced format the
device its model sits on does not support -- the same gate that refuses a
missing engine or detector -- so the claim, the earliest-finish lanes and
the automatic benchmark never offer it the row, and the queue plan never
prices the row on its lanes. A processor that reports no `gpus` counts as
fp32-only for a forced mode; for an auto mode its own runner decides at
start. A row that no connected machine can run is **held**
(`OCRWorker.precision_holds`): the admin card and the queue page (admins) say
"No connected machine can run bf16", and its jobs in `.mokuro-queue.json`
are `held`.

**Staleness.** A machine's stored benchmark counts only while it describes the
row's mode there (`profiles.stale_bench_reason`): one measured for another
mode, one whose precision trials are not exactly the device's current
candidates, or one that ran at another format than a fixed mode resolves to
is stale -- it is dropped and, where autobench is on, measured again.

**Defence in depth.** A runner asked for a forced format its device does not
support refuses at start (`PrecisionUnavailable`, whose message carries
`precision not available here`). The library gives the volume back
unrecorded -- never a failure -- and backs the row off on that machine.

Machines' pools no longer carry a precision; a `precision` left in an older
profile's pools is ignored, with one log line per machine and row. The runner
logs every pick and why, e.g. `[runner] paddle-manga precision: fp32
(auto-balanced; benchmark: fp32 0.57 p/s beat bf16 0.32 p/s)` or `[runner]
hayai-nova precision: bf16 (auto-accuracy)`, and each sidecar records it in
`ocr_engine.precision` -- mokuro's too.

## Benchmarks

A benchmark answers "should I commit to this row, on this machine?". It
samples real pages from the library (32 by default, spread across series and
volumes), **pre-empts only the machine it is for** — every job there is
stopped through the same cancel-without-failure path a removed row uses, so
the interrupted volumes simply run again later — and runs the row as
currently edited.

For a composed row the runner runs `--bench`: load once, one discarded
warm-up pass, then **trials** over the same pages with only the pools rebuilt
between them (the recognizer is never reloaded). The search follows the
pipeline's own verdict rather than sweeping a grid: widen the stage the
numbers name, within its structural ceiling and the host budget; keep a step
that improves pages/s by at least 3% and revert one that does not; then a
narrowing pass keeps a narrower width while throughput holds within 1% (the
same speed for fewer cores is a result worth having). A benchmark refuses
hand-set widths as a starting point — it would be measuring the hand. A
mokuro row that has fallen back to one command per volume is simply run
once over the sample.

**Timing is by page emissions only.** Every page leaving the pipeline is
stamped; the first `min(8, N/4)` of the first pass are dropped as fill, and
the rate is `(M − 1) / (t_last − t_first)` over the rest. Interpreter start,
imports, model load, detector spawn and pipeline fill are in no rate; they
are reported once as "first page after X s". A trial keeps re-feeding the
sample (repeated inside one continuous run, never as separate runs, so a road
that emits in bursts at the end of a feed bursts once) until its window
reaches 20 s or eight feeds. A trial still under 10 s is flagged
`short_window` and never decided on. While a trial runs the server samples
GPU and CPU busy once a second and reports the means over that window only.

A volume estimate is reading time only (`200 / pages-per-second`): a session
loads once and then reads every queued volume of that generation, so charging
the load to each volume would overstate all of them.

Benchmarks **queue**: each request is accepted with its position in its
machine's line and they run one at a time per machine, in the order asked.
A machine is held and pre-empted once for its whole line and released as soon
as its line is empty. Re-requesting a benchmark already queued for the same
row is the only thing refused (409). A row that has never been saved is
benchmarked under a `draft-*` key whose result lives in memory only. The last
result of each saved row is kept in `.ocr-bench.json`; `bench_done` carries
only the widths that differ from the derived ones, so applying it is
`pools = best` and `{}` means the derivation already wins.

**Autobench.** With `ocr.autobench` on, a `(row, machine)` pair that has
never been measured is benchmarked before that machine is offered the row's
volumes, and the widths found are stored in that machine's profile
(`processors/<name>.json`, or `processors/@local.json` for the library's own
hardware) — never in `config.yaml`. On the library's own hardware this
happens only for a row whose pools table is empty; any value set there is
used as written. A pair whose benchmark cannot be had runs untuned rather
than never.

## Remote processors

A processor is a client of the library, not a user: the `processor` role can
read files and use `/_processor/`, nothing else. It registers with its
hardware and its **catalog** (the engines, detectors and devices it can run,
and -- optionally -- what each card computes in, see
[Precision modes](#precision-modes)) and is only ever offered rows its
catalog can run.

**Channels.** Everything is opened by the processor, so it works behind NAT:

- the **assignment stream** — `GET /_processor/<id>/stream`, one chunked
  response for as long as the processor is connected, with a heartbeat line
  every 15 seconds;
- the **events channel** — `POST /_processor/<id>/sessions/<sid>/events`,
  one chunked request body per OCR session (a keep-alive frame every
  3 seconds, and every finished sidecar's bytes);
- **archive reads** — ordinary `GET`s of the library's `.cbz` files.

A proxy must stream the first two through unbuffered and without a body size
limit; see [deployment](deployment.md#remote-ocr-processors-behind-a-proxy).

**Protocol 2: whole archives.** A processor downloads the whole `.cbz` before
its OCR reads a page of it — one `GET` per volume, for the volume being read
and the one on deck — into RAM (`processor.archive_memory_mb`, held as
unnamed files in `/dev/shm` that the kernel frees even after a crash) or,
when it does not fit, into the processor's storage. A broken download resumes
with `Range` + `If-Range` from the byte it reached; a file replaced on the
library meanwhile is downloaded again whole rather than spliced. Every
member's CRC-32 is checked against the archive's own directory before OCR
starts: a copy damaged in transit is fetched again, and a copy damaged at the
library (the same bad bytes twice) is handled exactly as the library would
handle it locally. After two minutes without a new byte the volume goes back
to the queue unrecorded. A volume that fails to download three times is recorded
as "download failed". `MOKURO_NGINX_ACCEL=1` without nginx in front turns
every download into an empty answer; processors detect that and give those
volumes back.

When a processor gives a volume back undelivered, the library judges it
from its own file: a file that is gone or has changed size is simply offered
again; a file the library cannot read either is recorded as a failure of the
volume; anything else is the transfer's fault and costs the volume nothing.
Three such returns in a row hold that processor for ten minutes, doubling up
to an hour. The library writes every sidecar itself.

**Profiles.** Each processor has a profile on the library,
`<storage>/processors/<name>.json`: its host, its catalog and, per
generation, its pools, its last benchmark and the evidence of its runs. An
entry is tied to the row's recipe (engine, detector, patch budget); change
any of them and every machine measures the row afresh. A processor's numbers
never move another machine's rates or congestion history.

**Failure handling.** A processor that drops mid-volume returns its claims
unrecorded and reconnects with a backoff from 5 seconds to 5 minutes. A
runner that will not start on it stops that generation on that machine only,
with a backoff, and blames no volume. Disabling, deleting, re-roling or
re-passwording its account cuts it off within a heartbeat; a refused login is
logged, listed in the Processors card and ends the processor with a non-zero
exit. Each connected processor holds one of the library's request threads
for its assignment stream, one more per open session and one while it
benchmarks (`MOKURO_THREADS`, default 50).

A processor keeps running the runner build it started with until it
restarts. The library and processor must speak the same protocol version; a
mismatch is refused at registration.

## Earliest-finish scheduling

Every slot on every machine is a **lane**: `ocr.concurrency` of the library's
own (when it processes locally) plus each connected processor's
`max_sessions`. When a lane asks for work, the worker walks the queue in its
normal order (up to 256 volumes) and assigns each volume, in turn, to the
lane that would **finish it first**: that lane's current work, plus the
row's startup unless the lane already has a session open on that row, plus
the volume's pages at that machine's measured speed. Each lane's finish time
includes what the walk has already given it, so this is list scheduling, not
"is someone else faster": a slow machine is still fed further down a long
queue, while a lone volume — or the last few of a queue — goes to the machine
that finishes it first. The asking lane keeps a volume it would finish within
10% (at most 5 s) plus 2 s of the best other lane. The walk stops at the
first volume given to the asking lane, which it takes.

Volumes the walk gave to another lane are left for it, with a deadline: the
predicted start plus a 15-second grace. A prediction is not a promise: past
the deadline anyone may take the volume. For a lane that is idle now, the
deadline is fixed when first set; for a lane still busy with other work, it
follows the current prediction as that lane's work moves. Idle lanes waiting
for work are woken when a volume is left to them for the first time.

When a volume has no sidecar yet, its page count comes from its archive; when
a lane has no speed for the row at all, the walk cannot be priced and the
plain first-come rule applies. `MOKURO_EFT_TRACE=1` logs each decision: who
asked, its lanes, what it took, and what it left to whom until when.

**Speeds.** A machine's speed on a row comes from the best evidence it has,
newest first: volumes finished in its current session (an exponentially
weighted average in which the newest volume is half the answer), then its
recent runs from earlier sessions, then its saved benchmark; a volume in
flight is blended in as it proves itself. A rate is always pages over the
time between page **emissions** — model load and pipeline fill are never in
it; startup is charged separately, once per session. A volume read while the
host was busy with something else — CPU pressure at or above 60%, or other
processes using at least half the host's CPU over the volume's window — is
not learned as the machine's speed, and its queue card says "host busy".

The queue page's finishing times come from the same model, simulated over
every lane.

## The `ppocr-manga` engine: lines, not bubbles

Every other pipeline detects a bubble and reads it. `ppocr-manga` (a PP-OCRv6
DBNet line detector and SVTR-CTC recognizer fine-tuned on manga, running in
onnxruntime on the CPU) detects and reads **lines**: each is a rotated
rectangle with its text and a confidence. A geometry-only layout step
(`ocr/line_layout.py`) then:

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
and a cell full of ink that decoded as nothing — a glyph the model cannot
name — written as the geta mark `〓` so the line keeps its length.

Three repairs need the page image and run between recognition and layout:
pieces of one printed column that the detector cut apart are read again as
one line; each line's ends are probed for a bracket or stop the tight box
left outside (and, in a novel's text body, for a thin first glyph such as
`一`); and kanji the recognizer doubted are put to a vote against two
slightly wider crops. A last text pass undoes systematic slips of a
recognizer without a language model (katakana read as look-alike kanji or
hiragana, a stray small `ュ`, a dash read as long-vowel marks).

`lines_coords` are the rotated quads, corners in the line's own upright frame
(top-left, top-right, bottom-right, bottom-left), so a reader can render a
slanted line slanted. Detector and recognizer run in one process (both are
Apache-2.0). Known limits: rare kanji can be misread confidently; text tilted
more than 45° is taken for the other orientation; a table of contents set as
widely spaced short columns may read as rows.

## Two reads per line: engines on the `ppocr-manga` detector

A row of `hayai-nova` or `paddle-manga` on the `ppocr-manga` detector reads
the page first exactly as the `ppocr-manga` engine does (detection, the CTC
read, split columns joined, clipped brackets recovered, furigana separated).
The row's engine then reads a deskewed crop of every non-furigana line — the
line's own rotated rectangle with a quarter-em margin, a page's crops
batched — and the two reads are merged per line by sequence alignment
(`ocr/line_reconcile.py`):

- the engine's kana and kanji win;
- the CTC read restores what a sentence-writing decoder drops or folds:
  opening and closing brackets and stops (where the neighbouring glyphs
  align), full-width `！` `？` `……` and digits, blank cells, the second cell
  of a `――`, word gaps in Latin text;
- glyphs the engine skipped come back where the CTC read has every one of
  them at high confidence, and very confident CTC kana stand against the
  engine's other kana; kanji are never taken from the CTC read this way;
- lines the two reads still differ on are read once more from a wider crop,
  and two reads out of three settle each difference;
- an engine runaway (a line much longer than both the CTC read and the
  quad's room) or an empty read falls back to the CTC line, and the engine's
  token budget is tied to the quad's glyph room, so a runaway costs little;
- on one- and two-glyph lines, which give a language model no context, a
  confident CTC kanji read stands;
- a line only the engine could read is written when both of its reads agree
  and it is display lettering (large glyphs: hand-lettered sound effects);
  otherwise it keeps the CTC text.

Nothing extra is written into the sidecar. The processing workspace keeps a
per-page raw dump under `_detect/<engine>/` (both reads, the merged text, the
agreement and the source of each line) and a per-volume `review.json`
(`ocr-review/1`) listing the lines where the two recognizers still disagree —
the best cue there is for a wrong kanji, and the place a proofreader should
start. This pairing is the one to use for scanned novels: the line detector
finds every column and the engine reads the rare kanji the small recognizer
gets wrong.

## Sidecar provenance and pinned weights

Every sidecar a composed engine writes carries a top-level `ocr_engine`
block, and the server stamps one onto every non-primary layer that lacks it
(a mokuro row running as a secondary layer, for instance) — this is how a
reader tells server OCR from a layer a person edited:

```json
"ocr_engine": {
  "id": "hayai-nova",
  "recognizer": "JustANormalTinkerer/hayai-ocr-v2.5-nova",
  "detector": "ppocr-manga",
  "generator": "mokuro-bunko 0.5.0",
  "patch_budget": 512,
  "precision": "bf16",
  "weights": {
    "JustANormalTinkerer/hayai-ocr-v2.5-nova": "<commit sha>",
    "google/siglip2-base-patch16-naflex": "<commit sha>",
    "Kellenok/PP-OCRv6_manga": "<commit sha>"
  }
}
```

The `<Volume>.mokuro` from mokuro is upstream mokuro's shape plus an
`ocr_engine` block with its `id` and the `precision` it was read at.

Every Hugging Face repo is pinned to a commit — recognizers **and**
detectors. `hayai-nova` and PaddleOCR-VL load with `trust_remote_code=True`,
so without a pin a push to someone else's repository would change the Python
running on your machine, and a detector resolved at a moving `main` could box
differently between two runs of the same volume.

| Pinned in | Covers |
|---|---|
| `REPO_REVISIONS` (`ocr/engine_runner.py`) | every repo the runner loads: hayai-nova and its SigLIP2 processor, the PaddleOCR-VL base and manga LoRA |
| `REPO_REVISION` (`ocr/ppocr.py`) | `Kellenok/PP-OCRv6_manga`, for both the engine and the detector |
| `REVISION` (`ocr/detectors/animetext.py`) | the detectors with weights on the Hub |
| `WEIGHTS_SHA256` (`ocr/detectors/ctd.py`) | comic-text-detector, fetched by the `mokuro` package from an immutable release asset; the adapter verifies the file's sha256 before torch loads it (a `.pt` is a pickle, so a wrong file is code execution) |

Detector adapters carry their own pins and report what they loaded
(`_weights.json`, see `ocr/detectors/README.md`), which is how a detector's
commit reaches `ocr_engine.weights`. An adapter that cannot report claims
nothing, and PP-OCRv6 models loaded from a `MOKURO_PPOCR_MODELS` directory are
not claimed either.

To bump a pin: fetch the repo at the new commit into a scratch cache
(`HF_HOME=<scratch> hf download <repo> --revision <sha>`), read the diff — for
a `trust_remote_code` repo, the `.py` diff, since that is the code you agree
to run — re-run the engine over a test volume and compare the sidecar, then
change the sha and note it in the changelog. Never pin to a tag or a branch:
both move.

### Who wrote each sidecar

The file says what read it; the database says which machine. Every sidecar
the OCR worker installs, from a local run, a local session or a processor,
gets a row in the `ocr_sidecars` table (`ocr/provenance.py`): the sidecar's
library path, its volume, the generation (id and name), the machine (`local`
or the processor's name) and account, the engine, detector and precision,
the runner build (`mokuro-bunko <version>, runner <staged runner hash>`,
which a processor reports when it registers), pages and failed pages, and
the archive's size and mtime at the time.

There is one row per file on disk. A re-run replaces it. It is deleted when
the file goes: its volume is deleted or moved away over WebDAV, the sidecar
itself is deleted or overwritten over WebDAV, or the corrupt-sidecar sweep
removes it. A series folder moved over WebDAV takes its rows with it. A
sidecar with no row has an unknown producer (it predates the table, or
arrived some other way) and is never attributed.

The admin card's History line counts from these rows. A machine's share of
a generation is its sidecars of that row on disk now, so every machine's
share adds up to at most the total. The machine's lifetime count, re-runs
included, is in the line's tooltip. Every write and every refused result is
also in the audit log (see [configuration](configuration.md#audit-log)).

## `hayai-nova`'s patch budget

`patch_budget` is `max_num_patches` for hayai-nova's SigLIP2-NaFlex vision
tower. NaFlex fits the crop to the 16×16 patch grid that packs closest to the
budget, keeping its aspect ratio, then pads to exactly the budget — so the
budget *is* the resolution a line is read at: on a long vertical column it
buys 4, 5 or 6 patch rows across the glyph at 256, 384 or 512. Cost is linear
in the budget and independent of crop shape, and the memory difference across
the whole range is small next to the model itself, so lowering it saves time,
not memory. 512 is the default rather than the model card's 384 because on the
model author's own benchmark 384 reads worse than the previous hayai model
and only 512 beats it; dense vertical lettering and novel columns are exactly
where the difference shows.
