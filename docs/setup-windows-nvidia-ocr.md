# Mokuro Bunko on Windows: GPU OCR

mokuro-bunko 0.7 is one native program with no Python, uv or CUDA toolkit to
install. On Windows there are two ways to run OCR on a GPU:

| Build | GPU API | Hardware | Extra requirements |
|---|---|---|---|
| **full** (default) | DirectML | Any DirectX 12 GPU: NVIDIA, AMD, Intel | A current graphics driver. Nothing else. |
| **full-cuda** | CUDA (DirectML remains available) | NVIDIA | NVIDIA driver **580 or newer**, plus the CUDA 13 and cuDNN 9 libraries on the system `PATH` |

Start with the default `full` build; it works on every DirectX 12 GPU. Use
`full-cuda` when you have an NVIDIA card and want CUDA's speed. The CPU is
always the fallback.

> [!TIP]
> Run `mokuro-bunko.exe doctor` (or `doctor.bat` in the portable zip) first
> when something does not work. See [troubleshooting.md](troubleshooting.md).

## 1. Install

```powershell
powershell -c "irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.ps1 | iex"
```

For the CUDA build:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.ps1))) -Flavor full-cuda
```

It installs into `%LOCALAPPDATA%\mokuro-bunko\app`, keeps your library,
config and logs in `%LOCALAPPDATA%\mokuro-bunko`, adds Start-menu shortcuts,
runs `doctor` and starts the server. No administrator is needed. Add
`-Startup` to start the server at every logon, `-Portable` to keep
everything beside the program.

Prefer a zip? Download
`mokuro-bunko-<version>-x86_64-pc-windows-msvc-full.zip` (or `-full-cuda.zip`)
from the [releases page](https://github.com/Gnathonic/mokuro-bunko/releases),
extract it anywhere and run `run.bat`. In the portable zip everything stays
in `data\` next to it.

## 2. Check the GPU driver

Open a terminal and run:

```powershell
nvidia-smi
```

For the CUDA build the "Driver Version" must be **580 or newer**, and the CUDA 13 and cuDNN 9 runtime libraries
must be found on the `PATH` (the `bin` folders of the CUDA 13 toolkit and
cuDNN 9 installs). The CUDA Toolkit's compiler is not needed; only its
runtime DLLs are. The default DirectML build does not need any of this: a
current driver for your GPU is enough.

## 3. Start the server and choose the provider

Run `run.bat`, or use the Start-menu shortcut, and finish setup in the
browser that opens (`http://127.0.0.1:8080`).

`ocr.backend: auto` (the default) picks the best provider the build offers.
To force one, set it in `config.yaml` or from the command line:

```powershell
mokuro-bunko.exe serve --ocr directml      # or: cuda, cpu
mokuro-bunko.exe config set ocr.backend cuda
```

If the provider you ask for cannot start (missing driver or libraries), the
server logs a warning in `%LOCALAPPDATA%\mokuro-bunko\logs\server.log` and
runs OCR on the CPU.

## 4. First OCR run: models

The OCR models (a few hundred MB for hayai-nova, about 2 GB more for
paddle-manga if you enable it) are downloaded the first time a volume is
OCR'd, into `models\` under your data folder, and verified. To fetch them
up front:

```powershell
mokuro-bunko.exe models download
```

Drop a `.cbz` into `<data>\library\<Series>\` (or upload it over WebDAV or
the reader) and watch it appear on `http://127.0.0.1:8080/queue`. A
`<Volume>.mokuro` file and a `.webp` cover appear beside it when it is done.

## Using this PC as an OCR processor for another library

If the library runs on another (small) server, make this PC its processor:

```powershell
mokuro-bunko.exe processor setup
```

It asks for the library URL and a `processor` account, writes
`processor.yaml` and can start the processor at every logon (a Startup
entry). See [deployment.md](deployment.md#remote-ocr-processors).

## Troubleshooting

- `doctor` should show the build as `full` and the models as present.
- The server log says which execution provider started and why a requested
  one did not.
- CUDA build: confirm the driver is 580 or newer and the CUDA 13 and cuDNN 9
  DLLs are on the `PATH`; or use the default DirectML build.
- Windows SmartScreen may warn about the downloaded executable; the binaries
  are not Authenticode signed. Choose "More info", then "Run anyway".
- Antivirus software that quarantines `mokuro-bunko.exe` or the provider DLLs
  next to it breaks the install; add an exception for the install folder.
- Updates come from the admin panel's Updates card (signed, checksum-verified)
  or by running `install.ps1` again; your data is kept.
