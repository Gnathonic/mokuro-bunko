"""Configuration loading and validation for mokuro-bunko."""

from __future__ import annotations

import ipaddress
import logging
import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal

import yaml

from mokuro_bunko.ocr.generations import (
    GenerationSpec,
    default_generations,
    parse_generation_list,
)

RegistrationMode = Literal["disabled", "self", "invite", "approval"]
UserRole = Literal[
    "anonymous", "registered", "uploader", "inviter", "editor", "admin", "processor"
]
OcrBackend = Literal["auto", "cuda", "rocm", "cpu", "skip"]
DynDNSProvider = Literal["duckdns", "generic"]


def get_default_storage_path() -> Path:
    """Get the default storage path based on environment."""
    if os.name == "nt":  # Windows
        base = Path(os.environ.get("LOCALAPPDATA", Path.home() / "AppData" / "Local"))
    else:  # Linux/macOS
        base = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local" / "share"))
    return base / "mokuro-bunko"


def get_default_config_path() -> Path:
    """Get the default config path based on environment."""
    if os.name == "nt":  # Windows
        base = Path(os.environ.get("LOCALAPPDATA", Path.home() / "AppData" / "Local"))
    else:  # Linux/macOS
        base = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
    return base / "mokuro-bunko" / "config.yaml"


@dataclass
class ServerConfig:
    """Server configuration."""

    host: str = "0.0.0.0"
    port: int = 8080
    # Networks (CIDR or single addresses) of reverse proxies in front of this
    # server whose X-Real-IP / X-Forwarded-For are believed. This machine is
    # always trusted (the container's own nginx); add a proxy on another host
    # or container here, or rate limits count every client as that proxy.
    trusted_proxies: list[str] = field(default_factory=list)

    def __post_init__(self) -> None:
        # Port 0 is valid for binding to a random available port
        if not 0 <= self.port < 65536:
            raise ValueError(f"Invalid port: {self.port}")
        for network in self.trusted_proxies:
            try:
                ipaddress.ip_network(str(network), strict=False)
            except ValueError as e:
                raise ValueError(
                    f"server.trusted_proxies: {network!r} is not a network or address"
                ) from e


@dataclass
class StorageConfig:
    """Storage configuration."""

    base_path: Path = field(default_factory=get_default_storage_path)

    def __post_init__(self) -> None:
        if isinstance(self.base_path, str):
            self.base_path = Path(self.base_path)
        # Expand user home directory
        self.base_path = self.base_path.expanduser()

    @property
    def library_path(self) -> Path:
        """Path to the shared manga library."""
        return self.base_path / "library"

    @property
    def inbox_path(self) -> Path:
        """Path to the OCR upload queue."""
        return self.base_path / "inbox"

    @property
    def users_path(self) -> Path:
        """Path to per-user data."""
        return self.base_path / "users"

    def ensure_directories(self) -> None:
        """Create storage directories if they don't exist."""
        self.library_path.mkdir(parents=True, exist_ok=True)
        self.inbox_path.mkdir(parents=True, exist_ok=True)
        self.users_path.mkdir(parents=True, exist_ok=True)
        (self.library_path / "thumbnails").mkdir(exist_ok=True)


@dataclass
class RegistrationConfig:
    """Registration configuration."""

    mode: RegistrationMode = "self"
    default_role: UserRole = "registered"
    # Anonymous access controls (WebDAV):
    # - browse: PROPFIND/listing access
    # - download: GET/HEAD file download access
    allow_anonymous_browse: bool = True
    allow_anonymous_download: bool = True
    # Backward-compatibility with older configs/admin UI.
    # When true, both browse and download should require login.
    require_login: bool = False

    def __post_init__(self) -> None:
        valid_modes = ("disabled", "self", "invite", "approval")
        if self.mode not in valid_modes:
            raise ValueError(f"Invalid registration mode: {self.mode}")

        if str(self.default_role) == "writer":
            self.default_role = "uploader"

        valid_roles = ("registered", "uploader", "inviter", "editor")
        if self.default_role not in valid_roles:
            raise ValueError(
                f"Invalid default role: {self.default_role}. "
                f"Must be one of: {valid_roles}"
            )


@dataclass
class CorsConfig:
    """CORS configuration."""

    enabled: bool = True
    allowed_origins: list[str] = field(default_factory=lambda: [
        "https://reader.mokuro.app",
        "http://localhost:5173",
        "http://localhost:*",
        "http://127.0.0.1:*",
    ])
    allow_credentials: bool = True

    def is_origin_allowed(self, origin: str) -> bool:
        """Check if an origin is allowed."""
        if not self.enabled:
            return False

        for pattern in self.allowed_origins:
            if self._matches_pattern(origin, pattern):
                return True
        return False

    def _matches_pattern(self, origin: str, pattern: str) -> bool:
        """Check if origin matches pattern, supporting * wildcards for port."""
        if "*" not in pattern:
            return origin == pattern

        # Handle wildcard port matching
        if pattern.endswith(":*"):
            prefix = pattern[:-1]  # Remove the *
            if origin.startswith(prefix[:-1]):  # Remove trailing :
                # Check if what follows is a valid port
                remaining = origin[len(prefix) - 1:]
                if remaining.startswith(":"):
                    port_part = remaining[1:]
                    # Allow any port number
                    return port_part.isdigit()
        return False


@dataclass
class SslConfig:
    """SSL configuration."""

    enabled: bool = False
    auto_cert: bool = False
    cert_file: str = ""
    key_file: str = ""

    def __post_init__(self) -> None:
        if self.enabled and not self.auto_cert:
            if not self.cert_file or not self.key_file:
                raise ValueError(
                    "SSL enabled but cert_file and key_file not provided. "
                    "Either provide cert paths or set auto_cert: true"
                )


@dataclass
class AdminConfig:
    """Admin panel configuration."""

    enabled: bool = True
    path: str = "/_admin"


@dataclass
class CatalogConfig:
    """Public catalog configuration."""

    enabled: bool = False
    reader_url: str = "https://reader.mokuro.app"
    use_as_homepage: bool = False
    # Background AniList/MAL enrichment (ratings, tags, genres) for linked series.
    enrich_community: bool = True


QUEUE_DISPLAY_LEVELS = ("minimal", "normal", "detailed")


@dataclass
class QueueConfig:
    """OCR queue page configuration."""

    # Show Queue button in app header navigation.
    show_in_nav: bool = False
    # If false, queue data should only be visible to authenticated users.
    public_access: bool = True
    # How much the queue page shows, to every viewer: "minimal" (one line per
    # working machine, the pending count, speeds), "normal" (a card per
    # machine, the pending list, failures with generic reasons) or
    # "detailed" (normal plus each running volume's stage pipeline and rate
    # details, for tuning). Raw errors, paths and hardware labels are only
    # ever sent to an admin, whatever the level.
    display: str = "normal"

    def __post_init__(self) -> None:
        if self.display not in QUEUE_DISPLAY_LEVELS:
            # A display preference is not worth refusing to start over.
            logging.getLogger(__name__).warning(
                "queue.display %r is not one of %s; using 'normal'",
                self.display,
                ", ".join(QUEUE_DISPLAY_LEVELS),
            )
            self.display = "normal"


@dataclass
class DatabaseConfig:
    """Database runtime tuning (lock contention resilience)."""

    # SQLite busy_timeout: how long each statement blocks on a lock (ms).
    busy_timeout_ms: int = 5000
    # Statement/commit retries when an external process holds the write lock.
    lock_retries: int = 5
    # First retry backoff delay; doubles on each subsequent retry (seconds).
    retry_initial_delay_seconds: float = 0.05

    def __post_init__(self) -> None:
        if self.busy_timeout_ms < 100:
            raise ValueError("Database busy timeout must be at least 100 ms")
        if self.lock_retries < 1:
            raise ValueError("Database lock retries must be at least 1")
        if self.retry_initial_delay_seconds <= 0:
            raise ValueError("Database retry initial delay must be positive")


# Most OCR jobs that may run at once. One job was measured holding 2.4-3.9
# cores, so eight slots already saturate a 32-core host, and each slot is a
# separate subprocess with its own copy of the recognizer in RAM (or VRAM,
# where eight copies exceed most cards). A value above this is refused rather
# than clamped -- like every other OCR setting -- so a typo shows up at
# startup instead of forking that many OCR processes.
MAX_OCR_CONCURRENCY = 8


def _validate_concurrency(value: object) -> int:
    """Validate ``ocr.concurrency``: a whole number of slots, 1..MAX."""
    try:
        slots = int(str(value).strip())
    except (TypeError, ValueError):
        raise ValueError(f"Invalid OCR concurrency {value!r}") from None
    if slots < 1:
        raise ValueError(f"Invalid OCR concurrency: {slots} (must be at least 1)")
    if slots > MAX_OCR_CONCURRENCY:
        raise ValueError(
            f"Invalid OCR concurrency: {slots} (at most {MAX_OCR_CONCURRENCY}; "
            "one job already needs 2-4 cores and its own copy of the model)"
        )
    return slots


# Keys `ocr.generations` replaced. They are not read, not upgraded and not
# dual-written: a config that still carries one is refused, by name, rather
# than half-migrated into a list of rows nobody asked for. This area has
# never shipped, so nothing on disk depends on them.
RETIRED_OCR_KEYS: dict[str, str] = {
    "engines": "each engine becomes a generation row with its own name",
    "detector": "the detector is now per generation",
    "patch_budget": "the patch budget is now per generation",
}

# Keys whose FEATURE is gone: there is nowhere to move them to, so the only
# fix is to delete them. Refused by name for the same reason as above.
REMOVED_OCR_KEYS: dict[str, str] = {
    "char_map": (
        "the character-map system was removed (no per-character placement mode "
        "produced output worth using; readers lay characters on a uniform grid) "
        "— delete the key"
    ),
}


def _reject_retired_ocr_keys(data: dict[str, Any]) -> None:
    """Refuse a config that still uses the pre-generations or removed OCR keys."""
    gone = [key for key in REMOVED_OCR_KEYS if key in data]
    if gone:
        raise ValueError(
            "; ".join(f"ocr.{key}: {REMOVED_OCR_KEYS[key]}" for key in gone)
        )
    present = [key for key in RETIRED_OCR_KEYS if key in data]
    if not present:
        return
    named = ", ".join(f"ocr.{key}" for key in present)
    reasons = "; ".join(f"ocr.{key}: {RETIRED_OCR_KEYS[key]}" for key in present)
    raise ValueError(
        f"{named} was replaced by ocr.generations, a list of named OCR recipes "
        f"({reasons}) — rewrite the ocr section as generations, e.g. "
        "generations: [{name: mokuro, engine: mokuro, primary: true}]"
    )


@dataclass
class OcrConfig:
    """OCR configuration."""

    backend: OcrBackend = "auto"
    poll_interval: int = 30
    # How many OCR jobs the worker runs at once, each in its own subprocess
    # (two generations of one volume may run together: every sidecar is
    # stamped with the volume's own uuid, see OCRWorker.claim_next). NOT a pool size:
    # a generation's `pools` sizes the stages INSIDE one job. 1, the default,
    # is one job at a time. Takes effect at startup.
    concurrency: int = 1
    # Keep ONE runner process open per generation and stream volumes through
    # it, instead of starting one subprocess per volume. A model load costs
    # ~10.5s per (volume, generation) -- on a fast engine, longer than
    # reading a small volume -- so this is on. False is the per-volume
    # fallback: byte-identical sidecars, one process a volume, for a host
    # where a long-lived OCR process is a problem. Monolithic rows (the
    # mokuro CLI) are one invocation a volume either way. At startup.
    sessions: bool = True
    # Does the machine serving the library also run OCR? A small always-on
    # box sets this off and waits for a `mokuro-bunko processor` to log in;
    # the queue then simply holds, and says so, rather than grinding. The
    # processor registry reads it too, to decide whether this server's own
    # hardware is an entry of its own. At startup.
    local_processing: bool = True
    # A (row, processor) pair that has never been measured is benchmarked on
    # that machine before it is offered volumes, and the best widths found
    # applied to that processor's profile (spec section 4). Off means "run
    # with the auto-derived placement and bench by hand". At startup.
    autobench: bool = True
    # The named OCR recipes every library volume needs a sidecar for, IN THE
    # ORDER THE QUEUE RUNS THEM. Each row owns its engine, detector, patch
    # budget and per-stage pool sizes, and writes a file named
    # after it (`<Volume>.<name>.mokuro`, or the bare `<Volume>.mokuro` for
    # the one row flagged `primary`). Two rows may share an engine and differ
    # only in their detector -- that is the point of the list.
    generations: list[GenerationSpec] = field(default_factory=default_generations)

    def __post_init__(self) -> None:
        valid_backends = ("auto", "cuda", "rocm", "cpu", "skip")
        if self.backend not in valid_backends:
            raise ValueError(f"Invalid OCR backend: {self.backend}")
        if self.poll_interval < 1:
            raise ValueError(f"Invalid poll interval: {self.poll_interval}")
        self.concurrency = _validate_concurrency(self.concurrency)
        self.generations = parse_generation_list(self.generations)


@dataclass
class DynDNSConfig:
    """Dynamic DNS configuration."""

    enabled: bool = False
    provider: DynDNSProvider = "duckdns"
    token: str = ""
    domain: str = ""
    update_url: str = ""  # For generic provider
    interval: int = 300   # 5 minutes

    def __post_init__(self) -> None:
        valid_providers = ("duckdns", "generic")
        if self.provider not in valid_providers:
            raise ValueError(f"Invalid DynDNS provider: {self.provider}")
        if self.interval < 30:
            raise ValueError("DynDNS interval must be at least 30 seconds")


@dataclass
class Config:
    """Main configuration container."""

    server: ServerConfig = field(default_factory=ServerConfig)
    storage: StorageConfig = field(default_factory=StorageConfig)
    registration: RegistrationConfig = field(default_factory=RegistrationConfig)
    cors: CorsConfig = field(default_factory=CorsConfig)
    ssl: SslConfig = field(default_factory=SslConfig)
    admin: AdminConfig = field(default_factory=AdminConfig)
    catalog: CatalogConfig = field(default_factory=CatalogConfig)
    queue: QueueConfig = field(default_factory=QueueConfig)
    database: DatabaseConfig = field(default_factory=DatabaseConfig)
    ocr: OcrConfig = field(default_factory=OcrConfig)
    dyndns: DynDNSConfig = field(default_factory=DynDNSConfig)

    def to_dict(self) -> dict[str, Any]:
        """Convert Config to a dictionary."""
        return {
            "server": {
                "host": self.server.host,
                "port": self.server.port,
                "trusted_proxies": list(self.server.trusted_proxies),
            },
            "storage": {
                "base_path": str(self.storage.base_path),
            },
            "registration": {
                "mode": self.registration.mode,
                "default_role": self.registration.default_role,
                "allow_anonymous_browse": self.registration.allow_anonymous_browse,
                "allow_anonymous_download": self.registration.allow_anonymous_download,
                # Legacy compatibility key for older clients/tools
                "require_login": (
                    (not self.registration.allow_anonymous_browse)
                    and (not self.registration.allow_anonymous_download)
                ),
            },
            "cors": {
                "enabled": self.cors.enabled,
                "allowed_origins": self.cors.allowed_origins,
                "allow_credentials": self.cors.allow_credentials,
            },
            "ssl": {
                "enabled": self.ssl.enabled,
                "auto_cert": self.ssl.auto_cert,
                "cert_file": self.ssl.cert_file,
                "key_file": self.ssl.key_file,
            },
            "admin": {
                "enabled": self.admin.enabled,
                "path": self.admin.path,
            },
            "catalog": {
                "enabled": self.catalog.enabled,
                "reader_url": self.catalog.reader_url,
                "use_as_homepage": self.catalog.use_as_homepage,
                "enrich_community": self.catalog.enrich_community,
            },
            "queue": {
                "show_in_nav": self.queue.show_in_nav,
                "public_access": self.queue.public_access,
                "display": self.queue.display,
            },
            "database": {
                "busy_timeout_ms": self.database.busy_timeout_ms,
                "lock_retries": self.database.lock_retries,
                "retry_initial_delay_seconds": self.database.retry_initial_delay_seconds,
            },
            "ocr": {
                "backend": self.ocr.backend,
                "poll_interval": self.ocr.poll_interval,
                "concurrency": self.ocr.concurrency,
                "sessions": self.ocr.sessions,
                "local_processing": self.ocr.local_processing,
                "autobench": self.ocr.autobench,
                "generations": [row.to_dict() for row in self.ocr.generations],
            },
            "dyndns": {
                "enabled": self.dyndns.enabled,
                "provider": self.dyndns.provider,
                "token": self.dyndns.token,
                "domain": self.dyndns.domain,
                "update_url": self.dyndns.update_url,
                "interval": self.dyndns.interval,
            },
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> Config:
        """Create Config from a dictionary."""
        reg_data = dict(data.get("registration", {}))
        # Backward-compatibility migration:
        # Older configs only had `require_login`.
        if (
            "allow_anonymous_browse" not in reg_data
            and "allow_anonymous_download" not in reg_data
        ):
            require_login = bool(reg_data.get("require_login", False))
            reg_data["allow_anonymous_browse"] = not require_login
            reg_data["allow_anonymous_download"] = not require_login

        ocr_data = dict(data.get("ocr", {}))
        _reject_retired_ocr_keys(ocr_data)

        return cls(
            server=ServerConfig(**data.get("server", {})),
            storage=StorageConfig(**data.get("storage", {})),
            registration=RegistrationConfig(**reg_data),
            cors=CorsConfig(**data.get("cors", {})),
            ssl=SslConfig(**data.get("ssl", {})),
            admin=AdminConfig(**data.get("admin", {})),
            catalog=CatalogConfig(**data.get("catalog", {})),
            queue=QueueConfig(**data.get("queue", {})),
            database=DatabaseConfig(**data.get("database", {})),
            ocr=OcrConfig(**ocr_data),
            dyndns=DynDNSConfig(**data.get("dyndns", {})),
        )


def load_config(path: Path | None = None) -> Config:
    """Load configuration from YAML file.

    Args:
        path: Path to config file. If None, uses default location.
              If file doesn't exist, returns default config.

    Returns:
        Loaded configuration.

    Raises:
        ValueError: If config file has invalid values.
        yaml.YAMLError: If config file has invalid YAML syntax.
    """
    if path is None:
        path = get_default_config_path()

    if not path.exists():
        config = Config()
    else:
        with open(path) as f:
            data = yaml.safe_load(f) or {}
        config = Config.from_dict(data)

    _apply_env_overrides(config)
    return config


def save_config(config: Config, path: Path | None = None) -> None:
    """Save configuration to YAML file.

    Args:
        config: Configuration to save.
        path: Path to save to. If None, uses default location.
    """
    if path is None:
        path = get_default_config_path()

    path.parent.mkdir(parents=True, exist_ok=True)

    data = config.to_dict()

    with open(path, "w") as f:
        yaml.safe_dump(data, f, default_flow_style=False)


# Mapping of dotted config keys to their expected types
_CONFIG_TYPES: dict[str, type] = {
    "server.port": int,
    "server.host": str,
    "server.trusted_proxies": list,
    "storage.base_path": Path,
    "registration.mode": str,
    "registration.default_role": str,
    "registration.allow_anonymous_browse": bool,
    "registration.allow_anonymous_download": bool,
    "registration.require_login": bool,
    "cors.enabled": bool,
    "cors.allow_credentials": bool,
    "ssl.enabled": bool,
    "ssl.auto_cert": bool,
    "ssl.cert_file": str,
    "ssl.key_file": str,
    "admin.enabled": bool,
    "admin.path": str,
    "catalog.enabled": bool,
    "catalog.reader_url": str,
    "catalog.use_as_homepage": bool,
    "catalog.enrich_community": bool,
    "queue.show_in_nav": bool,
    "queue.public_access": bool,
    "queue.display": str,
    "ocr.backend": str,
    "ocr.poll_interval": int,
    "ocr.concurrency": int,
    "ocr.sessions": bool,
    "ocr.local_processing": bool,
    "ocr.autobench": bool,
    # A LIST OF OBJECTS, so its value is JSON rather than the comma-separated
    # form every other list key takes: `config set ocr.generations
    # '[{"name":"mokuro","engine":"mokuro","primary":true}]'`, and the same
    # text in $MOKURO_OCR_GENERATIONS for a containerised deployment.
    "ocr.generations": list,
    "dyndns.enabled": bool,
    "dyndns.provider": str,
    "dyndns.token": str,
    "dyndns.domain": str,
    "dyndns.update_url": str,
    "dyndns.interval": int,
}


def set_by_dotted_key(config: Config, key: str, value: str) -> None:
    """Set a config value by dotted key path.

    Args:
        config: Config instance to modify.
        key: Dotted key path (e.g., "server.port").
        value: String value to set (will be cast to appropriate type).

    Raises:
        KeyError: If the key path is invalid.
        ValueError: If the value cannot be cast to the expected type.
    """
    parts = key.split(".")
    if len(parts) != 2:
        raise KeyError(f"Invalid key: {key}. Expected format: section.field")

    section_name, field_name = parts

    section = getattr(config, section_name, None)
    if section is None:
        raise KeyError(f"Unknown config section: {section_name}")

    if not hasattr(section, field_name):
        raise KeyError(f"Unknown field '{field_name}' in section '{section_name}'")

    expected_type = _CONFIG_TYPES.get(key)
    if expected_type is None:
        raise KeyError(f"Unknown config key: {key}")

    if expected_type is bool:
        if value.lower() in ("true", "1", "yes"):
            typed_value: Any = True
        elif value.lower() in ("false", "0", "no"):
            typed_value = False
        else:
            raise ValueError(f"Invalid boolean value: {value}")
    elif key == "ocr.generations":
        typed_value = parse_generation_list(value)
    elif expected_type is int:
        typed_value = int(value)
    elif expected_type is Path:
        typed_value = Path(value)
    elif expected_type is list:
        typed_value = [part.strip() for part in value.split(",") if part.strip()]
    else:
        typed_value = value

    if key == "server.trusted_proxies":
        # Refused here as at load: a typo must not silently trust nothing.
        ServerConfig(trusted_proxies=typed_value)
    if key == "queue.display" and typed_value not in QUEUE_DISPLAY_LEVELS:
        raise ValueError(
            f"Invalid queue display level: {value!r} "
            f"(expected one of: {', '.join(QUEUE_DISPLAY_LEVELS)})"
        )
    setattr(section, field_name, typed_value)


def _apply_env_overrides(config: Config) -> None:
    """Apply MOKURO_* environment variable overrides to a config object."""
    # The environment carries the same retired keys a config file can, and a
    # container that still sets MOKURO_OCR_ENGINES would otherwise start with
    # the setting silently ignored.
    removed_env = [
        (f"MOKURO_OCR_{key.upper()}", reason)
        for key, reason in REMOVED_OCR_KEYS.items()
        if os.environ.get(f"MOKURO_OCR_{key.upper()}") is not None
    ]
    if removed_env:
        raise ValueError("; ".join(f"{name}: {reason}" for name, reason in removed_env))
    retired_env = [
        f"MOKURO_OCR_{key.upper()}"
        for key in RETIRED_OCR_KEYS
        if os.environ.get(f"MOKURO_OCR_{key.upper()}") is not None
    ]
    if retired_env:
        raise ValueError(
            f"{', '.join(retired_env)} was replaced by MOKURO_OCR_GENERATIONS, the JSON "
            'text of a list of named OCR recipes, e.g. \'[{"name": "mokuro", "engine": '
            '"mokuro", "primary": true}]\''
        )

    # Canonical variables, e.g. MOKURO_SERVER_HOST, MOKURO_SSL_ENABLED
    for dotted_key in _CONFIG_TYPES:
        env_key = f"MOKURO_{dotted_key.replace('.', '_').upper()}"
        env_val = os.environ.get(env_key)
        if env_val is None:
            continue
        set_by_dotted_key(config, dotted_key, env_val)

    # Backward-compatible aliases used by deployment files.
    aliases = {
        "MOKURO_HOST": "server.host",
        "MOKURO_PORT": "server.port",
        "MOKURO_STORAGE": "storage.base_path",
    }
    for env_key, dotted_key in aliases.items():
        env_val = os.environ.get(env_key)
        if env_val is None:
            continue
        set_by_dotted_key(config, dotted_key, env_val)
