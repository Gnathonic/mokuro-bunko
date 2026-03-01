"""Configuration loading and validation for mokuro-bunko."""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, Optional

import yaml


RegistrationMode = Literal["disabled", "self", "invite", "approval"]
UserRole = Literal["anonymous", "registered", "uploader", "inviter", "editor", "admin"]
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

    def __post_init__(self) -> None:
        # Port 0 is valid for binding to a random available port
        if not 0 <= self.port < 65536:
            raise ValueError(f"Invalid port: {self.port}")


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

        if self.default_role == "writer":
            self.default_role = "uploader"  # type: ignore[assignment]

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


@dataclass
class QueueConfig:
    """OCR queue page configuration."""

    # Show Queue button in app header navigation.
    show_in_nav: bool = False
    # If false, queue data should only be visible to authenticated users.
    public_access: bool = True


@dataclass
class OcrConfig:
    """OCR configuration."""

    backend: OcrBackend = "auto"
    poll_interval: int = 30

    def __post_init__(self) -> None:
        valid_backends = ("auto", "cuda", "rocm", "cpu", "skip")
        if self.backend not in valid_backends:
            raise ValueError(f"Invalid OCR backend: {self.backend}")
        if self.poll_interval < 1:
            raise ValueError(f"Invalid poll interval: {self.poll_interval}")


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
            raise ValueError(f"DynDNS interval must be at least 30 seconds")


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
    ocr: OcrConfig = field(default_factory=OcrConfig)
    dyndns: DynDNSConfig = field(default_factory=DynDNSConfig)

    def to_dict(self) -> dict[str, Any]:
        """Convert Config to a dictionary."""
        return {
            "server": {
                "host": self.server.host,
                "port": self.server.port,
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
            },
            "queue": {
                "show_in_nav": self.queue.show_in_nav,
                "public_access": self.queue.public_access,
            },
            "ocr": {
                "backend": self.ocr.backend,
                "poll_interval": self.ocr.poll_interval,
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

        return cls(
            server=ServerConfig(**data.get("server", {})),
            storage=StorageConfig(**data.get("storage", {})),
            registration=RegistrationConfig(**reg_data),
            cors=CorsConfig(**data.get("cors", {})),
            ssl=SslConfig(**data.get("ssl", {})),
            admin=AdminConfig(**data.get("admin", {})),
            catalog=CatalogConfig(**data.get("catalog", {})),
            queue=QueueConfig(**data.get("queue", {})),
            ocr=OcrConfig(**data.get("ocr", {})),
            dyndns=DynDNSConfig(**data.get("dyndns", {})),
        )


def load_config(path: Optional[Path] = None) -> Config:
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


def save_config(config: Config, path: Optional[Path] = None) -> None:
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
    "queue.show_in_nav": bool,
    "queue.public_access": bool,
    "ocr.backend": str,
    "ocr.poll_interval": int,
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
    elif expected_type is int:
        typed_value = int(value)
    elif expected_type is Path:
        typed_value = Path(value)
    else:
        typed_value = value

    setattr(section, field_name, typed_value)


def _apply_env_overrides(config: Config) -> None:
    """Apply MOKURO_* environment variable overrides to a config object."""
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
