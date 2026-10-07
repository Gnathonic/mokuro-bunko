"""Case-insensitive, case-preserving name resolution under the library.

NTFS and APFS treat `Kingdom` and `kingdom` as one name; a case-sensitive
host does not. These helpers let the server behave like the former on
either: see `middleware.path_case` for where requests are rewritten.
"""

from __future__ import annotations

import os
import unicodedata
from pathlib import Path


def fold_name(name: str) -> str:
    """The spelling-insensitive form two names collide under.

    NFC first, so a decomposed and a composed spelling collide as on APFS,
    then plain `lower()` -- the same two steps the series-identity fold
    (`normalize_volume_title_key`) applies, minus its whitespace collapsing,
    which no filesystem does to a name.
    """
    return unicodedata.normalize("NFC", name).lower()


def is_case_sensitive(path: Path) -> bool:
    """Whether the filesystem holding ``path`` tells names apart by case.

    Probed by asking for ``path`` under its own name case-swapped: only a
    case-insensitive filesystem finds the same directory there.
    """
    swapped = path.with_name(path.name.swapcase())
    # Names, not paths: `WindowsPath` equality is itself case-insensitive.
    if swapped.name == path.name:
        return True
    try:
        return not os.path.samefile(path, swapped)
    except OSError:
        return True


class LibraryPathCanonicalizer:
    """Maps a library-relative path onto the spelling already on disk."""

    def __init__(self, library_path: Path, *, case_sensitive: bool | None = None) -> None:
        self.library_path = Path(library_path)
        self._case_sensitive = (
            is_case_sensitive(self.library_path) if case_sensitive is None else case_sensitive
        )

    def _on_disk_name(self, parent: Path, name: str) -> str | None:
        """The name ``name`` is spelled with in ``parent``, or None when absent.

        The exact spelling wins over a variant (a library that already holds
        both keeps both addressable); among variants only, the first in
        sorted order, so the choice is stable.
        """
        if self._case_sensitive:
            try:
                os.lstat(parent / name)
                return name
            except OSError:
                pass
        target = fold_name(name)
        variants: list[str] = []
        try:
            with os.scandir(parent) as entries:
                for entry in entries:
                    if entry.name == name:
                        return name
                    if fold_name(entry.name) == target:
                        variants.append(entry.name)
        except OSError:
            return None
        return min(variants) if variants else None

    def canonicalize(self, relative: str, *, keep_last_if_variant_of: str | None = None) -> str:
        """``relative`` with every existing segment in its on-disk spelling.

        `keep_last_if_variant_of` is a MOVE's (already canonical) source: when
        the last segment resolves to that very entry, the client is renaming
        it and the requested spelling is kept.
        """
        parts = relative.split("/")
        if any(part in (".", "..") for part in parts):
            # Traversal is refused downstream; never resolve through it here.
            return relative
        parent = self.library_path
        resolved: list[str] = []
        # Empty parts (a trailing slash, a doubled one) are carried through
        # untouched so the request keeps its exact shape.
        named = [i for i, part in enumerate(parts) if part]
        for position, index in enumerate(named):
            part = parts[index]
            on_disk = self._on_disk_name(parent, part)
            if on_disk is None:
                break
            is_last = position == len(named) - 1
            if (
                is_last
                and keep_last_if_variant_of is not None
                and on_disk != part
                and "/".join([*resolved, on_disk]) == keep_last_if_variant_of.strip("/")
            ):
                break
            parts[index] = on_disk
            resolved.append(on_disk)
            parent = parent / on_disk
        return "/".join(parts)
