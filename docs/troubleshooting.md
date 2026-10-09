# Troubleshooting

Start here when something doesn't work.

## First step: run the doctor

```bash
mokuro-bunko doctor
doctor.bat                        # Windows zip / portable edition
```

It prints PASS / WARN / FAIL lines, with a fix hint under anything wrong:

- the build (lite or full, and its target)
- config validity (and any note about a migrated 0.5 setting) and storage writability
- ONNX Runtime (full build): the CPU runtime ppocr-manga and the PP-OCR
  detector run on
- OCR backend (full build): which backend pack is in use and where, whether
  its files are complete and its host libraries present, and whether another
  pack would use this machine's GPU
- Models (full build): whether the PP-OCR and host model files are downloaded
- Compiled packages (full build, when a hayai-nova or paddle-manga
  generation is enabled): whether the compiled package each one needs on
  this machine's device is on disk, and whether anything here can run it
- free disk space (warns under 2 GB)
- whether the configured port is already in use
- volumes currently failing OCR

Exit code is 0 unless a `FAIL` is found, so scripts can gate on it. On a
processor machine (a `processor.yaml` and no library configuration) it
checks the processor instead: its `processor.yaml`, the backend pack the
processor will load, its models and its disk. `doctor --processor` does
that on a machine that has both.

## Where the logs are

| Log | Location |
|---|---|
| Server log (rotating, 2 MiB x 5) | `<storage>/logs/server.log` |
| More detail | start with `-v`, or set `MOKURO_LOG`, e.g. `MOKURO_LOG=debug` |
| Failure records (JSON) | `<storage>/.ocr-failures.json` |
| Processor state | `mokuro-bunko processor status --config processor.yaml` (on the processor) |
| Processor output | `journalctl --user -u mokuro-bunko-processor` (Linux user service), its window (Windows) |
| Docker | `docker logs <container>` |

`<storage>` defaults to `%LOCALAPPDATA%\mokuro-bunko` on Windows and
`~/.local/share/mokuro-bunko` on Linux/macOS (portable edition: `data\`
inside the folder). `mokuro-bunko config path` prints the real locations.

## "My volumes never get OCR'd / no .mokuro files appear"

1. Is this a **lite** build, or `ocr.local_processing: false`, or
   `ocr.backend: skip`? Then nothing runs here until a processor connects;
   see "The queue is held" below.
2. Open the Queue page: `http://<server>:8080/queue`. Volumes that failed
   OCR are listed under **Failed** with the reason, attempt count and (for
   admins) the error. (Failed volumes retry with exponential backoff, up to
   1 hour between attempts, so they don't hammer your GPU forever.)
3. Read `<storage>/logs/server.log` around the failure for the engine's
   full error.
4. Run `mokuro-bunko doctor`. The usual causes are no OCR backend pack (then
   hayai-nova and paddle-manga cannot run here; see below) or models that
   could not be downloaded (no internet, a full disk).
5. To force a retry immediately: fix the cause, then either replace the
   `.cbz` file (updating its timestamp resets the failure record) or
   restart the server.
6. A volume whose `.mokuro` names pages its archive does not contain gets no
   extra layers: replace the archive with a complete one.

## "The queue is held" / "No processor connected"

The server runs no OCR of its own when it is a lite build,
`ocr.local_processing` is `false` or `ocr.backend` is `skip`; it waits for a
remote processor. The queue page and the admin panel then say "No processor
connected since …". Either start a processor (`mokuro-bunko processor setup`
on a machine with a full build, see
[deployment](deployment.md#remote-ocr-processors)) or use a full build with
local processing on and restart.

## "No backend pack installed"

`doctor` warns `OCR backend: no backend pack installed in <storage>/backends
(...)`, the server log says `no libtorch backend pack in ... (install one with
'mokuro-bunko install-ocr')`, and `models download` reports `compiled
packages: FAILED: the OCR backend is not installed`. The full build does not
contain libtorch, which hayai-nova and paddle-manga run on; until a pack is
installed they are not offered on this machine (ppocr-manga still runs, and
remote processors still work). Run:

```bash
mokuro-bunko install-ocr
```

It prints the pack it chose and why (`OCR backend: cu130 (NVIDIA GeForce RTX
4090 (driver 595.58.03))`), downloads it, checks it against the signed
release manifest and fetches the models. Then restart the server: the pack
is opened once per process. `install-ocr --list` shows what it detects
without installing anything.

- **"release X has no <variant> backend pack for <target>"**: there are packs
  for Linux x86_64 (`cpu`, `cu130`, `rocm7.1`), Windows x86_64 (`cpu`,
  `cu130`) and macOS on Apple silicon (`cpu`) only.
- **"... is release X, this is mokuro-bunko Y"**: the pack must come from the
  release of the running version. A source build of an unreleased version
  needs its own pack (`cargo run -p xtask -- torch-pack`, then
  `install-ocr --from dist`).
- **A host without internet**: download the pack archive
  (`mokuro-bunko-<version>-<target>-torch-<variant>.tar.zst`, or its numbered
  parts), `release.json` with `release.json.sig`, and for `cu130` the NVIDIA
  wheels it lists, into one directory elsewhere, then
  `mokuro-bunko install-ocr --from <dir>`.
- **A processor machine**: `install-ocr` installs into the processor's own
  storage when the machine has a `processor.yaml` and no library
  configuration, or with `install-ocr --processor`. The processor also finds
  a pack in the default library storage, for example one installed before
  `processor setup` (see [deployment](deployment.md#3-the-processor-machine)).
- **Docker**: the full image installs the pack into `/data/backends` on
  start (`install-ocr --if-needed`; its output is at the top of
  `docker logs`). Nothing is installed when `MOKURO_OCR_AUTO_INSTALL=false`
  or local OCR is off (`MOKURO_OCR_BACKEND=skip`,
  `MOKURO_OCR_LOCAL_PROCESSING=false`); a failed download is retried on the
  next start, or run `docker exec <container> mokuro-bunko install-ocr`
  and restart the container.
- **"OCR backend: ... files are missing or damaged"** (a FAIL), or a pack
  that fails to load: `mokuro-bunko install-ocr --force` downloads and checks
  it again.

## The wrong pack for this GPU, or the GPU is not used

- `doctor` warns `...; the cu130 pack would use this machine's GPU` (or
  `rocm7.1`): a CPU pack (or the other vendor's) is installed. Install the
  right one with the command it prints, e.g.
  `mokuro-bunko install-ocr --variant cu130`, and restart. A GPU pack also
  runs on the CPU, so the other direction needs nothing.
- **NVIDIA driver too old**: `install-ocr` says `OCR backend: cpu (<GPU> with
  driver 550.120: too old for CUDA 13)` and `Update the NVIDIA driver to
  580.65 or newer to OCR on the GPU, then run install-ocr again.` The `cu130`
  pack needs driver 580 or newer (`nvidia-smi` shows it). With an older
  driver an installed `cu130` pack runs on the CPU only, and the server log
  has a `libtorch backend:` warning saying why.
- **GPUs older than Turing** (GTX 10xx and older): there are no compiled
  packages for them. OCR runs on the CPU; `models download` says
  `note: running on the CPU: ...`.
- **AMD**: only Linux, and only RX 6000 (gfx1030, gfx1031, gfx1032, gfx1034), RX 7000
  (gfx1100 to gfx1102) and RX 9000 (gfx1200, gfx1201). Another card gives
  `AMD gfx906 is not one of the supported ROCm GPUs (...)` and the `cpu`
  pack. On Windows an AMD or Intel GPU runs OCR on the CPU.
- **A hidden GPU**: `install-ocr` says `no GPU visible: AMD gfx1201 (hidden by
  HIP_VISIBLE_DEVICES="")`. `CUDA_VISIBLE_DEVICES`, `HIP_VISIBLE_DEVICES` or
  `ROCR_VISIBLE_DEVICES` set to an empty value or `-1` hide every GPU of that
  vendor; unset them.
- `ocr.backend` (or `serve --ocr`) limits the devices: `cuda`, `rocm` or
  `cpu` (the CPU is always allowed). `webgpu`, `directml` and `coreml` are
  accepted for old configs, but no release has those runtimes, so they leave
  only the CPU: use `auto`.
- In Docker, a GPU the container was not given does not count: NVIDIA
  needs `--gpus all` (or `--runtime=nvidia` with `NVIDIA_VISIBLE_DEVICES`)
  and `NVIDIA_DRIVER_CAPABILITIES=compute,utility` (the image sets it); AMD
  needs `--device /dev/kfd --device /dev/dri`. `install-ocr --list` names a
  GPU the host has but the container cannot use under `Hidden:`, with the
  flag that passes it in: `docker run --rm --gpus all
  ghcr.io/gnathonic/mokuro-bunko:latest install-ocr --list`. After adding
  the GPU, restart the container: it installs that GPU's pack on start.
- `mokuro-bunko --version` prints the build; `doctor`'s `OCR backend` line
  shows the pack in use and what this machine would want, and the server log
  lists the devices the pack found when it loaded (`libtorch backend <variant>
  (...) loaded in ...: ...`).

## paddle-manga is slow on the CPU

paddle-manga runs on the CPU, as in 0.5.2, but slowly: about 40 s a page on 16
threads (a Ryzen 7 5800X), where hayai-nova takes about 4 s. Its CPU package
downloads 3.6 GB of weights (fp32; the same files its GPU packages use) and
needs about 6 GB of RAM while it reads. The setup wizard, the settings page
and the admin panel's pools table say so where the CPU is picked for it.

On a machine without a GPU, use hayai-nova (the default) for the library's
primary generation. Keep a paddle-manga generation only if you want its
reading as an extra layer and can wait for it, or connect a
[processor](deployment.md#remote-ocr-processors) with a GPU to run it.

## ROCm: missing host libraries, RX 6600/6700

- `install-ocr` warns `this rocm7.1 pack needs these libraries from the
  system, which were not found: libnuma.so ...` and `doctor` warns
  `missing host libraries: ...`. The ROCm runtime needs `libnuma`, including
  the unversioned `libnuma.so`. Install it (Debian/Ubuntu:
  `sudo apt install libnuma-dev`, Arch: `sudo pacman -S numactl`;
  `install-ocr` prints the package names of whatever is missing) and restart.
- The user running the server or processor must be able to open the GPU:
  add it to the `video` and `render` groups and log in again.
- An RX 6400 to 6700-class card (gfx1031, gfx1032, gfx1034) runs the gfx1030 kernels. The
  backend sets `HSA_OVERRIDE_GFX_VERSION=10.3.0` itself when the variable is
  unset (`doctor`'s `Compiled packages` line says
  `HSA_OVERRIDE_GFX_VERSION=10.3.0 set`); nothing to configure. A different
  value you set yourself is kept, and `install-ocr` warns about it: unset it
  or set `10.3.0`.

## `doctor`: "Compiled packages" FAIL

The recognizers load **compiled packages**, one per engine, precision and
device type (e.g. `linux-cuda-sm_89`, `linux-rocm-gfx1201`,
`linux-cpu-x86_64-v3`), downloaded into `<storage>/models/torch/`.

- **`NOT DOWNLOADED: hayai-nova bf16 on gpu:0 (...)`**: the package an enabled
  generation needs is not on disk yet. OCR would download it on first use;
  `mokuro-bunko models download` fetches it now. A package counts as on disk
  only with the weights it binds: `weights-vision.safetensors` and
  `weights-decoder.safetensors` in its engine/precision folder, shared by every
  package (GPU and CPU) of that engine and precision. If only those are named,
  they were deleted or never fetched; `models download` gets them.
- **`NOT RUNNABLE HERE: ...`**: no device here can run that row, e.g. a forced
  precision the hardware lacks, or an x86_64 CPU without AVX2 and FMA (the CPU
  packages are built for x86-64-v3). If no pack is installed, run
  `mokuro-bunko install-ocr`; otherwise use another precision for the row, or
  a processor on other hardware.

## Models do not download

The first OCR run (or `mokuro-bunko install-ocr`, or
`mokuro-bunko models download`) fetches the PP-OCR ONNX files, the
recognizers' host files and the compiled packages for this machine's device
into `<storage>/models/`, verifies each file's sha256 and unpacks the
packages there. `models download` lists each file as `ok` or `FAILED: ...` and
exits non-zero when anything failed. If it fails:

- check the machine can reach the internet (GitHub release assets and, for
  upstream files, Hugging Face), and that the disk has room (hayai-nova about
  0.6 GB in fp32 and 0.3 GB in bf16/fp16; paddle-manga about 3.6 GB in fp32,
  on the CPU too, and 1.8 GB in bf16/fp16);
- **"no compiled <engine> <precision> package for this device (looked for
  ...)"**: the release has no package for this device type; see
  ["Compiled packages" FAIL](#doctor-compiled-packages-fail) above;
- on a host without internet, download the files elsewhere and point
  `MOKURO_MODELS_DIR` at the directory, or copy them into `<storage>/models/`;
  `MOKURO_TORCH_MODELS_MIRROR` names a directory (or a base URL) holding the
  compiled packages' release assets under their release names, and
  `MOKURO_MODELS_MIRROR` does the same for the `models-v1` files. Simplest: put
  the backend pack archive and all those files in one folder and run
  `mokuro-bunko install-ocr --from <folder>`, which uses the folder for both
  (the wizard's "Install from a folder" does the same);
- `mokuro-bunko models verify` re-checks every file; delete a damaged one
  (`MISMATCH`) and download again;
- `MOKURO_MODELS_DOWNLOAD=0` forbids downloading (unset it); `doctor` then
  says `downloads are off (MOKURO_MODELS_DOWNLOAD)`.

## The pack stopped loading after an update

A backend pack belongs to exactly one release: release X's program opens only
release X's pack. After a manual copy of a new executable (or an old pack pinned
with `MOKURO_TORCH_PACK`) the log, `doctor` and `/control/status` say
`the backend pack is from mokuro-bunko A, this is B: each release runs only its
own pack`. Run `mokuro-bunko install-ocr` (it installs this release's pack and
replaces the old one) and restart. The admin panel's **Update** button,
`update apply` and automatic updates install the program and its pack together,
so this does not happen with them; an automatic update whose new pack does not
load rolls back to the previous release and says so (see
[Automatic updates](configuration.md#automatic-updates)). The backend's
interface version is checked second (`... implements backend ABI N, this
mokuro-bunko needs ABI M`). A pack built outside a release (no `bunko_version`
in its `pack.json`) is accepted by any build, for development.
Other load errors:

- **`libtorch libraries from outside the pack were loaded (...)`**: another
  libtorch on `LD_LIBRARY_PATH` or `LD_PRELOAD` was picked up; remove it from
  the server's environment.
- **`<library> is missing: neither the pack nor this system provides it`**:
  reinstall the pack (`install-ocr --force`); `doctor` lists missing host
  libraries.
- **`built for a newer system: needs GLIBC_2.xx`**: the full build and the
  Linux packs need glibc 2.28 or newer (Debian 11+, Ubuntu 20.04+, RHEL 8+).

## A processor connects but never does any work

Almost always a proxy between the processor and the library that does not
pass the WebSocket upgrade for `/_processor/<id>/socket`, or buffers or
limits the result uploads. The processor registers (a plain `POST`), but the
socket never opens or keeps dropping. Add the `Upgrade` / `Connection`
headers and long timeouts for `/_processor/` (nginx), turn off request
buffering and the body size limit there; Caddy needs nothing for the
socket. See
[Remote OCR processors behind a proxy](deployment.md#remote-ocr-processors-behind-a-proxy),
or point the processor at an address that reaches the library directly.
An old copy of `deploy/nginx.conf.example` from 0.5 has a `/_processor/`
block that breaks WebSockets.

Also check:

- `MOKURO_NGINX_ACCEL=1` is set only where nginx really serves downloads.
  Without it every archive download comes back empty, and the processor
  gives the volumes back.
- The processor can run what the library asks for: a processor is only
  offered rows it can run (a forced `fp16`, say, is not given to a CPU-only
  machine). Without a backend pack in its own storage it can run no
  hayai-nova or paddle-manga row at all; `processor setup` shows
  `engines: ...` for the machine, and see
  ["No backend pack installed"](#no-backend-pack-installed).
- The admin panel's Processors card shows each processor's state, its last
  refused login and how its downloads have gone.

## A processor is refused at registration

- **"this server speaks protocol 3, not 2"** (or "this library speaks
  protocol [3]" on the processor): the processor and the library run
  different releases, typically a 0.5.2 processor against a 0.7 library.
  Update both to 0.7 (see
  [Updating a library and its processors](deployment.md#updating-a-library-and-its-processors)).
- **Login refused**: the account does not exist, has the wrong password, is
  disabled, or does not have the `processor` role. The Processors card lists
  refused logins with the username tried, and the processor exits with a
  non-zero status.
- **"another processor is running on …"**: two processors share one
  `processor.storage`; give each its own.
- **The lite build cannot run a processor**: `processor` needs a full build.

## `processor status` says "never connected"

In the first seconds after the processor starts this is normal: the status
file catches up a few seconds after the connection. Ask again a little
later. If it stays, the processor's own output (`journalctl --user -u
mokuro-bunko-processor`, or its window on Windows) says why.

## On the library, a processor delivers bad results

Results the library refused are in the admin panel's audit log as
`ocr_sidecar_rejected` with the processor's account as the actor, and every
sidecar on disk records which machine wrote it (see
[configuration](configuration.md#audit-log) and
[OCR internals](ocr-internals.md#who-wrote-each-sidecar)). A machine that
corrupts its downloads in memory (unstable RAM, an overclock or XMP profile,
overheating) can corrupt the OCR it sends too: run a memory test before
trusting it as a processor.

## Which machine gets which volume?

With several machines, each volume goes to the machine predicted to finish it
first, so a slower machine may leave a volume for a faster one (for at most
15 seconds past when that machine was expected to take it). The Processors
card and the queue page's detailed level show each machine's speeds; see
[OCR internals](ocr-internals.md#earliest-finish-scheduling).

## Updates

- **No Update button, only a notice.** The install is not one the server may
  replace: Docker (pull the new image), a root-owned system install (re-run
  `install.sh`) or a distro package. A copy installed by
  `install.sh` as a user, the Windows zip or `install.ps1` has the button.
- **"signature does not verify" / checksum errors.** The download was
  tampered with or truncated, or `update.manifest_url` points at a mirror
  signed with another key. Nothing is installed; try again, and check the URL.
- **Checks do nothing.** `update.check` may be `false`; **Check now** still
  works. The server needs outbound HTTPS to GitHub.
- On Windows `run.bat` restarts the server after an update; if you started
  `mokuro-bunko.exe` by hand, start it again if it does not come back.
- **Automatic update "waiting" for a long time.** It installs only when no OCR
  volume is in flight anywhere (on this server and on remote processors) and no
  upload is running; new OCR work is held meanwhile. The Updates card and the
  tray say what it waits for. Turning `update.auto` off releases the hold.
- **"The update to X was rolled back".** X's OCR backend did not load on this
  machine after the switch, so the previous release and its pack came back. X is
  not tried again automatically (`<storage>/.update-blocked.json`); fix the cause
  shown (often a `MOKURO_TORCH_PACK` pinning another release's pack, a driver, a
  missing host library), then install it by hand. A newer release clears the block.
- **"The automatic update to X needs you".** Only you can fix it: the message
  says how (a newer NVIDIA driver, free disk space, a signature that does not
  verify, a Docker or package-managed install). It is tried again later anyway.
- **A processor "cannot follow its library".** The library runs an older version
  than the processor; processors never downgrade. Update the library.

## Port already in use

`doctor` reports it. Either another instance of the server is running, or
another app owns the port. Change `server.port` in the config, or stop the
other process.

## Server health at a glance

`GET /api/health` includes an `ocr` section:

```json
"ocr": {"backend": "cuda", "worker_alive": true, "pending": 0, "failed": 0}
```

`worker_alive: false` means the background OCR loop stopped heartbeating:
restart the server and check `server.log`. With `ocr.backend: skip` (or a
lite build) the section reads `{"backend": "skip", "worker_alive": null,
"pending": null, "failed": 0}`: this server runs no OCR, so there is no loop
to report on. `mokuro-bunko healthcheck` exits 0 when the local server
answers (what the Docker health check runs).

## Windows install issues

`install.ps1` prints each step and its failure; re-running it is safe and
updates in place (your data is kept). If SmartScreen blocks the executable,
choose "More info" then "Run anyway": the binaries are not Authenticode
signed. `doctor.bat` (portable zip) or `mokuro-bunko.exe doctor` checks the
installation. For NVIDIA GPUs see
[setup-windows-nvidia-ocr.md](setup-windows-nvidia-ocr.md).

## The first-run setup page will not open from another machine

The setup page opens without a token only from the server machine itself.
From another machine, or under Docker bridge networking, use the URL with the
one-time token that the server logged at startup (`/setup?token=...`, also in
`<storage>/.setup-token`), or set `MOKURO_SETUP_TOKEN`.
