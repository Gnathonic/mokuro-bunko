# Mokuro Bunko on Windows: GPU OCR

mokuro-bunko 0.7 is one native program with no Python, uv or CUDA toolkit to
install. On Windows the **full** build runs OCR; its recognizers (hayai-nova,
paddle-manga) run on libtorch, which the server installs by itself (or
`install-ocr` does) as a *backend pack* for the hardware it finds:

| Pack | Hardware | Extra requirements |
|---|---|---|
| `cu130` | NVIDIA, Turing (GTX 16xx / RTX 20xx) or newer | NVIDIA driver **580 or newer**. Nothing else: the pack brings the CUDA libraries. |
| `cpu` | Everything else (AMD and Intel GPUs run OCR on the CPU on Windows) | Nothing. |

ppocr-manga and the PP-OCR text detector run on the CPU in either case. The
CPU is always the fallback.

> [!TIP]
> Run `mokuro-bunko-cli.exe doctor` (or `doctor.bat`) first when something does
> not work. See [troubleshooting.md](troubleshooting.md). In a terminal always use
> `mokuro-bunko-cli.exe`: `mokuro-bunko.exe` is the app, with no console window.

## 1. Install

```powershell
powershell -c "irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.ps1 | iex"
```

It installs the full build into `%LOCALAPPDATA%\mokuro-bunko\app`, keeps your
library, config and logs in `%LOCALAPPDATA%\mokuro-bunko`, adds Start-menu
shortcuts, runs `doctor` and starts the app: a tray icon by the clock, which
runs the server, and your browser at `http://127.0.0.1:8080`. No administrator
is needed. Add `-Startup` to start the app at every logon, `-Portable` to keep
everything beside the program:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.ps1))) -Startup
```

These take the latest release; for a specific version, use the script of its
tag with `-Version`, e.g. `& ([scriptblock]::Create((irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/v0.7.0/scripts/install.ps1))) -Version 0.7.0`.

Prefer a zip? Download `mokuro-bunko-<version>-windows.zip` from the
[releases page](https://github.com/Gnathonic/mokuro-bunko/releases), extract
it anywhere and double-click `mokuro-bunko.exe` (the tray; the first time it
opens a chooser in your browser: pick **Library server**), or run `run.bat`
for the server in a console window. In the zip everything stays in `data\`
next to it (portable mode).

## 2. Check the GPU driver

Open a terminal and run:

```powershell
nvidia-smi
```

The "Driver Version" must be **580 or newer** for the CUDA pack. The CUDA
Toolkit and cuDNN are not needed: the pack carries the CUDA libraries it uses.
With an older driver `install-ocr` installs the CPU pack and tells you to
update the driver.

## 3. Set up the server and the OCR backend

The first visit to `http://127.0.0.1:8080` walks you through the setup: the
admin account, who may join, remote access and, last, **OCR on this machine**
(on by default when a usable GPU is found; Auto, CPU or the GPU found). When
you finish, the server installs the OCR backend by itself, in the background
while it already serves, and opens the admin panel's **This server** tab,
which shows the progress (and has Install / Reinstall / Remove).

On an NVIDIA GPU it installs the `cu130` pack: the pack from the release plus
about 1.3 GB of CUDA libraries from NVIDIA's own packages on PyPI, each
checked against its pinned sha256 (about 1.9 GB on disk). Otherwise it
installs the `cpu` pack (about 0.3 GB on disk). The pack goes into
`backends\` under your data folder, and the models are fetched right after;
local OCR starts by itself when it is done.

To do it by hand instead, from a terminal in the install folder
(`%LOCALAPPDATA%\mokuro-bunko\app`, or the folder you extracted the zip into):

```powershell
.\mokuro-bunko-cli.exe install-ocr --list    # what it detects and would install
.\mokuro-bunko-cli.exe install-ocr
```

A server that was running while you did that picks the pack up after a
restart (Quit in the tray and start it again, or close the `run.bat` window and
run it again).

## 4. Choose the backend

`ocr.backend: auto` (the default) uses the pack's GPU when there is one, else
the CPU. To keep OCR off the GPU, choose CPU in the admin panel (**This
server** → OCR), or from the command line:

```powershell
.\mokuro-bunko-cli.exe serve --ocr cpu      # or: auto, cuda
.\mokuro-bunko-cli.exe config set ocr.backend cpu
```

If the GPU cannot be used (an old driver, say), the server logs why in
`%LOCALAPPDATA%\mokuro-bunko\logs\server.log` and runs OCR on the CPU.
Without any pack, hayai-nova and paddle-manga do not run at all (only
ppocr-manga does) until the backend is installed.

## 5. First OCR run: models

The OCR models are fetched by `install-ocr`, or the first time a volume is
OCR'd, into `models\` under your data folder, and verified: about 0.5 GB for
hayai-nova (its compiled package for your GPU or CPU), about 2 GB more for
paddle-manga if you enable it. To fetch them up front later (for example
after enabling paddle-manga):

```powershell
.\mokuro-bunko-cli.exe models download
```

Drop a `.cbz` into `<data>\library\<Series>\` (or upload it over WebDAV or
the reader) and watch it appear on `http://127.0.0.1:8080/queue`. A
`<Volume>.mokuro` file and a `.webp` cover appear beside it when it is done.

## Using this PC as an OCR processor for another library

If the library runs on another (small) server, make this PC its processor:
double-click `mokuro-bunko.exe` and choose **Processor** in the chooser (with
nothing set up yet it opens by itself). Its pairing page checks the account,
saves `processor.yaml` and starts the processor from the tray; its OCR backend
installs in the background. Or from a terminal:

```powershell
.\mokuro-bunko-cli.exe processor setup
```

The processor keeps its pack and models in `%LOCALAPPDATA%\mokuro-bunko-processor`.
It also finds a pack installed in the library's default storage, as the one
above is. Once `processor.yaml` exists, `install-ocr --processor` installs
into the processor's own storage.

It asks for the library URL and a `processor` account, writes
`processor.yaml` and can start the processor at every logon (a Startup
entry). See [deployment.md](deployment.md#remote-ocr-processors).

## Troubleshooting

- `doctor` should show the build as `full`, an `OCR backend` line naming the
  `cu130` pack in use, and the models as present. A WARN line saying "the
  cu130 pack would use this machine's GPU" means the CPU pack is installed:
  run `mokuro-bunko-cli.exe install-ocr --variant cu130`.
- The server log lists the devices the pack found when it loaded
  (`libtorch backend cu130 ... loaded`), and why a GPU was not used.
- `install-ocr` says the driver is too old: update the NVIDIA driver to 580 or
  newer, then run `install-ocr` again.
- An update installs the new release's pack together with the program. A
  pack of another release does not load ("the backend pack is from
  mokuro-bunko A, this is B"): the server installs its own in the background,
  or run `mokuro-bunko-cli.exe install-ocr`.
- Windows SmartScreen may warn about the downloaded executable; the binaries
  are not Authenticode signed. Choose "More info", then "Run anyway".
- Antivirus software that quarantines `mokuro-bunko.exe` or the pack's DLLs
  breaks the install; add an exception for the install and data folders.
- Updates come from the admin panel's Updates card (signed, checksum-verified)
  or by running `install.ps1` again; your data is kept.
