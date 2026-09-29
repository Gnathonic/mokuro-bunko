# Troubleshooting

Start here when something doesn't work.

## First step: run the doctor

```bash
uv run mokuro-bunko doctor        # from a source install
doctor.bat                        # portable edition
```

It checks — with fix hints for anything wrong:

- Python version (CUDA OCR needs < 3.13; the repo pins 3.12)
- Config validity and storage writability
- NVIDIA driver presence (`nvidia-smi`)
- OCR environment + full stack smoke test (torch, CUDA availability,
  transformers version pin, sentencepiece, manga-ocr, mokuro)
- Free disk space
- Whether the configured port is already in use
- Volumes currently failing OCR

Exit code is 0 unless a `FAIL` is found, so scripts can gate on it.

## Where the logs are

| Log | Location |
|---|---|
| Server log (rotating) | `<storage>/logs/server.log` |
| Per-volume OCR output | `<storage>/logs/ocr/<series>_<volume>.log` (primary generation), `<series>_<volume>.<generation>.log` (every other) |
| Failure records (JSON) | `<storage>/.ocr-failures.json` |
| Processor state | `uv run mokuro-bunko processor status --config processor.yaml` (on the processor) |
| Processor output | `journalctl --user -u mokuro-bunko-processor` (Linux user service), its window (Windows) |
| Windows setup script log | `%TEMP%\mokuro-bunko-setup.log` |

`<storage>` defaults to `%LOCALAPPDATA%\mokuro-bunko` on Windows and
`~/.local/share/mokuro-bunko` on Linux/macOS (portable edition: `data\`
inside the folder).

## "My volumes never get OCR'd / no .mokuro files appear"

1. Open the Queue page: `http://<server>:8080/queue`. Volumes that failed
   OCR are listed under **Failed** with the error, attempt count, and log
   path. (Failed volumes retry with exponential backoff — up to 1 hour
   between attempts — so they don't hammer your GPU forever.)
2. Read the per-volume log at `<storage>/logs/ocr/<series>_<volume>.log`
   (the Failed entry shows admins the exact path; every generation other
   than the primary one adds its name, `<series>_<volume>.<generation>.log`)
   — this is the OCR engine's full output, including tracebacks.
3. Run `mokuro-bunko doctor`. The most common cause is a broken OCR
   environment, which shows up as an `OCR stack` FAIL.
4. To force a retry immediately: fix the cause, then either replace the
   `.cbz` file (updating its timestamp resets the failure record) or
   restart the server.
5. If the queue page says the queue is **held**, see the next section.

## "The queue is held" / "No processor connected"

The server runs no OCR of its own when `ocr.local_processing` is `false` or
`ocr.backend` is `skip`; it waits for a remote processor. The queue page and
the admin panel then say "No processor connected since …", and the server
log says once `OCR queue held: local processing is off and no processor is
connected`. Either start a processor (`uv run mokuro-bunko processor setup`, see
[deployment](deployment.md#remote-ocr-processors)) or turn local processing
back on and restart.

## A processor connects but never does any work

Almost always a proxy between the processor and the library buffering the
`/_processor/` paths or limiting their body size. The processor registers,
but the library never hears its session events and gives up on the session.
Turn off request and response buffering and the body size limit for
`/_processor/` — see
[Remote OCR processors behind a proxy](deployment.md#remote-ocr-processors-behind-a-proxy)
— or point the processor at an address that reaches the library directly.

Also check:

- `MOKURO_NGINX_ACCEL=1` is set only where nginx really serves downloads.
  Without it every archive download comes back empty, and the processor
  gives the volumes back.
- The processor has the detector a generation needs. A processor is only
  offered rows it can run; install others with
  `uv run mokuro-bunko processor install --config processor.yaml --detector ctd`.
- The admin panel's Processors card shows each processor's state, its last
  refused login and how its downloads have gone.

## A processor is refused at registration

- **"this library speaks protocol [2]"** — the processor and the library run
  different releases. Stop the processor, update both, restart the library,
  then start the processor (see
  [Updating a library and its processors](deployment.md#updating-a-library-and-its-processors)).
- **Login refused** — the account does not exist, has the wrong password, is
  disabled, or does not have the `processor` role. The Processors card lists
  refused logins with the username tried, and the processor exits with a
  non-zero status.
- **"another processor is running on …"** — two processors share one
  `processor.storage`; give each its own.

## Setting up a processor

### Windows: uv sync fails with "os error 448" (untrusted mount point)

`uv sync` stops with `os error 448` and "untrusted mount point" in a terminal
opened with "Run as administrator", or in an SSH session (which is elevated
too). uv links its Python through a folder junction that the unelevated
account created, and Windows refuses to follow it from an elevated process.

Run the setup from a normal terminal. Where that is not possible, give this
session a Python of its own, in `cmd`:

```bat
set "UV_PYTHON_INSTALL_DIR=%LOCALAPPDATA%\mokuro-bunko-python"
uv python install 3.12
uv sync
```

(PowerShell: `$env:UV_PYTHON_INSTALL_DIR = "$env:LOCALAPPDATA\mokuro-bunko-python"`.)

A failed sync can leave a `.venv` that points at the junction; every
command then fails with "uv trampoline failed to spawn Python" (os error 2).
Delete the checkout's `.venv` folder and run `uv sync` again.

### An AMD GPU, but the processor runs on the CPU

`processor setup` prints `Hardware: ... -> cpu`, or the install ends with a
boxed `WARNING: ... runs OCR on the CPU instead`. ROCm needs only the
kernel driver, so check what it exposes:

- `ls -l /dev/kfd /dev/dri/renderD*`: both must exist. No `/dev/kfd` means
  the `amdgpu` driver is not loaded with compute support.
- Where they are `crw-rw----`, you must be in their groups (`render`, often
  `video`): `sudo usermod -aG render,video "$USER"`, then log out and in;
  `id` should list them.
- Reinstall and read the smoke test:
  `uv run mokuro-bunko processor install --config processor.yaml --force`.
  For a card the PyTorch build has no kernels for, it prints
  `ROCm: this card is not in the torch build; HSA_OVERRIDE_GFX_VERSION=10.3.0`
  (or its family's value), then `cuda available: True` and the card's name.
  A value of `HSA_OVERRIDE_GFX_VERSION` you set yourself is kept; if the
  test fails with one set, try without it.

AMD GPU OCR needs Linux; on Windows a processor uses NVIDIA or the CPU.

### `processor status` says "never connected"

In the first seconds after the processor starts this is normal: the status
file catches up a few seconds after the connection. Ask again a little
later. If it stays, the processor's own output (`journalctl --user -u
mokuro-bunko-processor`, or its window on Windows) says why.

### "Bad CRC-32" or "inflate: incorrect data check" during the install

A GPU install is gigabytes of wheels. One damaged download is retried once
without pip's cache before the installer settles for the CPU. If the error
comes back on every attempt, in a different file each time, the downloads
are not the problem: the machine is corrupting data in memory (unstable RAM,
an overclock or XMP profile, overheating). Run a memory and stability test
before trusting it as a processor: a machine that corrupts its downloads
can corrupt the OCR results it sends too.

On the library, a processor delivering bad results shows in the admin
panel's audit log: results the library refused are logged as
`ocr_sidecar_rejected` with the processor's account as the actor, and every
sidecar on disk records which machine wrote it (see
[configuration](configuration.md#audit-log) and
[OCR internals](ocr-internals.md#who-wrote-each-sidecar)).

## Which machine gets which volume?

With several machines, each volume goes to the machine predicted to finish it
first, so a slower machine may leave a volume for a faster one (for at most
15 seconds past when that machine was expected to take it). To see every
decision, start the library with `MOKURO_EFT_TRACE=1`; each claim then logs
which machine asked, what it took, and what it left to whom.

## Known failure: transformers 5.x tokenizer error

Symptom in the OCR log:

```
ValueError: Couldn't instantiate the backend tokenizer ...
You need to have sentencepiece or tiktoken installed ...
```

Cause: a `transformers` 5.x release in the OCR environment — manga-ocr's
tokenizer requires `transformers >=4.25,<5`. The installer pins this
automatically (and the post-install smoke test catches it), so this should
only appear in environments installed before the pin existed. Fix:

```bash
mokuro-bunko install-ocr --force
```

## GPU not being used

- `doctor` should show `cuda available: True (<your GPU>)` under
  `OCR stack`. If it shows `False`:
  - Confirm `nvidia-smi` works in a terminal (driver installed?).
  - Python must be < 3.13 (CUDA wheel availability) — the repo's
    `.python-version` pins 3.12; `doctor` warns otherwise.
  - Reinstall the backend: `mokuro-bunko install-ocr --backend cuda --force`.
- If the configured backend is unavailable at launch, the server logs a
  warning and falls back to CPU — check `<storage>/logs/server.log`.

## Port already in use

`doctor` reports it. Either another instance of the server is running, or
another app owns the port. Change `server.port` in the config, or stop the
other process.

## Server health at a glance

`GET /api/health` includes an `ocr` section:

```json
"ocr": {"backend": "cuda", "worker_alive": true, "pending": 0, "failed": 0}
```

`worker_alive: false` means the background OCR loop stopped heartbeating —
restart the server and check `server.log`. With `ocr.backend: skip` the
section reads `{"backend": "skip", "worker_alive": null, "pending": null,
"failed": 0}`: this server runs no OCR, so there is no loop to report on.

## Windows setup script issues

The one-liner writes a transcript to `%TEMP%\mokuro-bunko-setup.log`. Each
step prints what it's doing; the failure message points at the step that
broke. The script is idempotent — re-running it is safe and skips what's
already done.
