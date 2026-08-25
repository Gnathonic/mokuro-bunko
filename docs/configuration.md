# Configuration Reference

Mokuro Bunko Server uses a YAML configuration file. The default locations are:

- **Linux/macOS**: `~/.config/mokuro-bunko/config.yaml`
- **Windows**: `%LOCALAPPDATA%\mokuro-bunko\config.yaml`

You can also specify a custom config file path:

```bash
mokuro-bunko serve --config /path/to/config.yaml
```

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

### OCR

```yaml
ocr:
  backend: auto
  poll_interval: 30
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `backend` | string | `"auto"` | OCR processing backend. |
| `poll_interval` | integer | `30` | Seconds between library scans for missing OCR assets. |

#### OCR Backends

| Backend | Description |
|---------|-------------|
| `auto` | Automatically select the best backend supported by this host/runtime. |
| `cuda` | NVIDIA GPU with CUDA support. |
| `rocm` | AMD GPU with ROCm support. |
| `cpu` | CPU-only processing (slower). |
| `skip` | Disable OCR, WebDAV server only. |

Cover thumbnails (`<Volume>.webp`, generated from each archive's first page) are
produced regardless of the backend — including `skip` — because readers use them
for volumes they have not downloaded. Only the OCR sidecar generation follows the
`backend` setting.

Install OCR dependencies with:

```bash
mokuro-bunko install-ocr
```

Inspect which backends are valid on the current machine/runtime:

```bash
mokuro-bunko install-ocr --list-backends
```

## Compiled metadata files

The server compiles two files into the shared library and keeps them current:

| File | Contents |
| --- | --- |
| `<Series>/series.json` | The series' facts (external ids, titles, synonyms, tag, unit) plus an index of its volumes: uuid, title, page and character counts, mokuro version, spine width, archive size, freshness stamps and shelf offsets. |
| `catalog.json` (library root) | One entry per series folder with the same facts — name, mapping and search data only. |

Both are regenerated when the library changes and whenever a client submits an
update, and are rewritten only when their content actually changed, so clients can
cache them on size/mtime.

Each volume entry may also carry `mokuro_size`/`mokuro_modified` and
`cover_size`/`cover_modified`: the byte size and integer epoch-second mtime of the
`.mokuro` sidecar and the cover `.webp`, taken from a plain filesystem stat when the
entry is compiled. Either pair is omitted (never `null`) when its file doesn't
exist. A client uses these to decide whether its own cached copy is stale without
downloading anything: rebuild when the stamped size differs from what it has, or
the stamped `_modified` is strictly newer than what it stored; an older-or-equal
`_modified` at an equal size is fresh. Stamps are always whole seconds, never
sub-second, because a generic WebDAV client only ever sees second-precision
`Last-Modified` HTTP dates.

Clients do not write these files. A `PUT` of `<Series>/series.json` is accepted as
an update *request*: the facts are validated and merged (newest stamp wins), the
volume list in the request is ignored in favour of the server's own compilation,
and both files are regenerated. A body carrying only facts, with no volume list at
all, is an equally valid update. Writing `catalog.json`, or deleting/moving either
file, is refused for every account. Submitting an update is ownership-gated, not a
plain progress-write permission: an editor-tier account (or above) may update any
series, an uploader account only a series it uploaded, and a registered-only
account cannot submit updates at all. The account that submitted an accepted
update is recorded in the audit log.

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
