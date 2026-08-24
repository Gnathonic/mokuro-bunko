"""Which virtual paths carry compiled metadata (contract §1).

`<Series>/series.json` and the root `catalog.json` are METADATA files: they
live in the shared library, this server compiles them, and they must never be
confused with the per-user progress files (`volume-data.json`,
`profiles.json`) that `PathMapper` maps into a user's private directory.

A stale root `series-metadata.json` written by an older reader is inert — an
ordinary library file that nothing here looks at.
"""

from __future__ import annotations

from mokuro_bunko.webdav.resources import PathMapper

SERIES_FILE_NAME = "series.json"
CATALOG_FILE_NAME = "catalog.json"

_READER_PREFIX = f"/{PathMapper.READER_ROOT}/"


def _library_relative(virtual_path: str) -> str | None:
    """Library-relative part of a `/mokuro-reader/...` path, else None.

    Per-user files are excluded here, which is the partitioning rule itself:
    a path that maps into a user's private directory can never be metadata.
    """
    normalized = "/" + virtual_path.strip("/")
    if not normalized.startswith(_READER_PREFIX):
        return None
    relative = normalized[len(_READER_PREFIX):]
    if not relative or relative in PathMapper.PER_USER_FILES:
        return None
    return relative


def is_catalog_file_path(virtual_path: str) -> bool:
    """True for the ROOT catalog.json only; a nested one is somebody else's file."""
    relative = _library_relative(virtual_path)
    return relative is not None and relative.lower() == CATALOG_FILE_NAME


def series_title_from_series_file_path(virtual_path: str) -> str | None:
    """`/mokuro-reader/<Series>/series.json` -> `<Series>`, else None.

    Exactly one folder level: the reader stores one sidecar per series folder,
    and a deeper path is not a series the catalog knows about.
    """
    relative = _library_relative(virtual_path)
    if relative is None:
        return None
    head, separator, tail = relative.rpartition("/")
    if not separator or tail.lower() != SERIES_FILE_NAME:
        return None
    if "/" in head or not head.strip():
        return None
    return head


def is_series_file_path(virtual_path: str) -> bool:
    """True for `<Series>/series.json` directly under the reader root."""
    return series_title_from_series_file_path(virtual_path) is not None


def is_compiled_metadata_path(virtual_path: str) -> bool:
    """Any file this server compiles, and therefore owns."""
    return is_catalog_file_path(virtual_path) or is_series_file_path(virtual_path)
