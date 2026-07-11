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
| Per-volume OCR output | `<storage>/logs/ocr/<volume>.log` |
| Failure records (JSON) | `<storage>/.ocr-failures.json` |
| Windows setup script log | `%TEMP%\mokuro-bunko-setup.log` |

`<storage>` defaults to `%LOCALAPPDATA%\mokuro-bunko` on Windows and
`~/.local/share/mokuro-bunko` on Linux/macOS (portable edition: `data\`
inside the folder).

## "My volumes never get OCR'd / no .mokuro files appear"

1. Open the Queue page: `http://<server>:8080/queue`. Volumes that failed
   OCR are listed under **Failed** with the error, attempt count, and log
   path. (Failed volumes retry with exponential backoff — up to 1 hour
   between attempts — so they don't hammer your GPU forever.)
2. Read the per-volume log at `<storage>/logs/ocr/<volume>.log` — this is
   the OCR engine's full output, including tracebacks.
3. Run `mokuro-bunko doctor`. The most common cause is a broken OCR
   environment, which shows up as an `OCR stack` FAIL.
4. To force a retry immediately: fix the cause, then either replace the
   `.cbz` file (updating its timestamp resets the failure record) or
   restart the server.

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
restart the server and check `server.log`.

## Windows setup script issues

The one-liner writes a transcript to `%TEMP%\mokuro-bunko-setup.log`. Each
step prints what it's doing; the failure message points at the step that
broke. The script is idempotent — re-running it is safe and skips what's
already done.
