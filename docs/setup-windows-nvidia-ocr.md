# Mokuro Bunko on Windows: GPU OCR

mokuro-bunko 0.7 is one native program with no Python, uv or CUDA toolkit to
install. On Windows the **full** build runs OCR; its recognizers (hayai-nova,
paddle-manga) run on libtorch, which `mokuro-bunko install-ocr` installs once
as a *backend pack* for the hardware it finds:

| Pack | Hardware | Extra requirements |
|---|---|---|
| `cu130` | NVIDIA, Turing (GTX 16xx / RTX 20xx) or newer | NVIDIA driver **580 or newer**. Nothing else: the pack brings the CUDA libraries. |
| `cpu` | Everything else (AMD and Intel GPUs run OCR on the CPU on Windows) | Nothing. |

ppocr-manga and the PP-OCR text detector run on the CPU in either case. The
CPU is always the fallback.

> [!TIP]
> Run `mokuro-bunko.exe doctor` (or `doctor.bat` in the portable zip) first
> when something does not work. See [troubleshooting.md](troubleshooting.md).

## 1. Install

```powershell
powershell -c "irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.ps1 | iex"
```

It installs the full build into `%LOCALAPPDATA%\mokuro-bunko\app`, keeps your
library, config and logs in `%LOCALAPPDATA%\mokuro-bunko`, adds Start-menu
shortcuts, runs `doctor` and starts the server. No administrator is needed.
Add `-Startup` to start the server at every logon, `-Portable` to keep
everything beside the program:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.ps1))) -Startup
```

Prefer a zip? Download
`mokuro-bunko-<version>-x86_64-pc-windows-msvc-full.zip` from the
[releases page](https://github.com/Gnathonic/mokuro-bunko/releases), extract
it anywhere and run `run.bat`. In the portable zip everything stays in
`data\` next to it.

## 2. Check the GPU driver

Open a terminal and run:

```powershell
nvidia-smi
```

The "Driver Version" must be **580 or newer** for the CUDA pack. The CUDA
Toolkit and cuDNN are not needed: the pack carries the CUDA libraries it uses.
With an older driver `install-ocr` installs the CPU pack and tells you to
update the driver.

## 3. Install the OCR backend

From a terminal in the install folder (`%LOCALAPPDATA%\mokuro-bunko\app`, or
the folder you extracted the zip into):

```powershell
.\mokuro-bunko.exe install-ocr --list    # what it detects and would install
.\mokuro-bunko.exe install-ocr
```

On an NVIDIA GPU it installs the `cu130` pack: the pack from the release plus
about 1.3 GB of CUDA libraries from NVIDIA's own packages on PyPI, each
checked against its pinned sha256 (about 1.9 GB on disk). Otherwise it
installs the `cpu` pack (about 0.3 GB on disk). The pack goes into
`backends\` under your data folder, and the models are fetched right after.
Then restart the server (close its window and run `run.bat` or the
Start-menu shortcut again).

## 4. Start the server and choose the backend

Run `run.bat`, or use the Start-menu shortcut, and finish setup in the
browser that opens (`http://127.0.0.1:8080`).

`ocr.backend: auto` (the default) uses the pack's GPU when there is one, else
the CPU. To keep OCR off the GPU, set it in `config.yaml` or from the command
line:

```powershell
mokuro-bunko.exe serve --ocr cpu      # or: auto, cuda
mokuro-bunko.exe config set ocr.backend cpu
```

If the GPU cannot be used (an old driver, say), the server logs why in
`%LOCALAPPDATA%\mokuro-bunko\logs\server.log` and runs OCR on the CPU.
Without any pack, hayai-nova and paddle-manga do not run at all (only
ppocr-manga does) until you run `install-ocr`.

## 5. First OCR run: models

The OCR models are fetched by `install-ocr`, or the first time a volume is
OCR'd, into `models\` under your data folder, and verified: about 0.5 GB for
hayai-nova (its compiled package for your GPU or CPU), about 2 GB more for
paddle-manga if you enable it. To fetch them up front later (for example
after enabling paddle-manga):

```powershell
mokuro-bunko.exe models download
```

Drop a `.cbz` into `<data>\library\<Series>\` (or upload it over WebDAV or
the reader) and watch it appear on `http://127.0.0.1:8080/queue`. A
`<Volume>.mokuro` file and a `.webp` cover appear beside it when it is done.

## Using this PC as an OCR processor for another library

If the library runs on another (small) server, make this PC its processor.
Install the pack, then set the processor up (or use the desktop app's
processor setup, which does both):

```powershell
.\mokuro-bunko.exe install-ocr
.\mokuro-bunko.exe processor setup
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
  run `mokuro-bunko.exe install-ocr --variant cu130`.
- The server log lists the devices the pack found when it loaded
  (`libtorch backend cu130 ... loaded`), and why a GPU was not used.
- `install-ocr` says the driver is too old: update the NVIDIA driver to 580 or
  newer, then run `install-ocr` again.
- After an update, if the pack no longer loads ("install the pack of this
  release"), run `mokuro-bunko.exe install-ocr --force`.
- Windows SmartScreen may warn about the downloaded executable; the binaries
  are not Authenticode signed. Choose "More info", then "Run anyway".
- Antivirus software that quarantines `mokuro-bunko.exe` or the pack's DLLs
  breaks the install; add an exception for the install and data folders.
- Updates come from the admin panel's Updates card (signed, checksum-verified)
  or by running `install.ps1` again; your data is kept.
