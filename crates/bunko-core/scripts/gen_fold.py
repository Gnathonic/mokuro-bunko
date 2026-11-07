#!/usr/bin/env python3
"""Fold v2: the one name fold Bunko and Mokuro Reader decide every name question by.

Generates, from the Unicode Character Database 17.0.0:

* crates/bunko-core/src/fold_table.rs   the Rust tables (`FOLD_MAP`, `DEFAULT_IGNORABLE`)
* interop/contract/fold-table.json      the same tables plus the canonical normalization
                                        data, normative for the reader
* interop/contract/fold.json            the fold's vectors, pairs, series_key vectors and
                                        the digest over every scalar value

Usage (stdlib only; run isolated):

    python3 -I crates/bunko-core/scripts/gen_fold.py [--ucd-dir DIR]          # write
    python3 -I crates/bunko-core/scripts/gen_fold.py [--ucd-dir DIR] --check  # CI

The UCD files are fetched into `--ucd-dir` (default `~/.cache/bunko/ucd/17.0.0`) when
missing, and each is checked against the SHA-256 pinned below. `--check` regenerates
everything in memory and fails on any difference with the committed files.

F(s), fold v2:
  * s is ASCII: its ASCII lowercase;
  * otherwise: drop Default_Ignorable_Code_Point, NFD, map each code point c to
    M(c) = casefold(full_upper(casefold(c))), NFC, drop Default_Ignorable_Code_Point.
casefold is CaseFolding.txt status C+F; full_upper is UnicodeData's simple uppercase
overridden by the unconditional SpecialCasing.txt entries (context-free, as Rust's
str::to_uppercase). The NFD/NFC here is this script's own (Python's unicodedata is
another UCD version), checked against NormalizationTest.txt before anything is written.

Generation fails unless: the reference normalizer passes NormalizationTest.txt; F is
idempotent; no code point folds to or from '.' or '/'; F(a.b) = F(a).F(b) over a fuzz
pool; and what Mokuro Reader 1.10.0 (lowercase) and sidecar names rule v1 (NFC, then
lowercase) join, F joins too.
"""
import argparse
import hashlib
import json
import os
import random
import sys
import urllib.request

UCD = "17.0.0"
FOLD = 2
BASE_URL = "https://www.unicode.org/Public/%s/ucd/" % UCD
PINS = {
    "UnicodeData.txt": "2e1efc1dcb59c575eedf5ccae60f95229f706ee6d031835247d843c11d96470c",
    "CaseFolding.txt": "ff8d8fefbf123574205085d6714c36149eb946d717a0c585c27f0f4ef58c4183",
    "SpecialCasing.txt": "efc25faf19de21b92c1194c111c932e03d2a5eaf18194e33f1156e96de4c9588",
    "DerivedCoreProperties.txt": "24c7fed1195c482faaefd5c1e7eb821c5ee1fb6de07ecdbaa64b56a99da22c08",
    "CompositionExclusions.txt": "2f239196ef3b5b61db5cc476e9bd80f534d15aa1b74e1be1dea5d042a344c85f",
    "NormalizationTest.txt": "5019ffd530751a741900c849c0e010332f142a3612234639bd200b82138a87db",
}

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))
OUT_RS = os.path.join(ROOT, "crates", "bunko-core", "src", "fold_table.rs")
OUT_TABLE = os.path.join(ROOT, "interop", "contract", "fold-table.json")
OUT_FOLD = os.path.join(ROOT, "interop", "contract", "fold.json")

SCALARS = [cp for cp in range(0x110000) if not 0xD800 <= cp <= 0xDFFF]

# Hangul syllables (Unicode 3.12, conjoining jamo behavior).
S_BASE, L_BASE, V_BASE, T_BASE = 0xAC00, 0x1100, 0x1161, 0x11A7
L_COUNT, V_COUNT, T_COUNT = 19, 21, 28
N_COUNT = V_COUNT * T_COUNT
S_COUNT = L_COUNT * N_COUNT

# series_key's whitespace, applied after F: Python's str.isspace() united with JS `\s`.
SERIES_WHITESPACE = (
    list(range(0x09, 0x0E))
    + list(range(0x1C, 0x20))
    + [0x20, 0x85, 0xA0, 0x1680]
    + list(range(0x2000, 0x200B))
    + [0x2028, 0x2029, 0x202F, 0x205F, 0x3000]
)


# --- the UCD ---------------------------------------------------------------------------


def ucd_file(ucd_dir, name):
    path = os.path.join(ucd_dir, name)
    if not os.path.exists(path):
        os.makedirs(ucd_dir, exist_ok=True)
        tmp = path + ".part"
        with urllib.request.urlopen(BASE_URL + name) as r, open(tmp, "wb") as f:
            f.write(r.read())
        os.replace(tmp, path)
    with open(path, "rb") as f:
        data = f.read()
    got = hashlib.sha256(data).hexdigest()
    if got != PINS[name]:
        sys.exit("%s: sha256 %s, pinned %s (UCD %s)" % (path, got, PINS[name], UCD))
    return data.decode("utf-8")


def data_lines(text):
    for line in text.splitlines():
        body = line.split("#", 1)[0].strip()
        if body:
            yield [field.strip() for field in body.split(";")]


def cps(field):
    return [int(x, 16) for x in field.split()]


class Ucd:
    def __init__(self, ucd_dir):
        self.ccc = {}
        self.decomp = {}  # canonical, one level, as UnicodeData lists it
        self.upper = {}
        self.lower = {}
        self.assigned = set()
        first = None
        for f in data_lines(ucd_file(ucd_dir, "UnicodeData.txt")):
            cp = int(f[0], 16)
            if f[1].endswith(", First>"):
                first = cp
                continue
            if f[1].endswith(", Last>"):
                self.assigned.update(range(first, cp + 1))
                continue
            self.assigned.add(cp)
            if int(f[3]):
                self.ccc[cp] = int(f[3])
            if f[5] and not f[5].startswith("<"):
                self.decomp[cp] = cps(f[5])
            if f[12]:
                self.upper[cp] = [int(f[12], 16)]
            if f[13]:
                self.lower[cp] = [int(f[13], 16)]
        for f in data_lines(ucd_file(ucd_dir, "SpecialCasing.txt")):
            if len(f) > 4 and f[4]:
                continue  # conditional (Final_Sigma, locales): never context-free
            cp = int(f[0], 16)
            self.lower[cp] = cps(f[1])
            self.upper[cp] = cps(f[3])
        self.casefold = {}
        for f in data_lines(ucd_file(ucd_dir, "CaseFolding.txt")):
            if f[1] in ("C", "F"):
                self.casefold[int(f[0], 16)] = cps(f[2])
        ignorable = []
        for f in data_lines(ucd_file(ucd_dir, "DerivedCoreProperties.txt")):
            if f[1] == "Default_Ignorable_Code_Point":
                lo, _, hi = f[0].partition("..")
                ignorable.append((int(lo, 16), int(hi or lo, 16)))
        self.ignorable_ranges = merge_ranges(ignorable)
        self.ignorable = set()
        for lo, hi in self.ignorable_ranges:
            self.ignorable.update(range(lo, hi + 1))
        excluded = set()
        for f in data_lines(ucd_file(ucd_dir, "CompositionExclusions.txt")):
            excluded.add(int(f[0], 16))
        # Full_Composition_Exclusion: the listed exclusions, singletons and non-starter
        # decompositions.
        for cp, d in self.decomp.items():
            if len(d) == 1 or self.ccc.get(cp, 0) or self.ccc.get(d[0], 0):
                excluded.add(cp)
        self.exclusions = excluded
        self.compose = {}
        for cp, d in self.decomp.items():
            if cp not in excluded:
                assert len(d) == 2, "U+%04X" % cp
                self.compose[(d[0], d[1])] = cp
        self.full_decomp = {}
        for cp in self.decomp:
            self.full_decomp[cp] = self._decompose(cp)
        self.normalization_test = ucd_file(ucd_dir, "NormalizationTest.txt")

    def _decompose(self, cp):
        d = self.decomp.get(cp)
        if d is None:
            return [cp]
        out = []
        for c in d:
            out.extend(self._decompose(c))
        return out


def merge_ranges(ranges):
    merged = []
    for lo, hi in sorted(ranges):
        if merged and lo <= merged[-1][1] + 1:
            merged[-1] = (merged[-1][0], max(hi, merged[-1][1]))
        else:
            merged.append((lo, hi))
    return merged


def ranges_of(cps_set):
    return merge_ranges((c, c) for c in cps_set)


# --- the reference normalizer and the fold -------------------------------------------


class Fold:
    def __init__(self, ucd):
        self.u = ucd
        self.m = {}
        for cp in SCALARS:
            once = self.casefold([cp])
            up = []
            for c in once:
                up.extend(ucd.upper.get(c, [c]))
            m = self.casefold(up)
            if m != [cp]:
                self.m[cp] = m

    def casefold(self, s):
        out = []
        for c in s:
            out.extend(self.u.casefold.get(c, [c]))
        return out

    def nfd(self, s):
        out = []
        for c in s:
            if S_BASE <= c < S_BASE + S_COUNT:
                i = c - S_BASE
                out.append(L_BASE + i // N_COUNT)
                out.append(V_BASE + (i % N_COUNT) // T_COUNT)
                if i % T_COUNT:
                    out.append(T_BASE + i % T_COUNT)
            else:
                out.extend(self.u.full_decomp.get(c, (c,)))
        # Canonical ordering: a stable sort of every run of non-starters by class.
        ccc = self.u.ccc
        i = 0
        n = len(out)
        while i < n:
            if ccc.get(out[i], 0) == 0:
                i += 1
                continue
            j = i
            while j < n and ccc.get(out[j], 0) != 0:
                j += 1
            if j - i > 1:
                out[i:j] = sorted(out[i:j], key=lambda c: ccc[c])
            i = j
        return out

    def compose_pair(self, a, b):
        if L_BASE <= a < L_BASE + L_COUNT and V_BASE <= b < V_BASE + V_COUNT:
            return S_BASE + ((a - L_BASE) * V_COUNT + (b - V_BASE)) * T_COUNT
        if S_BASE <= a < S_BASE + S_COUNT and (a - S_BASE) % T_COUNT == 0 and T_BASE < b < T_BASE + T_COUNT:
            return a + (b - T_BASE)
        return self.u.compose.get((a, b))

    def nfc(self, s):
        d = self.nfd(s)
        ccc = self.u.ccc
        out = []
        starter = None
        last = None  # class of the last character kept after the starter
        for c in d:
            cc = ccc.get(c, 0)
            if starter is not None and (last is None or last < cc):
                p = self.compose_pair(out[starter], c)
                if p is not None:
                    out[starter] = p
                    continue
            if cc == 0:
                starter = len(out)
                last = None
            else:
                last = cc
            out.append(c)
        return out

    def fold(self, s):
        if all(c < 0x80 for c in s):
            return [c + 32 if 0x41 <= c <= 0x5A else c for c in s]
        ign = self.u.ignorable
        x = self.nfd([c for c in s if c not in ign])
        y = []
        for c in x:
            y.extend(self.m.get(c, (c,)))
        return [c for c in self.nfc(y) if c not in ign]

    def lower(self, s):
        out = []
        for c in s:
            out.extend(self.u.lower.get(c, [c]))
        return out


def text(s):
    return "".join(map(chr, s))


def cp_list(t):
    return [ord(c) for c in t]


# --- checks ----------------------------------------------------------------------------


def check_normalizer(f):
    listed = set()
    part = None
    count = 0
    for line in f.u.normalization_test.splitlines():
        if line.startswith("@Part"):
            part = line.split()[0]
            continue
        body = line.split("#", 1)[0].strip()
        if not body:
            continue
        cols = [cps(x) for x in body.split(";")[:5]]
        c1, c2, c3, c4, c5 = cols
        if part == "@Part1":
            listed.add(c1[0])
        for x in (c1, c2, c3):
            assert f.nfc(x) == c2, ("NFC", line)
            assert f.nfd(x) == c3, ("NFD", line)
        for x in (c4, c5):
            assert f.nfc(x) == c4, ("NFC", line)
            assert f.nfd(x) == c5, ("NFD", line)
        count += 1
    for cp in SCALARS:
        if cp not in listed:
            assert f.nfd([cp]) == [cp] and f.nfc([cp]) == [cp], "U+%04X" % cp
    return count


def check_fold(f):
    dot, slash = ord("."), ord("/")
    for c in range(0x80):
        assert f.fold([0x100, c])[1:] == f.fold([c]), "ASCII U+%04X" % c
    for cp in SCALARS:
        once = f.fold([cp])
        assert f.fold(once) == once, "F is not idempotent at U+%04X" % cp
        if cp not in (dot, slash):
            assert dot not in once and slash not in once, "U+%04X folds to . or /" % cp
        # What 1.10.0 (lowercase) and rule v1 (NFC, then lowercase) join, F joins.
        assert f.fold(f.lower([cp])) == once, "R0 at U+%04X" % cp
        assert f.fold(f.lower(f.nfc([cp]))) == once, "rule v1 at U+%04X" % cp
    # The one context-dependent lowercase: a final capital sigma.
    assert f.fold([0x3C2]) == f.fold([0x3A3]) == f.fold([0x3C3])
    # Cased letters with a mark, decomposed and composed.
    marks = [c for c in range(0x300, 0x370)] + [0x3099, 0x309A, 0x20D7, 0x1AB0]
    cased = [c for c in SCALARS if c in f.m or c in f.u.lower or c in f.u.upper]
    for c in cased:
        for mk in marks:
            s = [c, mk]
            once = f.fold(s)
            assert f.fold(once) == once, ("idempotent", s)
            assert f.fold(f.lower(s)) == once, ("R0", s)
            assert f.fold(f.lower(f.nfc(s))) == once, ("rule v1", s)
            assert f.fold(f.nfc(s)) == once and f.fold(f.nfd(s)) == once, ("norm", s)
    # F(a.b) = F(a).F(b) over a fuzz pool.
    pool = [
        0x41, 0x61, 0x49, 0x69, 0x131, 0x130, 0x307, 0x301, 0x300, 0x345, 0x3A3, 0x3C3,
        0x3C2, 0xDF, 0x1E9E, 0x17F, 0x212A, 0x200B, 0x200D, 0x34F, 0xFE0F, 0xAD, 0xE0100,
        0x65, 0xE9, 0x304B, 0x3099, 0x304C, 0x1100, 0x1161, 0x11A8, 0xAC00, 0x2E, 0x20,
        0x3B9, 0x399, 0x1F80, 0x1FB3, 0xFB01, 0x1C5, 0xF900, 0x2126, 0x1F600, 0xFF21,
    ]
    rng = random.Random(20261107)
    for _ in range(100000):
        a = [rng.choice(pool) for _ in range(rng.randint(0, 6))]
        b = [rng.choice(pool) for _ in range(rng.randint(0, 6))]
        whole = f.fold(a + [dot] + b)
        assert whole == f.fold(a) + [dot] + f.fold(b), (a, b)
        assert f.fold(whole) == whole, (a, b)


def digest(f):
    h = hashlib.sha256()
    a, acute = ord("a"), 0x301
    for cp in SCALARS:
        for probe in ([cp], [a, cp], [cp, acute]):
            h.update(text(f.fold(probe)).encode("utf-8"))
            h.update(b"\n")
    return h.hexdigest()


# --- vectors ---------------------------------------------------------------------------

ZWSP, ZWJ, KELVIN = "\u200b", "\u200d", "\u212a"

VECTORS = [
    # Case.
    ("Kingdom", "ASCII: its lowercase"),
    ("Kingdom/Vol 1.CBZ", "ASCII path"),
    ("Stra\u00dfe", "sharp s folds to ss"),
    ("STRASSE", "ASCII"),
    ("\u1e9e", "capital sharp s"),
    ("\u017f", "long s"),
    ("\u03c2", "final sigma folds to sigma"),
    ("\u03a3", "capital sigma"),
    ("\u039f\u0394\u039f\u03a3", "no context: the last sigma is a sigma"),
    ("\u039f\u0394\u039f\u03a3.gcv", "and before a dot too"),
    ("\u039f\u0394\u039f\u03c2.gcv", "a final small sigma is a sigma"),
    (KELVIN, "Kelvin sign is k"),
    ("\u0131", "dotless i: uppercase I, so i"),
    ("I", "ASCII"),
    ("\u0130", "I with dot above: i + combining dot above"),
    ("i\u0307", "already folded"),
    ("\ufb01", "fi ligature: fi"),
    ("\u01c5", "titlecase digraph"),
    ("\u0345", "ypogegrammeni: iota"),
    ("\u1f80", "Greek with ypogegrammeni"),
    ("\u2126", "Ohm sign: omega"),
    # Normalization.
    ("Pok\u00e9mon", "NFC input"),
    ("POKE\u0301MON", "NFD input folds to NFC"),
    ("\u304c", "ga"),
    ("\u304b\u3099", "ka + voiced mark composes"),
    ("\uf900", "a CJK compatibility ideograph decomposes canonically"),
    ("\ud55c\uae00", "Hangul syllables stay composed"),
    ("\u1112\u1161\u11ab", "conjoining jamo compose"),
    # Default_Ignorable.
    ("Vol" + ZWSP + "1", "zero width space"),
    ("Vol" + ZWJ + "1", "zero width joiner"),
    ("Vol\u200e1", "left-to-right mark"),
    ("Vol1\ufe0f", "variation selector 16"),
    ("Vol\u20601", "word joiner"),
    ("E\u034f\u0301", "grapheme joiner between a letter and its accent: the accent still joins"),
    ("Vol\u31641", "Hangul filler"),
    ("SER\u00adIES.json", "soft hyphen"),
    ("Vol1\U000e0100", "variation selector 17"),
    ("\ufeffVol1", "byte order mark"),
    # Kept as written.
    ("a\u0600b", "Arabic number sign is not default-ignorable"),
    ("a\u00a0b", "no-break space is kept"),
    # Names from the 0.8 leak work: each folds to its ASCII twin.
    ("Q.ed" + ZWSP + "it.edit.cbz", "a reserved-layer hider"),
    ("Q.cura" + ZWSP + "ted.curated.cbz", "a curated hider"),
    ("V.ed\u0131t.curated.cbz", "dotless i in a layer id"),
    ("X" + ZWSP + ".curated.cbz", "zero width space in a stem"),
    ("X" + ZWSP + ".cbz", "zero width space before the suffix"),
    ("X.5" + ZWSP + ".cbz", "after a digit id"),
    ("\u039f\u0394\u039f\u03c2.gcv.cbz", "final sigma before a layer id"),
    ("Cafe\u0301.gcv.cbz", "NFD stem"),
    ("STRASSE.curated.cbz", "ASCII"),
    ("Stra\u00dfe.curated.cbz", "sharp s stem"),
    ("S/STRASSE.mokuro", "a path"),
    ("V.EDIT.mokuro", "ASCII id case"),
    ("V.ed" + ZWSP + "it.mokuro", "zero width space in an id"),
    ("V.e\u034fdit.mokuro", "grapheme joiner in an id"),
    ("V.edit-" + ZWSP + "2.mokuro", "zero width space in a counted id"),
    ("V.mo" + KELVIN + "uro", "a suffix spelled with a Kelvin sign"),
    ("V.cb" + ZWSP + "z", "a suffix with a zero width space"),
    ("V.cbz" + ZWSP, "a zero width space after the suffix"),
    ("V.\u212aanji.cbz", "Kelvin sign in an id"),
    ("\u017feries.json", "a fixed name"),
    ("series" + ZWSP + ".json", "a fixed name"),
    ("Vol\u017f.curated.cbz", "long s in a stem"),
    ("\u0131X.curated.cbz", "dotless i in a stem"),
    ("\U0001f600", "astral, no case"),
    ("\uff21", "fullwidth A folds to fullwidth a"),
]

SAME = [
    ("Kingdom", "KINGDOM", "ASCII case"),
    ("Stra\u00dfe", "STRASSE", "full case folding"),
    ("Stra\u00dfe", "strasse", "full case folding"),
    ("\u1e9e", "ss", "capital sharp s"),
    ("\u017f.cbz", "s.cbz", "long s"),
    ("\u039f\u0394\u039f\u03a3", "\u03bf\u03b4\u03bf\u03c2", "sigma, either form"),
    ("\u03bf\u03b4\u03bf\u03c2", "\u03bf\u03b4\u03bf\u03c3", "final sigma"),
    ("\u039f\u0394\u039f\u03a3.gcv", "\u03bf\u03b4\u03bf\u03c3.gcv", "no context"),
    (KELVIN, "k", "Kelvin sign"),
    ("\u0131", "I", "an uppercase table joins dotless i with I"),
    ("\u0131", "i", "and so with i"),
    ("\u0130", "i\u0307", "I with dot above"),
    ("\ufb01le", "FILE", "ligature"),
    ("Pok\u00e9mon", "POKE\u0301MON", "NFC and NFD"),
    ("\u304c", "\u304b\u3099", "kana voiced mark"),
    ("\u01f1", "\u01f3", "DZ digraph"),
    ("\u0345", "\u03b9", "ypogegrammeni"),
    ("Vol" + ZWSP + "1", "Vol1", "zero width space"),
    ("Vol1\ufe0f", "VOL1", "variation selector"),
    ("E\u034f\u0301", "\u00e9", "grapheme joiner"),
    ("Q.ed" + ZWSP + "it.edit.cbz", "q.edit.edit.cbz", "leak: reserved-layer hider"),
    ("V.ed\u0131t.curated.cbz", "v.edit.curated.cbz", "leak: dotless i"),
    ("X" + ZWSP + ".curated.cbz", "x.curated.cbz", "leak: zero width space stem"),
    ("Cafe\u0301.gcv.cbz", "caf\u00e9.gcv.cbz", "leak: NFD"),
    ("\u039f\u0394\u039f\u03c2.gcv.cbz", "\u03bf\u03b4\u03bf\u03c3.gcv.cbz", "leak: final sigma"),
    ("V.EDIT.mokuro", "V.edit.mokuro", "id case"),
    ("V.ed" + ZWSP + "it.mokuro", "V.edit.mokuro", "zero width space id"),
    ("V.mo" + KELVIN + "uro", "v.mokuro", "Kelvin suffix"),
    ("V.cb" + ZWSP + "z", "V.cbz", "zero width space suffix"),
    ("\u017feries.json", "series.json", "fixed name"),
]

DIFFERENT = [
    ("a", "b", "different letters"),
    ("Vol 1", "Vol 2", "different numbers"),
    ("a\u0600b", "ab", "Arabic number sign is kept"),
    ("a\u00a0b", "a b", "no-break space is not a space"),
    ("a\u00a0b", "ab", "nor nothing"),
    ("e", "\u00e9", "an accent counts"),
    ("\u304b", "\u304c", "a voiced mark counts"),
    ("\u0131", "\u0130", "dotless i and I with dot above"),
    ("Vol 1", "Vol  1", "whitespace counts in names"),
    ("\U0001f600", "\uff21", "unrelated"),
    ("V.edit.mokuro", "V.edit-2.mokuro", "a counted id"),
]

SERIES_KEYS = [
    ("  Dr   Stone ", "trim, then collapse"),
    ("Dr\tStone", "a tab is whitespace"),
    ("A\u3000\u001cB", "ideographic space and an information separator"),
    ("A " + ZWSP + " B", "a zero width space inside whitespace is dropped first"),
    ("\ufeffDr Stone", "byte order mark"),
    ("Stra\u00dfe", "the fold"),
    ("STRASSE", "the fold"),
    ("A\u00a0B", "no-break space is whitespace for series keys"),
    ("A\u2003\u2003B", "em spaces"),
    ("\u3000", "only whitespace"),
]


def series_key(f, s):
    ws = set(SERIES_WHITESPACE)
    out = []
    pending = False
    for c in f.fold(s):
        if c in ws:
            pending = bool(out)
            continue
        if pending:
            out.append(0x20)
            pending = False
        out.append(c)
    return out


# --- outputs ---------------------------------------------------------------------------


def hx(n):
    return "%x" % n


def signed(n):
    return hx(n) if n >= 0 else "-" + hx(-n)


def enc_ranges(ranges):
    out, prev = [], 0
    for lo, hi in ranges:
        out.append("%s:%s" % (hx(lo - prev), hx(hi - lo + 1)))
        prev = hi + 1
    return ";".join(out)


def enc_map(mapping):
    out, prev = [], 0
    for cp in sorted(mapping):
        out.append("%s:%s" % (hx(cp - prev), ",".join(signed(t - cp) for t in mapping[cp])))
        prev = cp
    return ";".join(out)


def enc_decomp(mapping):
    out, prev = [], 0
    for cp in sorted(mapping):
        out.append("%s:%s" % (hx(cp - prev), ",".join(hx(t) for t in mapping[cp])))
        prev = cp
    return ";".join(out)


def enc_ccc(ccc):
    runs = []
    for cp in sorted(ccc):
        if runs and runs[-1][1] == cp - 1 and runs[-1][2] == ccc[cp]:
            runs[-1][1] = cp
        else:
            runs.append([cp, cp, ccc[cp]])
    out, prev = [], 0
    for lo, hi, c in runs:
        out.append("%s:%s:%s" % (hx(lo - prev), hx(hi - lo + 1), hx(c)))
        prev = hi + 1
    return ";".join(out)


def json_text(value):
    return json.dumps(value, ensure_ascii=True, indent=2) + "\n"


def rust_str(s):
    return '"' + "".join("\\u{%x}" % c for c in s) + '"'


def outputs(ucd, f):
    table = {
        "fold": FOLD,
        "ucd": UCD,
        "map": enc_map(f.m),
        "ignorable": enc_ranges(ucd.ignorable_ranges),
        "decomp": enc_decomp(ucd.decomp),
        "ccc": enc_ccc(ucd.ccc),
        "exclusions": enc_ranges(ranges_of(ucd.exclusions)),
    }
    table_text = json_text(table)
    folded = lambda t: text(f.fold(cp_list(t)))
    for a, b, why in SAME:
        assert folded(a) == folded(b), (a, b, why)
    for a, b, why in DIFFERENT:
        assert folded(a) != folded(b), (a, b, why)
    key = lambda t: text(series_key(f, cp_list(t)))
    assert key("Stra\u00dfe") == key("STRASSE")
    assert key("Dr Stone") == key("  Dr   Stone ") == key("Dr  Stone")
    fold_json = {
        "fold": FOLD,
        "ucd": UCD,
        "table_sha256": hashlib.sha256(table_text.encode("utf-8")).hexdigest(),
        "digest": {
            "probes": "for every Unicode scalar value c in order: F(c), F('a'+c), "
            "F(c+U+0301), each UTF-8 and LF-terminated",
            "sha256": digest(f),
        },
        "vectors": [{"in": s, "out": folded(s), "why": why} for s, why in VECTORS],
        "same": [{"a": a, "b": b, "why": why} for a, b, why in SAME],
        "different": [{"a": a, "b": b, "why": why} for a, b, why in DIFFERENT],
        "series_key": [
            {"in": s, "out": text(series_key(f, cp_list(s))), "why": why} for s, why in SERIES_KEYS
        ],
    }
    rs = []
    rs.append("//! Fold v2's tables (UCD %s), generated by\n" % UCD)
    rs.append("//! `crates/bunko-core/scripts/gen_fold.py` with `interop/contract/fold-table.json`;\n")
    rs.append("//! do not edit. [`crate::names::fold_name`] reads them.\n\n")
    rs.append("/// The Unicode Character Database release the tables are made from.\n")
    rs.append('pub const FOLD_UCD: &str = "%s";\n\n' % UCD)
    rs.append("/// `M(c) = casefold(full_upper(casefold(c)))` for every scalar value it changes,\n")
    rs.append("/// sorted by code point.\n")
    rs.append("#[rustfmt::skip]\n")
    rs.append("pub static FOLD_MAP: &[(u32, &str)] = &[\n")
    for cp in sorted(f.m):
        rs.append("    (0x%04X, %s),\n" % (cp, rust_str(f.m[cp])))
    rs.append("];\n\n")
    rs.append("/// Default_Ignorable_Code_Point (DerivedCoreProperties.txt) as inclusive ranges,\n")
    rs.append("/// sorted.\n")
    rs.append("#[rustfmt::skip]\n")
    rs.append("pub static DEFAULT_IGNORABLE: &[(u32, u32)] = &[\n")
    for lo, hi in ucd.ignorable_ranges:
        rs.append("    (0x%04X, 0x%04X),\n" % (lo, hi))
    rs.append("];\n")
    return {
        OUT_RS: "".join(rs),
        OUT_TABLE: table_text,
        OUT_FOLD: json_text(fold_json),
    }


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    p.add_argument("--ucd-dir", default=os.path.join(os.path.expanduser("~"), ".cache", "bunko", "ucd", UCD))
    p.add_argument("--check", action="store_true", help="fail when a committed output differs")
    args = p.parse_args()
    ucd = Ucd(args.ucd_dir)
    f = Fold(ucd)
    n = check_normalizer(f)
    print("normalizer: %d NormalizationTest.txt lines pass" % n, file=sys.stderr)
    check_fold(f)
    print("fold: %d mapped code points, checks pass" % len(f.m), file=sys.stderr)
    stale = []
    for path, content in outputs(ucd, f).items():
        rel = os.path.relpath(path, ROOT)
        if args.check:
            try:
                with open(path, encoding="utf-8", newline="") as fh:
                    if fh.read() != content:
                        stale.append(rel)
            except FileNotFoundError:
                stale.append(rel)
        else:
            with open(path, "w", encoding="utf-8", newline="") as fh:
                fh.write(content)
            print("wrote " + rel, file=sys.stderr)
    if stale:
        sys.exit("stale, rerun gen_fold.py: " + ", ".join(stale))


if __name__ == "__main__":
    main()
