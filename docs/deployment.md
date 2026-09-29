# Deployment Guide

This guide covers the ways to install and run Mokuro Bunko Server, from a
single machine on a LAN to a library server with remote OCR processors behind
a reverse proxy. For every setting, see the
[configuration reference](configuration.md).

## Installing

Nothing is published to PyPI or a container registry yet, and there are no
standalone binaries: install from source.

**Prerequisites** on every machine that runs OCR: `git` (the OCR installer
fetches the optimized mokuro fork from GitHub) and, for GPU OCR, the NVIDIA
or AMD driver on the host. The installer manages Python packages only; the
CUDA toolkit is not needed, the PyTorch wheels bring their own runtime.

### From a source checkout (recommended)

Requires [uv](https://docs.astral.sh/uv/), which provisions Python 3.12
itself. Install it with `curl -LsSf https://astral.sh/uv/install.sh | sh`
(Windows PowerShell: `irm https://astral.sh/uv/install.ps1 | iex`); it goes
into `~/.local/bin`, so open a new terminal before using it.

```bash
git clone https://github.com/Gnathonic/mokuro-bunko.git
cd mokuro-bunko
uv sync
uv run mokuro-bunko serve        # first browser visit walks you through setup
```

The OCR environments are created inside the checkout (`.ocr-env`,
`.ocr-engines-env`) the first time they are needed, or up front with
`uv run mokuro-bunko install-ocr`. Update with `git pull && uv sync` and a
restart.

### As a system package

For a service account, install into a virtual environment of its own:

```bash
sudo python3 -m venv /opt/mokuro-bunko
sudo /opt/mokuro-bunko/bin/pip install "git+https://github.com/Gnathonic/mokuro-bunko.git"
sudo ln -s /opt/mokuro-bunko/bin/mokuro-bunko /usr/local/bin/mokuro-bunko
```

Installed this way, the OCR environments go under the running user's
`~/.mokuro-bunko/` unless `MOKURO_BUNKO_OCR_ENV` and
`MOKURO_BUNKO_OCR_ENGINES_ENV` say otherwise. Python 3.11 or later is
required; for CUDA OCR use 3.12 (CUDA wheels are not available for newer
interpreters).

### Windows

The one-command setup script and the portable folder edition are described
in the [README](../README.md#quick-start).

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
           proxy_set_header Connection "";

           client_max_body_size 0;
           proxy_request_buffering off;
           proxy_buffering off;
           proxy_send_timeout 300s;
           proxy_read_timeout 300s;
       }
   }

   server {
       listen 80;
       server_name mokuro.example.com;
       return 301 https://$server_name$request_uri;
   }
   ```
   A fuller example is in [`deploy/nginx.conf.example`](../deploy/nginx.conf.example).

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

Caddy automatically handles SSL certificates:

1. **Create Caddyfile**:
   ```
   mokuro.example.com {
       # Remote OCR processors: no body limit, flushed op by op.
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
testing. Remote OCR processors need a path that streams request bodies
without a size limit; if a tunnel or CDN in front of the library cannot,
point processors at an address that reaches the server directly (see
below).

## Remote OCR processors

OCR can run on a different machine from the library: typically a small,
always-on library server and a stronger GPU computer that is on when it is
used. The processor dials out to the library, so nothing needs to be opened
on it. What the settings mean is in
[the configuration reference](configuration.md#remote-ocr-processors) and
how it works in [OCR internals](ocr-internals.md#remote-processors); this is
the setup, in four steps: an account on the library, a network path to it,
the processor machine, and keeping the processor running.

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
the same (the option goes before `admin`), and from a source checkout run
them through `uv run`:

```bash
uv run mokuro-bunko --config /path/to/config.yaml admin add-user gpu-box --role processor
```

A name that was deleted before cannot be added again; bring the account back
with a new password instead (asked for, like `add-user`):

```bash
mokuro-bunko admin restore-user gpu-box --role processor
```

Decide whether the library's own hardware does OCR too. On a small server,
turn it off so it installs nothing and leaves everything to processors:

```yaml
ocr:
  local_processing: false
```

(or `MOKURO_OCR_LOCAL_PROCESSING=false`, or `ocr.backend: skip`). Restart the
library after changing it. The queue then holds — the queue page and the
admin panel say "No processor connected" — until a processor logs in.

The generations to run are configured on the library, as usual; processors
run whatever the library sends them.

### 2. A path from the processor to the library

The processor needs to reach the library's URL, and every proxy in between
must pass `/_processor/` through unbuffered — see
[Remote OCR processors behind a proxy](#remote-ocr-processors-behind-a-proxy).
If the public path cannot (a tunnel or CDN with a body limit), give the
processor a direct address instead: the library's port on the LAN (bind
`server.host` to the LAN interface, or forward a port to it), with TLS
verification off, or the certificate's path, if that address uses a
self-signed certificate (`processor setup --tls-verify false` or
`--tls-verify /path/to/cert.pem`). Each processor holds a few of the
library's request threads while connected (one for its assignment stream,
one per open session, one more while benchmarking); with several processors,
raise `MOKURO_THREADS` (default 50) on the library if needed.

### 3. The processor machine

A processor runs mokuro-bunko from a source checkout of the same release as
the library. It needs three things installed first — `git`, the GPU driver
and [uv](https://docs.astral.sh/uv/) — and then one command,
`processor setup`, does the rest.

#### Linux

**Prerequisites.**

- `git`, from the distribution's packages.
- The GPU driver. NVIDIA: the driver, nothing else (`nvidia-smi` should
  work); the CUDA toolkit is not needed, the PyTorch wheels bring their own
  runtime. AMD: the kernel's `amdgpu` driver, nothing else — see
  [AMD GPUs](#amd-gpus) below.
- uv:

  ```bash
  curl -LsSf https://astral.sh/uv/install.sh | sh
  ```

  It installs into `~/.local/bin`. Open a new terminal (or run
  `source ~/.local/bin/env`) so that `uv --version` works. uv provisions
  Python 3.12 itself; the system Python does not matter.

**Install and set up:**

```bash
git clone https://github.com/Gnathonic/mokuro-bunko.git
cd mokuro-bunko
uv sync
uv run mokuro-bunko processor setup
```

`processor setup` asks for the library's URL, the processor account and its
password:

```text
$ uv run mokuro-bunko processor setup
Library URL: https://library.example
Processor username: gpu-box
Password:
Checking gpu-box on https://library.example ...
Logged in: gpu-box is a processor account (protocol 2, mokuro-bunko 0.5.0).
Hardware: NVIDIA GeForce RTX 4060 (CUDA) -> cuda
Wrote /home/you/mokuro-bunko/processor.yaml (readable by you only)
Install the OCR environments now? (downloads several GB) [Y/n]:
Installing for backend: cuda
...
Processor environments ready
Run the processor as a systemd user service now? [Y/n]:
Installed /home/you/.config/systemd/user/mokuro-bunko-processor.service and started it.
Follow it with: journalctl --user -u mokuro-bunko-processor.service -f

Summary
  Config:    /home/you/mokuro-bunko/processor.yaml
  Installed: yes, the OCR environments (cuda)
  Running:   yes, as the systemd user service mokuro-bunko-processor.service
  Logs:      journalctl --user -u mokuro-bunko-processor.service -f
```

In order, it:

1. logs in to the library and checks that the account has the `processor`
   role and that the library speaks this release's processor protocol —
   before it writes anything, and without registering the machine;
2. detects the GPU and the backend `auto` resolves to;
3. writes `processor.yaml` with only the settings that differ from the
   defaults, readable by you only (mode 600);
4. installs the OCR environments (`processor install`: a few GB of
   downloads, several minutes), ending with a smoke test that runs a real
   computation on the GPU;
5. offers to run the processor as a service (see
   [step 4](#4-keeping-the-processor-running)).

Every answer can also be given as an option, for a script:
`--url`, `--username`, `--password-stdin` (the password from the first line
of standard input; there is deliberately no `--password`), `--name` (how the
library shows this machine; default the hostname), `--backend`
(`auto`, `cuda`, `rocm`, `cpu`), `--tls-verify` (`true`, `false`, or a
certificate's path), `--yes` (accept every default), `--no-install`,
`--no-service`, and `--force` (overwrite an existing `processor.yaml`).
`uv run mokuro-bunko processor setup --help` lists them.

The install includes every engine with the `ppocr-manga` detector. If one of
the library's generations uses another detector, install it too, because a
processor is only offered rows it can run:

```bash
uv run mokuro-bunko processor install --config processor.yaml --detector ctd
```

Within a few seconds of starting, the processor appears in the library's
admin panel (the OCR section's **Processors** card) and starts taking
volumes; with `ocr.autobench` on, each generation is first benchmarked on it
once. `uv run mokuro-bunko processor status --config processor.yaml` prints
what it last did.

##### AMD GPUs

- **No system ROCm.** `backend: auto` picks ROCm from the kernel driver
  alone: `/dev/kfd` and a GPU node are enough. The PyTorch ROCm wheels carry
  their own runtime, so nothing needs installing under `/opt/rocm`.
- **Cards the wheels were not built for** — an RX 6600 is `gfx1032`, and the
  wheels carry `gfx1030` — run as their family's target: the processor sets
  `HSA_OVERRIDE_GFX_VERSION` (`10.3.0` for these RDNA 2 cards) by itself. A
  value you set yourself wins. When the override is used, the install's
  smoke test says so:
  `ROCm: this card is not in the torch build; HSA_OVERRIDE_GFX_VERSION=10.3.0`.
- **Device permissions.** Check `ls -l /dev/kfd /dev/dri/renderD*`. Where
  they are not open to everyone (`crw-rw----`, group `render` or `video`),
  add yourself to those groups and log out and back in:

  ```bash
  sudo usermod -aG render,video "$USER"
  ```

NVIDIA needs only the driver.

#### Windows

**Prerequisites.**

- Git for Windows ([git-scm.com](https://git-scm.com/download/win), or
  `winget install --id Git.Git -e`), on `PATH`.
- The NVIDIA driver (`nvidia-smi` should work in a terminal). On Windows a
  processor runs OCR on an NVIDIA GPU or the CPU; AMD GPU OCR needs Linux.
- uv, in PowerShell:

  ```powershell
  irm https://astral.sh/uv/install.ps1 | iex
  ```

  It installs into `%USERPROFILE%\.local\bin`; open a new terminal so that
  `uv --version` works.

**Use a normal terminal**, not one opened with "Run as administrator" and
not an SSH session: both are elevated, and there `uv sync` can fail with
"os error 448" (see
[troubleshooting](troubleshooting.md#windows-uv-sync-fails-with-os-error-448-untrusted-mount-point)).

```powershell
git clone https://github.com/Gnathonic/mokuro-bunko.git
cd mokuro-bunko
uv sync
uv run mokuro-bunko processor setup
```

The questions and checks are the same as on Linux. `processor.yaml` is
restricted to your account (with `icacls`), and the last question is
"Start the processor now, and at every logon (a Startup entry)?". The
processor keeps its storage in `%LOCALAPPDATA%\mokuro-bunko-processor`, and
downloads archives to disk there (Windows has no `/dev/shm`; that is
normal).

#### By hand

`processor setup` only writes a file you can write yourself. Copy the
documented example, set the three `library` values, keep the file private,
then install and start:

```bash
cp docs/processor.example.yaml processor.yaml     # Windows: copy docs\processor.example.yaml processor.yaml
chmod 600 processor.yaml
uv run mokuro-bunko processor install --config processor.yaml
uv run mokuro-bunko processor install --config processor.yaml --detector ctd   # only if a generation uses ctd
uv run mokuro-bunko processor serve   --config processor.yaml
```

Every key is described in [`processor.example.yaml`](processor.example.yaml)
and in [the configuration reference](configuration.md#remote-ocr-processors).

### 4. Keeping the processor running

`processor setup` offers this as its last step; to do it later, or again
after moving the checkout or `processor.yaml`:

```bash
uv run mokuro-bunko processor service --config processor.yaml --install
```

Stopping the processor, however it runs, gives the volumes it held back to
the queue, unrecorded.

#### Linux: a systemd user service

`processor service --install` writes
`~/.config/systemd/user/mokuro-bunko-processor.service` from this install's
own paths — its `mokuro-bunko` entry point, the absolute path of
`processor.yaml`, and `MOKURO_BUNKO_OCR_ENV`, `MOKURO_BUNKO_OCR_ENGINES_ENV`
and `HF_HOME` if you set them — then enables and starts it. Without
`--install` it prints the unit instead.

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

- **Its output** is in that window; `uv run mokuro-bunko processor status
  --config processor.yaml` prints what it last did.
- **To stop it**, press <kbd>Ctrl</kbd>+<kbd>C</kbd> in its window, or close
  the window.
- **To stop starting it at logon**, delete `mokuro-bunko-processor.cmd` from
  the Startup folder.

#### A system service under a dedicated user

On a headless box, the processor can run as a system service under an
account of its own, starting at boot with nobody logged in.
[`deploy/mokuro-bunko-processor.service`](../deploy/mokuro-bunko-processor.service)
is that unit. It runs as the user `mokuro`, which must be able to reach the
GPU, and its `ExecStart` is
`/usr/local/bin/mokuro-bunko processor serve --config /etc/mokuro-bunko/processor.yaml`:
the paths of a [system package](#as-a-system-package) install.

From a source checkout, install everything as that user and point the unit
at the checkout:

```bash
sudo useradd -r -m -d /var/lib/mokuro -s /usr/sbin/nologin mokuro   # if it does not exist
sudo usermod -aG video,render mokuro     # the GPU's device groups (render: AMD)
sudo -u mokuro -H bash                   # a shell as mokuro (its home: /var/lib/mokuro)
curl -LsSf https://astral.sh/uv/install.sh | sh
git clone https://github.com/Gnathonic/mokuro-bunko.git ~/mokuro-bunko
cd ~/mokuro-bunko
~/.local/bin/uv sync
~/.local/bin/uv run mokuro-bunko processor setup --no-service
exit
```

Then install the unit with an override for its `ExecStart` (the empty line
clears the original):

```bash
sudo cp /var/lib/mokuro/mokuro-bunko/deploy/mokuro-bunko-processor.service /etc/systemd/system/
sudo systemctl edit mokuro-bunko-processor
```

```ini
[Service]
ExecStart=
ExecStart=/var/lib/mokuro/mokuro-bunko/.venv/bin/mokuro-bunko processor serve --config /var/lib/mokuro/mokuro-bunko/processor.yaml
```

```bash
sudo systemctl enable --now mokuro-bunko-processor
journalctl -u mokuro-bunko-processor -f
```

With a system package install the unit works as shipped: put the
configuration at `/etc/mokuro-bunko/processor.yaml`
(`sudo install -D -m 600 -o mokuro processor.yaml /etc/mokuro-bunko/processor.yaml`)
and run `processor install` as `mokuro`, so the OCR environments and model
caches end up where the service looks for them.

### Remote OCR processors behind a proxy

A processor talks to the library over three channels, all opened by the
processor:

- **the assignment stream** — `GET /_processor/<id>/stream`, one chunked
  response for as long as the processor is connected, with a heartbeat line
  every 15 seconds;
- **the events channel** — `POST /_processor/<id>/sessions/<sid>/events`,
  one chunked *request body* per OCR session, for as long as that session
  runs (a small keep-alive frame every 3 seconds, and every finished sidecar);
- **archive reads** — ordinary `GET`s of the library's `.cbz` files, each
  archive whole, one request per volume. A broken download is resumed with
  `Range` + `If-Range`, so a proxy must pass `Range` and the `ETag` through
  (no streaming settings needed). The repo's nginx template and Caddy both
  do.

A proxy in front of the library must pass the first two through as streams,
or processors connect and then never do any work:

| Requirement | nginx | Why |
|---|---|---|
| Do not buffer request bodies | `proxy_request_buffering off` | A buffered events body reaches the library only when the session ends; the library hears nothing and gives up on the session. |
| No request body size limit | `client_max_body_size 0` | The limit counts the whole body, and one session's body carries every sidecar it produces: a cap of a few hundred MB cuts a long session off. |
| HTTP/1.1 to the backend | `proxy_http_version 1.1` | Chunked bodies are HTTP/1.1. |
| Do not buffer responses | `proxy_buffering off` | The assignment stream must arrive op by op (the library also sends `X-Accel-Buffering: no`). |
| Timeouts well above the heartbeat | `proxy_read_timeout` / `proxy_send_timeout` ≥ 60 s | The stream beats every 15 s and the events body pings every 3 s; anything shorter cuts a healthy processor off. |

Scope these to `location /_processor/` (as in the examples above and in
[`deploy/nginx.conf.example`](../deploy/nginx.conf.example) and
`deploy/nginx-internal.conf.template`) so the upload limit still applies
everywhere else. With Caddy, give `/_processor/*` a `handle` of its own with
no `request_body` limit and `flush_interval -1`
([`deploy/caddy.example`](../deploy/caddy.example)). The Docker images' own
nginx already carries these settings.

Set `MOKURO_NGINX_ACCEL=1` only when nginx really is in front (the generic
Docker image sets it itself; the CUDA image starts its nginx when you set it):
without that nginx, every download is an empty answer, and processors give
such volumes back.

### Updating a library and its processors

The library and its processors must run the same release: they speak one
protocol version, and a processor from another release is refused at
registration ("this library speaks protocol [2]"). Update them together, in
this order:

1. **Stop** every processor: `systemctl --user stop mokuro-bunko-processor`
   (a user service), `sudo systemctl stop mokuro-bunko-processor` (a system
   service), or <kbd>Ctrl</kbd>+<kbd>C</kbd> in its window. Their volumes go
   back to the queue unrecorded.
2. **Update** the code everywhere: the library's install, and each
   processor's checkout with `git pull && uv sync` (run in the checkout).
3. **Restart** the library.
4. **Start** the processors: `systemctl --user start mokuro-bunko-processor`,
   `sudo systemctl start mokuro-bunko-processor`, or on Windows the Startup
   entry (double-click `mokuro-bunko-processor.cmd` in `shell:startup`, or
   log on again).

A running processor keeps the runner it started with until it restarts, so
updating its code while it runs takes effect only on the next start. Each
processor needs a `storage` of its own; a second `processor serve` on the
same storage refuses to start.

## Systemd Service

For running mokuro-bunko as a system service
([`deploy/mokuro-bunko.service`](../deploy/mokuro-bunko.service) is a
variant of this):

1. **Create service file** (`/etc/systemd/system/mokuro-bunko.service`):
   ```ini
   [Unit]
   Description=Mokuro Bunko Server
   After=network.target

   [Service]
   Type=simple
   User=mokuro
   Group=mokuro
   WorkingDirectory=/var/lib/mokuro-bunko
   Environment=MOKURO_STORAGE=/var/lib/mokuro-bunko/storage
   Environment=MOKURO_CONFIG=/var/lib/mokuro-bunko/config.yaml
   ExecStart=/usr/local/bin/mokuro-bunko serve
   Restart=always
   RestartSec=5

   # Security hardening
   NoNewPrivileges=yes
   PrivateTmp=yes
   ProtectSystem=strict
   ProtectHome=yes
   ReadWritePaths=/var/lib/mokuro-bunko

   [Install]
   WantedBy=multi-user.target
   ```
   With `ProtectHome=yes` nothing can be written under a home directory, so
   if this server runs OCR itself, also set
   `Environment=MOKURO_BUNKO_OCR_ENV=/var/lib/mokuro-bunko/.ocr-env`,
   `Environment=MOKURO_BUNKO_OCR_ENGINES_ENV=/var/lib/mokuro-bunko/.ocr-engines-env`
   and `Environment=HF_HOME=/var/lib/mokuro-bunko/huggingface` (or set
   `ocr.local_processing: false` and use a processor).

2. **Create user and directories**:
   ```bash
   sudo useradd -r -s /bin/false mokuro
   sudo mkdir -p /var/lib/mokuro-bunko
   sudo chown mokuro:mokuro /var/lib/mokuro-bunko
   ```

3. **Enable and start**:
   ```bash
   sudo systemctl daemon-reload
   sudo systemctl enable mokuro-bunko
   sudo systemctl start mokuro-bunko
   ```

4. **Check status**:
   ```bash
   sudo systemctl status mokuro-bunko
   journalctl -u mokuro-bunko -f
   ```

## Docker Deployment

No image is published; build one from the repository. The generic image keeps
everything — library, database, `config.yaml` and the OCR environments —
under `/data`, so mount a volume there.

### Basic Docker

```bash
docker build -f deploy/Dockerfile -t mokuro-bunko .
docker run -d \
  --name mokuro-bunko \
  -p 8080:8080 \
  -v /path/to/storage:/data \
  -e MOKURO_REGISTRATION_MODE=invite \
  mokuro-bunko
```

The generic image runs nginx in front of the server for fast downloads and
has no GPU runtime, so OCR in it runs on the CPU. For GPU OCR use the CUDA
image below, or run the container with `MOKURO_OCR_LOCAL_PROCESSING=false`
and a [remote processor](#remote-ocr-processors) on the GPU machine.

### Docker Compose

[`deploy/docker-compose.yml`](../deploy/docker-compose.yml) builds and runs
the generic image:

```bash
cd deploy
docker compose up -d
```

To leave OCR to a processor, add `MOKURO_OCR_LOCAL_PROCESSING=false` to its
`environment`. [`deploy/docker-compose.cloudflared.yml`](../deploy/docker-compose.cloudflared.yml)
adds a Cloudflare tunnel (`CLOUDFLARE_TUNNEL_TOKEN` from the Zero Trust
dashboard) and exposes no port.

### Docker with an NVIDIA GPU

`deploy/Dockerfile.unraid` is a CUDA image (it works outside Unraid too).
Build it and run it with the NVIDIA container runtime:

```bash
docker build -f deploy/Dockerfile.unraid -t mokuro-bunko:unraid-cuda .
docker compose -f deploy/docker-compose.unraid-cuda.yml up -d
```

It runs as `PUID`/`PGID` (default 99/100), keeps its config in `/config`
(`MOKURO_CONFIG=/config/config.yaml`) and its data in `/data`. Point
`MOKURO_BUNKO_OCR_ENV` and `MOKURO_BUNKO_OCR_ENGINES_ENV` at paths under
`/data` so the OCR environments survive a new container (the compose file
does).

### Unraid + NVIDIA GPU

The Unraid template is [`deploy/unraid/mokuro-bunko.xml`](../deploy/unraid/mokuro-bunko.xml).

1. Install the Unraid NVIDIA driver plugin (if using CUDA OCR).
2. Build the image on the Unraid host (see above): the template uses the
   local tag `mokuro-bunko:unraid-cuda`.
3. Import the template, and confirm `Extra Parameters` includes
   `--runtime=nvidia`.
4. Map:
   - `/data` -> `/mnt/user/appdata/mokuro-bunko/data`
   - `/config` -> `/mnt/user/appdata/mokuro-bunko/config`
5. Set env vars:
   - `MOKURO_CONFIG=/config/config.yaml`
   - `MOKURO_OCR_BACKEND=auto` (or `cuda`)
   - `MOKURO_BUNKO_OCR_ENV=/data/.ocr-env` and
     `MOKURO_BUNKO_OCR_ENGINES_ENV=/data/.ocr-engines-env` (persistent OCR
     environments)
   - `NVIDIA_VISIBLE_DEVICES=all`
   - `NVIDIA_DRIVER_CAPABILITIES=compute,utility`

Optional:
- `MOKURO_OCR_GENERATIONS` — the OCR generations as JSON, e.g.
  `[{"name":"mokuro","engine":"mokuro","primary":true},{"name":"hayai-nova","engine":"hayai-nova","detector":"ppocr-manga"}]`.
  Leave it empty to use `config.yaml`; the admin panel (OCR → Generations)
  is the easier way to edit them.
- `MOKURO_OCR_LOCAL_PROCESSING=false` — no OCR in the container; a remote
  processor runs the queue.
- `OCR_AUTO_INSTALL=true` — run `mokuro-bunko install-ocr --backend
  $MOKURO_OCR_BACKEND` on startup (the server installs anything else the
  generations need when it starts).
- `MOKURO_NGINX_ACCEL=1` — put nginx in front for downloads; Python then
  listens on `MOKURO_BACKEND_PORT` (default 8081) inside the container.
- `TAKE_OWNERSHIP=true` — chown `/data` and `/config` at boot.

A compose file with Unraid paths is
[`deploy/docker-compose.unraid-cuda.yml`](../deploy/docker-compose.unraid-cuda.yml).

## Admin Setup

### First Admin User

The first browser visit to a new server opens a setup page that creates the
admin account; `mokuro-bunko setup` does the same in the console. From the
command line (it asks for the password, hidden; a password given with
`--password` would stay in the shell's history):

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

- Put a reverse proxy (nginx/Caddy) in front, and let nginx serve library
  downloads (`MOKURO_NGINX_ACCEL=1` with the repo's
  `deploy/nginx-internal.conf.template`, as the Docker images do) so large
  downloads do not hold the server's request threads.
- Raise `MOKURO_THREADS` (default 50) for many concurrent clients or
  processors.
- Use SSD storage for the database, and raise file descriptor limits for
  large libraries.
- Move OCR to a dedicated machine with a
  [remote processor](#remote-ocr-processors).
