# Deployment Guide

This guide covers various deployment options for Mokuro Bunko Server.

## Quick Start

### Using pip

```bash
pip install mokuro-bunko
mokuro-bunko serve
```

### Using source checkout

```bash
git clone https://github.com/Gnathonic/mokuro-bunko.git
cd mokuro-bunko
uv sync
uv run mokuro-bunko serve
```

### Using Docker

```bash
docker build -f deploy/Dockerfile -t mokuro-bunko:local .
docker run -d \
  -p 8080:8080 \
  -v ./storage:/storage \
  mokuro-bunko:local
```

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
   }

   server {
       listen 80;
       server_name mokuro.example.com;
       return 301 https://$server_name$request_uri;
   }
   ```

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

       Keep mokuro-bunko bound to loopback/private network behind the proxy. Client IP
       forwarding headers are trusted only when requests come from local/private proxy
       peers.
  Admin API state-changing requests also enforce same-origin host checks using
  `Origin`/`Referer`; ensure your proxy preserves the external host in `Host`
  and/or `X-Forwarded-Host`.
      Admin state-changing API requests are also rate-limited per client IP.
      Rate-limit rejections return HTTP 429 with `Retry-After`,
      `X-RateLimit-Limit`, `X-RateLimit-Window`, and `X-RateLimit-Block`.

### Behind Caddy (Reverse Proxy)

Caddy automatically handles SSL certificates:

1. **Create Caddyfile**:
   ```
   mokuro.example.com {
       reverse_proxy 127.0.0.1:8080

       # WebDAV support
       @webdav {
           method PROPFIND PROPPATCH MKCOL COPY MOVE LOCK UNLOCK
       }
       handle @webdav {
           reverse_proxy 127.0.0.1:8080
       }
   }
   ```

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
   credentials-file: /home/user/.cloudflared/YOUR_TUNNEL_ID.json

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

### Docker with Cloudflare Tunnel

Use Docker Compose for a complete setup:

```yaml
version: "3.8"

services:
  mokuro-bunko:
    build:
      context: .
      dockerfile: deploy/Dockerfile
    volumes:
      - ./storage:/storage
      - ./config.yaml:/etc/mokuro-bunko/config.yaml:ro
    environment:
      - MOKURO_CONFIG=/etc/mokuro-bunko/config.yaml

  cloudflared:
    image: cloudflare/cloudflared:latest
    command: tunnel run
    environment:
      - TUNNEL_TOKEN=YOUR_TUNNEL_TOKEN
    depends_on:
      - mokuro-bunko
```

Get your tunnel token from the Cloudflare Zero Trust dashboard.

## Systemd Service

For running mokuro-bunko as a system service:

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

### Basic Docker

```bash
docker run -d \
  --name mokuro-bunko \
  -p 8080:8080 \
  -v /path/to/storage:/storage \
  -e MOKURO_REGISTRATION_MODE=invite \
  ghcr.io/xxx/mokuro-bunko
```

### Docker Compose

```yaml
version: "3.8"

services:
  mokuro-bunko:
    image: ghcr.io/xxx/mokuro-bunko
    ports:
      - "8080:8080"
    volumes:
      - mokuro-storage:/storage
    environment:
      - MOKURO_REGISTRATION_MODE=invite
      - MOKURO_OCR_BACKEND=skip
    restart: unless-stopped

volumes:
  mokuro-storage:
```

### Docker with GPU (OCR)

For NVIDIA GPU support:

```yaml
version: "3.8"

services:
  mokuro-bunko:
    image: ghcr.io/xxx/mokuro-bunko
    ports:
      - "8080:8080"
    volumes:
      - mokuro-storage:/storage
    environment:
      - MOKURO_OCR_BACKEND=cuda
    deploy:
      resources:
        reservations:
          devices:
            - driver: nvidia
              count: 1
              capabilities: [gpu]
    restart: unless-stopped

volumes:
  mokuro-storage:
```

### Unraid + NVIDIA GPU

This project includes an Unraid-focused image and template:

- `deploy/Dockerfile.unraid`
- `deploy/docker-entrypoint.unraid.sh`
- `deploy/unraid/mokuro-bunko.xml`

#### Build image manually

```bash
docker build -f deploy/Dockerfile.unraid -t mokuro-bunko:unraid-cuda .
```

#### Unraid template

1. Install the Unraid NVIDIA driver plugin (if using CUDA OCR).
2. Import `deploy/unraid/mokuro-bunko.xml` into Community Applications.
3. Confirm `Extra Parameters` includes `--runtime=nvidia`.
4. Map:
   - `/data` -> `/mnt/user/appdata/mokuro-bunko/data`
   - `/config` -> `/mnt/user/appdata/mokuro-bunko/config`
5. Set env vars:
   - `MOKURO_CONFIG=/config/config.yaml`
   - `MOKURO_OCR_BACKEND=auto` (or `cuda`)
   - `MOKURO_BUNKO_OCR_ENV=/data/.ocr-env` (persistent OCR env)
   - `NVIDIA_VISIBLE_DEVICES=all`
   - `NVIDIA_DRIVER_CAPABILITIES=compute,utility`

Optional:
- `OCR_AUTO_INSTALL=true` to run `mokuro-bunko install-ocr --backend $MOKURO_OCR_BACKEND` on startup.
- `TAKE_OWNERSHIP=true` to chown `/data` and `/config` at boot.

#### Compose example (Unraid paths)

Use `deploy/docker-compose.unraid-cuda.yml`.

## Admin Setup

### First Admin User

On first run, create an admin user via CLI:

```bash
mokuro-bunko add-user admin --role admin --password YOUR_PASSWORD
```

Or interactively:

```bash
mokuro-bunko add-user admin --role admin
# You'll be prompted for password
```

### Admin Commands

```bash
# List users
mokuro-bunko list-users

# Change user role
mokuro-bunko change-role username editor

# Generate invite code
mokuro-bunko generate-invite --role registered --expires 7d

# Delete user
mokuro-bunko delete-user username

# Approve pending user (when mode=approval)
mokuro-bunko approve-user username
```

## SSL/TLS Setup

### Self-Signed Certificate (Development)

```yaml
ssl:
  enabled: true
  auto_cert: true
```

The server generates certificates in `~/.mokuro-bunko/certs/`.

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

### OCR Not Working

1. Check OCR is installed:
   ```bash
   mokuro-bunko install-ocr --force
   ```

2. Check inbox folder permissions:
   ```bash
   ls -la /var/lib/mokuro-bunko/inbox/
   ```

3. Check logs for errors:
   ```bash
   journalctl -u mokuro-bunko -f
   ```

### Database Locked

If you see "database is locked" errors:

1. Stop all mokuro-bunko processes
2. Check for stale lock files in the storage directory
3. Ensure only one instance is running

## Performance Tuning

### For High Traffic

```yaml
server:
  host: "0.0.0.0"
  port: 8080
```

Use a reverse proxy (nginx/caddy) with:
- Connection pooling
- Gzip compression
- Caching for static assets

### For Large Libraries

- Use SSD storage for the database
- Increase file descriptor limits
- Consider separating OCR processing to a dedicated machine
