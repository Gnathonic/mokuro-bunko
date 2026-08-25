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
`is_compiled_metadata_path` — agrees with the resolver on every alias.

Final whole-branch review, F1: the Task 10 fix above normalized only the
tail AFTER the `/mokuro-reader/` prefix test, not the whole path, on the
theory that a library-relative part beginning with `/` (a boundary
double-slash spelling, e.g. `/mokuro-reader//catalog.json`) was "never
reachable as a bypass" because `safe_resolve_under` also refuses to resolve
it. That theory was empirically false: wsgidav's own path resolution for a
PUT to a non-existent resource (`get_uri_parent`/`get_uri_name`, then
`PathMapper.get_resource_inst`) does `"/" + path.strip("/")` on the PARENT
path before `safe_resolve_under` is ever consulted, which silently absorbs
the extra slash and lands the write on the real, un-prefixed file —
`safe_resolve_under` is never asked. Reproduced: `PUT
/mokuro-reader//catalog.json` wrote the raw compiled catalog for every
ADD_FILES role, bypassing `MetadataAPI` interception and the compiled-file
verb gate entirely, with no merge, no validation and no audit event. The fix
is to normalize the FULL virtual path (lexically, via `posixpath.normpath`)
BEFORE the `/mokuro-reader/` prefix test, not only the relative remainder
after it — so `//catalog.json`, `///catalog.json` and `//Dr Stone/series.json`
now collapse onto the same real path the resolver would land on, and are
recognized like any other alias. A path whose fully-normalized form still
does not start with the reader prefix (a genuine `..`-escape past the
library root, or a spelling that never reaches it) matches nothing — those
spellings are exactly the ones `safe_resolve_under` also refuses to resolve
under the library, so they remain unreachable as a bypass.
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
    The FULL path is lexically normalized first (final review F1 — see the
    module docstring) so a boundary double-slash like `/mokuro-reader//x`
    collapses onto the same real path the resolver would land on, exactly
    like every other alias, instead of being treated as an absolute-path
    override that discards the reader prefix.
    """
    normalized = posixpath.normpath("/" + virtual_path.strip("/"))
    if not normalized.startswith(_READER_PREFIX):
        return None  # includes any `..`-escape past the library root
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
