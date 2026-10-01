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
- ONNX Runtime (full build): whether local OCR can start
- whether the OCR models are downloaded
- free disk space (warns under 2 GB)
- whether the configured port is already in use
- volumes currently failing OCR

Exit code is 0 unless a `FAIL` is found, so scripts can gate on it.

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
4. Run `mokuro-bunko doctor`. The usual causes are models that could not be
   downloaded (no internet, a full disk) or an execution provider that does
   not work on this machine.
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

## Models do not download

The first OCR run (or `mokuro-bunko models download`) fetches the ONNX files
into `<storage>/models/` and verifies each file's sha256. If it fails:

- check the machine can reach the internet (GitHub release assets and, for
  upstream files, Hugging Face), and that the disk has room (paddle-manga
  needs about 2 GB or more);
- on a host without internet, download the files elsewhere and point
  `MOKURO_MODELS_DIR` at the directory, or copy them into `<storage>/models/`;
- `mokuro-bunko models verify` re-checks every file; delete a damaged one
  and download again;
- `MOKURO_MODELS_DOWNLOAD=0` forbids downloading (unset it).

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
- The processor can run what the library asks for: the models are
  downloaded and the execution provider works. A processor is only offered
  rows it can run (a forced `fp16`, say, is not given to a CPU-only machine).
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

## A GPU is not being used

- Check which build you have: `mokuro-bunko --version` prints the flavor and
  target. The CUDA provider is only in `full-cuda` builds and the CUDA Docker
  image; plain `full` on Linux is CPU only (Windows `full` uses DirectML,
  macOS `full` uses CoreML).
- **NVIDIA / CUDA**: the driver must be 580 or newer (`nvidia-smi`), and the
  CUDA 13 and cuDNN 9 libraries must be installed (the Docker image has
  them). A missing library makes the server log a provider failure at start
  and fall back to the CPU. Keep the provider libraries next to the
  executable (they are part of the `full-cuda` archive).
- **Windows**: DirectML needs a DirectX 12 GPU with a current driver.
- `ocr.backend` (or `serve --ocr`) forces a provider; if the one you ask for
  is unavailable the server logs a warning and uses the CPU. Check
  `<storage>/logs/server.log`.
- In Docker, the container needs `--gpus all` / `--runtime=nvidia` and
  `NVIDIA_DRIVER_CAPABILITIES=compute,utility`.

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
  `install.sh`), a distro package, or the Android app. A copy installed by
  `install.sh` as a user, the Windows zip or `install.ps1` has the button.
- **"signature does not verify" / checksum errors.** The download was
  tampered with or truncated, or `update.manifest_url` points at a mirror
  signed with another key. Nothing is installed; try again, and check the URL.
- **Checks do nothing.** `update.check` may be `false`; **Check now** still
  works. The server needs outbound HTTPS to GitHub.
- On Windows `run.bat` restarts the server after an update; if you started
  `mokuro-bunko.exe` by hand, start it again if it does not come back.

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
