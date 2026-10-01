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
| **full** | Lite plus ONNX Runtime, the OCR engines and `processor`. | A machine that does OCR: the library host itself, or a processor. |
| **full-cuda** | Full with the CUDA execution provider. | NVIDIA GPUs on Linux and Windows. |

Platform support of the release archives: Linux x86_64 and aarch64 (lite: a
static binary for any distribution; full: glibc 2.35 or newer, i.e. Debian
12, Ubuntu 22.04, RHEL 10; `full-cuda`: x86_64 only), Windows x86_64
(`full` uses DirectML), macOS Apple silicon (`full` uses CoreML) and macOS
Intel (lite only), plus an Android APK of the lite server.

**GPU prerequisites.** `full-cuda` needs an NVIDIA driver **580 or newer**
(`nvidia-smi` should work) plus CUDA 13 and cuDNN 9 libraries on the system;
the CUDA Docker image brings them. The Windows `full` build needs only a
DirectX 12 GPU with a current driver. Nothing else is installed on the host;
the OCR models are downloaded by mokuro-bunko itself (see below).

### Linux and macOS: `install.sh`

```bash
curl -fsSL https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.sh | sh
curl -fsSL https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.sh | sh -s -- --flavor lite --systemd
```

The script picks the archive for your OS, CPU and flavor (`--flavor
lite|full|full-cuda`, default: full where it exists, else lite), checks the
signature of the release manifest (needs OpenSSL 3; `--require-signature`
fails without it) and the archive's sha256, and installs into
`~/.local/lib/mokuro-bunko` with a link in `~/.local/bin` (as root:
`/usr/local/lib/mokuro-bunko` and `/usr/local/bin`; `--prefix DIR` changes
it). A full binary built for a newer glibc than yours is caught before
anything is replaced. `--systemd` installs and starts a service (see
[Systemd](#systemd-service)), `--processor` installs the OCR processor unit
(needs a full flavor), `--version X.Y.Z` pins a release, `--dry-run` shows
what would happen. Run it again to update.

Without the script: unpack `mokuro-bunko-<version>-<target>-<flavor>.tar.gz`
and run `./mokuro-bunko serve`. Keep the files of the archive together; in a
`full-cuda` build the CUDA provider libraries next to the executable are
loaded from there. On macOS a tarball downloaded in a browser is quarantined:
`xattr -d com.apple.quarantine mokuro-bunko` (the script is not affected).

### Windows: `install.ps1` or the portable zip

```powershell
powershell -c "irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.ps1 | iex"
```

Installs into `%LOCALAPPDATA%\mokuro-bunko\app` (no admin rights, nothing in
the registry), adds Start-menu shortcuts, runs `mokuro-bunko doctor` and
starts the server. Your data stays in `%LOCALAPPDATA%\mokuro-bunko`. Options
are `-Flavor full|full-cuda|lite`, `-Version`, `-InstallDir`, `-Portable`,
`-Startup` (start at logon), `-NoShortcut`, `-NoStart`; see the top of
[`scripts/install.ps1`](../scripts/install.ps1). It verifies the archive's
sha256 (PowerShell cannot verify the ed25519 signature; the in-app updater
does). Windows SmartScreen may warn: the executable is not Authenticode
signed.

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
cargo build --release -p mokuro-bunko                        # full, CPU
cargo build --release -p mokuro-bunko --no-default-features  # lite
cargo build --release -p mokuro-bunko --features cuda        # NVIDIA CUDA
./target/release/mokuro-bunko serve
```

### OCR models

A full build downloads the ONNX models for the configured OCR generations the
first time they run, into `<storage>/models/`, checking every file's sha256.
To fetch them ahead of time, for example before moving to a network without
internet access:

```bash
mokuro-bunko models download                    # everything
mokuro-bunko models download --engine hayai-nova
mokuro-bunko models list                        # what exists and what is on disk
mokuro-bunko models verify
```

`MOKURO_MODELS_DIR` points at a directory of model files for air-gapped
hosts, and `MOKURO_MODELS_DOWNLOAD=0` forbids downloads.

### Updating

A copy installed by `install.sh` as a user, the Windows zip or `install.ps1`
updates itself: the admin panel's **Updates** card (Status tab) shows new
releases and its **Update and restart** button downloads, verifies and
installs them. From a terminal: `mokuro-bunko update check` and
`mokuro-bunko update apply`. Root-installed system units, distro packages and
Docker only get a notice ("re-run `install.sh`", "pull the new image"). Your
data is never touched by an update. Set `update.check: false` to stop the
background check. See [configuration](configuration.md#updates).

## First start

The first browser visit to a new server opens a setup page that creates the
admin account. From the machine itself that just works. From another machine,
and under Docker bridge networking, the page needs a one-time token: while no
admin exists the server writes `<storage>/.setup-token` and logs the URL
(`http://host:8080/setup?token=...`) at startup. `MOKURO_SETUP_TOKEN` sets a
token of your own. `mokuro-bunko setup` does the same in the console.

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
same release as the library. There is nothing else to install: no Python, no
git, no checkout. Install it as in [Installing](#installing) (`install.sh
--flavor full` or `full-cuda`, `install.ps1`, a Docker image), then one
command, `processor setup`, does the rest. A processor machine needs the GPU
driver only (NVIDIA driver 580 or newer for the CUDA build; any DirectX 12
driver for DirectML on Windows).

```bash
mokuro-bunko processor setup
```

It asks for the library's URL, the processor account and its password. In
order, it:

1. logs in to the library and checks that the account has the `processor`
   role and that the library speaks this release's processor protocol,
   before it writes anything and without registering the machine;
2. detects the hardware and the execution provider `auto` resolves to;
3. writes `processor.yaml` with only the settings that differ from the
   defaults, readable by you only (mode 600);
4. offers to run the processor as a service (see
   [step 4](#4-keeping-the-processor-running)).

Every answer can also be given as an option, for a script: `--url`,
`--username`, `--password-stdin` (the password from the first line of
standard input; there is deliberately no `--password`), `--name` (how the
library shows this machine; default the hostname), `--backend` (`auto`,
`cuda`, `rocm`, `webgpu`, `directml`, `coreml`, `cpu`), `--tls-verify`
(`true`, `false`, or a certificate's path), `--config` (where to write the
file, default `processor.yaml`), `--yes` (accept every default),
`--no-service`, and `--force` (overwrite an existing file).
`mokuro-bunko processor setup --help` lists them.

The OCR models for the library's generations are downloaded the first time
they run (or up front with `mokuro-bunko models download`). Within a few
seconds of starting, the processor appears in the library's admin panel (the
OCR section's **Processors** card) and starts taking volumes; with
`ocr.autobench` on, each generation is first benchmarked on it once.
`mokuro-bunko processor status --config processor.yaml` prints what it last
did.

#### Linux

Install a full flavor (`install.sh --flavor full`, or `full-cuda` for NVIDIA)
and run `mokuro-bunko processor setup`. NVIDIA needs the driver, CUDA 13 and
cuDNN 9 libraries for the CUDA build. The release builds on Linux have
CUDA and CPU execution providers only, so an AMD GPU processor runs on the
CPU unless you build from source with `--features webgpu` and pass
`--backend webgpu`.

#### Windows

Install with `install.ps1` (the `full` flavor uses DirectML on any DirectX 12
GPU; `-Flavor full-cuda` for NVIDIA with CUDA) and run
`mokuro-bunko.exe processor setup` from a terminal in the install folder (or
the Start-menu shortcut's folder). The questions are the same.
`processor.yaml` is restricted to your account, and the last question is
"Start the processor now, and at every logon (a Startup entry)?". The
processor keeps its storage in `%LOCALAPPDATA%\mokuro-bunko-processor`.

#### macOS

Use the `aarch64-apple-darwin` full build (CoreML) and run
`mokuro-bunko processor setup`. `processor service` installs a launchd agent.

#### Docker (NVIDIA)

[`deploy/docker-compose.processor.yml`](../deploy/docker-compose.processor.yml)
runs the CUDA image as a processor: put `processor.yaml` in `./processor/`
and start it with `docker compose -f deploy/docker-compose.processor.yml up -d`
(it needs the NVIDIA container runtime and a driver 580 or newer). Models are
cached in the compose volume. The password can come from the environment
instead of the file: `MOKURO_PROCESSOR_PASSWORD`.

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

```bash
sudo useradd -r -m -d /var/lib/mokuro-bunko -s /usr/sbin/nologin mokuro   # if it does not exist
sudo usermod -aG video,render mokuro
sudo install -D -m 600 -o mokuro processor.yaml /etc/mokuro-bunko/processor.yaml
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

Set `MOKURO_NGINX_ACCEL=1` only when nginx really is in front (the full and
CUDA images set it up themselves when you ask for it): without that nginx,
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
   A full build keeps its models in `<storage>/models`, so no extra setting
   is needed.

3. **Enable and start**:
   ```bash
   sudo systemctl daemon-reload
   sudo systemctl enable --now mokuro-bunko
   ```

4. **Check status**:
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
| `latest`, `<ver>` | `deploy/docker/Dockerfile` | Server + CPU OCR + nginx and tini. amd64, arm64. | ~172 MB |
| `latest-cuda`, `<ver>-cuda` | `deploy/docker/Dockerfile.cuda` | As `latest`, with the CUDA execution provider (NVIDIA driver 580 or newer, NVIDIA container runtime). amd64. | a few GB (NVIDIA base) |

Everything lives under `/data` (library, database, `config.yaml`, models), so
mount a volume there. All images start as root and `bunko-init` drops to
`PUID:PGID` (1000:1000; 99:100 in the CUDA image, as on Unraid), sets `UMASK`
(002) and chowns the storage and config directories (recursively with
`TAKE_OWNERSHIP=true`). `docker run --user ...` works too. `MOKURO_*`
variables work as everywhere else; `MOKURO_INSTALL_KIND=docker` is set, so the
updater only reports the image to pull. The health check is
`mokuro-bunko healthcheck`.

To build an image yourself, `docker build -f deploy/docker/Dockerfile.lite
-t mokuro-bunko:lite .` (the Dockerfiles compile the binary inside).

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

The `latest` image has no GPU runtime, so OCR in it runs on the CPU. For GPU
OCR use the CUDA image below, or run the lite image (or `latest` with
`MOKURO_OCR_LOCAL_PROCESSING=false`) and a
[remote processor](#remote-ocr-processors) on the GPU machine. The first
visit to the setup page from the host needs the setup token (see
[First start](#first-start)): `docker logs mokuro-bunko` shows the URL.

### Docker Compose

- [`deploy/docker-compose.yml`](../deploy/docker-compose.yml): the `latest`
  image (CPU OCR).
- [`deploy/docker-compose.lite.yml`](../deploy/docker-compose.lite.yml): the
  lite image, no OCR; pair it with a processor.
- [`deploy/docker-compose.processor.yml`](../deploy/docker-compose.processor.yml):
  a CUDA OCR processor for a library elsewhere.
- [`deploy/docker-compose.unraid-cuda.yml`](../deploy/docker-compose.unraid-cuda.yml):
  the CUDA image with Unraid paths.
- [`deploy/docker-compose.cloudflared.yml`](../deploy/docker-compose.cloudflared.yml)
  adds a Cloudflare tunnel (`CLOUDFLARE_TUNNEL_TOKEN` from the Zero Trust
  dashboard) and exposes no port.

```bash
docker compose -f deploy/docker-compose.lite.yml up -d
```

### nginx download offload (full and CUDA images)

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

It needs the NVIDIA container runtime and a driver 580 or newer, runs as
`PUID`/`PGID` (default 99/100), keeps its config in `/config`
(`MOKURO_CONFIG=/config/config.yaml`) and its data, including the downloaded
models, in `/data`.

### Unraid + NVIDIA GPU

The Unraid templates are [`deploy/unraid/mokuro-bunko.xml`](../deploy/unraid/mokuro-bunko.xml)
(CUDA image, `ghcr.io/gnathonic/mokuro-bunko:latest-cuda`) and
[`deploy/unraid/mokuro-bunko-lite.xml`](../deploy/unraid/mokuro-bunko-lite.xml)
(lite image).

1. Install the Unraid NVIDIA driver plugin (a driver of 580 or newer for the
   CUDA 13 image).
2. Import the template, and confirm `Extra Parameters` includes
   `--runtime=nvidia`.
3. Map:
   - `/data` -> `/mnt/user/appdata/mokuro-bunko/data`
   - `/config` -> `/mnt/user/appdata/mokuro-bunko/config`
4. Environment variables:
   - `MOKURO_CONFIG=/config/config.yaml`
   - `MOKURO_OCR_BACKEND=auto` (or `cuda`)
   - `NVIDIA_VISIBLE_DEVICES=all`
   - `NVIDIA_DRIVER_CAPABILITIES=compute,utility`

Optional:
- `MOKURO_OCR_GENERATIONS`: the OCR generations as JSON, e.g.
  `[{"name":"hayai-nova","engine":"hayai-nova","detector":"ppocr-manga","primary":true},{"name":"paddle-manga","engine":"paddle-manga","detector":"ppocr-manga"}]`.
  Leave it empty to use `config.yaml`; the admin panel (OCR → Generations)
  is the easier way to edit them.
- `MOKURO_OCR_LOCAL_PROCESSING=false`: no OCR in the container; a remote
  processor runs the queue.
- `MOKURO_NGINX_ACCEL=1`: put nginx in front for downloads; the server then
  listens on `MOKURO_BACKEND_PORT` (default 8081) inside the container.
- `TAKE_OWNERSHIP=true`: chown `/data` and `/config` at boot.

The 0.5 variables `MOKURO_BUNKO_OCR_ENV`, `MOKURO_BUNKO_OCR_ENGINES_ENV`,
`MOKURO_BUNKO_MOKURO_SPEC` and `OCR_AUTO_INSTALL` are accepted and ignored,
so an existing template keeps working after you change its repository.

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
- **Windows**: Run `net use Z: http://server:8080/` in admin command prompt

### Database Locked

If you see "database is locked" errors:

1. Stop all mokuro-bunko processes
2. Check for stale lock files in the storage directory
3. Ensure only one instance is running

## Performance Tuning

- A lite server needs little: it streams downloads and uploads from and to
  disk and keeps byte-bounded caches. For a very large library raise
  `server.cache_mb`; on a one-core host set `server.threads: 1`.
- Put a reverse proxy (nginx/Caddy) in front, and in the full and CUDA images
  let nginx serve library downloads (`MOKURO_NGINX_ACCEL=1`, with the repo's
  `deploy/nginx-internal.conf.template`, which the images use) so large
  downloads never touch the server process.
- Use SSD storage for the database, and raise file descriptor limits for
  large libraries.
- Move OCR to a dedicated machine with a
  [remote processor](#remote-ocr-processors).
