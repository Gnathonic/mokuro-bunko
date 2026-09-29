# Mokuro Bunko on Windows with NVIDIA (CUDA) OCR

> [!TIP]
> **You probably don't need this document.** The one-command setup script
> does all of it:
> ```powershell
> powershell -c "irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/setup-windows.ps1 | iex"
> ```
> Or use the portable folder edition (`scripts\build-portable.ps1`). This page
> is the manual route, and what to check when something goes wrong. Start
> with `mokuro-bunko doctor` and [troubleshooting.md](troubleshooting.md).

## Prerequisites

- **An NVIDIA GPU with a recent driver.** Check with:
  ```powershell
  nvidia-smi
  ```
  The CUDA Toolkit is not needed: the PyTorch wheels bring their own CUDA
  runtime. Only the driver matters.
- **Git for Windows** ([git-scm.com](https://git-scm.com/download/win)), on
  `PATH`. The OCR installer fetches the optimized mokuro fork from GitHub.
- No system Python: `uv` provisions its own.

## 1. Install uv

```powershell
irm https://astral.sh/uv/install.ps1 | iex
```

It installs to `%USERPROFILE%\.local\bin`; open a new terminal (or add that
folder to `PATH`) so `uv --version` works.

Run everything below in a normal terminal, not one opened with "Run as
administrator" and not an SSH session: both are elevated, and there
`uv sync` can fail with "os error 448"
([troubleshooting](troubleshooting.md#windows-uv-sync-fails-with-os-error-448-untrusted-mount-point)).

## 2. Get the source and install the server

```powershell
git clone https://github.com/Gnathonic/mokuro-bunko.git
cd mokuro-bunko
uv sync
uv run mokuro-bunko --version
```

The repository pins Python 3.12 (`.python-version`), which `uv sync` uses.
CUDA OCR needs Python below 3.13, and the installer refuses CUDA on newer
interpreters.

## 3. Install OCR for CUDA

```powershell
uv run mokuro-bunko install-ocr --list-backends   # "cuda" should be listed
uv run mokuro-bunko install-ocr --backend cuda
```

This creates the OCR environment in `.ocr-env` inside the checkout: a CUDA
build of PyTorch (a download of about 2 GB) and mokuro. It ends by
smoke-testing the environment, so a broken install fails here rather than
silently later. Then check the whole setup:

```powershell
uv run mokuro-bunko doctor
```

`OCR stack` should report `cuda available: True` and your GPU's name.

Optional: the other OCR engines (`hayai-nova`, `paddle-manga`, `ppocr-manga`)
use a second environment, `.ocr-engines-env`, which the server creates when
a configured generation needs it. To install it up front:

```powershell
uv run mokuro-bunko install-ocr --backend cuda --engines hayai-nova,paddle-manga,ppocr-manga
```

## 4. Configure and start

```powershell
uv run mokuro-bunko serve
```

Open `http://localhost:8080/` in a browser: the first visit walks you through
creating the admin account and the basic settings. (`uv run mokuro-bunko
setup` does the same in the console.) The config file is
`%LOCALAPPDATA%\mokuro-bunko\config.yaml`; everything in it is described in
the [configuration reference](configuration.md). To force the GPU backend
rather than let `auto` choose:

```yaml
ocr:
  backend: cuda
```

## 5. See it work

Upload a `.cbz` through a WebDAV client or Mokuro Reader, or copy one into
`%LOCALAPPDATA%\mokuro-bunko\library\<Series>\`. Within `poll_interval`
seconds (30 by default) the OCR worker picks it up; the Queue page
(`http://localhost:8080/queue`) shows it running, and when it is done
`<Volume>.mokuro` and `<Volume>.webp` appear beside the archive. The server
log (`%LOCALAPPDATA%\mokuro-bunko\logs\server.log`) says which backend the
OCR worker started with (`OCR worker enabled (configured=cuda, active=cuda,
…)`).

For a test volume without copyright concerns, [`make_volume.py`](make_volume.py)
draws four pages of vertical Japanese text in speech bubbles (using
`C:\Windows\Fonts\msgothic.ttc`) and packs them into a `.cbz`:

```powershell
uv run python docs\make_volume.py .\pages ".\Bunko Test 01.cbz"
```

## As an OCR processor for another library

To lend this GPU to a library running on another machine instead of
running a library here, the machine needs only steps 1 and 2 (Git, the
driver, uv, the checkout and `uv sync`). Then:

```powershell
uv run mokuro-bunko processor setup
```

It asks for the library's URL and the `processor` account an admin created
there, checks them, writes `processor.yaml` (readable by your account only),
installs the CUDA OCR environments and offers to start the processor now
and at every logon (a Startup entry, no administrator needed). To add the
Startup entry later:

```powershell
uv run mokuro-bunko processor service --config processor.yaml --install
```

The processor then runs in its own minimized window while you are logged
in. The full walkthrough is in
[deployment](deployment.md#remote-ocr-processors).

## If it goes wrong

- **No `.mokuro` files appear.** Open the Queue page: failed volumes are
  listed with the reason. The full engine output is in
  `%LOCALAPPDATA%\mokuro-bunko\logs\ocr\`. See
  [troubleshooting.md](troubleshooting.md).
- **The install fails fetching mokuro.** Git is missing or not on `PATH`.
  Install Git for Windows, open a new terminal, and run `install-ocr` again.
- **`cuda available: False`.** Check `nvidia-smi` works, then reinstall:
  `uv run mokuro-bunko install-ocr --backend cuda --force`.
- **`uv` prints a warning about a missing minor-version link** while
  installing Python. It is cosmetic; `uv sync` still works.
- **Output does not appear live when piping the server's output.** Set
  `PYTHONUNBUFFERED=1`; everything also goes to `server.log`.
