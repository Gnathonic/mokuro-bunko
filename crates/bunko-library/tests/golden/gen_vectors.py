"""Golden vectors for bunko-library's pure helpers, computed by 0.5.2 itself.

Run with the 0.5.2 reference interpreter (natsort comes from the OCR env, as
it did in production):

    ~/.cache/mokuro-bunko-demo/ref052/bin/python crates/bunko-library/tests/golden/gen_vectors.py

Writes `vectors.json` next to this script. Non-ASCII characters are written
with chr() so this file stays pure ASCII.
"""

from __future__ import annotations

import json
import math
import random
import struct
import sys
import unicodedata
from pathlib import Path

sys.path.append(str(Path.home() / ".cache/mokuro-bunko-demo/mokuro-env/lib/python3.12/site-packages"))

import natsort  # noqa: E402,F401  (must import: reading_order falls back without it)

from mokuro_bunko.metadata import reader_compat as rc  # noqa: E402
from mokuro_bunko.metadata.validate import parse_series_update  # noqa: E402
from mokuro_bunko.metadata.merge import StoredSeries, merge_series_update  # noqa: E402
from mokuro_bunko.metadata.schema import SeriesFacts, SeriesIndexData  # noqa: E402
from mokuro_bunko.ocr.engine_runner import reading_order  # noqa: E402
from mokuro_bunko.ocr.generations import split_layer_sidecar  # noqa: E402

OUT = Path(__file__).resolve().parent / "vectors.json"
NOW = 1_790_000_000.25
rng = random.Random(20261001)

NFD_A = "Shinjuku a" + chr(0x301)  # a + combining acute
JP = "".join(chr(c) for c in (0x9032, 0x6483, 0x306E, 0x5DE8, 0x4EBA))
FULLWIDTH_12 = chr(0xFF11) + chr(0xFF12)
ARABIC_3 = chr(0x0663)
CIRCLED_1 = chr(0x2460)
SUPER_2 = chr(0xB2)
SIGMA = chr(0x3A3)
DOTTED_I = chr(0x130)
SHARP_S = chr(0xDF)
LIGATURE_FI = chr(0xFB01)
KELVIN = chr(0x212A)
NBSP = chr(0xA0)
IDEO_SPACE = chr(0x3000)
NEL = chr(0x85)
BOM = chr(0xFEFF)

TITLES = [
    "Vol 1", "Vol 01", "vol 1", "VOL 1", "Vol 1.5", "Vol 01.5", "Vol 10", "Vol 2", "Vol 9",
    "Volume 1", "Volume 001", "Volume 1 Part 2", "v1", "v01", "v1a", "v1b", "1", "01", "a",
    "A", "", " ", "Vol", "Vol " + CIRCLED_1, "Vol " + SUPER_2, "Vol " + FULLWIDTH_12, "Vol " + ARABIC_3,
    "Vol 3" + ARABIC_3, NFD_A, unicodedata.normalize("NFC", NFD_A), JP + " 1", JP + " 10", JP + " 2",
    "Stra" + SHARP_S + "e 1", "Strasse 1", LIGATURE_FI + "le 2", "file 2", "Vol 99999999999999999999999",
    "Vol 100000000000000000000000", "Vol 0", "Vol 00", "x" + DOTTED_I, "xi", "Z", "z", "_1", "-1",
    "Vol 1 (extra)", "Vol 1 [scan]", "#1", SIGMA + "x", chr(0x1F600) + " 1",
]


def natural_vectors():
    shuffled = TITLES[:]
    rng.shuffle(shuffled)
    ordered = sorted(shuffled, key=lambda t: (rc.natural_sort_key(t), t))
    return {"input": shuffled, "sorted": ordered}


def key_strings():
    base = TITLES + [
        "  Dr   Stone  ", "Dr" + NBSP + "Stone", "Dr" + IDEO_SPACE + "Stone", "Dr" + NEL + "Stone",
        BOM + "Dr Stone", "Dr\tStone\n", "ODOS" + SIGMA, "O" + SIGMA + "O" + SIGMA + " A" + SIGMA,
        SIGMA, "A" + SIGMA + chr(0x301), "A" + SIGMA + "'s", KELVIN + "elvin", DOTTED_I + "stanbul",
        chr(0x1C) + "x" + chr(0x1F), "Dr Stone/v01", "Bakemonogatari/v01", JP + "/" + JP + " 01",
    ]
    return base


def float_vectors():
    values = [12.0, 12.5, 1e-5, 1e-4, 0.0001, 1e16, 1e15, 1234567890123456.0, 1.5e16, -40.0, 0.1,
              1700000000.123456, 1e22, 5e-324, -0.0, 0.0, 123456789012345678.0, 2.5, 1/3, 2/3,
              1e300, 1.7976931348623157e308, 2.2250738585072014e-308, 9007199254740993.0, 1e-7,
              100.0, 1e17, 9.999999999999999e15, 0.5, 1700000000.5]
    for _ in range(400):
        exponent = rng.randint(-30, 30)
        values.append(rng.uniform(-1, 1) * 10 ** exponent)
    for _ in range(200):
        values.append(struct.unpack("<d", struct.pack("<Q", rng.getrandbits(64)))[0])
    # Ties between two shortest candidates (few fraction bits at large magnitudes).
    for _ in range(1500):
        values.append(rng.randint(10**12, 10**16) + rng.choice([0.125, 0.25, 0.375, 0.5, 0.625, 0.75, 0.875]))
    # What st_mtime looks like: sec + nsec * 1e-9.
    for _ in range(1500):
        values.append(rng.randint(0, 4_000_000_000) + rng.randint(0, 999_999_999) * 1e-9)
    out = []
    for v in values:
        if not math.isfinite(v):
            continue
        out.append({"bits": struct.unpack("<Q", struct.pack("<d", v))[0], "repr": repr(v)})
    return out


def iso_inputs():
    inputs = [
        "2026-08-18T19:36:24.324Z", "2026-08-18T19:36:24.324999999Z", "2026-08-18 19:36:24", "20260818T193624Z",
        "2026-08-18", "2026-W33-2", "2026W332", "2026W33", "2026-W33", "2026-W53-1", "2020-W53-1", "2026-W00-1",
        "2026-08-18T19", "2026-08-18T19:36", "2026-08-18T1936", "2026-08-18T19.5", "2026-08-18T19:36.5",
        "2026-08-18T19:36:24,5+01:00", "2026-08-18T19:36:24+0130", "2026-08-18T19:36:24-05", "2026-08-18T19:36:24+00:99",
        "2026-08-18T19:36:24+24:00", "2026-08-18T19:36:24+23:59:59.999999", "2026-08-18T19:36:24+0", "2026-08-18T19:36:24+000",
        "2026-08-18T24:00:00", "2026-02-29", "2024-02-29T00:00:00Z", "0001-01-01T00:00:00Z", "0001-01-01T00:00:00+01:00",
        "0999-05-05T01:02:03.4567+01:00", "9999-12-31T23:59:59.999999Z", "9999-12-31T23:59:59-01:00", "Aug 16 2020",
        "", "   ", " 2026-08-18T19:36:24Z ", IDEO_SPACE + "2026-08-18" + NEL, "2026-08-18T19:36:24ZZ", "2026-08-18TZ",
        "2026-08-18T", "2026-08-18" + chr(0x3042) + "12:00", "2026-08-18" + chr(0x1F600) + "12:00", "2026-08-1",
        "2026-0818", "202608-18", "2026-13-01", "2026-00-10", "2026-01-32", "2026-08-18T12:60", "2026-08-18T12:30:60",
        "2026-08-18T12:30:30.1234567", "2026-08-18T12:30:30.", "2026-08-18T12:30:30.12a", "2026-08-18T12:34:56:78",
        "2026-08-18T123456", "2026-08-18T1234567", "2026-08-18T12-30", "2026-08-18T12+05:30Z", "+2026-08-18",
        "2099-01-01T00:00:00Z", "2026-09-21T13:58:20.250Z", "2026-09-21T14:03:20.250Z", "2026-09-21T14:03:21Z",
        "1970-01-01T00:00:00.000Z", "1969-12-31T23:59:59.999999Z", "2026-W01", "2026-W01-0", "2026-W01-8",
        "2026-W1-1", "2026-W01-1T10", "2026W011T10", "2026W01T10", "2026W0110:00", "2026-W01-10", "2026-W01-1000",
        "2026-08-18T19:36:24.000001Z", "2026-08-18T19:36:24.0000005Z", "2026-08-18\t19:36",
    ]
    alphabet = "0123456789-:+.,TZW W"
    for _ in range(3000):
        n = rng.randint(6, 26)
        inputs.append("".join(rng.choice(alphabet) for _ in range(n)))
    templates = ["2026-08-18T19:36:24", "20260818T193624", "2026-W33-2T19:36", "2026-08-18 19:36:24.123456+05:30"]
    for _ in range(2000):
        text = list(rng.choice(templates))
        for _ in range(rng.randint(1, 3)):
            index = rng.randrange(len(text))
            text[index] = rng.choice(alphabet)
        inputs.append("".join(text))
    return inputs


def json_docs():
    q = chr(34)
    bs = chr(92)
    docs = [
        '{"a": 1, "a": 2}', '{"b": NaN, "c": Infinity, "d": -Infinity}', '[1e400, -1e400, 1E5, 1e-7, 0.1]',
        '123456789012345678901234567890', '-0', '-0.0', '[1,]', '{"a":1,}', 'nan', 'NaN', '[01]', '1.', '.5',
        '"' + bs + 'u00e9' + bs + 'n' + bs + 't' + bs + '/' + '"', '"a' + chr(10) + 'b"', '"' + bs + 'x"',
        '"' + bs + 'ud83d' + bs + 'ude00"', '  {"k" : [ true , false , null ] }  ', '{"k": "' + JP + '"}',
        '[1.0, 2.50, 3e2, 4E-2, 1.5e16]', '{"x": {"y": {"z": []}}}', '[] []', '', ' ', '{}', '[]',
        '{"a": 1}' + chr(0), BOM + '{}', '"' + chr(0x7F) + chr(0x2028) + '"', '{"' + bs + 'u0041": 1}',
        '[-1, -1.0, 1e0, 100000000000000000000.0, 9007199254740993]', 'true', 'null', 'tru', '[1 2]',
        '{"a" 1}', '{1: 2}', '[' + q + 'unterminated]', '{"a": -}', '-Infinity', '-NaN', '[Infinity1]',
    ]
    out = []
    for doc in docs:
        try:
            value = json.loads(doc)
            dumped = json.dumps(value, ensure_ascii=False, separators=(",", ":"), allow_nan=True)
            default = json.dumps(value)
            out.append({"doc": doc, "ok": True, "compact": dumped, "default": default})
        except (ValueError, RecursionError):
            out.append({"doc": doc, "ok": False})
    return out


def count_char_strings():
    samples = [JP, "abc", "", "a" + chr(0x3005) + chr(0x3006) + chr(0x3007)]
    for start, end in rc._COUNTED_RANGES:
        for cp in (start - 1, start, end, end + 1):
            if 0 <= cp < 0x110000 and not 0xD800 <= cp <= 0xDFFF:
                samples.append(chr(cp))
    samples.append("".join(chr(rng.randint(0x3000, 0x9FFF)) for _ in range(500)))
    return [{"text": s, "count": rc.count_chars(s)} for s in samples]


def matched_page_cases():
    cases = [
        (["a/p1.png", "a/P2.JPG", None], ["a/p1.png", "a/p2.webp"]),
        (["p1.png", "p2.png", "p3.png"], ["x1.jpg", "x2.jpg", "x3.jpg"]),
        (["p1.png", "p2.png", "p3.png"], ["p1.png", "x2.jpg", "x3.jpg"]),
        (["p1.png", "p2.png", "p3.png", "p4.png"], ["p1.png", "p2.png", "x3.jpg", "x4.jpg"]),
        (["a\\b\\p1.png", "p1.png"], ["a/b/p1.png", "p1.png"]),
        (["p1.png", "p1.png"], ["p1.png"]),
        (["p1.png", "p1.webp"], ["p1.jpg"]),
        ([None, None], ["x.jpg", "y.jpg"]),
        (["dir/"], ["dir/"]),
        ([".gitkeep"], [".gitkeep.jpg"]),
        (["a.b.c.jpg"], ["a.b.c.png", "a.b.c.png"]),
        (["Vol" + DOTTED_I + ".jpg"], ["vol" + chr(0x69) + chr(0x307) + ".jpg"]),
    ]
    return [{"pages": p, "files": f, "matched": rc.count_matched_pages(p, f)} for p, f in cases]


def system_file_cases():
    paths = ["__MACOSX/a.jpg", "a/._b.jpg", "a/b.jpg~", "Thumbs.db", "a/thumbs.db", "x.TMP", "x.bak", "x.temp.jpg",
             "a\\.DS_Store\\b.jpg", ".git/x", "ok/p1.jpg", "", "a//b.jpg", "x.", ".Trash-1000/1.png", "desktop.ini",
             "chapter.1/page001", "a.JPG", "a.jpeg", "noext", "a.Jxl", "a.tiff/x"]
    return [{"path": p, "system": rc.is_system_file(p), "ext": rc.trailing_extension(p),
             "image": rc.is_image_extension(rc.trailing_extension(p))} for p in paths]


def layer_cases():
    names = ["Vol 1.hayai-nova.mokuro", "Vol 1.hayai-nova.mokuro.gz", "Vol 1.mokuro", "Vol 1.Bad.mokuro",
             ".x.mokuro", "Vol 01.5.mokuro", "a.b.c.mokuro", "a..mokuro", "a." + "x" * 33 + ".mokuro",
             "a." + "x" * 32 + ".mokuro", "a.mokuro.gz.gz", "a.x.mokuro.GZ", "Vol 1.backup.v1.mokuro", "a.-.mokuro"]
    out = []
    for name in names:
        split = split_layer_sidecar(name)
        out.append({"name": name, "split": list(split) if split else None})
    return out


def natsort_cases():
    lists = [
        ["p10.jpg", "p9.jpg", CIRCLED_1 + ".jpg", "x/p1.jpg", "p1.jpg", "P1.jpg", "p01.jpg", "p001.jpg"],
        ["001.jpg", "002.jpg", "010.jpg", "1.jpg", "10.jpg", "2.jpg", "a/1.jpg", "a10/1.jpg", "a2/1.jpg"],
        ["img" + SUPER_2 + ".png", "img2.png", "img1" + CIRCLED_1 + ".png", "img" + ARABIC_3 + ".png", "img3.png"],
        [NFD_A + "1.jpg", unicodedata.normalize("NFC", NFD_A) + "2.jpg", "Shinjuku b1.jpg", "Shinjuku a1.jpg"],
        ["Ch.1/p.10.jpg", "Ch.1/p.9.jpg", "Ch.10/p.1.jpg", "Ch.2/p.1.jpg", "-1.jpg", "+1.jpg", ".1.jpg", "_1.jpg"],
        [JP + "_010.jpg", JP + "_9.jpg", JP + "_1.jpg", "1e5.jpg", "1.5.jpg", "1.10.jpg", "x 1.jpg", "x  1.jpg"],
    ]
    out = []
    for items in lists:
        rng.shuffle(items)
        out.append({"input": items, "sorted": [str(p) for p in reading_order([Path(p) for p in items])]})
    return out


def update_cases():
    payloads = [
        '{"version":2}', '{"version":true,"updated_at":"2026-01-01T00:00:00Z"}', '{"version":1.0,"updated_at":"2026-01-01T00:00:00Z"}',
        '{"version":2,"updated_at":"2026-01-01T00:00:00Z","x":NaN}', '{"version":3,"updated_at":"2026-01-01T00:00:00Z"}',
        '{"version":2,"updated_at":"2026-01-01T00:00:00Z","external_ids":{"anilist":98416.0,"mal":5,"kitsu":3},'
        '"titles":{"native":"  x ","romaji":"","english":5,"other":"y"},"synonyms":["a"," ","",3,"b","a"],"tag":"  HD  ",'
        '"unit":"chapters-ish","spine_offset":-40,"volumes":[{"volume_uuid":"u","offset":0},{"volume_uuid":"u","offset":3},'
        '{"volume_uuid":" "},{"volume_uuid":"v","offset":12.0},{"volume_uuid":"w","offset":true},"junk",{"offset":4}]}',
        '{"version":2,"updated_at":"2026-01-01T00:00:00Z","spine_offset":12.0,"unit":"volumes","tag":5}',
        '{"version":2,"updated_at":"2026-01-01T00:00:00Z","spine_offset":0}',
        '{"version":2,"updated_at":"2026-01-01T00:00:00Z","spine_offset":' + "9" * 400 + '}',
        '{"version":2,"updated_at":"2026-01-01T00:00:00Z","spine_offset":' + "9" * 30 + '}',
        '{"version":2,"updated_at":"2099-01-01T00:00:00Z"}', '{"version":2,"updated_at":5}', '[]', 'nul',
        '{"version":2,"updated_at":"2026-01-01T00:00:00Z","external_ids":{"anilist":' + "1" * 25 + '}}',
        '{"version":2,"version":1,"updated_at":"x","updated_at":"2026-01-01T00:00:00Z"}',
    ]
    out = []
    for payload in payloads:
        update = parse_series_update(payload.encode("utf-8"), now=NOW)
        if update is None:
            out.append({"payload": payload, "result": None})
            continue
        out.append({"payload": payload, "result": {
            "facts": facts_dict(update.facts),
            "spine_offset": update.spine_offset,
            "spine_offset_present": update.spine_offset_present,
            "volume_offsets": update.volume_offsets,
            "listed": sorted(update.listed_uuids),
        }})
    return out


def facts_dict(facts: SeriesFacts):
    return {"external_ids": facts.external_ids, "titles": facts.titles, "synonyms": list(facts.synonyms),
            "tag": facts.tag, "unit": facts.unit, "updated_at": facts.updated_at, "has_facts": facts.has_facts()}


def merge_cases():
    stored_variants = [
        None,
        StoredSeries(SeriesFacts(tag="x", updated_at="2026-01-01T00:00:00.000Z"), SeriesIndexData(spine_offset=12, volume_offsets={"u": 3, "v": 4})),
        StoredSeries(SeriesFacts(updated_at="2026-01-01T00:00:00.000Z"), SeriesIndexData()),
        StoredSeries(SeriesFacts(external_ids={"anilist": 5}, updated_at="2026-06-01T00:00:00.000Z"), SeriesIndexData(spine_offset=12.0)),
    ]
    payloads = [
        '{"version":2,"updated_at":"2026-01-01T00:00:00Z"}',
        '{"version":2,"updated_at":"2026-01-01T00:00:01Z"}',
        '{"version":2,"updated_at":"2025-01-01T00:00:00Z","tag":"y"}',
        '{"version":2,"updated_at":"2026-01-01T00:00:00Z","tag":"y"}',
        '{"version":2,"updated_at":"2026-06-01T00:00:00Z","external_ids":{"anilist":5},"spine_offset":12}',
        '{"version":2,"updated_at":"2026-06-01T00:00:00Z","external_ids":{"anilist":5},"spine_offset":12.0,"volumes":[{"volume_uuid":"u"},{"volume_uuid":"z","offset":-2}]}',
    ]
    out = []
    for si, stored in enumerate(stored_variants):
        for payload in payloads:
            update = parse_series_update(payload.encode(), now=NOW)
            result = merge_series_update(stored, update)
            out.append({"stored": si, "payload": payload, "facts": facts_dict(result.facts),
                        "spine_offset": result.index.spine_offset, "volume_offsets": result.index.volume_offsets,
                        "facts_changed": result.facts_changed, "index_changed": result.index_changed})
    return out


def safe_normalize(text):
    try:
        return rc.normalize_updated_at(text, now=NOW)
    except (ValueError, OverflowError, OSError) as error:  # 0.5.2 raised (a 500); the port rejects
        return {"raised": type(error).__name__}


def path_cases():
    from mokuro_bunko.metadata import paths as mp
    from mokuro_bunko.middleware.fs_watcher import _is_relevant, classify_change
    virtual = ["/mokuro-reader/catalog.json", "/mokuro-reader//catalog.json", "/mokuro-reader/CATALOG.JSON",
               "/x/../mokuro-reader/catalog.json", "/../mokuro-reader/catalog.json", "/mokuro-reader/A/catalog.json",
               "/mokuro-reader/Dr Stone/series.json", "/mokuro-reader/Dr Stone/./series.json", "/mokuro-reader/./Dr Stone/SERIES.json",
               "/mokuro-reader/Dr Stone/../Dr Stone/series.json", "/mokuro-reader/A/B/series.json", "/mokuro-reader/series.json",
               "/mokuro-reader/ /series.json", "/mokuro-reader//series.json", "mokuro-reader/x/series.json/", "/mokuro-reader",
               "/mokuro-reader/", "/mokuro-reader/volume-data.json", "/mokuro-reader/profiles.json", "/other/catalog.json",
               "/mokuro-reader/a/../../catalog.json", "/mokuro-reader/" + JP + "/series.json", "///mokuro-reader///A///series.json"]
    out = {"virtual": [], "changes": [], "relevant": []}
    for v in virtual:
        out["virtual"].append({"path": v, "catalog": mp.is_catalog_file_path(v),
                               "series": mp.series_title_from_series_file_path(v), "compiled": mp.is_compiled_metadata_path(v)})
    root = Path("/lib")
    for p in ["/lib", "/lib/A", "/lib/A/v.cbz", "/lib/thumbnails/x.webp", "/lib/A/B/c", "/elsewhere/x", "/library/A/x", "/lib/thumbnails"]:
        kind, name = classify_change(root, p)
        out["changes"].append({"path": p, "kind": kind, "name": name})
    for p, d in [("a.cbz", False), ("a.CBZ", False), ("a.mokuro", False), ("a.mokuro.gz", False), ("a.gz", False),
                 ("a.webp", False), ("a.json", False), ("dir", True), (".cbz", False), ("x.Mokuro", False)]:
        out["relevant"].append({"path": p, "dir": d, "relevant": _is_relevant(p, d)})
    return out


def main() -> None:
    strings = key_strings()
    vectors = {
        "now": NOW,
        "natural_sort": natural_vectors(),
        "keys": [{"text": s, "series_key": rc.normalize_series_key(s), "volume_key": rc.normalize_volume_title_key(s),
                  "uuid": rc.deterministic_uuid(s), "lower": s.lower(), "casefold": s.casefold()} for s in strings],
        "floats": float_vectors(),
        "updated_at": [{"input": s, "output": safe_normalize(s)} for s in iso_inputs()],
        "iso_stamp": [{"seconds": s, "output": rc.iso_stamp(s)} for s in
                      [0.0, 1.5, 1787081784.324, 1787081784.3245, 1787081784.9999996, -0.0005, -1.25, 253402300799.999,
                       -62135596800.0, 1e9 + 0.0000005, 1e9 + 0.0000015, 1e9 + 0.0000025]],
        "json": json_docs(),
        "count_chars": count_char_strings(),
        "matched_pages": matched_page_cases(),
        "system_files": system_file_cases(),
        "layers": layer_cases(),
        "natsort": natsort_cases(),
        "updates": update_cases(),
        "merges": merge_cases(),
        "paths": path_cases(),
    }
    OUT.write_text(json.dumps(vectors, ensure_ascii=True, indent=1, allow_nan=False), encoding="ascii")
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
