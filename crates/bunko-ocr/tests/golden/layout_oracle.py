"""The Python 0.5.2 line layout as a line-oriented JSON service, for the Rust golden
test (it stands in for bunko-layout until that crate exists).

stdin, one JSON object per line: {"op": "pieces" | "layout", "raw": <ppocr-lines/1 page>}
stdout, one JSON object per line: {"pieces": [[i, ...], ...]} or
{"ruby": [i, ...], "bodies": [i, ...]}.
"""

import json
import os
import sys
from pathlib import Path

# The 0.5.2 sources are no longer in this tree: MOKURO_REF_052 names a checkout of commit
# 199cff5 (release 0.5.2), default the sibling worktree `../ref-0.5.2`
# (`git worktree add --detach ../ref-0.5.2 199cff5`), as for tools/onnx_export.
REF_052 = Path(os.environ.get("MOKURO_REF_052", Path(__file__).resolve().parents[5] / "ref-0.5.2"))
OCR_SRC = REF_052 / "src" / "mokuro_bunko" / "ocr"
if not (OCR_SRC / "line_layout.py").is_file():
    raise SystemExit(
        f"0.5.2 reference sources not found at {REF_052}: set MOKURO_REF_052 to a checkout of "
        "commit 199cff5 (git worktree add --detach ../ref-0.5.2 199cff5)"
    )
sys.path.insert(0, str(OCR_SRC))
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
