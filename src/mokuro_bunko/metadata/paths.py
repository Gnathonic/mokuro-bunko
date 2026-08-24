"""Which virtual paths carry compiled metadata (contract §1).

`<Series>/series.json` and the root `catalog.json` are METADATA files: they
live in the shared library, this server compiles them, and they must never be
confused with the per-user progress files (`volume-data.json`,
`profiles.json`) that `PathMapper` maps into a user's private directory.

A stale root `series-metadata.json` written by an older reader is inert — an
ordinary library file that nothing here looks at.

Path normalization (Task 10 review F1): the real filesystem resolver
(`security.safe_resolve_under`, built on `Path.resolve()`) collapses
duplicate separators and `.`/`..` segments before it ever compares a virtual
path to a physical one. This module used to compare the raw, un-normalized
string instead, so `Dr Stone//series.json`, `Dr Stone/./series.json`,
`./Dr Stone/series.json`, and `Dr Stone/../Dr Stone/series.json` all landed
on the exact same physical file as `Dr Stone/series.json` while failing this
module's naive match — an ordinary PUT to any of those spellings bypassed
`MetadataAPI` entirely (verified empirically; see the review). `_library_relative`
now performs the same lexical collapse (via `posixpath.normpath`, no
filesystem access) so every consumer — `is_series_file_path`,
`series_title_from_series_file_path`, `is_catalog_file_path`, and therefore
`is_compiled_metadata_path` — agrees with the resolver on every alias. A
path whose normalized form starts with `..` (escapes the library root) or
`/` (a boundary double-slash that `Path.__truediv__` treats as an absolute
override, discarding the library root entirely) matches nothing: those
spellings are exactly the ones `safe_resolve_under` also refuses to resolve
under the library, so they were never reachable as a bypass in the first
place — rejecting them here is consistency, not a new plug.
"""

from __future__ import annotations

import posixpath

from mokuro_bunko.webdav.resources import PathMapper

SERIES_FILE_NAME = "series.json"
CATALOG_FILE_NAME = "catalog.json"

_READER_PREFIX = f"/{PathMapper.READER_ROOT}/"


def _library_relative(virtual_path: str) -> str | None:
    """Library-relative part of a `/mokuro-reader/...` path, else None.

    Per-user files are excluded here, which is the partitioning rule itself:
    a path that maps into a user's private directory can never be metadata.
    The result is lexically normalized (see module docstring) so this always
    agrees with what the real path resolver would land the request on.
    """
    normalized = "/" + virtual_path.strip("/")
    if not normalized.startswith(_READER_PREFIX):
        return None
    relative = normalized[len(_READER_PREFIX):]
    if not relative:
        return None
    collapsed = posixpath.normpath(relative)
    if collapsed in (".", "") or collapsed == ".." or collapsed.startswith("../"):
        return None  # escapes the library root: never resolvable, never metadata
    if collapsed.startswith("/"):
        return None  # boundary double-slash: an absolute-path override, not an alias
    if collapsed in PathMapper.PER_USER_FILES:
        return None
    return collapsed


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
