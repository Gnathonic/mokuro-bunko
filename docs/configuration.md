# Configuration Reference

Mokuro Bunko Server uses a YAML configuration file. The default locations are:

- **Linux/macOS**: `~/.config/mokuro-bunko/config.yaml`
- **Windows**: `%LOCALAPPDATA%\mokuro-bunko\config.yaml`

You can also specify a custom config file path:

```bash
mokuro-bunko serve --config /path/to/config.yaml
```

Use `mokuro-bunko config check` before production deploys. It validates storage
writeability, SSL prerequisites, cert/key integrity, timeout relationships,
admin path safety, and CORS origin pattern syntax.

## Configuration Options

### Server

```yaml
server:
  host: "0.0.0.0"  # Host to bind to
  port: 8080       # Port to listen on
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `host` | string | `"0.0.0.0"` | Network interface to bind to. Use `127.0.0.1` for local-only access. |
| `port` | integer | `8080` | TCP port for the server. |

### Storage

```yaml
storage:
  base_path: "/var/lib/mokuro-bunko"
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `base_path` | string | Platform-specific | Base directory for all data storage. |

Default paths:
- **Linux/macOS**: `~/.local/share/mokuro-bunko`
- **Windows**: `%LOCALAPPDATA%\mokuro-bunko`

#### Storage Structure

```
base_path/
├── library/          # Shared manga library
│   ├── manga_title/
│   │   ├── vol1.cbz
│   │   └── vol1.mokuro.gz
│   └── thumbnails/
├── inbox/            # OCR upload queue
├── users/            # Per-user reading progress
│   ├── alice/*.json.gz
│   └── bob/*.json.gz
└── mokuro.db         # SQLite database
```

### Registration

```yaml
registration:
  mode: "self"
  default_role: "registered"
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `mode` | string | `"disabled"` | Registration mode (see below). |
| `default_role` | string | `"registered"` | Default role for new users. |

#### Registration Modes

| Mode | Description |
|------|-------------|
| `disabled` | Admin creates all accounts via CLI or web panel. |
| `self` | Open registration - anyone can create an account. |
| `invite` | Invite codes required. Generate codes via admin panel or CLI. |
| `approval` | Users can register but admin must approve accounts. |

#### User Roles

| Role | Read | Write Progress | Add Files | Modify/Delete | Admin |
|------|------|----------------|-----------|---------------|-------|
| `anonymous` | Yes | No | No | No | No |
| `registered` | Yes | Own only | No | No | No |
| `uploader` | Yes | Own only | Yes | No | No |
| `inviter` | Yes | Own only | No | No | No |
| `editor` | Yes | Own only | Yes | Yes | No |
| `admin` | Yes | All | Yes | Yes | Yes |

`inviter` can access invite-management endpoints in the admin API without full admin privileges.

### CORS

```yaml
cors:
  enabled: true
  allowed_origins:
    - "https://reader.mokuro.app"
    - "http://localhost:5173"
    - "http://localhost:*"
  allow_credentials: true
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enabled` | boolean | `true` | Enable CORS headers. |
| `allowed_origins` | list | See below | Origins allowed to access the server. |
| `allow_credentials` | boolean | `true` | Allow cookies and auth headers in cross-origin requests. |

Default allowed origins:
- `https://reader.mokuro.app`
- `http://localhost:5173`
- `http://localhost:*`
- `http://127.0.0.1:*`

The `*` wildcard matches any port number (e.g., `http://localhost:*` matches `http://localhost:3000`).

### SSL/TLS

```yaml
ssl:
  enabled: false
  auto_cert: false
  cert_file: ""
  key_file: ""
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enabled` | boolean | `false` | Enable HTTPS. |
| `auto_cert` | boolean | `false` | Auto-generate self-signed certificate. |
| `cert_file` | string | `""` | Path to SSL certificate file (PEM format). |
| `key_file` | string | `""` | Path to SSL private key file (PEM format). |

#### SSL Modes

1. **Disabled** (default): Server runs on HTTP only.

2. **Auto-generated certificate**: Server generates a self-signed certificate.
   ```yaml
   ssl:
     enabled: true
     auto_cert: true
   ```
   Certificates are stored in `~/.mokuro-bunko/certs/`.

3. **Custom certificate**: Use your own certificates (e.g., from Let's Encrypt).
   ```yaml
   ssl:
     enabled: true
     cert_file: "/path/to/cert.pem"
     key_file: "/path/to/key.pem"
   ```

### Admin Panel

```yaml
admin:
  enabled: true
  path: "/_admin"
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enabled` | boolean | `true` | Enable the admin web panel. |
| `path` | string | `"/_admin"` | URL path for the admin panel. |

Access the admin panel at `http://your-server:8080/_admin/` (requires admin credentials).

### Queue

```yaml
queue:
  show_in_nav: false
  public_access: true
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `show_in_nav` | boolean | `false` | Show an OCR Queue button in the top navigation bar. |
| `public_access` | boolean | `true` | If `false`, queue status API access requires authenticated user credentials. |

### Database

```yaml
database:
  connect_timeout_seconds: 30
  busy_timeout_ms: 5000
  connect_retries: 5
  retry_initial_delay_seconds: 0.05
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `connect_timeout_seconds` | integer | `30` | SQLite connect timeout for DB open operations. |
| `busy_timeout_ms` | integer | `5000` | SQLite busy timeout while waiting for DB locks. |
| `connect_retries` | integer | `5` | Retries for transient `database is locked` open failures. |
| `retry_initial_delay_seconds` | float | `0.05` | Initial exponential backoff delay for DB retry logic. |

### OCR

```yaml
ocr:
  backend: auto
  poll_interval: 30
  hard_timeout_seconds: 3600
  no_progress_timeout_seconds: 600
  finalizing_timeout_seconds: 180
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `backend` | string | `"auto"` | OCR processing backend. |
| `poll_interval` | integer | `30` | Seconds between library scans for missing OCR assets. |
| `hard_timeout_seconds` | integer | `3600` | Maximum allowed runtime for one mokuro process. |
| `no_progress_timeout_seconds` | integer | `600` | Abort OCR if no page progress is detected within this window. |
| `finalizing_timeout_seconds` | integer | `180` | Abort prolonged post-processing/finalizing phase. |

#### OCR Backends

| Backend | Description |
|---------|-------------|
| `auto` | Automatically select the best backend supported by this host/runtime. |
| `cuda` | NVIDIA GPU with CUDA support. |
| `rocm` | AMD GPU with ROCm support. |
| `cpu` | CPU-only processing (slower). |
| `skip` | Disable OCR, WebDAV server only. |

Install OCR dependencies with:

```bash
mokuro-bunko install-ocr
```

Inspect which backends are valid on the current machine/runtime:

```bash
mokuro-bunko install-ocr --list-backends
```

Queue OCR APIs:
- `GET /queue/api/ocr` returns current worker/progress state plus recent OCR history.
  - `progress` may include `error`, `started_at`, and `updated_at` fields when available.
  - Supports the same optional history query filters as `/queue/api/ocr/history`.
- `GET /queue/api/ocr/history` returns recent OCR events persisted in storage.
  - Optional query: `?limit=1..500` (default `50`).
  - Optional filters: `status=done|error`, `series=<substring>`, `since=<unix_timestamp_seconds>`.
  - History retention is bounded to the most recent 500 events on disk.
- `POST /queue/api/ocr/control` supports `{ "action": "pause" }` and `{ "action": "resume" }` for admin users.

## Environment Variables

Configuration options can also be set via environment variables with the `MOKURO_` prefix:

```bash
MOKURO_SERVER_HOST=127.0.0.1
MOKURO_SERVER_PORT=9000
MOKURO_REGISTRATION_MODE=invite
MOKURO_SSL_ENABLED=true
```

Environment variables override config file values.

## Example Configurations

### Local Development

```yaml
server:
  host: "127.0.0.1"
  port: 8080

registration:
  mode: "self"

cors:
  enabled: true
  allowed_origins:
    - "http://localhost:*"

ocr:
  backend: "skip"
```

### Production (Behind Reverse Proxy)

```yaml
server:
  host: "127.0.0.1"
  port: 8080

storage:
  base_path: "/var/lib/mokuro-bunko"

registration:
  mode: "invite"
  default_role: "registered"

cors:
  enabled: true
  allowed_origins:
    - "https://reader.mokuro.app"
    - "https://your-domain.com"

ssl:
  enabled: false  # Handled by reverse proxy

admin:
  enabled: true

ocr:
  backend: "cuda"
  poll_interval: 60
```

When using reverse proxy headers (`X-Forwarded-For`, `X-Real-IP`), run mokuro-bunko
behind a local/private proxy hop (for example `127.0.0.1` or a private subnet).
Forwarded client IP headers from untrusted public peers are ignored by default.

### Public Read-Only Server

```yaml
server:
  host: "0.0.0.0"
  port: 8080

registration:
  mode: "disabled"

cors:
  enabled: true
  allow_credentials: false

admin:
  enabled: false

ocr:
  backend: "skip"
```

Note: `queue.public_access` only controls read access to queue status endpoints.
OCR worker control endpoints (`/queue/api/ocr/control`) always require an
authenticated admin user.
