"""Line layout: raw OCR lines in, mokuro blocks in reading order out.

The PP-OCRv6 manga engine (``ppocr.py``) detects and reads LINES: every line
comes back as a rotated quad with its text. A mokuro page wants BLOCKS (a
speech bubble, a paragraph) in reading order, with furigana gone. Everything
between the two is geometry and a few script rules, and it lives here so it
can be tested without a model, an image or the network::

    {"width", "height", "lines": [{"quad", "score", "text", "conf", ...}]}
        -> measure_lines        quad -> frame, extents, angle, orientation
        -> filter_furigana      ruby removed (and returned, for a future <ruby>)
        -> find_bodies          the text body of a novel page, if there is one
        -> classify_roles       page numbers / running titles / stray glyphs
        -> merge_lines          lines -> bubbles (owocr-style, geometry only)
        -> split_paragraphs     a novel body -> one block per paragraph
        -> order_blocks         reading order
        -> build_block          the mokuro block dict
        -> grow_boxes_over_ruby the block's box takes in its lines' furigana

One engine has to serve two very different page kinds, because a light novel
is mostly dense text pages (15-20 vertical columns of ~40 characters, full of
ruby) with the occasional illustration whose lettering looks like manga.
Nothing here switches on a "page kind": a page HAS a text body when enough
long aligned columns say so, and only the lines that belong to that body get
the novel rules (wider column spacing, paragraph splitting, margins).

Every threshold is a named constant relative to the line's character size --
the THICKNESS of the line box, one em -- so the same numbers hold for a
1300 px manga scan and a 2800 px novel scan. Each constant's comment names the
evidence that set it. Two sources recur:

* "Chimahon" = the Android reader whose text placement we are matching; its
  merger is a port of owocr's (``OwOCRMerger.kt``), its furigana rule is
  ``globalFuriganaFilter``. "Manatan" = the second reference (``merge.rs``).
* "bench" = our own measurements, 2026-09-19: 70 pages of a scanned bunko
  novel (12 Kingdoms, 1925x2800, 699 ruby runs) and 32 manga pages
  (two manga series), detector side 1280, unclip 1.5.

Pure standard library on purpose: the server can import it, and so can the
engine runner inside the engines environment.

Rotation. A quad is the line's own upright frame (top-left, top-right,
bottom-right, bottom-left), so a slanted shout keeps its true angle. Pairs of
lines are always compared in a common rotated frame, never through their
axis-aligned boxes: a 2500 px novel column scanned 1 degree off has an
axis-aligned box almost twice its real thickness, which would swallow the
gutter the rules measure.

Known limits, left alone on purpose (each pinned by a test that says why):

* A grid of glyphs the DETECTOR boxed row by row -- a contents page whose
  chapter headings stand side by side as two-glyph columns comes back as two
  horizontal 4-glyph lines (bench temp00005). The layout gets no columns to
  vote on, a square grid reads either way, and only the wording tells; undoing
  it takes the recognizer (crop the columns, read them again), so it belongs
  to the engine, like ``column_pieces``.
* A short doubted label over artwork (the map's compass, "W―" 0.72). Any rule
  on confidence, script and isolation that takes it out also takes out the
  novel's section numbers ("7" 0.73, "3" 0.84, "8" 0.70).
"""

from __future__ import annotations

import math
import statistics
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from typing import Any

Point = tuple[float, float]
Spans = tuple[float, float, float, float]

# --------------------------------------------------------------------------
# Thresholds
# --------------------------------------------------------------------------

# -- orientation -----------------------------------------------------------

# A box whose long side is under 1.3x its short side does not say which way
# its text runs (a lone glyph, "!?", a two-glyph ruby run). Chimahon's native
# vote treats 1.1 as "clearly oriented" for AREA voting; per-line decisions
# need more margin because one glyph is rarely square (bench: single-glyph
# ruby boxes measure 28x37 px = 1.32).
AMBIGUOUS_ASPECT = 1.3
# Aspect above which a line takes part in the page-level area vote
# (Chimahon native ``process+8860``: h > 1.1 w).
VOTE_ASPECT = 1.1
# An ambiguous line copies the nearest unambiguous line within this many of
# its own sizes; further away it takes the page's dominant orientation.
NEIGHBOUR_REACH_EM = 3.0

# -- rotation --------------------------------------------------------------

# A min-area rectangle around 1-2 glyphs has no meaningful angle (bench:
# three-glyph ruby runs report -3..-4 degrees beside columns at 0.0). Below
# this aspect the angle is ignored and the line adopts its partner's frame.
ANGLE_RELIABLE_ASPECT = 3.0
# An unreliable angle this close to its block's is noise and is taken out of
# the written quad (see ``_settled_quad``); further off it is a real rotation.
# Bench: spurious tilts of short lines reach 9.7 degrees, the smallest real
# rotation of a short line (OPM sound effects) is 18.
SETTLE_MAX_TILT_DEG = 12.0
# Two lines with trustworthy angles merge only when they agree this closely
# (Chimahon ``shouldGroupInSameParagraph``: 0.1 rad = 5.7 degrees).
ANGLE_TOLERANCE_DEG = 6.0

# -- furigana --------------------------------------------------------------

# Ruby is set at half the base size; after the detector's unclip padding a
# ruby box measures 0.64 of its base at the median (bench, n=699). Chimahon
# uses 0.75, Manatan 0.80. Bench: the thinnest REAL kana-only lines beside a
# kanji column measured 0.81-0.84, so 0.75 keeps a margin on that side.
FURIGANA_MAX_THICKNESS_RATIO = 0.75
# Ruby hugs its base: bench gap/base-thickness p50 -0.04, p95 0.14, largest
# true ruby 0.23; the nearest real column sits at 0.24-0.27 only for
# bracketed dialogue, which the script rule already protects. Chimahon 0.30.
FURIGANA_MAX_GAP_EM = 0.30
# ...and may overlap its base's box, but its centre stays outside the base's
# inner half (Manatan allows -0.5 thickness of gap; centre-based is steadier
# for boxes of different thickness).
FURIGANA_MIN_CENTRE_OFFSET_EM = 0.35
# Share of the ruby's own length that must lie alongside the base. Chimahon
# accepts 0.05, its native engine 0.4. Bench: real ruby >= 0.81; the false
# candidates this rejects (a line that merely starts beside a column's last
# glyph) were all <= 0.21.
FURIGANA_MIN_MAIN_OVERLAP = 0.5
# The detector boxes ~25% of ruby runs generously -- as thick as the gutter
# they sit in, 0.76-1.26 of the base (bench, manga and novel alike), so
# thickness alone misses them. Two further tiers catch those; both are open
# only to boxes up to this thick and to text that reads as ruby.
FURIGANA_GENEROUS_MAX_THICKNESS_RATIO = 1.35
# Tier "small glyphs": the box may be fat but the GLYPHS are still half size,
# so the run's length per glyph is about half the base's. Bench (157 manga
# candidates already in ruby position): true ruby of 2+ glyphs <= 0.78, real
# kana lines beside a kanji line >= 0.94 (おかげで 0.95, やたら 0.98). A lone
# glyph is excluded: the detector's padding dominates its length (0.90-0.96).
FURIGANA_MAX_GLYPH_PITCH_RATIO = 0.85
# Tier "lattice": real columns sit on the body's column lattice, a ruby run
# sits well inside one pitch of its base. Bench: ruby centres 0.75-0.95 em
# from the base centre, pitch 1.2-1.6 em => ruby <= 0.68 pitch, next column
# = 1.0 pitch. Only used when the page has a column lattice (a text body),
# so manga, where touching columns are normal (bench: same-bubble gap p50
# 0.07 em), is never judged by it. This is the tier that catches a fat
# single-glyph ruby on a novel page.
FURIGANA_LATTICE_MAX_PITCH_FRACTION = 0.70
# Below this confidence a single glyph's text is not evidence of anything:
# bench ruby glyphs misread as kanji ("乳" for a kana, conf 0.24) or Latin
# ("U", 0.14) were all under 0.5, correct single glyphs above 0.9.
LOW_CONFIDENCE = 0.5
# Ruby the recognizer could not read (see ``_ruby_candidacy``): at most this
# many glyphs, and -- when the reading was confident -- at most this thick.
# Bench: misread ruby glyphs measured 0.42-0.52 of their base.
FURIGANA_UNREADABLE_MAX_GLYPHS = 2
FURIGANA_UNREADABLE_MAX_RATIO = 0.6
# ...and on a page WITHOUT a lattice (manga), where an aside in small kanji is
# possible, only when it is tiny. Thin evidence, so a tight number: on the 12
# OPM bench pages exactly one confident non-kana line of 1-2 glyphs sits in
# ruby position at all -- the tail of a ruby run read as "灯" at 0.73, 0.34 of
# the column it hugs -- and no real text does. Ruby is set at half size, so
# anything under 0.45 is smaller than ruby itself.
FURIGANA_TINY_MAX_RATIO = 0.45
# On a lattice page a run this thin against the BODY's size is a candidate
# whatever it reads and however many glyphs: bench survivors ("きよ灯", "!わた",
# "…おく", "fれ", "仇") measured 0.54-0.62 of the body em, the thinnest real
# body-size line 0.81. Place still has to say ruby (:func:`ruby_of`).
FURIGANA_LATTICE_MAX_BODY_RATIO = 0.65

# -- text body (novel pages) ------------------------------------------------

# A "long" column. Manga lines rarely exceed 12 glyphs; a bunko column holds
# ~40, one tier of a two-tier page 20-25.
LONG_LINE_EM = 12.0
# A body needs this many long columns and this many columns in all. A manga
# narration box of five long columns qualifies, and may: the body rules only
# widen the column gap to the box's own spacing and split at sentence ends.
BODY_MIN_LONG_COLUMNS = 3
BODY_MIN_COLUMNS = 5
# Columns of one body are set in one size (bench: detected thickness of body
# columns 58-71 px => 1.22; Chimahon's "same size" tier is 1.35).
BODY_SIZE_RATIO = 1.35
# A column belongs to the body (tier) it STARTS in, give or take this much.
BODY_EXTENT_SLACK_EM = 1.0
# Two tiers are cut where the share of long columns running past a point
# drops to this fraction of the peak: zero in a clean gutter, one or two
# when a marginal heading straddles both tiers.
BAND_GUTTER_COVERAGE = 0.15
# Tops (bottoms) within this of each other are "the same edge" when the
# body's top and bottom are estimated (bench: aligned tops spread 0.1 em).
BODY_EDGE_CLUSTER_EM = 0.35
# How far above the lowest column end a better-supported end is still taken
# for the body's bottom (see ``_full_column_edge``): one hanging stop, plus
# the half em a clipped final mark costs.
BODY_HANGING_REACH_EM = 1.5
# Column spacing inside a body may exceed the manga gap: allow this many
# times the body's own median column gap (bench pitch 1.2-1.6 em, so gaps of
# 0.2-0.6 em with a median near 0.4; 1.6x covers the widest seen).
BODY_GAP_FACTOR = 1.6

# -- margins and noise -------------------------------------------------------

# Page numbers and running titles live in the outer 15% of the page height,
# clear of the body's extent by a quarter em, and are short.
MARGIN_BAND_FRACTION = 0.15
MARGIN_CLEARANCE_EM = 0.25
MARGIN_MAX_LENGTH_FRACTION = 0.6
# Margins are only meaningful beside a page-scale body.
MARGIN_MIN_BODY_COVERAGE = 0.5

# A page whose ONLY "text" is a glyph or two without a kanji, read at under
# this confidence, is an illustration: bench, the novel's full-page plates gave
# "ぐ" (0.77, on a leopard's spots) and "AL" (0.84, hatching) and nothing else.
# The same glyphs on a page that has ANY other text are left alone -- they are
# manga sound effects ("ザッ" 0.69, "ず" 0.63), however large; a splash page
# with nothing but "ザッ" (0.81) and "ガッ" (0.90) keeps both.
LONE_GLYPHS_MAX = 2
LONE_GLYPHS_MIN_CONF = 0.9

# -- merger (Chimahon OwOCRMerger.shouldGroupInSameParagraph) ----------------

# Lines further apart in size than this never share a block (Chimahon's
# widest tier; Manatan's ``font_size_ratio`` default is 3.0).
MERGE_MAX_SIZE_RATIO = 2.5
# Tier 1: neighbours across the reading direction, any alignment. Bench:
# same-bubble gap p95 0.63 em; nearest cross-bubble pair 1.35 em.
MERGE_GAP_EM = 0.75
# ...halved-ish when the sizes differ a lot (Chimahon: ratio > 1.8 -> 0.50).
MERGE_GAP_MIXED_SIZE_EM = 0.50
MERGE_MIXED_SIZE_RATIO = 1.8
# Tier 2: same size and starts aligned -> a wider gap is still one bubble.
MERGE_ALIGNED_GAP_EM = 1.25
MERGE_ALIGNED_SIZE_RATIO = 1.35
MERGE_ALIGNED_START_EM = 0.3
# Tier 3: looser size, tighter gap.
MERGE_LOOSE_GAP_EM = 1.0
MERGE_LOOSE_START_EM = 0.5
# Overlap along the reading axis, as a share of the shorter line.
MERGE_MIN_MAIN_OVERLAP = 0.5
# Both ends off by more than this => two staggered bubbles, not one.
MERGE_STAGGER_EM = 2.0
# One column the detector cut in two (Chimahon ``mergeCloseParagraphs``:
# gap <= 0.5 em, cross overlap > 0.95; boxes of the two halves differ more
# than that on our detector, bench 0.8+).
STITCH_MAX_GAP_EM = 0.6
STITCH_MIN_CROSS_OVERLAP = 0.7
# A lone glyph's box is not its column's size (bench opm-066: the "パ" cut off
# the top of "ワーを自分で" measured 1.45x the column), so this is looser than
# the same-size tiers above; the collinearity test carries the weight.
STITCH_MAX_SIZE_RATIO = 1.6
# Inside a text body two pieces on the same lattice column ARE one column;
# the hole between them is a dash or ellipsis the detector skipped (bench
# page 200: "...だろう？" / "―" / "痛みなら..." with 1 em holes around a 2 em dash).
BODY_STITCH_MAX_GAP_EM = 3.0

# The detector's box around a body column is the INK's width: bench, quads of
# 60-65 px around 65-70 px glyphs, strokes of 獣 and 窒 on or outside the edge.
# The reader's touch zones were 5-10% narrower than the glyphs and the font
# size derived from the box a little small. Written quads of body columns are
# therefore widened by this share of their thickness on each side -- never by
# more than half the body's column gap, so neighbours cannot touch. (Only the
# WRITTEN quad: a wider recognizer crop destabilises the read, see
# ``ppocr.VOTE_MAX_CONF``.)
BODY_QUAD_MARGIN_EM = 0.05

# -- paragraphs --------------------------------------------------------------

# An ideographic-space indent shows as a column starting ~1 em low (bench:
# 0.75-1.0 em, against 0-0.25 for flush columns).
PARAGRAPH_INDENT_MIN_EM = 0.6
# A column that opens with a bracket starts 0.25-0.75 em low even when flush
# (bench, n=197, mode 0.5), because the glyph's ink sits in the lower half of
# its cell. That much is taken off a bracket column's start before it is
# compared with the body's top edge, so a flush 「 is not read as an indent.
BRACKET_INK_INSET_EM = 0.45
# The previous column "ends short": bench full columns end within 0.25 em of
# the body's bottom, 0.5 em when the last glyph is a period or bracket.
PARAGRAPH_SHORT_END_EM = 1.25
PARAGRAPH_SOFT_END_EM = 0.3
# Deeper than this is a quoted/inset passage, not a paragraph indent: its
# columns stay together while the inset holds.
PARAGRAPH_DEEP_INSET_EM = 1.6

# -- reading order -----------------------------------------------------------

# Blocks share a row when they overlap the row's first block by this share of
# the shorter one (Chimahon rows: 0.2).
ROW_MIN_OVERLAP = 0.2
# ...or overlap a block already in the row by half. A small first block (a
# sound effect in the corner of a panel) is a poor yardstick for the whole
# tier: bench page opm-066, the bubble at the far right of the second tier
# overlapped the "バン" that opened the row by 19% but its neighbours by 83%.
# Half is strict enough that a staircase of bubbles does not chain the page.
ROW_CHAIN_OVERLAP = 0.5
# Lines of a vertical block are in the same column when their cross centres
# are within half an em.
COLUMN_CLUSTER_EM = 0.5

OPENING_BRACKETS = "「『（(〈《【〔［[｢"
SENTENCE_ENDINGS = "。．.！？!?」』）)〉》】〕］]｣…‥―—"
# Kanji the recognizer writes for the katakana of the same shape (see
# ``fix_kana_lookalikes``).
KANA_LOOKALIKES = {"夕": "タ", "力": "カ", "卜": "ト"}
# Emphasis dots (bouten) are set in the ruby position and read as dots/commas.
BOUTEN = "・･、，,.﹅﹆丶ヽ゛゜'`"


# --------------------------------------------------------------------------
# Script rules
# --------------------------------------------------------------------------


def is_kanji(ch: str) -> bool:
    """CJK ideograph (incl. extension A, compatibility, and the 々〆 marks)."""
    o = ord(ch)
    return (
        0x4E00 <= o <= 0x9FFF
        or 0x3400 <= o <= 0x4DBF
        or 0xF900 <= o <= 0xFAFF
        or 0x20000 <= o <= 0x2FFFF
        or ch in "々〆〇"
    )


def is_kana(ch: str) -> bool:
    """Hiragana, katakana (full or half width), the prolonged-sound mark."""
    o = ord(ch)
    return 0x3041 <= o <= 0x309F or 0x30A0 <= o <= 0x30FF or 0xFF66 <= o <= 0xFF9F


def has_kanji(text: str) -> bool:
    return any(is_kanji(ch) for ch in text)


def glyph_count(text: str) -> int:
    """Visible glyphs: whitespace is a recognizer artefact inside a ruby run."""
    return sum(1 for ch in text if not ch.isspace())


def is_katakana(ch: str) -> bool:
    """Full-width katakana, including the prolonged-sound mark."""
    return 0x30A1 <= ord(ch) <= 0x30FF


def fix_kana_lookalikes(text: str) -> str:
    """Katakana the recognizer wrote as the kanji that looks the same.

    "タイル" comes back as "夕イル" (U+5915, evening): in print the two are
    the same strokes, and a CTC recognizer has no language model to choose.
    Bench: 10 such "夕" in one 292-page novel, its most frequent systematic
    misread. A lookalike is turned into its katakana only where the kanji
    cannot be meant: directly before a katakana, and not after a kanji (so
    "全力パンチ" and "入力データ" keep their 力). Numerals and 口 are left out
    on purpose -- "二メートル" and "口コミ" are real.
    """
    if not any(ch in KANA_LOOKALIKES for ch in text):
        return text
    out = list(text)
    for i, ch in enumerate(text[:-1]):
        if ch in KANA_LOOKALIKES and is_katakana(text[i + 1]):
            if i == 0 or not is_kanji(text[i - 1]):
                out[i] = KANA_LOOKALIKES[ch]
    return "".join(out)


# Hiragana the recognizer writes inside a katakana word. へ/べ/ぺ are the same
# strokes as their katakana, り nearly; き is the recognizer's own confusion and
# the most damaging -- bench, one 292-page novel: the name ケイキ came out
# "ケイき" 29 times of 100, ヒョウキ "ヒョウき", ベルト "べルト". The confidence
# does not flag it (き 1.0, べ 0.72), and a mixed-script token breaks every
# dictionary and name lookup, so context decides (see ``_katakana_for``).
HIRAGANA_LOOKALIKES = {"へ": "ヘ", "べ": "ベ", "ぺ": "ペ", "り": "リ", "き": "キ"}
# Small ャュョ only follow an i-row kana (or テ/デ/フ/ヴ in loanwords). After
# anything else the recognizer shrank a full-size one: "ジョウュウ" (bench: 14
# of 38 for that name; confidence 0.66).
SMALL_KANA = {"ャ": "ヤ", "ュ": "ユ", "ョ": "ヨ", "ゃ": "や", "ゅ": "ゆ", "ょ": "よ"}
SMALL_KANA_HOSTS = "キシチニヒミリギジヂビピテデフヴきしちにひみりぎじぢびぴてでふ"
LONG_VOWEL = "ー"
DASH = "―"


def is_hiragana(ch: str) -> bool:
    return 0x3041 <= ord(ch) <= 0x3096


def _katakana_for(text: str, i: int) -> str | None:
    """The katakana a hiragana lookalike at ``text[i]`` should be, if context says so.

    * katakana on BOTH sides: inside a word ("アへン", "スりッパ");
    * き after two katakana: the end of a name ("ケイきを"). Real き there
      follows a hiragana ("イラつき"), and one katakana proves nothing;
    * へ/べ/ぺ opening a katakana word: before a katakana and after neither a
      kanji nor a hiragana, where へ would be the particle ("学校へトラックで")
      and べ a verb ending ("食べタイ") -- or, for べ/ぺ, which are no
      particles, before two katakana whatever comes first ("革のべルト").
    """
    ch = text[i]
    prev = text[i - 1] if i > 0 else ""
    after = text[i + 1 : i + 3]
    next_kata = bool(after) and is_katakana(after[0])
    if prev and is_katakana(prev) and next_kata:
        return HIRAGANA_LOOKALIKES[ch]
    if ch == "き":
        two_before = text[max(0, i - 2) : i]
        if len(two_before) == 2 and all(is_katakana(c) for c in two_before) and not next_kata:
            return "キ"
        return None
    if ch in "へべぺ" and next_kata:
        if not (prev and (is_hiragana(prev) or is_kanji(prev))):
            return HIRAGANA_LOOKALIKES[ch]
        if ch != "へ" and not is_kanji(prev) and len(after) == 2 and is_katakana(after[1]):
            return HIRAGANA_LOOKALIKES[ch]
    return None


def _fix_dashes(text: str) -> str:
    """Runs of "ー" that can only be a dash become "―".

    A two-cell dash is one long stroke, which the recognizer reads as the
    prolonged-sound mark (bench: "何かにーそれが何かは", "―あれが来たら" for
    "――あれが..."; ``ppocr.fill_gaps`` gives the second cell back). A long
    vowel needs a kana before it, so a run that opens the line or follows a
    kanji, a stop, a bracket or a space is a dash. After a hiragana both
    exist -- "すげーー！" and "何かに――それが" -- and what follows tells them
    apart: a drawn-out vowel ends its phrase ("！", "、", the end of a bubble),
    a dash of two cells runs on into the next clause or breaks the speech off
    ("怪我だって――」"). After a katakana it is always a long vowel.
    """
    if LONG_VOWEL not in text:
        return text
    out = list(text)
    i = 0
    while i < len(text):
        if text[i] != LONG_VOWEL:
            i += 1
            continue
        j = i
        while j < len(text) and text[j] == LONG_VOWEL:
            j += 1
        prev = text[i - 1] if i > 0 else ""
        nxt = text[j] if j < len(text) else ""
        if not prev or not is_kana(prev) or prev == DASH:
            dash = True
        elif is_hiragana(prev):
            # A run that ends the line has no next glyph to classify: it is a
            # drawn-out vowel ("すげー"), and the classifiers take one character.
            runs_on = bool(nxt) and (
                is_kana(nxt) or is_kanji(nxt) or nxt.isspace() or nxt in "」』。"
            )
            dash = j - i >= 2 and runs_on
        else:
            dash = False
        if dash:
            out[i:j] = DASH * (j - i)
        i = j
    return "".join(out)


def normalize_text(text: str) -> str:
    """A line's text as written to the sidecar: the recognizer's systematic slips undone.

    A CTC recognizer has no language model, so it cannot choose between
    glyphs that look alike; each rule here is a place where the neighbouring
    characters can (``fix_kana_lookalikes``, ``HIRAGANA_LOOKALIKES``,
    ``SMALL_KANA``, :func:`_fix_dashes`). Every rule maps one character to
    one character, so the line keeps its length and its grid. The raw dump
    keeps the text as read.
    """
    text = fix_kana_lookalikes(text)
    out = list(text)
    for i, ch in enumerate(text):
        if ch in HIRAGANA_LOOKALIKES:
            out[i] = _katakana_for(text, i) or ch
        elif ch in SMALL_KANA and i > 0 and (is_kana(text[i - 1]) or is_kanji(text[i - 1])):
            if text[i - 1] not in SMALL_KANA_HOSTS:
                out[i] = SMALL_KANA[ch]
        elif ch == "—" and len(text) > 1:
            # EM DASH for HORIZONTAL BAR: the same stroke, and "―—" breaks a search.
            around = text[max(0, i - 1) : i] + text[i + 1 : i + 2]
            if not around.isascii():
                out[i] = DASH
        elif ch == " " and 0 < i < len(text) - 1:
            if not text[i - 1].isascii() and not text[i + 1].isascii():
                out[i] = "\u3000"
    return _fix_dashes("".join(out))


def is_ruby_script(text: str) -> bool:
    """Could this text be a ruby run or a run of emphasis dots?

    Ruby is kana. Anything with a kanji, a Latin letter, a digit or a bracket
    is real text however small it is printed -- that is what keeps a short
    line of dialogue such as 「ああ」 safe whatever its box measures. A comma
    or dot is tolerated because the recognizer reads the tail of a ruby run,
    and bouten, as one. A full stop is not -- "た。" is a paragraph's last
    column -- except in front: no line BEGINS with one (kinsoku), so there it
    is the neighbouring column's stop caught in a ruby run's generous box
    (bench temp00120: "。よこだお").
    """
    glyphs = [ch for ch in text.lstrip("。．") if not ch.isspace()]
    if not glyphs:
        return False
    return all(is_kana(ch) or ch in BOUTEN for ch in glyphs)


# --------------------------------------------------------------------------
# Data
# --------------------------------------------------------------------------


@dataclass
class Line:
    """One detected line, measured in its own frame.

    ``width``/``height`` are the quad's extents along its own top and side
    edges; ``thickness`` (the short one, across the reading direction) is the
    character size every threshold is expressed in.
    """

    index: int
    quad: tuple[Point, Point, Point, Point]
    text: str
    score: float
    conf: float
    width: float
    height: float
    angle: float
    vertical: bool = True
    ambiguous: bool = False
    _spans: dict[float, Spans] = field(default_factory=dict, repr=False, compare=False)

    @property
    def thickness(self) -> float:
        return self.width if self.vertical else self.height

    @property
    def length(self) -> float:
        return self.height if self.vertical else self.width

    @property
    def aspect(self) -> float:
        short = min(self.width, self.height)
        return max(self.width, self.height) / short if short > 0 else math.inf

    @property
    def angle_reliable(self) -> bool:
        return self.aspect >= ANGLE_RELIABLE_ASPECT

    @property
    def centre(self) -> Point:
        return (sum(p[0] for p in self.quad) / 4, sum(p[1] for p in self.quad) / 4)

    def spans(self, theta: float = 0.0) -> Spans:
        """``(x0, x1, y0, y1)`` of the quad in a frame turned ``theta`` degrees.

        ``theta = 0`` is the axis-aligned box. Turning the frame to a line's
        own angle makes its box tight again, which is what gaps and overlaps
        between tilted lines have to be measured in.
        """
        key = round(theta, 3)
        cached = self._spans.get(key)
        if cached is None:
            c, s = math.cos(math.radians(key)), math.sin(math.radians(key))
            xs = [p[0] * c + p[1] * s for p in self.quad]
            ys = [-p[0] * s + p[1] * c for p in self.quad]
            cached = (min(xs), max(xs), min(ys), max(ys))
            self._spans[key] = cached
        return cached

    def main_cross(self, theta: float, vertical: bool) -> Spans:
        """``(main0, main1, cross0, cross1)`` for text running ``vertical``-ly."""
        x0, x1, y0, y1 = self.spans(theta)
        return (y0, y1, x0, x1) if vertical else (x0, x1, y0, y1)


@dataclass(frozen=True)
class Ruby:
    """A removed furigana run, kept so a later format can emit ``<ruby>``.

    ``span`` is the stretch of the base line it annotates, as fractions of the
    base's length along its reading axis; ``chars`` is that stretch on the
    base text's uniform character grid (``[start, end)``) -- the same grid the
    reader lays the base line out on.
    """

    line: int
    base: int
    text: str
    quad: tuple[Point, Point, Point, Point]
    span: tuple[float, float]
    chars: tuple[int, int]


@dataclass(frozen=True)
class Body:
    """The text body of a novel page (one per tier), in its own deskewed frame."""

    vertical: bool
    theta: float
    em: float
    top: float
    bottom: float
    cross0: float
    cross1: float
    gap: float
    members: frozenset[int]


@dataclass
class PageLayout:
    """Result of :func:`layout_page`.

    ``blocks`` are plain mokuro block dicts in reading order. ``groups`` holds,
    for each block, the input line indices in the block's line order, so a
    caller can carry per-line extras (confidences, character offsets) across.
    ``kinds`` names each block's role: ``body``, ``text``, ``header``,
    ``footer`` or ``noise``. ``dropped`` lists lines that carried nothing to
    lay out (empty text or a degenerate quad).
    """

    blocks: list[dict[str, Any]]
    groups: list[list[int]]
    kinds: list[str]
    ruby: list[Ruby]
    dropped: list[int]
    bodies: list[Body]


# --------------------------------------------------------------------------
# 1. Per-line metrics and orientation
# --------------------------------------------------------------------------


def canonical_quad(quad: Sequence[Sequence[float]]) -> tuple[Point, Point, Point, Point] | None:
    """The quad as top-left, top-right, bottom-right, bottom-left of the line's frame.

    Clockwise on screen, starting at the corner whose outgoing edge points
    most nearly along +x -- i.e. the upright frame reached by the SMALLEST
    rotation, so the tilt always lands in (-45, 45]. ``ppocr.order_quad``
    already emits this order and passes through unchanged; PaddleOCR's stock
    ordering (sort by x, then y) does not, and would turn a column leaning 30
    degrees into "horizontal text at -60", which the reader would then draw
    sideways. Returns ``None`` for a degenerate quad.
    """
    if len(quad) < 4:
        return None
    pts = [(float(p[0]), float(p[1])) for p in quad[:4]]
    area2 = sum(pts[i][0] * pts[(i + 1) % 4][1] - pts[(i + 1) % 4][0] * pts[i][1] for i in range(4))
    if abs(area2) < 1e-6:
        return None
    if area2 < 0:  # counter-clockwise on screen: flip the winding, keep corner 0
        pts = [pts[0], pts[3], pts[2], pts[1]]
    best, best_dx = 0, -math.inf
    for start in range(4):
        ex = pts[(start + 1) % 4][0] - pts[start][0]
        ey = pts[(start + 1) % 4][1] - pts[start][1]
        norm = math.hypot(ex, ey)
        dx = ex / norm if norm > 0 else -math.inf
        if dx > best_dx + 1e-6:
            best, best_dx = start, dx
    a, b, c, d = (pts[(best + k) % 4] for k in range(4))
    return (a, b, c, d)


def quad_frame(quad: Sequence[Point]) -> tuple[float, float, float]:
    """``(width, height, angle_deg)`` of a canonical quad.

    Extents are the distances between opposite edge MIDPOINTS: exact for a
    rectangle, and still the sensible answer for the slightly trapezoidal
    quads a perspective-aware detector may emit (same construction the reader
    and ``engine_runner.quad_extents`` use). The angle is that of the
    left-to-right midpoint vector, positive clockwise on screen.
    """
    mids = [
        ((quad[i][0] + quad[(i + 1) % 4][0]) / 2, (quad[i][1] + quad[(i + 1) % 4][1]) / 2)
        for i in range(4)
    ]
    across = (mids[1][0] - mids[3][0], mids[1][1] - mids[3][1])
    down = (mids[2][0] - mids[0][0], mids[2][1] - mids[0][1])
    angle = math.degrees(math.atan2(across[1], across[0]))
    return math.hypot(*across), math.hypot(*down), angle


def measure_lines(raw_lines: Sequence[Mapping[str, Any]]) -> tuple[list[Line], list[int]]:
    """Measure every raw line; returns ``(lines, dropped_indices)``.

    A line without text or with a degenerate quad has nothing to lay out and
    is dropped here, once, so every later stage can rely on real geometry.
    ``Line.vertical`` holds the geometric first guess (taller than wide);
    :func:`decide_orientations` settles the ambiguous ones.
    """
    lines: list[Line] = []
    dropped: list[int] = []
    for index, raw in enumerate(raw_lines):
        line = _measure(index, raw)
        if line is None or not line.text:
            dropped.append(index)
        else:
            lines.append(line)
    return lines, dropped


def _measure(index: int, raw: Mapping[str, Any]) -> Line | None:
    """One raw line measured, ``None`` for a degenerate quad. Text may be empty."""
    quad = canonical_quad(raw.get("quad") or ())
    if quad is None:
        return None
    width, height, angle = quad_frame(quad)
    if width <= 0 or height <= 0:
        return None
    return Line(
        index=index,
        quad=quad,
        text=str(raw.get("text") or "").strip(),
        score=float(raw.get("score") or 0.0),
        conf=float(raw.get("conf") or 0.0),
        width=width,
        height=height,
        angle=angle,
        vertical=height > width,
    )


def is_ambiguous(line: Line) -> bool:
    """Does the box leave the reading direction open?

    Aspect AND text: a lone glyph never says (a "一" is wide and flat inside a
    vertical column, a "ー" tall and thin inside a horizontal one), and any
    near-square box is undecided whatever it holds.
    """
    return glyph_count(line.text) <= 1 or line.aspect < AMBIGUOUS_ASPECT


def dominant_vertical(lines: Iterable[Line]) -> bool:
    """Page-level orientation by AREA vote, the way Chimahon's native code does.

    Area rather than count, so one long paragraph outweighs a handful of
    sound effects. A page with no clear line defaults to vertical: this is a
    Japanese reader.
    """
    vertical_area = horizontal_area = 0.0
    for line in lines:
        area = line.width * line.height
        if line.height > VOTE_ASPECT * line.width:
            vertical_area += area
        elif line.width > VOTE_ASPECT * line.height:
            horizontal_area += area
    return vertical_area >= horizontal_area


def _box_distance(a: Line, b: Line) -> float:
    ax0, ax1, ay0, ay1 = a.spans()
    bx0, bx1, by0, by1 = b.spans()
    dx = max(0.0, max(ax0, bx0) - min(ax1, bx1))
    dy = max(0.0, max(ay0, by0) - min(ay1, by1))
    return math.hypot(dx, dy)


def decide_orientations(lines: Sequence[Line]) -> None:
    """Settle ``vertical`` for ambiguous lines from their neighbours.

    A one- or two-glyph line is laid out like the text around it: it copies
    the nearest clearly-oriented line within ``NEIGHBOUR_REACH_EM`` of its own
    size, and failing that the page's dominant orientation.
    """
    clear = [ln for ln in lines if not is_ambiguous(ln)]
    page_vertical = dominant_vertical(clear)
    for line in lines:
        line.ambiguous = is_ambiguous(line)
        if not line.ambiguous:
            continue
        reach = NEIGHBOUR_REACH_EM * max(line.width, line.height)
        best: tuple[float, int] | None = None
        for other in clear:
            dist = _box_distance(line, other)
            if dist <= reach and (best is None or (dist, other.index) < best):
                best = (dist, other.index)
                line.vertical = other.vertical
        if best is None:
            line.vertical = page_vertical


# --------------------------------------------------------------------------
# Pair geometry
# --------------------------------------------------------------------------


def pair_theta(a: Line, b: Line) -> float | None:
    """Common frame angle for comparing two lines, ``None`` if they disagree.

    The longer line with a trustworthy angle sets the frame; a short line
    (whose min-area rectangle could have landed at any angle) follows it.
    """
    if a.angle_reliable and b.angle_reliable:
        if abs(a.angle - b.angle) > ANGLE_TOLERANCE_DEG:
            return None
        return a.angle if max(a.width, a.height) >= max(b.width, b.height) else b.angle
    if a.angle_reliable:
        return a.angle
    if b.angle_reliable:
        return b.angle
    return 0.0


def _overlap(a0: float, a1: float, b0: float, b1: float) -> float:
    """Signed overlap of two intervals: negative = the gap between them."""
    return min(a1, b1) - max(a0, b0)


# --------------------------------------------------------------------------
# 2. Furigana
# --------------------------------------------------------------------------


def column_lattice(lines: Sequence[Line]) -> tuple[float, float] | None:
    """``(pitch, em)`` of the page's column lattice, ``None`` when it has none.

    The pitch is the median centre-to-centre distance of neighbouring body
    columns (or rows), the em their median thickness. Long lines establish
    that there is a body and what its size and frame are; the pitch is then
    taken over every same-size line that cannot itself be ruby, because a
    page of short dialogue may have few long columns and none of them
    adjacent (bench page 200: six long columns, one adjacent pair).
    """
    long_lines = [ln for ln in lines if ln.length >= LONG_LINE_EM * ln.thickness]
    if not long_lines:
        return None
    vertical = dominant_vertical(long_lines)
    long_lines = [ln for ln in long_lines if ln.vertical == vertical]
    if len(long_lines) < BODY_MIN_LONG_COLUMNS:
        return None
    theta = statistics.median(ln.angle for ln in long_lines)
    em = statistics.median(ln.thickness for ln in long_lines)
    centres = sorted(
        (c0 + c1) / 2
        for ln in lines
        if ln.vertical == vertical and not is_ruby_script(ln.text)
        for _, _, c0, c1 in [ln.main_cross(theta, vertical)]
        if max(c1 - c0, em) / min(c1 - c0, em) <= BODY_SIZE_RATIO
    )
    steps = [
        b - a for a, b in zip(centres, centres[1:], strict=False) if 0.5 * em < b - a < 3.0 * em
    ]
    if len(steps) < BODY_MIN_LONG_COLUMNS:
        return None
    return statistics.median(steps), em


def column_pitch(lines: Sequence[Line]) -> float | None:
    """The lattice's pitch alone (see :func:`column_lattice`)."""
    lattice = column_lattice(lines)
    return lattice[0] if lattice is not None else None


def _ruby_candidacy(line: Line, lattice_em: float | None) -> float | None:
    """The thickness ratio this line must stay under to be ruby; ``None`` = never.

    Ruby is kana, so a line that reads as kana (or bouten) is a candidate at
    the ordinary ratio. Ruby glyphs are also the smallest print on the page
    and the recognizer misreads them: one or two glyphs read with low
    confidence prove nothing by their text and stay candidates. On a page
    with a column lattice even a confident misreading does (bench: a ruby
    glyph read as "仇" at 0.71) -- there, one or two glyphs of at most 0.6 em
    wedged against a column cannot be body text. Without a lattice (manga) an
    aside in small kanji is possible, so there only a tiny box qualifies.
    ``lattice_em`` is the body's glyph size on a lattice page: a run far
    thinner than that is a candidate on size alone
    (``FURIGANA_LATTICE_MAX_BODY_RATIO``).
    """
    if is_ruby_script(line.text):
        return FURIGANA_MAX_THICKNESS_RATIO
    lattice = lattice_em is not None
    if lattice_em is not None and line.thickness <= FURIGANA_LATTICE_MAX_BODY_RATIO * lattice_em:
        return FURIGANA_MAX_THICKNESS_RATIO
    if glyph_count(line.text) > FURIGANA_UNREADABLE_MAX_GLYPHS:
        return None
    if line.conf < LOW_CONFIDENCE:
        return FURIGANA_MAX_THICKNESS_RATIO
    return FURIGANA_UNREADABLE_MAX_RATIO if lattice else FURIGANA_TINY_MAX_RATIO


def ruby_of(
    cand: Line,
    base: Line,
    pitch: float | None = None,
    max_ratio: float = FURIGANA_MAX_THICKNESS_RATIO,
) -> tuple[float, float] | None:
    """If ``cand`` is furigana for ``base``: the annotated span, as base fractions.

    Judged in the base line's frame. Position first: on the RIGHT of a
    vertical base / ABOVE a horizontal one, within a small gap of it, and
    alongside it for most of its own length. Then size, by any of three tiers
    (see the constants): the box is thin (``max_ratio`` of the base); or the
    box is generous but its glyphs are small (length per glyph); or, on a
    page with a column lattice (``pitch``), it sits in the gutter well inside
    one pitch of its base. Which lines may be candidates or bases at all is a
    script question and the caller's (:func:`filter_furigana`).
    """
    theta = base.angle if base.angle_reliable else 0.0
    vertical = base.vertical
    bm0, bm1, bc0, bc1 = base.main_cross(theta, vertical)
    cm0, cm1, cc0, cc1 = cand.main_cross(theta, vertical)
    base_t = bc1 - bc0
    cand_t = cc1 - cc0
    if base_t <= 0 or cand_t <= 0:
        return None
    ratio = cand_t / base_t
    # Ruby side: +x of a vertical base, -y of a horizontal one.
    side = 1.0 if vertical else -1.0
    offset = side * ((cc0 + cc1) / 2 - (bc0 + bc1) / 2)
    if offset < FURIGANA_MIN_CENTRE_OFFSET_EM * base_t:
        return None
    gap = (cc0 - bc1) if vertical else (bc0 - cc1)
    if gap >= FURIGANA_MAX_GAP_EM * base_t:
        return None
    if _overlap(bm0, bm1, cm0, cm1) < FURIGANA_MIN_MAIN_OVERLAP * (cm1 - cm0):
        return None
    thin = ratio <= max_ratio
    # The two tiers below lean on the candidate's TEXT (its glyph count, its
    # being kana), so they are closed to a candidate admitted as unreadable.
    readable = is_ruby_script(cand.text)
    generous_box = readable and ratio <= FURIGANA_GENEROUS_MAX_THICKNESS_RATIO
    glyphs, base_glyphs = glyph_count(cand.text), glyph_count(base.text)
    small_glyphs = (
        generous_box
        and glyphs >= 2
        and base_glyphs >= 1
        and (cm1 - cm0) / glyphs <= FURIGANA_MAX_GLYPH_PITCH_RATIO * (bm1 - bm0) / base_glyphs
    )
    on_lattice_gap = (
        generous_box and pitch is not None and offset <= FURIGANA_LATTICE_MAX_PITCH_FRACTION * pitch
    )
    if not (thin or small_glyphs or on_lattice_gap):
        return None
    length = bm1 - bm0
    start = min(max((cm0 - bm0) / length, 0.0), 1.0)
    end = min(max((cm1 - bm0) / length, 0.0), 1.0)
    return (start, end)


def filter_furigana(lines: Sequence[Line]) -> tuple[list[Line], list[Ruby]]:
    """Split ``lines`` into kept lines and removed ruby runs.

    Script first (Chimahon ``globalFuriganaFilter``, Manatan ``merge.rs``):
    the candidate holds no kanji -- kana and bouten only -- and the base line
    does hold kanji, because ruby glosses kanji. A full-size kana-only line
    such as 「ああ」 fails the script test (brackets) and the geometry test
    (full thickness, a whole column gap away) and stays. Among several
    possible bases the closest wins, so the ruby record names the line it
    really annotates.
    """
    lattice = column_lattice(lines)
    pitch, lattice_em = lattice if lattice is not None else (None, None)
    limits = {ln.index: _ruby_candidacy(ln, lattice_em) for ln in lines}
    ruby: dict[int, Ruby] = {}
    # Pass 1: bases are lines that could never be ruby themselves. Pass 2: a
    # short kanji line that pass 1 kept (it was a candidate only because it is
    # short) is real text after all, and may carry a reading of its own.
    for final_pass in (False, True):
        bases = [
            ln
            for ln in lines
            if has_kanji(ln.text)
            and ln.index not in ruby
            and (limits[ln.index] is None or final_pass)
        ]
        for cand in lines:
            limit = limits[cand.index]
            if limit is None or cand.index in ruby:
                continue
            best: tuple[float, int, Line, tuple[float, float]] | None = None
            for base in bases:
                if base.index == cand.index:
                    continue
                span = ruby_of(cand, base, pitch, limit)
                if span is None:
                    continue
                dist = _box_distance(cand, base)
                if best is None or (dist, base.index) < best[:2]:
                    best = (dist, base.index, base, span)
            if best is None:
                continue
            _, _, base_line, span = best
            n = len(base_line.text)
            chars = (min(n, math.floor(span[0] * n + 1e-6)), min(n, math.ceil(span[1] * n - 1e-6)))
            ruby[cand.index] = Ruby(cand.index, base_line.index, cand.text, cand.quad, span, chars)
    kept = [ln for ln in lines if ln.index not in ruby]
    return kept, [ruby[k] for k in sorted(ruby)]


# --------------------------------------------------------------------------
# 3. Text bodies, margins, noise
# --------------------------------------------------------------------------


def _text_start(line: Line, theta: float, vertical: bool) -> float:
    """Where the line's first CELL starts, which for a bracket is above its ink."""
    start = line.main_cross(theta, vertical)[0]
    if line.text[:1] in OPENING_BRACKETS:
        start -= BRACKET_INK_INSET_EM * line.thickness
    return start


def _supported_edge(values: Sequence[float], window: float, *, lowest: bool) -> float:
    """The outermost edge that at least two lines agree on.

    A body's top is where its flush columns start -- not the single column a
    stray mark stretched upward. Take the outermost value that has company
    within ``window`` and return the median of that cluster.

    (The top also trusts any single LONG column: on a page of short dialogue
    one column may be the only one to start flush -- bench page 200 -- and a
    long column is the detector's most reliable output. The bottom has its
    own rule, :func:`_full_column_edge`.)
    """
    ordered = sorted(values, reverse=not lowest)
    need = 2 if len(ordered) >= 2 else 1
    for value in ordered:
        cluster = [v for v in ordered if abs(v - value) <= window]
        if len(cluster) >= need:
            return statistics.median(cluster)
    return ordered[0]


def _full_column_edge(ends: Sequence[float], window: float, reach: float) -> float:
    """Where the body's FULL columns end: the best-supported end near the last one.

    Not simply the lowest end. Japanese setting lets a closing stop hang below
    the last cell (burasage), so the single lowest column is often a full
    column PLUS its hanging "。", up to an em below where every other full
    column stops. Taking it as the bottom made ordinary full columns look an
    em short, and one whose final "、" the detector had clipped crossed
    ``PARAGRAPH_SHORT_END_EM`` and split its paragraph mid-sentence (bench
    page 250: "…手に馴染んだ剣が、" | "切っ先を上げるのさえ…"). Among the ends
    within ``reach`` of the lowest, the one most others agree with (within
    ``window``) wins; ties go to the lower. A lone column that reaches the
    bottom of a page of short dialogue is still found: it has no competitor
    within reach.
    """
    lowest = max(ends)
    near = [v for v in ends if lowest - v <= reach]
    best = max(near, key=lambda v: (sum(1 for u in near if abs(u - v) <= window), v))
    return statistics.median(u for u in near if abs(u - best) <= window)


def _coverage_bands(extents: Sequence[tuple[float, float]]) -> list[tuple[float, float]]:
    """Stretches of the reading axis that many long columns cover at once.

    The gutter of a two-tier page is where almost no column runs. "Almost":
    a tall heading in the margin may straddle both tiers, so the cut is where
    coverage falls to ``BAND_GUTTER_COVERAGE`` of its peak (and never above
    one line on a small page), not only where it reaches zero.
    """
    events = sorted([(m0, 1) for m0, _ in extents] + [(m1, -1) for _, m1 in extents])
    peak = level = 0
    for _, step in events:
        level += step
        peak = max(peak, level)
    floor = max(1.0, BAND_GUTTER_COVERAGE * peak) if peak > 2 else 0.0
    bands: list[tuple[float, float]] = []
    level, start = 0, None
    for position, step in events:
        level += step
        if start is None and level > floor:
            start = position
        elif start is not None and level <= floor:
            bands.append((start, position))
            start = None
    return bands


def find_bodies(lines: Sequence[Line]) -> list[Body]:
    """Text bodies of the page: bands of long, aligned, same-size columns.

    Long lines are banded by how they cover the reading axis, so a two-tier
    (上下二段) page yields two bodies, upper first, and an ordinary page one.
    Each band is then filled with every same-size line that starts inside it
    (short last lines of paragraphs, one-line dialogue). Everything is
    measured in the band's own frame -- the median angle of its long lines --
    so a page scanned a degree off still has a flat top edge: over a 1700 px
    body one degree is half an em, the size of the indent we look for.
    """
    bodies: list[Body] = []
    for vertical in (True, False):
        pool = [ln for ln in lines if ln.vertical == vertical]
        long_lines = [ln for ln in pool if ln.length >= LONG_LINE_EM * ln.thickness]
        if len(long_lines) < BODY_MIN_LONG_COLUMNS:
            continue
        theta = statistics.median(ln.angle for ln in long_lines)
        for b0, b1 in _coverage_bands([ln.main_cross(theta, vertical)[:2] for ln in long_lines]):
            band = [
                ln for ln in long_lines if b0 <= sum(ln.main_cross(theta, vertical)[:2]) / 2 <= b1
            ]
            if len(band) < BODY_MIN_LONG_COLUMNS:
                continue
            em = statistics.median(ln.thickness for ln in band)
            slack = BODY_EXTENT_SLACK_EM * em
            members = []
            for line in pool:
                start = line.main_cross(theta, vertical)[0]
                ratio = max(line.thickness, em) / min(line.thickness, em)
                if ratio <= BODY_SIZE_RATIO and b0 - slack <= start <= b1:
                    members.append(line)
            if len(members) < BODY_MIN_COLUMNS:
                continue
            spans = [ln.main_cross(theta, vertical) for ln in members]
            window = BODY_EDGE_CLUSTER_EM * em
            by_cross = sorted(spans, key=lambda s: s[2])
            gaps = [
                b[2] - a[3]
                for a, b in zip(by_cross, by_cross[1:], strict=False)
                if 0 < b[2] - a[3] < 2.5 * em
            ]
            bodies.append(
                Body(
                    vertical=vertical,
                    theta=theta,
                    em=em,
                    top=min(
                        _supported_edge(
                            [_text_start(ln, theta, vertical) for ln in members],
                            window,
                            lowest=True,
                        ),
                        min(_text_start(ln, theta, vertical) for ln in band),
                    ),
                    bottom=_full_column_edge(
                        [s[1] for s in spans], window, BODY_HANGING_REACH_EM * em
                    ),
                    cross0=min(s[2] for s in spans),
                    cross1=max(s[3] for s in spans),
                    gap=statistics.median(gaps) if gaps else 0.0,
                    members=frozenset(ln.index for ln in members),
                )
            )
    bodies.sort(key=lambda b: (b.top if b.vertical else b.cross0, b.cross0))
    return bodies


def _body_y_extent(body: Body) -> tuple[float, float]:
    return (body.top, body.bottom) if body.vertical else (body.cross0, body.cross1)


def _is_lone_doubt(line: Line) -> bool:
    return (
        glyph_count(line.text) <= LONE_GLYPHS_MAX
        and not has_kanji(line.text)
        and line.conf < LONE_GLYPHS_MIN_CONF
    )


def classify_roles(
    lines: Sequence[Line], bodies: Sequence[Body], page_height: float, page_width: float
) -> dict[int, str]:
    """Role of every line: ``text``, ``header``, ``footer`` or ``noise``.

    Nothing is deleted -- a reader may want the page number, and a "noise"
    glyph may be the one the recognizer got right. The role only fences a
    line off: margins and noise become blocks of their own and never enter
    the body's reading order mid-flow.

    * ``noise``: a line read with low confidence. Bench (272 novel text
      pages, 7300 lines): 9 lines of two or more glyphs read under 0.5, every
      one a misread ruby run or a smudge ("刂忍" 0.19, "ね心" 0.40); on the
      illustration pages and the cover the same rule takes out "男達" (the
      publisher's grape logo, 0.21), "H:" and "ド!" (hatching). Real text of
      one glyph does fall under it -- a lone "―" reads at 0.40 -- which is why
      noise may still rejoin its column (see :func:`merge_lines`).
    * ``noise`` again: when all a page has to say is ONE doubted glyph or two
      without a kanji, the page is artwork and that is its hatching (see
      ``LONE_GLYPHS_MAX``).
    * ``header``/``footer``: on a page with a page-scale body, a short line in
      the outer band of the page, clear of the body's vertical extent -- the
      nombre and the running title.
    """
    roles = {ln.index: "text" for ln in lines}
    for line in lines:
        if line.conf < LOW_CONFIDENCE:
            roles[line.index] = "noise"
    readable = [ln for ln in lines if roles[ln.index] == "text"]
    if len(readable) == 1 and _is_lone_doubt(readable[0]):
        roles[readable[0].index] = "noise"
    if not bodies or page_height <= 0:
        return roles
    extents = [_body_y_extent(b) for b in bodies]
    y_top = min(e[0] for e in extents)
    y_bottom = max(e[1] for e in extents)
    if (y_bottom - y_top) < MARGIN_MIN_BODY_COVERAGE * page_height:
        return roles
    em = statistics.median(b.em for b in bodies)
    theta = bodies[0].theta
    in_body = set().union(*(b.members for b in bodies))
    for line in lines:
        if roles[line.index] != "text" or line.index in in_body:
            continue
        if line.length > MARGIN_MAX_LENGTH_FRACTION * page_width:
            continue
        _, _, y0, y1 = line.spans(theta)
        centre = (y0 + y1) / 2
        clearance = MARGIN_CLEARANCE_EM * em
        if y1 <= y_top - clearance and centre <= MARGIN_BAND_FRACTION * page_height:
            roles[line.index] = "header"
        elif y0 >= y_bottom + clearance and centre >= (1 - MARGIN_BAND_FRACTION) * page_height:
            roles[line.index] = "footer"
    return roles


# --------------------------------------------------------------------------
# 4. Lines -> blocks
# --------------------------------------------------------------------------


def _is_stitch(
    ta: float, tb: float, cross_overlap: float, main_gap: float, body_gap: float | None
) -> bool:
    """One column the detector cut in two: same size, stacked along the reading axis."""
    em = max(ta, tb)
    limit = STITCH_MAX_GAP_EM if body_gap is None else BODY_STITCH_MAX_GAP_EM
    return (
        em / min(ta, tb) <= STITCH_MAX_SIZE_RATIO
        and cross_overlap >= STITCH_MIN_CROSS_OVERLAP * min(ta, tb)
        and main_gap <= limit * em
    )


def is_column_piece(a: Line, b: Line, body_gap: float | None = None) -> bool:
    """Are ``a`` and ``b`` two pieces of ONE column (row) the detector cut apart?"""
    theta = pair_theta(a, b)
    if a.vertical != b.vertical or theta is None:
        return False
    am0, am1, ac0, ac1 = a.main_cross(theta, a.vertical)
    bm0, bm1, bc0, bc1 = b.main_cross(theta, a.vertical)
    if ac1 <= ac0 or bc1 <= bc0:
        return False
    return _is_stitch(
        ac1 - ac0,
        bc1 - bc0,
        _overlap(ac0, ac1, bc0, bc1),
        -_overlap(am0, am1, bm0, bm1),
        body_gap,
    )


def column_pieces(page: Mapping[str, Any]) -> list[list[int]]:
    """Groups of raw line indices that are pieces of ONE printed column (row).

    The detector cuts a column where it sees no ink worth boxing: at a long
    dash ("…だろう？" / "―" / "痛みなら…" on bench page 200), or between a first
    glyph and the rest ("パ" + "ワーを自分で" on opm-066, where the "パ" box came
    back tilted and was then read as nothing). The pieces are found here, by
    the same collinearity test the merger uses; whoever holds the page image
    can read each group again as one line (``ppocr.PPOcr.join_lines``), which
    gives the reader one quad per printed line and gives the recognizer the
    dash back.

    Lines the recognizer read as NOTHING take part too -- they are dropped
    from the layout otherwise -- but only as pieces of a line that has text,
    in that line's orientation. Ruby never joins anything. Groups and their
    members come back in ascending index order.
    """
    raw_lines = page.get("lines") or []
    lines, dropped = measure_lines(raw_lines)
    decide_orientations(lines)
    kept, _ = filter_furigana(lines)
    blanks = [ln for ln in (_measure(i, raw_lines[i]) for i in dropped) if ln is not None]
    body_of: dict[int, Body] = {}
    for body in find_bodies(kept):
        for index in body.members:
            body_of.setdefault(index, body)

    parent = {ln.index: ln.index for ln in (*kept, *blanks)}

    def find(i: int) -> int:
        while parent[i] != i:
            parent[i] = parent[parent[i]]
            i = parent[i]
        return i

    for i, a in enumerate(kept):
        body_a = body_of.get(a.index)
        for b in kept[i + 1 :]:
            shared = body_a is not None and body_a is body_of.get(b.index)
            if is_column_piece(a, b, body_a.gap if body_a is not None and shared else None):
                parent[max(find(a.index), find(b.index))] = min(find(a.index), find(b.index))
        for blank in blanks:
            blank.vertical = a.vertical
            if is_column_piece(a, blank):
                parent[max(find(a.index), find(blank.index))] = min(
                    find(a.index), find(blank.index)
                )
    groups: dict[int, list[int]] = {}
    for index in sorted(parent):
        groups.setdefault(find(index), []).append(index)
    return [members for _, members in sorted(groups.items()) if len(members) > 1]


def should_merge(
    a: Line, b: Line, body_gap: float | None = None, body_top: float | None = None
) -> bool:
    """Do two lines belong to the same block? Geometry only.

    Chimahon's ``shouldGroupInSameParagraph`` with every size in ems: same
    orientation, compatible size, agreeing angle, side by side across the
    reading direction with a small gap and a real overlap along it. Three
    tiers trade gap against evidence (similar size, aligned starts). Two
    lines whose starts AND ends are both far apart are staggered bubbles and
    stay apart even when they touch. ``body_gap`` is set when both lines are
    columns of one text body: their spacing is then the body's own, and
    alignment proves nothing (an indented first column, a short last one).
    ``body_top`` is that body's top edge: a column's indent is part of the
    column, so inside a body the overlap is measured from there. Otherwise a
    paragraph's two-glyph last column ("た。") overlaps the indented column
    before it by less than half of itself, and a verb is cut in two blocks
    (bench: 21 times in one novel). Where paragraphs begin is not decided
    here but by :func:`split_paragraphs`, which sees the whole body.

    Chimahon's density gate (box areas vs their union) is deliberately not
    ported: a 5-glyph last column beside a 40-glyph one fails it by
    construction, which would shred every novel paragraph.
    """
    if a.vertical != b.vertical:
        return False
    theta = pair_theta(a, b)
    if theta is None:
        return False
    vertical = a.vertical
    am0, am1, ac0, ac1 = a.main_cross(theta, vertical)
    bm0, bm1, bc0, bc1 = b.main_cross(theta, vertical)
    ta, tb = ac1 - ac0, bc1 - bc0
    if ta <= 0 or tb <= 0:
        return False
    em = max(ta, tb)
    ratio = em / min(ta, tb)
    if ratio > MERGE_MAX_SIZE_RATIO:
        return False
    gap = -_overlap(ac0, ac1, bc0, bc1)
    main_overlap = _overlap(am0, am1, bm0, bm1)

    if _is_stitch(ta, tb, -gap, -main_overlap, body_gap):
        return True

    if body_gap is not None and body_top is not None:
        main_overlap = _overlap(min(am0, body_top), am1, min(bm0, body_top), bm1)
    if main_overlap < MERGE_MIN_MAIN_OVERLAP * min(am1 - am0, bm1 - bm0):
        return False
    if body_gap is not None:
        return gap < max(MERGE_GAP_EM * em, BODY_GAP_FACTOR * body_gap)
    start_diff = abs(am0 - bm0)
    end_diff = abs(am1 - bm1)
    if start_diff > MERGE_STAGGER_EM * em and end_diff > MERGE_STAGGER_EM * em:
        return False
    mean_t = (ta + tb) / 2
    tier1 = MERGE_GAP_MIXED_SIZE_EM if ratio > MERGE_MIXED_SIZE_RATIO else MERGE_GAP_EM
    if gap < tier1 * mean_t:
        return True
    if (
        ratio < MERGE_ALIGNED_SIZE_RATIO
        and gap < MERGE_ALIGNED_GAP_EM * mean_t
        and start_diff < MERGE_ALIGNED_START_EM * em
    ):
        return True
    return gap < MERGE_LOOSE_GAP_EM * mean_t and start_diff < MERGE_LOOSE_START_EM * em


def merge_lines(
    lines: Sequence[Line],
    bodies: Sequence[Body] = (),
    roles: Mapping[int, str] | None = None,
) -> list[list[Line]]:
    """Group lines into blocks (union-find over :func:`should_merge`).

    Lines of different roles never merge, so a page number cannot be pulled
    into the column above it. A "noise" glyph merges only as a collinear piece
    of a column -- the detector sometimes cuts the first glyph off a column
    and the recognizer then doubts it (bench opm-066: "パ" + "ワーを自分で"). Groups come back
    in a deterministic order (by lowest input index); reading order is
    :func:`order_blocks`' job.
    """
    roles = roles or {}
    parent = list(range(len(lines)))

    def find(i: int) -> int:
        while parent[i] != i:
            parent[i] = parent[parent[i]]
            i = parent[i]
        return i

    body_of: dict[int, Body] = {}
    for body in bodies:
        for index in body.members:
            body_of.setdefault(index, body)
    for i, a in enumerate(lines):
        role_a = roles.get(a.index, "text")
        for j in range(i + 1, len(lines)):
            b = lines[j]
            role_b = roles.get(b.index, "text")
            body_a = body_of.get(a.index)
            shared = body_a is not None and body_a is body_of.get(b.index)
            body_gap = body_a.gap if body_a is not None and shared else None
            body_top = body_a.top if body_a is not None and shared else None
            if "noise" in (role_a, role_b):
                # A doubtful glyph joins a block only as a piece of its column.
                joined = {role_a, role_b} == {"noise", "text"} and is_column_piece(a, b, body_gap)
            else:
                joined = role_a == role_b and should_merge(a, b, body_gap, body_top)
            if joined:
                ri, rj = find(i), find(j)
                if ri != rj:
                    parent[max(ri, rj)] = min(ri, rj)
    groups: dict[int, list[Line]] = {}
    for i, line in enumerate(lines):
        groups.setdefault(find(i), []).append(line)
    return [groups[k] for k in sorted(groups)]


# --------------------------------------------------------------------------
# 5/6. Line order inside a block, paragraphs
# --------------------------------------------------------------------------


def block_theta(group: Sequence[Line]) -> float:
    """Frame of a block: the median angle of its lines that have one."""
    angles = [ln.angle for ln in group if ln.angle_reliable]
    return statistics.median(angles) if angles else 0.0


def cluster_columns(group: Sequence[Line], theta: float | None = None) -> list[list[Line]]:
    """The block's lines as columns (rows, for horizontal text) in reading order.

    Vertical: columns right to left, pieces of one column top to bottom.
    Horizontal: rows top to bottom, pieces left to right. Lines whose cross
    centres lie within half an em are one column -- the pieces of a column
    the detector cut in two.
    """
    if not group:
        return []
    theta = block_theta(group) if theta is None else theta
    vertical = group[0].vertical
    flow = -1.0 if vertical else 1.0
    keyed = []
    for line in group:
        m0, _, c0, c1 = line.main_cross(theta, vertical)
        keyed.append((flow * (c0 + c1) / 2, m0, line.index, line))
    keyed.sort(key=lambda k: k[:3])
    columns: list[list[tuple[float, float, int, Line]]] = []
    for item in keyed:
        if columns and abs(item[0] - columns[-1][0][0]) <= COLUMN_CLUSTER_EM * item[3].thickness:
            columns[-1].append(item)
        else:
            columns.append([item])
    return [[it[3] for it in sorted(col, key=lambda k: (k[1], k[2]))] for col in columns]


def order_lines(group: Sequence[Line]) -> list[Line]:
    """Lines of one block in reading order."""
    return [line for column in cluster_columns(group) for line in column]


def split_paragraphs(group: Sequence[Line], body: Body) -> list[list[Line]]:
    """Split a merged body block at paragraph starts; lines come back ordered.

    Walking the columns in reading order, a column starts a paragraph when

    * it is indented -- starts about one em below the body's top edge (the
      ideographic space the detector cannot see, only skip);
    * the previous column ended well short of the body's bottom edge;
    * it opens flush with a bracket and the previous column had ended on a
      sentence-final glyph just short of the bottom: dialogue is set flush
      in most bunko, so there is no indent to see.

    A deeper inset that the previous column shares is a quoted passage, which
    stays together. A page that is one running paragraph stays one block.
    """
    columns = cluster_columns(group, body.theta)
    paragraphs: list[list[Line]] = []
    prev_inset = 0.0
    prev_short = 0.0
    prev_text = ""
    for k, column in enumerate(columns):
        spans = [ln.main_cross(body.theta, body.vertical) for ln in column]
        inset = (_text_start(column[0], body.theta, body.vertical) - body.top) / body.em
        short = (body.bottom - max(s[1] for s in spans)) / body.em
        text = "".join(ln.text for ln in column)
        if k == 0 or prev_short >= PARAGRAPH_SHORT_END_EM:
            start = True
        elif inset >= PARAGRAPH_DEEP_INSET_EM:
            start = abs(inset - prev_inset) > PARAGRAPH_INDENT_MIN_EM
        elif inset >= PARAGRAPH_INDENT_MIN_EM:
            start = True
        else:
            start = (
                text[:1] in OPENING_BRACKETS
                and prev_short >= PARAGRAPH_SOFT_END_EM
                and prev_text[-1:] in SENTENCE_ENDINGS
            )
        if start:
            paragraphs.append([])
        paragraphs[-1].extend(column)
        prev_inset, prev_short, prev_text = inset, short, text
    return paragraphs


# --------------------------------------------------------------------------
# 6. Block reading order
# --------------------------------------------------------------------------


def _group_box(group: Sequence[Line]) -> Spans:
    spans = [ln.spans() for ln in group]
    return (
        min(s[0] for s in spans),
        max(s[1] for s in spans),
        min(s[2] for s in spans),
        max(s[3] for s in spans),
    )


def order_blocks(
    groups: Sequence[Sequence[Line]],
    kinds: Sequence[str],
    bodies: Sequence[Body] = (),
) -> list[int]:
    """Reading order of the blocks, as indices into ``groups``.

    Headers first, footers and noise last -- never inside the flow. The flow
    itself is read in ROWS: the topmost unread block opens a row, every block
    overlapping it (or, strongly, a block already in it) joins, and the row is
    read right to left when
    its text is mostly vertical, left to right otherwise; then the next row.
    A novel body is one row (all its paragraphs overlap), so it comes out
    strictly right to left; a manga page comes out tier by tier; a page
    mixing orientations (an illustration with a horizontal caption) gets each
    row read its own way. Rows are seeded, not chained: chaining would let a
    staircase of bubbles fuse the whole page into one "row".

    Two-tier novel pages are cut at the gutter between the stacked bodies
    first, so the upper tier is read completely before the lower one even
    when a tall block straddles both.
    """
    boxes = [_group_box(g) for g in groups]
    order: list[int] = []
    flow = [i for i, kind in enumerate(kinds) if kind in ("text", "body")]
    order += _order_rows([i for i, k in enumerate(kinds) if k == "header"], groups, boxes)

    cuts: list[float] = []
    stacked = [b for b in bodies if b.vertical]
    for upper, lower in zip(stacked, stacked[1:], strict=False):
        shared = _overlap(upper.cross0, upper.cross1, lower.cross0, lower.cross1)
        narrower = min(upper.cross1 - upper.cross0, lower.cross1 - lower.cross0)
        if lower.top > upper.bottom and shared >= 0.5 * narrower:
            cuts.append((upper.bottom + lower.top) / 2)
    tiers: list[list[int]] = [[] for _ in range(len(cuts) + 1)]
    for i in flow:
        centre = (boxes[i][2] + boxes[i][3]) / 2
        tiers[sum(1 for cut in cuts if centre > cut)].append(i)
    for tier in tiers:
        order += _order_rows(tier, groups, boxes)

    order += _order_rows([i for i, k in enumerate(kinds) if k == "footer"], groups, boxes)
    order += _order_rows([i for i, k in enumerate(kinds) if k == "noise"], groups, boxes)
    return order


def _row_overlap(a0: float, a1: float, b0: float, b1: float) -> float:
    """Vertical overlap of two blocks as a share of the shorter one."""
    return _overlap(a0, a1, b0, b1) / max(min(a1 - a0, b1 - b0), 1e-6)


def _order_rows(
    indices: Sequence[int], groups: Sequence[Sequence[Line]], boxes: Sequence[Spans]
) -> list[int]:
    remaining = sorted(indices, key=lambda i: (boxes[i][2], -boxes[i][1], i))
    ordered: list[int] = []
    while remaining:
        seed = remaining[0]
        s0, s1 = boxes[seed][2], boxes[seed][3]
        row: list[int] = []
        for i in remaining:  # top to bottom, so the row grows downward
            y0, y1 = boxes[i][2], boxes[i][3]
            joins = i == seed or _row_overlap(s0, s1, y0, y1) >= ROW_MIN_OVERLAP
            joins = joins or any(
                _row_overlap(boxes[m][2], boxes[m][3], y0, y1) >= ROW_CHAIN_OVERLAP for m in row
            )
            if joins:
                row.append(i)
        glyphs_vertical = sum(
            glyph_count(ln.text) * (1 if ln.vertical else -1) for i in row for ln in groups[i]
        )
        if glyphs_vertical >= 0:
            row.sort(key=lambda i: (-boxes[i][1], boxes[i][2], i))
        else:
            row.sort(key=lambda i: (boxes[i][0], boxes[i][2], i))
        ordered += row
        taken = set(row)
        remaining = [i for i in remaining if i not in taken]
    return ordered


# --------------------------------------------------------------------------
# 7. The mokuro block
# --------------------------------------------------------------------------


def _widened(
    quad: tuple[Point, Point, Point, Point], vertical: bool, margin: float
) -> tuple[Point, Point, Point, Point]:
    """``quad`` grown by ``margin`` px on both sides ACROSS its reading axis."""
    a, b, c, d = quad
    far = b if vertical else d  # the corner across the line from ``a``
    ux, uy = far[0] - a[0], far[1] - a[1]
    norm = math.hypot(ux, uy)
    if margin <= 0 or norm <= 0:
        return quad
    ux, uy = ux / norm * margin, uy / norm * margin

    def lo(p: Point) -> Point:
        return (p[0] - ux, p[1] - uy)

    def hi(p: Point) -> Point:
        return (p[0] + ux, p[1] + uy)

    return (lo(a), hi(b), hi(c), lo(d)) if vertical else (lo(a), lo(b), hi(c), hi(d))


def _page_box(spans: Spans, page_width: float, page_height: float) -> list[int]:
    """``[x0, y0, x1, y1]`` in whole pixels around ``spans``, clamped to the page."""
    x0, x1, y0, y1 = spans
    w, h = max(int(page_width), 0), max(int(page_height), 0)

    def clamp(value: int, limit: int) -> int:
        return min(max(value, 0), limit) if limit > 0 else max(value, 0)

    return [
        clamp(math.floor(x0), w),
        clamp(math.floor(y0), h),
        clamp(math.ceil(x1), w),
        clamp(math.ceil(y1), h),
    ]


def build_block(
    group: Sequence[Line], page_width: float, page_height: float, margin_cap: float = 0.0
) -> dict[str, Any]:
    """One mokuro block from lines already in reading order.

    ``margin_cap`` is set for a text body's paragraphs (half the body's
    column gap): their quads are widened, see ``BODY_QUAD_MARGIN_EM``.

    ``box`` is the axis-aligned bounds of the member quads, clamped to the
    page (:func:`grow_boxes_over_ruby` then takes in the lines' furigana) --
    for a rotated block too, because that is all the format carries;
    the rotation itself survives in ``lines_coords``, whose quads keep their
    point order (top-left first, in the line's own frame) and are only
    rounded. ``font_size`` is the median line thickness: the median, so one
    fat or clipped line cannot resize the whole bubble.
    """
    theta = block_theta(group)
    margins = [min(BODY_QUAD_MARGIN_EM * ln.thickness, margin_cap) for ln in group]
    quads = [
        _widened(_settled_quad(ln, theta), ln.vertical, margin)
        for ln, margin in zip(group, margins, strict=True)
    ]
    xs = [p[0] for quad in quads for p in quad]
    ys = [p[1] for quad in quads for p in quad]
    return {
        "box": _page_box((min(xs), max(xs), min(ys), max(ys)), page_width, page_height),
        "vertical": group[0].vertical,
        "font_size": int(
            round(
                statistics.median(
                    ln.thickness + 2 * margin for ln, margin in zip(group, margins, strict=True)
                )
            )
        ),
        "lines": [normalize_text(ln.text) for ln in group],
        "lines_coords": [[[int(round(x)), int(round(y))] for x, y in quad] for quad in quads],
    }


def _rect_meets_quad(rect: Sequence[float], quad: Sequence[Sequence[float]]) -> bool:
    """Do the rectangle ``[x0, y0, x1, y1]`` and a convex quad share any AREA?

    Separating axes: the page's two and the quad's own two edge normals.
    Touching along an edge is not meeting -- a box may end where a line starts.
    """
    x0, y0, x1, y1 = rect
    if x1 <= x0 or y1 <= y0:
        return False
    corners = ((x0, y0), (x1, y0), (x1, y1), (x0, y1))
    axes = [(1.0, 0.0), (0.0, 1.0)]
    for k in range(2):
        ex, ey = quad[k + 1][0] - quad[k][0], quad[k + 1][1] - quad[k][1]
        axes.append((-ey, ex))
    for ax, ay in axes:
        rect_proj = [x * ax + y * ay for x, y in corners]
        quad_proj = [p[0] * ax + p[1] * ay for p in quad]
        if max(rect_proj) <= min(quad_proj) or max(quad_proj) <= min(rect_proj):
            return False
    return True


def grow_boxes_over_ruby(
    blocks: Sequence[dict[str, Any]],
    groups: Sequence[Sequence[int]],
    kinds: Sequence[str],
    ruby: Sequence[Ruby],
    page_width: float,
    page_height: float,
) -> None:
    """Grow every block's ``box`` over the furigana of its lines, in place.

    The reader paints a block's box white and draws the text on it. Ruby
    stands in the gutter to the right of its column (above a row): inside the
    lines' bounds wherever that column has a neighbour of its own block on
    that side, outside them beside the block's FIRST column -- and a box that
    stops at the lines slices those glyphs in half, leaving half-characters
    beside the drawn text (bench: every paragraph opening on a kanji with a
    reading, temp00097 and temp00200). So the box is the bounds of the lines
    AND of the ruby runs removed from them (``Ruby.base`` names the line).
    ``font_size`` and ``lines_coords`` are untouched: the text is still laid
    out on the lines alone.

    Never into a neighbour: the box grows towards a run only as far as the
    strip it adds stays clear of every line quad of every other block -- the
    STRIP, not the run, because the box is a rectangle and a run beside the
    foot of a column widens it all the way up. All-or-nothing would not do:
    the detector's ruby quads are padded (bench, novel: 40-58 px around 30 px
    of ink, in a 34 px gutter) and 4 in 10 of the runs that matter overlap
    the widened quad of the previous paragraph's last column by a few pixels;
    stopping AT that column still takes in the ink. Blocks of kind ``noise``
    are never drawn and so are no neighbours. Runs are taken in page order
    (by position, not input index), so the result does not depend on the
    order of the input lines.
    """
    owner = {index: k for k, group in enumerate(groups) for index in group}
    runs: dict[int, list[Ruby]] = {}
    for run in ruby:
        if run.base in owner:
            runs.setdefault(owner[run.base], []).append(run)
    for k, block_runs in runs.items():
        obstacles = [
            quad
            for other, block in enumerate(blocks)
            if other != k and kinds[other] != "noise"
            for quad in block["lines_coords"]
        ]
        box = list(blocks[k]["box"])
        for run in sorted(block_runs, key=lambda r: (_quad_spans(r.quad), r.text)):
            goal = _page_box(_quad_spans(run.quad), page_width, page_height)
            # up and down over the box's own width first, then sideways over
            # the new height: together the four strips are all that is added
            for side in (1, 3, 0, 2):
                outwards = min if side < 2 else max
                box[side] = _clear_reach(box, side, outwards(box[side], goal[side]), obstacles)
        blocks[k]["box"] = box


def _clear_reach(
    box: Sequence[int], side: int, goal: int, obstacles: Sequence[Sequence[Sequence[float]]]
) -> int:
    """How far edge ``side`` of ``box`` (``[x0, y0, x1, y1]``) can move towards ``goal``.

    The furthest whole pixel at which the strip the move adds to the box meets
    no obstacle quad. A wider strip contains a narrower one, so bisection
    finds it.
    """
    x0, y0, x1, y1 = box

    def clear(value: int) -> bool:
        strip = (
            (value, y0, x0, y1),
            (x0, value, x1, y0),
            (x1, y0, value, y1),
            (x0, y1, x1, value),
        )[side]
        return not any(_rect_meets_quad(strip, quad) for quad in obstacles)

    near, far = box[side], goal
    if far == near or clear(far):
        return far
    while abs(far - near) > 1:  # ``near`` is clear (at first an empty strip), ``far`` is not
        mid = (near + far) // 2
        if clear(mid):
            near = mid
        else:
            far = mid
    return near


def _quad_spans(quad: Sequence[Point]) -> Spans:
    xs = [p[0] for p in quad]
    ys = [p[1] for p in quad]
    return (min(xs), max(xs), min(ys), max(ys))


def _settled_quad(line: Line, theta: float) -> tuple[Point, Point, Point, Point]:
    """The line's quad, with a meaningless tilt taken out.

    The minimum-area rectangle around one to three glyphs lands at whatever
    angle the ink's corners suggest (bench: the map labels "芳" +9.7 and "目"
    +6.3 degrees, a flush 「嫌だ」 +4.1 beside columns at 0.0), and a reader
    that honours rotation would draw that text visibly crooked. A line whose
    own angle is not reliable is therefore turned, about its centre and with
    its size kept, to the angle of its block (the median of the block's
    reliable lines, else upright) -- unless it is further than
    ``SETTLE_MAX_TILT_DEG`` from it: a short shout set at -37 degrees is
    really rotated, and keeps its angle. A near-square box (one glyph) has no
    angle of its own at all and always settles: bench OPM, a stray "き" at
    +14 degrees.
    """
    near_square = line.aspect < AMBIGUOUS_ASPECT
    if line.angle_reliable or (not near_square and abs(line.angle - theta) > SETTLE_MAX_TILT_DEG):
        return line.quad
    cx, cy = line.centre
    c, s = math.cos(math.radians(theta)), math.sin(math.radians(theta))
    hw, hh = line.width / 2, line.height / 2
    corners = ((-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh))
    a, b, c4, d = ((cx + x * c - y * s, cy + x * s + y * c) for x, y in corners)
    return (a, b, c4, d)


# --------------------------------------------------------------------------
# The page
# --------------------------------------------------------------------------


def layout_page(page: Mapping[str, Any]) -> PageLayout:
    """Raw page lines -> mokuro blocks in reading order, furigana removed.

    Deterministic: every tie is broken by input index, so the same raw page
    always yields the same blocks. Every input line ends up in exactly one
    block, in ``ruby``, or (no text / no area) in ``dropped``.
    """
    width = float(page.get("width") or 0)
    height = float(page.get("height") or 0)
    lines, dropped = measure_lines(page.get("lines") or [])
    decide_orientations(lines)
    kept, ruby = filter_furigana(lines)
    bodies = find_bodies(kept)
    roles = classify_roles(kept, bodies, height, width)

    groups: list[list[Line]] = []
    kinds: list[str] = []
    caps: list[float] = []  # per block: how far its quads may be widened
    for group in merge_lines(kept, bodies, roles):
        group_roles = [roles.get(ln.index, "text") for ln in group]
        role = next((r for r in group_roles if r != "noise"), "noise")
        indices = {ln.index for ln in group}
        body = max(bodies, key=lambda b: len(indices & b.members), default=None)
        if role == "text" and body is not None and 2 * len(indices & body.members) >= len(indices):
            for paragraph in split_paragraphs(group, body):
                groups.append(paragraph)
                kinds.append("body")
                caps.append(body.gap / 2)
        else:
            groups.append(order_lines(group))
            kinds.append(role)
            caps.append(0.0)

    order = order_blocks(groups, kinds, bodies)
    blocks = [build_block(groups[i], width, height, caps[i]) for i in order]
    ordered_groups = [[ln.index for ln in groups[i]] for i in order]
    ordered_kinds = [kinds[i] for i in order]
    grow_boxes_over_ruby(blocks, ordered_groups, ordered_kinds, ruby, width, height)
    return PageLayout(
        blocks=blocks,
        groups=ordered_groups,
        kinds=ordered_kinds,
        ruby=ruby,
        dropped=dropped,
        bodies=list(bodies),
    )


def page_blocks(page: Mapping[str, Any]) -> list[dict[str, Any]]:
    """Just the mokuro blocks of :func:`layout_page` -- what the runner writes."""
    return layout_page(page).blocks
