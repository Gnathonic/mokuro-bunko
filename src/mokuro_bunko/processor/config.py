"""``processor.yaml``: the whole configuration of a processor.

Deliberately tiny. A processor owns no library, no users and no settings
the library server owns -- it owns where to log in, what to call itself,
how many pipelines its hardware can hold, and which OCR backend to build.
Everything else about a job arrives in an op.
"""

from __future__ import annotations

import os
import socket
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import yaml

VALID_BACKENDS = ("auto", "cuda", "rocm", "cpu")

# The spool's default RAM budget (`processor.archives`), spelled here too so
# loading a config never imports the spool.
DEFAULT_ARCHIVE_MEMORY_MB = 2048


class ProcessorConfigError(ValueError):
    """A processor configuration that cannot be acted on, named by key."""


# The library stores at most this much of a processor's name (and public
# name): `ocr.remote.registry.MAX_PROCESSOR_NAME`.
MAX_PUBLIC_NAME = 64


def default_storage_path() -> Path:
    """Where a processor keeps its storage unless ``processor.storage`` says.

    ``%LOCALAPPDATA%`` on Windows (``~/AppData/Local`` when it is unset),
    ``$XDG_DATA_HOME`` (``~/.local/share``) everywhere else.
    """
    if sys.platform == "win32":
        local = os.environ.get("LOCALAPPDATA")
        base = Path(local) if local else Path.home() / "AppData" / "Local"
    else:
        base = Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local" / "share")
    return base / "mokuro-bunko-processor"


@dataclass
class LibrarySettings:
    url: str
    username: str
    password: str
    # True, False, or a path to the library's certificate.
    tls_verify: bool | str = True


@dataclass
class ProcessorSettings:
    name: str = field(default_factory=socket.gethostname)
    # What the library's queue page shows VISITORS for this machine. Unset,
    # they see a numbered alias ("machine 2"); admins always see `name`.
    public_name: str | None = None
    max_sessions: int = 1
    storage: Path = field(default_factory=default_storage_path)
    # RAM the queued archives may use -- the volume in the runner and the one
    # on deck, across all sessions -- as unnamed files in /dev/shm. An
    # archive that does not fit goes to `storage` instead. 0 = always disk.
    archive_memory_mb: int = DEFAULT_ARCHIVE_MEMORY_MB


@dataclass
class ProcessorOcr:
    backend: str = "auto"


@dataclass
class ProcessorConfig:
    library: LibrarySettings
    processor: ProcessorSettings = field(default_factory=ProcessorSettings)
    ocr: ProcessorOcr = field(default_factory=ProcessorOcr)


def _section(data: dict[str, Any], name: str, allowed: tuple[str, ...]) -> dict[str, Any]:
    raw = data.get(name) or {}
    if not isinstance(raw, dict):
        raise ProcessorConfigError(f"{name}: must be a block of settings")
    unknown = sorted(set(raw) - set(allowed))
    if unknown:
        raise ProcessorConfigError(
            f"{name}.{unknown[0]}: no such setting (expected one of {', '.join(allowed)})"
        )
    return raw


def load_processor_config(path: Path) -> ProcessorConfig:
    """Read and validate one ``processor.yaml``."""
    try:
        data = yaml.safe_load(Path(path).read_text(encoding="utf-8")) or {}
    except OSError as e:
        raise ProcessorConfigError(f"could not read {path}: {e}") from e
    except yaml.YAMLError as e:
        raise ProcessorConfigError(f"{path} is not valid YAML: {e}") from e
    if not isinstance(data, dict):
        raise ProcessorConfigError(f"{path} must be a block of settings")
    unknown = sorted(set(data) - {"library", "processor", "ocr"})
    if unknown:
        raise ProcessorConfigError(f"{unknown[0]}: no such section")

    library = _section(data, "library", ("url", "username", "password", "tls_verify"))
    for key in ("url", "username", "password"):
        if not str(library.get(key) or "").strip():
            raise ProcessorConfigError(
                f"library.{key} is required: a processor logs in like any client"
            )
    verify = library.get("tls_verify", True)
    if not isinstance(verify, (bool, str)):
        raise ProcessorConfigError(
            "library.tls_verify: true, false, or the path to the library's certificate"
        )

    processor = _section(
        data,
        "processor",
        ("name", "public_name", "max_sessions", "storage", "archive_memory_mb"),
    )
    public_name = processor.get("public_name")
    if public_name is not None:
        if not isinstance(public_name, str):
            raise ProcessorConfigError("processor.public_name: text")
        public_name = public_name.strip()
        if len(public_name) > MAX_PUBLIC_NAME:
            raise ProcessorConfigError(
                f"processor.public_name: at most {MAX_PUBLIC_NAME} characters"
            )
        public_name = public_name or None
    try:
        sessions = int(processor.get("max_sessions", 1))
    except (TypeError, ValueError):
        raise ProcessorConfigError("processor.max_sessions: a whole number") from None
    if sessions < 1:
        raise ProcessorConfigError("processor.max_sessions: at least 1")

    archive_memory_mb = processor.get("archive_memory_mb", DEFAULT_ARCHIVE_MEMORY_MB)
    if (
        isinstance(archive_memory_mb, bool)
        or not isinstance(archive_memory_mb, int)
        or archive_memory_mb < 0
    ):
        raise ProcessorConfigError(
            "processor.archive_memory_mb: a whole number of megabytes, 0 or more "
            "(0 keeps every archive on disk)"
        )

    ocr = _section(data, "ocr", ("backend",))
    backend = str(ocr.get("backend", "auto"))
    if backend not in VALID_BACKENDS:
        raise ProcessorConfigError(
            f"ocr.backend: {backend!r} is not one of {', '.join(VALID_BACKENDS)}"
        )

    storage = processor.get("storage")
    return ProcessorConfig(
        library=LibrarySettings(
            url=str(library["url"]).rstrip("/"),
            username=str(library["username"]),
            password=str(library["password"]),
            tls_verify=verify,
        ),
        processor=ProcessorSettings(
            name=str(processor.get("name") or socket.gethostname()),
            public_name=public_name,
            max_sessions=sessions,
            storage=Path(storage).expanduser() if storage else default_storage_path(),
            archive_memory_mb=archive_memory_mb,
        ),
        ocr=ProcessorOcr(backend=backend),
    )
