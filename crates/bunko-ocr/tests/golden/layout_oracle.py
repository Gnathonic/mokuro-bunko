"""The Python 0.5.2 line layout as a line-oriented JSON service, for the Rust golden
test (it stands in for bunko-layout until that crate exists).

stdin, one JSON object per line: {"op": "pieces" | "layout", "raw": <ppocr-lines/1 page>}
stdout, one JSON object per line: {"pieces": [[i, ...], ...]} or
{"ruby": [i, ...], "bodies": [i, ...]}.
"""

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "src" / "mokuro_bunko" / "ocr"))
import line_layout  # noqa: E402

for line in sys.stdin:
    req = json.loads(line)
    raw = req["raw"]
    if req["op"] == "pieces":
        out = {"pieces": line_layout.column_pieces(raw)}
    else:
        first = line_layout.layout_page(raw)
        out = {
            "ruby": sorted({run.line for run in first.ruby}),
            "bodies": sorted(set().union(*(body.members for body in first.bodies))),
        }
    sys.stdout.write(json.dumps(out) + "\n")
    sys.stdout.flush()
