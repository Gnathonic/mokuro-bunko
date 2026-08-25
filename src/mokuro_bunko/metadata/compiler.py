"""Turn a series folder on disk into the reader's volume index (contract §2).

One entry per `.cbz`: an archive is what a reader can download, and a sidecar
without one is not a volume (the same rule `library_index.py` applies). The
series title is the FOLDER name and the volume title is the archive's stem —
never the `.mokuro`'s own `title`/`volume`, which real-world files get wrong
and which the reader matches against `.cbz` filenames anyway.

Parsing every `.mokuro` on every regeneration would mean re-reading gigabytes
on a large library, so each compiled entry is cached against the stat of the
files it came from; a regeneration that changes nothing is a stat walk.
"""

from __future__ import annotations

import gzip
import json
import os
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any

from mokuro_bunko.metadata.reader_compat import (
    count_page_chars,
    deterministic_uuid,
    natural_sort_key,
    normalize_volume_title_key,
)
from mokuro_bunko.metadata.schema import VolumeEntry

if TYPE_CHECKING:
    from mokuro_bunko.database import Database

_IMAGE_SUFFIXES = frozenset(
    {".jpg", ".jpeg", ".png", ".gif", ".bmp", ".webp", ".tiff", ".tif"}
)


@dataclass(frozen=True)
class SeriesFolder:
    """A top-level library folder that holds at least one archive."""

    title: str
    path: Path


def volume_key_for(series_title: str, volume_title: str) -> str:
    """Library-relative key of a volume's archive — the entry cache's key."""
    return f"{series_title}/{volume_title}.cbz"


def iter_series_folders(library_path: Path) -> list[SeriesFolder]:
    """Top-level folders holding at least one `.cbz`, sorted by name.

    Deliberately NOT `LibraryIndexCache`: that is a 30-second TTL cache and it
    indexes nested folders at any depth, while a regeneration must see the
    filesystem as it is right now and only the one level the reader treats as
    a series.
    """
    folders: list[SeriesFolder] = []
    try:
        entries = sorted(os.scandir(library_path), key=lambda item: item.name)
    except OSError:
        return []
    for entry in entries:
        if entry.name.startswith(".") or not entry.is_dir(follow_symlinks=True):
            continue
        if _has_archive(Path(entry.path)):
            folders.append(SeriesFolder(title=entry.name, path=Path(entry.path)))
    return folders


def _has_archive(folder: Path) -> bool:
    try:
        with os.scandir(folder) as scan:
            return any(
                item.is_file(follow_symlinks=True) and item.name.lower().endswith(".cbz")
                for item in scan
            )
    except OSError:
        return False


def _archive_names(folder: Path) -> list[str]:
    try:
        with os.scandir(folder) as scan:
            return sorted(
                item.name
                for item in scan
                if item.is_file(follow_symlinks=True) and item.name.lower().endswith(".cbz")
            )
    except OSError:
        return []


def _sidecar_for(cbz_path: Path) -> Path | None:
    """`<stem>.mokuro`, else `<stem>.mokuro.gz`, else None."""
    base = cbz_path.with_suffix("")
    for suffix in (".mokuro", ".mokuro.gz"):
        candidate = Path(f"{base}{suffix}")
        if candidate.is_file():
            return candidate
    return None


def _stat_key(path: Path | None) -> str:
    """Compact identity of a sidecar for cache validation ("" = none)."""
    if path is None:
        return ""
    try:
        stat_result = path.stat()
    except OSError:
        return ""
    return f"{path.name}:{stat_result.st_size}:{stat_result.st_mtime}"


def _read_sidecar(path: Path) -> dict[str, Any] | None:
    """Parse a `.mokuro`/`.mokuro.gz`; None when unreadable or not an object."""
    try:
        if path.name.lower().endswith(".gz"):
            with gzip.open(path, "rt", encoding="utf-8") as handle:
                data = json.load(handle)
        else:
            with path.open("r", encoding="utf-8") as handle:
                data = json.load(handle)
    except (OSError, UnicodeDecodeError, ValueError, EOFError):
        return None
    return data if isinstance(data, dict) else None


def _count_archive_images(cbz_path: Path) -> int:
    """Page count for an image-only volume: images inside the archive."""
    try:
        with zipfile.ZipFile(cbz_path, "r") as archive:
            return sum(
                1
                for name in archive.namelist()
                if Path(name).suffix.lower() in _IMAGE_SUFFIXES
            )
    except (zipfile.BadZipFile, OSError, EOFError):
        return 0


def _positive_number(value: Any) -> float | None:
    """A usable positive `spine_width`, returned UNCOERCED.

    Mirrors `validate.py`'s `_is_offset`: the reader passes `spine_width`
    through untouched from whatever produced the `.mokuro` (it is not a
    computed pixel measurement — see the type comment in the reader's
    `src/lib/types/index.ts`), so a whole-number value must survive as a
    Python `int` and serialize as `250`, never widened to `250.0` by a
    `float()` call here. PEP 484's numeric tower keeps this `int | float`
    compatible with the declared `float | None` return type.
    """
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return value if value > 0 else None


def _compile_volume(series_title: str, cbz_path: Path, sidecar: Path | None) -> VolumeEntry:
    volume_title = cbz_path.with_suffix("").name
    data = _read_sidecar(sidecar) if sidecar is not None else None

    try:
        archive_size = cbz_path.stat().st_size
    except OSError:
        archive_size = 0

    if data is None:
        # Image-only (or an unreadable sidecar): the reader derives this uuid
        # for its placeholder, so deriving the same one keeps synced progress
        # attached when the index arrives.
        return VolumeEntry(
            volume_uuid=deterministic_uuid(f"{series_title}/{volume_title}"),
            volume_title=volume_title,
            page_count=_count_archive_images(cbz_path),
            character_count=0,
            mokuro_version="",
            archive_size=archive_size or None,
        )

    pages = data.get("pages")
    raw_uuid = data.get("volume_uuid")
    uuid = (
        raw_uuid
        if isinstance(raw_uuid, str) and raw_uuid.strip()
        else deterministic_uuid(f"{series_title}/{volume_title}")
    )
    raw_version = data.get("version")
    version = raw_version if isinstance(raw_version, str) else ""

    # Upstream `.mokuro` files carry no `chars` key (only files the reader
    # itself wrote do), so counting is the normal path, not the fallback.
    raw_chars = data.get("chars")
    if isinstance(raw_chars, int) and not isinstance(raw_chars, bool) and raw_chars > 0:
        character_count = raw_chars
    else:
        character_count = count_page_chars(pages)

    return VolumeEntry(
        volume_uuid=uuid,
        volume_title=volume_title,
        page_count=len(pages) if isinstance(pages, list) else _count_archive_images(cbz_path),
        character_count=character_count,
        mokuro_version=version,
        spine_width=_positive_number(data.get("spine_width")),
        archive_size=archive_size or None,
    )


def _entry_to_dict(entry: VolumeEntry) -> dict[str, Any]:
    return {
        "volume_uuid": entry.volume_uuid,
        "volume_title": entry.volume_title,
        "page_count": entry.page_count,
        "character_count": entry.character_count,
        "mokuro_version": entry.mokuro_version,
        "spine_width": entry.spine_width,
        "archive_size": entry.archive_size,
    }


def _entry_from_dict(raw: dict[str, Any]) -> VolumeEntry | None:
    try:
        return VolumeEntry(
            volume_uuid=str(raw["volume_uuid"]),
            volume_title=str(raw["volume_title"]),
            page_count=int(raw["page_count"]),
            character_count=int(raw["character_count"]),
            mokuro_version=str(raw["mokuro_version"]),
            spine_width=raw.get("spine_width"),
            archive_size=raw.get("archive_size"),
        )
    except (KeyError, TypeError, ValueError):
        return None


def compile_series_volumes(
    series: SeriesFolder,
    *,
    database: Database | None = None,
) -> list[VolumeEntry]:
    """Every volume of one series, in natural title order.

    `series_key` here feeds only `series_entry_cache.series_key` — an
    informational column nothing currently queries by (every lookup filters
    on `volume_key`), but it IS indexed (`database.py`'s
    `idx_series_entry_cache_series`), and it is now the one remaining spot
    that fed a `*_key`-named column with the bare fold. Task 11 review
    round 3 (F16): aligned to `normalize_volume_title_key`, the same fold
    `metadata/service.py` keys `series_facts` with, so a future query
    against this column can't silently disagree with every other identity
    site in the codebase.
    """
    series_key = normalize_volume_title_key(series.title)
    entries: list[VolumeEntry] = []

    for name in _archive_names(series.path):
        cbz_path = series.path / name
        volume_title = cbz_path.with_suffix("").name
        sidecar = _sidecar_for(cbz_path)
        sidecar_key = _stat_key(sidecar)
        try:
            cbz_stat = cbz_path.stat()
        except OSError:
            continue

        key = volume_key_for(series.title, volume_title)
        entry: VolumeEntry | None = None
        if database is not None:
            cached = database.get_cached_volume_entry(
                key, cbz_stat.st_size, cbz_stat.st_mtime, sidecar_key
            )
            if cached is not None:
                entry = _entry_from_dict(cached)

        if entry is None:
            entry = _compile_volume(series.title, cbz_path, sidecar)
            if database is not None:
                database.put_cached_volume_entry(
                    key,
                    series_key,
                    _entry_to_dict(entry),
                    cbz_stat.st_size,
                    cbz_stat.st_mtime,
                    sidecar_key,
                )
        entries.append(entry)

    # Tiebreak on the raw title, matching `dump_series_file`'s own sort:
    # `natural_sort_key` is a TOTAL PREORDER (its own docstring), so distinct
    # titles can tie, and without a secondary key this function's own claimed
    # order would rest only on `_archive_names`'s implicit tie-free input
    # rather than being a self-contained guarantee.
    entries.sort(key=lambda item: (natural_sort_key(item.volume_title), item.volume_title))
    return entries
