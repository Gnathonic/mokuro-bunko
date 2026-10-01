"""Build the golden fixture library and record what Python 0.5.2 makes of it.

    ~/.cache/mokuro-bunko-demo/ref052/bin/python crates/bunko-library/tests/golden/gen_library.py

1. (Re)builds `library/` next to this script: synthetic `.cbz` archives (some
   hand-crafted byte by byte: prepended data, duplicate names, CP437 names,
   Info-ZIP Unicode Path fields, NUL-truncated names, a damaged archive),
   sidecars (synthetic, and real ones from ~/Downloads, read-only), covers,
   layers and hostile names (NFD/NFC, decimal volumes, dots, spaces,
   Japanese, colliding series keys). Writes `mtimes.json` (git does not keep
   mtimes; the Rust test re-applies them).
2. Copies the library to a temporary directory, applies the mtimes, and runs
   0.5.2 there: a cold full pass, a sequence of client PUTs, a final full pass.
   Records every compiled file after every step, the database tables, the
   library index snapshot, every volume manifest, and the archive page lists.

Outputs go to `expected/`. The Rust test (`tests/golden_library.rs`) replays
the same steps and asserts byte-identical results.
"""

from __future__ import annotations

import gzip
import hashlib
import json
import os
import shutil
import sqlite3
import struct
import sys
import tempfile
import unicodedata
import zipfile
import zlib
from pathlib import Path

sys.path.append(str(Path.home() / ".cache/mokuro-bunko-demo/mokuro-env/lib/python3.12/site-packages"))

import natsort  # noqa: E402,F401  (reading_order must use natsort, as in production)

from mokuro_bunko.catalog.manifest import build_volume_manifest  # noqa: E402
from mokuro_bunko.database import Database  # noqa: E402
from mokuro_bunko.library_index import LibraryIndexCache  # noqa: E402
from mokuro_bunko.metadata.compiler import _archive_image_names  # noqa: E402
from mokuro_bunko.metadata.service import MetadataService  # noqa: E402
from mokuro_bunko.ocr.engine_runner import ArchiveReader, list_pages  # noqa: E402
from mokuro_bunko.ocr.generations import sidecar_siblings  # noqa: E402

HERE = Path(__file__).resolve().parent
LIBRARY = HERE / "library"
EXPECTED = HERE / "expected"
DOWNLOADS = Path.home() / "Downloads"

JP = "".join(chr(c) for c in (0x9032, 0x6483, 0x306E, 0x5DE8, 0x4EBA))
NFD_SERIES = "Shinjuku a" + chr(0x301) + " Zombie"
NFC_SERIES = unicodedata.normalize("NFC", NFD_SERIES)
WOLF = "A story about a wolf and a hunter"
BASE_MTIME = 1_700_000_000
COMPILED_MTIME_NS = 1_800_000_000_500_000_000


def img(name: str) -> bytes:
    return b"IMG:" + name.encode("utf-8") * 3


def write_zip(path: Path, members, compression=zipfile.ZIP_DEFLATED) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w", compression=compression) as zf:
        for member in members:
            if isinstance(member, tuple):
                name, data = member
            else:
                name, data = member, img(member)
            info = zipfile.ZipInfo(name, date_time=(2024, 1, 2, 3, 4, 6))
            info.compress_type = compression
            zf.writestr(info, data)


def raw_zip(path: Path, entries, prepend: bytes = b"", comment: bytes = b"") -> None:
    """A zip written byte by byte. entry: dict(name=bytes, data=bytes, method=0|8,
    flags=int, extra=bytes (central), local_name=bytes|None)."""
    out = bytearray(prepend)
    central = bytearray()
    for entry in entries:
        name = entry["name"]
        data = entry["data"]
        method = entry.get("method", 8)
        flags = entry.get("flags", 0)
        crc = zlib.crc32(data)
        if method == 8:
            compressor = zlib.compressobj(6, zlib.DEFLATED, -15)
            payload = compressor.compress(data) + compressor.flush()
        else:
            payload = data
        local_name = entry.get("local_name", name)
        offset = len(out) - len(prepend)  # offsets relative to the zip start (prepended data = "concat")
        out += struct.pack("<4s5H3L2H", b"PK\x03\x04", 20, flags, method, 0x6000, 0x5821, crc,
                           len(payload), len(data), len(local_name), 0)
        out += local_name + payload
        extra = entry.get("extra", b"")
        central += struct.pack("<4s4B4HL2L5H2L", b"PK\x01\x02", 20, 3, 20, 0, flags, method, 0x6000, 0x5821,
                               crc, len(payload), len(data), len(name), len(extra), 0, 0, 0, 0o100644 << 16, offset)
        central += name + extra
    cd_offset = len(out) - len(prepend)
    out += central
    out += struct.pack("<4s4H2LH", b"PK\x05\x06", 0, 0, len(entries), len(entries), len(central), cd_offset, len(comment))
    out += comment
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(bytes(out))


def unicode_path_extra(raw_name: bytes, real_name: str) -> bytes:
    body = struct.pack("<BL", 1, zlib.crc32(raw_name)) + real_name.encode("utf-8")
    return struct.pack("<HH", 0x7075, len(body)) + body


def mokuro(pages, *, uuid=None, version="0.2.2", chars=None, extra=None) -> dict:
    doc = {"version": version, "title": "t", "title_uuid": "x", "volume": "v"}
    if uuid is not None:
        doc["volume_uuid"] = uuid
    doc["pages"] = pages
    if chars is not None:
        doc["chars"] = chars
    if extra:
        doc.update(extra)
    return doc


def page(img_path, lines=("テスト", "abc")):
    return {"version": "0.2.2", "img_width": 100, "img_height": 200, "img_path": img_path,
            "blocks": [{"box": [0, 0, 1, 1], "vertical": True, "font_size": 12, "lines_coords": [], "lines": list(lines)}]}


def dumps(doc) -> bytes:
    return json.dumps(doc, ensure_ascii=False).encode("utf-8")


def build_library() -> None:
    if LIBRARY.exists():
        shutil.rmtree(LIBRARY)
    LIBRARY.mkdir()

    # --- Dr Stone: plain series, decimal volume, layer, uppercase extension.
    ds = LIBRARY / "Dr Stone"
    p01 = [f"Dr Stone 01/p{n:03d}.jpg" for n in range(1, 6)]
    write_zip(ds / "Dr Stone 01.cbz", p01 + ["Dr Stone 01.webp", "__MACOSX/._p001.jpg", "Dr Stone 01/Thumbs.db"])
    (ds / "Dr Stone 01.mokuro").write_bytes(dumps(mokuro([page(p) for p in p01], uuid="cfb5220c-57db-4008-9f44-e659d794e381")))
    (ds / "Dr Stone 01.webp").write_bytes(b"RIFFcover01")
    p02 = [f"p{n}.png" for n in range(1, 11)]
    write_zip(ds / "Dr Stone 02.cbz", [p for p in p02 if p != "p7.png"], compression=zipfile.ZIP_STORED)
    (ds / "Dr Stone 02.mokuro.gz").write_bytes(gzip.compress(dumps(mokuro(
        [page(p) for p in p02], uuid="u-02", extra={"spine_width": 250})), mtime=0))
    (ds / "Dr Stone 02.hayai-nova.mokuro").write_bytes(dumps(mokuro([page(p) for p in p02], uuid="u-02")))
    (ds / "Dr Stone 02.ppocr-manga.mokuro.gz").write_bytes(gzip.compress(b"{}", mtime=0))
    p025 = [f"x/{n}.webp" for n in range(1, 4)]
    write_zip(ds / "Dr Stone 02.5.cbz", p025)
    (ds / "Dr Stone 02.5.mokuro").write_bytes(dumps(mokuro([page(p) for p in p025], uuid="u-025", extra={"spine_width": 33.25})))
    write_zip(ds / "Dr Stone 03.CBZ", ["a.jpg", "b.jpg"])
    (ds / "Dr Stone 03.mokuro").write_bytes(dumps(mokuro([page("a.jpg"), page("b.jpg")], uuid="u-03", chars=7)))
    write_zip(ds / "Dr Stone 10.cbz", ["10/1.jpg", "10/2.jpg", "10/10.jpg", "10/notes.txt"])
    (ds / "Dr Stone 10.nocover").write_bytes(b"")
    (ds / "notes.txt").write_bytes(b"not a volume")

    # --- A second folder folding to the same series key as "Dr Stone".
    write_zip(LIBRARY / "Dr  stone" / "Extra.cbz", ["e1.jpg"])

    # --- NFD folder name; lenient JSON; multi-member gzip; a damaged archive.
    nfd = LIBRARY / NFD_SERIES
    v1 = [f"{n:02d}.jpg" for n in range(1, 4)]
    write_zip(nfd / "Vol 1.cbz", v1)
    lenient = ('{"version": "0.2.2", "volume_uuid": "first", "volume_uuid": "nfd-v1", "spine_width": NaN, '
               '"pages": [' + ", ".join(json.dumps(page(p)) for p in v1) + ']}')
    (nfd / "Vol 1.mokuro").write_bytes(lenient.encode("utf-8"))
    v15 = ["a/1.jpg", "a/2.jpg"]
    write_zip(nfd / "Vol 1.5.cbz", v15)
    body = dumps(mokuro([page(p) for p in v15], uuid="nfd-v15", extra={"spine_width": 12}))
    (nfd / "Vol 1.5.mokuro.gz").write_bytes(gzip.compress(body[:20], mtime=0) + gzip.compress(body[20:], mtime=0) + b"\0\0\0")
    (nfd / "Vol 2.cbz").write_bytes(b"PK\x03\x04 this is not really a zip archive")
    (nfd / "Vol 2.mokuro").write_bytes(dumps(mokuro([page("1.jpg"), page("2.jpg")], uuid="nfd-v2")))
    (nfd / "Vol 2.webp").write_bytes(b"RIFFcoverv2")

    # --- Real sidecars (read-only copies from ~/Downloads).
    wolf = LIBRARY / WOLF
    for number, mode in (("2", "exact"), ("3", "stem"), ("4.1", "positional")):
        source = DOWNLOADS / f"{WOLF} {number}.mokuro"
        raw = source.read_bytes()
        doc = json.loads(raw)
        paths = [p["img_path"] for p in doc["pages"]]
        if mode == "exact":
            members = paths
            (wolf / f"{WOLF} {number}.mokuro").parent.mkdir(parents=True, exist_ok=True)
            (wolf / f"{WOLF} {number}.mokuro").write_bytes(raw)
        elif mode == "stem":
            members = [p.rsplit(".", 1)[0] + ".jpg" for p in paths][:-2]
            (wolf / f"{WOLF} {number}.mokuro.gz").write_bytes(gzip.compress(raw, mtime=0))
        else:
            members = [f"renamed/page_{i:03d}.png" for i in range(len(paths))]
            (wolf / f"{WOLF} {number}.mokuro.gz").write_bytes(gzip.compress(raw, mtime=0))
        write_zip(wolf / f"{WOLF} {number}.cbz", members)
    (wolf / f"{WOLF} 2.webp").write_bytes(b"RIFFwolf2")

    # --- Japanese names; unreadable and odd sidecars.
    jp = LIBRARY / JP
    write_zip(jp / f"{JP} 01.cbz", [f"{JP}/{n}.jpg" for n in range(1, 4)])
    (jp / f"{JP} 01.mokuro").write_bytes(b"\xef\xbb\xbf" + dumps(mokuro([page("1.jpg")], uuid="bom")))
    write_zip(jp / f"{JP} 02.cbz", ["1.jpg", "2.jpg"])
    (jp / f"{JP} 02.mokuro").write_bytes(b"[]")
    write_zip(jp / f"{JP} 03.cbz", ["1.jpg", "2.jpg", "3.jpg"])
    (jp / f"{JP} 03.mokuro").write_bytes(dumps(mokuro([{"blocks": []}, {"img_path": ""}, {"img_path": 5}],
                                                      uuid="  ", version=3, chars=1234567890123)))
    (jp / f"{JP} 03.webp").write_bytes(b"RIFFjp3")

    # --- Crafted archives: dots in names, every zip oddity.
    odd = LIBRARY / "Odd.Name. With.Dots"
    cp437_name = b"caf\x82/\x8e.jpg"  # "café/Ä.jpg" in CP437
    raw_zip(odd / "v.1.cbz", [
        {"name": b"pages/", "data": b"", "method": 0},
        {"name": b"pages/p2.jpg", "data": img("p2-first")},
        {"name": b"pages/p10.jpg", "data": img("p10"), "method": 0},
        {"name": b"pages/p1.jpg", "data": img("p1")},
        {"name": b"pages/p2.jpg", "data": img("p2-second")},
        {"name": b"pages/./p3.jpg", "data": img("p3-dot")},
        {"name": b"pages//p3.jpg", "data": img("p3-slash")},
        {"name": b"v.1.webp", "data": img("embedded cover")},
        {"name": b"__MACOSX/pages/._p1.jpg", "data": b"junk"},
        {"name": b"pages\\p4.jpg", "data": img("backslash")},
        {"name": cp437_name, "data": img("cp437")},
        {"name": b"orig\x00ignored.jpg", "data": img("nul")},
        {"name": b"legacy_name.jpg", "data": img("unicode path"), "extra": unicode_path_extra(b"legacy_name.jpg", JP + ".jpg")},
        {"name": "日本/p5.PNG".encode("utf-8"), "data": img("utf8"), "flags": 0x800},
        {"name": b"pages/p6.avif", "data": img("avif")},
        {"name": b"pages/p7.gif", "data": img("gif")},
        {"name": b"chapter.1/page001", "data": img("noext")},
        {"name": b"pages/p8.jpg.bak", "data": b"bak"},
    ], prepend=b"#!/bin/sh\necho self-extracting\n", comment=b"crafted")
    (odd / "v.1.mokuro").write_bytes(dumps(mokuro(
        [page("pages/p1.jpg"), page("pages/p2.png"), page("pages/p10.jpg"), page("missing.jpg"), {"img_path": None}],
        uuid="odd-v1")))
    (odd / "v.1.webp").write_bytes(b"RIFFoddcover")
    raw_zip(odd / "v.2.cbz", [{"name": b"x.jpg", "data": b"x" * 50, "method": 8}], comment=b"PK\x05\x06 fake eocd in comment")
    (odd / "v.1.hayai-nova.mokuro").write_bytes(b"{}")
    (odd / "v.1.Upper.mokuro").write_bytes(b"{}")

    # --- Not series: nested, hidden, empty, sidecar-only, a root-level archive.
    write_zip(LIBRARY / "Nested" / "Inner" / "x.cbz", ["1.jpg"])
    (LIBRARY / "Nested" / "Inner" / "x.webp").write_bytes(b"RIFFx")
    write_zip(LIBRARY / ".hidden" / "h.cbz", ["1.jpg"])
    (LIBRARY / "Empty").mkdir()
    (LIBRARY / "Empty" / ".keep").write_bytes(b"")
    (LIBRARY / "sidecar-only").mkdir()
    (LIBRARY / "sidecar-only" / "orphan.mokuro").write_bytes(dumps(mokuro([page("1.jpg")])))
    write_zip(LIBRARY / "loose.cbz", ["1.jpg"])

    # --- mtimes: deterministic, some with sub-second parts.
    mtimes = {}
    files = sorted(p for p in LIBRARY.rglob("*") if p.is_file())
    for index, path in enumerate(files):
        rel = path.relative_to(LIBRARY).as_posix()
        seconds = BASE_MTIME + index * 1000
        nanos = 0 if index % 3 == 0 else (index * 123_456_789) % 1_000_000_000
        mtimes[rel] = [seconds, nanos]
    (HERE / "mtimes.json").write_text(json.dumps(mtimes, ensure_ascii=True, indent=1, sort_keys=True), encoding="ascii")


def apply_mtimes(root: Path) -> None:
    mtimes = json.loads((HERE / "mtimes.json").read_text(encoding="ascii"))
    for rel, (seconds, nanos) in mtimes.items():
        ns = seconds * 1_000_000_000 + nanos
        os.utime(root / rel, ns=(ns, ns))


PUTS = [
    ("Dr Stone", {"version": 2, "updated_at": "2026-08-18T19:36:24.324Z", "external_ids": {"anilist": 98416, "mal": 103897},
                  "titles": {"native": "Dr.STONE", "romaji": "Dr. STONE"}, "synonyms": [JP], "tag": "HD Scan",
                  "unit": "volumes", "spine_offset": 12.5,
                  "volumes": [{"volume_uuid": "cfb5220c-57db-4008-9f44-e659d794e381", "offset": -40}]}, "alice"),
    (NFC_SERIES, {"version": 2, "updated_at": "2026-01-01T00:00:00Z", "titles": {"english": "Zombie"},
                  "spine_offset": 12.0, "volumes": [{"volume_uuid": "nfd-v1", "offset": 2.5}]}, "bob"),
    ("Dr Stone", None, "alice"),  # None = repeat the first payload byte for byte
    ("dr stone", {"version": 1, "updated_at": "2020-01-01T00:00:00Z",
                  "volumes": [{"volume_uuid": "cfb5220c-57db-4008-9f44-e659d794e381"}]}, "carol"),
    ("Nope", {"version": 2, "updated_at": "2026-01-01T00:00:00Z"}, "dave"),
    (WOLF, {"version": 2, "updated_at": "2026-03-01T00:00:00+09:00", "external_ids": {"mal": 5}, "unit": "chapters-ish",
            "synonyms": ["  ", "Ookami"]}, "erin"),
    (JP, "{not json", "frank"),
    ("Odd.Name. With.Dots", {"version": 2, "updated_at": "2026-05-05T05:05:05.555555Z", "spine_offset": 10 ** 30,
                             "volumes": [{"volume_uuid": "odd-v1", "offset": 10 ** 30}]}, "gina"),
    (NFC_SERIES, {"version": 2, "updated_at": "2025-01-01T00:00:00Z"}, None),
]


def payload_bytes(payload) -> bytes:
    if isinstance(payload, str):
        return payload.encode("utf-8")
    return json.dumps(payload, ensure_ascii=False).encode("utf-8")


def compiled_files(root: Path) -> dict[str, str]:
    out = {}
    for path in sorted(root.rglob("*.json")):
        out[path.relative_to(root).as_posix()] = path.read_text(encoding="utf-8")
    return out


def dump_db(db_path: Path) -> dict:
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    tables = {
        "series_facts": ("series_key", ["series_key", "series_title", "external_ids", "titles", "synonyms", "tag", "unit",
                                        "facts_updated_at", "spine_offset", "volume_offsets", "updated_by"]),
        "series_entry_cache": ("volume_key", ["volume_key", "series_key", "entry_json", "cbz_size", "cbz_mtime", "sidecar_key"]),
        "catalog_series": ("series_key", ["series_key", "folder_name", "cover_path", "volume_count", "latest_volume_modified",
                                          "total_pages", "total_chars", "missing_pages", "damaged_volumes"]),
        "volume_identities": ("volume_key", ["volume_key", "volume_uuid"]),
    }
    out = {}
    for table, (key, columns) in tables.items():
        rows = conn.execute(f"SELECT {', '.join(columns)} FROM {table} ORDER BY {key}").fetchall()
        out[table] = [[{"type": type(row[c]).__name__, "value": row[c] if not isinstance(row[c], float) else repr(row[c])}
                       for c in columns] for row in rows]
    schema = [r[0] for r in conn.execute("SELECT sql FROM sqlite_master WHERE type='table' AND name IN "
                                          "('series_facts','series_entry_cache','catalog_series','volume_identities')")]
    conn.close()
    out["schema"] = schema
    return out


def snapshot_json(snapshot) -> dict:
    return {
        "series": [{"name": s.name, "cover": s.cover, "volumes": [
            {"name": v.name, "has_cbz": v.has_cbz, "has_mokuro": v.has_mokuro, "has_mokuro_gz": v.has_mokuro_gz,
             "cover": v.cover, "sidecars": list(v.sidecars)} for v in s.volumes]} for s in snapshot.series],
        "pending_ocr": [list(p) for p in snapshot.pending_ocr],
        "pending_thumbnails": snapshot.pending_thumbnails,
    }


def archives_json(root: Path) -> dict:
    out = {}
    for path in sorted(root.rglob("*")):
        if not path.is_file() or path.suffix.lower() != ".cbz":
            continue
        rel = path.relative_to(root).as_posix()
        names = _archive_image_names(path)
        try:
            reader = ArchiveReader(path)
            pages = [{"path": str(p), "sha256": hashlib.sha256(reader.read(p)).hexdigest()} for p in reader.pages()]
            reader.close()
        except zipfile.BadZipFile:
            pages = None
        siblings = [p.relative_to(root).as_posix() for p in sidecar_siblings(path)]
        out[rel] = {"reader_image_names": names, "pages": pages, "siblings": siblings}
    return out


PAGEDIR = HERE / "pagedir"


def build_pagedir() -> None:
    if PAGEDIR.exists():
        shutil.rmtree(PAGEDIR)
    for rel in ["10.jpg", "9.jpg", "sub/1.png", "sub/deeper/2.webp", "x.txt", ".hidden.jpg", "b.JPEG", "c.avif",
                "d.gif", "e.jpg.", ".jpg", JP + "/1.jpg", "pagedir.webp"]:
        path = PAGEDIR / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(img(rel))


def run_reference() -> None:
    if EXPECTED.exists():
        shutil.rmtree(EXPECTED)
    EXPECTED.mkdir()
    with tempfile.TemporaryDirectory(dir=Path.home() / ".cache/mokuro-bunko-demo/tmp") as tmp:
        root = Path(tmp) / "library"
        shutil.copytree(LIBRARY, root)
        apply_mtimes(root)
        db_path = Path(tmp) / "mokuro.db"
        database = Database(db_path)
        service = MetadataService(root, database)
        steps = []
        changed = service.regenerate_all()
        steps.append({"step": "initial full pass", "changed": changed, "files": compiled_files(root)})
        first_payload = payload_bytes(PUTS[0][1])
        for title, payload, actor in PUTS:
            body = first_payload if payload is None else payload_bytes(payload)
            accepted = service.apply_series_update(title, body, actor)
            steps.append({"step": f"PUT {title}", "title": title, "payload": body.decode("utf-8"), "actor": actor,
                          "accepted": accepted, "files": compiled_files(root)})
        changed = service.regenerate_all()
        steps.append({"step": "final full pass", "changed": changed, "files": compiled_files(root)})
        service.stop()

        # Compiled files carry wall-clock mtimes; pin them so manifests are reproducible.
        for path in root.rglob("*.json"):
            os.utime(path, ns=(COMPILED_MTIME_NS, COMPILED_MTIME_NS))
        index = LibraryIndexCache(root).get_snapshot()
        manifests = {}
        for series in index.series:
            for volume in series.volumes:
                manifest = build_volume_manifest(root / series.name, series.name, volume.name, ["ppocr-manga", "hayai-nova"])
                manifests[f"{series.name}/{volume.name}"] = manifest
        (EXPECTED / "steps.json").write_text(json.dumps(steps, ensure_ascii=True, indent=1), encoding="ascii")
        (EXPECTED / "db.json").write_text(json.dumps(dump_db(db_path), ensure_ascii=True, indent=1), encoding="ascii")
        (EXPECTED / "index.json").write_text(json.dumps(snapshot_json(index), ensure_ascii=True, indent=1), encoding="ascii")
        (EXPECTED / "manifests.json").write_text(json.dumps(manifests, ensure_ascii=True, indent=1), encoding="ascii")
        (EXPECTED / "pagedir.json").write_text(json.dumps([p.as_posix() for p in list_pages(PAGEDIR)], ensure_ascii=True),
                                               encoding="ascii")
        (EXPECTED / "archives.json").write_text(json.dumps(archives_json(root), ensure_ascii=True, indent=1), encoding="ascii")
    print(f"wrote {EXPECTED}")


if __name__ == "__main__":
    (Path.home() / ".cache/mokuro-bunko-demo/tmp").mkdir(parents=True, exist_ok=True)
    if "--expect-only" not in sys.argv:
        build_library()
        build_pagedir()
    run_reference()
