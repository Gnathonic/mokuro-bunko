"""Generate the bunko-layout golden fixtures from the Python 0.5.2 code.

    ~/.cache/mokuro-bunko-demo/ref052/bin/python gen_golden.py

Imports ``mokuro_bunko`` from this worktree's ``src/`` (the ref052 env does)
and writes ``cases/*.json``. Inputs:

* ``inputs/real_pages.json``: real PP-OCR line records (``harvest_pages.py``);
* ``<repo>/tests/fixtures/ppocr/*.json``: the 0.5.2 test fixture pages;
* ``inputs/page129-*.detect.json``: a real reconciled-road raw dump with the
  VLM engines' actual first and second reads (research run 2026-09-19);
* seeded synthetic pages and seeded text mutations made here.

Everything is deterministic (fixed seeds), so re-running reproduces the files.
"""

from __future__ import annotations

import json
import math
import os
import random
import struct
import subprocess
import sys
import tempfile
import types
import unicodedata
import uuid
from difflib import SequenceMatcher
from pathlib import Path
from typing import Any

from golden_common import HERE, ROOT, jsonable, layout_result, reconciled_from_state, reconciled_state, write_json

from mokuro_bunko.ocr import engine_runner as er
from mokuro_bunko.ocr import line_layout as ll
from mokuro_bunko.ocr import line_reconcile as lr
from mokuro_bunko.ocr import ppocr
from mokuro_bunko.ocr import processor as proc
from mokuro_bunko.metadata.reader_compat import deterministic_uuid

CASES = HERE / "cases"
INPUTS = HERE / "inputs"
FIXTURES = ROOT / "tests" / "fixtures" / "ppocr"
VERSION = er.MOKURO_FORMAT_VERSION

# ---------------------------------------------------------------------------
# inputs
# ---------------------------------------------------------------------------


def real_pages() -> list[dict[str, Any]]:
    return json.loads((INPUTS / "real_pages.json").read_text(encoding="utf-8"))["pages"]


def fixture_pages() -> list[tuple[str, dict[str, Any]]]:
    return [
        (p.stem, json.loads(p.read_text(encoding="utf-8")))
        for p in sorted(FIXTURES.glob("*.json"))
        if p.stem != "probes-novel-dialogue-200"
    ]


def as_lines(page: dict[str, Any]) -> list[types.SimpleNamespace]:
    out = []
    for i, rec in enumerate(page["lines"]):
        out.append(
            types.SimpleNamespace(
                idx=i,
                quad=[[float(x), float(y)] for x, y in rec["quad"]],
                score=float(rec.get("score", 0.0)),
                text=str(rec.get("text", "")),
                conf=float(rec.get("conf", 0.0)),
                vertical=bool(rec.get("vertical", True)),
                angle=float(rec.get("angle", 0.0)),
                char_confs=[float(c) for c in rec.get("char_confs", [])],
            )
        )
    return out


def rounded(page: dict[str, Any]) -> dict[str, Any]:
    """``ppocr.page_to_json`` of a page of unrounded records."""
    return ppocr.page_to_json(as_lines(page), page["width"], page["height"], detector=page.get("detector"))


# ---------------------------------------------------------------------------
# synthetic pages
# ---------------------------------------------------------------------------

KANJI = "日本語漢字読書学校先生時間今何見聞話言思出入大小中上下前後左右東西南北山川花鳥風月"
KANA = "あいうえおかきくけこさしすせそたちつてとなにぬねのはひふへほまみむめもやゆよらりるれろわをんっゃゅょ"
KATA = "アイウエオカキクケコサシスセソタチツテトナニヌネノハヒフヘホマミムメモヤユヨラリルレロワヲンッャュョー"
PUNCT = "。、！？…‥「」『』（）―ー・"
ASCII = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!?.-"
LOOKALIKE = "夕力卜へべぺりき—ー ャュョゃゅょ"


def rand_text(rng: random.Random, n: int, style: str = "mix") -> str:
    if n <= 0:
        return ""
    if style == "kana":
        pool = KANA
    elif style == "kata":
        pool = KATA
    elif style == "ascii":
        pool = ASCII
    else:
        pool = KANJI * 2 + KANA * 2 + KATA + PUNCT + LOOKALIKE
    text = "".join(rng.choice(pool) for _ in range(n))
    if style == "mix" and rng.random() < 0.25:
        text = "「" + text[1:] + ("」" if rng.random() < 0.7 else "")
    return text


def rect_quad(cx: float, cy: float, w: float, h: float, angle: float) -> list[list[float]]:
    c, s = math.cos(math.radians(angle)), math.sin(math.radians(angle))
    corners = [(-w / 2, -h / 2), (w / 2, -h / 2), (w / 2, h / 2), (-w / 2, h / 2)]
    return [[cx + x * c - y * s, cy + x * s + y * c] for x, y in corners]


def scramble(rng: random.Random, quad: list[list[float]]) -> list[list[float]]:
    """Exercise canonical_quad: another start corner and/or the other winding."""
    r = rng.random()
    if r < 0.75:
        return quad
    k = rng.randrange(4)
    q = quad[k:] + quad[:k]
    if rng.random() < 0.5:
        q = [q[0], q[3], q[2], q[1]]
    return q


def line_rec(rng: random.Random, quad: list[list[float]], text: str, conf: float | None = None) -> dict[str, Any]:
    return {
        "quad": [[round(x, 2), round(y, 2)] for x, y in quad],
        "score": round(rng.uniform(0.55, 0.97), 4),
        "text": text,
        "conf": round(conf if conf is not None else rng.choice([rng.uniform(0.85, 1.0), rng.uniform(0.3, 1.0)]), 4),
    }


def synth_manga(rng: random.Random) -> dict[str, Any]:
    W, H = rng.choice([(1200, 1700), (1654, 2400), (1100, 1600)])
    lines: list[dict[str, Any]] = []
    for _ in range(rng.randint(2, 8)):
        em = rng.uniform(18, 70)
        ncols = rng.randint(1, 5)
        gap = rng.uniform(0.02, 0.6) * em
        x0 = rng.uniform(0.1, 0.9) * W
        y0 = rng.uniform(0.05, 0.8) * H
        tilt = rng.choice([0.0] * 7 + [rng.uniform(-4, 4)] * 2 + [rng.uniform(-35, 35)])
        horizontal = rng.random() < 0.15
        bubble = []
        for c in range(ncols):
            glyphs = rng.randint(1, 10)
            length = glyphs * em * rng.uniform(0.95, 1.1)
            start = rng.uniform(-0.6, 0.6) * em
            if horizontal:
                cx, cy = x0 + start + length / 2, y0 + c * (em + gap)
                w, h = length, em * rng.uniform(0.9, 1.1)
            else:
                cx, cy = x0 - c * (em + gap), y0 + start + length / 2
                w, h = em * rng.uniform(0.9, 1.1), length
            style = rng.choice(["mix", "mix", "kana", "kata"])
            text = rand_text(rng, glyphs, style)
            if rng.random() < 0.08:
                text = rng.choice(["", " ", "　"])
            bubble.append((cx, cy, w, h, text))
            if not horizontal and rng.random() < 0.3 and glyphs >= 2:
                rl = rng.randint(1, 3) * em * 0.5
                rx = cx + w / 2 + rng.uniform(-0.05, 0.2) * em + 0.25 * em
                ry = cy - h / 2 + rng.uniform(0, max(0.0, h - rl))
                bubble.append((rx, ry + rl / 2, em * rng.uniform(0.4, 0.8), rl, rand_text(rng, max(1, int(rl / (0.5 * em))), "kana")))
            if rng.random() < 0.12 and not horizontal:
                # a column the detector cut in two
                cut = rng.uniform(0.3, 0.7) * h
                g = rng.uniform(0.0, 1.0) * em
                bubble[-1] = (cx, cy - h / 2 + cut / 2, w, cut, text[: max(1, len(text) // 2)])
                rest = h - cut
                bubble.append((cx, cy - h / 2 + cut + g + rest / 2, w * rng.uniform(0.9, 1.1), rest, rng.choice([text[len(text) // 2 :] or "―", "", "―"])))
        bx = sum(b[0] for b in bubble) / len(bubble)
        by = sum(b[1] for b in bubble) / len(bubble)
        for cx, cy, w, h, text in bubble:
            dx, dy = cx - bx, cy - by
            c, s = math.cos(math.radians(tilt)), math.sin(math.radians(tilt))
            rx, ry = bx + dx * c - dy * s, by + dx * s + dy * c
            q = scramble(rng, rect_quad(rx, ry, w, h, tilt + rng.uniform(-1.5, 1.5) * (rng.random() < 0.3)))
            lines.append(line_rec(rng, q, text))
    for _ in range(rng.randint(0, 4)):
        # stray glyphs, noise, degenerate quads
        cx, cy, s = rng.uniform(0, W), rng.uniform(0, H), rng.uniform(10, 60)
        kind = rng.random()
        if kind < 0.15:
            q = [[cx, cy], [cx + s, cy], [cx + 2 * s, cy], [cx + 3 * s, cy]]
        else:
            q = rect_quad(cx, cy, s * rng.uniform(0.7, 1.4), s * rng.uniform(0.7, 1.4), rng.uniform(-40, 40))
        lines.append(line_rec(rng, q, rand_text(rng, rng.randint(1, 2)), conf=rng.uniform(0.1, 0.95)))
    rng.shuffle(lines)
    return {"format": "ppocr-lines/1", "width": W, "height": H, "lines": lines}


def synth_novel(rng: random.Random) -> dict[str, Any]:
    W, H = rng.choice([(1925, 2800), (1300, 1900)])
    em = rng.uniform(45, 65) * W / 1925
    pitch = em * rng.uniform(1.25, 1.6)
    tiers = rng.choice([1, 1, 1, 2])
    top = 0.12 * H
    bottom = 0.88 * H
    theta = rng.choice([0.0, 0.0, rng.uniform(-1.2, 1.2)])
    lines: list[dict[str, Any]] = []
    ncols = rng.randint(8, 16)
    right = W - 0.1 * W
    tier_h = (bottom - top - (tiers - 1) * 2 * em) / tiers
    for t in range(tiers):
        ttop = top + t * (tier_h + 2 * em)
        for c in range(ncols):
            cx = right - c * pitch
            indent = em if rng.random() < 0.25 else (0.45 * em if rng.random() < 0.2 else 0.0)
            full = tier_h - indent
            length = full if rng.random() < 0.7 else rng.uniform(1, 0.8 * full / em) * em
            glyphs = max(1, int(length / em))
            text = rand_text(rng, glyphs, "mix")
            if indent and rng.random() < 0.5:
                text = "「" + text[1:]
            cy = ttop + indent + length / 2
            q = scramble(rng, rect_quad(cx, cy, em * rng.uniform(0.95, 1.05), length, theta + rng.uniform(-0.3, 0.3)))
            lines.append(line_rec(rng, q, text, conf=rng.uniform(0.8, 1.0)))
            for _ in range(rng.randint(0, 3)):
                rl = rng.randint(1, 4) * 0.5 * em
                ry = cy - length / 2 + rng.uniform(0, max(0.0, length - rl))
                rq = rect_quad(cx + em / 2 + 0.3 * em, ry + rl / 2, 0.5 * em * rng.uniform(0.9, 1.3), rl, theta)
                lines.append(line_rec(rng, rq, rand_text(rng, max(1, int(rl / (0.5 * em))), "kana")))
    # page number and running title
    lines.append(line_rec(rng, rect_quad(W / 2, 0.95 * H, 2 * em * 0.6, 0.6 * em, 0.0), str(rng.randint(1, 300)), conf=0.95))
    lines.append(line_rec(rng, rect_quad(W * 0.8, 0.05 * H, 0.6 * em, 6 * 0.6 * em, 0.0), rand_text(rng, 6, "mix"), conf=0.97))
    rng.shuffle(lines)
    return {"format": "ppocr-lines/1", "width": W, "height": H, "lines": lines}


def synth_horizontal(rng: random.Random) -> dict[str, Any]:
    W, H = 1600, 2200
    em = rng.uniform(25, 50)
    lines: list[dict[str, Any]] = []
    y = 0.1 * H
    for _p in range(rng.randint(2, 5)):
        for r in range(rng.randint(1, 6)):
            indent = em if r == 0 and rng.random() < 0.5 else 0.0
            length = rng.uniform(5, 30) * em
            text = rand_text(rng, max(1, int(length / em)), rng.choice(["mix", "ascii"]))
            q = scramble(rng, rect_quad(0.1 * W + indent + length / 2, y, length, em, rng.uniform(-0.5, 0.5)))
            lines.append(line_rec(rng, q, text))
            if rng.random() < 0.2:
                lines.append(line_rec(rng, rect_quad(0.1 * W + indent + length / 3, y - 0.8 * em, 2 * em, 0.5 * em, 0.0), rand_text(rng, 3, "kana")))
            y += em * rng.uniform(1.2, 1.7)
        y += em * rng.uniform(0.5, 3.0)
    return {"format": "ppocr-lines/1", "width": W, "height": H, "lines": lines}


def jitter(rng: random.Random, page: dict[str, Any]) -> dict[str, Any]:
    out = json.loads(json.dumps(page))
    out.pop("detector", None)
    keep = []
    for rec in out["lines"]:
        if rng.random() < 0.1:
            continue
        dx, dy = rng.uniform(-3, 3), rng.uniform(-3, 3)
        rec["quad"] = [[round(x + dx + rng.uniform(-0.5, 0.5), 2), round(y + dy + rng.uniform(-0.5, 0.5), 2)] for x, y in rec["quad"]]
        rec["conf"] = round(min(1.0, max(0.0, rec["conf"] + rng.uniform(-0.15, 0.1))), 4)
        rec.pop("char_confs", None)
        keep.append(rec)
    out["lines"] = keep
    return out


# ---------------------------------------------------------------------------
# text mutations (engine reads)
# ---------------------------------------------------------------------------


def mutate(rng: random.Random, text: str) -> str:
    t = text
    for _ in range(rng.choice([0, 0, 1, 1, 1, 2, 3])):
        op = rng.randrange(16)
        if op == 0:
            t = unicodedata.normalize("NFKC", t)
        elif op == 1 and t[:1] in "「『（":
            t = t[1:]
        elif op == 2 and t[-1:] in "」』）。、":
            t = t[:-1]
        elif op == 3 and t:
            i = rng.randrange(len(t))
            t = t[:i] + rng.choice(KANJI + KANA + KATA) + t[i + 1 :]
        elif op == 4:
            i = rng.randrange(len(t) + 1)
            t = t[:i] + rng.choice(KANJI + KANA + PUNCT + "!?.") + t[i:]
        elif op == 5 and t:
            i = rng.randrange(len(t))
            t = t[:i] + t[i + 1 :]
        elif op == 6:
            unit = rand_text(rng, rng.randint(1, 3), rng.choice(["kana", "kata"]))
            t = t + unit * rng.randint(3, 30)
        elif op == 7:
            t = ""
        elif op == 8:
            t = t.replace("―", rng.choice(["-", "—", "ー", "―", "‐"]))
        elif op == 9:
            t = t.replace("――", rng.choice(["ー", "—", "-"]))
        elif op == 10 and t:
            i = rng.randrange(len(t) + 1)
            t = t[:i] + rng.choice([" ", "　", "\n"]) + t[i:]
        elif op == 11:
            t = t.replace("…", rng.choice(["...", "......", "．．．", "…", ".."]))
        elif op == 12:
            t = "".join(ch for ch in t if ch not in PUNCT)
        elif op == 13:
            t = rand_text(rng, rng.randint(1, 8), rng.choice(["mix", "kana", "ascii"]))
        elif op == 14:
            t = t.replace("！", "!").replace("？", "?")
        elif op == 15 and t:
            t = t + rng.choice(["!", "!!", "?", "!?", "...", "。", "」"])
    return t


# ---------------------------------------------------------------------------
# sections
# ---------------------------------------------------------------------------


def gen_capture() -> None:
    cache = Path.home() / ".cache" / "mokuro-bunko-demo" / "tmp" / "bunko-layout"
    cache.mkdir(parents=True, exist_ok=True)
    out = cache / "capture.json"
    env = dict(os.environ, GOLDEN_CAPTURE=str(out), PYTHONPATH=str(HERE))
    subprocess.run(
        [
            sys.executable, "-m", "pytest", "-q", "-p", "capture_plugin",
            "tests/unit/test_line_reconcile.py", "tests/unit/test_line_layout.py",
            "tests/unit/test_engine_runner.py", "-k",
            "Reconcil or reconcil or layout or Layout or PPOcr or ppocr or line",
        ],  # fmt: skip
        cwd=ROOT, env=env, check=True, stdout=subprocess.DEVNULL,
    )
    cases = json.loads(out.read_text(encoding="utf-8"))
    kept = []
    for c in cases:
        if c["fn"] in ("layout_page", "column_pieces"):
            page = c["args"].get("page") or {}
            if not isinstance(page, dict) or len(page.get("lines") or []) >= 20:
                continue  # the fixture pages: covered by layout_pages.json
        kept.append(c)
    kept.sort(key=lambda c: (c["fn"], json.dumps(c, ensure_ascii=False, sort_keys=True)))
    write_json(CASES / "unit_capture.json", kept)


def layout_expect(page: dict[str, Any]) -> dict[str, Any]:
    page_dict, result = er.layout_page_dict(page, ll, VERSION)
    return {
        "layout": layout_result(ll, result),
        "column_pieces": ll.column_pieces(page),
        "page_json": json.dumps(page_dict, ensure_ascii=False),
    }


def gen_layout() -> None:
    cases = []
    for page in real_pages():
        cases.append({"name": page["name"], "ref": "real", "expect": layout_expect(rounded(page))})
    for name, page in fixture_pages():
        cases.append({"name": name, "ref": "fixture", "expect": layout_expect(page)})
    for name in ("page129-paddle", "page129-hayai"):
        page = json.loads((INPUTS / f"{name}.detect.json").read_text(encoding="utf-8"))
        cases.append({"name": name, "ref": "page129", "expect": layout_expect(page)})
    rng = random.Random(20261001)
    reals = real_pages()
    for k in range(8):
        page = jitter(rng, rounded(reals[rng.randrange(len(reals))]))
        cases.append({"name": f"jitter-{k}", "page": page, "expect": layout_expect(page)})
    for k in range(70):
        page = [synth_manga, synth_manga, synth_novel, synth_horizontal][k % 4](rng)
        cases.append({"name": f"synth-{k}", "page": page, "expect": layout_expect(page)})
    write_json(CASES / "layout_pages.json", cases)


def road_run(
    page: dict[str, Any], first_reads: dict[int, str], second_reads: dict[int, str] | None
) -> dict[str, Any]:
    lines = as_lines(page)
    w, h = page["width"], page["height"]
    info = page.get("detector") or {}
    caps_seen: list[list[int]] = []

    class Recognizer:
        token_caps = True

        def __call__(self, crops: list[Any], max_tokens: list[int] | None = None) -> list[str]:
            caps_seen.append(list(max_tokens or []))
            return [(first_reads if kind == "first" else (second_reads or {})).get(idx, "") for kind, idx in crops]

    reader = er.ReconciledPageReader.__new__(er.ReconciledPageReader)
    reader.ppocr, reader.layout, reader.reconcile = ppocr, ll, lr
    reader.recognize = Recognizer()
    reader.crop_fn = lambda img, blk, k: [("first", blk["idx"])]
    reader.second_crop_fn = (lambda img, blk, k: [("second", blk["idx"])]) if second_reads is not None else None
    reader.engine_seconds, reader.repos, reader.raw, reader.ruby_count = 0.0, {}, {}, 0
    reader._line_block = lambda line: {  # noqa: SLF001
        "lines": [[[float(x), float(y)] for x, y in line.quad]],
        "vertical": bool(line.vertical),
        "idx": line.idx,
    }
    img = types.SimpleNamespace(shape=(h, w, 3))
    first = ll.layout_page(ppocr.page_to_json(lines, w, h, detector=info))
    read = reader.engine_read(er.DetectedPage(img, lines, info, first))
    result = reader.finish_read(read, VERSION)
    return {
        "targets": read.targets,
        "pitch": read.pitch,
        "first_caps": caps_seen[0] if caps_seen else [],
        "settled": [reconciled_state(r) for r in read.settled],
        "lines": [{"text": ln.text, "conf": ln.conf} for ln in lines],
        "page_json": json.dumps(result.page, ensure_ascii=False),
        "raw_json": json.dumps(result.raw, ensure_ascii=False, default=er.json_default),
        "doubtful_json": json.dumps(result.doubtful, ensure_ascii=False),
        "page": result.page,
    }


def gen_road() -> tuple[list[dict[str, Any]], list[tuple[str, dict[str, Any]]]]:
    rng = random.Random(7700)
    cases = []
    pages_out: list[tuple[str, dict[str, Any]]] = []
    sources: list[tuple[str, str, dict[str, Any]]] = [("real", p["name"], p) for p in real_pages()]
    sources += [("fixture", name, page) for name, page in fixture_pages()]
    for ref, name, page in sources:
        flavours = ["paddle", "hayai"] if ref == "real" else [rng.choice(["paddle", "hayai"])]
        for flavour in flavours:
            first_reads, second_reads = {}, ({} if flavour == "paddle" else None)
            for i, rec in enumerate(page["lines"]):
                ctc = ll.normalize_text(str(rec.get("text", "")).strip())
                base = ctc if ctc or rng.random() < 0.5 else rand_text(rng, rng.randint(1, 4), "kata")
                first_reads[i] = mutate(rng, base)
                if second_reads is not None:
                    second_reads[i] = mutate(rng, rng.choice([ctc, first_reads[i], base]))
            expect = road_run(page, first_reads, second_reads)
            pages_out.append((f"{ref}/{name}.webp", expect.pop("page")))
            cases.append(
                {
                    "name": f"{name}:{flavour}",
                    "ref": ref,
                    "page_name": name,
                    "first_reads": {str(k): v for k, v in first_reads.items()},
                    "second_reads": None if second_reads is None else {str(k): v for k, v in second_reads.items()},
                    "expect": expect,
                }
            )
    # page 129: the engines' REAL reads, as the research run recorded them
    for name, flavour in (("page129-paddle", "paddle"), ("page129-hayai", "hayai")):
        dump = json.loads((INPUTS / f"{name}.detect.json").read_text(encoding="utf-8"))
        page = {k: dump[k] for k in ("format", "width", "height", "detector")}
        page["lines"] = []
        first_reads, second_reads = {}, ({} if flavour == "paddle" else None)
        for i, rec in enumerate(dump["lines"]):
            entry = {k: rec[k] for k in ("quad", "score", "text", "conf", "vertical", "angle", "char_confs") if k in rec}
            if "ctc" in rec:
                entry["text"] = rec["ctc"]
                first_reads[i] = rec.get("vlm", "")
                if second_reads is not None and "vlm_second" in rec:
                    second_reads[i] = rec["vlm_second"]
            if "engine_only" in rec and "dropped" in rec.get("notes", []):
                entry["conf"] = rec["conf"]
            page["lines"].append(entry)
        expect = road_run(page, first_reads, second_reads)
        pages_out.append((f"page129/{name}.jpeg", expect.pop("page")))
        cases.append(
            {
                "name": f"{name}:{flavour}",
                "ref": "inline",
                "page": page,
                "first_reads": {str(k): v for k, v in first_reads.items()},
                "second_reads": None if second_reads is None else {str(k): v for k, v in second_reads.items()},
                "expect": expect,
            }
        )
    # two pieces of one column the join did not hold, whose engine reads share
    # the seam (``_trim_repeats``), plus an engine-only line next to them
    em = 40.0
    seam_lines = [
        {"quad": [[500.0, 100.0], [540.0, 100.0], [540.0, 340.0], [500.0, 340.0]], "score": 0.91,
         "text": "今日は良い天", "conf": 0.97, "vertical": True, "angle": 0.0, "char_confs": [0.99] * 6},
        {"quad": [[501.0, 350.0], [541.0, 350.0], [541.0, 510.0], [501.0, 510.0]], "score": 0.88,
         "text": "気ですね", "conf": 0.96, "vertical": True, "angle": 0.0, "char_confs": [0.98] * 4},
        {"quad": [[420.0, 100.0], [460.0, 100.0], [460.0, 260.0], [420.0, 260.0]], "score": 0.86,
         "text": "", "conf": 0.0, "vertical": True, "angle": 0.0, "char_confs": []},
        {"quad": [[300.0, 100.0], [300.0 + em, 100.0], [300.0 + em, 400.0], [300.0, 400.0]], "score": 0.9,
         "text": "そうだね", "conf": 0.99, "vertical": True, "angle": 0.0, "char_confs": [0.99] * 4},
    ]  # fmt: skip
    for name, top1, first_reads, second_reads in (
        ("seam-1", 350.0, {0: "今日は良い天", 1: "天気ですね", 2: "ザッ", 3: "そうだね"}, None),
        ("seam-2", 320.0, {0: "今日は良い天気", 1: "天気ですね", 2: "ザザザザザザザザザザザザザザ", 3: "そうだな"},
         {1: "天気ですね", 2: "ザザ", 3: "そうだね"}),
    ):  # fmt: skip
        page_lines = json.loads(json.dumps(seam_lines))
        page_lines[1]["quad"] = [[501.0, top1], [541.0, top1], [541.0, 510.0], [501.0, 510.0]]
        seam_page = {"format": "ppocr-lines/1", "width": 800, "height": 1000, "lines": page_lines}
        expect = road_run(seam_page, first_reads, second_reads)
        expect.pop("page")
        cases.append(
            {
                "name": name, "ref": "inline", "page": seam_page,
                "first_reads": {str(k): v for k, v in first_reads.items()},
                "second_reads": None if second_reads is None else {str(k): v for k, v in second_reads.items()},
                "expect": expect,
            }
        )  # fmt: skip
    write_json(CASES / "road.json", cases)
    return cases, pages_out


def gen_sidecar(pages: list[tuple[str, dict[str, Any]]]) -> None:
    rng = random.Random(4242)
    cases = []
    engines = [
        ("ppocr-manga", {"Kellenok/PP-OCRv6_manga": "ba1d479e0ffe1e7ad3b8f6bd4f6b2b3a0d1c2e3f"}, None, None),
        ("hayai-nova", {"Kellenok/PP-OCRv6_manga": "ba1d479e0ffe1e7ad3b8f6bd4f6b2b3a0d1c2e3f",
                        "JustANormalTinkerer/hayai-ocr-v2.5-nova": er.REPO_REVISIONS["JustANormalTinkerer/hayai-ocr-v2.5-nova"],
                        "google/siglip2-base-patch16-naflex": er.REPO_REVISIONS["google/siglip2-base-patch16-naflex"]}, "fp16", "mokuro-bunko 0.5.2"),
        ("paddle-manga", {}, "bf16", None),
        ("paddle-manga", {"PaddlePaddle/PaddleOCR-VL-1.6": er.REPO_REVISIONS["PaddlePaddle/PaddleOCR-VL-1.6"]}, "", "mokuro-bunko 0.7.0"),
    ]  # fmt: skip
    titles = ["Dr Stone", "魔のものたちは企てる", " Odd \"name\" \\ with/ slash　", "One-Punch Man", "İstanbul ΣΟΦΟΣ"]
    holder = tempfile.TemporaryDirectory(prefix="bunko-layout-golden-")
    tmp = Path(holder.name)
    for k, (engine, weights, precision, generator) in enumerate(engines * 2):
        n = rng.randint(1, 5)
        chosen_idx = [rng.randrange(len(pages)) for _ in range(n)]
        chosen = [pages[i] for i in chosen_idx]
        title = titles[k % len(titles)]
        meta = {
            "id": engine,
            "recognizer": er.RECOGNIZER_REPOS[engine],
            "detector": "ppocr-manga",
            "generator": generator or "mokuro-bunko",
            **({"patch_budget": 384 if k % 2 else 512} if engine in er.PATCH_BUDGET_ENGINES else {}),
            **({"weights": dict(weights)} if weights else {}),
            **({"precision": precision} if precision else {}),
        }
        engine_meta = meta if k != 3 else None
        volume = er.build_volume(
            [(p if k % 3 else p.replace("/", "\\"), page) for p, page in chosen],
            version=VERSION, title=title, volume=f"{title} {k:02d}",
            title_uuid=str(uuid.UUID(int=rng.getrandbits(128), version=4)),
            volume_uuid=str(uuid.UUID(int=rng.getrandbits(128), version=4)),
            engine_meta=engine_meta,
        )  # fmt: skip
        out = tmp / f"v{k}.mokuro"
        tmp_path = out.with_name(out.name + ".tmp")
        er.dump_json(volume, tmp_path)
        tmp_path.replace(out)
        runner_bytes = out.read_text(encoding="utf-8")
        library = tmp / "library"
        series_dir = library if k % 4 == 0 else library / f" {title} "
        cbz = series_dir / f"{title} {k:02d}.cbz"
        primary = k % 2 == 0
        generation = types.SimpleNamespace(primary=primary, engine=engine, name="default" if primary else f"gen{k}", id=k)
        volume_uuid = deterministic_uuid(f"{title}/{cbz.stem}") if k % 3 == 0 else str(uuid.UUID(int=rng.getrandbits(128), version=4))
        if k == 5:
            data = json.loads(runner_bytes)
            data.pop("ocr_engine", None)
            out.write_text(json.dumps(data, ensure_ascii=False), encoding="utf-8")
            runner_bytes = out.read_text(encoding="utf-8")
        ns = types.SimpleNamespace(library_path=library, inbox_path=library / "inbox")
        ns._derive_series_name = lambda p, ns=ns: proc.OCRProcessor._derive_series_name(ns, p)  # noqa: SLF001
        ns.volume_uuid_for = lambda p, g, u=volume_uuid: u
        ns._stamp_ocr_engine = proc.OCRProcessor._stamp_ocr_engine  # noqa: SLF001
        ns._log = lambda msg: None  # noqa: SLF001
        proc.OCRProcessor._normalize_mokuro_metadata(ns, out, cbz, generation)  # noqa: SLF001
        cases.append(
            {
                "name": f"volume-{k}",
                "header": {
                    "version": VERSION, "title": title, "title_uuid": volume["title_uuid"],
                    "volume": volume["volume"], "volume_uuid": volume["volume_uuid"],
                },  # fmt: skip
                "engine": None if engine_meta is None else {
                    "id": engine, "generator": generator, "patches": meta.get("patch_budget", 512),
                    "weights": list(weights.items()), "precision": precision,
                },  # fmt: skip
                "page_indices": chosen_idx,
                "runner_modified": k == 5,
                "img_paths": [p if k % 3 else p.replace("/", "\\") for p, _ in chosen],
                "runner_bytes": runner_bytes,
                "normalize": {
                    "cbz": str(cbz.relative_to(tmp)), "library": "library", "inbox": "library/inbox",
                    "series_name": proc.OCRProcessor._derive_series_name(ns, cbz),  # noqa: SLF001
                    "volume": cbz.stem, "volume_uuid": volume_uuid, "primary": primary,
                    "engine": engine, "generator": f"mokuro-bunko {proc.__version__}", "generation": generation.name,
                },  # fmt: skip
                "normalized_bytes": out.read_text(encoding="utf-8"),
            }
        )
    holder.cleanup()
    uuids = []
    for s in titles + ["", "a", "Series/Volume 01", "ＡＢＣ/全角", "😀 emoji/𠮷", "  Mixed Case/Vol 1  ", "x" * 300]:
        uuids.append({"value": s, "deterministic": deterministic_uuid(s), "uuid5": str(uuid.uuid5(uuid.NAMESPACE_DNS, s))})
    write_json(CASES / "sidecar.json", {"volumes": cases, "uuids": uuids})


def gen_difflib() -> None:
    rng = random.Random(1234567)
    cases = []
    for k in range(1200):
        alpha = "abcdefgh"[: rng.randint(1, 8)]
        if k % 10 == 0:
            la, lb = rng.randint(0, 120), rng.randint(0, 120)
        else:
            la, lb = rng.randint(0, 25), rng.randint(0, 25)
        a = [rng.choice(alpha) for _ in range(la)]
        if rng.random() < 0.5 and a:
            b = list(a)
            for _ in range(rng.randint(0, 5)):
                op = rng.randrange(3)
                i = rng.randrange(len(b) + 1)
                if op == 0:
                    b.insert(i, rng.choice(alpha))
                elif op == 1 and b and i < len(b):
                    del b[i]
                elif b and i < len(b):
                    b[i] = rng.choice(alpha)
        else:
            b = [rng.choice(alpha) for _ in range(lb)]
        sm = SequenceMatcher(None, a, b, autojunk=False)
        cases.append(
            {
                "a": "".join(a),
                "b": "".join(b),
                "opcodes": [list(op) for op in sm.get_opcodes()],
                "blocks": [list(m) for m in sm.get_matching_blocks()],
                "ratio": sm.ratio(),
            }
        )
    write_json(CASES / "difflib.json", cases)


def gen_reconcile_fuzz() -> None:
    rng = random.Random(99)
    texts = []
    for page in real_pages():
        texts += [ln["text"] for ln in page["lines"] if ln["text"]]
    for _name, page in fixture_pages():
        texts += [ln["text"] for ln in page["lines"] if ln["text"]]
    texts += ["「なんだって――」", "……そうか", "No.1", "OK!", "ドドドドドド", "ヴォォォォォ", "９ １４１", "ONE PUNCH", "すげーー！", "何かに――それが", "〓字", "ﾊﾝｶｸ"]
    cases = []
    handmade = [
        ("廊下が真", "廊下が真一", True), ("一廊下が", "一廊下が", True), ("廊下が", "一廊下が", True),
        ("あれが来たら", "―あれが来たら", True), ("何かにーそれが", "何かに――それが", False),
        ("だって-」", "だって――」", False), ("だってー", "だって――", False), ("なに!?", "なに!? ", False),
        ("ちょっと待て!", "ちょっと待て！　", False), ("ONEPUNCH", "ONE PUNCH", False), ("9141", "9 141", False),
        ("……そうか", "……そうか", False), ("...そうか", "……そうか", False), ("........だ", "………だ", False),
        ("するために", "するたびに", False), ("挟む", "抜む", False), ("", "〓", False), ("ぐくーーーーーーーーーーーーーーーーーーーー", "", False),
        ("(笑)", "（笑）", False), ("ABC", "ＡＢＣ", False), ("ｱｲｳ", "アイウ", False),
    ]  # fmt: skip
    for vlm, ctc, thin in handmade:
        for conf, confs in ((0.97, [0.999] * len(ctc)), (0.3, None), (None, None)):
            r = lr.reconcile_line(vlm, ctc, len(ctc), thin=thin, ctc_conf=conf, ctc_char_confs=confs)
            cases.append({
                "vlm": vlm, "ctc": ctc, "cells": len(ctc), "thin": thin, "ctc_conf": conf, "ctc_char_confs": confs,
                "result": reconciled_state(r), "needs_second": lr.needs_second_read(r),
                "corroborates": lr.corroborates(vlm, r.text, len(ctc)),
                "overlap_repeat": [ctc, vlm, 3, lr.overlap_repeat(ctc, vlm, 3)],
            })  # fmt: skip
    for k in range(1100):
        ctc_raw = rng.choice(texts) if rng.random() < 0.9 else rand_text(rng, rng.randint(0, 6))
        if rng.random() < 0.1:
            ctc_raw = rng.choice([" ", "　", ""]) + ctc_raw + rng.choice(["", " ", "　"])
        ctc = ll.normalize_text(ctc_raw.strip()) if rng.random() < 0.8 else ctc_raw
        vlm = mutate(rng, ctc if rng.random() < 0.9 else rng.choice(texts))
        cells = rng.choice([0, len(ctc), max(1, len(ctc) // 2), rng.randint(1, 30)])
        thin = rng.random() < 0.3
        conf = rng.choice([None, round(rng.uniform(0.2, 1.0), 4), 0.95, 0.5, 0.4999])
        if rng.random() < 0.6:
            confs = [round(rng.choice([rng.uniform(0.9, 1.0), rng.uniform(0.99, 1.0), rng.uniform(0.3, 1.0)]), 4) for _ in ctc]
            if rng.random() < 0.1 and confs:
                confs = confs[:-1]
        else:
            confs = None
        r = lr.reconcile_line(vlm, ctc, cells, thin=thin, ctc_conf=conf, ctc_char_confs=confs)
        case = {
            "vlm": vlm, "ctc": ctc, "cells": cells, "thin": thin, "ctc_conf": conf, "ctc_char_confs": confs,
            "result": reconciled_state(r), "needs_second": lr.needs_second_read(r),
        }  # fmt: skip
        if rng.random() < 0.6:
            second = mutate(rng, rng.choice([ctc, vlm, r.text]))
            before = reconciled_state(r)
            settled = lr.settle_disputes(reconciled_from_state(lr, before), second, cells)
            case["second"] = second
            case["settled"] = reconciled_state(settled)
        if r.engine_only or rng.random() < 0.2:
            kw = {
                "cells": cells, "det_score": round(rng.uniform(0.4, 0.95), 4), "main": rng.uniform(0, 600),
                "thickness": rng.uniform(0, 200), "pitch": rng.choice([0.0, rng.uniform(20, 80)]),
                "neighbours": rng.randint(0, 3),
            }  # fmt: skip
            case["verdict_args"] = kw
            case["verdict"] = list(lr.engine_only_verdict(r, **kw))
        case["corroborates"] = lr.corroborates(vlm, r.text, cells)
        case["overlap_repeat"] = [ctc, vlm, rng.randint(-1, 8)]
        case["overlap_repeat"].append(lr.overlap_repeat(*case["overlap_repeat"]))
        cases.append(case)
    write_json(CASES / "reconcile_fuzz.json", cases)


def gen_pyfloat() -> None:
    rng = random.Random(31337)

    def rand_double() -> float:
        r = rng.random()
        if r < 0.3:
            return rng.uniform(-3000, 3000)
        if r < 0.5:
            return round(rng.uniform(-3000, 3000), rng.randint(0, 3))
        if r < 0.6:
            return rng.choice([0.0, -0.0, 0.5, 1.5, 2.5, 0.125, 2.675, 1e-7, 123456.789])
        if r < 0.8:
            return rng.uniform(-1, 1) * 10 ** rng.randint(-8, 18)
        return struct.unpack("<d", struct.pack("<Q", rng.getrandbits(64)))[0]

    hyp, rnd, sums, reprs, trig = [], [], [], [], []
    for _ in range(3000):
        x, y = rand_double(), rand_double()
        if math.isfinite(x) and math.isfinite(y):
            hyp.append([x, y, math.hypot(x, y)])
            if abs(x) < 1e6:
                trig.append([x, y, math.degrees(math.atan2(y, x)), math.cos(math.radians(x)), math.sin(math.radians(x)), (x * x + y * y) ** 0.5, x**2])
        n = rng.randint(0, 5)
        if math.isfinite(x) and abs(x) < 1e15:
            rnd.append([x, n, round(x, n)])
        vals = [rand_double() for _ in range(rng.randint(0, 6))]
        if all(math.isfinite(v) for v in vals):
            sums.append([vals, sum(vals)])
        if math.isfinite(x):
            reprs.append([x, repr(x)])
    space = [cp for cp in range(0x110000) if chr(cp).isspace()]
    nfkc = {}
    unassigned = []
    start = None
    for cp in range(0x110000):
        if 0xD800 <= cp <= 0xDFFF:
            continue
        ch = chr(cp)
        cat = unicodedata.category(ch)
        if cat == "Cn":
            if start is None:
                start = cp
        elif start is not None:
            unassigned.append([start, cp - 1])
            start = None
        n = unicodedata.normalize("NFKC", ch)
        if n != ch:
            nfkc[str(cp)] = n
    if start is not None:
        unassigned.append([start, 0x10FFFF])
    write_json(
        CASES / "pyfloat.json",
        {
            "unidata_version": unicodedata.unidata_version,
            "hypot": hyp, "round": rnd, "sum": sums, "repr": reprs, "trig": trig,
            "isspace": space, "nfkc": nfkc, "unassigned": unassigned,
        },  # fmt: skip
    )


def main() -> None:
    CASES.mkdir(exist_ok=True)
    gen_capture()
    gen_layout()
    _road, pages = gen_road()
    gen_sidecar(pages)
    gen_difflib()
    gen_reconcile_fuzz()
    gen_pyfloat()


if __name__ == "__main__":
    main()
