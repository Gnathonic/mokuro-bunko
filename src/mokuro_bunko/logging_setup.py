"""Central logging configuration for mokuro-bunko.

The server historically wrote status via bare ``print()`` calls and left the
:mod:`logging` module unconfigured, so nothing was ever persisted. This module
gives every run a rotating log file under ``<storage>/logs/server.log`` while
keeping console output for interactive use.
"""

from __future__ import annotations

import logging
import sys
from logging.handlers import RotatingFileHandler
from pathlib import Path

LOG_DIR_NAME = "logs"
SERVER_LOG_NAME = "server.log"

_FILE_FORMAT = "%(asctime)s %(levelname)s [%(name)s] %(message)s"
_CONSOLE_FORMAT = "%(levelname)s [%(name)s] %(message)s"


def get_log_dir(storage_path: Path) -> Path:
    """Return the log directory for a storage root (not created)."""
    return storage_path / LOG_DIR_NAME


def get_ocr_log_dir(storage_path: Path) -> Path:
    """Return the per-volume OCR log directory (not created)."""
    return storage_path / LOG_DIR_NAME / "ocr"


def setup_logging(storage_path: Path, verbose: bool = False) -> Path | None:
    """Configure root logging with console + rotating file handlers.

    Safe to call more than once: handlers installed by this function are
    replaced rather than duplicated.

    Args:
        storage_path: Storage root; the log file lives in ``<storage>/logs``.
        verbose: When True, log DEBUG to the console (file always gets INFO+).

    Returns:
        Path to the server log file, or None if the file handler could not
        be created (logging still works on the console).
    """
    root = logging.getLogger()
    root.setLevel(logging.DEBUG)

    # Remove handlers we previously installed (idempotent reconfiguration).
    for handler in list(root.handlers):
        if getattr(handler, "_mokuro_bunko", False):
            root.removeHandler(handler)
            handler.close()

    console = logging.StreamHandler(stream=sys.stdout)
    console.setLevel(logging.DEBUG if verbose else logging.INFO)
    console.setFormatter(logging.Formatter(_CONSOLE_FORMAT))
    console._mokuro_bunko = True  # type: ignore[attr-defined]
    root.addHandler(console)

    log_file: Path | None = None
    try:
        log_dir = get_log_dir(storage_path)
        log_dir.mkdir(parents=True, exist_ok=True)
        log_file = log_dir / SERVER_LOG_NAME
        file_handler = RotatingFileHandler(
            log_file,
            maxBytes=2 * 1024 * 1024,
            backupCount=5,
            encoding="utf-8",
            delay=True,
        )
        file_handler.setLevel(logging.INFO)
        file_handler.setFormatter(logging.Formatter(_FILE_FORMAT))
        file_handler._mokuro_bunko = True  # type: ignore[attr-defined]
        root.addHandler(file_handler)
    except OSError as e:
        logging.getLogger(__name__).warning(
            "Could not create log file under %s: %s (console logging only)",
            storage_path,
            e,
        )

    # Quiet noisy third-party loggers; our own modules stay at INFO.
    for noisy in ("wsgidav", "cheroot", "urllib3"):
        logging.getLogger(noisy).setLevel(logging.WARNING)

    return log_file
