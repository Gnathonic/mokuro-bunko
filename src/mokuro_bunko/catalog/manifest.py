"""The per-volume manifest a reader deep link points at.

One JSON document naming every file the reader should fetch for one volume:
its archive, its primary OCR sidecar, each extra OCR layer, its cover and the
series file, each with its size and modification time. A reader given the
manifest stops guessing file names from the archive's stem.

Built from ONE directory listing of the series folder (``os.scandir``); only
the files it names are stat'ed, through their own directory entries.

A file belongs to the LONGEST archive stem it starts with plus ``.``:
``Vol 1.5.mokuro`` is the primary OCR of ``Vol 1.5`` when ``Vol 1.5.cbz`` is
there, and a layer ``5`` of ``Vol 1`` only when it is not -- the rule
``LibraryIndexCache`` and the reader already follow.
"""

from __future__ import annotations

import os
import urllib.parse
from collections.abc import Iterable
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from mokuro_bunko.ocr.generations import split_layer_sidecar
from mokuro_bunko.webdav.resources import PathMapper

MANIFEST_VERSION = 1
SERIES_FILE_NAME = "series.json"

# What `encodeURIComponent` leaves unescaped beyond `quote`'s own
# `A-Z a-z 0-9 _ . - ~`: the catalog builds the archive link with it, one
# path segment at a time, and a manifest URL must match that link exactly.
_URI_COMPONENT_SAFE = "!*'()"


def reader_file_url(series: str, file_name: str) -> str:
    """``/mokuro-reader/<series>/<file>``, each segment escaped as the catalog does."""
    return (
        f"/{PathMapper.READER_ROOT}/{_encode_component(series)}/{_encode_component(file_name)}"
    )


def manifest_url(series: str, volume: str) -> str:
    """The absolute-path URL of a volume's manifest, as the catalog's link builds it."""
    return (
        f"/catalog/api/manifest?series={_encode_component(series)}"
        f"&volume={_encode_component(volume)}"
    )


def _encode_component(value: str) -> str:
    return urllib.parse.quote(value.encode("utf-8", "surrogateescape"), safe=_URI_COMPONENT_SAFE)


def _file_entry(series: str, entry: os.DirEntry[str]) -> dict[str, Any]:
    stat = entry.stat()
    return {
        "url": reader_file_url(series, entry.name),
        "size": stat.st_size,
        "modified": datetime.fromtimestamp(stat.st_mtime, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    }


def _is_file(entry: os.DirEntry[str] | None) -> bool:
    if entry is None:
        return False
    try:
        return entry.is_file()
    except OSError:
        return False


def build_volume_manifest(
    series_dir: Path,
    series: str,
    volume: str,
    layer_order: Iterable[str] = (),
) -> dict[str, Any] | None:
    """The manifest for ``<series_dir>/<volume>.cbz``, or None when there is no such archive.

    ``series`` is the name the URLs are built with (the folder's path under the
    library root, as the catalog knows it). ``layer_order`` is the configured
    generation order: layers it names come first, in its order, and the rest
    follow alphabetically.

    The caller has already decided that the request may read the archive and
    that ``series_dir`` is inside the library.
    """
    try:
        with os.scandir(series_dir) as listing:
            entries = {entry.name: entry for entry in listing}
    except OSError:
        return None

    archive = entries.get(f"{volume}.cbz")
    if not _is_file(archive):
        return None
    assert archive is not None

    # Every archive stem longer than this one that would also claim a name
    # starting with ``<volume>.`` -- such a name is that volume's, not ours.
    prefix = f"{volume}."
    longer = [
        name[: -len(".cbz")]
        for name in entries
        if name.casefold().endswith(".cbz") and name.startswith(prefix) and name != archive.name
    ]

    plain_ocr: os.DirEntry[str] | None = None
    gz_ocr: os.DirEntry[str] | None = None
    cover: os.DirEntry[str] | None = None
    layers: dict[str, os.DirEntry[str]] = {}

    for name, entry in entries.items():
        if not name.startswith(prefix) or entry is archive:
            continue
        if any(name.startswith(f"{stem}.") for stem in longer):
            continue
        rest = name[len(prefix):]
        if rest == "mokuro":
            plain_ocr = entry
        elif rest == "mokuro.gz":
            gz_ocr = entry
        elif rest == "webp":
            cover = entry
        else:
            split = split_layer_sidecar(name)
            if split is None or split[0] != volume:
                continue
            layer_id = split[1]
            # Plain beats `.gz` for the same id.
            if layer_id not in layers or name.endswith(".mokuro"):
                layers[layer_id] = entry

    layers = {layer_id: entry for layer_id, entry in layers.items() if _is_file(entry)}
    ordered: list[str] = []
    for layer_id in layer_order:
        if layer_id in layers and layer_id not in ordered:
            ordered.append(layer_id)
    ordered.extend(sorted(layer_id for layer_id in layers if layer_id not in ordered))

    ocr = plain_ocr if _is_file(plain_ocr) else gz_ocr if _is_file(gz_ocr) else None
    series_file = entries.get(SERIES_FILE_NAME)

    return {
        "version": MANIFEST_VERSION,
        "series": series,
        "volume": volume,
        "archive": _file_entry(series, archive),
        "ocr": _file_entry(series, ocr) if ocr is not None else None,
        "layers": [
            {"id": layer_id, **_file_entry(series, layers[layer_id])} for layer_id in ordered
        ],
        "cover": _file_entry(series, cover) if cover is not None and _is_file(cover) else None,
        "series_file": (
            _file_entry(series, series_file) if series_file is not None and _is_file(series_file) else None
        ),
    }
