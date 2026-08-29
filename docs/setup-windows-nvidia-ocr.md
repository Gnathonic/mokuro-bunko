# Mokuro Bunko on Windows with NVIDIA (CUDA) OCR

> [!TIP]
> **You probably don't need this document.** The one-command setup script now
> automates everything below (including the pitfalls):
> ```powershell
> powershell -c "irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/setup-windows.ps1 | iex"
> ```
> Or use the portable folder edition (`scripts\build-portable.ps1`). This
> document remains as the manual deep-dive / reference for what the script
> does. If something breaks, start with `mokuro-bunko doctor` and
> [troubleshooting.md](troubleshooting.md).

A reproducible, end-to-end record of installing **mokuro-bunko** from source on
Windows 11, enabling **GPU (CUDA) OCR** on an NVIDIA card, and verifying it by
OCR-ing a real manga volume.

Verified on **2026-07-09** with:

| Component | Value |
|---|---|
| OS | Windows 11 Pro (10.0.26200) |
| GPU | NVIDIA GeForce RTX 3070 (8 GB, Ampere, compute capability 8.6) |
| Driver | NVIDIA 610.62, CUDA UMD 13.3 (via `nvidia-smi`) |
| mokuro-bunko | 0.1.7 (from source, `main`) |
| Python (server) | 3.12.13 (provisioned by `uv`) |
| OCR stack | torch 2.13.0+cu130, torchvision 0.28.0+cu130, mokuro 0.2.4, manga-ocr 0.1.14 |

> [!IMPORTANT]
> On this date `install-ocr` produced a **broken OCR environment out of the box**
> because it pulled `transformers 5.13.0`, which cannot load the `manga-ocr-base`
> tokenizer. The fix (pin `transformers<5`) is documented in
> [Step 8](#step-8-fix-the-ocr-environment-transformers-pin) and
> [Known issues](#known-issues). If OCR silently produces no `.mokuro` files,
> this is almost certainly why.

---

## Prerequisites

- An NVIDIA GPU with a recent driver already installed on the host. Confirm with:
  ```powershell
  nvidia-smi
  ```
  You do **not** need the CUDA Toolkit installed — the PyTorch wheels bundle the
  CUDA runtime. Only the driver matters. (mokuro-bunko installs Python packages
  only; GPU **drivers** must be present on the host.)
- Git.
- No system Python is required — `uv` provisions its own.

---

## Step 1 — Install `uv`

`uv` is the Python/project manager mokuro-bunko uses. Install it (no admin needed):

```powershell
irm https://astral.sh/uv/install.ps1 | iex
```

It installs to `C:\Users\<you>\.local\bin`. Add that to `PATH` for the session
(or restart the shell):

```powershell
$env:Path = "C:\Users\$env:USERNAME\.local\bin;$env:Path"
uv --version   # -> uv 0.11.28
```

## Step 2 — Clone the repository

```powershell
git clone https://github.com/Gnathonic/mokuro-bunko.git
cd mokuro-bunko
```

## Step 3 — Pin Python 3.12 (important for CUDA)

> **Now automatic:** the repo ships a `.python-version` file pinning 3.12, so
> `uv sync` handles this with no action needed. Kept for reference.

The GPU OCR wheels (`cu130`) require a supported interpreter, and mokuro-bunko's
installer **refuses the CUDA backend on Python ≥ 3.13**. The OCR virtualenv is
created with the *same* Python the server runs on, so pin the project to 3.12:

```powershell
uv python pin 3.12   # not needed anymore -- .python-version is in the repo
```

> If `uv python install`/`pin` prints
> `Missing expected target directory for Python minor version link ...`, it is a
> cosmetic Windows symlink quirk — `uv sync` in the next step still resolves the
> interpreter and works.

## Step 4 — Install the server and its dependencies

```powershell
uv sync
```

This creates `.venv` (Python 3.12) and installs mokuro-bunko 0.1.7 plus the
lightweight server deps (wsgidav, cheroot, Pillow, bcrypt, …). Sanity check:

```powershell
uv run mokuro-bunko --version   # -> mokuro-bunko, version 0.1.7
```

## Step 5 — Confirm the CUDA backend is detected

```powershell
uv run mokuro-bunko install-ocr --list-backends
```

Expected on an NVIDIA host:

```
Supported OCR backends:
  - cuda
  - cpu
Unavailable backends:
  - rocm: ROCm backend requires a detected AMD ROCm setup
  - mps: MPS backend requires Apple Silicon (macOS arm64)
```

`cuda` appearing here means `nvidia-smi` was found and the GPU backend is
eligible.

## Step 6 — Install the CUDA OCR environment

The heavy ML stack lives in an isolated venv at `.\.ocr-env` (kept separate from
the server). Install it for CUDA:

```powershell
uv run mokuro-bunko install-ocr --backend cuda
```

This downloads **torch/torchvision from the `cu130` index (~1.9 GB)** and then
`mokuro` + `manga-ocr`. Takes several minutes on first run. It ends with
`OCR installation complete!`.

## Step 7 — Verify PyTorch actually sees the GPU

```powershell
& .\.ocr-env\Scripts\python.exe -c "import torch; print(torch.__version__); print('cuda avail:', torch.cuda.is_available()); print(torch.cuda.get_device_name(0))"
```

Verified output:

```
2.13.0+cu130
cuda avail: True
NVIDIA GeForce RTX 3070
```

## Step 8 — Fix the OCR environment (transformers pin)

> **Fixed in the installer (historical note).** `install-ocr` now installs
> `mokuro "transformers>=4.25,<5" sentencepiece` and smoke-tests the
> environment afterwards, so this failure is caught (or prevented) at install
> time. If you have an OCR env from before the fix, just reinstall:
> `mokuro-bunko install-ocr --force`. Original issue kept for reference:

As of 2026-07-09, an unpinned install resolved `transformers` to `5.13.0`.
manga-ocr's `kha-white/manga-ocr-base` tokenizer cannot be instantiated under
transformers 5.x — OCR fails with:

```
ValueError: Couldn't instantiate the backend tokenizer ...
You need to have sentencepiece or tiktoken installed ...
```

Because `mokuro` swallows the per-volume error and still exits 0, the server
silently produced thumbnails but never `.mokuro` files. (Failures like this
are now also captured to `<storage>/logs/ocr/<volume>.log` and surfaced on
the Queue page.) The manual fix was:

```powershell
& .\.ocr-env\Scripts\pip.exe install "transformers==4.46.3" sentencepiece
```

Resolves to: `transformers 4.46.3`, `tokenizers 0.20.3`, `huggingface-hub 0.36.2`
(torch stays `2.13.0+cu130`).

## Step 9 — Configure the server

`mokuro-bunko setup` is an **interactive** wizard (storage path, port, SSL, admin
user, registration mode, connectivity). For an automated/reproducible setup you
can instead write the config file directly and create the admin from the CLI.

Default config path on Windows: `%LOCALAPPDATA%\mokuro-bunko\config.yaml`.
Minimal config that forces the CUDA backend and enables the web catalog:

```yaml
# %LOCALAPPDATA%\mokuro-bunko\config.yaml
server:
  host: 127.0.0.1        # or 0.0.0.0 to expose on the LAN
  port: 8080
storage:
  base_path: C:/Users/<you>/AppData/Local/mokuro-bunko
registration:
  mode: disabled
  allow_anonymous_browse: true
  allow_anonymous_download: true
ocr:
  backend: cuda          # force the NVIDIA backend
  poll_interval: 30
catalog:
  enabled: true
```

Create an admin user (non-interactively) and confirm:

```powershell
$cfg = "$env:LOCALAPPDATA\mokuro-bunko\config.yaml"
uv run mokuro-bunko -c $cfg admin add-user admin --role admin --password "<password>"
uv run mokuro-bunko -c $cfg admin list-users
```

(Or just run `uv run mokuro-bunko setup` and answer the prompts.)

## Step 10 — Run the server

```powershell
uv run mokuro-bunko -c $cfg serve
```

- Serves WebDAV + the web catalog at `http://<host>:8080/`.
- Starts a background **OCR worker** that scans `storage\library\` every
  `poll_interval` seconds for `.cbz` volumes missing a `.mokuro` sidecar,
  OCR's them on the GPU, and writes `<name>.mokuro` + `<name>.webp` next to
  each volume.

Point [Mokuro Reader](https://reader.mokuro.app) at the server, or upload via any
WebDAV client. Files land in `storage\library\` and are OCR'd automatically.

---

## How OCR is triggered (mental model)

- Volumes are OCR'd when they sit in `storage\library\<Series>\<Volume>.cbz`
  **without** a matching `.mokuro`. WebDAV uploads land there directly.
- The server worker (`OCRWorker`) runs, per volume:
  ```
  .ocr-env\Scripts\python.exe -m mokuro "<volume>" --disable_confirmation --no_cache
  ```
  on `device cuda`, then imports the resulting sidecar back next to the `.cbz`.
- Output artifacts per volume: `<Volume>.mokuro` (OCR overlay JSON) and
  `<Volume>.webp` (cover thumbnail).

---

## Verification / test with a volume

A copyright-free test volume was generated: 4 pages of vertical Japanese text
rendered into manga-style speech bubbles, packed as `Bunko Test 01.cbz`
(see [`make_volume.py`](#appendix-test-volume-generator)).

### GPU was used

- mokuro log: `Initializing text detector, using device cuda`.
- `nvidia-smi` during the run: GPU memory rose **1192 MiB → 2130 MiB (+~940 MB)**
  and utilization spiked while the 4 pages were processed, then dropped — i.e.
  the mokuro process allocated a CUDA context and ran inference on the RTX 3070.
- 4 pages OCR'd in ~8 s.

### OCR output was correct

Running the exact worker command on the volume produced `Bunko Test 01.mokuro`
(`Processed successfully: 1/1`). Every speech bubble was recognized verbatim:

| Page | Recognized Japanese |
|---|---|
| 1 | おはようございます / きょうはいい天気ですね |
| 2 | 本を読むのが好きです / このマンガは面白い |
| 3 | 日本語を勉強しています / がんばってください |
| 4 | またあした会いましょう / さようなら |

### End-to-end through the running server

With the OCR env fixed, `Bunko Test 01.cbz` was placed in
`storage\library\Bunko Test\` and the server started. The OCR worker picked it
up automatically (~36 s) — server log:

```
OCR worker enabled (configured=cuda, active=cuda, interval=30s)
[OCR] Found 1 CBZ files missing mokuro sidecars
[OCR] Running: ...\.ocr-env\Scripts\python.exe -m mokuro ...\Bunko Test 01 --disable_confirmation --no_cache
[OCR] Normalized sidecar metadata: Bunko Test 01.mokuro
[OCR] Created sidecar: Bunko Test 01.mokuro
```

It produced `library\Bunko Test\Bunko Test 01.mokuro` (metadata normalized to
`title=Bunko Test`, stable `title_uuid`) plus the `.webp` thumbnail, and the
volume is served over WebDAV where Mokuro Reader consumes it:

```
PROPFIND /mokuro-reader/Bunko%20Test/  -> 207
  /mokuro-reader/Bunko Test/Bunko Test 01.cbz
  /mokuro-reader/Bunko Test/Bunko Test 01.mokuro
  /mokuro-reader/Bunko Test/Bunko Test 01.webp
```

(The library is exposed under the virtual WebDAV root **`/mokuro-reader/`**, not
`/library/`.)

---

## Known issues

1. ~~**`transformers 5.x` breaks manga-ocr (must pin `<5`).**~~ **Fixed:**
   `OCRInstaller.install_mokuro()` now pins `"transformers>=4.25,<5"` and
   installs `sentencepiece`, and every install is smoke-tested afterwards.
2. ~~**Silent OCR failure.**~~ **Fixed:** mokuro's output is captured to
   `<storage>/logs/ocr/<volume>.log`, exit-0 failures are detected from
   mokuro's own summary, failures are persisted (with the reason) and shown
   on the Queue page, and broken volumes retry with exponential backoff
   instead of every 30s.
3. **`uv` minor-version link warning** on Windows is cosmetic; `uv sync` works.
4. **Server stdout is block-buffered** when piped; set `PYTHONUNBUFFERED=1` to see
   OCR progress lines live. (Everything is also written to
   `<storage>/logs/server.log` regardless.)

### Debugging OCR by hand

Reproduce exactly what the server does, with visible output:

```powershell
& .\.ocr-env\Scripts\python.exe -m mokuro "C:\path\to\a\volume-folder-or.cbz" --disable_confirmation --no_cache
```

---

## Appendix: test volume generator

[`make_volume.py`](make_volume.py) (in this folder) draws 4 manga-style pages of
vertical Japanese text in speech bubbles (using `C:\Windows\Fonts\msgothic.ttc`)
and packs them into a `.cbz`. Run it in the OCR or server venv (both have
Pillow); args are `<output-pages-dir> <output-cbz-path>`:

```powershell
uv run python docs\make_volume.py .\pages ".\Bunko Test 01.cbz"
```

The key idea: any folder of page images (or a `.cbz`/`.zip`) is a valid mokuro
"volume". Drop the `.cbz` into `storage\library\<Series>\` (or upload via WebDAV)
and the server OCRs it automatically.
