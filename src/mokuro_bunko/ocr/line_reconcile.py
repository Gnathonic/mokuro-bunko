"""Two reads of one line, reconciled: a language-model recognizer and the CTC one.

When the ``ppocr-manga`` detector feeds another engine (``paddle-manga``:
PaddleOCR-VL with the manga LoRA), every line has two reads -- the engine's,
and the detector's own small CTC recognizer, which is already loaded and costs
~0.35 s a page. They fail in different places, so neither is thrown away:

* the language-model read ("vlm" below, whatever the engine) names kana and
  kanji better: it knows 窒息 is a word and 紫息 is not. **Its characters win.**
* it was trained on NFKC-folded, whitespace-stripped targets, so it CANNOT
  write what the print has where that differs: ``！`` comes out ``!``, ``…``
  comes out ``...``, the blank cell after a ``？`` is gone. It also drops a
  line's opening ``「`` or closing ``」`` now and then (bench: 3 of 26 dialogue
  lines). The CTC read sees glyph cells, not words; those are its strengths,
  and that is all it contributes.
* it can run away on a line crop (``ぐくーーーー…``, the base model's
  documented habit) or return nothing; then the CTC read stands in.

* on a rare kanji the engine is not steady either: the same glyph came back
  挟 from one crop and 抜 from a slightly wider one. Where the two reads
  differ, a SECOND engine read (a wider crop) can break the tie
  (:func:`settle_disputes`): two reads out of three win.

Several of the rules below measure a read against the room its quad has, so
that room has to be right. The detector's quad is not always a line: over a
dense hand-lettered panel it returns one near-square blob covering ten slanted
columns, and counting ITS room the way a line's is counted (length in
thicknesses) gives 1 cell for 40 glyphs of handwriting. :func:`line_cells`
takes the page's body pitch and measures such a quad as a region instead
(``REGION_MIN_PITCHES``).

Whether a line the CTC recognizer never backed is lettering or line art is
decided by :func:`engine_only_verdict` -- on what the DETECTOR made of the
quad and, for the small glyphs its score is least sure of, on the company the
quad keeps and on its thickness against the body pitch. That size test is
load-bearing, not decoration: it is what separates ruby and a small kana from
a drawn object at the same score. What the decision never rests on is the text
the engine read, or display size, which is what the guard this replaced went
by.

The merge is a sequence alignment of the two reads after folding both with
NFKC, so width differences do not count as disagreement. How much of the line
the two agree on is reported per line (``agreement``): it changes nothing in
the sidecar, but a line where two independent recognizers differ on a kanji is
exactly the line an editor should look at first.

Pure standard library: staged next to the engine runner, unit-tested without
any model.
"""

from __future__ import annotations

import math
import unicodedata
from collections.abc import Sequence
from dataclasses import dataclass, field
from difflib import SequenceMatcher
from typing import Any

# Marks the engine drops at a line's ends while the CTC read (helped by
# ``ppocr.recover_clipped_ends``) has them. Stops included: a column ending in
# "。" is where the detector's box and the engine's crop are tightest.
OPENERS = "「『（〈《【〔"
CLOSERS = "」』）〉》】〕。、"

# A mark is only given back when the glyphs next to it are the ones the CTC
# read has there too: that many characters of the line must align right at the
# mark (or the whole line, when it is shorter).
ANCHOR_GLYPHS = 2
# ... and the line as a whole must be the same line: share of characters the
# two reads agree on, width folded, the candidate marks left out. Under this
# the engine read something genuinely different (or the CTC read is garbage --
# lettered SFX), and patching one read with pieces of the other would invent
# a third text nobody read.
PATCH_MIN_AGREEMENT = 0.8

# Runaway: the merged line is this much longer than BOTH the CTC read and the
# number of glyph cells the quad has room for. Real lines exceed their cell
# count only by pairs set in one cell ("!!", "!?", "12"), which the slack
# covers; 1.35 keeps a line whose CTC read dropped a quarter of its glyphs.
RUNAWAY_RATIO = 1.35
RUNAWAY_SLACK = 2

# A quad's glyph room is its length in thicknesses -- for a LINE. The manga
# line detector also returns quads that are no line at all: a dense
# hand-lettered panel comes back as one near-square blob over ten slanted
# columns, a two-row afterword header as one box. Their aspect ratio is ~1, so
# the line formula gives them room for ONE glyph, and everything measured
# against that room misfires at once: the token budget cuts the read off
# mid-sentence, the runaway rule then throws what was read away, and an
# engine-only line can never be confirmed. A quad this many body pitches thick
# is such a region, and its room is counted in cells of the page's own
# lettering: columns x rows (page 129: 42 and 56 cells, read as 1 before).
REGION_MIN_PITCHES = 2.0
# Room enough for a phrase: the stretch two reads of a REGION must share
# before the wider one corroborates the narrower (:func:`corroborates`), and
# the glyphs a region's read must have before the region rule of
# :func:`engine_only_verdict` believes it.
ROOM_MIN_CELLS = 4
# A tail of one short unit repeated this often, on a line already longer than
# cells and CTC read allow, is a runaway even under RUNAWAY_RATIO. Never used
# alone: "ドドドドドド" is a sound effect, and the CTC read is as long.
REPEAT_MAX_UNIT = 6
REPEAT_MIN_COUNT = 4
# A repetition that has filled the quad twice over is the engine's documented
# runaway and not a sound effect (:func:`engine_looped`). Twice, rather than
# ``RUNAWAY_RATIO``, because a quad's cells are its length in thicknesses and
# display lettering does not keep to that grid: "ヴォォォォォ" is six glyphs in
# a quad two cells long, the trailing vowels drawn small inside the "ヴ".
LOOP_RATIO = 2.0
# Over a REGION that bound is barely a bound: a region's cells are columns x
# rows, so a blob three body pitches thick has room for three times the glyphs
# any ONE run of lettering across it could hold, and "ハッ" repeated 31 times
# (62 glyphs) stays under it -- the runaway guard switched off over exactly
# the quads with the most room to run away, and (``DETECTOR_REGION``) the ones
# admitted at the lower detector bar. So a read that is NOTHING BUT one short
# unit repeated is measured against the reading axis as well
# (:func:`axis_cells`): how far one run of glyphs can go before it has left
# the quad. This much of the read must be that repetition, or it is a line
# with a repeating tail and the whole-quad bound is the one that applies.
LOOP_MIN_SHARE = 0.8

# Token budget of a line: ~1 token per glyph for kana and common kanji, up to
# 3 for a rare kanji the tokenizer spells in bytes -- the very glyphs this
# path exists for -- so the cap is generous per cell and has a floor.
TOKENS_PER_CELL = 1.5
TOKENS_FLOOR = 12
TOKENS_EXTRA = 8
TOKENS_CEILING = 160

# A line of one or two glyphs gives a language model no context, and context
# is all it has over the CTC recognizer: on a map's one-kanji labels the engine
# wrote エち for 巧, 沖 for 漣, 亦心 for 恋, each of which the CTC read had right
# at 1.00. There a CONFIDENT CTC kanji stands, and so does any confident glyph
# the engine answered with a bare mark ("!" for a display-size の at 0.99).
# Kana against kana are left to the engine -- short kana lines are sound
# effects, its home ground (ザッ against the CTC read's ガッ at 0.90) -- and so
# is a CTC read that doubts itself (日 at 0.68 for the 目 of 目次).
SHORT_LINE_GLYPHS = 2
SHORT_LINE_MIN_CONF = 0.9
# Under this the CTC read is what ``line_layout`` calls noise. If the engine
# then reads something else, the line rests on the engine alone.
CTC_DOUBT_CONF = 0.5
# What the DETECTOR made of the quad, which is the one signal that tells a
# line the CTC recognizer never read from a line that is not there at all.
# DBNet scores a quad by the mean probability of its own text/not-text map
# over the ink inside it, and it was trained on manga lettering, so it is the
# second opinion the engine's own reads cannot be: on a corpus of 335 judged
# engine-only lines from six volumes, the engine's two reads are textually
# identical on 311 of them, phantom or not. 0.80 is where that corpus turns --
# 0.70-0.80 is 58% lettering, 0.80-0.90 is 81%, over 0.90 is 100%.
DETECTOR_SURE = 0.80
# ... and over a REGION -- a whole hand-lettered panel in one quad -- with a
# phrase's worth of glyphs read out of it, less will do: a panel of
# handwriting is ink the detector is right about and unsure of at once. The
# glyph count is what keeps hatching out: an impact burst comes back as one
# near-square blob with room for 24 cells and ONE glyph read from it.
DETECTOR_REGION = 0.60
# ... and less will do for a small glyph in the COMPANY of read text. Printed
# ruby, a printed kana alone in its column, the small "ッ" of a hand-lettered
# sound effect: the detector puts those in the 0.65-0.80 band, where its ink
# is as much lettering as not (the judged corpus: 38 of the 69 quads there are
# lettering, 55%, against 83 of 96 -- 87% -- above 0.80), and the 0.80 bar
# drops them all. What separates them from
# the art in that band is not the quad but what is AROUND it -- the page has a
# body pitch at all, other lines the CTC recognizer READ run parallel to this
# one close by (``neighbours``), and the quad is smaller than one body cell,
# which is what ruby and small kana are and what a whole drawn object is not.
# Hatching, motion marks and a chibi face are mostly alone on their patch of
# page. Measured: this keeps 14 lines the 0.80 bar dropped, 13 of them
# lettering; moving the bar itself to 0.75 buys the same recall at 0.81
# precision instead of 0.87 (see ``engine_only_verdict``).
DETECTOR_BODY = 0.65
BODY_MAX_PITCH_SHARE = 0.8
BODY_MIN_NEIGHBOURS = 1
# What a line resting on the engine alone is given as its confidence once
# :func:`engine_only_verdict` has kept it: over ``line_layout.LOW_CONFIDENCE``
# so it is laid out, under the 0.9 a lone glyph needs to be believed.
CONFIRMED_CONF = 0.75
# How much of the two engine reads must be the same read for the second to
# corroborate the first. Not equality: over a REGION the two crops see
# different amounts of the panel (the wider one reads on into the next
# column), so there what the narrower read has must be IN the wider read --
# containment, not agreement. On a line quad it stays agreement, where 0.85
# is still "the same read": "ドド" against "ドドド" is 0.8.
CONFIRMED_MIN_AGREEMENT = 0.85

# Glyphs the engine SKIPPED come back from the CTC read when it is this sure of
# every one of them. A decoder that writes sentences skips: the dangling first
# glyph of the next sentence after a column's last "。" (seven columns of one
# novel), a first short sentence before "……", the middle of "…こと以上のこと…".
# A CTC read emits a glyph only over ink, so its extra glyphs are there -- unless
# it doubts them: its spurious ones (a rule read as "一", a map's "●", a stray
# "1") sat at 0.67-0.94 or are no text at all; the real ones at 0.94-1.00.
SKIPPED_MIN_CONF = 0.94
# Kana against kana the engine's language model is no judge: it writes the
# likelier word ("するために" for a printed "するたびに", three times in one
# novel and from both crops; "答えられて" for "答えられずに"). The CTC read was
# at 0.997-1.0 on each of those, and never over 0.986 where IT was the one
# that slipped (ラ read as う, っ as つ). Set on that one book; kept tight.
KANA_STANDS_CONF = 0.995

IDEOGRAPHIC_SPACE = "\u3000"
DASH = "―"
# What the engine may write for a printed dash. "ー" counts only where it
# cannot be a long vowel -- after something that is not a kana (the rule
# ``line_layout._fix_dashes`` uses); elsewhere the language model is the
# better judge of the two.
ENGINE_DASHES = "-‐–—―"
LONG_VOWEL = "ー"
# Glyphs of one thin stroke. At a line's end the engine takes them for a rule
# or a bubble's outline and leaves them out ("廊下が真一" came back "廊下が真");
# the CTC read has them through ``ppocr``'s thin-glyph probe. Given back only
# on the columns of a text body, the one place that probe is trusted too.
THIN_GLYPHS = "一―"
# ``ppocr.MISSING_GLYPH``: what the CTC read writes for a character outside its
# dictionary. (Spelled out here: this module is staged alone, next to the
# runner, and imports nothing of the package.)
MISSING_GLYPH = "〓"


def fold(text: str) -> str:
    """NFKC without surrounding or inner whitespace: what the engines are trained to write."""
    return "".join(ch for ch in unicodedata.normalize("NFKC", text) if not ch.isspace())


@dataclass(frozen=True)
class _Token:
    """One unit of the alignment and the stretch ``[start, end)`` of the text it stands for."""

    value: str
    start: int
    end: int


_DOTS = ".…‥"
_BLANKS = "\u3000 "
ELLIPSIS = "…"


def _tokens(text: str, *, blanks: bool = False) -> list[_Token]:
    """``text`` as alignment tokens: NFKC-folded characters, whitespace dropped.

    Two things are not one character to one token. A run of dots is ONE
    token whatever it is made of: the engine writes "..." for "……" as often
    as "......", so dots are compared as "an ellipsis here", and its length
    is taken from the read that counts cells. ``blanks`` keeps spaces as
    tokens (the CTC read's blank cells and word gaps; the engine, trained on
    whitespace-stripped targets, never has any).
    """
    out: list[_Token] = []
    for i, ch in enumerate(text):
        if ch in _BLANKS and blanks:
            out.append(_Token(ch, i, i + 1))
            continue
        folded = "".join(c for c in unicodedata.normalize("NFKC", ch) if not c.isspace())
        if folded and all(c in _DOTS for c in folded):
            last = out[-1] if out else None
            if last is not None and last.end == i and last.value in (".", ELLIPSIS):
                out[-1] = _Token(ELLIPSIS, last.start, i + 1)
            else:
                out.append(_Token(ELLIPSIS if len(folded) > 1 else ".", i, i + 1))
            continue
        out.extend(_Token(c, i, i + 1) for c in folded)
    return out


# The alignment runs on folded text, where "（" is "(".
_OPENERS_FOLDED = fold(OPENERS)
_CLOSERS_FOLDED = fold(CLOSERS)


def _is_japanese(ch: str) -> bool:
    o = ord(ch)
    return 0x3041 <= o <= 0x30FF or 0x3400 <= o <= 0x9FFF or ch in "々〆〇"


def _is_kanji(ch: str) -> bool:
    return 0x3400 <= ord(ch) <= 0x9FFF or ch in "々〆〇"


def _is_kana(ch: str) -> bool:
    return 0x3041 <= ord(ch) <= 0x30FF


def _is_plain_kana(ch: str) -> bool:
    """A kana glyph proper: not the long-vowel mark, not the middle dot."""
    return _is_kana(ch) and ch not in "ー・゠"


def _keeps_width(ch: str) -> bool:
    """Is ``ch`` a printed form NFKC would lose, and one worth giving back?

    Full-width punctuation, digits and Latin, ``…``/``‥``: yes. Half-width
    kana: no -- the CTC recognizer writes those for small print, and a reader
    gains nothing from ``ｱ``.
    """
    if unicodedata.normalize("NFKC", ch) == ch:
        return False
    return not 0xFF61 <= ord(ch) <= 0xFF9F


def widen_punctuation(text: str, keep: Sequence[bool] | None = None) -> str:
    """Full-width forms for marks the engine folded, where no CTC read vouches.

    The convention is the CTC recognizer's own (bench, one 292-page novel:
    ``？`` 302, ``！`` 114, ``!!`` 25, ``!?`` 20, lone ``?`` 14, lone ``!`` 7):
    a mark alone in its cell is full-width, a PAIR set in one cell stays
    ASCII. Runs of dots become ``…`` (``‥`` for two). Only in Japanese text --
    ``OK!`` and ``No.1`` stay as they are. ``keep[i]`` protects a character
    the CTC read confirmed as written.
    """
    if not any(ch in "!?." for ch in text) or not any(_is_japanese(ch) for ch in text):
        return text
    kept = list(keep) if keep is not None else [False] * len(text)
    out: list[str] = []
    i = 0
    while i < len(text):
        ch = text[i]
        if ch not in "!?." or kept[i]:
            out.append(ch)
            i += 1
            continue
        j = i
        dots = ch == "."
        while j < len(text) and text[j] in "!?." and not kept[j] and (text[j] == ".") == dots:
            j += 1
        run = text[i:j]
        after_ascii = i > 0 and text[i - 1].isascii() and text[i - 1].isalnum()
        before_ascii = j < len(text) and text[j].isascii() and text[j].isalnum()
        if after_ascii or (ch == "." and before_ascii) or (ch == "." and len(run) == 1):
            out.append(run)
        elif ch == ".":
            # "...." is the engine stuttering over "…", not an ellipsis and a stop.
            out.append("‥" if len(run) == 2 else "…" * max(1, round(len(run) / 3)))
        elif len(run) == 1:
            out.append("！" if ch == "!" else "？")
        else:
            out.append(run)
        i = j
    return "".join(out)


def region_cells(main: float, thickness: float, pitch: float) -> int:
    """Glyph cells a quad that is NO line has room for: columns x rows at ``pitch``."""
    if main <= 0 or thickness <= 0 or pitch <= 0:
        return 0
    return max(1, int(round(main / pitch))) * max(1, int(round(thickness / pitch)))


def is_region(main: float, thickness: float, pitch: float) -> bool:
    """Is this quad a region of several columns rather than one line? (``REGION_MIN_PITCHES``)"""
    if pitch <= 0 or main <= 0 or thickness <= 0:
        return False
    return thickness >= REGION_MIN_PITCHES * pitch


def line_cells(main: float, thickness: float, pitch: float = 0.0) -> int:
    """Glyph cells a quad has room for (Japanese lettering is fixed-pitch).

    A line's room is its length in thicknesses. ``pitch`` is the page's body
    pitch (:func:`body_pitch` in the runner), and with it a quad too thick to
    be one line is measured as a region instead (:func:`region_cells`) -- the
    blob the detector returns over a hand-lettered panel. 0 leaves the line
    formula alone, which is what a caller without a page to measure gets.
    """
    if main <= 0 or thickness <= 0:
        return 0
    cells = max(1, int(round(main / thickness)))
    if is_region(main, thickness, pitch):
        cells = max(cells, region_cells(main, thickness, pitch))
    return cells


def axis_cells(main: float, thickness: float, pitch: float = 0.0) -> int:
    """Glyph cells along the READING axis: how far one run of glyphs can go.

    For a line that is :func:`line_cells` -- its length in thicknesses. For a
    REGION it is not: a region's room is columns x rows, and no single run of
    lettering in it is that long. Measured in the page's own cells there
    (``main / pitch``), which is the length of ONE of its columns.
    """
    if main <= 0 or thickness <= 0:
        return 0
    if is_region(main, thickness, pitch):
        return max(1, int(round(main / pitch)))
    return max(1, int(round(main / thickness)))


def token_cap(cells: int) -> int:
    """``max_new_tokens`` for a line of ``cells`` glyph cells (see ``TOKENS_PER_CELL``)."""
    wanted = int(math.ceil(max(cells, 0) * TOKENS_PER_CELL)) + TOKENS_EXTRA
    return min(max(wanted, TOKENS_FLOOR), TOKENS_CEILING)


def repeated_tail(text: str) -> int:
    """Length of a tail made of one short unit repeated ``REPEAT_MIN_COUNT``+ times, else 0."""
    best = 0
    for unit in range(1, REPEAT_MAX_UNIT + 1):
        if len(text) < unit * REPEAT_MIN_COUNT:
            break
        tail = text[-unit:]
        count = 1
        while text[-unit * (count + 1) : len(text) - unit * count] == tail:
            count += 1
        if count >= REPEAT_MIN_COUNT:
            best = max(best, unit * count)
    return best


def _cells_of(text: str) -> float:
    """Glyph cells ``text`` takes: Latin letters and digits are set two to the em."""
    return sum(0.5 if ch.isascii() and ch.isalnum() else 1.0 for ch in text)


def engine_looped(vlm: str, cells: int, axis: int = 0) -> bool:
    """Did the engine repeat one unit until it had filled the quad twice over?

    The documented habit of the base model on a crop with nothing in it to
    read is to write one short unit again and again ("ぐくーーーー…") until
    its token budget runs out. That is the runaway worth throwing a whole line
    away for, and the only yardstick such a line has is its quad's cells: the
    CTC read behind it is nothing, or noise, and neither buys room
    (``LOOP_RATIO``). The engine's own first read is what is measured, since
    the merge may already have cut the repetition off.

    Two bounds, either of which makes it a runaway:

    * the read has filled the whole quad twice over -- room counted the way
      the quad's own shape counts it (``cells``, columns x rows for a region);
    * the read IS a repetition (``LOOP_MIN_SHARE`` of it), and the repetition
      alone has run twice past what the READING axis has room for (``axis``,
      :func:`axis_cells`, defaulting to ``cells``).

    On a line quad the two are the same measurement and the second can only
    fire where the first already has. They part over a region, where the first
    bound is columns x rows: a blob three pitches thick with ten cells in each
    column is given 30 cells, and "ハッハッハッ…" for 62 glyphs sits inside
    twice that while being a runaway from its tenth glyph. Lettering that
    genuinely repeats stays in: "くんくんくんくん" over a panel fifteen cells
    long is eight glyphs against a budget of 32.

    Known limit: the second bound narrows that hole, it does not close it. It
    only applies to a read that is NOTHING BUT the repetition
    (``LOOP_MIN_SHARE``), so a MIXED runaway over a region still passes both.
    On the same 30-cell blob, 26 plausible glyphs followed by "ハッ" thirteen
    times is a 26-glyph loop against an axis budget of 22 -- but the loop is
    only half the read, so the axis bound never looks at it, and 52 glyphs is
    inside the whole-quad budget of 62. Bounding the repetition wherever it
    starts would need a reason to believe the glyphs before it, and on a line
    the CTC recognizer never backed there is none.
    """
    text = fold(vlm)
    repeat = repeated_tail(text)
    if cells <= 0 or not text or repeat <= 0:
        return False
    if _cells_of(text) > LOOP_RATIO * cells + RUNAWAY_SLACK:
        return True
    if repeat < LOOP_MIN_SHARE * len(text):
        return False
    room = axis if axis > 0 else cells
    return _cells_of(text[len(text) - repeat :]) > LOOP_RATIO * room + RUNAWAY_SLACK


def is_runaway(text: str, ctc: str, cells: int) -> bool:
    """Did the engine keep generating past the line? (See ``RUNAWAY_RATIO``.)

    Lengths are in cells, not characters: a Latin line ("www.example.co.jp")
    has about twice as many characters as its quad has ems, and would be
    thrown away as a runaway exactly when the engine read all of it.
    """
    room = max(_cells_of(fold(ctc)), cells)
    if room <= 0:
        return False
    length = _cells_of(fold(text))
    if length > RUNAWAY_RATIO * room + RUNAWAY_SLACK:
        return True
    return length > room + RUNAWAY_SLACK and repeated_tail(text) > 0


@dataclass
class Reconciled:
    """One line after the merge. ``asdict`` of it is what the raw dump keeps."""

    # The text the sidecar gets.
    text: str
    vlm: str
    ctc: str
    # Share of characters the merged text and the CTC read agree on, width
    # folded (1.0: two recognizers, one reading). None when only one read
    # exists -- nothing was compared, which is not the same as disagreement.
    agreement: float | None
    # "merged" (the engine's characters, possibly patched), or "ctc" when the
    # engine's read was unusable.
    source: str = "merged"
    # What was done: opener, closer, width, space, dash, thin, skipped, kana,
    # vote, retry, short, confirmed, seam, runaway, empty, disagree, and for
    # an engine-only line the rule that decided it -- backed, region, body /
    # looped, unbacked (see ``engine_only_verdict``), the last two followed by
    # "dropped". For whoever reads the dump, and for the page's tally.
    notes: list[str] = field(default_factory=list)
    # The engine's second read, when one was taken (see ``settle_disputes``).
    second: str | None = None
    # No usable CTC read stands behind the text: it read nothing, or doubted
    # what it read and the engine read something else. Sound effects mostly
    # -- and hatching the engine took for text, which is why such a line is
    # only laid out once ``engine_only_verdict`` has kept it.
    engine_only: bool = False
    # A second engine read said the same line again. RECORDED, not believed:
    # over the judged corpus the second read repeats the first whether the ink
    # is lettering or not (see ``DETECTOR_SURE``), so it decides nothing.
    confirmed: bool = False

    def to_json(self) -> dict[str, Any]:
        return {
            "vlm": self.vlm,
            **({"vlm_second": self.second} if self.second is not None else {}),
            "ctc": self.ctc,
            "merged": self.text,
            "agreement": None if self.agreement is None else round(self.agreement, 4),
            "source": self.source,
            "notes": list(self.notes),
            **({"engine_only": True, "confirmed": self.confirmed} if self.engine_only else {}),
        }


def _ratio(a: Sequence[str], b: Sequence[str]) -> float:
    if not a and not b:
        return 1.0
    return SequenceMatcher(None, list(a), list(b), autojunk=False).ratio()


def _values(tokens: Sequence[_Token]) -> list[str]:
    return [t.value for t in tokens if t.value not in _BLANKS]


def _core(values: Sequence[str]) -> list[str]:
    """``values`` without the leading openers and trailing closers."""
    head = 0
    while head < len(values) and values[head] in _OPENERS_FOLDED:
        head += 1
    tail = len(values)
    while tail > head and values[tail - 1] in _CLOSERS_FOLDED:
        tail -= 1
    return list(values[head:tail])


def reconcile_line(
    vlm: str,
    ctc: str,
    cells: int = 0,
    *,
    thin: bool = False,
    ctc_conf: float | None = None,
    ctc_char_confs: Sequence[float] | None = None,
) -> Reconciled:
    """Merge the engine's read of a line with the CTC read (module docstring).

    ``ctc`` should already be normalized (``line_layout.normalize_text``), so
    a dash the CTC recognizer read as ``ー`` does not count against it.
    ``cells`` is the quad's glyph room (:func:`line_cells`), 0 if unknown.
    ``thin`` marks a column of a text body (see ``THIN_GLYPHS``); ``ctc_conf``
    is the CTC recognizer's confidence in its read, when known, and
    ``ctc_char_confs`` its confidence per character of ``ctc`` (ignored unless
    it has one entry a character).
    """
    vlm_text = "".join(ch for ch in vlm if not ch.isspace())
    ctc_text = ctc.strip()
    confs = list(ctc_char_confs) if ctc_char_confs is not None else []
    if len(confs) != len(ctc_text) or len(ctc_text) != len(ctc):
        confs = []
    if not vlm_text:
        return Reconciled(ctc_text, vlm, ctc, None, "ctc", ["empty"] if ctc_text else [])
    vlm_text, dash_runs = _adopt_dash_runs(vlm_text, ctc_text)
    mine = _tokens(vlm_text)
    theirs = _tokens(ctc_text, blanks=True)
    if not _values(theirs):
        # Nothing to compare with or to fall back to; a runaway is at least
        # cut where the repetition starts, so the line keeps its real glyphs.
        # Only a REPETITION is cut here (:func:`engine_looped`): with no CTC
        # read to fall back on, cutting a read that merely overruns the cells
        # its quad was measured to have would invent a text nobody read, and
        # display lettering overruns those cells as a matter of course.
        text = widen_punctuation(vlm_text)
        if engine_looped(vlm_text, cells):
            text = text[: len(text) - repeated_tail(text)] or text[:cells]
            return Reconciled(text, vlm, ctc, None, "merged", ["runaway"], engine_only=True)
        return Reconciled(text, vlm, ctc, None, engine_only=True)

    mine_values = [t.value for t in mine]
    # The engine may have the end marks itself; then the CTC core is the wrong
    # yardstick and the full read the right one. Take the kinder of the two.
    # Whichever dash the engine wrote is no disagreement about the line.
    folded = _dashes_folded(mine_values)
    same_line = (
        max(_ratio(folded, _core(_values(theirs))), _ratio(folded, _values(theirs)))
        >= PATCH_MIN_AGREEMENT
    )

    notes: list[str] = ["dash"] if dash_runs else []
    # Edits to vlm_text, by character index: a stretch replaced (start ->
    # (end, text)), text inserted BEFORE an index, and the characters the CTC
    # read vouches for as written (the default width policy leaves those).
    replace: dict[int, tuple[int, str]] = {}
    insert_before: dict[int, str] = {}
    vouched = [False] * len(vlm_text)
    matcher = SequenceMatcher(None, mine_values, [t.value for t in theirs], autojunk=False)
    opcodes = matcher.get_opcodes()

    def anchored(k: int, *, forward: bool) -> bool:
        """Is opcode ``k`` an equal stretch long enough to vouch for a neighbour?"""
        if not 0 <= k < len(opcodes) or opcodes[k][0] != "equal":
            return False
        _, i1, i2, _, _ = opcodes[k]
        rest = len(mine) - i1 if forward else i2
        return i2 - i1 >= min(ANCHOR_GLYPHS, rest)

    def dashlike(i: int) -> bool:
        value = mine_values[i]
        if value in ENGINE_DASHES:
            return True
        return value == LONG_VOWEL and (i == 0 or not _is_kana(mine_values[i - 1]))

    for k, (tag, i1, i2, j1, j2) in enumerate(opcodes):
        if tag == "equal":
            for a, b in zip(mine[i1:i2], theirs[j1:j2], strict=True):
                printed = _printed_form(vlm_text, ctc_text, a, b, mine, theirs)
                if printed is not None:
                    replace[a.start] = (a.end, printed)
                    notes.append("width")
                for at in range(a.start, a.end):
                    vouched[at] = True
            continue
        if not same_line or j2 == j1:
            continue
        added = [t.value for t in theirs[j1:j2]]
        source = ctc_text[theirs[j1].start : theirs[j2 - 1].end]
        at = mine[i1].start if i1 < len(mine) else len(vlm_text)
        end_marks = _OPENERS_FOLDED + (THIN_GLYPHS if thin else "")
        if tag == "insert" and i1 == 0 and all(v in end_marks for v in added):
            if anchored(k + 1, forward=True):
                insert_before[at] = source
                notes.append("opener" if added[0] in _OPENERS_FOLDED else "thin")
            continue
        end_marks = _CLOSERS_FOLDED + (THIN_GLYPHS if thin else "")
        if tag == "insert" and i1 == len(mine) and all(v in end_marks for v in added):
            if anchored(k - 1, forward=False):
                insert_before[at] = source
                notes.append("closer" if added[-1] in _CLOSERS_FOLDED else "thin")
            continue
        stretch = confs[theirs[j1].start : theirs[j2 - 1].end]
        if tag == "insert" and _sure_text(source, stretch, SKIPPED_MIN_CONF):
            before = i1 == 0 or anchored(k - 1, forward=False)
            after = i1 == len(mine) or anchored(k + 1, forward=True)
            if before and after:
                insert_before[at] = insert_before.get(at, "") + source
                notes.append("skipped")
            continue
        if (
            tag == "replace"
            and all(_is_plain_kana(v) for v in mine_values[i1:i2])
            and all(_is_plain_kana(ch) for ch in source)
            and stretch
            and min(stretch) >= KANA_STANDS_CONF
        ):
            replace[mine[i1].start] = (mine[i2 - 1].end, source)
            notes.append("kana")
            continue
        if source.strip(" ") == "":
            # A word gap in Latin or digits ("ONE PUNCH", a contents page's
            # "9 141"): between two characters both reads agree on.
            around = mine_values[i1 - 1 : i1 + 1] if tag == "insert" and 0 < i1 else []
            if len(around) == 2 and all(v.isascii() and v.isalnum() for v in around):
                if anchored(k - 1, forward=False) or anchored(k + 1, forward=True):
                    insert_before[at] = insert_before.get(at, "") + " "
                    notes.append("space")
            continue
        # The blank cell after "？", the second cell of a "――": cells of the
        # fixed-pitch grid that the engine's training targets never had, so
        # it cannot write them however well it reads. The CTC read's run of
        # blanks and dashes stands in for whatever dash the engine wrote.
        if any(ch not in IDEOGRAPHIC_SPACE + DASH for ch in source):
            continue
        if not all(dashlike(i) for i in range(i1, i2)):
            continue
        beside_dash = any(dashlike(i) for i in (i1 - 1, i2) if 0 <= i < len(mine))
        after_mark = 0 < i1 and mine_values[i1 - 1] in "!?"
        if i2 > i1 or (DASH in source and beside_dash) or (DASH not in source and after_mark):
            if i2 > i1:
                replace[mine[i1].start] = (mine[i2 - 1].end, source)
            else:
                insert_before[at] = insert_before.get(at, "") + source
            notes.append("dash" if DASH in source else "space")

    merged = _assemble(vlm_text, replace, insert_before, vouched)
    notes = list(dict.fromkeys(notes))
    if not same_line:
        notes.append("disagree")
    if is_runaway(merged, ctc_text, cells):
        agreement = _ratio(mine_values, _values(theirs))
        return Reconciled(ctc_text, vlm, ctc, agreement, "ctc", [*notes, "runaway"])
    their_core = _core(_values(theirs))
    merged_values = _values(_tokens(merged))
    if (
        ctc_conf is not None
        and ctc_conf >= SHORT_LINE_MIN_CONF
        and len(their_core) <= SHORT_LINE_GLYPHS
        and _core(merged_values) != their_core
        and (
            any(_is_kanji(v) for v in their_core)
            or (any(_is_japanese(v) for v in their_core) and not any(map(_is_japanese, merged)))
            # The last glyph or two of a wrapped sentence and its "」", alone
            # in a column: without a sentence around it the engine takes the
            # bracket for a glyph ("いく", "ねー", "アレ" for "い」", "ね」",
            # "ア」" -- a dozen columns of one novel).
            or _lacks_end_bracket(merged_values, _values(theirs))
        )
    ):
        agreement = _ratio(mine_values, _values(theirs))
        return Reconciled(ctc_text, vlm, ctc, agreement, "ctc", [*notes, "short"])
    agreement = _ratio(_values(_tokens(merged)), _values(theirs))
    doubted = ctc_conf is not None and ctc_conf < CTC_DOUBT_CONF and not same_line
    return Reconciled(merged, vlm, ctc, agreement, "merged", notes, engine_only=doubted)


def _sure_text(source: str, confs: Sequence[float], floor: float) -> bool:
    """Is ``source`` text, every glyph of it read with confidence ``floor`` or better?

    Brackets are exempt from the floor (a bracket the end probe recovered
    carries the probe's confidence, not the line's), blank cells come along;
    anything that is not Japanese text or its punctuation is not vouched for.
    """
    if not source or len(confs) != len(source):
        return False
    brackets = OPENERS + CLOSERS.replace("。", "").replace("、", "")
    for ch, conf in zip(source, confs, strict=True):
        if ch in brackets or ch in _BLANKS:
            continue
        if not (_is_japanese(ch) or ch in "。、！？…‥") or conf < floor:
            return False
    return any(ch not in _BLANKS and ch not in brackets for ch in source)


def _lacks_end_bracket(merged: Sequence[str], theirs: Sequence[str]) -> bool:
    """Does the CTC read open or close with a bracket the merged text does not have there?"""
    if not theirs:
        return False
    closers = _CLOSERS_FOLDED.replace("。", "").replace("、", "")
    opens = theirs[0] in _OPENERS_FOLDED and list(merged[:1]) != [theirs[0]]
    closes = theirs[-1] in closers and list(merged[-1:]) != [theirs[-1]]
    return opens or closes


def _adopt_dash_runs(vlm_text: str, ctc_text: str) -> tuple[str, int]:
    """The engine's single dash swapped for the CTC read's RUN of dashes: ``(text, runs)``.

    "――" is two cells of the grid, and the engine writes one mark for it --
    after a kana a "ー", which no later rule may touch because it could be a
    long vowel. Against two or more dash cells it cannot be: a long vowel is
    never doubled. Done before the alignment proper, so that the run anchors
    what stands next to it (the "」" of "…だって――」").
    """
    mine = _tokens(vlm_text)
    theirs = _tokens(ctc_text, blanks=True)
    matcher = SequenceMatcher(
        None, [t.value for t in mine], [t.value for t in theirs], autojunk=False
    )
    out, runs = vlm_text, 0
    for tag, i1, i2, j1, j2 in reversed(matcher.get_opcodes()):
        if tag != "replace" or i2 - i1 != 1:
            continue
        if mine[i1].value != LONG_VOWEL and mine[i1].value not in ENGINE_DASHES:
            continue
        while j2 > j1 and j2 == len(theirs) and theirs[j2 - 1].value in _CLOSERS_FOLDED:
            j2 -= 1
        while j1 < j2 and j1 == 0 and theirs[j1].value in _OPENERS_FOLDED:
            j1 += 1
        run = [t.value for t in theirs[j1:j2]]
        if len(run) >= 2 and all(v == DASH for v in run):
            out = out[: mine[i1].start] + DASH * len(run) + out[mine[i1].end :]
            runs += 1
    return out, runs


def _printed_form(
    vlm_text: str,
    ctc_text: str,
    mine: _Token,
    theirs: _Token,
    all_mine: Sequence[_Token],
    all_theirs: Sequence[_Token],
) -> str | None:
    """The CTC read's printed form of an agreed token (``！``, ``……``, ``２``), if it differs."""
    engine, printed = vlm_text[mine.start : mine.end], ctc_text[theirs.start : theirs.end]
    if engine == printed:
        return None
    if mine.value == ELLIPSIS:
        # Length from the read that counts cells. It spells a long ellipsis in
        # ASCII dots now and then ("........." or ".………" for "………"): three
        # dots to the cell then, and never a worse spelling than the engine's.
        if all(ch in "…‥" for ch in printed):
            return printed
        cells = sum(ch in "…‥" for ch in printed) + round(printed.count(".") / 3)
        return ELLIPSIS * cells if cells > len(engine.replace("...", ELLIPSIS)) else None
    # One character to one token on both sides (not "⁉" = "!?", "㌔" = "キロ"),
    # or there is no telling which character the printed form replaces.
    for token, tokens in ((mine, all_mine), (theirs, all_theirs)):
        if sum(1 for t in tokens if t.start == token.start) != 1:
            return None
    return printed if _keeps_width(printed) else None


def _assemble(
    vlm_text: str,
    replace: dict[int, tuple[int, str]],
    insert_before: dict[int, str],
    vouched: Sequence[bool],
) -> str:
    """The merged text; unvouched ASCII marks then get the default width policy."""
    chars: list[str] = []
    keep: list[bool] = []
    i = 0
    while i <= len(vlm_text):
        for extra in insert_before.get(i, ""):
            chars.append(extra)
            keep.append(True)
        if i == len(vlm_text):
            break
        end, new = replace.get(i, (i + 1, vlm_text[i]))
        for piece in new:
            chars.append(piece)
            keep.append(vouched[i] or i in replace)
        i = end
    return widen_punctuation("".join(chars), keep)


def needs_second_read(line: Reconciled) -> bool:
    """Is a second engine read worth its time for this line?

    Yes where the two reads still differ after the merge (a kanji, a dropped
    glyph) and where the engine's first read was unusable.
    """
    if line.source == "ctc":
        # Empty or runaway -- not "nothing to read", and not a short line the
        # CTC read was given outright.
        return bool(line.notes) and "short" not in line.notes
    if line.engine_only:
        return bool(line.text)
    return line.agreement is not None and line.agreement < 1.0


def settle_disputes(line: Reconciled, second: str, cells: int = 0) -> Reconciled:
    """Let a second engine read break the ties the merge left (two of three win).

    Every stretch where the merged text and the CTC read still differ is put
    to the second read: if the line WITH the CTC read's version of that one
    stretch is closer to the second read than the line without, the CTC read
    had it right and the first engine read slipped (bench: 挟 read as 抜 at
    one margin and as 挟 at another, the CTC read saying 挟 throughout). A
    three-way split changes nothing. Differing glyphs that sit side by side
    are settled one by one (:func:`_single_disputes`), and the CTC read's "〓"
    is never a candidate -- otherwise a vote for the glyph next to it would
    carry it in. Dashes are compared as dashes, whichever one a read wrote
    (:func:`_dashes_folded`); what is written is the CTC read's. An engine
    read that was unusable the first time is simply replaced by a usable
    second one.
    """
    second_text = "".join(ch for ch in second if not ch.isspace())
    if line.source == "ctc":
        retry = reconcile_line(second, line.ctc, cells)
        if second_text and retry.source == "merged" and "disagree" not in retry.notes:
            retry.vlm, retry.second = line.vlm, second
            kept = [note for note in line.notes if note in ("empty", "runaway")]
            retry.notes = [*kept, *retry.notes, "retry"]
            return retry
        line.second = second
        return line
    line.second = second
    if not second_text:
        return line
    if line.engine_only:
        # Nothing to vote against; the second read can only say "same again",
        # and on this path it nearly always does, whatever the ink is.
        # Recorded for the dump; ``engine_only_verdict`` does not ask.
        if corroborates(line.vlm, second_text, cells):
            line.confirmed = True
            line.notes.append("confirmed")
        return line
    witness = _dashes_folded([t.value for t in _tokens(second_text)])
    theirs = _tokens(line.ctc.strip(), blanks=True)
    text = line.text
    changed = False
    # Right to left, so the character positions of earlier stretches hold.
    mine = _tokens(text)
    matcher = SequenceMatcher(
        None, [t.value for t in mine], [t.value for t in theirs], autojunk=False
    )
    for i1, i2, j1, j2 in reversed(_single_disputes(matcher.get_opcodes())):
        source = line.ctc.strip()[theirs[j1].start : theirs[j2 - 1].end] if j2 > j1 else ""
        if source and all(ch in _BLANKS for ch in source):
            continue  # no engine read has blank cells; nothing to vote with
        if MISSING_GLYPH in source:
            continue  # "could not read it" is no reading to vote for
        start = mine[i1].start if i1 < len(mine) else len(text)
        end = mine[i2 - 1].end if i2 > i1 else start
        swapped = text[:start] + source + text[end:]
        before = _matches(_dashes_folded([t.value for t in _tokens(text)]), witness)
        after = _matches(_dashes_folded([t.value for t in _tokens(swapped)]), witness)
        if after > before:
            text, changed = swapped, True
    if not changed:
        return line
    agreement = _ratio(_values(_tokens(text)), _values(theirs))
    return Reconciled(
        text, line.vlm, line.ctc, agreement, "merged", [*line.notes, "vote"], second
    )


def corroborates(vlm: str, second: str, cells: int = 0) -> bool:
    """Does a second engine read say the first read's line again? (``CONFIRMED_MIN_AGREEMENT``)

    Normally that is agreement between the two reads. Over a quad with room
    for a phrase (``ROOM_MIN_CELLS``) the two crops do not see the same thing
    -- the wider one reads on past where the narrower one stopped -- so there
    containment will do: what the narrower read has is in the wider one. Only
    where there is a phrase's worth of it to contain, though; two glyphs
    inside three are as much a second opinion as any other pair of short
    reads, and art reads short.
    """
    mine = _values(_tokens(fold(vlm)))
    theirs = _values(_tokens(fold(second)))
    if not mine or not theirs:
        return False
    shared = _matches(mine, theirs)
    if cells >= ROOM_MIN_CELLS and shared >= ROOM_MIN_CELLS:
        if shared >= CONFIRMED_MIN_AGREEMENT * min(len(mine), len(theirs)):
            return True
    return _ratio(mine, theirs) >= CONFIRMED_MIN_AGREEMENT


def engine_only_verdict(
    line: Reconciled,
    *,
    cells: int = 0,
    det_score: float = 0.0,
    main: float = 0.0,
    thickness: float = 0.0,
    pitch: float = 0.0,
    neighbours: int = 0,
) -> tuple[bool, str]:
    """Keep a line the CTC recognizer never backed, or drop it? ``(keep, why)``.

    The engine reads blank paper, hatching and speed lines as kana, the same
    way twice, and such a line must not reach the sidecar: the layout takes a
    doubted quad that is collinear with a column for a piece of it. What
    separates the two populations is what the DETECTOR made of the quad and,
    for the small glyphs its score is least sure of, the company the quad
    keeps. Not the text the engine read, and not display size, which is what
    the guard this replaced went by:

    * ``looped`` -- the engine repeated itself past its quad's room
      (:func:`engine_looped`). Dropped whatever the detector says: that is a
      fact about the read, not about the ink.
    * ``backed`` -- the detector is sure the quad is a line (``DETECTOR_SURE``).
    * ``region`` -- the quad is a whole hand-lettered panel and a phrase's
      worth of glyphs was read out of it, the detector over
      ``DETECTOR_REGION``. Handwriting is ink a line detector is right about
      and unsure of at once.
    * ``body`` -- a glyph smaller than a body cell, in the company of lines
      the CTC recognizer read (``DETECTOR_BODY``, ``neighbours``): printed
      ruby, a printed kana alone in its column, the small "ッ" of a sound
      effect. Context art has not, over the band where the detector's score
      says least.
    * ``unbacked`` -- the detector was not sure this quad is a line; dropped.

    ``why`` goes into the per-line dump either way, beside the quad's score.
    ``neighbours`` is how many lines the CTC recognizer READ run parallel to
    this quad nearby (``parallel_neighbours`` in the runner); 0 for a caller
    with no page to look at, which switches the ``body`` rule off.

    A line the engine read but read WRONG is kept on the same terms as any
    other. The box is over real lettering, and for a reader that is the
    difference between a sound effect they can select, look up and correct in
    the OCR editor and one that is not there at all: a wrong read is visible
    as wrong, with the glyphs in front of them, while a missing box is
    invisible and nothing downstream can recover it. Dropping it would not
    hand them the right text either. The choice barely moves the rule in any
    case -- only 4 of the 62 lines it keeps on held-out data are wrong reads,
    because ink the engine misreads is usually ink the detector doubted too.

    What this knowingly gets wrong, measured on the held-out half of a corpus
    of 335 judged engine-only lines -- 174 lines from 81 pages: 66 read right,
    24 read wrong, 84 art. It is the half the ``body`` parameters were not
    swept on; where each constant actually comes from is below.

    Read every precision figure below as a slight over-estimate, because the
    corpus was re-judged in one direction only. After a first pass, 27 rows
    labelled art whose reads were nothing but dash, dot or ellipsis
    punctuation were looked at again in PAGE context rather than as crops, and
    6 of them turned out to be lettering (a long-vowel stroke between the two
    kana of a hand-lettered balloon, a typeset ellipsis inside a speech
    balloon, a fragment of a printed dash). No row labelled lettering was
    re-judged; the pass noticed no counter-example on the pages it happened to
    have open, which is not the same as having looked. So mistakes that cost
    the rule recall were hunted down and fixed, while mistakes that flatter
    its precision were never looked for. Removing that bias means re-judging
    a sample of the lettering rows the same way, in page context, and nobody
    has.

    It keeps 62, of which 6 are art, every one of them through ``backed``:
    hatching and speed lines inside a panel, two sets of emphasis strokes
    beside a figure, an ink illustration, dark silhouette shapes and a piece
    of colour art on clothing, scored 0.81 to 0.89.

    It drops 112, of which 34 are lettering and 14 of those were read
    correctly. Those 14 are small -- seven of one glyph, six of two, one of
    three -- and they are ten pieces of hand-lettering (sound effects, a title
    glyph, a long-vowel stroke) and four printed glyphs (a ruby, a kana in
    running text, a page number). Their scores run 0.28 to 0.79, so most are
    nowhere near the bar rather than just under it. Ten of the fourteen have
    no read text parallel to them, which is what the ``body`` rule asks for;
    the other four fail it on geometry or score -- three are as thick as the
    body cell or thicker (0.82 to 1.77 of the pitch), one sits at 0.28.

    Without the ``body`` rule the same half keeps 52 and drops 24
    correctly-read lines instead of 14, at the same 6 pieces of art. Buying
    most of those ten back by moving ``DETECTOR_SURE`` to 0.75 instead keeps
    67 and doubles the art to 12. The guard this whole rule replaced (display
    size plus a second engine read) kept 43 with 8 art in them and dropped 32
    correctly-read lines.

    Where the constants come from, which is not one method:

    * ``DETECTOR_SURE`` = 0.80 is not an F1 choice. On the fitting half F1
      only rises as the bar falls -- 0.60 scores 0.71 against 0.80's 0.60 --
      so an F1 sweep would put the bar at the bottom of its range. 0.80 is the
      step in the corpus itself (55% lettering in 0.65-0.80, 87% above),
      taken because a phantom box costs a reader more than a missing one.
    * ``DETECTOR_REGION`` = 0.60 this corpus cannot see at all. Only two
      region keeps fall below 0.80, at 0.77 and 0.79, so every bar from 0.40
      to 0.75 gives the same confusion matrix. It is reasoning -- room below
      ``DETECTOR_SURE`` for ink a line detector is right about and unsure of
      at once -- and not a measurement.
    * ``DETECTOR_BODY`` = 0.65, ``BODY_MAX_PITCH_SHARE`` = 0.8 and
      ``BODY_MIN_NEIGHBOURS`` = 1 were swept on the fitting half, and 0.65 is
      not that sweep's best cell: 0.60 is, by one line (43 kept with 9 art,
      against 42 with 8). 0.65 is the round value inside a band the corpus
      cannot tell apart. 0.7, 0.8 and 0.9 for the pitch share score
      identically there; 1 neighbour beats 2.

    The corpus is six volumes and 163 pages, and it cannot resolve a bar to
    better than about a twentieth: every threshold here is a round number
    chosen on a band, not on a line or two.

    LIMITATION -- ON A DECORATIVE PAGE THE DETECTOR'S SCORE IS CONFIDENTLY
    WRONG, and the corpus above contains no such page, so none of the numbers
    in it measure this. A fresh 25-page sample (15 manga, 10 novel) drawn
    afterwards produced 43 engine-only decisions and 18 keeps, of which 6 were
    lettering: precision 0.333, against 0.875 on the corpus. Excluding two
    decorative pages -- one covered in drawn star and flower sparkles, one a
    full-colour illustration with no body pitch at all -- it is 0.667.

    * Nine of the twelve false keeps came through ``backed``, at 0.817 to
      0.895: drawn stars and flower sparkles, blue feather shapes in a colour
      illustration, a yellow zigzag on a figure, hatched speed lines, and a
      face's eyes and eyebrows. That clause is the one this rule leaves
      untouched, and it is where the precision goes.
    * The ``body`` clause fired 4 times: once on lettering, three times on
      art, and all three of those on the one sparkle-covered page, at 0.728
      to 0.739. On the corpus it ran 13 lettering to 1 art.

    A drawn star is a small, closed, high-contrast stroke with clean edges,
    which is what DBNet was trained to score as a line, so it comes back at
    0.89 exactly as a kana does. No threshold on that score separates the two,
    and no clause here has another signal to fall back on: the page's pitch
    and a quad's company only establish that the page HAS running text, which
    a decorated page does too. This is the ceiling of the design rather than a
    tuning error. Getting past it needs something the detector does not give
    us -- ink statistics inside the quad, a recognizer confidence that means
    anything on art, or a page-level "this page is decorated" signal.
    """
    if engine_looped(line.vlm, cells, axis_cells(main, thickness, pitch)):
        return False, "looped"
    if det_score >= DETECTOR_SURE:
        return True, "backed"
    if (
        det_score >= DETECTOR_REGION
        and is_region(main, thickness, pitch)
        and len(fold(line.text)) >= ROOM_MIN_CELLS
    ):
        return True, "region"
    if (
        det_score >= DETECTOR_BODY
        and pitch > 0
        and neighbours >= BODY_MIN_NEIGHBOURS
        and 0 < thickness <= BODY_MAX_PITCH_SHARE * pitch
        and fold(line.text)
    ):
        return True, "body"
    return False, "unbacked"


def _single_disputes(
    opcodes: Sequence[tuple[str, int, int, int, int]],
) -> list[tuple[int, int, int, int]]:
    """The differing stretches of an alignment, cut down to one glyph each.

    The aligner fuses neighbouring differences into one ``replace``; a vote on
    the whole stretch is all or nothing, and a rare kanji next to another
    doubtful glyph is this path's everyday case. Glyphs are paired off from
    the end that touches agreed text (from the left, except at a line's
    start); what one side has more of is left as an insertion or deletion of
    its own. A wrong pairing costs nothing: its votes simply fail.
    """
    out: list[tuple[int, int, int, int]] = []
    for tag, i1, i2, j1, j2 in opcodes:
        if tag == "equal":
            continue
        pairs = min(i2 - i1, j2 - j1)
        if i1 == 0 and pairs:
            out.append((i1, i2 - pairs, j1, j2 - pairs))
            out.extend((i2 - n, i2 - n + 1, j2 - n, j2 - n + 1) for n in range(pairs, 0, -1))
        else:
            out.extend((i1 + n, i1 + n + 1, j1 + n, j1 + n + 1) for n in range(pairs))
            out.append((i1 + pairs, i2, j1 + pairs, j2))
    return [(i1, i2, j1, j2) for i1, i2, j1, j2 in out if i2 > i1 or j2 > j1]


def _dashes_folded(values: Sequence[str]) -> list[str]:
    """``values`` with every dash, and every "ー" that cannot be a long vowel, as ``DASH``.

    For counting votes only: the engine writes a printed "―" as "-", "—" or
    "ー" from one crop to the next, and the CTC read's "―" should find its
    witness in any of them.
    """
    out: list[str] = []
    for i, value in enumerate(values):
        dash = value in ENGINE_DASHES or (
            value == LONG_VOWEL and (i == 0 or not _is_kana(values[i - 1]))
        )
        out.append(DASH if dash else value)
    return out


def _matches(a: Sequence[str], b: Sequence[str]) -> int:
    """Characters two token lists share, in order."""
    blocks = SequenceMatcher(None, list(a), list(b), autojunk=False).get_matching_blocks()
    return sum(block.size for block in blocks)


def overlap_repeat(before: str, after: str, max_glyphs: int) -> int:
    """Glyphs at the start of ``after`` that repeat the end of ``before``.

    For two pieces of one column that could not be joined and whose crops
    share ink: the longest suffix of ``before`` that ``after`` starts with, no
    longer than the shared stretch has room for (``max_glyphs``). Without
    that bound "ドドド" + "ドドド" would lose half its sound.
    """
    a, b = before.strip(), after.strip()
    for n in range(min(len(a), len(b), max(0, max_glyphs)), 0, -1):
        if a[-n:] == b[:n]:
            return n
    return 0


def page_summary(lines: Sequence[Reconciled]) -> dict[str, Any]:
    """Tally for the raw dump and the log: how the page's lines were settled."""
    compared = [ln.agreement for ln in lines if ln.agreement is not None]
    notes: dict[str, int] = {}
    for ln in lines:
        for note in ln.notes:
            notes[note] = notes.get(note, 0) + 1
    return {
        "lines": len(lines),
        "compared": len(compared),
        "full_agreement": sum(1 for a in compared if a >= 1.0),
        "mean_agreement": round(sum(compared) / len(compared), 4) if compared else None,
        "from_ctc": sum(1 for ln in lines if ln.source == "ctc"),
        "notes": dict(sorted(notes.items())),
    }
