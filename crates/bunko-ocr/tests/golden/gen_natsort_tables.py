"""Regenerate src/natsort_tables.rs from Python's Unicode data and natsort 8.4.

    ~/.cache/mokuro-bunko-demo/ref052-ocr/bin/python \
        crates/bunko-ocr/tests/golden/gen_natsort_tables.py > crates/bunko-ocr/src/natsort_tables.rs
"""

import re
import sys
import unicodedata

from natsort.unicode_numbers import digits_no_decimals

print("// Generated from Python", sys.version.split()[0], "(Unicode", unicodedata.unidata_version + ") and natsort 8.4:")
print("// `re` \\d (Unicode decimal digits, Nd) and natsort's digits_no_decimals.")
print("// Regenerate with tests/golden/gen_natsort_tables.py.\n")
dec = [c for c in range(0x110000) if re.match(r"\d", chr(c))]
runs = []
for c in dec:
    v = unicodedata.decimal(chr(c))
    if runs and runs[-1][1] + 1 == c and runs[-1][2] + (c - runs[-1][0]) == v:
        runs[-1][1] = c
    else:
        runs.append([c, c, v])
print("/// `(first, last, value of first)` runs of decimal digits.")
print(f"pub(crate) const DECIMAL_RUNS: [(u32, u32, u32); {len(runs)}] = [")
for a, b, v in runs:
    print(f"    (0x{a:05X}, 0x{b:05X}, {v}),")
print("];\n")
d = sorted((ord(c), unicodedata.digit(c)) for c in digits_no_decimals)
print("/// Single-character digits that are not decimals (superscripts, circled...): `(char, value)`.")
print(f"pub(crate) const DIGIT_CHARS: [(u32, u32); {len(d)}] = [")
for c, v in d:
    print(f"    (0x{c:05X}, {v}),")
print("];")
