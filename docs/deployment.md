# Deployment Guide

This guide covers the ways to install and run Mokuro Bunko Server, from a
single machine on a LAN to a lite library server with remote OCR processors
behind a reverse proxy. For every setting, see the
[configuration reference](configuration.md). Coming from 0.5.2? Read
[MIGRATING-0.7.md](MIGRATING-0.7.md) first.

## Installing

mokuro-bunko is one native executable, `mokuro-bunko`, with nothing else to
install: no Python, no uv, no git, no CUDA toolkit. Releases are on the
[GitHub releases page](https://github.com/Gnathonic/mokuro-bunko/releases).
Pick a build:

| Build | Contains | Use it for |
|---|---|---|
| **lite** | The server only; OCR is done by remote processors. | A small VPS, NAS or Raspberry Pi (1 GB of RAM is enough). |
| **full** | Lite plus the OCR engines, local OCR and `processor`. | A machine that does OCR: the library host itself, or a processor. |

There is one full build per platform, whatever the GPU: the GPU support comes
from the OCR backend pack that `mokuro-bunko install-ocr` installs afterwards
(see [OCR backend](#ocr-backend-install-ocr)).

Platform support of the release archives:

- **Linux x86_64**: lite (a static binary for any distribution) and full
  (glibc 2.28 or newer: Debian 11+, Ubuntu 20.04+, RHEL 8+).
- **Linux aarch64 (arm64)**: lite only. OCR for an arm64 library comes from a
  [remote processor](#remote-ocr-processors) on an x86_64, Windows or macOS
  machine.
- **Windows x86_64**: lite and full.
- **macOS Apple silicon**: lite and full (OCR on the CPU).
- **macOS Intel**: lite only.

**GPU prerequisites.** Only the GPU driver comes from the host; the backend
pack brings libtorch and, for NVIDIA, the CUDA libraries:

| Pack | Hardware | Host needs |
|---|---|---|
| `cu130` | NVIDIA GPUs, Turing (GTX 16xx / RTX 20xx) or newer; Linux and Windows | An NVIDIA driver **580 or newer** (`nvidia-smi` shows it). No CUDA toolkit, no cuDNN. |
| `rocm7.1` | AMD Radeon RX 6000 (gfx1030; the RX 6600/6700 class, gfx1031/1032, runs the gfx1030 kernels with `HSA_OVERRIDE_GFX_VERSION=10.3.0`, which is set automatically), RX 7000 and RX 9000; Linux only | The amdgpu kernel driver (`/dev/kfd`), access to the GPU (`video` and `render` groups) and `libnuma` (Debian/Ubuntu: `libnuma-dev`, Arch: `numactl`). No ROCm install. |
| `cpu` | Everything else, including macOS on Apple silicon, AMD and Intel GPUs on Windows, and older NVIDIA drivers | Nothing. The recognizers on the CPU need an x86_64 CPU with AVX2 and FMA (x86-64-v3), or arm64. |

ppocr-manga and the PP-OCR text detector run on ONNX Runtime on the CPU,
which is linked into the full build: nothing to install for them. The OCR
models are downloaded by mokuro-bunko itself (see below).

### Linux and macOS: `install.sh`

```bash
curl -fsSL https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.sh | sh
curl -fsSL https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.sh | sh -s -- --flavor lite --systemd
```

The script picks the archive for your OS, CPU and flavor (`--flavor
lite|full`, default: full where it exists, else lite), checks the
signature of the release manifest (needs OpenSSL 3; `--require-signature`
fails without it) and the archive's sha256, and installs into
`~/.local/lib/mokuro-bunko` with a link in `~/.local/bin` (as root:
`/usr/local/lib/mokuro-bunko` and `/usr/local/bin`; `--prefix DIR` changes
it). A full binary built for a newer glibc than yours is caught before
anything is replaced. `--systemd` installs and starts a service (see
[Systemd](#systemd-service)), `--processor` installs the OCR processor unit
(needs a full flavor), `--version X.Y.Z` pins a release, `--dry-run` shows
what would happen. Run it again to update. On a full build, run
`mokuro-bunko install-ocr` once afterwards (see
[OCR backend](#ocr-backend-install-ocr)).

Without the script: unpack `mokuro-bunko-<version>-<target>-<flavor>.tar.gz`
and run `./mokuro-bunko serve`. On macOS a tarball downloaded in a browser is
quarantined: `xattr -d com.apple.quarantine mokuro-bunko` (the script is not
affected).

### Windows: `install.ps1` or the portable zip

```powershell
powershell -c "irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.ps1 | iex"
```

Installs into `%LOCALAPPDATA%\mokuro-bunko\app` (no admin rights, nothing in
the registry), adds Start-menu shortcuts, runs `mokuro-bunko doctor` and
starts the server. Your data stays in `%LOCALAPPDATA%\mokuro-bunko`. Options
are `-Flavor full|lite`, `-Version`, `-InstallDir`, `-Portable`,
`-Startup` (start at logon), `-NoShortcut`, `-NoStart`; see the top of
[`scripts/install.ps1`](../scripts/install.ps1). It verifies the archive's
sha256 (PowerShell cannot verify the ed25519 signature; the in-app updater
does). Windows SmartScreen may warn: the executable is not Authenticode
signed. Then run `mokuro-bunko.exe install-ocr` once from the install folder
(the `cu130` pack on an NVIDIA GPU, else the CPU pack).

The **portable zip** (`mokuro-bunko-<version>-x86_64-pc-windows-msvc-<flavor>.zip`)
needs no installer: extract it anywhere and run `run.bat`. While `PORTABLE.txt`
is present, config, library, logs and models live in `data\` next to it and
nothing is written to AppData. `doctor.bat` diagnoses problems. For NVIDIA
GPUs see [setup-windows-nvidia-ocr.md](setup-windows-nvidia-ocr.md).

### Docker

See [Docker](#docker) below.

### From source

```bash
git clone https://github.com/Gnathonic/mokuro-bunko.git
cd mokuro-bunko
cargo build --release -p mokuro-bunko                           # full
cargo build --release -p mokuro-bunko --no-default-features     # lite
cargo run -p xtask -- torch-pack --variant cpu --out dist       # an OCR backend pack (cpu|cu130|rocm7.1)
./target/release/mokuro-bunko install-ocr --from dist           # install it
./target/release/mokuro-bunko serve
```

### OCR backend (`install-ocr`)

The OCR recognizers (hayai-nova, paddle-manga) run on libtorch, which a full
build does not contain: `mokuro-bunko install-ocr` downloads it once as a
*backend pack* for the hardware it finds, `cu130`, `rocm7.1` or `cpu` (see
the table above), into `<storage>/backends/`, then fetches the models.

```bash
mokuro-bunko install-ocr                  # the pack ocr.backend asks for on this hardware, and the models
mokuro-bunko install-ocr --list           # what it detects, which pack it would install, what is installed
mokuro-bunko install-ocr --variant cpu    # pick the pack yourself (auto, cpu, cu130, rocm7.1)
```

The pack comes from the release of the same version, checked against the
signed release manifest; the `cu130` pack's CUDA libraries (about 1.6 GB) are
fetched from NVIDIA's own packages on PyPI, each pinned by sha256. Sizes on
disk: `cpu` 0.4 GB, `cu130` 2.5 GB, `rocm7.1` 6.8 GB. `--from DIR` installs
from files downloaded elsewhere (the pack archive, optionally `release.json`
and its `.sig`, and the NVIDIA wheels) on a host without internet,
`--no-models` skips the models, `--force` reinstalls. Without `--variant` it
follows `ocr.backend`: `auto` takes the GPU it finds, `cpu` stays on the CPU
even with a GPU, `cuda`/`rocm` take that GPU (or `cpu`, with a hint, when it is
not there); `--variant auto` looks at the hardware only. `install-ocr` is a
foreground command for scripts; `MOKURO_BACKENDS_DIR` moves the packs elsewhere.

**`serve` and `processor serve` install it themselves, in the background**, on
any system (the full Docker image carries no pack and relies on this): when
local OCR is on, an enabled generation needs the backend (hayai-nova,
paddle-manga; a processor: always) and no fitting pack of this release is
installed, they start serving at once and run the same detection and install
(`install-ocr --if-needed`, as a child process) meanwhile:

- the progress (stage, percent, bytes, the pack) is in the log, the admin
  panel (Settings → OCR → Processors), the dashboard and the tray, and in
  `/control/status` (`install`);
- when it is done the server's local OCR starts by itself (no restart); a
  processor registers at once but as not available (reason `installing`; the
  library's processor list shows "installing OCR backend: NN%") and takes work
  once its backend is ready;
- a failed install is a problem ("needs you") with a Retry button in the
  dashboard and the admin panel; one that may pass by itself (the network) is
  retried after 5, 15 and 60 minutes, and every start tries again;
- one install at a time per backends directory (`.install.lock`); an install
  cut short resumes at the next start (downloads continue from their `.part`
  files and are checked by sha256);
- `MOKURO_OCR_AUTO_INSTALL=false`: never by itself; the missing backend is a
  problem with an Install button instead.

Until it is installed hayai-nova and paddle-manga are not offered on the
machine (ppocr-manga still is, and remote processors still work).

### OCR models

A full build downloads what the configured OCR generations need the first
time they run, into `<storage>/models/`, checking every file's sha256: the
PP-OCR ONNX files, hayai-nova's and paddle-manga's host files (tokenizer,
embeddings) and the **compiled libtorch packages** for this machine's device,
which are unpacked there once and loaded in place. `install-ocr` fetches them
up front; so does `models download`, for example before moving to a network
without internet access:

```bash
mokuro-bunko models download                    # what the enabled generations need here
mokuro-bunko models download --engine hayai-nova
mokuro-bunko models list                        # what exists and what is on disk
mokuro-bunko models verify
```

Sizes: hayai-nova about 0.5 GB per precision, paddle-manga about 2 GB per
precision, ppocr-manga about 23 MB. `MOKURO_MODELS_DIR` points at a directory
of model files for air-gapped hosts, and `MOKURO_MODELS_DOWNLOAD=0` forbids
downloads (see [configuration](configuration.md#ocr-models)).

### Updating

A copy installed by `install.sh` as a user, the Windows zip or `install.ps1`
updates itself: the admin panel's **Updates** card (Status tab) shows new
releases and its **Update and restart** button downloads, verifies and
installs them. From a terminal: `mokuro-bunko update check` and
`mokuro-bunko update apply`. Root-installed system units, distro packages and
Docker only get a notice ("re-run `install.sh`", "pull the new image"). Your
data is never touched by an update. Set `update.check: false` to stop the
background check. See [configuration](configuration.md#updates).

An update replaces the executable only; the installed backend pack stays. If
`doctor` or the server log then says the pack implements another backend ABI
("install the pack of this release"), run `mokuro-bunko install-ocr --force`.

## First start

The first browser visit to a new server opens a setup page that creates the
admin account. From the machine itself that just works. From another machine
(a server in Docker, on a NAS, or reached over SSH) the page first asks for a
one-time **setup code**: while no admin exists the server makes a new code at
every start and prints it in its log:

```
First run: create the admin account at http://<this server's address>:8080/setup (setup code: 7KQ2M-X9D4R)
```

The code is kept in memory only, works until an admin exists, and allows five
tries a minute per address (30 in all). `mokuro-bunko setup` does the same in
the console, and `mokuro-bunko admin add-user NAME --role admin` adds an admin
directly.

Docker (or any unattended start) can skip the page: `MOKURO_ADMIN_USERNAME` and
`MOKURO_ADMIN_PASSWORD` (or `MOKURO_ADMIN_PASSWORD_FILE`, e.g. a Docker secret)
create the admin on the first start. Once an admin exists they are ignored (one
log line), so changing them later changes nothing: change passwords in the admin
panel.

## Deployment Scenarios

### Local Network (LAN)

For home or office use where the server is accessible only on your local network:

1. **Run the server**:
   ```bash
   mokuro-bunko serve --host 0.0.0.0 --port 8080
   ```

2. **Find your local IP**:
   ```bash
   # Linux/macOS
   ip addr show | grep "inet "
   # Windows
   ipconfig
   ```

3. **Connect from other devices** using `http://YOUR_LOCAL_IP:8080`

### Behind Nginx (Reverse Proxy)

For production deployments with SSL termination:

1. **Create Nginx configuration** (`/etc/nginx/sites-available/mokuro`):
   ```nginx
   # WebSocket upgrade for the OCR processors' socket. Plain requests keep
   # an empty Connection header so upstream keep-alive still works.
   map $http_upgrade $connection_upgrade {
       default upgrade;
       ""      "";
   }

   server {
       listen 443 ssl http2;
       server_name mokuro.example.com;

       ssl_certificate /etc/letsencrypt/live/mokuro.example.com/fullchain.pem;
       ssl_certificate_key /etc/letsencrypt/live/mokuro.example.com/privkey.pem;

       # Increase timeouts for WebDAV
       proxy_connect_timeout 300;
       proxy_send_timeout 300;
       proxy_read_timeout 300;

       # Allow large file uploads
       client_max_body_size 0;

       location / {
           proxy_pass http://127.0.0.1:8080;
           proxy_set_header Host $host;
           proxy_set_header X-Real-IP $remote_addr;
           proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
           proxy_set_header X-Forwarded-Proto $scheme;

           # WebDAV methods
           proxy_http_version 1.1;
           proxy_set_header Connection "";
       }

       # Remote OCR processors: see "Remote OCR processors behind a proxy".
       location /_processor/ {
           proxy_pass http://127.0.0.1:8080;
           proxy_set_header Host $host;
           proxy_set_header X-Real-IP $remote_addr;
           proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
           proxy_set_header X-Forwarded-Proto $scheme;
           proxy_http_version 1.1;
           proxy_set_header Upgrade $http_upgrade;
           proxy_set_header Connection $connection_upgrade;

           client_max_body_size 0;
           proxy_request_buffering off;
           proxy_buffering off;
           proxy_connect_timeout 10s;
           proxy_send_timeout 3600s;
           proxy_read_timeout 3600s;
       }
   }

   server {
       listen 80;
       server_name mokuro.example.com;
       return 301 https://$server_name$request_uri;
   }
   ```
   [`deploy/nginx.conf.example`](../deploy/nginx.conf.example) is a fuller
   example; if your copy still has the 0.5 `/_processor/` block (with
   `Connection ""` and no `Upgrade`), replace it with the one above, or
   processors will not connect.

2. **Enable the site**:
   ```bash
   ln -s /etc/nginx/sites-available/mokuro /etc/nginx/sites-enabled/
   nginx -t && systemctl reload nginx
   ```

3. **Configure mokuro-bunko** (`config.yaml`):
   ```yaml
   server:
     host: "127.0.0.1"
     port: 8080

   cors:
     allowed_origins:
       - "https://mokuro.example.com"
       - "https://reader.mokuro.app"
   ```

### Behind Caddy (Reverse Proxy)

Caddy automatically handles SSL certificates and forwards WebSockets as they
are:

1. **Create Caddyfile**:
   ```
   mokuro.example.com {
       # Remote OCR processors: no body limit, flushed as it arrives.
       @processor path /_processor/*
       handle @processor {
           reverse_proxy 127.0.0.1:8080 {
               flush_interval -1
           }
       }

       handle {
           reverse_proxy 127.0.0.1:8080
       }
   }
   ```
   A fuller example is in [`deploy/caddy.example`](../deploy/caddy.example).

2. **Run Caddy**:
   ```bash
   caddy run --config /etc/caddy/Caddyfile
   ```

### Cloudflare Tunnel

Expose your local server to the internet without port forwarding:

1. **Install cloudflared**:
   ```bash
   # Debian/Ubuntu
   curl -L --output cloudflared.deb https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64.deb
   sudo dpkg -i cloudflared.deb
   ```

2. **Authenticate**:
   ```bash
   cloudflared tunnel login
   ```

3. **Create tunnel**:
   ```bash
   cloudflared tunnel create mokuro
   ```

4. **Configure tunnel** (`~/.cloudflared/config.yml`):
   ```yaml
   tunnel: YOUR_TUNNEL_ID
   credentials-file: ~/.cloudflared/YOUR_TUNNEL_ID.json

   ingress:
     - hostname: mokuro.example.com
       service: http://localhost:8080
     - service: http_status:404
   ```

5. **Create DNS record**:
   ```bash
   cloudflared tunnel route dns mokuro mokuro.example.com
   ```

6. **Run tunnel**:
   ```bash
   cloudflared tunnel run mokuro
   ```

`mokuro-bunko tunnel cloudflare` starts a temporary quick tunnel instead, for
testing. A tunnel or CDN in front of the library also carries the processors'
sidecar uploads, which can be tens of MB, and a long-lived WebSocket; if it
limits request bodies below that, point processors at an address that reaches
the server directly (see below).

## Remote OCR processors

OCR can run on a different machine from the library: typically a small,
always-on lite server and a stronger GPU computer that is on when it is used.
This is how a 1 GB VPS gets OCR. The processor dials out to the library, so
nothing needs to be opened on it. What the settings mean is in
[the configuration reference](configuration.md#remote-ocr-processors) and how
it works in [OCR internals](ocr-internals.md#remote-processors); this is the
setup, in four steps: an account on the library, a network path to it, the
processor machine, and keeping the processor running. The library and its
processors must both be 0.7.

### 1. The library server

Create an account with the `processor` role for each processor (the role is
only ever given by an admin, never by an invite):

```bash
mokuro-bunko admin add-user gpu-box --role processor
```

It asks for the password twice, without showing it. Leave `--password` off
the command line: there it would end up in the shell's history. You type the
password once more, into the processor's setup, so make it long and random
and copy it from the output of:

```bash
openssl rand -base64 24
```

```powershell
$b = New-Object byte[] 24; [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($b); [Convert]::ToBase64String($b)
```

The admin commands work on the library's own database, which they find
through the library's configuration file. If the library runs with
`--config /path/to/config.yaml` or `MOKURO_CONFIG`, give the admin commands
the same (the option goes before `admin`):

```bash
mokuro-bunko --config /path/to/config.yaml admin add-user gpu-box --role processor
```

A name that was deleted before cannot be added again; bring the account back
with a new password instead (asked for, like `add-user`):

```bash
mokuro-bunko admin restore-user gpu-box --role processor
```

Decide whether the library's own hardware does OCR too. A lite build never
does. On a full build that should leave OCR to processors:

```yaml
ocr:
  local_processing: false
```

(or `MOKURO_OCR_LOCAL_PROCESSING=false`, or `ocr.backend: skip`). Restart the
library after changing it. The queue then holds (the queue page and the admin
panel say "No processor connected") until a processor logs in.

The generations to run are configured on the library, as usual; processors
run whatever the library sends them.

### 2. A path from the processor to the library

The processor needs to reach the library's URL, and every proxy in between
must pass `/_processor/` through, WebSocket upgrade included; see
[Remote OCR processors behind a proxy](#remote-ocr-processors-behind-a-proxy).
If the public path cannot (a tunnel or CDN with a body limit), give the
processor a direct address instead: the library's port on the LAN (bind
`server.host` to the LAN interface, or forward a port to it), with TLS
verification off, or the certificate's path, if that address uses a
self-signed certificate (`processor setup --tls-verify false` or
`--tls-verify /path/to/cert.pem`). A connected processor holds one WebSocket;
the server is async, so several processors do not need any tuning.

### 3. The processor machine

A processor is the **same `mokuro-bunko` executable**, full build, of the
same release as the library. There is no Python, git or checkout to install.
Install it as in [Installing](#installing) (`install.sh --flavor full`,
`install.ps1`, a Docker image), install the OCR backend pack, then one
command, `processor setup`, does the rest. From the host the processor needs
the GPU driver only (see the [GPU prerequisites](#installing)).

A processor keeps its pack and models in its own storage, `processor.storage`
(`~/.local/share/mokuro-bunko-processor` on Linux and macOS,
`%LOCALAPPDATA%\mokuro-bunko-processor` on Windows). `install-ocr` puts them
there by itself on a machine that has a `processor.yaml` and no library
configuration; `install-ocr --processor` asks for it explicitly. A pack
installed before `processor setup` lands in the default library storage
instead, and the processor finds it there too. Look for packs in this order:

1. `MOKURO_BACKENDS_DIR`;
2. its own storage;
3. the library storage.

So either order works:

```bash
mokuro-bunko install-ocr        # or, after processor setup: install-ocr --processor
```

(The full Docker image installs it on start and needs no step.) The desktop
app's processor setup offers the same install, with progress. Then:

```bash
mokuro-bunko processor setup
```

It asks for the library's URL, the processor account and its password. In
order, it:

1. logs in to the library and checks that the account has the `processor`
   role and that the library speaks this release's processor protocol,
   before it writes anything and without registering the machine;
2. shows this machine's CPU and GPU and the engines it can run (from its
   backend pack and models);
3. writes `processor.yaml` with only the settings that differ from the
   defaults, readable by you only (mode 600);
4. offers to run the processor as a service (see
   [step 4](#4-keeping-the-processor-running)).

Every answer can also be given as an option, for a script: `--url`,
`--username`, `--password-stdin` (the password from the first line of
standard input; there is deliberately no `--password`), `--name` (how the
library shows this machine; default the hostname), `--tls-verify`
(`true`, `false`, or a certificate's path), `--config` (where to write the
file, default `processor.yaml`), `--yes` (accept every default),
`--no-service`, and `--force` (overwrite an existing file).
`mokuro-bunko processor setup --help` lists them. `--backend` is accepted for
0.5 scripts and changes nothing: the processor uses what its pack and
hardware can run, and `MOKURO_OCR_BACKEND` (`cuda`, `rocm`, `cpu`) in its
environment narrows that.

The OCR models for the library's generations are downloaded into the
processor's storage the first time they run (or up front by `install-ocr` as
above). Within a few
seconds of starting, the processor appears in the library's admin panel (the
OCR section's **Processors** card) and starts taking volumes; with
`ocr.autobench` on, each generation is first benchmarked on it once.
`mokuro-bunko processor status --config processor.yaml` prints what it last
did.

#### Linux

Install the full build (`install.sh --flavor full`; x86_64 only, arm64 Linux
has no full build and cannot be a processor), install the pack into the
processor's storage as above and run `mokuro-bunko processor setup`. NVIDIA
gets the `cu130` pack (driver 580 or newer, nothing else). A supported AMD
GPU gets the `rocm7.1` pack; it needs `libnuma` from the system
(`install-ocr` prints the package names of any host library it misses) and
the user running the processor must be in the `video` and `render` groups.
Anything else runs on the CPU.

#### Windows

Install with `install.ps1` (the `full` flavor), install the pack into the
processor's storage as above (the `cu130` pack on an NVIDIA GPU; AMD and Intel
GPUs run OCR on the CPU) and run
`mokuro-bunko.exe processor setup` from a terminal in the install folder (or
the Start-menu shortcut's folder). The questions are the same.
`processor.yaml` is restricted to your account, and the last question is
"Start the processor now, and at every logon (a Startup entry)?". The
processor keeps its storage in `%LOCALAPPDATA%\mokuro-bunko-processor`.

#### macOS

Use the `aarch64-apple-darwin` full build (Apple silicon; OCR on the CPU with
the `cpu` pack), install the pack as above and run
`mokuro-bunko processor setup`. `processor service` installs a launchd agent.
Intel Macs have no full build.

#### Docker

[`deploy/docker-compose.processor.yml`](../deploy/docker-compose.processor.yml)
runs the full image as a processor: put `processor.yaml` in `./processor/`
and start it with `docker compose -f deploy/docker-compose.processor.yml up -d`.
Before `processor serve` the entrypoint installs the backend pack for the GPU
the container is given (`gpus: all` for NVIDIA, with the container toolkit and
a driver 580 or newer; `devices: [/dev/kfd, /dev/dri]` for AMD; neither: the
CPU) into the volume, with the models; later starts reuse them.
`MOKURO_OCR_BACKEND` (`cpu`, `cuda`, `rocm`) overrides the choice. The password
can come from the environment instead of the file: `MOKURO_PROCESSOR_PASSWORD`.

#### By hand

`processor setup` only writes a file you can write yourself. Copy the
documented example, set the three `library` values, keep the file private,
then start:

```bash
cp docs/processor.example.yaml processor.yaml     # Windows: copy docs\processor.example.yaml processor.yaml
chmod 600 processor.yaml
mokuro-bunko processor serve --config processor.yaml
```

Every key is described in [`processor.example.yaml`](processor.example.yaml)
and in [the configuration reference](configuration.md#remote-ocr-processors).

### 4. Keeping the processor running

`processor setup` offers this as its last step; to do it later, or again
after moving the executable or `processor.yaml`:

```bash
mokuro-bunko processor service --config processor.yaml --install
```

Stopping the processor, however it runs, gives the volumes it held back to
the queue, unrecorded (SIGTERM and Ctrl+C both stop it cleanly).

#### Linux: a systemd user service

`processor service --install` writes
`~/.config/systemd/user/mokuro-bunko-processor.service` from the running
binary's own path and the absolute path of `processor.yaml`, then enables and
starts it. Without `--install` it prints the unit instead.

A user service stops when you log out and does not start at boot unless
your account lingers; the command says so when it does not. To keep the
processor running:

```bash
loginctl enable-linger "$USER"
```

To follow, stop or remove it:

```bash
journalctl --user -u mokuro-bunko-processor -f
systemctl --user stop mokuro-bunko-processor
systemctl --user disable --now mokuro-bunko-processor
rm ~/.config/systemd/user/mokuro-bunko-processor.service
systemctl --user daemon-reload
```

#### Windows: a Startup entry

`processor service --install` writes `mokuro-bunko-processor.cmd` into your
Startup folder (`%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup`;
<kbd>Win</kbd>+<kbd>R</kbd>, `shell:startup` opens it) and starts the
processor now. No administrator is needed. At every logon the processor
starts in its own minimized window, so it runs while you are logged in.

- **Its output** is in that window; `mokuro-bunko processor status
  --config processor.yaml` prints what it last did.
- **To stop it**, press <kbd>Ctrl</kbd>+<kbd>C</kbd> in its window, or close
  the window.
- **To stop starting it at logon**, delete `mokuro-bunko-processor.cmd` from
  the Startup folder.

#### macOS: a launchd agent

`processor service --install` writes
`~/Library/LaunchAgents/io.github.gnathonic.mokuro-bunko-processor.plist` and
loads it into your login session.

#### A system service under a dedicated user

On a headless Linux box the processor can run as a system service under an
account of its own, starting at boot with nobody logged in.
`install.sh --processor` (as root) installs the unit;
[`deploy/mokuro-bunko-processor.service`](../deploy/mokuro-bunko-processor.service)
is the same unit. It runs as the user `mokuro`, which must be able to reach
the GPU (the `video` group, and `render` for some AMD setups), and its
`ExecStart` is
`/usr/local/bin/mokuro-bunko processor serve --config /etc/mokuro-bunko/processor.yaml`.

Install the backend pack as that user, into the storage its `processor.yaml`
names (`--processor` reads that file):

```bash
sudo useradd -r -m -d /var/lib/mokuro-bunko -s /usr/sbin/nologin mokuro   # if it does not exist
sudo usermod -aG video,render mokuro
sudo install -D -m 600 -o mokuro processor.yaml /etc/mokuro-bunko/processor.yaml
sudo -u mokuro env HOME=/var/lib/mokuro-bunko \
  MOKURO_PROCESSOR_CONFIG=/etc/mokuro-bunko/processor.yaml \
  /usr/local/bin/mokuro-bunko install-ocr --processor
sudo systemctl enable --now mokuro-bunko-processor
journalctl -u mokuro-bunko-processor -f
```

### Remote OCR processors behind a proxy

A processor talks to the library over two channels, both opened by the
processor:

- **the socket**: `GET /_processor/<id>/socket`, one WebSocket for as long as
  the processor is connected. It carries every operation and every event of
  every session; WebSocket ping/pong keeps it alive. A proxy that does not
  forward the `Upgrade` handshake makes the processor register and then never
  receive work;
- **result uploads**: `PUT /_processor/<id>/results/<sid>/<claim>`, one plain
  request per finished volume, streaming the sidecar (up to tens of MB);
- **archive reads**: ordinary `GET`s of the library's `.cbz` files, each
  archive whole, one request per volume. A broken download is resumed with
  `Range` + `If-Range`, so a proxy must pass `Range` and the `ETag` through
  (no streaming settings needed). The repo's nginx template and Caddy both
  do.

| Requirement | nginx | Why |
|---|---|---|
| Pass the WebSocket upgrade | `proxy_set_header Upgrade $http_upgrade;` and `proxy_set_header Connection $connection_upgrade;` (with the `map` shown above) | Without them nginx answers the upgrade as a plain request and the socket never opens. |
| HTTP/1.1 to the backend | `proxy_http_version 1.1` | WebSockets and chunked bodies are HTTP/1.1. |
| Long timeouts | `proxy_read_timeout` / `proxy_send_timeout` of `3600s` | The socket is idle between operations; nginx's default of 60 s would close it. (Pings keep a healthy socket busy, but be generous.) |
| No request body size limit | `client_max_body_size 0` | A sidecar upload may be larger than your general limit. |
| Do not buffer request bodies | `proxy_request_buffering off` | Uploads should stream to the server rather than be spooled by nginx first. |
| Do not buffer responses | `proxy_buffering off` | Keeps downloads and the socket moving. |

Scope these to `location /_processor/` (as in the nginx example above and in
`deploy/nginx-internal.conf.template`) so the upload limit still applies
everywhere else. With **Caddy**, WebSockets work without configuration; give
`/_processor/*` its own `handle` with no `request_body` limit and
`flush_interval -1` ([`deploy/caddy.example`](../deploy/caddy.example)).
**Cloudflare** and most CDNs forward WebSockets, but cap request bodies on
some plans. The Docker images' own nginx already carries these settings.

Set `MOKURO_NGINX_ACCEL=1` only when nginx really is in front (the full
image sets it up itself when you ask for it): without that nginx,
every download is an empty answer, and processors give such volumes back.

### Updating a library and its processors

The library and its processors must run the same protocol version (v3 in
0.7); a processor from another release is refused at registration ("this
server speaks protocol 3, not 2"). Update them together:

1. **Stop** every processor (`systemctl --user stop mokuro-bunko-processor`,
   `sudo systemctl stop mokuro-bunko-processor`, or <kbd>Ctrl</kbd>+<kbd>C</kbd>
   in its window). Their volumes go back to the queue unrecorded.
2. **Update** the library (admin panel, `mokuro-bunko update apply`,
   `install.sh`, or the new image) and each processor the same way
   (`mokuro-bunko update apply` works on a processor machine).
3. **Restart** the library if it did not restart itself.
4. **Start** the processors.

Between two 0.7.x releases that keep protocol v3 a processor reconnects by
itself when the library restarts. Each processor needs a `storage` of its own;
a second `processor serve` on the same storage refuses to start.

## Systemd Service

For running mokuro-bunko as a system service:
`install.sh --systemd` as root writes
[`deploy/mokuro-bunko.service`](../deploy/mokuro-bunko.service) for you;
as a user it installs a user unit under `~/.config/systemd/user/`. By hand:

1. **Create the user and directories**:
   ```bash
   sudo useradd -r -s /usr/sbin/nologin -d /var/lib/mokuro-bunko mokuro
   sudo install -d -o mokuro -g mokuro /var/lib/mokuro-bunko
   ```

2. **Create the service file** (`/etc/systemd/system/mokuro-bunko.service`):
   ```ini
   [Unit]
   Description=Mokuro Bunko Server
   Wants=network-online.target
   After=network-online.target

   [Service]
   Type=simple
   User=mokuro
   Group=mokuro
   WorkingDirectory=/var/lib/mokuro-bunko
   Environment=MOKURO_STORAGE=/var/lib/mokuro-bunko/storage
   Environment=MOKURO_CONFIG=/var/lib/mokuro-bunko/config.yaml
   # The binary is root-owned: the admin panel reports new releases and
   # re-running install.sh applies them.
   Environment=MOKURO_INSTALL_KIND=install.sh
   ExecStart=/usr/local/bin/mokuro-bunko serve
   Restart=on-failure
   RestartSec=5s
   TimeoutStopSec=30s

   # Security hardening
   NoNewPrivileges=yes
   PrivateTmp=yes
   ProtectSystem=strict
   ProtectHome=yes
   ReadWritePaths=/var/lib/mokuro-bunko
   LimitNOFILE=65536

   [Install]
   WantedBy=multi-user.target
   ```
   With `ProtectHome=yes` nothing can be written under a home directory,
   which is why the config and storage live under `/var/lib/mokuro-bunko`.
   A full build keeps its backend pack in `<storage>/backends` and its models
   in `<storage>/models`, so no extra setting is needed.

3. **Install the OCR backend** (full build), as the service user and with
   the service's storage and config:
   ```bash
   sudo -u mokuro env MOKURO_STORAGE=/var/lib/mokuro-bunko/storage \
     MOKURO_CONFIG=/var/lib/mokuro-bunko/config.yaml \
     /usr/local/bin/mokuro-bunko install-ocr
   ```
   For an AMD GPU also add the user to the GPU groups:
   `sudo usermod -aG video,render mokuro`.

4. **Enable and start**:
   ```bash
   sudo systemctl daemon-reload
   sudo systemctl enable --now mokuro-bunko
   ```

5. **Check status**:
   ```bash
   sudo systemctl status mokuro-bunko
   journalctl -u mokuro-bunko -f
   ```

SIGTERM is a clean shutdown, so `systemctl stop` finishes in a moment.

## Docker

Images are published at `ghcr.io/gnathonic/mokuro-bunko`:

| Tag | Dockerfile | What | Size |
|---|---|---|---|
| `latest-lite`, `<ver>-lite` | `deploy/docker/Dockerfile.lite` | Server only, distroless, no OCR, no nginx. amd64, arm64. | 29 MB; idles at ~16 MiB RSS |
| `latest`, `<ver>` | `deploy/docker/Dockerfile` | Server + local OCR + nginx and tini. amd64. No OCR backend inside: once it serves, the server downloads the one for the GPU the container is given (CPU, NVIDIA or AMD) in the background. | see [PACKAGING.md §2](rust-port/PACKAGING.md#2-docker-images-ghcriognathonicmokuro-bunko) |
| `latest-cuda`, `<ver>-cuda` | — | The same image as `latest`, under the name of the 0.5.2 CUDA image, so templates written for it keep working. | same image |

There are no arm64 OCR images (arm64 hosts run the lite image and OCR on a
processor elsewhere). `latest*` follow stable releases (from 0.7.0 on); `beta`
and `beta-lite` follow every release, beta or stable (what this branch's
compose files and Unraid templates default to while 0.7 is in beta).

Everything lives under `/data` (library, database, `config.yaml`, the OCR
backend in `/data/backends`, the models in `/data/models`), so mount a volume
there. `MOKURO_CONFIG` defaults to `/data/config.yaml` (the Unraid template
sets `/config/config.yaml`). All images start as root and `bunko-init` drops to
`PUID:PGID` (1000:1000 by default; the Unraid template passes 99:100), keeping
the groups that own GPU device nodes passed in, sets `UMASK` (002) and chowns
the storage and config directories (recursively with `TAKE_OWNERSHIP=true`).
`docker run --user ...` works too. `MOKURO_*` variables work as everywhere
else; `MOKURO_INSTALL_KIND=docker` is set, so the updater only reports the
image to pull. The health check is `mokuro-bunko healthcheck`.

To build an image yourself, `docker build -f deploy/docker/Dockerfile.lite
-t mokuro-bunko:lite .` (the Dockerfiles compile the binary inside). For a
host without internet, `docker build -f deploy/docker/Dockerfile
--build-arg BAKE_PACK=1 --build-arg PACK_VARIANT=cpu .` (or `cu130`, `rocm7.1`)
builds a backend pack into the image; it is used until a pack of the same
release is installed into `/data/backends`. Published images never carry one.

### OCR backend on first start

The server (and `processor serve`) answers within seconds of the start; the OCR
backend installs in the background meanwhile, as on any system (see [OCR
backend](#ocr-backend-install-ocr)), with what the container was given:

1. Nothing happens when local OCR is off (`ocr.backend: skip` or
   `ocr.local_processing: false`, also through `MOKURO_OCR_BACKEND` /
   `MOKURO_OCR_LOCAL_PROCESSING`) or `MOKURO_OCR_AUTO_INSTALL=false`.
2. It looks at the GPUs the container was given: an NVIDIA GPU passed in by
   the NVIDIA container toolkit (`--gpus all`, or `--runtime=nvidia` with
   `NVIDIA_VISIBLE_DEVICES`), an AMD GPU passed in as `--device /dev/kfd
   --device /dev/dri`. A GPU the host has but the container was not given is
   ignored, and `install-ocr --list` / `doctor` say which flag would pass it in.
3. It applies `ocr.backend` (`MOKURO_OCR_BACKEND`): `auto` takes that GPU,
   `cpu` stays on the CPU, `cuda` / `rocm` take that vendor's GPU, or the CPU
   with a note when it is not there.
4. It downloads the matching backend pack of this release into
   `/data/backends` (~100 MB for the CPU; ~350 MB plus ~1.6 GB of NVIDIA's CUDA
   libraries from PyPI; ~3 GB for AMD) and the models of the enabled OCR
   engines into `/data/models`; local OCR then starts by itself.

Later starts find the pack and download nothing. Started with a different GPU
(or with `MOKURO_OCR_BACKEND` asking for a GPU it now sees) the container
installs that GPU's pack and removes the one it replaces; started without its
GPU it keeps the GPU pack, which runs on the CPU too. An image update installs
the new release's pack on its first start. If the download fails, the server
keeps serving (ppocr-manga and remote processors work; hayai-nova and
paddle-manga wait for the pack), the admin panel shows the failure with a
Retry button, and it is tried again later and at the next start.
`MOKURO_OCR_AUTO_INSTALL=false` leaves the install to you: the Install button,
or `docker exec mokuro-bunko mokuro-bunko install-ocr`. 0.5.2's
`OCR_AUTO_INSTALL=true` still works; its `OCR_AUTO_INSTALL=false` is ignored,
as it never stopped 0.5.2 from installing OCR on start.

### Basic Docker

```bash
docker run -d \
  --name mokuro-bunko \
  -p 8080:8080 \
  -v /path/to/storage:/data \
  -e PUID=1000 -e PGID=1000 \
  -e MOKURO_STORAGE=/data -e MOKURO_CONFIG=/data/config.yaml \
  -e MOKURO_REGISTRATION_MODE=invite \
  ghcr.io/gnathonic/mokuro-bunko:latest
```

Without a GPU OCR runs on the CPU; the first start downloads the CPU backend
and the models into `/data`. For GPU OCR pass the GPU in (below), or run the
lite image (or `latest` with `MOKURO_OCR_LOCAL_PROCESSING=false`) and a
[remote processor](#remote-ocr-processors) on the GPU machine. The first
visit to the setup page from another computer asks for the setup code (see
[First start](#first-start)): `docker logs mokuro-bunko` shows it. Or pass
`-e MOKURO_ADMIN_USERNAME=... -e MOKURO_ADMIN_PASSWORD=...` to create the
admin on the first start.

### Docker Compose

- [`deploy/docker-compose.yml`](../deploy/docker-compose.yml): the `latest`
  image (CPU OCR; commented lines for an NVIDIA or AMD GPU).
- [`deploy/docker-compose.lite.yml`](../deploy/docker-compose.lite.yml): the
  lite image, no OCR; pair it with a processor.
- [`deploy/docker-compose.processor.yml`](../deploy/docker-compose.processor.yml):
  an OCR processor for a library elsewhere (NVIDIA, AMD or CPU).
- [`deploy/docker-compose.unraid-cuda.yml`](../deploy/docker-compose.unraid-cuda.yml):
  the full image with an NVIDIA GPU and Unraid paths.
- [`deploy/docker-compose.cloudflared.yml`](../deploy/docker-compose.cloudflared.yml)
  adds a Cloudflare tunnel (`CLOUDFLARE_TUNNEL_TOKEN` from the Zero Trust
  dashboard) and exposes no port.

```bash
docker compose -f deploy/docker-compose.lite.yml up -d
```

### nginx download offload (full image)

`MOKURO_NGINX_ACCEL=1` makes the entrypoint start nginx on `MOKURO_PORT` and
move the server to `127.0.0.1:MOKURO_BACKEND_PORT` (8081); nginx then serves
library downloads itself through `X-Accel-Redirect`, which is the fastest way
to hand out large volumes. It is **off by default** (the async server does not
need it for ordinary use). The nginx in the image already carries the
processor WebSocket settings. The lite image has no nginx and drops the
variable with a warning, so a carried-over `MOKURO_NGINX_ACCEL=1` never
produces empty downloads.

### Docker with an NVIDIA GPU

```bash
docker compose -f deploy/docker-compose.unraid-cuda.yml up -d
```

It needs the NVIDIA container toolkit, a driver 580 or newer and a Turing
(GTX 16xx / RTX 20xx) or newer GPU. The first start downloads the `cu130`
pack (about 2 GB with NVIDIA's CUDA libraries) into `/data/backends`; only the
driver comes from the host. To check what the container sees and which pack it
would install:

```bash
docker run --rm --gpus all ghcr.io/gnathonic/mokuro-bunko:latest install-ocr --list
```

### Docker with an AMD GPU

```bash
docker run -d --name mokuro-bunko --device /dev/kfd --device /dev/dri \
  -p 8080:8080 -v /path/to/storage:/data ghcr.io/gnathonic/mokuro-bunko:latest
```

Supported: Radeon RX 6000 (gfx1030; RX 6600/6700 run its kernels), RX 7000
and RX 9000 on Linux, with the amdgpu kernel driver; nothing from ROCm is
needed on the host. The first start downloads the `rocm7.1` pack (about 3 GB;
6.8 GB unpacked). The server keeps the groups that own `/dev/kfd` and
`/dev/dri/renderD*` when it drops to `PUID:PGID`; with `docker run --user`,
add them with `--group-add`.

### Unraid

The Unraid templates are [`deploy/unraid/mokuro-bunko.xml`](../deploy/unraid/mokuro-bunko.xml)
(full image, `ghcr.io/gnathonic/mokuro-bunko:beta` while 0.7 is in beta, `latest`
from 0.7.0 on; `latest-cuda` is the same image) and
[`deploy/unraid/mokuro-bunko-lite.xml`](../deploy/unraid/mokuro-bunko-lite.xml)
(lite image, `beta-lite` / `latest-lite`). The templates of this branch update
from it (`TemplateURL` on the `0.7` branch).

1. For an NVIDIA GPU, install the Unraid NVIDIA driver plugin (a driver of 580
   or newer for the CUDA 13 pack) and keep `--runtime=nvidia` in
   `Extra Parameters`. For an AMD GPU, replace it with
   `--device=/dev/kfd --device=/dev/dri`. Without a GPU, clear it.
2. Import the template.
3. Map:
   - `/data` -> `/mnt/user/appdata/mokuro-bunko/data`
   - `/config` -> `/mnt/user/appdata/mokuro-bunko/config`
4. Environment variables:
   - `MOKURO_CONFIG=/config/config.yaml`
   - `MOKURO_OCR_BACKEND=auto` (or `cuda`, `rocm`, `cpu`)
   - `NVIDIA_VISIBLE_DEVICES=all`
   - `NVIDIA_DRIVER_CAPABILITIES=compute,utility`

The web UI comes up within seconds; the OCR backend for the GPU the container
sees downloads into `/data/backends` in the background (about 2 GB for
NVIDIA), with the progress in the log and the admin panel. For the first admin,
open `http://<unraid>:<port>/setup` and enter the setup code from the
container's log, or fill in the template's *Admin username* / *Admin password*
before the first start.

Optional:
- `MOKURO_OCR_GENERATIONS`: the OCR generations as JSON, e.g.
  `[{"name":"hayai-nova","engine":"hayai-nova","detector":"ppocr-manga","primary":true},{"name":"paddle-manga","engine":"paddle-manga","detector":"ppocr-manga"}]`.
  Leave it empty to use `config.yaml`; the admin panel (OCR → Generations)
  is the easier way to edit them.
- `MOKURO_OCR_LOCAL_PROCESSING=false`: no OCR in the container; a remote
  processor runs the queue (nothing is downloaded).
- `MOKURO_OCR_AUTO_INSTALL=false`: no automatic OCR backend install (the
  admin panel offers an Install button).
- `MOKURO_NGINX_ACCEL=1`: put nginx in front for downloads; the server then
  listens on `MOKURO_BACKEND_PORT` (default 8081) inside the container.
- `TAKE_OWNERSHIP=true`: chown `/data` and `/config` at boot.

The 0.5 variables `MOKURO_BUNKO_OCR_ENV`, `MOKURO_BUNKO_OCR_ENGINES_ENV` and
`MOKURO_BUNKO_MOKURO_SPEC` are accepted and ignored, `OCR_AUTO_INSTALL=true`
keeps the install on start on and `OCR_AUTO_INSTALL=false` is ignored (0.5.2
installed OCR on start either way), so an existing template keeps working
after you change its repository.

## Admin Setup

### First Admin User

The first browser visit to a new server opens a setup page that creates the
admin account (see [First start](#first-start)); `mokuro-bunko setup` does the
same in the console. From the command line (it asks for the password,
hidden; a password given with `--password` would stay in the shell's history):

```bash
mokuro-bunko admin add-user admin --role admin
```

Every admin command works on the database of the library whose
configuration it reads: pass the library's own `--config` (before `admin`)
or `MOKURO_CONFIG` if it is not in the default place.

### Admin Commands

```bash
# List users
mokuro-bunko admin list-users

# Change user role
mokuro-bunko admin change-role username editor

# Generate invite code
mokuro-bunko admin generate-invite --role registered --expires 7d

# Delete user
mokuro-bunko admin delete-user username

# Bring a deleted user back (a deleted name cannot be added again)
mokuro-bunko admin restore-user username

# Approve pending user (when mode=approval)
mokuro-bunko admin approve-user username
```

`mokuro-bunko admin --help` lists the rest (`set-password`, `disable-user`,
`list-invites`, `delete-invite`).

## SSL/TLS Setup

### Self-Signed Certificate (Development)

```yaml
ssl:
  enabled: true
  auto_cert: true
```

The server generates certificates in `~/.local/share/mokuro-bunko/certs/`
(`%LOCALAPPDATA%\mokuro-bunko\certs\` on Windows). `mokuro-bunko ssl status`
shows the certificate in use.

### Let's Encrypt (Production)

1. **Obtain certificate** using certbot:
   ```bash
   sudo certbot certonly --standalone -d mokuro.example.com
   ```

2. **Configure mokuro-bunko**:
   ```yaml
   ssl:
     enabled: true
     cert_file: "/etc/letsencrypt/live/mokuro.example.com/fullchain.pem"
     key_file: "/etc/letsencrypt/live/mokuro.example.com/privkey.pem"
   ```

3. **Set up auto-renewal**:
   ```bash
   sudo certbot renew --dry-run
   ```

## Troubleshooting

See [troubleshooting.md](troubleshooting.md) for OCR problems, logs and the
`doctor` command. A few deployment-specific checks:

### Connection Refused

- Check the server is running: `systemctl status mokuro-bunko`
- Check the port is open: `ss -tlnp | grep 8080`
- Check firewall: `sudo ufw status`

### CORS Errors

Add your client's origin to the allowed list:

```yaml
cors:
  allowed_origins:
    - "https://your-client-domain.com"
```

### WebDAV Mount Issues

Some WebDAV clients require specific settings:

- **davfs2** (Linux): May need to set `use_locks 0` in `/etc/davfs2/davfs2.conf`
- **Windows** (Explorer's "Map network drive", or `net use Z: http://server:8080/mokuro-reader /user:NAME`
  in an ordinary command prompt; no admin needed). Windows' WebDAV client has three
  limits (measured on Windows 11, build 26200):
  - It sends a password over plain `http://` only after
    `HKLM\SYSTEM\CurrentControlSet\Services\WebClient\Parameters` `BasicAuthLevel` is set
    to `2` (as admin, then `net stop WebClient & net start WebClient`). Until then the drive
    maps but is read-only: creating a folder fails with "The request is not supported".
    Over `https://` with a certificate Windows trusts (a tunnel or reverse proxy) no change
    is needed.
  - Files over 50 MB (`FileSizeLimitInBytes`, default 50000000) cannot be copied in either
    direction until that value is raised (e.g. to `4294967295`).
  - Copying a **new** volume in does not work ("The parameter is incorrect"): Windows
    first creates an empty file, which the server refuses because an empty file is not
    a `.cbz`. Browsing, reading, renaming (also case-only), moving and deleting work. Add
    volumes with Mokuro Reader, or put them into the library folder on the server
    machine.

### Database Locked

If you see "database is locked" errors:

1. Stop all mokuro-bunko processes
2. Check for stale lock files in the storage directory
3. Ensure only one instance is running

## Performance Tuning

- A lite server needs little: it streams downloads and uploads from and to
  disk and keeps byte-bounded caches. For a very large library raise
  `server.cache_mb`; on a one-core host set `server.threads: 1`.
- Put a reverse proxy (nginx/Caddy) in front, and in the full image
  let nginx serve library downloads (`MOKURO_NGINX_ACCEL=1`, with the repo's
  `deploy/nginx-internal.conf.template`, which the images use) so large
  downloads never touch the server process.
- Use SSD storage for the database, and raise file descriptor limits for
  large libraries.
- Move OCR to a dedicated machine with a
  [remote processor](#remote-ocr-processors).
