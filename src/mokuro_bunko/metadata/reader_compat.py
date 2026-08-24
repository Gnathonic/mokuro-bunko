"""Ports of the reader client's pure helpers, kept byte-identical.

Compiled files have to be indistinguishable from the ones the client writes,
so these mirror their TypeScript originals exactly rather than approximating
them. Each function names the source it was ported from; change one only
together with the other side.
"""

from __future__ import annotations

import bisect
import re
import time
import unicodedata
from datetime import datetime, timezone
from typing import Any

# Ported from `src/lib/util/count-chars.ts`. The client's character class is
# `[○◯々-〇〻ぁ-ゖゝ-ゞァ-ヺー\p{Script=Hiragana}\p{Script=Katakana}\p{Script=Han}]`;
# Python's `re` has no `\p{Script=...}`, so the class was expanded to its
# codepoint ranges by evaluating the regex over every codepoint under Node and
# recording the runs. Regenerate the table the same way if the class changes.
_COUNTED_RANGES: tuple[tuple[int, int], ...] = (
    (0x25CB, 0x25CB), (0x25EF, 0x25EF), (0x2E80, 0x2E99), (0x2E9B, 0x2EF3),
    (0x2F00, 0x2FD5), (0x3005, 0x3007), (0x3021, 0x3029), (0x3038, 0x303B),
    (0x3041, 0x3096), (0x309D, 0x309F), (0x30A1, 0x30FA), (0x30FC, 0x30FF),
    (0x31F0, 0x31FF), (0x32D0, 0x32FE), (0x3300, 0x3357), (0x3400, 0x4DBF),
    (0x4E00, 0x9FFF), (0xF900, 0xFA6D), (0xFA70, 0xFAD9), (0xFF66, 0xFF6F),
    (0xFF71, 0xFF9D), (0x16FE2, 0x16FE3), (0x16FF0, 0x16FF6), (0x1AFF0, 0x1AFF3),
    (0x1AFF5, 0x1AFFB), (0x1AFFD, 0x1AFFE), (0x1B000, 0x1B122), (0x1B132, 0x1B132),
    (0x1B150, 0x1B152), (0x1B155, 0x1B155), (0x1B164, 0x1B167), (0x1F200, 0x1F200),
    (0x20000, 0x2A6DF), (0x2A700, 0x2B81D), (0x2B820, 0x2CEAD), (0x2CEB0, 0x2EBE0),
    (0x2EBF0, 0x2EE5D), (0x2F800, 0x2FA1D), (0x30000, 0x3134A), (0x31350, 0x33479),
)
_RANGE_STARTS = tuple(start for start, _ in _COUNTED_RANGES)
_RANGE_ENDS = tuple(end for _, end in _COUNTED_RANGES)

FUTURE_TOLERANCE_SECONDS = 5 * 60


def _is_counted(char: str) -> bool:
    code_point = ord(char)
    index = bisect.bisect_right(_RANGE_STARTS, code_point) - 1
    return index >= 0 and code_point <= _RANGE_ENDS[index]


def count_chars(text: str) -> int:
    """Japanese characters in one OCR line (client: `countChars`)."""
    return sum(1 for char in text if _is_counted(char))


def count_page_chars(pages: Any) -> int:
    """Total over every line of every block (client: `getCharCount`).

    Defensive on shape: a `.mokuro` is foreign data that may have been
    hand-edited, and one malformed block must not fail a whole library scan.
    """
    total = 0
    if not isinstance(pages, list):
        return 0
    for page in pages:
        if not isinstance(page, dict):
            continue
        blocks = page.get("blocks")
        if not isinstance(blocks, list):
            continue
        for block in blocks:
            if not isinstance(block, dict):
                continue
            lines = block.get("lines")
            if not isinstance(lines, list):
                continue
            for line in lines:
                if isinstance(line, str):
                    total += count_chars(line)
    return total


def _to_int32(value: int) -> int:
    value &= 0xFFFFFFFF
    return value - 0x100000000 if value >= 0x80000000 else value


def _utf16_units(text: str) -> list[int]:
    """JS `charCodeAt` semantics: UTF-16 code units, not code points."""
    data = text.encode("utf-16-le", "surrogatepass")
    return [data[i] | (data[i + 1] << 8) for i in range(0, len(data), 2)]


def deterministic_uuid(value: str) -> str:
    """Port of the client's `generateDeterministicUUID` (djb2-xor pair).

    Image-only volumes have no `.mokuro` and therefore no real uuid; the
    client derives a placeholder uuid from `"<Series>/<Volume>"` with this
    function, so bunko MUST derive the same one or synced progress recorded
    against the placeholder would strand when the index arrives.

    The shape it produces is 8-4-4-4-8, not a real UUID — that is what the
    client emits, quirks (`hex2[5:8]` skipping a digit) included.
    """
    normalized = value.lower().strip()
    hash1 = 5381
    hash2 = 52711
    for unit in _utf16_units(normalized):
        hash1 = _to_int32(hash1 * 33) ^ unit
        hash2 = _to_int32(hash2 * 33) ^ unit
    hash1 &= 0xFFFFFFFF
    hash2 &= 0xFFFFFFFF
    hex1 = f"{hash1:08x}"
    hex2 = f"{hash2:08x}"
    hash3 = f"{(hash1 ^ hash2) & 0xFFFFFFFF:08x}"
    hash4 = f"{(hash1 + hash2) & 0xFFFFFFFF:08x}"
    variant = f"{8 + (int(hash3[0], 16) % 4):x}"
    return f"{hex1}-{hex2[:4]}-4{hex2[5:8]}-{variant}{hash3[1:4]}-{hash3[4:]}{hash4[:4]}"


def normalize_series_key(title: str) -> str:
    """Client: `normalizeSeriesKey` — trim, collapse whitespace, lowercase."""
    return re.sub(r"\s+", " ", title.strip()).lower()


def normalize_volume_title_key(title: str) -> str:
    """Client: `normalizeVolumeTitleKey` — the series fold plus NFC.

    A filename that round-tripped through a filesystem can come back
    decomposed while the JSON beside it stays composed: byte-different, same
    title.
    """
    return normalize_series_key(unicodedata.normalize("NFC", title))


_DIGIT_RUN = re.compile(r"(\d+)")


def natural_sort_key(title: str) -> tuple[tuple[int, object], ...]:
    """Order volume titles the way the client's collator does.

    The client sorts with `Intl.Collator(undefined, {numeric: true,
    sensitivity: 'base'})`. Full ICU parity is not reachable from the stdlib
    and is not needed: nothing downstream depends on the file's order (readers
    re-sort on read). What IS required is that the order be TOTAL and STABLE,
    so a rebuild that changed nothing produces the same bytes and therefore the
    same size/mtime (contract §3/§4). Digit runs compare numerically and sort
    before letters; text compares case- and accent-folded.
    """
    key: list[tuple[int, object]] = []
    for part in _DIGIT_RUN.split(title):
        if not part:
            continue
        if part.isdigit():
            key.append((0, int(part)))
        else:
            folded = "".join(
                char
                for char in unicodedata.normalize("NFKD", part)
                if not unicodedata.combining(char)
            ).casefold()
            key.append((1, folded))
    return tuple(key)


def normalize_updated_at(value: object, now: float | None = None) -> str | None:
    """Client: `normalizeUpdatedAt` — comparable ISO string, or None.

    `updated_at` decides merges by lexicographic comparison, so a non-ISO
    string ("Aug 16 2020" sorts above every ISO date) or a far-future value
    would win against every honest timestamp forever. Unparsable -> None
    (caller drops the payload); more than five minutes ahead -> clamped.
    """
    if not isinstance(value, str):
        return None
    text = value.strip()
    if not text:
        return None
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    seconds = parsed.timestamp()
    reference = time.time() if now is None else now
    if seconds > reference + FUTURE_TOLERANCE_SECONDS:
        seconds = reference
    return iso_stamp(seconds)


def iso_stamp(seconds: float) -> str:
    """`Date.prototype.toISOString()` shape: UTC, exactly 3 decimals, `Z`."""
    moment = datetime.fromtimestamp(seconds, tz=timezone.utc)
    return moment.strftime("%Y-%m-%dT%H:%M:%S.") + f"{moment.microsecond // 1000:03d}Z"
